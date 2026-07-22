// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {Test} from "forge-std/Test.sol";
import {console2} from "forge-std/console2.sol";
import {CommonBase} from "forge-std/Base.sol";
import {StdUtils} from "forge-std/StdUtils.sol";

import {PropPool} from "../src/PropPool.sol";
import {IPropPool} from "../src/interfaces/IPropPool.sol";
import {PropCurve} from "../src/libraries/PropCurve.sol";
import {MockERC20} from "../src/mocks/MockERC20.sol";

/// @title PropPoolHandler
/// @notice Stateful fuzzing driver for `PropPool`. One pair, four actors, every reachable state.
///
/// ## Why nothing in here reverts on a violation
///
/// `foundry.toml` sets `fail_on_revert = false` for the invariant profile, which is *required*
/// here — the handler deliberately drives the pool into states where a swap must revert
/// (paused, stale, over capacity, below the reserve floor), and a reverting call is a legitimate
/// outcome of that. The consequence is that a `require`/`assert` failure inside a handler
/// function would be swallowed along with everything else. So the handler never asserts. It
/// *records* every violation into a counter and the invariant functions in the test contract
/// assert those counters are zero. That way a detected violation cannot be lost to
/// `fail_on_revert = false`.
///
/// ## The property that matters most
///
/// Before every swap the handler asks the view path what it will get, then executes, then
/// compares. `quoted == executed`, exactly, with no tolerance, in every direction, at every
/// size, at every level of partial capacity consumption, and at every position inside the
/// staleness window. That gap — quote says one thing, fill does another — is the
/// quote-spoofing surface 0x measured at 5-10bp per trade on Solana prop AMMs. A tolerance of
/// even one wei here would defeat the purpose of the test, so there is none.
contract PropPoolHandler is CommonBase, StdUtils {
    // ---------------------------------------------------------------------
    // Fixture
    // ---------------------------------------------------------------------

    PropPool public immutable POOL;
    MockERC20 public immutable BASE_TOKEN;
    MockERC20 public immutable QUOTE_TOKEN;

    uint16 public constant PAIR_ID = 1;
    uint8 public constant PRICE_SCALE_EXP = 18;
    /// @dev Absolute ladder floor. Every ladder the handler pushes stays above it, so
    ///      `validateLadder` never rejects a push and the fuzzer does not waste runs on
    ///      rejected calls.
    uint56 public constant MIN_PRICE = 1e9;
    uint32 public constant MAX_STALE_SECS = 60;
    uint96 public constant MIN_BASE_RESERVE = 50e18;
    uint96 public constant MIN_QUOTE_RESERVE = 50_000e6;

    address public immutable MANAGER;
    address public immutable UPDATER;
    address public immutable GUARDIAN;

    address[4] public actors;

    // ---------------------------------------------------------------------
    // Violation counters — every one of these must stay at zero
    // ---------------------------------------------------------------------

    /// @notice Exact-in: the view path and the executed swap disagreed.
    uint256 public quoteDivergences;
    /// @notice Exact-out: `getAmountIn` and the executed swap disagreed.
    uint256 public inverseQuoteDivergences;
    /// @notice The tokens that actually moved did not match the swap's own return value.
    uint256 public deliveryMismatches;
    /// @notice A swap filled while the pair was paused.
    uint256 public pausedFills;
    /// @notice A swap filled while the quote was stale.
    uint256 public staleFills;
    /// @notice A successful swap left the out-side reserve below its configured floor.
    uint256 public floorBreaches;
    /// @notice A fill priced outside the ladder's worst rung.
    uint256 public ladderEscapes;
    /// @notice The two view entry points disagreed with each other.
    uint256 public viewPathDivergences;

    /// @notice First violation seen, for forensics. Empty means clean.
    string public note;

    // ---------------------------------------------------------------------
    // Coverage counters — read by the tests to prove the fuzzer got anywhere
    // ---------------------------------------------------------------------

    uint256 public exactInFills;
    uint256 public exactOutFills;
    uint256 public pushedFills;
    uint256 public blockedAttempts;
    uint256 public attemptsWhilePaused;
    uint256 public attemptsWhileStale;
    uint256 public partialCapacityFills;
    /// @notice Fills that landed with the quote strictly inside (not at the edge of) the
    ///         staleness window, i.e. `0 < age < maxStaleSecs`.
    uint256 public fillsInsideStalenessWindow;
    /// @notice Times a view function reverted. See `PropPoolViewDomainTest` — the view path is
    ///         contractually forbidden from reverting, so this is a bug counter, not coverage.
    uint256 public viewReverts;

    // ---------------------------------------------------------------------
    // Fixed-ladder accumulators — the "no path escapes the ladder" evidence
    // ---------------------------------------------------------------------

    uint256 public cumBidBaseIn;
    uint256 public cumBidQuoteOut;
    uint256 public cumAskQuoteIn;
    uint256 public cumAskBaseOut;

    constructor(
        PropPool pool_,
        MockERC20 base_,
        MockERC20 quote_,
        address manager_,
        address updater_,
        address guardian_,
        address[4] memory actors_
    ) {
        POOL = pool_;
        BASE_TOKEN = base_;
        QUOTE_TOKEN = quote_;
        MANAGER = manager_;
        UPDATER = updater_;
        GUARDIAN = guardian_;
        actors = actors_;
    }

    // ---------------------------------------------------------------------
    // Updater actions
    // ---------------------------------------------------------------------

    /// @notice Push a coherent ladder built the way the off-chain engine builds one: a mid, a
    ///         half-spread and a width, in bps. Anything `validateLadder` would reject is
    ///         unreachable by construction, which is the point — an incoherent ladder is
    ///         `PropCurve.t.sol`'s job, not this suite's.
    function pushLadder(uint256 midSeed, uint256 halfSpreadSeed, uint256 widthSeed) public {
        uint256 mid = _bound(midSeed, 1.2e9, 4e9);
        uint256 halfBps = _bound(halfSpreadSeed, 1, 200);
        uint256 widthBps = _bound(widthSeed, 0, 500);

        uint256 maxBid = (mid * (10_000 - halfBps)) / 10_000;
        uint256 minBid = (maxBid * (10_000 - widthBps)) / 10_000;
        uint256 minAsk = (mid * (10_000 + halfBps)) / 10_000;
        uint256 maxAsk = (minAsk * (10_000 + widthBps)) / 10_000;

        uint256[] memory packed = new uint256[](1);
        packed[0] = minBid | (maxBid << 56) | (minAsk << 112) | (maxAsk << 168) | (uint256(PAIR_ID) << 224);

        vm.prank(UPDATER);
        POOL.updateQuote(packed);
    }

    /// @notice New capacity epoch. Zero is included on purpose: a zero-capacity side must quote
    ///         zero and refuse to fill, and that is a reachable operational state (one-sided
    ///         withdrawal by setting the other side to 0).
    /// @dev Both bounds are BASE units now — ask capacity is base the pool will sell (`PropCurve`
    ///      amendment 1), so `4_000_000e6` read as base would be 4e12 wei of an 18-decimal token,
    ///      i.e. an ask side that is permanently exhausted. Symmetric with the bid bound.
    function refreshCapacity(uint256 bidSeed, uint256 askSeed) public {
        uint96 bidCap = uint96(_bound(bidSeed, 0, 2_000e18));
        uint96 askCap = uint96(_bound(askSeed, 0, 2_000e18));
        vm.prank(UPDATER);
        POOL.refreshCapacity(PAIR_ID, bidCap, askCap);
    }

    // ---------------------------------------------------------------------
    // Guardian actions
    // ---------------------------------------------------------------------

    /// @notice Pause with probability 1/8, unpause otherwise.
    /// @dev Not a plain `bool` argument. With a fair coin the pause flag does a symmetric random
    ///      walk and the pair ends up halted for roughly half the sequence; combined with the
    ///      global switch, the coverage probe measured 85% of all swap attempts blocked, which
    ///      starves every property that needs an actual fill. Biasing toward "running" keeps the
    ///      paused-never-fills coverage while leaving the pool tradeable most of the time.
    function nudgePause(uint256 seed) public {
        vm.prank(GUARDIAN);
        if (seed % 8 == 0) POOL.pause(PAIR_ID);
        else POOL.unpause(PAIR_ID);
    }

    function nudgeGlobalPause(uint256 seed) public {
        vm.prank(GUARDIAN);
        if (seed % 16 == 0) POOL.pauseAll();
        else POOL.unpauseAll();
    }

    // ---------------------------------------------------------------------
    // Time
    // ---------------------------------------------------------------------

    /// @notice Warp by up to 1.5x the staleness window, so every position inside the window is
    ///         reachable and so is falling off the cliff, without spending most of the sequence
    ///         on the far side of it.
    function warp(uint256 secSeed) public {
        vm.warp(block.timestamp + _bound(secSeed, 1, (3 * uint256(MAX_STALE_SECS)) / 2));
    }

    // ---------------------------------------------------------------------
    // Inventory
    // ---------------------------------------------------------------------

    function topUp(uint256 seed, bool isBase) public {
        MockERC20 token = isBase ? BASE_TOKEN : QUOTE_TOKEN;
        uint256 amount = _bound(seed, 1, isBase ? 5_000e18 : 10_000_000e6);
        token.mint(MANAGER, amount);
        vm.startPrank(MANAGER);
        token.approve(address(POOL), amount);
        POOL.deposit(address(token), amount);
        vm.stopPrank();
    }

    /// @notice A plain transfer into the pool, unaccounted. Exercises the gap between
    ///         `_reserve` and `balanceOf` that `sync` exists to close.
    function donate(uint256 seed, bool isBase) public {
        MockERC20 token = isBase ? BASE_TOKEN : QUOTE_TOKEN;
        token.mint(address(POOL), _bound(seed, 1, isBase ? 1e18 : 1e6));
    }

    function syncReserves(bool isBase) public {
        vm.prank(MANAGER);
        POOL.sync(address(isBase ? BASE_TOKEN : QUOTE_TOKEN));
    }

    // ---------------------------------------------------------------------
    // Swaps — exact in
    // ---------------------------------------------------------------------

    function swapExactIn(uint256 actorSeed, uint256 amountSeed, bool isBid) public {
        Attempt memory a = _open(actorSeed, isBid);
        uint256 amountIn = _pickAmountIn(amountSeed, isBid);
        a.tokenIn.mint(a.actor, amountIn);
        _observe(a);

        (a.quoteOk, a.quoted) = _quoteByPair(isBid, amountIn);
        {
            (bool viewOk, uint256 viewQuoted) = _quoteByTokens(a.tokenIn, a.tokenOut, amountIn);
            if (a.quoteOk != viewOk || a.quoted != viewQuoted) {
                viewPathDivergences++;
                _flag("getAmountOut disagrees with quoteByPair");
            }
        }

        vm.prank(a.actor);
        // forgefmt: disable-next-item
        try POOL.swap(
            address(a.tokenIn), address(a.tokenOut), int256(amountIn), 0, a.actor, 7, type(uint256).max
        ) returns (uint256 result) {
            exactInFills++;
            _closeExactIn(a, amountIn, result);
        } catch {
            blockedAttempts++;
            // Every reason `swap` can refuse is also a reason the view must have said zero:
            // the actor was funded, allowance is infinite, the deadline is `uint256.max`, the
            // receiver is non-zero and `limitAmount` is 0, so slippage cannot trip.
            if (a.quoteOk && a.quoted != 0) {
                quoteDivergences++;
                _flag("exact-in: view promised a fill, swap reverted");
            }
        }
    }

    // ---------------------------------------------------------------------
    // Swaps — exact out
    // ---------------------------------------------------------------------

    function swapExactOut(uint256 actorSeed, uint256 amountSeed, bool isBid) public {
        Attempt memory a = _open(actorSeed, isBid);
        uint256 amountOut = _pickAmountOut(amountSeed, isBid);
        if (amountOut == 0) return;
        // Fund past the largest input the epoch could possibly demand, so a shortfall of the
        // actor's own balance can never be confused with the pool refusing to fill.
        //
        // Computed, not hardcoded. The old `4_000_000e6` was sized against a quote-denominated ask
        // capacity; the ask side's demand is now the quote COST of the epoch's base (amendment 1),
        // which at this handler's ladder bounds reaches ~8.6e12 quote units. A near-full-epoch ask
        // exact-out therefore out-ran the mint, `safeTransferFrom` reverted, and the handler read
        // that as "the view promised a fill and the swap refused" — a false positive on the one
        // property this suite exists to check. `_epochInputCeiling` is the exact bound: `amountOut`
        // is capped at the epoch's remaining base, so its cost is capped at the cost of all of it.
        a.tokenIn.mint(a.actor, _epochInputCeiling(isBid) + (isBid ? 1e18 : 1e6));
        _observe(a);

        (a.quoteOk, a.quoted) = _quoteAmountIn(a.tokenIn, a.tokenOut, amountOut);

        vm.prank(a.actor);
        // forgefmt: disable-next-item
        try POOL.swap(
            address(a.tokenIn), address(a.tokenOut), -int256(amountOut), type(uint256).max, a.actor, 7, type(uint256).max
        ) returns (uint256 spent) {
            exactOutFills++;
            _closeExactOut(a, spent, amountOut);
        } catch {
            blockedAttempts++;
            if (a.quoteOk && a.quoted != 0) {
                inverseQuoteDivergences++;
                _flag("exact-out: view promised a fill, swap reverted");
            }
        }
    }

    // ---------------------------------------------------------------------
    // Swaps — push then settle (the router's path)
    // ---------------------------------------------------------------------

    function swapPushed(uint256 actorSeed, uint256 amountSeed, bool isBid) public {
        Attempt memory a = _open(actorSeed, isBid);
        a.tokenIn.mint(address(POOL), _pickAmountIn(amountSeed, isBid));
        _observe(a);

        // Whatever has arrived since the pool last accounted for this token — including any
        // earlier donation or the residue of a push whose swap reverted. Recomputed rather
        // than assumed, exactly as `swapWithContractBalance` computes it.
        uint256 amountIn = a.tokenIn.balanceOf(address(POOL)) - POOL.reserveOf(address(a.tokenIn));
        if (amountIn == 0) return;

        (a.quoteOk, a.quoted) = _quoteByPair(isBid, amountIn);

        vm.prank(a.actor);
        // forgefmt: disable-next-item
        try POOL.swapWithContractBalance(
            address(a.tokenIn), address(a.tokenOut), 0, a.actor, 7, type(uint256).max
        ) returns (uint256 result) {
            pushedFills++;
            _closePushed(a, amountIn, result);
        } catch {
            blockedAttempts++;
            if (a.quoteOk && a.quoted != 0) {
                quoteDivergences++;
                _flag("pushed: view promised a fill, swap reverted");
            }
        }
    }

    // ---------------------------------------------------------------------
    // Attempt bookkeeping
    // ---------------------------------------------------------------------

    /// @dev The pre-state of one swap attempt, carried as a single memory struct.
    ///      Not cosmetic: the natural formulation keeps eleven locals live across a `try`
    ///      boundary and the legacy code generator runs out of stack. `via_ir = false` in
    ///      `foundry.toml` is a deliberate choice for iteration speed, so the tests have to
    ///      live within it.
    struct Attempt {
        address actor;
        MockERC20 tokenIn;
        MockERC20 tokenOut;
        bool isBid;
        bool paused;
        bool stale;
        bool quoteOk;
        uint256 quoted;
        uint256 inBefore;
        uint256 outBefore;
        IPropPool.PairSnapshot snap;
    }

    function _open(uint256 actorSeed, bool isBid) private view returns (Attempt memory a) {
        a.actor = actors[_bound(actorSeed, 0, 3)];
        (a.tokenIn, a.tokenOut) = _tokens(isBid);
        a.isBid = isBid;
    }

    /// @dev Freeze the pool's pre-trade state. Called after funding so the balance deltas below
    ///      measure the swap and nothing else.
    function _observe(Attempt memory a) private {
        a.snap = POOL.snapshot(PAIR_ID);
        (a.paused, a.stale) = _blocked(a.snap);
        if (a.paused) attemptsWhilePaused++;
        if (a.stale) attemptsWhileStale++;
        a.inBefore = a.tokenIn.balanceOf(a.actor);
        a.outBefore = a.tokenOut.balanceOf(a.actor);
    }

    function _closeExactIn(Attempt memory a, uint256 amountIn, uint256 result) private {
        _recordFill(a, amountIn, result);
        if (!a.quoteOk) {
            quoteDivergences++;
            _flag("exact-in: view reverted but swap filled");
        } else if (result != a.quoted) {
            quoteDivergences++;
            _flag("exact-in: quoted != executed");
        }
        if (a.inBefore - a.tokenIn.balanceOf(a.actor) != amountIn) {
            deliveryMismatches++;
            _flag("exact-in: tokenIn delta != amountIn");
        }
        if (a.tokenOut.balanceOf(a.actor) - a.outBefore != result) {
            deliveryMismatches++;
            _flag("exact-in: tokenOut delta != returned amountOut");
        }
    }

    function _closeExactOut(Attempt memory a, uint256 spent, uint256 amountOut) private {
        _recordFill(a, spent, amountOut);
        if (!a.quoteOk) {
            inverseQuoteDivergences++;
            _flag("exact-out: view reverted but swap filled");
        } else if (spent != a.quoted) {
            inverseQuoteDivergences++;
            _flag("exact-out: getAmountIn != executed amountIn");
        }
        if (a.inBefore - a.tokenIn.balanceOf(a.actor) != spent) {
            deliveryMismatches++;
            _flag("exact-out: tokenIn delta != returned amountIn");
        }
        if (a.tokenOut.balanceOf(a.actor) - a.outBefore != amountOut) {
            deliveryMismatches++;
            _flag("exact-out: taker did not receive exactly amountOut");
        }
    }

    /// @dev No `tokenIn` delta check: on this path the handler pushed the input straight to the
    ///      pool, so the actor's `tokenIn` balance is untouched by construction.
    function _closePushed(Attempt memory a, uint256 amountIn, uint256 result) private {
        _recordFill(a, amountIn, result);
        if (!a.quoteOk) {
            quoteDivergences++;
            _flag("pushed: view reverted but swap filled");
        } else if (result != a.quoted) {
            quoteDivergences++;
            _flag("pushed: quoted != executed");
        }
        if (a.tokenOut.balanceOf(a.actor) - a.outBefore != result) {
            deliveryMismatches++;
            _flag("pushed: tokenOut delta != returned amountOut");
        }
    }

    function _recordFill(Attempt memory a, uint256 amountIn, uint256 amountOut) private {
        if (a.paused) {
            pausedFills++;
            _flag("a paused pair filled");
        }
        if (a.stale) {
            staleFills++;
            _flag("a stale pair filled");
        }

        uint256 age = block.timestamp - uint256(a.snap.updatedAt);
        if (age > 0 && age < uint256(a.snap.maxStaleSecs)) fillsInsideStalenessWindow++;

        uint256 used =
            a.snap.usedGen == a.snap.capGen ? (a.isBid ? uint256(a.snap.bidUsed) : uint256(a.snap.askUsed)) : 0;
        if (used != 0) partialCapacityFills++;

        _checkLadder(a.snap, a.isBid, amountIn, amountOut);
        _checkFloor(a.isBid);
        _accumulate(a.isBid, amountIn, amountOut);
    }

    /// @notice The ladder is a hard bound on the *pool-favourable* side of every fill, and it
    ///         must hold for any sequence of trades, not just for one.
    ///
    /// @dev Written as a cross-multiplication rather than as a division so there is no rounding
    ///      in the check itself:
    ///
    ///        bid — the pool buys base. It must never pay more than `maxBid` per base:
    ///              `amountOut * scale <= amountIn * maxBid`
    ///        ask — the pool sells base. It must never accept less than `minAsk` per base:
    ///              `amountIn * scale >= amountOut * minAsk`
    ///
    ///      Only the pool-favourable direction is asserted. The other direction cannot hold at
    ///      the unit level and must not be asserted: `amountOut` is floored, so a one-unit trade
    ///      can realise a price below `minBid`, and that is the correct rounding direction.
    function _checkLadder(IPropPool.PairSnapshot memory snap, bool isBid, uint256 amountIn, uint256 amountOut) private {
        uint256 scale = 10 ** uint256(snap.priceScaleExp);
        if (isBid) {
            if (amountOut * scale > amountIn * uint256(snap.maxBid)) {
                ladderEscapes++;
                _flag("bid filled above maxBid");
            }
        } else {
            if (amountIn * scale < amountOut * uint256(snap.minAsk)) {
                ladderEscapes++;
                _flag("ask filled below minAsk");
            }
        }
    }

    function _checkFloor(bool isBid) private {
        (address token, uint256 floor) = isBid
            ? (address(QUOTE_TOKEN), uint256(MIN_QUOTE_RESERVE))
            : (address(BASE_TOKEN), uint256(MIN_BASE_RESERVE));
        if (POOL.reserveOf(token) < floor || MockERC20(token).balanceOf(address(POOL)) < floor) {
            floorBreaches++;
            _flag("successful swap left the out-side reserve below its floor");
        }
    }

    function _accumulate(bool isBid, uint256 amountIn, uint256 amountOut) private {
        if (isBid) {
            cumBidBaseIn += amountIn;
            cumBidQuoteOut += amountOut;
        } else {
            cumAskQuoteIn += amountIn;
            cumAskBaseOut += amountOut;
        }
    }

    // ---------------------------------------------------------------------
    // View wrappers — the view path must never revert, so failures are recorded
    // ---------------------------------------------------------------------

    function _quoteByPair(bool isBid, uint256 amountIn) private returns (bool ok, uint256 out) {
        try POOL.quoteByPair(PAIR_ID, isBid, amountIn) returns (uint256 r) {
            return (true, r);
        } catch {
            viewReverts++;
            return (false, 0);
        }
    }

    function _quoteByTokens(MockERC20 tokenIn, MockERC20 tokenOut, uint256 amountIn)
        private
        returns (bool ok, uint256 out)
    {
        try POOL.getAmountOut(address(tokenIn), address(tokenOut), amountIn) returns (uint256 r) {
            return (true, r);
        } catch {
            viewReverts++;
            return (false, 0);
        }
    }

    function _quoteAmountIn(MockERC20 tokenIn, MockERC20 tokenOut, uint256 amountOut)
        private
        returns (bool ok, uint256 into)
    {
        try POOL.getAmountIn(address(tokenIn), address(tokenOut), amountOut) returns (uint256 r) {
            return (true, r);
        } catch {
            viewReverts++;
            return (false, 0);
        }
    }

    // ---------------------------------------------------------------------
    // Helpers
    // ---------------------------------------------------------------------

    function _tokens(bool isBid) private view returns (MockERC20 tokenIn, MockERC20 tokenOut) {
        return isBid ? (BASE_TOKEN, QUOTE_TOKEN) : (QUOTE_TOKEN, BASE_TOKEN);
    }

    function _blocked(IPropPool.PairSnapshot memory snap) private view returns (bool paused, bool stale) {
        paused = POOL.allPaused() || (snap.flags & 1) != 0;
        stale = snap.updatedAt == 0 || uint256(snap.updatedAt) > block.timestamp
            || block.timestamp - uint256(snap.updatedAt) > uint256(snap.maxStaleSecs);
    }

    /// @dev Sizes cluster around the epoch's ceiling on this leg — that is where the interesting
    ///      arithmetic lives — with a deliberate overshoot band so "one unit too big" is hit.
    ///
    ///      **The ceiling is not `remaining` on the ask side.** Capacity and usage are base units on
    ///      both sides (amendment 1) and an ask's `amountIn` is quote, so the epoch's ceiling on an
    ///      ask exact-in leg is the quote COST of all its remaining base — the same quantity
    ///      `PropPool._maxAmountIn` computes and `PropCurve.amountOutAsk` compares against
    ///      internally. Feeding `remaining` straight through sized ask attempts by a base figure read
    ///      as quote: ~12 orders of magnitude too small on an 18/6 pair, so essentially every ask
    ///      draw was dust or a revert and the coverage probe's pushed-fill floor stopped being met.
    function _pickAmountIn(uint256 seed, bool isBid) private view returns (uint256) {
        IPropPool.PairSnapshot memory snap = POOL.snapshot(PAIR_ID);
        uint256 capacity = isBid ? uint256(snap.bidCapacity) : uint256(snap.askCapacity);
        uint256 used = snap.usedGen == snap.capGen ? (isBid ? uint256(snap.bidUsed) : uint256(snap.askUsed)) : 0;
        uint256 remaining = capacity > used ? capacity - used : 0;
        uint256 ceiling = isBid ? remaining : _askQuoteCeiling(snap, used, remaining);
        uint256 hi = ceiling == 0 ? (isBid ? 1e18 : 1e6) : ceiling + ceiling / 8 + 1;
        return _bound(seed, 1, hi);
    }

    /// @dev Bounded by what the epoch could actually deliver, again with an overshoot band, so
    ///      the exact-out path is exercised across its whole fillable range instead of spending most
    ///      draws on trivially-unfillable sizes.
    ///
    ///      An ask's `amountOut` IS the base leg, so here `remaining` is already the right ceiling
    ///      and needs no conversion — the asymmetry with `_pickAmountIn` is the whole point of
    ///      amendment 1. Only the bid's `amountOut` is quote and has to go through the curve.
    function _pickAmountOut(uint256 seed, bool isBid) private returns (uint256) {
        IPropPool.PairSnapshot memory snap = POOL.snapshot(PAIR_ID);
        uint256 capacity = isBid ? uint256(snap.bidCapacity) : uint256(snap.askCapacity);
        uint256 used = snap.usedGen == snap.capGen ? (isBid ? uint256(snap.bidUsed) : uint256(snap.askUsed)) : 0;
        uint256 remaining = capacity > used ? capacity - used : 0;
        if (remaining == 0) return _bound(seed, 1, isBid ? 1e6 : 1e18);
        if (!isBid) return _bound(seed, 1, remaining + remaining / 8 + 1);
        (, uint256 maxOut) = _quoteByPair(true, remaining);
        if (maxOut == 0) return _bound(seed, 1, 1e6);
        return _bound(seed, 1, maxOut + maxOut / 8 + 1);
    }

    /// @dev `PropCurve.amountInAsk(remaining, ...)`, guarded so it cannot revert and take a handler
    ///      call with it. `ZeroCapacity` needs `askCapacity == 0`, which forces `remaining == 0`;
    ///      `ZeroPrice` needs `maxAsk == 0`, i.e. a pair that has never been quoted;
    ///      `AmountExceedsCapacity` cannot fire because `used + remaining == capacity`; and
    ///      `AmountOutOfDomain` is excluded by the bound `refreshCapacity` enforces at write time.
    function _askQuoteCeiling(IPropPool.PairSnapshot memory snap, uint256 used, uint256 remaining)
        private
        pure
        returns (uint256)
    {
        if (remaining == 0 || snap.maxAsk == 0) return 0;
        return PropCurve.amountInAsk(remaining, snap.minAsk, snap.maxAsk, snap.askCapacity, used, snap.priceScaleExp);
    }

    /// @dev The largest `amountIn` this side's epoch can demand, in the INPUT token — the handler's
    ///      mirror of `PropPool._maxAmountIn`. Remaining base for a bid, the quote cost of that base
    ///      for an ask.
    function _epochInputCeiling(bool isBid) private view returns (uint256) {
        IPropPool.PairSnapshot memory snap = POOL.snapshot(PAIR_ID);
        uint256 capacity = isBid ? uint256(snap.bidCapacity) : uint256(snap.askCapacity);
        uint256 used = snap.usedGen == snap.capGen ? (isBid ? uint256(snap.bidUsed) : uint256(snap.askUsed)) : 0;
        if (capacity <= used) return 0;
        uint256 remaining = capacity - used;
        return isBid ? remaining : _askQuoteCeiling(snap, used, remaining);
    }

    function _flag(string memory reason) private {
        if (bytes(note).length == 0) note = reason;
    }
}

