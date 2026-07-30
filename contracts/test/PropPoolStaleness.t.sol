// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {Test} from "forge-std/Test.sol";
import {console2} from "forge-std/console2.sol";
import {CommonBase} from "forge-std/Base.sol";
import {StdUtils} from "forge-std/StdUtils.sol";

import {PropPool} from "../src/PropPool.sol";
import {IPropPool} from "../src/interfaces/IPropPool.sol";
import {PropCurve} from "../src/libraries/PropCurve.sol";
import {MockERC20} from "../src/mocks/MockERC20.sol";

contract PropPoolStalenessTest is Test {

    PropPool internal pool;
    MockERC20 internal baseToken;
    MockERC20 internal quoteToken;

    address internal owner = makeAddr("owner");
    address internal manager = makeAddr("manager");
    address internal updater = makeAddr("updater");
    address internal guardian = makeAddr("guardian");
    address internal taker = makeAddr("taker");

    uint16 internal constant PAIR = 1;
    uint8 internal constant PRICE_SCALE_EXP = 18;
    uint56 internal constant MIN_PRICE = 1e9;

    uint32 internal constant MAX_STALE_SECS = 60;

    uint16 internal constant DECAY_SECS = 30;

    uint96 internal constant CAPACITY = 1_000e18;

    uint256 internal constant MID = 2e9;
    uint256 internal constant HALF_SPREAD_BPS = 5;
    uint256 internal constant WIDTH_BPS = 25;

    uint256 internal constant SEED_BASE = 20_000e18;
    uint256 internal constant SEED_QUOTE = 40_000_000e6;

    function setUp() public {
        vm.warp(1_800_000_000);

        pool = new PropPool(owner, manager, updater, guardian);
        baseToken = new MockERC20("Base", "BASE", 18);
        quoteToken = new MockERC20("Quote", "QUOTE", 6);

        vm.prank(owner);
        pool.addPair(address(baseToken), address(quoteToken), PRICE_SCALE_EXP, MAX_STALE_SECS, MIN_PRICE);

        baseToken.mint(manager, SEED_BASE);
        quoteToken.mint(manager, SEED_QUOTE);
        vm.startPrank(manager);
        baseToken.approve(address(pool), type(uint256).max);
        quoteToken.approve(address(pool), type(uint256).max);
        pool.deposit(address(baseToken), SEED_BASE);
        pool.deposit(address(quoteToken), SEED_QUOTE);
        vm.stopPrank();

        _pushLadder();
        vm.prank(updater);
        pool.refreshCapacity(PAIR, CAPACITY, CAPACITY);

        baseToken.mint(taker, 100_000e18);
        quoteToken.mint(taker, 200_000_000e6);
        vm.startPrank(taker);
        baseToken.approve(address(pool), type(uint256).max);
        quoteToken.approve(address(pool), type(uint256).max);
        vm.stopPrank();
    }

    function test_theRampIsWhatItClaimsAtEveryAge() public {
        _enableDecay(DECAY_SECS);
        uint256 t0 = block.timestamp;

        (uint96 bid, uint96 ask, uint16 window) = pool.effectiveCapacity(PAIR);
        assertEq(uint256(bid), uint256(CAPACITY), "age 0 must be the full posted capacity");
        assertEq(uint256(ask), uint256(CAPACITY), "age 0 must be the full posted capacity, ask side");
        assertEq(uint256(window), uint256(DECAY_SECS), "the window is not reported");

        console2.log("linear ramp, capacity = 1000e18 base, decaySecs = 30");
        console2.log("  age(s)   effective (base wei)          % of posted");
        for (uint256 age = 0; age <= uint256(DECAY_SECS); ++age) {
            vm.warp(t0 + age);
            (bid, ask,) = pool.effectiveCapacity(PAIR);

            uint256 expected = age >= DECAY_SECS ? 0 : (uint256(CAPACITY) * (DECAY_SECS - age)) / DECAY_SECS;
            assertEq(uint256(bid), expected, "bid side does not match the closed form");
            assertEq(uint256(ask), expected, "ask side does not match the closed form");

            if (age % 5 == 0 || age == DECAY_SECS) {
                console2.log(
                    string.concat(
                        "  ",
                        _pad(vm.toString(age), 9),
                        _pad(vm.toString(expected), 30),
                        vm.toString((expected * 100) / uint256(CAPACITY))
                    )
                );
            }
        }

        vm.warp(t0 + DECAY_SECS);
        (bid, ask,) = pool.effectiveCapacity(PAIR);
        assertEq(uint256(bid), 0, "the ramp must reach exactly zero at decaySecs");
        assertEq(uint256(ask), 0, "the ramp must reach exactly zero at decaySecs, ask side");

        vm.warp(t0 + DECAY_SECS + 1);
        (bid, ask,) = pool.effectiveCapacity(PAIR);
        assertEq(uint256(bid) + uint256(ask), 0, "the ramp came back past its end");
        vm.warp(t0 + MAX_STALE_SECS);
        (bid, ask,) = pool.effectiveCapacity(PAIR);
        assertEq(uint256(bid) + uint256(ask), 0, "the ramp came back at the staleness cliff");
    }

    function test_theRampHandlesAOneSecondWindow() public {
        _enableDecay(1);
        (uint96 bid,,) = pool.effectiveCapacity(PAIR);
        assertEq(uint256(bid), uint256(CAPACITY), "a 1s window must still be full at age 0");

        vm.warp(block.timestamp + 1);
        (bid,,) = pool.effectiveCapacity(PAIR);
        assertEq(uint256(bid), 0, "a 1s window must be empty at age 1");
        assertEq(pool.quoteByPair(PAIR, true, 1e18), 0, "a 1s window still quoted at age 1");
    }

    function test_disabledIsTheDefaultAndIsInert() public {
        (uint96 bid, uint96 ask, uint16 window) = pool.effectiveCapacity(PAIR);
        assertEq(uint256(window), 0, "a new pair must default to no ramp");
        assertEq(uint256(bid), uint256(CAPACITY), "disabled must not touch the bid capacity");
        assertEq(uint256(ask), uint256(CAPACITY), "disabled must not touch the ask capacity");

        uint256 fresh = pool.quoteByPair(PAIR, true, 100e18);
        for (uint256 age = 1; age <= uint256(MAX_STALE_SECS); ++age) {
            vm.warp(block.timestamp + 1);
            (bid, ask,) = pool.effectiveCapacity(PAIR);
            assertEq(uint256(bid), uint256(CAPACITY), "disabled ramp decayed the bid side");
            assertEq(uint256(ask), uint256(CAPACITY), "disabled ramp decayed the ask side");
            assertEq(pool.quoteByPair(PAIR, true, 100e18), fresh, "disabled ramp moved the quote");
        }

        assertEq(pool.quoteByPair(PAIR, true, CAPACITY), _swapBid(CAPACITY), "disabled ramp refused the full epoch");
    }

    function test_theRampCanBeSwitchedBackOff() public {
        _enableDecay(DECAY_SECS);
        vm.warp(block.timestamp + 29);
        (uint96 bid,,) = pool.effectiveCapacity(PAIR);
        assertEq(uint256(bid), uint256(CAPACITY) / 30, "the ramp is not where it should be at age 29");

        _enableDecay(0);
        uint16 window;
        (bid,, window) = pool.effectiveCapacity(PAIR);
        assertEq(uint256(window), 0, "the window was not cleared");
        assertEq(uint256(bid), uint256(CAPACITY), "switching the ramp off did not restore depth");
    }

    function test_aWindowPastTheCliffIsAPartialHaircut() public {
        _enableDecay(120);
        vm.warp(block.timestamp + MAX_STALE_SECS);
        (uint96 bid,,) = pool.effectiveCapacity(PAIR);
        assertEq(uint256(bid), (uint256(CAPACITY) * (120 - 60)) / 120, "the haircut is not the closed form");
        assertGt(pool.quoteByPair(PAIR, true, 100e18), 0, "a haircut pair must still quote inside the window");

        vm.warp(block.timestamp + 1);
        (bid,,) = pool.effectiveCapacity(PAIR);
        assertEq(uint256(bid), 0, "the staleness cliff did not override the unfinished ramp");
    }

    function test_onlyTheManagerSetsTheDecayWindow() public {

        vm.prank(manager);
        vm.expectRevert(PropPool.NotUpdater.selector);
        pool.updateQuote(new uint256[](0));

        vm.prank(updater);
        vm.expectRevert(PropPool.NotManager.selector);
        pool.setPairDecay(PAIR, 30);

        vm.prank(owner);
        vm.expectRevert(PropPool.NotManager.selector);
        pool.setPairDecay(PAIR, 30);

        vm.prank(guardian);
        vm.expectRevert(PropPool.NotManager.selector);
        pool.setPairDecay(PAIR, 30);

        vm.prank(taker);
        vm.expectRevert(PropPool.NotManager.selector);
        pool.setPairDecay(PAIR, 30);

        vm.prank(manager);
        pool.setPairDecay(PAIR, 30);
        (,, uint16 window) = pool.effectiveCapacity(PAIR);
        assertEq(uint256(window), 30, "the manager could not set the window");
    }

    function test_theDecayWindowIsPerPairAndRejectsUnknownPairs() public {
        MockERC20 other = new MockERC20("Other", "OTH", 18);
        vm.prank(owner);
        uint16 second = pool.addPair(address(other), address(quoteToken), PRICE_SCALE_EXP, MAX_STALE_SECS, MIN_PRICE);

        vm.startPrank(manager);
        vm.expectRevert(PropPool.UnknownPair.selector);
        pool.setPairDecay(0, 30);
        vm.expectRevert(PropPool.UnknownPair.selector);
        pool.setPairDecay(second + 1, 30);

        pool.setPairDecay(PAIR, 30);
        vm.stopPrank();

        (,, uint16 w1) = pool.effectiveCapacity(PAIR);
        (,, uint16 w2) = pool.effectiveCapacity(second);
        assertEq(uint256(w1), 30, "pair 1 did not take the window");
        assertEq(uint256(w2), 0, "the window leaked onto another pair");
    }

    function test_refreshCapacityAndPausePreserveTheDecayWindow() public {
        _enableDecay(DECAY_SECS);

        vm.prank(updater);
        pool.refreshCapacity(PAIR, CAPACITY / 2, CAPACITY / 4);
        (uint96 bid, uint96 ask, uint16 window) = pool.effectiveCapacity(PAIR);
        assertEq(uint256(window), uint256(DECAY_SECS), "refreshCapacity cleared the decay window");
        assertEq(uint256(bid), uint256(CAPACITY) / 2, "the new epoch did not take");
        assertEq(uint256(ask), uint256(CAPACITY) / 4, "the new ask epoch did not take");

        uint256[] memory packed = new uint256[](1);
        packed[0] = uint256(CAPACITY) | (uint256(CAPACITY) << 96) | (uint256(PAIR) << 224);
        vm.prank(updater);
        pool.refreshCapacityBatch(packed);
        (,, window) = pool.effectiveCapacity(PAIR);
        assertEq(uint256(window), uint256(DECAY_SECS), "refreshCapacityBatch cleared the decay window");

        vm.prank(guardian);
        pool.pause(PAIR);
        assertEq(pool.snapshot(PAIR).flags & 1, 1, "pause did not take");
        vm.prank(guardian);
        pool.unpause(PAIR);
        (,, window) = pool.effectiveCapacity(PAIR);
        assertEq(uint256(window), uint256(DECAY_SECS), "pause/unpause cleared the decay window");
        assertEq(pool.snapshot(PAIR).flags & 1, 0, "unpause did not take");

        vm.prank(guardian);
        pool.pause(PAIR);
        uint32 genBefore = pool.snapshot(PAIR).capGen;
        vm.prank(manager);
        pool.setPairDecay(PAIR, 15);
        IPropPool.PairSnapshot memory s = pool.snapshot(PAIR);
        assertEq(s.flags & 1, 1, "setPairDecay cleared the guardian's pause");
        assertEq(uint256(s.capGen), uint256(genBefore), "setPairDecay bumped the generation");
        assertEq(uint256(s.bidCapacity), uint256(CAPACITY), "setPairDecay moved the bid capacity");
        assertEq(uint256(s.askCapacity), uint256(CAPACITY), "setPairDecay moved the ask capacity");
    }

    function test_theViewsTrackTheRampAndTheSwapPathAgrees() public {
        _enableDecay(DECAY_SECS);
        uint256 t0 = block.timestamp;

        for (uint256 age; age <= uint256(DECAY_SECS); ++age) {
            vm.warp(t0 + age);
            (uint96 effBid,,) = pool.effectiveCapacity(PAIR);

            if (effBid == 0) {
                assertEq(pool.quoteByPair(PAIR, true, 1), 0, "a fully decayed side still quoted");
                assertEq(pool.getAmountOut(address(baseToken), address(quoteToken), 1), 0, "getAmountOut disagrees");
                assertEq(pool.getAmountIn(address(baseToken), address(quoteToken), 1), 0, "getAmountIn disagrees");
                vm.prank(taker);
                vm.expectRevert(PropPool.InsufficientCapacity.selector);
                pool.swap(address(baseToken), address(quoteToken), int256(1), 0, taker, 0, type(uint256).max);
                continue;
            }

            uint256 atBound = pool.quoteByPair(PAIR, true, uint256(effBid));
            assertGt(atBound, 0, "the epoch's own effective bound did not quote");
            assertEq(
                atBound,
                pool.getAmountOut(address(baseToken), address(quoteToken), uint256(effBid)),
                "the two view paths disagree at the bound"
            );
            assertEq(pool.quoteByPair(PAIR, true, uint256(effBid) + 1), 0, "quoted one unit past the ramp");

            uint256 snapId = vm.snapshotState();
            assertEq(_swapBid(uint256(effBid)), atBound, "quoted != executed at the effective bound");
            vm.revertToState(snapId);

            vm.prank(taker);
            vm.expectRevert(PropPool.InsufficientCapacity.selector);
            pool.swap(
                address(baseToken), address(quoteToken), int256(uint256(effBid) + 1), 0, taker, 0, type(uint256).max
            );
        }
    }

    function test_theAskSideRampIsMeasuredInBaseAndConvertedThroughTheCurve() public {
        _enableDecay(DECAY_SECS);
        vm.warp(block.timestamp + 15);

        (, uint96 effAsk,) = pool.effectiveCapacity(PAIR);
        assertEq(uint256(effAsk), uint256(CAPACITY) / 2, "the ask ramp is not at half");

        uint256 ceiling = _maxAmountInFor(false);
        assertGt(ceiling, 0, "the ramped ask ceiling collapsed to nothing");

        uint256 baseOut = pool.quoteByPair(PAIR, false, ceiling);
        assertGt(baseOut, 0, "the ramped ask ceiling did not quote");
        assertLe(baseOut, uint256(effAsk), "the ceiling bought past the ramp");

        assertGe(baseOut + 1e12, uint256(effAsk), "the ceiling is not tight against the ramp");

        assertEq(pool.quoteByPair(PAIR, false, ceiling + 1), 0, "quoted one quote unit past the ramp");
        vm.prank(taker);
        vm.expectRevert(PropPool.InsufficientCapacity.selector);
        pool.swap(address(quoteToken), address(baseToken), int256(ceiling + 1), 0, taker, 0, type(uint256).max);

        assertEq(_swapAsk(ceiling), baseOut, "quoted != executed on the ask side");
        assertEq(uint256(pool.snapshot(PAIR).askUsed), baseOut, "usage did not follow the base delivered");
        assertLe(uint256(pool.snapshot(PAIR).askUsed), uint256(effAsk), "usage passed the ramp's bound");
        assertEq(pool.quoteByPair(PAIR, false, 1), 0, "the ramped ask side still had depth after its ceiling");
    }

    function test_effectiveCapacityNeverReverts() public {
        _enableDecay(DECAY_SECS);

        (uint96 bid, uint96 ask, uint16 window) = pool.effectiveCapacity(0);
        assertEq(uint256(bid) + uint256(ask) + uint256(window), 0, "pair 0 must read as nothing");
        (bid, ask, window) = pool.effectiveCapacity(type(uint16).max);
        assertEq(uint256(bid) + uint256(ask) + uint256(window), 0, "an out-of-range pair must read as nothing");

        MockERC20 other = new MockERC20("Other", "OTH", 18);
        vm.prank(owner);
        uint16 never = pool.addPair(address(other), address(quoteToken), PRICE_SCALE_EXP, MAX_STALE_SECS, MIN_PRICE);
        (bid, ask,) = pool.effectiveCapacity(never);
        assertEq(uint256(bid) + uint256(ask), 0, "a never-quoted pair must report no depth");

        vm.prank(guardian);
        pool.pause(PAIR);
        (bid, ask, window) = pool.effectiveCapacity(PAIR);
        assertEq(uint256(bid) + uint256(ask), 0, "a paused pair must report no depth");
        assertEq(uint256(window), uint256(DECAY_SECS), "the window must still be reported while paused");
        vm.prank(guardian);
        pool.unpause(PAIR);

        vm.prank(guardian);
        pool.pauseAll();
        (bid, ask,) = pool.effectiveCapacity(PAIR);
        assertEq(uint256(bid) + uint256(ask), 0, "a globally paused pool must report no depth");
        vm.prank(guardian);
        pool.unpauseAll();

        vm.warp(block.timestamp + uint256(MAX_STALE_SECS) + 1);
        (bid, ask,) = pool.effectiveCapacity(PAIR);
        assertEq(uint256(bid) + uint256(ask), 0, "a stale pair must report no depth");
    }

    function test_snapshotAdvertisesTheRampWithoutDisturbingTheOtherFlags() public {
        assertEq(pool.snapshot(PAIR).flags & 0x4000, 0, "a pair with no ramp must not advertise one");

        _enableDecay(DECAY_SECS);
        assertEq(pool.snapshot(PAIR).flags & 0x4000, 0x4000, "a ramped pair must advertise it");
        assertEq(pool.snapshot(PAIR).flags & 1, 0, "the derived bit disturbed the paused bit");

        vm.prank(guardian);
        pool.pause(PAIR);
        assertEq(pool.snapshot(PAIR).flags, 0x4001, "both bits must coexist");

        _enableDecay(0);
        assertEq(pool.snapshot(PAIR).flags, 1, "disabling the ramp must clear the bit and nothing else");

        vm.prank(guardian);
        pool.unpause(PAIR);

        _enableDecay(DECAY_SECS);
        vm.warp(block.timestamp + 15);
        IPropPool.PairSnapshot memory s = pool.snapshot(PAIR);
        assertEq(uint256(s.bidCapacity), uint256(CAPACITY), "snapshot must report the curve's capacity");
        (uint96 effBid,,) = pool.effectiveCapacity(PAIR);
        assertEq(uint256(effBid), uint256(CAPACITY) / 2, "effectiveCapacity must report the bound");
    }

    function test_agedQuoteIsEitherTheSameNumberOrZero() public {
        _enableDecay(DECAY_SECS);
        uint256 t0 = block.timestamp;

        uint256[5] memory sizes = [uint256(1e18), 10e18, 100e18, 500e18, 999e18];
        uint256[5] memory fresh;
        for (uint256 k; k < 5; ++k) {
            fresh[k] = pool.quoteByPair(PAIR, true, sizes[k]);
            assertGt(fresh[k], 0, "the fresh quote must be fillable");
        }

        uint256 refusals;
        for (uint256 age = 1; age <= uint256(DECAY_SECS); ++age) {
            vm.warp(t0 + age);
            for (uint256 k; k < 5; ++k) {
                uint256 aged = pool.quoteByPair(PAIR, true, sizes[k]);
                if (aged == 0) {
                    ++refusals;
                    continue;
                }
                assertEq(aged, fresh[k], "an aged quote moved to a different non-zero number");

                uint256 snapId = vm.snapshotState();
                assertEq(_swapBid(sizes[k]), aged, "quoted != executed at this age");
                vm.revertToState(snapId);
            }
        }
        assertGt(refusals, 0, "the ramp never refused anything: the test proved nothing");
    }

    function testFuzz_agedQuoteIsTheSameNumberOrZeroAndMatchesExecution(uint256 amountIn, uint32 age, bool isBid)
        public
    {
        _enableDecay(DECAY_SECS);
        uint256 ceiling = _maxAmountInFor(isBid);
        amountIn = bound(amountIn, 1, ceiling);
        age = uint32(bound(age, 0, uint256(MAX_STALE_SECS)));

        uint256 fresh = pool.quoteByPair(PAIR, isBid, amountIn);

        (address tokenIn, address tokenOut) =
            isBid ? (address(baseToken), address(quoteToken)) : (address(quoteToken), address(baseToken));

        vm.warp(block.timestamp + age);
        uint256 aged = pool.quoteByPair(PAIR, isBid, amountIn);
        assertEq(aged, pool.getAmountOut(tokenIn, tokenOut, amountIn), "the two view paths disagree");

        if (aged != 0) assertEq(aged, fresh, "an aged quote moved to a different non-zero number");

        if (aged == 0) {
            vm.prank(taker);
            vm.expectRevert();
            pool.swap(tokenIn, tokenOut, int256(amountIn), 0, taker, 0, type(uint256).max);
            return;
        }
        uint256 before = MockERC20(tokenOut).balanceOf(taker);
        vm.prank(taker);
        uint256 executed = pool.swap(tokenIn, tokenOut, int256(amountIn), 0, taker, 0, type(uint256).max);
        assertEq(executed, aged, "quoted != executed");
        assertEq(MockERC20(tokenOut).balanceOf(taker) - before, aged, "delivered != quoted");
    }

    function testFuzz_inverseQuoteUnderTheRamp(uint256 amountOut, uint32 age, bool isBid) public {
        _enableDecay(DECAY_SECS);
        age = uint32(bound(age, 0, uint256(MAX_STALE_SECS)));
        vm.warp(block.timestamp + age);

        (address tokenIn, address tokenOut) =
            isBid ? (address(baseToken), address(quoteToken)) : (address(quoteToken), address(baseToken));

        uint256 ceiling = pool.quoteByPair(PAIR, isBid, _maxAmountInFor(isBid));
        if (ceiling == 0) {

            assertEq(pool.getAmountIn(tokenIn, tokenOut, 1), 0, "a fully decayed side priced an exact-out");
            return;
        }
        amountOut = bound(amountOut, 1, ceiling);

        uint256 quotedIn = pool.getAmountIn(tokenIn, tokenOut, amountOut);
        assertGt(quotedIn, 0, "the inverse view refused a fillable size");
        assertLt(pool.quoteByPair(PAIR, isBid, quotedIn - 1), amountOut, "the inverse quote was not minimal");

        uint256 outBefore = MockERC20(tokenOut).balanceOf(taker);
        vm.prank(taker);
        uint256 spent = pool.swap(tokenIn, tokenOut, -int256(amountOut), type(uint256).max, taker, 0, type(uint256).max);
        assertEq(spent, quotedIn, "getAmountIn != executed amountIn");
        assertEq(MockERC20(tokenOut).balanceOf(taker) - outBefore, amountOut, "exact-out delivered the wrong amount");
    }

    function test_usageIsNotRescaledByTheRamp() public {
        _enableDecay(DECAY_SECS);
        _swapBid(800e18);
        assertEq(uint256(pool.snapshot(PAIR).bidUsed), 800e18, "usage was rescaled");

        vm.warp(block.timestamp + 15);
        (uint96 effBid,,) = pool.effectiveCapacity(PAIR);
        assertEq(uint256(effBid), 500e18, "the bound is not where the ramp puts it");
        assertEq(pool.quoteByPair(PAIR, true, 1), 0, "an over-spent side still quoted under the ramp");
        vm.prank(taker);
        vm.expectRevert(PropPool.InsufficientCapacity.selector);
        pool.swap(address(baseToken), address(quoteToken), int256(1), 0, taker, 0, type(uint256).max);

        vm.prank(updater);
        pool.refreshCapacity(PAIR, CAPACITY, CAPACITY);
        assertGt(pool.quoteByPair(PAIR, true, 1e18), 0, "a new epoch did not restore the side");
    }

    function test_exposedDepthAgainstTheJump() public {
        _enableDecay(DECAY_SECS);
        uint256 t0 = block.timestamp;

        uint256 lossPerBase = ((MID * 825) / 10_000) / 100;

        console2.log("exposed depth vs quote age, decaySecs = 30, epoch = 1000 base (~$2M)");
        console2.log("  age(s)   exposed base      loss on a +100bp jump ($)   vs constant depth");
        uint256 constantLoss;
        for (uint256 age; age <= uint256(DECAY_SECS); age += 5) {
            vm.warp(t0 + age);
            (uint96 effBid,,) = pool.effectiveCapacity(PAIR);
            uint256 loss = (uint256(effBid) * lossPerBase) / 1e18 / 1e6;
            if (age == 0) constantLoss = loss;
            console2.log(
                string.concat(
                    "  ",
                    _pad(vm.toString(age), 9),
                    _pad(vm.toString(uint256(effBid) / 1e18), 18),
                    _pad(vm.toString(loss), 28),
                    constantLoss == 0 ? "-" : string.concat(vm.toString((loss * 100) / constantLoss), "%")
                )
            );
        }

        vm.warp(t0 + 1);
        (uint96 atOneSecond,,) = pool.effectiveCapacity(PAIR);
        assertGe(uint256(atOneSecond) * 100, uint256(CAPACITY) * 96, "a 1s-old ladder lost more than 4% of its depth");

        vm.warp(t0 + uint256(DECAY_SECS) - 1);
        (uint96 atEnd,,) = pool.effectiveCapacity(PAIR);
        assertEq(uint256(atEnd), uint256(CAPACITY) / uint256(DECAY_SECS), "the tail is not one capacity-second");

        uint256 ramped;
        uint256 flat;
        for (uint256 age; age < uint256(DECAY_SECS); ++age) {
            vm.warp(t0 + age);
            (uint96 e,,) = pool.effectiveCapacity(PAIR);
            ramped += uint256(e);
            flat += uint256(CAPACITY);
        }
        console2.log("integrated exposure over the window, ramped vs flat (base-seconds):");
        console2.log(ramped / 1e18, flat / 1e18);
        assertLe(ramped * 2, flat + (flat / uint256(DECAY_SECS)), "the ramp did not halve integrated exposure");
        assertGe(ramped * 2 + flat / uint256(DECAY_SECS), flat, "the ramp cut more than the closed form allows");
    }

    function test_gas_swapAndViewsUnderTheRamp() public {
        _warmUp();

        uint256 gSwapOff = _gasSwapBid(50e18);
        uint256 gOutOff = _gasQuote(50e18);
        uint256 gInOff = _gasQuoteIn(50e6);

        _enableDecay(DECAY_SECS);
        uint256 gSwapOn = _gasSwapBid(50e18);
        uint256 gOutOn = _gasQuote(50e18);
        uint256 gInOn = _gasQuoteIn(50e6);

        vm.warp(block.timestamp + 15);
        uint256 gSwapAged = _gasSwapBid(50e18);
        uint256 gOutAged = _gasQuote(50e18);

        console2.log("gas, cold, WETH/USDC-shaped pair");
        console2.log("  call                         ramp off    ramp on (age 0)   ramp on (age 15)");
        console2.log(
            string.concat(
                "  swap, exact-in bid           ",
                _pad(vm.toString(gSwapOff), 12),
                _pad(vm.toString(gSwapOn), 18),
                vm.toString(gSwapAged)
            )
        );
        console2.log(
            string.concat(
                "  getAmountOut                 ",
                _pad(vm.toString(gOutOff), 12),
                _pad(vm.toString(gOutOn), 18),
                vm.toString(gOutAged)
            )
        );
        console2.log(
            string.concat(
                "  getAmountIn                  ", _pad(vm.toString(gInOff), 12), _pad(vm.toString(gInOn), 18), "-"
            )
        );
        console2.log("  effectiveCapacity (new view) ", _gasEffectiveCapacity());

        vm.warp(block.timestamp - 15);
        _enableDecay(0);
        uint256 wOutOff = _gasQuoteWarm(50e18);
        _enableDecay(DECAY_SECS);
        uint256 wOutOn = _gasQuoteWarm(50e18);
        console2.log("gas, warm: comparable with the contract header's 6,085 for getAmountOut");
        console2.log(
            string.concat("  getAmountOut                 ", _pad(vm.toString(wOutOff), 12), vm.toString(wOutOn))
        );
        assertLt(_absDiff(wOutOn, wOutOff), 500, "the warm quote delta is not arithmetic either");

        assertLt(gSwapAged, 110_000, "swap left the 110k budget");

        assertLt(_absDiff(gSwapOn, gSwapOff), 500, "an enabled ramp cost more than arithmetic on swap");
        assertLt(_absDiff(gOutOn, gOutOff), 500, "an enabled ramp cost more than arithmetic on getAmountOut");
        assertLt(_absDiff(gInOn, gInOff), 500, "an enabled ramp cost more than arithmetic on getAmountIn");

        assertLt(_absDiff(gSwapAged, gSwapOff), 1_000, "an aged ramp cost more than arithmetic on swap");
    }

    function _absDiff(uint256 a, uint256 b) internal pure returns (uint256) {
        return a > b ? a - b : b - a;
    }

    function _enableDecay(uint16 decaySecs) internal {
        vm.prank(manager);
        pool.setPairDecay(PAIR, decaySecs);
    }

    function _pushLadder() internal {
        uint256 maxBid = (MID * (10_000 - HALF_SPREAD_BPS)) / 10_000;
        uint256 minBid = (maxBid * (10_000 - WIDTH_BPS)) / 10_000;
        uint256 minAsk = (MID * (10_000 + HALF_SPREAD_BPS)) / 10_000;
        uint256 maxAsk = (minAsk * (10_000 + WIDTH_BPS)) / 10_000;
        uint256[] memory packed = new uint256[](1);
        packed[0] = minBid | (maxBid << 56) | (minAsk << 112) | (maxAsk << 168) | (uint256(PAIR) << 224);
        vm.prank(updater);
        pool.updateQuote(packed);
    }

    function _swapBid(uint256 amountIn) internal returns (uint256) {
        vm.prank(taker);
        return pool.swap(address(baseToken), address(quoteToken), int256(amountIn), 0, taker, 0, type(uint256).max);
    }

    function _swapAsk(uint256 amountIn) internal returns (uint256) {
        vm.prank(taker);
        return pool.swap(address(quoteToken), address(baseToken), int256(amountIn), 0, taker, 0, type(uint256).max);
    }

    function _maxAmountInFor(bool isBid) internal view returns (uint256) {
        IPropPool.PairSnapshot memory s = pool.snapshot(PAIR);
        (uint96 effBid, uint96 effAsk,) = pool.effectiveCapacity(PAIR);
        uint256 available = isBid ? uint256(effBid) : uint256(effAsk);
        uint256 used = s.usedGen == s.capGen ? (isBid ? uint256(s.bidUsed) : uint256(s.askUsed)) : 0;
        if (available <= used) return 0;
        uint256 room = available - used;
        if (isBid) return room;
        if (available == uint256(s.askCapacity)) {
            return PropCurve.amountInAsk(room, s.minAsk, s.maxAsk, s.askCapacity, used, s.priceScaleExp);
        }
        return PropCurve.amountInAsk(room + 1, s.minAsk, s.maxAsk, s.askCapacity, used, s.priceScaleExp) - 1;
    }

    function _warmUp() internal {
        _gasSwapBid(50e18);
        _gasQuote(50e18);
        _gasQuoteIn(50e6);
        _gasEffectiveCapacity();
    }

    function _gasSwapBid(uint256 amountIn) internal returns (uint256) {
        uint256 snapId = vm.snapshotState();
        vm.cool(address(pool));
        vm.prank(taker);
        uint256 g0 = gasleft();
        pool.swap(address(baseToken), address(quoteToken), int256(amountIn), 0, taker, 0, type(uint256).max);
        uint256 used = g0 - gasleft();
        vm.revertToState(snapId);
        return used;
    }

    function _gasQuoteWarm(uint256 amountIn) internal view returns (uint256) {
        pool.getAmountOut(address(baseToken), address(quoteToken), amountIn);
        uint256 g0 = gasleft();
        pool.getAmountOut(address(baseToken), address(quoteToken), amountIn);
        return g0 - gasleft();
    }

    function _gasQuote(uint256 amountIn) internal returns (uint256) {
        vm.cool(address(pool));
        uint256 g0 = gasleft();
        pool.getAmountOut(address(baseToken), address(quoteToken), amountIn);
        return g0 - gasleft();
    }

    function _gasQuoteIn(uint256 amountOut) internal returns (uint256) {
        vm.cool(address(pool));
        uint256 g0 = gasleft();
        pool.getAmountIn(address(baseToken), address(quoteToken), amountOut);
        return g0 - gasleft();
    }

    function _gasEffectiveCapacity() internal returns (uint256) {
        vm.cool(address(pool));
        uint256 g0 = gasleft();
        pool.effectiveCapacity(PAIR);
        return g0 - gasleft();
    }

    function _pad(string memory s, uint256 width) internal pure returns (string memory) {
        while (bytes(s).length < width) {
            s = string.concat(s, " ");
        }
        return s;
    }
}

