// SPDX-License-Identifier: GPL-3.0
pragma solidity ^0.8.28;

/// @dev Ported verbatim from Uniswap V2 core v1.0.1 `contracts/interfaces/IUniswapV2Callee.sol`.
///      https://github.com/Uniswap/v2-core
interface IUniswapV2Callee {
    function uniswapV2Call(address sender, uint256 amount0, uint256 amount1, bytes calldata data) external;
}
