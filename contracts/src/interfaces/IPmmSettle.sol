// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

/// @title IPmmSettle
/// @notice The RFQ half of the DuBu prop AMM: a firm, signed, off-chain quote settled on chain.
///
/// # Why a second path exists at all
///
/// `IPropPool` already gives a third-party aggregator everything it needs — a stateless
/// `getAmountOut` view, a plain `eth_call`, no custom code. That path is the one that gets
/// distribution, and nothing here is allowed to weaken it.
///
/// RFQ buys the two things the curve path structurally cannot:
///
///  1. **A price that need not be expressible as a ladder.** `PropCurve` is a linear-impact
///     single tick described by four `uint56`s. Everything the quoter knows — skew, volatility
///     response, a per-counterparty spread — has to be folded into those four numbers before it
///     can be posted. A signed order carries an arbitrary rate, so a quote that the ladder would
///     have to round or refuse can still be given.
///
///  2. **A quote given to one counterparty without being posted to everyone.** `updateQuote`
///     writes to public storage; every taker and every arbitrageur reads the same four prices.
///     An order is a signature handed to whoever asked for it.
///
///     Be precise about what that does and does not mean, because the difference is where RFQ
///     protocols usually oversell themselves. The **quote** is private until it is used. The
///     **fill** is not restricted: once a signature exists, anyone holding `takerAsset` can
///     present it. See the note on `fillOrder` — an on-chain taker allowlist is not
///     implementable in this architecture without the calldata-tail `payerOrigin` convention
///     that `archi_v2.md` §6.2 explicitly refuses to borrow.
///
/// # What this is a fork of, and the four things it changes
///
/// `archi_v2.md` §4.6 chose to fork `okxlabs/Web3-DEX-EVM-PMM`'s `PmmProtocol` rather than write
/// an RFQ leg from scratch, and recorded four caveats. Those caveats are the specification for
/// everything below that differs, so each is named at the place it is answered:
///
///  1. *The decay is one-directional.* Answered by [`previewFill`] (surface it before the taker
///     commits), by `maxDecayPpm` on [`fillOrder`] (let the taker bound it), and by the
///     `makerAmountQuoted` / `makerAmountFilled` pair on [`OrderFilled`] (make the difference a
///     matter of public record).
///  2. *Orders are one-shot.* Answered by [`remainingTaker`] and the exact fill accounting
///     documented on `PmmSettle`.
///  3. *The settlement floor is checked before the decay.* Answered by the ordering in
///     `PmmSettle._fill`, which is commented at the line where it matters.
///  4. *The adapter is unaudited and untested.* Answered by `test/PmmAdapter.t.sol`.
interface IPmmSettle {
    // ---------------------------------------------------------------------
    // The order
    // ---------------------------------------------------------------------

    /// @notice One firm quote, signed by the maker off chain.
    ///
    /// @dev Field widths are chosen so the whole struct fits the shared numeric domain rather
    ///      than to save calldata — an EIP-712 `encodeData` pads every member to 32 bytes
    ///      regardless, so narrowing a field costs nothing on the wire and buys an exact
    ///      off-chain mirror. `engine/crates/dubu-core/src/rfq.rs` holds `makerAmount` and
    ///      `takerAmount` in `u128` and everything else in its natural Rust integer, with no
    ///      narrowing anywhere; that is only sound because `PmmSettle.MAX_AMOUNT` rejects
    ///      anything wider on chain. Same discipline, and the same reason, as
    ///      `PropCurve.MAX_AMOUNT_OUT`: the chain and the engine share exactly one domain, so
    ///      the engine can never treat as unquotable a trade the chain would settle.
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
    ///                     `PmmSettle` for why it is checked after the decay and not before, and
    ///                     for why the forked design's hard-coded 60% is what made orders
    ///                     one-shot.
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

