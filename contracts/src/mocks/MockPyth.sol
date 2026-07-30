// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {IPyth, PythErrors, PythStructs} from "../interfaces/IPyth.sol";

contract MockPyth is IPyth {

    uint256 public immutable validTimePeriod;

    uint256 public immutable singleUpdateFeeInWei;

    mapping(bytes32 => PythStructs.PriceFeed) private _priceFeeds;

    constructor(uint256 validTimePeriod_, uint256 singleUpdateFeeInWei_) {
        validTimePeriod = validTimePeriod_;
        singleUpdateFeeInWei = singleUpdateFeeInWei_;
    }

    function getValidTimePeriod() external view returns (uint256) {
        return validTimePeriod;
    }

    function getPriceUnsafe(bytes32 id) public view returns (PythStructs.Price memory price) {
        return _queryPriceFeed(id).price;
    }

    function getEmaPriceUnsafe(bytes32 id) public view returns (PythStructs.Price memory price) {
        return _queryPriceFeed(id).emaPrice;
    }

    function getPriceNoOlderThan(bytes32 id, uint256 age) public view returns (PythStructs.Price memory price) {
        price = getPriceUnsafe(id);
        if (_absDiff(block.timestamp, price.publishTime) > age) revert PythErrors.StalePrice();
    }

    function getEmaPriceNoOlderThan(bytes32 id, uint256 age) public view returns (PythStructs.Price memory price) {
        price = getEmaPriceUnsafe(id);
        if (_absDiff(block.timestamp, price.publishTime) > age) revert PythErrors.StalePrice();
    }

    function getUpdateFee(bytes[] calldata updateData) public view returns (uint256 feeAmount) {
        return singleUpdateFeeInWei * updateData.length;
    }

    function updatePriceFeeds(bytes[] calldata updateData) external payable {
        if (msg.value < getUpdateFee(updateData)) revert PythErrors.InsufficientFee();

        for (uint256 i = 0; i < updateData.length; ++i) {
            PythStructs.PriceFeed memory feed = abi.decode(updateData[i], (PythStructs.PriceFeed));
            if (feed.id == bytes32(0)) revert PythErrors.InvalidUpdateData();

            if (_priceFeeds[feed.id].price.publishTime < feed.price.publishTime) {
                _priceFeeds[feed.id] = feed;
                emit PriceFeedUpdate(feed.id, uint64(feed.price.publishTime), feed.price.price, feed.price.conf);
            }
        }
    }

    function setPrice(bytes32 id, int64 price, uint64 conf, int32 expo, uint256 publishTime) public {

        if (id == bytes32(0)) revert PythErrors.InvalidArgument();

        PythStructs.Price memory p = PythStructs.Price({price: price, conf: conf, expo: expo, publishTime: publishTime});

        _priceFeeds[id] = PythStructs.PriceFeed({id: id, price: p, emaPrice: p});

        emit PriceFeedUpdate(id, uint64(publishTime), price, conf);
    }

    function setPriceNow(bytes32 id, int64 price, uint64 conf, int32 expo) external {
        setPrice(id, price, conf, expo, block.timestamp);
    }

    function setEmaPrice(bytes32 id, int64 price, uint64 conf, int32 expo, uint256 publishTime) external {

        _queryPriceFeed(id);
        _priceFeeds[id].emaPrice = PythStructs.Price({price: price, conf: conf, expo: expo, publishTime: publishTime});
    }

    function createPriceFeedUpdateData(
        bytes32 id,
        int64 price,
        uint64 conf,
        int32 expo,
        int64 emaPrice,
        uint64 emaConf,
        uint64 publishTime
    ) external pure returns (bytes memory updateData) {
        return abi.encode(
            PythStructs.PriceFeed({
                id: id,
                price: PythStructs.Price({price: price, conf: conf, expo: expo, publishTime: publishTime}),
                emaPrice: PythStructs.Price({price: emaPrice, conf: emaConf, expo: expo, publishTime: publishTime})
            })
        );
    }

    function _queryPriceFeed(bytes32 id) internal view returns (PythStructs.PriceFeed memory feed) {
        feed = _priceFeeds[id];

        if (feed.id == bytes32(0)) revert PythErrors.PriceFeedNotFound();
    }

    function _absDiff(uint256 x, uint256 y) internal pure returns (uint256) {
        return x > y ? x - y : y - x;
    }
}
