// SPDX-License-Identifier: GPL-3.0
pragma solidity ^0.8.28;

/// @title Math
/// @dev Ported verbatim from Uniswap V2 core v1.0.1 `contracts/libraries/Math.sol`.
///      https://github.com/Uniswap/v2-core
///
/// PORT NOTES
///   - pragma `=0.5.16` -> `^0.8.28`. No other change.
///   - Deliberately NOT wrapped in `unchecked`. Every expression here is provably
///     overflow-free for all `uint256` inputs, so 0.8 checked arithmetic is behaviourally
///     identical to 0.5 wrapping arithmetic:
///       * `y / 2 + 1`         : `y/2 <= 2**255 - 1`, so `+1` cannot overflow.
///       * `(y / x + x) / 2`   : on entry `x <= y/2 + 1` and `x >= 1`, so `y/x <= y` and the
///                               sum is bounded by the next iterate of a monotonically
///                               decreasing Newton sequence; it never exceeds `y/2 + 3`.
///     Keeping the checks costs a few gas but changes no result. `sqrt` is only used in
///     `mint`/`_mintFee`, never on the swap hot path we benchmark.
///
/// @notice a library for performing various math operations
library Math {
    function min(uint256 x, uint256 y) internal pure returns (uint256 z) {
        z = x < y ? x : y;
    }

    // babylonian method (https://en.wikipedia.org/wiki/Methods_of_computing_square_roots#Babylonian_method)
    function sqrt(uint256 y) internal pure returns (uint256 z) {
        if (y > 3) {
            z = y;
            uint256 x = y / 2 + 1;
            while (x < z) {
                z = x;
                x = (y / x + x) / 2;
            }
        } else if (y != 0) {
            z = 1;
        }
    }
}
