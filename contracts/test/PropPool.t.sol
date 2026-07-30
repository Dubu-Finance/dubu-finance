// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {Test, stdError} from "forge-std/Test.sol";
import {console2} from "forge-std/console2.sol";
import {PropPool} from "../src/PropPool.sol";
import {IPropPool} from "../src/interfaces/IPropPool.sol";
import {PropCurve} from "../src/libraries/PropCurve.sol";
import {ReentrancyLock} from "../src/libraries/ReentrancyLock.sol";
import {MockERC20} from "../src/mocks/MockERC20.sol";

contract ReenteringToken is MockERC20 {
    PropPool public pool;
    address public tokenIn;
    address public tokenOut;
    uint256 public mode;

    uint256 public observedQuote;

    constructor(uint8 decimals_) MockERC20("Reentering", "RE", decimals_) {}

    function arm(PropPool pool_, address tokenIn_, address tokenOut_, uint256 mode_) external {
        pool = pool_;
        tokenIn = tokenIn_;
        tokenOut = tokenOut_;
        mode = mode_;
    }

    function _transfer(address from, address to, uint256 amount) internal override {
        super._transfer(from, to, amount);
        uint256 m = mode;
        if (m == 0) return;
        mode = 0;
        if (m == 1) {
            pool.swap(tokenIn, tokenOut, 1, 0, address(this), 0, block.timestamp + 1);
        } else if (m == 2) {
            pool.swapWithContractBalance(tokenIn, tokenOut, 0, address(this), 0, block.timestamp + 1);
        } else {

            observedQuote = pool.getAmountOut(tokenIn, tokenOut, 1e18);
        }
    }
}

contract RevertingToken is MockERC20 {
    error Boom();

    bool public revertOnTransfer;

    constructor() MockERC20("Reverting", "RV", 6) {}

    function setRevertOnTransfer(bool on) external {
        revertOnTransfer = on;
    }

    function _transfer(address from, address to, uint256 amount) internal override {
        if (revertOnTransfer) revert Boom();
        super._transfer(from, to, amount);
    }
}

