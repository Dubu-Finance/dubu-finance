// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {Test} from "forge-std/Test.sol";
import {console2} from "forge-std/console2.sol";
import {PropPool} from "../src/PropPool.sol";
import {IPropPool} from "../src/interfaces/IPropPool.sol";
import {IPyth, PythErrors, PythStructs} from "../src/interfaces/IPyth.sol";
import {MockERC20} from "../src/mocks/MockERC20.sol";
import {MockPyth} from "../src/mocks/MockPyth.sol";

contract RevertingPyth is IPyth {
    error Down();

    function getValidTimePeriod() external pure returns (uint256) {
        revert Down();
    }

    function getPriceUnsafe(bytes32) external pure returns (PythStructs.Price memory) {
        revert Down();
    }

    function getEmaPriceUnsafe(bytes32) external pure returns (PythStructs.Price memory) {
        revert Down();
    }

    function getPriceNoOlderThan(bytes32, uint256) external pure returns (PythStructs.Price memory) {
        revert Down();
    }

    function getEmaPriceNoOlderThan(bytes32, uint256) external pure returns (PythStructs.Price memory) {
        revert Down();
    }

    function updatePriceFeeds(bytes[] calldata) external payable {
        revert Down();
    }

    function getUpdateFee(bytes[] calldata) external pure returns (uint256) {
        revert Down();
    }
}

contract GasBombPyth is IPyth {
    function getValidTimePeriod() external pure returns (uint256) {
        return 60;
    }

    function getPriceUnsafe(bytes32) external view returns (PythStructs.Price memory p) {
        while (gasleft() > 0) {}
        return p;
    }

    function getEmaPriceUnsafe(bytes32) external pure returns (PythStructs.Price memory p) {
        return p;
    }

    function getPriceNoOlderThan(bytes32, uint256) external pure returns (PythStructs.Price memory p) {
        return p;
    }

    function getEmaPriceNoOlderThan(bytes32, uint256) external pure returns (PythStructs.Price memory p) {
        return p;
    }

    function updatePriceFeeds(bytes[] calldata) external payable {}

    function getUpdateFee(bytes[] calldata) external pure returns (uint256) {
        return 0;
    }
}

