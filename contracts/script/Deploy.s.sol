// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {console2} from "forge-std/console2.sol";

import {DubuScript, Deployment, Market, Roles} from "./DubuScript.sol";
import {PropPool} from "../src/PropPool.sol";

contract Deploy is DubuScript {

    uint256 internal constant GAS_BUDGET = 60_000_000;

    function run() external returns (Deployment memory d) {
        address deployer = msg.sender;
        Roles memory r = _roles(deployer);

        _preflight(deployer, r);

        vm.startBroadcast();
        d = _deployStack(deployer, r);
        console2.log("");
        console2.log("Configuring markets");
        bool ok = _configureMarkets(d, deployer);
        vm.stopBroadcast();

        _report(d, r, deployer, ok);
    }

    function _preflight(address deployer, Roles memory r) internal view {
        _rule();
        console2.log(" DuBu deployment preflight");
        _rule();
        console2.log(string.concat("  chain id              ", vm.toString(block.chainid)));
        console2.log(string.concat("  block                 ", vm.toString(block.number)));
        console2.log(string.concat("  deployer              ", vm.toString(deployer)));

        if (block.chainid != GIWA_SEPOLIA) {
            console2.log("");
            console2.log("  WARNING: this is not GIWA Sepolia (91342). Every token this script");
            console2.log("  deploys has an unauthenticated public mint. NEVER run it against a");
            console2.log("  chain with real value on it.");
        }

        _assertAffordable(deployer, GAS_BUDGET);

        console2.log("");
        console2.log("  Genesis pre-installs (used, never redeployed)");
        _logStep("      ", "Permit2", PERMIT2);
        _logStep("      ", "Multicall3", MULTICALL3);
        _logStep("      ", "WETH9", WETH9);
        _logStep("      ", "Pyth", PYTH);

        _printRoles(r, deployer);
        _printMarketPlan();
        _rule();
        console2.log("");
    }

    function _printRoles(Roles memory r, address deployer) internal pure {
        console2.log("");
        console2.log("  PropPool roles");
        _logStep("      ", "owner    (timelock)", r.owner);
        _logStep("      ", "manager  (params)", r.manager);
        _logStep("      ", "updater  (hot key)", r.updater);
        _logStep("      ", "guardian (pause)", r.guardian);

        uint256 distinct = 1;
        if (r.manager != r.owner) ++distinct;
        if (r.updater != r.owner && r.updater != r.manager) ++distinct;
        if (r.guardian != r.owner && r.guardian != r.manager && r.guardian != r.updater) ++distinct;

        if (distinct == 4) return;

        console2.log("");
        console2.log(
            string.concat("  WARNING: the four roles resolve to ", vm.toString(distinct), " distinct address(es).")
        );
        if (r.owner == deployer && r.updater == deployer) {
            console2.log("  All of them default to the deployer, which is fine for a testnet demo and");
            console2.log("  wrong everywhere else. The split exists so that the key which signs a quote");
            console2.log("  every few seconds can move no funds, pause nothing and add no pairs. Sharing");
            console2.log("  it with the owner key means a leaked hot key drains the pool. Set OWNER,");
            console2.log("  MANAGER, UPDATER and GUARDIAN in .env before anything real is at stake.");
        } else {
            console2.log("  Roles that share an address share a blast radius. See the note on PropPool's");
            console2.log("  `updater` role: it is designed on the assumption that it leaks.");
        }
    }

    function _printMarketPlan() internal view {
        console2.log("");
        console2.log("  Markets");
        console2.log("    market       dec    ref price      exp   mid (price units)   headroom");
        for (uint256 i; i < MARKET_COUNT; ++i) {
            Deployment memory empty;
            Market memory m = _market(i, empty);
            _assertMarketIsSane(m);
            (,,, uint256 maxAsk) = _ladder(m.mid);

            console2.log(
                string.concat(
                    "    ",
                    _pad(m.name, 13),
                    _pad(
                        string.concat(vm.toString(uint256(m.baseDecimals)), "/", vm.toString(uint256(m.quoteDecimals))),
                        7
                    ),
                    _pad(string.concat("$", _thousands(m.midWhole)), 15),
                    _pad(vm.toString(uint256(m.priceScaleExp)), 6),
                    _pad(vm.toString(m.mid), 20),
                    string.concat(vm.toString(MAX_PRICE / maxAsk), "x")
                )
            );
        }
        console2.log("    exp is the largest that keeps maxAsk * 8 inside uint56, so the pair survives");
        console2.log("    an 8x move in the reference price without a new pair id. Larger exp = finer");
        console2.log("    prices and fewer bisection steps in the two inverted directions.");
    }

    function _report(Deployment memory d, Roles memory r, address deployer, bool ok) internal view {
        console2.log("");
        _rule();
        console2.log(" Deployed");
        _rule();
        _logStep("      ", "mUSDC (6)", d.mUsdc);
        _logStep("      ", "mWETH (18)", d.mWeth);
        _logStep("      ", "mWBTC (8)", d.mWbtc);
        _logStep("      ", "UniswapV2Factory", d.factory);
        _logStep("      ", "UniswapV2Router02", d.v2Router);
        _logStep("      ", "PropPool", d.pool);
        _logStep("      ", "Router", d.router);
        _logStep("      ", "PropPoolAdapter", d.propAdapter);
        _logStep("      ", "UniV2Adapter", d.uniAdapter);

        console2.log("");
        console2.log("  Pairs");
        for (uint256 i; i < MARKET_COUNT; ++i) {
            Market memory m = _market(i, d);
            (uint16 pairId,) = PropPool(d.pool).pairIdFor(m.base, m.quote);
            console2.log(
                string.concat(
                    "    ",
                    _pad(m.name, 13),
                    "univ2 ",
                    vm.toString(m.pair),
                    "   propPool id ",
                    vm.toString(uint256(pairId)),
                    "  exp ",
                    vm.toString(uint256(m.priceScaleExp))
                )
            );
        }

        _printExports(d);

        if (!ok) {
            console2.log("");
            console2.log(" INCOMPLETE: at least one PropPool pair was not registered because this");
            console2.log(" sender does not own the pool. Register them from the owner key, then re-run.");
        }

        console2.log("");
        console2.log(" Next: make demo   (seeds both venues and runs the comparison sweep)");
        console2.log(string.concat(" Deployer ", vm.toString(deployer), " holds owner=", vm.toString(r.owner)));
        console2.log("");
    }
}
