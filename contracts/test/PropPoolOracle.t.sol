// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {Test} from "forge-std/Test.sol";
import {console2} from "forge-std/console2.sol";
import {PropPool} from "../src/PropPool.sol";
import {IPropPool} from "../src/interfaces/IPropPool.sol";
import {IPyth, PythErrors, PythStructs} from "../src/interfaces/IPyth.sol";
import {MockERC20} from "../src/mocks/MockERC20.sol";
import {MockPyth} from "../src/mocks/MockPyth.sol";

/// @notice An oracle that reverts on every read. Stands in for "Pyth is deployed but broken" —
///         a paused proxy, a failed upgrade, a chain where the contract exists and answers
///         nothing useful.
/// @dev Distinct from the never-populated-feed case, which `MockPyth` already reproduces
///      faithfully via `PriceFeedNotFound`. This one reverts *before* any feed lookup, which is
///      what the pool's `catch` arm has to survive.
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

/// @notice An oracle that consumes all the gas it is given rather than returning.
/// @dev The residual hole `_referencePrice` documents and does not close. Pinned here so the
///      accepted behaviour is a recorded decision rather than an untested assumption: reaching
///      this state requires the *owner* to have pointed `pyth` at a hostile contract, and the
///      owner is the timelock. The updater cannot call `setPyth`.
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