// =========================================================================================
//                            Shared fixture for both suites
// =========================================================================================

/// @dev Deployment only. Deliberately holds **no** `invariant_*` function: a test contract that
///      inherits one becomes an invariant suite, and a suite that never calls `targetContract`
///      lets the fuzzer drive every contract created in `setUp` — including the tokens, with the
///      pool itself as `msg.sender`. That produces "failures" (the pool transferring its own
///      inventory away) that say nothing about the pool. The invariants live one level down.
abstract contract PropPoolFixture is Test {
    PropPool internal pool;
    MockERC20 internal baseToken;
    MockERC20 internal quoteToken;
    PropPoolHandler internal handler;

    address internal owner = makeAddr("owner");
    address internal manager = makeAddr("manager");
    address internal updater = makeAddr("updater");
    address internal guardian = makeAddr("guardian");

    uint16 internal constant PAIR_ID = 1;
    uint8 internal constant PRICE_SCALE_EXP = 18;
    uint56 internal constant MIN_PRICE = 1e9;
    uint32 internal constant MAX_STALE_SECS = 60;
    uint96 internal constant MIN_BASE_RESERVE = 50e18;
    uint96 internal constant MIN_QUOTE_RESERVE = 50_000e6;

    uint256 internal constant SEED_BASE = 20_000e18;
    uint256 internal constant SEED_QUOTE = 40_000_000e6;

    function _deploy() internal {
        // A realistic wall-clock timestamp. `updateQuote` stamps `uint32(block.timestamp)`, and
        // starting from Foundry's default of 1 would make the freshness arithmetic untypical.
        vm.warp(1_800_000_000);

        pool = new PropPool(owner, manager, updater, guardian);
        baseToken = new MockERC20("Base", "BASE", 18);
        quoteToken = new MockERC20("Quote", "QUOTE", 6);

        vm.prank(owner);
        pool.addPair(address(baseToken), address(quoteToken), PRICE_SCALE_EXP, MAX_STALE_SECS, MIN_PRICE);
        vm.prank(manager);
        pool.setPairConfig(PAIR_ID, MAX_STALE_SECS, MIN_PRICE, MIN_BASE_RESERVE, MIN_QUOTE_RESERVE);

        baseToken.mint(manager, SEED_BASE);
        quoteToken.mint(manager, SEED_QUOTE);
        vm.startPrank(manager);
        baseToken.approve(address(pool), type(uint256).max);
        quoteToken.approve(address(pool), type(uint256).max);
        pool.deposit(address(baseToken), SEED_BASE);
        pool.deposit(address(quoteToken), SEED_QUOTE);
        vm.stopPrank();

        address[4] memory actors = [makeAddr("alice"), makeAddr("bob"), makeAddr("carol"), makeAddr("dave")];
        for (uint256 i; i < 4; ++i) {
            vm.startPrank(actors[i]);
            baseToken.approve(address(pool), type(uint256).max);
            quoteToken.approve(address(pool), type(uint256).max);
            vm.stopPrank();
        }

        handler = new PropPoolHandler(pool, baseToken, quoteToken, manager, updater, guardian, actors);

        _pushLadder(2e9, 5, 25);
        // Both BASE units. The ask side used to read `2_000_000e6` as quote; 1_000 base at the 2e9
        // mid is the same ~$2M of sell-side budget, and its quote ceiling is `_askQuoteCeiling()`.
        vm.prank(updater);
        pool.refreshCapacity(PAIR_ID, 1_000e18, 1_000e18);
    }

    /// @dev The largest `amountIn` the epoch accepts on this side, in the INPUT token — the fixture's
    ///      mirror of `PropPool._maxAmountIn`. For a bid that is the remaining base; for an ask it is
    ///      the quote COST of the remaining base, because capacity and usage are base-denominated on
    ///      both sides (`PropCurve` amendment 1) while an ask's input is quote. Any test that bounds
    ///      an ask `amountIn` by `askCapacity` is comparing two different units.
    function _maxAmountInFor(bool isBid) internal view returns (uint256) {
        IPropPool.PairSnapshot memory snap = pool.snapshot(PAIR_ID);
        uint256 capacity = isBid ? uint256(snap.bidCapacity) : uint256(snap.askCapacity);
        uint256 used = snap.usedGen == snap.capGen ? (isBid ? uint256(snap.bidUsed) : uint256(snap.askUsed)) : 0;
        if (capacity <= used) return 0;
        uint256 remaining = capacity - used;
        if (isBid) return remaining;
        return PropCurve.amountInAsk(remaining, snap.minAsk, snap.maxAsk, snap.askCapacity, used, snap.priceScaleExp);
    }

    function _pushLadder(uint256 mid, uint256 halfBps, uint256 widthBps) internal {
        uint256 maxBid = (mid * (10_000 - halfBps)) / 10_000;
        uint256 minBid = (maxBid * (10_000 - widthBps)) / 10_000;
        uint256 minAsk = (mid * (10_000 + halfBps)) / 10_000;
        uint256 maxAsk = (minAsk * (10_000 + widthBps)) / 10_000;
        uint256[] memory packed = new uint256[](1);
        packed[0] = minBid | (maxBid << 56) | (minAsk << 112) | (maxAsk << 168) | (uint256(PAIR_ID) << 224);
        vm.prank(updater);
        pool.updateQuote(packed);
    }
}

