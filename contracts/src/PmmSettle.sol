// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {IPmmSettle} from "./interfaces/IPmmSettle.sol";
import {SafeTransfer} from "./libraries/SafeTransfer.sol";

/// @title PmmSettle
/// @notice On-chain settlement for EIP-712 signed maker orders — the RFQ leg of the DuBu prop AMM.
///
/// # Custody: none
///
/// This contract never holds a token, not between transactions and not during one. Both legs are
/// `transferFrom` pulls issued inside `fillOrder`:
///
/// ```text
///   takerAsset:  msg.sender  ->  maker      (the filler must have approved this contract)
///   makerAsset:  maker       ->  receiver   (the maker must have approved this contract)
/// ```
///
/// The alternative — the maker pre-deposits inventory here and settlement pays out of it — was
/// rejected, and the reason is the same one that makes `IAdapter` custody-free. A contract that
/// holds standing inventory has a balance to drain, needs a withdrawal path, needs an owner to
/// authorise that path, and then needs a pause for when the owner's key leaks. `Router` and both
/// existing adapters have none of those, and the honest description of this repo's threat model
/// is "there is nothing to seize and nothing to stop". Adding a vault to the RFQ leg would make
/// that sentence false for the whole system, in exchange for saving the maker one `approve`.
///
/// The cost is stated rather than hidden: the maker's approval to this contract is a standing
/// allowance, so a bug here is a bug with reach into the maker's inventory. That allowance is the
/// single trust concentration in this file, which is why the file is short and why every branch is
/// pinned by a test. It is also the maker's own risk and nobody else's — a taker's exposure is
/// bounded by the exact allowance they grant for one fill.
///
/// # Caveat 2: partial fills, and exactly how they interact with the decay
///
/// `archi_v2.md` §4.6 caveat 2 records that the forked design burns the order id *before* the
/// fill, keeps no remaining-amount accounting, and settles 60-100% of an order in a single take
/// with the balance forfeited. One quote therefore serves one taker, once. That is the substantive
/// thing this contract does differently.
///
/// The accounting is kept in **taker units**, in `_filledTaker[orderHash]`, and it is exact:
///
/// ```text
///   remaining(h)  = order.takerAmount - _filledTaker[h]
///   a fill of `t` requires  t <= remaining(h)   and sets  _filledTaker[h] += t
/// ```
///
/// so the taker legs of any sequence of fills sum to **at most** `takerAmount`, and a request for
/// one unit more than the remainder reverts rather than being clamped. Taker units are the right
/// side to account in because the taker leg is the exact-in side: the router pushes taker tokens
/// and the adapter reads its own balance, so the taker leg is a number the chain measured and the
/// maker leg is a number derived from it.
///
/// The maker leg of a fill is pro-rata at the signed rate, floored:
///
/// ```text
///   quoted(t) = floor(makerAmount * t / takerAmount)
/// ```
///
/// which is where the same splitting argument `PropCurve` makes for the curve applies here, for
/// the same reason and with the same bound. `sum_i floor(M*t_i/T) <= floor(M*(sum_i t_i)/T) <= M`,
/// so **no decomposition of an order ever draws more maker asset than filling it whole**, and the
/// shortfall is at most `n - 1` units for `n` pieces — one unit per extra piece, kept by the
/// maker. There is deliberately no "sweep the remainder on the last fill" rule: which fill is last
/// is not knowable when the earlier ones settle, and a rule that paid the dust to whoever happened
/// to close the order would make fill order economically observable.
///
/// The decay composes on top of that, per fill and independently:
///
/// ```text
///   filled(t, now) = floor( quoted(t) * (1e6 - decay(now)) / 1e6 )
/// ```
///
/// `decay` is a function of the settlement time only, never of how much has already been filled.
/// Two consequences worth stating because neither is accidental:
///
///   * fills are **priced independently** — an early fill is not made worse by a later one, so a
///     streamed quote does not penalise whoever arrives first;
///   * the decay is **not** a capacity mechanism. It does not deepen as the order is consumed the
///     way `PropCurve`'s ladder does with usage. Ladder impact prices size; this prices *age*.
///     Anything the maker wants to charge for size belongs in the rate they signed, or on the
///     curve path, which is built for it.
///
/// # Caveat 1: the decay is one-directional, so it must be visible
///
/// The decay only ever moves value from the taker to the maker: the taker pays `takerAmountIn` in
/// full and receives less as the quote ages. That asymmetry is defensible — it prices the option
/// the taker holds between signing and inclusion — but a firm quote shown at T that quietly
/// settles worse at T+delta is, from the outside, indistinguishable from the prop-AMM quote
/// spoofing this project was built to eliminate. We also run the aggregator, so "we did not
/// notice" is not available to us.
///
/// Three mechanisms, at three different times, and all three are required:
///
///   * **before** — [`previewFill`] returns the quoted and realised amounts side by side at a
///     caller-chosen timestamp, `pure`, so the price a taker is shown already has the decay in it;
///   * **during** — `maxDecayPpm` on [`fillOrder`] lets the taker bound the decay and revert
///     rather than accept a surprise;
///   * **after** — [`IPmmSettle.OrderFilled`] carries `makerAmountQuoted` next to
///     `makerAmountFilled`, so the difference is on chain for anyone to sum.
///
/// # Caveat 3: the settlement floor is checked AFTER the decay
///
/// The forked design checks its 60% floor against the pre-decay maker amount and then applies up
/// to 5% of decay, so an order advertising a 60% minimum delivers 57%. The check here is on
/// `makerAmountOut`, after the decay, and the line is commented where it happens. A floor that is
/// not a floor on what the taker receives is not a floor, it is a decoration.
///
/// Note also what the floor *is* here. `minFillBps` is signed per order and defaults to nothing;
/// it is not the hard-coded 60% of the forked design. That constant is not a safety rail, it is
/// precisely what makes an order one-shot — with no remaining-amount accounting, permitting a 10%
/// fill would forfeit the other 90%, so the floor exists to stop takers from destroying most of an
/// order. Once fills accumulate exactly, a streamed quote wants `minFillBps == 0`, and a maker who
/// genuinely means "all of it, at the price I quoted, or nothing" sets `10_000` and gets exactly
/// that — which, because the check is now after the decay, also means such an order is unfillable
/// once any decay has accrued. That is the correct reading of the request, and the ordering the
/// fork inherited silently broke it.
///
/// # Replay
///
/// Replay protection is the remaining-amount accounting, not the nonce. A signature can be
/// presented any number of times and will settle a total of exactly `takerAmount`, after which
/// every further presentation reverts `FillExceedsRemaining`. That is the property partial fills
/// require: "used once" and "used up" are different questions, and only the second one is
/// answerable by a bitmap.
///
/// The nonce bitmap is therefore a **cancellation handle**, and only that. Setting a bit kills
/// every unfilled order carrying that nonce, whatever its hash — which is a feature: a maker
/// streaming twenty quotes under one nonce retires all twenty with one `SSTORE`. It is also why a
/// fully consumed order does *not* burn its bit; doing so would kill its siblings.
///
/// # Deliberately not implemented
///
///   * **An on-chain taker allowlist.** The privacy RFQ buys is that the quote is not posted, not
///     that the fill is gated. Gating it is not expressible here: with the push-model adapter in
///     the path, `msg.sender` is the adapter, so a signed `taker` field could only ever name the
///     adapter. Recovering the real payer needs the router to forward an authenticated origin,
///     which is the off-ABI calldata-tail `payerOrigin` convention `archi_v2.md` §6.2 lists among
///     the things not to borrow. A tail-appended word that is not part of the ABI makes router
///     calldata unauditable, which is a worse trade than an open fill.
///   * **ERC-1271.** The maker is the quoter's own signing key, the same custody story as
///     `dubu-updater`. Contract signers are a real requirement for a maker behind a Safe and this
///     contract does not serve them; the surface they add — a `staticcall` to caller-supplied code
///     during signature verification — is not worth carrying before there is a maker who needs it.
///   * **Permit2 on the maker leg.** The forked design's `usePermit2` branch, its
///     `permit2Signature` / `permit2Witness` / `permit2WitnessType` fields, and the ordering
///     hazard they create (the order hash covers `keccak256(permit2Signature)`, so the Permit2
///     signature must be produced first) are all absent. The maker is a long-lived market-making
///     account, not a user wallet; a single standing allowance is the honest primitive for it.
///     `Router` implements Permit2 for the *taker* side of a route, which is where a user actually
///     benefits.
///   * **Native ETH, and any `payable` path.** Same reason as `Router`: GIWA pre-installs WETH9,
///     and a refund path is a reentrancy surface bought for one hop of convenience.
///   * **Fee-on-transfer and rebasing token support.** Both legs are accounted at the requested
///     amount, exactly as `SafeTransfer` documents and `PropPool` assumes. Such a token must not
///     be quoted.
contract PmmSettle is IPmmSettle {
    // ---------------------------------------------------------------------
    // Errors
    // ---------------------------------------------------------------------

    error ZeroReceiver();
    error ZeroMaker();
    error ZeroAsset();
    error IdenticalAssets();
    error ZeroOrderAmount();
    error AmountOutOfDomain();
    error DecayCapExceeded(uint256 decayCap);
    error MinFillOutOfRange(uint256 minFillBps);
    error ZeroFill();
    error OrderExpired(uint256 expiry, uint256 nowTs);
    error NonceCancelled(address maker, uint64 nonce);
    error BadSignature();
    error BadSignatureLength(uint256 length);
    error MalleableSignature();
    error FillExceedsRemaining(uint256 requested, uint256 remaining);
    error DecayTooHigh(uint256 decayPpm, uint256 maxDecayPpm);
    error BelowSettlementFloor(uint256 makerAmountOut, uint256 floorAmount);
    error NothingDelivered();
    error Reentrancy();

    // ---------------------------------------------------------------------
    // Constants
    // ---------------------------------------------------------------------

    /// @notice Denominator of `decayPerSec`, `decayCap` and every decay figure this contract
    ///         reports. Parts per million.
    /// @dev Parts per million rather than the basis points used everywhere else in this repo, and
    ///      that is a deliberate inconsistency. `archi_v2.md` §5.2 records the production
    ///      reference values for the curve side — `width_bps = 0.20`, `improvement_bps = 0.10`,
    ///      `min_spread_bps = 0.05` — so a decay *rate* quantised to whole basis points would be
    ///      20x coarser than the spread it is supposed to price. It also keeps the units
    ///      byte-compatible with the forked design, so a maker's existing rate parameters carry
    ///      over unchanged.
    uint256 public constant DECAY_DENOMINATOR = 1e6;

    /// @notice Hard ceiling on `Order.decayCap`: 50_000 ppm, i.e. 5%.
    /// @dev Inherited from the forked design and kept at the same value on purpose. The decay is
    ///      one-directional, so the cap is the only thing bounding what a stale quote can cost a
    ///      taker who did not set `maxDecayPpm`. Raising it is a governance-shaped decision and
    ///      this contract has no governance, so it is a constant.
    uint256 public constant MAX_DECAY_CAP = 50_000;

    /// @notice Denominator of `Order.minFillBps`.
    uint256 public constant BPS_DENOMINATOR = 10_000;

    /// @notice Hard ceiling on both legs of an order, and on any single fill.
    ///
    /// @dev `type(uint128).max`, matching `PropCurve.MAX_AMOUNT_OUT`, and load-bearing for two
    ///      separate reasons.
    ///
    ///      **It closes the domain.** `dubu-core` mirrors this contract's EIP-712 encoding in
    ///      `u128`. Without this bound the chain could settle a fill the engine cannot represent,
    ///      which is the silent-loss failure `PropCurve.MAX_AMOUNT_OUT` exists to prevent: the
    ///      engine would treat a size as unquotable while the chain filled it.
    ///
    ///      **It discharges every overflow obligation in this file**, in checked arithmetic, with
    ///      no `unchecked` block anywhere:
    ///
    ///      | expression                          | bound                       | value     |
    ///      |-------------------------------------|-----------------------------|-----------|
    ///      | `makerAmount * takerAmountIn`       | `(2^128 - 1)^2`             | `< 2^256` |
    ///      | `quoted * DECAY_DENOMINATOR`        | `2^128 * 10^6 < 2^128*2^20` | `< 2^148` |
    ///      | `makerAmountOut * BPS_DENOMINATOR`  | `2^128 * 10^4 < 2^128*2^14` | `< 2^142` |
    ///      | `makerAmount * minFillBps`          | `2^128 * 10^4`              | `< 2^142` |
    ///
    ///      A full `uint128` of an 18-decimal token is ~3.4e20 whole units, so nothing reachable
    ///      is rejected.
    uint256 public constant MAX_AMOUNT = type(uint128).max;

    // forgefmt: disable-start
    /// @notice `keccak256` of the `Order` EIP-712 type string.
    /// @dev Written as adjacent string literals, one field per line, because this constant is
    ///      mirrored byte for byte by `dubu_core::rfq::ORDER_TYPEHASH` and a reviewer comparing
    ///      the two needs to diff them by eye. A single 200-character literal cannot be diffed by
    ///      eye. Field order here IS the struct's field order and IS the `encodeData` order;
    ///      EIP-712 gives no independent canonicalisation, so all three must be changed together
    ///      or none.
    bytes32 public constant ORDER_TYPEHASH = keccak256(
        "Order("
            "address maker,"
            "address makerAsset,"
            "address takerAsset,"
            "uint256 makerAmount,"
            "uint256 takerAmount,"
            "uint64 nonce,"
            "uint64 expiry,"
            "uint64 decayStart,"
            "uint32 decayPerSec,"
            "uint32 decayCap,"
            "uint16 minFillBps"
        ")"
    );
    // forgefmt: disable-end

    bytes32 private constant _EIP712_DOMAIN_TYPEHASH =
        keccak256("EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)");

    /// @dev Hashed at compile time; the strings themselves are never needed at runtime.
    bytes32 private constant _NAME_HASH = keccak256("DuBu PmmSettle");
    bytes32 private constant _VERSION_HASH = keccak256("1");

    /// @dev `secp256k1n / 2`. A signature with `s` above this is the second valid encoding of the
    ///      same authorisation.
    bytes32 private constant _HALF_CURVE_ORDER = 0x7FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF5D576E7357A4501DDFE92F46681B20A0;

    // ---------------------------------------------------------------------
    // Storage
    // ---------------------------------------------------------------------

    /// @inheritdoc IPmmSettle
    /// @dev Keyed on the EIP-712 digest, so two orders differing in any signed field — including
    ///      the nonce — have independent accounting. This is the mapping that makes an order
    ///      re-presentable without being replayable.
    mapping(bytes32 => uint256) public override filledTaker;

    /// @dev `maker => nonce >> 8 => bitmap`. Cancellation only; see the contract note on replay.
    mapping(address => mapping(uint256 => uint256)) private _cancelledNonces;

    uint256 private immutable _CACHED_CHAIN_ID;
    bytes32 private immutable _CACHED_DOMAIN_SEPARATOR;

    // ---------------------------------------------------------------------
    // Reentrancy
    // ---------------------------------------------------------------------

    /// @dev Transient (EIP-1153), matching `Router` rather than `ReentrancyLock`.
    ///
    ///      Both are the same mechanism; the difference is the slot. `ReentrancyLock` pins a
    ///      namespaced constant derived from `"dubu.PropPool.reentrancyLock.v1"` because a
    ///      contract that inherits several guarded components must be able to reason about
    ///      collisions by inspection. This contract inherits nothing and has exactly one guarded
    ///      function, so a compiler-assigned slot is not a hazard, and reusing a slot whose
    ///      preimage names a different contract would be actively misleading to a reader.
    ///
    ///      The guard is not decorative. `fillOrder` calls `transferFrom` on two caller-influenced
    ///      token addresses, and a token is arbitrary code. Without it, a malicious `makerAsset`
    ///      could re-enter during the payout — after `_filledTaker` is written, which is safe, but
    ///      also while an adapter's exact-amount allowance is still live, which is not.
    bool private transient _entered;

    /// @dev Body split into `_lock`/`_unlock` rather than written inline, mirroring `Router`. A
    ///      modifier is copied into every site that uses it; there is only one site here, so this
    ///      is consistency with the sibling contract rather than a code-size argument.
    modifier nonReentrant() {
        _lock();
        _;
        _unlock();
    }

    function _lock() private {
        if (_entered) revert Reentrancy();
        _entered = true;
    }

    function _unlock() private {
        _entered = false;
    }

    // ---------------------------------------------------------------------
    // Construction
    // ---------------------------------------------------------------------

    constructor() {
        _CACHED_CHAIN_ID = block.chainid;
        _CACHED_DOMAIN_SEPARATOR = _computeDomainSeparator();
    }

    // ---------------------------------------------------------------------
    // EIP-712
    // ---------------------------------------------------------------------

    /// @inheritdoc IPmmSettle
    /// @dev Cached in the bytecode and re-derived when `block.chainid` no longer matches.
    ///      `block.chainid` is the one domain input that can change under a deployed contract: a
    ///      chain fork produces two chains sharing this address and this code, and a separator
    ///      cached across that boundary would make every live order valid on both — a maker's
    ///      inventory settled twice against one signature. Same treatment, for the same reason, as
    ///      `MockERC20.DOMAIN_SEPARATOR`.
    // forge-lint: disable-next-line(mixed-case-function)
    function DOMAIN_SEPARATOR() public view override returns (bytes32) {
        return block.chainid == _CACHED_CHAIN_ID ? _CACHED_DOMAIN_SEPARATOR : _computeDomainSeparator();
    }

    /// @inheritdoc IPmmSettle
    function hashOrder(Order calldata order) public view override returns (bytes32) {
        return keccak256(abi.encodePacked("\x19\x01", DOMAIN_SEPARATOR(), _structHash(order)));
    }

    function _computeDomainSeparator() private view returns (bytes32) {
        return keccak256(abi.encode(_EIP712_DOMAIN_TYPEHASH, _NAME_HASH, _VERSION_HASH, block.chainid, address(this)));
    }

    /// @dev `encodeData` per EIP-712: the typehash followed by every member, each padded to one
    ///      word. `abi.encode` does the padding, and it is the whole reason the narrow members are
    ///      free — a `uint16` and a `uint256` occupy the same 32 bytes here.
    function _structHash(Order calldata order) private pure returns (bytes32) {
        return keccak256(
            abi.encode(
                ORDER_TYPEHASH,
                order.maker,
                order.makerAsset,
                order.takerAsset,
                order.makerAmount,
                order.takerAmount,
                order.nonce,
                order.expiry,
                order.decayStart,
                order.decayPerSec,
                order.decayCap,
                order.minFillBps
            )
        );
    }

    // ---------------------------------------------------------------------
    // Pricing — pure, and callable before anyone commits to anything
    // ---------------------------------------------------------------------

    /// @inheritdoc IPmmSettle
    function decayPpmAt(Order calldata order, uint256 atTimestamp) public pure override returns (uint256) {
        uint256 start = order.decayStart;
        uint256 rate = order.decayPerSec;
        uint256 cap = order.decayCap;
        // Any one of the three being zero means "this order does not decay". Three separate
        // disables rather than one flag because each is independently meaningful to a quoter, and
        // because a rate with no cap is the one combination that must never be treated as valid.
        if (start == 0 || rate == 0 || cap == 0) return 0;
        if (atTimestamp <= start) return 0;

        uint256 elapsed = atTimestamp - start;
        // `rate >= 1` here, so `elapsed >= cap` already implies `elapsed * rate >= cap`. Returning
        // early on that keeps the multiply below bounded by `cap * rate < 2^32 * 2^32 = 2^64`
        // whatever timestamp the caller passed. This function is `pure` and takes the timestamp as
        // an argument, so "the caller passed 2^255" is reachable input, not a hypothetical, and a
        // preview that panics on it would be a preview an aggregator cannot batch.
        if (elapsed >= cap) return cap;

        uint256 accrued = elapsed * rate;
        return accrued > cap ? cap : accrued;
    }

    /// @inheritdoc IPmmSettle
    function previewFill(Order calldata order, uint256 takerAmountIn, uint256 atTimestamp)
        public
        pure
        override
        returns (uint256 quotedMakerOut, uint256 realisedMakerOut, uint256 decayPpm)
    {
        _checkOrderShape(order);
        if (takerAmountIn > MAX_AMOUNT) revert AmountOutOfDomain();

        quotedMakerOut = (order.makerAmount * takerAmountIn) / order.takerAmount;
        decayPpm = decayPpmAt(order, atTimestamp);
        realisedMakerOut = (quotedMakerOut * (DECAY_DENOMINATOR - decayPpm)) / DECAY_DENOMINATOR;
    }

    /// @inheritdoc IPmmSettle
    function remainingTaker(Order calldata order) external view override returns (uint256) {
        // `_filledTaker` only ever grows by an amount already checked against the remainder, so
        // the subtraction cannot underflow for any order whose accounting this contract wrote.
        // For an order it has never seen the entry is zero and the answer is the full size.
        return order.takerAmount - filledTaker[hashOrder(order)];
    }

    /// @inheritdoc IPmmSettle
    /// @dev `slot = nonce >> 8`, `bit = nonce & 0xff`. `1 << bit` is the intended direction and
    ///      not the transposed-operands bug the lint looks for: `bit` is the shift *amount*,
    ///      bounded to 0..255 by the mask, and shifting `1` by it selects the nonce's bit.
    function isNonceCancelled(address maker, uint64 nonce) public view override returns (bool) {
        // forge-lint: disable-next-line(incorrect-shift)
        return _cancelledNonces[maker][nonce >> 8] & (1 << (nonce & 0xff)) != 0;
    }

    // ---------------------------------------------------------------------
    // Cancellation
    // ---------------------------------------------------------------------

    /// @inheritdoc IPmmSettle
    function cancelNonce(uint64 nonce) external override {
        // forge-lint: disable-next-line(incorrect-shift)
        _cancel(nonce >> 8, 1 << (nonce & 0xff));
    }

    /// @inheritdoc IPmmSettle
    function cancelNonceSlot(uint256 slot, uint256 mask) external override {
        _cancel(slot, mask);
    }

    /// @dev Idempotent on purpose: re-cancelling an already-dead nonce is a no-op that still
    ///      emits, rather than a revert. A maker tripping a kill switch is retracting quotes under
    ///      time pressure and generally does not know which of them are already dead; a revert
    ///      there would abort the whole batch and leave live quotes standing, which is the exact
    ///      failure the switch exists to prevent.
    function _cancel(uint256 slot, uint256 mask) private {
        _cancelledNonces[msg.sender][slot] |= mask;
        emit NoncesCancelled(msg.sender, slot, mask);
    }

    // ---------------------------------------------------------------------
    // Settlement
    // ---------------------------------------------------------------------

    /// @inheritdoc IPmmSettle
    function fillOrder(
        Order calldata order,
        bytes calldata signature,
        uint256 takerAmountIn,
        uint32 maxDecayPpm,
        address receiver
    ) external override nonReentrant returns (uint256 makerAmountOut) {
        if (receiver == address(0)) revert ZeroReceiver();
        if (takerAmountIn == 0) revert ZeroFill();
        _checkOrderShape(order);

        // Expiry is inclusive: an order stamped `expiry` is fillable in the block whose timestamp
        // equals it. `>` rather than `>=` matches `Router._validate`'s deadline and `PropPool`'s,
        // and the boundary is pinned by a test in both directions because "off by one second at
        // the edge" is not a difference a maker can see in production.
        if (block.timestamp > order.expiry) revert OrderExpired(order.expiry, block.timestamp);
        if (isNonceCancelled(order.maker, order.nonce)) revert NonceCancelled(order.maker, order.nonce);

        bytes32 orderHash = hashOrder(order);
        _verifySignature(order.maker, orderHash, signature);

        uint256 remaining = _consume(orderHash, order.takerAmount, takerAmountIn);

        uint256 quoted;
        uint256 decayPpm;
        (quoted, makerAmountOut, decayPpm) = _price(order, takerAmountIn, maxDecayPpm);

        // Taker leg first. The maker signed this order and is not present in the transaction to
        // react to anything; the filler is. Paying the absent party before the present one is the
        // conservative ordering, and with the guard held neither token can observe a half-settled
        // state anyway.
        SafeTransfer.safeTransferFrom(order.takerAsset, msg.sender, order.maker, takerAmountIn);
        SafeTransfer.safeTransferFrom(order.makerAsset, order.maker, receiver, makerAmountOut);

        _emitFill(order, orderHash, receiver, takerAmountIn, quoted, makerAmountOut, decayPpm, remaining);
    }

    // ---------------------------------------------------------------------
    // Internals
    // ---------------------------------------------------------------------

    /// @dev Caveat 2, in four lines: check the request against the remainder, commit the new total,
    ///      return what is left.
    ///
    ///      Split out of `fillOrder` rather than inlined because the legacy code generator runs out
    ///      of stack slots otherwise — `foundry.toml` pins `via_ir = false` for iteration speed and
    ///      this contract must build either way, the same constraint `PropCurve.amountOutAsk`
    ///      documents at its scoping block.
    ///
    ///      The write lands here, before either transfer. Checks-effects-interactions is the belt;
    ///      the reentrancy guard is the braces. Both, because the interactions are calls into two
    ///      token contracts whose addresses came out of a signed order this contract did not
    ///      author.
    function _consume(bytes32 orderHash, uint256 takerAmount, uint256 takerAmountIn)
        private
        returns (uint256 remaining)
    {
        uint256 filled = filledTaker[orderHash];
        remaining = takerAmount - filled;
        // Strictly greater, and never clamped down to the remainder. A contract that silently
        // filled less than it was asked for would hand the caller a number they did not choose and
        // would make an over-sized request indistinguishable from an exact one; the adapter clamps
        // deliberately, one frame up, where it can also refund the difference.
        if (takerAmountIn > remaining) revert FillExceedsRemaining(takerAmountIn, remaining);
        filledTaker[orderHash] = filled + takerAmountIn;
        remaining = remaining - takerAmountIn;
    }

    /// @dev The quoted leg, the realised leg, and the gap between them.
    ///
    ///      `view` rather than `pure` only because it reads `block.timestamp`; nothing here touches
    ///      storage. It is the same arithmetic `previewFill` runs, with the settlement-time checks
    ///      that a preview has no business making bolted on — which is why the two share
    ///      `decayPpmAt` rather than each deriving the decay.
    function _price(Order calldata order, uint256 takerAmountIn, uint32 maxDecayPpm)
        private
        view
        returns (uint256 quoted, uint256 makerAmountOut, uint256 decayPpm)
    {
        // Pro-rata at the signed rate, floored. This is the number the taker was shown.
        quoted = (order.makerAmount * takerAmountIn) / order.takerAmount;

        decayPpm = decayPpmAt(order, block.timestamp);
        if (decayPpm > maxDecayPpm) revert DecayTooHigh(decayPpm, maxDecayPpm);

        // One multiply chain, one division, one rounding — `floor` — and it lands in the maker's
        // favour, matching `PropCurve` amendment 2's discipline of never rounding a rate and never
        // rounding twice. Writing it as `quoted - quoted * decay / 1e6` would round the *cut* down,
        // i.e. in the taker's favour, and would round twice.
        makerAmountOut = (quoted * (DECAY_DENOMINATOR - decayPpm)) / DECAY_DENOMINATOR;

        // A fill that moves the taker's tokens and delivers nothing is not a small fill, it is a
        // theft with a receipt. Reachable two ways: a `takerAmountIn` below the rate's resolution,
        // and a 100% decay, which `MAX_DECAY_CAP` makes unreachable but which is not this
        // function's assumption to make.
        if (makerAmountOut == 0) revert NothingDelivered();

        // --- caveat 3: the floor goes HERE, after the decay ---------------------------------
        // The order matters and this is the whole of it. The forked design checks its floor against
        // the pre-decay amount, then applies up to `MAX_DECAY_CAP` to the result, so an order
        // advertising a 60% minimum can deliver 57%. Checking `makerAmountOut` instead makes
        // `minFillBps` a bound on what the taker actually receives, which is the only reading under
        // which the field means anything. Hoisting this above the `makerAmountOut` assignment would
        // restore the gap silently, with every test in this repo still passing except the one that
        // exists to catch exactly that.
        if (order.minFillBps != 0) {
            uint256 floorAmount = (order.makerAmount * order.minFillBps) / BPS_DENOMINATOR;
            if (makerAmountOut < floorAmount) revert BelowSettlementFloor(makerAmountOut, floorAmount);
        }
    }

    /// @dev Split out for the same stack reason as `_consume`. The event's ten fields are the
    ///      audit record; see `IPmmSettle.OrderFilled` for why the quoted/realised pair is the
    ///      point of it.
    function _emitFill(
        Order calldata order,
        bytes32 orderHash,
        address receiver,
        uint256 takerAmountIn,
        uint256 quoted,
        uint256 makerAmountOut,
        uint256 decayPpm,
        uint256 remaining
    ) private {
        emit OrderFilled(
            orderHash,
            order.maker,
            receiver,
            order.makerAsset,
            order.takerAsset,
            takerAmountIn,
            quoted,
            makerAmountOut,
            decayPpm,
            remaining
        );
    }

    /// @dev Everything about an order that is wrong regardless of who is asking or when.
    ///
    ///      Shared by `fillOrder` and `previewFill` so the two can never disagree about which
    ///      orders exist. A preview that priced an order the fill path rejects would be worse than
    ///      no preview: it would put a number in front of a taker that no transaction can realise.
    function _checkOrderShape(Order calldata order) private pure {
        if (order.maker == address(0)) revert ZeroMaker();
        if (order.makerAsset == address(0) || order.takerAsset == address(0)) revert ZeroAsset();
        // Both legs are pulled with `transferFrom` against the same token, so a self-pair would
        // settle the maker against themselves and leave the filler's tokens with the maker.
        if (order.makerAsset == order.takerAsset) revert IdenticalAssets();
        if (order.makerAmount == 0 || order.takerAmount == 0) revert ZeroOrderAmount();
        if (order.makerAmount > MAX_AMOUNT || order.takerAmount > MAX_AMOUNT) revert AmountOutOfDomain();
        if (order.decayCap > MAX_DECAY_CAP) revert DecayCapExceeded(order.decayCap);
        if (order.minFillBps > BPS_DENOMINATOR) revert MinFillOutOfRange(order.minFillBps);
    }

    /// @dev 65-byte `(r, s, v)` ECDSA over the EIP-712 digest.
    ///
    ///      `ecrecover` returns `address(0)` for a malformed input rather than reverting, so the
    ///      zero check is what stops a garbage signature from authorising an order whose `maker`
    ///      is also zero. `_checkOrderShape` already rejects that maker, which makes the check
    ///      redundant — and it is written anyway, because a defence that depends on a validation
    ///      three call frames away is a defence that survives exactly until someone reorders the
    ///      function.
    ///
    ///      Malleability is rejected even though it is **not** a replay vector here: the fill
    ///      accounting keys on the order hash, so the second valid encoding of a signature settles
    ///      against the same remaining amount as the first and buys an attacker nothing. It is
    ///      rejected because a signature with two valid byte encodings is a discrepancy between
    ///      what an off-chain indexer deduplicates on and what the chain settled, and this
    ///      contract's entire value proposition is that those two never differ.
    function _verifySignature(address maker, bytes32 digest, bytes calldata signature) private pure {
        if (signature.length != 65) revert BadSignatureLength(signature.length);

        bytes32 r = bytes32(signature[0:32]);
        bytes32 s = bytes32(signature[32:64]);
        uint8 v = uint8(signature[64]);

        if (uint256(s) > uint256(_HALF_CURVE_ORDER)) revert MalleableSignature();
        // `ecrecover` treats any `v` outside {27, 28} as invalid and returns zero, so this is
        // strictly a better error message rather than a distinct defence.
        if (v != 27 && v != 28) revert MalleableSignature();

        address recovered = ecrecover(digest, v, r, s);
        if (recovered == address(0) || recovered != maker) revert BadSignature();
    }
}
