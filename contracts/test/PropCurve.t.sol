// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {Test} from "forge-std/Test.sol";
import {console2} from "forge-std/console2.sol";
import {PropCurve} from "../src/libraries/PropCurve.sol";

contract PropCurveHarness {
    function outBid(uint256 amountIn, uint256 minBid, uint256 maxBid, uint256 cap, uint256 used, uint8 exp)
        external
        pure
        returns (uint256)
    {
        return PropCurve.amountOutBid(amountIn, minBid, maxBid, cap, used, exp);
    }

    function outAsk(uint256 amountIn, uint256 minAsk, uint256 maxAsk, uint256 cap, uint256 used, uint8 exp)
        external
        pure
        returns (uint256)
    {
        return PropCurve.amountOutAsk(amountIn, minAsk, maxAsk, cap, used, exp);
    }

    function inBid(uint256 amountOut, uint256 minBid, uint256 maxBid, uint256 cap, uint256 used, uint8 exp)
        external
        pure
        returns (uint256)
    {
        return PropCurve.amountInBid(amountOut, minBid, maxBid, cap, used, exp);
    }

    function inAsk(uint256 amountOut, uint256 minAsk, uint256 maxAsk, uint256 cap, uint256 used, uint8 exp)
        external
        pure
        returns (uint256)
    {
        return PropCurve.amountInAsk(amountOut, minAsk, maxAsk, cap, used, exp);
    }

    function validate(uint256 minBid, uint256 maxBid, uint256 minAsk, uint256 maxAsk, uint256 minPrice) external pure {
        PropCurve.validateLadder(minBid, maxBid, minAsk, maxAsk, minPrice);
    }

    function topBid(uint256 minBid, uint256 maxBid, uint256 cap, uint256 used) external pure returns (uint256) {
        return PropCurve.executableTopBid(minBid, maxBid, cap, used);
    }

    function topAsk(uint256 minAsk, uint256 maxAsk, uint256 cap, uint256 used) external pure returns (uint256) {
        return PropCurve.executableTopAsk(minAsk, maxAsk, cap, used);
    }
}