// =========================================================================================
//                      Invariants shared by the two stateful suites
// =========================================================================================

abstract contract PropPoolInvariantBase is PropPoolFixture {
    /// @notice Balances and accounted reserves never fall below the configured floor.
    /// @dev Checked as a state property after every handler call, which is the literal reading
    ///      of "never falls below the floor while a swap succeeds": the only thing that can
    ///      lower a reserve here is a swap (the handler never withdraws), so if the property
    ///      holds after every call it held during every fill.
    function invariant_reserveNeverBelowFloor() public view {
        assertGe(pool.reserveOf(address(baseToken)), MIN_BASE_RESERVE, "base reserve below floor");
        assertGe(pool.reserveOf(address(quoteToken)), MIN_QUOTE_RESERVE, "quote reserve below floor");
        assertGe(baseToken.balanceOf(address(pool)), MIN_BASE_RESERVE, "base balance below floor");
        assertGe(quoteToken.balanceOf(address(pool)), MIN_QUOTE_RESERVE, "quote balance below floor");
        assertEq(handler.floorBreaches(), 0, handler.note());
    }

    /// @notice Cumulative `used` never exceeds capacity inside one generation.
    function invariant_usedNeverExceedsCapacity() public view {
        IPropPool.PairSnapshot memory snap = pool.snapshot(PAIR_ID);
        if (snap.usedGen == snap.capGen) {
            assertLe(snap.bidUsed, snap.bidCapacity, "bidUsed > bidCapacity");
            assertLe(snap.askUsed, snap.askCapacity, "askUsed > askCapacity");
        }
    }

    /// @notice Accounted inventory is never more than the pool actually holds.
    function invariant_reserveNeverExceedsBalance() public view {
        assertLe(pool.reserveOf(address(baseToken)), baseToken.balanceOf(address(pool)), "base over-accounted");
        assertLe(pool.reserveOf(address(quoteToken)), quoteToken.balanceOf(address(pool)), "quote over-accounted");
    }

    /// @notice A paused or stale pair never fills.
    function invariant_pausedOrStaleNeverFills() public view {
        assertEq(handler.pausedFills(), 0, "a paused pair filled");
        assertEq(handler.staleFills(), 0, "a stale pair filled");
    }

    /// @notice No fill escapes the ladder's worst rung, in either direction.
    function invariant_ladderBoundsEveryFill() public view {
        assertEq(handler.ladderEscapes(), 0, handler.note());
    }

    /// @notice **The one that matters.** For identical state, the view quote and the executed
    ///         swap agree exactly — same number, both directions, no tolerance.
    function invariant_quoteEqualsExecution() public view {
        assertEq(handler.quoteDivergences(), 0, handler.note());
        assertEq(handler.inverseQuoteDivergences(), 0, handler.note());
        assertEq(handler.deliveryMismatches(), 0, handler.note());
        assertEq(handler.viewPathDivergences(), 0, handler.note());
    }
}

