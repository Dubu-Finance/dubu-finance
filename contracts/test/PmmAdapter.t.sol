// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {Test, Vm} from "forge-std/Test.sol";

import {Router, RouteParams, Batch, Hop, SwapStep} from "../src/Router.sol";
import {RouteDecoder} from "../src/libraries/RouteDecoder.sol";
import {IAdapter} from "../src/interfaces/IAdapter.sol";
import {PmmAdapter, IERC20Approve} from "../src/adapters/PmmAdapter.sol";
import {PmmSettle} from "../src/PmmSettle.sol";
import {IPmmSettle} from "../src/interfaces/IPmmSettle.sol";
import {MockERC20} from "../src/mocks/MockERC20.sol";

contract UnapprovableToken is MockERC20 {
    constructor() MockERC20("Unapprovable", "UNA", 18) {}

    function approve(address, uint256) public pure override returns (bool) {
        return false;
    }
}

abstract contract PmmAdapterBase is Test {
    Router internal router;
    PmmAdapter internal adapter;
    PmmSettle internal settle;

    MockERC20 internal makerAsset;
    MockERC20 internal takerAsset;

    uint256 internal constant MAKER_PK = 0xA11CE;
    address internal maker;
    address internal payer = address(0x9A7E4);
    address internal receiver = address(0x4EC0);

    uint256 internal constant MAKER_SIZE = 3_000e6;
    uint256 internal constant TAKER_SIZE = 1e18;

    bytes32 internal constant TYPEHASH = 0x23e655e78a91115e92aff2d730688fc421a3773ea96b4afcd69c21acf9e8be56;

    function setUp() public virtual {
        maker = vm.addr(MAKER_PK);

        router = new Router();
        settle = new PmmSettle();
        adapter = new PmmAdapter();

        makerAsset = new MockERC20("Maker", "MKR", 6);
        takerAsset = new MockERC20("Taker", "TKR", 18);

        makerAsset.mint(maker, 1_000_000e6);
        vm.prank(maker);
        makerAsset.approve(address(settle), type(uint256).max);

        takerAsset.mint(payer, 1_000e18);
        vm.prank(payer);
        takerAsset.approve(address(router), type(uint256).max);

        vm.warp(1_800_000_000);
    }

    function _order() internal view returns (IPmmSettle.Order memory o) {
        o = IPmmSettle.Order({
            maker: maker,
            makerAsset: address(makerAsset),
            takerAsset: address(takerAsset),
            makerAmount: MAKER_SIZE,
            takerAmount: TAKER_SIZE,
            nonce: 1,
            expiry: uint64(block.timestamp + 60),
            decayStart: 0,
            decayPerSec: 0,
            decayCap: 0,
            minFillBps: 0
        });
    }

    function _sign(IPmmSettle.Order memory o) internal view returns (bytes memory) {
        bytes32 structHash = keccak256(
            abi.encode(
                TYPEHASH,
                o.maker,
                o.makerAsset,
                o.takerAsset,
                o.makerAmount,
                o.takerAmount,
                o.nonce,
                o.expiry,
                o.decayStart,
                o.decayPerSec,
                o.decayCap,
                o.minFillBps
            )
        );
        bytes32 separator = keccak256(
            abi.encode(
                keccak256("EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)"),
                keccak256("DuBu PmmSettle"),
                keccak256("1"),
                block.chainid,
                address(settle)
            )
        );
        (uint8 v, bytes32 r, bytes32 s) =
            vm.sign(MAKER_PK, keccak256(abi.encodePacked("\x19\x01", separator, structHash)));
        return abi.encodePacked(r, s, v);
    }

    function _payload(IPmmSettle.Order memory o, uint32 maxDecayPpm) internal view returns (bytes memory) {
        return adapter.encodePayload(o, _sign(o), maxDecayPpm);
    }

    function _route(bytes memory payload, uint256 amountIn, bool fundAdapter, bool reverse)
        internal
        view
        returns (RouteParams memory p)
    {
        SwapStep[] memory steps = new SwapStep[](1);
        steps[0] = SwapStep({
            adapter: address(adapter),
            rawData: RouteDecoder.encode(address(settle), 10_000, reverse, fundAdapter),
            payload: payload
        });

        Hop[] memory hops = new Hop[](1);
        hops[0] = Hop({tokenIn: address(takerAsset), steps: steps});

        Batch[] memory batches = new Batch[](1);
        batches[0] = Batch({weightBps: 10_000, hops: hops});

        p = RouteParams({
            tokenIn: address(takerAsset),
            tokenOut: address(makerAsset),
            receiver: receiver,
            amountIn: amountIn,
            quotedAmountOut: (MAKER_SIZE * amountIn) / TAKER_SIZE,
            deadline: block.timestamp + 300,
            batches: batches
        });
    }

    function _assertAdapterIsClean() internal view {
        assertEq(takerAsset.balanceOf(address(adapter)), 0, "adapter holds taker asset");
        assertEq(makerAsset.balanceOf(address(adapter)), 0, "adapter holds maker asset");
        assertEq(takerAsset.allowance(address(adapter), address(settle)), 0, "adapter left an allowance");
        assertEq(makerAsset.allowance(address(adapter), address(settle)), 0, "adapter left an allowance");
    }
}

