// SPDX-License-Identifier: GPL-3.0-or-later
pragma solidity ^0.8.28;

/// @dev Ported verbatim from Uniswap V2 periphery v1.1.0-beta.0 `contracts/interfaces/IWETH.sol`.
///      https://github.com/Uniswap/v2-periphery
interface IWETH {
    function deposit() external payable;
    function transfer(address to, uint256 value) external returns (bool);
    function withdraw(uint256) external;
}