contract PropPoolDecayHandler is CommonBase, StdUtils {
    PropPool public immutable POOL;
    MockERC20 public immutable BASE_TOKEN;
    MockERC20 public immutable QUOTE_TOKEN;

    uint16 public constant PAIR_ID = 1;
    uint32 public constant MAX_STALE_SECS = 60;
    uint16 public constant DECAY_SECS = 30;
    uint96 public constant MIN_BASE_RESERVE = 50e18;
    uint96 public constant MIN_QUOTE_RESERVE = 50_000e6;

    address public immutable MANAGER;
    address public immutable UPDATER;
    address public immutable GUARDIAN;
    address[4] public actors;

    uint256 public quoteDivergences;

    uint256 public inverseQuoteDivergences;

    uint256 public viewPathDivergences;

    uint256 public boundDivergences;

    uint256 public viewReverts;

    uint256 public decayedFills;

    string public note;

    uint256 public fills;
    uint256 public blockedAttempts;
    uint256 public fillsUnderPartialDecay;
    uint256 public attemptsWhileFullyDecayed;

    constructor(
        PropPool pool_,
        MockERC20 base_,
        MockERC20 quote_,
        address manager_,
        address updater_,
        address guardian_,
        address[4] memory actors_
    ) {
        POOL = pool_;
        BASE_TOKEN = base_;
        QUOTE_TOKEN = quote_;
        MANAGER = manager_;
        UPDATER = updater_;
        GUARDIAN = guardian_;
        actors = actors_;
    }

    function pushLadder(uint256 midSeed, uint256 halfSpreadSeed, uint256 widthSeed) public {
        uint256 mid = _bound(midSeed, 1.2e9, 4e9);
        uint256 halfBps = _bound(halfSpreadSeed, 1, 200);
        uint256 widthBps = _bound(widthSeed, 0, 500);

        uint256 maxBid = (mid * (10_000 - halfBps)) / 10_000;
        uint256 minBid = (maxBid * (10_000 - widthBps)) / 10_000;
        uint256 minAsk = (mid * (10_000 + halfBps)) / 10_000;
        uint256 maxAsk = (minAsk * (10_000 + widthBps)) / 10_000;

        uint256[] memory packed = new uint256[](1);
        packed[0] = minBid | (maxBid << 56) | (minAsk << 112) | (maxAsk << 168) | (uint256(PAIR_ID) << 224);
        vm.prank(UPDATER);
        POOL.updateQuote(packed);
    }

    function refreshCapacity(uint256 bidSeed, uint256 askSeed) public {
        vm.prank(UPDATER);
        POOL.refreshCapacity(PAIR_ID, uint96(_bound(bidSeed, 0, 2_000e18)), uint96(_bound(askSeed, 0, 2_000e18)));
    }

    function retuneDecay(uint256 seed) public {
        uint16 window = seed % 8 == 0 ? 0 : uint16(_bound(seed, 1, 90));
        vm.prank(MANAGER);
        POOL.setPairDecay(PAIR_ID, window);
    }

    function nudgePause(uint256 seed) public {
        vm.prank(GUARDIAN);
        if (seed % 8 == 0) POOL.pause(PAIR_ID);
        else POOL.unpause(PAIR_ID);
    }

    function warp(uint256 secSeed) public {
        vm.warp(block.timestamp + _bound(secSeed, 1, (3 * uint256(DECAY_SECS)) / 2));
    }

    function topUp(uint256 seed, bool isBase) public {
        MockERC20 token = isBase ? BASE_TOKEN : QUOTE_TOKEN;
        uint256 amount = _bound(seed, 1, isBase ? 5_000e18 : 10_000_000e6);
        token.mint(MANAGER, amount);
        vm.startPrank(MANAGER);
        token.approve(address(POOL), amount);
        POOL.deposit(address(token), amount);
        vm.stopPrank();
    }

    struct Attempt {
        address actor;
        MockERC20 tokenIn;
        MockERC20 tokenOut;
        bool isBid;
        bool exhausted;
        bool partlyDecayed;
        bool quoteOk;
        uint256 quoted;
        uint256 outBefore;
    }

    function _open(uint256 actorSeed, bool isBid) private view returns (Attempt memory a) {
        a.actor = actors[_bound(actorSeed, 0, 3)];
        (a.tokenIn, a.tokenOut) = isBid ? (BASE_TOKEN, QUOTE_TOKEN) : (QUOTE_TOKEN, BASE_TOKEN);
        a.isBid = isBid;

        uint256 available = _available(isBid);
        uint256 used = _used(isBid);
        a.exhausted = available == 0 || used >= available;
        a.partlyDecayed = available != 0 && available < _nominal(isBid);
    }

    function swapExactIn(uint256 actorSeed, uint256 amountSeed, bool isBid) public {
        Attempt memory a = _open(actorSeed, isBid);
        uint256 amountIn = _pickAmountIn(amountSeed, isBid);
        a.tokenIn.mint(a.actor, amountIn);
        if (a.exhausted) attemptsWhileFullyDecayed++;

        (a.quoteOk, a.quoted) = _quoteByPair(isBid, amountIn);
        {
            (bool okB, uint256 viaTokens) = _quoteByTokens(a.tokenIn, a.tokenOut, amountIn);
            if (a.quoteOk != okB || a.quoted != viaTokens) {
                viewPathDivergences++;
                _flag("getAmountOut disagrees with quoteByPair");
            }
        }
        if (a.exhausted && a.quoted != 0) {
            boundDivergences++;
            _flag("effectiveCapacity said empty and the view still quoted");
        }

        a.outBefore = a.tokenOut.balanceOf(a.actor);
        vm.prank(a.actor);

        try POOL.swap(
            address(a.tokenIn), address(a.tokenOut), int256(amountIn), 0, a.actor, 7, type(uint256).max
        ) returns (uint256 result) {
            _closeExactIn(a, result);
        } catch {
            blockedAttempts++;
            if (a.quoteOk && a.quoted != 0) {
                quoteDivergences++;
                _flag("view promised a fill, swap reverted");
            }
        }
    }

    function _closeExactIn(Attempt memory a, uint256 result) private {
        fills++;
        if (a.exhausted) {
            decayedFills++;
            _flag("a side the ramp had emptied still filled");
        }
        if (a.partlyDecayed) fillsUnderPartialDecay++;
        if (!a.quoteOk) {
            quoteDivergences++;
            _flag("view reverted but swap filled");
        } else if (result != a.quoted) {
            quoteDivergences++;
            _flag("quoted != executed");
        }
        if (a.tokenOut.balanceOf(a.actor) - a.outBefore != result) {
            quoteDivergences++;
            _flag("delivered != returned");
        }
    }

    function swapExactOut(uint256 actorSeed, uint256 amountSeed, bool isBid) public {
        Attempt memory a = _open(actorSeed, isBid);
        uint256 amountOut = _pickAmountOut(amountSeed, isBid);
        if (amountOut == 0) return;
        a.tokenIn.mint(a.actor, _epochInputCeiling(isBid) + (isBid ? 1e18 : 1e6));

        (a.quoteOk, a.quoted) = _quoteAmountIn(a.tokenIn, a.tokenOut, amountOut);

        vm.prank(a.actor);

        try POOL.swap(
            address(a.tokenIn), address(a.tokenOut), -int256(amountOut), type(uint256).max, a.actor, 7, type(uint256).max
        ) returns (uint256 spent) {
            fills++;
            if (a.partlyDecayed) fillsUnderPartialDecay++;
            if (!a.quoteOk) {
                inverseQuoteDivergences++;
                _flag("inverse view reverted but swap filled");
            } else if (spent != a.quoted) {
                inverseQuoteDivergences++;
                _flag("getAmountIn != executed amountIn");
            }
        } catch {
            blockedAttempts++;
            if (a.quoteOk && a.quoted != 0) {
                inverseQuoteDivergences++;
                _flag("inverse view promised a fill, swap reverted");
            }
        }
    }

    function _available(bool isBid) private view returns (uint256) {
        (uint96 bid, uint96 ask,) = POOL.effectiveCapacity(PAIR_ID);
        return isBid ? uint256(bid) : uint256(ask);
    }

    function _nominal(bool isBid) private view returns (uint256) {
        IPropPool.PairSnapshot memory s = POOL.snapshot(PAIR_ID);
        return isBid ? uint256(s.bidCapacity) : uint256(s.askCapacity);
    }

    function _used(bool isBid) private view returns (uint256) {
        IPropPool.PairSnapshot memory s = POOL.snapshot(PAIR_ID);
        if (s.usedGen != s.capGen) return 0;
        return isBid ? uint256(s.bidUsed) : uint256(s.askUsed);
    }

    function _pickAmountIn(uint256 seed, bool isBid) private view returns (uint256) {
        uint256 ceiling = _epochInputCeiling(isBid);
        uint256 hi = ceiling == 0 ? (isBid ? 1e18 : 1e6) : ceiling + ceiling / 8 + 1;
        return _bound(seed, 1, hi);
    }

    function _pickAmountOut(uint256 seed, bool isBid) private returns (uint256) {
        uint256 available = _available(isBid);
        uint256 used = _used(isBid);
        if (available <= used) return _bound(seed, 1, isBid ? 1e6 : 1e18);
        uint256 room = available - used;
        if (!isBid) return _bound(seed, 1, room + room / 8 + 1);
        (, uint256 maxOut) = _quoteByPair(true, room);
        if (maxOut == 0) return _bound(seed, 1, 1e6);
        return _bound(seed, 1, maxOut + maxOut / 8 + 1);
    }

    function _epochInputCeiling(bool isBid) private view returns (uint256) {
        IPropPool.PairSnapshot memory s = POOL.snapshot(PAIR_ID);
        uint256 available = _available(isBid);
        uint256 used = _used(isBid);
        if (available <= used) return 0;
        uint256 room = available - used;
        if (isBid) return room;
        if (s.maxAsk == 0 || s.askCapacity == 0) return 0;
        return PropCurve.amountInAsk(room, s.minAsk, s.maxAsk, s.askCapacity, used, s.priceScaleExp);
    }

    function _quoteByPair(bool isBid, uint256 amountIn) private returns (bool ok, uint256 out) {
        try POOL.quoteByPair(PAIR_ID, isBid, amountIn) returns (uint256 r) {
            return (true, r);
        } catch {
            viewReverts++;
            return (false, 0);
        }
    }

    function _quoteByTokens(MockERC20 tokenIn, MockERC20 tokenOut, uint256 amountIn)
        private
        returns (bool ok, uint256 out)
    {
        try POOL.getAmountOut(address(tokenIn), address(tokenOut), amountIn) returns (uint256 r) {
            return (true, r);
        } catch {
            viewReverts++;
            return (false, 0);
        }
    }

    function _quoteAmountIn(MockERC20 tokenIn, MockERC20 tokenOut, uint256 amountOut)
        private
        returns (bool ok, uint256 into)
    {
        try POOL.getAmountIn(address(tokenIn), address(tokenOut), amountOut) returns (uint256 r) {
            return (true, r);
        } catch {
            viewReverts++;
            return (false, 0);
        }
    }

    function _flag(string memory reason) private {
        if (bytes(note).length == 0) note = reason;
    }
}