// =========================================================================================
//                       Suite 1 — everything moves, including the ladder
// =========================================================================================

contract PropPoolInvariantTest is PropPoolInvariantBase {
    function setUp() public {
        _deploy();
        targetContract(address(handler));
    }
}

// =========================================================================================
//     Suite 2 — the ladder is frozen: no sequence of swaps may escape its worst price
// =========================================================================================

/// @notice Same handler, but `pushLadder` is removed from the fuzzer's vocabulary. Capacity
///         refreshes, pauses, warps, deposits and swaps all stay in — the point is that with
///         the four prices held fixed, *no* path through the rest of the state machine lets
///         the pool buy above `maxBid` or sell below `minAsk`, cumulatively or per fill.
contract PropPoolFixedLadderInvariantTest is PropPoolInvariantBase {
    uint256 internal fixedMaxBid;
    uint256 internal fixedMinAsk;
    uint256 internal fixedScale;

    function setUp() public {
        _deploy();

        IPropPool.PairSnapshot memory snap = pool.snapshot(PAIR_ID);
        fixedMaxBid = uint256(snap.maxBid);
        fixedMinAsk = uint256(snap.minAsk);
        fixedScale = 10 ** uint256(snap.priceScaleExp);

        bytes4[] memory selectors = new bytes4[](9);
        selectors[0] = PropPoolHandler.swapExactIn.selector;
        selectors[1] = PropPoolHandler.swapExactOut.selector;
        selectors[2] = PropPoolHandler.swapPushed.selector;
        selectors[3] = PropPoolHandler.refreshCapacity.selector;
        selectors[4] = PropPoolHandler.nudgePause.selector;
        selectors[5] = PropPoolHandler.nudgeGlobalPause.selector;
        selectors[6] = PropPoolHandler.warp.selector;
        selectors[7] = PropPoolHandler.topUp.selector;
        selectors[8] = PropPoolHandler.donate.selector;

        targetSelector(FuzzSelector({addr: address(handler), selectors: selectors}));
        targetContract(address(handler));
    }

    /// @notice The ladder never moved, so the aggregate of every fill in the run must also sit
    ///         inside it. Per-fill bounds are already asserted by
    ///         `invariant_ladderBoundsEveryFill`; this is the statement that they cannot be
    ///         gamed by splitting, interleaving, or refreshing capacity between them.
    function invariant_cumulativeFillsStayInsideFixedLadder() public view {
        IPropPool.PairSnapshot memory snap = pool.snapshot(PAIR_ID);
        assertEq(uint256(snap.maxBid), fixedMaxBid, "ladder moved: maxBid");
        assertEq(uint256(snap.minAsk), fixedMinAsk, "ladder moved: minAsk");

        assertLe(
            handler.cumBidQuoteOut() * fixedScale,
            handler.cumBidBaseIn() * fixedMaxBid,
            "cumulative bids paid above maxBid"
        );
        assertGe(
            handler.cumAskQuoteIn() * fixedScale,
            handler.cumAskBaseOut() * fixedMinAsk,
            "cumulative asks sold below minAsk"
        );
    }
}

