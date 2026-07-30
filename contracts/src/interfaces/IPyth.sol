// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

contract PythStructs {

    struct Price {

        int64 price;

        uint64 conf;

        int32 expo;

        uint256 publishTime;
    }

    struct PriceFeed {

        bytes32 id;

        Price price;

        Price emaPrice;
    }
}

library PythErrors {

    error InvalidArgument();

    error InvalidUpdateData();

    error InsufficientFee();

    error PriceFeedNotFound();

    error StalePrice();
}

interface IPythEvents {

    event PriceFeedUpdate(bytes32 indexed id, uint64 publishTime, int64 price, uint64 conf);
}

interface IPyth is IPythEvents {
    function getValidTimePeriod() external view returns (uint256 validTimePeriod);
    function getPriceUnsafe(bytes32 id) external view returns (PythStructs.Price memory price);
    function getEmaPriceUnsafe(bytes32 id) external view returns (PythStructs.Price memory price);
    function getPriceNoOlderThan(bytes32 id, uint256 age) external view returns (PythStructs.Price memory price);
    function getEmaPriceNoOlderThan(bytes32 id, uint256 age) external view returns (PythStructs.Price memory price);
    function updatePriceFeeds(bytes[] calldata updateData) external payable;
    function getUpdateFee(bytes[] calldata updateData) external view returns (uint256 feeAmount);
}
