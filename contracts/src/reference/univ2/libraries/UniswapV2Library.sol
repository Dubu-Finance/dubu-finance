// SPDX-License-Identifier: GPL-3.0-or-later
pragma solidity ^0.8.28;

import {IUniswapV2Pair} from "../interfaces/IUniswapV2Pair.sol";
import {IUniswapV2Factory} from "../interfaces/IUniswapV2Factory.sol";

/// @title UniswapV2Library
/// @dev Ported from Uniswap V2 periphery v1.1.0-beta.0 `contracts/libraries/UniswapV2Library.sol`.
///      https://github.com/Uniswap/v2-periphery
///
/// PORT NOTES
///   - pragma `>=0.5.0` -> `^0.8.28`; `SafeMath` dropped for 0.8 native checked arithmetic.
///   - `pairFor` no longer derives the pair address via CREATE2. See the doc comment on
///     `pairFor` below and README D2. This is the single most consequential deviation in the
///     port and it changes the function's mutability from `pure` to `view`.
///   - THE PRICING MATH IS UNTOUCHED. `quote`, `getAmountOut` and `getAmountIn` are
///     character-for-character upstream (modulo `.mul(` -> `*`). The 997/1000 constants encode
///     the 0.3% fee and must match `UniswapV2Pair.swap`'s k-check exactly; if they drift, the
///     router quotes a number the pair will reject, and the benchmark is measuring a bug.
library UniswapV2Library {
    // returns sorted token addresses, used to handle return values from pairs sorted in this order
    function sortTokens(address tokenA, address tokenB) internal pure returns (address token0, address token1) {
        require(tokenA != tokenB, "UniswapV2Library: IDENTICAL_ADDRESSES");
        (token0, token1) = tokenA < tokenB ? (tokenA, tokenB) : (tokenB, tokenA);
        require(token0 != address(0), "UniswapV2Library: ZERO_ADDRESS");
    }

    /// @notice Returns the pair address for two tokens.
    ///
    /// @dev DEVIATION FROM UPSTREAM (README D2).
    ///
    /// Upstream computes the address locally, with no external call:
    ///
    ///     pair = address(uint(keccak256(abi.encodePacked(
    ///         hex'ff',
    ///         factory,
    ///         keccak256(abi.encodePacked(token0, token1)),
    ///         hex'96e8ac4277198ff8b6f785478aa9a39f403cb768dd02cbee326c3e7da348845f'  // init code hash
    ///     ))));
    ///
    /// That trailing constant is `keccak256(type(UniswapV2Pair).creationCode)` for the *mainnet*
    /// build: solc 0.5.16, optimizer on, 999999 runs. We recompile the pair with solc 0.8.28,
    /// `evm_version = "prague"`, `optimizer_runs = 1_000_000`, `bytecode_hash = "none"`. The
    /// creation code is different, so the hash is different, so the derived address would be
    /// wrong — every router call would land on an address with no code and revert (or, worse,
    /// silently succeed against something unrelated). Hardcoding a freshly computed hash is
    /// also fragile: it silently rots the moment anyone touches the pair, the optimizer
    /// settings, or the EVM version.
    ///
    /// So we ask the factory instead. Cost of the change:
    ///   - one `STATICCALL` (~2600 gas cold / 100 warm) plus one `SLOAD` in the factory
    ///     (~2100 cold / 100 warm) per lookup, versus ~100 gas of hashing upstream;
    ///   - `pure` becomes `view`, which propagates to `getReserves`, `getAmountsOut` and
    ///     `getAmountsIn` (all of which were already `view`, so nothing else changes).
    ///
    /// Benchmark impact: a single-hop swap does 2 `pairFor` lookups (`swapExactTokensForTokens`
    /// -> once for the transfer target, once inside `_swap`) against the same factory slot, so
    /// only the first is cold. Budget roughly 4-5k gas of overhead on the first hop and ~0.2k
    /// on each repeat. THIS OVERHEAD IS ON THE V2 SIDE OF THE COMPARISON, i.e. it makes V2 look
    /// slightly worse on gas than a canonical deployment would. When reporting gas numbers
    /// (as opposed to slippage numbers, which are unaffected), subtract it or say so.
    ///
    /// Behavioural difference: upstream returns a deterministic address whether or not the pair
    /// exists, and the failure mode for a nonexistent pair is a revert with no data from
    /// calling `getReserves()` on an empty account. Here we get `address(0)` back and turn it
    /// into a named error, which is strictly more debuggable and cannot mask a real fill.
    function pairFor(address factory, address tokenA, address tokenB) internal view returns (address pair) {
        (address token0, address token1) = sortTokens(tokenA, tokenB);
        pair = IUniswapV2Factory(factory).getPair(token0, token1);
        require(pair != address(0), "UniswapV2Library: PAIR_NOT_FOUND");
    }

    // fetches and sorts the reserves for a pair
    function getReserves(address factory, address tokenA, address tokenB)
        internal
        view
        returns (uint256 reserveA, uint256 reserveB)
    {
        (address token0,) = sortTokens(tokenA, tokenB);
        (uint256 reserve0, uint256 reserve1,) = IUniswapV2Pair(pairFor(factory, tokenA, tokenB)).getReserves();
        (reserveA, reserveB) = tokenA == token0 ? (reserve0, reserve1) : (reserve1, reserve0);
    }

    // given some amount of an asset and pair reserves, returns an equivalent amount of the other asset
    function quote(uint256 amountA, uint256 reserveA, uint256 reserveB) internal pure returns (uint256 amountB) {
        require(amountA > 0, "UniswapV2Library: INSUFFICIENT_AMOUNT");
        require(reserveA > 0 && reserveB > 0, "UniswapV2Library: INSUFFICIENT_LIQUIDITY");
        amountB = amountA * reserveB / reserveA;
    }

    // given an input amount of an asset and pair reserves, returns the maximum output amount of the other asset
    function getAmountOut(uint256 amountIn, uint256 reserveIn, uint256 reserveOut)
        internal
        pure
        returns (uint256 amountOut)
    {
        require(amountIn > 0, "UniswapV2Library: INSUFFICIENT_INPUT_AMOUNT");
        require(reserveIn > 0 && reserveOut > 0, "UniswapV2Library: INSUFFICIENT_LIQUIDITY");
        uint256 amountInWithFee = amountIn * 997;
        uint256 numerator = amountInWithFee * reserveOut;
        uint256 denominator = reserveIn * 1000 + amountInWithFee;
        amountOut = numerator / denominator;
    }

    // given an output amount of an asset and pair reserves, returns a required input amount of the other asset
    function getAmountIn(uint256 amountOut, uint256 reserveIn, uint256 reserveOut)
        internal
        pure
        returns (uint256 amountIn)
    {
        require(amountOut > 0, "UniswapV2Library: INSUFFICIENT_OUTPUT_AMOUNT");
        require(reserveIn > 0 && reserveOut > 0, "UniswapV2Library: INSUFFICIENT_LIQUIDITY");
        uint256 numerator = reserveIn * amountOut * 1000;
        // checked == upstream: `SafeMath.sub` also reverted for `amountOut > reserveOut`. For
        // `amountOut == reserveOut` upstream produced denominator 0 and reverted on the
        // division; 0.8 does the same (Panic 0x12). Only the revert payload differs.
        uint256 denominator = (reserveOut - amountOut) * 997;
        amountIn = (numerator / denominator) + 1;
    }

    // performs chained getAmountOut calculations on any number of pairs
    function getAmountsOut(address factory, uint256 amountIn, address[] memory path)
        internal
        view
        returns (uint256[] memory amounts)
    {
        require(path.length >= 2, "UniswapV2Library: INVALID_PATH");
        amounts = new uint256[](path.length);
        amounts[0] = amountIn;
        for (uint256 i; i < path.length - 1; i++) {
            (uint256 reserveIn, uint256 reserveOut) = getReserves(factory, path[i], path[i + 1]);
            amounts[i + 1] = getAmountOut(amounts[i], reserveIn, reserveOut);
        }
    }

    // performs chained getAmountIn calculations on any number of pairs
    function getAmountsIn(address factory, uint256 amountOut, address[] memory path)
        internal
        view
        returns (uint256[] memory amounts)
    {
        require(path.length >= 2, "UniswapV2Library: INVALID_PATH");
        amounts = new uint256[](path.length);
        amounts[amounts.length - 1] = amountOut;
        for (uint256 i = path.length - 1; i > 0; i--) {
            (uint256 reserveIn, uint256 reserveOut) = getReserves(factory, path[i - 1], path[i]);
            amounts[i - 1] = getAmountIn(amounts[i], reserveIn, reserveOut);
        }
    }
}
