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
/// The pool holds its own inventory and quotes a four-point price ladder per pair that an off-chain
/// engine pushes. There is no strategy on chain: everything expressive is folded into
/// `minBid/maxBid/minAsk/maxAsk` off-chain, and the chain enforces only coherence, freshness,
/// capacity and inventory floors. `PropCurve` holds the pricing; `docs/04-pool.md` the prose.
///
/// ## Storage shape
///
/// Three hot words per pair, each written by exactly one actor:
///
/// ```text
///   word 0, `updateQuote`      0..55 minBid, 56..111 maxBid, 112..167 minAsk,
///                              168..223 maxAsk, 224..255 updatedAt
///   word 1, `refreshCapacity`  0..95 bidCapacity, 96..191 askCapacity, 192..223 capGen,
///                              224..239 flags, 240..255 decaySecs
///   word 2, `swap`             0..95 bidUsed, 96..191 askUsed, 192..223 usedGen
/// ```
///
/// Raw `uint256` words rather than packed structs, so each writer's single-SSTORE property is a
/// fact about the source and not a hope about the optimiser.
///
/// Word 1's top two fields are **not** the updater's: `flags` bit 0 is the guardian's pause and
/// `decaySecs` is the manager's staleness ramp, and `refreshCapacity` preserves bits 224..255 with
/// a read-modify-write. The word is shared because `_load` reads it anyway, and a second mapping
/// would put a cold SLOAD on the swap and quote paths.
///
/// **Every capacity and usage field above is denominated in the pair's BASE token, on both
/// sides**, `askCapacity` included: `PropCurve` amendment 1 moved the ask side from quote to base,
/// because a quote-denominated output is the reciprocal of the midpoint and `1/p` is convex, which
/// made splitting an ask strictly dominant for the taker. The bit positions did not move, only the
/// unit. The base leg is `amountIn` for a bid and `amountOut` for an ask; that is what capacity
/// bounds and what `swap` adds to the used counter.
///
/// ## Generation mechanism
///
/// `refreshCapacity` bumps `capGen`; `swap` stamps `usedGen = capGen`; the used counters read as
/// zero whenever the two differ. So a price push costs one SSTORE and cannot restore the pool's
/// risk budget, which also denies an attacker a way to refill capacity by inducing price churn.
///
/// ## The reference-oracle deviation bound
///
/// **Pyth is a BOUND, never a price source.** Coherence is not correctness: a confidently wrong
/// feed, or a leaked updater key, produces a coherent ladder at the wrong level. If the ladder were
/// priced from Pyth and validated against Pyth the check would compare a number with itself and
/// pass unconditionally, so independence is the entire mechanism — never read `_referencePrice`
/// into the ladder.
///
/// Exactly two of the four prices are bounded, the two that bear the loss: `maxBid` at or below
/// `ref * (1 + maxDeviationBps)` and `minAsk` at or above `ref * (1 - maxDeviationBps)`. `minBid`
/// and `maxAsk` sit at the far end, where widening only moves them in the pool's favour, so
/// bounding them would price the pool's own *width* against the oracle.
///
/// The check runs in `updateQuote`, not `swap`: the threat is a wrong or compromised updater and
/// `updateQuote` is that actor's only write. It also keeps an external call — which a hostile or
/// unavailable callee could revert to poison an aggregator's multicall — out of `swap` and the
/// views. A configured feed with no establishable reference **reverts** the push rather than
/// falling through to the unbounded path, since fail-open would let whoever can stall Pyth remove
/// the bound; the **manager**, not the owner, can zero `feedId` to end such an outage.
///
/// ## Capacity that decays with quote age
///
///     effectiveCapacity = capacity * (decaySecs - age) / decaySecs,  0 at and past decaySecs
///
/// The shape is linear because the hazard is: a jump having landed since the last push has
/// probability `1 - e^{-λa} ≈ λa`, linear in the age `a`, while the loss *given* a jump does not
/// depend on `a`. A step (which `maxStaleSecs` alone is) manufactures an MEV race at its
/// discontinuity, a convex shape cuts hardest at small `a` where the hazard is smallest, and a
/// residual floor is a permanently exposed slice.
///
/// **It is a bound in the capacity guard, never an argument to `PropCurve`.** `PairState` carries
/// `capacity`, the nominal epoch every curve call still receives, and `available`, the decayed
/// bound `_maxAmountIn` / `_maxAmountOut` measure remaining room against. Feeding the decayed
/// number to the curve would re-slope the ladder, changing the *price* with age and breaking the
/// property `PropPool.invariant.t.sol`'s `invariant_quoteEqualsExecution` rests on:
///
///   1. At a fixed `block.timestamp` the view and the swap agree exactly, both computing
///      `available` from the same `_load`, the same words and the same arithmetic.
///   2. **Across timestamps the quote can only shrink to zero, never move to a different number.**
///      Ageing changes `available`; it changes neither `capacity`, `used`, nor any of the four
///      prices, so the curve sees identical arguments at every age for a fixed `amountIn`. All age
///      can do is push `amountIn` past `_maxAmountIn`, turning the quote into 0 and the swap into
///      `InsufficientCapacity`.
///
/// `decaySecs == 0` disables the ramp and is the default for every pair. It is **manager**-set, via
/// `setPairDecay`, for the same reason `maxDeviationBps` is: a hot key that could lengthen or zero
/// its own decay window would be bounded by nothing.
///
/// ## Roles
///
/// - `owner`    — rotates every role, adds pairs, withdraws inventory, sets Pyth. For a timelock.
/// - `manager`  — per-pair config: the reference bound, the decay window, inventory top-ups.
/// - `updater`  — `updateQuote` / `refreshCapacity` and nothing else. **Hard invariant: no function
///                reachable by the updater transfers a token or touches `_reserve`.** Assume this
///                key leaks: it can post bad-but-coherent prices, which is what the reference bound
///                is for, but it cannot set a feed id, a deviation limit, a staleness window, a
///                decay window or the Pyth address, so it cannot widen its own leash.
/// - `guardian` — pause only, held on separate hardware from the updater.
contract PropPool is IPropPool {
    using SafeTransfer for address;

    // --- Errors ---

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
    ///      Carries the reference so the updater's logs record what the chain saw, not just what
    ///      the bot sent.
    error BidCeilingExceeded(uint16 pairId, uint256 referencePrice, uint256 maxBid);
    /// @dev The ladder would have the pool SELL base below `referencePrice * (1 - maxDeviation)`.
    error AskFloorBreached(uint16 pairId, uint256 referencePrice, uint256 minAsk);
    /// @dev A feed is configured but no reference could be established. `status` is one of the
    ///      `REF_*` codes, so the revert says *which* way the oracle failed.
    error ReferenceUnavailable(uint16 pairId, uint8 status);

    // --- Additional events (the integration-visible ones live in IPropPool) ---

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
    /// @dev `decaySecs == 0` in this event means the ramp was switched off for the pair.
    event PairDecayUpdated(uint16 indexed pairId, uint16 decaySecs);

    // --- Bit layout constants ---

    uint256 private constant MASK_16 = 0xffff;
    uint256 private constant MASK_32 = 0xffffffff;
    uint256 private constant MASK_56 = 0xffffffffffffff;
    uint256 private constant MASK_96 = 0xffffffffffffffffffffffff;

    /// @dev Widest price the quote word can carry, and so the worst case `_refreshCapacity` must
    ///      assume when it bounds capacity against `PropCurve.MAX_AMOUNT_OUT`: prices arrive from a
    ///      different writer in a different transaction, so the capacity check cannot look at the
    ///      ladder that happens to be stored.
    uint256 private constant MAX_PRICE = MASK_56;

    /// @dev Bits 0..223 of a quote word: the four ladder prices, exactly as they arrive in
    ///      `updateQuote` calldata. The calldata layout coincides with the storage layout so the
    ///      update is a mask-and-or, not a re-pack.
    uint256 private constant LADDER_MASK = (uint256(1) << 224) - 1;

    uint256 private constant SHIFT_MAX_BID = 56;
    uint256 private constant SHIFT_MIN_ASK = 112;
    uint256 private constant SHIFT_MAX_ASK = 168;
    uint256 private constant SHIFT_UPDATED_AT = 224;

    uint256 private constant SHIFT_ASK_CAPACITY = 96;
    uint256 private constant SHIFT_GEN = 192;
    uint256 private constant SHIFT_FLAGS = 224;

    /// @dev Bits 240..255 of the capacity word: `decaySecs`, the manager's staleness ramp. Zero
    ///      disables. It sits in the updater's word but is not the updater's field — see
    ///      `_refreshCapacity`, which preserves everything from bit 224 up.
    uint256 private constant SHIFT_DECAY = 240;

    /// @dev Everything at or above bit 224 of the capacity word: the guardian's flags and the
    ///      manager's `decaySecs`. `_refreshCapacity` carries this mask across untouched, which is
    ///      what keeps the updater from writing either.
    uint256 private constant CAP_WORD_PRESERVED = ~((uint256(1) << SHIFT_FLAGS) - 1);

    /// @dev flags bit 0 (word bit 224) = paused; bits 1..13 reserved. **Bits 14 and 15 are not
    ///      stored flags and never will be** — `snapshot` synthesizes both, carved out of the top
    ///      of the range so a future stored flag can claim bit 1 without colliding.
    uint256 private constant FLAG_PAUSED = uint256(1) << SHIFT_FLAGS;

    /// @notice `snapshot().flags` bit 15: this pair has a reference-oracle feed configured.
    /// @dev Derived at read time, never stored. Zero means the pair quotes unbounded, a legitimate
    ///      configuration (see `setPairOracle`). `pairOracle(pairId)` is the detailed answer.
    uint16 internal constant FLAG_SNAPSHOT_BOUNDED = uint16(1) << 15;

    /// @notice `snapshot().flags` bit 14: this pair's capacity decays with the age of its quote.
    /// @dev Derived at read time from `decaySecs != 0`, never stored. It rides in `flags` because
    ///      `PairSnapshot` is a static tuple off-chain decoders mirror positionally and must not
    ///      grow a field. When set, `bidCapacity`/`askCapacity` are the *curve's* capacity and no
    ///      longer the fillable bound; `effectiveCapacity(pairId)` returns that bound.
    uint16 internal constant FLAG_SNAPSHOT_DECAYING = uint16(1) << 14;

    /// @dev `_route` value: bits 0..15 pairId (1-based), bit 16 set when `tokenIn` is the base.
    ///      A zero value therefore means "no such route" even for an ask on pair 1, which is why
    ///      pair ids start at 1 — `pairIdFor` has no third return value to signal "not found".
    uint256 private constant ROUTE_IS_BID = uint256(1) << 16;

    // --- Types ---

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
    ///      `pLow`/`pHigh` are `(minBid, maxBid)` for a bid and `(minAsk, maxAsk)` for an ask; in
    ///      both cases the price at zero usage is the taker-favourable end, so the curve argument
    ///      order is identical and the two sides share one code path.
    ///
    ///      `capacity`, `available` and `used` are BASE units on both sides.
    ///      `reserveOut`/`minReserveOut` are in `tokenOut` — the quote token for a bid, the base
    ///      token for an ask — so they are comparable with `capacity` on the ask side only.
    ///
    ///      **`capacity` and `available` are two different numbers.** `capacity` is the nominal
    ///      epoch the updater posted and the only one of the two any `PropCurve` call sees, so the
    ///      ladder's slope does not move as the quote ages. `available` is `capacity` scaled down
    ///      by `_decayed` and is what `_maxAmountIn` / `_maxAmountOut` measure remaining room
    ///      against. `available <= capacity` always, with equality at `decaySecs == 0` or age zero.
    struct PairState {
        address tokenOut;
        uint8 priceScaleExp;
        uint256 pLow;
        uint256 pHigh;
        uint256 capacity;
        uint256 available;
        uint256 used;
        uint256 reserveOut;
        uint256 minReserveOut;
    }

    /// @notice Per-pair reference-oracle configuration. Manager-set, updater-invisible.
    ///
    /// @dev Field order is load-bearing: `feedId` owns slot 0 alone and the three small fields
    ///      pack into slot 1, so `_referencePrice` returns on a zero `feedId` before touching slot
    ///      1 and an unbounded pair costs one cold SLOAD with no external call. Kept in its own
    ///      mapping so a fourth manager-owned word cannot disturb the single-SLOAD
    ///      freshness-and-scale read `_load` depends on.
    struct PairOracle {
        /// @notice Pyth price feed id. **Zero disables the bound for this pair entirely.**
        /// @dev Not every listable asset has a Pyth feed, so zero is a supported production state
        ///      rather than a placeholder, and it is reported in `snapshot().flags` bit 15.
        bytes32 feedId;
        /// @notice How far the ladder's taker-favourable ends may sit from the reference, in bps.
        /// @dev Capped at `BPS` (100%). At exactly `BPS` the ask floor degenerates to zero and the
        ///      bid ceiling to `2 * ref`, a legitimate "very loose" setting; above it the floor
        ///      arithmetic would underflow, so it is rejected rather than clamped.
        uint16 maxDeviationBps;
        /// @notice Tolerated distance between Pyth's `publishTime` and `block.timestamp`.
        /// @dev Ours, not Pyth's advisory `getValidTimePeriod`: how stale a *bound* may be is a
        ///      property of our risk model. Compared as an absolute difference, so a future-dated
        ///      price is stale in the same way a past-dated one is.
        uint32 maxPythStaleSecs;
        /// @notice Decimal shift from Pyth's own scale into this pair's `priceScaleExp` scale.
        ///
        /// @dev The number to check first in a review:
        ///
        ///          refExpo   = priceScaleExp + quoteDecimals - baseDecimals
        ///          poolPrice = pythPrice * 10**(pythExpo + refExpo)
        ///
        ///      Stored because it is fixed by the pair's decimals and its immutable
        ///      `priceScaleExp` — +12 for mWETH/mUSDC, +10 for mWBTC/mUSDC — while `pythExpo` is
        ///      read from the response every time because Pyth can change it. Signed, since a pair
        ///      with more base decimals than `priceScaleExp + quoteDecimals` needs a negative
        ///      shift. A wrong value fails **closed** both ways: `BidCeilingExceeded` if too small,
        ///      `AskFloorBreached` or `REF_INVALID` if too large.
        int8 refExpo;
    }

    uint256 private constant STATUS_OK = 0;
    uint256 private constant STATUS_UNKNOWN = 1;
    uint256 private constant STATUS_PAUSED = 2;
    uint256 private constant STATUS_STALE = 3;

    // --- Reference-bound constants and status codes ---

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

    /// @dev Largest scaled reference the bound will accept. Ladder prices are `uint56` (~7.2e16),
    ///      so a reference above `type(uint64).max` describes a broken `refExpo` or a broken feed
    ///      rather than a market move, and is reported as `REF_INVALID` rather than as a ladder
    ///      that is out of bound — different operator problems. It also keeps
    ///      `ref * (BPS + maxDeviationBps)` at most ~3.7e23, far from overflow.
    uint256 private constant MAX_REFERENCE_PRICE = type(uint64).max;

    /// @dev Bound on `pythExpo + refExpo` before the power-of-ten is formed. `10**58` multiplied
    ///      by the widest `int64` price (~9.2e18) is ~9.2e76, still inside `uint256`; anything
    ///      beyond would overflow before `MAX_REFERENCE_PRICE` could reject it. Real values are
    ///      +4 and +2, so this only ever fires on a misconfiguration or a feed with an absurd
    ///      exponent.
    int256 private constant MAX_NET_EXPONENT = 58;

    // --- Storage ---

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
    /// @dev Storage rather than `immutable` because an oracle root of trust should be rotatable by
    ///      the timelock rather than frozen at deploy. Read lazily inside `_referencePrice` and
    ///      never before `feedId` has proved non-zero, so a pool with no bounded pairs never
    ///      touches it.
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
    /// @dev Deliberately *not* `balanceOf`: the reserve floor must not be satisfiable by a donation
    ///      the pool cannot rely on, the view path must make no external call a hostile token could
    ///      revert to poison an aggregator's batch, and `swapWithContractBalance` needs a baseline
    ///      to measure the caller's push against. `sync` folds donations and rebases in explicitly.
    mapping(address => uint256) private _reserve;

    // --- Modifiers ---

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

    // --- Construction ---

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

    // --- Role rotation — all four independent, all owner-driven ---

    /// @notice Two-step so a typo in the timelock address cannot orphan the pool. The other three
    ///         roles are one-step: they are recoverable by the owner, ownership is not.
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

    // --- Pair administration ---

    /// @notice Register a new (base, quote) pair and return its id.
    /// @dev Rejects both token orderings, so `WETH/USDC` and `USDC/WETH` can never coexist as two
    ///      pairs with independent, mutually arbitrageable ladders.
    /// @param priceScaleExp decimal alignment: `quoteAmount = baseAmount * price / 10**exp`.
    ///                      Immutable afterwards, since changing it reprices every stored ladder by
    ///                      orders of magnitude. **Pick the largest exponent that keeps the pair's
    ///                      prices inside `uint56`, not the smallest that works**: the bisected
    ///                      directions converge in about `log2(size / price)` steps, ~36k gas
    ///                      between exponent 18 and 25, so it is a gas parameter too.
    /// @param minPrice      absolute floor on `minBid`, independent of any oracle. Non-zero, so a
    ///                      compromised updater cannot post a zero bid, and so `minAsk > 0` keeps
    ///                      the inverse curve well defined.
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
    /// @dev Raising `minPrice` above a stored ladder would make every quote unfillable until the
    ///      next push, and leave the floor not describing the price the pool will actually pay.
    ///      Reverting forces the order: push a compliant ladder first, raise the floor second.
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

    /// @notice Set, retune, or disable one pair's capacity decay window.
    ///
    /// @dev Manager-gated because a stale ladder is what a failed, wedged or leaked updater
    ///      produces, so a key that could raise or zero its own `decaySecs` could restore its own
    ///      full exposure.
    ///
    ///      Nothing is validated, because every `uint16` names a coherent policy, the two that look
    ///      wrong included: below `maxStaleSecs` it is a soft freshness requirement costing a slice
    ///      of depth per missed heartbeat rather than the whole book, above it a partial haircut
    ///      that never completes. The two windows are independent.
    /// @param decaySecs age at which fillable depth reaches zero, in seconds. **Zero disables.**
    function setPairDecay(uint16 pairId, uint16 decaySecs) external onlyManager {
        if (pairId == 0 || pairId > pairCount) revert UnknownPair();

        uint256 word = _capacityWord[pairId];
        _capacityWord[pairId] = (word & ~(MASK_16 << SHIFT_DECAY)) | (uint256(decaySecs) << SHIFT_DECAY);

        emit PairDecayUpdated(pairId, decaySecs);
    }

    // --- Reference-oracle administration ---
    //
    // Owner sets the oracle contract; manager sets the per-pair terms. The updater appears
    // nowhere: a key that could raise its own `maxDeviationBps`, lengthen its own
    // `maxPythStaleSecs` or zero its own `feedId` would be bounded by nothing.

    /// @notice Point the pool at a Pyth deployment.
    /// @dev Owner-gated because this is a root of trust rather than a risk parameter: whoever sets
    ///      it decides what "independent" means for every bounded pair at once. On GIWA Sepolia the
    ///      argument is the genesis pre-install `0x2880aB155794e7179c9eE2e38200202908C17B43`. The
    ///      zero address is rejected because unsetting it would break every configured pair into
    ///      `REF_UNAVAILABLE`, halting every push with no per-pair record of why. Disabling the
    ///      bound is `setPairOracle(pairId, 0, ...)`.
    function setPyth(address newPyth) external onlyOwner {
        if (newPyth == address(0)) revert ZeroAddress();
        emit PythUpdated(pyth, newPyth);
        pyth = newPyth;
    }

    /// @notice Set, retune, or disable one pair's reference bound.
    ///
    /// @dev **This function does not read Pyth.** The check fails closed, so an operator must be
    ///      able to point a pair at a feed before that feed's first on-chain push, and to *disable*
    ///      a bound while Pyth is down without needing Pyth to answer. The cost is that a typo'd
    ///      `feedId` or a wrong `refExpo` is not caught here; `referencePrice(pairId)` is the
    ///      preflight, and it is a free `eth_call` that never reverts.
    ///
    /// @param feedId           Pyth price feed id, or **zero to disable the bound for this pair**.
    ///                         The remaining arguments are stored but unread while it is zero.
    /// @param maxDeviationBps  tolerance on `maxBid` above and `minAsk` below the reference.
    /// @param maxPythStaleSecs our freshness window on Pyth's `publishTime`, not Pyth's own.
    /// @param refExpo          `priceScaleExp + quoteDecimals - baseDecimals`. See `PairOracle`.
    function setPairOracle(uint16 pairId, bytes32 feedId, uint16 maxDeviationBps, uint32 maxPythStaleSecs, int8 refExpo)
        external
        onlyManager
    {
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
            feedId: feedId, maxDeviationBps: maxDeviationBps, maxPythStaleSecs: maxPythStaleSecs, refExpo: refExpo
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
    ///      preflight for a push `updateQuote` would reject. `status` is `REF_OK` only when `price`
    ///      is meaningful; every other code returns `price == 0`. Scaled rather than raw, so it is
    ///      directly comparable with `snapshot(pairId).maxBid`/`.minAsk`.
    function referencePrice(uint16 pairId) external view returns (uint256 price, uint8 status) {
        return _referencePrice(pairId);
    }

    // --- Updater surface — writes prices and capacity, moves no tokens ---

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
    /// @dev One SSTORE per pair: the write is a mask-and-or of the calldata word, with no
    ///      re-packing and no second slot. Existence is checked against `pairCount` rather than
    ///      `PairConfig.exists` to avoid a second cold SLOAD per entry.
    ///
    ///      **The reference-oracle bound is enforced here**, immediately after `validateLadder`,
    ///      which can only reject an *incoherent* book and not a wrong one. A pair with no feed
    ///      costs +111 gas, which makes it viable to batch bounded and unbounded markets together.
    ///
    ///      The batch is atomic: one out-of-bound pair reverts every pair in the call. Partial
    ///      application would leave the updater guessing which ladders landed, and those that did
    ///      were priced by a feed that just proved itself wrong on another pair.
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
    /// @param askCapacity total base the pool will *sell* this epoch, also in base units — not
    ///                    quote; see `PropCurve` amendment 1.
    function refreshCapacity(uint16 pairId, uint96 bidCapacity, uint96 askCapacity) external onlyUpdater {
        if (pairId == 0 || pairId > pairCount) revert UnknownPair();
        _refreshCapacity(pairId, bidCapacity, askCapacity);
    }

    /// @notice Batched capacity refresh.
    ///
    ///   bits   0..95  bidCapacity (base units)
    ///   bits  96..191 askCapacity (base units, not quote — see `PropCurve` amendment 1)
    ///   bits 224..239 pairId      — same position as in `updateQuote`, so both encoders on the
    ///                               engine side share one field writer
    function refreshCapacityBatch(uint256[] calldata packed) external onlyUpdater {
        uint256 n = pairCount;
        uint256 len = packed.length;
        for (uint256 i; i < len;) {
            uint256 word = packed[i];
            uint256 pairId = (word >> SHIFT_FLAGS) & MASK_16;
            if (pairId == 0 || pairId > n) revert UnknownPair();
            _refreshCapacity(uint16(pairId), uint96(word & MASK_96), uint96((word >> SHIFT_ASK_CAPACITY) & MASK_96));
            unchecked {
                ++i;
            }
        }
    }

    /// @dev Read-modify-write because the top 32 bits of the word are not the updater's: the paused
    ///      flag is the guardian's and `decaySecs` the manager's, and `CAP_WORD_PRESERVED` carries
    ///      both across untouched. Splitting the word would move the extra SLOAD from this
    ///      infrequent path onto the swap path. `capGen` wraps at 2^32 harmlessly — only equality
    ///      against `usedGen` matters, and a wrap resurrecting a stale `used` counter would need
    ///      2^32 refreshes between a swap and the matching epoch.
    ///
    ///      ## The domain bound, and why it lives here
    ///
    ///      `PropCurve` closes the domain it shares with the Rust mirror at
    ///      `MAX_AMOUNT_OUT == type(uint128).max`. Capacity, price and `priceScaleExp` have three
    ///      different writers — this function, `updateQuote` and `addPair` — so the only site that
    ///      can bound their product without reading another actor's word is this one, by assuming
    ///      the worst price the field can hold:
    ///
    ///          capacity * type(uint56).max <= MAX_AMOUNT_OUT * 10**priceScaleExp
    ///
    ///      That keeps `amountOutBid(capacity, ...)` and `amountInAsk(capacity, ...)` in-domain for
    ///      *every* ladder the updater can subsequently post, which is what lets `_outFor` and
    ///      `_inFor` probe the epoch's whole remaining depth without a `PropCurve` revert escaping
    ///      through a view. **The two are one mechanism: do not weaken this check without
    ///      re-reading `_maxAmountIn`.** It restricts nothing in practice — for any
    ///      `priceScaleExp >= 8` the bound exceeds `type(uint96).max`.
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
            | (uint256(gen) << SHIFT_GEN) | (word & CAP_WORD_PRESERVED);

        emit CapacityRefreshed(pairId, bidCapacity, askCapacity, gen);
    }

    // --- Guardian surface ---

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

    /// @notice Counterpart to `pauseAll`. Guardian-held because requiring a timelock to resume
    ///         would turn every false alarm into a multi-hour outage.
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

    // --- Inventory ---

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
    /// @dev The escape hatch for donations, for a pair funded by a plain transfer instead of
    ///      `deposit`, and for a token that rebased downward. Manager-gated because it can raise
    ///      the reserve floor headroom, a risk parameter.
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

    // --- Quoting — every path here returns 0 instead of reverting ---

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
    /// @dev Returns the raw stored counters, both generations included, rather than the effective
    ///      ones: a simulator needs `usedGen != capGen` to reproduce the contract's own rule.
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

        // Bit 15, synthesized rather than stored. It says a bound is configured, NOT that the bound
        // is currently satisfiable — that would mean reading Pyth, and `snapshot` owes the same
        // no-revert contract as the quoting views. `referencePrice` answers that question.
        if (_oracle[pairId].feedId != bytes32(0)) s.flags |= FLAG_SNAPSHOT_BOUNDED;

        // Bit 14, likewise derived. `bidCapacity` and `askCapacity` here are the NOMINAL epoch, the
        // number `PropCurve` is priced on; when this bit is set they are no longer the fillable
        // bound, which `effectiveCapacity(pairId)` returns along with the window it ramps over.
        if ((c >> SHIFT_DECAY) & MASK_16 != 0) s.flags |= FLAG_SNAPSHOT_DECAYING;

        s.bidUsed = uint96(u & MASK_96);
        s.askUsed = uint96((u >> SHIFT_ASK_CAPACITY) & MASK_96);
        s.usedGen = uint32((u >> SHIFT_GEN) & MASK_32);

        s.priceScaleExp = cfg.priceScaleExp;
        s.maxStaleSecs = cfg.maxStaleSecs;
    }

    /// @inheritdoc IPropPool
    ///
    /// @dev Two SLOAD-heavy `_load` calls where one hand-rolled read would do, so that this reports
    ///      **exactly** what the swap path enforces; drift means an integrator's router sizes a
    ///      trade the pool then refuses. Totality is inherited from `_load`, so unknown pair,
    ///      never-quoted pair, paused, globally paused and past the staleness cliff all read
    ///      `(0, 0, decaySecs)`. `decaySecs` is returned even at zero depth, because "zero because
    ///      paused" and "zero because the ramp completed" are different operator problems.
    function effectiveCapacity(uint16 pairId)
        external
        view
        returns (uint96 bidCapacity, uint96 askCapacity, uint16 decaySecs)
    {
        decaySecs = uint16((_capacityWord[pairId] >> SHIFT_DECAY) & MASK_16);

        (uint256 bidStatus, PairState memory bid) = _load(pairId, true);
        if (bidStatus == STATUS_OK) bidCapacity = uint96(bid.available);

        (uint256 askStatus, PairState memory ask) = _load(pairId, false);
        if (askStatus == STATUS_OK) askCapacity = uint96(ask.available);
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

    // --- Swapping ---

    /// @inheritdoc IPropPool
    /// @dev Guard order is deadline, existence, pause, freshness, capacity, reserve floor,
    ///      slippage — cheapest and most-likely-to-fail first, and slippage last so a taker's limit
    ///      is judged against a price that has already passed every safety gate.
    ///
    ///      `specifiedAmount` is denominated in whichever leg it pins: base for an exact-input bid
    ///      or an exact-output ask, quote for the other two. Capacity is charged in base on both
    ///      sides regardless, so the epoch's budget sees `amountIn` for a bid and `amountOut` for
    ///      an ask.
    ///
    ///      Word 2 is written after the transfers. That is not CEI; the transient lock is what
    ///      makes it safe. The residual surface is read-only re-entrancy — a hostile `tokenOut` can
    ///      `getAmountOut` from inside its own transfer and see pre-trade `used` — which it cannot
    ///      act on in the same transaction. `partnerId` is recorded and otherwise ignored.
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
            // `_inFor` rounds the required input up, so the pool may hold a sub-unit surplus —
            // the price of never rounding in the taker's favour.
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
    /// @dev The input amount is `balanceOf(tokenIn) - _reserve[tokenIn]`: everything that arrived
    ///      since the pool last accounted for that token. `IPropPool`'s caveat is literal and
    ///      load-bearing — any *other* transfer of `tokenIn` landing before this call, a second
    ///      router's push or an airdrop, is credited to this caller. Batch nothing else into the
    ///      same transaction, and do not use this path for a token the pool receives from anywhere
    ///      but routers.
    ///
    ///      Exact-in only: exact-out over a pushed balance would require refunding the surplus,
    ///      reintroducing the approval hop this path exists to avoid.
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

    // --- Internal: state loading ---

    /// @dev Single source of truth for "can this pair trade right now, and on what numbers".
    ///      Returns a status rather than reverting, so the view path can turn it into a zero and
    ///      the swap path into a specific error without the two drifting apart.
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
        uint256 age;
        unchecked {
            age = block.timestamp - updatedAt; // the line above establishes the direction
        }
        if (age > cfg.maxStaleSecs) return (STATUS_STALE, st);

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

        // The ladder has a single `updatedAt`, so both sides are equally old and one factor serves.
        // `capacity` keeps the nominal number the curve is priced on; only the bound moves.
        st.available = _decayed(st.capacity, age, (capWord >> SHIFT_DECAY) & MASK_16);

        uint256 uWord = _usedWord[pairId];
        if (((uWord >> SHIFT_GEN) & MASK_32) == ((capWord >> SHIFT_GEN) & MASK_32)) {
            st.used = isBid ? (uWord & MASK_96) : ((uWord >> SHIFT_ASK_CAPACITY) & MASK_96);
        }

        st.reserveOut = _reserve[st.tokenOut];
    }

    /// @dev The one place the decay shape is written down, shared by every quote, every swap and
    ///      `effectiveCapacity` through `_load`, so the three cannot drift.
    ///
    ///      Exact at both ends: `age == 0` gives `capacity` with no rounding loss, and
    ///      `age == decaySecs` gives zero rather than a unit of dust, the comparison being `>=`.
    ///      Flooring is the pool-favourable direction, and it is integer arithmetic with no
    ///      fixed-point library, so an off-chain simulator reproduces the number exactly.
    ///
    ///      No overflow: `capacity` is `uint96`-derived and `decaySecs` `uint16`-derived, so the
    ///      product is below 2^112, and the `age >= decaySecs` arm guards the subtraction.
    function _decayed(uint256 capacity, uint256 age, uint256 decaySecs) private pure returns (uint256) {
        if (decaySecs == 0) return capacity;
        if (age >= decaySecs) return 0;
        return (capacity * (decaySecs - age)) / decaySecs;
    }

    // --- Internal: the reference bound ---
    //
    // Read the contract header before changing anything here: Pyth is a bound and never a price
    // source, and the check runs at push time rather than at swap time.

    /// @dev The reference price in this pair's own units, or a status explaining why there isn't
    ///      one. **Total — no input and no oracle state makes this revert**, which is required
    ///      because `referencePrice` exposes it as a view under `IPropPool`'s no-revert contract.
    ///      Totality rests on four guards, Solidity offering no single one: the `feedId == 0`
    ///      short-circuit (also the gas argument — an unbounded pair pays one cold SLOAD and
    ///      stops); the `p.code.length == 0` test, which must precede the call because `try`/
    ///      `catch` does **not** catch a return-data decoding failure and a call to a codeless
    ///      address succeeds returning nothing; `catch` for the ordinary revert; and range-limiting
    ///      the exponent before `10 **` forms it. An oracle that burns all the gas is not a hole —
    ///      by EIP-150 the callee gets 63/64 and reverts into `catch`, and the retained 1/64 covers
    ///      the arithmetic that follows, so add no non-arithmetic work after the `try` block.
    ///
    ///      `getPriceUnsafe` rather than `getPriceNoOlderThan`: the latter reverts `StalePrice`,
    ///      which `catch` would flatten into `REF_UNAVAILABLE`, losing the distinction between
    ///      "Pyth is gone" and "Pyth is behind". Comparing here also keeps the window ours.
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

        // Absolute difference, matching Pyth's own `diff`: a reference is only a reference once it
        // has been observed, so a future-dated price is stale in the way a past-dated one is.
        uint256 publishTime = quoted.publishTime;
        uint256 age = publishTime > block.timestamp ? publishTime - block.timestamp : block.timestamp - publishTime;
        if (age > o.maxPythStaleSecs) return (0, REF_STALE);

        // Rejected, not cast. `price` is `int64` and Pyth feeds for rates and spreads do go
        // negative; a wrapping cast would turn -1 into ~1.8e19, a bound the ladder trivially
        // satisfies on one side and trivially fails on the other. Zero would pin the bid ceiling.
        if (quoted.price <= 0) return (0, REF_INVALID);

        // `pythExpo` comes off the response every time: it is per-feed and Pyth can change it.
        int256 net = int256(quoted.expo) + int256(o.refExpo);
        if (net > MAX_NET_EXPONENT || net < -MAX_NET_EXPONENT) return (0, REF_INVALID);

        // Every cast below is discharged by the two lines above: `quoted.price > 0` makes the
        // widening to `uint256` value-preserving, and `|net| <= 58` plus the branch's sign test
        // makes `uint256(net)` and `uint256(-net)` small positive numbers, the latter also out of
        // reach of the `-type(int256).min` trap.
        uint256 raw = uint256(int256(quoted.price));
        if (net >= 0) {
            price = raw * (10 ** uint256(net));
        } else {
            // Floors asymmetrically: the truncation tightens the bid ceiling and loosens the ask
            // floor by up to one unit in the last place. Prefer a `priceScaleExp` keeping
            // `net >= 0`, which `addPair`'s largest-exponent rule already pushes towards.
            price = raw / (10 ** uint256(-net));
        }

        // `price == 0` catches the division collapsing the reference to nothing, which is what a
        // badly wrong `refExpo` looks like from here. The upper bound is explained at the constant.
        if (price == 0 || price > MAX_REFERENCE_PRICE) return (0, REF_INVALID);

        status = REF_OK;
    }

    /// @dev Called from `updateQuote` immediately after `validateLadder`, in that order so an
    ///      incoherent book reports as incoherent rather than as off-reference. Only `maxBid` and
    ///      `minAsk` are passed, because only those two are bounded.
    ///
    ///      Fails closed on every non-OK status except `REF_DISABLED`, which is a configuration the
    ///      operator chose and returns silently.
    ///
    ///      No overflow: `maxBid` and `minAsk` are `uint56` so the left sides reach ~7.3e20, and
    ///      `ref` is capped at `MAX_REFERENCE_PRICE` with `dev <= BPS` so the right sides reach
    ///      ~3.7e23.
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

    // --- Internal: curve application ---
    //
    // Capacity and usage are base-denominated on both sides, so capacity bounds the input of a bid
    // and the output of an ask, and the other leg must go through the curve before it can be
    // compared with anything: both are `uint96`, so comparing an ask `amountIn` against
    // `askCapacity` compiles and is wrong by a factor of the price. `PropCurve` ships exact,
    // unconditionally monotone primitives in all four directions, so there is no bisection here.
    //
    // Every helper is total, returning 0 rather than letting `PropCurve` revert, which is the
    // obligation `IPropPool` states for the view path. `_maxAmountIn` documents the part of that
    // which is not local to this section.

    /// @dev The largest `amountIn` this side's epoch can accept, in the direction's INPUT token.
    ///      Callers must have established `available > 0 && used < available`.
    ///
    ///      For a bid the input *is* the base leg, so this is the remaining room. For an ask the
    ///      input is quote and the room is base, so the ceiling is the quote cost of all the
    ///      remaining base — which is why a plain `askUsed + amountIn > askCapacity` test cannot be
    ///      resurrected: those two are base units and `amountIn` is not.
    ///
    ///      **The room is measured against `available`, the cost against `capacity`**, because the
    ///      ramp is a bound and not a repricing.
    ///
    ///      The ramped ask ceiling is `cost(room + 1) - 1`, not `cost(room)`: `cost` is *ceiled*
    ///      into quote units, so many consecutive base amounts share a cost and `cost(room)` is
    ///      usually also the cost of `room + 1`, which `amountOutAsk` would then deliver. Clamping
    ///      the output instead would give `min(curveOut, room)` for the same `amountIn` — a third
    ///      possible value for an aged quote, which the header's property 2 promises does not
    ///      exist. The `available == capacity` arm preserves the un-ramped path bit-for-bit.
    ///
    ///      **This call cannot itself revert, and the reason is not local.** `_refreshCapacity`'s
    ///      bound keeps `cost(capacity - used)` inside `PropCurve.MAX_AMOUNT_OUT` for every ladder
    ///      the updater can post, and `cost` is non-decreasing, so `room + 1` is in-domain too;
    ///      `ZeroCapacity` and `AmountExceedsCapacity` are excluded by the caller's
    ///      `0 < available <= capacity`; and `ZeroPrice` needs `maxAsk == 0`, which `STATUS_OK`
    ///      excludes via `validateLadder`'s `maxAsk > minBid >= minPrice >= 1`.
    function _maxAmountIn(PairState memory st, bool isBid) private pure returns (uint256) {
        uint256 room = st.available - st.used;
        if (isBid) return room;
        if (st.available == st.capacity) {
            return PropCurve.amountInAsk(room, st.pLow, st.pHigh, st.capacity, st.used, st.priceScaleExp);
        }
        return PropCurve.amountInAsk(room + 1, st.pLow, st.pHigh, st.capacity, st.used, st.priceScaleExp) - 1;
    }

    /// @dev The largest `amountOut` this side's epoch can deliver, in the direction's OUTPUT token:
    ///      the mirror of `_maxAmountIn`, revert-free for the same reasons and with the same split
    ///      between `available` for the room and `capacity` for the price. Doubles as the
    ///      fillability probe for the exact-output paths.
    function _maxAmountOut(PairState memory st, bool isBid) private pure returns (uint256) {
        uint256 room = st.available - st.used;
        if (!isBid) return room;
        return PropCurve.amountOutBid(room, st.pLow, st.pHigh, st.capacity, st.used, st.priceScaleExp);
    }

    /// @dev `amount + minReserveOut > reserveOut`, written as a subtraction because the additive
    ///      form panics 0x11 for any `amount` within `minReserveOut` of `type(uint256).max`, and
    ///      both `getAmountIn` and `swap`'s exact-output branch take that amount straight from the
    ///      caller — a panic inside a view breaks the no-revert promise as surely as a named error.
    ///      The `reserveOut < minReserveOut` arm is reachable: the manager can raise a floor above
    ///      the inventory currently held.
    function _breachesFloor(PairState memory st, uint256 amount) private pure returns (bool) {
        return st.reserveOut < st.minReserveOut || amount > st.reserveOut - st.minReserveOut;
    }

    /// @dev Exact input. `amountIn` is base for a bid and quote for an ask; the return is the other
    ///      token. 0 for every unfillable condition, so the view path never reverts.
    ///
    ///      The exhaustion test is against `available`, not `capacity`, which is what makes a fully
    ///      decayed ladder quote zero. `used` is *not* scaled by the ramp — it is base the pool
    ///      genuinely traded — so the ramp bounds the depth still exposed, `available - used`, and
    ///      a side that spent its epoch while fresh reads as exhausted sooner.
    function _outFor(PairState memory st, bool isBid, uint256 amountIn) private pure returns (uint256) {
        if (amountIn == 0) return 0;
        if (st.available == 0 || st.used >= st.available) return 0;
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
    ///      **There is deliberately no search here.** `PropCurve` ships both exact-output
    ///      primitives itself: `amountInBid` bisects the *bid numerator* with a fixed-point
    ///      bracket refinement PropPool could not reproduce, and `amountInAsk` is a closed form.
    ///      Re-deriving either here would be a second implementation of the same algebra, free to
    ///      drift from the one `swap` settles against.
    ///
    ///      Rounding is the curve's: up, i.e. the exact-output taker pays the sub-unit surplus.
    function _inFor(PairState memory st, bool isBid, uint256 amountOut) private pure returns (uint256) {
        if (amountOut == 0) return 0;
        if (st.available == 0 || st.used >= st.available) return 0;
        // Fillability probe. It also bounds `amountOut` by `MAX_AMOUNT_OUT` on the bid side, which
        // is what keeps `amountInBid`'s domain check out of reach — see `_maxAmountOut`.
        if (amountOut > _maxAmountOut(st, isBid)) return 0;

        return isBid
            ? PropCurve.amountInBid(amountOut, st.pLow, st.pHigh, st.capacity, st.used, st.priceScaleExp)
            : PropCurve.amountInAsk(amountOut, st.pLow, st.pHigh, st.capacity, st.used, st.priceScaleExp);
    }

    /// @dev The swap path's twin of `_outFor`: same conditions, named errors instead of a 0. The
    ///      capacity test is `_maxAmountIn` on both sides, so an over-sized ask reverts
    ///      `InsufficientCapacity` rather than surfacing `PropCurve.AmountExceedsCapacity`. A size
    ///      refused because the ladder aged past its ramp gets the same error, the ramp being a
    ///      capacity bound; `effectiveCapacity(pairId)` separates the two.
    function _resolveExactIn(uint16 pairId, bool isBid, uint256 amountIn) private view returns (uint256 amountOut) {
        PairState memory st = _loadChecked(pairId, isBid);
        // Short-circuit order matters: `_maxAmountIn` requires `used < available`.
        if (st.available == 0 || st.used >= st.available || amountIn > _maxAmountIn(st, isBid)) {
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

    // --- Internal: settlement ---

    /// @dev Word 2 and the two reserve counters, written after the external calls under the
    ///      transient lock. The used counters are re-derived from storage rather than carried down
    ///      from `_load`, so the generation rule is applied in one place.
    ///
    ///      **Both counters advance by the trade's BASE leg**, `amountIn` for a bid and `amountOut`
    ///      for an ask. Charging the ask counter `amountIn` would advance a base counter by a quote
    ///      amount, understating ask usage by ~12 orders of magnitude on an 18/6 pair and
    ///      effectively removing the ask side's risk budget.
    ///
    ///      Neither uint96 field can overflow: the base leg was bounded upstream by
    ///      `capacity - used` — via `_maxAmountIn` for a bid, `_maxAmountOut` for an exact-output
    ///      ask, and by construction for an exact-input ask, since `PropCurve.amountOutAsk` never
    ///      returns more base than the epoch's remaining room.
    function _settle(uint16 pairId, bool isBid, address tokenIn, address tokenOut, uint256 amountIn, uint256 amountOut)
        private
    {
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
