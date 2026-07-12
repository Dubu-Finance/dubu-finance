// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

/// @title RouteDecoder
/// @notice Bit-packed route steps: one `uint256` describes one leg of a swap.
///
/// The second (and last) idea borrowed from OKX DEX-Router-EVM-V1. On an L2 the dominant cost of a
/// swap is calldata posted to L1 DA — archi_v2.md §4.3 measures a non-zero calldata byte at 16 gas
/// against ~5 gas for a mul/div, so trading arithmetic for bytes wins. Packing the three fields
/// that describe *where* and *how much* into a single word costs 32 bytes per leg instead of the
/// 96 an ABI-encoded `(address,uint16,bool)` struct would burn.
///
/// ```text
///   bit  255      reverse       call sellQuote instead of sellBase
///   bit  254      fundAdapter   push this leg's tokens to the adapter, not to the pool
///   bits 253..176 reserved      MUST be zero
///   bits 175..160 weight        share of this hop's input, out of WEIGHT_DENOMINATOR
///   bits 159..0   pool          the venue contract
/// ```
///
/// Bits 255, 175..160 and 159..0 are the borrowed layout, unchanged, so an off-chain encoder
/// written against OKX's `rawData` convention keeps working.
///
/// Bit 254 is ours. OKX carries the funding destination in a parallel `assetTo[]` array, which is
/// another 32 calldata bytes per leg for one bit of information. Both adapters shipped in v1 want
/// the tokens at the *pool* (a UniV2 pair and `IPropPool.swapWithContractBalance` each settle
/// against their own balance delta), so `fundAdapter = 0` is the common path and the flag exists
/// only so a future venue that needs the adapter to hold the funds does not force a struct change.
///
/// Bits 253..176 are checked to be zero rather than ignored. If a later router version gives them
/// meaning, a step encoded for that version must fail loudly here instead of being silently
/// reinterpreted by this one. Silent reinterpretation of an unrecognised encoding is exactly the
/// class of bug that makes aggregator calldata unauditable.
///
/// @dev Pure, storage-free, and mirrored by the Rust path finder. Keep it that way so the two can
///      be differential-tested against each other, same discipline as PropCurve.
library RouteDecoder {
    // ---------------------------------------------------------------------
    // Errors
    // ---------------------------------------------------------------------

    error PoolAddressZero();
    error ReservedBitsSet(uint256 rawData);
    error ZeroWeight();
    error WeightOutOfRange(uint256 weight);
    error WeightSumExceeded(uint256 totalWeight);

    // ---------------------------------------------------------------------
    // Layout
    // ---------------------------------------------------------------------

    /// @notice Weights are basis points of the hop's available input.
    uint256 internal constant WEIGHT_DENOMINATOR = 10_000;

    uint256 private constant _REVERSE_FLAG = 1 << 255;
    uint256 private constant _FUND_ADAPTER_FLAG = 1 << 254;
    uint256 private constant _WEIGHT_OFFSET = 160;
    uint256 private constant _WEIGHT_MASK = 0xffff;
    uint256 private constant _POOL_MASK = (1 << 160) - 1;
    /// @dev bits 253..176 — everything between the two flags and the weight field.
    uint256 private constant _RESERVED_MASK = ((1 << 78) - 1) << 176;

    // ---------------------------------------------------------------------
    // Decode
    // ---------------------------------------------------------------------

    /// @notice The venue this leg trades against.
    /// @dev The `uint160` cast cannot truncate: `_POOL_MASK` has already cleared everything above
    ///      bit 159, and `validateStep` has already rejected any word with reserved bits set.
    function pool(uint256 rawData) internal pure returns (address) {
        // forge-lint: disable-next-line(unsafe-typecast)
        return address(uint160(rawData & _POOL_MASK));
    }

    /// @notice This leg's share of its hop's input, in basis points.
    function weight(uint256 rawData) internal pure returns (uint256) {
        return (rawData >> _WEIGHT_OFFSET) & _WEIGHT_MASK;
    }

    /// @notice True when the router must call `sellQuote` rather than `sellBase`.
    function isReverse(uint256 rawData) internal pure returns (bool) {
        return rawData & _REVERSE_FLAG != 0;
    }

    /// @notice True when this leg's tokens go to the adapter instead of straight to the pool.
    function fundsAdapter(uint256 rawData) internal pure returns (bool) {
        return rawData & _FUND_ADAPTER_FLAG != 0;
    }

    // ---------------------------------------------------------------------
    // Encode (off-chain parity / tests)
    // ---------------------------------------------------------------------

    /// @notice Inverse of the decoders. Exists so the Solidity tests and the Rust encoder can be
    ///         checked against one definition of the layout instead of two.
    function encode(address venue, uint256 weightBps, bool reverse, bool fundAdapter)
        internal
        pure
        returns (uint256 rawData)
    {
        if (venue == address(0)) revert PoolAddressZero();
        if (weightBps == 0) revert ZeroWeight();
        if (weightBps > WEIGHT_DENOMINATOR) revert WeightOutOfRange(weightBps);

        rawData = uint256(uint160(venue)) | (weightBps << _WEIGHT_OFFSET);
        if (reverse) rawData |= _REVERSE_FLAG;
        if (fundAdapter) rawData |= _FUND_ADAPTER_FLAG;
    }

    // ---------------------------------------------------------------------
    // Validation
    // ---------------------------------------------------------------------

    /// @notice Per-step sanity: known encoding, real pool, usable weight.
    function validateStep(uint256 rawData) internal pure {
        if (rawData & _RESERVED_MASK != 0) revert ReservedBitsSet(rawData);
        if (rawData & _POOL_MASK == 0) revert PoolAddressZero();

        uint256 w = (rawData >> _WEIGHT_OFFSET) & _WEIGHT_MASK;
        if (w == 0) revert ZeroWeight();
        // The field is 16 bits wide but only 10_000 of the 65_536 values are meaningful. Rejecting
        // the rest catches an encoder that packed a percentage, or a price, into the weight slot.
        if (w > WEIGHT_DENOMINATOR) revert WeightOutOfRange(w);
    }

    /// @notice The fork-weight invariant: the legs of one hop may claim **at most** the whole
    ///         input, never more.
    ///
    /// @dev Why at-most and not exactly-10_000 — three reasons, in descending order of importance:
    ///
    ///  1. **Over-allocation is the only direction that is actually dangerous, and this rejects
    ///     it.** Weights summing above 10_000 would have the hop try to spend more than it holds.
    ///     Against a normal adapter that reverts on the last leg for insufficient balance, which
    ///     is merely a bad user experience. Against balance-reading adapters — which is *all* of
    ///     them, by construction of `IAdapter` — it is worse: the surplus would be drawn from
    ///     whatever else the router happens to be holding, i.e. another batch's funds, mid-route.
    ///     Under-allocation cannot overspend, so it needs no defence.
    ///
    ///  2. **"Exactly 10_000" would be a false comfort.** Each leg gets `floor(available * w /
    ///     10_000)`; those floors sum to at most `available` and generically to strictly less.
    ///     A remainder therefore exists no matter what the declared weights sum to, so the caller
    ///     still needs a remainder rule. Demanding exact equality would advertise a precision the
    ///     arithmetic does not deliver.
    ///
    ///  3. **Deliberate partial routing is legitimate.** The prop AMM's per-epoch remaining
    ///     capacity (archi_v2.md §4.4) caps how much a leg can absorb; a planner that wants to
    ///     send only 70% of a hop through the venues it found — because the other 30% would cross
    ///     into a worse ladder rung, or simply revert on `AmountExceedsCapacity` — should say so
    ///     with weights summing to 7_000, not be forced to invent a filler leg.
    ///
    /// How the remainder is handled, precisely — this rule lives in `Router`, and it is
    /// two-branched on purpose:
    ///
    ///  * `totalWeight == WEIGHT_DENOMINATOR`: the caller asked for the whole input to be routed,
    ///    so the **last** leg receives `available - sum(previous floors)` instead of its own floor
    ///    share. Rounding dust is absorbed by the last leg and nothing is stranded. Which leg
    ///    absorbs it is arbitrary; last is chosen because it needs no extra pass.
    ///
    ///  * `totalWeight < WEIGHT_DENOMINATOR`: every leg, including the last, takes its own floor
    ///    share, and the withheld portion stays with the router. Topping the last leg up here
    ///    would silently repair a mistyped weight and change the route the caller signed off on,
    ///    which is a worse failure mode than leaving the tokens behind.
    ///
    /// The withheld portion of the **route's input token** is swept back to the payer when the
    /// swap finishes. Withholding on an *intermediate* hop instead strands that intermediate token
    /// in the router, where the next caller's balance read will pick it up — the router is
    /// ownerless and has no rescue function by design. Plans should only under-allocate on a
    /// batch's first hop.
    function validateWeightSum(uint256 totalWeight) internal pure {
        if (totalWeight > WEIGHT_DENOMINATOR) revert WeightSumExceeded(totalWeight);
    }

    // ---------------------------------------------------------------------
    // Allocation
    // ---------------------------------------------------------------------

    /// @notice A leg's floor share of `available`.
    /// @dev Rounds down, so the sum over a hop never exceeds `available` — see `validateWeightSum`
    ///      for what happens to the difference.
    function shareOf(uint256 available, uint256 weightBps) internal pure returns (uint256) {
        return (available * weightBps) / WEIGHT_DENOMINATOR;
    }
}