// =========================================================================================
//   Targeted, deterministic companions. The stateful suites can only *catch* these; these
//   prove the fuzzer would have had something to catch.
// =========================================================================================

contract PropPoolGuardTest is PropPoolFixture {
    address internal taker = makeAddr("taker");

    /// @notice Smallest base input whose bid output is non-zero at this fixture's ladder
    ///         (`ceil(10**18 / maxBid)`, maxBid ~2e9). Below it the quote is 0 and `swap`
    ///         reverts with `ZeroOutput` — the pool refusing to round in the taker's favour.
    uint256 internal constant DUST_BASE_IN = 1e9;

    function setUp() public {
        _deploy();
        baseToken.mint(taker, 1_000e18);
        quoteToken.mint(taker, 2_000_000e6);
        vm.startPrank(taker);
        baseToken.approve(address(pool), type(uint256).max);
        quoteToken.approve(address(pool), type(uint256).max);
        vm.stopPrank();
    }

    function _swapBid(uint256 amountIn) internal returns (uint256) {
        vm.prank(taker);
        return pool.swap(address(baseToken), address(quoteToken), int256(amountIn), 0, taker, 0, type(uint256).max);
    }

    function _swapAsk(uint256 amountIn) internal returns (uint256) {
        vm.prank(taker);
        return pool.swap(address(quoteToken), address(baseToken), int256(amountIn), 0, taker, 0, type(uint256).max);
    }

    function test_pausedPairNeitherQuotesNorFills() public {
        vm.prank(guardian);
        pool.pause(PAIR_ID);

        assertEq(pool.quoteByPair(PAIR_ID, true, 1e18), 0, "paused pair still quoted");
        assertEq(pool.getAmountOut(address(baseToken), address(quoteToken), 1e18), 0);
        assertEq(pool.getAmountIn(address(baseToken), address(quoteToken), 1e6), 0);

        vm.expectRevert(PropPool.PoolPaused.selector);
        _swapBid(1e18);

        vm.prank(guardian);
        pool.unpause(PAIR_ID);
        assertGt(_swapBid(1e18), 0, "unpause did not restore fills");
    }

    function test_globallyPausedPoolNeitherQuotesNorFills() public {
        vm.prank(guardian);
        pool.pauseAll();

        assertEq(pool.quoteByPair(PAIR_ID, true, 1e18), 0);
        vm.expectRevert(PropPool.PoolPaused.selector);
        _swapBid(1e18);

        vm.prank(guardian);
        pool.unpauseAll();
        assertGt(_swapBid(1e18), 0);
    }

    function test_staleQuoteNeitherQuotesNorFills() public {
        // One second inside the window still fills; one second past it does not.
        vm.warp(block.timestamp + MAX_STALE_SECS);
        assertGt(pool.quoteByPair(PAIR_ID, true, 1e18), 0, "edge of window should still fill");
        assertGt(_swapBid(1e18), 0);

        vm.warp(block.timestamp + 1);
        assertEq(pool.quoteByPair(PAIR_ID, true, 1e18), 0, "past the cliff but still quoting");
        vm.expectRevert(PropPool.StaleQuote.selector);
        _swapBid(1e18);
    }

    /// @notice A pair with a `maxStaleSecs` larger than the current unix timestamp must still
    ///         read as stale before its first push. `updatedAt == 0` is checked explicitly in
    ///         `_load` for exactly this reason.
    function test_neverQuotedPairIsStaleNotFresh() public {
        MockERC20 other = new MockERC20("Other", "OTH", 18);
        vm.prank(owner);
        uint16 id = pool.addPair(address(other), address(quoteToken), 18, type(uint32).max, 1);
        assertEq(pool.quoteByPair(id, true, 1e18), 0, "a never-quoted pair must not look fresh");
        assertEq(pool.getAmountOut(address(other), address(quoteToken), 1e18), 0);
    }

    /// @notice Exhausting capacity does not make the pool quote or fill one unit more.
    function test_capacityIsAHardCeiling() public {
        IPropPool.PairSnapshot memory snap = pool.snapshot(PAIR_ID);
        uint256 capacity = uint256(snap.bidCapacity);

        baseToken.mint(taker, capacity);
        _swapBid(capacity - 10e18);

        assertEq(pool.quoteByPair(PAIR_ID, true, 10e18 + 1), 0, "quoted beyond capacity");
        assertGt(pool.quoteByPair(PAIR_ID, true, 10e18), 0, "refused a fill inside capacity");

        vm.expectRevert(PropPool.InsufficientCapacity.selector);
        _swapBid(10e18 + 1);

        _swapBid(10e18);
        snap = pool.snapshot(PAIR_ID);
        assertEq(uint256(snap.bidUsed), capacity, "used should sit exactly at capacity");
        assertEq(pool.quoteByPair(PAIR_ID, true, 1), 0, "quoted with capacity fully used");
    }

    // =====================================================================
    // CLOSED — splitting a trade no longer beats executing it whole, on
    // either side. Two mechanisms, both fixed; these are the regression
    // witnesses, with their assertions turned the right way round.
    // =====================================================================
    //
    // `PropCurve.amountOutBid` used to state the invariant outright and not deliver it:
    //
    //   "Every rounding on this path now points the same way (midUsage up, discount up,
    //    amountOut down), which is what makes splitting a trade provably never beat executing
    //    it whole."
    //
    // It failed on both sides, for two independent reasons.
    //
    // 1. ASK SIDE — structural, not rounding, and the one with economic teeth.
    //
    //    Charging the midpoint price of the consumed interval is exact only when the output is
    //    *linear* in price. It was, on the bid: `amountOut = amountIn * avgBid / S`, and the
    //    integral of a linear price over `[a,b]` really is the price at `(a+b)/2`. On the ask the
    //    output was `amountIn * S / avgAsk` — the *reciprocal* of the price, which is convex. The
    //    true continuous fill is `∫ dq / p(q) = (C/W) * ln(p_end/p_start)`, a logarithm, and by
    //    Jensen the midpoint rule always under-delivered against it, so every extra split was a
    //    finer quadrature of the same integral and strictly improved the taker's fill:
    //
    //       ladder width    one split     saturated (99 splits)
    //          25 bps       0.0039 bps        0.0052 bps
    //         100 bps       0.0619 bps        0.0825 bps
    //         500 bps       1.4874 bps        1.9835 bps
    //        2000 bps      20.7039 bps       27.6828 bps
    //
    //    Noise at the tight widths the pool intends to quote; several bps of free fill at the widths
    //    a stressed quoter would post, leaked in the same units the pool exists to earn. Splitting
    //    every ask was a strictly dominant taker strategy.
    //
    //    FIXED by amendment 1: ask capacity and usage are BASE units, so the ask side's primitive is
    //    `amountInAsk` (base out, quote cost) and both legs are linear in the price. The exact
    //    rational integral is additive by construction and `amountOutAsk` is its exact inverse, hence
    //    super-additive. `test_splitAdvantageIsBoundedAndDrivenByLadderWidth` below now measures the
    //    same grid and finds the advantage is identically zero at every width and every split count —
    //    and, critically, that what is left no longer scales with width at all.
    //
    // 2. BID SIDE — rounding, and negligible, but real.
    //
    //    `discount = ceil(width * midUsage / capacity)` cost the taker `amountIn * f / S` of output,
    //    where `f = ceil(...) - exact ∈ [0,1)`. `f` was a function of `midUsage`, so a split
    //    resampled it — three draws instead of one, and a lucky split point landed on a smaller
    //    total.
    //
    //    FIXED by amendment 2: one rounding per trade, applied to the amount rather than the price,
    //    so there is nothing to resample. `sum floor(x_i) <= floor(sum x_i)` and the shortfall is at
    //    most one output unit per extra piece.

    /// @notice Regression witness, mechanism 1: two equal ask legs must not beat one whole ask.
    /// @dev `total` is the epoch's QUOTE CEILING, not `askCapacity`. Ask capacity is base
    ///      (amendment 1), so passing it as a quote `amountIn` asks for ~1000x the epoch's quote
    ///      budget and gets `InsufficientCapacity` — which is why the pre-amendment form of this test
    ///      reverted rather than failing an assertion.
    function test_BUG_splittingAnAskBeatsExecutingItWhole() public {
        uint256 total = _maxAmountInFor(false);
        uint256 whole = pool.quoteByPair(PAIR_ID, false, total);
        assertEq(whole, uint256(pool.snapshot(PAIR_ID).askCapacity), "the ceiling must buy the whole epoch");

        uint256 snapId = vm.snapshotState();
        quoteToken.mint(taker, total);
        uint256 partA = _swapAsk(total / 2);
        uint256 partB = _swapAsk(total - total / 2);
        vm.revertToState(snapId);

        assertLe(partA + partB, whole, "splitting an ask beat executing it whole");
        // And the split gives up at most one base unit, in the pool's favour.
        assertGe(partA + partB + 1, whole, "two-leg ask split lost more than one base unit");
    }

    /// @notice Regression witness, mechanism 2: a specific unequal bid split must not win.
    /// @dev The split point is not arbitrary: it was found by search over this fixture's exact
    ///      ladder (`maxBid = 1_999_000_000`, `width = 4_997_500`, `capacity = 1e21`,
    ///      `priceScaleExp = 18`), landing on a favourable residue of
    ///      `width * midUsage mod capacity`. Under one-rounding-on-the-amount there is no such
    ///      residue to find, and this split now ties the whole trade exactly.
    function test_BUG_splittingABidCanBeatExecutingItWhole() public {
        uint256 total = 989_311_460_852_191_150_373;
        uint256 firstPart = 274_342_765_907_638_902_005;

        uint256 whole = pool.quoteByPair(PAIR_ID, true, total);

        uint256 snapId = vm.snapshotState();
        baseToken.mint(taker, total);
        uint256 partA = _swapBid(firstPart);
        uint256 partB = _swapBid(total - firstPart);
        vm.revertToState(snapId);

        assertLe(partA + partB, whole, "splitting a bid beat executing it whole");
        assertGe(partA + partB + 1, whole, "two-leg bid split lost more than one quote unit");
    }

    /// @notice The counterpart of the table above, re-measured. The ask-side advantage used to scale
    ///         with the SQUARE of the posted ladder width and saturate at the log integral; it is now
    ///         identically zero at every width and every split count, and the residual — a shortfall,
    ///         in the pool's favour — is driven only by the number of pieces, not by the width.
    ///
    /// @dev That independence is the real evidence the fix is structural rather than a re-tuning.
    ///      A quadrature error shrinks with width; an integer-rounding residue does not care about
    ///      width at all, and this measures exactly that: ~`n - 1` base units at every one of the
    ///      four widths, including the 2000 bps ladder that used to leak 27.7 bps.
    function test_splitAdvantageIsBoundedAndDrivenByLadderWidth() public {
        uint16[4] memory widths = [25, 100, 500, 2000];

        console2.log("ask-side split, base units the taker GIVES UP vs one shot (0 = tie, none gain)");
        console2.log("  width(bps)   1 split      9 splits     99 splits");
        for (uint256 i; i < 4; ++i) {
            _pushLadder(2e9, 5, widths[i]);
            (uint256 gain2, uint256 loss2) = _askSplitDelta(2);
            (uint256 gain10, uint256 loss10) = _askSplitDelta(10);
            (uint256 gain100, uint256 loss100) = _askSplitDelta(100);
            console2.log(
                string.concat(
                    "  ",
                    _pad(vm.toString(uint256(widths[i])), 11),
                    _pad(vm.toString(loss2), 13),
                    _pad(vm.toString(loss10), 13),
                    vm.toString(loss100)
                )
            );

            // No advantage at any granularity — the property amendment 1 bought.
            assertEq(gain2, 0, "a 2-leg ask split still beat one shot");
            assertEq(gain10, 0, "a 10-leg ask split still beat one shot");
            assertEq(gain100, 0, "a 100-leg ask split still beat one shot");

            // The residual is bounded by one base unit per extra piece, in the pool's favour.
            assertLe(loss2, 1, "2-leg residual above the n-1 unit bound");
            assertLe(loss10, 9, "10-leg residual above the n-1 unit bound");
            assertLe(loss100, 99, "100-leg residual above the n-1 unit bound");
        }

        // Width independence, stated directly: the 2000 bps ladder — the one that used to leak
        // 27.7 bps to a 100-way split — now behaves the same as the 25 bps one, to within a few
        // base wei out of 1e21. A quadrature error could not do that.
        _pushLadder(2e9, 5, 25);
        (, uint256 tight) = _askSplitDelta(100);
        _pushLadder(2e9, 5, 2000);
        (uint256 wideGain, uint256 wide) = _askSplitDelta(100);
        assertEq(wideGain, 0, "the widest ladder still leaks to splitting");
        assertLe(wide > tight ? wide - tight : tight - wide, 99, "the residual still tracks ladder width");
    }

    /// @dev Executes a full-epoch ask as `n` equal legs and returns `(gain, loss)` in BASE units
    ///      against executing it whole — exactly one of the two is non-zero, and `gain` must always
    ///      be the zero one. State is snapshotted and restored, so each measurement starts from the
    ///      same place. The legs are equal slices of the epoch's QUOTE CEILING, and usage advances by
    ///      the base each leg delivers, which the pool does for us.
    function _askSplitDelta(uint256 n) internal returns (uint256 gain, uint256 loss) {
        uint256 total = _maxAmountInFor(false);
        uint256 whole = pool.quoteByPair(PAIR_ID, false, total);

        uint256 snapId = vm.snapshotState();
        quoteToken.mint(taker, total);
        uint256 got;
        uint256 spent;
        for (uint256 i; i < n; ++i) {
            uint256 leg = i + 1 == n ? total - spent : total / n;
            spent += leg;
            got += _swapAsk(leg);
        }
        vm.revertToState(snapId);

        if (got > whole) return (got - whole, 0);
        return (0, whole - got);
    }

    function _pad(string memory s, uint256 width) internal pure returns (string memory) {
        while (bytes(s).length < width) {
            s = string.concat(s, " ");
        }
        return s;
    }

    /// @notice Quote and execution agree across the whole staleness window, at every level of
    ///         partial capacity consumption, in both directions.
    /// @dev Both draws are bounded by `_maxAmountInFor`, the epoch's ceiling on this side IN THE
    ///      INPUT TOKEN — which for an ask is the quote cost of its base, not `askCapacity`. Bounding
    ///      a quote-denominated ask `amountIn` by the base capacity would draw sizes ~1000x the
    ///      ceiling on this fixture, every one of them would quote 0, and the ask half of this test
    ///      would spend every run in the "must revert" branch without ever comparing a fill.
    function testFuzz_quoteMatchesExecution(uint256 preFill, uint256 amountIn, uint32 age, bool isBid) public {
        age = uint32(bound(age, 0, MAX_STALE_SECS));
        uint256 ceiling = _maxAmountInFor(isBid);

        preFill = bound(preFill, 0, ceiling - 1);
        amountIn = bound(amountIn, 1, ceiling - preFill);

        (address tokenIn, address tokenOut) =
            isBid ? (address(baseToken), address(quoteToken)) : (address(quoteToken), address(baseToken));

        MockERC20(tokenIn).mint(taker, preFill + amountIn);
        // Partially consume the epoch first, so the measured size is priced from a mid-ladder
        // position rather than always from zero usage. Skipped when `preFill` is so small that it
        // rounds to zero output — the pool correctly refuses that, and it is not what is under
        // test here.
        if (preFill != 0 && pool.quoteByPair(PAIR_ID, isBid, preFill) != 0) {
            vm.prank(taker);
            pool.swap(tokenIn, tokenOut, int256(preFill), 0, taker, 0, type(uint256).max);
        }

        vm.warp(block.timestamp + age);

        uint256 quoted = pool.quoteByPair(PAIR_ID, isBid, amountIn);
        assertEq(quoted, pool.getAmountOut(tokenIn, tokenOut, amountIn), "the two view paths disagree");

        if (quoted == 0) {
            vm.prank(taker);
            vm.expectRevert();
            pool.swap(tokenIn, tokenOut, int256(amountIn), 0, taker, 0, type(uint256).max);
            return;
        }

        uint256 before = MockERC20(tokenOut).balanceOf(taker);
        vm.prank(taker);
        uint256 executed = pool.swap(tokenIn, tokenOut, int256(amountIn), 0, taker, 0, type(uint256).max);

        assertEq(executed, quoted, "quoted != executed");
        assertEq(MockERC20(tokenOut).balanceOf(taker) - before, quoted, "delivered != quoted");
    }

    /// @notice Same, for the inverse quote.
    /// @dev `preFill` is an exact-IN amount, so it is bounded by `_maxAmountInFor` — the epoch's
    ///      ceiling in the input token — for the same reason as `testFuzz_quoteMatchesExecution`.
    ///      `amountOut` is then bounded by what the remainder of the epoch can deliver, read off the
    ///      forward quote at the post-fill ceiling rather than assumed from capacity.
    function testFuzz_inverseQuoteMatchesExecution(uint256 preFill, uint256 amountOut, bool isBid) public {
        uint256 inCeiling = _maxAmountInFor(isBid);
        preFill = bound(preFill, 0, inCeiling / 2);

        (address tokenIn, address tokenOut) =
            isBid ? (address(baseToken), address(quoteToken)) : (address(quoteToken), address(baseToken));

        MockERC20(tokenIn).mint(taker, inCeiling * 2);
        if (preFill != 0 && pool.quoteByPair(PAIR_ID, isBid, preFill) != 0) {
            vm.prank(taker);
            pool.swap(tokenIn, tokenOut, int256(preFill), 0, taker, 0, type(uint256).max);
        }

        uint256 ceiling = pool.quoteByPair(PAIR_ID, isBid, _maxAmountInFor(isBid));
        if (ceiling == 0) return;
        amountOut = bound(amountOut, 1, ceiling);

        uint256 quotedIn = pool.getAmountIn(tokenIn, tokenOut, amountOut);
        assertGt(quotedIn, 0, "inverse quote refused a fillable size");

        // Minimality, checked *before* the fill: one unit less of input must not have covered
        // `amountOut`. Asking afterwards would evaluate the forward curve at a different usage
        // level than the one `getAmountIn` inverted, which would not be the same question.
        assertLt(pool.quoteByPair(PAIR_ID, isBid, quotedIn - 1), amountOut, "inverse quote was not minimal");

        uint256 outBefore = MockERC20(tokenOut).balanceOf(taker);
        vm.prank(taker);
        uint256 spent = pool.swap(tokenIn, tokenOut, -int256(amountOut), type(uint256).max, taker, 0, type(uint256).max);

        assertEq(spent, quotedIn, "getAmountIn != executed amountIn");
        assertEq(MockERC20(tokenOut).balanceOf(taker) - outBefore, amountOut, "exact-out delivered the wrong amount");
    }
}

