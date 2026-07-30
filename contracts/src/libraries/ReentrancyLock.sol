// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

library ReentrancyLock {

    error Reentrancy();

    uint256 internal constant LOCK_SLOT = 0x294b8a77648546bace7918cd2cca34829417a6f5e209061b996db4a2aa678071;

    function acquire() internal {
        assembly ("memory-safe") {
            if tload(LOCK_SLOT) {
                mstore(0x00, 0xab143c06)
                revert(0x1c, 0x04)
            }
            tstore(LOCK_SLOT, 1)
        }
    }

    function release() internal {
        assembly ("memory-safe") {
            tstore(LOCK_SLOT, 0)
        }
    }

    function held() internal view returns (bool locked) {
        assembly ("memory-safe") {
            locked := tload(LOCK_SLOT)
        }
    }
}
