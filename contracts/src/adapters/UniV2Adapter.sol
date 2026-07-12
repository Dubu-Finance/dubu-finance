// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {IAdapter, IERC20Minimal} from "../interfaces/IAdapter.sol";

/// @notice The four functions of a Uniswap V2 pair this adapter touches.
/// @dev Declared locally and deliberately NOT imported from `src/reference/univ2/`. That tree is
///      a deployment artefact owned by another workstream; binding an adapter to it would couple
///      this contract's compilation to a fork's source layout for no benefit. A local interface is
///      also the honest description of the trust boundary: the adapter assumes only that the
///      address behaves like a V2 pair, and nothing about how it was built.
interface IUniswapV2Pair {
    function getReserves() external view returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast);
    function swap(uint256 amount0Out, uint256 amount1Out, address to, bytes calldata data) external;
    function token0() external view returns (address);
    function token1() external view returns (address);
}

/// @title UniV2Adapter
/// @notice Routes a hop into a constant-product Uniswap V2 pair.
///
/// V2's `swap` does no pricing: it verifies that the k-invariant holds *after* the caller's tokens
/// have arrived and the requested output has left. The caller therefore has to compute the output
/// itself, and it must compute it the way the pair will check it — same fee, same rounding, same
/// order of operations. Off-by-one here does not produce a slightly worse fill, it produces a
/// reverted transaction (too much out) or a donation to the LPs (too little out).
///
/// Stateless, unowned, immutable. Holds nothing, caches nothing, has no storage variables.
///
/// @dev Deployed once per fee tier, not once per pair. The 0.3% fee is compiled in rather than
///      read from calldata: a fee supplied by the caller is a number the pair will not agree with
///      unless it is correct, so the only thing a fee parameter can do is let a mis-encoded plan
///      under-quote itself and donate the difference. A V2 fork with a different fee gets its own
///      deployment of this contract with the constants changed — adapters are free.
contract UniV2Adapter is IAdapter {
    // ---------------------------------------------------------------------
    // Errors
    // ---------------------------------------------------------------------

    error ZeroPool();
    error InsufficientInputAmount();
    error InsufficientLiquidity();

    // ---------------------------------------------------------------------
    // Constants
    // ---------------------------------------------------------------------

    /// @notice 0.3% fee, expressed as V2 expresses it: 997/1000 of the input reaches the curve.
    uint256 internal constant FEE_NUMERATOR = 997;
    uint256 internal constant FEE_DENOMINATOR = 1000;

    // ---------------------------------------------------------------------
    // IAdapter
    // ---------------------------------------------------------------------

    /// @notice Sell `token0` for `token1`.
    /// @param to   recipient of `token1`. Always the router.
    /// @param pool the V2 pair.
    /// @dev `data` is unused. Everything needed — the fee, the reserves, the token ordering — is
    ///      either compiled in or readable from the pair, so putting any of it in calldata would
    ///      cost L1 DA bytes to duplicate information the chain already has.
    function sellBase(address to, address pool, bytes calldata) external override {
        if (pool == address(0)) revert ZeroPool();
        (uint256 reserve0, uint256 reserve1) = _reserves(pool);

        // The router pushed this leg's tokens to the pair before calling. The pair's stored
        // reserve is the balance as of the last `_update`, so the surplus is exactly our input.
        // Reading it back rather than being told it is what makes the amount parameter on
        // `IAdapter` unnecessary — and it is also strictly more correct, because it accounts for
        // a fee-on-transfer token without knowing that it is one.
        uint256 amountIn = IERC20Minimal(IUniswapV2Pair(pool).token0()).balanceOf(pool) - reserve0;
        uint256 amountOut = getAmountOut(amountIn, reserve0, reserve1);

        IUniswapV2Pair(pool).swap(0, amountOut, to, new bytes(0));
    }

    /// @notice Sell `token1` for `token0` — the mirror of `sellBase`.
    function sellQuote(address to, address pool, bytes calldata) external override {
        if (pool == address(0)) revert ZeroPool();
        (uint256 reserve0, uint256 reserve1) = _reserves(pool);

        uint256 amountIn = IERC20Minimal(IUniswapV2Pair(pool).token1()).balanceOf(pool) - reserve1;
        uint256 amountOut = getAmountOut(amountIn, reserve1, reserve0);

        IUniswapV2Pair(pool).swap(amountOut, 0, to, new bytes(0));
    }

    // ---------------------------------------------------------------------
    // Pricing
    // ---------------------------------------------------------------------

    /// @notice Constant-product output, bit-for-bit as `UniswapV2Library.getAmountOut` computes it.
    ///
    /// ```text
    ///   amountInWithFee = amountIn * 997
    ///   amountOut       = (amountInWithFee * reserveOut) / (reserveIn * 1000 + amountInWithFee)
    /// ```
    ///
    /// @dev The single floor division at the end is load-bearing. Fee and curve are folded into
    ///      one fraction and truncated once, so the residual always lands on the pool's side of
    ///      the k-invariant. Simplifying to `amountIn * 997 / 1000` first, or computing the fee
    ///      as a separate rounded subtraction, changes the last unit and can push `amountOut` one
    ///      wei above what `swap` will accept — the pair reverts on `K` and the whole route dies.
    ///
    ///      Overflow is not a concern under checked arithmetic: reserves are `uint112` and V2's
    ///      `_update` reverts before either can exceed that, so the widest intermediate
    ///      (`amountIn * 997 * reserveOut`) needs at most ~234 bits for any input the pair could
    ///      have accepted.
    ///
    ///      Public and pure so the off-chain path finder can price against the exact function the
    ///      chain will execute rather than a re-implementation of it.
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

    // ---------------------------------------------------------------------
    // Internals
    // ---------------------------------------------------------------------

    /// @dev The timestamp field is discarded; this adapter has no use for the TWAP accumulator.
    function _reserves(address pool) private view returns (uint256 reserve0, uint256 reserve1) {
        (uint112 r0, uint112 r1,) = IUniswapV2Pair(pool).getReserves();
        return (uint256(r0), uint256(r1));
    }
}
