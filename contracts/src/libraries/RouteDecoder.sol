// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

library RouteDecoder {

    error PoolAddressZero();
    error ReservedBitsSet(uint256 rawData);
    error ZeroWeight();
    error WeightOutOfRange(uint256 weight);
    error WeightSumExceeded(uint256 totalWeight);

    uint256 internal constant WEIGHT_DENOMINATOR = 10_000;

    uint256 private constant _REVERSE_FLAG = 1 << 255;
    uint256 private constant _FUND_ADAPTER_FLAG = 1 << 254;
    uint256 private constant _WEIGHT_OFFSET = 160;
    uint256 private constant _WEIGHT_MASK = 0xffff;
    uint256 private constant _POOL_MASK = (1 << 160) - 1;

    uint256 private constant _RESERVED_MASK = ((1 << 78) - 1) << 176;

    function pool(uint256 rawData) internal pure returns (address) {

        return address(uint160(rawData & _POOL_MASK));
    }

    function weight(uint256 rawData) internal pure returns (uint256) {
        return (rawData >> _WEIGHT_OFFSET) & _WEIGHT_MASK;
    }

    function isReverse(uint256 rawData) internal pure returns (bool) {
        return rawData & _REVERSE_FLAG != 0;
    }

    function fundsAdapter(uint256 rawData) internal pure returns (bool) {
        return rawData & _FUND_ADAPTER_FLAG != 0;
    }

    function encode(address venue, uint256 weightBps, bool reverse, bool fundAdapter)
        internal
        pure
        returns (uint256 rawData)
    {
        if (venue == address(0)) revert PoolAddressZero();
        if (weightBps == 0) revert ZeroWeight();
        if (weightBps > WEIGHT_DENOMINATOR) revert WeightOutOfRange(weightBps);

        rawData = uint256(uint160(venue)) | (weightBps << _WEIGHT_OFFSET);
        if (reverse) rawData |= _REVERSE_FLAG;
        if (fundAdapter) rawData |= _FUND_ADAPTER_FLAG;
    }

    function validateStep(uint256 rawData) internal pure {
        if (rawData & _RESERVED_MASK != 0) revert ReservedBitsSet(rawData);
        if (rawData & _POOL_MASK == 0) revert PoolAddressZero();

        uint256 w = (rawData >> _WEIGHT_OFFSET) & _WEIGHT_MASK;
        if (w == 0) revert ZeroWeight();

        if (w > WEIGHT_DENOMINATOR) revert WeightOutOfRange(w);
    }

    function validateWeightSum(uint256 totalWeight) internal pure {
        if (totalWeight > WEIGHT_DENOMINATOR) revert WeightSumExceeded(totalWeight);
    }

    function shareOf(uint256 available, uint256 weightBps) internal pure returns (uint256) {
        return (available * weightBps) / WEIGHT_DENOMINATOR;
    }
}
