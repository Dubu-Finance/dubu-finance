// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import {DubuScript} from "./DubuScript.sol";
import {PropPool} from "../src/PropPool.sol";
import {MockERC20} from "../src/mocks/MockERC20.sol";
import {console2} from "forge-std/console2.sol";

contract AddMarkets is DubuScript {

    struct NewMarket {
        string name;
        string symbol;
        uint8 decimals;
        uint256 midWhole;

        string venueSymbol;
    }

    function _newMarkets() internal pure returns (NewMarket[] memory out) {
        out = new NewMarket[](4);

        out[0] = NewMarket("Mock Apple", "mAAPL", 8, 339, "xyz:AAPL");
        out[1] = NewMarket("Mock Tesla", "mTSLA", 8, 307, "xyz:TSLA");
        out[2] = NewMarket("Mock SK Hynix", "mSKHY", 8, 133, "xyz:SKHY");
        out[3] = NewMarket("Mock SpaceX", "mSPCX", 8, 115, "xyz:SPCX");
    }

    function _addOne(PropPool pool, address quote, uint8 quoteDecimals, address me, NewMarket memory s)
        private
        returns (uint16 pairId)
    {
        MockERC20 token = new MockERC20(s.name, s.symbol, s.decimals);
        uint8 exp = _priceScaleExpFor(s.decimals, quoteDecimals, s.midWhole);
        uint256 mid = _encodeMid(s.midWhole, s.decimals, quoteDecimals, exp);

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
