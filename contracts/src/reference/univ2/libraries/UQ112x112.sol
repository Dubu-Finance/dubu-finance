// SPDX-License-Identifier: GPL-3.0
pragma solidity ^0.8.28;

/// @title UQ112x112
/// @dev Ported from Uniswap V2 core v1.0.1 `contracts/libraries/UQ112x112.sol`.
///      https://github.com/Uniswap/v2-core
///
/// @notice a library for handling binary fixed point numbers
///         (https://en.wikipedia.org/wiki/Q_(number_format))
///
/// range: [0, 2**112 - 1]
/// resolution: 1 / 2**112
///
/// PORT NOTES
///   - pragma `=0.5.16` -> `^0.8.28`.
///   - `encode` is wrapped in `unchecked` to preserve upstream's wrapping semantics exactly.
///     It is in fact provably overflow-free — the maximum input is `2**112 - 1`, and
///     `(2**112 - 1) * 2**112 = 2**224 - 2**112 < 2**224` — which is what upstream's
///     "never overflows" comment asserts. `unchecked` therefore changes no result; it only
///     removes a check the compiler cannot elide on its own, keeping the produced code as
///     close to the 0.5.16 original as possible. `encode` is on the swap hot path (it runs
///     on the first swap of every block via `_update`), so the saved check also keeps the
///     gas comparison against the prop AMM honest.
///   - `uqdiv` needs no `unchecked`: integer division cannot overflow, and division by zero
///     reverts in both 0.5 and 0.8 (0.5: invalid opcode / 0.8: Panic(0x12)). `_update` only
///     calls it when both reserves are non-zero, so the zero case is unreachable.
library UQ112x112 {
    uint224 constant Q112 = 2 ** 112;

    // encode a uint112 as a UQ112x112
    function encode(uint112 y) internal pure returns (uint224 z) {
        unchecked {
            z = uint224(y) * Q112; // never overflows
        }
    }

    // divide a UQ112x112 by a uint112, returning a UQ112x112
    function uqdiv(uint224 x, uint112 y) internal pure returns (uint224 z) {
        z = x / uint224(y);
    }
}
