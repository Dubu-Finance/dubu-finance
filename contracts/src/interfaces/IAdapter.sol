// SPDX-License-Identifier: MIT
// Adapter interface from OKX DEX-Router-EVM-V1. MIT, Copyright (c) 2022-2025 OKX labs.
pragma solidity ^0.8.28;

interface IAdapter {

    function sellBase(address to, address pool, bytes calldata data) external;

    function sellQuote(address to, address pool, bytes calldata data) external;
}

interface IERC20Minimal {
    function balanceOf(address account) external view returns (uint256);
    function transfer(address to, uint256 amount) external returns (bool);
    function transferFrom(address from, address to, uint256 amount) external returns (bool);
}
