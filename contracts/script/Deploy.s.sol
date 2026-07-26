// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {console2} from "forge-std/console2.sol";

import {DubuScript, Deployment, Market, Roles} from "./DubuScript.sol";
import {PropPool} from "../src/PropPool.sol";

/// @title Deploy — the whole DuBu stack onto GIWA Sepolia
///
/// ```
/// make deploy                                    # keystore, or PRIVATE_KEY if .env sets it
/// forge script script/Deploy.s.sol:Deploy \      # dry run against live chain state
///   --rpc-url https://sepolia-rpc.giwa.io --sender 0x5AD1... -vvv
/// ```
///
/// ## What it deploys, and what it deliberately does not
///
/// Deployed: three mintable tokens, a UniswapV2 factory + Router02 + one pair per comparison
/// market, the PropPool, the aggregation Router, and both adapters.
///
/// **Not** deployed: Permit2, Multicall3, WETH9 and Pyth. All four are genesis pre-installs on
/// GIWA at the addresses in `DubuScript`, and `Router.PERMIT2` is already a compile-time constant
/// pointing at the canonical one. Redeploying any of them would produce a second, unused copy and
/// a router that silently disagrees with every other integrator on the chain.
///
/// Both sides of every pair are our own `MintableToken`. That is not laziness: GIWA Sepolia has no
/// canonical stablecoin — Circle publishes no CCTP domain for chain 91342 and every "USDC" on the
/// explorer is somebody's mock — and the only asset that arrives honestly is ETH, capped at
/// 0.015/day by the faucet. A demo built on bridged assets tops out at a few dollars of TVL, which
/// makes every slippage curve we want to show meaningless.
///
/// ## Resumability
///
/// Every address is read from env first and only deployed when unset, and `addPair` is skipped
/// when the pair already exists. A run that dies half way through is resumed by pasting the export
/// block this script prints and running it again. That property is worth more here than elegance:
/// the alternative is a partially-configured pool and a manual reconciliation.
contract Deploy is DubuScript {
    /// @notice Conservative upper bound for the preflight, expressed in *gas limits*, not gas used.
    ///
    /// @dev A cold full deployment is 13 transactions. Measured on a fork of live GIWA state:
    ///      22,756,498 gas actually consumed, against a 30,199,577 sum of the limits forge sets at
    ///      its default 130% multiplier — and the Makefile raises that to 200% (see the note there
    ///      on cold-access accounting), which puts the sum of limits near 46M. A node admits a
    ///      transaction only if the sender can pay `gasLimit * gasPrice`, so the *limits* are what
    ///      the affordability check has to be sized against. 60M leaves margin for a compiler bump.
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

    // ---------------------------------------------------------------------
    // Preflight — everything checkable before a single transaction is signed
    // ---------------------------------------------------------------------

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

    /// @dev Prints the resolved role set and, when they collapse onto one key, says plainly what
    ///      that costs. The split is the security model: the updater signs many times a minute
    ///      from a hot process and must be assumed to leak, which is only survivable because it
    ///      can move no funds. One key holding all four gives that property up entirely.
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

    /// @dev The `priceScaleExp` derivation, printed with the numbers it was derived from, because
    ///      it is immutable per pair and picking it wrong is a redeploy rather than a config
    ///      change.
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

    // ---------------------------------------------------------------------
    // Report
    // ---------------------------------------------------------------------

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
