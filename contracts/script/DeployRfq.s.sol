// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {console2} from "forge-std/console2.sol";

import {DubuScript} from "./DubuScript.sol";
import {IPmmSettle} from "../src/interfaces/IPmmSettle.sol";
import {PmmSettle} from "../src/PmmSettle.sol";
import {PmmAdapter} from "../src/adapters/PmmAdapter.sol";
import {MockERC20} from "../src/mocks/MockERC20.sol";

/// @title DeployRfq — the RFQ leg onto GIWA Sepolia, next to the stack that is already live
///
/// ```
/// make deploy-rfq-dry                                # dry run against live chain state
/// make deploy-rfq                                    # keystore, or PRIVATE_KEY if .env sets it
/// ```
///
/// ## What it deploys, and what it deliberately does not
///
/// Deployed: `PmmSettle` and `PmmAdapter`. Both take no constructor arguments, own nothing, and
/// have no owner, no pause, no upgrade path and no registry to be listed in.
///
/// **Not** deployed or touched: `PropPool`, `Router`, the three mock tokens, the UniV2 stack.
/// Those are live (see `DEPLOYMENTS.md`) and this script only reads their addresses out of env so
/// it can approve against the tokens and echo the rest. In particular there is nothing to *wire*:
/// `Router` has no adapter registry — a route names its adapter in the step word — so "deploying
/// the RFQ leg" is two `CREATE`s and a set of ERC-20 allowances, and the allowances are the part
/// that can be got wrong.
///
/// ## The two failures this script exists to catch before they are expensive
///
///  1. **A signer whose EIP-712 domain disagrees with the chain.** The maker signs a digest
///     computed off chain; `PmmSettle` recomputes it and `ecrecover`s against it. If the two
///     domains differ by one byte the recovered address is not the maker, every fill reverts
///     `BadSignature`, and the maker sees *nothing* — no revert of their own, no log, just quotes
///     nobody can take. So the domain separator is derived here independently of the contract,
///     checked against a locally-instantiated `PmmSettle` before anything is broadcast, checked
///     again against the deployed instance, and then printed in the exact shape a signer needs.
///
///  2. **A maker who has not approved the settler.** `PmmSettle` custodies nothing — both legs of
///     a fill are `transferFrom` pulls — so an unapproved maker's orders also fail only at fill
///     time, and also silently from the maker's side. The approvals are part of the deployment,
///     not a follow-up step, and their size is argued for in the log rather than defaulted to
///     infinity.
///
/// ## Resumability
///
/// `PMM_SETTLE` and `PMM_ADAPTER` are read from env first and only deployed when unset, matching
/// `Deploy`. The approvals are idempotent on top of that: an allowance already at or above the
/// target is logged as `reuse` and costs no transaction, so re-running after a half-finished run
/// converges rather than re-approving.
contract DeployRfq is DubuScript {
    // ---------------------------------------------------------------------
    // Gas
    // ---------------------------------------------------------------------

    /// @notice Conservative upper bound for the preflight, expressed in *gas limits*, not gas used.
    ///
    /// @dev A cold full run is 5 transactions: two `CREATE`s and three `approve`s. Measured on a
    ///      dry run against live GIWA state: 2,228,490 gas actually consumed (PmmSettle 1,301,020,
    ///      PmmAdapter 780,260, ~49,000 per approve), against a 2,897,037 sum of the limits forge
    ///      sets at its default 130% multiplier — and the Makefile raises that to 200% (see the
    ///      note there on cold-access accounting), which puts the sum of limits at ~4.46M. A node
    ///      admits a transaction only if the sender can pay `gasLimit * gasPrice`, so the *limits*
    ///      are what the affordability check has to be sized against. 8M is ~1.8x that, which
    ///      leaves room for a compiler bump without making the check meaningless.
    uint256 internal constant GAS_BUDGET = 8_000_000;

    // ---------------------------------------------------------------------
    // The EIP-712 domain, derived here rather than read from the contract
    // ---------------------------------------------------------------------

    /// @dev These four constants are a deliberate second implementation of
    ///      `PmmSettle._computeDomainSeparator`. Reading them off the contract would turn every
    ///      assertion below into `x == x`; the whole value of the check is that two independent
    ///      encodings of the domain agree, in the same way `test/PmmSettle.t.sol` and
    ///      `dubu_core::rfq` are two independent encodings that agree.
    bytes32 internal constant EIP712_DOMAIN_TYPEHASH =
        keccak256("EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)");
    string internal constant DOMAIN_NAME = "DuBu PmmSettle";
    string internal constant DOMAIN_VERSION = "1";

    /// @notice The exact 32 bytes `dubu_core::rfq::ORDER_TYPEHASH` holds, and the value
    ///         `test/PmmSettle.t.sol` pins from the third side.
    /// @dev Written out rather than derived, for the same reason it is written out in both of
    ///      those files: a reviewer has to be able to diff it by eye. Any change to a field name,
    ///      type or position moves it, and then the Rust, the test and this script all fail at once
    ///      instead of a signer quietly producing digests nothing can verify.
    bytes32 internal constant RUST_ORDER_TYPEHASH = 0x23e655e78a91115e92aff2d730688fc421a3773ea96b4afcd69c21acf9e8be56;

    // ---------------------------------------------------------------------
    // Maker allowances
    // ---------------------------------------------------------------------

    /// @notice Notional, in whole quote tokens, of standing allowance granted per quotable asset.
    ///
    /// @dev The number itself is arguable; that it is *finite* is not. `PmmSettle` holds no
    ///      inventory, so this allowance is not a convenience — it is the maker's entire exposure
    ///      to a bug in the settler, and it is the one trust concentration the contract's own
    ///      header names. Sizing it to what the maker intends to quote makes the blast radius a
    ///      number somebody chose; `type(uint256).max` makes it "everything this key will ever
    ///      hold, forever", which is a different decision and should have to be typed out.
    ///
    ///      $1M matches the top of the `Demo` sweep and sits at half the curve side's $2M
    ///      per-epoch capacity, so the RFQ leg can quote any size the rest of the demo can.
    uint256 internal constant ALLOWANCE_NOTIONAL_USD = 1_000_000;

    // ---------------------------------------------------------------------
    // Types
    // ---------------------------------------------------------------------

    /// @notice The live token set the maker will quote against. Read from env, never deployed here.
    struct Assets {
        address usdc;
        address weth;
        address wbtc;
    }

    /// @notice One maker approval: the asset, and how much of it `PmmSettle` may pull.
    struct Approval {
        string symbol;
        address token;
        uint8 decimals;
        uint256 target;
    }

    /// @notice Everything resolved before a transaction is signed.
    struct Plan {
        address maker;
        address settle;
        address adapter;
        bool deploySettle;
        bool deployAdapter;
        /// @dev The separator `settle` must compute, derived by this script from the chain id and
        ///      that address alone. Known before the deployment because it is a pure function of
        ///      two things we already know — which is exactly why the signer can be configured
        ///      before the broadcast lands.
        bytes32 separator;
        bool unlimited;
    }

    function run() external returns (address settle, address adapter) {
        address deployer = msg.sender;

        Assets memory a = _assets();
        Plan memory p = _plan(deployer);
        Approval[] memory approvals = _approvalPlan(a, p.unlimited);
        (IPmmSettle.Order memory order, uint256 takerIn) = _smokeOrder(deployer, a);

        _preflight(deployer, p, a, approvals, order, takerIn);

        vm.startBroadcast();
        (settle, adapter) = _deploy(p);
        console2.log("");
        console2.log("Maker approvals");
        _approve(settle, p.maker, approvals);
        vm.stopBroadcast();

        _report(settle, adapter, p, approvals, order, takerIn);
    }

    // =====================================================================
    // Resolution — env in, a fully determined plan out
    // =====================================================================

    /// @dev The same env var names `Deploy` writes and `script/README.md` documents, so one
    ///      `.env` drives both scripts. A missing one is fatal and fatal *early*: the approvals
    ///      are the substance of this deployment, and a script that deployed two contracts and
    ///      then discovered it had nothing to approve would have to be resumed by hand.
    function _assets() internal view returns (Assets memory a) {
        a.usdc = _requireAddr("MUSDC");
        a.weth = _requireAddr("MWETH");
        a.wbtc = _requireAddr("MWBTC");
    }

    function _requireAddr(string memory key) internal view returns (address out) {
        out = _envAddr(key, address(0));
        if (out == address(0)) {
            console2.log(string.concat("  ", key, " is not set."));
            console2.log("  This script quotes against the token set that is already live; it deploys none of");
            console2.log("  it. Paste the export block from `make deploy`, or the addresses from");
            console2.log("  DEPLOYMENTS.md, into .env and re-run.");
            revert(string.concat("DeployRfq: ", key, " is required"));
        }
    }

    /// @dev The addresses, and — when a contract is about to be created — the address it will be
    ///      created at.
    ///
    ///      Predicting rather than waiting is what lets the domain separator be asserted and
    ///      published *before* the broadcast: a separator is a pure function of the chain id and
    ///      the verifying contract, both of which are knowable in advance. The prediction is not
    ///      trusted — `_report` asserts the deployed address is the predicted one — but a
    ///      prediction that is checked is strictly better than a value that only exists afterwards,
    ///      because it means the off-chain signer's configuration can be reviewed against this
    ///      output before any of it is real.
    function _plan(address deployer) internal returns (Plan memory p) {
        p.maker = _envAddr("RFQ_MAKER", deployer);
        p.unlimited = _envUint("RFQ_ALLOWANCE_UNLIMITED", 0) != 0;

        uint256 nonce = vm.getNonce(deployer);

        p.settle = _envAddr("PMM_SETTLE", address(0));
        if (p.settle == address(0)) {
            p.deploySettle = true;
            p.settle = vm.computeCreateAddress(deployer, nonce);
            ++nonce;
        }

        p.adapter = _envAddr("PMM_ADAPTER", address(0));
        if (p.adapter == address(0)) {
            p.deployAdapter = true;
            p.adapter = vm.computeCreateAddress(deployer, nonce);
        }

        p.separator = _domainSeparator(p.settle);
    }

    /// @notice `keccak256(abi.encode(DOMAIN_TYPEHASH, keccak256(name), keccak256(version),
    ///         chainId, verifyingContract))` — EIP-712 §"Definition of domainSeparator".
    function _domainSeparator(address verifying) internal view returns (bytes32) {
        return keccak256(
            abi.encode(
                EIP712_DOMAIN_TYPEHASH,
                keccak256(bytes(DOMAIN_NAME)),
                keccak256(bytes(DOMAIN_VERSION)),
                block.chainid,
                verifying
            )
        );
    }

    /// @dev Per-asset allowance, in that asset's smallest unit, sized to a fixed *notional* so the
    ///      three numbers mean the same thing. Overridable per asset in whole tokens, because the
    ///      right answer depends on the maker's inventory and this file cannot know it.
    function _approvalPlan(Assets memory a, bool unlimited) internal view returns (Approval[] memory p) {
        (, uint8 wethDec, uint8 usdcDec, uint256 wethMid) = _marketSpec(0);
        (, uint8 wbtcDec,, uint256 wbtcMid) = _marketSpec(1);

        p = new Approval[](3);
        p[0] = Approval({
            symbol: "mUSDC",
            token: a.usdc,
            decimals: usdcDec,
            target: _target("RFQ_ALLOWANCE_MUSDC", ALLOWANCE_NOTIONAL_USD, usdcDec, unlimited)
        });
        p[1] = Approval({
            symbol: "mWETH",
            token: a.weth,
            decimals: wethDec,
            target: _target("RFQ_ALLOWANCE_MWETH", ALLOWANCE_NOTIONAL_USD / wethMid, wethDec, unlimited)
        });
        p[2] = Approval({
            symbol: "mWBTC",
            token: a.wbtc,
            decimals: wbtcDec,
            target: _target("RFQ_ALLOWANCE_MWBTC", ALLOWANCE_NOTIONAL_USD / wbtcMid, wbtcDec, unlimited)
        });
    }

    function _target(string memory key, uint256 wholeDefault, uint8 decimals, bool unlimited)
        internal
        view
        returns (uint256)
    {
        if (unlimited) return type(uint256).max;
        return _envUint(key, wholeDefault) * (10 ** uint256(decimals));
    }

    /// @notice One small, no-decay, no-floor order at the mWETH/mUSDC reference mid, and the slice
    ///         of it the printed smoke test fills.
    ///
    /// @dev Built from `_marketSpec`, not from literals, so the smoke test quotes the same $2,000
    ///      mid as `Deploy`, `Demo` and `test/Integration.t.sol`. The fill is a tenth of the order
    ///      on purpose: a full fill would exercise neither the pro-rata split nor the
    ///      remaining-amount accounting, which are the two things this contract does that the
    ///      design it forked does not.
    function _smokeOrder(address maker, Assets memory a)
        internal
        view
        returns (IPmmSettle.Order memory o, uint256 takerIn)
    {
        (, uint8 baseDec, uint8 quoteDec, uint256 midWhole) = _marketSpec(0);

        uint256 takerAmount = 10 ** (uint256(baseDec) - 2); // 0.01 mWETH
        uint256 makerAmount = (midWhole * takerAmount * (10 ** uint256(quoteDec))) / (10 ** uint256(baseDec));
        takerIn = takerAmount / 10;

        o = IPmmSettle.Order({
            maker: maker,
            makerAsset: a.usdc,
            takerAsset: a.weth,
            makerAmount: makerAmount,
            takerAmount: takerAmount,
            nonce: uint64(_envUint("RFQ_SMOKE_NONCE", 1)),
            // Long, because the printed command is meant to survive being pasted into a terminal
            // tomorrow. The signature is bound to this value, so re-running the script produces a
            // different digest and the old signature stops verifying — which is the correct
            // behaviour and worth knowing before it surprises somebody.
            expiry: uint64(block.timestamp + _envUint("RFQ_SMOKE_TTL", 7 days)),
            decayStart: 0,
            decayPerSec: 0,
            decayCap: 0,
            minFillBps: 0
        });
    }

    // =====================================================================
    // Preflight — everything checkable before a single transaction is signed
    // =====================================================================

    function _preflight(
        address deployer,
        Plan memory p,
        Assets memory a,
        Approval[] memory approvals,
        IPmmSettle.Order memory order,
        uint256 takerIn
    ) internal {
        _rule();
        console2.log(" DuBu RFQ deployment preflight");
        _rule();
        console2.log(string.concat("  chain id              ", vm.toString(block.chainid)));
        console2.log(string.concat("  block                 ", vm.toString(block.number)));
        console2.log(string.concat("  deployer              ", vm.toString(deployer)));
        console2.log(string.concat("  maker (signs orders)  ", vm.toString(p.maker)));

        if (block.chainid != GIWA_SEPOLIA) {
            console2.log("");
            console2.log("  WARNING: this is not GIWA Sepolia (91342). The tokens this script approves");
            console2.log("  have unauthenticated public mints. NEVER run it against a chain with real");
            console2.log("  value on it.");
        }

        _assertAffordable(deployer, GAS_BUDGET);

        _checkEip712(p, order, takerIn);
        _printLiveStack(a);
        _printTrustModel(deployer, p);
        _printApprovalPlan(p, approvals);

        _rule();
        console2.log("");
    }

    /// @notice The domain check, in full, before anything is broadcast.
    ///
    /// @dev Three separate claims, and they are separate on purpose because they fail for
    ///      different reasons:
    ///
    ///        1. the deployed bytecode's `ORDER_TYPEHASH` is the one `dubu_core::rfq` pins — this
    ///           fails when somebody edits the `Order` struct on one side only;
    ///        2. this script's derivation of the domain separator reproduces what a real
    ///           `PmmSettle` computes — this fails when the domain name, version or encoding moves;
    ///        3. the smoke order prices the way this script says it does — this fails when the
    ///           order is misshapen in a way `_checkOrderShape` rejects, which would otherwise be
    ///           discovered by whoever pasted the printed command.
    ///
    ///      All three are answered by a `PmmSettle` instantiated **here, outside the broadcast
    ///      window**. That instance is local to the script's own EVM: forge records nothing outside
    ///      `startBroadcast`, so it costs no transaction, no gas and no nonce on the deployer. It
    ///      is the only way to ask the real bytecode a question before the real bytecode is on
    ///      chain, and asking it is worth far more than the simulation time.
    ///
    ///      Claim 2 is checked at the probe's *own* address, which is not the address that will be
    ///      deployed. That is not a weakness: it is what makes the check a check of the derivation
    ///      rather than of one value. A derivation that agrees with the contract at an arbitrary
    ///      address agrees at every address, so `p.separator` — the same function evaluated at the
    ///      address the deployment will land on — is then known to be right before it exists.
    function _checkEip712(Plan memory p, IPmmSettle.Order memory order, uint256 takerIn) internal {
        PmmSettle probe = new PmmSettle();

        require(
            probe.ORDER_TYPEHASH() == RUST_ORDER_TYPEHASH,
            "DeployRfq: ORDER_TYPEHASH does not match dubu_core::rfq - the Order struct moved on one side"
        );
        require(
            probe.DOMAIN_SEPARATOR() == _domainSeparator(address(probe)),
            "DeployRfq: this script's domain derivation disagrees with PmmSettle's"
        );

        // Shape and price of the order the report is about to print a signing command for.
        (uint256 quoted, uint256 realised, uint256 decayPpm) = probe.previewFill(order, takerIn, order.expiry);
        uint256 expected = (order.makerAmount * takerIn) / order.takerAmount;
        require(quoted == expected, "DeployRfq: smoke order does not price pro-rata");
        require(decayPpm == 0 && realised == quoted, "DeployRfq: smoke order is meant not to decay");
        require(order.expiry > block.timestamp, "DeployRfq: smoke order already expired");

        console2.log("");
        console2.log("  EIP-712 domain (checked against a local PmmSettle, nothing broadcast)");
        console2.log(string.concat("      name                 ", DOMAIN_NAME));
        console2.log(string.concat("      version              ", DOMAIN_VERSION));
        console2.log(string.concat("      chainId              ", vm.toString(block.chainid)));
        console2.log(string.concat("      verifyingContract    ", vm.toString(p.settle)));
        console2.log(string.concat("      ORDER_TYPEHASH       ", vm.toString(RUST_ORDER_TYPEHASH)));
        console2.log(string.concat("      DOMAIN_SEPARATOR     ", vm.toString(p.separator)));
        console2.log(
            p.deploySettle
                ? "      (separator is for the predicted address; asserted against the instance below)"
                : "      (separator asserted against the live instance below)"
        );
    }

    /// @dev What this deployment attaches to. None of it is written to — `Router` has no adapter
    ///      registry and `PropPool` has no idea the RFQ leg exists — but a run against the wrong
    ///      token set would approve the wrong contracts and print a smoke test that reverts, so the
    ///      addresses are echoed and the tokens are asked what they are.
    function _printLiveStack(Assets memory a) internal view {
        console2.log("");
        console2.log("  Already live (read, never deployed, never modified)");
        _logStep("      ", "PropPool", _envAddr("PROP_POOL", address(0)));
        _logStep("      ", "Router", _envAddr("ROUTER", address(0)));
        _checkToken("mUSDC", a.usdc, 6);
        _checkToken("mWETH", a.weth, 18);
        _checkToken("mWBTC", a.wbtc, 8);
    }

    /// @dev `decimals()` is checked rather than assumed because every amount this script prints —
    ///      the allowance targets and both legs of the smoke order — is scaled by it. A token at
    ///      the wrong address that happens to answer would otherwise produce a plausible,
    ///      thousand-fold wrong order.
    function _checkToken(string memory symbol, address token, uint8 expectedDecimals) internal view {
        require(token.code.length != 0, string.concat("DeployRfq: ", symbol, " has no code at that address"));
        uint8 actual = MockERC20(token).decimals();
        require(
            actual == expectedDecimals,
            string.concat(
                "DeployRfq: ",
                symbol,
                " reports ",
                vm.toString(uint256(actual)),
                " decimals, expected ",
                vm.toString(uint256(expectedDecimals))
            )
        );
        _logStep("      ", string.concat(symbol, " (", vm.toString(uint256(actual)), ")"), token);
    }

    /// @dev The role echo. There are no roles, and saying so explicitly is the point: a reader
    ///      arriving from `Deploy`'s four-role preflight will look for the equivalent here and
    ///      should find the answer rather than an absence.
    function _printTrustModel(address deployer, Plan memory p) internal pure {
        console2.log("");
        console2.log("  Roles and switches");
        console2.log("      PmmSettle   no owner, no pause, no upgrade path, no taker allowlist");
        console2.log("      PmmAdapter  no owner, no storage, no standing approval of its own");
        console2.log("      The maker's ERC-20 allowance below is the ONLY trust concentration in");
        console2.log("      this deployment. PmmSettle custodies nothing, so a bug in it reaches");
        console2.log("      exactly as far as that allowance and no further.");

        if (p.maker == deployer) {
            console2.log("");
            console2.log("      NOTE: maker == deployer, so one key signs quotes, holds the inventory and");
            console2.log("      pays for the deployment. Fine for a demo. In production the quoting key is");
            console2.log("      hot by construction -- it signs continuously -- and it is the key this");
            console2.log("      allowance is granted from. Set RFQ_MAKER to separate them.");
        }
    }

    function _printApprovalPlan(Plan memory p, Approval[] memory approvals) internal view {
        console2.log("");
        console2.log(string.concat("  Maker approvals to PmmSettle ", vm.toString(p.settle)));
        console2.log("    asset    allowance          maker balance      status");

        for (uint256 i; i < approvals.length; ++i) {
            Approval memory ap = approvals[i];
            uint256 current = MockERC20(ap.token).allowance(p.maker, p.settle);
            uint256 balance = MockERC20(ap.token).balanceOf(p.maker);

            console2.log(
                string.concat(
                    "    ",
                    _pad(ap.symbol, 9),
                    _pad(ap.target == type(uint256).max ? "UNLIMITED" : _units(ap.target, ap.decimals), 19),
                    _pad(_units(balance, ap.decimals), 19),
                    current >= ap.target ? "already granted" : "approve"
                )
            );
        }

        if (p.unlimited) {
            console2.log("");
            console2.log("      RFQ_ALLOWANCE_UNLIMITED is set. Each approval below is type(uint256).max --");
            console2.log("      an unbounded, permanent claim on every unit of these tokens the maker will");
            console2.log("      ever hold, revocable only by another transaction. This is stated here");
            console2.log("      because an unlimited allowance that nobody saw granted is how makers lose");
            console2.log("      inventory to a settler bug they had no exposure to on paper.");
        } else {
            console2.log("");
            console2.log(
                string.concat(
                    "      Each allowance is $",
                    _thousands(ALLOWANCE_NOTIONAL_USD),
                    " of notional at the reference mid, NOT type(uint256).max."
                )
            );
            console2.log("      PmmSettle pulls on fill and holds nothing, so this number is the maker's");
            console2.log("      entire exposure to a bug in it. $1M is the top of the Demo sweep and half");
            console2.log("      the curve side's per-epoch capacity, so the RFQ leg can quote any size the");
            console2.log("      rest of the demo can. Override per asset with RFQ_ALLOWANCE_MUSDC /");
            console2.log("      _MWETH / _MWBTC (whole tokens), in the maker's own units of risk.");
        }
    }

    // =====================================================================
    // Deployment
    // =====================================================================

    function _deploy(Plan memory p) internal returns (address settle, address adapter) {
        if (p.deploySettle) {
            settle = address(new PmmSettle());
            _logStep("deploy", "PmmSettle", settle);
        } else {
            settle = p.settle;
            _logStep("reuse ", "PmmSettle", settle);
        }

        if (p.deployAdapter) {
            adapter = address(new PmmAdapter());
            _logStep("deploy", "PmmAdapter", adapter);
        } else {
            adapter = p.adapter;
            _logStep("reuse ", "PmmAdapter", adapter);
        }
    }

    /// @dev Idempotent: an allowance already at or above the target is left alone. Re-approving
    ///      would be harmless here — `MockERC20.approve` is a plain assignment — but it would cost
    ///      a transaction per re-run and would make the log lie about what changed.
    ///
    ///      A USDT-shaped token that refuses a non-zero-to-non-zero `approve` would need a reset to
    ///      zero first. Not written, because all three assets here are this repo's own `MockERC20`
    ///      and a defence against a token we control the source of is a defence against nothing.
    ///      A maker adding such a token to the quotable set has to add that step.
    function _approve(address settle, address maker, Approval[] memory approvals) internal {
        for (uint256 i; i < approvals.length; ++i) {
            Approval memory ap = approvals[i];
            uint256 current = MockERC20(ap.token).allowance(maker, settle);

            if (current >= ap.target) {
                console2.log(
                    string.concat(
                        "  reuse  ",
                        _pad(ap.symbol, 8),
                        "allowance already ",
                        current == type(uint256).max ? "UNLIMITED" : _units(current, ap.decimals)
                    )
                );
                continue;
            }

            MockERC20(ap.token).approve(settle, ap.target);
            console2.log(
                string.concat(
                    "  approve ",
                    _pad(ap.symbol, 7),
                    ap.target == type(uint256).max ? "UNLIMITED" : _units(ap.target, ap.decimals)
                )
            );
        }
    }

    // =====================================================================
    // Report
    // =====================================================================

    function _report(
        address settle,
        address adapter,
        Plan memory p,
        Approval[] memory approvals,
        IPmmSettle.Order memory order,
        uint256 takerIn
    ) internal view {
        _confirm(settle, adapter, p, approvals);

        console2.log("");
        _rule();
        console2.log(" Deployed");
        _rule();
        _logStep("      ", "PmmSettle", settle);
        _logStep("      ", "PmmAdapter", adapter);

        console2.log("");
        console2.log("  Signer configuration -- the engine MUST use exactly these four values");
        console2.log(string.concat("      name                 ", DOMAIN_NAME));
        console2.log(string.concat("      version              ", DOMAIN_VERSION));
        console2.log(string.concat("      chainId              ", vm.toString(block.chainid)));
        console2.log(string.concat("      verifyingContract    ", vm.toString(settle)));
        console2.log("");
        console2.log(string.concat("      ORDER_TYPEHASH       ", vm.toString(PmmSettle(settle).ORDER_TYPEHASH())));
        console2.log(string.concat("      DOMAIN_SEPARATOR     ", vm.toString(PmmSettle(settle).DOMAIN_SEPARATOR())));
        console2.log("      Both read back from the deployed instance and asserted equal to the values");
        console2.log("      this script derived independently. dubu_core::rfq::Domain::new(chainId,");
        console2.log("      verifyingContract).separator() must reproduce the separator byte for byte;");
        console2.log("      if it does not, every order the engine signs verifies nowhere.");

        _printRouting(adapter);
        _printExports(settle, adapter);
        _printSmokeTest(settle, order, takerIn);
    }

    /// @notice The handful of claims that could not be made earlier, because their subjects did not
    ///         exist earlier.
    ///
    /// @dev Everything checkable before `vm.startBroadcast` was checked there — the domain
    ///      derivation, the typehash, the token decimals, the order's shape and price, the gas
    ///      budget. What remains is genuinely posterior: that the `CREATE`s landed where the nonce
    ///      said they would, that the deployed instance computes the separator that was published
    ///      for it, and that the allowances are in place. A nonce race — another transaction from
    ///      the deployer between the preflight and the broadcast — is the realistic way the first
    ///      one fails, and it is loud rather than silent because the signer configuration printed
    ///      above would otherwise name an address with no code at it.
    function _confirm(address settle, address adapter, Plan memory p, Approval[] memory approvals) internal view {
        require(settle == p.settle, "DeployRfq: PmmSettle did not land at the predicted address (deployer nonce moved)");
        require(
            adapter == p.adapter, "DeployRfq: PmmAdapter did not land at the predicted address (deployer nonce moved)"
        );
        require(settle.code.length != 0 && adapter.code.length != 0, "DeployRfq: a deployment has no code");

        require(
            PmmSettle(settle).DOMAIN_SEPARATOR() == p.separator,
            "DeployRfq: the deployed instance disagrees with the published domain separator"
        );
        require(
            PmmSettle(settle).ORDER_TYPEHASH() == RUST_ORDER_TYPEHASH,
            "DeployRfq: the deployed instance's ORDER_TYPEHASH is not the one dubu_core pins"
        );

        for (uint256 i; i < approvals.length; ++i) {
            Approval memory ap = approvals[i];
            require(
                MockERC20(ap.token).allowance(p.maker, settle) >= ap.target,
                string.concat("DeployRfq: ", ap.symbol, " allowance did not reach the target")
            );
        }
    }

    /// @dev The two things a planner has to get right, restated at the address they now apply to,
    ///      because both fail as `NothingToFill` rather than as anything that names the cause.
    function _printRouting(address adapter) internal pure {
        console2.log("");
        console2.log("  Routing through the Router");
        console2.log("      Nothing was registered anywhere: a route names its adapter in the step word,");
        console2.log("      so the Router needs no change and there is no allowlist to be added to.");
        console2.log(string.concat("      step.pool  = the PmmSettle address, and step venue = ", vm.toString(adapter)));
        console2.log("      bit 254 (fundAdapter) MUST be set on a PmmAdapter step -- PmmSettle pulls the");
        console2.log("      taker leg from msg.sender, which is the adapter. Unset it and the adapter's");
        console2.log("      balance is zero and the leg reverts NothingToFill.");
        console2.log("      bit 255 (reverse) carries no information here; sellBase and sellQuote are the");
        console2.log("      same function, because an order names both assets and the maker signed them.");
    }

    function _printExports(address settle, address adapter) internal pure {
        console2.log("");
        _rule();
        console2.log(" Copy into .env (or export) to make every later run resumable");
        _rule();
        console2.log(string.concat("export PMM_SETTLE=", vm.toString(settle)));
        console2.log(string.concat("export PMM_ADAPTER=", vm.toString(adapter)));
        _rule();
    }

    // =====================================================================
    // Smoke test
    // =====================================================================

    /// @notice A copy-pasteable three-command fill: write the typed data, sign it, send it.
    ///
    /// @dev The typed-data JSON is not a convenience — it is the domain and the type, written out
    ///      in the one format an independent tool will parse, so "does the signer agree with the
    ///      chain" stops being a claim and becomes something a shell answers in ten seconds. If
    ///      `cast wallet sign` produces a signature that `fillOrder` accepts, then the name, the
    ///      version, the chain id, the verifying contract, the field order and every type width
    ///      agree, all at once.
    function _printSmokeTest(address settle, IPmmSettle.Order memory o, uint256 takerIn) internal view {
        uint256 out = (o.makerAmount * takerIn) / o.takerAmount;

        console2.log("");
        _rule();
        console2.log(" Smoke test -- sign one order and fill a tenth of it, from a shell");
        _rule();
        console2.log(
            string.concat(
                "  ",
                _units(o.takerAmount, 18),
                " mWETH offered for ",
                _units(o.makerAmount, 6),
                " mUSDC (the $2,000 reference mid),"
            )
        );
        console2.log(
            string.concat("  no decay, no settlement floor. The fill below spends ", _units(takerIn, 18), " mWETH and")
        );
        console2.log(
            string.concat(
                "  receives ",
                _units(out, 6),
                " mUSDC, leaving ",
                _units(o.takerAmount - takerIn, 18),
                " mWETH on the order -- so it"
            )
        );
        console2.log("  exercises the pro-rata split and the remaining-amount accounting, not just a");
        console2.log("  one-shot settle. Re-run it and watch remainingTaker fall.");
        console2.log("");
        console2.log(
            string.concat("  expiry ", vm.toString(uint256(o.expiry)), " -- the signature is bound to it, so a later")
        );
        console2.log("  run of this script prints a different order and this signature stops verifying.");

        if (o.maker == msg.sender) {
            console2.log("");
            console2.log("  NOTE: maker and filler are the same key here, so both transfers are self-sends");
            console2.log("  and net to zero. The command still proves the whole path -- digest, domain,");
            console2.log("  ecrecover, allowances, fill accounting, event -- but it does not prove value");
            console2.log("  moved. Set RFQ_MAKER, or pass a different receiver in step 3, to see it move.");
        }

        console2.log("");
        console2.log("  1. write the typed data (this IS the domain the engine must sign under)");
        console2.log("");
        _printTypedData(settle, o);

        console2.log("");
        console2.log("  2. sign it as the maker");
        console2.log("");
        console2.log("SIG=$(cast wallet sign --data --from-file /tmp/dubu-rfq-order.json --account dubu-deployer)");
        console2.log("echo $SIG");

        console2.log("");
        console2.log("  3. fill it. maxDecayPpm is 0 -- the tightest correct bound, because this order");
        console2.log("     does not decay. Never pass type(uint32).max without meaning to.");
        console2.log("");
        console2.log(string.concat("cast send ", vm.toString(settle), " \\"));
        console2.log(
            "  \"fillOrder((address,address,address,uint256,uint256,uint64,uint64,uint64,uint32,uint32,uint16),bytes,uint256,uint32,address)\" \\"
        );
        console2.log(string.concat("  \"", _orderTuple(o), "\" \\"));
        console2.log(string.concat("  $SIG ", vm.toString(takerIn), " 0 ", vm.toString(o.maker), " \\"));
        console2.log("  --rpc-url https://sepolia-rpc.giwa.io --account dubu-deployer");

        console2.log("");
        console2.log("  4. check what is left on the order");
        console2.log("");
        console2.log(
            string.concat(
                "cast call ",
                vm.toString(settle),
                " \"remainingTaker((address,address,address,uint256,uint256,uint64,uint64,uint64,uint32,uint32,uint16))(uint256)\" \\"
            )
        );
        console2.log(string.concat("  \"", _orderTuple(o), "\" --rpc-url https://sepolia-rpc.giwa.io"));
        console2.log("");
        console2.log(
            string.concat(
                "  expected: ",
                vm.toString(o.takerAmount - takerIn),
                "  (",
                _units(o.takerAmount - takerIn, 18),
                " mWETH)"
            )
        );
        console2.log("");
        console2.log("  Short of tokens? Both legs come from the maker's own balance:");
        console2.log(
            string.concat(
                "    cast send ",
                vm.toString(o.makerAsset),
                " \"claim()\" --rpc-url https://sepolia-rpc.giwa.io --account dubu-deployer"
            )
        );
        console2.log(
            string.concat(
                "    cast send ",
                vm.toString(o.takerAsset),
                " \"claim()\" --rpc-url https://sepolia-rpc.giwa.io --account dubu-deployer"
            )
        );
        _rule();
        console2.log("");
    }

    /// @dev EIP-712 typed data, in the JSON shape `cast wallet sign --data` parses.
    ///
    ///      The member list under `Order` is the type string, spelled as JSON: the order of these
    ///      eleven entries is load bearing and must match `IPmmSettle.Order`, `ORDER_TYPEHASH` and
    ///      `dubu_core::rfq::ORDER_TYPE_STRING`. EIP-712 offers no independent canonicalisation, so
    ///      four expressions of one ordering have to be edited together or not at all.
    function _printTypedData(address settle, IPmmSettle.Order memory o) internal view {
        console2.log("cat > /tmp/dubu-rfq-order.json <<'JSON'");
        console2.log("{");
        console2.log("  \"types\": {");
        console2.log("    \"EIP712Domain\": [");
        console2.log("      {\"name\":\"name\",\"type\":\"string\"},{\"name\":\"version\",\"type\":\"string\"},");
        console2.log(
            "      {\"name\":\"chainId\",\"type\":\"uint256\"},{\"name\":\"verifyingContract\",\"type\":\"address\"}"
        );
        console2.log("    ],");
        console2.log("    \"Order\": [");
        console2.log("      {\"name\":\"maker\",\"type\":\"address\"},{\"name\":\"makerAsset\",\"type\":\"address\"},");
        console2.log(
            "      {\"name\":\"takerAsset\",\"type\":\"address\"},{\"name\":\"makerAmount\",\"type\":\"uint256\"},"
        );
        console2.log("      {\"name\":\"takerAmount\",\"type\":\"uint256\"},{\"name\":\"nonce\",\"type\":\"uint64\"},");
        console2.log("      {\"name\":\"expiry\",\"type\":\"uint64\"},{\"name\":\"decayStart\",\"type\":\"uint64\"},");
        console2.log(
            "      {\"name\":\"decayPerSec\",\"type\":\"uint32\"},{\"name\":\"decayCap\",\"type\":\"uint32\"},"
        );
        console2.log("      {\"name\":\"minFillBps\",\"type\":\"uint16\"}");
        console2.log("    ]");
        console2.log("  },");
        console2.log("  \"primaryType\": \"Order\",");
        console2.log(
            string.concat(
                "  \"domain\": {\"name\":\"",
                DOMAIN_NAME,
                "\",\"version\":\"",
                DOMAIN_VERSION,
                "\",\"chainId\":",
                vm.toString(block.chainid),
                ",\"verifyingContract\":\"",
                vm.toString(settle),
                "\"},"
            )
        );
        console2.log("  \"message\": {");
        console2.log(
            string.concat(
                "    \"maker\":\"",
                vm.toString(o.maker),
                "\",\"makerAsset\":\"",
                vm.toString(o.makerAsset),
                "\",\"takerAsset\":\"",
                vm.toString(o.takerAsset),
                "\","
            )
        );
        console2.log(
            string.concat(
                "    \"makerAmount\":\"",
                vm.toString(o.makerAmount),
                "\",\"takerAmount\":\"",
                vm.toString(o.takerAmount),
                "\",\"nonce\":\"",
                vm.toString(uint256(o.nonce)),
                "\","
            )
        );
        console2.log(
            string.concat(
                "    \"expiry\":\"",
                vm.toString(uint256(o.expiry)),
                "\",\"decayStart\":\"0\",\"decayPerSec\":\"0\",\"decayCap\":\"0\",\"minFillBps\":\"0\""
            )
        );
        console2.log("  }");
        console2.log("}");
        console2.log("JSON");
    }

    /// @dev The same eleven fields in the positional form `cast send` wants for a tuple argument.
    function _orderTuple(IPmmSettle.Order memory o) internal pure returns (string memory) {
        return string.concat(
            "(",
            vm.toString(o.maker),
            ",",
            vm.toString(o.makerAsset),
            ",",
            vm.toString(o.takerAsset),
            ",",
            vm.toString(o.makerAmount),
            ",",
            vm.toString(o.takerAmount),
            ",",
            vm.toString(uint256(o.nonce)),
            ",",
            vm.toString(uint256(o.expiry)),
            ",0,0,0,0)"
        );
    }
}