// =========================================================================================
//  CLOSED — the view path can no longer be handed a size that leaves the shared domain,
//  because `refreshCapacity` refuses to create a pair where one exists.
// =========================================================================================

/// @notice `IPropPool.getAmountOut` is documented as never reverting ("Returns 0 rather than
///         reverting when the pool cannot fill ... Reverting here breaks batch quoting for
///         integrators, so don't"), and `PropPool._mulSat` restates it ("these run inside view
///         functions that are contractually forbidden from reverting").
///
///         `PropCurve.AmountOutOfDomain` used to break that. It is raised when a quote-denominated
///         leg exceeds `type(uint128).max`, and it sat on the view path via `_outFor`, so a quote
///         for an over-domain size reverted instead of returning zero. The bound itself is correct
///         and necessary — it is what keeps the Solidity and Rust domains identical — but on the
///         view path it had to become a zero.
///
///         It was reachable because nothing bounded the product: `bidCapacity` is a `uint96` and the
///         ladder prices are `uint56`, so at `priceScaleExp = 0` an output of `capacity * price`
///         reaches ~5.7e45 against a ~3.4e38 ceiling. This fixture's original configuration —
///         full-`uint96` capacity at exp 0 with a 7e16 ladder — was exactly that.
///
///         **The fix bounds capacity at write time instead of catching the error at read time.**
///         `refreshCapacity` now rejects `max(bidCapacity, askCapacity) * type(uint56).max >
///         MAX_AMOUNT_OUT * 10**priceScaleExp` with `CapacityOutOfDomain`. That is stronger than a
///         view-side catch: capacity, price and `priceScaleExp` have three different writers, and
///         `refreshCapacity` is the only one that can bound the product without reading another
///         actor's word, so it assumes the worst price the field can hold. The result is that
///         `amountOutBid(capacity, ...)` and `amountInAsk(capacity, ...)` are in-domain for *every*
///         ladder the updater can subsequently post — which is what lets `_outFor` and `_inFor`
///         probe the epoch's whole remaining depth with no `PropCurve` revert to escape.
///
///         So this suite no longer builds the poisoned pair. It asserts that the pair cannot be
///         built, that the bound is tight, and that at the largest capacity it does admit every view
///         returns a number.
contract PropPoolViewDomainTest is Test {
    PropPool internal pool;
    MockERC20 internal baseToken;
    MockERC20 internal quoteToken;

    address internal owner = makeAddr("owner");
    address internal manager = makeAddr("manager");
    address internal updater = makeAddr("updater");
    address internal guardian = makeAddr("guardian");

    /// @notice The largest capacity `refreshCapacity` admits at `priceScaleExp == 0`:
    ///         `floor((2**128 - 1) / (2**56 - 1))`.
    uint96 internal constant MAX_CAP_AT_EXP0 = 4_722_366_482_869_645_279_232;

    /// @dev Flat at 7e16, just under the `uint56` ceiling, with the strict `maxAsk > minBid` met by a
    ///      single unit. Unchanged from the pre-fix fixture — only the capacity moved.
    uint56 internal constant LADDER_PRICE = 7e16;

    function setUp() public {
        vm.warp(1_800_000_000);
        pool = new PropPool(owner, manager, updater, guardian);
        baseToken = new MockERC20("Base", "BASE", 18);
        quoteToken = new MockERC20("Quote", "QUOTE", 18);

        // priceScaleExp = 0: the pair is "1 base unit costs `price` quote units". Legal — the
        // constructor only rejects an exponent above 38 — and it is the configuration a
        // 1:1-decimals pair with a large integer price would naturally use. It is also the only
        // exponent at which the capacity bound bites at all: `10**8 > 2**152 / 2**128`, so from
        // `priceScaleExp >= 8` the bound exceeds `type(uint96).max` and rejects nothing.
        vm.prank(owner);
        pool.addPair(address(baseToken), address(quoteToken), 0, 3600, 1);

        uint256[] memory packed = new uint256[](1);
        packed[0] = uint256(LADDER_PRICE) | (uint256(LADDER_PRICE) << 56) | (uint256(LADDER_PRICE) << 112)
            | ((uint256(LADDER_PRICE) + 1) << 168) | (uint256(1) << 224);
        vm.prank(updater);
        pool.updateQuote(packed);

        vm.prank(updater);
        pool.refreshCapacity(1, MAX_CAP_AT_EXP0, MAX_CAP_AT_EXP0);

        // Inventory on both sides, and the quote side has to cover the whole epoch's leg
        // (~3.3e38 units), so the reserve-floor short circuit cannot hide the answer behind a
        // legitimate zero.
        baseToken.mint(manager, MAX_CAP_AT_EXP0);
        quoteToken.mint(manager, type(uint128).max);
        vm.startPrank(manager);
        baseToken.approve(address(pool), type(uint256).max);
        quoteToken.approve(address(pool), type(uint256).max);
        pool.deposit(address(baseToken), MAX_CAP_AT_EXP0);
        pool.deposit(address(quoteToken), type(uint128).max);
        vm.stopPrank();
    }

    /// @notice The configuration the defect needed is rejected at the source, and the bound is tight
    ///         rather than conservative: one unit past the admitted maximum is refused, and the
    ///         maximum itself is accepted.
    function test_capacityThatCouldLeaveTheDomainIsRejected() public {
        vm.startPrank(updater);

        vm.expectRevert(PropPool.CapacityOutOfDomain.selector);
        pool.refreshCapacity(1, type(uint96).max, type(uint96).max);

        vm.expectRevert(PropPool.CapacityOutOfDomain.selector);
        pool.refreshCapacity(1, MAX_CAP_AT_EXP0 + 1, 0);

        // The bound is on the LARGER of the two, so the ask side is checked too.
        vm.expectRevert(PropPool.CapacityOutOfDomain.selector);
        pool.refreshCapacity(1, 0, MAX_CAP_AT_EXP0 + 1);

        pool.refreshCapacity(1, MAX_CAP_AT_EXP0, MAX_CAP_AT_EXP0);
        vm.stopPrank();

        assertEq(uint256(pool.snapshot(1).bidCapacity), MAX_CAP_AT_EXP0, "the admitted maximum was not written");
    }

    /// @notice At the largest capacity the bound admits, the aggregator-facing views return numbers.
    ///         Zero for what the epoch cannot fill, a real quote for what it can, and no reverts.
    function test_BUG_viewQuoteRevertsInsteadOfReturningZero() public view {
        // The whole epoch is inside the shared domain — that is what the write-time bound buys.
        uint256 whole = pool.getAmountOut(address(baseToken), address(quoteToken), MAX_CAP_AT_EXP0);
        assertGt(whole, 0, "the whole epoch must quote");
        assertLe(whole, uint256(type(uint128).max), "the admitted maximum left the shared domain");

        // And the sizes that used to revert now read zero, including the one this test was named for.
        uint256 amountIn = uint256(type(uint96).max);
        assertEq(
            pool.getAmountOut(address(baseToken), address(quoteToken), amountIn),
            0,
            "the aggregator-facing view reverted instead of returning 0"
        );
        assertEq(pool.quoteByPair(1, true, amountIn), 0, "quoteByPair reverted instead of returning 0");
        assertEq(pool.quoteByPair(1, true, type(uint256).max), 0, "an absurd size must read 0");
        assertEq(pool.quoteByPair(1, false, type(uint256).max), 0, "an absurd ask size must read 0");
    }

    /// @notice The inverse view, which used to inherit the same revert through `_inFor`'s fillability
    ///         probe at `capacity - used`. That probe is now guaranteed in-domain, so it answers.
    function test_BUG_witness_viewQuoteRevertsWithAmountOutOfDomain() public view {
        assertEq(pool.getAmountIn(address(baseToken), address(quoteToken), 1), 1, "one quote unit costs one base unit");

        // A target past what the epoch can deliver is a zero, not a revert, at both extremes.
        assertEq(pool.getAmountIn(address(baseToken), address(quoteToken), type(uint256).max), 0, "absurd target");
        assertEq(pool.getAmountIn(address(quoteToken), address(baseToken), type(uint256).max), 0, "absurd ask target");
    }
}

