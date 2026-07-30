// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {console2} from "forge-std/console2.sol";

import {DubuScript, Deployment} from "./DubuScript.sol";
import {FlowModel} from "./lib/FlowModel.sol";

import {PropPool} from "../src/PropPool.sol";
import {IPropPool} from "../src/interfaces/IPropPool.sol";
import {MintableToken} from "../src/mocks/MintableToken.sol";

contract Simulate is DubuScript {

    uint256 internal constant GAS_BUDGET = 40_000_000;

    address internal constant LIVE_POOL = 0xBbE55E29BbC6d71EcAb1ac011c9Ac5206aB2Fe74;
    address internal constant LIVE_MUSDC = 0xd28596C6750D87C53EA146134AfAB53de86C5155;
    address internal constant LIVE_MWETH = 0x81e46C6379498beBEB5DCcD47ab2DdFaf967d445;
    address internal constant LIVE_MWBTC = 0x3548991B5EF2D7805EFa95bEa6CeDeAee3869875;

    Deployment internal D;
    address internal me;
    bool internal broadcasting;

    bool internal refIsExternal;

    struct Live {
        uint16 pairId;
        bool quotable;
        string reason;
        uint256 ref;
        uint8 exp;
        uint256 scale;
    }

    struct Tally {
        uint256 fills;
        uint256 declined;
        uint256 notional;
        int256 spread;
        uint256 quoteToMint;
        uint256 baseToMint;
    }

    function run() external {
        me = msg.sender;
        broadcasting = _envUint("SIMULATE_BROADCAST", 0) == 1;

        _header();

        D = _resolveDeployment();
        _printAddresses();

        uint256 selected = _envUint("SIM_MARKETS", 3);
        uint256 quotable;
        for (uint256 i; i < MARKET_COUNT; ++i) {
            if ((selected >> i) & 1 == 0) continue;
            if (_runMarket(i)) ++quotable;
        }

        if (quotable == 0) _noQuotableMarkets();
        _caveats();
    }

    function _header() internal view {
        _rule();
        console2.log(" DuBu taker-flow simulator -- MODE 2, against live chain state");
        _rule();
        console2.log(string.concat("  chain id              ", vm.toString(block.chainid)));
        console2.log(string.concat("  block                 ", vm.toString(block.number)));
        console2.log(string.concat("  timestamp             ", vm.toString(block.timestamp)));
        console2.log(string.concat("  sender (the taker)    ", vm.toString(me)));
        console2.log(string.concat("  seed                  ", vm.toString(_envUint("SIM_SEED", 20_260_727))));
        console2.log("");
        if (broadcasting) {
            console2.log("  MODE: BROADCAST. Real transactions will be signed and sent.");
            console2.log("  This CONSUMES the live pool's epoch capacity and MOVES its inventory. If the");
            console2.log("  updater bot is running, its risk module will attribute these fills as trade PnL.");
            _assertAffordable(me, GAS_BUDGET);
        } else {
            console2.log("  MODE: DRY RUN. Nothing is signed and nothing is sent.");
            console2.log("  Everything below is still EXECUTED, in a local EVM forked at the block above, so");
            console2.log("  the fills are real fills against real state -- they just never leave this process.");
            console2.log("  Set SIMULATE_BROADCAST=1 (make simulate) to send them.");
        }
        _rule();
    }

    function _resolveDeployment() internal view returns (Deployment memory d) {
        d.mUsdc = _envAddr("MUSDC", LIVE_MUSDC);
        d.mWeth = _envAddr("MWETH", LIVE_MWETH);
        d.mWbtc = _envAddr("MWBTC", LIVE_MWBTC);
        d.pool = _envAddr("PROP_POOL", LIVE_POOL);

        require(d.pool.code.length != 0, "Simulate: no contract at PROP_POOL on this chain");
        require(d.mUsdc.code.length != 0, "Simulate: no contract at MUSDC on this chain");
        require(d.mWeth.code.length != 0, "Simulate: no contract at MWETH on this chain");
        require(d.mWbtc.code.length != 0, "Simulate: no contract at MWBTC on this chain");
    }

    function _printAddresses() internal view {
        console2.log("");
        _logStep("      ", "PropPool", D.pool);
        _logStep("      ", "mUSDC", D.mUsdc);
        _logStep("      ", "mWETH", D.mWeth);
        _logStep("      ", "mWBTC", D.mWbtc);
        console2.log(
            string.concat(
                "         pairs ",
                vm.toString(uint256(PropPool(D.pool).pairCount())),
                "   globally paused: ",
                PropPool(D.pool).allPaused() ? "YES" : "no"
            )
        );
    }

    function _runMarket(uint256 i) internal returns (bool quotable) {
        Live memory L = _diagnose(i);

        console2.log("");
        _rule();
        (string memory name,,,) = _marketSpec(i);
        console2.log(string.concat(" ", name, "   pairId ", vm.toString(uint256(L.pairId))));
        _rule();

        if (!L.quotable) {
            console2.log(string.concat("  NOT QUOTABLE: ", L.reason));
            _explainNotQuotable();
            return false;
        }

        _printExposure(i, L);
        _runFlow(i, L);
        return true;
    }

    function _diagnose(uint256 i) internal returns (Live memory L) {
        (address base, address quote) = _tokensOf(i);
        (L.pairId,) = PropPool(D.pool).pairIdFor(base, quote);
        if (L.pairId == 0) {
            L.reason = "the pair is not registered on this pool";
            return L;
        }

        IPropPool.PairSnapshot memory s = PropPool(D.pool).snapshot(L.pairId);
        L.exp = s.priceScaleExp;
        L.scale = 10 ** uint256(s.priceScaleExp);
        (L.ref, refIsExternal) = _referenceFor(i, s);

        if (PropPool(D.pool).allPaused()) {
            L.reason = "the pool is globally paused (guardian pauseAll)";
            return L;
        }
        if (s.flags & 1 != 0) {
            L.reason = "this pair is paused (guardian pause)";
            return L;
        }
        if (s.updatedAt == 0) {
            L.reason = "no ladder has ever been posted for this pair";
            return L;
        }
        if (block.timestamp > s.updatedAt && block.timestamp - s.updatedAt > s.maxStaleSecs) {
            L.reason = string.concat(
                "the ladder is stale: ",
                vm.toString(block.timestamp - s.updatedAt),
                "s old against a ",
                vm.toString(uint256(s.maxStaleSecs)),
                "s window"
            );
            return L;
        }
        if (s.bidCapacity == 0 && s.askCapacity == 0) {
            L.reason = "both capacities are zero -- the updater has withdrawn its quotes";
            return L;
        }

        (uint256 bidCap, uint256 bidUsed) = FlowModel.room(s, true);
        (uint256 askCap, uint256 askUsed) = FlowModel.room(s, false);
        if (bidUsed >= bidCap && askUsed >= askCap) {
            L.reason = "both epochs are fully consumed; only refreshCapacity reopens them";
            return L;
        }
        if (PropPool(D.pool).reserveOf(base) == 0 && PropPool(D.pool).reserveOf(quote) == 0) {
            L.reason = "the pool holds no inventory for this pair";
            return L;
        }

        L.quotable = true;
    }

    function _explainNotQuotable() internal pure {
        console2.log("");
        console2.log("  This is the expected state when the updater bot is NOT running: it withdraws");
        console2.log("  capacity on shutdown, which leaves the ladder intact and the pool quoting zero.");
        console2.log("  `getAmountOut` returns 0 rather than reverting (IPropPool's no-revert obligation),");
        console2.log("  so an aggregator sees no liquidity rather than a broken pair -- which is correct,");
        console2.log("  and is also why this has to be diagnosed explicitly instead of inferred from a 0.");
        console2.log("");
        console2.log("  To make this market simulable, start the updater:");
        console2.log("    cd ../engine && cargo run -p dubu-updater -- --config crates/dubu-updater/updater.toml");
        console2.log("  Nothing in this script will do it for you: pushing a ladder needs the updater role,");
        console2.log("  and a simulator that quotes its own book is measuring itself.");
    }

    function _referenceFor(uint256 i, IPropPool.PairSnapshot memory s)
        internal
        view
        returns (uint256 ref, bool external_)
    {
        (, uint8 baseDecimals, uint8 quoteDecimals,) = _marketSpec(i);
        uint256 cents = _envUint(string.concat("SIM_REF_", vm.toString(i + 1)), 0);

        if (cents != 0) {
            ref = (cents * (10 ** uint256(quoteDecimals)) * (10 ** uint256(s.priceScaleExp)))
                / (100 * (10 ** uint256(baseDecimals)));
            return (ref, true);
        }

        return ((uint256(s.maxBid) + uint256(s.minAsk)) / 2, false);
    }

    function _printExposure(uint256 i, Live memory L) internal view {
        (address base, address quote) = _tokensOf(i);
        IPropPool.PairSnapshot memory s = PropPool(D.pool).snapshot(L.pairId);

        console2.log(
            string.concat(
                "  ladder age ",
                vm.toString(block.timestamp - s.updatedAt),
                "s / ",
                vm.toString(uint256(s.maxStaleSecs)),
                "s      priceScaleExp ",
                vm.toString(uint256(s.priceScaleExp))
            )
        );
        _printEpoch("bid", s, true);
        _printEpoch("ask", s, false);
        console2.log(
            string.concat(
                "  inventory   base ",
                _units(PropPool(D.pool).reserveOf(base), _baseDecimalsOf(i)),
                "   quote ",
                _units(PropPool(D.pool).reserveOf(quote), 6)
            )
        );

        console2.log("");
        if (refIsExternal) {
            console2.log(
                string.concat("  reference (SIM_REF_", vm.toString(i + 1), ", operator-supplied)  ", vm.toString(L.ref))
            );
        } else {
            console2.log(string.concat("  reference (POOL'S OWN LADDER MIDPOINT)  ", vm.toString(L.ref)));
            console2.log("  >> SIM_REF is unset, so the reference IS the pool's quote. Every edge below is");
            console2.log("  >> zero by construction and NOTHING here measures adverse selection. Supply");
            console2.log("  >> SIM_REF_1 / SIM_REF_2 in cents (194382 = $1,943.82) for a real measurement.");
        }

        (int256 bidEdge, int256 askEdge) = FlowModel.edges(s, L.ref);
        console2.log(
            string.concat(
                "  executable top:  bid ",
                _bpsSigned(bidEdge),
                " bp vs reference    ask ",
                _bpsSigned(askEdge),
                " bp vs reference"
            )
        );
        console2.log("  (negative is the pool's edge: what a taker gives up. Positive is a free option.)");

        _printFreeOption(i, L, s, bidEdge, askEdge);
    }

    function _printEpoch(string memory side, IPropPool.PairSnapshot memory s, bool isBid) internal pure {
        (uint256 cap, uint256 used) = FlowModel.room(s, isBid);
        console2.log(
            string.concat(
                "  ",
                side,
                " epoch   capacity ",
                FlowModel.u2s(cap),
                "  used ",
                FlowModel.u2s(used),
                "  (base units; capGen ",
                FlowModel.u2s(s.capGen),
                ", usedGen ",
                FlowModel.u2s(s.usedGen),
                ")"
            )
        );
    }

    function _printFreeOption(uint256 i, Live memory L, IPropPool.PairSnapshot memory s, int256 bidEdge, int256 askEdge)
        internal
        view
    {
        uint256 costE2 = _envUint("SIM_INFORMED_COST_E2", 100);

        int256 cost = int256(costE2);
        console2.log("");

        if (bidEdge <= cost && askEdge <= cost) {
            console2.log("  FREE OPTION NOW: none. Both sides are inside the taker's own cost.");
            return;
        }

        (address base, address quote) = _tokensOf(i);
        if (askEdge >= bidEdge) {
            uint256 q = FlowModel.informedAskSize(
                s,
                FlowModel.fillableRoom(IPropPool(D.pool), uint16(i), s, false),
                FlowModel.refNetForAsk(L.ref, costE2),
                L.scale,
                PropPool(D.pool).reserveOf(base),
                100
            );
            uint256 pay = q == 0 ? 0 : PropPool(D.pool).getAmountIn(quote, base, q);
            uint256 worth = FlowModel.value(q, L.ref, L.exp);
            _logFreeOption("BUY base from the pool", q, _baseDecimalsOf(i), pay, worth);
        } else {
            uint256 q = FlowModel.informedBidSize(
                s,
                FlowModel.fillableRoom(IPropPool(D.pool), uint16(i), s, true),
                FlowModel.refNetForBid(L.ref, costE2),
                L.scale,
                PropPool(D.pool).reserveOf(quote),
                100
            );
            uint256 got = q == 0 ? 0 : PropPool(D.pool).getAmountOut(base, quote, q);
            uint256 worth = FlowModel.value(q, L.ref, L.exp);
            _logFreeOption("SELL base to the pool", q, _baseDecimalsOf(i), worth, got);
        }
    }

    function _logFreeOption(string memory action, uint256 q, uint8 baseDecimals, uint256 give, uint256 get)
        internal
        pure
    {
        console2.log(string.concat("  FREE OPTION NOW: ", action));
        console2.log(string.concat("    size          ", _units(q, baseDecimals), " base"));
        console2.log(string.concat("    pays          ", _units(give, 6), " quote"));
        console2.log(string.concat("    worth at ref  ", _units(get, 6), " quote"));
        if (get > give) {
            console2.log(string.concat("    PROFIT        ", _units(get - give, 6), " quote, available to anyone"));
        } else {
            console2.log("    (no size clears the taker's cost once the ladder's impact is paid)");
        }
    }

    function _runFlow(uint256 i, Live memory L) internal {
        uint256 n = _envUint("SIM_FILLS", 12);
        if (n == 0) return;

        console2.log("");
        console2.log(string.concat("  Scripted uninformed flow: ", vm.toString(n), " fills"));
        console2.log("  Sizes are FlowModel's Pareto-1 mixture (90/9/0.9/0.1 per decade, mean ~$2,035),");
        console2.log("  direction a fair coin. Identical draws to MODE 1 for the same seed.");
        console2.log("");
        console2.log("    #   side       notional     amountIn        amountOut       vs reference");

        FlowModel.Rng memory r = FlowModel.rng(_envUint("SIM_SEED", 20_260_727) + i);
        Tally memory tally;

        if (broadcasting) {
            vm.startBroadcast();
            _approve(i);
        } else {
            vm.startPrank(me);
            _approve(i);
        }

        for (uint256 k; k < n; ++k) {
            uint256 raw = FlowModel.drawNotional(r) * 1e6;
            bool buysBase = FlowModel.next(r) % 2 == 0;
            _oneFill(i, L, tally, k, raw, buysBase);
        }

        if (broadcasting) vm.stopBroadcast();
        else vm.stopPrank();

        _printTally(i, tally);
    }

    struct FillLog {
        uint256 k;
        bool buysBase;

        uint256 raw;
        uint256 amountIn;
        uint256 amountOut;
        uint256 baseQty;
        uint256 quoteQty;
        uint256 notional;
        int256 pnl;
    }

    function _oneFill(uint256 i, Live memory L, Tally memory tally, uint256 k, uint256 raw, bool buysBase) internal {
        FillLog memory f;
        f.k = k;
        f.raw = raw;
        f.buysBase = buysBase;

        (address base, address quote) = _tokensOf(i);
        (address tokenIn, address tokenOut) = buysBase ? (quote, base) : (base, quote);
        f.amountIn = buysBase ? raw : (raw * L.scale) / L.ref;

        uint256 quoted = PropPool(D.pool).getAmountOut(tokenIn, tokenOut, f.amountIn);
        if (quoted == 0) {
            tally.declined += 1;
            console2.log(string.concat(_fillPrefix(f), "DECLINED (over capacity, or the reserve floor binds)"));
            return;
        }

        MintableToken(tokenIn).mint(me, f.amountIn);
        if (buysBase) tally.quoteToMint += f.amountIn;
        else tally.baseToMint += f.amountIn;

        f.amountOut = PropPool(D.pool)
            .swap(
                tokenIn,
                tokenOut,

                int256(f.amountIn),
                quoted,
                me,
                0,
                block.timestamp + DEADLINE_SLACK
            );

        _settle(i, L, tally, f);
    }

    function _settle(uint256 i, Live memory L, Tally memory tally, FillLog memory f) internal view {
        f.baseQty = f.buysBase ? f.amountOut : f.amountIn;
        f.quoteQty = f.buysBase ? f.amountIn : f.amountOut;
        f.notional = FlowModel.value(f.baseQty, L.ref, L.exp);

        f.pnl = FlowModel.fillPnl(!f.buysBase, f.baseQty, f.quoteQty, L.ref, L.exp);

        tally.fills += 1;
        tally.notional += f.notional;
        tally.spread += f.pnl;

        _logFill(i, L, f);
    }

    function _fillPrefix(FillLog memory f) internal pure returns (string memory) {
        return string.concat(
            "    ",
            _pad(FlowModel.u2s(f.k), 4),
            _pad(f.buysBase ? "buy base" : "sell base", 11),
            _pad(string.concat("$", FlowModel.thousands(f.raw / 1e6)), 13)
        );
    }

    function _logFill(uint256 i, Live memory L, FillLog memory f) internal view {
        uint8 bd = _baseDecimalsOf(i);
        console2.log(
            string.concat(
                _fillPrefix(f),
                _pad(_units(f.amountIn, f.buysBase ? 6 : bd), 16),
                _pad(_units(f.amountOut, f.buysBase ? bd : 6), 16),
                _bpsSigned(FlowModel.bpsOf(f.pnl, f.notional)),
                " bp"
            )
        );

        string memory head = string.concat(
            "    FILL block=",
            vm.toString(block.number),
            " pair=",
            vm.toString(uint256(L.pairId)),
            " side=",
            f.buysBase ? "ask" : "bid"
        );
        console2.log(
            string.concat(
                head,
                " base=",
                vm.toString(f.baseQty),
                " quote=",
                vm.toString(f.quoteQty),
                " ref=",
                vm.toString(L.ref),
                " refExternal=",
                refIsExternal ? "1" : "0"
            )
        );
    }

    function _printTally(uint256 i, Tally memory tally) internal pure {
        console2.log("");
        console2.log(
            string.concat(
                "    filled ",
                vm.toString(tally.fills),
                ", declined ",
                vm.toString(tally.declined),
                ", notional $",
                FlowModel.thousands(tally.notional / 1e6)
            )
        );
        console2.log(
            string.concat(
                "    realised spread to the pool  ",
                FlowModel.signedUnits(tally.spread, 6),
                " quote  (",
                _bpsSigned(FlowModel.bpsOf(tally.spread, tally.notional)),
                " bp)"
            )
        );
        console2.log(
            string.concat(
                "    minted for this run          ",
                _units(tally.quoteToMint, 6),
                " mUSDC + ",
                _units(tally.baseToMint, _baseDecimalsOf(i)),
                " base"
            )
        );
        console2.log("    NO MARKOUT: a script cannot see t+1, t+10 or t+60. Join the FILL lines above");
        console2.log("    against a reference feed at those offsets to get it (`make simulate-dry | grep");
        console2.log("    FILL`). MODE 1 has markout directly, because it owns the clock.");
    }

    function _tokensOf(uint256 i) internal view returns (address base, address quote) {
        return (i == 0 ? D.mWeth : D.mWbtc, D.mUsdc);
    }

    function _baseDecimalsOf(uint256 i) internal pure returns (uint8 d) {
        (, d,,) = _marketSpec(i);
    }

    function _approve(uint256 i) internal {
        (address base, address quote) = _tokensOf(i);
        if (MintableToken(base).allowance(me, D.pool) != type(uint256).max) {
            MintableToken(base).approve(D.pool, type(uint256).max);
        }
        if (MintableToken(quote).allowance(me, D.pool) != type(uint256).max) {
            MintableToken(quote).approve(D.pool, type(uint256).max);
        }
    }

    function _bpsSigned(int256 e2) internal pure returns (string memory) {
        if (e2 == type(int256).min) return "n/a";
        return FlowModel.sbps(e2);
    }

    function _noQuotableMarkets() internal pure {
        console2.log("");
        _rule();
        console2.log(" NO MARKET WAS QUOTABLE -- nothing was simulated.");
        _rule();
        console2.log(" This is not a failure of the script and it is not a bug in the pool. The most");
        console2.log(" likely cause by far is that the updater bot is not running: it withdraws capacity");
        console2.log(" on shutdown, which is the correct behaviour for a maker that has stopped watching");
        console2.log(" the market, and it leaves the pool quoting nothing. Each market's own diagnosis is");
        console2.log(" printed above.");
        _rule();
    }

    function _caveats() internal view {
        console2.log("");
        _rule();
        console2.log(" WHAT THIS RUN DOES NOT SHOW");
        _rule();
        console2.log(" 1. No markout. A script has no t+60. The FILL lines are emitted so the join can");
        console2.log("    be done afterwards; the in-EVM mode (test/Simulation.t.sol) has it directly.");
        console2.log(" 2. Uninformed flow only. The informed trade is measured and priced above but is");
        console2.log("    never executed: it would drain a live epoch and tell you nothing the quote");
        console2.log("    path has not already told you.");
        if (!refIsExternal) {
            console2.log(" 3. THE REFERENCE WAS THE POOL'S OWN LADDER. Every edge above is therefore zero");
            console2.log("    by construction, exactly the defect DEPLOYMENTS.md lists as caveat 4 on the");
            console2.log("    demo table. Set SIM_REF_1 / SIM_REF_2 before quoting a single number here.");
        } else {
            console2.log(" 3. The reference was supplied by the operator and is not verified against");
            console2.log("    anything. A wrong SIM_REF produces a confidently wrong edge.");
        }
        console2.log(" 4. Both tokens are freely mintable mocks and the pool is ours. This measures the");
        console2.log("    curve and the quoting loop, not a market.");
        _rule();
        console2.log("");
        console2.log(" The question this tool exists for is answered in MODE 1:");
        console2.log("   forge test --match-path test/Simulation.t.sol -vv");
        console2.log("");
    }
}
