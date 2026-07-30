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

contract PropPoolHandler is CommonBase, StdUtils {

    PropPool public immutable POOL;
    MockERC20 public immutable BASE_TOKEN;
    MockERC20 public immutable QUOTE_TOKEN;

    uint16 public constant PAIR_ID = 1;
    uint8 public constant PRICE_SCALE_EXP = 18;

    uint56 public constant MIN_PRICE = 1e9;
    uint32 public constant MAX_STALE_SECS = 60;
    uint96 public constant MIN_BASE_RESERVE = 50e18;
    uint96 public constant MIN_QUOTE_RESERVE = 50_000e6;

    address public immutable MANAGER;
    address public immutable UPDATER;
    address public immutable GUARDIAN;

    address[4] public actors;

    uint256 public quoteDivergences;

    uint256 public inverseQuoteDivergences;

    uint256 public deliveryMismatches;

    uint256 public pausedFills;

    uint256 public staleFills;

    uint256 public floorBreaches;

    uint256 public ladderEscapes;

    uint256 public viewPathDivergences;

    string public note;

    uint256 public exactInFills;
    uint256 public exactOutFills;
    uint256 public pushedFills;
    uint256 public blockedAttempts;
    uint256 public attemptsWhilePaused;
    uint256 public attemptsWhileStale;
    uint256 public partialCapacityFills;

    uint256 public fillsInsideStalenessWindow;

    uint256 public viewReverts;

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

    function refreshCapacity(uint256 bidSeed, uint256 askSeed) public {
        uint96 bidCap = uint96(_bound(bidSeed, 0, 2_000e18));
        uint96 askCap = uint96(_bound(askSeed, 0, 2_000e18));
        vm.prank(UPDATER);
        POOL.refreshCapacity(PAIR_ID, bidCap, askCap);
    }

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

    function warp(uint256 secSeed) public {
        vm.warp(block.timestamp + _bound(secSeed, 1, (3 * uint256(MAX_STALE_SECS)) / 2));
    }

    function topUp(uint256 seed, bool isBase) public {
        MockERC20 token = isBase ? BASE_TOKEN : QUOTE_TOKEN;
        uint256 amount = _bound(seed, 1, isBase ? 5_000e18 : 10_000_000e6);
        token.mint(MANAGER, amount);
        vm.startPrank(MANAGER);
        token.approve(address(POOL), amount);
        POOL.deposit(address(token), amount);
        vm.stopPrank();
    }

    function donate(uint256 seed, bool isBase) public {
        MockERC20 token = isBase ? BASE_TOKEN : QUOTE_TOKEN;
        token.mint(address(POOL), _bound(seed, 1, isBase ? 1e18 : 1e6));
    }

    function syncReserves(bool isBase) public {
        vm.prank(MANAGER);
        POOL.sync(address(isBase ? BASE_TOKEN : QUOTE_TOKEN));
    }

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

        try POOL.swap(
            address(a.tokenIn), address(a.tokenOut), int256(amountIn), 0, a.actor, 7, type(uint256).max
        ) returns (uint256 result) {
            exactInFills++;
            _closeExactIn(a, amountIn, result);
        } catch {
            blockedAttempts++;

            if (a.quoteOk && a.quoted != 0) {
                quoteDivergences++;
                _flag("exact-in: view promised a fill, swap reverted");
            }
        }
    }

    function swapExactOut(uint256 actorSeed, uint256 amountSeed, bool isBid) public {
        Attempt memory a = _open(actorSeed, isBid);
        uint256 amountOut = _pickAmountOut(amountSeed, isBid);
        if (amountOut == 0) return;

        a.tokenIn.mint(a.actor, _epochInputCeiling(isBid) + (isBid ? 1e18 : 1e6));
        _observe(a);

        (a.quoteOk, a.quoted) = _quoteAmountIn(a.tokenIn, a.tokenOut, amountOut);

        vm.prank(a.actor);

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

    function swapPushed(uint256 actorSeed, uint256 amountSeed, bool isBid) public {
        Attempt memory a = _open(actorSeed, isBid);
        a.tokenIn.mint(address(POOL), _pickAmountIn(amountSeed, isBid));
        _observe(a);

        uint256 amountIn = a.tokenIn.balanceOf(address(POOL)) - POOL.reserveOf(address(a.tokenIn));
        if (amountIn == 0) return;

        (a.quoteOk, a.quoted) = _quoteByPair(isBid, amountIn);

        vm.prank(a.actor);

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

    function _tokens(bool isBid) private view returns (MockERC20 tokenIn, MockERC20 tokenOut) {
        return isBid ? (BASE_TOKEN, QUOTE_TOKEN) : (QUOTE_TOKEN, BASE_TOKEN);
    }

    function _blocked(IPropPool.PairSnapshot memory snap) private view returns (bool paused, bool stale) {
        paused = POOL.allPaused() || (snap.flags & 1) != 0;
        stale = snap.updatedAt == 0 || uint256(snap.updatedAt) > block.timestamp
            || block.timestamp - uint256(snap.updatedAt) > uint256(snap.maxStaleSecs);
    }

    function _pickAmountIn(uint256 seed, bool isBid) private view returns (uint256) {
        IPropPool.PairSnapshot memory snap = POOL.snapshot(PAIR_ID);
        uint256 capacity = isBid ? uint256(snap.bidCapacity) : uint256(snap.askCapacity);
        uint256 used = snap.usedGen == snap.capGen ? (isBid ? uint256(snap.bidUsed) : uint256(snap.askUsed)) : 0;
        uint256 remaining = capacity > used ? capacity - used : 0;
        uint256 ceiling = isBid ? remaining : _askQuoteCeiling(snap, used, remaining);
        uint256 hi = ceiling == 0 ? (isBid ? 1e18 : 1e6) : ceiling + ceiling / 8 + 1;
        return _bound(seed, 1, hi);
    }

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

    function _askQuoteCeiling(IPropPool.PairSnapshot memory snap, uint256 used, uint256 remaining)
        private
        pure
        returns (uint256)
    {
        if (remaining == 0 || snap.maxAsk == 0) return 0;
        return PropCurve.amountInAsk(remaining, snap.minAsk, snap.maxAsk, snap.askCapacity, used, snap.priceScaleExp);
    }

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

        vm.prank(updater);
        pool.refreshCapacity(PAIR_ID, 1_000e18, 1_000e18);
    }

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

abstract contract PropPoolInvariantBase is PropPoolFixture {

    function invariant_reserveNeverBelowFloor() public view {
        assertGe(pool.reserveOf(address(baseToken)), MIN_BASE_RESERVE, "base reserve below floor");
        assertGe(pool.reserveOf(address(quoteToken)), MIN_QUOTE_RESERVE, "quote reserve below floor");
        assertGe(baseToken.balanceOf(address(pool)), MIN_BASE_RESERVE, "base balance below floor");
        assertGe(quoteToken.balanceOf(address(pool)), MIN_QUOTE_RESERVE, "quote balance below floor");
        assertEq(handler.floorBreaches(), 0, handler.note());
    }

    function invariant_usedNeverExceedsCapacity() public view {
        IPropPool.PairSnapshot memory snap = pool.snapshot(PAIR_ID);
        if (snap.usedGen == snap.capGen) {
            assertLe(snap.bidUsed, snap.bidCapacity, "bidUsed > bidCapacity");
            assertLe(snap.askUsed, snap.askCapacity, "askUsed > askCapacity");
        }
    }

    function invariant_reserveNeverExceedsBalance() public view {
        assertLe(pool.reserveOf(address(baseToken)), baseToken.balanceOf(address(pool)), "base over-accounted");
        assertLe(pool.reserveOf(address(quoteToken)), quoteToken.balanceOf(address(pool)), "quote over-accounted");
    }

    function invariant_pausedOrStaleNeverFills() public view {
        assertEq(handler.pausedFills(), 0, "a paused pair filled");
        assertEq(handler.staleFills(), 0, "a stale pair filled");
    }

    function invariant_ladderBoundsEveryFill() public view {
        assertEq(handler.ladderEscapes(), 0, handler.note());
    }

    function invariant_quoteEqualsExecution() public view {
        assertEq(handler.quoteDivergences(), 0, handler.note());
        assertEq(handler.inverseQuoteDivergences(), 0, handler.note());
        assertEq(handler.deliveryMismatches(), 0, handler.note());
        assertEq(handler.viewPathDivergences(), 0, handler.note());
    }
}

contract PropPoolInvariantTest is PropPoolInvariantBase {
    function setUp() public {
        _deploy();
        targetContract(address(handler));
    }
}

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

contract PropPoolGuardTest is PropPoolFixture {
    address internal taker = makeAddr("taker");

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

        vm.warp(block.timestamp + MAX_STALE_SECS);
        assertGt(pool.quoteByPair(PAIR_ID, true, 1e18), 0, "edge of window should still fill");
        assertGt(_swapBid(1e18), 0);

        vm.warp(block.timestamp + 1);
        assertEq(pool.quoteByPair(PAIR_ID, true, 1e18), 0, "past the cliff but still quoting");
        vm.expectRevert(PropPool.StaleQuote.selector);
        _swapBid(1e18);
    }

    function test_neverQuotedPairIsStaleNotFresh() public {
        MockERC20 other = new MockERC20("Other", "OTH", 18);
        vm.prank(owner);
        uint16 id = pool.addPair(address(other), address(quoteToken), 18, type(uint32).max, 1);
        assertEq(pool.quoteByPair(id, true, 1e18), 0, "a never-quoted pair must not look fresh");
        assertEq(pool.getAmountOut(address(other), address(quoteToken), 1e18), 0);
    }

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

        assertGe(partA + partB + 1, whole, "two-leg ask split lost more than one base unit");
    }

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

            assertEq(gain2, 0, "a 2-leg ask split still beat one shot");
            assertEq(gain10, 0, "a 10-leg ask split still beat one shot");
            assertEq(gain100, 0, "a 100-leg ask split still beat one shot");

            assertLe(loss2, 1, "2-leg residual above the n-1 unit bound");
            assertLe(loss10, 9, "10-leg residual above the n-1 unit bound");
            assertLe(loss100, 99, "100-leg residual above the n-1 unit bound");
        }

        _pushLadder(2e9, 5, 25);
        (, uint256 tight) = _askSplitDelta(100);
        _pushLadder(2e9, 5, 2000);
        (uint256 wideGain, uint256 wide) = _askSplitDelta(100);
        assertEq(wideGain, 0, "the widest ladder still leaks to splitting");
        assertLe(wide > tight ? wide - tight : tight - wide, 99, "the residual still tracks ladder width");
    }

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

    function testFuzz_quoteMatchesExecution(uint256 preFill, uint256 amountIn, uint32 age, bool isBid) public {
        age = uint32(bound(age, 0, MAX_STALE_SECS));
        uint256 ceiling = _maxAmountInFor(isBid);

        preFill = bound(preFill, 0, ceiling - 1);
        amountIn = bound(amountIn, 1, ceiling - preFill);

        (address tokenIn, address tokenOut) =
            isBid ? (address(baseToken), address(quoteToken)) : (address(quoteToken), address(baseToken));

        MockERC20(tokenIn).mint(taker, preFill + amountIn);

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

        assertLt(pool.quoteByPair(PAIR_ID, isBid, quotedIn - 1), amountOut, "inverse quote was not minimal");

        uint256 outBefore = MockERC20(tokenOut).balanceOf(taker);
        vm.prank(taker);
        uint256 spent = pool.swap(tokenIn, tokenOut, -int256(amountOut), type(uint256).max, taker, 0, type(uint256).max);

        assertEq(spent, quotedIn, "getAmountIn != executed amountIn");
        assertEq(MockERC20(tokenOut).balanceOf(taker) - outBefore, amountOut, "exact-out delivered the wrong amount");
    }
}