// =========================================================================================
//  Coverage probe. An invariant suite that passes because the fuzzer never reached the
//  interesting states is worthless, and nothing in Foundry's output distinguishes the two.
//  This drives the handler directly with a fixed pseudo-random tape and prints what it hit.
// =========================================================================================

contract PropPoolCoverageProbeTest is PropPoolFixture {
    function setUp() public {
        _deploy();
    }

    function test_handlerReachesTheStatesTheInvariantsCareAbout() public {
        uint256 s = uint256(keccak256("dubu.coverage.tape"));
        for (uint256 i; i < 3000; ++i) {
            s = uint256(keccak256(abi.encode(s, i)));
            uint256 pick = s % 100;
            if (pick < 34) handler.swapExactIn(s >> 8, s >> 24, (s >> 4) & 1 == 0);
            else if (pick < 56) handler.swapExactOut(s >> 8, s >> 24, (s >> 4) & 1 == 0);
            else if (pick < 68) handler.swapPushed(s >> 8, s >> 24, (s >> 4) & 1 == 0);
            else if (pick < 78) handler.pushLadder(s >> 8, s >> 24, s >> 40);
            else if (pick < 86) handler.refreshCapacity(s >> 8, s >> 24);
            else if (pick < 90) handler.warp(s >> 8);
            else if (pick < 93) handler.nudgePause(s >> 8);
            else if (pick < 95) handler.nudgeGlobalPause(s >> 8);
            else if (pick < 97) handler.topUp(s >> 8, (s >> 4) & 1 == 0);
            else if (pick < 99) handler.donate(s >> 8, (s >> 4) & 1 == 0);
            else handler.syncReserves((s >> 4) & 1 == 0);
        }

        console2.log("exact-in fills                    ", handler.exactInFills());
        console2.log("exact-out fills                   ", handler.exactOutFills());
        console2.log("pushed (router-path) fills        ", handler.pushedFills());
        console2.log("refused attempts                  ", handler.blockedAttempts());
        console2.log("attempts while paused             ", handler.attemptsWhilePaused());
        console2.log("attempts while stale              ", handler.attemptsWhileStale());
        console2.log("fills on partly-consumed capacity ", handler.partialCapacityFills());
        console2.log("fills inside staleness window     ", handler.fillsInsideStalenessWindow());
        console2.log("view reverts (bug counter)        ", handler.viewReverts());

        assertGt(handler.exactInFills(), 100, "no exact-in coverage");
        assertGt(handler.exactOutFills(), 100, "no exact-out coverage");
        assertGt(handler.pushedFills(), 50, "no swapWithContractBalance coverage");
        assertGt(handler.blockedAttempts(), 100, "the pool never refused anything");
        assertGt(handler.attemptsWhilePaused(), 10, "never tried to trade a paused pair");
        assertGt(handler.attemptsWhileStale(), 10, "never tried to trade a stale pair");
        assertGt(handler.partialCapacityFills(), 100, "never filled against partly-used capacity");
        assertGt(handler.fillsInsideStalenessWindow(), 50, "never filled mid-staleness-window");

        // And every violation counter is still clean after 3000 driven actions.
        assertEq(handler.quoteDivergences(), 0, handler.note());
        assertEq(handler.inverseQuoteDivergences(), 0, handler.note());
        assertEq(handler.deliveryMismatches(), 0, handler.note());
        assertEq(handler.pausedFills(), 0, handler.note());
        assertEq(handler.staleFills(), 0, handler.note());
        assertEq(handler.ladderEscapes(), 0, handler.note());
        assertEq(handler.floorBreaches(), 0, handler.note());
    }
}

