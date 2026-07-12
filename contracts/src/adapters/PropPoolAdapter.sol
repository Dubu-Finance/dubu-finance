// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {IAdapter} from "../interfaces/IAdapter.sol";
import {IPropPool} from "../interfaces/IPropPool.sol";

/// @title PropPoolAdapter
/// @notice Routes a hop into the DuBu proprietary AMM.
///
/// Stateless, unowned, permissionless, immutable, and — the part that matters most for this
/// particular venue — **caching nothing**.
///
/// The prop AMM's ladder is four numbers pushed by an off-chain quoter and reset every capacity
/// epoch (archi_v2.md §4.1, §4.4). `getAmountOut` is a view that was true when it was called and
/// may not be true in the next flashblock, ~200ms later. Any adapter that read a quote and then
/// settled against it would be re-creating, inside our own router, the exact quote-spoofing window
/// that 0x measured at 5–10bp per fill on Solana prop AMMs. So this adapter never calls a quote
/// function at all. It calls `swapWithContractBalance`, and the pool prices the trade against its
/// own live storage in the same call frame that moves the tokens. There is no window.
///
/// This contract has no storage variables. That is a property to preserve, not an accident.
///
/// @dev Deployed once and shared. It holds no funds between transactions and, because the router
///      pushes each leg's tokens to the *pool* rather than here, it holds none during one either.
contract PropPoolAdapter is IAdapter {
    // ---------------------------------------------------------------------
    // Errors
    // ---------------------------------------------------------------------

    error InvalidPayload(uint256 length);
    error ZeroPool();
    error ZeroToken();
    error IdenticalTokens();

    /// @notice Exact ABI length of the payload: five 32-byte words.
    uint256 internal constant PAYLOAD_LENGTH = 160;

    // ---------------------------------------------------------------------
    // IAdapter
    // ---------------------------------------------------------------------

    /// @notice Base -> quote. The pool bids; the trade walks down the bid ladder.
    /// @param to   Where the pool must send the quote token. Always the router.
    /// @param pool The `IPropPool` instance.
    /// @param data `abi.encode(base, quote, limitAmount, partnerId, deadline)` — see `encodePayload`.
    function sellBase(address to, address pool, bytes calldata data) external override {
        (address base, address quote, uint256 limitAmount, uint256 partnerId, uint256 deadline) = _decode(data);
        _swap(pool, base, quote, limitAmount, to, partnerId, deadline);
    }

    /// @notice Quote -> base. The pool asks; the trade walks up the ask ladder.
    /// @dev Same payload as `sellBase`; only the direction of the pair is flipped. Keeping one
    ///      encoding for both directions means the off-chain planner builds the payload once per
    ///      pair and sets bit 255 of the packed step to choose the side.
    ///
    ///      The pool's external surface is identical for both directions, but the *sizing* is not:
    ///      the ask epoch's capacity is denominated in the pair's BASE token (PropCurve amendment
    ///      1), so what bounds this leg is the base the pool has left to sell, converted to a quote
    ///      ceiling at the current ladder. A planner that sizes an ask leg against `askCapacity`
    ///      directly is comparing a quote amount with a base one; use `PairSnapshot` plus the
    ///      curve's `amountInAsk` for the ceiling, or let the leg's `limitAmount` catch it.
    function sellQuote(address to, address pool, bytes calldata data) external override {
        (address base, address quote, uint256 limitAmount, uint256 partnerId, uint256 deadline) = _decode(data);
        _swap(pool, quote, base, limitAmount, to, partnerId, deadline);
    }

    // ---------------------------------------------------------------------
    // Payload
    // ---------------------------------------------------------------------

    /// @notice Build the `data` blob for a step against this adapter.
    ///
    /// @param base        the pair's base token.
    /// @param quote       the pair's quote token.
    /// @param limitAmount the pool's own bound for this leg — minimum out, since the adapter only
    ///                    ever drives the exact-in entry point. Pass 0 to rely solely on the
    ///                    router's aggregate slippage check; pass a real number when the plan
    ///                    wants this specific leg to fail rather than fill badly. The router's
    ///                    check is on the aggregate across every batch, so a leg-level bound is
    ///                    the only way to express "do not fill *this* venue below X".
    /// @param partnerId   routing-source tag, forwarded verbatim. Reserved for rebates and spread
    ///                    tiering; unused ids are accepted by the pool.
    /// @param deadline    forwarded to the pool. Not defaulted to `block.timestamp` — a silent
    ///                    always-passes deadline is worse than no deadline, because it looks like
    ///                    protection. The planner sets it and the router checks its own too.
    ///
    /// @dev Exposed as `pure external` so tests and the Rust planner encode against one definition
    ///      instead of two.
    function encodePayload(address base, address quote, uint256 limitAmount, uint256 partnerId, uint256 deadline)
        external
        pure
        returns (bytes memory)
    {
        return abi.encode(base, quote, limitAmount, partnerId, deadline);
    }

    /// @dev The pair is carried as an explicit token pair rather than as a `pairId`.
    ///      `swapWithContractBalance` is token-addressed, and `IPropPool` exposes no
    ///      id-to-tokens getter — resolving one would mean scanning the `getSupportedPairs()`
    ///      array on every hop. `pairId` is the right key for the *quoting* path, which is
    ///      off-chain; it buys nothing here.
    function _decode(bytes calldata data)
        private
        pure
        returns (address base, address quote, uint256 limitAmount, uint256 partnerId, uint256 deadline)
    {
        // Exact, not minimum. A payload of the wrong length is an encoder bug, and a bare
        // `abi.decode` would either revert with no name or silently ignore trailing junk.
        if (data.length != PAYLOAD_LENGTH) revert InvalidPayload(data.length);
        (base, quote, limitAmount, partnerId, deadline) =
            abi.decode(data, (address, address, uint256, uint256, uint256));
    }

    // ---------------------------------------------------------------------
    // Internals
    // ---------------------------------------------------------------------

    /// @dev The router has already transferred this leg's `tokenIn` to `pool`;
    ///      `swapWithContractBalance` measures the pool's own balance delta to size the trade.
    ///
    ///      That measurement is why the receiver is `to` — the router — and never something
    ///      decoded from `data`. The router sizes the next hop from its own balance delta, so
    ///      diverting a leg's output elsewhere would leave it measuring zero and reverting, or
    ///      worse, measuring somebody else's tokens.
    ///
    ///      Note for the tiering work in archi_v2.md §4.5: from the pool's point of view
    ///      `msg.sender` here is *this adapter*, not the router and certainly not the taker. Any
    ///      caller-based spread tiering must key off `partnerId` or an explicit verified-taker
    ///      flag, never `msg.sender`.
    function _swap(
        address pool,
        address tokenIn,
        address tokenOut,
        uint256 limitAmount,
        address to,
        uint256 partnerId,
        uint256 deadline
    ) private {
        if (pool == address(0)) revert ZeroPool();
        if (tokenIn == address(0) || tokenOut == address(0)) revert ZeroToken();
        if (tokenIn == tokenOut) revert IdenticalTokens();

        IPropPool(pool).swapWithContractBalance(tokenIn, tokenOut, limitAmount, to, partnerId, deadline);
    }
}
