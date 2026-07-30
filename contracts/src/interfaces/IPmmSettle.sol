// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

/// @title IPmmSettle
/// @notice The RFQ half of the DuBu prop AMM: a firm, signed, off-chain quote settled on chain.
///
/// # Why a second path exists at all
///
/// `IPropPool` already gives a third-party aggregator everything it needs. RFQ buys the two things
/// the curve path structurally cannot:
///
///  1. **A price that need not be expressible as a ladder.** `PropCurve` is a linear-impact single
///     tick described by four `uint56`s, so skew, volatility response and any per-counterparty
///     spread have to be folded into those four numbers before they can be posted. A signed order
///     carries an arbitrary rate.
///  2. **A quote given to one counterparty without being posted to everyone.** `updateQuote` writes
///     to public storage; an order is a signature handed to whoever asked for it. The limit is
///     exact: the **quote** is private until used, but the **fill** is not restricted — once a
///     signature exists, anyone holding `takerAsset` can present it, and an on-chain taker
///     allowlist is not implementable here (see `fillOrder`).
///
/// # Derivation, and the four things this fork changes
///
/// `PmmSettle` forks `okxlabs/Web3-DEX-EVM-PMM`'s `PmmProtocol` (MIT, Copyright (c) 2022-2025 OKX
/// labs) rather than writing an RFQ leg from scratch. Four recorded caveats specify what differs,
/// each named where it is answered:
///
///  1. *The decay is one-directional.* Answered by [`previewFill`] before the taker commits, by
///     `maxDecayPpm` on [`fillOrder`], and by the `makerAmountQuoted` / `makerAmountFilled` pair on
///     [`OrderFilled`].
///  2. *Orders are one-shot.* Answered by [`remainingTaker`] and `PmmSettle`'s fill accounting.
///  3. *The settlement floor is checked before the decay.* Answered by the ordering in
///     `PmmSettle._price`.
///  4. *The adapter is unaudited and untested.* Answered by `test/PmmAdapter.t.sol`.
interface IPmmSettle {
    // --- The order ---

    /// @notice One firm quote, signed by the maker off chain.
    ///
    /// @dev Field widths fit the shared numeric domain rather than saving calldata: EIP-712
    ///      `encodeData` pads every member to 32 bytes anyway, so narrowing is free on the wire and
    ///      buys an exact off-chain mirror. `dubu-core/src/rfq.rs` holds both amounts in `u128`,
    ///      which is sound only because `PmmSettle.MAX_AMOUNT` rejects anything wider on chain —
    ///      the same discipline as `PropCurve.MAX_AMOUNT_OUT`, so the engine can never treat as
    ///      unquotable a trade the chain would settle.
    ///
    /// @param maker        signer, and the account both legs settle against. This contract holds
    ///                     no inventory, so `maker` must have approved it for `makerAsset`.
    /// @param makerAsset   what the maker delivers.
    /// @param takerAsset   what the maker receives.
    /// @param makerAmount  maker leg at full size. This is the **quoted** amount: the number a
    ///                     taker inspecting the order before signing anything sees, and the
    ///                     number the decay is measured against.
    /// @param takerAmount  taker leg at full size. Also the unit the fill accounting is kept in.
    /// @param nonce        the maker's cancellation handle — see [`cancelNonce`]. It is NOT the
    ///                     replay guard; the remaining-amount accounting is.
    /// @param expiry       last block timestamp at which this order may be filled, inclusive.
    /// @param decayStart   timestamp at which the decay begins. Zero disables the decay
    ///                     entirely, as does a zero `decayPerSec` or a zero `decayCap`.
    /// @param decayPerSec  decay accrued per second, in parts per million of the maker leg.
    /// @param decayCap     ceiling on the accrued decay, in ppm. Bounded by
    ///                     `PmmSettle.MAX_DECAY_CAP`.
    /// @param minFillBps   the maker's floor on a single fill, in basis points of `makerAmount`,
    ///                     measured on what the taker actually receives. Zero disables it. See
    ///                     `PmmSettle` for why it is checked after the decay and not before.
    struct Order {
        address maker;
        address makerAsset;
        address takerAsset;
        uint256 makerAmount;
        uint256 takerAmount;
        uint64 nonce;
        uint64 expiry;
        uint64 decayStart;
        uint32 decayPerSec;
        uint32 decayCap;
        uint16 minFillBps;
    }

    // --- Events ---

