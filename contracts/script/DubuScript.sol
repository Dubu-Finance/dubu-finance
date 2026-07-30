// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {Script} from "forge-std/Script.sol";
import {console2} from "forge-std/console2.sol";

import {PropPool} from "../src/PropPool.sol";
import {Router} from "../src/Router.sol";
import {PropCurve} from "../src/libraries/PropCurve.sol";
import {PropPoolAdapter} from "../src/adapters/PropPoolAdapter.sol";
import {UniV2Adapter} from "../src/adapters/UniV2Adapter.sol";
import {MintableToken} from "../src/mocks/MintableToken.sol";
import {UniswapV2Factory} from "../src/reference/univ2/UniswapV2Factory.sol";
import {UniswapV2Router02} from "../src/reference/univ2/UniswapV2Router02.sol";

struct Deployment {
    address mUsdc;
    address mWeth;
    address mWbtc;
    address factory;
    address v2Router;
    address pool;
    address router;
    address propAdapter;
    address uniAdapter;
}

struct Roles {
    address owner;
    address manager;
    address updater;
    address guardian;
}

struct Market {
    string name;
    address base;
    address quote;
    uint8 baseDecimals;
    uint8 quoteDecimals;

    uint256 midWhole;
    uint8 priceScaleExp;

    uint256 scale;

    uint256 mid;
    uint256 minPrice;
    uint256 tvlBase;
    uint256 tvlQuote;
    uint96 capacity;
    address pair;
}

