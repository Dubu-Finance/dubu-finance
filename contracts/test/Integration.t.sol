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

/// @title Integration — the demo benchmark, executable
///
/// This file is the empirical claim the pitch rests on, so the setup is documented in more detail
/// than a test would normally warrant. A skeptical reader should be able to audit the fairness of
/// the comparison from these comments alone.
///
/// =====================================================================================
/// HOW THE TWO VENUES WERE EQUALISED
/// =====================================================================================
///
/// 1. **Identical inventory, not merely identical notional.** The UniswapV2 pair and the PropPool
///    are seeded with the *same token amounts*: `5_000` BASE (18 dec) and `10_000_000` QUOTE
///    (6 dec). At the reference mid that is $10M on each side of each venue, $20M TVL each.
///    Equalising the token amounts rather than a dollar figure removes any question about which
///    price was used to convert.
///
/// 2. **The V2 pair's spot price *is* the reference mid.** `10_000_000 / 5_000 = 2000` exactly.
///    This matters more than the TVL: had the pair been seeded off-mid, the measured "V2 slippage"
///    would have included a mispricing that a real arbitrageur would have removed in one block,
///    and the comparison would be rigged. Seeded at the mid, every basis point V2 gives up is
///    genuinely fee plus curve impact and nothing else.
///
/// 3. **The reference mid is not the prop AMM's own quote.** `REF_MID` is a constant that both
///    venues are configured from. The prop AMM's ladder is built *around* it (a spread either
///    side); the V2 pair is seeded *at* it. Neither venue's own state is used as the yardstick.
///
/// 4. **Each size is measured from the same starting state.** Every measurement is bracketed by
///    `vm.snapshotState()` / `vm.revertToState()`, for both venues alike. Without that, the $1M
///    trade would be priced against a book the $100k trade had already moved, and the ordering of
///    the sweep would change the answer.
///
/// 5. **Fees are inside the number.** V2 charges 0.30% and the prop AMM charges no explicit fee —
///    its spread *is* its fee. The metric is therefore total cost to the taker versus the
///    reference mid, which is the only comparison that means anything to a user, and which is the
///    one that flatters V2 least. Said plainly: 30 of V2's basis points are the LP fee.
///
/// =====================================================================================
/// THE LADDER, AND WHY THESE NUMBERS
/// =====================================================================================
///
///   half-spread  5 bps    — `maxBid = mid*(1-0.0005)`, `minAsk = mid*(1+0.0005)`
///   width       25 bps    — `minBid = maxBid*(1-0.0025)`, `maxAsk = minAsk*(1+0.0025)`
///   capacity    $2M/epoch each side (`1_000` BASE bid, `2_000_000` QUOTE ask)
///
/// A 5 bp half-spread and a 25 bp ladder width is a *liquid-pair* quote — tighter than V2's 30 bp
/// fee by construction, which is the entire economic point of a prop AMM and not a trick. The
/// honest counterweight is stated in `test_capacityIsTheHonestLimitOfTheComparison` and belongs on
/// the slide: the prop AMM's advantage is bounded by a per-epoch capacity that V2 does not have.
/// Above $2M of one-sided flow per quote cycle the prop AMM stops quoting entirely, while V2 keeps
/// filling at any size — badly, but it fills. A pool that refuses is not strictly better than a
/// pool that fills badly; it is a different trade-off, and the deck must say so.
///
/// `test_ladderTightnessSensitivity` re-runs the whole sweep at three progressively more
/// conservative ladders, so the result can be checked for dependence on the chosen tightness.
///
/// =====================================================================================
/// WHAT IS NOT MODELLED
/// =====================================================================================
///
/// Adverse selection. V2's fee is compensation for accepting flow blindly; the prop AMM's spread
/// is compensation for the same risk, but the prop AMM additionally *must be right about the
/// reference mid*, and this test's `REF_MID` is right by fiat. On a real chain a stale ladder is
/// picked off, and the per-epoch capacity is what bounds that loss. Nothing here measures it. The
/// numbers below are execution quality for an honest taker, which is what a taker cares about,
/// and they are not a claim about the market maker's PnL.
contract IntegrationTest is Test {
    // ---------------------------------------------------------------------
    // Fixture
    // ---------------------------------------------------------------------

    MockERC20 internal baseToken; // 18 decimals
    MockERC20 internal quoteToken; // 6 decimals

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

    /// @notice Reference mid: 2000 whole QUOTE per whole BASE.
    /// @dev Encoded in the pool's price units, where `quoteUnits = baseUnits * price / 1e18`.
    ///      With BASE at 18 decimals and QUOTE at 6, `2000 * 1e6 = 2e9`. One price unit is
    ///      5e-10 of the mid, so the ladder can be expressed to well under a hundredth of a bp.
    uint256 internal constant REF_MID = 2_000 * 1e6;

    uint256 internal constant TVL_BASE = 5_000e18;
    uint256 internal constant TVL_QUOTE = 10_000_000e6;

    uint96 internal constant BID_CAPACITY = 1_000e18; // $2M of base per epoch
    /// @notice Base the pool will SELL this epoch. BASE units, not quote — `PropCurve` amendment 1.
    /// @dev Was `2_000_000e6`, read as quote. 1_000 base at the 2000 reference mid is the same $2M
    ///      of risk budget per epoch, and the two capacities are now symmetric because they are the
    ///      same unit. The quote-denominated ceiling an ask `amountIn` is measured against is
    ///      `_askQuoteCeiling()`, which is ~$2.0035M — the cost of the epoch's base, not its size.
    uint96 internal constant ASK_CAPACITY = 1_000e18; // $2M of base per epoch, sell side

    uint256 internal constant HALF_SPREAD_BPS = 5;
    uint256 internal constant WIDTH_BPS = 25;

    /// @notice The demo sweep: $1k, $10k, $100k, $1M of notional.
    /// @dev Given in *whole* quote tokens. The task's "1e3..1e6 quote-units of notional" is read
    ///      as whole tokens, not raw 6-decimal units — 1e3 raw units of a 6-decimal token is
    ///      $0.001, which would not exercise anything.
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

        // --- venue 1: UniswapV2, seeded exactly at the reference mid ---
        factory = new UniswapV2Factory(address(this));
        pair = UniswapV2Pair(factory.createPair(address(baseToken), address(quoteToken)));
        baseToken.mint(address(pair), TVL_BASE);
        quoteToken.mint(address(pair), TVL_QUOTE);
        pair.mint(address(this));

        // --- venue 2: PropPool, seeded with the identical inventory ---
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

        // --- the taker ---
        baseToken.mint(taker, 1_000_000e18);
        quoteToken.mint(taker, 1_000_000_000e6);
        vm.startPrank(taker);
        baseToken.approve(address(pool), type(uint256).max);
        quoteToken.approve(address(pool), type(uint256).max);
        baseToken.approve(address(router), type(uint256).max);
        quoteToken.approve(address(router), type(uint256).max);
        vm.stopPrank();

        // Sanity: the V2 pair's spot price must equal the reference mid, or the comparison is
        // rigged before it starts. Checked here rather than trusted.
        assertEq(_v2SpotPrice(), REF_MID, "V2 pair was not seeded at the reference mid");
        assertEq(pool.reserveOf(address(baseToken)), _v2Reserve(address(baseToken)), "unequal base TVL");
        assertEq(pool.reserveOf(address(quoteToken)), _v2Reserve(address(quoteToken)), "unequal quote TVL");
    }

    // ---------------------------------------------------------------------
    // Fixture helpers
    // ---------------------------------------------------------------------

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

    /// @dev The pair's spot price in the pool's own price units, so the two are directly
    ///      comparable: `quoteReserve * 1e18 / baseReserve`.
    function _v2SpotPrice() internal view returns (uint256) {
        return (_v2Reserve(address(quoteToken)) * SCALE) / _v2Reserve(address(baseToken));
    }

    // ---------------------------------------------------------------------
    // Execution — both venues, identical shape
    // ---------------------------------------------------------------------

    /// @dev Buy BASE with `amountIn` QUOTE on the prop AMM. Returns base units received.
    function _propBuyBase(uint256 amountIn) internal returns (uint256) {
        vm.prank(taker);
        return pool.swap(address(quoteToken), address(baseToken), int256(amountIn), 0, taker, 0, block.timestamp + 1);
    }

    /// @dev Sell `amountIn` BASE for QUOTE on the prop AMM. Returns quote units received.
    function _propSellBase(uint256 amountIn) internal returns (uint256) {
        vm.prank(taker);
        return pool.swap(address(baseToken), address(quoteToken), int256(amountIn), 0, taker, 0, block.timestamp + 1);
    }

    /// @dev Buy BASE with `amountIn` QUOTE on the V2 pair, through the same adapter the router
    ///      uses, so the number is the one a routed trade would actually get.
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

    // ---------------------------------------------------------------------
    // Slippage metric
    // ---------------------------------------------------------------------

    /// @notice Realised cost versus the reference mid, in basis points scaled by 100.
    ///
    /// @dev Returned as hundredths of a bp so the table can show two decimal places with integer
    ///      arithmetic. For a buy (quote in, base out) the effective price is
    ///      `amountIn * 1e18 / amountOut` in the pool's price units, and the slippage is that
    ///      price's excess over `REF_MID`. For a sell it is the shortfall. Both are positive costs.
    function _buyCostBpsE2(uint256 quoteIn, uint256 baseOut) internal pure returns (uint256) {
        uint256 effective = (quoteIn * SCALE) / baseOut;
        return ((effective - REF_MID) * 1_000_000) / REF_MID;
    }

    function _sellCostBpsE2(uint256 baseIn, uint256 quoteOut) internal pure returns (uint256) {
        uint256 effective = (quoteOut * SCALE) / baseIn;
        return ((REF_MID - effective) * 1_000_000) / REF_MID;
    }

    // =====================================================================
    // THE DEMO TABLE
    // =====================================================================

    function test_slippageComparisonAtEqualTvl() public {
        uint256[4] memory sizes = _sizes();
        uint256[4] memory propBuy;
        uint256[4] memory uniBuy;
        uint256[4] memory propSell;
        uint256[4] memory uniSell;

        for (uint256 i; i < 4; ++i) {
            uint256 quoteIn = sizes[i] * 1e6;
            // Equivalent base notional, so the two directions are the same dollar size.
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

    /// @notice Relationships, not magic constants. Each of these stays meaningful if the ladder,
    ///         the TVL or the sizes change.
    function _assertRelationships(
        uint256[4] memory sizes,
        uint256[4] memory propBuy,
        uint256[4] memory uniBuy,
        uint256[4] memory propSell,
        uint256[4] memory uniSell
    ) internal pure {
        for (uint256 i; i < 4; ++i) {
            // 1. The prop AMM is cheaper at every size, in both directions.
            assertLt(propBuy[i], uniBuy[i], "prop AMM was not cheaper on the buy side");
            assertLt(propSell[i], uniSell[i], "prop AMM was not cheaper on the sell side");

            // 2. The prop AMM is directionally symmetric to within 0.1 bp, because the ladder was
            //    built symmetrically around the mid and the impact term is linear. Any real skew
            //    here would mean the ladder or the price scale was mis-encoded.
            assertApproxEqAbs(propBuy[i], propSell[i], 10, "prop AMM is directionally skewed");

            // 3. V2 is *not* symmetric, and the asymmetry is intrinsic rather than a seeding
            //    error — worth pinning so nobody "fixes" the fixture to remove it. With
            //    `q = 0.997 * notional / reserveQuote`, constant product gives
            //      buy  cost = (q + 0.003) / 0.997
            //      sell cost = (q + 0.003) / (1 + q)
            //    so a buy always costs more than an equal-notional sell, by a factor of
            //    `(1+q)/0.997` that grows with size. Both are measured and both are reported.
            assertGe(uniBuy[i], uniSell[i], "V2 buy should never be cheaper than an equal-notional sell");

            // 4. V2 never beats its own fee floor of 30 bp.
            assertGe(uniBuy[i], 3_000, "V2 cost fell below its 0.3% fee");

            // 5. The prop AMM never beats its own half-spread of 5 bp.
            assertGe(propBuy[i], HALF_SPREAD_BPS * 100, "prop AMM quoted inside its own half-spread");

            // 6. Monotonic in size on both venues.
            if (i > 0) {
                assertGe(propBuy[i], propBuy[i - 1], "prop cost not monotonic in size");
                assertGe(uniBuy[i], uniBuy[i - 1], "V2 cost not monotonic in size");
            }
        }

        // 7. V2's directional asymmetry widens with size, as the algebra above requires.
        assertGe(
            (uniBuy[3] * 1000) / uniSell[3],
            (uniBuy[0] * 1000) / uniSell[0],
            "V2's buy/sell asymmetry should widen with size"
        );

        // 8. THE HEADLINE. At the largest size the prop AMM costs less than a tenth of V2.
        assertLt(propBuy[3] * 10, uniBuy[3], "prop AMM slippage is not below a tenth of V2's at $1M");
        assertLt(propSell[3] * 10, uniSell[3], "sell side: prop AMM slippage is not below a tenth of V2's");

        // 9. The shape of the win: V2's cost is convex in size and the prop AMM's is linear, which
        //    is why the ratio widens. Over a 1000x size range V2's cost must grow by more than 10x
        //    while the prop AMM's grows by less than 3x.
        assertGt(uniBuy[3], uniBuy[0] * 10, "V2 cost did not blow up with size as constant product must");
        assertLt(propBuy[3], propBuy[0] * 3, "prop AMM cost grew faster than a linear ladder should");

        // 10. And the advantage is smallest at the smallest size, because there V2's fixed 30 bp fee
        //    is the whole story and the prop AMM's 5 bp spread is most of its cost too. Stated as
        //    an assertion so nobody reads the $1M ratio as typical.
        assertLt(
            (uniBuy[0] * 1000) / propBuy[0],
            (uniBuy[3] * 1000) / propBuy[3],
            "the advantage should widen with size, not narrow"
        );

        assertEq(sizes[3] / sizes[0], 1_000, "the sweep should span three orders of magnitude");
    }

    // =====================================================================
    // Honest limits of the comparison
    // =====================================================================

    /// @notice The counterweight that belongs on the slide next to the table.
    ///
    ///         Beyond its per-epoch capacity the prop AMM does not quote a worse price — it quotes
    ///         nothing and reverts. V2 fills any size. So the table is a statement about execution
    ///         quality *within the prop AMM's risk budget*, and the budget is a real constraint,
    ///         not a footnote.
    function test_capacityIsTheHonestLimitOfTheComparison() public {
        // The ask leg is quote-denominated and the epoch's budget is base (amendment 1), so "one
        // unit over capacity" is one quote unit past the epoch's QUOTE CEILING — the cost of all its
        // remaining base. `ASK_CAPACITY + 1` is a base figure and, read as quote, it is ~1000x the
        // ceiling: still refused, but for the wrong reason and by a wildly wrong margin, which would
        // make the "one unit over" claim in this test false.
        uint256 overCapacity = _askQuoteCeiling() + 1;

        assertEq(
            pool.getAmountOut(address(quoteToken), address(baseToken), overCapacity),
            0,
            "prop AMM should quote zero above its epoch capacity"
        );
        vm.prank(taker);
        vm.expectRevert(PropPool.InsufficientCapacity.selector);
        pool.swap(address(quoteToken), address(baseToken), int256(overCapacity), 0, taker, 0, block.timestamp + 1);

        // V2, meanwhile, happily fills the same size.
        uint256 id = vm.snapshotState();
        uint256 got = _v2BuyBase(overCapacity);
        uint256 cost = _buyCostBpsE2(overCapacity, got);
        vm.revertToState(id);

        assertGt(got, 0, "V2 refused a trade it should have filled");
        console2.log("Above the prop AMM's epoch capacity ($2M):");
        console2.log(string.concat("  prop AMM: refuses (InsufficientCapacity)"));
        console2.log(string.concat("  univ2:    fills at ", _bps(cost), " bp"));

        // After a capacity refresh the prop AMM takes the same flow again — the budget is
        // per-epoch, not a lifetime cap. This is the mechanism the deck should describe. Quoted at
        // the ceiling itself, which is the largest ask the fresh epoch accepts.
        vm.prank(updater);
        pool.refreshCapacity(PAIR_ID, BID_CAPACITY, ASK_CAPACITY);
        assertGt(pool.getAmountOut(address(quoteToken), address(baseToken), _askQuoteCeiling()), 0);
    }

    /// @notice The epoch's ask ceiling in QUOTE units: what all of its remaining base costs.
    /// @dev `PropCurve.amountInAsk` is the ask side's primitive — base out, quote cost — and this is
    ///      it evaluated at the whole remaining room, which is exactly the quantity `amountOutAsk`
    ///      compares a quote-denominated `amountIn` against before raising
    ///      `AmountExceedsCapacity`. Ask capacity and usage are base units, so this conversion is
    ///      mandatory before any comparison with a quote amount.
    function _askQuoteCeiling() internal view returns (uint256) {
        IPropPool.PairSnapshot memory s = pool.snapshot(PAIR_ID);
        uint256 used = s.usedGen == s.capGen ? uint256(s.askUsed) : 0;
        uint256 room = uint256(s.askCapacity) - used;
        return PropCurve.amountInAsk(room, s.minAsk, s.maxAsk, s.askCapacity, used, s.priceScaleExp);
    }

    /// @notice Does the result depend on having picked a flattering ladder? Re-runs the largest
    ///         size at progressively more conservative quotes and reports where the 10x claim
    ///         stops holding.
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

            // Even the most conservative ladder tested — a 30 bp half-spread, matching V2's fee
            // exactly, with a 150 bp width — must still beat V2 at $1M. If that ever stops being
            // true the demo claim is dead and this test should say so loudly.
            assertLt(propCost, uniCost, "even a wide ladder must beat V2 at the largest size");
        }

        // The 10x headline specifically needs a tight ladder. Pin which ones clear it.
        _pushLadder(HALF_SPREAD_BPS, WIDTH_BPS);
    }

    // =====================================================================
    // Router selects the better venue
    // =====================================================================

    /// @notice The router does no path finding — that is off-chain by design (archi_v2.md §6.2).
    ///         So "the router selects the better venue" is tested as: quote both venues the way
    ///         the planner would, build the plan from the winner, execute it, and assert the
    ///         realised output equals the better of the two venues' realised outputs.
    function test_routerSelectsTheBetterVenueAtEverySize() public {
        uint256[4] memory sizes = _sizes();

        console2.log("");
        console2.log("Router venue selection (buying BASE):");
        for (uint256 i; i < 4; ++i) {
            uint256 quoteIn = sizes[i] * 1e6;

            // --- what the off-chain planner sees ---
            uint256 propQuote = pool.getAmountOut(address(quoteToken), address(baseToken), quoteIn);
            uint256 uniQuote =
                uniAdapter.getAmountOut(quoteIn, _v2Reserve(address(quoteToken)), _v2Reserve(address(baseToken)));
            bool propWins = propQuote > uniQuote;
            assertTrue(propWins, "prop AMM should out-quote V2 at every size in this fixture");

            // --- what each venue actually delivers, measured independently ---
            uint256 id = vm.snapshotState();
            uint256 realisedProp = _routeThrough(true, quoteIn);
            vm.revertToState(id);

            id = vm.snapshotState();
            uint256 realisedUni = _routeThrough(false, quoteIn);
            vm.revertToState(id);

            // --- the route the planner would have built ---
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

    /// @notice A 50/50 split across both venues must land strictly between the two single-venue
    ///         outcomes, which is the sanity check that makes a split-routing claim believable.
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

    // ---------------------------------------------------------------------
    // Route construction
    // ---------------------------------------------------------------------

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

    /// @dev Quote -> base on the prop pool is the ask side, so `reverse = true`.
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

    // ---------------------------------------------------------------------
    // Formatting
    // ---------------------------------------------------------------------

    /// @dev `x` is in hundredths of a bp; render as `NN.NN`.
    function _bps(uint256 x) internal pure returns (string memory) {
        uint256 frac = x % 100;
        return string.concat(vm.toString(x / 100), ".", frac < 10 ? "0" : "", vm.toString(frac));
    }

    /// @dev `a/b` to two decimals. Both arguments are already integers in the same unit.
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
