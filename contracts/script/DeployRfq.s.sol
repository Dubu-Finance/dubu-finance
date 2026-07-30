// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {console2} from "forge-std/console2.sol";

import {DubuScript} from "./DubuScript.sol";
import {IPmmSettle} from "../src/interfaces/IPmmSettle.sol";
import {PmmSettle} from "../src/PmmSettle.sol";
import {PmmAdapter} from "../src/adapters/PmmAdapter.sol";
import {MockERC20} from "../src/mocks/MockERC20.sol";

contract DeployRfq is DubuScript {

    uint256 internal constant GAS_BUDGET = 8_000_000;

    bytes32 internal constant EIP712_DOMAIN_TYPEHASH =
        keccak256("EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)");
    string internal constant DOMAIN_NAME = "DuBu PmmSettle";
    string internal constant DOMAIN_VERSION = "1";

    bytes32 internal constant RUST_ORDER_TYPEHASH = 0x23e655e78a91115e92aff2d730688fc421a3773ea96b4afcd69c21acf9e8be56;

    uint256 internal constant ALLOWANCE_NOTIONAL_USD = 1_000_000;

    struct Assets {
        address usdc;
        address weth;
        address wbtc;
    }

    struct Approval {
        string symbol;
        address token;
        uint8 decimals;
        uint256 target;
    }

    struct Plan {
        address maker;
        address settle;
        address adapter;
        bool deploySettle;
        bool deployAdapter;

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

    function _smokeOrder(address maker, Assets memory a)
        internal
        view
        returns (IPmmSettle.Order memory o, uint256 takerIn)
    {
        (, uint8 baseDec, uint8 quoteDec, uint256 midWhole) = _marketSpec(0);

        uint256 takerAmount = 10 ** (uint256(baseDec) - 2);
        uint256 makerAmount = (midWhole * takerAmount * (10 ** uint256(quoteDec))) / (10 ** uint256(baseDec));
        takerIn = takerAmount / 10;

        o = IPmmSettle.Order({
            maker: maker,
            makerAsset: a.usdc,
            takerAsset: a.weth,
            makerAmount: makerAmount,
            takerAmount: takerAmount,
            nonce: uint64(_envUint("RFQ_SMOKE_NONCE", 1)),

            expiry: uint64(block.timestamp + _envUint("RFQ_SMOKE_TTL", 7 days)),
            decayStart: 0,
            decayPerSec: 0,
            decayCap: 0,
            minFillBps: 0
        });
    }

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

    function _printLiveStack(Assets memory a) internal view {
        console2.log("");
        console2.log("  Already live (read, never deployed, never modified)");
        _logStep("      ", "PropPool", _envAddr("PROP_POOL", address(0)));
        _logStep("      ", "Router", _envAddr("ROUTER", address(0)));
        _checkToken("mUSDC", a.usdc, 6);
        _checkToken("mWETH", a.weth, 18);
        _checkToken("mWBTC", a.wbtc, 8);
    }

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
