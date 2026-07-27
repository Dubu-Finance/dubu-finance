// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

/// @title IPropPool
/// @notice External integration surface for the DuBu proprietary AMM.
///
/// Two quoting paths are exposed on purpose:
///
///  1. `getAmountOut(tokenIn, tokenOut, amountIn)` — token-address addressed, stateless view.
///     This is the path third-party aggregators (0x, KyberSwap, OKX) integrate with. It costs
///     them a plain `eth_call` and zero custom code, which is exactly how Wasabi and ElfomoFi
///     got distribution on Base. Do not break this signature.
///
///  2. `quoteByPair(pairId, ...)` — index addressed. Same math, one less lookup. Used by our
///     own router where the pair id is already known from the routing plan.
///
/// @dev `swap` follows the ElfomoFi/Wasabi convention of a signed `specifiedAmount` so a single
///      entrypoint covers exact-in and exact-out. Aggregators already generate calldata for this
///      shape.
interface IPropPool {
    // ---------------------------------------------------------------------
    // Types
    // ---------------------------------------------------------------------

    struct Pair {
        uint16 pairId;
        address base;
        address quote;
    }

    /// @notice Everything a quoter needs to reproduce the curve off-chain for one pair.
    /// @dev Returned as a struct so an aggregator can fetch state once and simulate many sizes
    ///      locally without N round trips. `usedGen != capGen` means the used counters are stale
    ///      and must be treated as zero — see PropPool's generation mechanism.
    ///
    ///      **All four capacity and usage fields are denominated in the pair's BASE token**, ask
    ///      side included. `askCapacity` is how much base the pool will sell this epoch and
    ///      `askUsed` how much it has sold, so an ask's quote-denominated `amountIn` is not
    ///      comparable with either — convert it through the curve first (`PropCurve.amountInAsk`
    ///      gives the quote cost of a base amount, `amountOutAsk` inverts it). This changed with
    ///      PropCurve's amendment 1: a quote-denominated ask capacity made the output the
    ///      reciprocal of the midpoint price, and `1/p` being convex made splitting an ask
    ///      strictly dominant for the taker. Bit positions in the packed words did not move.
    ///
    ///      The four prices are `uint56` and the four capacity/usage fields `uint96`, but a
    ///      simulator must also respect the shared domain: every quote-denominated amount the
    ///      curve accepts or returns is capped at `type(uint128).max`. `refreshCapacity` will not
    ///      accept a capacity that could push a quote leg past it, so a snapshot never describes a
    ///      pair whose own capacity is out of domain.
    ///
    ///      ## `flags`
    ///
    ///      | bit    | meaning                                                                  |
    ///      |--------|--------------------------------------------------------------------------|
    ///      | 0      | paused — this pair will not quote or fill                                 |
    ///      | 1..14  | reserved                                                                 |
    ///      | 15     | **bounded** — a reference oracle is configured for this pair              |
    ///
    ///      Bits 0..14 are read straight out of the pool's capacity word. **Bit 15 is derived, not
    ///      stored**, and is the answer to "is this pair's ladder checked against an independent
    ///      price, or does it quote on the operator's word alone?" Zero is a supported production
    ///      configuration — not every listable asset has a Pyth feed, and a pair that could not be
    ///      listed without one would simply not be listed — so an integrator sizing its exposure to
    ///      this venue should be able to tell the two apart, and this is where.
    ///
    ///      The bit says a bound is *configured*, not that it is currently *satisfiable*; the
    ///      latter needs a live oracle read, which no function under the no-revert contract makes.
    ///      `PropPool.pairOracle(pairId)` returns the full configuration and
    ///      `PropPool.referencePrice(pairId)` returns the current reference with a status code,
    ///      both totally, if you need more than the bit.
    ///
    ///      **Do not append fields to this struct.** It is a static tuple and off-chain consumers
    ///      mirror it positionally — the Rust updater declares its own `sol!` copy — so a new
    ///      field changes the return encoding under every one of them at once. New per-pair state
    ///      goes in its own view, as `pairOracle` did.
    struct PairSnapshot {
        uint56 minBid;
        uint56 maxBid;
        uint56 minAsk;
        uint56 maxAsk;
        uint32 updatedAt;
        uint96 bidCapacity;
        uint96 askCapacity;
        uint96 bidUsed;
        uint96 askUsed;
        uint32 capGen;
        uint32 usedGen;
        uint16 flags;
        uint8 priceScaleExp;
        uint32 maxStaleSecs;
    }

    // ---------------------------------------------------------------------
    // Events
    // ---------------------------------------------------------------------

