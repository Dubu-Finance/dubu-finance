// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {IAdapter, IERC20Minimal} from "../interfaces/IAdapter.sol";
import {IPmmSettle} from "../interfaces/IPmmSettle.sol";
import {SafeTransfer} from "../libraries/SafeTransfer.sol";

/// @notice The one ERC-20 write `IERC20Minimal` does not declare and this adapter needs.
/// @dev Local rather than added to `IERC20Minimal`, so the shared interface keeps describing a
///      system with no allowances in it and the exception stays visible in the file that makes it.
interface IERC20Approve {
    function approve(address spender, uint256 amount) external returns (bool);
}

/// @title PmmAdapter
/// @notice Routes a hop into `PmmSettle` — the RFQ leg — behind the same `IAdapter` surface as
///         every other venue.
///
/// Stateless, unowned, permissionless, immutable, and holding no storage variables. The order and
/// its signature arrive in the step's `data` blob and are used once. Nothing is memoised: what a
/// cached order would go stale against is not a re-pushed ladder but a `remaining` counter that any
/// other taker can move in the block before this one lands.
///
/// # Two things the planner must get right
///
///  1. **Bit 254 of the step word (`fundAdapter`) must be set.** `PmmSettle` pulls the taker leg
///     from `msg.sender`, which is this adapter, so the tokens have to be here rather than at the
///     venue. Forget it and the balance read below is zero and the leg reverts `NothingToFill`.
///  2. **The step's `pool` field is the `PmmSettle` address**, not the maker's. The maker is inside
///     the signed order; it is not the router's to choose.
///
/// # `sellBase` and `sellQuote` are the same function here
///
/// Bit 255 of the step word selects a direction, and there is none to select: an order names
/// `makerAsset` and `takerAsset` explicitly and the maker signed that pair, so the "reverse" of an
/// order is a different order with a different signature. Both entry points are implemented
/// identically, because reverting from `sellQuote` would reject an executable plan over a bit that
/// carries no information for this venue.
///
/// @dev Never a custodian between transactions; during one it is. The router pushes the taker leg
///      here, this contract grants `PmmSettle` an allowance for exactly what it is about to spend,
///      and `PmmSettle` consumes exactly that, so both the balance and the allowance are zero when
///      the call returns. `test/PmmAdapter.t.sol` asserts both after every path.
///
///      That allowance is a real exception to `IAdapter`'s "no approval to revoke", bounded three
///      ways: granted to one address decoded from the plan, sized to a single fill, and consumed in
///      the same call frame. Avoiding it would mean `PmmSettle` measuring its own balance delta,
///      making it a custodian — the larger of the two concessions.
contract PmmAdapter is IAdapter {
    // --- Errors ---

    error ZeroSettle();
    error NothingToFill();
    error ApproveFailed(address token);

    // --- IAdapter ---

    /// @notice Fill an RFQ order.
    /// @param to     where the maker leg is delivered. Always the router.
    /// @param pool   the `PmmSettle` instance.
    /// @param data   `abi.encode(order, signature, maxDecayPpm)` — see `encodePayload`.
    function sellBase(address to, address pool, bytes calldata data) external override {
        _fill(to, pool, data);
    }

    /// @notice Identical to `sellBase`. See the contract note on why there is no second direction.
    function sellQuote(address to, address pool, bytes calldata data) external override {
        _fill(to, pool, data);
    }

    // --- Payload ---

    /// @notice Build the `data` blob for a step against this adapter.
    ///
    /// @param order        the maker's signed order, verbatim. Every field is covered by the
    ///                     signature, so a planner that edits one gets `BadSignature` rather than
    ///                     a differently-priced fill.
    /// @param signature    65-byte `(r, s, v)` over the order's EIP-712 digest.
    /// @param maxDecayPpm  this leg's ceiling on the quote decay, in ppm of the maker leg.
    ///
    /// @dev `maxDecayPpm` is where `PropPoolAdapter`'s `limitAmount` would go, and it is a
    ///      different shape. On the curve path a leg-level bound has to be an absolute amount,
    ///      because the pool prices against live storage. Here the rate is fixed in a signature the
    ///      taker already inspected and the only free variable between quoting and settlement is
    ///      how old the quote is when it lands, so bounding the age bounds the outcome — and does
    ///      so independently of how much of the order is still available. A plan pinning a minimum
    ///      output for a full fill would revert on a partial one, which is what a streamed quote is
    ///      for.
    ///
    ///      Pass `type(uint32).max` to accept whatever the order's own `decayCap` permits. The
    ///      router's aggregate `minAmountOut` is a weaker bound, since a good leg can cover for a
    ///      decayed one.
    ///
    ///      Exposed `external pure` so the tests and the Rust planner encode against one definition
    ///      instead of two, as `PropPoolAdapter.encodePayload` does.
    function encodePayload(IPmmSettle.Order calldata order, bytes calldata signature, uint32 maxDecayPpm)
        external
        pure
        returns (bytes memory)
    {
        return abi.encode(order, signature, maxDecayPpm);
    }

    // --- Internals ---

    /// @dev No exact-length check on `data`, unlike `PropPoolAdapter`: the payload carries a
    ///      variable-length signature, so its encoded length is not a constant and a bound would be
    ///      a guess. `abi.decode` still rejects a malformed blob, and a well-formed blob carrying
    ///      the wrong order fails at the signature.
    function _fill(address to, address settle, bytes calldata data) private {
        if (settle == address(0)) revert ZeroSettle();

        (IPmmSettle.Order memory order, bytes memory signature, uint32 maxDecayPpm) =
            abi.decode(data, (IPmmSettle.Order, bytes, uint32));

        // The router pushed this leg's tokens here (bit 254). Reading the balance back rather than
        // being told the size is what makes `IAdapter`'s missing amount parameter work, and why a
        // mis-set funding bit surfaces as `NothingToFill` rather than a fill of the wrong size.
        uint256 balance = IERC20Minimal(order.takerAsset).balanceOf(address(this));

        // Read live, never cached, and *after* the tokens have arrived: other takers may have
        // consumed part of this same order since the plan was built, so the remaining amount is not
        // a number the plan is allowed to assert.
        uint256 remaining = IPmmSettle(settle).remainingTaker(order);

        uint256 takerIn = balance < remaining ? balance : remaining;
        if (takerIn == 0) revert NothingToFill();

        // Exactly what the fill will consume, so the allowance is zero again when `fillOrder`
        // returns. Sizing it to `balance` would leave a live allowance behind on the over-funded
        // path, and `type(uint256).max` would leave a permanent one — an adapter with a standing
        // approval attached, which `IAdapter` says never exists. The always-zero end state also
        // keeps this compatible with USDT-shaped tokens that refuse a non-zero-to-non-zero
        // `approve`.
        _approve(order.takerAsset, settle, takerIn);

        IPmmSettle(settle).fillOrder(order, signature, takerIn, maxDecayPpm, to);

        // Over-funding is a normal outcome, not an error: the plan sized this leg against a
        // remaining amount that has since shrunk. The surplus goes to `to`, the router, which is
        // the only address in scope this adapter can name — an adapter has no idea who the payer
        // is. The router then sweeps it back to the payer if `takerAsset` is the route's input
        // token, or strands it if it is an intermediate, exactly as for an under-allocated
        // intermediate hop in `RouteDecoder.validateWeightSum`. Plans that can over-fund this leg
        // should place it on a batch's first hop.
        uint256 leftover = balance - takerIn;
        if (leftover != 0) SafeTransfer.safeTransfer(order.takerAsset, to, leftover);
    }

    /// @dev `token.approve(spender, amount)`, tolerating the tokens that return nothing. Same
    ///      three-part test as `SafeTransfer` — the call succeeded, the return was empty or a
    ///      32-byte `true`, and the target has code — but written in the high-level form, since
    ///      this is one call per fill rather than two per swap. `SafeTransfer` is documented as
    ///      transfer helpers on the swap hot path and is not gaining a `safeApprove`.
    ///
    ///      The `code.length` check is the one that matters: a call to an address with no code
    ///      succeeds with empty return data, so without it a mistyped `takerAsset` would approve
    ///      nothing and the fill would revert somewhere less obvious.
    function _approve(address token, address spender, uint256 amount) private {
        if (token.code.length == 0) revert ApproveFailed(token);
        (bool ok, bytes memory ret) = token.call(abi.encodeCall(IERC20Approve.approve, (spender, amount)));
        if (!ok || (ret.length != 0 && !abi.decode(ret, (bool)))) revert ApproveFailed(token);
    }
}
