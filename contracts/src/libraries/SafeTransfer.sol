// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

library SafeTransfer {

    error TransferFailed();

    error TransferFromFailed();

    function safeTransfer(address token, address to, uint256 amount) internal {
        assembly ("memory-safe") {

            let fmp := mload(0x40)

            mstore(0x14, to)
            mstore(0x34, amount)
            mstore(0x00, 0xa9059cbb000000000000000000000000)

            let ok := call(gas(), token, 0, 0x10, 0x44, 0x00, 0x20)

            if iszero(ok) {
                returndatacopy(fmp, 0x00, returndatasize())
                revert(fmp, returndatasize())
            }

            let returnedOk := or(iszero(returndatasize()), and(gt(returndatasize(), 31), eq(mload(0x00), 1)))
            if iszero(and(returnedOk, iszero(iszero(extcodesize(token))))) {
                mstore(0x00, 0x90b8ec18)
                revert(0x1c, 0x04)
            }

            mstore(0x40, fmp)
        }
    }

    function safeTransferFrom(address token, address from, address to, uint256 amount) internal {
        assembly ("memory-safe") {
            let fmp := mload(0x40)

            mstore(0x60, amount)
            mstore(0x40, to)
            mstore(0x2c, shl(96, from))
            mstore(0x0c, 0x23b872dd000000000000000000000000)

            let ok := call(gas(), token, 0, 0x1c, 0x64, 0x00, 0x20)

            if iszero(ok) {
                returndatacopy(fmp, 0x00, returndatasize())
                revert(fmp, returndatasize())
            }

            let returnedOk := or(iszero(returndatasize()), and(gt(returndatasize(), 31), eq(mload(0x00), 1)))
            if iszero(and(returnedOk, iszero(iszero(extcodesize(token))))) {
                mstore(0x00, 0x7939f424)
                revert(0x1c, 0x04)
            }

            mstore(0x60, 0)
            mstore(0x40, fmp)
        }
    }
}
