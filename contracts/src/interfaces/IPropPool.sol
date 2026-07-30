// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

interface IPropPool {

    struct Pair {
        uint16 pairId;
        address base;
        address quote;
    }

    struct PairSnapshot {
        uint56 minBid;
        uint56 maxBid;
        uint56 minAsk;
        uint56 maxAsk;
        uint32 updatedAt;
        uint96 bidCapacity;
        uint96 askCapacity;
        uint96 bidUsed;
        uint96 askUsed;
        uint32 capGen;
        uint32 usedGen;
        uint16 flags;
        uint8 priceScaleExp;
        uint32 maxStaleSecs;
    }

    event PairAdded(uint16 indexed pairId, address indexed base, address indexed quote);
    event QuoteUpdated(uint16 indexed pairId, uint56 minBid, uint56 maxBid, uint56 minAsk, uint56 maxAsk);

    event CapacityRefreshed(uint16 indexed pairId, uint96 bidCapacity, uint96 askCapacity, uint32 capGen);
    event Swap(
        uint16 indexed pairId,
        address indexed sender,
        address indexed receiver,
        bool isBid,
        uint256 amountIn,
        uint256 amountOut,
        uint256 partnerId
    );
    event Paused(uint16 indexed pairId, bool paused);

    function getAmountOut(address tokenIn, address tokenOut, uint256 amountIn) external view returns (uint256 amountOut);

    function getAmountIn(address tokenIn, address tokenOut, uint256 amountOut) external view returns (uint256 amountIn);

    function quoteByPair(uint16 pairId, bool isBid, uint256 amountIn) external view returns (uint256 amountOut);

    function snapshot(uint16 pairId) external view returns (PairSnapshot memory);

    function effectiveCapacity(uint16 pairId)
        external
        view
        returns (uint96 bidCapacity, uint96 askCapacity, uint16 decaySecs);

    function swap(
        address tokenIn,
        address tokenOut,
        int256 specifiedAmount,
        uint256 limitAmount,
        address receiver,
        uint256 partnerId,
        uint256 deadline
    ) external returns (uint256 result);

    function swapWithContractBalance(
        address tokenIn,
        address tokenOut,
        uint256 limitAmount,
        address receiver,
        uint256 partnerId,
        uint256 deadline
    ) external returns (uint256 amountOut);

    function getSupportedPairs() external view returns (Pair[] memory);
    function pairIdFor(address tokenIn, address tokenOut) external view returns (uint16 pairId, bool isBid);
}

interface IPropSwapCallback {

    function propSwapCallback(address tokenIn, uint256 amountIn, bytes calldata data) external;
}