abstract contract PropPoolDecayFixture is Test {
    PropPool internal pool;
    MockERC20 internal baseToken;
    MockERC20 internal quoteToken;
    PropPoolDecayHandler internal handler;

    address internal owner = makeAddr("owner");
    address internal manager = makeAddr("manager");
    address internal updater = makeAddr("updater");
    address internal guardian = makeAddr("guardian");

    uint16 internal constant PAIR_ID = 1;

    function _deploy() internal {
        vm.warp(1_800_000_000);

        pool = new PropPool(owner, manager, updater, guardian);
        baseToken = new MockERC20("Base", "BASE", 18);
        quoteToken = new MockERC20("Quote", "QUOTE", 6);

        vm.prank(owner);
        pool.addPair(address(baseToken), address(quoteToken), 18, 60, 1e9);
        vm.startPrank(manager);
        pool.setPairConfig(PAIR_ID, 60, 1e9, 50e18, 50_000e6);
        pool.setPairDecay(PAIR_ID, 30);
        vm.stopPrank();

        baseToken.mint(manager, 20_000e18);
        quoteToken.mint(manager, 40_000_000e6);
        vm.startPrank(manager);
        baseToken.approve(address(pool), type(uint256).max);
        quoteToken.approve(address(pool), type(uint256).max);
        pool.deposit(address(baseToken), 20_000e18);
        pool.deposit(address(quoteToken), 40_000_000e6);
        vm.stopPrank();

        address[4] memory actors = [makeAddr("alice"), makeAddr("bob"), makeAddr("carol"), makeAddr("dave")];
        for (uint256 i; i < 4; ++i) {
            vm.startPrank(actors[i]);
            baseToken.approve(address(pool), type(uint256).max);
            quoteToken.approve(address(pool), type(uint256).max);
            vm.stopPrank();
        }

        handler = new PropPoolDecayHandler(pool, baseToken, quoteToken, manager, updater, guardian, actors);

        uint256 maxBid = (2e9 * 9995) / 10_000;
        uint256 minBid = (maxBid * 9975) / 10_000;
        uint256 minAsk = (2e9 * 10_005) / 10_000;
        uint256 maxAsk = (minAsk * 10_025) / 10_000;
        uint256[] memory packed = new uint256[](1);
        packed[0] = minBid | (maxBid << 56) | (minAsk << 112) | (maxAsk << 168) | (uint256(PAIR_ID) << 224);
        vm.prank(updater);
        pool.updateQuote(packed);
        vm.prank(updater);
        pool.refreshCapacity(PAIR_ID, 1_000e18, 1_000e18);
    }
}