    // ---------------------------------------------------------------------
    // Events
    // ---------------------------------------------------------------------

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
    /// @dev `makerAmountQuoted` and `makerAmountFilled` are emitted side by side, and that pair
    ///      is the entire anti-spoofing property of this contract. A quote shown at T that
    ///      settles materially worse at T+delta is behaviourally indistinguishable from the
    ///      prop-AMM quote spoofing 0x measured at 5-10bp per fill — the thing this project
    ///      exists to not do. The honest answer to "did your RFQ leg quote one price and settle
    ///      another" is a number anyone can difference out of the log, not a blog post.
    ///
    ///      The router publishes the same shape one level up: `Router.RouteExecuted` carries
    ///      `amountOut` next to `quotedAmountOut`. This event is the per-leg version, so a route
    ///      that went through RFQ can be reconciled at both altitudes.
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

    // ---------------------------------------------------------------------
    // Settlement
    // ---------------------------------------------------------------------

    /// @notice Settle `takerAmountIn` of `order`, delivering the maker leg to `receiver`.
    ///
    /// @param takerAmountIn  taker leg to spend. Must be at most the order's remaining amount;
    ///                       this contract never silently clamps a request down.
    /// @param maxDecayPpm    the taker's ceiling on the decay applied to this fill. This is the
    ///                       taker's half of caveat 1 and it is not optional to think about:
    ///                       pass `type(uint32).max` only when the aggregate bound one level up
    ///                       is genuinely the whole protection you want.
    /// @param receiver       who receives `makerAsset`.
    /// @return makerAmountOut what was delivered, after the decay.
    ///
    /// @dev **Anyone may call this.** The signature authorises the maker's side; it does not
    ///      name a taker, and the taker leg is pulled from `msg.sender`, so a filler always pays
    ///      with their own tokens. A third party who front-runs a fill therefore buys the
    ///      maker's quote with their own money — no loss to the maker, and the same position the
    ///      forked design takes.
    ///
    ///      An on-chain taker allowlist is **deliberately not implemented**; see `PmmSettle`.
    function fillOrder(
        Order calldata order,
        bytes calldata signature,
        uint256 takerAmountIn,
        uint32 maxDecayPpm,
        address receiver
    ) external returns (uint256 makerAmountOut);

    // ---------------------------------------------------------------------
    // Cancellation
    // ---------------------------------------------------------------------

    /// @notice Retire one of the caller's nonces.
    function cancelNonce(uint64 nonce) external;

    /// @notice Retire up to 256 of the caller's nonces in one `SSTORE`.
    /// @param slot the bitmap slot, `nonce >> 8`.
    /// @param mask bits to set; bit `i` retires nonce `slot * 256 + i`.
    /// @dev The kill switch. `archi_v2.md` §5.4 requires the engine's bleed and loss-budget
    ///      latches to withdraw every live quote on trip, and an RFQ maker with a hundred
    ///      streamed orders outstanding cannot do that one transaction at a time.
    function cancelNonceSlot(uint256 slot, uint256 mask) external;

    // ---------------------------------------------------------------------
    // Views
    // ---------------------------------------------------------------------

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
    /// @dev This is how the decay reaches the taker **before** they commit, and `atTimestamp` is
    ///      an argument rather than `block.timestamp` because that is the honest question. The
    ///      decay a taker suffers depends on when their transaction is *included*, not on when
    ///      the quote was served, so a frontend that priced at `now` would show a number it
    ///      knows is optimistic. Quote it at the expected inclusion time, and show the value at
    ///      `expiry` next to it as the worst case the taker is agreeing to.
    ///
    ///      `pure`, so an aggregator batches it into the same `eth_call` as everything else.
    function previewFill(Order calldata order, uint256 takerAmountIn, uint256 atTimestamp)
        external
        pure
        returns (uint256 quotedMakerOut, uint256 realisedMakerOut, uint256 decayPpm);

    /// @notice EIP-712 domain separator for this contract on the *current* chain.
    // forge-lint: disable-next-line(mixed-case-function)
    function DOMAIN_SEPARATOR() external view returns (bytes32);
}
