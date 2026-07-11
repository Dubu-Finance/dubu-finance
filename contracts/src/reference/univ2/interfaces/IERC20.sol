// SPDX-License-Identifier: GPL-3.0
pragma solidity ^0.8.28;

/// @dev Ported verbatim from Uniswap V2 core v1.0.1 `contracts/interfaces/IERC20.sol`.
///      https://github.com/Uniswap/v2-core
///
/// PORT NOTES
///   - pragma `>=0.5.0` -> `^0.8.28`
///   - `uint` spelled out as `uint256` (foundry.toml sets `int_types = "long"` for `forge fmt`);
///     this is a lexical change only, `uint` and `uint256` are the same type.
interface IERC20 {
    event Approval(address indexed owner, address indexed spender, uint256 value);
    event Transfer(address indexed from, address indexed to, uint256 value);

    function name() external view returns (string memory);
    function symbol() external view returns (string memory);
    function decimals() external view returns (uint8);
    function totalSupply() external view returns (uint256);
    function balanceOf(address owner) external view returns (uint256);
    function allowance(address owner, address spender) external view returns (uint256);

    function approve(address spender, uint256 value) external returns (bool);
    function transfer(address to, uint256 value) external returns (bool);
    function transferFrom(address from, address to, uint256 value) external returns (bool);
}