    event PairAdded(uint16 indexed pairId, address indexed base, address indexed quote);
    event QuoteUpdated(uint16 indexed pairId, uint56 minBid, uint56 maxBid, uint56 minAsk, uint56 maxAsk);
    /// @dev Both capacities are BASE units — the base the pool will buy and sell this epoch. See
    ///      `PairSnapshot`.
    event CapacityRefreshed(uint16 indexed pairId, uint96 bidCapacity, uint96 askCapacity, uint32 capGen);
    event Swap(
        uint16 indexed pairId,
        address indexed sender,
        address indexed receiver,
        bool isBid,
        uint256 amountIn,
        uint256 amountOut,
        uint256 partnerId
    );
    event Paused(uint16 indexed pairId, bool paused);

    // ---------------------------------------------------------------------
    // Quoting — view, stateless, must never revert on "no liquidity"
    // ---------------------------------------------------------------------

    /// @notice Aggregator-facing quote. Returns 0 rather than reverting when the pool cannot
    ///         fill (stale quote, paused, capacity exhausted, unknown pair). Reverting here
    ///         breaks batch quoting for integrators, so don't.
    ///
    /// @dev "Don't" is total, and it is a stronger claim than the list above. `amountIn` is
    ///      unvalidated caller input, so the obligation covers arithmetic as well as liquidity:
    ///      every amount that would take the curve outside the `uint128` domain it shares with the
    ///      off-chain engine, and every amount that would overflow an intermediate, must also come
    ///      back as 0. `type(uint256).max` is a valid argument with the answer 0, not a revert.
    ///      A single poisoned pair must not be able to take down an aggregator's whole multicall,
    ///      including the quotes for healthy pairs batched alongside it.
    ///
    ///      `amountIn` is base when `tokenIn` is the pair's base token and quote otherwise; the
    ///      return is always the other token. Zero-cost integration path — do not break the
    ///      signature.
    function getAmountOut(address tokenIn, address tokenOut, uint256 amountIn)
        external
        view
        returns (uint256 amountOut);

    /// @notice Inverse of `getAmountOut`: the input needed to receive exactly `amountOut`.
    ///
    /// @dev Same no-revert obligation, same reasons, including for absurd inputs — `amountOut`
    ///      within a reserve floor of `type(uint256).max` must return 0 and not panic.
    ///
    ///      Rounds up: the returned input is the least one that delivers at least `amountOut`, so
    ///      `getAmountOut(getAmountIn(y)) >= y` and any sub-unit surplus stays with the pool.
    function getAmountIn(address tokenIn, address tokenOut, uint256 amountOut)
        external
        view
        returns (uint256 amountIn);

    /// @notice Index-addressed quote. `isBid == true` means base in / quote out; `false` means
    ///         quote in / base out. Same math and the same no-revert obligation as `getAmountOut`,
    ///         one lookup cheaper.
    function quoteByPair(uint16 pairId, bool isBid, uint256 amountIn) external view returns (uint256 amountOut);

    /// @notice Full curve state for one pair, for off-chain simulation.
    function snapshot(uint16 pairId) external view returns (PairSnapshot memory);

    // ---------------------------------------------------------------------
    // Swapping
    // ---------------------------------------------------------------------

    /// @param specifiedAmount positive = exact input, negative = exact output. Denominated in the
    ///                        leg it pins: `tokenIn` if positive, `tokenOut` if negative. The pool
    ///                        charges its capacity epoch in base either way, so the amount the
    ///                        epoch's budget sees is the input of a bid and the output of an ask.
    /// @param limitAmount     exact-in: minimum out. exact-out: maximum in.
    /// @param partnerId       routing-source tag. Reserved for rebates and spread tiering;
    ///                        see the OnchainVerify tiering design. Unused ids are not rejected.
    /// @return result         exact-in: amount out. exact-out: amount in.
    function swap(
        address tokenIn,
        address tokenOut,
        int256 specifiedAmount,
        uint256 limitAmount,
        address receiver,
        uint256 partnerId,
        uint256 deadline
    ) external returns (uint256 result);

    /// @notice Swap where the caller has already transferred `tokenIn` to this contract.
    /// @dev For routers that push tokens before calling, avoiding an approval hop. The pool
    ///      measures its own balance delta, so the caller must not batch unrelated transfers
    ///      of `tokenIn` into the same transaction.
    function swapWithContractBalance(
        address tokenIn,
        address tokenOut,
        uint256 limitAmount,
        address receiver,
        uint256 partnerId,
        uint256 deadline
    ) external returns (uint256 amountOut);

    // ---------------------------------------------------------------------
    // Discovery
    // ---------------------------------------------------------------------

    function getSupportedPairs() external view returns (Pair[] memory);
    function pairIdFor(address tokenIn, address tokenOut) external view returns (uint16 pairId, bool isBid);
}

/// @notice Callback for the pull-style swap path.
interface IPropSwapCallback {
    /// @dev Called after `tokenOut` has been sent to `receiver` and before the pool checks that
    ///      `tokenIn` has arrived. The callee must transfer exactly `amountIn` of `tokenIn` to
    ///      the pool before returning.
    function propSwapCallback(address tokenIn, uint256 amountIn, bytes calldata data) external;
}
