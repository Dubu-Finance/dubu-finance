// SPDX-License-Identifier: MIT
// Forked from okxlabs/Web3-DEX-EVM-PMM PmmProtocol. MIT, Copyright (c) 2022-2025 OKX labs.
pragma solidity ^0.8.28;

interface IPmmSettle {

    struct Order {
        address maker;
        address makerAsset;
        address takerAsset;
        uint256 makerAmount;
        uint256 takerAmount;
        uint64 nonce;
        uint64 expiry;
        uint64 decayStart;
        uint32 decayPerSec;
        uint32 decayCap;
        uint16 minFillBps;
    }

    event OrderFilled(
        bytes32 indexed orderHash,
        address indexed maker,
        address indexed receiver,
        address makerAsset,
        address takerAsset,
        uint256 takerAmountIn,
        uint256 makerAmountQuoted,
        uint256 makerAmountFilled,
        uint256 decayPpm,
        uint256 takerRemaining
    );

    event NoncesCancelled(address indexed maker, uint256 indexed slot, uint256 mask);

    function fillOrder(
        Order calldata order,
        bytes calldata signature,
        uint256 takerAmountIn,
        uint32 maxDecayPpm,
        address receiver
    ) external returns (uint256 makerAmountOut);

    function cancelNonce(uint64 nonce) external;

    function cancelNonceSlot(uint256 slot, uint256 mask) external;

    function hashOrder(Order calldata order) external view returns (bytes32);

    function filledTaker(bytes32 orderHash) external view returns (uint256);

    function remainingTaker(Order calldata order) external view returns (uint256);

    function isNonceCancelled(address maker, uint64 nonce) external view returns (bool);

    function decayPpmAt(Order calldata order, uint256 atTimestamp) external pure returns (uint256);

    function previewFill(Order calldata order, uint256 takerAmountIn, uint256 atTimestamp)
        external
        pure
        returns (uint256 quotedMakerOut, uint256 realisedMakerOut, uint256 decayPpm);

    function DOMAIN_SEPARATOR() external view returns (bytes32);
}
