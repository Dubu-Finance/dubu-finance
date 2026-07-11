// SPDX-License-Identifier: GPL-3.0
pragma solidity ^0.8.28;

/// @dev Ported from Uniswap V2 core v1.0.1 `contracts/interfaces/IUniswapV2Pair.sol`.
///      https://github.com/Uniswap/v2-core
///
/// PORT NOTE (see README "Deviations", item D3)
///   Upstream this is a standalone interface that *re-declares* the whole ERC-20 + permit
///   surface rather than inheriting `IUniswapV2ERC20`. Under Solidity 0.5.x that was fine,
///   because 0.5 had no C3 override checking. Under 0.8, `UniswapV2Pair is IUniswapV2Pair,
///   UniswapV2ERC20` would then inherit e.g. `name()` from two bases with no common ancestor,
///   which 0.8 rejects with "Derived contract must override function" — and the implementation
///   lives in `UniswapV2ERC20`, so there is nothing in `UniswapV2Pair` to attach an
///   `override(...)` list to.
///
///   Fix: make the inheritance explicit (`is IUniswapV2ERC20`) so the duplicated declarations
///   resolve through a common base. The declared members are byte-for-byte the same set as
///   upstream, so the ABI, the selectors and the interface id are all unchanged. This is a
///   type-system change only.
import {IUniswapV2ERC20} from "./IUniswapV2ERC20.sol";

interface IUniswapV2Pair is IUniswapV2ERC20 {
    // ---- inherited from IUniswapV2ERC20, listed here as upstream does -------------------
    //   event Approval(address indexed owner, address indexed spender, uint value);
    //   event Transfer(address indexed from, address indexed to, uint value);
    //   name, symbol, decimals, totalSupply, balanceOf, allowance
    //   approve, transfer, transferFrom
    //   DOMAIN_SEPARATOR, PERMIT_TYPEHASH, nonces, permit
    // -------------------------------------------------------------------------------------

    event Mint(address indexed sender, uint256 amount0, uint256 amount1);
    event Burn(address indexed sender, uint256 amount0, uint256 amount1, address indexed to);
    event Swap(
        address indexed sender,
        uint256 amount0In,
        uint256 amount1In,
        uint256 amount0Out,
        uint256 amount1Out,
        address indexed to
    );
    event Sync(uint112 reserve0, uint112 reserve1);

    function MINIMUM_LIQUIDITY() external pure returns (uint256);
    function factory() external view returns (address);
    function token0() external view returns (address);
    function token1() external view returns (address);
    function getReserves() external view returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast);
    function price0CumulativeLast() external view returns (uint256);
    function price1CumulativeLast() external view returns (uint256);
    function kLast() external view returns (uint256);

    function mint(address to) external returns (uint256 liquidity);
    function burn(address to) external returns (uint256 amount0, uint256 amount1);
    function swap(uint256 amount0Out, uint256 amount1Out, address to, bytes calldata data) external;
    function skim(address to) external;
    function sync() external;

    function initialize(address, address) external;
}
