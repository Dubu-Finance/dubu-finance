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

/// @notice Every address the deployment produces. Order matches the dependency order they are
///         created in, which is also the order they have to be supplied in when resuming.
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

/// @notice The four PropPool roles, resolved from env with a deployer fallback.
struct Roles {
    address owner;
    address manager;
    address updater;
    address guardian;
}

/// @notice One comparison market, fully resolved: addresses, decimals, the derived price scale,
///         the encoded ladder mid, and the venue sizing both venues are seeded to.
struct Market {
    string name;
    address base;
    address quote;
    uint8 baseDecimals;
    uint8 quoteDecimals;
    /// @dev Reference price in *whole* quote tokens per *whole* base token. Both venues are
    ///      configured from this and neither venue's own state is ever used as the yardstick.
    uint256 midWhole;
    uint8 priceScaleExp;
    /// @dev `10 ** priceScaleExp`. `quoteAmount = baseAmount * price / scale`.
    uint256 scale;
    /// @dev `midWhole` encoded into the pool's price units.
    uint256 mid;
    uint256 minPrice;
    uint256 tvlBase;
    uint256 tvlQuote;
    uint96 capacity;
    address pair;
}

/// @title DubuScript
/// @notice Shared base for `Deploy` and `Demo`: the market definitions, the price-scale
///         derivation, env plumbing that tolerates the empty values `.env.example` ships, and the
///         table formatting.
///
/// Nothing here broadcasts. Every function is either pure, a view, or a helper the concrete script
/// calls from inside its own `vm.startBroadcast()` window — so that "what gets sent" is decided in
/// one place per script and is readable there.
abstract contract DubuScript is Script {
    // ---------------------------------------------------------------------
    // Chain facts. Measured 2026-07-26 against https://sepolia-rpc.giwa.io, not assumed.
    // ---------------------------------------------------------------------

    uint256 internal constant GIWA_SEPOLIA = 91342;

    /// @dev Genesis pre-installs. Used, never redeployed.
    address internal constant WETH9 = 0x4200000000000000000000000000000000000006;
    address internal constant PERMIT2 = 0x000000000022D473030F116dDEE9F6B43aC78BA3;
    address internal constant MULTICALL3 = 0xcA11bde05977b3631167028862bE2a173976CA11;
    address internal constant PYTH = 0x2880aB155794e7179c9eE2e38200202908C17B43;

    /// @dev Measured gas price was 1,000,251 wei (0.001 gwei). The default here is deliberately
    ///      ~2x that: the point of the preflight is to fail loudly on a thin balance, and a
    ///      preflight that assumes the cheapest observed price is not a check.
    uint256 internal constant DEFAULT_GAS_PRICE_WEI = 2_000_000;

    // ---------------------------------------------------------------------
    // Ladder — identical to test/Integration.t.sol, on purpose
    // ---------------------------------------------------------------------

    uint256 internal constant BPS = 10_000;
    uint256 internal constant HALF_SPREAD_BPS = 5;
    uint256 internal constant WIDTH_BPS = 25;

    /// @notice Per-venue TVL, in whole quote tokens on *each* side. $10M base + $10M quote = $20M.
    uint256 internal constant TVL_QUOTE_WHOLE = 10_000_000;

    /// @notice Per-epoch capacity, in whole quote tokens of notional, on each side.
    uint256 internal constant CAPACITY_QUOTE_WHOLE = 2_000_000;

    /// @notice How far the reference price may move before a ladder end no longer fits `uint56`.
    ///
    /// @dev `priceScaleExp` is immutable per pair, so choosing it is choosing how much room the
    ///      pair has left. `PropPool.addPair` says to take the *largest* exponent that fits, which
    ///      is a gas argument (the bisection converges in ~log2(size/price) steps). Taken
    ///      literally it also leaves a WETH/USDC pair unable to quote a 4x move without a new
    ///      pair id. 8x is the compromise: on both markets here it costs exactly one decimal
    ///      exponent against the maximum, which the measured table in `addPair` prices at ~4% of a
    ///      swap's gas.
    uint256 internal constant PRICE_HEADROOM = 8;

    uint256 internal constant MAX_PRICE = type(uint56).max;

    /// @notice Deadline slack on every swap and route the scripts submit.
    /// @dev Generous because a broadcast run is a few hundred sequential transactions and the
    ///      script's `block.timestamp` is the fork's, not the one the tx eventually lands in.
    uint256 internal constant DEADLINE_SLACK = 1 hours;

    // ---------------------------------------------------------------------
    // Faucet drips — one $10k-equivalent claim per token per address per day
    // ---------------------------------------------------------------------

    uint256 internal constant CLAIM_USDC = 10_000e6; // $10,000
    uint256 internal constant CLAIM_WETH = 5e18; // 5 mWETH  @ $2,000
    uint256 internal constant CLAIM_WBTC = 1e7; // 0.1 mWBTC @ $100,000

    // =====================================================================
    // Market definitions
    // =====================================================================

    uint256 internal constant MARKET_COUNT = 2;

    /// @notice Decimals and reference price per market, with no addresses attached.
    ///
    /// @dev Two markets, and the asymmetry is the whole reason for the second one. `priceScaleExp`
    ///      is the identity when both tokens share a decimals value, so a deployment built from
    ///      one 18/6 pair leaves the *other* shape of that code path — an 8-decimal base — dead on
    ///      chain. 18/6 and 8/6 together exercise it in both directions of magnitude.
    function _marketSpec(uint256 i)
        internal
        pure
        returns (string memory name, uint8 baseDecimals, uint8 quoteDecimals, uint256 midWhole)
    {
        if (i == 0) return ("mWETH/mUSDC", 18, 6, 2_000);
        if (i == 1) return ("mWBTC/mUSDC", 8, 6, 100_000);
        revert("DubuScript: no such market");
    }

    /// @notice The full market, with addresses filled in from `d`.
    function _market(uint256 i, Deployment memory d) internal view returns (Market memory m) {
        (m.name, m.baseDecimals, m.quoteDecimals, m.midWhole) = _marketSpec(i);
        m.base = i == 0 ? d.mWeth : d.mWbtc;
        m.quote = d.mUsdc;

        m.priceScaleExp = _priceScaleExpFor(m.baseDecimals, m.quoteDecimals, m.midWhole);
        m.scale = 10 ** uint256(m.priceScaleExp);
        m.mid = _encodeMid(m.midWhole, m.baseDecimals, m.quoteDecimals, m.priceScaleExp);

        // Absolute floor at half the reference price, independent of any oracle. Comfortably
        // below `minBid` (which sits ~30bp under the mid) and comfortably above zero, which is
        // what `addPair` requires.
        m.minPrice = m.mid / 2;

        // Equal notional on both legs: $10M of base and $10M of quote, per venue.
        m.tvlQuote = TVL_QUOTE_WHOLE * (10 ** uint256(m.quoteDecimals));
        m.tvlBase = (TVL_QUOTE_WHOLE * (10 ** uint256(m.baseDecimals))) / m.midWhole;
        // Capacity is BASE-denominated on both sides (PropCurve amendment 1), so the $2M/epoch
        // budget has to be converted at the reference price before it is posted.
        m.capacity = uint96((CAPACITY_QUOTE_WHOLE * (10 ** uint256(m.baseDecimals))) / m.midWhole);

        if (d.factory != address(0)) m.pair = UniswapV2Factory(d.factory).getPair(m.base, m.quote);
    }

    // =====================================================================
    // Price scale
    // =====================================================================

    /// @notice `price` such that `quoteAmount = baseAmount * price / 10**exp` reproduces
    ///         `midWhole` whole quote tokens for one whole base token.
    function _encodeMid(uint256 midWhole, uint8 baseDecimals, uint8 quoteDecimals, uint8 exp)
        internal
        pure
        returns (uint256)
    {
        return (midWhole * (10 ** uint256(quoteDecimals)) * (10 ** uint256(exp))) / (10 ** uint256(baseDecimals));
    }

    /// @notice The largest `priceScaleExp` at which this market's whole ladder — plus
    ///         `PRICE_HEADROOM` of room for the reference price to move — still fits `uint56`.
    ///
    /// @dev Searched downward from `PropCurve.MAX_PRICE_SCALE_EXP` rather than derived in closed
    ///      form, because the binding constraint is the *top of the ladder* after two bps
    ///      multiplications, not the mid, and reproducing that rounding in an inverse formula is
    ///      more code than 39 iterations of a pure loop that runs once per market off-chain.
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

    /// @notice The four ladder prices, built symmetrically around `mid`.
    /// @dev Byte-for-byte the arithmetic of `IntegrationTest._pushLadder`, so the on-chain sweep
    ///      and the off-chain test are quoting the same book.
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

    /// @notice The `updateQuote` calldata word for one pair.
    function _packLadder(uint16 pairId, uint256 mid) internal pure returns (uint256) {
        (uint256 minBid, uint256 maxBid, uint256 minAsk, uint256 maxAsk) = _ladder(mid);
        return minBid | (maxBid << 56) | (minAsk << 112) | (maxAsk << 168) | (uint256(pairId) << 224);
    }

    /// @notice Everything that has to hold before a single transaction is signed.
    ///
    /// @dev Each of these is a revert on chain, several of them silent-ish: a price that does not
    ///      fit `uint56` is truncated by the packing rather than rejected, and a capacity outside
    ///      the curve's shared domain is rejected only at `refreshCapacity`, several transactions
    ///      after the pair was created with an immutable exponent.
    function _assertMarketIsSane(Market memory m) internal pure {
        (uint256 minBid,,, uint256 maxAsk) = _ladder(m.mid);

        require(m.priceScaleExp <= PropCurve.MAX_PRICE_SCALE_EXP, "priceScaleExp above PropCurve max");
        require(m.mid != 0, "encoded mid is zero: priceScaleExp too small for these decimals");
        require(maxAsk <= MAX_PRICE, "maxAsk does not fit uint56");
        require(minBid >= m.minPrice, "minBid below the pair's own minPrice floor");
        require(m.minPrice != 0 && m.minPrice <= MAX_PRICE, "minPrice out of range");
        require(m.capacity != 0, "capacity is zero");
        require(m.tvlBase != 0 && m.tvlQuote != 0, "TVL is zero");

        // The bound `_refreshCapacity` enforces, checked here so it cannot surface as a revert
        // three transactions after the immutable exponent was fixed.
        require(
            uint256(m.capacity) * MAX_PRICE <= PropCurve.MAX_AMOUNT_OUT * m.scale, "capacity outside the curve domain"
        );
    }

    // =====================================================================
    // Env plumbing
    // =====================================================================

    /// @notice `key` as an address, or `dflt` when it is unset **or set to the empty string**.
    ///
    /// @dev The empty case is not hypothetical: `.env.example` ships `OWNER=`, `MANAGER=`,
    ///      `UPDATER=`, `GUARDIAN=` and `PRIVATE_KEY=` all empty, and the Makefile `export`s
    ///      whatever `.env` holds. `vm.envOr(name, address)` reverts on an unparseable value, so
    ///      going through the string form is what makes "present but blank" mean "not set".
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

    /// @notice All four roles default to the deployer. Production splits them; testnet does not.
    function _roles(address deployer) internal view returns (Roles memory r) {
        r.owner = _envAddr("OWNER", deployer);
        r.manager = _envAddr("MANAGER", deployer);
        r.updater = _envAddr("UPDATER", deployer);
        r.guardian = _envAddr("GUARDIAN", deployer);
    }

    /// @notice Freshness window for every pair created by these scripts.
    /// @dev Default 3600s, not the 60s `test/Integration.t.sol` uses. The test pushes a ladder and
    ///      swaps against it in the same block; a broadcast run puts minutes and a few hundred
    ///      transactions between the two, and a 60s window would make the demo fail for a reason
    ///      that has nothing to do with the demo. A live quoter runs well inside 60 — that is a
    ///      statement about the quoter, not about this constant.
    function _maxStaleSecs() internal view returns (uint32) {
        return uint32(_envUint("MAX_STALE_SECS", 3600));
    }

    // =====================================================================
    // Deployment — shared by Deploy and Demo so there is one definition
    // =====================================================================

    /// @notice Deploy anything whose address is not already supplied by env. Call inside a
    ///         broadcast window.
    ///
    /// @dev Resumability is the whole design here. A script that half-succeeds and cannot be
    ///      re-run costs more than it saves, so every step is `read env -> skip or deploy` and
    ///      every skip is logged with the address it reused.
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
            // WETH9 is the genesis pre-install. Nothing in the demo takes the ETH path, but a
            // Router02 wired to the wrong WETH is a trap for whoever integrates next.
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

    /// @notice Create the V2 pair and register the PropPool pair for every market. Idempotent on
    ///         both halves. Call inside a broadcast window.
    /// @return ok false when at least one `addPair` had to be skipped because the sender is not
    ///         the pool's owner — the caller decides whether that is fatal.
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

    // =====================================================================
    // Logging
    // =====================================================================

    function _logStep(string memory verb, string memory what, address where) internal pure {
        console2.log(string.concat("  ", verb, " ", _pad(what, 22), " ", vm.toString(where)));
    }

    /// @notice The copy-pasteable address block. Printed by both scripts, because either one can
    ///         be the thing that created these addresses and a resumable run needs them either way.
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

    /// @notice Balance / gas-budget preflight. Reverts rather than discovering it mid-deployment.
    /// @param gasBudget conservative upper bound on the gas the script will consume.
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

    // =====================================================================
    // Formatting
    // =====================================================================

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

    /// @dev wei -> a fixed 6-decimal ETH string. Enough to read a testnet balance at a glance and
    ///      short enough to sit in a log line.
    function _ether(uint256 weiAmount) internal pure returns (string memory) {
        uint256 whole = weiAmount / 1e18;
        uint256 frac = (weiAmount % 1e18) / 1e12; // 6 dp
        string memory f = vm.toString(frac);
        while (bytes(f).length < 6) {
            f = string.concat("0", f);
        }
        return string.concat(vm.toString(whole), ".", f);
    }

    /// @dev A raw token amount rendered as `whole.ffffff`, always six decimal places. Six is
    ///      enough to read any fill here (0.000001 mWBTC is $0.10) and short enough that columns
    ///      stay aligned, which raw 18-decimal integers do not.
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
