// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import {DubuScript} from "./DubuScript.sol";
import {PropPool} from "../src/PropPool.sol";
import {MockERC20} from "../src/mocks/MockERC20.sol";
import {console2} from "forge-std/console2.sol";

/// @title Add markets to a pool that is already live.
///
/// @notice `Deploy.s.sol` builds a deployment from nothing. This adds to one that is running, which
///         is a different job: the pool, the router, the adapters and the two existing markets stay
///         exactly as they are, and nothing here can touch them.
///
/// @dev Why a separate script rather than extending `MARKET_COUNT`:
///
///      `Deploy` derives every address from its own run. Raising `MARKET_COUNT` there and re-running
///      it against a live deployment would re-derive the existing markets too, and `addPair` reverts
///      with `PairExists` — so the run fails after having already deployed the new tokens, leaving
///      orphans behind. Adding is not deploying, and the two want different scripts.
///
///      `priceScaleExp` is derived by [`DubuScript::_priceScaleExpFor`], the same helper the original
///      deployment used, because it is immutable per pair: picking it wrong is a redeploy of that
///      market, not a config change.
///
/// @dev ⚠️ TESTNET ONLY. The tokens this deploys have an unrestricted `mint`.
contract AddMarkets is DubuScript {
    /// @dev Reference prices in whole quote per whole base. Only the ratio matters for the encoding;
    ///      the updater overwrites the ladder within a second of starting.
    struct NewMarket {
        string name;
        string symbol;
        uint8 decimals;
        uint256 midWhole;
        /// @dev The venue symbol the hedge leg will use. Recorded here so the on-chain listing and
        ///      the hedge configuration are decided in one place rather than drifting apart.
        string venueSymbol;
    }

    function _newMarkets() internal pure returns (NewMarket[] memory out) {
        out = new NewMarket[](4);
        // Equities, hedged on Hyperliquid's `xyz` HIP-3 book rather than Binance, which carries
        // none of them. All four quote there -- including SPCX, which is SpaceX: unlisted, and with
        // a perp anyway. Reference prices are the mids read on 2026-07-28; only the ratio matters
        // for the encoding, and the updater overwrites the ladder within a second of starting.
        //
        // 8 decimals throughout: equities are quoted to cents and fractional shares are the point,
        // so the extra precision costs nothing and 18 would waste the price range.
        out[0] = NewMarket("Mock Apple", "mAAPL", 8, 339, "xyz:AAPL");
        out[1] = NewMarket("Mock Tesla", "mTSLA", 8, 307, "xyz:TSLA");
        out[2] = NewMarket("Mock SK Hynix", "mSKHY", 8, 133, "xyz:SKHY");
        out[3] = NewMarket("Mock SpaceX", "mSPCX", 8, 115, "xyz:SPCX");
    }

    /// @dev One market, in its own frame. The loop body was inline and hit "stack too deep": the
    ///      encoding needs the spec, the derived exponent, the mid, both sizings and two addresses
    ///      live at once, which is more than the EVM's 16 reachable slots.
    function _addOne(PropPool pool, address quote, uint8 quoteDecimals, address me, NewMarket memory s)
        private
        returns (uint16 pairId)
    {
        MockERC20 token = new MockERC20(s.name, s.symbol, s.decimals);
        uint8 exp = _priceScaleExpFor(s.decimals, quoteDecimals, s.midWhole);
        uint256 mid = _encodeMid(s.midWhole, s.decimals, quoteDecimals, exp);

        // Same shape as the original deployment: equal notional on both legs, and a floor at half
        // the reference price so it is independent of any oracle.
        uint256 tvlBase = (TVL_QUOTE_WHOLE * (10 ** uint256(s.decimals))) / s.midWhole;

        token.mint(me, tvlBase * 4);
        token.approve(address(pool), type(uint256).max);

        pairId = pool.addPair(address(token), quote, exp, _maxStaleSecs(), uint56(mid / 2));
        pool.deposit(address(token), tvlBase);

        console2.log(
            string.concat(
                "    ",
                _pad(s.symbol, 6),
                " pair ",
                vm.toString(uint256(pairId)),
                "  exp ",
                vm.toString(uint256(exp)),
                "  hedge ",
                s.venueSymbol
            )
        );
        console2.log("      token  ", address(token));
    }

    function run() external {
        // `_maxStaleSecs` reads MAX_STALE_SECS from env with the same default the original
        // deployment used, so a market added later inherits the freshness window the others have.
        address poolAddr = vm.envAddress("PROP_POOL");
        address quote = vm.envAddress("MUSDC");
        uint256 pk = vm.envUint("PRIVATE_KEY");
        address me = vm.addr(pk);

        PropPool pool = PropPool(poolAddr);
        uint8 quoteDecimals = MockERC20(quote).decimals();

        console2.log("");
        console2.log("  Adding markets to a live pool");
        console2.log("    pool         ", poolAddr);
        console2.log("    quote        ", quote);
        console2.log("    pairs before ", pool.pairCount());
        console2.log("");

        NewMarket[] memory specs = _newMarkets();

        vm.startBroadcast(pk);
        for (uint256 i; i < specs.length; ++i) {
            _addOne(pool, quote, quoteDecimals, me, specs[i]);
        }
        vm.stopBroadcast();

        console2.log("");
        console2.log("    pairs after  ", pool.pairCount());
        console2.log("");
        console2.log("  The quote leg is NOT topped up here. Each new market draws on the same mUSDC");
        console2.log("  reserve the existing two share, so listing three more without adding quote");
        console2.log("  splits one balance five ways -- fund it before raising capacity.");
    }
}