contract PropPoolTest is Test {

    PropPool internal pool;
    MockERC20 internal base;
    MockERC20 internal quote;

    address internal owner = address(0xAA01);
    address internal manager = address(0xAA02);
    address internal updater = address(0xAA03);
    address internal guardian = address(0xAA04);
    address internal taker = address(0xAA05);
    address internal outsider = address(0xAA06);

    uint16 internal constant PAIR = 1;
    uint8 internal constant EXP = 18;
    uint32 internal constant MAX_STALE = 60;
    uint56 internal constant MIN_PRICE = 1_000_000_000;

    uint56 internal constant MIN_BID = 2_970_000_000;
    uint56 internal constant MAX_BID = 2_990_000_000;
    uint56 internal constant MIN_ASK = 3_010_000_000;
    uint56 internal constant MAX_ASK = 3_030_000_000;

    uint96 internal constant BID_CAP = 100e18;

    uint96 internal constant ASK_CAP = 100e18;

    uint256 internal constant ASK_QUOTE_CEILING = 302_000e6;

    uint256 internal constant BASE_INVENTORY = 1_000e18;
    uint256 internal constant QUOTE_INVENTORY = 1_000_000e6;

    function setUp() public {
        base = new MockERC20("Base", "BASE", 18);
        quote = new MockERC20("Quote", "QUOTE", 6);
        pool = new PropPool(owner, manager, updater, guardian);

        vm.prank(owner);
        uint16 id = pool.addPair(address(base), address(quote), EXP, MAX_STALE, MIN_PRICE);
        assertEq(id, PAIR, "first pair id must be 1");

        _push(MIN_BID, MAX_BID, MIN_ASK, MAX_ASK);
        _refresh(BID_CAP, ASK_CAP);
        _fund();

        base.mint(taker, 10_000e18);
        quote.mint(taker, 10_000_000e6);
        vm.startPrank(taker);
        base.approve(address(pool), type(uint256).max);
        quote.approve(address(pool), type(uint256).max);
        vm.stopPrank();
    }

    function _packQuote(uint16 pairId, uint56 minBid, uint56 maxBid, uint56 minAsk, uint56 maxAsk)
        internal
        pure
        returns (uint256)
    {
        return uint256(minBid) | (uint256(maxBid) << 56) | (uint256(minAsk) << 112) | (uint256(maxAsk) << 168)
            | (uint256(pairId) << 224);
    }

    function _packCapacity(uint16 pairId, uint96 bidCap, uint96 askCap) internal pure returns (uint256) {
        return uint256(bidCap) | (uint256(askCap) << 96) | (uint256(pairId) << 224);
    }

    function _push(uint56 minBid, uint56 maxBid, uint56 minAsk, uint56 maxAsk) internal {
        uint256[] memory w = new uint256[](1);
        w[0] = _packQuote(PAIR, minBid, maxBid, minAsk, maxAsk);
        vm.prank(updater);
        pool.updateQuote(w);
    }

    function _refresh(uint96 bidCap, uint96 askCap) internal {
        vm.prank(updater);
        pool.refreshCapacity(PAIR, bidCap, askCap);
    }

    function _fund() internal {
        base.mint(manager, BASE_INVENTORY);
        quote.mint(manager, QUOTE_INVENTORY);
        vm.startPrank(manager);
        base.approve(address(pool), type(uint256).max);
        quote.approve(address(pool), type(uint256).max);
        pool.deposit(address(base), BASE_INVENTORY);
        pool.deposit(address(quote), QUOTE_INVENTORY);
        vm.stopPrank();
    }

    function _bidOut(uint256 amountIn, uint256 used) internal pure returns (uint256) {
        return _bidOutAt(amountIn, used, BID_CAP);
    }

    function _bidOutAt(uint256 amountIn, uint256 used, uint256 cap) internal pure returns (uint256) {
        uint256 span = uint256(MAX_BID) - MIN_BID;
        uint256 num = amountIn * (2 * uint256(MAX_BID) * cap - span * (2 * used + amountIn));
        return num / (2 * cap * (10 ** EXP));
    }

    function _swapExactIn(uint256 amountIn, bool isBid) internal returns (uint256) {
        vm.prank(taker);
        return pool.swap(
            isBid ? address(base) : address(quote),
            isBid ? address(quote) : address(base),
            int256(amountIn),
            0,
            taker,
            0,
            block.timestamp
        );
    }

    enum Gate {
        UpdaterAllowed,
        OwnerOnly,
        ManagerOnly,
        GuardianOnly,
        Permissionless,
        ViewOnly
    }

    struct Fn {
        string name;
        bytes data;
        Gate gate;
    }

    function _surface() internal view returns (Fn[] memory fns) {
        uint256[] memory oneQuote = new uint256[](1);
        oneQuote[0] = _packQuote(PAIR, MIN_BID, MAX_BID, MIN_ASK, MAX_ASK);
        uint256[] memory oneCap = new uint256[](1);
        oneCap[0] = _packCapacity(PAIR, BID_CAP, ASK_CAP);

        fns = new Fn[](46);
        uint256 i;

        fns[i++] = Fn("updateQuote", abi.encodeCall(PropPool.updateQuote, (oneQuote)), Gate.UpdaterAllowed);
        fns[i++] = Fn(
            "refreshCapacity", abi.encodeCall(PropPool.refreshCapacity, (PAIR, BID_CAP, ASK_CAP)), Gate.UpdaterAllowed
        );
        fns[i++] =
            Fn("refreshCapacityBatch", abi.encodeCall(PropPool.refreshCapacityBatch, (oneCap)), Gate.UpdaterAllowed);

        fns[i++] = Fn("transferOwnership", abi.encodeCall(PropPool.transferOwnership, (updater)), Gate.OwnerOnly);
        fns[i++] = Fn("acceptOwnership", abi.encodeCall(PropPool.acceptOwnership, ()), Gate.OwnerOnly);
        fns[i++] = Fn("setManager", abi.encodeCall(PropPool.setManager, (updater)), Gate.OwnerOnly);
        fns[i++] = Fn("setUpdater", abi.encodeCall(PropPool.setUpdater, (updater)), Gate.OwnerOnly);
        fns[i++] = Fn("setGuardian", abi.encodeCall(PropPool.setGuardian, (updater)), Gate.OwnerOnly);
        fns[i++] =
            Fn("addPair", abi.encodeCall(PropPool.addPair, (address(0xB1), address(0xB2), 6, 60, 1)), Gate.OwnerOnly);

        fns[i++] = Fn("withdraw", abi.encodeCall(PropPool.withdraw, (address(quote), 1e6, updater)), Gate.OwnerOnly);

        fns[i++] = Fn("setPyth", abi.encodeCall(PropPool.setPyth, (address(0xB0DE))), Gate.OwnerOnly);

        fns[i++] =
            Fn("setPairConfig", abi.encodeCall(PropPool.setPairConfig, (PAIR, 60, MIN_PRICE, 0, 0)), Gate.ManagerOnly);
        fns[i++] = Fn("deposit", abi.encodeCall(PropPool.deposit, (address(quote), 1e6)), Gate.ManagerOnly);
        fns[i++] = Fn("sync", abi.encodeCall(PropPool.sync, (address(quote))), Gate.ManagerOnly);

        fns[i++] = Fn(
            "setPairOracle", abi.encodeCall(PropPool.setPairOracle, (PAIR, bytes32(uint256(1)), 100, 60, int8(12))),
            Gate.ManagerOnly
        );

        fns[i++] = Fn("setPairDecay", abi.encodeCall(PropPool.setPairDecay, (PAIR, 30)), Gate.ManagerOnly);

        fns[i++] = Fn("pause", abi.encodeCall(PropPool.pause, (PAIR)), Gate.GuardianOnly);
        fns[i++] = Fn("unpause", abi.encodeCall(PropPool.unpause, (PAIR)), Gate.GuardianOnly);
        fns[i++] = Fn("pauseAll", abi.encodeCall(PropPool.pauseAll, ()), Gate.GuardianOnly);
        fns[i++] = Fn("unpauseAll", abi.encodeCall(PropPool.unpauseAll, ()), Gate.GuardianOnly);

        fns[i++] = Fn(
            "swap",
            abi.encodeCall(
                PropPool.swap, (address(base), address(quote), int256(1e18), 0, updater, 0, block.timestamp)
            ),
            Gate.Permissionless
        );
        fns[i++] = Fn(
            "swapWithContractBalance",
            abi.encodeCall(
                PropPool.swapWithContractBalance, (address(base), address(quote), 0, updater, 0, block.timestamp)
            ),
            Gate.Permissionless
        );

        fns[i++] = Fn(
            "getAmountOut", abi.encodeCall(PropPool.getAmountOut, (address(base), address(quote), 1e18)), Gate.ViewOnly
        );
        fns[i++] = Fn(
            "getAmountIn", abi.encodeCall(PropPool.getAmountIn, (address(base), address(quote), 1e6)), Gate.ViewOnly
        );
        fns[i++] = Fn("quoteByPair", abi.encodeCall(PropPool.quoteByPair, (PAIR, true, 1e18)), Gate.ViewOnly);
        fns[i++] = Fn("snapshot", abi.encodeCall(PropPool.snapshot, (PAIR)), Gate.ViewOnly);

        fns[i++] = Fn("effectiveCapacity", abi.encodeCall(PropPool.effectiveCapacity, (PAIR)), Gate.ViewOnly);
        fns[i++] = Fn("pairIdFor", abi.encodeCall(PropPool.pairIdFor, (address(base), address(quote))), Gate.ViewOnly);
        fns[i++] = Fn("getSupportedPairs", abi.encodeCall(PropPool.getSupportedPairs, ()), Gate.ViewOnly);
        fns[i++] = Fn("reserveOf", abi.encodeCall(PropPool.reserveOf, (address(quote))), Gate.ViewOnly);
        fns[i++] = Fn("pairConfig", abi.encodeCall(PropPool.pairConfig, (PAIR)), Gate.ViewOnly);
        fns[i++] = Fn("pairOracle", abi.encodeCall(PropPool.pairOracle, (PAIR)), Gate.ViewOnly);
        fns[i++] = Fn("referencePrice", abi.encodeCall(PropPool.referencePrice, (PAIR)), Gate.ViewOnly);
        fns[i++] = Fn("pyth", abi.encodeWithSignature("pyth()"), Gate.ViewOnly);
        fns[i++] = Fn("owner", abi.encodeWithSignature("owner()"), Gate.ViewOnly);
        fns[i++] = Fn("pendingOwner", abi.encodeWithSignature("pendingOwner()"), Gate.ViewOnly);
        fns[i++] = Fn("manager", abi.encodeWithSignature("manager()"), Gate.ViewOnly);
        fns[i++] = Fn("updater", abi.encodeWithSignature("updater()"), Gate.ViewOnly);
        fns[i++] = Fn("guardian", abi.encodeWithSignature("guardian()"), Gate.ViewOnly);
        fns[i++] = Fn("allPaused", abi.encodeWithSignature("allPaused()"), Gate.ViewOnly);
        fns[i++] = Fn("pairCount", abi.encodeWithSignature("pairCount()"), Gate.ViewOnly);

        fns[i++] = Fn("REF_OK", abi.encodeWithSignature("REF_OK()"), Gate.ViewOnly);
        fns[i++] = Fn("REF_DISABLED", abi.encodeWithSignature("REF_DISABLED()"), Gate.ViewOnly);
        fns[i++] = Fn("REF_UNAVAILABLE", abi.encodeWithSignature("REF_UNAVAILABLE()"), Gate.ViewOnly);
        fns[i++] = Fn("REF_STALE", abi.encodeWithSignature("REF_STALE()"), Gate.ViewOnly);
        fns[i++] = Fn("REF_INVALID", abi.encodeWithSignature("REF_INVALID()"), Gate.ViewOnly);

        assertEq(i, fns.length, "surface census length out of sync with its entries");
    }

    function test_UpdaterCannotMoveFunds() public {
        Fn[] memory fns = _surface();

        uint256 rBase0 = pool.reserveOf(address(base));
        uint256 rQuote0 = pool.reserveOf(address(quote));
        uint256 bBase0 = base.balanceOf(address(pool));
        uint256 bQuote0 = quote.balanceOf(address(pool));
        uint256 uBase0 = base.balanceOf(updater);
        uint256 uQuote0 = quote.balanceOf(updater);

        uint256 allowed;
        for (uint256 i; i < fns.length; ++i) {
            Fn memory f = fns[i];
            vm.prank(updater);
            (bool ok, bytes memory ret) = address(pool).call(f.data);

            if (f.gate == Gate.UpdaterAllowed) {
                assertTrue(ok, string.concat("updater must be able to call ", f.name));
                ++allowed;
            } else if (f.gate == Gate.OwnerOnly) {
                assertFalse(ok, string.concat("updater reached owner-only ", f.name));
                assertEq(bytes4(ret), PropPool.NotOwner.selector, string.concat("wrong gate on ", f.name));
            } else if (f.gate == Gate.ManagerOnly) {
                assertFalse(ok, string.concat("updater reached manager-only ", f.name));
                assertEq(bytes4(ret), PropPool.NotManager.selector, string.concat("wrong gate on ", f.name));
            } else if (f.gate == Gate.GuardianOnly) {
                assertFalse(ok, string.concat("updater reached guardian-only ", f.name));
                assertEq(bytes4(ret), PropPool.NotGuardian.selector, string.concat("wrong gate on ", f.name));
            }

        }

        assertEq(allowed, 3, "exactly three functions may be reachable by the updater");

        assertEq(pool.reserveOf(address(base)), rBase0, "base reserve moved");
        assertEq(pool.reserveOf(address(quote)), rQuote0, "quote reserve moved");
        assertEq(base.balanceOf(address(pool)), bBase0, "pool base balance moved");
        assertEq(quote.balanceOf(address(pool)), bQuote0, "pool quote balance moved");
        assertEq(base.balanceOf(updater), uBase0, "updater gained base");
        assertEq(quote.balanceOf(updater), uQuote0, "updater gained quote");
    }

    function test_UpdaterHasNoPrivilegeOnThePermissionlessPath() public {
        vm.prank(updater);
        vm.expectRevert();
        pool.swap(address(base), address(quote), 1e18, 0, updater, 0, block.timestamp);

        vm.prank(updater);
        vm.expectRevert(PropPool.ZeroAmount.selector);
        pool.swapWithContractBalance(address(base), address(quote), 0, updater, 0, block.timestamp);
    }

    function test_ExternalSurfaceCensus_IsComplete() public {
        bytes memory code = address(pool).code;
        assertGt(code.length, 0, "no runtime code");

        bytes4[] memory known = _knownPush4();

        uint256 unknown;
        uint256 i;
        while (i < code.length) {
            uint8 op = uint8(code[i]);
            if (op == 0x63) {

                if (i + 4 >= code.length) break;
                bytes4 sel = bytes4(
                    uint32(uint8(code[i + 1])) << 24 | uint32(uint8(code[i + 2])) << 16 | uint32(uint8(code[i + 3]))
                        << 8 | uint32(uint8(code[i + 4]))
                );
                bool hit;
                for (uint256 k; k < known.length; ++k) {
                    if (known[k] == sel) {
                        hit = true;
                        break;
                    }
                }
                if (!hit) {
                    console2.log("unclassified PUSH4 in PropPool runtime code:");
                    console2.logBytes4(sel);
                    ++unknown;
                }
                i += 5;
            } else if (op >= 0x60 && op <= 0x7f) {
                i += 1 + (uint256(op) - 0x5f);
            } else {
                ++i;
            }
        }

        assertEq(
            unknown,
            0,
            "unclassified 4-byte constant in PropPool: if you added an external function, classify it in _surface()"
        );
    }

    function _knownPush4() internal view returns (bytes4[] memory known) {
        Fn[] memory fns = _surface();

        bytes4[] memory extra = new bytes4[](44);
        uint256 e;
        extra[e++] = PropPool.NotOwner.selector;
        extra[e++] = PropPool.NotManager.selector;
        extra[e++] = PropPool.NotUpdater.selector;
        extra[e++] = PropPool.NotGuardian.selector;
        extra[e++] = PropPool.ZeroAddress.selector;
        extra[e++] = PropPool.IdenticalTokens.selector;
        extra[e++] = PropPool.PairExists.selector;
        extra[e++] = PropPool.UnknownPair.selector;
        extra[e++] = PropPool.PriceScaleTooLarge.selector;
        extra[e++] = PropPool.ZeroMinPrice.selector;
        extra[e++] = PropPool.ZeroStaleWindow.selector;
        extra[e++] = PropPool.MinPriceStrandsQuote.selector;
        extra[e++] = PropPool.DeadlineExpired.selector;
        extra[e++] = PropPool.ZeroAmount.selector;
        extra[e++] = PropPool.AmountOverflow.selector;
        extra[e++] = PropPool.PoolPaused.selector;
        extra[e++] = PropPool.StaleQuote.selector;
        extra[e++] = PropPool.InsufficientCapacity.selector;

        extra[e++] = PropPool.CapacityOutOfDomain.selector;
        extra[e++] = PropPool.ReserveFloorBreached.selector;
        extra[e++] = PropPool.SlippageExceeded.selector;
        extra[e++] = PropPool.ZeroOutput.selector;
        extra[e++] = PropPool.LengthMismatch.selector;

        extra[e++] = PropPool.PythNotSet.selector;
        extra[e++] = PropPool.DeviationTooLarge.selector;
        extra[e++] = PropPool.ZeroPythStaleWindow.selector;
        extra[e++] = PropPool.BidCeilingExceeded.selector;
        extra[e++] = PropPool.AskFloorBreached.selector;
        extra[e++] = PropPool.ReferenceUnavailable.selector;

        extra[e++] = bytes4(0x96834ad3);
        extra[e++] = PropCurve.AmountExceedsCapacity.selector;
        extra[e++] = PropCurve.ZeroCapacity.selector;
        extra[e++] = PropCurve.ZeroPrice.selector;
        extra[e++] = PropCurve.CrossedBook.selector;
        extra[e++] = PropCurve.BidBelowMinPrice.selector;
        extra[e++] = PropCurve.AmountOutOfDomain.selector;
        extra[e++] = ReentrancyLock.Reentrancy.selector;
        extra[e++] = bytes4(0x90b8ec18);
        extra[e++] = bytes4(0x7939f424);
        extra[e++] = bytes4(0x70a08231);
        extra[e++] = bytes4(0xffffffff);
        extra[e++] = bytes4(0x4e487b71);
        extra[e++] = bytes4(0x08c379a0);
        extra[e++] = bytes4(0x00000000);

        known = new bytes4[](fns.length + e);
        for (uint256 k; k < fns.length; ++k) {
            known[k] = bytes4(fns[k].data);
        }
        for (uint256 k; k < e; ++k) {
            known[fns.length + k] = extra[k];
        }
    }

    function test_Constructor_RejectsZeroRoles() public {
        vm.expectRevert(PropPool.ZeroAddress.selector);
        new PropPool(address(0), manager, updater, guardian);
        vm.expectRevert(PropPool.ZeroAddress.selector);
        new PropPool(owner, address(0), updater, guardian);
        vm.expectRevert(PropPool.ZeroAddress.selector);
        new PropPool(owner, manager, address(0), guardian);
        vm.expectRevert(PropPool.ZeroAddress.selector);
        new PropPool(owner, manager, updater, address(0));
    }

    function test_OwnershipTransfer_IsTwoStep() public {
        vm.prank(owner);
        pool.transferOwnership(outsider);
        assertEq(pool.owner(), owner, "owner must not change on offer");
        assertEq(pool.pendingOwner(), outsider, "pending owner not recorded");

        vm.prank(updater);
        vm.expectRevert(PropPool.NotOwner.selector);
        pool.acceptOwnership();

        vm.prank(outsider);
        pool.acceptOwnership();
        assertEq(pool.owner(), outsider, "ownership not transferred");
        assertEq(pool.pendingOwner(), address(0), "pending owner not cleared");
    }

    function test_RoleSetters_RejectZeroAndGateOnOwner() public {
        vm.startPrank(owner);
        vm.expectRevert(PropPool.ZeroAddress.selector);
        pool.setManager(address(0));
        vm.expectRevert(PropPool.ZeroAddress.selector);
        pool.setUpdater(address(0));
        vm.expectRevert(PropPool.ZeroAddress.selector);
        pool.setGuardian(address(0));

        pool.setManager(outsider);
        pool.setUpdater(outsider);
        pool.setGuardian(outsider);
        vm.stopPrank();

        assertEq(pool.manager(), outsider);
        assertEq(pool.updater(), outsider);
        assertEq(pool.guardian(), outsider);

        uint256[] memory w = new uint256[](1);
        w[0] = _packQuote(PAIR, MIN_BID, MAX_BID, MIN_ASK, MAX_ASK);
        vm.prank(updater);
        vm.expectRevert(PropPool.NotUpdater.selector);
        pool.updateQuote(w);
    }

    function test_AddPair_RegistersBothRouteDirections() public {
        (uint16 id, bool isBid) = pool.pairIdFor(address(base), address(quote));
        assertEq(id, PAIR);
        assertTrue(isBid, "base -> quote must be a bid");

        (id, isBid) = pool.pairIdFor(address(quote), address(base));
        assertEq(id, PAIR);
        assertFalse(isBid, "quote -> base must be an ask");

        IPropPool.Pair[] memory pairs = pool.getSupportedPairs();
        assertEq(pairs.length, 1);
        assertEq(pairs[0].pairId, PAIR);
        assertEq(pairs[0].base, address(base));
        assertEq(pairs[0].quote, address(quote));
    }

    function test_UnknownRoute_IsZeroInBothFields() public view {
        (uint16 id, bool isBid) = pool.pairIdFor(address(base), address(0xDEAD));
        assertEq(id, 0);
        assertFalse(isBid);
        assertEq(pool.pairCount(), 1);
    }

    function test_AddPair_RejectsDuplicate_BothTokenOrderings() public {
        vm.startPrank(owner);
        vm.expectRevert(PropPool.PairExists.selector);
        pool.addPair(address(base), address(quote), EXP, MAX_STALE, MIN_PRICE);

        vm.expectRevert(PropPool.PairExists.selector);
        pool.addPair(address(quote), address(base), EXP, MAX_STALE, MIN_PRICE);
        vm.stopPrank();
        assertEq(pool.pairCount(), 1, "pairCount moved on a rejected add");
    }

    function test_AddPair_RejectsIdenticalTokens() public {
        vm.prank(owner);
        vm.expectRevert(PropPool.IdenticalTokens.selector);
        pool.addPair(address(base), address(base), EXP, MAX_STALE, MIN_PRICE);
    }

    function test_AddPair_RejectsBadConfiguration() public {
        MockERC20 t = new MockERC20("T", "T", 18);
        vm.startPrank(owner);
        vm.expectRevert(PropPool.ZeroAddress.selector);
        pool.addPair(address(0), address(t), EXP, MAX_STALE, MIN_PRICE);
        vm.expectRevert(PropPool.ZeroAddress.selector);
        pool.addPair(address(t), address(0), EXP, MAX_STALE, MIN_PRICE);
        vm.expectRevert(PropPool.PriceScaleTooLarge.selector);
        pool.addPair(address(t), address(base), PropCurve.MAX_PRICE_SCALE_EXP + 1, MAX_STALE, MIN_PRICE);
        vm.expectRevert(PropPool.ZeroMinPrice.selector);
        pool.addPair(address(t), address(base), EXP, MAX_STALE, 0);
        vm.expectRevert(PropPool.ZeroStaleWindow.selector);
        pool.addPair(address(t), address(base), EXP, 0, MIN_PRICE);

        pool.addPair(address(t), address(base), PropCurve.MAX_PRICE_SCALE_EXP, 1, 1);
        vm.stopPrank();
        assertEq(pool.pairCount(), 2);
    }

    function test_AddPair_OnlyOwner() public {
        vm.prank(manager);
        vm.expectRevert(PropPool.NotOwner.selector);
        pool.addPair(address(0xB1), address(0xB2), EXP, MAX_STALE, MIN_PRICE);
    }

    function testFuzz_UpdateQuote_BitPackingRoundTrip(uint56 minBid, uint56 maxBid, uint56 minAsk, uint56 maxAsk)
        public
    {

        minBid = uint56(bound(minBid, MIN_PRICE, type(uint56).max - 3));
        maxBid = uint56(bound(maxBid, minBid, type(uint56).max - 2));
        minAsk = uint56(bound(minAsk, maxBid, type(uint56).max - 1));
        maxAsk = uint56(bound(maxAsk, minAsk == minBid ? minAsk + 1 : minAsk, type(uint56).max));

        _push(minBid, maxBid, minAsk, maxAsk);

        IPropPool.PairSnapshot memory s = pool.snapshot(PAIR);
        assertEq(s.minBid, minBid, "minBid");
        assertEq(s.maxBid, maxBid, "maxBid");
        assertEq(s.minAsk, minAsk, "minAsk");
        assertEq(s.maxAsk, maxAsk, "maxAsk");
        assertEq(s.updatedAt, uint32(block.timestamp), "updatedAt");
    }

    function test_UpdateQuote_ExtremesOfTheField() public {
        _push(MIN_PRICE, type(uint56).max, type(uint56).max, type(uint56).max);
        IPropPool.PairSnapshot memory s = pool.snapshot(PAIR);
        assertEq(s.minBid, MIN_PRICE);
        assertEq(s.maxBid, type(uint56).max);
        assertEq(s.minAsk, type(uint56).max);
        assertEq(s.maxAsk, type(uint56).max);
    }

    function test_UpdateQuote_IgnoresBitsAboveThePairId() public {
        uint256[] memory w = new uint256[](1);
        w[0] = _packQuote(PAIR, MIN_BID, MAX_BID, MIN_ASK, MAX_ASK) | (uint256(type(uint16).max) << 240);
        vm.prank(updater);
        pool.updateQuote(w);
        assertEq(pool.snapshot(PAIR).updatedAt, uint32(block.timestamp), "high bits leaked into updatedAt");
    }

    function test_UpdateQuote_RejectsUnknownPair() public {
        uint256[] memory w = new uint256[](1);
        w[0] = _packQuote(0, MIN_BID, MAX_BID, MIN_ASK, MAX_ASK);
        vm.prank(updater);
        vm.expectRevert(PropPool.UnknownPair.selector);
        pool.updateQuote(w);

        w[0] = _packQuote(2, MIN_BID, MAX_BID, MIN_ASK, MAX_ASK);
        vm.prank(updater);
        vm.expectRevert(PropPool.UnknownPair.selector);
        pool.updateQuote(w);
    }

    function test_UpdateQuote_BatchIsAllOrNothing() public {
        MockERC20 t = new MockERC20("T", "T", 18);
        vm.prank(owner);
        pool.addPair(address(t), address(quote), EXP, MAX_STALE, MIN_PRICE);

        uint256[] memory w = new uint256[](2);
        w[0] = _packQuote(1, MIN_BID + 1, MAX_BID, MIN_ASK, MAX_ASK);
        w[1] = _packQuote(2, MIN_BID, MAX_BID, MIN_ASK, MAX_ASK);
        vm.prank(updater);
        pool.updateQuote(w);
        assertEq(pool.snapshot(1).minBid, MIN_BID + 1);
        assertEq(pool.snapshot(2).minBid, MIN_BID);

        w[0] = _packQuote(1, MIN_BID + 2, MAX_BID, MIN_ASK, MAX_ASK);
        w[1] = _packQuote(2, MAX_ASK, MAX_ASK, MIN_BID, MIN_BID);
        vm.prank(updater);
        vm.expectRevert(PropCurve.CrossedBook.selector);
        pool.updateQuote(w);
        assertEq(pool.snapshot(1).minBid, MIN_BID + 1, "entry 0 was not rolled back");
    }

    function test_UpdateQuote_RejectsEveryInversion() public {
        uint256[] memory w = new uint256[](1);

        w[0] = _packQuote(PAIR, MIN_PRICE - 1, MAX_BID, MIN_ASK, MAX_ASK);
        vm.prank(updater);
        vm.expectRevert(PropCurve.BidBelowMinPrice.selector);
        pool.updateQuote(w);

        uint56[4][4] memory crossed = [
            [MAX_BID, MIN_BID, MIN_ASK, MAX_ASK],
            [MIN_BID, MIN_ASK, MAX_BID, MAX_ASK],
            [MIN_BID, MAX_BID, MAX_ASK, MIN_ASK],
            [MIN_BID, MIN_BID, MIN_BID, MIN_BID]
        ];
        for (uint256 i; i < crossed.length; ++i) {
            w[0] = _packQuote(PAIR, crossed[i][0], crossed[i][1], crossed[i][2], crossed[i][3]);
            vm.prank(updater);
            vm.expectRevert(PropCurve.CrossedBook.selector);
            pool.updateQuote(w);
        }
    }

    function test_UpdateQuote_MinPriceFloorIsPerPair() public {
        MockERC20 t = new MockERC20("T", "T", 18);
        vm.prank(owner);
        pool.addPair(address(t), address(quote), EXP, MAX_STALE, 1);

        uint256[] memory w = new uint256[](1);
        w[0] = _packQuote(2, 1, 2, 3, 4);
        vm.prank(updater);
        pool.updateQuote(w);
        assertEq(pool.snapshot(2).minBid, 1);

        w[0] = _packQuote(PAIR, 1, 2, 3, 4);
        vm.prank(updater);
        vm.expectRevert(PropCurve.BidBelowMinPrice.selector);
        pool.updateQuote(w);
    }

    function test_SetPairConfig_CannotStrandTheStoredLadder() public {
        vm.prank(manager);
        vm.expectRevert(PropPool.MinPriceStrandsQuote.selector);
        pool.setPairConfig(PAIR, MAX_STALE, MIN_BID + 1, 0, 0);

        vm.prank(manager);
        pool.setPairConfig(PAIR, MAX_STALE, MIN_BID, 0, 0);
        assertEq(pool.pairConfig(PAIR).minPrice, MIN_BID);
    }

    function test_SetPairConfig_Guards() public {
        vm.startPrank(manager);
        vm.expectRevert(PropPool.UnknownPair.selector);
        pool.setPairConfig(0, MAX_STALE, MIN_PRICE, 0, 0);
        vm.expectRevert(PropPool.UnknownPair.selector);
        pool.setPairConfig(2, MAX_STALE, MIN_PRICE, 0, 0);
        vm.expectRevert(PropPool.ZeroMinPrice.selector);
        pool.setPairConfig(PAIR, MAX_STALE, 0, 0, 0);
        vm.expectRevert(PropPool.ZeroStaleWindow.selector);
        pool.setPairConfig(PAIR, 0, MIN_PRICE, 0, 0);
        vm.stopPrank();
    }

    function test_SetPairConfig_UnquotedPairMayRaiseTheFloor() public {
        MockERC20 t = new MockERC20("T", "T", 18);
        vm.prank(owner);
        pool.addPair(address(t), address(quote), EXP, MAX_STALE, 1);
        vm.prank(manager);
        pool.setPairConfig(2, MAX_STALE, type(uint56).max, 0, 0);
        assertEq(pool.pairConfig(2).minPrice, type(uint56).max);
    }

    function test_Staleness_SwapRevertsButViewsReturnZero() public {
        uint256 fresh = pool.getAmountOut(address(base), address(quote), 1e18);
        assertGt(fresh, 0, "fixture should quote before going stale");

        vm.warp(block.timestamp + MAX_STALE + 1);

        assertEq(pool.getAmountOut(address(base), address(quote), 1e18), 0, "stale getAmountOut");
        assertEq(pool.getAmountOut(address(quote), address(base), 1000e6), 0, "stale getAmountOut ask");
        assertEq(pool.getAmountIn(address(base), address(quote), 1000e6), 0, "stale getAmountIn");
        assertEq(pool.quoteByPair(PAIR, true, 1e18), 0, "stale quoteByPair");

        assertEq(pool.snapshot(PAIR).minBid, MIN_BID, "snapshot must still report");
        assertEq(pool.getSupportedPairs().length, 1);

        vm.prank(taker);
        vm.expectRevert(PropPool.StaleQuote.selector);
        pool.swap(address(base), address(quote), 1e18, 0, taker, 0, block.timestamp);
    }

    function test_Staleness_ExactBoundary() public {

        vm.warp(block.timestamp + MAX_STALE);
        assertGt(pool.getAmountOut(address(base), address(quote), 1e18), 0, "boundary must be fresh");
        _swapExactIn(1e18, true);

        vm.warp(block.timestamp + 1);
        assertEq(pool.getAmountOut(address(base), address(quote), 1e18), 0, "one past boundary must be stale");
    }

    function test_Staleness_FutureQuoteIsStale() public {
        uint256 t = block.timestamp;
        vm.warp(t - 1);
        assertEq(pool.getAmountOut(address(base), address(quote), 1e18), 0, "future quote must read stale");
        vm.prank(taker);
        vm.expectRevert(PropPool.StaleQuote.selector);
        pool.swap(address(base), address(quote), 1e18, 0, taker, 0, block.timestamp);
    }

    function test_NeverQuoted_IsStale_EvenWithHugeStaleWindow() public {
        MockERC20 t = new MockERC20("T", "T", 18);
        vm.prank(owner);
        uint16 id = pool.addPair(address(t), address(quote), EXP, type(uint32).max, 1);
        _refreshFor(id, BID_CAP, ASK_CAP);

        assertEq(pool.getAmountOut(address(t), address(quote), 1e18), 0, "unquoted pair must not quote");
        vm.prank(taker);
        vm.expectRevert(PropPool.StaleQuote.selector);
        pool.swap(address(t), address(quote), 1e18, 0, taker, 0, block.timestamp);
    }

    function test_Views_ReturnZeroForEveryUnfillableCondition() public {

        assertEq(pool.getAmountOut(address(base), address(0xDEAD), 1e18), 0, "unknown route");
        assertEq(pool.getAmountIn(address(base), address(0xDEAD), 1e6), 0, "unknown route in");
        assertEq(pool.quoteByPair(0, true, 1e18), 0, "pair 0");
        assertEq(pool.quoteByPair(99, true, 1e18), 0, "pair out of range");

        assertEq(pool.snapshot(99).minBid, 0, "snapshot of unknown pair");

        assertEq(pool.getAmountOut(address(base), address(quote), 0), 0, "zero in");
        assertEq(pool.getAmountIn(address(base), address(quote), 0), 0, "zero out");

        assertEq(pool.getAmountOut(address(base), address(quote), uint256(BID_CAP) + 1), 0, "past capacity");
        assertEq(pool.getAmountOut(address(base), address(quote), type(uint256).max), 0, "absurd size");

        _swapExactIn(BID_CAP, true);
        assertEq(pool.getAmountOut(address(base), address(quote), 1e18), 0, "exhausted epoch");
        assertEq(pool.getAmountIn(address(base), address(quote), 1e6), 0, "exhausted epoch in");

        vm.prank(guardian);
        pool.pause(PAIR);
        assertEq(pool.getAmountOut(address(quote), address(base), 1000e6), 0, "paused");
        vm.prank(guardian);
        pool.pauseAll();
        assertEq(pool.getAmountOut(address(quote), address(base), 1000e6), 0, "globally paused");
    }

    function test_BUG2_ViewPathRevertsOnAmountOutOfDomain() public {

        MockERC20 b0 = new MockERC20("W0", "W0", 18);
        MockERC20 q0 = new MockERC20("X0", "X0", 18);
        vm.prank(owner);
        uint16 rejected = pool.addPair(address(b0), address(q0), 0, MAX_STALE, 1);
        vm.prank(updater);
        vm.expectRevert(PropPool.CapacityOutOfDomain.selector);
        pool.refreshCapacity(rejected, type(uint96).max, type(uint96).max);

        vm.prank(updater);
        vm.expectRevert(PropPool.CapacityOutOfDomain.selector);
        pool.refreshCapacity(rejected, MAX_CAP_AT_EXP0 + 1, 0);
        vm.prank(updater);
        pool.refreshCapacity(rejected, MAX_CAP_AT_EXP0, MAX_CAP_AT_EXP0);

        (uint16 id, MockERC20 b2, MockERC20 q2) = _addWidePair();

        uint256 whole = pool.getAmountOut(address(b2), address(q2), MAX_CAP_AT_EXP0);
        assertGt(whole, 0, "the whole epoch must still quote");
        assertLe(whole, type(uint128).max, "the admitted maximum left the shared domain");

        assertEq(pool.quoteByPair(id, true, uint256(MAX_CAP_AT_EXP0) + 1), 0, "sizes past capacity read 0");
        assertEq(pool.getAmountOut(address(b2), address(q2), type(uint96).max), 0, "over-capacity size must read 0");
        assertEq(pool.getAmountOut(address(b2), address(q2), type(uint256).max), 0, "absurd size must read 0");
        assertEq(pool.quoteByPair(id, false, type(uint256).max), 0, "absurd ask size must read 0");
    }

    function test_BUG2b_GetAmountInOverflowsInsteadOfReturningZero() public {
        vm.prank(manager);
        pool.setPairConfig(PAIR, MAX_STALE, MIN_BID, 1, 1);

        uint256 got = pool.getAmountIn(address(base), address(quote), type(uint256).max);
        assertEq(got, 0, "BUG2b: getAmountIn panicked instead of returning 0");
    }

    function test_Pause_Unpause_PerPair() public {
        vm.prank(guardian);
        pool.pause(PAIR);
        assertEq(pool.snapshot(PAIR).flags & 1, 1, "paused flag not set");

        vm.prank(taker);
        vm.expectRevert(PropPool.PoolPaused.selector);
        pool.swap(address(base), address(quote), 1e18, 0, taker, 0, block.timestamp);

        vm.prank(guardian);
        pool.unpause(PAIR);
        assertEq(pool.snapshot(PAIR).flags & 1, 0, "paused flag not cleared");
        assertGt(_swapExactIn(1e18, true), 0, "swap must work after unpause");
    }

    function test_PauseAll_UnpauseAll() public {
        vm.prank(guardian);
        pool.pauseAll();
        assertTrue(pool.allPaused());
        vm.prank(taker);
        vm.expectRevert(PropPool.PoolPaused.selector);
        pool.swap(address(base), address(quote), 1e18, 0, taker, 0, block.timestamp);

        vm.prank(guardian);
        pool.unpauseAll();
        assertFalse(pool.allPaused());
        assertGt(_swapExactIn(1e18, true), 0, "swap must work after unpauseAll");
    }

    function test_Pause_OnlyGuardian() public {
        vm.startPrank(owner);
        vm.expectRevert(PropPool.NotGuardian.selector);
        pool.pause(PAIR);
        vm.expectRevert(PropPool.NotGuardian.selector);
        pool.pauseAll();
        vm.stopPrank();
    }

    function test_Pause_UnknownPair() public {
        vm.startPrank(guardian);
        vm.expectRevert(PropPool.UnknownPair.selector);
        pool.pause(0);
        vm.expectRevert(PropPool.UnknownPair.selector);
        pool.pause(2);
        vm.stopPrank();
    }

    function test_Pause_SurvivesCapacityRefreshByTheUpdater() public {
        vm.prank(guardian);
        pool.pause(PAIR);
        _refresh(BID_CAP, ASK_CAP);
        assertEq(pool.snapshot(PAIR).flags & 1, 1, "updater cleared the guardian's pause");
        vm.prank(taker);
        vm.expectRevert(PropPool.PoolPaused.selector);
        pool.swap(address(base), address(quote), 1e18, 0, taker, 0, block.timestamp);
    }

    function test_CapacityExhaustion_ExactThenOneMore() public {
        _swapExactIn(BID_CAP, true);
        assertEq(pool.snapshot(PAIR).bidUsed, BID_CAP, "bidUsed not stamped");

        vm.prank(taker);
        vm.expectRevert(PropPool.InsufficientCapacity.selector);
        pool.swap(address(base), address(quote), 1, 0, taker, 0, block.timestamp);

        assertGt(pool.getAmountOut(address(quote), address(base), 1000e6), 0, "ask side must be independent");
    }

    function test_CapacityExhaustion_OneUnitOverInOneShot() public {
        vm.prank(taker);
        vm.expectRevert(PropPool.InsufficientCapacity.selector);
        pool.swap(address(base), address(quote), int256(uint256(BID_CAP) + 1), 0, taker, 0, block.timestamp);
    }

    function test_Generation_RefreshCapacityRestoresUsableCapacity() public {
        _swapExactIn(BID_CAP, true);
        assertEq(pool.getAmountOut(address(base), address(quote), 1e18), 0, "epoch should be exhausted");

        uint32 genBefore = pool.snapshot(PAIR).capGen;
        _refresh(BID_CAP, ASK_CAP);
        IPropPool.PairSnapshot memory s = pool.snapshot(PAIR);

        assertEq(s.capGen, genBefore + 1, "capGen must advance");
        assertTrue(s.usedGen != s.capGen, "used counters must be orphaned by the refresh");
        assertEq(s.bidUsed, BID_CAP, "raw counter is deliberately left in place");
        assertGt(pool.getAmountOut(address(base), address(quote), 1e18), 0, "capacity not restored");

        assertEq(pool.getAmountOut(address(base), address(quote), 1e18), _bidOut(1e18, 0), "not repriced from zero");
    }

    function test_Generation_PriceUpdateDoesNotRestoreCapacity() public {
        _swapExactIn(BID_CAP, true);
        uint32 capGen = pool.snapshot(PAIR).capGen;

        for (uint56 i; i < 20; ++i) {
            _push(MIN_BID + i, MAX_BID + i, MIN_ASK + i, MAX_ASK + i);
            assertEq(pool.getAmountOut(address(base), address(quote), 1), 0, "a price push restored capacity");
        }

        IPropPool.PairSnapshot memory s = pool.snapshot(PAIR);
        assertEq(s.capGen, capGen, "capGen must not move on a price push");
        assertEq(s.usedGen, capGen, "usedGen must still match, keeping the counters live");
        assertEq(s.bidUsed, BID_CAP, "used counter must survive price churn");

        vm.prank(taker);
        vm.expectRevert(PropPool.InsufficientCapacity.selector);
        pool.swap(address(base), address(quote), 1, 0, taker, 0, block.timestamp);
    }

    function test_Generation_RefreshToSmallerCapacityStartsFromZeroUsage() public {
        _swapExactIn(BID_CAP, true);
        _refresh(1e18, ASK_CAP);
        assertEq(pool.getAmountOut(address(base), address(quote), 1e18), _bidOutCap(1e18, 1e18), "new epoch mispriced");
        assertEq(pool.getAmountOut(address(base), address(quote), 1e18 + 1), 0, "new capacity not enforced");
    }

    function test_RefreshCapacityBatch_PacksPairIdInTheSamePlace() public {
        MockERC20 t = new MockERC20("T", "T", 18);
        vm.prank(owner);
        uint16 id = pool.addPair(address(t), address(quote), EXP, MAX_STALE, 1);

        uint256[] memory w = new uint256[](2);
        w[0] = _packCapacity(PAIR, 7e18, 8e6);
        w[1] = _packCapacity(id, 9e18, 10e6);
        vm.prank(updater);
        pool.refreshCapacityBatch(w);

        assertEq(pool.snapshot(PAIR).bidCapacity, 7e18);
        assertEq(pool.snapshot(PAIR).askCapacity, 8e6);
        assertEq(pool.snapshot(id).bidCapacity, 9e18);
        assertEq(pool.snapshot(id).askCapacity, 10e6);
    }

    function test_RefreshCapacity_Guards() public {
        uint256[] memory w = new uint256[](1);
        w[0] = _packCapacity(0, 1, 1);
        vm.startPrank(updater);
        vm.expectRevert(PropPool.UnknownPair.selector);
        pool.refreshCapacity(0, 1, 1);
        vm.expectRevert(PropPool.UnknownPair.selector);
        pool.refreshCapacity(2, 1, 1);
        vm.expectRevert(PropPool.UnknownPair.selector);
        pool.refreshCapacityBatch(w);
        vm.stopPrank();

        vm.prank(manager);
        vm.expectRevert(PropPool.NotUpdater.selector);
        pool.refreshCapacity(PAIR, 1, 1);
    }

    function test_RefreshCapacity_ZeroIsAValidHalt() public {
        _refresh(0, 0);
        assertEq(pool.getAmountOut(address(base), address(quote), 1e18), 0, "zero capacity must quote 0");
        vm.prank(taker);
        vm.expectRevert(PropPool.InsufficientCapacity.selector);
        pool.swap(address(base), address(quote), 1e18, 0, taker, 0, block.timestamp);
    }

    function test_MinReserve_EnforcedOnTheOutputSide() public {

        uint256 out = pool.getAmountOut(address(base), address(quote), 1e18);
        vm.prank(manager);
        pool.setPairConfig(PAIR, MAX_STALE, MIN_BID, 0, uint96(QUOTE_INVENTORY - out + 1));

        assertEq(pool.getAmountOut(address(base), address(quote), 1e18), 0, "view must report unfillable");
        assertEq(pool.getAmountIn(address(base), address(quote), out), 0, "getAmountIn must report unfillable");

        vm.prank(taker);
        vm.expectRevert(PropPool.ReserveFloorBreached.selector);
        pool.swap(address(base), address(quote), 1e18, 0, taker, 0, block.timestamp);

        vm.prank(manager);
        pool.setPairConfig(PAIR, MAX_STALE, MIN_BID, 0, uint96(QUOTE_INVENTORY - out));
        assertEq(_swapExactIn(1e18, true), out, "exactly at the floor must fill");
        assertEq(pool.reserveOf(address(quote)), QUOTE_INVENTORY - out, "floor not respected exactly");
    }

    function test_MinReserve_AppliesToTheBaseSideForAsks() public {
        uint256 out = pool.getAmountOut(address(quote), address(base), 1000e6);
        vm.prank(manager);
        pool.setPairConfig(PAIR, MAX_STALE, MIN_BID, uint96(BASE_INVENTORY - out + 1), 0);
        assertEq(pool.getAmountOut(address(quote), address(base), 1000e6), 0, "ask side floor not applied");
        vm.prank(taker);
        vm.expectRevert(PropPool.ReserveFloorBreached.selector);
        pool.swap(address(quote), address(base), 1000e6, 0, taker, 0, block.timestamp);
    }

    function test_Deposit_Withdraw_Sync_RoleGating() public {
        vm.prank(owner);
        vm.expectRevert(PropPool.NotManager.selector);
        pool.deposit(address(quote), 1);
        vm.prank(manager);
        vm.expectRevert(PropPool.NotOwner.selector);
        pool.withdraw(address(quote), 1, manager);
        vm.prank(owner);
        vm.expectRevert(PropPool.NotManager.selector);
        pool.sync(address(quote));
        vm.prank(guardian);
        vm.expectRevert(PropPool.NotManager.selector);
        pool.deposit(address(quote), 1);
    }

    function test_Deposit_AccountsExactly() public {
        quote.mint(manager, 5e6);
        vm.startPrank(manager);
        pool.deposit(address(quote), 5e6);
        vm.stopPrank();
        assertEq(pool.reserveOf(address(quote)), QUOTE_INVENTORY + 5e6);
        assertEq(quote.balanceOf(address(pool)), QUOTE_INVENTORY + 5e6);
    }

    function test_Withdraw_CannotExceedAccountedReserve() public {

        quote.mint(address(pool), 500e6);
        assertEq(pool.reserveOf(address(quote)), QUOTE_INVENTORY, "donation must not credit the reserve");

        vm.prank(owner);
        vm.expectRevert(stdError.arithmeticError);
        pool.withdraw(address(quote), QUOTE_INVENTORY + 1, owner);

        vm.prank(owner);
        pool.withdraw(address(quote), QUOTE_INVENTORY, owner);
        assertEq(pool.reserveOf(address(quote)), 0);
        assertEq(quote.balanceOf(address(pool)), 500e6, "donation should still be sitting there");

        vm.prank(manager);
        pool.sync(address(quote));
        assertEq(pool.reserveOf(address(quote)), 500e6, "sync did not fold the donation in");
    }

    function test_Withdraw_RejectsZeroReceiver() public {
        vm.prank(owner);
        vm.expectRevert(PropPool.ZeroAddress.selector);
        pool.withdraw(address(quote), 1, address(0));
    }

    function test_Sync_LowersReserveOnADownwardRebase() public {
        vm.prank(address(pool));
        quote.burn(400_000e6);
        assertEq(pool.reserveOf(address(quote)), QUOTE_INVENTORY, "reserve is accounted, not measured");
        vm.prank(manager);
        pool.sync(address(quote));
        assertEq(pool.reserveOf(address(quote)), QUOTE_INVENTORY - 400_000e6);
    }

    function test_Swap_ExactIn_MovesTokensAndStampsCounters() public {
        uint256 expected = _bidOut(1e18, 0);
        assertEq(pool.getAmountOut(address(base), address(quote), 1e18), expected, "quote != execution");

        uint256 tBase = base.balanceOf(taker);
        uint256 tQuote = quote.balanceOf(taker);

        vm.expectEmit(true, true, true, true, address(pool));
        emit IPropPool.Swap(PAIR, taker, taker, true, 1e18, expected, 7);
        vm.prank(taker);
        uint256 result = pool.swap(address(base), address(quote), 1e18, 0, taker, 7, block.timestamp);

        assertEq(result, expected, "return value");
        assertEq(base.balanceOf(taker), tBase - 1e18, "taker base");
        assertEq(quote.balanceOf(taker), tQuote + expected, "taker quote");
        assertEq(pool.reserveOf(address(base)), BASE_INVENTORY + 1e18, "base reserve");
        assertEq(pool.reserveOf(address(quote)), QUOTE_INVENTORY - expected, "quote reserve");

        IPropPool.PairSnapshot memory s = pool.snapshot(PAIR);
        assertEq(s.bidUsed, 1e18, "bidUsed");
        assertEq(s.askUsed, 0, "askUsed must not move on a bid");
        assertEq(s.usedGen, s.capGen, "usedGen must be stamped to capGen");
    }

    function test_Swap_ExactIn_ChargesTheWorseAveragePriceAsUsageGrows() public {
        uint256 first = _swapExactIn(10e18, true);
        uint256 second = _swapExactIn(10e18, true);
        assertLt(second, first, "the second identical bid must be filled worse");
        assertEq(second, _bidOut(10e18, 10e18), "second leg mispriced against the used counter");
    }

    function test_Swap_ExactOut_DeliversExactlyAndRoundsInputUp() public {
        uint256 want = 1000e6;
        uint256 needed = pool.getAmountIn(address(base), address(quote), want);
        assertGt(needed, 0, "getAmountIn must find a size");

        uint256 tQuote = quote.balanceOf(taker);
        vm.prank(taker);
        uint256 paid =
            pool.swap(address(base), address(quote), -int256(want), type(uint256).max, taker, 0, block.timestamp);

        assertEq(paid, needed, "swap and getAmountIn must agree");
        assertEq(quote.balanceOf(taker) - tQuote, want, "taker must receive exactly the exact-out amount");

        assertGe(_bidOut(paid, 0), want, "input rounded in the taker's favour");

        assertLt(_bidOut(paid - 1, 0), want, "getAmountIn is not minimal");
    }

    function testFuzz_GetAmountIn_RoundTripsInThePoolsFavour(uint256 want, bool isBid) public view {
        address tokenIn = isBid ? address(base) : address(quote);
        address tokenOut = isBid ? address(quote) : address(base);

        want = bound(want, 1, isBid ? 200_000e6 : 30e18);
        uint256 needed = pool.getAmountIn(tokenIn, tokenOut, want);
        if (needed == 0) return;

        uint256 delivered = pool.getAmountOut(tokenIn, tokenOut, needed);
        assertGe(delivered, want, "required input under-delivers: rounding favours the taker");

        uint256 again = pool.getAmountIn(tokenIn, tokenOut, delivered);
        assertLe(again, needed, "round trip inflated the required input");
    }

    function test_BUG5_GetAmountIn_IsNotMinimal_RoundTripInflates() public view {
        uint256 amountIn = 2e18;
        uint256 out = pool.getAmountOut(address(base), address(quote), amountIn);
        assertGt(out, 0, "fixture must quote");
        uint256 back = pool.getAmountIn(address(base), address(quote), out);

        console2.log("getAmountOut(2e18)      =", out);
        console2.log("getAmountIn(that)      =", back);
        console2.log("overstated by (wei)    =", back > amountIn ? back - amountIn : 0);

        assertLe(back, amountIn, "BUG5 regressed: getAmountIn asked for more than the input that produced the output");
    }

    function testFuzz_GetAmountIn_AlwaysDeliversTheTarget(uint256 amountIn, bool isBid) public view {
        address tokenIn = isBid ? address(base) : address(quote);
        address tokenOut = isBid ? address(quote) : address(base);
        amountIn = bound(amountIn, 1, isBid ? uint256(BID_CAP) : ASK_QUOTE_CEILING);

        uint256 out = pool.getAmountOut(tokenIn, tokenOut, amountIn);
        if (out == 0) return;
        uint256 back = pool.getAmountIn(tokenIn, tokenOut, out);
        assertGt(back, 0, "a size the pool just quoted must be reachable");
        assertGe(pool.getAmountOut(tokenIn, tokenOut, back), out, "the required input under-delivers");
    }

    function test_Swap_SlippageLimits() public {
        uint256 expected = _bidOut(1e18, 0);

        vm.prank(taker);
        vm.expectRevert(PropPool.SlippageExceeded.selector);
        pool.swap(address(base), address(quote), 1e18, expected + 1, taker, 0, block.timestamp);

        vm.prank(taker);
        assertEq(pool.swap(address(base), address(quote), 1e18, expected, taker, 0, block.timestamp), expected);

        uint256 needed = pool.getAmountIn(address(base), address(quote), 1000e6);
        vm.prank(taker);
        vm.expectRevert(PropPool.SlippageExceeded.selector);
        pool.swap(address(base), address(quote), -int256(1000e6), needed - 1, taker, 0, block.timestamp);

        vm.prank(taker);
        assertEq(pool.swap(address(base), address(quote), -int256(1000e6), needed, taker, 0, block.timestamp), needed);
    }

    function test_Swap_Deadline() public {
        vm.prank(taker);
        vm.expectRevert(PropPool.DeadlineExpired.selector);
        pool.swap(address(base), address(quote), 1e18, 0, taker, 0, block.timestamp - 1);

        vm.prank(taker);
        assertGt(pool.swap(address(base), address(quote), 1e18, 0, taker, 0, block.timestamp), 0);

        vm.prank(taker);
        vm.expectRevert(PropPool.DeadlineExpired.selector);
        pool.swapWithContractBalance(address(base), address(quote), 0, taker, 0, block.timestamp - 1);
    }

    function test_Swap_ArgumentGuards() public {
        vm.startPrank(taker);
        vm.expectRevert(PropPool.ZeroAddress.selector);
        pool.swap(address(base), address(quote), 1e18, 0, address(0), 0, block.timestamp);
        vm.expectRevert(PropPool.ZeroAmount.selector);
        pool.swap(address(base), address(quote), 0, 0, taker, 0, block.timestamp);

        vm.expectRevert(PropPool.AmountOverflow.selector);
        pool.swap(address(base), address(quote), type(int256).min, 0, taker, 0, block.timestamp);
        vm.expectRevert(PropPool.UnknownPair.selector);
        pool.swap(address(base), address(0xDEAD), 1e18, 0, taker, 0, block.timestamp);

        vm.expectRevert(PropPool.UnknownPair.selector);
        pool.swap(address(base), address(base), 1e18, 0, taker, 0, block.timestamp);
        vm.expectRevert(PropPool.ZeroAddress.selector);
        pool.swapWithContractBalance(address(base), address(quote), 0, address(0), 0, block.timestamp);
        vm.stopPrank();
    }

    function test_Swap_ZeroOutputReverts() public {

        assertEq(pool.getAmountOut(address(base), address(quote), 1), 0, "sanity: dust floors to zero");
        vm.prank(taker);
        vm.expectRevert(PropPool.ZeroOutput.selector);
        pool.swap(address(base), address(quote), 1, 0, taker, 0, block.timestamp);
    }

    function test_SwapWithContractBalance_MeasuresTheBalanceDelta() public {
        uint256 expected = _bidOut(1e18, 0);

        vm.prank(taker);
        base.transfer(address(pool), 1e18);

        vm.prank(taker);
        uint256 out = pool.swapWithContractBalance(address(base), address(quote), 0, taker, 0, block.timestamp);
        assertEq(out, expected, "push path priced differently from the pull path");
        assertEq(pool.reserveOf(address(base)), BASE_INVENTORY + 1e18, "push not accounted");
        assertEq(pool.snapshot(PAIR).bidUsed, 1e18, "used counter not stamped on the push path");
    }

    function test_SwapWithContractBalance_RequiresAPush() public {
        vm.prank(taker);
        vm.expectRevert(PropPool.ZeroAmount.selector);
        pool.swapWithContractBalance(address(base), address(quote), 0, taker, 0, block.timestamp);
    }

    function test_SwapWithContractBalance_CreditsAnyUnaccountedBalance() public {
        base.mint(address(pool), 2e18);
        vm.prank(taker);
        uint256 out = pool.swapWithContractBalance(address(base), address(quote), 0, taker, 0, block.timestamp);
        assertEq(out, _bidOut(2e18, 0), "the caller was not credited the whole unaccounted balance");
    }

    function test_Reentrancy_HostileOutputTokenCannotReenterSwap() public {
        (uint16 id, ReenteringToken re) = _addReenteringPair();
        re.arm(pool, address(base), address(re), 1);

        vm.prank(taker);
        vm.expectRevert(ReentrancyLock.Reentrancy.selector);
        pool.swap(address(base), address(re), 1e18, 0, taker, 0, block.timestamp);

        assertEq(pool.snapshot(id).bidUsed, 0, "a reverted swap must leave no usage");
        assertEq(pool.reserveOf(address(base)), BASE_INVENTORY, "a reverted swap must leave no reserve change");
    }

    function test_Reentrancy_HostileOutputTokenCannotReenterPushPath() public {
        (, ReenteringToken re) = _addReenteringPair();
        re.arm(pool, address(base), address(re), 2);

        vm.prank(taker);
        base.transfer(address(pool), 1e18);

        vm.prank(taker);
        vm.expectRevert(ReentrancyLock.Reentrancy.selector);
        pool.swapWithContractBalance(address(base), address(re), 0, taker, 0, block.timestamp);
    }

    function test_Reentrancy_LockAlsoCoversDepositAndWithdraw() public {
        (, ReenteringToken re) = _addReenteringPair();
        re.arm(pool, address(base), address(re), 1);
        re.mint(manager, 1e6);
        vm.prank(manager);
        re.approve(address(pool), type(uint256).max);
        vm.prank(manager);
        vm.expectRevert(ReentrancyLock.Reentrancy.selector);
        pool.deposit(address(re), 1e6);
    }

    function test_ReadOnlyReentrancy_SeesPreTradeUsedCounters() public {
        (, ReenteringToken re) = _addReenteringPair();
        uint256 before = pool.getAmountOut(address(base), address(re), 1e18);
        re.arm(pool, address(base), address(re), 3);

        vm.prank(taker);
        pool.swap(address(base), address(re), 1e18, 0, taker, 0, block.timestamp);

        assertEq(re.observedQuote(), before, "the view inside the transfer should see pre-trade state");
        assertLt(pool.getAmountOut(address(base), address(re), 1e18), before, "post-trade quote should be worse");
    }

    function test_BUG6_SafeTransferCannotBubbleTheTokensRevert() public {
        RevertingToken bad = new RevertingToken();
        vm.prank(owner);
        uint16 id = pool.addPair(address(base), address(bad), EXP, MAX_STALE, MIN_PRICE);

        uint256[] memory w = new uint256[](1);
        w[0] = _packQuote(id, MIN_BID, MAX_BID, MIN_ASK, MAX_ASK);
        vm.prank(updater);
        pool.updateQuote(w);
        _refreshFor(id, BID_CAP, ASK_CAP);

        bad.mint(manager, QUOTE_INVENTORY);
        vm.startPrank(manager);
        bad.approve(address(pool), type(uint256).max);
        pool.deposit(address(bad), QUOTE_INVENTORY);
        vm.stopPrank();

        bad.setRevertOnTransfer(true);

        vm.prank(taker);
        vm.expectRevert(RevertingToken.Boom.selector);
        pool.swap(address(base), address(bad), 1e18, 0, taker, 0, block.timestamp);
    }

    function test_SafeTransferFrom_DoesBubbleTheTokensRevert() public {
        RevertingToken bad = new RevertingToken();
        vm.prank(owner);
        uint16 id = pool.addPair(address(bad), address(quote), EXP, MAX_STALE, MIN_PRICE);

        uint256[] memory w = new uint256[](1);
        w[0] = _packQuote(id, MIN_BID, MAX_BID, MIN_ASK, MAX_ASK);
        vm.prank(updater);
        pool.updateQuote(w);
        _refreshFor(id, BID_CAP, ASK_CAP);

        bad.mint(taker, 10e18);
        vm.prank(taker);
        bad.approve(address(pool), type(uint256).max);
        bad.setRevertOnTransfer(true);

        vm.prank(taker);
        vm.expectRevert(RevertingToken.Boom.selector);
        pool.swap(address(bad), address(quote), 1e18, 0, taker, 0, block.timestamp);
    }

    function test_Gas_UpdateQuote_SinglePair() public {
        uint256[] memory w = new uint256[](1);
        w[0] = _packQuote(PAIR, MIN_BID + 1, MAX_BID, MIN_ASK, MAX_ASK);

        vm.prank(updater);
        uint256 g0 = gasleft();
        pool.updateQuote(w);
        uint256 cold = g0 - gasleft();

        w[0] = _packQuote(PAIR, MIN_BID + 2, MAX_BID, MIN_ASK, MAX_ASK);
        vm.prank(updater);
        g0 = gasleft();
        pool.updateQuote(w);
        uint256 dirty = g0 - gasleft();

        console2.log("updateQuote 1 pair, first touch in tx (execution only):", cold);
        console2.log("updateQuote 1 pair, slot already dirty:                ", dirty);
        console2.log("  + 21000 intrinsic + calldata =>                      ", cold + 21_000);
        assertLt(cold, 35_000, "updateQuote over the 35k target");
    }

    function test_Gas_UpdateQuote_FivePairBatch() public {
        for (uint16 i; i < 4; ++i) {
            MockERC20 t = new MockERC20("T", "T", 18);
            vm.prank(owner);
            pool.addPair(address(t), address(quote), EXP, MAX_STALE, 1);
        }
        uint256[] memory seed = new uint256[](5);
        for (uint16 i; i < 5; ++i) {
            seed[i] = _packQuote(i + 1, MIN_BID, MAX_BID, MIN_ASK, MAX_ASK);
        }

        vm.prank(updater);
        pool.updateQuote(seed);

        uint256[] memory w = new uint256[](5);
        for (uint16 i; i < 5; ++i) {
            w[i] = _packQuote(i + 1, MIN_BID + 1, MAX_BID, MIN_ASK, MAX_ASK);
        }
        vm.prank(updater);
        uint256 g0 = gasleft();
        pool.updateQuote(w);
        uint256 used = g0 - gasleft();
        console2.log("updateQuote 5 pairs, dirty words (execution only):", used);
        console2.log("  per pair:", used / 5);
    }

    function test_Gas_Swap_ExactIn() public {
        vm.prank(taker);
        uint256 g0 = gasleft();
        pool.swap(address(base), address(quote), 1e18, 0, taker, 0, block.timestamp);
        uint256 cold = g0 - gasleft();

        vm.prank(taker);
        g0 = gasleft();
        pool.swap(address(base), address(quote), 1e18, 0, taker, 0, block.timestamp);
        uint256 dirty = g0 - gasleft();

        console2.log("swap exact-in, first touch in tx (execution only):", cold);
        console2.log("swap exact-in, everything dirty:                 ", dirty);
        console2.log("  + 21000 intrinsic + calldata =>                ", cold + 21_000);
        assertLt(cold, 110_000, "swap over the 110k target");
    }

    function test_Gas_Swap_ExactIn_AskSide_MissesTheTarget() public {
        vm.prank(taker);
        uint256 g0 = gasleft();
        pool.swap(address(quote), address(base), int256(1000e6), 0, taker, 0, block.timestamp);
        uint256 cold = g0 - gasleft();

        console2.log("swap exact-in ASK, first touch in tx (execution only):", cold);
        console2.log("  target 110,000 => MISSED by:                        ", cold - 110_000);
        assertGt(cold, 110_000, "ask exact-in now MEETS the 110k target: update this test and archi_v2 4.2");
        assertLt(cold, 156_000, "ask exact-in regressed past its measured 149,318");
    }

    function test_Gas_Swap_ExactOut() public {
        vm.prank(taker);
        uint256 g0 = gasleft();
        pool.swap(address(base), address(quote), -int256(1000e6), type(uint256).max, taker, 0, block.timestamp);
        uint256 cold = g0 - gasleft();
        console2.log("swap exact-out, first touch in tx:", cold);

        assertLt(cold, 155_000, "exact-out swap regressed past its measured 143,046");
    }

    function test_Gas_RefreshCapacity() public {
        vm.prank(updater);
        uint256 g0 = gasleft();
        pool.refreshCapacity(PAIR, BID_CAP, ASK_CAP);
        uint256 cold = g0 - gasleft();
        console2.log("refreshCapacity, first touch in tx (execution only):", cold);
    }

    function test_Gas_GetAmountOut_View() public view {
        uint256 g0 = gasleft();
        pool.getAmountOut(address(base), address(quote), 1e18);
        console2.log("getAmountOut, first touch in tx:", g0 - gasleft());
    }

    function _refreshFor(uint16 pairId, uint96 bidCap, uint96 askCap) internal {
        vm.prank(updater);
        pool.refreshCapacity(pairId, bidCap, askCap);
    }

    function _bidOutCap(uint256 amountIn, uint96 cap) internal pure returns (uint256) {
        return _bidOutAt(amountIn, 0, cap);
    }

    uint96 internal constant MAX_CAP_AT_EXP0 = 4_722_366_482_869_645_279_232;

    function _addWidePair() internal returns (uint16 id, MockERC20 b2, MockERC20 q2) {
        b2 = new MockERC20("W", "W", 18);
        q2 = new MockERC20("X", "X", 18);
        vm.prank(owner);
        id = pool.addPair(address(b2), address(q2), 0, MAX_STALE, 1);

        uint256[] memory w = new uint256[](1);
        uint256 top = type(uint56).max;

        w[0] = _packQuote(id, uint56(top - 1), uint56(top - 1), uint56(top - 1), uint56(top));
        vm.prank(updater);
        pool.updateQuote(w);
        _refreshFor(id, MAX_CAP_AT_EXP0, MAX_CAP_AT_EXP0);

        b2.mint(manager, MAX_CAP_AT_EXP0);
        q2.mint(manager, type(uint128).max);
        vm.startPrank(manager);
        b2.approve(address(pool), type(uint256).max);
        q2.approve(address(pool), type(uint256).max);
        pool.deposit(address(b2), MAX_CAP_AT_EXP0);
        pool.deposit(address(q2), type(uint128).max);
        vm.stopPrank();
    }

    function _addReenteringPair() internal returns (uint16 id, ReenteringToken re) {
        re = new ReenteringToken(6);
        vm.prank(owner);
        id = pool.addPair(address(base), address(re), EXP, MAX_STALE, MIN_PRICE);

        uint256[] memory w = new uint256[](1);
        w[0] = _packQuote(id, MIN_BID, MAX_BID, MIN_ASK, MAX_ASK);
        vm.prank(updater);
        pool.updateQuote(w);
        _refreshFor(id, BID_CAP, ASK_CAP);

        re.mint(manager, QUOTE_INVENTORY);
        vm.startPrank(manager);
        re.approve(address(pool), type(uint256).max);
        pool.deposit(address(re), QUOTE_INVENTORY);
        vm.stopPrank();
    }
}
