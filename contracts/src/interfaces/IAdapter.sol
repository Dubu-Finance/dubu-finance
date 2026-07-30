// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

/// @title IAdapter
/// @notice The one venue-integration primitive the DuBu router knows about.
///
/// Borrowed, deliberately and in full, from OKX DEX-Router-EVM-V1. What it buys is what it
/// *omits*:
///
///   **There is no amount parameter.**
///
/// The router moves the tokens for a hop *before* it calls the adapter — either straight to the
/// venue (the usual case: UniV2 pairs and `IPropPool.swapWithContractBalance` both settle against
/// their own measured balance delta) or to the adapter itself. The adapter then reads a balance to
/// learn how much it is working with. Three consequences follow:
///
///  1. **Stateless.** An adapter holds no storage, so nothing can go stale. That matters most for
///     the prop AMM, whose ladder is re-pushed every quote cycle — an adapter that memoised a
///     quote would settle against a price that no longer exists.
///  2. **Unowned and permissionless.** No admin, no pause, no upgrade path, no allowlist, so a
///     third party can deploy an adapter for their own venue and route through us without asking.
///     That is a precondition for the neutrality position in archi_v2.md §6.3: we cannot credibly
///     claim "the prop AMM is just one adapter among many" if we control which adapters exist.
///  3. **Fund-free between transactions.** An adapter is never a custodian, so there is no standing
///     balance to drain and no approval to revoke.
///
/// @dev Adapter addresses arrive in router calldata and are NOT allowlisted. A hostile adapter can
///      therefore be named by a caller, but it can only consume what that same caller's route
///      pushed into it plus any stray dust the router is holding, and the router's reentrancy guard
///      and post-route sweep keep the blast radius at the caller's own money. Do not add an
///      allowlist to fix a problem the push model already solves; that trades away point 2 for
///      nothing.
interface IAdapter {
    /// @notice Sell the venue's *base* side. For a UniV2 pair that is `token0 -> token1`; for a
    ///         DuBu prop pair it is `base -> quote` (the pool bids).
    /// @param to    Where the venue must deliver output. The router always passes itself, because
    ///              it re-measures its own balance between hops rather than trusting a number
    ///              supplied off-chain.
    /// @param pool  The venue contract, decoded from bits 159..0 of the step's packed word.
    /// @param data  Opaque, adapter-defined payload. Whatever this particular venue's call needs
    ///              that is not derivable on-chain cheaply.
    function sellBase(address to, address pool, bytes calldata data) external;

    /// @notice Sell the venue's *quote* side — the mirror of `sellBase`. Selected by bit 255 of
    ///         the step's packed word.
    function sellQuote(address to, address pool, bytes calldata data) external;
}

/// @notice The slice of ERC-20 the router and its adapters actually touch.
/// @dev Local rather than pulled from OpenZeppelin: this repo's only dependency is forge-std, and a
///      three-function local interface is easier to audit than a transitive import.
///
///      The `bool` returns are declared because compliant tokens return them, but no caller here
///      may assume a return value exists — USDT-shaped tokens return nothing at all. Every write
///      goes through the router's `_safeTransfer`/`_safeTransferFrom`, which treat empty returndata
///      as success and a `false` return as failure.
interface IERC20Minimal {
    function balanceOf(address account) external view returns (uint256);
    function transfer(address to, uint256 amount) external returns (bool);
    function transferFrom(address from, address to, uint256 amount) external returns (bool);
}
