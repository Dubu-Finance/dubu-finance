// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {IPropPool} from "./interfaces/IPropPool.sol";
import {IPyth, PythStructs} from "./interfaces/IPyth.sol";
import {PropCurve} from "./libraries/PropCurve.sol";
import {ReentrancyLock} from "./libraries/ReentrancyLock.sol";
import {SafeTransfer} from "./libraries/SafeTransfer.sol";

/// @dev The only ERC-20 method this contract calls that is not a transfer.
interface IERC20Balance {
    function balanceOf(address account) external view returns (uint256);
}

/// @title PropPool
/// @notice Single-contract proprietary AMM for GIWA (OP Stack, chain 91342).
///
/// The pool holds its own inventory and quotes a four-point price ladder per pair that an
/// off-chain engine pushes. On-chain there is no strategy: no reserves curve, no spread model,
/// no volatility term. Everything expressive is folded into `minBid/maxBid/minAsk/maxAsk`
/// off-chain and the chain only enforces coherence, freshness, capacity and inventory floors.
/// See `PropCurve` for the pricing itself and `context/archi_v2.md` §4 for why.
///
/// ## Storage shape (the part worth reading before changing anything)
///
/// Per pair there are exactly three hot words, and each is written by exactly one actor:
///
/// | word | writer            | bits                                                              |
/// |------|-------------------|-------------------------------------------------------------------|
/// | 0    | `updateQuote`     | 0..55 minBid, 56..111 maxBid, 112..167 minAsk, 168..223 maxAsk, 224..255 updatedAt |
/// | 1    | `refreshCapacity` | 0..95 bidCapacity, 96..191 askCapacity, 192..223 capGen, 224..239 flags |
/// | 2    | `swap`            | 0..95 bidUsed, 96..191 askUsed, 192..223 usedGen                   |
///
/// They are held as raw `uint256` words rather than packed structs so that the single-SSTORE
/// property of each writer is a fact about the source, not a hope about the optimiser. Bit
/// positions match the field order the design specifies (lowest bits = first field), so a
/// struct with that field order is a drop-in reinterpretation of the same word.
///
/// **Every capacity and usage field above is denominated in the pair's BASE token, on both
/// sides.** `askCapacity` used to be quote-denominated; `PropCurve`'s amendment 1 moved it to
/// base, because a quote-denominated ask made the output the reciprocal of the midpoint price and
/// `1/p` is convex, which made splitting an ask strictly dominant for the taker. The bit positions
/// did not move — only the unit — so the off-chain encoders are unchanged and every comparison
/// against a *quote* amount had to be rewritten. The base leg of a trade is `amountIn` for a bid
/// and `amountOut` for an ask; that is the amount capacity bounds and the amount `swap` adds to
/// the used counter.
///
/// ## Generation mechanism
///
/// `refreshCapacity` bumps `capGen`; `swap` stamps `usedGen = capGen`. The used counters are
/// only real while `usedGen == capGen`, otherwise they read as zero. This exists so a price
/// push costs one SSTORE and nothing else: zeroing the used counters on every quote update
/// would add a second write *and* hand an attacker a way to restore the pool's risk budget by
/// inducing price churn. Refilling capacity stays a separate, deliberate risk decision.
///
/// ## The reference-oracle deviation bound
///
/// Everything above enforces *coherence*: the ladder does not cross, it clears `minPrice`, it is
/// fresh, it has capacity behind it. None of that knows what the asset is worth. A feed that is
/// confidently wrong — a bad parse, a stale socket that keeps serving, an exchange printing
/// garbage through an outage — produces a perfectly coherent ladder at the wrong level, and the
/// first informed taker collects the difference against real inventory. So does a leaked updater
/// key, which is the same failure with intent behind it.
///
/// The bound is an independent price, checked on chain, from Pyth.
///
/// **Pyth is a BOUND, never a price source, and getting this backwards is the single most likely
/// misreading of this file.** The pool does not price from Pyth and it does not blend Pyth into
/// the ladder. If it priced from Pyth and validated against Pyth the check would compare a number
/// with itself and pass unconditionally — it would cost gas and prove nothing. Fair value comes
/// from the off-chain engine's own feeds; Pyth's only job is to refuse a ladder that has drifted
/// implausibly far from a reference the updater does not control. Independence is the entire
/// mechanism. If you ever find yourself reading `_referencePrice` into the ladder rather than
/// comparing the ladder against it, stop.
///
/// ### What is bounded, and why those two numbers
///
/// Exactly two of the four ladder prices, the two at the taker-favourable end:
///
///   * `maxBid` — the highest price the pool will ever PAY for base — must be at or below
///     `ref * (1 + maxDeviationBps)`.
///   * `minAsk` — the lowest price the pool will ever ACCEPT for base — must be at or above
///     `ref * (1 - maxDeviationBps)`.
///
/// These are the two loss-bearing prices, and they are the only two. `minBid` and `maxAsk` sit at
/// the far end of the ladder, where the pool pays less and charges more, so widening the ladder
/// only ever moves them in the pool's favour; bounding them would price the pool's own *width*
/// against the oracle and force the operator to keep `maxDeviationBps` above whatever spread the
/// desk happens to be quoting. Bounding the mid instead is what `archi_v2.md` §4.4 sketches, and
/// it implies these two only via `validateLadder`'s non-crossing rule — true today, but a
/// second-hand guarantee that lives in another library. The direct form states the economic claim
/// itself: never buy above the reference by more than the tolerance, never sell below it by more.
///
/// ### Where the check lives: `updateQuote`, not `swap`
///
/// Deliberate, and the alternative is not obviously worse, so here is the reasoning.
///
/// Checking at push time rejects a bad ladder at the boundary where the untrusted actor writes.
/// The threat model is a wrong or compromised updater, and `updateQuote` is the *only* write that
/// actor has; bounding the write bounds the threat at its source. It also leaves `swap` — the path
/// takers pay for and aggregators route on — at exactly the gas it had before.
///
/// ### What it costs, measured
///
/// Every figure below comes from `test/PropPoolOracle.t.sol`, which prints them, and is measured
/// **cold**: `vm.cool` before each call, a genuinely different ladder each time (re-pushing an
/// identical word collapses the SSTORE to 100 gas and flatters the result by 2,800), and a dirty
/// quote slot so the write is 2,900 rather than 22,100 — the steady state for a bot pushing every
/// block. It is not cheap and this table is not arranged to make it look cheap.
///
///   | call                                         | gas    | delta   |
///   |----------------------------------------------|--------|---------|
///   | `updateQuote`, 1 pair, no feed configured    | 15,178 | —       |
///   | `updateQuote`, 1 pair, bounded               | 34,175 | +18,997 |
///   | `updateQuote`, 2 pairs, both bounded         | 60,944 | +26,769 for the second |
///   | `updateQuote`, 1 pair, pool never uses Pyth  | 15,067 | (+111 to carry the feature unused) |
///   | `swap`, exact-in bid                         | 19,164 | **+0**  |
///   | `getAmountOut`                               |  6,085 | **+0**  |
///
/// So the bound roughly doubles a push and leaves swaps and quotes untouched. Read plainly: this
/// is **outside** the ~20k budget `updateQuote` was designed to, and it is being accepted anyway,
/// because the gas is paid by us on a key we control rather than by takers, and because the thing
/// it buys is the difference between a coherent ladder and a correct one.
///
/// Of the +18,997, roughly 6,800 is ours (three cold SLOADs — both oracle words and the `pyth`
/// address — plus the cold account access) and the rest is inside Pyth. The mock reads a whole
/// five-word `PriceFeed`; the real contract's `PriceInfo` is packed tighter, so the on-chain
/// figure should come in somewhat *below* this. That is the direction an estimate should err in,
/// but it has not been measured against the live contract and should not be quoted as if it had.
///
/// Two numbers are worth keeping in view when tuning. The marginal cost of a second bounded pair
/// is 26,769, not 18,997 — only the Pyth account access and the `pyth` SLOAD amortise across a
/// batch, and each feed's own storage is cold per feed. And a pair with no feed costs **+111
/// gas**: one cold `feedId` SLOAD that short-circuits before any external call, which is what
/// makes it viable to batch unbounded and bounded markets together.
///
/// Checking at swap time would catch a different failure — the ladder was right when pushed and
/// Pyth has since moved — and three things argue against it:
///
///  1. It would have to be duplicated into the views. A `swap` that reverts on a bound the quote
///     path did not apply means `getAmountOut` lies, aggregators route into reverts, and the pool
///     is dropped from their books. So the external call lands in the batch-quote path, which is
///     precisely the thing `_reserve` exists to keep out of it (see that comment: the view path
///     must not make an external call that a hostile or unavailable callee can revert to poison an
///     aggregator's whole multicall). Pyth is that callee.
///  2. Post-push drift is already `maxStaleSecs`'s job, and §4.4 chose it deliberately as a hard
///     cliff. A swap-time oracle check is a second, weaker freshness mechanism layered on the
///     first, and its failure mode is worse than the first's.
///  3. It hands Pyth's liveness a veto over every trade. Pyth is a pull oracle: if nobody pushes,
///     it goes stale, and with a swap-time check every swap in the pool reverts because of a third
///     party's inaction.
///
/// The coverage gap is smaller than it looks, and it is worth being precise about. The updater
/// heartbeats far below `maxStaleSecs` (the engine caps its heartbeat at `maxStaleSecs * 4 / 5`
/// and is actually driven by `newHeads`, ~1s), and *every* heartbeat re-runs this check. So a live
/// ladder is re-bounded against Pyth about once a second, not once per `maxStaleSecs`. What
/// push-time checking genuinely does not cover is the interval between the last successful push
/// and the staleness cliff — a ladder that stops being re-pushed while the market moves under it.
/// That window is bounded by `maxStaleSecs`, which is the manager's dial, and it is the same
/// window the design already accepted for every other reason.
///
/// ### When Pyth is unavailable: fail closed
///
/// If a pair has a feed configured and the reference cannot be established — Pyth reverts, the
/// feed was never populated, the price is stale by our own window, the price is non-positive, the
/// scaling is out of range — `updateQuote` **reverts**. It does not fall through to the
/// unbounded path.
///
/// What that trades away, stated plainly: anyone who can stall Pyth can stop us posting new
/// ladders, and after `maxStaleSecs` the pool stops quoting. That is a denial of service, and it
/// is real. The alternative is worse in kind rather than in degree: falling through means an
/// attacker who can stall Pyth has *removed the bound*, and stalling it is exactly what they would
/// do first if they held the updater key. Fail-open converts a liveness attack into a solvency
/// attack. Fail-closed converts it into an outage.
///
/// The outage is also not a wedge, which is the property that makes it acceptable. Nothing else in
/// the pool depends on Pyth: `pause`, `refreshCapacity`, `withdraw`, `deposit`, `sync` and every
/// view keep working, in-flight ladders keep trading until they go stale, and the **manager** — not
/// the owner, not a timelock — can set `feedId` to zero and restore pushes in one transaction if
/// Pyth is durably down. Recovery does not require the slowest role.
///
/// ## Roles
///
/// - `owner`    — rotates every role, adds pairs, withdraws inventory, sets the Pyth address.
///                Destined for a timelock.
/// - `manager`  — per-pair configuration, including the whole reference bound, and inventory
///                top-ups.
/// - `updater`  — `updateQuote` / `refreshCapacity` and nothing else. **Hard invariant: no
///                function reachable by the updater transfers a token or touches `_reserve`.**
///                This key signs many times a minute from a hot process; assume it leaks.
///                It can still cause loss by posting bad-but-coherent prices, and bounding *that*
///                is the reference bound above. Note which role owns which dial: the updater
///                cannot set a feed id, a deviation limit, a staleness window or the Pyth address,
///                so it cannot widen its own leash — only push ladders that fit inside it.
/// - `guardian` — pause only, held on separate hardware from the updater.
contract PropPool is IPropPool {
    using SafeTransfer for address;

    // ---------------------------------------------------------------------
    // Errors
    // ---------------------------------------------------------------------

    error NotOwner();
    error NotManager();
    error NotUpdater();
    error NotGuardian();
    error ZeroAddress();
    error IdenticalTokens();
    error PairExists();
    error UnknownPair();
    error PriceScaleTooLarge();
    error ZeroMinPrice();
    error ZeroStaleWindow();
    error MinPriceStrandsQuote();
    error DeadlineExpired();
    error ZeroAmount();
    error AmountOverflow();
    error PoolPaused();
    error StaleQuote();
    error InsufficientCapacity();
    error ReserveFloorBreached();
    error SlippageExceeded();
    error ZeroOutput();
    error LengthMismatch();
    error CapacityOutOfDomain();
    error PythNotSet();
    error DeviationTooLarge();
    error ZeroPythStaleWindow();

    /// @dev The ladder would have the pool BUY base above `referencePrice * (1 + maxDeviationBps)`.
    ///      Carries the reference it was judged against so the updater's logs record what the
    ///      chain saw, not just what the bot sent.
    error BidCeilingExceeded(uint16 pairId, uint256 referencePrice, uint256 maxBid);
    /// @dev The ladder would have the pool SELL base below `referencePrice * (1 - maxDeviationBps)`.
    error AskFloorBreached(uint16 pairId, uint256 referencePrice, uint256 minAsk);
    /// @dev A feed is configured but no reference could be established. `status` is one of the
    ///      `REF_*` codes, so the revert says *which* way the oracle failed.
    error ReferenceUnavailable(uint16 pairId, uint8 status);

    // ---------------------------------------------------------------------
    // Additional events (the integration-visible ones live in IPropPool)
    // ---------------------------------------------------------------------

    event OwnershipTransferStarted(address indexed previousOwner, address indexed newOwner);
    event OwnershipTransferred(address indexed previousOwner, address indexed newOwner);
    event ManagerUpdated(address indexed previousManager, address indexed newManager);
    event UpdaterUpdated(address indexed previousUpdater, address indexed newUpdater);
    event GuardianUpdated(address indexed previousGuardian, address indexed newGuardian);
    event PairConfigUpdated(
        uint16 indexed pairId, uint32 maxStaleSecs, uint56 minPrice, uint96 minBaseReserve, uint96 minQuoteReserve
    );
    event ReserveSynced(address indexed token, uint256 reserve);
    event PythUpdated(address indexed previousPyth, address indexed newPyth);
    event PairOracleUpdated(
        uint16 indexed pairId, bytes32 feedId, uint16 maxDeviationBps, uint32 maxPythStaleSecs, int8 refExpo
    );

    // ---------------------------------------------------------------------
    // Bit layout constants
    // ---------------------------------------------------------------------

    uint256 private constant MASK_16 = 0xffff;
    uint256 private constant MASK_32 = 0xffffffff;
    uint256 private constant MASK_56 = 0xffffffffffffff;
    uint256 private constant MASK_96 = 0xffffffffffffffffffffffff;

    /// @dev Widest price the quote word can carry. Every price field is `uint56`, so this is the
    ///      worst case `_refreshCapacity` has to assume when it bounds capacity against
    ///      `PropCurve.MAX_AMOUNT_OUT` — prices arrive from a different writer and a different
    ///      transaction, so the capacity check cannot look at the ladder that happens to be stored.
    uint256 private constant MAX_PRICE = MASK_56;

    /// @dev Bits 0..223 of a quote word: the four ladder prices, exactly as they arrive in
    ///      `updateQuote` calldata. The calldata layout was chosen to coincide with the storage
    ///      layout so the update is a mask-and-or, not a re-pack.
    uint256 private constant LADDER_MASK = (uint256(1) << 224) - 1;

    uint256 private constant SHIFT_MAX_BID = 56;
    uint256 private constant SHIFT_MIN_ASK = 112;
    uint256 private constant SHIFT_MAX_ASK = 168;
    uint256 private constant SHIFT_UPDATED_AT = 224;

    uint256 private constant SHIFT_ASK_CAPACITY = 96;
    uint256 private constant SHIFT_GEN = 192;
    uint256 private constant SHIFT_FLAGS = 224;

    /// @dev flags bit 0 (word bit 224) = paused. Bits 1..14 are reserved; the natural next
    ///      occupants are `bidDisabled` / `askDisabled` for one-sided withdrawal.
    ///
    ///      **Bit 15 is not a stored flag and never will be.** `snapshot` synthesizes it from the
    ///      oracle config to report whether the pair is bounded — see `FLAG_SNAPSHOT_BOUNDED`. It
    ///      is carved out of the top of the range rather than taken from the bottom precisely so
    ///      that a future stored flag can claim bit 1 without colliding with it.
    uint256 private constant FLAG_PAUSED = uint256(1) << SHIFT_FLAGS;

    /// @notice `snapshot().flags` bit 15: this pair has a reference-oracle feed configured.
    /// @dev Derived at read time, never stored. Zero means the pair quotes unbounded, which is a
    ///      legitimate configuration (see `setPairOracle`) and one an integrator should be able to
    ///      see without a second call. `pairOracle(pairId)` is the authoritative, detailed answer.
    uint16 internal constant FLAG_SNAPSHOT_BOUNDED = uint16(1) << 15;

    /// @dev `_route` value: bits 0..15 pairId (1-based), bit 16 set when `tokenIn` is the base.
    ///      A zero value therefore means "no such route" even for an ask on pair 1, which is why
    ///      pair ids start at 1 — `pairIdFor` has no third return value to signal "not found".
    uint256 private constant ROUTE_IS_BID = uint256(1) << 16;

    // ---------------------------------------------------------------------
    // Types
    // ---------------------------------------------------------------------

    /// @notice Cold per-pair configuration. Kept in its own mapping so that nothing the manager
    ///         touches shares a slot with anything the updater or a swapper touches.
    /// @dev Field order is load-bearing: `quote | priceScaleExp | maxStaleSecs | minPrice` is
    ///      160 + 8 + 32 + 56 = 256 bits exactly, so a swap reads the entire freshness-and-scale
    ///      set in one SLOAD.
    struct PairConfig {
        address base;
        address quote;
        uint8 priceScaleExp;
        uint32 maxStaleSecs;
        uint56 minPrice;
        uint96 minBaseReserve;
        uint96 minQuoteReserve;
        bool exists;
    }

    /// @dev Everything the pricing path needs, resolved for one direction, in memory.
    ///      `pLow`/`pHigh` are `(minBid, maxBid)` for a bid and `(minAsk, maxAsk)` for an ask —
    ///      in both cases the price at zero usage is the taker-favourable end, so the curve
    ///      argument order is identical and the two sides share one code path.
    ///
    ///      `capacity` and `used` are BASE units on both sides. `reserveOut`/`minReserveOut` are in
    ///      `tokenOut`, which is the quote token for a bid and the base token for an ask — so on the
    ///      ask side they are directly comparable with `capacity`, and on the bid side they are not.
    struct PairState {
        address tokenOut;
        uint8 priceScaleExp;
        uint256 pLow;
        uint256 pHigh;
        uint256 capacity;
        uint256 used;
        uint256 reserveOut;
        uint256 minReserveOut;
    }

    /// @notice Per-pair reference-oracle configuration. Manager-set, updater-invisible.
    ///
    /// @dev Field order is load-bearing in the same way `PairConfig`'s is, but for a different
    ///      reason: `feedId` is a full 32-byte word and therefore owns slot 0 alone, while the
    ///      three small fields pack into slot 1. `_referencePrice` reads `feedId` first and
    ///      returns before touching slot 1 when it is zero, so an unbounded pair costs exactly one
    ///      cold SLOAD on the hot push path and never makes the external call.
    ///
    ///      Kept in its own mapping rather than folded into `PairConfig` so that adding a fourth
    ///      manager-owned word does not disturb the single-SLOAD freshness-and-scale read that
    ///      `_load` depends on.
    struct PairOracle {
        /// @notice Pyth price feed id. **Zero disables the bound for this pair entirely.**
        /// @dev Not every listable asset has a Pyth feed, and a pair that cannot be configured is
        ///      a pair that cannot be listed. Zero is a supported production state, not a
        ///      placeholder — it is reported in `snapshot().flags` bit 15 so an integrator can
        ///      tell a bounded pair from an unbounded one.
        bytes32 feedId;
        /// @notice How far the ladder's taker-favourable ends may sit from the reference, in bps.
        /// @dev Capped at `BPS` (100%). At exactly `BPS` the ask floor degenerates to zero and the
        ///      bid ceiling to `2 * ref`, which is a legitimate "very loose" setting; above it the
        ///      floor arithmetic would underflow, so it is rejected rather than clamped.
        uint16 maxDeviationBps;
        /// @notice Tolerated distance between Pyth's `publishTime` and `block.timestamp`.
        /// @dev Ours, not Pyth's. `getValidTimePeriod` is the oracle's own advisory number; the
        ///      tolerable staleness of a *bound* is a property of our risk model. Compared as an
        ///      absolute difference, so a future-dated price is stale in the same way a past-dated
        ///      one is — a reference that has not been observed yet is not a reference.
        uint32 maxPythStaleSecs;
        /// @notice Decimal shift from Pyth's own scale into this pair's `priceScaleExp` scale.
        ///
        /// @dev This is the number to get right, and it is the number a reviewer should check
        ///      first. The pool's price convention is
        ///      `quoteAmount = baseAmount * price / 10**priceScaleExp`, so one whole base token
        ///      fetches `10**baseDecimals * price / 10**priceScaleExp` quote units, which is
        ///      `humanPrice * 10**quoteDecimals` quote units. Solving,
        ///
        ///          poolPrice = humanPrice * 10**(priceScaleExp + quoteDecimals - baseDecimals)
        ///
        ///      and Pyth reports `humanPrice = pythPrice * 10**pythExpo`. Hence
        ///
        ///          refExpo   = priceScaleExp + quoteDecimals - baseDecimals
        ///          poolPrice = pythPrice * 10**(pythExpo + refExpo)
        ///
        ///      `pythExpo` is read from the response every time, never hardcoded: it is per-feed
        ///      and Pyth can change it. `refExpo` is the part that is fixed by the pair's own
        ///      decimals and its immutable `priceScaleExp`, so it is stored.
        ///
        ///      Worked, for the two live markets:
        ///
        ///        | market      | base/quote dp | priceScaleExp | refExpo | pythExpo | net |
        ///        |-------------|---------------|---------------|---------|----------|-----|
        ///        | mWETH/mUSDC | 18 / 6        | 24            | +12     | -8       | +4  |
        ///        | mWBTC/mUSDC | 8 / 6         | 12            | +10     | -8       | +2  |
        ///
        ///      ETH at $2,000 arrives as `2e11` and scales to `2e15`, which is exactly the pool
        ///      mid the deploy script encodes. BTC at $100,000 arrives as `1e13` and scales to
        ///      `1e15`, likewise.
        ///
        ///      A wrong `refExpo` fails **closed**, in both directions: too small and the
        ///      reference collapses far below the ladder, tripping `BidCeilingExceeded`; too large
        ///      and it explodes above it, tripping `AskFloorBreached` or `REF_INVALID`. Either way
        ///      the very first push after a misconfiguration reverts loudly instead of quietly
        ///      bounding against nonsense. It is signed because a pair with more base decimals
        ///      than `priceScaleExp + quoteDecimals` legitimately needs a negative shift.
        int8 refExpo;
    }

    uint256 private constant STATUS_OK = 0;
    uint256 private constant STATUS_UNKNOWN = 1;
    uint256 private constant STATUS_PAUSED = 2;
    uint256 private constant STATUS_STALE = 3;

    // ---------------------------------------------------------------------
    // Reference-bound constants and status codes
    // ---------------------------------------------------------------------

    uint256 private constant BPS = 10_000;

    /// @notice `referencePrice` succeeded; the returned price is usable as a bound.
    uint8 public constant REF_OK = 0;
    /// @notice No feed id configured. The pair quotes unbounded, on purpose.
    uint8 public constant REF_DISABLED = 1;
    /// @notice The Pyth address is unset, has no code, or the call reverted.
    uint8 public constant REF_UNAVAILABLE = 2;
    /// @notice A price exists but is outside `maxPythStaleSecs` of now, in either direction.
    uint8 public constant REF_STALE = 3;
    /// @notice The price is non-positive, or the scaling puts it outside a representable range.
    uint8 public constant REF_INVALID = 4;

    /// @dev Largest scaled reference the bound will accept.
    ///
    ///      Ladder prices are `uint56`, so the widest one is ~7.2e16. A reference above
    ///      `type(uint64).max` (~1.8e19) is more than 250x that, which no market move produces
    ///      against a pair whose own ladder still fits the field — it describes a broken `refExpo`
    ///      or a broken feed, and it is rejected as `REF_INVALID` rather than silently failing
    ///      every ladder as "out of bound", because those are different operator problems.
    ///
    ///      It is also what keeps `ref * (BPS + maxDeviationBps)` far from overflow: at most
    ///      `1.8e19 * 20000 = 3.7e23`, against a `uint256` ceiling of ~1.2e77.
    uint256 private constant MAX_REFERENCE_PRICE = type(uint64).max;

    /// @dev Bound on `pythExpo + refExpo` before the power-of-ten is formed. `10**58` multiplied
    ///      by the widest `int64` price (~9.2e18) is ~9.2e76, still inside `uint256`; anything
    ///      beyond would overflow before `MAX_REFERENCE_PRICE` could reject it. Real values are
    ///      +4 and +2, so this only ever fires on a misconfiguration or a feed with an absurd
    ///      exponent.
    int256 private constant MAX_NET_EXPONENT = 58;

    // ---------------------------------------------------------------------
    // Storage
    // ---------------------------------------------------------------------

    address public owner;
    /// @notice Number of pairs. Ids are dense in `[1, pairCount]`, so this doubles as the
    ///         existence test and lets `updateQuote` validate a whole batch off one warm SLOAD
    ///         instead of reading `PairConfig.exists` per entry.
    uint16 public pairCount;

    address public pendingOwner;
    address public manager;
    address public updater;

    address public guardian;
    /// @notice Global kill switch. Shares a slot with `guardian`: both are written only in the
    ///         emergency path, and `swap` reads the pair word for the per-pair flag anyway.
    bool public allPaused;

    /// @notice The reference oracle. Zero until the owner sets it, after which it can be rotated
    ///         but not unset.
    /// @dev Storage rather than `immutable` for two reasons, and cheapness is not one of them.
    ///      First, the constructor signature is part of the deployment surface and every script
    ///      and test that builds a pool calls it; adding a fifth argument to buy a read that
    ///      happens at most once per `updateQuote` is a bad trade. Second, the oracle root of
    ///      trust is exactly the kind of thing that should be rotatable by the timelock rather
    ///      than frozen at deploy — Pyth's own address is a proxy, but the pool should not have to
    ///      be redeployed if that ever stops being true.
    ///
    ///      Read lazily, inside `_referencePrice`, and never before `feedId` has proved non-zero.
    ///      A pool with no bounded pairs therefore never touches this slot at all.
    address public pyth;

    mapping(uint16 => PairConfig) private _config;
    mapping(uint16 => PairOracle) private _oracle;
    mapping(uint16 => uint256) private _quoteWord;
    mapping(uint16 => uint256) private _capacityWord;
    mapping(uint16 => uint256) private _usedWord;

    /// @notice `tokenIn => tokenOut => (pairId | isBid flag)`. Both orderings are written at
    ///         `addPair` so quoting is one SLOAD with no sorting and no hashing.
    mapping(address => mapping(address => uint256)) private _route;

    /// @notice Accounted inventory per token.
    /// @dev Deliberately *not* `balanceOf`. Three reasons: (1) the reserve floor should not be
    ///      satisfiable by a donation the pool cannot rely on, (2) the view path must not make
    ///      an external call that a hostile token could revert to poison an aggregator's batch,
    ///      (3) `swapWithContractBalance` needs a baseline to measure the caller's push against.
    ///      Donations and rebases are folded in explicitly by `sync`.
    mapping(address => uint256) private _reserve;

    // ---------------------------------------------------------------------
    // Modifiers
    // ---------------------------------------------------------------------

    modifier onlyOwner() {
        if (msg.sender != owner) revert NotOwner();
        _;
    }

    modifier onlyManager() {
        if (msg.sender != manager) revert NotManager();
        _;
    }

    modifier onlyUpdater() {
        if (msg.sender != updater) revert NotUpdater();
        _;
    }

    modifier onlyGuardian() {
        if (msg.sender != guardian) revert NotGuardian();
        _;
    }

    /// @dev Transient lock. Every guarded body must fall through to the release — no early
    ///      `return` inside one of these, or the pool stays locked for the rest of the tx.
    modifier locked() {
        ReentrancyLock.acquire();
        _;
        ReentrancyLock.release();
    }

    // ---------------------------------------------------------------------
    // Construction
    // ---------------------------------------------------------------------

    constructor(address owner_, address manager_, address updater_, address guardian_) {
        if (owner_ == address(0) || manager_ == address(0) || updater_ == address(0) || guardian_ == address(0)) {
            revert ZeroAddress();
        }
        owner = owner_;
        manager = manager_;
        updater = updater_;
        guardian = guardian_;
        emit OwnershipTransferred(address(0), owner_);
        emit ManagerUpdated(address(0), manager_);
        emit UpdaterUpdated(address(0), updater_);
        emit GuardianUpdated(address(0), guardian_);
    }

    // ---------------------------------------------------------------------
    // Role rotation — all four independent, all owner-driven
    // ---------------------------------------------------------------------

    /// @notice Two-step so a typo in the timelock address cannot orphan the pool. The other
    ///         three roles are one-step: they are recoverable by the owner, ownership is not.
    function transferOwnership(address newOwner) external onlyOwner {
        pendingOwner = newOwner;
        emit OwnershipTransferStarted(owner, newOwner);
    }

    function acceptOwnership() external {
        address pending = pendingOwner;
        if (msg.sender != pending) revert NotOwner();
        emit OwnershipTransferred(owner, pending);
        owner = pending;
        pendingOwner = address(0);
    }

    function setManager(address newManager) external onlyOwner {
        if (newManager == address(0)) revert ZeroAddress();
        emit ManagerUpdated(manager, newManager);
        manager = newManager;
    }

    /// @notice Rotating the updater is the leak-response path and must be a single cheap tx.
    function setUpdater(address newUpdater) external onlyOwner {
        if (newUpdater == address(0)) revert ZeroAddress();
        emit UpdaterUpdated(updater, newUpdater);
        updater = newUpdater;
    }

    function setGuardian(address newGuardian) external onlyOwner {
        if (newGuardian == address(0)) revert ZeroAddress();
        emit GuardianUpdated(guardian, newGuardian);
        guardian = newGuardian;
    }

    // ---------------------------------------------------------------------
    // Pair administration
    // ---------------------------------------------------------------------

    /// @notice Register a new (base, quote) pair and return its id.
    /// @dev Rejects both token orderings so `WETH/USDC` and `USDC/WETH` can never coexist as
    ///      two pairs with independent, mutually arbitrageable ladders.
    /// @param priceScaleExp decimal alignment: `quoteAmount = baseAmount * price / 10**exp`.
    ///                      Immutable afterwards — changing it silently reprices every stored
    ///                      ladder by orders of magnitude, so a new pair is the honest path.
    ///
    ///                      **Pick the largest exponent that keeps the pair's prices inside
    ///                      `uint56`, not the smallest that works.** It is a gas parameter as well
    ///                      as a precision one, because the two directions the curve solves by
    ///                      bisection converge in about `log2(size / price)` steps: the finer the
    ///                      price, the fewer steps. Measured on the same WETH/USDC pair, whole swap,
    ///                      first touch in the tx:
    ///
    ///                        | exp | exact-in ask | exact-out bid |
    ///                        |-----|--------------|---------------|
    ///                        | 18  | 151,043      | 143,585       |
    ///                        | 24  | 119,369      | 116,789       |
    ///                        | 25  | 114,719      | 112,638       |
    ///
    ///                      The bid exact-in (99,331) and ask exact-out (101,748) legs are closed
    ///                      forms and do not move with the exponent. `priceScaleExp` also interacts
    ///                      with the capacity ceiling `refreshCapacity` enforces, though only below
    ///                      8, where no realistic capacity reaches it.
    /// @param minPrice      absolute floor on `minBid`, independent of any oracle. Must be
    ///                      non-zero: a zero floor lets a compromised updater post a zero bid,
    ///                      and it is also what keeps `minAsk > 0` and therefore keeps the
    ///                      inverse curve well defined.
    function addPair(address base, address quote, uint8 priceScaleExp, uint32 maxStaleSecs, uint56 minPrice)
        external
        onlyOwner
        returns (uint16 pairId)
    {
        if (base == address(0) || quote == address(0)) revert ZeroAddress();
        if (base == quote) revert IdenticalTokens();
        if (priceScaleExp > PropCurve.MAX_PRICE_SCALE_EXP) revert PriceScaleTooLarge();
        if (minPrice == 0) revert ZeroMinPrice();
        if (maxStaleSecs == 0) revert ZeroStaleWindow();
        if (_route[base][quote] != 0 || _route[quote][base] != 0) revert PairExists();

        pairId = ++pairCount;

        _config[pairId] = PairConfig({
            base: base,
            quote: quote,
            priceScaleExp: priceScaleExp,
            maxStaleSecs: maxStaleSecs,
            minPrice: minPrice,
            minBaseReserve: 0,
            minQuoteReserve: 0,
            exists: true
        });

        _route[base][quote] = uint256(pairId) | ROUTE_IS_BID;
        _route[quote][base] = uint256(pairId);

        emit PairAdded(pairId, base, quote);
    }

    /// @notice Update the mutable half of a pair's configuration.
    /// @dev Raising `minPrice` above a ladder that is already stored would leave the pair in a
    ///      state where every quote is unfillable until the next push, and — worse — where the
    ///      floor silently fails to describe the price the pool is actually willing to pay.
    ///      Reverting forces the operator to sequence it correctly: push a compliant ladder
    ///      first, raise the floor second.
    function setPairConfig(
        uint16 pairId,
        uint32 maxStaleSecs,
        uint56 minPrice,
        uint96 minBaseReserve,
        uint96 minQuoteReserve
    ) external onlyManager {
        if (pairId == 0 || pairId > pairCount) revert UnknownPair();
        if (minPrice == 0) revert ZeroMinPrice();
        if (maxStaleSecs == 0) revert ZeroStaleWindow();

        uint256 word = _quoteWord[pairId];
        // `word == 0` means no ladder has ever been posted, so there is nothing to strand.
        if (word != 0 && (word & MASK_56) < minPrice) revert MinPriceStrandsQuote();

        PairConfig storage cfg = _config[pairId];
        cfg.maxStaleSecs = maxStaleSecs;
        cfg.minPrice = minPrice;
        cfg.minBaseReserve = minBaseReserve;
        cfg.minQuoteReserve = minQuoteReserve;

        emit PairConfigUpdated(pairId, maxStaleSecs, minPrice, minBaseReserve, minQuoteReserve);
    }

    // ---------------------------------------------------------------------
    // Reference-oracle administration
    //
    // Owner sets the oracle contract; manager sets the per-pair terms. The updater appears
    // nowhere in this section, which is the point of it: a key that could raise its own
    // `maxDeviationBps`, lengthen its own `maxPythStaleSecs` or zero its own `feedId` would be
    // bounded by nothing at all, and the bound exists precisely because that key is hot.
    // ---------------------------------------------------------------------

    /// @notice Point the pool at a Pyth deployment.
    /// @dev Owner-gated because this is a root of trust, not a risk parameter: whoever sets it
    ///      decides what "independent" means for every bounded pair at once. On GIWA Sepolia the
    ///      argument is the genesis pre-install `0x2880aB155794e7179c9eE2e38200202908C17B43`.
    ///
    ///      The zero address is rejected. Unsetting it would silently break every configured pair
    ///      into `REF_UNAVAILABLE` — which fails closed, so it is not a *safety* hole, but it
    ///      would halt every push across the pool with no per-pair record of why. Disabling the
    ///      bound is `setPairOracle(pairId, 0, ...)`, per pair, deliberately, with an event.
    function setPyth(address newPyth) external onlyOwner {
        if (newPyth == address(0)) revert ZeroAddress();
        emit PythUpdated(pyth, newPyth);
        pyth = newPyth;
    }

    /// @notice Set, retune, or disable one pair's reference bound.
    ///
    /// @dev Manager-gated, and separate from `setPairConfig` so that retuning a deviation limit
    ///      does not require restating the reserve floors (and vice versa) — restating a parameter
    ///      you did not mean to change is how parameters get changed by accident.
    ///
    ///      **This function does not read Pyth.** Configuring a pair therefore does not depend on
    ///      the oracle being live, which matters given that the check fails closed: an operator
    ///      must be able to point a pair at a feed before that feed's first on-chain push, and
    ///      must be able to *disable* a bound while Pyth is down without needing Pyth to answer.
    ///      The cost is that a typo'd `feedId` or a wrong `refExpo` is not caught here. Call
    ///      `referencePrice(pairId)` immediately afterwards and check for `REF_OK` — it is a free
    ///      `eth_call`, it never reverts, and it is the intended preflight.
    ///
    /// @param feedId           Pyth price feed id, or **zero to disable the bound for this pair**.
    ///                         The remaining arguments are stored but unread while it is zero.
    /// @param maxDeviationBps  tolerance on `maxBid` above and `minAsk` below the reference.
    /// @param maxPythStaleSecs our freshness window on Pyth's `publishTime`, not Pyth's own.
    /// @param refExpo          `priceScaleExp + quoteDecimals - baseDecimals`. See `PairOracle`.
    function setPairOracle(
        uint16 pairId,
        bytes32 feedId,
        uint16 maxDeviationBps,
        uint32 maxPythStaleSecs,
        int8 refExpo
    ) external onlyManager {
        if (pairId == 0 || pairId > pairCount) revert UnknownPair();

        // Validated only when the bound is actually being switched on. Disabling a pair must not
        // be blockable by the state of parameters that are about to stop mattering.
        if (feedId != bytes32(0)) {
            if (pyth == address(0)) revert PythNotSet();
            // Above BPS the ask floor `ref * (BPS - dev)` underflows. Rejected rather than
            // clamped: a manager who typed 20000 meant something, and it was not "100%".
            if (maxDeviationBps > BPS) revert DeviationTooLarge();
            // A zero window demands a price published in this exact second, which no pull oracle
            // guarantees. It would brick every push on the pair, so it is a config error.
            if (maxPythStaleSecs == 0) revert ZeroPythStaleWindow();
        }

        _oracle[pairId] = PairOracle({
            feedId: feedId,
            maxDeviationBps: maxDeviationBps,
            maxPythStaleSecs: maxPythStaleSecs,
            refExpo: refExpo
        });

        emit PairOracleUpdated(pairId, feedId, maxDeviationBps, maxPythStaleSecs, refExpo);
    }

    /// @notice One pair's reference-oracle configuration. `feedId == 0` means unbounded.
    function pairOracle(uint16 pairId) external view returns (PairOracle memory) {
        return _oracle[pairId];
    }

    /// @notice The reference price for `pairId`, already scaled into the pair's own price units,
    ///         and the status that produced it.
    ///
    /// @dev **Total: this never reverts, for any `pairId`, in any oracle state.** It is the
    ///      preflight the updater should run before spending gas on a push that `updateQuote`
    ///      would reject, and the way an integrator answers "is this pair currently boundable"
    ///      without simulating a state-changing call. `status` is `REF_OK` only when `price` is
    ///      meaningful; every other code returns `price == 0`.
    ///
    ///      Directly comparable with `snapshot(pairId).maxBid` and `.minAsk` — same units, same
    ///      `priceScaleExp` — which is the whole reason it returns a scaled number rather than the
    ///      raw Pyth pair.
    ///
    ///      Reminder, because it is the misreading this file most expects: this is a **bound**.
    ///      Nothing in the pool prices from it. Do not build a quoter that does.
    function referencePrice(uint16 pairId) external view returns (uint256 price, uint8 status) {
        return _referencePrice(pairId);
    }

    // ---------------------------------------------------------------------
    // Updater surface — writes prices and capacity, moves no tokens
    // ---------------------------------------------------------------------

    /// @notice Push new ladders. One packed word per pair.
    ///
    /// Word layout (identical to storage word 0 except that `updatedAt`'s space carries the
    /// pair id on the way in):
    ///
    ///   bits   0..55  minBid
    ///   bits  56..111 maxBid
    ///   bits 112..167 minAsk
    ///   bits 168..223 maxAsk
    ///   bits 224..239 pairId
    ///
    /// @dev One SSTORE per pair, and the write is a mask-and-or of the calldata word — no
    ///      re-packing, no read-modify-write, no second slot. Measured with
    ///      `forge test --gas-report` on a warm slot with a single-entry batch: **28,747 gas**
    ///      total for the external call, of which 2,900 is the SSTORE and ~2,100 is the cold
    ///      config SLOAD for `minPrice`. A five-pair batch measured 61,159 gas
    ///      (≈ 12.2k/pair amortised: 2.9k SSTORE + 2.1k cold config SLOAD + ~1.6k calldata +
    ///      log). Existence is checked against `pairCount`, not `PairConfig.exists`, precisely
    ///      to avoid a second cold SLOAD per entry.
    ///
    ///      **The reference-oracle bound is enforced here**, immediately after `validateLadder`,
    ///      which can only reject an *incoherent* book and not a wrong one. That is what makes
    ///      this the pool's price-correctness boundary rather than merely its price-coherence one,
    ///      and it is the reason this function — not `swap` — pays for the oracle. Read the
    ///      contract header for the placement argument and the fail-closed policy; the short
    ///      version is that the updater is the untrusted actor, this is its only write, and
    ///      leaving `swap` and the views free of an external call is worth the gas landing here.
    ///
    ///      Cost of the bound, measured cold in `test/PropPoolOracle.t.sol`: **+18,997 gas** on the
    ///      first bounded pair and **+26,769** on each subsequent one — only the Pyth account
    ///      access and the `pyth` SLOAD amortise across a batch; each feed's own storage is cold
    ///      per feed. A pair with **no feed configured costs +111 gas**, a single cold `feedId`
    ///      SLOAD that short-circuits before the external call, so a batch of unbounded pairs is
    ///      essentially unchanged. The contract header has the full table and is explicit that
    ///      this puts a bounded push outside the original ~20k budget on purpose.
    ///
    ///      The batch is atomic, and deliberately so: one out-of-bound pair reverts every pair in
    ///      the call. Partial application would leave the updater guessing which ladders landed,
    ///      and the ladders that did land were priced by a feed that just proved itself wrong on
    ///      another pair.
    function updateQuote(uint256[] calldata packed) external onlyUpdater {
        uint256 n = pairCount;
        uint256 stamp = uint256(uint32(block.timestamp)) << SHIFT_UPDATED_AT;
        uint256 len = packed.length;

        for (uint256 i; i < len;) {
            uint256 word = packed[i];
            uint256 pairId = (word >> SHIFT_UPDATED_AT) & MASK_16;
            if (pairId == 0 || pairId > n) revert UnknownPair();

            uint256 minBid = word & MASK_56;
            uint256 maxBid = (word >> SHIFT_MAX_BID) & MASK_56;
            uint256 minAsk = (word >> SHIFT_MIN_ASK) & MASK_56;
            uint256 maxAsk = (word >> SHIFT_MAX_ASK) & MASK_56;

            PropCurve.validateLadder(minBid, maxBid, minAsk, maxAsk, _config[uint16(pairId)].minPrice);
            _checkReferenceBound(uint16(pairId), maxBid, minAsk);

            _quoteWord[uint16(pairId)] = (word & LADDER_MASK) | stamp;

            emit QuoteUpdated(uint16(pairId), uint56(minBid), uint56(maxBid), uint56(minAsk), uint56(maxAsk));

            unchecked {
                ++i;
            }
        }
    }

    /// @notice Post a fresh capacity epoch for one pair.
    /// @dev This is the risk decision, not the price decision. Bumping `capGen` is what makes
    ///      the stored `used` counters read as zero again; nothing else in the system does that.
    /// @param bidCapacity total base the pool will buy this epoch, in base's smallest unit.
    /// @param askCapacity total base the pool will *sell* this epoch, also in base units. This
    ///                    argument used to be quote-denominated; see `PropCurve` amendment 1.
    function refreshCapacity(uint16 pairId, uint96 bidCapacity, uint96 askCapacity) external onlyUpdater {
        if (pairId == 0 || pairId > pairCount) revert UnknownPair();
        _refreshCapacity(pairId, bidCapacity, askCapacity);
    }

    /// @notice Batched capacity refresh.
    ///
    ///   bits   0..95  bidCapacity (base units)
    ///   bits  96..191 askCapacity (base units — was quote; the position is unchanged, the unit
    ///                              is not. See `PropCurve` amendment 1.)
    ///   bits 224..239 pairId      — same position as in `updateQuote`, so both encoders on the
    ///                               engine side share one field writer
    function refreshCapacityBatch(uint256[] calldata packed) external onlyUpdater {
        uint256 n = pairCount;
        uint256 len = packed.length;
        for (uint256 i; i < len;) {
            uint256 word = packed[i];
            uint256 pairId = (word >> SHIFT_FLAGS) & MASK_16;
            if (pairId == 0 || pairId > n) revert UnknownPair();
            _refreshCapacity(
                uint16(pairId), uint96(word & MASK_96), uint96((word >> SHIFT_ASK_CAPACITY) & MASK_96)
            );
            unchecked {
                ++i;
            }
        }
    }

    /// @dev Read-modify-write because the paused flag lives in the same word and belongs to the
    ///      guardian. Capacity refresh is deliberate and infrequent, so the extra SLOAD is not
    ///      worth splitting the word for.
    ///
    ///      `capGen` wraps at 2^32 on purpose — only equality against `usedGen` matters. A wrap
    ///      that resurrected a stale `used` counter would need exactly 2^32 refreshes between a
    ///      swap and the matching epoch; at one refresh per second that is 136 years.
    ///
    ///      ## The domain bound, and why it lives here
    ///
    ///      A quote leg is `base * price / 10**priceScaleExp`, and `PropCurve` closes the domain it
    ///      shares with the Rust mirror at `MAX_AMOUNT_OUT == type(uint128).max` for every
    ///      quote-denominated amount. Three factors decide whether a leg lands inside it: capacity,
    ///      price and `priceScaleExp`. They have three different writers — this function,
    ///      `updateQuote`, and `addPair` — so no single call site sees all three, and the only one
    ///      that can bound the product without reading another actor's word is this one, by
    ///      assuming the worst price the field can hold:
    ///
    ///          capacity * type(uint56).max <= MAX_AMOUNT_OUT * 10**priceScaleExp
    ///
    ///      That makes `amountOutBid(capacity, ...)` and `amountInAsk(capacity, ...)` in-domain for
    ///      *every* ladder the updater can subsequently post, which is exactly what lets `_outFor`
    ///      and `_inFor` probe the epoch's whole remaining depth without a `PropCurve` revert
    ///      escaping through a view. **The two are one mechanism: do not weaken this check without
    ///      re-reading `_maxAmountIn`.**
    ///
    ///      It is not a restriction in practice. `10**8 > 2^26 > 2^152 / 2^128`, so for any
    ///      `priceScaleExp >= 8` the bound exceeds `type(uint96).max` and rejects nothing at all.
    ///      Only the small-exponent pairs are constrained, and only above capacities no risk desk
    ///      would post: at `priceScaleExp == 0` the ceiling is still ~4.7e21 base units.
    ///
    ///      The alternative — a `priceScaleExp >= 8` floor in `addPair` — was rejected: an
    ///      8-decimal base against an 18-decimal quote (WBTC/DAI) needs `priceScaleExp <= 2` to
    ///      keep its price inside `uint56` at all, and refusing to list it would be a real loss to
    ///      close an unreachable corner.
    function _refreshCapacity(uint16 pairId, uint96 bidCapacity, uint96 askCapacity) private {
        uint256 largest = bidCapacity > askCapacity ? bidCapacity : askCapacity;
        // `largest * MAX_PRICE < 2^152` and `MAX_AMOUNT_OUT * 10**38 < 2^255`; neither side can
        // overflow, so this needs no unchecked block and no division.
        if (largest * MAX_PRICE > PropCurve.MAX_AMOUNT_OUT * (10 ** uint256(_config[pairId].priceScaleExp))) {
            revert CapacityOutOfDomain();
        }

        uint256 word = _capacityWord[pairId];
        uint32 gen;
        unchecked {
            gen = uint32(((word >> SHIFT_GEN) & MASK_32) + 1);
        }
        _capacityWord[pairId] = uint256(bidCapacity) | (uint256(askCapacity) << SHIFT_ASK_CAPACITY)
            | (uint256(gen) << SHIFT_GEN) | (word & (MASK_16 << SHIFT_FLAGS));

        emit CapacityRefreshed(pairId, bidCapacity, askCapacity, gen);
    }

    // ---------------------------------------------------------------------
    // Guardian surface
    // ---------------------------------------------------------------------

    function pause(uint16 pairId) external onlyGuardian {
        _setPaused(pairId, true);
    }

    function unpause(uint16 pairId) external onlyGuardian {
        _setPaused(pairId, false);
    }

    /// @dev Emitted against the reserved pair id 0, which is never a real pair.
    function pauseAll() external onlyGuardian {
        allPaused = true;
        emit Paused(0, true);
    }

    /// @notice Counterpart to `pauseAll`. Guardian-held: the role is defined as halt/resume, and
    ///         requiring a timelock to resume would turn every false alarm into a multi-hour
    ///         outage while the pool's quotes sit unfillable.
    function unpauseAll() external onlyGuardian {
        allPaused = false;
        emit Paused(0, false);
    }

    function _setPaused(uint16 pairId, bool paused) private {
        if (pairId == 0 || pairId > pairCount) revert UnknownPair();
        uint256 word = _capacityWord[pairId];
        _capacityWord[pairId] = paused ? (word | FLAG_PAUSED) : (word & ~FLAG_PAUSED);
        emit Paused(pairId, paused);
    }

    // ---------------------------------------------------------------------
    // Inventory
    // ---------------------------------------------------------------------

    /// @notice Fund the pool. Pull-based so `_reserve` stays exact without a `sync`.
    function deposit(address token, uint256 amount) external onlyManager locked {
        token.safeTransferFrom(msg.sender, address(this), amount);
        uint256 reserve = _reserve[token] + amount;
        _reserve[token] = reserve;
        emit ReserveSynced(token, reserve);
    }

    /// @notice Withdraw inventory. Owner-only: this is the one place value leaves the pool for
    ///         a destination the curve did not choose, so it sits behind the timelock role.
    function withdraw(address token, uint256 amount, address to) external onlyOwner locked {
        if (to == address(0)) revert ZeroAddress();
        _reserve[token] -= amount; // underflows rather than spending unaccounted balance
        token.safeTransfer(to, amount);
        emit ReserveSynced(token, _reserve[token]);
    }

    /// @notice Reconcile accounted inventory to the real balance.
    /// @dev The escape hatch for donations, for a pair funded by a plain transfer before anyone
    ///      remembered `deposit`, and for a token that rebased downward. Manager-gated because
    ///      it can raise the reserve floor headroom, which is a risk parameter.
    function sync(address token) external onlyManager {
        uint256 balance = IERC20Balance(token).balanceOf(address(this));
        _reserve[token] = balance;
        emit ReserveSynced(token, balance);
    }

    function reserveOf(address token) external view returns (uint256) {
        return _reserve[token];
    }

    function pairConfig(uint16 pairId) external view returns (PairConfig memory) {
        return _config[pairId];
    }

    // ---------------------------------------------------------------------
    // Quoting — every path here returns 0 instead of reverting
    // ---------------------------------------------------------------------

    /// @inheritdoc IPropPool
    function getAmountOut(address tokenIn, address tokenOut, uint256 amountIn)
        external
        view
        returns (uint256 amountOut)
    {
        (uint16 pairId, bool isBid) = pairIdFor(tokenIn, tokenOut);
        return quoteByPair(pairId, isBid, amountIn);
    }

    /// @inheritdoc IPropPool
    function quoteByPair(uint16 pairId, bool isBid, uint256 amountIn) public view returns (uint256 amountOut) {
        (uint256 status, PairState memory st) = _load(pairId, isBid);
        if (status != STATUS_OK) return 0;

        amountOut = _outFor(st, isBid, amountIn);
        if (amountOut == 0) return 0;
        if (_breachesFloor(st, amountOut)) return 0;
    }

    /// @inheritdoc IPropPool
    function getAmountIn(address tokenIn, address tokenOut, uint256 amountOut)
        external
        view
        returns (uint256 amountIn)
    {
        (uint16 pairId, bool isBid) = pairIdFor(tokenIn, tokenOut);
        (uint256 status, PairState memory st) = _load(pairId, isBid);
        if (status != STATUS_OK) return 0;
        if (amountOut == 0) return 0;
        if (_breachesFloor(st, amountOut)) return 0;
        return _inFor(st, isBid, amountOut);
    }

    /// @inheritdoc IPropPool
    /// @dev Returns the raw stored counters, including both generations, rather than the
    ///      effective ones. A simulator needs to see `usedGen != capGen` to reproduce the
    ///      contract's own rule; collapsing it here would hide the mechanism.
    function snapshot(uint16 pairId) external view returns (PairSnapshot memory s) {
        if (pairId == 0 || pairId > pairCount) return s;

        uint256 q = _quoteWord[pairId];
        uint256 c = _capacityWord[pairId];
        uint256 u = _usedWord[pairId];
        PairConfig storage cfg = _config[pairId];

        s.minBid = uint56(q & MASK_56);
        s.maxBid = uint56((q >> SHIFT_MAX_BID) & MASK_56);
        s.minAsk = uint56((q >> SHIFT_MIN_ASK) & MASK_56);
        s.maxAsk = uint56((q >> SHIFT_MAX_ASK) & MASK_56);
        s.updatedAt = uint32(q >> SHIFT_UPDATED_AT);

        s.bidCapacity = uint96(c & MASK_96);
        s.askCapacity = uint96((c >> SHIFT_ASK_CAPACITY) & MASK_96);
        s.capGen = uint32((c >> SHIFT_GEN) & MASK_32);
        s.flags = uint16((c >> SHIFT_FLAGS) & MASK_16);

        // Bit 15, synthesized rather than stored: does this pair have a reference bound at all?
        //
        // It rides in `flags` rather than in a new struct field because `PairSnapshot` is a static
        // tuple that off-chain decoders — the Rust updater's `sol!` mirror among them — reproduce
        // field by field, and appending to it changes the return encoding under every one of them.
        // A spare bit in a field they already decode as `uint16` costs them nothing. `pairOracle`
        // is the detailed answer; this is the one that comes free with a call they already make.
        //
        // Note what it does NOT claim: that the bound is currently *satisfiable*. Answering that
        // means reading Pyth, and `snapshot` is bound by the same no-revert contract as the
        // quoting views. `referencePrice` is the (also total) call for that question.
        if (_oracle[pairId].feedId != bytes32(0)) s.flags |= FLAG_SNAPSHOT_BOUNDED;

        s.bidUsed = uint96(u & MASK_96);
        s.askUsed = uint96((u >> SHIFT_ASK_CAPACITY) & MASK_96);
        s.usedGen = uint32((u >> SHIFT_GEN) & MASK_32);

        s.priceScaleExp = cfg.priceScaleExp;
        s.maxStaleSecs = cfg.maxStaleSecs;
    }

    /// @inheritdoc IPropPool
    function pairIdFor(address tokenIn, address tokenOut) public view returns (uint16 pairId, bool isBid) {
        uint256 route = _route[tokenIn][tokenOut];
        pairId = uint16(route & MASK_16);
        isBid = route & ROUTE_IS_BID != 0;
    }

    /// @inheritdoc IPropPool
    function getSupportedPairs() external view returns (Pair[] memory pairs) {
        uint256 n = pairCount;
        pairs = new Pair[](n);
        for (uint256 i; i < n;) {
            uint16 id = uint16(i + 1);
            PairConfig storage cfg = _config[id];
            pairs[i] = Pair({pairId: id, base: cfg.base, quote: cfg.quote});
            unchecked {
                ++i;
            }
        }
    }

    // ---------------------------------------------------------------------
    // Swapping
    // ---------------------------------------------------------------------

    /// @inheritdoc IPropPool
    /// @dev Guard order is deadline, existence, pause, freshness, capacity, reserve floor,
    ///      slippage — cheapest and most-likely-to-fail first, and slippage last so a taker's
    ///      limit is judged against a price that has already passed every safety gate.
    ///
    ///      `specifiedAmount` is denominated in whichever leg it pins: base for an exact-input bid
    ///      or an exact-output ask, quote for the other two. Capacity is charged in base on both
    ///      sides, so the leg the epoch's budget sees is `amountIn` for a bid and `amountOut` for
    ///      an ask, whichever of the two the taker specified.
    ///
    ///      Word 2 is written after the transfers. That is not CEI, and the transient lock is
    ///      the reason it is safe: no re-entrant *state-changing* path exists. The residual
    ///      surface is read-only re-entrancy — a hostile `tokenOut` can call `getAmountOut`
    ///      from inside its own transfer and see pre-trade `used`. It cannot act on that quote
    ///      in the same transaction, so the exposure is limited to integrators who treat our
    ///      view output as an oracle from inside their own callbacks.
    ///
    ///      `partnerId` is recorded and otherwise ignored. Spread tiering off an OnchainVerify
    ///      attestation would hook in here, adjusting the ladder ends before `_outFor`.
    function swap(
        address tokenIn,
        address tokenOut,
        int256 specifiedAmount,
        uint256 limitAmount,
        address receiver,
        uint256 partnerId,
        uint256 deadline
    ) external locked returns (uint256 result) {
        if (block.timestamp > deadline) revert DeadlineExpired();
        if (receiver == address(0)) revert ZeroAddress();
        if (specifiedAmount == 0) revert ZeroAmount();
        if (specifiedAmount == type(int256).min) revert AmountOverflow();

        (uint16 pairId, bool isBid) = pairIdFor(tokenIn, tokenOut);

        uint256 amountIn;
        uint256 amountOut;
        if (specifiedAmount > 0) {
            amountIn = uint256(specifiedAmount);
            amountOut = _resolveExactIn(pairId, isBid, amountIn);
            if (amountOut < limitAmount) revert SlippageExceeded();
            result = amountOut;
        } else {
            // Exact output: the taker is delivered exactly `amountOut`. `_inFor` rounds the
            // required input up, so the pool may end up holding a sub-unit surplus. That dust
            // is the price of never rounding in the taker's favour.
            amountOut = uint256(-specifiedAmount);
            amountIn = _resolveExactOut(pairId, isBid, amountOut);
            if (amountIn > limitAmount) revert SlippageExceeded();
            result = amountIn;
        }

        tokenIn.safeTransferFrom(msg.sender, address(this), amountIn);
        tokenOut.safeTransfer(receiver, amountOut);

        _settle(pairId, isBid, tokenIn, tokenOut, amountIn, amountOut);

        emit Swap(pairId, msg.sender, receiver, isBid, amountIn, amountOut, partnerId);
    }

    /// @inheritdoc IPropPool
    /// @dev The input amount is `balanceOf(tokenIn) - _reserve[tokenIn]`, i.e. everything that
    ///      has arrived since the pool last accounted for that token. The caveat in IPropPool is
    ///      literal and load-bearing: any *other* transfer of `tokenIn` that lands before this
    ///      call — a second router's push, an airdrop, a donation — is credited to this caller.
    ///      Batch nothing else into the same transaction, and do not use this path on a token
    ///      the pool receives from anywhere but routers.
    ///
    ///      Exact-in only. Exact-out over a pushed balance would require refunding the surplus,
    ///      which reintroduces the approval hop this path exists to avoid.
    function swapWithContractBalance(
        address tokenIn,
        address tokenOut,
        uint256 limitAmount,
        address receiver,
        uint256 partnerId,
        uint256 deadline
    ) external locked returns (uint256 amountOut) {
        if (block.timestamp > deadline) revert DeadlineExpired();
        if (receiver == address(0)) revert ZeroAddress();

        (uint16 pairId, bool isBid) = pairIdFor(tokenIn, tokenOut);

        // Underflows if the token rebased below the accounted reserve; `sync` is the fix.
        uint256 amountIn = IERC20Balance(tokenIn).balanceOf(address(this)) - _reserve[tokenIn];
        if (amountIn == 0) revert ZeroAmount();

        amountOut = _resolveExactIn(pairId, isBid, amountIn);
        if (amountOut < limitAmount) revert SlippageExceeded();

        tokenOut.safeTransfer(receiver, amountOut);

        _settle(pairId, isBid, tokenIn, tokenOut, amountIn, amountOut);

        emit Swap(pairId, msg.sender, receiver, isBid, amountIn, amountOut, partnerId);
    }

    // ---------------------------------------------------------------------
    // Internal: state loading
    // ---------------------------------------------------------------------

    /// @dev Single source of truth for "can this pair trade right now, and on what numbers".
    ///      Returns a status code instead of reverting so the view path can turn it into a zero
    ///      and the swap path can turn it into a specific error, without the two drifting apart.
    function _load(uint16 pairId, bool isBid) private view returns (uint256 status, PairState memory st) {
        if (pairId == 0 || pairId > pairCount) return (STATUS_UNKNOWN, st);
        if (allPaused) return (STATUS_PAUSED, st);

        uint256 capWord = _capacityWord[pairId];
        if (capWord & FLAG_PAUSED != 0) return (STATUS_PAUSED, st);

        PairConfig storage cfg = _config[pairId];
        uint256 qWord = _quoteWord[pairId];
        uint256 updatedAt = qWord >> SHIFT_UPDATED_AT;

        // `updatedAt == 0` is checked explicitly rather than left to the staleness arithmetic:
        // `maxStaleSecs` is a uint32 and can exceed the current unix timestamp, so a large
        // window would otherwise make a pair that has never been quoted look fresh.
        if (updatedAt == 0) return (STATUS_STALE, st);
        if (updatedAt > block.timestamp) return (STATUS_STALE, st);
        if (block.timestamp - updatedAt > cfg.maxStaleSecs) return (STATUS_STALE, st);

        st.priceScaleExp = cfg.priceScaleExp;
        if (isBid) {
            st.pLow = qWord & MASK_56; // minBid — worst, at full usage
            st.pHigh = (qWord >> SHIFT_MAX_BID) & MASK_56; // maxBid — best, at zero usage
            st.capacity = capWord & MASK_96; // base units
            st.tokenOut = cfg.quote;
            st.minReserveOut = cfg.minQuoteReserve;
        } else {
            st.pLow = (qWord >> SHIFT_MIN_ASK) & MASK_56; // minAsk — best, at zero usage
            st.pHigh = (qWord >> SHIFT_MAX_ASK) & MASK_56; // maxAsk — worst, at full usage
            st.capacity = (capWord >> SHIFT_ASK_CAPACITY) & MASK_96; // base units, like the bid side
            st.tokenOut = cfg.base;
            st.minReserveOut = cfg.minBaseReserve;
        }

        uint256 uWord = _usedWord[pairId];
        if (((uWord >> SHIFT_GEN) & MASK_32) == ((capWord >> SHIFT_GEN) & MASK_32)) {
            st.used = isBid ? (uWord & MASK_96) : ((uWord >> SHIFT_ASK_CAPACITY) & MASK_96);
        }

        st.reserveOut = _reserve[st.tokenOut];
    }

    // ---------------------------------------------------------------------
    // Internal: the reference bound
    //
    // Read the contract header before changing anything here. In particular: Pyth is a bound and
    // never a price source, and the check runs at push time rather than at swap time. Both are
    // decisions with reasons, and both are easy to "fix" into something worse.
    // ---------------------------------------------------------------------

    /// @dev The reference price in this pair's own units, or a status explaining why there isn't
    ///      one. **Total — no input and no oracle state makes this revert.** That is a hard
    ///      requirement, not a convenience: `referencePrice` exposes it as a view and
    ///      `IPropPool`'s no-revert contract would be broken by a Pyth call that escaped.
    ///
    ///      The totality is assembled from four separate guards, because Solidity gives no single
    ///      one that covers the ground:
    ///
    ///        1. `feedId == 0` short-circuits before anything external happens. This is also the
    ///           gas argument: an unbounded pair pays one cold SLOAD and stops.
    ///        2. `p.code.length == 0` is checked *before* the call. `try`/`catch` does **not**
    ///           catch a return-data decoding failure — the docs are explicit that such an
    ///           exception happens in the current context and is not caught — and a call to an
    ///           address with no code succeeds while returning nothing, which is precisely a
    ///           decoding failure. Without this line an unset or self-destructed oracle would
    ///           revert straight through the `try`.
    ///        3. `catch` handles the ordinary revert: `PriceFeedNotFound` for a feed nobody has
    ///           pushed yet, and anything else the oracle chooses to throw.
    ///        4. The arithmetic below is guarded rather than merely `unchecked`-avoided: the
    ///           exponent is range-limited before `10 **` forms it, and the product is bounded by
    ///           `MAX_REFERENCE_PRICE` before any caller multiplies it by a bps factor.
    ///
    ///      An oracle that burns all the gas rather than returning is **not** a hole, and the
    ///      reason is EIP-150 rather than anything written here: the callee receives 63/64 of the
    ///      remaining gas, exhausts it, and reverts, which arm 3 catches — and the retained 1/64 is
    ///      more than enough to finish, since nothing after the call does more than arithmetic.
    ///      Measured `REF_UNAVAILABLE` at 100k, 200k, 1M and 10M gas. Do not add work after the
    ///      `try` block on the strength of that margin without re-measuring it.
    ///
    ///      The residual hole is an oracle address that has code and returns well-formed but
    ///      *untrue* data — a lying oracle rather than a broken one. Nothing on chain can detect
    ///      that, and reaching it requires the owner to have pointed `pyth` at a hostile contract,
    ///      which is outside the threat model this bound exists for: the untrusted actor here is
    ///      the updater, and the updater cannot set `pyth`.
    ///
    ///      `getPriceUnsafe` is used rather than `getPriceNoOlderThan`, and the choice is
    ///      deliberate: the latter reverts `StalePrice`, which `catch` would flatten into
    ///      `REF_UNAVAILABLE` and lose the distinction between "Pyth is gone" and "Pyth is behind".
    ///      Those are different operational problems. Doing the comparison here also means the
    ///      window is ours (`maxPythStaleSecs`) rather than the oracle's advisory
    ///      `getValidTimePeriod`, which is the correct ownership: how stale a *bound* may be is a
    ///      property of our risk model.
    function _referencePrice(uint16 pairId) private view returns (uint256 price, uint8 status) {
        PairOracle storage o = _oracle[pairId];

        bytes32 feedId = o.feedId;
        if (feedId == bytes32(0)) return (0, REF_DISABLED);

        address p = pyth;
        if (p == address(0) || p.code.length == 0) return (0, REF_UNAVAILABLE);

        PythStructs.Price memory quoted;
        try IPyth(p).getPriceUnsafe(feedId) returns (PythStructs.Price memory r) {
            quoted = r;
        } catch {
            return (0, REF_UNAVAILABLE);
        }

        // Absolute difference, matching Pyth's own `diff`. A price published ahead of the chain is
        // stale in exactly the way a price published behind it is: a reference is only a reference
        // if it has been observed, and a future timestamp has not been.
        uint256 publishTime = quoted.publishTime;
        uint256 age =
            publishTime > block.timestamp ? publishTime - block.timestamp : block.timestamp - publishTime;
        if (age > o.maxPythStaleSecs) return (0, REF_STALE);

        // Rejected, not cast. `price` is `int64` and Pyth feeds for rates and spreads do go
        // negative; a wrapping cast would turn -1 into ~1.8e19 and hand the ladder a bound it
        // trivially satisfies on one side and trivially fails on the other. Zero is rejected with
        // it — it is not a price, and as a bound it would pin the bid ceiling to zero.
        if (quoted.price <= 0) return (0, REF_INVALID);

        // `pythExpo` comes off the response every time. It is per-feed, Pyth can change it, and a
        // consumer that hardcodes it is one governance action away from being wrong by orders of
        // magnitude — which, in a deviation bound, means accepting anything.
        int256 net = int256(quoted.expo) + int256(o.refExpo);
        if (net > MAX_NET_EXPONENT || net < -MAX_NET_EXPONENT) return (0, REF_INVALID);

        // Every cast below is discharged by the two lines above: `quoted.price > 0` makes the
        // widening to `uint256` value-preserving, and `|net| <= 58` plus the branch's own sign
        // test makes both `uint256(net)` and `uint256(-net)` small positive numbers — the latter
        // also cannot be the `-type(int256).min` trap for the same reason.
        uint256 raw = uint256(int256(quoted.price));
        if (net >= 0) {
            price = raw * (10 ** uint256(net));
        } else {
            // Floors. Only reachable when `priceScaleExp + quoteDecimals - baseDecimals` is more
            // negative than Pyth's own exponent, which `addPair`'s "take the largest exponent that
            // fits" rule already pushes away from; both live markets land at net +4 and +2. When it
            // is reachable the truncation tightens the bid ceiling and loosens the ask floor by at
            // most one unit in the last place, so it is not symmetric — prefer a `priceScaleExp`
            // that keeps `net >= 0` and the question does not arise.
            price = raw / (10 ** uint256(-net));
        }

        // `price == 0` catches the division collapsing the reference to nothing, which is what a
        // badly wrong `refExpo` looks like from here. The upper bound is explained at the
        // constant.
        if (price == 0 || price > MAX_REFERENCE_PRICE) return (0, REF_INVALID);

        status = REF_OK;
    }

    /// @dev The guard itself, called from `updateQuote` immediately after `validateLadder` — the
    ///      ladder must be coherent before asking whether it is *right*, so that an incoherent
    ///      book reports as incoherent rather than as off-reference.
    ///
    ///      Only `maxBid` and `minAsk` are passed, because only those two are bounded; see the
    ///      contract header for why bounding the ladder's far ends, or its mid, are both worse.
    ///
    ///      Fails closed on every non-OK status except `REF_DISABLED`. `REF_DISABLED` is a
    ///      configuration the operator chose and returns silently; everything else is the oracle
    ///      failing to do its job, and the pool declines to post a new ladder until it does. The
    ///      header spells out what that trades away.
    ///
    ///      Overflow, since these are the products the bound rests on: `maxBid` and `minAsk` are
    ///      `uint56` (< 7.3e16) so the left sides reach at most ~7.3e20, and `ref` is capped at
    ///      `MAX_REFERENCE_PRICE` with `dev <= BPS` so the right sides reach at most ~3.7e23. Both
    ///      are ~53 orders of magnitude clear of `uint256`. No `unchecked`, and none needed.
    function _checkReferenceBound(uint16 pairId, uint256 maxBid, uint256 minAsk) private view {
        (uint256 ref, uint8 status) = _referencePrice(pairId);
        if (status == REF_DISABLED) return;
        if (status != REF_OK) revert ReferenceUnavailable(pairId, status);

        // Warm: `_referencePrice` already touched this slot for `maxPythStaleSecs`/`refExpo`.
        uint256 dev = _oracle[pairId].maxDeviationBps;

        // The pool must never offer to BUY base above the reference plus tolerance.
        if (maxBid * BPS > ref * (BPS + dev)) revert BidCeilingExceeded(pairId, ref, maxBid);
        // ...nor to SELL base below the reference minus tolerance.
        if (minAsk * BPS < ref * (BPS - dev)) revert AskFloorBreached(pairId, ref, minAsk);
    }

    function _loadChecked(uint16 pairId, bool isBid) private view returns (PairState memory st) {
        uint256 status;
        (status, st) = _load(pairId, isBid);
        if (status == STATUS_UNKNOWN) revert UnknownPair();
        if (status == STATUS_PAUSED) revert PoolPaused();
        if (status == STATUS_STALE) revert StaleQuote();
    }

    // ---------------------------------------------------------------------
    // Internal: curve application
    //
    // Two properties of the amended `PropCurve` shape everything in this section.
    //
    //  1. **Capacity and usage are base-denominated on both sides.** The base leg of a trade is
    //     `amountIn` for a bid and `amountOut` for an ask, so capacity bounds the *input* of a bid
    //     and the *output* of an ask. The other leg of each direction is quote-denominated and has
    //     to go through the curve before it can be compared with anything. The pre-amendment code
    //     compared a quote-denominated ask `amountIn` against `askCapacity` directly; both are
    //     `uint96`, so it compiled, and it was wrong by a factor of the price.
    //
    //  2. **All four directions are exact and monotone**, unconditionally (`PropCurve`'s header
    //     carries the proof). `amountInBid` / `amountInAsk` are the exact-output primitives, so
    //     PropPool's own bisection — which existed only because the pre-amendment curve was not
    //     monotone — is deleted rather than reimplemented.
    //
    // Every helper here is total: it returns 0 rather than letting `PropCurve` revert, which is the
    // obligation `IPropPool` states for the view path. `_maxAmountIn` documents the one part of
    // that which is not local to this section.
    // ---------------------------------------------------------------------

    /// @dev The largest `amountIn` this side's epoch can accept, in the direction's INPUT token.
    ///      Callers must have established `capacity > 0 && used < capacity`.
    ///
    ///      For a bid the input *is* the base leg, so this is just the remaining room. For an ask
    ///      the input is quote and the room is base, so the ceiling is the quote cost of all the
    ///      remaining base — precisely the quantity `PropCurve.amountOutAsk` compares against
    ///      internally before reverting `AmountExceedsCapacity`. Recomputing it here is what turns
    ///      that revert into a 0, and it is the reason a plain `askUsed + amountIn > askCapacity`
    ///      test cannot be resurrected: those two are base units and `amountIn` is not.
    ///
    ///      **This call cannot itself revert, and the reason is not local.** `amountInAsk` raises
    ///      `AmountOutOfDomain` when the cost exceeds `PropCurve.MAX_AMOUNT_OUT`; the bound
    ///      `_refreshCapacity` enforces when capacity is written keeps `cost(capacity)` inside that
    ///      domain for every ladder the updater can post. `ZeroCapacity` is excluded by the
    ///      caller's precondition and `AmountExceedsCapacity` by `used + room == capacity`.
    ///      `ZeroPrice` needs `maxAsk == 0`, which `_load` cannot produce: it only returns
    ///      `STATUS_OK` for a pair whose stored ladder passed `validateLadder`, and that forces
    ///      `maxAsk > minBid >= minPrice >= 1`.
    function _maxAmountIn(PairState memory st, bool isBid) private pure returns (uint256) {
        uint256 room = st.capacity - st.used;
        if (isBid) return room;
        return PropCurve.amountInAsk(room, st.pLow, st.pHigh, st.capacity, st.used, st.priceScaleExp);
    }

    /// @dev The largest `amountOut` this side's epoch can deliver, in the direction's OUTPUT token.
    ///      Mirror of `_maxAmountIn` — for an ask the output is the base leg and the room is the
    ///      answer, for a bid it is the quote the whole remaining room would fetch — and revert-free
    ///      for the same reasons. Doubles as the fillability probe for the exact-output paths: a
    ///      target above this is one the epoch cannot deliver at any input.
    function _maxAmountOut(PairState memory st, bool isBid) private pure returns (uint256) {
        uint256 room = st.capacity - st.used;
        if (!isBid) return room;
        return PropCurve.amountOutBid(room, st.pLow, st.pHigh, st.capacity, st.used, st.priceScaleExp);
    }

    /// @dev `amount + minReserveOut > reserveOut`, written as a subtraction.
    ///
    ///      The additive form panics 0x11 for any `amount` within `minReserveOut` of
    ///      `type(uint256).max`, and both `getAmountIn` and `swap`'s exact-output branch take that
    ///      amount straight from the caller. A panic inside a view breaks the no-revert promise
    ///      exactly as a named error would. The `reserveOut < minReserveOut` arm is not hypothetical
    ///      either: the manager can raise a floor above the inventory currently held.
    function _breachesFloor(PairState memory st, uint256 amount) private pure returns (bool) {
        return st.reserveOut < st.minReserveOut || amount > st.reserveOut - st.minReserveOut;
    }

    /// @dev Exact input. `amountIn` is base for a bid and quote for an ask; the return is the other
    ///      token. 0 for every unfillable condition, so the view path never reverts.
    function _outFor(PairState memory st, bool isBid, uint256 amountIn) private pure returns (uint256) {
        if (amountIn == 0) return 0;
        if (st.capacity == 0 || st.used >= st.capacity) return 0;
        if (amountIn > _maxAmountIn(st, isBid)) return 0;

        return isBid
            ? PropCurve.amountOutBid(amountIn, st.pLow, st.pHigh, st.capacity, st.used, st.priceScaleExp)
            : PropCurve.amountOutAsk(amountIn, st.pLow, st.pHigh, st.capacity, st.used, st.priceScaleExp);
    }

    /// @notice Smallest `amountIn` whose forward quote delivers at least `amountOut`. 0 if none.
    ///
    /// @dev Exact output. `amountOut` is quote for a bid and base for an ask; the return is the
    ///      other token. 0 for every unfillable condition.
    ///
    ///      **There is no search here any more, and deleting it was a correctness change as much
    ///      as a gas one.** This used to bisect `_outFor`, which is only valid if the forward curve
    ///      is monotone — a property the pre-amendment curve did not have, because it quantised the
    ///      intermediate price and one whole price unit of quantisation loss multiplied the entire
    ///      `amountIn`. The amended curve folds the price into the amount and rounds once, which
    ///      makes both directions non-decreasing on the whole domain unconditionally, and it ships
    ///      the two exact-output primitives itself: `amountInBid` bisects the *bid numerator*, with
    ///      a fixed-point bracket refinement PropPool could not reproduce, and `amountInAsk` is a
    ///      closed form. Re-deriving either here would be a second implementation of the same
    ///      algebra, free to drift from the one `swap` settles against.
    ///
    ///      Measured on a WETH/USDC-shaped pair, whole exact-out swap, first touch in the tx:
    ///      143,585 at `priceScaleExp = 18` and 116,789 at 24 — against 208,767 for the deleted
    ///      search. See `addPair` for why the exponent moves it that much.
    ///
    ///      Rounding is the curve's: up, i.e. the exact-output taker pays the sub-unit surplus.
    function _inFor(PairState memory st, bool isBid, uint256 amountOut) private pure returns (uint256) {
        if (amountOut == 0) return 0;
        if (st.capacity == 0 || st.used >= st.capacity) return 0;
        // Fillability probe. It also bounds `amountOut` by `MAX_AMOUNT_OUT` on the bid side, which
        // is what keeps `amountInBid`'s domain check out of reach — see `_maxAmountOut`.
        if (amountOut > _maxAmountOut(st, isBid)) return 0;

        return isBid
            ? PropCurve.amountInBid(amountOut, st.pLow, st.pHigh, st.capacity, st.used, st.priceScaleExp)
            : PropCurve.amountInAsk(amountOut, st.pLow, st.pHigh, st.capacity, st.used, st.priceScaleExp);
    }

    /// @dev The swap path's twin of `_outFor`: same conditions, named errors instead of a 0, so a
    ///      taker learns why. The capacity test is `_maxAmountIn` on both sides — for an ask that is
    ///      the quote cost of the epoch's remaining base, so an over-sized ask still reverts
    ///      `InsufficientCapacity` rather than surfacing `PropCurve.AmountExceedsCapacity`.
    function _resolveExactIn(uint16 pairId, bool isBid, uint256 amountIn) private view returns (uint256 amountOut) {
        PairState memory st = _loadChecked(pairId, isBid);
        // Short-circuit order matters: `_maxAmountIn` requires `used < capacity`.
        if (st.capacity == 0 || st.used >= st.capacity || amountIn > _maxAmountIn(st, isBid)) {
            revert InsufficientCapacity();
        }
        amountOut = isBid
            ? PropCurve.amountOutBid(amountIn, st.pLow, st.pHigh, st.capacity, st.used, st.priceScaleExp)
            : PropCurve.amountOutAsk(amountIn, st.pLow, st.pHigh, st.capacity, st.used, st.priceScaleExp);
        if (amountOut == 0) revert ZeroOutput();
        if (_breachesFloor(st, amountOut)) revert ReserveFloorBreached();
    }

    function _resolveExactOut(uint16 pairId, bool isBid, uint256 amountOut) private view returns (uint256 amountIn) {
        PairState memory st = _loadChecked(pairId, isBid);
        if (_breachesFloor(st, amountOut)) revert ReserveFloorBreached();
        amountIn = _inFor(st, isBid, amountOut);
        if (amountIn == 0) revert InsufficientCapacity();
    }

    // ---------------------------------------------------------------------
    // Internal: settlement
    // ---------------------------------------------------------------------

    /// @dev Word 2 and the two reserve counters, written after the external calls under the
    ///      transient lock. The used counters are re-derived from storage rather than carried
    ///      down from `_load` so that the generation rule is applied in exactly one place.
    ///
    ///      **Both counters advance by the trade's BASE leg**, which is `amountIn` for a bid and
    ///      `amountOut` for an ask. Charging the ask counter `amountIn` — as this did while
    ///      `askCapacity` was quote-denominated — now advances a base counter by a quote amount,
    ///      which on an 18/6 pair understates ask usage by ~12 orders of magnitude and effectively
    ///      removes the ask side's risk budget.
    ///
    ///      Neither uint96 field can overflow: the base leg was bounded by `capacity - used`
    ///      upstream, directly for a bid (`_maxAmountIn`) and for an exact-output ask
    ///      (`_maxAmountOut`), and by construction for an exact-input ask, since
    ///      `PropCurve.amountOutAsk` never returns more base than the epoch's remaining room.
    function _settle(
        uint16 pairId,
        bool isBid,
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        uint256 amountOut
    ) private {
        uint256 gen = (_capacityWord[pairId] >> SHIFT_GEN) & MASK_32;
        uint256 uWord = _usedWord[pairId];

        uint256 bidUsed;
        uint256 askUsed;
        if (((uWord >> SHIFT_GEN) & MASK_32) == gen) {
            bidUsed = uWord & MASK_96;
            askUsed = (uWord >> SHIFT_ASK_CAPACITY) & MASK_96;
        }

        if (isBid) bidUsed += amountIn;
        else askUsed += amountOut;

        _usedWord[pairId] = bidUsed | (askUsed << SHIFT_ASK_CAPACITY) | (gen << SHIFT_GEN);

        _reserve[tokenIn] += amountIn;
        _reserve[tokenOut] -= amountOut;
    }

}

// RFQ (signed, one-shot maker orders with confidence decay) is a separate contract — a fork of
// okxlabs/Web3-DEX-EVM-PMM's PmmProtocol — and shares nothing with this file but the inventory
// it would have to be granted. Not implemented here.
