// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {IAdapter, IERC20Minimal} from "../interfaces/IAdapter.sol";
import {IPmmSettle} from "../interfaces/IPmmSettle.sol";
import {SafeTransfer} from "../libraries/SafeTransfer.sol";

interface IERC20Approve {
    function approve(address spender, uint256 amount) external returns (bool);
}

contract PmmAdapter is IAdapter {

    error ZeroSettle();
    error NothingToFill();
    error ApproveFailed(address token);

    function sellBase(address to, address pool, bytes calldata data) external override {
        _fill(to, pool, data);
    }

    function sellQuote(address to, address pool, bytes calldata data) external override {
        _fill(to, pool, data);
    }

    function encodePayload(IPmmSettle.Order calldata order, bytes calldata signature, uint32 maxDecayPpm)
        external
        pure
        returns (bytes memory)
    {
        return abi.encode(order, signature, maxDecayPpm);
    }

    function _fill(address to, address settle, bytes calldata data) private {
        if (settle == address(0)) revert ZeroSettle();

        (IPmmSettle.Order memory order, bytes memory signature, uint32 maxDecayPpm) =
            abi.decode(data, (IPmmSettle.Order, bytes, uint32));

        uint256 balance = IERC20Minimal(order.takerAsset).balanceOf(address(this));

        uint256 remaining = IPmmSettle(settle).remainingTaker(order);

        uint256 takerIn = balance < remaining ? balance : remaining;
        if (takerIn == 0) revert NothingToFill();

        _approve(order.takerAsset, settle, takerIn);

        IPmmSettle(settle).fillOrder(order, signature, takerIn, maxDecayPpm, to);

        uint256 leftover = balance - takerIn;
        if (leftover != 0) SafeTransfer.safeTransfer(order.takerAsset, to, leftover);
    }

    function _approve(address token, address spender, uint256 amount) private {
        if (token.code.length == 0) revert ApproveFailed(token);
        (bool ok, bytes memory ret) = token.call(abi.encodeCall(IERC20Approve.approve, (spender, amount)));
        if (!ok || (ret.length != 0 && !abi.decode(ret, (bool)))) revert ApproveFailed(token);
    }
}
