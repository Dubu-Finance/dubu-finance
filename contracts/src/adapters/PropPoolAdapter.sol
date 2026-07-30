// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {IAdapter} from "../interfaces/IAdapter.sol";
import {IPropPool} from "../interfaces/IPropPool.sol";

contract PropPoolAdapter is IAdapter {

    error InvalidPayload(uint256 length);
    error ZeroPool();
    error ZeroToken();
    error IdenticalTokens();

    uint256 internal constant PAYLOAD_LENGTH = 160;

    function sellBase(address to, address pool, bytes calldata data) external override {
        (address base, address quote, uint256 limitAmount, uint256 partnerId, uint256 deadline) = _decode(data);
        _swap(pool, base, quote, limitAmount, to, partnerId, deadline);
    }

    function sellQuote(address to, address pool, bytes calldata data) external override {
        (address base, address quote, uint256 limitAmount, uint256 partnerId, uint256 deadline) = _decode(data);
        _swap(pool, quote, base, limitAmount, to, partnerId, deadline);
    }

    function encodePayload(address base, address quote, uint256 limitAmount, uint256 partnerId, uint256 deadline)
        external
        pure
        returns (bytes memory)
    {
        return abi.encode(base, quote, limitAmount, partnerId, deadline);
    }

    function _decode(bytes calldata data)
        private
        pure
        returns (address base, address quote, uint256 limitAmount, uint256 partnerId, uint256 deadline)
    {

        if (data.length != PAYLOAD_LENGTH) revert InvalidPayload(data.length);
        (base, quote, limitAmount, partnerId, deadline) =
            abi.decode(data, (address, address, uint256, uint256, uint256));
    }

    function _swap(
        address pool,
        address tokenIn,
        address tokenOut,
        uint256 limitAmount,
        address to,
        uint256 partnerId,
        uint256 deadline
    ) private {
        if (pool == address(0)) revert ZeroPool();
        if (tokenIn == address(0) || tokenOut == address(0)) revert ZeroToken();
        if (tokenIn == tokenOut) revert IdenticalTokens();

        IPropPool(pool).swapWithContractBalance(tokenIn, tokenOut, limitAmount, to, partnerId, deadline);
    }
}