contract PropPoolOracleTest is Test {

    PropPool internal pool;
    MockPyth internal pyth;

    MockERC20 internal weth;
    MockERC20 internal wbtc;
    MockERC20 internal usdc;
    MockERC20 internal dai;

    address internal owner = address(0xAA01);
    address internal manager = address(0xAA02);
    address internal updater = address(0xAA03);
    address internal guardian = address(0xAA04);
    address internal taker = address(0xAA05);

    uint16 internal constant PAIR_ETH = 1;
    uint16 internal constant PAIR_BTC = 2;

    uint16 internal constant PAIR_PLAIN = 3;

    uint8 internal constant EXP_ETH = 24;
    uint8 internal constant EXP_BTC = 12;

    int8 internal constant REF_EXPO_ETH = 12;
    int8 internal constant REF_EXPO_BTC = 10;

    bytes32 internal constant FEED_ETH = keccak256("Crypto.ETH/USD");
    bytes32 internal constant FEED_BTC = keccak256("Crypto.BTC/USD");

    int32 internal constant PYTH_EXPO = -8;
    int64 internal constant PYTH_ETH = 2_000e8;
    int64 internal constant PYTH_BTC = 100_000e8;

    uint256 internal constant REF_ETH = 2e15;
    uint256 internal constant REF_BTC = 1e15;

    uint16 internal constant DEV_BPS = 100;
    uint32 internal constant PYTH_STALE = 30;
    uint32 internal constant MAX_STALE = 60;

    uint56 internal constant MIN_PRICE_ETH = 1e15;
    uint56 internal constant MIN_PRICE_BTC = 5e14;

    uint256 internal constant BPS = 10_000;

    function setUp() public {

        vm.warp(1_800_000_000);

        weth = new MockERC20("Mock WETH", "mWETH", 18);
        wbtc = new MockERC20("Mock WBTC", "mWBTC", 8);
        usdc = new MockERC20("Mock USDC", "mUSDC", 6);
        dai = new MockERC20("Mock DAI", "mDAI", 18);

        pool = new PropPool(owner, manager, updater, guardian);
        pyth = new MockPyth(60, 0);

        vm.startPrank(owner);
        assertEq(pool.addPair(address(weth), address(usdc), EXP_ETH, MAX_STALE, MIN_PRICE_ETH), PAIR_ETH);
        assertEq(pool.addPair(address(wbtc), address(usdc), EXP_BTC, MAX_STALE, MIN_PRICE_BTC), PAIR_BTC);
        assertEq(pool.addPair(address(dai), address(usdc), EXP_ETH, MAX_STALE, MIN_PRICE_ETH), PAIR_PLAIN);
        pool.setPyth(address(pyth));
        vm.stopPrank();

        _setFeeds();

        vm.startPrank(manager);
        pool.setPairOracle(PAIR_ETH, FEED_ETH, DEV_BPS, PYTH_STALE, REF_EXPO_ETH);
        pool.setPairOracle(PAIR_BTC, FEED_BTC, DEV_BPS, PYTH_STALE, REF_EXPO_BTC);
        vm.stopPrank();

        _fund();
    }

    function _setFeeds() internal {
        pyth.setPriceNow(FEED_ETH, PYTH_ETH, 1e8, PYTH_EXPO);
        pyth.setPriceNow(FEED_BTC, PYTH_BTC, 1e8, PYTH_EXPO);
    }

    function _fund() internal {
        weth.mint(manager, 10_000e18);
        wbtc.mint(manager, 10_000e8);
        usdc.mint(manager, 100_000_000e6);
        vm.startPrank(manager);
        weth.approve(address(pool), type(uint256).max);
        wbtc.approve(address(pool), type(uint256).max);
        usdc.approve(address(pool), type(uint256).max);
        pool.deposit(address(weth), 1_000e18);
        pool.deposit(address(wbtc), 100e8);
        pool.deposit(address(usdc), 10_000_000e6);
        vm.stopPrank();

        weth.mint(taker, 1_000e18);
        usdc.mint(taker, 10_000_000e6);
        vm.startPrank(taker);
        weth.approve(address(pool), type(uint256).max);
        usdc.approve(address(pool), type(uint256).max);
        vm.stopPrank();
    }

    function _pack(uint16 pairId, uint256 minBid, uint256 maxBid, uint256 minAsk, uint256 maxAsk)
        internal
        pure
        returns (uint256)
    {
        return minBid | (maxBid << 56) | (minAsk << 112) | (maxAsk << 168) | (uint256(pairId) << 224);
    }

    function _push(uint16 pairId, uint256 minBid, uint256 maxBid, uint256 minAsk, uint256 maxAsk) internal {
        uint256[] memory w = new uint256[](1);
        w[0] = _pack(pairId, minBid, maxBid, minAsk, maxAsk);
        vm.prank(updater);
        pool.updateQuote(w);
    }

    function _ladder(uint256 mid)
        internal
        pure
        returns (uint256 minBid, uint256 maxBid, uint256 minAsk, uint256 maxAsk)
    {
        maxBid = (mid * (BPS - 5)) / BPS;
        minBid = (maxBid * (BPS - 25)) / BPS;
        minAsk = (mid * (BPS + 5)) / BPS;
        maxAsk = (minAsk * (BPS + 25)) / BPS;
    }

    function _pushLadder(uint16 pairId, uint256 mid) internal {
        (uint256 a, uint256 b, uint256 c, uint256 d) = _ladder(mid);
        _push(pairId, a, b, c, d);
    }

    function _one(uint16 pairId, uint256 mid) internal pure returns (uint256[] memory w) {
        (uint256 a, uint256 b, uint256 c, uint256 d) = _ladder(mid);
        w = new uint256[](1);
        w[0] = _pack(pairId, a, b, c, d);
    }

    function _coolAll() internal {
        vm.cool(address(pool));
        vm.cool(address(pyth));
    }

    function _refresh(uint16 pairId, uint96 bidCap, uint96 askCap) internal {
        vm.prank(updater);
        pool.refreshCapacity(pairId, bidCap, askCap);
    }

    function test_Scaling_Weth18_Usdc6_Exp24() public view {
        (uint256 price, uint8 status) = pool.referencePrice(PAIR_ETH);
        assertEq(status, pool.REF_OK(), "ETH reference not OK");
        assertEq(price, REF_ETH, "ETH reference did not scale to the pool's mid");

        assertEq(uint256(uint64(PYTH_ETH)) * 10 ** uint256(int256(PYTH_EXPO) + REF_EXPO_ETH), REF_ETH);
    }

    function test_Scaling_Wbtc8_Usdc6_Exp12() public view {
        (uint256 price, uint8 status) = pool.referencePrice(PAIR_BTC);
        assertEq(status, pool.REF_OK(), "BTC reference not OK");
        assertEq(price, REF_BTC, "BTC reference did not scale to the pool's mid");
        assertEq(uint256(uint64(PYTH_BTC)) * 10 ** uint256(int256(PYTH_EXPO) + REF_EXPO_BTC), REF_BTC);
    }

    function test_Scaling_ReferenceIsComparableWithTheStoredLadder() public {
        _pushLadder(PAIR_ETH, REF_ETH);
        IPropPool.PairSnapshot memory s = pool.snapshot(PAIR_ETH);
        (uint256 ref,) = pool.referencePrice(PAIR_ETH);

        assertLt(uint256(s.maxBid), ref, "top of book should sit just under a dead-on reference");
        assertGt(uint256(s.minAsk), ref, "best ask should sit just over a dead-on reference");

        assertGt(uint256(s.minBid) * BPS, ref * (BPS - 100));
        assertLt(uint256(s.maxAsk) * BPS, ref * (BPS + 100));
    }

    function test_Scaling_ExpoChangeIsHonoured() public {

        pyth.setPriceNow(FEED_ETH, 2_000e5, 1e5, -5);
        (uint256 price, uint8 status) = pool.referencePrice(PAIR_ETH);
        assertEq(status, pool.REF_OK());
        assertEq(price, REF_ETH, "a re-exponented feed must produce the same reference");
    }

    function test_Scaling_NegativeNetExponentDivides() public {
        vm.prank(manager);
        pool.setPairOracle(PAIR_ETH, FEED_ETH, DEV_BPS, PYTH_STALE, 6);

        (uint256 price, uint8 status) = pool.referencePrice(PAIR_ETH);
        assertEq(status, pool.REF_OK());
        assertEq(price, uint256(uint64(PYTH_ETH)) / 100, "negative net exponent must divide");
    }

    function test_LadderInsideTheBoundIsAccepted() public {
        _pushLadder(PAIR_ETH, REF_ETH);
        assertEq(pool.snapshot(PAIR_ETH).updatedAt, uint32(block.timestamp), "ladder did not land");

        pyth.setPriceNow(FEED_ETH, (PYTH_ETH * 10_050) / 10_000, 1e8, PYTH_EXPO);
        _pushLadder(PAIR_ETH, (REF_ETH * 10_050) / 10_000);
        pyth.setPriceNow(FEED_ETH, (PYTH_ETH * 9_950) / 10_000, 1e8, PYTH_EXPO);
        _pushLadder(PAIR_ETH, (REF_ETH * 9_950) / 10_000);
    }

    function test_LadderAboveTheBoundIsRejected() public {

        uint256 mid = (REF_ETH * 11_000) / 10_000;
        (uint256 minBid, uint256 maxBid, uint256 minAsk, uint256 maxAsk) = _ladder(mid);

        uint256[] memory w = new uint256[](1);
        w[0] = _pack(PAIR_ETH, minBid, maxBid, minAsk, maxAsk);
        vm.prank(updater);
        vm.expectRevert(abi.encodeWithSelector(PropPool.BidCeilingExceeded.selector, PAIR_ETH, REF_ETH, maxBid));
        pool.updateQuote(w);
    }

    function test_LadderBelowTheBoundIsRejected() public {
        uint256 mid = (REF_ETH * 9_000) / 10_000;
        (uint256 minBid, uint256 maxBid, uint256 minAsk, uint256 maxAsk) = _ladder(mid);

        uint256[] memory w = new uint256[](1);
        w[0] = _pack(PAIR_ETH, minBid, maxBid, minAsk, maxAsk);
        vm.prank(updater);
        vm.expectRevert(abi.encodeWithSelector(PropPool.AskFloorBreached.selector, PAIR_ETH, REF_ETH, minAsk));
        pool.updateQuote(w);
    }

    function test_BidCeiling_ExactBoundaryAcceptedAndOneUnitOverRejected() public {
        uint256 ceiling = (REF_ETH * (BPS + DEV_BPS)) / BPS;
        assertEq(ceiling, 2.02e15, "fixture arithmetic drifted; the boundary must be exact");

        _push(PAIR_ETH, MIN_PRICE_ETH, ceiling, ceiling + 1e12, ceiling + 2e12);
        assertEq(uint256(pool.snapshot(PAIR_ETH).maxBid), ceiling, "exact boundary must be accepted");

        uint256[] memory w = new uint256[](1);
        w[0] = _pack(PAIR_ETH, MIN_PRICE_ETH, ceiling + 1, ceiling + 1e12, ceiling + 2e12);
        vm.prank(updater);
        vm.expectRevert(
            abi.encodeWithSelector(PropPool.BidCeilingExceeded.selector, PAIR_ETH, REF_ETH, ceiling + 1)
        );
        pool.updateQuote(w);
    }

    function test_AskFloor_ExactBoundaryAcceptedAndOneUnitUnderRejected() public {
        uint256 floor_ = (REF_ETH * (BPS - DEV_BPS)) / BPS;
        assertEq(floor_, 1.98e15, "fixture arithmetic drifted; the boundary must be exact");

        _push(PAIR_ETH, MIN_PRICE_ETH, floor_ - 1e12, floor_, floor_ + 1e12);
        assertEq(uint256(pool.snapshot(PAIR_ETH).minAsk), floor_, "exact boundary must be accepted");

        uint256[] memory w = new uint256[](1);
        w[0] = _pack(PAIR_ETH, MIN_PRICE_ETH, floor_ - 1e12, floor_ - 1, floor_ + 1e12);
        vm.prank(updater);
        vm.expectRevert(abi.encodeWithSelector(PropPool.AskFloorBreached.selector, PAIR_ETH, REF_ETH, floor_ - 1));
        pool.updateQuote(w);
    }

    function test_Bound_IgnoresTheLaddersFarEnds() public {
        uint256 ceiling = (REF_ETH * (BPS + DEV_BPS)) / BPS;

        _push(PAIR_ETH, MIN_PRICE_ETH, ceiling, ceiling, uint256(type(uint56).max));
        assertEq(uint256(pool.snapshot(PAIR_ETH).minBid), MIN_PRICE_ETH, "a far-end bid must not be bounded");
        assertEq(
            uint256(pool.snapshot(PAIR_ETH).maxAsk), uint256(type(uint56).max), "a far-end ask must not be bounded"
        );
    }

    function test_CompromisedUpdater_CannotSellInventoryBelowFairValue() public {
        uint256 mid = (REF_ETH * 6_000) / BPS;
        (uint256 minBid, uint256 maxBid, uint256 minAsk, uint256 maxAsk) = _ladder(mid);

        assertTrue(maxAsk >= minAsk && minAsk >= maxBid && maxBid >= minBid, "ladder must be coherent");
        assertGe(minBid, MIN_PRICE_ETH, "ladder must clear the pair's absolute floor");

        uint256[] memory w = new uint256[](1);
        w[0] = _pack(PAIR_ETH, minBid, maxBid, minAsk, maxAsk);
        vm.prank(updater);
        vm.expectRevert(abi.encodeWithSelector(PropPool.AskFloorBreached.selector, PAIR_ETH, REF_ETH, minAsk));
        pool.updateQuote(w);

        assertEq(pool.snapshot(PAIR_ETH).updatedAt, 0, "a rejected ladder must not be stored");
        assertEq(pool.getAmountOut(address(usdc), address(weth), 1_000e6), 0, "no quote may survive the rejection");
    }

    function test_Batch_OneBadPairRevertsTheWholeCall() public {
        (uint256 a1, uint256 b1, uint256 c1, uint256 d1) = _ladder(REF_ETH);
        (uint256 a2, uint256 b2, uint256 c2, uint256 d2) = _ladder((REF_BTC * 11_000) / 10_000);

        uint256[] memory w = new uint256[](2);
        w[0] = _pack(PAIR_ETH, a1, b1, c1, d1);
        w[1] = _pack(PAIR_BTC, a2, b2, c2, d2);

        vm.prank(updater);
        vm.expectRevert(abi.encodeWithSelector(PropPool.BidCeilingExceeded.selector, PAIR_BTC, REF_BTC, b2));
        pool.updateQuote(w);

        assertEq(pool.snapshot(PAIR_ETH).updatedAt, 0, "the good pair in a reverted batch must not persist");
    }

    function test_Bound_IsPerPair() public {

        _pushLadder(PAIR_ETH, REF_ETH);
        _pushLadder(PAIR_BTC, REF_BTC);

        (uint256 a, uint256 b, uint256 c, uint256 d) = _ladder(REF_ETH);
        uint256[] memory w = new uint256[](1);
        w[0] = _pack(PAIR_BTC, a, b, c, d);
        vm.prank(updater);
        vm.expectRevert(abi.encodeWithSelector(PropPool.BidCeilingExceeded.selector, PAIR_BTC, REF_BTC, b));
        pool.updateQuote(w);
    }

    function test_ZeroDeviation_PinsTheBookToTheReference() public {
        vm.prank(manager);
        pool.setPairOracle(PAIR_ETH, FEED_ETH, 0, PYTH_STALE, REF_EXPO_ETH);

        _push(PAIR_ETH, MIN_PRICE_ETH, REF_ETH, REF_ETH, REF_ETH + 1e12);

        uint256[] memory w = new uint256[](1);
        w[0] = _pack(PAIR_ETH, MIN_PRICE_ETH, REF_ETH + 1, REF_ETH + 1, REF_ETH + 1e12);
        vm.prank(updater);
        vm.expectRevert(abi.encodeWithSelector(PropPool.BidCeilingExceeded.selector, PAIR_ETH, REF_ETH, REF_ETH + 1));
        pool.updateQuote(w);
    }

    function test_MaxDeviation_IsStillABound() public {
        vm.prank(manager);
        pool.setPairOracle(PAIR_ETH, FEED_ETH, uint16(BPS), PYTH_STALE, REF_EXPO_ETH);

        _push(PAIR_ETH, MIN_PRICE_ETH, REF_ETH * 2, REF_ETH * 2, REF_ETH * 2 + 1e12);

        uint256[] memory w = new uint256[](1);
        w[0] = _pack(PAIR_ETH, MIN_PRICE_ETH, REF_ETH * 2 + 1, REF_ETH * 2 + 1, REF_ETH * 2 + 1e12);
        vm.prank(updater);
        vm.expectRevert(
            abi.encodeWithSelector(PropPool.BidCeilingExceeded.selector, PAIR_ETH, REF_ETH, REF_ETH * 2 + 1)
        );
        pool.updateQuote(w);
    }

    function test_StalePyth_RejectsAtTheBoundary() public {
        pyth.setPrice(FEED_ETH, PYTH_ETH, 1e8, PYTH_EXPO, block.timestamp - PYTH_STALE);
        _pushLadder(PAIR_ETH, REF_ETH);

        pyth.setPrice(FEED_ETH, PYTH_ETH, 1e8, PYTH_EXPO, block.timestamp - PYTH_STALE - 1);
        (uint256 minBid, uint256 maxBid, uint256 minAsk, uint256 maxAsk) = _ladder(REF_ETH);
        uint256[] memory w = new uint256[](1);
        w[0] = _pack(PAIR_ETH, minBid, maxBid, minAsk, maxAsk);
        uint8 stale = pool.REF_STALE();
        vm.prank(updater);
        vm.expectRevert(abi.encodeWithSelector(PropPool.ReferenceUnavailable.selector, PAIR_ETH, stale));
        pool.updateQuote(w);
    }

    function test_FutureDatedPyth_IsStaleToo() public {
        pyth.setPrice(FEED_ETH, PYTH_ETH, 1e8, PYTH_EXPO, block.timestamp + PYTH_STALE + 1);

        (uint256 price, uint8 status) = pool.referencePrice(PAIR_ETH);
        assertEq(status, pool.REF_STALE(), "a future-dated price must read as stale");
        assertEq(price, 0);

        (uint256 minBid, uint256 maxBid, uint256 minAsk, uint256 maxAsk) = _ladder(REF_ETH);
        uint256[] memory w = new uint256[](1);
        w[0] = _pack(PAIR_ETH, minBid, maxBid, minAsk, maxAsk);
        uint8 stale = pool.REF_STALE();
        vm.prank(updater);
        vm.expectRevert(abi.encodeWithSelector(PropPool.ReferenceUnavailable.selector, PAIR_ETH, stale));
        pool.updateQuote(w);
    }

    function test_NegativePythPrice_IsRejected() public {
        pyth.setPriceNow(FEED_ETH, -1, 1e8, PYTH_EXPO);

        (uint256 price, uint8 status) = pool.referencePrice(PAIR_ETH);
        assertEq(status, pool.REF_INVALID(), "a negative price must be invalid, not cast");
        assertEq(price, 0);

        _expectPushFails(PAIR_ETH, REF_ETH, pool.REF_INVALID());
    }

    function test_ZeroPythPrice_IsRejected() public {
        pyth.setPriceNow(FEED_ETH, 0, 1e8, PYTH_EXPO);
        (, uint8 status) = pool.referencePrice(PAIR_ETH);
        assertEq(status, pool.REF_INVALID());
        _expectPushFails(PAIR_ETH, REF_ETH, pool.REF_INVALID());
    }

    function test_UnpopulatedFeed_FailsClosed() public {
        vm.prank(manager);
        pool.setPairOracle(PAIR_ETH, keccak256("Crypto.NOPE/USD"), DEV_BPS, PYTH_STALE, REF_EXPO_ETH);

        (uint256 price, uint8 status) = pool.referencePrice(PAIR_ETH);
        assertEq(status, pool.REF_UNAVAILABLE(), "an unpopulated feed must be UNAVAILABLE, not price 0");
        assertEq(price, 0);

        _expectPushFails(PAIR_ETH, REF_ETH, pool.REF_UNAVAILABLE());
    }

    function test_RevertingPyth_FailsClosed() public {
        address dead = address(new RevertingPyth());
        vm.prank(owner);
        pool.setPyth(dead);

        (, uint8 status) = pool.referencePrice(PAIR_ETH);
        assertEq(status, pool.REF_UNAVAILABLE());
        _expectPushFails(PAIR_ETH, REF_ETH, pool.REF_UNAVAILABLE());
    }

    function test_PythWithNoCode_IsUnavailableRatherThanReverting() public {
        vm.prank(owner);
        pool.setPyth(address(0xDEAD));

        (uint256 price, uint8 status) = pool.referencePrice(PAIR_ETH);
        assertEq(status, pool.REF_UNAVAILABLE(), "a codeless oracle must be UNAVAILABLE, not a revert");
        assertEq(price, 0);

        _expectPushFails(PAIR_ETH, REF_ETH, pool.REF_UNAVAILABLE());
    }

    function test_AbsurdExponent_IsInvalidRatherThanOverflowing() public {
        pyth.setPriceNow(FEED_ETH, PYTH_ETH, 1e8, type(int32).min);
        (, uint8 status) = pool.referencePrice(PAIR_ETH);
        assertEq(status, pool.REF_INVALID(), "hugely negative expo must be INVALID");

        pyth.setPriceNow(FEED_ETH, PYTH_ETH, 1e8, type(int32).max);
        (, status) = pool.referencePrice(PAIR_ETH);
        assertEq(status, pool.REF_INVALID(), "hugely positive expo must be INVALID");
    }

    function test_ReferenceAboveTheRepresentableCeiling_IsInvalid() public {
        vm.prank(manager);
        pool.setPairOracle(PAIR_ETH, FEED_ETH, DEV_BPS, PYTH_STALE, 40);

        (uint256 price, uint8 status) = pool.referencePrice(PAIR_ETH);
        assertEq(status, pool.REF_INVALID(), "an out-of-range reference must be INVALID");
        assertEq(price, 0);
        _expectPushFails(PAIR_ETH, REF_ETH, pool.REF_INVALID());
    }

    function test_ReferenceCollapsingToZero_IsInvalid() public {
        vm.prank(manager);
        pool.setPairOracle(PAIR_ETH, FEED_ETH, DEV_BPS, PYTH_STALE, -40);

        (, uint8 status) = pool.referencePrice(PAIR_ETH);
        assertEq(status, pool.REF_INVALID(), "a reference floored to zero must be INVALID");
    }

    function test_WrongRefExpo_FailsClosedInBothDirections() public {
        vm.prank(manager);
        pool.setPairOracle(PAIR_ETH, FEED_ETH, DEV_BPS, PYTH_STALE, REF_EXPO_ETH + 1);
        (uint256 hi, uint8 s1) = pool.referencePrice(PAIR_ETH);
        assertEq(s1, pool.REF_OK());
        assertEq(hi, REF_ETH * 10);
        (uint256 minBid, uint256 maxBid, uint256 minAsk, uint256 maxAsk) = _ladder(REF_ETH);
        uint256[] memory w = new uint256[](1);
        w[0] = _pack(PAIR_ETH, minBid, maxBid, minAsk, maxAsk);
        vm.prank(updater);
        vm.expectRevert(abi.encodeWithSelector(PropPool.AskFloorBreached.selector, PAIR_ETH, hi, minAsk));
        pool.updateQuote(w);

        vm.prank(manager);
        pool.setPairOracle(PAIR_ETH, FEED_ETH, DEV_BPS, PYTH_STALE, REF_EXPO_ETH - 1);
        (uint256 lo,) = pool.referencePrice(PAIR_ETH);
        assertEq(lo, REF_ETH / 10);
        vm.prank(updater);
        vm.expectRevert(abi.encodeWithSelector(PropPool.BidCeilingExceeded.selector, PAIR_ETH, lo, maxBid));
        pool.updateQuote(w);
    }

    function test_FailClosed_IsRecoverableByTheManagerAlone() public {
        _pushLadder(PAIR_ETH, REF_ETH);
        _refresh(PAIR_ETH, 100e18, 100e18);

        address dead = address(new RevertingPyth());
        vm.prank(owner);
        pool.setPyth(dead);

        _expectPushFails(PAIR_ETH, REF_ETH, pool.REF_UNAVAILABLE());

        assertGt(pool.getAmountOut(address(weth), address(usdc), 1e18), 0, "quoting must survive a dead oracle");
        vm.prank(taker);
        assertGt(pool.swap(address(weth), address(usdc), int256(1e18), 0, taker, 0, block.timestamp), 0);
        _refresh(PAIR_ETH, 50e18, 50e18);
        vm.prank(guardian);
        pool.pause(PAIR_ETH);
        vm.prank(guardian);
        pool.unpause(PAIR_ETH);

        vm.prank(manager);
        pool.setPairOracle(PAIR_ETH, bytes32(0), 0, 0, 0);
        _pushLadder(PAIR_ETH, REF_ETH);
        assertEq(pool.snapshot(PAIR_ETH).updatedAt, uint32(block.timestamp), "pushes must resume");
    }

    function test_Disabled_AcceptsAnyCoherentLadder() public {
        vm.prank(manager);
        pool.setPairOracle(PAIR_ETH, bytes32(0), 0, 0, 0);

        _push(PAIR_ETH, MIN_PRICE_ETH, MIN_PRICE_ETH, uint256(type(uint56).max), uint256(type(uint56).max));
        assertEq(uint256(pool.snapshot(PAIR_ETH).maxAsk), uint256(type(uint56).max));
    }

    function test_Disabled_DoesNotConsultPythEvenWhenPythIsDead() public {
        vm.prank(manager);
        pool.setPairOracle(PAIR_ETH, bytes32(0), 0, 0, 0);
        address dead = address(new RevertingPyth());
        vm.prank(owner);
        pool.setPyth(dead);

        _pushLadder(PAIR_ETH, REF_ETH);
        assertEq(pool.snapshot(PAIR_ETH).updatedAt, uint32(block.timestamp));

        (uint256 price, uint8 status) = pool.referencePrice(PAIR_ETH);
        assertEq(status, pool.REF_DISABLED(), "a disabled pair reports DISABLED, not UNAVAILABLE");
        assertEq(price, 0);
    }

    function test_Disabled_IsTheDefaultForANewPair() public {
        vm.prank(owner);
        uint16 id = pool.addPair(address(weth), address(wbtc), 12, MAX_STALE, 1);

        assertEq(pool.pairOracle(id).feedId, bytes32(0));
        (, uint8 status) = pool.referencePrice(id);
        assertEq(status, pool.REF_DISABLED());

        uint256[] memory w = new uint256[](1);
        w[0] = _pack(id, 1e12, 1e12, 2e12, 2e12);
        vm.prank(updater);
        pool.updateQuote(w);
    }

    function test_Disabled_AndBoundedPairsCoexist() public {
        vm.prank(manager);
        pool.setPairOracle(PAIR_BTC, bytes32(0), 0, 0, 0);

        (uint256 a1, uint256 b1, uint256 c1, uint256 d1) = _ladder(REF_ETH);
        (uint256 a2, uint256 b2, uint256 c2, uint256 d2) = _ladder(REF_BTC);
        uint256[] memory w = new uint256[](2);
        w[0] = _pack(PAIR_ETH, a1, b1, c1, d1);
        w[1] = _pack(PAIR_BTC, a2, b2, c2, d2);
        vm.prank(updater);
        pool.updateQuote(w);

        assertEq(pool.snapshot(PAIR_ETH).updatedAt, uint32(block.timestamp));
        assertEq(pool.snapshot(PAIR_BTC).updatedAt, uint32(block.timestamp));
    }

    function test_Snapshot_ReportsWhetherThePairIsBounded() public {
        assertEq(pool.snapshot(PAIR_ETH).flags & 0x8000, 0x8000, "a configured pair must read as bounded");

        vm.prank(manager);
        pool.setPairOracle(PAIR_ETH, bytes32(0), 0, 0, 0);
        assertEq(pool.snapshot(PAIR_ETH).flags & 0x8000, 0, "a disabled pair must read as unbounded");
        assertEq(pool.snapshot(PAIR_ETH).flags & 1, 0, "the derived bit must not disturb the paused bit");

        vm.prank(guardian);
        pool.pause(PAIR_ETH);
        assertEq(pool.snapshot(PAIR_ETH).flags & 1, 1, "paused bit lost");
        assertEq(pool.snapshot(PAIR_ETH).flags & 0x8000, 0, "bounded bit lost");

        vm.prank(manager);
        pool.setPairOracle(PAIR_ETH, FEED_ETH, DEV_BPS, PYTH_STALE, REF_EXPO_ETH);
        assertEq(pool.snapshot(PAIR_ETH).flags, 0x8001, "both bits must coexist");
    }

    function test_UpdaterCannotChangeTheDeviationLimitOrTheFeedId() public {
        vm.prank(updater);
        vm.expectRevert(PropPool.NotManager.selector);
        pool.setPairOracle(PAIR_ETH, FEED_ETH, 9_000, PYTH_STALE, REF_EXPO_ETH);

        vm.prank(updater);
        vm.expectRevert(PropPool.NotManager.selector);
        pool.setPairOracle(PAIR_ETH, bytes32(0), 0, 0, 0);

        vm.prank(updater);
        vm.expectRevert(PropPool.NotManager.selector);
        pool.setPairOracle(PAIR_ETH, FEED_ETH, DEV_BPS, type(uint32).max, REF_EXPO_ETH);

        address other = address(new RevertingPyth());
        vm.prank(updater);
        vm.expectRevert(PropPool.NotOwner.selector);
        pool.setPyth(other);

        PropPool.PairOracle memory o = pool.pairOracle(PAIR_ETH);
        assertEq(o.feedId, FEED_ETH);
        assertEq(o.maxDeviationBps, DEV_BPS);
        assertEq(o.maxPythStaleSecs, PYTH_STALE);
        assertEq(pool.pyth(), address(pyth));
    }

    function test_OwnerAndManagerRolesDoNotOverlap() public {
        vm.prank(owner);
        vm.expectRevert(PropPool.NotManager.selector);
        pool.setPairOracle(PAIR_ETH, FEED_ETH, DEV_BPS, PYTH_STALE, REF_EXPO_ETH);

        vm.prank(manager);
        vm.expectRevert(PropPool.NotOwner.selector);
        pool.setPyth(address(pyth));

        vm.prank(guardian);
        vm.expectRevert(PropPool.NotManager.selector);
        pool.setPairOracle(PAIR_ETH, bytes32(0), 0, 0, 0);
    }

    function test_SetPairOracle_RejectsBadConfiguration() public {
        vm.startPrank(manager);

        vm.expectRevert(PropPool.UnknownPair.selector);
        pool.setPairOracle(0, FEED_ETH, DEV_BPS, PYTH_STALE, REF_EXPO_ETH);
        vm.expectRevert(PropPool.UnknownPair.selector);
        pool.setPairOracle(99, FEED_ETH, DEV_BPS, PYTH_STALE, REF_EXPO_ETH);

        vm.expectRevert(PropPool.DeviationTooLarge.selector);
        pool.setPairOracle(PAIR_ETH, FEED_ETH, uint16(BPS + 1), PYTH_STALE, REF_EXPO_ETH);

        vm.expectRevert(PropPool.ZeroPythStaleWindow.selector);
        pool.setPairOracle(PAIR_ETH, FEED_ETH, DEV_BPS, 0, REF_EXPO_ETH);

        vm.stopPrank();
    }

    function test_SetPairOracle_DisablingIgnoresTheOtherArguments() public {
        vm.prank(manager);
        pool.setPairOracle(PAIR_ETH, bytes32(0), type(uint16).max, 0, type(int8).min);
        assertEq(pool.pairOracle(PAIR_ETH).feedId, bytes32(0));
        (, uint8 status) = pool.referencePrice(PAIR_ETH);
        assertEq(status, pool.REF_DISABLED());
    }

    function test_SetPairOracle_RequiresPythToBeSet() public {
        PropPool fresh = new PropPool(owner, manager, updater, guardian);
        vm.prank(owner);
        fresh.addPair(address(weth), address(usdc), EXP_ETH, MAX_STALE, MIN_PRICE_ETH);

        vm.prank(manager);
        vm.expectRevert(PropPool.PythNotSet.selector);
        fresh.setPairOracle(PAIR_ETH, FEED_ETH, DEV_BPS, PYTH_STALE, REF_EXPO_ETH);

        vm.prank(manager);
        fresh.setPairOracle(PAIR_ETH, bytes32(0), 0, 0, 0);
    }

    function test_SetPyth_RejectsZeroAndEmits() public {
        vm.prank(owner);
        vm.expectRevert(PropPool.ZeroAddress.selector);
        pool.setPyth(address(0));

        address next = address(new MockPyth(60, 0));
        vm.expectEmit(true, true, false, false);
        emit PropPool.PythUpdated(address(pyth), next);
        vm.prank(owner);
        pool.setPyth(next);
        assertEq(pool.pyth(), next);
    }

    function test_SetPairOracle_Emits() public {
        vm.expectEmit(true, false, false, true);
        emit PropPool.PairOracleUpdated(PAIR_ETH, FEED_ETH, 250, 45, REF_EXPO_ETH);
        vm.prank(manager);
        pool.setPairOracle(PAIR_ETH, FEED_ETH, 250, 45, REF_EXPO_ETH);

        PropPool.PairOracle memory o = pool.pairOracle(PAIR_ETH);
        assertEq(o.feedId, FEED_ETH);
        assertEq(o.maxDeviationBps, 250);
        assertEq(o.maxPythStaleSecs, 45);
        assertEq(o.refExpo, REF_EXPO_ETH);
    }

    function test_Views_StayTotalWhilePythIsBroken() public {
        _pushLadder(PAIR_ETH, REF_ETH);
        _refresh(PAIR_ETH, 100e18, 100e18);

        uint256 healthy = pool.getAmountOut(address(weth), address(usdc), 1e18);
        assertGt(healthy, 0);

        address dead = address(new RevertingPyth());
        vm.prank(owner);
        pool.setPyth(dead);

        assertEq(pool.getAmountOut(address(weth), address(usdc), 1e18), healthy, "quote changed with a dead oracle");
        assertGt(pool.getAmountIn(address(weth), address(usdc), 1_000e6), 0);
        assertGt(pool.quoteByPair(PAIR_ETH, true, 1e18), 0);
        pool.snapshot(PAIR_ETH);
        pool.getSupportedPairs();
        pool.pairConfig(PAIR_ETH);
        pool.pairOracle(PAIR_ETH);

        assertEq(pool.getAmountOut(address(weth), address(usdc), type(uint256).max), 0);
        assertEq(pool.getAmountIn(address(weth), address(usdc), type(uint256).max), 0);
    }

    function test_ReferencePrice_IsTotalInEveryState() public {
        (, uint8 s) = pool.referencePrice(0);
        assertEq(s, pool.REF_DISABLED());
        (, s) = pool.referencePrice(type(uint16).max);
        assertEq(s, pool.REF_DISABLED());

        vm.prank(owner);
        pool.setPyth(address(0xDEAD));
        (, s) = pool.referencePrice(PAIR_ETH);
        assertEq(s, pool.REF_UNAVAILABLE());

        address dead = address(new RevertingPyth());
        vm.prank(owner);
        pool.setPyth(dead);
        (, s) = pool.referencePrice(PAIR_ETH);
        assertEq(s, pool.REF_UNAVAILABLE());

        vm.prank(owner);
        pool.setPyth(address(pyth));
        pyth.setPriceNow(FEED_ETH, type(int64).min, 1e8, type(int32).min);
        (, s) = pool.referencePrice(PAIR_ETH);
        assertEq(s, pool.REF_INVALID());
    }

    function test_GasBombPyth_IsCaughtByTheSixtyThreeSixtyFourthsRule() public {
        address bomb = address(new GasBombPyth());
        vm.prank(owner);
        pool.setPyth(bomb);

        uint256[4] memory limits = [uint256(100_000), 200_000, 1_000_000, 10_000_000];
        for (uint256 i; i < limits.length; ++i) {
            (bool ok, bytes memory ret) =
                address(pool).staticcall{gas: limits[i]}(abi.encodeCall(PropPool.referencePrice, (PAIR_ETH)));
            assertTrue(ok, "a gas-bomb oracle must not break totality");
            (uint256 price, uint8 status) = abi.decode(ret, (uint256, uint8));
            assertEq(status, pool.REF_UNAVAILABLE(), "a gas-bomb oracle must read as UNAVAILABLE");
            assertEq(price, 0);
        }

        _expectPushFails(PAIR_ETH, REF_ETH, pool.REF_UNAVAILABLE());
    }

    function test_Swap_DoesNotConsultTheOracle() public {
        _pushLadder(PAIR_ETH, REF_ETH);
        _refresh(PAIR_ETH, 100e18, 100e18);

        address dead = address(new RevertingPyth());
        vm.prank(owner);
        pool.setPyth(dead);

        vm.prank(taker);
        uint256 out = pool.swap(address(weth), address(usdc), int256(1e18), 0, taker, 0, block.timestamp);
        assertGt(out, 0, "bid leg must not consult the oracle");

        vm.prank(taker);
        uint256 inAmt =
            pool.swap(address(usdc), address(weth), -int256(1e17), type(uint256).max, taker, 0, block.timestamp);
        assertGt(inAmt, 0, "ask leg must not consult the oracle");
    }

    function test_Gas_UpdateQuote_ColdTransaction_BoundedVersusUnbounded() public {

        _pushLadder(PAIR_ETH, REF_ETH);
        _pushLadder(PAIR_PLAIN, REF_ETH);

        uint256[] memory boundedA = _one(PAIR_ETH, (REF_ETH * 10_001) / BPS);
        uint256[] memory boundedB = _one(PAIR_ETH, (REF_ETH * 10_002) / BPS);
        uint256[] memory plainA = _one(PAIR_PLAIN, (REF_ETH * 10_001) / BPS);

        _coolAll();
        vm.prank(updater);
        uint256 g0 = gasleft();
        pool.updateQuote(boundedA);
        uint256 gBoundedCold = g0 - gasleft();

        vm.prank(updater);
        g0 = gasleft();
        pool.updateQuote(boundedB);
        uint256 gBoundedWarm = g0 - gasleft();

        _coolAll();
        vm.prank(updater);
        g0 = gasleft();
        pool.updateQuote(plainA);
        uint256 gPlainCold = g0 - gasleft();

        console2.log("updateQuote, cold, 1 unbounded pair:", gPlainCold);
        console2.log("updateQuote, cold, 1 bounded pair  :", gBoundedCold);
        console2.log("cost of the reference bound        :", gBoundedCold - gPlainCold);
        console2.log("(same push, all warm)              :", gBoundedWarm);

        assertGt(gBoundedCold, gBoundedWarm + 5_000, "the cold measurement is not actually cold");
        assertGt(gBoundedCold, gPlainCold, "the bound is not free, and should not appear to be");

        assertLt(gBoundedCold - gPlainCold, 25_000, "the bound costs far more than a single oracle read");
    }

    function test_Gas_UpdateQuote_UnboundedPairIsNearlyFree() public {
        PropPool bare = new PropPool(owner, manager, updater, guardian);
        vm.prank(owner);
        bare.addPair(address(weth), address(usdc), EXP_ETH, MAX_STALE, MIN_PRICE_ETH);

        uint256[] memory warmup = _one(PAIR_ETH, REF_ETH);
        uint256[] memory measured = _one(PAIR_ETH, (REF_ETH * 10_001) / BPS);
        uint256[] memory measuredPlain = _one(PAIR_PLAIN, (REF_ETH * 10_001) / BPS);

        vm.prank(updater);
        bare.updateQuote(warmup);
        _pushLadder(PAIR_PLAIN, REF_ETH);

        vm.cool(address(bare));
        vm.prank(updater);
        uint256 g0 = gasleft();
        bare.updateQuote(measured);
        uint256 gBare = g0 - gasleft();

        _coolAll();
        vm.prank(updater);
        g0 = gasleft();
        pool.updateQuote(measuredPlain);
        uint256 gPlain = g0 - gasleft();

        console2.log("updateQuote, cold, pool with the feature unused    :", gBare);
        console2.log("updateQuote, cold, unbounded pair in a bounded pool:", gPlain);
        console2.log("cost of carrying the feature unused               :", gPlain - gBare);

        assertLt(gPlain - gBare, 2_500, "an unbounded pair must cost at most one cold SLOAD more");
    }

    function test_Gas_UpdateQuote_BatchAmortisesTheOracleAccount() public {
        _pushLadder(PAIR_ETH, REF_ETH);
        _pushLadder(PAIR_BTC, REF_BTC);

        (uint256 a1, uint256 b1, uint256 c1, uint256 d1) = _ladder((REF_ETH * 10_001) / BPS);
        (uint256 a2, uint256 b2, uint256 c2, uint256 d2) = _ladder((REF_BTC * 10_001) / BPS);

        uint256[] memory one = new uint256[](1);
        one[0] = _pack(PAIR_ETH, a1, b1, c1, d1);
        uint256[] memory two = new uint256[](2);
        two[0] = _pack(PAIR_ETH, a1, b1, c1, d1);
        two[1] = _pack(PAIR_BTC, a2, b2, c2, d2);

        _coolAll();
        vm.prank(updater);
        uint256 g0 = gasleft();
        pool.updateQuote(one);
        uint256 gOne = g0 - gasleft();

        _coolAll();
        vm.prank(updater);
        g0 = gasleft();
        pool.updateQuote(two);
        uint256 gTwo = g0 - gasleft();

        console2.log("updateQuote, cold, 1 bounded pair :", gOne);
        console2.log("updateQuote, cold, 2 bounded pairs:", gTwo);
        console2.log("marginal cost of the second pair  :", gTwo - gOne);

        assertLt(gTwo - gOne, gOne, "the second bounded pair must be cheaper than the first");
    }

    function test_Gas_Swap_IsUnchangedByTheBound() public {
        _pushLadder(PAIR_ETH, REF_ETH);
        _refresh(PAIR_ETH, 100e18, 100e18);

        vm.prank(taker);
        pool.swap(address(weth), address(usdc), int256(1e18), 0, taker, 0, block.timestamp);

        uint256 snap = vm.snapshotState();

        vm.prank(manager);
        pool.setPairOracle(PAIR_ETH, FEED_ETH, DEV_BPS, PYTH_STALE, REF_EXPO_ETH);
        vm.prank(taker);
        uint256 g0 = gasleft();
        pool.swap(address(weth), address(usdc), int256(1e18), 0, taker, 0, block.timestamp);
        uint256 gBounded = g0 - gasleft();

        vm.revertToState(snap);
        vm.prank(manager);
        pool.setPairOracle(PAIR_ETH, bytes32(0), 0, 0, 0);
        vm.prank(taker);
        g0 = gasleft();
        pool.swap(address(weth), address(usdc), int256(1e18), 0, taker, 0, block.timestamp);
        uint256 gPlain = g0 - gasleft();

        console2.log("swap, bounded pair  :", gBounded);
        console2.log("swap, unbounded pair:", gPlain);

        assertEq(gBounded, gPlain, "swap must be exactly unaffected by the reference bound");
    }

    function test_Gas_View_IsUnchangedByTheBound() public {
        _pushLadder(PAIR_ETH, REF_ETH);
        _refresh(PAIR_ETH, 100e18, 100e18);
        pool.getAmountOut(address(weth), address(usdc), 1e18);

        uint256 snap = vm.snapshotState();

        vm.prank(manager);
        pool.setPairOracle(PAIR_ETH, FEED_ETH, DEV_BPS, PYTH_STALE, REF_EXPO_ETH);
        pool.getAmountOut(address(weth), address(usdc), 1e18);
        uint256 g0 = gasleft();
        pool.getAmountOut(address(weth), address(usdc), 1e18);
        uint256 gBounded = g0 - gasleft();

        vm.revertToState(snap);
        vm.prank(manager);
        pool.setPairOracle(PAIR_ETH, bytes32(0), 0, 0, 0);
        pool.getAmountOut(address(weth), address(usdc), 1e18);
        g0 = gasleft();
        pool.getAmountOut(address(weth), address(usdc), 1e18);
        uint256 gPlain = g0 - gasleft();

        console2.log("getAmountOut, bounded pair  :", gBounded);
        console2.log("getAmountOut, unbounded pair:", gPlain);
        assertEq(gBounded, gPlain, "getAmountOut must not depend on oracle configuration");
    }

    function _expectPushFails(uint16 pairId, uint256 mid, uint8 status) internal {
        (uint256 minBid, uint256 maxBid, uint256 minAsk, uint256 maxAsk) = _ladder(mid);
        uint256[] memory w = new uint256[](1);
        w[0] = _pack(pairId, minBid, maxBid, minAsk, maxAsk);
        vm.prank(updater);
        vm.expectRevert(abi.encodeWithSelector(PropPool.ReferenceUnavailable.selector, pairId, status));
        pool.updateQuote(w);
    }
}