contract PropPoolViewDomainTest is Test {
    PropPool internal pool;
    MockERC20 internal baseToken;
    MockERC20 internal quoteToken;

    address internal owner = makeAddr("owner");
    address internal manager = makeAddr("manager");
    address internal updater = makeAddr("updater");
    address internal guardian = makeAddr("guardian");

    uint96 internal constant MAX_CAP_AT_EXP0 = 4_722_366_482_869_645_279_232;

    uint56 internal constant LADDER_PRICE = 7e16;

    function setUp() public {
        vm.warp(1_800_000_000);
        pool = new PropPool(owner, manager, updater, guardian);
        baseToken = new MockERC20("Base", "BASE", 18);
        quoteToken = new MockERC20("Quote", "QUOTE", 18);

        vm.prank(owner);
        pool.addPair(address(baseToken), address(quoteToken), 0, 3600, 1);

        uint256[] memory packed = new uint256[](1);
        packed[0] = uint256(LADDER_PRICE) | (uint256(LADDER_PRICE) << 56) | (uint256(LADDER_PRICE) << 112)
            | ((uint256(LADDER_PRICE) + 1) << 168) | (uint256(1) << 224);
        vm.prank(updater);
        pool.updateQuote(packed);

        vm.prank(updater);
        pool.refreshCapacity(1, MAX_CAP_AT_EXP0, MAX_CAP_AT_EXP0);

        baseToken.mint(manager, MAX_CAP_AT_EXP0);
        quoteToken.mint(manager, type(uint128).max);
        vm.startPrank(manager);
        baseToken.approve(address(pool), type(uint256).max);
        quoteToken.approve(address(pool), type(uint256).max);
        pool.deposit(address(baseToken), MAX_CAP_AT_EXP0);
        pool.deposit(address(quoteToken), type(uint128).max);
        vm.stopPrank();
    }

    function test_capacityThatCouldLeaveTheDomainIsRejected() public {
        vm.startPrank(updater);

        vm.expectRevert(PropPool.CapacityOutOfDomain.selector);
        pool.refreshCapacity(1, type(uint96).max, type(uint96).max);

        vm.expectRevert(PropPool.CapacityOutOfDomain.selector);
        pool.refreshCapacity(1, MAX_CAP_AT_EXP0 + 1, 0);

        vm.expectRevert(PropPool.CapacityOutOfDomain.selector);
        pool.refreshCapacity(1, 0, MAX_CAP_AT_EXP0 + 1);

        pool.refreshCapacity(1, MAX_CAP_AT_EXP0, MAX_CAP_AT_EXP0);
        vm.stopPrank();

        assertEq(uint256(pool.snapshot(1).bidCapacity), MAX_CAP_AT_EXP0, "the admitted maximum was not written");
    }

    function test_BUG_viewQuoteRevertsInsteadOfReturningZero() public view {

        uint256 whole = pool.getAmountOut(address(baseToken), address(quoteToken), MAX_CAP_AT_EXP0);
        assertGt(whole, 0, "the whole epoch must quote");
        assertLe(whole, uint256(type(uint128).max), "the admitted maximum left the shared domain");

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

    function test_BUG_witness_viewQuoteRevertsWithAmountOutOfDomain() public view {
        assertEq(pool.getAmountIn(address(baseToken), address(quoteToken), 1), 1, "one quote unit costs one base unit");

        assertEq(pool.getAmountIn(address(baseToken), address(quoteToken), type(uint256).max), 0, "absurd target");
        assertEq(pool.getAmountIn(address(quoteToken), address(baseToken), type(uint256).max), 0, "absurd ask target");
    }
}

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

        assertEq(handler.quoteDivergences(), 0, handler.note());
        assertEq(handler.inverseQuoteDivergences(), 0, handler.note());
        assertEq(handler.deliveryMismatches(), 0, handler.note());
        assertEq(handler.pausedFills(), 0, handler.note());
        assertEq(handler.staleFills(), 0, handler.note());
        assertEq(handler.ladderEscapes(), 0, handler.note());
        assertEq(handler.floorBreaches(), 0, handler.note());
    }
}
