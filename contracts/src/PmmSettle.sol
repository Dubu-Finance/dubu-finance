// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {IPmmSettle} from "./interfaces/IPmmSettle.sol";
import {SafeTransfer} from "./libraries/SafeTransfer.sol";

/// @title PmmSettle
/// @notice On-chain settlement for EIP-712 signed maker orders — the RFQ leg of the DuBu prop AMM.
///
/// Forked from `okxlabs/Web3-DEX-EVM-PMM`'s `PmmProtocol` (MIT, Copyright (c) 2022-2025 OKX labs).
/// `IPmmSettle` records what this fork changes and why.
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
/// A contract holding standing inventory has a balance to drain, a withdrawal path, an owner to
/// authorise it and a pause for when that owner's key leaks — the argument that also makes
/// `IAdapter` custody-free. The cost is that the maker's approval here is a standing allowance, so
/// a bug in this file reaches the maker's inventory; a taker's exposure is bounded by the allowance
/// they grant for one fill.
///
/// # Partial fills, and how they interact with the decay
///
/// The accounting is kept in **taker units**, in `filledTaker[orderHash]`:
///
/// ```text
///   remaining(h)  = order.takerAmount - filledTaker[h]
///   a fill of `t` requires  t <= remaining(h)   and sets  filledTaker[h] += t
/// ```
///
/// Taker units, because the taker leg is the exact-in side: the router pushes taker tokens and the
/// adapter reads its own balance, so the taker leg is measured and the maker leg derived from it.
///
/// The maker leg is pro-rata at the signed rate, floored: `quoted(t) = floor(makerAmount * t /
/// takerAmount)`. Since `sum_i floor(M*t_i/T) <= floor(M*(sum_i t_i)/T) <= M`, **no decomposition
/// of an order ever draws more maker asset than filling it whole**, the shortfall being at most one
/// unit per extra piece, kept by the maker. There is no "sweep the remainder on the last fill"
/// rule: which fill is last is not knowable when the earlier ones settle, and paying the dust to
/// whoever closes the order would make fill order economically observable.
///
/// The decay composes on top per fill — `filled(t, now) = floor(quoted(t) * (1e6 - decay(now)) /
/// 1e6)` — and `decay` is a function of settlement time only, never of how much has already been
/// filled. So fills are **priced independently** and the decay is **not** a capacity mechanism: it
/// does not deepen with usage the way `PropCurve`'s ladder does. Anything the maker wants to charge
/// for size belongs in the signed rate.
///
/// # The decay is one-directional, so it must be visible
///
/// The decay only ever moves value from the taker to the maker: the taker pays `takerAmountIn` in
/// full and receives less as the quote ages. That prices the option the taker holds between signing
/// and inclusion, but from outside, a firm quote that quietly settles worse is indistinguishable
/// from quote spoofing. Three mechanisms, all required:
///
///   * **before** — [`previewFill`] returns quoted and realised side by side at a caller-chosen
///     timestamp, `pure`, so the price a taker is shown already has the decay in it;
///   * **during** — `maxDecayPpm` on [`fillOrder`] lets the taker bound the decay and revert
///     rather than accept a surprise;
///   * **after** — [`IPmmSettle.OrderFilled`] carries `makerAmountQuoted` next to
///     `makerAmountFilled`, so the difference is on chain for anyone to sum.
///
/// # The settlement floor is checked AFTER the decay
///
/// `minFillBps` bounds `makerAmountOut`, post-decay. Checking it against the pre-decay amount and
/// then applying up to `MAX_DECAY_CAP` would let an order advertising a 60% minimum deliver 57% —
/// a floor that is not a floor on what the taker receives.
///
/// It is signed per order and defaults to nothing, because a hard-coded floor is what makes an
/// order one-shot: with no remaining-amount accounting, permitting a 10% fill forfeits the other
/// 90%. A streamed quote wants `minFillBps == 0`; `10_000` means "all of it at the price I quoted,
/// or nothing", and — the check being after the decay — is unfillable once any decay has accrued.
///
/// # Replay
///
/// Replay protection is the remaining-amount accounting, not the nonce. A signature can be
/// presented any number of times and settles a total of exactly `takerAmount`, after which every
/// further presentation reverts `FillExceedsRemaining`. Partial fills require that: "used once" and
/// "used up" are different questions, and only the second is answerable by a bitmap.
///
/// The nonce bitmap is therefore a **cancellation handle** and only that. Setting a bit kills every
/// unfilled order carrying that nonce, whatever its hash, so a maker streaming twenty quotes under
/// one nonce retires all twenty with one `SSTORE` — and why a fully consumed order does not burn
/// its bit, which would kill its siblings.
///
/// # Not implemented
///
///   * **An on-chain taker allowlist**, because it is not expressible: with the push-model adapter
///     in the path `msg.sender` is the adapter, so a signed `taker` field could only name the
///     adapter, and recovering the real payer needs an authenticated origin in an off-ABI calldata
///     tail that makes router calldata unauditable.
///   * **ERC-1271**, whose `staticcall` into caller-supplied code during signature verification is
///     surface not worth carrying before a maker needs it.
///   * **Permit2 on the maker leg**: the maker is a long-lived market-making account, so one
///     standing allowance is the right primitive.
///   * **Native ETH and any `payable` path**, for `Router`'s reason — a refund path is a
///     reentrancy surface bought for one hop of convenience, and GIWA pre-installs WETH9.
///   * **Fee-on-transfer and rebasing tokens.** Both legs are accounted at the requested amount,
///     as `SafeTransfer` documents and `PropPool` assumes. Such a token must not be quoted.
contract PmmSettle is IPmmSettle {
    // --- Errors ---

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

    // --- Constants ---

    /// @notice Denominator of `decayPerSec`, `decayCap` and every decay figure this contract
    ///         reports. Parts per million.
    /// @dev Parts per million rather than the basis points used elsewhere in this repo: the curve
    ///      side's production spreads are fractions of a basis point (`width_bps = 0.20`,
    ///      `min_spread_bps = 0.05`), so a decay *rate* quantised to whole bps would be 20x coarser
    ///      than the spread it prices. It also keeps the units byte-compatible with the fork's.
    uint256 public constant DECAY_DENOMINATOR = 1e6;

    /// @notice Hard ceiling on `Order.decayCap`: 50_000 ppm, i.e. 5%.
    /// @dev The decay is one-directional, so this cap is the only thing bounding what a stale quote
    ///      can cost a taker who did not set `maxDecayPpm`. A constant because raising it is a
    ///      governance-shaped decision and this contract has no governance.
    uint256 public constant MAX_DECAY_CAP = 50_000;

    /// @notice Denominator of `Order.minFillBps`.
    uint256 public constant BPS_DENOMINATOR = 10_000;

    /// @notice Hard ceiling on both legs of an order, and on any single fill.
    ///
    /// @dev `type(uint128).max`, matching `PropCurve.MAX_AMOUNT_OUT`, and load-bearing twice over.
    ///      **It closes the domain**: `dubu-core` mirrors this contract's EIP-712 encoding in
    ///      `u128`, so without it the chain could settle a fill the engine cannot represent.
    ///      **It discharges every overflow obligation in this file** under checked arithmetic, with
    ///      no `unchecked` block anywhere: `makerAmount * takerAmountIn < 2^256`, and each of
    ///      `quoted * DECAY_DENOMINATOR`, `makerAmountOut * BPS_DENOMINATOR` and
    ///      `makerAmount * minFillBps` is under 2^148. A full `uint128` of an 18-decimal token is
    ///      ~3.4e20 whole units, so no reachable size is rejected.
    uint256 public constant MAX_AMOUNT = type(uint128).max;

    // forgefmt: disable-start
    /// @notice `keccak256` of the `Order` EIP-712 type string.
    /// @dev One field per line because this constant is mirrored byte for byte by
    ///      `dubu_core::rfq::ORDER_TYPEHASH` and a reviewer has to diff the two by eye. Field order
    ///      here IS the struct's field order and IS the `encodeData` order; EIP-712 has no
    ///      independent canonicalisation, so all three change together or none do.
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

    // --- Storage ---

    /// @inheritdoc IPmmSettle
    /// @dev Keyed on the EIP-712 digest, so two orders differing in any signed field — the nonce
    ///      included — have independent accounting. This is what makes an order re-presentable
    ///      without being replayable.
    mapping(bytes32 => uint256) public override filledTaker;

    /// @dev `maker => nonce >> 8 => bitmap`. Cancellation only; see the contract note on replay.
    mapping(address => mapping(uint256 => uint256)) private _cancelledNonces;

    uint256 private immutable _CACHED_CHAIN_ID;
    bytes32 private immutable _CACHED_DOMAIN_SEPARATOR;

    // --- Reentrancy ---

    /// @dev Transient (EIP-1153), matching `Router` rather than `ReentrancyLock`: same mechanism,
    ///      different slot, and this contract inherits nothing so it needs no namespaced constant.
    ///
    ///      The guard is load-bearing. `fillOrder` calls `transferFrom` on two caller-influenced
    ///      token addresses, and a token is arbitrary code: a malicious `makerAsset` could re-enter
    ///      during the payout, after `filledTaker` is written — safe — but while an adapter's
    ///      exact-amount allowance is still live, which is not.
    bool private transient _entered;

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

    // --- Construction ---

    constructor() {
        _CACHED_CHAIN_ID = block.chainid;
        _CACHED_DOMAIN_SEPARATOR = _computeDomainSeparator();
    }

    // --- EIP-712 ---

    /// @inheritdoc IPmmSettle
    /// @dev Cached in the bytecode and re-derived when `block.chainid` no longer matches, the chain
    ///      id being the one domain input that can change under a deployed contract: a fork
    ///      produces two chains sharing this address and this code, and a separator cached across
    ///      that boundary would make every live order valid on both — a maker's inventory settled
    ///      twice against one signature.
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

    /// @dev `encodeData` per EIP-712: the typehash followed by every member, each padded to a word
    ///      by `abi.encode` — which is why the narrow members are free on the wire.
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

    // --- Pricing — pure, and callable before anyone commits to anything ---

    /// @inheritdoc IPmmSettle
    function decayPpmAt(Order calldata order, uint256 atTimestamp) public pure override returns (uint256) {
        uint256 start = order.decayStart;
        uint256 rate = order.decayPerSec;
        uint256 cap = order.decayCap;
        // Any one of the three being zero means "this order does not decay". Three disables rather
        // than one flag because each is independently meaningful to a quoter, and because a rate
        // with no cap is the one combination that must never be treated as valid.
        if (start == 0 || rate == 0 || cap == 0) return 0;
        if (atTimestamp <= start) return 0;

        uint256 elapsed = atTimestamp - start;
        // `rate >= 1` here, so `elapsed >= cap` implies `elapsed * rate >= cap`; returning early on
        // it keeps the multiply below `cap * rate < 2^64` for any timestamp the caller passed. This
        // is `pure` with the timestamp as an argument, so `2^255` is reachable input and a preview
        // that panicked on it is a preview an aggregator cannot batch.
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

    // --- Cancellation ---

    /// @inheritdoc IPmmSettle
    function cancelNonce(uint64 nonce) external override {
        // forge-lint: disable-next-line(incorrect-shift)
        _cancel(nonce >> 8, 1 << (nonce & 0xff));
    }

    /// @inheritdoc IPmmSettle
    function cancelNonceSlot(uint256 slot, uint256 mask) external override {
        _cancel(slot, mask);
    }

    /// @dev Idempotent: re-cancelling an already-dead nonce emits rather than reverting. A maker
    ///      tripping a kill switch does not know which quotes are already dead, and a revert there
    ///      would abort the whole batch and leave live quotes standing.
    function _cancel(uint256 slot, uint256 mask) private {
        _cancelledNonces[msg.sender][slot] |= mask;
        emit NoncesCancelled(msg.sender, slot, mask);
    }

    // --- Settlement ---

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
        // equals it, matching `Router._validate`'s deadline and `PropPool`'s.
        if (block.timestamp > order.expiry) revert OrderExpired(order.expiry, block.timestamp);
        if (isNonceCancelled(order.maker, order.nonce)) revert NonceCancelled(order.maker, order.nonce);

        bytes32 orderHash = hashOrder(order);
        _verifySignature(order.maker, orderHash, signature);

        uint256 remaining = _consume(orderHash, order.takerAmount, takerAmountIn);

        uint256 quoted;
        uint256 decayPpm;
        (quoted, makerAmountOut, decayPpm) = _price(order, takerAmountIn, maxDecayPpm);

        // Taker leg first: the maker is not present in this transaction to react to anything and
        // the filler is, so the absent party is paid before the present one.
        SafeTransfer.safeTransferFrom(order.takerAsset, msg.sender, order.maker, takerAmountIn);
        SafeTransfer.safeTransferFrom(order.makerAsset, order.maker, receiver, makerAmountOut);

        _emitFill(order, orderHash, receiver, takerAmountIn, quoted, makerAmountOut, decayPpm, remaining);
    }

    // --- Internals ---

    /// @dev Check the request against the remainder, commit the new total, return what is left.
    ///      Split out of `fillOrder` because the legacy code generator runs out of stack slots
    ///      otherwise, and `foundry.toml` pins `via_ir = false`.
    ///
    ///      The write lands here, before either transfer: the interactions are calls into two token
    ///      contracts whose addresses came out of a signed order this contract did not author.
    function _consume(bytes32 orderHash, uint256 takerAmount, uint256 takerAmountIn)
        private
        returns (uint256 remaining)
    {
        uint256 filled = filledTaker[orderHash];
        remaining = takerAmount - filled;
        // Never clamped down to the remainder: that would make an over-sized request
        // indistinguishable from an exact one. The adapter clamps one frame up, where it can also
        // refund the difference.
        if (takerAmountIn > remaining) revert FillExceedsRemaining(takerAmountIn, remaining);
        filledTaker[orderHash] = filled + takerAmountIn;
        remaining = remaining - takerAmountIn;
    }

    /// @dev The quoted leg, the realised leg, and the gap between them: `previewFill`'s arithmetic
    ///      plus the settlement-time checks a preview has no business making. Both route through
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

        // One multiply chain, one division, one floor, landing in the maker's favour — the same
        // discipline as `PropCurve` amendment 2: never round a rate, never round twice. Written as
        // `quoted - quoted * decay / 1e6` it would round the *cut* down and would round twice.
        makerAmountOut = (quoted * (DECAY_DENOMINATOR - decayPpm)) / DECAY_DENOMINATOR;

        // A fill that moves the taker's tokens and delivers nothing must not settle. Reachable via
        // a `takerAmountIn` below the rate's resolution, and via a 100% decay — which
        // `MAX_DECAY_CAP` excludes, but that is not this function's assumption to make.
        if (makerAmountOut == 0) revert NothingDelivered();

        // The floor goes HERE, after the decay: checking `makerAmountOut` makes `minFillBps` a
        // bound on what the taker actually receives. Hoisted above the assignment it would bound
        // the pre-decay amount and up to `MAX_DECAY_CAP` would then come off the result — an order
        // advertising a 60% minimum delivering 57%.
        if (order.minFillBps != 0) {
            uint256 floorAmount = (order.makerAmount * order.minFillBps) / BPS_DENOMINATOR;
            if (makerAmountOut < floorAmount) revert BelowSettlementFloor(makerAmountOut, floorAmount);
        }
    }

    /// @dev Split out for the same stack reason as `_consume`. See `IPmmSettle.OrderFilled` for
    ///      why the quoted/realised pair is the audit record.
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

    /// @dev Everything about an order that is wrong regardless of who is asking or when. Shared by
    ///      `fillOrder` and `previewFill`, so a preview cannot price an order the fill path
    ///      rejects.
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
    ///      `ecrecover` returns `address(0)` for malformed input rather than reverting, so the zero
    ///      check stops a garbage signature authorising an order whose `maker` is also zero.
    ///      `_checkOrderShape` already rejects that maker; the check is kept anyway because a
    ///      defence resting on a validation three call frames away survives only until someone
    ///      reorders the function.
    ///
    ///      Malleability is **not** a replay vector here — the accounting keys on the order hash,
    ///      so the second valid encoding settles against the same remaining amount. It is rejected
    ///      because two valid byte encodings are a discrepancy between what an off-chain indexer
    ///      deduplicates on and what the chain settled.
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
