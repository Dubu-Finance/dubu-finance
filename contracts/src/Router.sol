// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {IAdapter, IERC20Minimal} from "./interfaces/IAdapter.sol";
import {RouteDecoder} from "./libraries/RouteDecoder.sol";
import {SafeTransfer} from "./libraries/SafeTransfer.sol";

interface IPermit2 {
    struct TokenPermissions {
        address token;
        uint256 amount;
    }

    struct PermitTransferFrom {
        TokenPermissions permitted;
        uint256 nonce;
        uint256 deadline;
    }

    struct SignatureTransferDetails {
        address to;
        uint256 requestedAmount;
    }

    function permitTransferFrom(
        PermitTransferFrom memory permit,
        SignatureTransferDetails calldata transferDetails,
        address owner,
        bytes calldata signature
    ) external;
}

struct SwapStep {

    address adapter;

    uint256 rawData;

    bytes payload;
}

struct Hop {
    address tokenIn;
    SwapStep[] steps;
}

struct Batch {

    uint16 weightBps;
    Hop[] hops;
}

struct RouteParams {
    address tokenIn;
    address tokenOut;
    address receiver;
    uint256 amountIn;

    uint256 quotedAmountOut;
    uint256 deadline;
    Batch[] batches;
}

contract Router {
    using RouteDecoder for uint256;

    error DeadlineExpired(uint256 deadline, uint256 nowTs);
    error ZeroReceiver();
    error ZeroToken();
    error IdenticalTokens();
    error ZeroAmountIn();
    error EmptyRoute();
    error EmptyBatch();
    error EmptyHop();
    error ZeroAdapter();
    error ZeroShare();
    error HopProducedNothing(uint256 batchIndex, uint256 hopIndex);
    error BatchTokenMismatch(address expected, address actual);
    error NothingReceived();
    error ZeroOutput();
    error InsufficientOutput(uint256 amountOut, uint256 required);
    error ExcessiveInput(uint256 amountIn, uint256 limit);
    error PermitTokenMismatch(address permitted, address tokenIn);
    error PermitAmountTooLow(uint256 permitted, uint256 amountIn);
    error PermitTransferredNothing();
    error TokenCallFailed(address token);
    error Reentrancy();

    event RouteExecuted(
        address indexed sender,
        address indexed tokenIn,
        address indexed tokenOut,
        address receiver,
        uint256 amountIn,
        uint256 amountOut,
        uint256 quotedAmountOut,
        bytes32 planHash
    );

    IPermit2 public constant PERMIT2 = IPermit2(0x000000000022D473030F116dDEE9F6B43aC78BA3);

    bool private transient _entered;

    modifier nonReentrant() {
        _lock();
        _;
        _unlock();
    }

    function _lock() private {
        if (_entered) revert Reentrancy();
        _entered = true;
    }

    function _unlock() private {
        _entered = false;
    }

    function swapExactIn(RouteParams calldata p, uint256 minAmountOut)
        external
        nonReentrant
        returns (uint256 amountOut)
    {
        (uint256 tokenInBefore, uint256 received) = _pull(p);
        (, amountOut) = _run(p, tokenInBefore, received, minAmountOut, type(uint256).max);
    }

    function swapExactOut(RouteParams calldata p, uint256 exactAmountOut, uint256 maxAmountIn)
        external
        nonReentrant
        returns (uint256 amountIn, uint256 amountOut)
    {

        if (p.amountIn > maxAmountIn) revert ExcessiveInput(p.amountIn, maxAmountIn);
        (uint256 tokenInBefore, uint256 received) = _pull(p);
        (amountIn, amountOut) = _run(p, tokenInBefore, received, exactAmountOut, maxAmountIn);
    }

    function swapExactInWithPermit2(
        RouteParams calldata p,
        uint256 minAmountOut,
        IPermit2.PermitTransferFrom calldata permit,
        bytes calldata signature
    ) external nonReentrant returns (uint256 amountOut) {
        (uint256 tokenInBefore, uint256 received) = _pullWithPermit2(p, permit, signature);
        (, amountOut) = _run(p, tokenInBefore, received, minAmountOut, type(uint256).max);
    }

    function swapExactOutWithPermit2(
        RouteParams calldata p,
        uint256 exactAmountOut,
        uint256 maxAmountIn,
        IPermit2.PermitTransferFrom calldata permit,
        bytes calldata signature
    ) external nonReentrant returns (uint256 amountIn, uint256 amountOut) {
        if (p.amountIn > maxAmountIn) revert ExcessiveInput(p.amountIn, maxAmountIn);
        (uint256 tokenInBefore, uint256 received) = _pullWithPermit2(p, permit, signature);
        (amountIn, amountOut) = _run(p, tokenInBefore, received, exactAmountOut, maxAmountIn);
    }

    function _pull(RouteParams calldata p) private returns (uint256 tokenInBefore, uint256 received) {
        _validate(p);
        tokenInBefore = _balanceOf(p.tokenIn);
        _safeTransferFrom(p.tokenIn, msg.sender, address(this), p.amountIn);
        received = _balanceOf(p.tokenIn) - tokenInBefore;
        if (received == 0) revert NothingReceived();
    }

    function _pullWithPermit2(
        RouteParams calldata p,
        IPermit2.PermitTransferFrom calldata permit,
        bytes calldata signature
    ) private returns (uint256 tokenInBefore, uint256 received) {
        _validate(p);
        if (permit.permitted.token != p.tokenIn) revert PermitTokenMismatch(permit.permitted.token, p.tokenIn);
        if (permit.permitted.amount < p.amountIn) revert PermitAmountTooLow(permit.permitted.amount, p.amountIn);

        tokenInBefore = _balanceOf(p.tokenIn);
        PERMIT2.permitTransferFrom(
            permit,
            IPermit2.SignatureTransferDetails({to: address(this), requestedAmount: p.amountIn}),
            msg.sender,
            signature
        );
        received = _balanceOf(p.tokenIn) - tokenInBefore;
        if (received == 0) revert PermitTransferredNothing();
    }

    function _validate(RouteParams calldata p) private view {
        if (block.timestamp > p.deadline) revert DeadlineExpired(p.deadline, block.timestamp);
        if (p.receiver == address(0)) revert ZeroReceiver();
        if (p.tokenIn == address(0) || p.tokenOut == address(0)) revert ZeroToken();

        if (p.tokenIn == p.tokenOut) revert IdenticalTokens();
        if (p.amountIn == 0) revert ZeroAmountIn();
        if (p.batches.length == 0) revert EmptyRoute();
    }

    function _run(RouteParams calldata p, uint256 tokenInBefore, uint256 received, uint256 minOut, uint256 maxIn)
        private
        returns (uint256 spent, uint256 amountOut)
    {
        (spent, amountOut) = _execute(p, tokenInBefore, received);

        if (amountOut == 0) revert ZeroOutput();
        if (amountOut < minOut) revert InsufficientOutput(amountOut, minOut);
        if (spent > maxIn) revert ExcessiveInput(spent, maxIn);

        _safeTransfer(p.tokenOut, p.receiver, amountOut);

        emit RouteExecuted(
            msg.sender, p.tokenIn, p.tokenOut, p.receiver, spent, amountOut, p.quotedAmountOut, keccak256(msg.data)
        );
    }

    function _execute(RouteParams calldata p, uint256 tokenInBefore, uint256 received)
        private
        returns (uint256 spent, uint256 amountOut)
    {
        uint256 tokenOutBefore = _balanceOf(p.tokenOut);

        Batch[] calldata batches = p.batches;
        uint256 n = batches.length;

        uint256 totalWeight;
        for (uint256 i; i < n; ++i) {
            totalWeight += batches[i].weightBps;
        }
        RouteDecoder.validateWeightSum(totalWeight);

        uint256 allocated;
        for (uint256 i; i < n; ++i) {
            uint256 share = (i + 1 == n && totalWeight == RouteDecoder.WEIGHT_DENOMINATOR)
                ? received - allocated
                : RouteDecoder.shareOf(received, batches[i].weightBps);
            if (share == 0) revert ZeroShare();
            allocated += share;
            _runBatch(batches[i], p.tokenIn, share, i);
        }

        amountOut = _balanceOf(p.tokenOut) - tokenOutBefore;

        uint256 leftover = _balanceOf(p.tokenIn) - tokenInBefore;
        if (leftover != 0) _safeTransfer(p.tokenIn, msg.sender, leftover);
        spent = received - leftover;
    }

    function _runBatch(Batch calldata b, address routeTokenIn, uint256 amountIn, uint256 batchIndex) private {
        Hop[] calldata hops = b.hops;
        uint256 n = hops.length;
        if (n == 0) revert EmptyBatch();
        if (hops[0].tokenIn != routeTokenIn) revert BatchTokenMismatch(routeTokenIn, hops[0].tokenIn);

        uint256 available = amountIn;
        for (uint256 h; h < n; ++h) {
            bool hasNext = h + 1 < n;

            address nextToken = hasNext ? hops[h + 1].tokenIn : address(0);
            uint256 nextBefore = hasNext ? _balanceOf(nextToken) : 0;

            _runHop(hops[h], available);

            if (hasNext) {
                available = _balanceOf(nextToken) - nextBefore;
                if (available == 0) revert HopProducedNothing(batchIndex, h);
            }
        }
    }

    function _runHop(Hop calldata hop, uint256 available) private {
        SwapStep[] calldata steps = hop.steps;
        uint256 n = steps.length;
        if (n == 0) revert EmptyHop();

        uint256 totalWeight;
        for (uint256 i; i < n; ++i) {
            RouteDecoder.validateStep(steps[i].rawData);
            totalWeight += steps[i].rawData.weight();
        }
        RouteDecoder.validateWeightSum(totalWeight);

        uint256 allocated;
        for (uint256 i; i < n; ++i) {
            uint256 rawData = steps[i].rawData;
            address adapter = steps[i].adapter;
            if (adapter == address(0)) revert ZeroAdapter();

            address pool = rawData.pool();

            uint256 share = (i + 1 == n && totalWeight == RouteDecoder.WEIGHT_DENOMINATOR)
                ? available - allocated
                : RouteDecoder.shareOf(available, rawData.weight());
            if (share == 0) revert ZeroShare();
            allocated += share;

            _safeTransfer(hop.tokenIn, rawData.fundsAdapter() ? adapter : pool, share);

            if (rawData.isReverse()) {
                IAdapter(adapter).sellQuote(address(this), pool, steps[i].payload);
            } else {
                IAdapter(adapter).sellBase(address(this), pool, steps[i].payload);
            }
        }
    }

    function _balanceOf(address token) private view returns (uint256) {
        return IERC20Minimal(token).balanceOf(address(this));
    }

    function _safeTransfer(address token, address to, uint256 amount) private {
        if (token.code.length == 0) revert TokenCallFailed(token);
        (bool ok, bytes memory ret) = token.call(abi.encodeCall(IERC20Minimal.transfer, (to, amount)));
        if (!ok || (ret.length != 0 && !abi.decode(ret, (bool)))) revert TokenCallFailed(token);
    }

    function _safeTransferFrom(address token, address from, address to, uint256 amount) private {
        if (token.code.length == 0) revert TokenCallFailed(token);
        (bool ok, bytes memory ret) = token.call(abi.encodeCall(IERC20Minimal.transferFrom, (from, to, amount)));
        if (!ok || (ret.length != 0 && !abi.decode(ret, (bool)))) revert TokenCallFailed(token);
    }
}
