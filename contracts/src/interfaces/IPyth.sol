// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

// ---------------------------------------------------------------------------------------------
// Canonical Pyth surface.
//
// Everything in this file is a faithful subset of `pyth-sdk-solidity` — same type names, same
// field names, same field order, same error names, same function selectors. It is declared here
// rather than imported because the SDK is not vendored into this repo.
//
// This file was lifted verbatim out of `src/mocks/MockPyth.sol` when `PropPool` grew the
// reference-oracle bound, exactly as that file's header instructed. Both the pool and the mock
// import it now, so there is one declaration of the oracle ABI and not two that can drift.
//
// The point of the fidelity is that swapping in the real oracle is a one-line change. Pyth is
// live on GIWA Sepolia at 0x2880aB155794e7179c9eE2e38200202908C17B43 (pull, ~400ms), and it
// satisfies this interface exactly, so a deploy script goes from
//
//     pool.setPyth(address(new MockPyth(60, 0)));
//   to
//     pool.setPyth(0x2880aB155794e7179c9eE2e38200202908C17B43);
//
// with no other edit anywhere. Tests keep working too: the errors below hash to the same
// selectors as the real ones, so `vm.expectRevert(PythErrors.StalePrice.selector)` survives.
// ---------------------------------------------------------------------------------------------

/// @title PythStructs
/// @notice Mirror of `pyth-sdk-solidity/PythStructs.sol`. Declared as a `contract` rather than a
///         `library` because that is what the SDK does, and the distinction is visible to any
///         file that later imports both.
/// @dev The SDK spells `publishTime` as `uint`; `uint256` here is the identical type, respelled
///      only to satisfy this repo's `int_types = "long"` formatter setting. Do not rename or
///      reorder the fields — ABI decoding of real update data depends on the layout.
contract PythStructs {
    /// @notice A price with a confidence interval and a decimal exponent.
    ///
    /// The value is `price * 10 ** expo`, and **`expo` is normally negative** — this is the
    /// single most common way to misread a Pyth feed. ETH/USD at $3,500 arrives as
    /// `price = 350000000000, expo = -8`, not as `3500`. A consumer that ignores `expo`, or
    /// that assumes it is a positive decimal count, is off by sixteen orders of magnitude and
    /// will happily accept any quote at all when it is used as a deviation bound.
    ///
    /// `expo` is per-feed and Pyth can change it, so it must be read from the response rather
    /// than hardcoded. `price` is signed because some Pyth feeds (rates, spreads) legitimately
    /// go negative; crypto pairs do not, but the bound check still has to handle the type.
    struct Price {
        // Price.
        int64 price;
        // Confidence interval around the price, in the same units and exponent as `price`.
        uint64 conf;
        // Price exponent. Normally negative — see above.
        int32 expo;
        // Unix timestamp at which this price was published by Pyth, NOT the block timestamp at
        // which it landed on chain. The gap between the two is the staleness a pull oracle has
        // to be bounded against.
        uint256 publishTime;
    }

    struct PriceFeed {
        // The price feed id, e.g. ETH/USD. See https://pyth.network/developers/price-feed-ids
        bytes32 id;
        // Latest available price.
        Price price;
        // Latest available exponentially-weighted moving average price.
        Price emaPrice;
    }
}

/// @notice Mirror of `pyth-sdk-solidity/PythErrors.sol`, restricted to the errors this subset can
///         raise. Selectors derive from the name and argument list alone, so these are
///         byte-identical to the real contract's and tests do not need rewriting after the swap.
library PythErrors {
    /// @notice Function arguments are invalid.
    error InvalidArgument();
    /// @notice Update data is malformed (deserialization failure).
    error InvalidUpdateData();
    /// @notice The fee paid to `updatePriceFeeds` was below `getUpdateFee`.
    error InsufficientFee();
    /// @notice No price has ever been pushed on chain for this feed id.
    error PriceFeedNotFound();
    /// @notice A price exists but is outside the caller's freshness tolerance.
    error StalePrice();
}

/// @notice Mirror of `pyth-sdk-solidity/IPythEvents.sol`.
interface IPythEvents {
    /// @dev `publishTime` is `uint64` here while `PythStructs.Price.publishTime` is `uint256`.
    ///      That inconsistency is in the real contract; it is reproduced rather than fixed so
    ///      off-chain log decoders written against Pyth's ABI work against the mock unchanged.
    event PriceFeedUpdate(bytes32 indexed id, uint64 publishTime, int64 price, uint64 conf);
}

/// @notice The slice of `pyth-sdk-solidity/IPyth.sol` that DuBu uses.
///
/// Deliberately a subset: the full interface also carries `parsePriceFeedUpdates`,
/// `updatePriceFeedsIfNecessary`, `getPrice` and friends, none of which the pool's reference
/// bound needs, and each of which would be a lie if mocked shallowly. Every signature below is
/// copied exactly, so the deployed contract at 0x2880aB… satisfies this interface even though it
/// implements strictly more.
///
/// @dev `PropPool` calls exactly one of these — `getPriceUnsafe` — and bounds the staleness
///      itself. See `PropPool._referencePrice` for why it does not use `getPriceNoOlderThan`.
interface IPyth is IPythEvents {
    function getValidTimePeriod() external view returns (uint256 validTimePeriod);
    function getPriceUnsafe(bytes32 id) external view returns (PythStructs.Price memory price);
    function getEmaPriceUnsafe(bytes32 id) external view returns (PythStructs.Price memory price);
    function getPriceNoOlderThan(bytes32 id, uint256 age) external view returns (PythStructs.Price memory price);
    function getEmaPriceNoOlderThan(bytes32 id, uint256 age) external view returns (PythStructs.Price memory price);
    function updatePriceFeeds(bytes[] calldata updateData) external payable;
    function getUpdateFee(bytes[] calldata updateData) external view returns (uint256 feeAmount);
}