contract PropPoolDecayInvariantTest is PropPoolDecayFixture {
    function setUp() public {
        _deploy();
        targetContract(address(handler));
    }

    function invariant_quoteEqualsExecutionUnderTheRamp() public view {
        assertEq(handler.quoteDivergences(), 0, handler.note());
        assertEq(handler.inverseQuoteDivergences(), 0, handler.note());
        assertEq(handler.viewPathDivergences(), 0, handler.note());
    }

    function invariant_effectiveCapacityIsTheEnforcedBound() public view {
        assertEq(handler.boundDivergences(), 0, handler.note());
        assertEq(handler.decayedFills(), 0, handler.note());
    }

    function invariant_viewsNeverRevertUnderTheRamp() public view {
        assertEq(handler.viewReverts(), 0, "a view reverted under the ramp");
    }

    function invariant_usedNeverExceedsNominalCapacity() public view {
        IPropPool.PairSnapshot memory s = pool.snapshot(PAIR_ID);
        if (s.usedGen == s.capGen) {
            assertLe(s.bidUsed, s.bidCapacity, "bidUsed > bidCapacity");
            assertLe(s.askUsed, s.askCapacity, "askUsed > askCapacity");
        }
    }

    function invariant_reserveNeverBelowFloor() public view {
        assertGe(pool.reserveOf(address(baseToken)), 50e18, "base reserve below floor");
        assertGe(pool.reserveOf(address(quoteToken)), 50_000e6, "quote reserve below floor");
    }
}