    /// @notice One fill, whole or partial.
    ///
    /// @param orderHash         EIP-712 digest. The fill-accounting key, and what an indexer
    ///                          groups a streamed quote's fills by.
    /// @param maker             the signer.
    /// @param receiver          who received `makerAsset`.
    /// @param makerAsset        what the maker delivered.
    /// @param takerAsset        what the maker received.
    /// @param takerAmountIn     taker leg actually moved.
    /// @param makerAmountQuoted what `takerAmountIn` buys at the **signed** rate, before any
    ///                          decay: `floor(makerAmount * takerAmountIn / takerAmount)`.
    /// @param makerAmountFilled what actually moved, after the decay.
    /// @param decayPpm          the decay applied to this fill, in ppm.
    /// @param takerRemaining    taker leg still available on this order after this fill.
    ///
    /// @dev `makerAmountQuoted` and `makerAmountFilled` are emitted side by side, which is the
    ///      whole anti-spoofing property of this contract: a quote shown at T that settles
    ///      materially worse at T+delta is indistinguishable from the prop-AMM quote spoofing 0x
    ///      measured at 5-10bp per fill, so the answer has to be a number anyone can difference out
    ///      of the log. `Router.RouteExecuted` publishes the same shape one level up.
    event OrderFilled(
        bytes32 indexed orderHash,
        address indexed maker,
        address indexed receiver,
        address makerAsset,
        address takerAsset,
        uint256 takerAmountIn,
        uint256 makerAmountQuoted,
        uint256 makerAmountFilled,
        uint256 decayPpm,
        uint256 takerRemaining
    );

    /// @notice A maker retired one or more nonces. Every unfilled order carrying a retired nonce
    ///         is dead, whatever its remaining amount and whatever its expiry.
    /// @param maker the retiring maker.
    /// @param slot  `nonce >> 8`.
    /// @param mask  the bits newly set in that slot.
    event NoncesCancelled(address indexed maker, uint256 indexed slot, uint256 mask);

    // --- Settlement ---

    /// @notice Settle `takerAmountIn` of `order`, delivering the maker leg to `receiver`.
    ///
    /// @param takerAmountIn  taker leg to spend. Must be at most the order's remaining amount;
    ///                       this contract never silently clamps a request down.
    /// @param maxDecayPpm    the taker's ceiling on the decay applied to this fill. Pass
    ///                       `type(uint32).max` only when the aggregate bound one level up is
    ///                       genuinely the whole protection you want.
    /// @param receiver       who receives `makerAsset`.
    /// @return makerAmountOut what was delivered, after the decay.
    ///
    /// @dev **Anyone may call this.** The signature authorises the maker's side and names no
    ///      taker, and the taker leg is pulled from `msg.sender`, so a front-runner buys the
    ///      maker's quote with their own money. An on-chain taker allowlist is not implemented;
    ///      see `PmmSettle`.
    function fillOrder(
        Order calldata order,
        bytes calldata signature,
        uint256 takerAmountIn,
        uint32 maxDecayPpm,
        address receiver
    ) external returns (uint256 makerAmountOut);

    // --- Cancellation ---

    /// @notice Retire one of the caller's nonces.
    function cancelNonce(uint64 nonce) external;

    /// @notice Retire up to 256 of the caller's nonces in one `SSTORE`.
    /// @param slot the bitmap slot, `nonce >> 8`.
    /// @param mask bits to set; bit `i` retires nonce `slot * 256 + i`.
    /// @dev The kill switch: the engine's bleed and loss-budget latches must withdraw every live
    ///      quote on trip, which a maker with a hundred streamed orders cannot do one tx at a time.
    function cancelNonceSlot(uint256 slot, uint256 mask) external;

    // --- Views ---

    /// @notice EIP-712 digest of `order` under this contract's domain.
    function hashOrder(Order calldata order) external view returns (bytes32);

    /// @notice Taker leg already settled against `orderHash`.
    function filledTaker(bytes32 orderHash) external view returns (uint256);

    /// @notice Taker leg still available on `order`. Zero for a fully filled order.
    /// @dev Does not account for expiry or cancellation — it answers "how much size is left",
    ///      not "is this fillable". A router sizing a leg must check both.
    function remainingTaker(Order calldata order) external view returns (uint256);

    /// @notice Whether `maker` has retired `nonce`.
    function isNonceCancelled(address maker, uint64 nonce) external view returns (bool);

    /// @notice The decay this order would suffer at `atTimestamp`, in ppm.
    function decayPpmAt(Order calldata order, uint256 atTimestamp) external pure returns (uint256);

    /// @notice What `takerAmountIn` buys at `atTimestamp`, quoted and realised side by side.
    ///
    /// @return quotedMakerOut   the pro-rata maker leg at the signed rate, before decay.
    /// @return realisedMakerOut what would actually be delivered.
    /// @return decayPpm         the gap between them, in ppm.
    ///
    /// @dev How the decay reaches the taker **before** they commit. `atTimestamp` is an argument
    ///      rather than `block.timestamp` because the decay depends on when the transaction is
    ///      *included*, so a frontend pricing at `now` shows a number it knows is optimistic. Quote
    ///      it at the expected inclusion time with the value at `expiry` beside it as the worst
    ///      case. `pure`, so an aggregator batches it with everything else.
    function previewFill(Order calldata order, uint256 takerAmountIn, uint256 atTimestamp)
        external
        pure
        returns (uint256 quotedMakerOut, uint256 realisedMakerOut, uint256 decayPpm);

    /// @notice EIP-712 domain separator for this contract on the *current* chain.
    // forge-lint: disable-next-line(mixed-case-function)
    function DOMAIN_SEPARATOR() external view returns (bytes32);
}
