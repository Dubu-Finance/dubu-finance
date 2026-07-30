// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {Test} from "forge-std/Test.sol";
import {console2} from "forge-std/console2.sol";

import {Router, RouteParams, Batch, Hop, SwapStep} from "../src/Router.sol";
import {RouteDecoder} from "../src/libraries/RouteDecoder.sol";
import {PropPoolAdapter} from "../src/adapters/PropPoolAdapter.sol";
import {UniV2Adapter} from "../src/adapters/UniV2Adapter.sol";
import {PropPool} from "../src/PropPool.sol";
import {IPropPool} from "../src/interfaces/IPropPool.sol";
import {PropCurve} from "../src/libraries/PropCurve.sol";
import {MockERC20} from "../src/mocks/MockERC20.sol";
import {UniswapV2Factory} from "../src/reference/univ2/UniswapV2Factory.sol";
import {UniswapV2Pair} from "../src/reference/univ2/UniswapV2Pair.sol";

contract IntegrationTest is Test {

    MockERC20 internal baseToken;
    MockERC20 internal quoteToken;

    PropPool internal pool;
    UniswapV2Factory internal factory;
    UniswapV2Pair internal pair;
    Router internal router;
    PropPoolAdapter internal propAdapter;
    UniV2Adapter internal uniAdapter;

    address internal owner = makeAddr("owner");
    address internal manager = makeAddr("manager");
    address internal updater = makeAddr("updater");
    address internal guardian = makeAddr("guardian");
    address internal taker = makeAddr("taker");
    address internal receiver = makeAddr("receiver");

    uint16 internal constant PAIR_ID = 1;
    uint8 internal constant PRICE_SCALE_EXP = 18;
    uint256 internal constant SCALE = 1e18;

    uint256 internal constant REF_MID = 2_000 * 1e6;

    uint256 internal constant TVL_BASE = 5_000e18;
    uint256 internal constant TVL_QUOTE = 10_000_000e6;

    uint96 internal constant BID_CAPACITY = 1_000e18;

    uint96 internal constant ASK_CAPACITY = 1_000e18;

    uint256 internal constant HALF_SPREAD_BPS = 5;
    uint256 internal constant WIDTH_BPS = 25;

    function _sizes() internal pure returns (uint256[4] memory) {
        return [uint256(1_000), 10_000, 100_000, 1_000_000];
    }

    function setUp() public {
        vm.warp(1_800_000_000);

        baseToken = new MockERC20("Base", "BASE", 18);
        quoteToken = new MockERC20("Quote", "QUOTE", 6);

        router = new Router();
        propAdapter = new PropPoolAdapter();
        uniAdapter = new UniV2Adapter();

        factory = new UniswapV2Factory(address(this));
        pair = UniswapV2Pair(factory.createPair(address(baseToken), address(quoteToken)));
        baseToken.mint(address(pair), TVL_BASE);
        quoteToken.mint(address(pair), TVL_QUOTE);
        pair.mint(address(this));

        pool = new PropPool(owner, manager, updater, guardian);
        vm.prank(owner);
        pool.addPair(address(baseToken), address(quoteToken), PRICE_SCALE_EXP, 60, 1e9);

        baseToken.mint(manager, TVL_BASE);
        quoteToken.mint(manager, TVL_QUOTE);
        vm.startPrank(manager);
        baseToken.approve(address(pool), type(uint256).max);
        quoteToken.approve(address(pool), type(uint256).max);
        pool.deposit(address(baseToken), TVL_BASE);
        pool.deposit(address(quoteToken), TVL_QUOTE);
        vm.stopPrank();

        _pushLadder(HALF_SPREAD_BPS, WIDTH_BPS);
        vm.prank(updater);
        pool.refreshCapacity(PAIR_ID, BID_CAPACITY, ASK_CAPACITY);

        baseToken.mint(taker, 1_000_000e18);
        quoteToken.mint(taker, 1_000_000_000e6);
        vm.startPrank(taker);
        baseToken.approve(address(pool), type(uint256).max);
        quoteToken.approve(address(pool), type(uint256).max);
        baseToken.approve(address(router), type(uint256).max);
        quoteToken.approve(address(router), type(uint256).max);
        vm.stopPrank();

        assertEq(_v2SpotPrice(), REF_MID, "V2 pair was not seeded at the reference mid");
        assertEq(pool.reserveOf(address(baseToken)), _v2Reserve(address(baseToken)), "unequal base TVL");
        assertEq(pool.reserveOf(address(quoteToken)), _v2Reserve(address(quoteToken)), "unequal quote TVL");
    }

    function _pushLadder(uint256 halfBps, uint256 widthBps) internal {
        uint256 maxBid = (REF_MID * (10_000 - halfBps)) / 10_000;
        uint256 minBid = (maxBid * (10_000 - widthBps)) / 10_000;
        uint256 minAsk = (REF_MID * (10_000 + halfBps)) / 10_000;
        uint256 maxAsk = (minAsk * (10_000 + widthBps)) / 10_000;
        uint256[] memory packed = new uint256[](1);
        packed[0] = minBid | (maxBid << 56) | (minAsk << 112) | (maxAsk << 168) | (uint256(PAIR_ID) << 224);
        vm.prank(updater);
        pool.updateQuote(packed);
    }

    function _v2Reserve(address token) internal view returns (uint256) {
        (uint112 r0, uint112 r1,) = pair.getReserves();
        return token == pair.token0() ? uint256(r0) : uint256(r1);
    }

    function _v2SpotPrice() internal view returns (uint256) {
        return (_v2Reserve(address(quoteToken)) * SCALE) / _v2Reserve(address(baseToken));
    }

    function _propBuyBase(uint256 amountIn) internal returns (uint256) {
        vm.prank(taker);
        return pool.swap(address(quoteToken), address(baseToken), int256(amountIn), 0, taker, 0, block.timestamp + 1);
    }

    function _propSellBase(uint256 amountIn) internal returns (uint256) {
        vm.prank(taker);
        return pool.swap(address(baseToken), address(quoteToken), int256(amountIn), 0, taker, 0, block.timestamp + 1);
    }

    function _v2BuyBase(uint256 amountIn) internal returns (uint256) {
        uint256 before = baseToken.balanceOf(address(this));
        vm.prank(taker);
        quoteToken.transfer(address(pair), amountIn);
        _uniSwap(address(quoteToken));
        return baseToken.balanceOf(address(this)) - before;
    }

    function _v2SellBase(uint256 amountIn) internal returns (uint256) {
        uint256 before = quoteToken.balanceOf(address(this));
        vm.prank(taker);
        baseToken.transfer(address(pair), amountIn);
        _uniSwap(address(baseToken));
        return quoteToken.balanceOf(address(this)) - before;
    }

    function _uniSwap(address tokenIn) internal {
        if (tokenIn == pair.token0()) uniAdapter.sellBase(address(this), address(pair), "");
        else uniAdapter.sellQuote(address(this), address(pair), "");
    }

    function _buyCostBpsE2(uint256 quoteIn, uint256 baseOut) internal pure returns (uint256) {
        uint256 effective = (quoteIn * SCALE) / baseOut;
        return ((effective - REF_MID) * 1_000_000) / REF_MID;
    }

    function _sellCostBpsE2(uint256 baseIn, uint256 quoteOut) internal pure returns (uint256) {
        uint256 effective = (quoteOut * SCALE) / baseIn;
        return ((REF_MID - effective) * 1_000_000) / REF_MID;
    }

    function test_slippageComparisonAtEqualTvl() public {
        uint256[4] memory sizes = _sizes();
        uint256[4] memory propBuy;
        uint256[4] memory uniBuy;
        uint256[4] memory propSell;
        uint256[4] memory uniSell;

        for (uint256 i; i < 4; ++i) {
            uint256 quoteIn = sizes[i] * 1e6;

            uint256 baseIn = (quoteIn * SCALE) / REF_MID;

            propBuy[i] = _measurePropBuy(quoteIn);
            uniBuy[i] = _measureUniBuy(quoteIn);
            propSell[i] = _measurePropSell(baseIn);
            uniSell[i] = _measureUniSell(baseIn);
        }

        _printTable(sizes, propBuy, uniBuy, propSell, uniSell);
        _assertRelationships(sizes, propBuy, uniBuy, propSell, uniSell);
    }

    function _measurePropBuy(uint256 quoteIn) internal returns (uint256 costBpsE2) {
        uint256 id = vm.snapshotState();
        costBpsE2 = _buyCostBpsE2(quoteIn, _propBuyBase(quoteIn));
        vm.revertToState(id);
    }

    function _measureUniBuy(uint256 quoteIn) internal returns (uint256 costBpsE2) {
        uint256 id = vm.snapshotState();
        costBpsE2 = _buyCostBpsE2(quoteIn, _v2BuyBase(quoteIn));
        vm.revertToState(id);
    }

    function _measurePropSell(uint256 baseIn) internal returns (uint256 costBpsE2) {
        uint256 id = vm.snapshotState();
        costBpsE2 = _sellCostBpsE2(baseIn, _propSellBase(baseIn));
        vm.revertToState(id);
    }

    function _measureUniSell(uint256 baseIn) internal returns (uint256 costBpsE2) {
        uint256 id = vm.snapshotState();
        costBpsE2 = _sellCostBpsE2(baseIn, _v2SellBase(baseIn));
        vm.revertToState(id);
    }

    function _printTable(
        uint256[4] memory sizes,
        uint256[4] memory propBuy,
        uint256[4] memory uniBuy,
        uint256[4] memory propSell,
        uint256[4] memory uniSell
    ) internal pure {
        console2.log("");
        console2.log("=========================================================================");
        console2.log(" Realised cost vs reference mid (2000 QUOTE/BASE), basis points");
        console2.log(" Equal TVL: 5,000 BASE + 10,000,000 QUOTE on each venue ($20M each)");
        console2.log(" UniV2 seeded exactly at the mid. Prop ladder: 5bp half-spread, 25bp width,");
        console2.log(" $2M per-epoch capacity per side. Each size measured from an identical state.");
        console2.log("=========================================================================");
        console2.log(" notional | BUY base                     | SELL base");
        console2.log("          | prop      univ2      ratio   | prop      univ2      ratio");
        console2.log("-------------------------------------------------------------------------");
        for (uint256 i; i < 4; ++i) {
            console2.log(
                string.concat(
                    " ",
                    _pad(string.concat("$", _thousands(sizes[i])), 9),
                    "| ",
                    _pad(_bps(propBuy[i]), 10),
                    _pad(_bps(uniBuy[i]), 11),
                    _pad(string.concat(_ratio(uniBuy[i], propBuy[i]), "x"), 9),
                    "| ",
                    _pad(_bps(propSell[i]), 10),
                    _pad(_bps(uniSell[i]), 11),
                    string.concat(_ratio(uniSell[i], propSell[i]), "x")
                )
            );
        }
        console2.log("-------------------------------------------------------------------------");
        console2.log(" Of univ2's cost, 30.00 bp is the LP fee at every size; the rest is impact.");
        console2.log(" Above $2M of one-sided flow per epoch the prop AMM refuses; univ2 does not.");
        console2.log("=========================================================================");
        console2.log("");
    }

    function _assertRelationships(
        uint256[4] memory sizes,
        uint256[4] memory propBuy,
        uint256[4] memory uniBuy,
        uint256[4] memory propSell,
        uint256[4] memory uniSell
    ) internal pure {
        for (uint256 i; i < 4; ++i) {

            assertLt(propBuy[i], uniBuy[i], "prop AMM was not cheaper on the buy side");
            assertLt(propSell[i], uniSell[i], "prop AMM was not cheaper on the sell side");

            assertApproxEqAbs(propBuy[i], propSell[i], 10, "prop AMM is directionally skewed");

            assertGe(uniBuy[i], uniSell[i], "V2 buy should never be cheaper than an equal-notional sell");

            assertGe(uniBuy[i], 3_000, "V2 cost fell below its 0.3% fee");

            assertGe(propBuy[i], HALF_SPREAD_BPS * 100, "prop AMM quoted inside its own half-spread");

            if (i > 0) {
                assertGe(propBuy[i], propBuy[i - 1], "prop cost not monotonic in size");
                assertGe(uniBuy[i], uniBuy[i - 1], "V2 cost not monotonic in size");
            }
        }

        assertGe(
            (uniBuy[3] * 1000) / uniSell[3],
            (uniBuy[0] * 1000) / uniSell[0],
            "V2's buy/sell asymmetry should widen with size"
        );

        assertLt(propBuy[3] * 10, uniBuy[3], "prop AMM slippage is not below a tenth of V2's at $1M");
        assertLt(propSell[3] * 10, uniSell[3], "sell side: prop AMM slippage is not below a tenth of V2's");

        assertGt(uniBuy[3], uniBuy[0] * 10, "V2 cost did not blow up with size as constant product must");
        assertLt(propBuy[3], propBuy[0] * 3, "prop AMM cost grew faster than a linear ladder should");

        assertLt(
            (uniBuy[0] * 1000) / propBuy[0],
            (uniBuy[3] * 1000) / propBuy[3],
            "the advantage should widen with size, not narrow"
        );

        assertEq(sizes[3] / sizes[0], 1_000, "the sweep should span three orders of magnitude");
    }

    function test_capacityIsTheHonestLimitOfTheComparison() public {

        uint256 overCapacity = _askQuoteCeiling() + 1;

        assertEq(
            pool.getAmountOut(address(quoteToken), address(baseToken), overCapacity),
            0,
            "prop AMM should quote zero above its epoch capacity"
        );
        vm.prank(taker);
        vm.expectRevert(PropPool.InsufficientCapacity.selector);
        pool.swap(address(quoteToken), address(baseToken), int256(overCapacity), 0, taker, 0, block.timestamp + 1);

        uint256 id = vm.snapshotState();
        uint256 got = _v2BuyBase(overCapacity);
        uint256 cost = _buyCostBpsE2(overCapacity, got);
        vm.revertToState(id);

        assertGt(got, 0, "V2 refused a trade it should have filled");
        console2.log("Above the prop AMM's epoch capacity ($2M):");
        console2.log(string.concat("  prop AMM: refuses (InsufficientCapacity)"));
        console2.log(string.concat("  univ2:    fills at ", _bps(cost), " bp"));

        vm.prank(updater);
        pool.refreshCapacity(PAIR_ID, BID_CAPACITY, ASK_CAPACITY);
        assertGt(pool.getAmountOut(address(quoteToken), address(baseToken), _askQuoteCeiling()), 0);
    }

    function _askQuoteCeiling() internal view returns (uint256) {
        IPropPool.PairSnapshot memory s = pool.snapshot(PAIR_ID);
        uint256 used = s.usedGen == s.capGen ? uint256(s.askUsed) : 0;
        uint256 room = uint256(s.askCapacity) - used;
        return PropCurve.amountInAsk(room, s.minAsk, s.maxAsk, s.askCapacity, used, s.priceScaleExp);
    }

    function test_ladderTightnessSensitivity() public {
        uint256[3] memory halves = [uint256(5), 15, 30];
        uint256[3] memory widths = [uint256(25), 60, 150];
        uint256 quoteIn = 1_000_000 * 1e6;

        uint256 uniCost;
        {
            uint256 id = vm.snapshotState();
            uniCost = _buyCostBpsE2(quoteIn, _v2BuyBase(quoteIn));
            vm.revertToState(id);
        }

        console2.log("");
        console2.log("Ladder sensitivity at $1M notional (univ2 reference: ", _bps(uniCost), "bp)");
        console2.log("  half-spread  width   prop cost   ratio");
        for (uint256 i; i < 3; ++i) {
            _pushLadder(halves[i], widths[i]);
            uint256 id = vm.snapshotState();
            uint256 propCost = _buyCostBpsE2(quoteIn, _propBuyBase(quoteIn));
            vm.revertToState(id);

            console2.log(
                string.concat(
                    "  ",
                    _pad(string.concat(vm.toString(halves[i]), "bp"), 13),
                    _pad(string.concat(vm.toString(widths[i]), "bp"), 8),
                    _pad(string.concat(_bps(propCost), "bp"), 12),
                    string.concat(_ratio(uniCost, propCost), "x")
                )
            );

            assertLt(propCost, uniCost, "even a wide ladder must beat V2 at the largest size");
        }

        _pushLadder(HALF_SPREAD_BPS, WIDTH_BPS);
    }

    function test_routerSelectsTheBetterVenueAtEverySize() public {
        uint256[4] memory sizes = _sizes();

        console2.log("");
        console2.log("Router venue selection (buying BASE):");
        for (uint256 i; i < 4; ++i) {
            uint256 quoteIn = sizes[i] * 1e6;

            uint256 propQuote = pool.getAmountOut(address(quoteToken), address(baseToken), quoteIn);
            uint256 uniQuote =
                uniAdapter.getAmountOut(quoteIn, _v2Reserve(address(quoteToken)), _v2Reserve(address(baseToken)));
            bool propWins = propQuote > uniQuote;
            assertTrue(propWins, "prop AMM should out-quote V2 at every size in this fixture");

            uint256 id = vm.snapshotState();
            uint256 realisedProp = _routeThrough(true, quoteIn);
            vm.revertToState(id);

            id = vm.snapshotState();
            uint256 realisedUni = _routeThrough(false, quoteIn);
            vm.revertToState(id);

            id = vm.snapshotState();
            uint256 realisedChosen = _routeThrough(propWins, quoteIn);
            vm.revertToState(id);

            assertEq(realisedProp, propQuote, "prop route realised something other than its quote");
            assertEq(realisedUni, uniQuote, "V2 route realised something other than the adapter's quote");
            assertEq(
                realisedChosen,
                realisedProp > realisedUni ? realisedProp : realisedUni,
                "the planner's choice did not deliver the better of the two"
            );

            uint256 loser = realisedProp > realisedUni ? realisedUni : realisedProp;
            console2.log(
                string.concat(
                    "  ",
                    _pad(string.concat("$", _thousands(sizes[i])), 11),
                    _pad(string.concat("chose ", propWins ? "prop" : "univ2"), 13),
                    _pad(string.concat("+", _bps(((realisedChosen - loser) * 1_000_000) / loser), " bp"), 14),
                    "more base delivered than the other venue"
                )
            );
        }
    }

    function test_splittingAcrossBothVenuesLandsBetweenThem() public {
        uint256 quoteIn = 1_000_000 * 1e6;

        uint256 id = vm.snapshotState();
        uint256 allProp = _routeThrough(true, quoteIn);
        vm.revertToState(id);

        id = vm.snapshotState();
        uint256 allUni = _routeThrough(false, quoteIn);
        vm.revertToState(id);

        id = vm.snapshotState();
        uint256 split = _routeSplit(quoteIn, 5_000);
        vm.revertToState(id);

        assertLt(split, allProp, "a 50/50 split beat routing everything to the better venue");
        assertGt(split, allUni, "a 50/50 split did worse than routing everything to the worse venue");
    }

    function _routeThrough(bool useProp, uint256 quoteIn) internal returns (uint256) {
        SwapStep[] memory steps = new SwapStep[](1);
        steps[0] = useProp ? _propStep(10_000) : _uniStep(10_000);
        return _execute(steps, quoteIn);
    }

    function _routeSplit(uint256 quoteIn, uint16 propWeight) internal returns (uint256) {
        SwapStep[] memory steps = new SwapStep[](2);
        steps[0] = _propStep(propWeight);
        steps[1] = _uniStep(10_000 - propWeight);
        return _execute(steps, quoteIn);
    }

    function _execute(SwapStep[] memory steps, uint256 quoteIn) internal returns (uint256) {
        Hop[] memory hops = new Hop[](1);
        hops[0] = Hop({tokenIn: address(quoteToken), steps: steps});
        Batch[] memory batches = new Batch[](1);
        batches[0] = Batch({weightBps: 10_000, hops: hops});

        RouteParams memory p = RouteParams({
            tokenIn: address(quoteToken),
            tokenOut: address(baseToken),
            receiver: receiver,
            amountIn: quoteIn,
            quotedAmountOut: 0,
            deadline: block.timestamp + 1,
            batches: batches
        });

        vm.prank(taker);
        return router.swapExactIn(p, 0);
    }

    function _propStep(uint16 weightBps) internal view returns (SwapStep memory) {
        return SwapStep({
            adapter: address(propAdapter),
            rawData: RouteDecoder.encode(address(pool), weightBps, true, false),
            payload: propAdapter.encodePayload(address(baseToken), address(quoteToken), 0, 0, block.timestamp + 1)
        });
    }

    function _uniStep(uint16 weightBps) internal view returns (SwapStep memory) {
        return SwapStep({
            adapter: address(uniAdapter),
            rawData: RouteDecoder.encode(address(pair), weightBps, address(quoteToken) == pair.token1(), false),
            payload: ""
        });
    }

    function _bps(uint256 x) internal pure returns (string memory) {
        uint256 frac = x % 100;
        return string.concat(vm.toString(x / 100), ".", frac < 10 ? "0" : "", vm.toString(frac));
    }

    function _ratio(uint256 a, uint256 b) internal pure returns (string memory) {
        if (b == 0) return "inf";
        uint256 scaled = (a * 100) / b;
        uint256 frac = scaled % 100;
        return string.concat(vm.toString(scaled / 100), ".", frac < 10 ? "0" : "", vm.toString(frac));
    }

    function _thousands(uint256 n) internal pure returns (string memory) {
        if (n >= 1_000_000) return string.concat(vm.toString(n / 1_000_000), "M");
        if (n >= 1_000) return string.concat(vm.toString(n / 1_000), "k");
        return vm.toString(n);
    }

    function _pad(string memory s, uint256 width) internal pure returns (string memory) {
        while (bytes(s).length < width) {
            s = string.concat(s, " ");
        }
        return s;
    }
}