/// @title PropPoolOracleTest
/// @notice The reference-oracle deviation bound: `PropPool.updateQuote`'s check that a coherent
///         ladder is also a *correct* one.
///
/// ## What this file is actually testing
///
/// Everything else in the suite proves the pool enforces coherence — the book does not cross, it
/// clears `minPrice`, it is fresh, it has capacity. None of that knows what the asset is worth,
/// and the gap it leaves is the one that costs money: a ladder built from a feed that is
/// confidently wrong is perfectly coherent at the wrong level, and the pool quotes it with real
/// inventory behind it. So the tests here are adversarial about *level*, not about structure.
///
/// The fixture mirrors the two live GIWA Sepolia markets exactly, because the scaling is where
/// this check is most likely to be silently wrong and the two markets exercise the exponent
/// arithmetic in both directions:
///
///   | pair | market      | base/quote dp | priceScaleExp | refExpo | pythExpo | net |
///   |------|-------------|---------------|---------------|---------|----------|-----|
///   | 1    | mWETH/mUSDC | 18 / 6        | 24            | +12     | -8       | +4  |
///   | 2    | mWBTC/mUSDC | 8 / 6         | 12            | +10     | -8       | +2  |
///
/// A guard that ignored `expo`, or read it as a positive decimal count, would be off by sixteen
/// orders of magnitude and would accept every ladder ever posted while looking like it worked.
/// `test_Scaling_*` are the tests that would catch that, and they assert exact equality against
/// the mid the deploy script encodes rather than an approximate band.
contract PropPoolOracleTest is Test {
    // ---------------------------------------------------------------------
    // Fixture
    // ---------------------------------------------------------------------

    PropPool internal pool;
    MockPyth internal pyth;

    MockERC20 internal weth; // 18 decimals
    MockERC20 internal wbtc; // 8 decimals
    MockERC20 internal usdc; // 6 decimals
    MockERC20 internal dai; // 18 decimals, used only by the never-configured control pair

    address internal owner = address(0xAA01);
    address internal manager = address(0xAA02);
    address internal updater = address(0xAA03);
    address internal guardian = address(0xAA04);
    address internal taker = address(0xAA05);

    uint16 internal constant PAIR_ETH = 1;
    uint16 internal constant PAIR_BTC = 2;

    /// @notice A pair whose oracle is never configured. The control in every gas comparison, and
    ///         a standing check that an unbounded pair remains a first-class citizen.
    /// @dev Identical in shape to `PAIR_ETH` — same decimals, same `priceScaleExp`, same
    ///      `minPrice` — so a bounded/unbounded gas delta measured across the two is the oracle
    ///      and nothing else.
    uint16 internal constant PAIR_PLAIN = 3;

    /// @dev Both derived by `DubuScript._priceScaleExpFor`, reproduced here as literals so that a
    ///      change to that derivation shows up as a failing scaling test rather than as two files
    ///      quietly agreeing on a new number.
    uint8 internal constant EXP_ETH = 24;
    uint8 internal constant EXP_BTC = 12;

    /// @dev `priceScaleExp + quoteDecimals - baseDecimals`.
    int8 internal constant REF_EXPO_ETH = 12; // 24 + 6 - 18
    int8 internal constant REF_EXPO_BTC = 10; // 12 + 6 - 8

    bytes32 internal constant FEED_ETH = keccak256("Crypto.ETH/USD");
    bytes32 internal constant FEED_BTC = keccak256("Crypto.BTC/USD");

    /// @dev Pyth's canonical exponent for USD crypto feeds. Negative, which is the entire point.
    int32 internal constant PYTH_EXPO = -8;
    int64 internal constant PYTH_ETH = 2_000e8; // $2,000.00000000
    int64 internal constant PYTH_BTC = 100_000e8; // $100,000.00000000

    /// @dev What those must scale to in each pair's own price units.
    uint256 internal constant REF_ETH = 2e15;
    uint256 internal constant REF_BTC = 1e15;

    uint16 internal constant DEV_BPS = 100; // 1%
    uint32 internal constant PYTH_STALE = 30;
    uint32 internal constant MAX_STALE = 60;

    uint56 internal constant MIN_PRICE_ETH = 1e15; // mid / 2, as the deploy script sets it
    uint56 internal constant MIN_PRICE_BTC = 5e14;

    uint256 internal constant BPS = 10_000;

    function setUp() public {
        // A realistic wall clock, so `publishTime` arithmetic in either direction has room.
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

    // ---------------------------------------------------------------------
    // Helpers
    // ---------------------------------------------------------------------

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

    /// @dev A ladder centred on `mid`, byte-for-byte `DubuScript._ladder`: 5 bps half-spread,
    ///      25 bps width. This is the shape the live updater actually posts.
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

    /// @dev A single-entry `updateQuote` batch for a ladder centred on `mid`.
    function _one(uint16 pairId, uint256 mid) internal pure returns (uint256[] memory w) {
        (uint256 a, uint256 b, uint256 c, uint256 d) = _ladder(mid);
        w = new uint256[](1);
        w[0] = _pack(pairId, a, b, c, d);
    }

    /// @dev Return every account a bounded push touches to the state it is in at the start of a
    ///      real transaction: cold. `vm.cool` clears both the account's warmth and every storage
    ///      slot of it that has been accessed, which together are most of the cost being measured.
    function _coolAll() internal {
        vm.cool(address(pool));
        vm.cool(address(pyth));
    }

    function _refresh(uint16 pairId, uint96 bidCap, uint96 askCap) internal {
        vm.prank(updater);
        pool.refreshCapacity(pairId, bidCap, askCap);
    }

    // =====================================================================
    // Scaling — the exponent arithmetic, per live market
    //
    // If any single thing in this feature is wrong, it is this. `expo` is negative, `refExpo`
    // depends on two token decimals and an immutable per-pair exponent, and being wrong by one
    // order of magnitude in either direction produces a bound that either rejects everything or
    // accepts everything.
    // =====================================================================

    /// @notice mWETH/mUSDC: 18/6 at `priceScaleExp = 24`. $2,000 must land on exactly 2e15, which
    ///         is the mid `DubuScript._encodeMid` produces for the same market.
    function test_Scaling_Weth18_Usdc6_Exp24() public view {
        (uint256 price, uint8 status) = pool.referencePrice(PAIR_ETH);
        assertEq(status, pool.REF_OK(), "ETH reference not OK");
        assertEq(price, REF_ETH, "ETH reference did not scale to the pool's mid");

        // Stated the long way once, so the derivation is legible next to the assertion:
        // 2000e8 * 10**(-8 + 12) == 2e11 * 1e4 == 2e15.
        assertEq(uint256(uint64(PYTH_ETH)) * 10 ** uint256(int256(PYTH_EXPO) + REF_EXPO_ETH), REF_ETH);
    }

    /// @notice mWBTC/mUSDC: 8/6 at `priceScaleExp = 12`. The other direction of magnitude — an
    ///         8-decimal base, where `refExpo` differs and the net exponent is +2, not +4.
    function test_Scaling_Wbtc8_Usdc6_Exp12() public view {
        (uint256 price, uint8 status) = pool.referencePrice(PAIR_BTC);
        assertEq(status, pool.REF_OK(), "BTC reference not OK");
        assertEq(price, REF_BTC, "BTC reference did not scale to the pool's mid");
        assertEq(uint256(uint64(PYTH_BTC)) * 10 ** uint256(int256(PYTH_EXPO) + REF_EXPO_BTC), REF_BTC);
    }

    /// @notice The scaled reference is directly comparable with the stored ladder — same units,
    ///         same `priceScaleExp`. That comparability is the reason `referencePrice` returns a
    ///         scaled number instead of the raw Pyth pair.
    function test_Scaling_ReferenceIsComparableWithTheStoredLadder() public {
        _pushLadder(PAIR_ETH, REF_ETH);
        IPropPool.PairSnapshot memory s = pool.snapshot(PAIR_ETH);
        (uint256 ref,) = pool.referencePrice(PAIR_ETH);

        assertLt(uint256(s.maxBid), ref, "top of book should sit just under a dead-on reference");
        assertGt(uint256(s.minAsk), ref, "best ask should sit just over a dead-on reference");
        // The whole ladder inside 1% of the reference, which is what a 5bps/25bps book means.
        assertGt(uint256(s.minBid) * BPS, ref * (BPS - 100));
        assertLt(uint256(s.maxAsk) * BPS, ref * (BPS + 100));
    }

    /// @notice Pyth can change a feed's exponent, so it is read from every response rather than
    ///         hardcoded. Moving it must move the reference by exactly the corresponding power.
    /// @dev A consumer that cached or assumed `expo` would return the same number here and pass
    ///      every other test in this file.
    function test_Scaling_ExpoChangeIsHonoured() public {
        // Same dollar value, expressed at expo -5 instead of -8: 2000e5 rather than 2000e8.
        pyth.setPriceNow(FEED_ETH, 2_000e5, 1e5, -5);
        (uint256 price, uint8 status) = pool.referencePrice(PAIR_ETH);
        assertEq(status, pool.REF_OK());
        assertEq(price, REF_ETH, "a re-exponented feed must produce the same reference");
    }

    /// @notice Negative net exponents are handled, not just positive ones.
    /// @dev Not reachable on either live market — both land at +4 and +2 — but `refExpo` is signed
    ///      for a reason and the division branch should not be dead code nobody has ever run.
    function test_Scaling_NegativeNetExponentDivides() public {
        vm.prank(manager);
        pool.setPairOracle(PAIR_ETH, FEED_ETH, DEV_BPS, PYTH_STALE, 6); // net = -8 + 6 = -2

        (uint256 price, uint8 status) = pool.referencePrice(PAIR_ETH);
        assertEq(status, pool.REF_OK());
        assertEq(price, uint256(uint64(PYTH_ETH)) / 100, "negative net exponent must divide");
    }

    // =====================================================================
    // The bound itself
    // =====================================================================

    function test_LadderInsideTheBoundIsAccepted() public {
        _pushLadder(PAIR_ETH, REF_ETH);
        assertEq(pool.snapshot(PAIR_ETH).updatedAt, uint32(block.timestamp), "ladder did not land");

        // And it keeps working while the reference drifts inside tolerance: 0.5% either way.
        pyth.setPriceNow(FEED_ETH, (PYTH_ETH * 10_050) / 10_000, 1e8, PYTH_EXPO);
        _pushLadder(PAIR_ETH, (REF_ETH * 10_050) / 10_000);
        pyth.setPriceNow(FEED_ETH, (PYTH_ETH * 9_950) / 10_000, 1e8, PYTH_EXPO);
        _pushLadder(PAIR_ETH, (REF_ETH * 9_950) / 10_000);
    }

    /// @notice The bid ceiling. A ladder that has the pool BUYING base well above the reference is
    ///         the compromised-updater case that costs the most: the pool pays over the odds for
    ///         inventory and the taker keeps the difference.
    function test_LadderAboveTheBoundIsRejected() public {
        // 10% above the reference, against a 1% tolerance.
        uint256 mid = (REF_ETH * 11_000) / 10_000;
        (uint256 minBid, uint256 maxBid, uint256 minAsk, uint256 maxAsk) = _ladder(mid);

        uint256[] memory w = new uint256[](1);
        w[0] = _pack(PAIR_ETH, minBid, maxBid, minAsk, maxAsk);
        vm.prank(updater);
        vm.expectRevert(abi.encodeWithSelector(PropPool.BidCeilingExceeded.selector, PAIR_ETH, REF_ETH, maxBid));
        pool.updateQuote(w);
    }

    /// @notice The ask floor. The mirror failure: the pool SELLS base far under the reference and
    ///         is drained of inventory at a discount.
    function test_LadderBelowTheBoundIsRejected() public {
        uint256 mid = (REF_ETH * 9_000) / 10_000; // 10% below
        (uint256 minBid, uint256 maxBid, uint256 minAsk, uint256 maxAsk) = _ladder(mid);

        uint256[] memory w = new uint256[](1);
        w[0] = _pack(PAIR_ETH, minBid, maxBid, minAsk, maxAsk);
        vm.prank(updater);
        vm.expectRevert(abi.encodeWithSelector(PropPool.AskFloorBreached.selector, PAIR_ETH, REF_ETH, minAsk));
        pool.updateQuote(w);
    }

    /// @notice The exact bid boundary, both sides of it, one price unit apart.
    ///
    /// @dev The rule is `maxBid * BPS > ref * (BPS + dev)` reverts, so equality is accepted. With
    ///      `ref = 2e15` and `dev = 100`, the largest admissible `maxBid` is
    ///      `2e15 * 10100 / 10000 = 2.02e15` exactly — no rounding anywhere, which is why these
    ///      constants were chosen. `minAsk` is set above `maxBid` to keep the book uncrossed and
    ///      comfortably above its own floor, so the only thing under test is the bid rule.
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

    /// @notice The exact ask boundary, both sides. `minAsk * BPS < ref * (BPS - dev)` reverts, so
    ///         equality is again accepted: `2e15 * 9900 / 10000 = 1.98e15`, exact.
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

    /// @notice Only the two taker-favourable ends are bounded. Widening the ladder moves `minBid`
    ///         down and `maxAsk` up — both in the pool's favour — and must never be rejected, no
    ///         matter how far it goes.
    /// @dev This is the property that decouples `maxDeviationBps` from the desk's spread. If the
    ///      far ends were bounded too, widening during volatility would silently eat the safety
    ///      margin and the operator would have to keep the two dials in sync by hand.
    function test_Bound_IgnoresTheLaddersFarEnds() public {
        uint256 ceiling = (REF_ETH * (BPS + DEV_BPS)) / BPS;

        // Both bounded ends pinned at the tightest place the rules allow them to meet, then the
        // two unbounded ends dragged as far apart as the field permits: `minBid` down to the
        // pair's absolute floor, `maxAsk` up to the top of `uint56`. That is a ~36x-wide book —
        // nothing a desk would post, and entirely pool-favourable at both extremes.
        _push(PAIR_ETH, MIN_PRICE_ETH, ceiling, ceiling, uint256(type(uint56).max));
        assertEq(uint256(pool.snapshot(PAIR_ETH).minBid), MIN_PRICE_ETH, "a far-end bid must not be bounded");
        assertEq(
            uint256(pool.snapshot(PAIR_ETH).maxAsk), uint256(type(uint56).max), "a far-end ask must not be bounded"
        );
    }

    /// @notice The attack the whole feature exists for: the updater key leaks and the attacker
    ///         posts a coherent, fresh, in-capacity ladder that sells the pool's base at a 40%
    ///         discount to fair value.
    ///
    /// @dev The size is chosen precisely, and 40% rather than 90% is the point of the test.
    ///      `minPrice` is a static absolute floor and the deploy script sets it at half the
    ///      reference — it *has* to sit far below the market, because a floor near fair value
    ///      strands every quote the moment there is a real drawdown. So `minPrice` catches a 10x
    ///      haircut and lets a 40% one through, and a 40% haircut against the pool's whole ask
    ///      capacity is already a catastrophic loss.
    ///
    ///      Asserted below rather than described: this ladder clears `validateLadder` and clears
    ///      `minPrice`. Every guard that existed before this feature passes it. The oracle is the
    ///      only thing standing in front of it.
    function test_CompromisedUpdater_CannotSellInventoryBelowFairValue() public {
        uint256 mid = (REF_ETH * 6_000) / BPS; // 60% of fair value
        (uint256 minBid, uint256 maxBid, uint256 minAsk, uint256 maxAsk) = _ladder(mid);

        assertTrue(maxAsk >= minAsk && minAsk >= maxBid && maxBid >= minBid, "ladder must be coherent");
        assertGe(minBid, MIN_PRICE_ETH, "ladder must clear the pair's absolute floor");

        uint256[] memory w = new uint256[](1);
        w[0] = _pack(PAIR_ETH, minBid, maxBid, minAsk, maxAsk);
        vm.prank(updater);
        vm.expectRevert(abi.encodeWithSelector(PropPool.AskFloorBreached.selector, PAIR_ETH, REF_ETH, minAsk));
        pool.updateQuote(w);

        // Nothing landed, so there is no ladder to be drained through.
        assertEq(pool.snapshot(PAIR_ETH).updatedAt, 0, "a rejected ladder must not be stored");
        assertEq(pool.getAmountOut(address(usdc), address(weth), 1_000e6), 0, "no quote may survive the rejection");
    }

    /// @notice A batch is atomic: one out-of-bound pair reverts every pair in the call.
    /// @dev Partial application would leave the updater guessing which ladders landed, and the
    ///      ones that did were priced by a feed that just proved itself wrong on another pair.
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

    /// @notice The bound is per pair: each is judged against its own feed, its own `refExpo` and
    ///         its own reference. Cross-wiring them would be invisible on a single-market fixture.
    function test_Bound_IsPerPair() public {
        // Each pair accepts its own ladder against its own feed, `refExpo` and reference.
        _pushLadder(PAIR_ETH, REF_ETH);
        _pushLadder(PAIR_BTC, REF_BTC);

        // The ETH ladder pushed at the BTC pair is 2x that pair's reference, and must be judged
        // against the BTC feed rather than the one that produced it.
        (uint256 a, uint256 b, uint256 c, uint256 d) = _ladder(REF_ETH);
        uint256[] memory w = new uint256[](1);
        w[0] = _pack(PAIR_BTC, a, b, c, d);
        vm.prank(updater);
        vm.expectRevert(abi.encodeWithSelector(PropPool.BidCeilingExceeded.selector, PAIR_BTC, REF_BTC, b));
        pool.updateQuote(w);
    }

    /// @notice `maxDeviationBps == 0` collapses the bound onto the reference exactly: the pool may
    ///         not bid above it and may not ask below it. Satisfiable — that is a zero-width
    ///         spread straddling the reference — and worth pinning as a supported setting.
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

    /// @notice `maxDeviationBps == BPS` is the loosest legal setting: the ask floor degenerates to
    ///         zero and the bid ceiling to twice the reference. Legal, and still a bound.
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

    // =====================================================================
    // Pyth misbehaving — every arm fails closed
    // =====================================================================

    /// @notice A price older than our own window stops pushes. Boundary included: exactly
    ///         `maxPythStaleSecs` old is fresh, one second past it is not.
    function test_StalePyth_RejectsAtTheBoundary() public {
        pyth.setPrice(FEED_ETH, PYTH_ETH, 1e8, PYTH_EXPO, block.timestamp - PYTH_STALE);
        _pushLadder(PAIR_ETH, REF_ETH); // exactly at the window: still fresh

        pyth.setPrice(FEED_ETH, PYTH_ETH, 1e8, PYTH_EXPO, block.timestamp - PYTH_STALE - 1);
        (uint256 minBid, uint256 maxBid, uint256 minAsk, uint256 maxAsk) = _ladder(REF_ETH);
        uint256[] memory w = new uint256[](1);
        w[0] = _pack(PAIR_ETH, minBid, maxBid, minAsk, maxAsk);
        uint8 stale = pool.REF_STALE();
        vm.prank(updater);
        vm.expectRevert(abi.encodeWithSelector(PropPool.ReferenceUnavailable.selector, PAIR_ETH, stale));
        pool.updateQuote(w);
    }

    /// @notice A future-dated price is stale in exactly the same way a past-dated one is.
    /// @dev The comparison is an absolute difference, matching Pyth's own `diff`. A reference is
    ///      only a reference if it has been observed, and a timestamp ahead of the chain has not
    ///      been. A naive `block.timestamp - publishTime` would underflow-revert here instead —
    ///      inside a function that promises never to revert.
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

    /// @notice A negative price is rejected, not cast.
    /// @dev `price` is `int64` and Pyth feeds for rates and spreads legitimately go negative. A
    ///      wrapping cast would turn -1 into ~1.8e19 — a reference vastly above any `uint56`
    ///      ladder, which would trivially satisfy the bid ceiling and trivially fail the ask
    ///      floor. Not a safe direction to be wrong in; rejected outright instead.
    function test_NegativePythPrice_IsRejected() public {
        pyth.setPriceNow(FEED_ETH, -1, 1e8, PYTH_EXPO);

        (uint256 price, uint8 status) = pool.referencePrice(PAIR_ETH);
        assertEq(status, pool.REF_INVALID(), "a negative price must be invalid, not cast");
        assertEq(price, 0);

        _expectPushFails(PAIR_ETH, REF_ETH, pool.REF_INVALID());
    }

    /// @notice Zero is not a price. As a bound it would pin the bid ceiling to zero and reject
    ///         every ladder that could ever exist, which is a confusing way to report a dead feed.
    function test_ZeroPythPrice_IsRejected() public {
        pyth.setPriceNow(FEED_ETH, 0, 1e8, PYTH_EXPO);
        (, uint8 status) = pool.referencePrice(PAIR_ETH);
        assertEq(status, pool.REF_INVALID());
        _expectPushFails(PAIR_ETH, REF_ETH, pool.REF_INVALID());
    }

    /// @notice A feed id that Pyth has never had a price for reverts `PriceFeedNotFound`, and the
    ///         pool must fail closed rather than treat the missing price as zero.
    /// @dev `MockPyth`'s header calls this out as the mock fidelity that matters most: a guard
    ///      written against a mock that returned zero would bound every ladder against 0.
    function test_UnpopulatedFeed_FailsClosed() public {
        vm.prank(manager);
        pool.setPairOracle(PAIR_ETH, keccak256("Crypto.NOPE/USD"), DEV_BPS, PYTH_STALE, REF_EXPO_ETH);

        (uint256 price, uint8 status) = pool.referencePrice(PAIR_ETH);
        assertEq(status, pool.REF_UNAVAILABLE(), "an unpopulated feed must be UNAVAILABLE, not price 0");
        assertEq(price, 0);

        _expectPushFails(PAIR_ETH, REF_ETH, pool.REF_UNAVAILABLE());
    }

    /// @notice An oracle that reverts on every call. The `catch` arm.
    function test_RevertingPyth_FailsClosed() public {
        address dead = address(new RevertingPyth());
        vm.prank(owner);
        pool.setPyth(dead);

        (, uint8 status) = pool.referencePrice(PAIR_ETH);
        assertEq(status, pool.REF_UNAVAILABLE());
        _expectPushFails(PAIR_ETH, REF_ETH, pool.REF_UNAVAILABLE());
    }

    /// @notice An oracle address with no code behind it.
    /// @dev The `extcodesize` guard, and it is load-bearing rather than defensive: a call to an
    ///      address with no code *succeeds* and returns nothing, which is a return-data decoding
    ///      failure — and Solidity does not route those to `catch`. Without the guard this case
    ///      would revert straight through a function documented as total.
    function test_PythWithNoCode_IsUnavailableRatherThanReverting() public {
        vm.prank(owner);
        pool.setPyth(address(0xDEAD)); // an EOA as far as the EVM is concerned

        (uint256 price, uint8 status) = pool.referencePrice(PAIR_ETH);
        assertEq(status, pool.REF_UNAVAILABLE(), "a codeless oracle must be UNAVAILABLE, not a revert");
        assertEq(price, 0);

        _expectPushFails(PAIR_ETH, REF_ETH, pool.REF_UNAVAILABLE());
    }

    /// @notice An exponent far outside anything real is rejected before `10 **` forms it.
    /// @dev Guards the multiplication, and doubles as the catch-all for a garbage `expo` from a
    ///      misbehaving feed. `int32` can carry -2^31; nothing here may overflow on the way to
    ///      finding that out.
    function test_AbsurdExponent_IsInvalidRatherThanOverflowing() public {
        pyth.setPriceNow(FEED_ETH, PYTH_ETH, 1e8, type(int32).min);
        (, uint8 status) = pool.referencePrice(PAIR_ETH);
        assertEq(status, pool.REF_INVALID(), "hugely negative expo must be INVALID");

        pyth.setPriceNow(FEED_ETH, PYTH_ETH, 1e8, type(int32).max);
        (, status) = pool.referencePrice(PAIR_ETH);
        assertEq(status, pool.REF_INVALID(), "hugely positive expo must be INVALID");
    }

    /// @notice A `refExpo` that is wrong in the "too large" direction pushes the reference past
    ///         anything a `uint56` ladder could describe, and is reported as a broken
    ///         configuration rather than as a ladder that happens to be out of bound.
    /// @dev Those are different operator problems and deserve different errors.
    function test_ReferenceAboveTheRepresentableCeiling_IsInvalid() public {
        vm.prank(manager);
        pool.setPairOracle(PAIR_ETH, FEED_ETH, DEV_BPS, PYTH_STALE, 40); // net = +32

        (uint256 price, uint8 status) = pool.referencePrice(PAIR_ETH);
        assertEq(status, pool.REF_INVALID(), "an out-of-range reference must be INVALID");
        assertEq(price, 0);
        _expectPushFails(PAIR_ETH, REF_ETH, pool.REF_INVALID());
    }

    /// @notice A `refExpo` wrong in the "too small" direction collapses the reference to zero
    ///         under the flooring division, which is also a configuration error and not a price.
    function test_ReferenceCollapsingToZero_IsInvalid() public {
        vm.prank(manager);
        pool.setPairOracle(PAIR_ETH, FEED_ETH, DEV_BPS, PYTH_STALE, -40); // net = -48

        (, uint8 status) = pool.referencePrice(PAIR_ETH);
        assertEq(status, pool.REF_INVALID(), "a reference floored to zero must be INVALID");
    }

    /// @notice A wrong `refExpo` fails closed in **both** directions on the very first push, which
    ///         is the property that makes a manual exponent tolerable at all.
    /// @dev If a misconfiguration could fail *open* — bound the ladder against nonsense while
    ///      appearing to work — the operator would have no signal at all. Here they get a revert
    ///      the first time the updater tries to post.
    function test_WrongRefExpo_FailsClosedInBothDirections() public {
        vm.prank(manager);
        pool.setPairOracle(PAIR_ETH, FEED_ETH, DEV_BPS, PYTH_STALE, REF_EXPO_ETH + 1); // 10x high
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
        pool.setPairOracle(PAIR_ETH, FEED_ETH, DEV_BPS, PYTH_STALE, REF_EXPO_ETH - 1); // 10x low
        (uint256 lo,) = pool.referencePrice(PAIR_ETH);
        assertEq(lo, REF_ETH / 10);
        vm.prank(updater);
        vm.expectRevert(abi.encodeWithSelector(PropPool.BidCeilingExceeded.selector, PAIR_ETH, lo, maxBid));
        pool.updateQuote(w);
    }

    /// @notice Failing closed must not wedge the pool: everything that is not `updateQuote` keeps
    ///         working while the oracle is down, and the **manager** — not the owner, not a
    ///         timelock — can restore pushes in one transaction.
    /// @dev This is the property that makes fail-closed an outage rather than a lockup, and it is
    ///      the whole justification for choosing it over falling through.
    function test_FailClosed_IsRecoverableByTheManagerAlone() public {
        _pushLadder(PAIR_ETH, REF_ETH);
        _refresh(PAIR_ETH, 100e18, 100e18);

        address dead = address(new RevertingPyth());
        vm.prank(owner);
        pool.setPyth(dead);

        // Pushes stop...
        _expectPushFails(PAIR_ETH, REF_ETH, pool.REF_UNAVAILABLE());

        // ...but the pool is not wedged. The in-flight ladder still trades, quotes still quote,
        // capacity still refreshes, and the guardian can still halt.
        assertGt(pool.getAmountOut(address(weth), address(usdc), 1e18), 0, "quoting must survive a dead oracle");
        vm.prank(taker);
        assertGt(pool.swap(address(weth), address(usdc), int256(1e18), 0, taker, 0, block.timestamp), 0);
        _refresh(PAIR_ETH, 50e18, 50e18);
        vm.prank(guardian);
        pool.pause(PAIR_ETH);
        vm.prank(guardian);
        pool.unpause(PAIR_ETH);

        // And recovery needs only the manager.
        vm.prank(manager);
        pool.setPairOracle(PAIR_ETH, bytes32(0), 0, 0, 0);
        _pushLadder(PAIR_ETH, REF_ETH);
        assertEq(pool.snapshot(PAIR_ETH).updatedAt, uint32(block.timestamp), "pushes must resume");
    }

    // =====================================================================
    // The disabled path
    // =====================================================================

    /// @notice `feedId == 0` disables the bound entirely, and it is a supported production state:
    ///         not every listable asset has a Pyth feed, and a pair that cannot be configured is a
    ///         pair that cannot be listed.
    function test_Disabled_AcceptsAnyCoherentLadder() public {
        vm.prank(manager);
        pool.setPairOracle(PAIR_ETH, bytes32(0), 0, 0, 0);

        // Absurd in both directions; only coherence is enforced.
        _push(PAIR_ETH, MIN_PRICE_ETH, MIN_PRICE_ETH, uint256(type(uint56).max), uint256(type(uint56).max));
        assertEq(uint256(pool.snapshot(PAIR_ETH).maxAsk), uint256(type(uint56).max));
    }

    /// @notice A disabled pair never touches the oracle at all — not even to discover it is
    ///         broken. That short-circuit is both the gas argument and the availability argument.
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

    /// @notice A pair that has never been configured defaults to disabled, so adding the feature
    ///         to a live pool does not brick pairs nobody has touched yet.
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

    /// @notice Disabled and bounded pairs coexist in one batch, and a dead oracle on the bounded
    ///         one does not stop the disabled one from being pushed on its own.
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

    /// @notice The disabled state is visible from the snapshot an aggregator already fetches.
    /// @dev Bit 15 is derived, not stored, so it must not disturb bit 0 — the paused flag — in
    ///      either direction. Both are asserted together for exactly that reason.
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

    // =====================================================================
    // Role separation — the updater cannot widen its own leash
    // =====================================================================

    /// @notice The point of the whole feature. A hot key that could raise its own deviation limit,
    ///         lengthen its own staleness window, or zero its own feed id would be bounded by
    ///         nothing at all.
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

        // Nothing moved.
        PropPool.PairOracle memory o = pool.pairOracle(PAIR_ETH);
        assertEq(o.feedId, FEED_ETH);
        assertEq(o.maxDeviationBps, DEV_BPS);
        assertEq(o.maxPythStaleSecs, PYTH_STALE);
        assertEq(pool.pyth(), address(pyth));
    }

    /// @notice The roles are strictly separated in both directions: the owner does not get the
    ///         manager's dials for free, and the manager does not get the owner's.
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

    // =====================================================================
    // Configuration validation
    // =====================================================================

    function test_SetPairOracle_RejectsBadConfiguration() public {
        vm.startPrank(manager);

        vm.expectRevert(PropPool.UnknownPair.selector);
        pool.setPairOracle(0, FEED_ETH, DEV_BPS, PYTH_STALE, REF_EXPO_ETH);
        vm.expectRevert(PropPool.UnknownPair.selector);
        pool.setPairOracle(99, FEED_ETH, DEV_BPS, PYTH_STALE, REF_EXPO_ETH);

        // Above 100% the ask floor `ref * (BPS - dev)` would underflow.
        vm.expectRevert(PropPool.DeviationTooLarge.selector);
        pool.setPairOracle(PAIR_ETH, FEED_ETH, uint16(BPS + 1), PYTH_STALE, REF_EXPO_ETH);

        // A zero window demands a price published in this exact second — it would brick the pair.
        vm.expectRevert(PropPool.ZeroPythStaleWindow.selector);
        pool.setPairOracle(PAIR_ETH, FEED_ETH, DEV_BPS, 0, REF_EXPO_ETH);

        vm.stopPrank();
    }

    /// @notice The validation only applies when the bound is being switched on. Disabling a pair
    ///         must never be blockable by parameters that are about to stop mattering.
    function test_SetPairOracle_DisablingIgnoresTheOtherArguments() public {
        vm.prank(manager);
        pool.setPairOracle(PAIR_ETH, bytes32(0), type(uint16).max, 0, type(int8).min);
        assertEq(pool.pairOracle(PAIR_ETH).feedId, bytes32(0));
        (, uint8 status) = pool.referencePrice(PAIR_ETH);
        assertEq(status, pool.REF_DISABLED());
    }

    /// @notice A feed cannot be configured before the pool knows where Pyth is — that would store
    ///         a configuration whose only possible outcome is `REF_UNAVAILABLE` on every push.
    function test_SetPairOracle_RequiresPythToBeSet() public {
        PropPool fresh = new PropPool(owner, manager, updater, guardian);
        vm.prank(owner);
        fresh.addPair(address(weth), address(usdc), EXP_ETH, MAX_STALE, MIN_PRICE_ETH);

        vm.prank(manager);
        vm.expectRevert(PropPool.PythNotSet.selector);
        fresh.setPairOracle(PAIR_ETH, FEED_ETH, DEV_BPS, PYTH_STALE, REF_EXPO_ETH);

        // Disabling is still fine with no oracle configured at all.
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

    // =====================================================================
    // The no-revert contract
    //
    // `IPropPool` promises the quoting views return 0 rather than reverting, totally, because
    // aggregators batch them and one poisoned pair must not take down the quotes for the healthy
    // pairs alongside it. The bound must not have introduced a way to break that.
    // =====================================================================

    /// @notice Every view stays total while the oracle is dead. It costs nothing to hold this
    ///         property here because the views do not call Pyth at all — which is itself the
    ///         placement decision, asserted rather than assumed.
    function test_Views_StayTotalWhilePythIsBroken() public {
        _pushLadder(PAIR_ETH, REF_ETH);
        _refresh(PAIR_ETH, 100e18, 100e18);

        uint256 healthy = pool.getAmountOut(address(weth), address(usdc), 1e18);
        assertGt(healthy, 0);

        address dead = address(new RevertingPyth());
        vm.prank(owner);
        pool.setPyth(dead);

        // Not merely non-reverting: unchanged. A view that started returning 0 here would be a
        // silent outage for every aggregator quoting this pair.
        assertEq(pool.getAmountOut(address(weth), address(usdc), 1e18), healthy, "quote changed with a dead oracle");
        assertGt(pool.getAmountIn(address(weth), address(usdc), 1_000e6), 0);
        assertGt(pool.quoteByPair(PAIR_ETH, true, 1e18), 0);
        pool.snapshot(PAIR_ETH);
        pool.getSupportedPairs();
        pool.pairConfig(PAIR_ETH);
        pool.pairOracle(PAIR_ETH);

        // Absurd inputs remain 0-not-revert, exactly as before.
        assertEq(pool.getAmountOut(address(weth), address(usdc), type(uint256).max), 0);
        assertEq(pool.getAmountIn(address(weth), address(usdc), type(uint256).max), 0);
    }

    /// @notice `referencePrice` is total too, in every oracle state, including ones a `try`/`catch`
    ///         alone would not survive.
    function test_ReferencePrice_IsTotalInEveryState() public {
        (, uint8 s) = pool.referencePrice(0); // pair 0 does not exist
        assertEq(s, pool.REF_DISABLED());
        (, s) = pool.referencePrice(type(uint16).max); // nor does this one
        assertEq(s, pool.REF_DISABLED());

        vm.prank(owner);
        pool.setPyth(address(0xDEAD)); // no code: the case `catch` cannot handle
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

    /// @notice An oracle that burns all the gas rather than returning does **not** defeat
    ///         totality, and the reason is EIP-150 rather than anything the pool does.
    ///
    /// @dev Worth a test precisely because it is counter-intuitive and because the margin it
    ///      relies on is invisible in the source. The callee receives 63/64 of the remaining gas,
    ///      exhausts it, and reverts; `catch` catches that like any other revert; and the 1/64 the
    ///      caller retained is enough to finish, because nothing after the `try` block does more
    ///      than arithmetic. Swept across four orders of magnitude so the conclusion is not an
    ///      artefact of one gas limit.
    ///
    ///      The load-bearing part is the "nothing after the `try`" clause. Adding storage writes
    ///      or another external call after it would eat into the retained 1/64 and could turn this
    ///      green test red — which is exactly what it is here to do.
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

        // And it fails closed on the push path, like every other unavailable oracle.
        _expectPushFails(PAIR_ETH, REF_ETH, pool.REF_UNAVAILABLE());
    }

    // =====================================================================
    // Swap path — untouched, and asserted to be untouched
    // =====================================================================

    /// @notice `swap` never consults the oracle. The strongest available form of the assertion:
    ///         an oracle that reverts on every call, and a swap that completes anyway.
    /// @dev This is the placement decision made testable. If someone later moves the check into
    ///      `swap`, this test fails, and the contract header explains why that would be a
    ///      regression rather than a hardening.
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

    // =====================================================================
    // Gas
    //
    // Numbers, not adjectives. The assertions are deliberately loose bands — they exist to catch
    // an order-of-magnitude regression, not to break on a compiler bump — while the logged values
    // are the ones quoted in `PropPool`'s natspec.
    // =====================================================================

    /// @notice **The production number.** What the bound costs a live updater transaction.
    ///
    /// @dev Measured cold, because a real push is cold. Each transaction starts with a fresh
    ///      access list, so the Pyth account, Pyth's own storage, the pair config and the oracle
    ///      config are all cold every single time, however often the bot pushes. A warm-warm
    ///      measurement taken inside one test body understates the cost by roughly the difference
    ///      between a 2,600-gas cold account access and a 100-gas warm one, plus 2,000 per cold
    ///      storage slot — which is most of the answer.
    ///
    ///      The recipe:
    ///        * push both ladders first, so the quote slot is *dirty* and the SSTORE is 2,900
    ///          rather than 22,100 — the steady state for a bot pushing every block;
    ///        * push a *different* ladder when measuring. Re-pushing the identical word in the
    ///          same block writes the same 256 bits and the SSTORE collapses to 100 gas, quietly
    ///          removing 2,800 from the figure;
    ///        * `vm.cool` every account the push reads, immediately before measuring. Note that
    ///          `vm.revertToState` does **not** do this — it restores the access list as it stood
    ///          when the snapshot was taken, so a snapshot captured after a warm-up call replays
    ///          a warm access list and the "cold" measurement is silently warm. The `assertGt`
    ///          against a warm control below is what catches that mistake.
    ///
    ///      `PAIR_PLAIN` is the control: same decimals, same `priceScaleExp`, same `minPrice` as
    ///      `PAIR_ETH`, oracle never configured. The difference between the two is the bound and
    ///      nothing else.
    function test_Gas_UpdateQuote_ColdTransaction_BoundedVersusUnbounded() public {
        // Dirty both quote slots, so the measured SSTORE is a 2,900 dirty-slot write rather than
        // a 22,100 zero-to-nonzero one — the steady state for a bot pushing every block.
        _pushLadder(PAIR_ETH, REF_ETH);
        _pushLadder(PAIR_PLAIN, REF_ETH);

        // **A different ladder from the warm-up.** Pushing the identical word in the same block
        // writes the same 256 bits and the SSTORE collapses to 100 gas, which silently removes
        // 2,800 of the number under measurement. All three ladders below sit inside the 1% bound.
        uint256[] memory boundedA = _one(PAIR_ETH, (REF_ETH * 10_001) / BPS);
        uint256[] memory boundedB = _one(PAIR_ETH, (REF_ETH * 10_002) / BPS);
        uint256[] memory plainA = _one(PAIR_PLAIN, (REF_ETH * 10_001) / BPS);

        _coolAll();
        vm.prank(updater);
        uint256 g0 = gasleft();
        pool.updateQuote(boundedA);
        uint256 gBoundedCold = g0 - gasleft();

        // Warm control, immediately afterwards. If this is not meaningfully cheaper, the "cold"
        // measurement above was not cold and the whole comparison is worthless.
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
        // Budget: three cold SLOADs on our side (the two oracle words and the `pyth` address),
        // one cold account access for Pyth, and Pyth's own struct read — `MockPyth` stores a full
        // `PriceFeed`, five words, which is the single largest line item and is the mock being
        // pessimistic relative to the real contract's tighter `PriceInfo`. ~19k. A band rather
        // than an equality so a compiler bump does not break it, but tight enough that a second
        // external call or a storage write on this path would show up here.
        assertLt(gBoundedCold - gPlainCold, 25_000, "the bound costs far more than a single oracle read");
    }

    /// @notice An unbounded pair stays essentially as cheap as it was before the feature existed:
    ///         one cold `feedId` SLOAD that short-circuits before the external call.
    ///
    /// @dev This is what makes it viable to list assets with no Pyth feed alongside ones that have
    ///      them, and to batch the two together. Measured against a pool that has never had
    ///      `setPyth` called at all — the true "feature absent" baseline — on identical pair
    ///      geometry, both cold.
    function test_Gas_UpdateQuote_UnboundedPairIsNearlyFree() public {
        PropPool bare = new PropPool(owner, manager, updater, guardian);
        vm.prank(owner);
        bare.addPair(address(weth), address(usdc), EXP_ETH, MAX_STALE, MIN_PRICE_ETH);

        uint256[] memory warmup = _one(PAIR_ETH, REF_ETH);
        uint256[] memory measured = _one(PAIR_ETH, (REF_ETH * 10_001) / BPS);
        uint256[] memory measuredPlain = _one(PAIR_PLAIN, (REF_ETH * 10_001) / BPS);

        vm.prank(updater);
        bare.updateQuote(warmup); // dirty the slot
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

    /// @notice Batch amortisation. The Pyth account is cold once per *transaction*, not once per
    ///         pair, so the second and subsequent bounded pairs in a batch are meaningfully
    ///         cheaper than the first. This is the number that matters for a multi-market updater,
    ///         since the bot batches every market into one push.
    /// @dev Cold on both sides, for the reasons spelled out on the cold/unbounded test above.
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

    /// @notice `swap` is unchanged — asserted **equal**, not merely close.
    ///
    /// @dev Measured on the *same* pool from the *same* state, via a state snapshot, with only the
    ///      oracle configuration differing. Comparing two separately-deployed pools would not do:
    ///      they differ in pair count, reserve values and slot warmth, and the resulting handful of
    ///      gas would drown the thing under test. Reverting to a snapshot makes the two runs
    ///      identical in everything except whether `_oracle[1].feedId` is set, which turns "swap
    ///      does not read the oracle" into an exact equality rather than a tolerance.
    function test_Gas_Swap_IsUnchangedByTheBound() public {
        _pushLadder(PAIR_ETH, REF_ETH);
        _refresh(PAIR_ETH, 100e18, 100e18);
        // One swap first so every slot the measured swap touches is already dirty.
        vm.prank(taker);
        pool.swap(address(weth), address(usdc), int256(1e18), 0, taker, 0, block.timestamp);

        uint256 snap = vm.snapshotState();

        // Both branches perform the same `setPairOracle` call so the two runs are identical in
        // slot warmth and call history; only the value written differs.
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

    /// @notice The quoting views are unaffected too, for the same reason and by the same method.
    function test_Gas_View_IsUnchangedByTheBound() public {
        _pushLadder(PAIR_ETH, REF_ETH);
        _refresh(PAIR_ETH, 100e18, 100e18);
        pool.getAmountOut(address(weth), address(usdc), 1e18); // warm

        uint256 snap = vm.snapshotState();

        // Symmetric branches: same call sequence on both sides, different stored `feedId`.
        vm.prank(manager);
        pool.setPairOracle(PAIR_ETH, FEED_ETH, DEV_BPS, PYTH_STALE, REF_EXPO_ETH);
        pool.getAmountOut(address(weth), address(usdc), 1e18); // re-warm
        uint256 g0 = gasleft();
        pool.getAmountOut(address(weth), address(usdc), 1e18);
        uint256 gBounded = g0 - gasleft();

        vm.revertToState(snap);
        vm.prank(manager);
        pool.setPairOracle(PAIR_ETH, bytes32(0), 0, 0, 0);
        pool.getAmountOut(address(weth), address(usdc), 1e18); // re-warm
        g0 = gasleft();
        pool.getAmountOut(address(weth), address(usdc), 1e18);
        uint256 gPlain = g0 - gasleft();

        console2.log("getAmountOut, bounded pair  :", gBounded);
        console2.log("getAmountOut, unbounded pair:", gPlain);
        assertEq(gBounded, gPlain, "getAmountOut must not depend on oracle configuration");
    }

    // ---------------------------------------------------------------------
    // Shared assertion
    // ---------------------------------------------------------------------

    /// @dev A dead-on ladder that would pass every other guard, rejected for `status`.
    function _expectPushFails(uint16 pairId, uint256 mid, uint8 status) internal {
        (uint256 minBid, uint256 maxBid, uint256 minAsk, uint256 maxAsk) = _ladder(mid);
        uint256[] memory w = new uint256[](1);
        w[0] = _pack(pairId, minBid, maxBid, minAsk, maxAsk);
        vm.prank(updater);
        vm.expectRevert(abi.encodeWithSelector(PropPool.ReferenceUnavailable.selector, pairId, status));
        pool.updateQuote(w);
    }
}
