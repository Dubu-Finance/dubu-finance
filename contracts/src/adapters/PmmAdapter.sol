// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {IAdapter, IERC20Minimal} from "../interfaces/IAdapter.sol";
import {IPmmSettle} from "../interfaces/IPmmSettle.sol";
import {SafeTransfer} from "../libraries/SafeTransfer.sol";

/// @notice The one ERC-20 write `IERC20Minimal` does not declare and this adapter needs.
/// @dev Declared here rather than added to `IERC20Minimal` because no other adapter approves
///      anything and the router never approves anything at all. Keeping it local means the shared
///      interface continues to describe a system with no allowances in it, and the exception is
///      visible in the one file that makes it.
interface IERC20Approve {
    function approve(address spender, uint256 amount) external returns (bool);
}

/// @title PmmAdapter
/// @notice Routes a hop into `PmmSettle` — the RFQ leg — behind the same `IAdapter` surface as
///         every other venue.
///
/// Stateless, unowned, permissionless, immutable, and holding no storage variables. The order and
/// its signature arrive in the step's `data` blob and are used once, in the frame they arrive in.
/// Nothing is memoised: for RFQ that matters even more than it does for the curve path, because
/// what a cached order would go stale against is not a re-pushed ladder but a `remaining` counter
/// that any other taker can move in the block before this one lands.
///
/// # Two things the planner must get right, and how each fails loudly
///
///  1. **Bit 254 of the step word (`fundAdapter`) must be set.** `PmmSettle` pulls the taker leg
///     from `msg.sender`, and `msg.sender` is this adapter, so the tokens have to be here rather
///     than at the venue. This is the first shipped adapter to need bit 254 —
///     `RouteDecoder` predicted it ("the flag exists only so a future venue that needs the adapter
///     to hold the funds does not force a struct change") and this is that venue. Forget it and
///     the balance read below is zero and the leg reverts `NothingToFill`.
///
///  2. **The step's `pool` field is the `PmmSettle` address**, not the maker's. The maker is
///     inside the signed order; it is not the router's to choose.
///
/// # `sellBase` and `sellQuote` are the same function here, and that is not laziness
///
/// Bit 255 of the step word selects a direction. There is no direction to select: an order names
/// `makerAsset` and `takerAsset` explicitly and the maker signed that pair, so the "reverse" of an
/// order is a different order with a different signature. Both entry points are implemented, both
/// do the same thing, and a planner may set bit 255 or not with identical results. The alternative
/// — reverting from `sellQuote` — would reject a perfectly executable plan over a bit that carries
/// no information for this venue.
///
/// @dev Never a custodian between transactions. During one it is: the router pushes the taker leg
///      here, this contract grants `PmmSettle` an allowance for exactly what it is about to spend,
///      and `PmmSettle` consumes exactly that. Both the balance and the allowance are provably
///      zero when the call returns — anything the fill did not consume is swept to `to` on the way
///      out, and the allowance was sized to the fill. `test/PmmAdapter.t.sol` asserts both after
///      every path, including the over-funded one.
///
///      That allowance is a real exception to `IAdapter`'s "no approval to revoke" and is worth
///      naming rather than burying. It is bounded three ways: it is granted to one immutable-per-
///      call address decoded from the plan, it is sized to a single fill, and it is consumed in
///      the same call frame. Avoiding it entirely would mean `PmmSettle` measuring its own balance
///      delta — which would make it a custodian, which is the larger of the two concessions.
contract PmmAdapter is IAdapter {
    // ---------------------------------------------------------------------
    // Errors
    // ---------------------------------------------------------------------

    error ZeroSettle();
    error NothingToFill();
    error ApproveFailed(address token);

    // ---------------------------------------------------------------------
    // IAdapter
    // ---------------------------------------------------------------------

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

    // ---------------------------------------------------------------------
    // Payload
    // ---------------------------------------------------------------------

    /// @notice Build the `data` blob for a step against this adapter.
    ///
    /// @param order        the maker's signed order, verbatim. Every field is covered by the
    ///                     signature, so a planner that edits one gets `BadSignature` rather than
    ///                     a differently-priced fill.
    /// @param signature    65-byte `(r, s, v)` over the order's EIP-712 digest.
    /// @param maxDecayPpm  this leg's ceiling on the quote decay, in ppm of the maker leg.
    ///
    /// @dev `maxDecayPpm` is where `PropPoolAdapter`'s `limitAmount` would go, and it is a
    ///      different shape on purpose.
    ///
    ///      On the curve path a leg-level bound has to be an absolute amount, because the pool
    ///      prices against live storage and the planner cannot know what the ladder will say. Here
    ///      the rate is fixed in a signature the taker already inspected, and the *only* free
    ///      variable between quoting and settlement is how old the quote is when it lands. So
    ///      bounding the age bounds the outcome exactly — and, unlike an absolute amount, it does
    ///      so independently of how much of the order is still available. A plan that pinned a
    ///      minimum output for a full fill would revert on a partial one, which is the opposite of
    ///      what a streamed quote is for.
    ///
    ///      Pass `type(uint32).max` to accept whatever the order's own `decayCap` permits. That is
    ///      a real choice with a real cost and should be made explicitly; the aggregate
    ///      `minAmountOut` the router enforces is a weaker bound, because it can be satisfied by a
    ///      good leg covering for a decayed one.
    ///
    ///      Exposed `external pure` so the tests and the Rust planner encode against one
    ///      definition instead of two, exactly as `PropPoolAdapter.encodePayload` does.
    function encodePayload(IPmmSettle.Order calldata order, bytes calldata signature, uint32 maxDecayPpm)
        external
        pure
        returns (bytes memory)
    {
        return abi.encode(order, signature, maxDecayPpm);
    }

    // ---------------------------------------------------------------------
    // Internals
    // ---------------------------------------------------------------------

    /// @dev The whole adapter.
    ///
    ///      No exact-length check on `data`, unlike `PropPoolAdapter`: the payload carries a
    ///      variable-length signature, so its encoded length is not a constant and a bound would
    ///      be a guess. `abi.decode` still rejects a malformed blob, and a well-formed blob
    ///      carrying the wrong order fails at the signature.
    function _fill(address to, address settle, bytes calldata data) private {
        if (settle == address(0)) revert ZeroSettle();

        (IPmmSettle.Order memory order, bytes memory signature, uint32 maxDecayPpm) =
            abi.decode(data, (IPmmSettle.Order, bytes, uint32));

        // The router pushed this leg's tokens here (bit 254). Reading the balance back rather than
        // being told the size is what makes `IAdapter`'s missing amount parameter work, and it is
        // the reason a mis-set funding bit surfaces as `NothingToFill` instead of as a fill of the
        // wrong size.
        uint256 balance = IERC20Minimal(order.takerAsset).balanceOf(address(this));

        // Read live, never cached, and read *after* the tokens have arrived. Between the block the
        // plan was built in and this one, other takers may have consumed part of this same order —
        // that is the point of a streamed quote — so the remaining amount is not a number the plan
        // is allowed to assert.
        uint256 remaining = IPmmSettle(settle).remainingTaker(order);

        uint256 takerIn = balance < remaining ? balance : remaining;
        if (takerIn == 0) revert NothingToFill();

        // Exactly what the fill will consume, so the allowance is zero again when `fillOrder`
        // returns. Sizing it to `balance` instead would leave a live allowance behind on the
        // over-funded path, and sizing it to `type(uint256).max` would leave a permanent one —
        // which would make this contract the one thing `IAdapter` says an adapter is never: an
        // address with a standing approval attached to it.
        //
        // The always-zero end state is also what keeps this compatible with the USDT-shaped tokens
        // that refuse a non-zero-to-non-zero `approve`: the next call always starts from zero.
        _approve(order.takerAsset, settle, takerIn);

        IPmmSettle(settle).fillOrder(order, signature, takerIn, maxDecayPpm, to);

        // Over-funding is a normal outcome, not an error: the plan sized this leg against a
        // remaining amount that has since shrunk. The surplus goes to `to` — the router — which is
        // the only address in scope that this adapter can name. It cannot be returned to the payer
        // directly, because an adapter has no idea who the payer is, and the calldata-tail
        // `payerOrigin` convention that would tell it is on `archi_v2.md` §6.2's do-not-borrow
        // list.
        //
        // What the router then does with it is already specified: if `takerAsset` is the route's
        // input token it is swept back to the payer with the rest of the leftover, and if it is an
        // intermediate it is stranded there — the same outcome, and the same warning, as an
        // under-allocated intermediate hop in `RouteDecoder.validateWeightSum`. Plans that can
        // over-fund this leg should place it on a batch's first hop.
        uint256 leftover = balance - takerIn;
        if (leftover != 0) SafeTransfer.safeTransfer(order.takerAsset, to, leftover);
    }

    /// @dev `token.approve(spender, amount)`, tolerating the tokens that return nothing.
    ///
    ///      `SafeTransfer` has no `safeApprove` and is not getting one: it is documented as
    ///      transfer helpers on the swap hot path, and an allowance grant is neither. The check
    ///      here is the same three-part test the library applies — the call succeeded, the return
    ///      was empty or a 32-byte `true`, and the target has code — written in the high-level
    ///      form because this is one call per fill rather than two per swap, and legibility is
    ///      worth more than the memory the encoder allocates.
    ///
    ///      The `code.length` check is the one that matters: a call to an address with no code
    ///      succeeds with empty return data, so without it a mistyped `takerAsset` would approve
    ///      nothing, and the fill would then revert somewhere less obvious.
    function _approve(address token, address spender, uint256 amount) private {
        if (token.code.length == 0) revert ApproveFailed(token);
        (bool ok, bytes memory ret) = token.call(abi.encodeCall(IERC20Approve.approve, (spender, amount)));
        if (!ok || (ret.length != 0 && !abi.decode(ret, (bool)))) revert ApproveFailed(token);
    }
}
