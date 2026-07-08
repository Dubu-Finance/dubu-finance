// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

/// @title SafeTransfer
/// @notice Minimal ERC-20 transfer helpers that tolerate the three return conventions in the wild.
///
/// @dev ERC-20 says `transfer` returns `bool`. Reality:
///        1. returns nothing at all (USDT on Ethereum, BNB, and most pre-2018 tokens),
///        2. returns a 32-byte `true`,
///        3. returns a 32-byte `false` instead of reverting (a few, e.g. some Compound forks).
///      A plain `IERC20(t).transfer(...)` reverts on (1) because the ABI decoder demands 32 bytes,
///      and silently succeeds on (3) if the return value is discarded. Both are fund-loss bugs.
///
///      Written in assembly rather than with `abi.encodeCall` + `functionCall` because this sits
///      on the swap hot path twice per trade, and because the high-level form allocates memory for
///      calldata and return data that we do not need.
///
///      No OpenZeppelin/Solmate dependency on purpose: this pool ships with forge-std only, and a
///      40-line primitive is cheaper to audit than a transitive dependency tree.
///
///      What is deliberately NOT handled: fee-on-transfer and rebasing tokens. This library
///      reports success/failure, not the amount actually received. `PropPool` accounts inbound
///      amounts as the requested amount on the pull path, so such tokens must not be listed.
///
///      Two limits of the "bubble verbatim" promise, both inherent to `CALL` rather than to this
///      code, and both pinned by tests in `test/SafeTransfer.t.sol`:
///
///        - A token that reverts with no data bubbles as a revert with no data, which is
///          byte-identical to a token that consumed every unit of forwarded gas. The caller cannot
///          tell the two apart. Substituting `TransferFailed()` for the empty payload would make
///          the failure attributable but would abandon "verbatim", so it is not done here; the
///          practical hazard is not the ambiguity but the 63/64 rule, which leaves a `try/catch`
///          caller resuming on 1/64 of the gas it had.
///        - `returndatacopy` on the failure path is unbounded, so a token can make the bubble cost
///          memory-expansion gas proportional to the payload it chose to return. Capping the copy
///          is the standard mitigation and is exactly the trade against verbatim bubbling; the
///          attacker pays symmetric expansion cost to produce the payload, so it is priced, not
///          free. Revisit if this library is ever used behind an unpriced `try/catch`.
library SafeTransfer {
    /// @notice `transfer` returned false, returned garbage, or the target has no code.
    error TransferFailed();
    /// @notice `transferFrom` returned false, returned garbage, or the target has no code.
    error TransferFromFailed();

    /// @notice `token.transfer(to, amount)`, reverting unless the token clearly succeeded.
    /// @dev The `extcodesize` check matters: a `call` to an account with no code returns success
    ///      with empty return data, which is indistinguishable from convention (1). Without it,
    ///      a mistyped or self-destructed token address would make every transfer "succeed" while
    ///      moving nothing.
    function safeTransfer(address token, address to, uint256 amount) internal {
        assembly ("memory-safe") {
            // Cache the free-memory pointer BEFORE the calldata layout below reaches into its word.
            // `mstore(0x34, amount)` covers 0x34..0x53, and 0x40..0x53 is the high 20 bytes of the
            // pointer at 0x40 — so from here to the restore at the bottom, `mload(0x40)` is garbage
            // and must never be read. Reading it on the failure path is exactly what made every
            // failing transfer revert with no data and burn the frame's gas.
            let fmp := mload(0x40)

            // Build calldata in scratch space, spilling into the free-memory-pointer word.
            // Layout: [0x10] selector | [0x14] to | [0x34] amount  => 0x44 bytes from 0x10.
            // Written low-to-high, but the selector store lands last so it re-zeroes 0x14..0x1f:
            // that is what cleans any dirty high bits of `to` out of its ABI padding.
            mstore(0x14, to)
            mstore(0x34, amount)
            mstore(0x00, 0xa9059cbb000000000000000000000000) // transfer(address,uint256)

            let ok := call(gas(), token, 0, 0x10, 0x44, 0x00, 0x20)

            // Bubble the callee's revert verbatim. Integrators (and our own simulation harness)
            // rely on seeing the token's own error, not a generic one.
            if iszero(ok) {
                returndatacopy(fmp, 0x00, returndatasize())
                revert(fmp, returndatasize())
            }

            // Accept: empty return data, or >=32 bytes whose first word is exactly 1.
            // `gt(returndatasize(), 31)` is required before trusting mload(0x00): on a short
            // return the word still holds our own selector constant.
            let returnedOk := or(iszero(returndatasize()), and(gt(returndatasize(), 31), eq(mload(0x00), 1)))
            if iszero(and(returnedOk, iszero(iszero(extcodesize(token))))) {
                mstore(0x00, 0x90b8ec18) // TransferFailed()
                revert(0x1c, 0x04)
            }

            // Restore the free-memory pointer from the cached copy. The previous form
            // (`mstore(0x34, 0)`) happened to work only because a live pointer is always below
            // 2^96, so its clobbered high bytes were zero anyway; writing the real value back is
            // the same one `mstore` and carries no assumption about the pointer's magnitude.
            // 0x00..0x3f is left dirty on purpose: that is Solidity's scratch space.
            mstore(0x40, fmp)
        }
    }

    /// @notice `token.transferFrom(from, to, amount)`, reverting unless the token clearly succeeded.
    function safeTransferFrom(address token, address from, address to, uint256 amount) internal {
        assembly ("memory-safe") {
            let fmp := mload(0x40)

            // Layout: [0x1c] selector | [0x20] from | [0x40] to | [0x60] amount => 0x64 from 0x1c.
            // Written high-to-low so each store's zero padding lands on the previous field's pad.
            mstore(0x60, amount)
            mstore(0x40, to)
            mstore(0x2c, shl(96, from))
            mstore(0x0c, 0x23b872dd000000000000000000000000) // transferFrom(address,address,uint256)

            let ok := call(gas(), token, 0, 0x1c, 0x64, 0x00, 0x20)

            if iszero(ok) {
                returndatacopy(fmp, 0x00, returndatasize())
                revert(fmp, returndatasize())
            }

            let returnedOk := or(iszero(returndatasize()), and(gt(returndatasize(), 31), eq(mload(0x00), 1)))
            if iszero(and(returnedOk, iszero(iszero(extcodesize(token))))) {
                mstore(0x00, 0x7939f424) // TransferFromFailed()
                revert(0x1c, 0x04)
            }

            mstore(0x60, 0) // restore the zero slot
            mstore(0x40, fmp) // restore the free-memory pointer
        }
    }
}