contract PropCurveTest is Test {
    PropCurveHarness internal h;

    uint256 internal constant U56 = type(uint56).max;
    uint256 internal constant MAX_OUT = type(uint128).max;
    uint256 internal constant U96 = type(uint96).max;

    uint256 internal constant DOMAIN_PRICE = 67_280_421_310_721;
    uint256 internal constant DOMAIN_SIZE = 5_057_672_949_897_463_733_145_855;

    function setUp() public {
        h = new PropCurveHarness();
    }

    function test_ZeroAmountIn_ReturnsZero() public view {
        assertEq(h.outBid(0, 100, 200, 1000, 0, 0), 0, "bid zero in");
        assertEq(h.outAsk(0, 100, 200, 1000, 0, 0), 0, "ask zero in");
    }

    function test_ZeroAmountIn_BeatsZeroCapacityCheck() public view {
        assertEq(h.outBid(0, 100, 200, 0, 0, 0), 0, "bid zero in / zero cap");
        assertEq(h.outAsk(0, 100, 200, 0, 0, 0), 0, "ask zero in / zero cap");
    }

    function test_ZeroCapacity_Reverts() public {
        vm.expectRevert(PropCurve.ZeroCapacity.selector);
        h.outBid(1, 100, 200, 0, 0, 0);

        vm.expectRevert(PropCurve.ZeroCapacity.selector);
        h.outAsk(1, 100, 200, 0, 0, 0);
    }

    function test_ExactCapacityBoundary_Succeeds() public view {

        assertEq(h.outBid(600, 100, 200, 1000, 400, 0), 600 * 130, "bid at exact capacity");

        uint256 askCeiling = 102_000;
        assertEq(h.inAsk(600, 100, 200, 1000, 400, 0), askCeiling, "ask cost of the whole epoch");

        assertEq(h.outAsk(askCeiling, 100, 200, 1000, 400, 0), uint256(600), "ask at exact capacity");
    }

    function test_OneUnitPastCapacity_Reverts() public {
        vm.expectRevert(PropCurve.AmountExceedsCapacity.selector);
        h.outBid(601, 100, 200, 1000, 400, 0);

        vm.expectRevert(PropCurve.AmountExceedsCapacity.selector);
        h.inAsk(601, 100, 200, 1000, 400, 0);

        vm.expectRevert(PropCurve.AmountExceedsCapacity.selector);
        h.outAsk(102_001, 100, 200, 1000, 400, 0);
    }

    function test_ExhaustedEpoch_Reverts() public {
        vm.expectRevert(PropCurve.AmountExceedsCapacity.selector);
        h.outBid(1, 100, 200, 1000, 1000, 0);

        vm.expectRevert(PropCurve.AmountExceedsCapacity.selector);
        h.outAsk(1, 100, 200, 1000, 1000, 0);
    }

    function test_LastUnitOfEpoch_PricedAtFloor() public view {

        assertEq(h.outBid(1, 100, 200, 1000, 999, 0), 100, "bid floor");

        assertEq(h.inAsk(1, 100, 200, 1000, 999, 0), 200, "ask ceiling");
    }

    function test_PriceScaleExp_FullRange() public view {
        uint256 amountIn = 1e24;
        uint256 cap = U96;
        for (uint8 e; e <= PropCurve.MAX_PRICE_SCALE_EXP; ++e) {

            if (e <= 16) {
                uint256 price = 10 ** e;
                assertEq(h.outBid(amountIn, price, price, cap, 0, e), amountIn, "bid identity");
            }

            uint256 expected = (10 ** uint256(e)) < cap ? (10 ** uint256(e)) : cap;
            assertEq(h.outAsk(1, 1, 1, cap, 0, e), expected, "ask identity");
        }
    }

    function test_MaxPriceScaleExp_NoOverflow() public {
        assertEq(PropCurve.MAX_PRICE_SCALE_EXP, 38, "exp ceiling moved");
        uint8 e = PropCurve.MAX_PRICE_SCALE_EXP;

        uint256 cap = U96;
        uint256 out = h.outBid(cap, U56, U56, cap, 0, e);
        assertEq(out, (cap * U56) / (10 ** e), "bid at exp 38");
        assertLe(out, MAX_OUT, "bid at exp 38 in domain");

        uint256 cost = h.inAsk(cap, U56, U56, cap, 0, e);
        assertLe(cost, MAX_OUT, "ask cost at exp 38 in domain");

        vm.expectRevert(PropCurve.AmountOutOfDomain.selector);
        h.outAsk(MAX_OUT + 1, 1, 1, cap, 0, e);
    }

    function test_PriceScaleExp_AboveMax_NotLibraryEnforced() public view {
        assertEq(h.outBid(1e24, 1, 1, U96, 0, 41), uint256(0), "no library check");
    }

    function test_AsymmetricDecimals_18_6() public view {
        uint256 price = 3e9;
        uint256 cap = 30e18;

        assertEq(h.outBid(1e18, price, price, cap, 0, 18), 3000e6, "18/6 bid");

        assertEq(h.outAsk(3000e6, price, price, cap, 0, 18), 1e18, "18/6 ask");

        assertEq(h.inAsk(1e18, price, price, cap, 0, 18), 3000e6, "18/6 ask cost");
    }

    function test_AsymmetricDecimals_8_6() public view {
        uint256 price = 6e10;
        assertEq(h.outBid(1e8, price, price, 100e8, 0, 8), 60_000e6, "8/6 bid");
        assertEq(h.outAsk(60_000e6, price, price, 10_000_000e6, 0, 8), 1e8, "8/6 ask");
    }

    function test_AsymmetricDecimals_6_18() public view {

        uint256 price = 2.5e14;
        assertEq(h.outBid(1e6, price, price, 1_000_000e6, 0, 6), 2.5e14, "6/18 bid");
        assertEq(h.outAsk(2.5e14, price, price, 100e18, 0, 6), 1e6, "6/18 ask");
    }

    function test_MaxUint56Prices() public view {
        uint256 cap = 1e24;
        assertEq(h.outBid(1e6, 1, U56, cap, 0, 0), 72_057_594_037_927_934_963_971, "max-width bid");

        assertEq(1e6 * (U56 - 1), 72_057_594_037_927_934_000_000, "pre-amendment value moved");

        assertEq(h.outBid(1e6, U56, U56, cap, 0, 0), 1e6 * U56, "flat max bid");
        assertEq(h.outAsk(U56, U56, U56, U96, 0, 0), 1, "flat max ask");
    }

    function test_FlatLadder_ConstantPrice() public view {
        uint256 cap = 1_000_000;
        uint256 price = 12_345_678;
        uint256[6] memory usages = [uint256(0), 1, 2, cap / 2, cap - 2, cap - 1];
        for (uint256 i; i < usages.length; ++i) {
            uint256 u = usages[i];
            assertEq(h.outBid(1, price, price, cap, u, 0), price, "flat bid constant");
            assertEq(h.outAsk(price, price, price, cap * price, u, 0), 1, "flat ask constant");
        }

        assertEq(h.outBid(cap, price, price, cap, 0, 0), cap * price, "flat bid whole epoch");
    }

    function test_ZeroPrice_Ask() public {
        vm.expectRevert(PropCurve.ZeroPrice.selector);
        h.outAsk(1, 0, 0, 1000, 0, 0);
    }

    function test_AmountOutOfDomain_Bid_JustUnder() public view {
        assertEq(PropCurve.MAX_AMOUNT_OUT, MAX_OUT, "domain ceiling moved");
        assertEq(DOMAIN_SIZE * DOMAIN_PRICE, MAX_OUT, "domain fixture no longer lands on the ceiling");
        assertLe(DOMAIN_PRICE, U56, "domain price outside the uint56 field");
        assertLe(DOMAIN_SIZE, U96, "domain size outside the uint96 field");

        assertEq(h.outBid(DOMAIN_SIZE, DOMAIN_PRICE, DOMAIN_PRICE, U96, 0, 0), MAX_OUT, "exactly at ceiling");
        assertEq(
            h.outBid(DOMAIN_SIZE - 1, DOMAIN_PRICE, DOMAIN_PRICE, U96, 0, 0),
            MAX_OUT - DOMAIN_PRICE,
            "one size step under the ceiling"
        );
    }

    function test_AmountOutOfDomain_Bid_JustOver() public {
        vm.expectRevert(PropCurve.AmountOutOfDomain.selector);
        h.outBid(DOMAIN_SIZE + 1, DOMAIN_PRICE, DOMAIN_PRICE, U96, 0, 0);
    }

    function test_AmountOutOfDomain_Ask_JustUnder() public view {

        assertEq(h.outAsk(MAX_OUT, DOMAIN_PRICE, DOMAIN_PRICE, U96, 0, 0), DOMAIN_SIZE, "exactly at ceiling");
        assertEq(h.outAsk(MAX_OUT - 1, DOMAIN_PRICE, DOMAIN_PRICE, U96, 0, 0), DOMAIN_SIZE - 1, "one under the ceiling");

        assertEq(h.inAsk(DOMAIN_SIZE, DOMAIN_PRICE, DOMAIN_PRICE, U96, 0, 0), MAX_OUT, "cost exactly at ceiling");
    }

    function test_AmountOutOfDomain_Ask_JustOver() public {
        vm.expectRevert(PropCurve.AmountOutOfDomain.selector);
        h.outAsk(MAX_OUT + 1, DOMAIN_PRICE, DOMAIN_PRICE, U96, 0, 0);

        vm.expectRevert(PropCurve.AmountOutOfDomain.selector);
        h.inAsk(DOMAIN_SIZE + 1, DOMAIN_PRICE, DOMAIN_PRICE, U96, 0, 0);
    }

    function test_AmountOutOfDomain_ViaScale() public {
        vm.expectRevert(PropCurve.AmountOutOfDomain.selector);
        h.outAsk(MAX_OUT + 1, 1, 1, U96, 0, 38);

        assertEq(h.inAsk(U96, 1, 1, U96, 0, 38), uint256(1), "epoch costs one quote unit at exp 38");
        vm.expectRevert(PropCurve.AmountExceedsCapacity.selector);
        h.outAsk(MAX_OUT, 1, 1, U96, 0, 38);
    }

    function test_ExecutableTop_Edges() public view {
        assertEq(h.topBid(100, 200, 0, 0), 0, "zero cap bid top is 0");
        assertEq(h.topAsk(100, 200, 0, 0), type(uint256).max, "zero cap ask top is unfillable");
        assertEq(h.topBid(100, 200, 1000, 1000), 100, "exhausted bid top is the floor");
        assertEq(h.topBid(100, 200, 1000, 5000), 100, "over-exhausted bid top is the floor");
        assertEq(h.topAsk(100, 200, 1000, 1000), 200, "exhausted ask top is the ceiling");
        assertEq(h.topBid(100, 200, 1000, 0), 200, "fresh bid top is maxBid");
        assertEq(h.topAsk(100, 200, 1000, 0), 100, "fresh ask top is minAsk");
        assertEq(h.topBid(100, 200, 1000, 500), 150, "half-consumed bid top");
        assertEq(h.topAsk(100, 200, 1000, 500), 150, "half-consumed ask top");
    }

    function test_ValidateLadder_AcceptsDocumentedOrdering() public view {
        h.validate(100, 110, 120, 130, 100);
        h.validate(100, 100, 100, 101, 100);
        h.validate(100, 110, 110, 110, 1);
        h.validate(1, 1, 1, U56, 1);
    }

    function test_ValidateLadder_RejectsEveryInversion() public {

        vm.expectRevert(PropCurve.BidBelowMinPrice.selector);
        h.validate(99, 110, 120, 130, 100);

        vm.expectRevert(PropCurve.CrossedBook.selector);
        h.validate(110, 100, 120, 130, 1);

        vm.expectRevert(PropCurve.CrossedBook.selector);
        h.validate(100, 120, 110, 130, 1);

        vm.expectRevert(PropCurve.CrossedBook.selector);
        h.validate(100, 110, 130, 120, 1);

        vm.expectRevert(PropCurve.CrossedBook.selector);
        h.validate(130, 120, 110, 100, 1);
    }

    function test_ValidateLadder_RejectsZeroSpreadBook() public {
        vm.expectRevert(PropCurve.CrossedBook.selector);
        h.validate(100, 100, 100, 100, 100);
    }

    function test_ValidateLadder_MinPriceCheckedBeforeCrossing() public {
        vm.expectRevert(PropCurve.BidBelowMinPrice.selector);
        h.validate(1, 0, 0, 0, 100);
    }

    function testFuzz_ValidateLadder_ExactlyTheDocumentedPredicate(
        uint56 minBid,
        uint56 maxBid,
        uint56 minAsk,
        uint56 maxAsk,
        uint56 minPrice
    ) public {
        bool floorOk = minBid >= minPrice;
        bool orderOk = maxAsk >= minAsk && minAsk >= maxBid && maxBid >= minBid;
        bool strictOk = maxAsk > minBid;

        if (floorOk && orderOk && strictOk) {
            h.validate(minBid, maxBid, minAsk, maxAsk, minPrice);
        } else if (!floorOk) {
            vm.expectRevert(PropCurve.BidBelowMinPrice.selector);
            h.validate(minBid, maxBid, minAsk, maxAsk, minPrice);
        } else {
            vm.expectRevert(PropCurve.CrossedBook.selector);
            h.validate(minBid, maxBid, minAsk, maxAsk, minPrice);
        }
    }

    function test_BUG3_AmountOutNotMonotonicInAmountIn_Bid() public view {
        uint256 maxBid = 3e9;
        uint256 minBid = maxBid - (maxBid * 100) / 10_000;
        uint256 cap = 30e18;

        uint256 lower = h.outBid(2e18, minBid, maxBid, cap, 0, 18);
        uint256 higher = h.outBid(2e18 + 1, minBid, maxBid, cap, 0, 18);

        console2.log("out(2.000000000000000000 WETH) =", lower);
        console2.log("out(2.000000000000000001 WETH) =", higher);

        assertGe(higher, lower, "BUG3 regressed: one extra wei of input bought a smaller output");
    }

    function test_BUG3_AmountOutNotMonotonicInAmountIn_Ask() public view {
        uint256 lower = h.outAsk(180, 29, 63, 297, 49, 1);
        uint256 higher = h.outAsk(181, 29, 63, 297, 49, 1);
        console2.log("ask out(180) =", lower);
        console2.log("ask out(181) =", higher);
        assertGe(higher, lower, "BUG3 regressed: one extra unit of input bought a smaller output");
    }

    function testFuzz_AmountOut_NeverExceedsTopOfBook(
        uint256 amountIn,
        uint256 minBid,
        uint256 width,
        uint256 cap,
        uint256 used,
        uint8 exp
    ) public view {
        (minBid, width, cap, used, exp) = _boundLadder(minBid, width, cap, used, exp);
        uint256 maxBid = minBid + width;
        amountIn = bound(amountIn, 1, cap - used);

        (bool ok, uint256 out) = _tryBid(amountIn, minBid, maxBid, cap, used, exp);
        if (!ok) return;

        assertLe(out * (10 ** exp) * cap, amountIn * (maxBid * cap - width * used), "bid out above the exact top");

        uint256 top = h.topBid(minBid, maxBid, cap, used);
        assertLt(out * (10 ** exp), amountIn * (top + 1), "bid out above the reported top of book");
    }

    function testFuzz_AmountOut_MonotoneEverywhere(
        uint256 amountIn,
        uint256 minBid,
        uint256 width,
        uint256 cap,
        uint256 used,
        uint8 exp
    ) public view {
        (minBid, width, cap, used, exp) = _boundLadder(minBid, width, cap, used, exp);
        uint256 maxBid = minBid + width;
        if (cap - used < 2) return;
        amountIn = bound(amountIn, 1, cap - used - 1);

        (bool okA, uint256 a) = _tryBid(amountIn, minBid, maxBid, cap, used, exp);
        (bool okB, uint256 b) = _tryBid(amountIn + 1, minBid, maxBid, cap, used, exp);
        if (!okA || !okB) return;
        assertGe(b, a, "bid not monotone");
    }

    function testFuzz_RealisedBidNeverBeatsExecutableTop(
        uint256 amountIn,
        uint256 minBid,
        uint256 width,
        uint256 cap,
        uint256 used,
        uint8 exp
    ) public view {
        (minBid, width, cap, used, exp) = _boundLadder(minBid, width, cap, used, exp);
        uint256 maxBid = minBid + width;
        amountIn = bound(amountIn, 1, cap - used);

        (bool ok, uint256 out) = _tryBid(amountIn, minBid, maxBid, cap, used, exp);
        if (!ok) return;

        uint256 top = h.topBid(minBid, maxBid, cap, used);

        assertLt(out * (10 ** exp), (top + 1) * amountIn, "realised bid better than executable top");

        assertLe(minBid * amountIn, out * (10 ** exp) + (10 ** exp), "realised bid under the floor");
    }

    function testFuzz_RealisedAskNeverBeatsExecutableTop(
        uint256 amountIn,
        uint256 minAsk,
        uint256 width,
        uint256 cap,
        uint256 used,
        uint8 exp
    ) public view {
        (minAsk, width, cap, used, exp) = _boundLadder(minAsk, width, cap, used, exp);
        uint256 maxAsk = minAsk + width;
        amountIn = bound(amountIn, 1, cap - used);

        (bool ok, uint256 out) = _tryAsk(amountIn, minAsk, maxAsk, cap, used, exp);
        if (!ok || out == 0) return;

        uint256 top = h.topAsk(minAsk, maxAsk, cap, used);

        assertLt(top * out, amountIn * (10 ** exp) + out, "realised ask better than executable top");
    }

    function test_BUG1_Ask_SplittingBeatsOneShot() public view {
        uint256 minAsk = 3e9;
        uint256 cap = 30e18;
        uint8 exp = 18;

        uint16[3] memory widthsBps = [20, 100, 1000];
        uint256[3] memory pieces = [uint256(2), 4, 16];

        for (uint256 w; w < widthsBps.length; ++w) {
            uint256 maxAsk = minAsk + (minAsk * widthsBps[w]) / 10_000;

            uint256 ceilingQuote = h.inAsk(cap, minAsk, maxAsk, cap, 0, exp);
            uint256 one = h.outAsk(ceilingQuote, minAsk, maxAsk, cap, 0, exp);
            assertEq(one, cap, "the epoch's quote ceiling must buy exactly its base");
            console2.log("width bps", widthsBps[w]);
            console2.log("  epoch quote ceiling", ceilingQuote);

            for (uint256 p; p < pieces.length; ++p) {
                uint256 n = pieces[p];

                uint256 sum = _splitAsk(ceilingQuote, minAsk, maxAsk, cap, exp, n);
                console2.log("  pieces", n);
                console2.log("    quote-in one shot", one);
                console2.log("    quote-in split   ", sum);
                assertLe(sum, one, "BUG1 regressed: a split ask bought more base than one shot");

                assertGe(sum + (n - 1), one, "quote-in split shortfall exceeded one base unit per extra piece");

                uint256 sumCost = _splitAskCost(cap, minAsk, maxAsk, cap, exp, n);
                console2.log("    base-out one shot cost", ceilingQuote);
                console2.log("    base-out split cost   ", sumCost);
                assertGe(sumCost, ceilingQuote, "BUG1 regressed: a split ask paid less quote than one shot");
                assertLe(sumCost, ceilingQuote + (n - 1), "base-out split excess exceeded the n-1 unit bound");
            }
        }
    }

    function testFuzz_Bid_SplitNeverBeatsOneShot(
        uint256 amountIn,
        uint256 minBid,
        uint256 width,
        uint256 cap,
        uint256 used,
        uint8 exp,
        uint8 pieces
    ) public view {
        (minBid, width, cap, used, exp) = _boundLadder(minBid, width, cap, used, exp);
        if (cap - used < 2) return;
        amountIn = bound(amountIn, 2, cap - used);
        uint256 n = bound(pieces, 2, 8);
        if (amountIn < n) return;

        (bool ok, uint256 one) = _tryBid(amountIn, minBid, minBid + width, cap, used, exp);
        if (!ok) return;
        (bool okSplit, uint256 sum) = _splitBid(
            Walk({amountIn: amountIn, pLow: minBid, pHigh: minBid + width, cap: cap, used: used, n: n, exp: exp})
        );
        if (!okSplit) return;

        assertLe(sum, one, "a split bid collected more quote than one shot");
        assertGe(sum + (n - 1), one, "split bid shortfall exceeded one quote unit per extra piece");
    }

    function test_BUG4_Bid_SplittingBeatsOneShot_SmallScale() public view {
        uint256 one = h.outBid(53, 8, 38, 58, 4, 0);
        uint256 a = h.outBid(50, 8, 38, 58, 4, 0);
        uint256 b = h.outBid(3, 8, 38, 58, 54, 0);
        console2.log("one shot 53 ->", one);
        console2.log("split 50 + 3 ->", a + b);
        assertLe(a + b, one, "BUG4 regressed: splitting a bid beat executing it whole");

        assertGe(a + b + 1, one, "bid split shortfall exceeded one output unit for two pieces");
    }

    function testFuzz_Ask_SplitNeverBeatsTopOfBook(
        uint256 amountIn,
        uint256 minAsk,
        uint256 width,
        uint256 cap,
        uint256 used,
        uint8 exp,
        uint8 pieces
    ) public view {
        (minAsk, width, cap, used, exp) = _boundLadder(minAsk, width, cap, used, exp);
        if (cap - used < 2) return;
        amountIn = bound(amountIn, 2, cap - used);
        uint256 n = bound(pieces, 2, 8);
        if (amountIn < n) return;

        (bool ok, uint256 sum) = _splitAskSafe(
            Walk({amountIn: amountIn, pLow: minAsk, pHigh: minAsk + width, cap: cap, used: used, n: n, exp: exp})
        );
        if (!ok || sum == 0) return;

        uint256 top = h.topAsk(minAsk, minAsk + width, cap, used);
        if (top == type(uint256).max) return;

        assertLt(top * sum, amountIn * (10 ** exp) + sum, "ask splitting beat the top of book");
    }

    function testFuzz_Ask_SplitCostNeverBelowOneShot(
        uint256 amountOut,
        uint256 minAsk,
        uint256 width,
        uint256 cap,
        uint256 used,
        uint8 exp,
        uint8 pieces
    ) public view {
        (minAsk, width, cap, used, exp) = _boundLadder(minAsk, width, cap, used, exp);
        if (cap - used < 2) return;
        amountOut = bound(amountOut, 2, cap - used);
        uint256 n = bound(pieces, 2, 8);
        if (amountOut < n) return;

        (bool ok, uint256 one) = _tryAskCost(amountOut, minAsk, minAsk + width, cap, used, exp);
        if (!ok) return;
        (bool okSplit, uint256 sum) = _splitAskCostWalk(
            Walk({amountIn: amountOut, pLow: minAsk, pHigh: minAsk + width, cap: cap, used: used, n: n, exp: exp})
        );
        if (!okSplit) return;

        assertGe(sum, one, "a split ask paid less quote than executing it whole");
        assertLe(sum, one + (n - 1), "split ask excess exceeded one quote unit per extra piece");
    }

    function testFuzz_Bid_SplitNeverExceedsExactIntegral(
        uint256 amountIn,
        uint256 minBid,
        uint256 width,
        uint256 cap,
        uint8 exp,
        uint8 pieces
    ) public view {
        (minBid, width, cap,, exp) = _boundLadder(minBid, width, cap, 0, exp);
        amountIn = bound(amountIn, 2, cap);
        uint256 n = bound(pieces, 2, 8);
        if (amountIn < n) return;

        (bool ok, uint256 sum) = _splitBid(
            Walk({amountIn: amountIn, pLow: minBid, pHigh: minBid + width, cap: cap, used: 0, n: n, exp: exp})
        );
        if (!ok) return;

        assertLe(
            sum * 2 * cap * (10 ** exp),
            amountIn * (2 * cap * (minBid + width) - width * amountIn),
            "split sum exceeded the exact integral"
        );
    }

    struct Ladder {
        uint256 minBid;
        uint256 maxBid;
        uint256 minAsk;
        uint256 maxAsk;
    }

    function _ladder(uint256 minBid, uint256 bidWidth, uint256 spread, uint256 askWidth)
        internal
        pure
        returns (bool ok, Ladder memory L)
    {
        L.minBid = bound(minBid, 1, U56 / 8);
        L.maxBid = L.minBid + bound(bidWidth, 0, L.minBid);
        L.minAsk = L.maxBid + bound(spread, 0, L.maxBid);
        L.maxAsk = L.minAsk + bound(askWidth, 0, L.minAsk);
        ok = L.maxAsk <= U56 && L.maxAsk > L.minBid;
    }

    function testFuzz_BidThenAsk_NeverReturnsMoreThanPutIn(
        uint256 baseIn,
        uint256 minBid,
        uint256 bidWidth,
        uint256 spread,
        uint256 askWidth,
        uint8 exp
    ) public view {
        (bool built, Ladder memory L) = _ladder(minBid, bidWidth, spread, askWidth);
        if (!built) return;
        exp = uint8(bound(exp, 0, 24));
        baseIn = bound(baseIn, 1, type(uint96).max);

        (bool okBid, uint256 quoteOut) = _tryBid(baseIn, L.minBid, L.maxBid, type(uint96).max, 0, exp);
        if (!okBid || quoteOut == 0) return;

        (bool okAsk, uint256 baseBack) =
            _tryAsk(quoteOut, L.minAsk, L.maxAsk, quoteOut > type(uint96).max ? quoteOut : type(uint96).max, 0, exp);
        if (!okAsk) return;

        assertLe(baseBack, baseIn, "bid then ask returned more base than was put in");
    }

    function testFuzz_AskThenBid_NeverReturnsMoreThanPutIn(
        uint256 quoteIn,
        uint256 minBid,
        uint256 bidWidth,
        uint256 spread,
        uint256 askWidth,
        uint8 exp
    ) public view {
        (bool built, Ladder memory L) = _ladder(minBid, bidWidth, spread, askWidth);
        if (!built) return;
        exp = uint8(bound(exp, 0, 24));
        quoteIn = bound(quoteIn, 1, type(uint96).max);

        (bool okAsk, uint256 baseOut) = _tryAsk(quoteIn, L.minAsk, L.maxAsk, type(uint96).max, 0, exp);
        if (!okAsk || baseOut == 0) return;

        (bool okBid, uint256 quoteBack) =
            _tryBid(baseOut, L.minBid, L.maxBid, baseOut > type(uint96).max ? baseOut : type(uint96).max, 0, exp);
        if (!okBid) return;

        assertLe(quoteBack, quoteIn, "ask then bid returned more quote than was put in");
    }

    string internal constant VECTORS_PATH = "testdata/curve_vectors.json";

    struct Vec {
        uint256 amountIn;
        uint256 capacity;
        uint256 expected;
        uint256 maxAsk;
        uint256 maxBid;
        uint256 minAsk;
        uint256 minBid;
        uint256 minPrice;
        uint256 priceScaleExp;
        uint256 used;
        string expectRevert;
        string kind;
        string name;
    }

    function test_Differential_CurveVectors() public {
        if (!vm.exists(VECTORS_PATH)) {
            console2.log("SKIP: %s absent; differential vectors not asserted", VECTORS_PATH);
            return;
        }
        (uint256 checked, uint256 skipped) = _runVectors(vm.readFile(VECTORS_PATH));
        console2.log("differential vectors asserted:", checked);
        console2.log("differential vectors skipped: ", skipped);
        assertGt(checked, 0, "vector file present but nothing decoded - schema mismatch");
        assertEq(skipped, 0, "some vectors could not be decoded or dispatched");

        assertGe(checked, 250, "fewer vectors asserted than the fixture carries");
    }

    function test_Differential_DecoderSelfTest() public {
        string memory path = "testdata/_propcurve_decoder_selftest.json";
        vm.writeFile(path, string.concat("[", _selfTestAmountVectors(), ",", _selfTestOtherVectors(), "]"));
        (uint256 checked, uint256 skipped) = _runVectors(vm.readFile(path));
        vm.removeFile(path);
        assertEq(skipped, 1, "the unknown-kind vector must be the only skip");
        assertEq(checked, 10, "self test did not decode every vector");
    }

    function _selfTestAmountVectors() private pure returns (string memory) {
        return string.concat(
            '{"amountIn":"600","capacity":"1000","expectRevert":"","expected":"78000",',
            '"kind":"amountOutBid","maxAsk":"0","maxBid":"200","minAsk":"0","minBid":"100",',
            '"minPrice":"0","name":"self/bid","priceScaleExp":"0","used":"400"},',
            '{"amountIn":"78000","capacity":"1000","expectRevert":"","expected":"600",',
            '"kind":"amountInBid","maxAsk":"0","maxBid":"200","minAsk":"0","minBid":"100",',
            '"minPrice":"0","name":"self/bid_exact_out","priceScaleExp":"0","used":"400"},',
            '{"amountIn":"600","capacity":"1000","expectRevert":"","expected":"102000",',
            '"kind":"amountInAsk","maxAsk":"200","maxBid":"0","minAsk":"100","minBid":"0",',
            '"minPrice":"0","name":"self/ask_exact_out","priceScaleExp":"0","used":"400"},',
            '{"amountIn":"102000","capacity":"1000","expectRevert":"","expected":"600",',
            '"kind":"amountOutAsk","maxAsk":"200","maxBid":"0","minAsk":"100","minBid":"0",',
            '"minPrice":"0","name":"self/ask_ceiling","priceScaleExp":"0","used":"400"},',
            '{"amountIn":"600","capacity":"1000","expectRevert":"","expected":"4",',
            '"kind":"amountOutAsk","maxAsk":"200","maxBid":"0","minAsk":"100","minBid":"0",',
            '"minPrice":"0","name":"self/ask","priceScaleExp":"0","used":"400"}'
        );
    }

    function _selfTestOtherVectors() private pure returns (string memory) {
        return string.concat(
            '{"amountIn":"601","capacity":"1000","expectRevert":"AmountExceedsCapacity","expected":"0",',
            '"kind":"amountOutBid","maxAsk":"0","maxBid":"200","minAsk":"0","minBid":"100",',
            '"minPrice":"0","name":"self/over","priceScaleExp":"0","used":"400"},',
            '{"amountIn":"601","capacity":"1000","expectRevert":"AmountExceedsCapacity","expected":"0",',
            '"kind":"amountInAsk","maxAsk":"200","maxBid":"0","minAsk":"100","minBid":"0",',
            '"minPrice":"0","name":"self/ask_over","priceScaleExp":"0","used":"400"},',
            '{"amountIn":"0","capacity":"0","expectRevert":"CrossedBook","expected":"0",',
            '"kind":"validateLadder","maxAsk":"1","maxBid":"2","minAsk":"3","minBid":"4",',
            '"minPrice":"1","name":"self/crossed","priceScaleExp":"0","used":"0"},',
            '{"amountIn":"0","capacity":"1000","expectRevert":"","expected":"150",',
            '"kind":"executableTopBid","maxAsk":"0","maxBid":"200","minAsk":"0","minBid":"100",',
            '"minPrice":"0","name":"self/top_bid","priceScaleExp":"0","used":"500"},',

            '{"amountIn":"0","capacity":"1000","expectRevert":"ZeroPrice","expected":"0",',
            '"kind":"amountSideways","maxAsk":"0","maxBid":"200","minAsk":"0","minBid":"100",',
            '"minPrice":"0","name":"self/unknown_kind","priceScaleExp":"0","used":"0"},',
            '{"amountIn":"0","capacity":"1000","expectRevert":"","expected":"150",',
            '"kind":"executableTopAsk","maxAsk":"200","maxBid":"0","minAsk":"100","minBid":"0",',
            '"minPrice":"0","name":"self/top_ask","priceScaleExp":"0","used":"500"}'
        );
    }

    function _runVectors(string memory json) internal returns (uint256 checked, uint256 skipped) {
        string memory root = _exists(json, "$[0]") ? "$" : (_exists(json, ".vectors[0]") ? ".vectors" : "");
        if (bytes(root).length == 0) {
            console2.log("SKIP: no vector array at either `$` or `.vectors`");
            return (0, 1);
        }
        uint256 n = _arrayLength(json, root);
        console2.log("vector root:", root);
        console2.log("vector count:", n);

        for (uint256 i; i < n; ++i) {
            if (_assertVector(json, string.concat(root, "[", vm.toString(i), "]"), i)) ++checked;
            else ++skipped;
        }
    }

    function _exists(string memory json, string memory path) internal view returns (bool) {
        try vm.parseJson(json, path) returns (bytes memory raw) {
            return raw.length > 0;
        } catch {
            return false;
        }
    }

    function _arrayLength(string memory json, string memory root) internal view returns (uint256 n) {
        while (n < 8192 && _exists(json, string.concat(root, "[", vm.toString(n), "]"))) {
            ++n;
        }
    }

    function _decode(string memory json, string memory p) internal view returns (bool ok, Vec memory v) {
        (ok, v.kind) = _s(json, p, _keysKind());
        if (!ok) return (false, v);
        (, v.name) = _s(json, p, _keysName());
        (, v.expectRevert) = _s(json, p, _keysRevert());
        (, v.amountIn) = _u(json, p, _keysAmountIn());
        (, v.capacity) = _u(json, p, _keysCapacity());
        (, v.used) = _u(json, p, _keysUsed());
        (, v.priceScaleExp) = _u(json, p, _keysExp());
        (, v.minBid) = _u(json, p, _keysMinBid());
        (, v.maxBid) = _u(json, p, _keysMaxBid());
        (, v.minAsk) = _u(json, p, _keysMinAsk());
        (, v.maxAsk) = _u(json, p, _keysMaxAsk());
        (, v.minPrice) = _u(json, p, _keysMinPrice());
        (, v.expected) = _u(json, p, _keysExpected());
        return (true, v);
    }

    function _assertVector(string memory json, string memory p, uint256 idx) internal returns (bool) {
        (bool ok, Vec memory v) = _decode(json, p);
        if (!ok) return false;

        string memory label = string.concat("vector ", vm.toString(idx), " (", v.name, ")");

        if (!_dispatchable(v.kind)) {
            console2.log("SKIP: unknown vector kind:", v.kind);
            return false;
        }

        bool expectsRevert = bytes(v.expectRevert).length != 0 && !_eq(v.expectRevert, "none");
        if (expectsRevert) vm.expectRevert(_selectorFor(v.expectRevert));

        if (_eq(v.kind, "amountOutBid")) {
            uint256 got = h.outBid(v.amountIn, v.minBid, v.maxBid, v.capacity, v.used, uint8(v.priceScaleExp));
            if (!expectsRevert) assertEq(got, v.expected, label);
        } else if (_eq(v.kind, "amountOutAsk")) {

            uint256 got = h.outAsk(v.amountIn, v.minAsk, v.maxAsk, v.capacity, v.used, uint8(v.priceScaleExp));
            if (!expectsRevert) assertEq(got, v.expected, label);
        } else if (_eq(v.kind, "amountInBid")) {

            uint256 got = h.inBid(v.amountIn, v.minBid, v.maxBid, v.capacity, v.used, uint8(v.priceScaleExp));
            if (!expectsRevert) assertEq(got, v.expected, label);
        } else if (_eq(v.kind, "amountInAsk")) {

            uint256 got = h.inAsk(v.amountIn, v.minAsk, v.maxAsk, v.capacity, v.used, uint8(v.priceScaleExp));
            if (!expectsRevert) assertEq(got, v.expected, label);
        } else if (_eq(v.kind, "validateLadder")) {
            h.validate(v.minBid, v.maxBid, v.minAsk, v.maxAsk, v.minPrice);
        } else if (_eq(v.kind, "executableTopBid")) {
            uint256 got = h.topBid(v.minBid, v.maxBid, v.capacity, v.used);
            if (!expectsRevert) assertEq(got, v.expected, label);
        } else {
            uint256 got = h.topAsk(v.minAsk, v.maxAsk, v.capacity, v.used);
            if (!expectsRevert) assertEq(got, v.expected, label);
        }
        return true;
    }

    function _dispatchable(string memory kind) internal pure returns (bool) {
        return _eq(kind, "amountOutBid") || _eq(kind, "amountOutAsk") || _eq(kind, "amountInBid")
            || _eq(kind, "amountInAsk") || _eq(kind, "validateLadder") || _eq(kind, "executableTopBid")
            || _eq(kind, "executableTopAsk");
    }

    function _selectorFor(string memory name) internal pure returns (bytes4) {
        if (_contains(name, "AmountExceedsCapacity")) return PropCurve.AmountExceedsCapacity.selector;
        if (_contains(name, "AmountOutOfDomain")) return PropCurve.AmountOutOfDomain.selector;
        if (_contains(name, "ZeroCapacity")) return PropCurve.ZeroCapacity.selector;
        if (_contains(name, "ZeroPrice")) return PropCurve.ZeroPrice.selector;
        if (_contains(name, "CrossedBook")) return PropCurve.CrossedBook.selector;
        if (_contains(name, "BidBelowMinPrice")) return PropCurve.BidBelowMinPrice.selector;
        revert(string.concat("unknown expected error in vectors: ", name));
    }

    function _keysKind() internal pure returns (string[4] memory) {
        return ["kind", "fn", "side", "function"];
    }

    function _keysName() internal pure returns (string[4] memory) {
        return ["name", "label", "id", "case"];
    }

    function _keysRevert() internal pure returns (string[6] memory) {
        return ["expectRevert", "revert", "expectedError", "error", "expected_revert", "revertReason"];
    }

    function _keysAmountIn() internal pure returns (string[3] memory) {
        return ["amountIn", "amount_in", "input"];
    }

    function _keysCapacity() internal pure returns (string[3] memory) {
        return ["capacity", "cap", "capacityTotal"];
    }

    function _keysUsed() internal pure returns (string[3] memory) {
        return ["used", "usage", "usedAmount"];
    }

    function _keysExp() internal pure returns (string[4] memory) {
        return ["priceScaleExp", "price_scale_exp", "exp", "scaleExp"];
    }

    function _keysMinBid() internal pure returns (string[3] memory) {
        return ["minBid", "min_bid", "pLowBid"];
    }

    function _keysMaxBid() internal pure returns (string[3] memory) {
        return ["maxBid", "max_bid", "pHighBid"];
    }

    function _keysMinAsk() internal pure returns (string[3] memory) {
        return ["minAsk", "min_ask", "pLowAsk"];
    }

    function _keysMaxAsk() internal pure returns (string[3] memory) {
        return ["maxAsk", "max_ask", "pHighAsk"];
    }

    function _keysMinPrice() internal pure returns (string[3] memory) {
        return ["minPrice", "min_price", "floor"];
    }

    function _keysExpected() internal pure returns (string[4] memory) {
        return ["expected", "expectedOut", "amountOut", "out"];
    }

    function _uOne(string memory json, string memory p, string memory key) private view returns (bool, uint256) {
        string memory path = string.concat(p, ".", key);
        try vm.parseJsonString(json, path) returns (string memory s) {
            if (bytes(s).length != 0) return (true, vm.parseUint(s));
        } catch {}
        try vm.parseJsonUint(json, path) returns (uint256 v) {
            return (true, v);
        } catch {}
        return (false, 0);
    }

    function _u(string memory json, string memory p, string[3] memory keys) private view returns (bool, uint256) {
        for (uint256 i; i < keys.length; ++i) {
            (bool ok, uint256 v) = _uOne(json, p, keys[i]);
            if (ok) return (true, v);
        }
        return (false, 0);
    }

    function _u(string memory json, string memory p, string[4] memory keys) private view returns (bool, uint256) {
        for (uint256 i; i < keys.length; ++i) {
            (bool ok, uint256 v) = _uOne(json, p, keys[i]);
            if (ok) return (true, v);
        }
        return (false, 0);
    }

    function _s(string memory json, string memory p, string[4] memory keys) private view returns (bool, string memory) {
        for (uint256 i; i < keys.length; ++i) {
            try vm.parseJsonString(json, string.concat(p, ".", keys[i])) returns (string memory v) {
                return (true, v);
            } catch {}
        }
        return (false, "");
    }

    function _s(string memory json, string memory p, string[6] memory keys) private view returns (bool, string memory) {
        for (uint256 i; i < keys.length; ++i) {
            try vm.parseJsonString(json, string.concat(p, ".", keys[i])) returns (string memory v) {
                return (true, v);
            } catch {}
        }
        return (false, "");
    }

    function _eq(string memory a, string memory b) private pure returns (bool) {
        return keccak256(bytes(a)) == keccak256(bytes(b));
    }

    function _contains(string memory haystack, string memory needle) private pure returns (bool) {
        bytes memory hb = bytes(haystack);
        bytes memory nb = bytes(needle);
        if (nb.length == 0 || nb.length > hb.length) return false;
        for (uint256 i; i + nb.length <= hb.length; ++i) {
            bool hit = true;
            for (uint256 j; j < nb.length; ++j) {
                if (hb[i + j] != nb[j]) {
                    hit = false;
                    break;
                }
            }
            if (hit) return true;
        }
        return false;
    }

    function _boundLadder(uint256 pLow, uint256 width, uint256 cap, uint256 used, uint8 exp)
        internal
        pure
        returns (uint256, uint256, uint256, uint256, uint8)
    {
        exp = uint8(bound(exp, 0, PropCurve.MAX_PRICE_SCALE_EXP));
        pLow = bound(pLow, 1, U56);
        width = bound(width, 0, U56 - pLow);
        cap = bound(cap, 2, type(uint96).max);
        used = bound(used, 0, cap - 1);
        return (pLow, width, cap, used, exp);
    }

    function _tryBid(uint256 amountIn, uint256 minBid, uint256 maxBid, uint256 cap, uint256 used, uint8 exp)
        internal
        view
        returns (bool, uint256)
    {
        try h.outBid(amountIn, minBid, maxBid, cap, used, exp) returns (uint256 out) {
            return (true, out);
        } catch {
            return (false, 0);
        }
    }

    function _tryAsk(uint256 amountIn, uint256 minAsk, uint256 maxAsk, uint256 cap, uint256 used, uint8 exp)
        internal
        view
        returns (bool, uint256)
    {
        try h.outAsk(amountIn, minAsk, maxAsk, cap, used, exp) returns (uint256 out) {
            return (true, out);
        } catch {
            return (false, 0);
        }
    }

    function _splitAsk(uint256 amountIn, uint256 minAsk, uint256 maxAsk, uint256 cap, uint8 exp, uint256 n)
        internal
        view
        returns (uint256 sum)
    {
        uint256 per = amountIn / n;
        uint256 u;
        for (uint256 i; i < n; ++i) {
            uint256 amt = i + 1 == n ? amountIn - per * (n - 1) : per;
            uint256 got = h.outAsk(amt, minAsk, maxAsk, cap, u, exp);
            sum += got;
            u += got;
        }
    }

    function _splitAskCost(uint256 amountOut, uint256 minAsk, uint256 maxAsk, uint256 cap, uint8 exp, uint256 n)
        internal
        view
        returns (uint256 sum)
    {
        uint256 per = amountOut / n;
        uint256 u;
        for (uint256 i; i < n; ++i) {
            uint256 amt = i + 1 == n ? amountOut - per * (n - 1) : per;
            sum += h.inAsk(amt, minAsk, maxAsk, cap, u, exp);
            u += amt;
        }
    }

    struct Walk {
        uint256 amountIn;
        uint256 pLow;
        uint256 pHigh;
        uint256 cap;
        uint256 used;
        uint256 n;
        uint8 exp;
    }

    function _splitBid(Walk memory w) internal view returns (bool, uint256) {
        uint256 sum;
        uint256 u = w.used;
        uint256 per = w.amountIn / w.n;
        for (uint256 i; i < w.n; ++i) {
            uint256 amt = i + 1 == w.n ? w.amountIn - per * (w.n - 1) : per;
            (bool ok, uint256 o) = _tryBid(amt, w.pLow, w.pHigh, w.cap, u, w.exp);
            if (!ok) return (false, 0);
            sum += o;
            u += amt;
        }
        return (true, sum);
    }

    function _splitAskSafe(Walk memory w) internal view returns (bool, uint256) {
        uint256 sum;
        uint256 u = w.used;
        uint256 per = w.amountIn / w.n;
        for (uint256 i; i < w.n; ++i) {
            uint256 amt = i + 1 == w.n ? w.amountIn - per * (w.n - 1) : per;
            (bool ok, uint256 o) = _tryAsk(amt, w.pLow, w.pHigh, w.cap, u, w.exp);
            if (!ok) return (false, 0);
            sum += o;
            u += o;
        }
        return (true, sum);
    }

    function _splitAskCostWalk(Walk memory w) internal view returns (bool, uint256) {
        uint256 sum;
        uint256 u = w.used;
        uint256 per = w.amountIn / w.n;
        for (uint256 i; i < w.n; ++i) {
            uint256 amt = i + 1 == w.n ? w.amountIn - per * (w.n - 1) : per;
            (bool ok, uint256 c) = _tryAskCost(amt, w.pLow, w.pHigh, w.cap, u, w.exp);
            if (!ok) return (false, 0);
            sum += c;
            u += amt;
        }
        return (true, sum);
    }

    function _tryAskCost(uint256 amountOut, uint256 minAsk, uint256 maxAsk, uint256 cap, uint256 used, uint8 exp)
        internal
        view
        returns (bool, uint256)
    {
        try h.inAsk(amountOut, minAsk, maxAsk, cap, used, exp) returns (uint256 cost) {
            return (true, cost);
        } catch {
            return (false, 0);
        }
    }
}
