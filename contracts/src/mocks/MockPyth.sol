// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

// The canonical Pyth surface — `PythStructs`, `PythErrors`, `IPythEvents`, `IPyth` — used to be
// declared inline in this file, with a note to lift it into `src/interfaces/IPyth.sol` once
// `PropPool` needed the reference bound. It does now, so it was lifted. Both the pool and this
// mock import the one declaration; see that file for the fidelity notes.
import {IPyth, PythErrors, PythStructs} from "../interfaces/IPyth.sol";

// ---------------------------------------------------------------------------------------------
// MockPyth
// ---------------------------------------------------------------------------------------------

/// @title MockPyth
/// @notice Local stand-in for the Pyth pull oracle, with a direct setter.
///
/// DuBu uses Pyth as a *bound*, not as a price source. Fair value comes from the off-chain
/// engine's CEX feeds; the oracle's only job on chain is to refuse a quote that has drifted
/// implausibly far from an independently attested reference, which is the backstop against a
/// compromised updater key. That means the tests that matter are adversarial ones — reference
/// price far above the ladder, far below it, stale by one second past the tolerance, published
/// in the future — and none of them can be driven by waiting for the real oracle to cooperate.
/// Hence a mock whose price is a storage slot a test can write.
///
/// The mock's fidelity is load-bearing in three specific places, each of which has produced a
/// real bug in somebody's integration:
///
///   1. `getPriceUnsafe` **reverts** for a feed that has never been populated. It does not
///      return a zero price. A guard written against a mock that returned zero would treat an
///      unconfigured feed as "reference price is 0" and bound every quote against it.
///   2. `getPriceNoOlderThan` compares the **absolute** difference against `age`, so a price
///      published in the future is stale in exactly the same way as one published in the past.
///   3. `updatePriceFeeds` silently ignores an update that is not strictly newer than the stored
///      one. A successful transaction therefore does not imply the price moved.
///
/// @dev ⚠️ TESTNET AND LOCAL TESTING ONLY. ⚠️ `setPrice` is unauthenticated: whoever holds the
///      ability to send a transaction holds the ability to set the reference price to anything,
///      which defeats the entire purpose of the bound. On any chain that matters, point the pool
///      at 0x2880aB155794e7179c9eE2e38200202908C17B43 instead.
contract MockPyth is IPyth {
    // ---------------------------------------------------------------------
    // State
    // ---------------------------------------------------------------------

    /// @notice Default freshness window advertised to callers that do not specify their own.
    /// @dev Real Pyth deployments configure this at 60 seconds. It is only advisory — the pool
    ///      should pass its own `age` to `getPriceNoOlderThan` rather than trust this, since the
    ///      tolerable staleness of a reference bound is a property of our risk model, not of the
    ///      oracle's configuration.
    // forge-lint: disable-next-line(screaming-snake-case-immutable)
    uint256 public immutable validTimePeriod;

    /// @notice Wei charged per entry in `updateData`. Deploy with 0 unless a test is
    ///         specifically exercising fee handling.
    /// @dev There is no withdrawal path, matching the real contract, where fees accrue to Pyth
    ///      governance rather than to the caller. Any ETH sent here with a non-zero fee is gone.
    // forge-lint: disable-next-line(screaming-snake-case-immutable)
    uint256 public immutable singleUpdateFeeInWei;

    mapping(bytes32 => PythStructs.PriceFeed) private _priceFeeds;

    constructor(uint256 validTimePeriod_, uint256 singleUpdateFeeInWei_) {
        validTimePeriod = validTimePeriod_;
        singleUpdateFeeInWei = singleUpdateFeeInWei_;
    }

    // ---------------------------------------------------------------------
    // IPyth — reads
    // ---------------------------------------------------------------------

    function getValidTimePeriod() external view returns (uint256) {
        return validTimePeriod;
    }

    /// @notice Latest price for `id` with no freshness check at all.
    /// @dev "Unsafe" is the real contract's word for it and it is accurate: the caller is
    ///      responsible for bounding `publishTime` itself. The pool uses this rather than
    ///      `getPriceNoOlderThan` only where it wants to report staleness with its own error.
    function getPriceUnsafe(bytes32 id) public view returns (PythStructs.Price memory price) {
        return _queryPriceFeed(id).price;
    }

    function getEmaPriceUnsafe(bytes32 id) public view returns (PythStructs.Price memory price) {
        return _queryPriceFeed(id).emaPrice;
    }

    /// @notice Latest price for `id`, reverting `StalePrice` if it is more than `age` seconds
    ///         away from now.
    function getPriceNoOlderThan(bytes32 id, uint256 age) public view returns (PythStructs.Price memory price) {
        price = getPriceUnsafe(id);
        if (_absDiff(block.timestamp, price.publishTime) > age) revert PythErrors.StalePrice();
    }

    function getEmaPriceNoOlderThan(bytes32 id, uint256 age) public view returns (PythStructs.Price memory price) {
        price = getEmaPriceUnsafe(id);
        if (_absDiff(block.timestamp, price.publishTime) > age) revert PythErrors.StalePrice();
    }

    // ---------------------------------------------------------------------
    // IPyth — pull update path
    // ---------------------------------------------------------------------

    function getUpdateFee(bytes[] calldata updateData) public view returns (uint256 feeAmount) {
        return singleUpdateFeeInWei * updateData.length;
    }

    /// @notice Apply pull updates. Each entry must be a `PythStructs.PriceFeed` as produced by
    ///         `createPriceFeedUpdateData`.
    /// @dev Real Pyth entries are Wormhole VAAs, which cannot be forged locally, so the mock
    ///      substitutes a plain ABI encoding. That is the one place this contract is not a
    ///      drop-in, and it only affects code that *constructs* update data — the updater bot,
    ///      which gets the real bytes from Hermes in production and from the helper below in
    ///      tests. Nothing the pool calls is affected.
    ///
    ///      Overpayment is kept rather than refunded, as in the real contract.
    function updatePriceFeeds(bytes[] calldata updateData) external payable {
        if (msg.value < getUpdateFee(updateData)) revert PythErrors.InsufficientFee();

        for (uint256 i = 0; i < updateData.length; ++i) {
            PythStructs.PriceFeed memory feed = abi.decode(updateData[i], (PythStructs.PriceFeed));
            if (feed.id == bytes32(0)) revert PythErrors.InvalidUpdateData();

            // Strictly-newer-wins, and a stale or replayed entry is dropped *without reverting*.
            // Mirrored exactly: an updater bot that infers "the transaction succeeded, therefore
            // the on-chain price is now mine" is wrong against real Pyth too, and a mock that
            // reverted here would hide that until mainnet.
            if (_priceFeeds[feed.id].price.publishTime < feed.price.publishTime) {
                _priceFeeds[feed.id] = feed;
                emit PriceFeedUpdate(feed.id, uint64(feed.price.publishTime), feed.price.price, feed.price.conf);
            }
        }
    }

    // ---------------------------------------------------------------------
    // Mock-only controls — absent from IPyth, so nothing that calls them survives the swap
    // ---------------------------------------------------------------------

    /// @notice Write a price directly, bypassing the update path entirely.
    /// @dev The bypass is the feature: it takes one call to put the reference oracle into any
    ///      state the guard needs to be tested against, including states real Pyth would never
    ///      reach on demand (an hour stale, published in the future, negative).
    ///
    ///      `emaPrice` is written to the same value. A feed whose spot is live while its EMA is
    ///      zeroed is a state the real oracle never produces, and silently breaks any consumer
    ///      that reads both. Call `setEmaPrice` *after* this to make them differ — a later
    ///      `setPrice` will overwrite the EMA again.
    /// @param expo the decimal exponent, normally negative: $3,500.00 at expo -8 is 350000000000
    /// @param publishTime the oracle's publish timestamp, not necessarily `block.timestamp`
    function setPrice(bytes32 id, int64 price, uint64 conf, int32 expo, uint256 publishTime) public {
        // Feed id 0 is the "never populated" sentinel in `_queryPriceFeed` — the real contract
        // has the same hole. Reject it loudly rather than let a test wonder why its price
        // vanished.
        if (id == bytes32(0)) revert PythErrors.InvalidArgument();

        PythStructs.Price memory p = PythStructs.Price({price: price, conf: conf, expo: expo, publishTime: publishTime});

        _priceFeeds[id] = PythStructs.PriceFeed({id: id, price: p, emaPrice: p});

        // Truncating cast, deliberately unguarded: the event field is uint64 while the struct
        // field is uint256, an inconsistency inherited from the real contract. Storage keeps the
        // full value and every comparison reads storage, so a test that parks `publishTime` past
        // 2**64 to probe the future-dated path still behaves correctly — only the log is lossy,
        // exactly as it would be against real Pyth.
        // forge-lint: disable-next-line(unsafe-typecast)
        emit PriceFeedUpdate(id, uint64(publishTime), price, conf);
    }

    /// @notice `setPrice` with `publishTime = block.timestamp`.
    /// @dev The common case, and worth its own entrypoint: a deployment or demo script that
    ///      hardcodes a publish timestamp produces a feed that is already stale by the time the
    ///      transaction lands, and the resulting `StalePrice` looks like a pool bug.
    function setPriceNow(bytes32 id, int64 price, uint64 conf, int32 expo) external {
        setPrice(id, price, conf, expo, block.timestamp);
    }

    /// @notice Override only the EMA leg of an existing feed. Requires the feed to exist.
    function setEmaPrice(bytes32 id, int64 price, uint64 conf, int32 expo, uint256 publishTime) external {
        // Reuses the not-found revert so an EMA write against an unset feed fails the same way a
        // read would, instead of quietly creating a feed with a zero spot price.
        _queryPriceFeed(id);
        _priceFeeds[id].emaPrice = PythStructs.Price({price: price, conf: conf, expo: expo, publishTime: publishTime});
    }

    /// @notice Encode one feed into the format `updatePriceFeeds` expects.
    /// @dev The real helper takes an extra trailing `prevPublishTime` that the VAA path needs;
    ///      this encoding has no VAA and no use for it. Test-only either way.
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

    // ---------------------------------------------------------------------
    // Internals
    // ---------------------------------------------------------------------

    function _queryPriceFeed(bytes32 id) internal view returns (PythStructs.PriceFeed memory feed) {
        feed = _priceFeeds[id];
        // A never-written feed has `id == 0` because the setter always stores `id`. This is the
        // real contract's sentinel and its revert; do not soften it into a zero return.
        if (feed.id == bytes32(0)) revert PythErrors.PriceFeedNotFound();
    }

    /// @dev Absolute difference, matching Pyth's own `diff`. Using the absolute value rather
    ///      than `block.timestamp - publishTime` is what makes a future-dated price stale too,
    ///      which is the correct behaviour: a reference bound is only meaningful if the
    ///      reference was observed, and a timestamp ahead of the chain has not been.
    function _absDiff(uint256 x, uint256 y) internal pure returns (uint256) {
        return x > y ? x - y : y - x;
    }
}
