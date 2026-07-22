// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {Test} from "forge-std/Test.sol";
import {console2} from "forge-std/console2.sol";
import {PropCurve} from "../src/libraries/PropCurve.sol";

/// @title PropCurveHarness
/// @notice External wrapper around the internal library.
/// @dev `vm.expectRevert` latches onto the next CALL, and an internal library call is not a call.
///      Every revert assertion in this file therefore goes through this contract. It also gives
///      the fuzz tests a cheap `try/catch` boundary for the two domain reverts, which is what lets
///      an invariant be asserted only over inputs where the curve is actually defined.
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

    /// @dev The two exact-output primitives. Amendment 1 made the ask side's *natural* direction
    ///      base-out / quote-in, so `inAsk` is the ask primitive and `outAsk` its inversion; on the
    ///      bid side it is the other way round. All four are needed here: the differential fixture
    ///      carries vectors for each, and several properties are only expressible in the
    ///      exact-output direction.
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

/// @title PropCurveTest
/// @notice Unit, property and differential tests for the DuBu curve math.
///
/// ## The `test_BUG*` tests are regression witnesses, not open defects
///
/// Four tests are named `test_BUG*`. Each was a deterministic witness for a property the library's
/// doc comments claimed and the code did not deliver, and each is now GREEN because amendments 1
/// and 2 closed the defect it witnessed. They are kept, with their assertions turned the right way
/// round, because a witness that passes is the cheapest possible regression test: it fails again
/// the moment the specific arithmetic that caused the defect comes back. See the block comment
/// above each for what was wrong and what fixed it. Nothing in this file should be red.
///
/// The companion fuzz tests assert the strict properties the amendments made true, rather than the
/// tight characterisations of the violations they used to pin.
contract PropCurveTest is Test {
    PropCurveHarness internal h;

    uint256 internal constant U56 = type(uint56).max; // 72_057_594_037_927_935
    uint256 internal constant MAX_OUT = type(uint128).max;
    uint256 internal constant U96 = type(uint96).max; // 79_228_162_514_264_337_593_543_950_335

    /// @notice A flat ladder price that divides `MAX_AMOUNT_OUT` exactly, with the matching size.
    ///
    /// @dev The domain-boundary tests need `amountOutBid` to land *on* `type(uint128).max`, and
    ///      under the amended arithmetic the only lever is `q * price` — the price is no longer
    ///      divided out early, and capacity is a `uint96` in the domain PropPool narrows to, so a
    ///      unit price cannot reach the ceiling (`q <= 2**96 - 1 < 2**128 - 1`).
    ///
    ///      `2**128 - 1 == (2**64 - 1) * 274177 * 67280421310721`, so `DOMAIN_PRICE` fits `uint56`,
    ///      `DOMAIN_SIZE` fits `uint96`, and their product is `MAX_AMOUNT_OUT` on the nose.
    uint256 internal constant DOMAIN_PRICE = 67_280_421_310_721;
    uint256 internal constant DOMAIN_SIZE = 5_057_672_949_897_463_733_145_855;

    function setUp() public {
        h = new PropCurveHarness();
    }

    // =====================================================================
    // Units — zero amounts
    // =====================================================================

    function test_ZeroAmountIn_ReturnsZero() public view {
        assertEq(h.outBid(0, 100, 200, 1000, 0, 0), 0, "bid zero in");
        assertEq(h.outAsk(0, 100, 200, 1000, 0, 0), 0, "ask zero in");
    }

    /// @dev The zero-amount short circuit is ordered *before* the capacity check, so a zero-amount
    ///      quote on a pair with no capacity epoch must still be a quiet zero and not a revert.
    ///      PropPool's view path depends on exactly this ordering.
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

    // =====================================================================
    // Units — capacity boundary
    // =====================================================================

    /// @dev Capacity bounds the BASE leg on both sides now (amendment 1), so "exact capacity" is
    ///      `used + amountIn == capacity` for a bid and `used + amountOut == capacity` for an ask.
    ///      The ask's quote-denominated entry point reaches the same boundary through the epoch's
    ///      quote *ceiling*, which is the cost of all its remaining base — `ASK_CEILING` below.
    function test_ExactCapacityBoundary_Succeeds() public view {
        // Single rounding: floor(q * (2*maxBid*C - W*(2u + q)) / (2*C*S))
        //               = floor(600 * (2*200*1000 - 100*(800 + 600)) / (2*1000*1))
        //               = floor(600 * 260000 / 2000) = 600 * 130.
        assertEq(h.outBid(600, 100, 200, 1000, 400, 0), 600 * 130, "bid at exact capacity");

        // ceil(q * (2*minAsk*C + W*(2u + q)) / (2*C*S))
        //   = ceil(600 * (2*100*1000 + 100*(800 + 600)) / 2000) = 600 * 170 = 102000, exactly.
        uint256 askCeiling = 102_000;
        assertEq(h.inAsk(600, 100, 200, 1000, 400, 0), askCeiling, "ask cost of the whole epoch");
        // Spending exactly that buys exactly the epoch's remaining base.
        assertEq(h.outAsk(askCeiling, 100, 200, 1000, 400, 0), uint256(600), "ask at exact capacity");
    }

    function test_OneUnitPastCapacity_Reverts() public {
        vm.expectRevert(PropCurve.AmountExceedsCapacity.selector);
        h.outBid(601, 100, 200, 1000, 400, 0);

        // Base-denominated: one base unit past the epoch's remaining room.
        vm.expectRevert(PropCurve.AmountExceedsCapacity.selector);
        h.inAsk(601, 100, 200, 1000, 400, 0);

        // Quote-denominated: one quote unit past the epoch's quote ceiling. `601` is NOT past it
        // any more — it buys 4 base — which is exactly what amendment 1 changed.
        vm.expectRevert(PropCurve.AmountExceedsCapacity.selector);
        h.outAsk(102_001, 100, 200, 1000, 400, 0);
    }

    /// @dev `used == capacity` is the exhausted epoch. Any non-zero amount must be rejected;
    ///      it must not silently price at the ladder floor.
    function test_ExhaustedEpoch_Reverts() public {
        vm.expectRevert(PropCurve.AmountExceedsCapacity.selector);
        h.outBid(1, 100, 200, 1000, 1000, 0);

        vm.expectRevert(PropCurve.AmountExceedsCapacity.selector);
        h.outAsk(1, 100, 200, 1000, 1000, 0);
    }

    /// @dev The last unit of an epoch is priced at (or worse than) the ladder's worst rung, never
    ///      better. Both sides are asserted through the direction whose leg is the single base
    ///      unit, which is the only way to see a per-unit price at all: for the ask that is
    ///      `amountInAsk` (base out, quote cost), because the quote-denominated entry point takes
    ///      an amount, not a size.
    function test_LastUnitOfEpoch_PricedAtFloor() public view {
        // floor(1 * (2*200*1000 - 100*(2*999 + 1)) / (2*1000)) = floor(200100/2000) = 100 = minBid.
        assertEq(h.outBid(1, 100, 200, 1000, 999, 0), 100, "bid floor");
        // ceil(1 * (2*100*1000 + 100*(2*999 + 1)) / (2*1000)) = ceil(399900/2000) = 200 = maxAsk.
        // The exact rational cost is 199.95; the single ceil lands it on the ladder ceiling.
        assertEq(h.inAsk(1, 100, 200, 1000, 999, 0), 200, "ask ceiling");
    }

    // =====================================================================
    // Units — priceScaleExp across its whole declared range
    // =====================================================================

    /// @dev Capacity is constrained to `uint96` here, and that is not a convenience: the amended
    ///      numerators multiply capacity by a price *before* dividing (`2*maxBid*C`,
    ///      `2*C*10**exp`), so a `type(uint256).max` capacity — which the pre-amendment code
    ///      tolerated because it divided the price out early — now overflows the intermediate.
    ///      `uint96` is the domain `IPropPool.PairSnapshot` declares and `PropPool` narrows to, and
    ///      the library's own overflow table is derived against exactly that, so it is the only
    ///      capacity the library claims to be total on.
    function test_PriceScaleExp_FullRange() public view {
        uint256 amountIn = 1e24; // <= U96, so it is a size the epoch could actually hold
        uint256 cap = U96;
        for (uint8 e; e <= PropCurve.MAX_PRICE_SCALE_EXP; ++e) {
            // Flat ladder so the exact average is maxBid == 10**e, hence out == amountIn for every
            // e. Only valid while 10**e fits a uint56 (e <= 16).
            if (e <= 16) {
                uint256 price = 10 ** e;
                assertEq(h.outBid(amountIn, price, price, cap, 0, e), amountIn, "bid identity");
            }
            // Unit price on the ask side: one quote unit buys `10**e` base, because
            // `cost(q) = ceil(q / 10**e)` and the largest affordable size is `10**e` exactly.
            // Once `10**e` passes the epoch's room the answer is the room, which is the ask side's
            // honest ceiling now that its output is base-denominated.
            uint256 expected = (10 ** uint256(e)) < cap ? (10 ** uint256(e)) : cap;
            assertEq(h.outAsk(1, 1, 1, cap, 0, e), expected, "ask identity");
        }
    }

    function test_MaxPriceScaleExp_NoOverflow() public {
        assertEq(PropCurve.MAX_PRICE_SCALE_EXP, 38, "exp ceiling moved");
        uint8 e = PropCurve.MAX_PRICE_SCALE_EXP;

        // Largest reachable bid input: uint96 capacity, uint56 prices. 2**96 * 2**56 = 2**152,
        // nowhere near uint256, and the division by 10**38 brings it back under the out ceiling.
        uint256 cap = U96;
        uint256 out = h.outBid(cap, U56, U56, cap, 0, e);
        assertEq(out, (cap * U56) / (10 ** e), "bid at exp 38");
        assertLe(out, MAX_OUT, "bid at exp 38 in domain");

        // The other quote-denominated amount `MAX_AMOUNT_OUT` bounds is `amountInAsk`'s return, and
        // at exp 38 it is just as far inside: the exponent IS the headroom. Nothing the uint96 /
        // uint56 corner can express leaves the domain at this exponent, which is the honest
        // statement of "no overflow" — the pre-amendment version reached `AmountOutOfDomain` here
        // only because the ask side's output was quote-denominated and got multiplied by 10**38.
        uint256 cost = h.inAsk(cap, U56, U56, cap, 0, e);
        assertLe(cost, MAX_OUT, "ask cost at exp 38 in domain");

        // What the guard still catches at exp 38 is a quote-denominated *argument*. `amountOutAsk`
        // seeds its bracket with `amountIn * 10**38`, which is under 2**255 only because the input
        // is capped at `MAX_AMOUNT_OUT` first, so the check has to precede everything else.
        vm.expectRevert(PropCurve.AmountOutOfDomain.selector);
        h.outAsk(MAX_OUT + 1, 1, 1, cap, 0, e);
    }

    /// @dev A price scale above the declared maximum is not rejected by the library — the caller
    ///      (PropPool.addPair) owns that check. This pins the split of responsibility so that
    ///      moving the check does not silently leave a gap. `2 * U96 * 10**41 < 2**256`, so the
    ///      denominator still fits and the answer is a quiet zero rather than a panic.
    function test_PriceScaleExp_AboveMax_NotLibraryEnforced() public view {
        assertEq(h.outBid(1e24, 1, 1, U96, 0, 41), uint256(0), "no library check");
    }

    // =====================================================================
    // Units — asymmetric decimals
    // =====================================================================

    /// @dev 18-decimal base / 6-decimal quote (WETH/USDC shape) at 3000 quote per base.
    ///      quote = base * price / 10**18 with price = 3e9.
    function test_AsymmetricDecimals_18_6() public view {
        uint256 price = 3e9;
        uint256 cap = 30e18;
        // Flat ladder: 1 WETH must come out as exactly 3000.000000 USDC.
        assertEq(h.outBid(1e18, price, price, cap, 0, 18), 3000e6, "18/6 bid");
        // Ask: 3000 USDC in must come out as exactly 1 WETH. Ask capacity is BASE (amendment 1), so
        // the epoch is 30 WETH of sellable base — ~90k USDC at this mid — not "100_000e6 of quote".
        assertEq(h.outAsk(3000e6, price, price, cap, 0, 18), 1e18, "18/6 ask");
        // And the same trade from the base side costs exactly 3000 USDC, with nothing to round.
        assertEq(h.inAsk(1e18, price, price, cap, 0, 18), 3000e6, "18/6 ask cost");
    }

    /// @dev 8-decimal base / 6-decimal quote (WBTC/USDC shape) at 60000 quote per base.
    function test_AsymmetricDecimals_8_6() public view {
        uint256 price = 6e10; // 60000e6 * 1e8 / 1e8
        assertEq(h.outBid(1e8, price, price, 100e8, 0, 8), 60_000e6, "8/6 bid");
        assertEq(h.outAsk(60_000e6, price, price, 10_000_000e6, 0, 8), 1e8, "8/6 ask");
    }

    /// @dev 6-decimal base / 18-decimal quote — the inverted direction, where the scale has to
    ///      make a small integer price mean a very large ratio.
    function test_AsymmetricDecimals_6_18() public view {
        // 1 USDC (1e6) = 0.00025 WETH (2.5e14 wei). price / 10**exp = 2.5e14 / 1e6 = 2.5e8.
        uint256 price = 2.5e14;
        assertEq(h.outBid(1e6, price, price, 1_000_000e6, 0, 6), 2.5e14, "6/18 bid");
        assertEq(h.outAsk(2.5e14, price, price, 100e18, 0, 6), 1e6, "6/18 ask");
    }

    // =====================================================================
    // Units — extreme prices, flat ladder
    // =====================================================================

    /// @dev Also the sharpest single witness for amendment 2. On a ladder spanning the whole uint56
    ///      range the exact average price over `[0, 1e6]` is
    ///      `U56 - (U56 - 1) * 5e5 / 1e24 = 72057594037927934.96397...`, i.e. the impact term is
    ///      *four* orders of magnitude smaller than one price unit. The pre-amendment code
    ///      quantised that term with `ceil`, charging a whole price unit of impact and returning
    ///      `1e6 * (U56 - 1) = 72057594037927934000000`. Folding the price into the amount and
    ///      rounding once returns `72057594037927934963971` — 963971 quote units more, on a 1e6
    ///      trade, purely from not rounding the price.
    function test_MaxUint56Prices() public view {
        uint256 cap = 1e24;
        assertEq(h.outBid(1e6, 1, U56, cap, 0, 0), 72_057_594_037_927_934_963_971, "max-width bid");
        // What the same call returned while the discount was quantised to a whole price unit.
        assertEq(1e6 * (U56 - 1), 72_057_594_037_927_934_000_000, "pre-amendment value moved");

        // Flat at the uint56 ceiling: the taker's output is one unit per 10**exp of input scale.
        assertEq(h.outBid(1e6, U56, U56, cap, 0, 0), 1e6 * U56, "flat max bid");
        assertEq(h.outAsk(U56, U56, U56, U96, 0, 0), 1, "flat max ask");
    }

    /// @dev minBid == maxBid degenerates the ladder to a constant price: the discount term must be
    ///      exactly zero at *every* usage level, including the last unit, and the quote must be
    ///      independent of `used`. This is the configuration a zero-impact pair would use, and a
    ///      stray `+ capacity - 1` in the ceiling would show up here as a phantom one-tick charge.
    function test_FlatLadder_ConstantPrice() public view {
        uint256 cap = 1_000_000;
        uint256 price = 12_345_678;
        uint256[6] memory usages = [uint256(0), 1, 2, cap / 2, cap - 2, cap - 1];
        for (uint256 i; i < usages.length; ++i) {
            uint256 u = usages[i];
            assertEq(h.outBid(1, price, price, cap, u, 0), price, "flat bid constant");
            assertEq(h.outAsk(price, price, price, cap * price, u, 0), 1, "flat ask constant");
        }
        // And the whole epoch at once prices identically to any slice of it.
        assertEq(h.outBid(cap, price, price, cap, 0, 0), cap * price, "flat bid whole epoch");
    }

    /// @dev Zero prices on the ask side are the one place the library can divide by zero, so it
    ///      has its own error. Reachable only if the caller skipped `validateLadder`/`minPrice`.
    function test_ZeroPrice_Ask() public {
        vm.expectRevert(PropCurve.ZeroPrice.selector);
        h.outAsk(1, 0, 0, 1000, 0, 0);
    }

    // =====================================================================
    // Units — the AmountOutOfDomain boundary, from both sides
    // =====================================================================

    /// @dev A unit price no longer reaches the ceiling: the quote leg is `q * price / 10**exp` with
    ///      `q <= type(uint96).max`, so hitting `type(uint128).max` needs a price, and hitting it
    ///      *exactly* needs one that divides it. See `DOMAIN_PRICE`.
    function test_AmountOutOfDomain_Bid_JustUnder() public view {
        assertEq(PropCurve.MAX_AMOUNT_OUT, MAX_OUT, "domain ceiling moved");
        assertEq(DOMAIN_SIZE * DOMAIN_PRICE, MAX_OUT, "domain fixture no longer lands on the ceiling");
        assertLe(DOMAIN_PRICE, U56, "domain price outside the uint56 field");
        assertLe(DOMAIN_SIZE, U96, "domain size outside the uint96 field");

        // Flat ladder at exp 0: amountOut == q * price exactly, with no rounding at all.
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

    /// @dev On the ask side `MAX_AMOUNT_OUT` bounds the quote-denominated *argument* of
    ///      `amountOutAsk` and the quote-denominated *return* of `amountInAsk`. Both are pinned.
    function test_AmountOutOfDomain_Ask_JustUnder() public view {
        // Argument exactly at the ceiling: affordable, and it buys DOMAIN_SIZE base.
        assertEq(h.outAsk(MAX_OUT, DOMAIN_PRICE, DOMAIN_PRICE, U96, 0, 0), DOMAIN_SIZE, "exactly at ceiling");
        assertEq(h.outAsk(MAX_OUT - 1, DOMAIN_PRICE, DOMAIN_PRICE, U96, 0, 0), DOMAIN_SIZE - 1, "one under the ceiling");
        // Return exactly at the ceiling.
        assertEq(h.inAsk(DOMAIN_SIZE, DOMAIN_PRICE, DOMAIN_PRICE, U96, 0, 0), MAX_OUT, "cost exactly at ceiling");
    }

    function test_AmountOutOfDomain_Ask_JustOver() public {
        vm.expectRevert(PropCurve.AmountOutOfDomain.selector);
        h.outAsk(MAX_OUT + 1, DOMAIN_PRICE, DOMAIN_PRICE, U96, 0, 0);

        // And the cost side: one more base unit costs one DOMAIN_PRICE more than the ceiling.
        vm.expectRevert(PropCurve.AmountOutOfDomain.selector);
        h.inAsk(DOMAIN_SIZE + 1, DOMAIN_PRICE, DOMAIN_PRICE, U96, 0, 0);
    }

    /// @dev The boundary at the largest price scale, which is the shape the u128 mirror argument in
    ///      the doc comment is actually about: `amountOutAsk` multiplies its quote argument by
    ///      `10**priceScaleExp` to seed the bracket, and `MAX_AMOUNT_OUT * 10**38 < 2**255` is what
    ///      keeps that inside a word. So the domain check must run BEFORE the capacity probe, and
    ///      this pins that ordering — at exp 38 the same amount fails both tests, and the one it
    ///      reports is the domain one.
    function test_AmountOutOfDomain_ViaScale() public {
        vm.expectRevert(PropCurve.AmountOutOfDomain.selector);
        h.outAsk(MAX_OUT + 1, 1, 1, U96, 0, 38);

        // Exactly at the bound the domain check passes and the honest capacity error surfaces
        // instead: at exp 38 a unit ladder prices the whole epoch at one quote unit.
        assertEq(h.inAsk(U96, 1, 1, U96, 0, 38), uint256(1), "epoch costs one quote unit at exp 38");
        vm.expectRevert(PropCurve.AmountExceedsCapacity.selector);
        h.outAsk(MAX_OUT, 1, 1, U96, 0, 38);
    }

    // =====================================================================
    // Units — executable top
    // =====================================================================

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

    // =====================================================================
    // validateLadder
    // =====================================================================

    function test_ValidateLadder_AcceptsDocumentedOrdering() public view {
        h.validate(100, 110, 120, 130, 100); // strict interior
        h.validate(100, 100, 100, 101, 100); // everything collapsed except the strict top
        h.validate(100, 110, 110, 110, 1); // minAsk == maxBid == maxAsk
        h.validate(1, 1, 1, U56, 1); // widest legal book
    }

    function test_ValidateLadder_RejectsEveryInversion() public {
        // minBid below the pair floor
        vm.expectRevert(PropCurve.BidBelowMinPrice.selector);
        h.validate(99, 110, 120, 130, 100);

        // maxBid < minBid
        vm.expectRevert(PropCurve.CrossedBook.selector);
        h.validate(110, 100, 120, 130, 1);

        // minAsk < maxBid  (crossed book)
        vm.expectRevert(PropCurve.CrossedBook.selector);
        h.validate(100, 120, 110, 130, 1);

        // maxAsk < minAsk
        vm.expectRevert(PropCurve.CrossedBook.selector);
        h.validate(100, 110, 130, 120, 1);

        // fully reversed
        vm.expectRevert(PropCurve.CrossedBook.selector);
        h.validate(130, 120, 110, 100, 1);
    }

    /// @dev The one non-obvious rule: `maxAsk > minBid` is STRICT, so a completely flat book with
    ///      zero spread is rejected even though it satisfies every non-strict comparison.
    function test_ValidateLadder_RejectsZeroSpreadBook() public {
        vm.expectRevert(PropCurve.CrossedBook.selector);
        h.validate(100, 100, 100, 100, 100);
    }

    /// @dev BidBelowMinPrice is checked first, so a ladder that is both crossed and under the
    ///      floor reports the floor. Pinned because operators triage off the error selector.
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

    // =====================================================================
    // Fuzz — monotonicity
    // =====================================================================

    /**
     * BUG 3 — CLOSED by amendment 2. Kept as a regression witness; the assertion never changed,
     * only its outcome.
     *
     * What was wrong: `midUsage = used + ceil(amountIn/2)` stepped up by one every second unit of
     * `amountIn`; when that step pushed `width * midUsage / capacity` across an integer boundary the
     * CEILING on `discount` jumped a whole price tick, and that tick was then multiplied by the
     * *entire* `amountIn`. One extra unit of input bought `avgBid/scale` more and lost
     * `amountIn/scale` — so once `amountIn > scale` the loss won:
     *
     *   swap 2.000000000000000000 WETH -> 5998.000000 USDC
     *   swap 2.000000000000000001 WETH -> 5997.999998 USDC   (2 units LESS for 1 wei MORE)
     *
     * on a WETH/USDC-shaped pair with a 100 bps ladder and 30 WETH of bid capacity — not a corner
     * case. It also broke `PropPool.getAmountIn`, whose binary search named monotonicity as its
     * precondition, in three ways: non-minimal answers, self-contradicting views, and a fillability
     * probe that read the right edge of a dipping curve as its maximum.
     *
     * What fixed it: rounding once, on the amount, never on the price. `amountOut` is now
     * `floor(q * (2*maxBid*C - W*(2u + q)) / (2*C*S))`, whose numerator is a downward parabola with
     * its vertex at or beyond `C - u` for every ladder (the proof reduces to `minBid >= 0`), so
     * monotonicity holds on the whole domain with no side condition. PropPool's own bisection was
     * deleted as a consequence — `amountInBid` / `amountInAsk` are the exact-output primitives now.
     */
    function test_BUG3_AmountOutNotMonotonicInAmountIn_Bid() public view {
        uint256 maxBid = 3e9;
        uint256 minBid = maxBid - (maxBid * 100) / 10_000; // 100 bps ladder
        uint256 cap = 30e18;

        uint256 lower = h.outBid(2e18, minBid, maxBid, cap, 0, 18);
        uint256 higher = h.outBid(2e18 + 1, minBid, maxBid, cap, 0, 18);

        console2.log("out(2.000000000000000000 WETH) =", lower);
        console2.log("out(2.000000000000000001 WETH) =", higher);

        assertGe(higher, lower, "BUG3 regressed: one extra wei of input bought a smaller output");
    }

    /// @dev The same witness on the ask side. Small-integer parameters because the pre-amendment ask
    ///      output was `amountIn * scale / avgAsk`, so a one-tick jump in `avgAsk` bit hardest when
    ///      `avgAsk` was small relative to the ladder width. `amountOutAsk` is now the exact
    ///      inversion of a strictly increasing function, so it is monotone by construction.
    function test_BUG3_AmountOutNotMonotonicInAmountIn_Ask() public view {
        uint256 lower = h.outAsk(180, 29, 63, 297, 49, 1);
        uint256 higher = h.outAsk(181, 29, 63, 297, 49, 1);
        console2.log("ask out(180) =", lower);
        console2.log("ask out(181) =", higher);
        assertGe(higher, lower, "BUG3 regressed: one extra unit of input bought a smaller output");
    }

    /// @notice The property that DOES hold: the curve never returns more than the taker would get
    ///         at the best price in the interval, at any size. Monotonicity's strict form is
    ///         covered by `test_BUG3_*`.
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

        // Against the EXACT rational zero-size top there is no slack at all. Cross-multiplied by
        // `cap` so the top stays rational: `out * S * C <= q * (maxBid*C - W*u)`. Bounds:
        // `out * S <= q * avg <= 2**152` and `q * maxBid * C <= 2**248`.
        assertLe(out * (10 ** exp) * cap, amountIn * (maxBid * cap - width * used), "bid out above the exact top");

        // Against the REPORTED top there is exactly one price unit of slack per unit of size, and
        // it points the safe way: `executableTopBid` ceils the drift, i.e. floors the price it
        // reports, deliberately (see its doc comment), so the realised leg can sit just above
        // `amountIn * top`. Strictly less than one unit of price, hence strictly less than
        // `amountIn` of quote.
        uint256 top = h.topBid(minBid, maxBid, cap, used);
        assertLt(out * (10 ** exp), amountIn * (top + 1), "bid out above the reported top of book");
    }

    /// @notice Monotonicity, now unconditional. Amendment 2 removed the side condition: the price is
    ///         no longer quantised, so there are no tick-crossing points to exclude, and the bid
    ///         numerator's vertex is proved to lie at or beyond `C - u` for every ladder
    ///         `validateLadder` admits.
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

    // =====================================================================
    // Fuzz — realised price never better than the executable top
    // =====================================================================

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
        // realised avg = out * scale / amountIn, and it never beats the executable top by as much
        // as one price unit. The one unit of slack is `executableTopBid`'s own rounding: it ceils
        // the drift, so the price it reports is the exact zero-size price FLOORED, and the exact
        // price is the true bound. `testFuzz_AmountOut_NeverExceedsTopOfBook` asserts the exact,
        // slack-free form; this one is about the reported number a quoter triggers on.
        assertLt(out * (10 ** exp), (top + 1) * amountIn, "realised bid better than executable top");
        // and never below the ladder floor by more than the final flooring
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
        // realised avg price paid = amountIn * scale / out, never better than the executable top by
        // as much as one price unit. Same one unit of slack, same cause, mirrored:
        // `executableTopAsk` ceils the drift too, so the ask it reports is the exact zero-size ask
        // rounded UP — away from the taker on this side — and `top * out` can therefore sit just
        // above the quote actually collected.
        assertLt(top * out, amountIn * (10 ** exp) + out, "realised ask better than executable top");
    }

    // =====================================================================
    // Fuzz — splitting never beats one shot
    // =====================================================================

    /**
     * BUG 1 — CLOSED by amendment 1. Kept as a regression witness, with the assertion inverted.
     *
     * What was wrong: ask capacity was denominated in the QUOTE token, so the ask side's output was
     * `amountIn * scale / p_mid` — the RECIPROCAL of the midpoint price. For A = p_mid(whole trade)
     * and d = half the price move of each half-trade:
     *
     *     one shot   : x / A
     *     two halves : (x/2)/(A-d) + (x/2)/(A+d) = x * A / (A^2 - d^2)   >   x / A
     *
     * strictly, for every d != 0. `1/p` is convex, the midpoint rule under-integrates a convex
     * function, and finer subdivisions integrate it better, so splitting an ask was STRICTLY
     * DOMINANT for the taker — 0.0033 bps of free fill at a 20 bps ladder, 7.57 bps at 1000 bps,
     * converging upward on the true log integral as the trade was cut finer. That is the opposite
     * of the design intent, and the pool's own router would have found it automatically while
     * equalising marginal prices across split routes.
     *
     * What fixed it: denominating ask capacity in BASE units. Both legs of both sides are now
     * linear in the price (`q * avg / S` and its exact inverse), the exact rational integral is
     * additive by construction, and the only residue is integer rounding — one *amount* unit per
     * piece, in the pool's favour, instead of a price tick scaled by the whole trade size.
     *
     * This test now asserts the property in both ask directions:
     *
     *   quote-in  (`amountOutAsk`)  — the exact inverse of an additive function is super-additive,
     *                                 so a split can never buy MORE base than one shot.
     *   base-out  (`amountInAsk`)   — `sum ceil(x_i) >= ceil(sum x_i)`, so a split can never pay
     *                                 LESS quote, and the excess is at most `n - 1` quote units.
     */
    function test_BUG1_Ask_SplittingBeatsOneShot() public view {
        uint256 minAsk = 3e9;
        uint256 cap = 30e18; // 30 WETH of sellable base — ~90k USDC at this mid. BASE units.
        uint8 exp = 18;

        uint16[3] memory widthsBps = [20, 100, 1000];
        uint256[3] memory pieces = [uint256(2), 4, 16];

        for (uint256 w; w < widthsBps.length; ++w) {
            uint256 maxAsk = minAsk + (minAsk * widthsBps[w]) / 10_000;
            // The epoch's quote ceiling: what all of its base costs. This is the quote-denominated
            // amount that corresponds to "consume the whole epoch", and it is what the
            // pre-amendment test was reaching for when it passed `cap` as a quote amount.
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
                // Each leg floors independently, so the split gives up at most one base unit per
                // extra piece. Bounded, and in the pool's favour.
                assertGe(sum + (n - 1), one, "quote-in split shortfall exceeded one base unit per extra piece");

                uint256 sumCost = _splitAskCost(cap, minAsk, maxAsk, cap, exp, n);
                console2.log("    base-out one shot cost", ceilingQuote);
                console2.log("    base-out split cost   ", sumCost);
                assertGe(sumCost, ceilingQuote, "BUG1 regressed: a split ask paid less quote than one shot");
                assertLe(sumCost, ceilingQuote + (n - 1), "base-out split excess exceeded the n-1 unit bound");
            }
        }
    }

    /// @notice The bid side's additivity, stated strictly. Amendment 2 made this exact rather than
    ///         "bounded by one tick": the rational integral is additive and every piece floors, so
    ///         `sum floor(x_i) <= floor(sum x_i)` and no decomposition of a bid ever collects more
    ///         quote than executing it whole. The shortfall is at most one quote unit per extra
    ///         piece — one *amount* unit, not a price tick times `amountIn`.
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

    /**
     * BUG 4 — CLOSED by amendment 2. Kept as a regression witness, with the assertion inverted.
     *
     * What was wrong: `ceil` was applied to the *price* before the multiplication by `amountIn`, so
     * a whole tick of quantisation error got scaled by `amountIn / scale`, and
     * `ceil(a) + ceil(b) < 2*ceil((a+b)/2)` whenever `a` and `b` straddle an integer their mean does
     * not land on. That made a bid split win too, on top of BUG 1's structural ask leak:
     *
     *   minBid 8, maxBid 38, capacity 58, used 4, priceScaleExp 0
     *   one shot   : 53 in -> 1113 out
     *   50 then 3  : 1150 + 27 = 1177 out    (+5.7%)
     *
     * It was invisible at a well-chosen `priceScaleExp` (the WETH/USDC shape leaked 0) and
     * percent-level at a small one, which nothing rejects — so the qualification-free claim in the
     * source was simply false.
     *
     * What fixed it: folding the price into the amount. There is now exactly one rounding per
     * trade, applied to the amount, so the residue cannot be multiplied by the trade size. The
     * witness parameters below are unchanged; only the direction of the inequality is.
     */
    function test_BUG4_Bid_SplittingBeatsOneShot_SmallScale() public view {
        uint256 one = h.outBid(53, 8, 38, 58, 4, 0);
        uint256 a = h.outBid(50, 8, 38, 58, 4, 0);
        uint256 b = h.outBid(3, 8, 38, 58, 54, 0);
        console2.log("one shot 53 ->", one);
        console2.log("split 50 + 3 ->", a + b);
        assertLe(a + b, one, "BUG4 regressed: splitting a bid beat executing it whole");
        // And the shortfall is one output unit per extra piece, not a price tick times the size.
        assertGe(a + b + 1, one, "bid split shortfall exceeded one output unit for two pieces");
    }

    /// @notice What the ask side's splitting is bounded by: the top of book. No amount of slicing
    ///         can make the pool sell below the best price in the interval.
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
        // One price unit of slack per unit of base sold, for the same reason as in
        // `testFuzz_RealisedAskNeverBeatsExecutableTop`: `executableTopAsk` ceils the drift.
        assertLt(top * sum, amountIn * (10 ** exp) + sum, "ask splitting beat the top of book");
    }

    /// @notice Split-additivity on the ASK side, the mirror of
    ///         `testFuzz_Bid_SplitNeverExceedsExactIntegral`. It is a theorem on both sides now, so
    ///         it gets a fuzz test on both.
    ///
    /// @dev Stated in the ask side's primitive direction (base out, quote cost), where the algebra
    ///      is exact: the rational integral is additive, every piece ceils, so
    ///      `sum ceil(x_i) >= ceil(sum x_i)`. Both bounds are asserted — no decomposition of an ask
    ///      ever pays LESS than executing it whole (the property amendment 1 bought), and it never
    ///      pays more than `n - 1` quote units MORE (the property that keeps the fix from being a
    ///      tax on splitting).
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

    /// @notice The bid side's additivity, stated positively: sum of pieces never *exceeds* the
    ///         real-valued curve either, so the pool is never worse off than the exact integral.
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

        // R = amountIn * (maxBid - width * (amountIn/2) / cap) / scale, kept exact by scaling the
        // whole numerator: 2 * cap * scale * R = amountIn * (2*cap*maxBid - width*amountIn).
        // amountIn <= 2**96, cap <= 2**96, maxBid <= 2**56  =>  <= 2**154. Safe.
        assertLe(
            sum * 2 * cap * (10 ** exp),
            amountIn * (2 * cap * (minBid + width) - width * amountIn),
            "split sum exceeded the exact integral"
        );
    }

    // =====================================================================
    // Fuzz — bid then ask round trip
    // =====================================================================

    struct Ladder {
        uint256 minBid;
        uint256 maxBid;
        uint256 minAsk;
        uint256 maxAsk;
    }

    /// @dev Builds a ladder that satisfies `validateLadder`'s ordering, or reports that the fuzz
    ///      inputs could not be coerced into one.
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

    /// @notice A bid followed by an ask of the equivalent size never hands back more base than
    ///         went in. This is the "no free lunch across the spread" property, and it is what
    ///         `validateLadder`'s `minAsk >= maxBid` exists to guarantee.
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

    /// @notice The same round trip in the other order: ask then bid.
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

    // =====================================================================
    // Differential test against the Rust engine's vectors
    // =====================================================================

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

    /// @notice Every vector the Rust engine emitted must reproduce exactly on this side.
    ///
    /// @dev The fixture is a flat, alphabetically-keyed array of objects, one per call. The schema
    ///      kept its 13 fields and its byte-wise sorted order:
    ///
    ///        amountIn capacity expectRevert expected kind maxAsk maxBid minAsk minBid
    ///        minPrice name priceScaleExp used
    ///
    ///      `kind` selects the function and there are now FOUR amount kinds, not two:
    ///      `amountOutBid`, `amountInBid`, `amountInAsk`, `amountOutAsk`, plus `validateLadder`,
    ///      `executableTopBid` and `executableTopAsk`. `expectRevert` carries an error NAME when the
    ///      vector expects a revert and is empty otherwise, and every numeric field is a decimal
    ///      string so that uint256 values survive JSON.
    ///
    ///      Two consumer notes travel with the schema:
    ///
    ///        * `amountIn` carries the REQUESTED OUTPUT for `amountInBid` and `amountOutAsk` — it is
    ///          the pinned leg of the call, not necessarily an input token amount.
    ///        * `capacity` and `used` are BASE units on both sides (amendment 1). An ask vector's
    ///          `capacity` is sellable base, so it is not comparable with a quote `amountIn`.
    ///
    ///      Field reads go through `_u`/`_s`, which try `vm.parseJsonString` + `vm.parseUint` FIRST.
    ///      That ordering is load bearing, not defensive: `vm.parseJson`'s numeric coercion changes
    ///      shape at the `i64::MAX` boundary, and 33 of the 250 vectors carry a field past it (the
    ///      largest is `type(uint128).max`). Reading each field as a string and parsing it here is
    ///      the only encoding-independent path. `abi.decode` over the whole struct is NOT a
    ///      substitute — it re-enters the same coercion, one field at a time, with no fallback.
    ///
    ///      If the file is absent the test logs and returns: a missing fixture is not a curve
    ///      defect.
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
        // The fixture carries 250 vectors across seven kinds. A floor rather than an equality so
        // regenerating with more is fine, but shrinking is not: this test used to assert only the
        // two `amountOut*` kinds and quietly skip the 105 exact-output vectors, and a bare
        // `skipped == 0` on a dispatcher that never saw them would have looked clean.
        assertGe(checked, 250, "fewer vectors asserted than the fixture carries");
    }

    /// @dev Proves the decoder works without depending on the generator: writes a fixture in the
    ///      documented shape, decodes it, asserts it, deletes it. Values are hand-computed, and all
    ///      four amount kinds appear — the pre-amendment self test covered only the two `amountOut*`
    ///      kinds, so it could not have caught the dispatcher silently skipping the other two.
    ///
    ///      Ladder for every vector below: minBid 100, maxBid 200, minAsk 100, maxAsk 200,
    ///      capacity 1000 BASE, used 400 BASE, exp 0. So `W = 100`, `2*C*S = 2000`.
    ///
    ///        amountOutBid(600) = floor(600 * (2*200*1000 - 100*(800+600)) / 2000) = 78000
    ///        amountInBid(78000) = 600, and it is minimal: 599 gives floor(155799900/2000) = 77899
    ///        amountInAsk(600)  = ceil(600 * (2*100*1000 + 100*(800+600)) / 2000) = 102000
    ///        amountOutAsk(102000) = 600, the whole remaining room, exactly
    ///        amountOutAsk(600) = 4, since cost(4) = 561 <= 600 < cost(5) = 702
    function test_Differential_DecoderSelfTest() public {
        string memory path = "testdata/_propcurve_decoder_selftest.json";
        vm.writeFile(path, string.concat("[", _selfTestAmountVectors(), ",", _selfTestOtherVectors(), "]"));
        (uint256 checked, uint256 skipped) = _runVectors(vm.readFile(path));
        vm.removeFile(path);
        assertEq(skipped, 1, "the unknown-kind vector must be the only skip");
        assertEq(checked, 10, "self test did not decode every vector");
    }

    /// @dev Split in two because one `string.concat` over the whole fixture exhausts the legacy code
    ///      generator's stack; `foundry.toml` keeps `via_ir` off for iteration speed.
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
            // A kind the dispatcher does not know: it must be counted as skipped and must not leak
            // an armed `expectRevert` into the next vector. This is the regression guard for the
            // failure that hid the two missing amount kinds.
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

        // Dispatchability is decided BEFORE `vm.expectRevert` is armed. Arming it and then failing
        // to make a call leaves the cheatcode latched onto whatever the harness does next, which
        // surfaces as "call didn't revert at a lower depth than cheatcode call depth" from a
        // completely unrelated vector — the failure mode that hid the two missing kinds.
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
            // `amountIn` is the quote spent; the answer is base.
            uint256 got = h.outAsk(v.amountIn, v.minAsk, v.maxAsk, v.capacity, v.used, uint8(v.priceScaleExp));
            if (!expectsRevert) assertEq(got, v.expected, label);
        } else if (_eq(v.kind, "amountInBid")) {
            // `amountIn` carries the requested QUOTE OUTPUT; the answer is the base input.
            uint256 got = h.inBid(v.amountIn, v.minBid, v.maxBid, v.capacity, v.used, uint8(v.priceScaleExp));
            if (!expectsRevert) assertEq(got, v.expected, label);
        } else if (_eq(v.kind, "amountInAsk")) {
            // `amountIn` carries the requested BASE OUTPUT; the answer is the quote input.
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

    /// @dev The dispatcher's vocabulary, kept in one place so it cannot drift from `_assertVector`.
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

    // Candidate key spellings, most-likely first.
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

    /// @dev String-first, deliberately. The fixture encodes every numeric field as a decimal string
    ///      precisely so that values past `i64::MAX` survive JSON, and `vm.parseJson*` coerces by
    ///      magnitude at that boundary — so the string path is the exact one and the bare-number
    ///      path is the fallback for a hand-written fixture. Reversing this ordering is what makes a
    ///      `type(uint128).max` field decode to something else without erroring.
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

    // =====================================================================
    // Helpers
    // =====================================================================

    /// @dev Bounds fuzz inputs to the domain PropPool can actually present: uint56 prices, uint96
    ///      capacity, `exp <= MAX_PRICE_SCALE_EXP`, `used < capacity`.
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

    /// @dev Quote-in ask walk. **Usage advances by the BASE leg**, i.e. by what each piece actually
    ///      bought — capacity and usage are base-denominated on both sides now (amendment 1), so
    ///      advancing `u` by the quote amount spent would compare a quote figure against a base
    ///      counter and mis-price every leg after the first.
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

    /// @dev Base-out ask walk: split `amountOut` base into `n` contiguous pieces and total the quote
    ///      cost. Here the split variable IS the base leg, so usage advances by the piece.
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

    /// @dev Equal-sized split walk. Returns `(false, 0)` if any leg leaves the curve's domain, so
    ///      a fuzz case that wanders out of it is skipped rather than asserted. Arguments travel in
    ///      a struct because the alternative is `Stack too deep` with the optimizer's non-IR
    ///      pipeline, which `foundry.toml` pins for iteration speed.
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

    /// @dev Quote-in ask walk, revert-tolerant. Usage advances by the BASE leg — see `_splitAsk`.
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

    /// @dev Base-out ask walk, revert-tolerant. `w.amountIn` carries the BASE amount to split.
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
