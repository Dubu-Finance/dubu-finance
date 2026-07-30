// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {IAdapter, IERC20Minimal} from "../interfaces/IAdapter.sol";

interface IUniswapV2Pair {
    function getReserves() external view returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast);
    function swap(uint256 amount0Out, uint256 amount1Out, address to, bytes calldata data) external;
    function token0() external view returns (address);
    function token1() external view returns (address);
}

contract UniV2Adapter is IAdapter {

    error ZeroPool();
    error InsufficientInputAmount();
    error InsufficientLiquidity();

    uint256 internal constant FEE_NUMERATOR = 997;
    uint256 internal constant FEE_DENOMINATOR = 1000;

    function sellBase(address to, address pool, bytes calldata) external override {
        if (pool == address(0)) revert ZeroPool();
        (uint256 reserve0, uint256 reserve1) = _reserves(pool);

        uint256 amountIn = IERC20Minimal(IUniswapV2Pair(pool).token0()).balanceOf(pool) - reserve0;
        uint256 amountOut = getAmountOut(amountIn, reserve0, reserve1);

        IUniswapV2Pair(pool).swap(0, amountOut, to, new bytes(0));
    }

    function sellQuote(address to, address pool, bytes calldata) external override {
        if (pool == address(0)) revert ZeroPool();
        (uint256 reserve0, uint256 reserve1) = _reserves(pool);

        uint256 amountIn = IERC20Minimal(IUniswapV2Pair(pool).token1()).balanceOf(pool) - reserve1;
        uint256 amountOut = getAmountOut(amountIn, reserve1, reserve0);

        IUniswapV2Pair(pool).swap(amountOut, 0, to, new bytes(0));
    }

    function getAmountOut(uint256 amountIn, uint256 reserveIn, uint256 reserveOut)
        public
        pure
        returns (uint256 amountOut)
    {
        if (amountIn == 0) revert InsufficientInputAmount();
        if (reserveIn == 0 || reserveOut == 0) revert InsufficientLiquidity();

        uint256 amountInWithFee = amountIn * FEE_NUMERATOR;
        uint256 numerator = amountInWithFee * reserveOut;
        uint256 denominator = reserveIn * FEE_DENOMINATOR + amountInWithFee;
        amountOut = numerator / denominator;
    }

    function _reserves(address pool) private view returns (uint256 reserve0, uint256 reserve1) {
        (uint112 r0, uint112 r1,) = IUniswapV2Pair(pool).getReserves();
        return (uint256(r0), uint256(r1));
    }
}