abstract contract DubuScript is Script {

    uint256 internal constant GIWA_SEPOLIA = 91342;

    address internal constant WETH9 = 0x4200000000000000000000000000000000000006;
    address internal constant PERMIT2 = 0x000000000022D473030F116dDEE9F6B43aC78BA3;
    address internal constant MULTICALL3 = 0xcA11bde05977b3631167028862bE2a173976CA11;
    address internal constant PYTH = 0x2880aB155794e7179c9eE2e38200202908C17B43;

    uint256 internal constant DEFAULT_GAS_PRICE_WEI = 2_000_000;

    uint256 internal constant BPS = 10_000;
    uint256 internal constant HALF_SPREAD_BPS = 5;
    uint256 internal constant WIDTH_BPS = 25;

    uint256 internal constant TVL_QUOTE_WHOLE = 10_000_000;

    uint256 internal constant CAPACITY_QUOTE_WHOLE = 2_000_000;

    uint256 internal constant PRICE_HEADROOM = 8;

    uint256 internal constant MAX_PRICE = type(uint56).max;

    uint256 internal constant DEADLINE_SLACK = 1 hours;

    uint256 internal constant CLAIM_USDC = 10_000e6;
    uint256 internal constant CLAIM_WETH = 5e18;
    uint256 internal constant CLAIM_WBTC = 1e7;

    uint256 internal constant MARKET_COUNT = 2;

    function _marketSpec(uint256 i)
        internal
        pure
        returns (string memory name, uint8 baseDecimals, uint8 quoteDecimals, uint256 midWhole)
    {
        if (i == 0) return ("mWETH/mUSDC", 18, 6, 2_000);
        if (i == 1) return ("mWBTC/mUSDC", 8, 6, 100_000);
        revert("DubuScript: no such market");
    }

    function _market(uint256 i, Deployment memory d) internal view returns (Market memory m) {
        (m.name, m.baseDecimals, m.quoteDecimals, m.midWhole) = _marketSpec(i);
        m.base = i == 0 ? d.mWeth : d.mWbtc;
        m.quote = d.mUsdc;

        m.priceScaleExp = _priceScaleExpFor(m.baseDecimals, m.quoteDecimals, m.midWhole);
        m.scale = 10 ** uint256(m.priceScaleExp);
        m.mid = _encodeMid(m.midWhole, m.baseDecimals, m.quoteDecimals, m.priceScaleExp);

        m.minPrice = m.mid / 2;

        m.tvlQuote = TVL_QUOTE_WHOLE * (10 ** uint256(m.quoteDecimals));
        m.tvlBase = (TVL_QUOTE_WHOLE * (10 ** uint256(m.baseDecimals))) / m.midWhole;

        m.capacity = uint96((CAPACITY_QUOTE_WHOLE * (10 ** uint256(m.baseDecimals))) / m.midWhole);

        if (d.factory != address(0)) m.pair = UniswapV2Factory(d.factory).getPair(m.base, m.quote);
    }

    function _encodeMid(uint256 midWhole, uint8 baseDecimals, uint8 quoteDecimals, uint8 exp)
        internal
        pure
        returns (uint256)
    {
        return (midWhole * (10 ** uint256(quoteDecimals)) * (10 ** uint256(exp))) / (10 ** uint256(baseDecimals));
    }

    function _priceScaleExpFor(uint8 baseDecimals, uint8 quoteDecimals, uint256 midWhole)
        internal
        pure
        returns (uint8)
    {
        for (uint8 e = PropCurve.MAX_PRICE_SCALE_EXP;; --e) {
            uint256 mid = _encodeMid(midWhole, baseDecimals, quoteDecimals, e);
            if (mid != 0) {
                (,,, uint256 maxAsk) = _ladder(mid);
                if (maxAsk * PRICE_HEADROOM <= MAX_PRICE) return e;
            }
            if (e == 0) break;
        }
        revert("DubuScript: no priceScaleExp keeps this market inside uint56");
    }

    function _ladder(uint256 mid)
        internal
        pure
        returns (uint256 minBid, uint256 maxBid, uint256 minAsk, uint256 maxAsk)
    {
        maxBid = (mid * (BPS - HALF_SPREAD_BPS)) / BPS;
        minBid = (maxBid * (BPS - WIDTH_BPS)) / BPS;
        minAsk = (mid * (BPS + HALF_SPREAD_BPS)) / BPS;
        maxAsk = (minAsk * (BPS + WIDTH_BPS)) / BPS;
    }

    function _packLadder(uint16 pairId, uint256 mid) internal pure returns (uint256) {
        (uint256 minBid, uint256 maxBid, uint256 minAsk, uint256 maxAsk) = _ladder(mid);
        return minBid | (maxBid << 56) | (minAsk << 112) | (maxAsk << 168) | (uint256(pairId) << 224);
    }

    function _assertMarketIsSane(Market memory m) internal pure {
        (uint256 minBid,,, uint256 maxAsk) = _ladder(m.mid);

        require(m.priceScaleExp <= PropCurve.MAX_PRICE_SCALE_EXP, "priceScaleExp above PropCurve max");
        require(m.mid != 0, "encoded mid is zero: priceScaleExp too small for these decimals");
        require(maxAsk <= MAX_PRICE, "maxAsk does not fit uint56");
        require(minBid >= m.minPrice, "minBid below the pair's own minPrice floor");
        require(m.minPrice != 0 && m.minPrice <= MAX_PRICE, "minPrice out of range");
        require(m.capacity != 0, "capacity is zero");
        require(m.tvlBase != 0 && m.tvlQuote != 0, "TVL is zero");

        require(
            uint256(m.capacity) * MAX_PRICE <= PropCurve.MAX_AMOUNT_OUT * m.scale, "capacity outside the curve domain"
        );
    }

    function _envAddr(string memory key, address dflt) internal view returns (address) {
        string memory raw = vm.envOr(key, string(""));
        if (bytes(raw).length == 0) return dflt;
        return vm.parseAddress(raw);
    }

    function _envUint(string memory key, uint256 dflt) internal view returns (uint256) {
        string memory raw = vm.envOr(key, string(""));
        if (bytes(raw).length == 0) return dflt;
        return vm.parseUint(raw);
    }

    function _roles(address deployer) internal view returns (Roles memory r) {
        r.owner = _envAddr("OWNER", deployer);
        r.manager = _envAddr("MANAGER", deployer);
        r.updater = _envAddr("UPDATER", deployer);
        r.guardian = _envAddr("GUARDIAN", deployer);
    }

    function _maxStaleSecs() internal view returns (uint32) {
        return uint32(_envUint("MAX_STALE_SECS", 3600));
    }

    function _deployStack(address deployer, Roles memory r) internal returns (Deployment memory d) {
        d.mUsdc = _envAddr("MUSDC", address(0));
        if (d.mUsdc == address(0)) {
            d.mUsdc = address(new MintableToken("DuBu Mock USDC", "mUSDC", 6, CLAIM_USDC));
            _logStep("deploy", "mUSDC", d.mUsdc);
        } else {
            _logStep("reuse ", "mUSDC", d.mUsdc);
        }

        d.mWeth = _envAddr("MWETH", address(0));
        if (d.mWeth == address(0)) {
            d.mWeth = address(new MintableToken("DuBu Mock WETH", "mWETH", 18, CLAIM_WETH));
            _logStep("deploy", "mWETH", d.mWeth);
        } else {
            _logStep("reuse ", "mWETH", d.mWeth);
        }

        d.mWbtc = _envAddr("MWBTC", address(0));
        if (d.mWbtc == address(0)) {
            d.mWbtc = address(new MintableToken("DuBu Mock WBTC", "mWBTC", 8, CLAIM_WBTC));
            _logStep("deploy", "mWBTC", d.mWbtc);
        } else {
            _logStep("reuse ", "mWBTC", d.mWbtc);
        }

        d.factory = _envAddr("UNIV2_FACTORY", address(0));
        if (d.factory == address(0)) {
            d.factory = address(new UniswapV2Factory(deployer));
            _logStep("deploy", "UniswapV2Factory", d.factory);
        } else {
            _logStep("reuse ", "UniswapV2Factory", d.factory);
        }

        d.v2Router = _envAddr("UNIV2_ROUTER02", address(0));
        if (d.v2Router == address(0)) {

            d.v2Router = address(new UniswapV2Router02(d.factory, WETH9));
            _logStep("deploy", "UniswapV2Router02", d.v2Router);
        } else {
            _logStep("reuse ", "UniswapV2Router02", d.v2Router);
        }

        d.pool = _envAddr("PROP_POOL", address(0));
        if (d.pool == address(0)) {
            d.pool = address(new PropPool(r.owner, r.manager, r.updater, r.guardian));
            _logStep("deploy", "PropPool", d.pool);
        } else {
            _logStep("reuse ", "PropPool", d.pool);
        }

        d.router = _envAddr("ROUTER", address(0));
        if (d.router == address(0)) {
            d.router = address(new Router());
            _logStep("deploy", "Router", d.router);
        } else {
            _logStep("reuse ", "Router", d.router);
        }

        d.propAdapter = _envAddr("PROP_ADAPTER", address(0));
        if (d.propAdapter == address(0)) {
            d.propAdapter = address(new PropPoolAdapter());
            _logStep("deploy", "PropPoolAdapter", d.propAdapter);
        } else {
            _logStep("reuse ", "PropPoolAdapter", d.propAdapter);
        }

        d.uniAdapter = _envAddr("UNIV2_ADAPTER", address(0));
        if (d.uniAdapter == address(0)) {
            d.uniAdapter = address(new UniV2Adapter());
            _logStep("deploy", "UniV2Adapter", d.uniAdapter);
        } else {
            _logStep("reuse ", "UniV2Adapter", d.uniAdapter);
        }
    }

    function _configureMarkets(Deployment memory d, address sender) internal returns (bool ok) {
        ok = true;
        uint32 staleWindow = _maxStaleSecs();

        for (uint256 i; i < MARKET_COUNT; ++i) {
            Market memory m = _market(i, d);
            _assertMarketIsSane(m);

            address pair = UniswapV2Factory(d.factory).getPair(m.base, m.quote);
            if (pair == address(0)) {
                pair = UniswapV2Factory(d.factory).createPair(m.base, m.quote);
                _logStep("create", string.concat("UniswapV2Pair ", m.name), pair);
            } else {
                _logStep("reuse ", string.concat("UniswapV2Pair ", m.name), pair);
            }

            (uint16 pairId,) = PropPool(d.pool).pairIdFor(m.base, m.quote);
            if (pairId != 0) {
                console2.log(string.concat("  reuse  PropPool pair ", m.name, " -> id ", vm.toString(uint256(pairId))));
                continue;
            }

            if (PropPool(d.pool).owner() != sender) {
                ok = false;
                console2.log(
                    string.concat(
                        "  SKIP   PropPool.addPair(",
                        m.name,
                        "): sender is not the pool owner (owner=",
                        vm.toString(PropPool(d.pool).owner()),
                        "). Run addPair from the owner key, then re-run this script."
                    )
                );
                continue;
            }

            pairId = PropPool(d.pool).addPair(m.base, m.quote, m.priceScaleExp, staleWindow, uint56(m.minPrice));
            console2.log(string.concat("  addPair ", m.name, " -> id ", vm.toString(uint256(pairId))));
        }
    }

    function _logStep(string memory verb, string memory what, address where) internal pure {
        console2.log(string.concat("  ", verb, " ", _pad(what, 22), " ", vm.toString(where)));
    }

    function _printExports(Deployment memory d) internal pure {
        console2.log("");
        _rule();
        console2.log(" Copy into .env (or export) to make every later run resumable");
        _rule();
        console2.log(string.concat("export MUSDC=", vm.toString(d.mUsdc)));
        console2.log(string.concat("export MWETH=", vm.toString(d.mWeth)));
        console2.log(string.concat("export MWBTC=", vm.toString(d.mWbtc)));
        console2.log(string.concat("export UNIV2_FACTORY=", vm.toString(d.factory)));
        console2.log(string.concat("export UNIV2_ROUTER02=", vm.toString(d.v2Router)));
        console2.log(string.concat("export PROP_POOL=", vm.toString(d.pool)));
        console2.log(string.concat("export ROUTER=", vm.toString(d.router)));
        console2.log(string.concat("export PROP_ADAPTER=", vm.toString(d.propAdapter)));
        console2.log(string.concat("export UNIV2_ADAPTER=", vm.toString(d.uniAdapter)));
        _rule();
    }

    function _rule() internal pure {
        console2.log("---------------------------------------------------------------------------");
    }

    function _assertAffordable(address deployer, uint256 gasBudget) internal view {
        uint256 gasPrice = _envUint("GAS_PRICE_WEI", DEFAULT_GAS_PRICE_WEI);
        uint256 required = gasBudget * gasPrice;
        uint256 balance = deployer.balance;

        console2.log(string.concat("  gas budget (assumed)  ", vm.toString(gasBudget), " gas"));
        console2.log(string.concat("  gas price  (assumed)  ", vm.toString(gasPrice), " wei"));
        console2.log(string.concat("  cost at that price    ", _ether(required), " ETH"));
        console2.log(string.concat("  deployer balance      ", _ether(balance), " ETH"));

        if (balance < required) {
            console2.log("  INSUFFICIENT BALANCE. Top up the deployer or lower GAS_PRICE_WEI if the");
            console2.log("  measured price is genuinely lower than the assumption above.");
            revert("DubuScript: deployer cannot cover the gas budget");
        }
        console2.log(string.concat("  headroom              ", vm.toString(balance / required), "x"));
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

    function _ether(uint256 weiAmount) internal pure returns (string memory) {
        uint256 whole = weiAmount / 1e18;
        uint256 frac = (weiAmount % 1e18) / 1e12;
        string memory f = vm.toString(frac);
        while (bytes(f).length < 6) {
            f = string.concat("0", f);
        }
        return string.concat(vm.toString(whole), ".", f);
    }

    function _units(uint256 amount, uint8 decimals) internal pure returns (string memory) {
        uint256 denom = 10 ** uint256(decimals);
        uint256 frac = amount % denom;
        string memory f = decimals >= 6
            ? vm.toString(frac / (10 ** (uint256(decimals) - 6)))
            : vm.toString(frac * (10 ** (6 - uint256(decimals))));
        while (bytes(f).length < 6) {
            f = string.concat("0", f);
        }
        return string.concat(vm.toString(amount / denom), ".", f);
    }

    function _pad(string memory s, uint256 width) internal pure returns (string memory) {
        while (bytes(s).length < width) {
            s = string.concat(s, " ");
        }
        return s;
    }
}
