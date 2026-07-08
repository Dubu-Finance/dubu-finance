// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

/// @title ReentrancyLock
/// @notice Transient-storage (EIP-1153) reentrancy guard for the swap hot path.
///
/// @dev Why transient and not the classic storage flag:
///      the storage pattern pays a cold SLOAD (2100) plus two SSTOREs (~2900 + ~100) on every
///      guarded call, roughly 2.9k gas that buys nothing but mutual exclusion. TSTORE/TLOAD are
///      100 gas each, so the same guarantee costs ~300 gas. On a pool whose whole reason to exist
///      is being the cheapest venue on the chain, 2.6k gas per swap is not a rounding error.
///      `foundry.toml` pins `evm_version = "prague"`, and GIWA (OP Stack, chain 91342) reports
///      prague, so EIP-1153 is available.
///
///      Transient storage is cleared at the end of the *transaction*, not the call frame, so the
///      lock MUST be released explicitly on every non-reverting exit. A revert unwinds the
///      transient write along with everything else, so the only failure mode to avoid is an early
///      `return` that skips `release`.
///
///      The slot is a fixed namespaced constant rather than a compiler-assigned one: transient
///      slot assignment is not part of the ABI, and a contract that inherits or delegatecalls into
///      several guarded components must be able to reason about collisions by inspection.
library ReentrancyLock {
    /// @notice Raised when a guarded function is re-entered.
    error Reentrancy();

    /// @dev keccak256("dubu.PropPool.reentrancyLock.v1").
    ///      Not truncated or `& ~0xff`-aligned the way ERC-7201 storage roots are: nothing is
    ///      indexed off this slot, so there is no adjacent range to reserve.
    uint256 internal constant LOCK_SLOT = 0x294b8a77648546bace7918cd2cca34829417a6f5e209061b996db4a2aa678071;

    /// @notice Take the lock, reverting if it is already held.
    function acquire() internal {
        assembly ("memory-safe") {
            if tload(LOCK_SLOT) {
                mstore(0x00, 0xab143c06) // Reentrancy()
                revert(0x1c, 0x04)
            }
            tstore(LOCK_SLOT, 1)
        }
    }

    /// @notice Release the lock. Must be reached on every successful exit of a guarded function.
    function release() internal {
        assembly ("memory-safe") {
            tstore(LOCK_SLOT, 0)
        }
    }

    /// @notice Whether the lock is currently held.
    /// @dev Exposed so view functions can, if they ever need to, tell that they are being called
    ///      from inside a swap's external-call window and that the `used` counters they are about
    ///      to read have not been written yet.
    function held() internal view returns (bool locked) {
        assembly ("memory-safe") {
            locked := tload(LOCK_SLOT)
        }
    }
}