contract PropPoolDecayCoverageProbeTest is PropPoolDecayFixture {
    function setUp() public {
        _deploy();
    }

    function test_handlerReachesPartiallyDecayedLadders() public {
        uint256 s = uint256(keccak256("dubu.decay.tape"));
        for (uint256 i; i < 3000; ++i) {
            s = uint256(keccak256(abi.encode(s, i)));
            uint256 pick = s % 100;
            if (pick < 40) handler.swapExactIn(s >> 8, s >> 24, (s >> 4) & 1 == 0);
            else if (pick < 62) handler.swapExactOut(s >> 8, s >> 24, (s >> 4) & 1 == 0);
            else if (pick < 72) handler.pushLadder(s >> 8, s >> 24, s >> 40);
            else if (pick < 82) handler.refreshCapacity(s >> 8, s >> 24);
            else if (pick < 92) handler.warp(s >> 8);
            else if (pick < 96) handler.retuneDecay(s >> 8);
            else if (pick < 98) handler.nudgePause(s >> 8);
            else handler.topUp(s >> 8, (s >> 4) & 1 == 0);
        }

        console2.log("fills                              ", handler.fills());
        console2.log("refused attempts                   ", handler.blockedAttempts());
        console2.log("fills against a partly decayed side", handler.fillsUnderPartialDecay());
        console2.log("attempts against a fully decayed side", handler.attemptsWhileFullyDecayed());
        console2.log("view reverts (bug counter)         ", handler.viewReverts());

        assertGt(handler.fills(), 100, "no fills at all");
        assertGt(handler.fillsUnderPartialDecay(), 50, "never filled against a partly decayed ladder");
        assertGt(handler.attemptsWhileFullyDecayed(), 20, "never tried to trade a fully decayed ladder");
        assertGt(handler.blockedAttempts(), 50, "the pool never refused anything");

        assertEq(handler.quoteDivergences(), 0, handler.note());
        assertEq(handler.inverseQuoteDivergences(), 0, handler.note());
        assertEq(handler.viewPathDivergences(), 0, handler.note());
        assertEq(handler.boundDivergences(), 0, handler.note());
        assertEq(handler.decayedFills(), 0, handler.note());
        assertEq(handler.viewReverts(), 0, handler.note());
    }
}
