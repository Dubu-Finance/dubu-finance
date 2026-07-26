// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {console2} from "forge-std/console2.sol";

import {DubuScript, Deployment, Market, Roles} from "./DubuScript.sol";

import {Router, RouteParams, Batch, Hop, SwapStep} from "../src/Router.sol";
import {PropPool} from "../src/PropPool.sol";
import {IPropPool} from "../src/interfaces/IPropPool.sol";
import {PropCurve} from "../src/libraries/PropCurve.sol";
import {RouteDecoder} from "../src/libraries/RouteDecoder.sol";
import {PropPoolAdapter} from "../src/adapters/PropPoolAdapter.sol";
import {UniV2Adapter} from "../src/adapters/UniV2Adapter.sol";
import {MintableToken} from "../src/mocks/MintableToken.sol";
import {UniswapV2Pair} from "../src/reference/univ2/UniswapV2Pair.sol";
import {UniswapV2Router02} from "../src/reference/univ2/UniswapV2Router02.sol";

/// @title Demo — test/Integration.t.sol's comparison, executed on chain
///
/// ```
/// make demo                                  # broadcast against GIWA Sepolia
/// forge script script/Demo.s.sol:Demo \      # dry run against live chain state
///   --rpc-url https://sepolia-rpc.giwa.io --sender 0x5AD1... -vvv
/// ```
///
/// The pitch numbers have to be reproducible by a skeptic holding nothing but an RPC URL, not just
/// by our own test suite. So this script rebuilds the integration test's fixture out of real
/// deployed contracts and real transactions, using the same TVL, the same reference mid, the same
/// 5 bp / 25 bp ladder and the same size sweep.
///
/// =====================================================================================
/// HOW THE TWO VENUES ARE EQUALISED — the same five properties the test documents
/// =====================================================================================
///
/// 1. **Identical inventory, not merely identical notional.** Both venues get the same token
///    amounts: $10M of base and $10M of quote at the reference mid, $20M each. Equalising token
///    amounts rather than a dollar figure removes any question about which price was used.
///
/// 2. **The V2 pair's spot price *is* the reference mid**, asserted before a single trade is sent.
///    Seeded off-mid, the measured "V2 slippage" would silently include a mispricing that a real
///    arbitrageur removes in one block, and the comparison would be rigged.
///
/// 3. **The reference mid is not the prop AMM's own quote.** It is a constant both venues are
///    configured from — the prop ladder is built *around* it, the V2 pair is seeded *at* it.
///
/// 4. **Each size is measured from the same starting state.** The test brackets every measurement
///    with `vm.snapshotState()`. On a real chain there is no such thing, so each measurement is
///    followed by an explicit restore: `refreshCapacity` opens a fresh epoch on the prop AMM
///    (which is what zeroes the used counters, and the only thing that does), and the V2 pair is
///    burned back to dust and re-minted to the reference reserves exactly. Both restores are
///    asserted, not assumed.
///
/// 5. **Fees are inside the number.** V2 charges 0.30% and the prop AMM charges no explicit fee —
///    its spread *is* its fee. The metric is total cost to the taker versus the reference mid,
///    which is the only comparison that means anything to a user and the one that flatters V2
///    least. 30 of V2's basis points are the LP fee, at every size.
///
/// Every fill on both venues goes through the same `Router`, so no part of the difference is a
/// wrapper artefact. The prop leg is additionally cross-checked against `getAmountOut` — the
/// aggregator-facing view — so the number an integrator would have been quoted is provably the
/// number that settled.
contract Demo is DubuScript {
    /// @notice Conservative upper bound for the preflight.
    /// @dev A full two-market run against an empty env is 198 transactions; forge budgets
    ///      49,549,721 gas for them at its default multiplier, cold deployment included. Measured
    ///      on a fork of live GIWA state: 203 transactions and 13,655,800 gas actually consumed
    ///      for the demo alone, against an already-deployed stack. Sized against *limits* rather
    ///      than usage, for the reason `Deploy.GAS_BUDGET` documents. At the measured 0.001 gwei
    ///      this whole budget is 0.00012 ETH.
    uint256 internal constant GAS_BUDGET = 120_000_000;

    /// @dev Held in storage rather than threaded through every helper. A script's own storage
    ///      costs nothing anybody pays for — it lives in the local EVM and is never broadcast.
    Deployment internal D;
    address internal me;

    struct Sweep {
        uint256[4] sizes;
        uint256[4] propBuy;
        uint256[4] uniBuy;
        uint256[4] propSell;
        uint256[4] uniSell;
    }

    /// @notice The demo sweep: $1k, $10k, $100k, $1M of notional, in whole quote tokens.
    function _sizes() internal pure returns (uint256[4] memory) {
        return [uint256(1_000), 10_000, 100_000, 1_000_000];
    }

    // =====================================================================
    // Entry point
    // =====================================================================

    function run() external {
        me = msg.sender;
        Roles memory r = _roles(me);

        _rule();
        console2.log(" DuBu on-chain comparison sweep");
        _rule();
        console2.log(string.concat("  chain id              ", vm.toString(block.chainid)));
        console2.log(string.concat("  block                 ", vm.toString(block.number)));
        console2.log(string.concat("  sender                ", vm.toString(me)));
        _assertAffordable(me, GAS_BUDGET);
        _rule();
        console2.log("");

        vm.startBroadcast();
        Deployment memory d = _deployStack(me, r);
        require(_configureMarkets(d, me), "Demo: a PropPool pair could not be registered");
        vm.stopBroadcast();
        D = d;

        // Printed even when nothing was deployed: this script is the one people run, and a run
        // that had to build its own stack is exactly the run whose addresses would otherwise be
        // buried in a few hundred lines of transaction output.
        _printExports(d);

        _assertDemoRoles();

        uint256 selected = _envUint("DEMO_MARKETS", 3);
        for (uint256 i; i < MARKET_COUNT; ++i) {
            if ((selected >> i) & 1 == 0) continue;
            _runMarket(i);
        }

        _caveats();
    }

    /// @dev The demo drives all four roles: `deposit`/`sync` is the manager, `withdraw` is the
    ///      owner, `updateQuote`/`refreshCapacity` is the updater. Splitting them is right for
    ///      production and makes a single-key demo impossible, so say so precisely rather than
    ///      failing later with `NotUpdater` half way through a sweep.
    function _assertDemoRoles() internal view {
        PropPool pool = PropPool(D.pool);
        if (pool.owner() == me && pool.manager() == me && pool.updater() == me) return;

        console2.log("  This script seeds inventory (manager + owner) and pushes quotes (updater),");
        console2.log("  so the sender has to hold those roles. On this pool they are:");
        _logStep("      ", "owner", pool.owner());
        _logStep("      ", "manager", pool.manager());
        _logStep("      ", "updater", pool.updater());
        revert("Demo: sender does not hold owner/manager/updater on this pool");
    }

    // =====================================================================
    // One market, end to end
    // =====================================================================

    function _runMarket(uint256 i) internal {
        Market memory m = _market(i, D);
        _assertMarketIsSane(m);
        (uint16 pairId,) = PropPool(D.pool).pairIdFor(m.base, m.quote);
        require(pairId != 0, "Demo: market is not registered on the pool");

        console2.log("");
        _rule();
        console2.log(
            string.concat(
                " ",
                m.name,
                "   reference mid $",
                _thousands(m.midWhole),
                "   priceScaleExp ",
                vm.toString(uint256(m.priceScaleExp))
            )
        );
        _rule();

        vm.startBroadcast();
        _seed(m);
        _resetProp(m, pairId);
        vm.stopBroadcast();

        _assertFairness(m);

        vm.startBroadcast();
        Sweep memory s = _sweep(m, pairId);
        vm.stopBroadcast();

        _printTable(m, s);

        vm.startBroadcast();
        _seed(m); // back to the reference state before the routing pass
        _resetProp(m, pairId);
        _routerSelection(m, pairId);
        vm.stopBroadcast();

        _printCapacityLimit(m, pairId);
    }

    // ---------------------------------------------------------------------
    // Seeding — both venues to the identical reference state
    // ---------------------------------------------------------------------

    function _seed(Market memory m) internal {
        _approveMax(m.base, D.pool);
        _approveMax(m.quote, D.pool);
        _approveMax(m.base, D.router);
        _approveMax(m.quote, D.router);
        _approveMax(m.base, D.v2Router);
        _approveMax(m.quote, D.v2Router);

        _seedV2(m);
        _setPoolReserve(m.base, m.tvlBase);
        _setPoolReserve(m.quote, m.tvlQuote);
    }

    /// @dev First seed goes through `UniswapV2Router02.addLiquidity`, which is the path anybody
    ///      else adding liquidity to these pairs will use — and, with both reserves at zero, is
    ///      exact: `_addLiquidity` returns the desired amounts untouched. Every later restore uses
    ///      `_resetV2`, because the router's proportional-quote logic would land a few units off
    ///      the target and the mid assertion is exact.
    function _seedV2(Market memory m) internal {
        (uint112 r0, uint112 r1,) = UniswapV2Pair(m.pair).getReserves();
        if (r0 != 0 || r1 != 0) {
            _resetV2(m);
            return;
        }

        _ensureBalance(m.base, m.tvlBase);
        _ensureBalance(m.quote, m.tvlQuote);
        UniswapV2Router02(payable(D.v2Router))
            .addLiquidity(
                m.base, m.quote, m.tvlBase, m.tvlQuote, m.tvlBase, m.tvlQuote, me, block.timestamp + DEADLINE_SLACK
            );
    }

    /// @notice Put the V2 pair back to exactly the reference reserves.
    ///
    /// @dev There is no way to take tokens *out* of a V2 pair except by burning liquidity, and no
    ///      way to burn a precise amount of one side. So: burn everything the sender holds, which
    ///      leaves only the `MINIMUM_LIQUIDITY` dust and leaves it proportional; mint the exact
    ///      difference straight into the pair (both tokens are ours, so this is free); then `mint`,
    ///      whose `_update` writes reserves = balances. The result is exact, and it is asserted.
    function _resetV2(Market memory m) internal {
        UniswapV2Pair pair = UniswapV2Pair(m.pair);
        (uint256 t0, uint256 t1) = _v2Targets(m);

        (uint112 r0, uint112 r1,) = pair.getReserves();
        if (uint256(r0) == t0 && uint256(r1) == t1) return;

        uint256 lp = pair.balanceOf(me);
        if (lp != 0) {
            require(pair.transfer(m.pair, lp), "Demo: LP transfer to the pair failed");
            pair.burn(me);
            (r0, r1,) = pair.getReserves();
        }

        require(uint256(r0) <= t0 && uint256(r1) <= t1, "Demo: V2 reset cannot remove liquidity it does not own");
        if (t0 > uint256(r0)) MintableToken(pair.token0()).mint(m.pair, t0 - uint256(r0));
        if (t1 > uint256(r1)) MintableToken(pair.token1()).mint(m.pair, t1 - uint256(r1));
        pair.mint(me);

        (r0, r1,) = pair.getReserves();
        require(uint256(r0) == t0 && uint256(r1) == t1, "Demo: V2 reset did not land on the reference reserves");
    }

    /// @notice A fresh capacity epoch and a freshly stamped ladder.
    /// @dev Both, every time. `refreshCapacity` is what makes the used counters read as zero
    ///      again — a quote push deliberately does not, so that inducing price churn cannot
    ///      restore the pool's risk budget. Re-pushing the ladder on top is what keeps a run that
    ///      spans a few hundred sequential transactions from going stale mid-sweep.
    function _resetProp(Market memory m, uint16 pairId) internal {
        uint256[] memory packed = new uint256[](1);
        packed[0] = _packLadder(pairId, m.mid);
        PropPool(D.pool).updateQuote(packed);
        PropPool(D.pool).refreshCapacity(pairId, m.capacity, m.capacity);
    }

    /// @dev Fairness is checked, not trusted — this is the assertion the whole comparison rests on.
    function _assertFairness(Market memory m) internal view {
        uint256 spot = (_v2Reserve(m, m.quote) * m.scale) / _v2Reserve(m, m.base);
        if (spot != m.mid) {
            console2.log(string.concat("  v2 spot ", vm.toString(spot), " != reference mid ", vm.toString(m.mid)));
            revert("Demo: V2 pair is not seeded at the reference mid");
        }
        require(PropPool(D.pool).reserveOf(m.base) == _v2Reserve(m, m.base), "Demo: unequal base TVL");
        require(PropPool(D.pool).reserveOf(m.quote) == _v2Reserve(m, m.quote), "Demo: unequal quote TVL");

        console2.log(
            string.concat(
                "  both venues: ",
                _thousands(m.tvlBase / (10 ** uint256(m.baseDecimals))),
                " base + ",
                _thousands(m.tvlQuote / (10 ** uint256(m.quoteDecimals))),
                " quote;  v2 spot == reference mid == ",
                vm.toString(m.mid)
            )
        );
        console2.log(
            string.concat(
                "  ladder: ",
                vm.toString(HALF_SPREAD_BPS),
                "bp half-spread, ",
                vm.toString(WIDTH_BPS),
                "bp width;  capacity $",
                _thousands(CAPACITY_QUOTE_WHOLE),
                "/epoch per side"
            )
        );
    }

    // ---------------------------------------------------------------------
    // The sweep
    // ---------------------------------------------------------------------

    function _sweep(Market memory m, uint16 pairId) internal returns (Sweep memory s) {
        s.sizes = _sizes();
        for (uint256 i; i < 4; ++i) {
            uint256 quoteIn = s.sizes[i] * (10 ** uint256(m.quoteDecimals));
            // Equivalent base notional, so the two directions are the same dollar size.
            uint256 baseIn = (quoteIn * m.scale) / m.mid;

            s.propBuy[i] = _buyCostE2(m, quoteIn, _propSwap(m.quote, m.base, quoteIn));
            _resetProp(m, pairId);

            s.uniBuy[i] = _buyCostE2(m, quoteIn, _uniSwap(m, m.quote, m.base, quoteIn));
            _resetV2(m);

            s.propSell[i] = _sellCostE2(m, baseIn, _propSwap(m.base, m.quote, baseIn));
            _resetProp(m, pairId);

            s.uniSell[i] = _sellCostE2(m, baseIn, _uniSwap(m, m.base, m.quote, baseIn));
            _resetV2(m);
        }
    }

    /// @dev Exact-in against the prop AMM, cross-checked against the aggregator-facing view in the
    ///      same script run. `getAmountOut` is the integration path third parties use; if it ever
    ///      disagreed with what settles, every number in the table would be a quote rather than a
    ///      fill, and so would every quote an aggregator ever served.
    function _propSwap(address tokenIn, address tokenOut, uint256 amountIn) internal returns (uint256 amountOut) {
        _ensureBalance(tokenIn, amountIn);
        require(amountIn <= uint256(type(int256).max), "Demo: size does not fit the signed amount");

        uint256 quoted = PropPool(D.pool).getAmountOut(tokenIn, tokenOut, amountIn);
        require(quoted != 0, "Demo: prop AMM quoted zero (stale, paused, or over capacity)");

        // casting to 'int256' is safe because of the bound asserted immediately above; a positive
        // `specifiedAmount` is what selects the exact-input branch of `swap`.
        // forge-lint: disable-next-line(unsafe-typecast)
        int256 specified = int256(amountIn);
        amountOut = PropPool(D.pool).swap(tokenIn, tokenOut, specified, 0, me, 0, block.timestamp + DEADLINE_SLACK);
        require(amountOut == quoted, "Demo: prop AMM settled away from its own quote");
    }

    /// @dev The V2 leg, through the same `Router` and the same `UniV2Adapter` a routed trade uses,
    ///      in one transaction. Doing it as a raw transfer-then-adapter pair would leave the pair
    ///      holding unattributed tokens between two broadcast transactions.
    function _uniSwap(Market memory m, address tokenIn, address tokenOut, uint256 amountIn)
        internal
        returns (uint256 amountOut)
    {
        uint256 quoted =
            UniV2Adapter(D.uniAdapter).getAmountOut(amountIn, _v2Reserve(m, tokenIn), _v2Reserve(m, tokenOut));
        amountOut = _route(m, tokenIn, tokenOut, amountIn, false, quoted);
        require(amountOut == quoted, "Demo: V2 settled away from the adapter's own quote");
    }

    // ---------------------------------------------------------------------
    // Routing
    // ---------------------------------------------------------------------

    /// @notice One venue, 100% of the input, through the production Router.
    function _route(Market memory m, address tokenIn, address tokenOut, uint256 amountIn, bool useProp, uint256 quoted)
        internal
        returns (uint256)
    {
        _ensureBalance(tokenIn, amountIn);

        SwapStep[] memory steps = new SwapStep[](1);
        steps[0] = useProp ? _propStep(m, tokenIn) : _uniStep(m, tokenIn);

        Hop[] memory hops = new Hop[](1);
        hops[0] = Hop({tokenIn: tokenIn, steps: steps});

        Batch[] memory batches = new Batch[](1);
        batches[0] = Batch({weightBps: 10_000, hops: hops});

        RouteParams memory p = RouteParams({
            tokenIn: tokenIn,
            tokenOut: tokenOut,
            receiver: me,
            amountIn: amountIn,
            quotedAmountOut: quoted,
            deadline: block.timestamp + DEADLINE_SLACK,
            batches: batches
        });

        return Router(D.router).swapExactIn(p, 0);
    }

    /// @dev `sellQuote` is quote -> base on the pool, so `reverse` is set when the input is the
    ///      pair's quote token.
    function _propStep(Market memory m, address tokenIn) internal view returns (SwapStep memory) {
        return SwapStep({
            adapter: D.propAdapter,
            rawData: RouteDecoder.encode(D.pool, 10_000, tokenIn == m.quote, false),
            payload: PropPoolAdapter(D.propAdapter)
                .encodePayload(m.base, m.quote, 0, 0, block.timestamp + DEADLINE_SLACK)
        });
    }

    /// @dev `sellQuote` is token1 -> token0 on a V2 pair, so `reverse` follows the pair's own
    ///      address ordering rather than our notion of base and quote.
    function _uniStep(Market memory m, address tokenIn) internal view returns (SwapStep memory) {
        return SwapStep({
            adapter: D.uniAdapter,
            rawData: RouteDecoder.encode(m.pair, 10_000, tokenIn == UniswapV2Pair(m.pair).token1(), false),
            payload: ""
        });
    }

    /// @notice The router does no path finding — that is off-chain by design (archi_v2.md 6.2).
    ///         So "the router picks the better venue" is demonstrated the way it actually works:
    ///         quote both venues as the planner would, build the plan from the winner, execute it,
    ///         and check the realised output against the quote the plan was built on.
    function _routerSelection(Market memory m, uint16 pairId) internal {
        console2.log("");
        console2.log("  Router venue selection (buying base with quote)");
        console2.log("    notional   chose    prop quote     univ2 quote    realised       edge");

        uint256[4] memory sizes = _sizes();
        for (uint256 i; i < 4; ++i) {
            uint256 quoteIn = sizes[i] * (10 ** uint256(m.quoteDecimals));
            _ensureBalance(m.quote, quoteIn);

            uint256 propQuote = PropPool(D.pool).getAmountOut(m.quote, m.base, quoteIn);
            uint256 uniQuote =
                UniV2Adapter(D.uniAdapter).getAmountOut(quoteIn, _v2Reserve(m, m.quote), _v2Reserve(m, m.base));

            bool propWins = propQuote > uniQuote;

            uint256 realised = _route(m, m.quote, m.base, quoteIn, propWins, propWins ? propQuote : uniQuote);
            require(realised == (propWins ? propQuote : uniQuote), "Demo: the plan's venue did not deliver its quote");

            _logSelection(sizes[i], m.baseDecimals, propWins, propQuote, uniQuote, realised);

            _resetProp(m, pairId);
            _resetV2(m);
        }
        console2.log("    Quotes and fills are in whole base tokens. `edge` is how much more base the");
        console2.log("    chosen venue delivered than the other one would have, in bp.");
    }

    /// @dev Split out of `_routerSelection` purely because the two together do not fit the EVM's
    ///      16-slot stack without via-ir, and `via_ir` is off in this repo's default profile.
    function _logSelection(
        uint256 size,
        uint8 baseDecimals,
        bool propWins,
        uint256 propQuote,
        uint256 uniQuote,
        uint256 realised
    ) internal pure {
        uint256 best = propWins ? propQuote : uniQuote;
        uint256 worst = propWins ? uniQuote : propQuote;
        console2.log(
            string.concat(
                "    ",
                _pad(string.concat("$", _thousands(size)), 11),
                _pad(propWins ? "prop" : "univ2", 9),
                _pad(_units(propQuote, baseDecimals), 15),
                _pad(_units(uniQuote, baseDecimals), 15),
                _pad(_units(realised, baseDecimals), 15),
                string.concat("+", _bps(((best - worst) * 1_000_000) / worst), " bp")
            )
        );
    }

    // ---------------------------------------------------------------------
    // The honest limit: capacity
    // ---------------------------------------------------------------------

    /// @notice Above the epoch's capacity the prop AMM does not quote a worse price — it quotes
    ///         nothing and reverts. V2 fills any size. That is a different trade-off, not a
    ///         strictly worse one, and the deck has to say so.
    function _printCapacityLimit(Market memory m, uint16 pairId) internal view {
        IPropPool.PairSnapshot memory s = PropPool(D.pool).snapshot(pairId);
        uint256 used = s.usedGen == s.capGen ? uint256(s.askUsed) : 0;
        uint256 room = uint256(s.askCapacity) - used;
        // Ask capacity and usage are BASE units (PropCurve amendment 1), so the quote-denominated
        // ceiling an ask `amountIn` is measured against is the cost of all the epoch's remaining
        // base — not the capacity number itself.
        uint256 ceiling = PropCurve.amountInAsk(room, s.minAsk, s.maxAsk, s.askCapacity, used, s.priceScaleExp);
        uint256 over = ceiling + 1;

        require(
            PropPool(D.pool).getAmountOut(m.quote, m.base, over) == 0,
            "Demo: prop AMM quoted above its own epoch ceiling"
        );
        uint256 uniOut = UniV2Adapter(D.uniAdapter).getAmountOut(over, _v2Reserve(m, m.quote), _v2Reserve(m, m.base));

        console2.log("");
        console2.log(
            string.concat(
                "  One unit above the epoch's ask ceiling (",
                vm.toString(over),
                " quote units, ~$",
                _thousands(over / (10 ** uint256(m.quoteDecimals))),
                ")"
            )
        );
        console2.log("    prop AMM:  quotes 0, swap reverts InsufficientCapacity");
        console2.log(string.concat("    univ2:     fills at ", _bps(_buyCostE2(m, over, uniOut)), " bp"));
    }

    // ---------------------------------------------------------------------
    // Slippage metric — identical to test/Integration.t.sol
    // ---------------------------------------------------------------------

    /// @notice Realised cost versus the reference mid, in basis points scaled by 100.
    /// @dev Hundredths of a bp so the table shows two decimals with integer arithmetic. For a buy
    ///      the effective price is `quoteIn * scale / baseOut` in the pool's price units and the
    ///      cost is its excess over the mid; for a sell it is the shortfall. Both are positive.
    function _buyCostE2(Market memory m, uint256 quoteIn, uint256 baseOut) internal pure returns (uint256) {
        uint256 effective = (quoteIn * m.scale) / baseOut;
        return ((effective - m.mid) * 1_000_000) / m.mid;
    }

    function _sellCostE2(Market memory m, uint256 baseIn, uint256 quoteOut) internal pure returns (uint256) {
        uint256 effective = (quoteOut * m.scale) / baseIn;
        return ((m.mid - effective) * 1_000_000) / m.mid;
    }

    // ---------------------------------------------------------------------
    // Output
    // ---------------------------------------------------------------------

    function _printTable(Market memory m, Sweep memory s) internal pure {
        console2.log("");
        console2.log("=========================================================================");
        console2.log(
            string.concat(
                " Realised cost vs reference mid ($", _thousands(m.midWhole), " quote per base), basis points"
            )
        );
        console2.log(" Both venues seeded to $20M each. UniV2 seeded exactly at the mid.");
        console2.log(" Prop ladder: 5bp half-spread, 25bp width, $2M per-epoch capacity per side.");
        console2.log(" Every size measured from an identical, restored state. Executed on chain.");
        console2.log("=========================================================================");
        console2.log(" notional | BUY base                     | SELL base");
        console2.log("          | prop      univ2      ratio   | prop      univ2      ratio");
        console2.log("-------------------------------------------------------------------------");
        for (uint256 i; i < 4; ++i) {
            console2.log(
                string.concat(
                    " ",
                    _pad(string.concat("$", _thousands(s.sizes[i])), 9),
                    "| ",
                    _pad(_bps(s.propBuy[i]), 10),
                    _pad(_bps(s.uniBuy[i]), 11),
                    _pad(string.concat(_ratio(s.uniBuy[i], s.propBuy[i]), "x"), 9),
                    "| ",
                    _pad(_bps(s.propSell[i]), 10),
                    _pad(_bps(s.uniSell[i]), 11),
                    string.concat(_ratio(s.uniSell[i], s.propSell[i]), "x")
                )
            );
        }
        console2.log("-------------------------------------------------------------------------");
        console2.log(" Of univ2's cost, 30.00 bp is the LP fee at every size; the rest is impact.");
        console2.log("=========================================================================");
    }

    /// @notice The counterweights, printed here rather than only in the deck. A table that goes
    ///         out without them is a table somebody else gets to correct in public.
    function _caveats() internal pure {
        console2.log("");
        _rule();
        console2.log(" READ THIS NEXT TO THE TABLE");
        _rule();
        console2.log(" 1. At small sizes this is a fee comparison, not a curve comparison.");
        console2.log("    At $1k almost none of the gap is the shape of the curve: univ2 charges a");
        console2.log("    flat 30 bp fee and the prop AMM charges a 5 bp half-spread, and that");
        console2.log("    accounts for essentially the whole difference. A prop AMM quoting 30 bp");
        console2.log("    would show no advantage at all at that size. The curve only starts to");
        console2.log("    matter where constant-product impact does, which is the right-hand end of");
        console2.log("    the table.");
        console2.log("");
        console2.log(" 2. Above the epoch capacity the prop AMM refuses; univ2 fills.");
        console2.log("    Past ~$2M of one-sided flow per quote cycle the prop AMM stops quoting");
        console2.log("    entirely and the swap reverts, while univ2 keeps filling at any size --");
        console2.log("    badly, but it fills. A pool that refuses is not strictly better than a");
        console2.log("    pool that fills badly; it is a different trade-off. The capacity is a");
        console2.log("    risk budget, it resets only on refreshCapacity, and it is the mechanism");
        console2.log("    that bounds how much a stale ladder can be picked off for.");
        console2.log("");
        console2.log(" 3. Adverse selection is not modelled here, at all.");
        console2.log("    Univ2's fee is compensation for accepting flow blindly. The prop AMM's");
        console2.log("    spread is compensation for the same risk, but the prop AMM additionally");
        console2.log("    has to be RIGHT about the reference mid -- and in this script the mid is");
        console2.log("    right by fiat. On a live chain a stale ladder gets picked off and nothing");
        console2.log("    above measures that. These are execution-quality numbers for an honest");
        console2.log("    taker, which is what a taker cares about. They are NOT a claim about the");
        console2.log("    market maker's PnL.");
        console2.log("");
        console2.log(" 4. Both tokens are freely mintable and both venues were seeded by us.");
        console2.log("    GIWA has no canonical stablecoin, so there is no third-party liquidity to");
        console2.log("    compare against. This measures the curves, not the market.");
        _rule();
        console2.log("");
    }

    // ---------------------------------------------------------------------
    // Small helpers
    // ---------------------------------------------------------------------

    function _v2Reserve(Market memory m, address token) internal view returns (uint256) {
        (uint112 r0, uint112 r1,) = UniswapV2Pair(m.pair).getReserves();
        return token == UniswapV2Pair(m.pair).token0() ? uint256(r0) : uint256(r1);
    }

    function _v2Targets(Market memory m) internal view returns (uint256 t0, uint256 t1) {
        return UniswapV2Pair(m.pair).token0() == m.base ? (m.tvlBase, m.tvlQuote) : (m.tvlQuote, m.tvlBase);
    }

    /// @dev Mints the shortfall rather than a fixed float, so repeated runs do not compound.
    function _ensureBalance(address token, uint256 need) internal {
        uint256 have = MintableToken(token).balanceOf(me);
        if (have >= need) return;
        MintableToken(token).mint(me, need - have);
    }

    function _approveMax(address token, address spender) internal {
        if (MintableToken(token).allowance(me, spender) == type(uint256).max) return;
        MintableToken(token).approve(spender, type(uint256).max);
    }

    /// @dev Exactly, in both directions. The prop AMM's price does not depend on its inventory —
    ///      only on the ladder and the epoch counters — but the equal-TVL claim does, and it is
    ///      asserted, so the sweep's drift has to be undone rather than tolerated.
    function _setPoolReserve(address token, uint256 target) internal {
        uint256 have = PropPool(D.pool).reserveOf(token);
        if (have == target) return;
        if (have < target) {
            uint256 diff = target - have;
            _ensureBalance(token, diff);
            PropPool(D.pool).deposit(token, diff);
        } else {
            PropPool(D.pool).withdraw(token, have - target, me);
        }
    }
}