contract PmmAdapterRoutingTest is PmmAdapterBase {
    function test_wholeOrderFillsThroughTheRouter() public {
        IPmmSettle.Order memory o = _order();
        RouteParams memory p = _route(_payload(o, type(uint32).max), TAKER_SIZE, true, false);

        vm.prank(payer);
        uint256 out = router.swapExactIn(p, MAKER_SIZE);

        assertEq(out, MAKER_SIZE);
        assertEq(makerAsset.balanceOf(receiver), MAKER_SIZE);
        assertEq(takerAsset.balanceOf(maker), TAKER_SIZE);
        assertEq(takerAsset.balanceOf(payer), 1_000e18 - TAKER_SIZE);
        assertEq(settle.remainingTaker(o), 0);
        _assertAdapterIsClean();
    }

    function test_sellQuoteIsTheSameFillAsSellBase() public {
        IPmmSettle.Order memory o = _order();
        RouteParams memory p = _route(_payload(o, type(uint32).max), TAKER_SIZE / 2, true, true);

        vm.prank(payer);
        uint256 out = router.swapExactIn(p, 1);

        assertEq(out, MAKER_SIZE / 2);
        assertEq(settle.remainingTaker(o), TAKER_SIZE / 2);
        _assertAdapterIsClean();
    }

    function test_routerHoldsNothingAfterwards() public {
        IPmmSettle.Order memory o = _order();
        RouteParams memory p = _route(_payload(o, type(uint32).max), TAKER_SIZE, true, false);

        vm.prank(payer);
        router.swapExactIn(p, MAKER_SIZE);

        assertEq(takerAsset.balanceOf(address(router)), 0);
        assertEq(makerAsset.balanceOf(address(router)), 0);
        assertEq(takerAsset.balanceOf(address(settle)), 0);
        assertEq(makerAsset.balanceOf(address(settle)), 0);
    }

    function test_overFundedLegRefundsTheSurplus() public {
        IPmmSettle.Order memory o = _order();
        bytes memory payload = _payload(o, type(uint32).max);

        address early = address(0xEA11);
        takerAsset.mint(early, 6e17);
        vm.startPrank(early);
        takerAsset.approve(address(settle), 6e17);
        settle.fillOrder(o, _sign(o), 6e17, type(uint32).max, early);
        vm.stopPrank();
        assertEq(settle.remainingTaker(o), 4e17);

        RouteParams memory p = _route(payload, TAKER_SIZE, true, false);
        uint256 payerBefore = takerAsset.balanceOf(payer);

        vm.prank(payer);
        uint256 out = router.swapExactIn(p, 1);

        assertEq(out, (MAKER_SIZE * 4e17) / TAKER_SIZE);

        assertEq(takerAsset.balanceOf(payer), payerBefore - 4e17);
        assertEq(settle.remainingTaker(o), 0);
        _assertAdapterIsClean();
    }

    function test_oneQuoteStreamsToSeveralRoutedTakers() public {
        IPmmSettle.Order memory o = _order();
        bytes memory payload = _payload(o, type(uint32).max);

        address[3] memory payers = [address(0x9A01), address(0x9A02), address(0x9A03)];
        for (uint256 i; i < payers.length; ++i) {
            takerAsset.mint(payers[i], 25e16);
            vm.startPrank(payers[i]);
            takerAsset.approve(address(router), 25e16);

            RouteParams memory p = _route(payload, 25e16, true, false);
            p.receiver = payers[i];
            uint256 out = router.swapExactIn(p, 1);
            vm.stopPrank();

            assertEq(out, (MAKER_SIZE * 25e16) / TAKER_SIZE);
            assertEq(makerAsset.balanceOf(payers[i]), out);
        }
        assertEq(settle.remainingTaker(o), TAKER_SIZE - 75e16);
        _assertAdapterIsClean();
    }

    function test_routeExecutedCarriesQuotedAndRealised() public {
        IPmmSettle.Order memory o = _order();
        o.decayStart = uint64(block.timestamp);
        o.decayPerSec = 5_000;
        o.decayCap = 50_000;
        o.expiry = uint64(block.timestamp + 600);

        RouteParams memory p = _route(_payload(o, type(uint32).max), TAKER_SIZE, true, false);
        vm.warp(uint256(o.decayStart) + 4);

        uint256 realised = (MAKER_SIZE * (1e6 - 20_000)) / 1e6;

        vm.recordLogs();
        vm.prank(payer);
        uint256 out = router.swapExactIn(p, 1);
        assertEq(out, realised);

        Vm.Log[] memory logs = vm.getRecordedLogs();
        bool sawLeg;
        bool sawRoute;
        for (uint256 i; i < logs.length; ++i) {
            if (logs[i].topics[0] == IPmmSettle.OrderFilled.selector) {
                (,, uint256 takerIn, uint256 quoted, uint256 filled, uint256 decayPpm,) =
                    abi.decode(logs[i].data, (address, address, uint256, uint256, uint256, uint256, uint256));
                assertEq(takerIn, TAKER_SIZE);
                assertEq(quoted, MAKER_SIZE);
                assertEq(filled, realised);
                assertEq(decayPpm, 20_000);
                assertGt(quoted, filled);
                sawLeg = true;
            } else if (logs[i].topics[0] == Router.RouteExecuted.selector) {
                (,, uint256 amountOut, uint256 quotedAmountOut,) =
                    abi.decode(logs[i].data, (address, uint256, uint256, uint256, bytes32));
                assertEq(amountOut, realised);
                assertEq(quotedAmountOut, MAKER_SIZE);
                assertLt(amountOut, quotedAmountOut);
                sawRoute = true;
            }
        }
        assertTrue(sawLeg, "OrderFilled not emitted");
        assertTrue(sawRoute, "RouteExecuted not emitted");
    }

    function test_maxDecayPpmRejectsTheLeg() public {
        IPmmSettle.Order memory o = _order();
        o.decayStart = uint64(block.timestamp);
        o.decayPerSec = 5_000;
        o.decayCap = 50_000;
        o.expiry = uint64(block.timestamp + 600);

        RouteParams memory p = _route(_payload(o, 9_999), TAKER_SIZE, true, false);
        vm.warp(uint256(o.decayStart) + 2);

        vm.prank(payer);
        vm.expectRevert(abi.encodeWithSelector(PmmSettle.DecayTooHigh.selector, 10_000, 9_999));
        router.swapExactIn(p, 1);
    }

    function test_missingFundAdapterBitReverts() public {
        IPmmSettle.Order memory o = _order();
        RouteParams memory p = _route(_payload(o, type(uint32).max), TAKER_SIZE, false, false);

        vm.prank(payer);
        vm.expectRevert(PmmAdapter.NothingToFill.selector);
        router.swapExactIn(p, 1);
    }

    function test_exhaustedOrderReverts() public {
        IPmmSettle.Order memory o = _order();
        bytes memory payload = _payload(o, type(uint32).max);

        RouteParams memory p = _route(payload, TAKER_SIZE, true, false);
        vm.prank(payer);
        router.swapExactIn(p, 1);

        RouteParams memory again = _route(payload, TAKER_SIZE, true, false);
        vm.prank(payer);
        vm.expectRevert(PmmAdapter.NothingToFill.selector);
        router.swapExactIn(again, 1);
    }

    function test_aggregateSlippageBoundStillBinds() public {
        IPmmSettle.Order memory o = _order();
        RouteParams memory p = _route(_payload(o, type(uint32).max), TAKER_SIZE, true, false);

        vm.prank(payer);
        vm.expectRevert(abi.encodeWithSelector(Router.InsufficientOutput.selector, MAKER_SIZE, MAKER_SIZE + 1));
        router.swapExactIn(p, MAKER_SIZE + 1);
    }
}

contract PmmAdapterDirectTest is PmmAdapterBase {
    function test_zeroSettleAddressReverts() public {
        bytes memory payload = _payload(_order(), type(uint32).max);
        vm.expectRevert(PmmAdapter.ZeroSettle.selector);
        adapter.sellBase(address(this), address(0), payload);

        vm.expectRevert(PmmAdapter.ZeroSettle.selector);
        adapter.sellQuote(address(this), address(0), payload);
    }

    function test_emptyBalanceReverts() public {
        bytes memory payload = _payload(_order(), type(uint32).max);
        vm.expectRevert(PmmAdapter.NothingToFill.selector);
        adapter.sellBase(address(this), address(settle), payload);
    }

    function test_malformedPayloadReverts() public {
        vm.expectRevert();
        adapter.sellBase(address(this), address(settle), hex"deadbeef");
    }

    function test_encodePayloadIsPlainAbiEncode() public view {
        IPmmSettle.Order memory o = _order();
        bytes memory sig = _sign(o);
        assertEq(adapter.encodePayload(o, sig, 1234), abi.encode(o, sig, uint32(1234)));
    }

    function test_adapterHasNoStorage() public {
        IPmmSettle.Order memory o = _order();
        takerAsset.mint(address(adapter), TAKER_SIZE);
        adapter.sellBase(receiver, address(settle), _payload(o, type(uint32).max));

        for (uint256 slot; slot < 8; ++slot) {
            assertEq(vm.load(address(adapter), bytes32(slot)), bytes32(0));
        }
        _assertAdapterIsClean();
        assertEq(makerAsset.balanceOf(receiver), MAKER_SIZE);
    }

    function test_directCallClampsAndRefunds() public {
        IPmmSettle.Order memory o = _order();
        takerAsset.mint(address(adapter), TAKER_SIZE * 2);

        adapter.sellBase(receiver, address(settle), _payload(o, type(uint32).max));

        assertEq(makerAsset.balanceOf(receiver), MAKER_SIZE);
        assertEq(takerAsset.balanceOf(receiver), TAKER_SIZE);
        assertEq(takerAsset.balanceOf(maker), TAKER_SIZE);
        _assertAdapterIsClean();
    }

    function test_codelessTakerAssetRevertsOnApprove() public {
        IPmmSettle.Order memory o = _order();
        o.takerAsset = address(0xC0DE1E55);
        bytes memory payload = adapter.encodePayload(o, _sign(o), type(uint32).max);

        vm.expectRevert();
        adapter.sellBase(receiver, address(settle), payload);
    }

    function test_tokenRefusingApprovalReverts() public {
        UnapprovableToken stubborn = new UnapprovableToken();
        IPmmSettle.Order memory o = _order();
        o.takerAsset = address(stubborn);
        bytes memory payload = adapter.encodePayload(o, _sign(o), type(uint32).max);

        stubborn.mint(address(adapter), TAKER_SIZE);

        vm.expectRevert(abi.encodeWithSelector(PmmAdapter.ApproveFailed.selector, address(stubborn)));
        adapter.sellBase(receiver, address(settle), payload);
    }

    function test_remainingIsReadLiveAndNeverCached() public {
        IPmmSettle.Order memory o = _order();
        bytes memory payload = _payload(o, type(uint32).max);

        address early = address(0xEA11);
        takerAsset.mint(early, 9e17);
        vm.startPrank(early);
        takerAsset.approve(address(settle), 9e17);
        settle.fillOrder(o, _sign(o), 9e17, type(uint32).max, early);
        vm.stopPrank();

        takerAsset.mint(address(adapter), TAKER_SIZE);
        adapter.sellBase(receiver, address(settle), payload);

        assertEq(makerAsset.balanceOf(receiver), (MAKER_SIZE * 1e17) / TAKER_SIZE);
        assertEq(takerAsset.balanceOf(receiver), TAKER_SIZE - 1e17);
        _assertAdapterIsClean();
    }

    function test_conformsToIAdapter() public {
        IPmmSettle.Order memory o = _order();
        takerAsset.mint(address(adapter), TAKER_SIZE);
        IAdapter(address(adapter)).sellBase(receiver, address(settle), _payload(o, type(uint32).max));
        assertEq(makerAsset.balanceOf(receiver), MAKER_SIZE);
    }

    function test_localApproveInterfaceMatchesTheToken() public {
        assertTrue(IERC20Approve(address(takerAsset)).approve(address(settle), 1));
        assertEq(takerAsset.allowance(address(this), address(settle)), 1);
    }
}
