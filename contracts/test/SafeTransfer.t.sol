// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {Test} from "forge-std/Test.sol";
import {SafeTransfer} from "../src/libraries/SafeTransfer.sol";

interface ITestToken {
    function mint(address to, uint256 amount) external;
    function approve(address spender, uint256 amount) external returns (bool);
    function balanceOf(address who) external view returns (uint256);
}

contract Harness {

    uint256 private constant PROBE_LEN = 96;

    struct MemProbe {
        uint256 fmpBefore;
        uint256 fmpAfter;
        uint256 zeroSlotAfter;
        uint256 preStart;
        uint256 postStart;
        uint256 probeLen;
        bool canariesIntact;
    }

    struct FailProbe {
        bytes reason;
        uint256 gasUsed;
        uint256 gasBudget;
        uint256 preStart;
        uint256 postStart;
        uint256 probeLen;
        bool canariesIntact;
    }

    function doTransfer(address token, address to, uint256 amount) external {
        SafeTransfer.safeTransfer(token, to, amount);
    }

    function doTransferFrom(address token, address from, address to, uint256 amount) external {
        SafeTransfer.safeTransferFrom(token, from, to, amount);
    }

    function approveOn(address token, address spender, uint256 amount) external {
        ITestToken(token).approve(spender, amount);
    }

    function doTransferDirtyTo(address token, uint256 dirtyTo, uint256 amount) external {
        address to;
        assembly {
            to := dirtyTo
        }
        SafeTransfer.safeTransfer(token, to, amount);
    }

    function doTransferFromDirty(address token, uint256 dirtyFrom, uint256 dirtyTo, uint256 amount) external {
        address from;
        address to;
        assembly {
            from := dirtyFrom
            to := dirtyTo
        }
        SafeTransfer.safeTransferFrom(token, from, to, amount);
    }

    function probeTransferMemory(address token, address to, uint256 amount) external returns (MemProbe memory p) {
        bytes memory pre = _fill(0xAB);
        uint256 preStart;
        uint256 fmpBefore;
        assembly {
            preStart := pre
            fmpBefore := mload(0x40)
        }

        SafeTransfer.safeTransfer(token, to, amount);

        uint256 fmpAfter;
        uint256 zeroSlotAfter;
        assembly {
            fmpAfter := mload(0x40)
            zeroSlotAfter := mload(0x60)
        }

        bytes memory post = _fill(0xCD);
        uint256 postStart;
        assembly {
            postStart := post
        }

        p.fmpBefore = fmpBefore;
        p.fmpAfter = fmpAfter;
        p.zeroSlotAfter = zeroSlotAfter;
        p.preStart = preStart;
        p.postStart = postStart;
        p.probeLen = PROBE_LEN;
        p.canariesIntact = _check(pre, 0xAB) && _check(post, 0xCD);
    }

    function probeTransferFromMemory(address token, address from, address to, uint256 amount)
        external
        returns (MemProbe memory p)
    {
        bytes memory pre = _fill(0xAB);
        uint256 preStart;
        uint256 fmpBefore;
        assembly {
            preStart := pre
            fmpBefore := mload(0x40)
        }

        SafeTransfer.safeTransferFrom(token, from, to, amount);

        uint256 fmpAfter;
        uint256 zeroSlotAfter;
        assembly {
            fmpAfter := mload(0x40)
            zeroSlotAfter := mload(0x60)
        }

        bytes memory post = _fill(0xCD);
        uint256 postStart;
        assembly {
            postStart := post
        }

        p.fmpBefore = fmpBefore;
        p.fmpAfter = fmpAfter;
        p.zeroSlotAfter = zeroSlotAfter;
        p.preStart = preStart;
        p.postStart = postStart;
        p.probeLen = PROBE_LEN;
        p.canariesIntact = _check(pre, 0xAB) && _check(post, 0xCD);
    }

    function probeFailingTransfer(address token, address to, uint256 amount, uint256 budget)
        external
        returns (FailProbe memory p)
    {
        bytes memory pre = _fill(0xAB);
        uint256 preStart;
        assembly {
            preStart := pre
        }

        uint256 g0 = gasleft();
        (bool ok, bytes memory ret) =
            address(this).call{gas: budget}(abi.encodeCall(this.doTransfer, (token, to, amount)));
        p.gasUsed = g0 - gasleft();
        require(!ok, "probe: expected the transfer to fail");

        bytes memory post = _fill(0xCD);
        uint256 postStart;
        assembly {
            postStart := post
        }

        p.reason = ret;
        p.gasBudget = budget;
        p.preStart = preStart;
        p.postStart = postStart;
        p.probeLen = PROBE_LEN;
        p.canariesIntact = _check(pre, 0xAB) && _check(post, 0xCD);
    }

    function probeFailingTransferFrom(address token, address from, address to, uint256 amount, uint256 budget)
        external
        returns (FailProbe memory p)
    {
        bytes memory pre = _fill(0xAB);
        uint256 preStart;
        assembly {
            preStart := pre
        }

        uint256 g0 = gasleft();
        (bool ok, bytes memory ret) =
            address(this).call{gas: budget}(abi.encodeCall(this.doTransferFrom, (token, from, to, amount)));
        p.gasUsed = g0 - gasleft();
        require(!ok, "probe: expected the transferFrom to fail");

        bytes memory post = _fill(0xCD);
        uint256 postStart;
        assembly {
            postStart := post
        }

        p.reason = ret;
        p.gasBudget = budget;
        p.preStart = preStart;
        p.postStart = postStart;
        p.probeLen = PROBE_LEN;
        p.canariesIntact = _check(pre, 0xAB) && _check(post, 0xCD);
    }

    function _fill(bytes1 v) private pure returns (bytes memory b) {
        b = new bytes(PROBE_LEN);
        for (uint256 i; i < PROBE_LEN; ++i) {
            b[i] = v;
        }
    }

    function _check(bytes memory b, bytes1 v) private pure returns (bool) {
        for (uint256 i; i < b.length; ++i) {
            if (b[i] != v) return false;
        }
        return true;
    }
}

abstract contract Ledger {
    mapping(address => uint256) public balanceOf;
    mapping(address => mapping(address => uint256)) public allowance;

    function mint(address to, uint256 amount) external {
        balanceOf[to] += amount;
    }

    function approve(address spender, uint256 amount) external returns (bool) {
        allowance[msg.sender][spender] = amount;
        return true;
    }

    function _move(address from, address to, uint256 amount) internal {
        uint256 b = balanceOf[from];
        require(b >= amount, "LEDGER: balance");
        balanceOf[from] = b - amount;
        balanceOf[to] += amount;
    }

    function _spend(address owner, address spender, uint256 amount) internal {
        uint256 a = allowance[owner][spender];
        require(a >= amount, "LEDGER: allowance");
        if (a != type(uint256).max) allowance[owner][spender] = a - amount;
    }
}

contract TrueToken is Ledger {
    function transfer(address to, uint256 amount) external returns (bool) {
        _move(msg.sender, to, amount);
        return true;
    }

    function transferFrom(address from, address to, uint256 amount) external returns (bool) {
        _spend(from, msg.sender, amount);
        _move(from, to, amount);
        return true;
    }
}

contract SilentToken is Ledger {
    function transfer(address to, uint256 amount) external {
        _move(msg.sender, to, amount);
    }

    function transferFrom(address from, address to, uint256 amount) external {
        _spend(from, msg.sender, amount);
        _move(from, to, amount);
    }
}

contract FalseToken is Ledger {
    function transfer(address, uint256) external pure returns (bool) {
        return false;
    }

    function transferFrom(address, address, uint256) external pure returns (bool) {
        return false;
    }
}

contract GarbageWordToken {
    fallback() external {
        assembly {
            mstore(0x00, 2)
            return(0x00, 0x20)
        }
    }
}

contract ShortReturnToken {
    uint256 public immutable len;

    constructor(uint256 l) {
        len = l;
    }

    fallback() external {
        uint256 n = len;
        assembly {
            mstore(0x00, 1)
            return(0x00, n)
        }
    }
}

contract OverlongToken {
    uint256 public immutable firstWord;

    constructor(uint256 w) {
        firstWord = w;
    }

    fallback() external {
        uint256 w = firstWord;
        assembly {
            mstore(0x00, w)
            mstore(0x20, not(0))
            mstore(0x40, not(0))
            return(0x00, 0x60)
        }
    }
}

contract CustomErrorToken {
    error Boom(address caller, uint256 amount, bytes32 tag);

    function transfer(address, uint256 amount) external view returns (bool) {
        revert Boom(msg.sender, amount, keccak256("boom"));
    }

    function transferFrom(address, address, uint256 amount) external view returns (bool) {
        revert Boom(msg.sender, amount, keccak256("boom"));
    }
}

contract LongStringToken {
    string private reason;

    constructor(string memory r) {
        reason = r;
    }

    fallback() external {
        string memory r = reason;
        revert(r);
    }
}

contract NoDataToken {
    fallback() external {
        assembly {
            revert(0x00, 0x00)
        }
    }
}

contract BombToken {
    uint256 public immutable size;

    constructor(uint256 s) {
        size = s;
    }

    fallback() external {
        uint256 n = size;
        assembly {
            let p := 0x80
            for {let i := 0} lt(i, n) {i := add(i, 0x20)} {mstore(add(p, i), add(div(i, 0x20), 1))}
            revert(p, n)
        }
    }
}

contract GasBurnerToken {
    fallback() external {
        assembly {
            invalid()
        }
    }
}

contract GasReporterToken {
    uint256 public seen;

    fallback() external {
        seen = gasleft();
        assembly {
            mstore(0x00, 1)
            return(0x00, 0x20)
        }
    }
}

contract AlwaysOkToken {
    fallback() external {
        assembly {
            mstore(0x00, 1)
            return(0x00, 0x20)
        }
    }
}

contract FeeToken is Ledger {
    uint256 public constant FEE_BPS = 100;

    function transfer(address to, uint256 amount) external returns (bool) {
        uint256 fee = (amount * FEE_BPS) / 10_000;
        _move(msg.sender, to, amount - fee);
        _move(msg.sender, address(this), fee);
        return true;
    }

    function transferFrom(address from, address to, uint256 amount) external returns (bool) {
        _spend(from, msg.sender, amount);
        uint256 fee = (amount * FEE_BPS) / 10_000;
        _move(from, to, amount - fee);
        _move(from, address(this), fee);
        return true;
    }
}

contract ReenteringToken is Ledger {
    Harness private immutable harness;
    address private inner;
    address private innerTo;
    uint256 private innerAmount;

    constructor(Harness h) {
        harness = h;
    }

    function setInner(address token, address to, uint256 amount) external {
        inner = token;
        innerTo = to;
        innerAmount = amount;
    }

    function transfer(address to, uint256 amount) external returns (bool) {
        if (inner != address(0)) harness.doTransfer(inner, innerTo, innerAmount);
        _move(msg.sender, to, amount);
        return true;
    }
}

contract SafeTransferTest is Test {
    Harness internal h;

    address internal alice = address(0xA11CE);
    address internal bob = address(0xB0B);

    address internal codeless = address(0xDeAD0000000000000000000000000000000beef1);

    string internal longReason;

    uint256 internal constant AMOUNT = 7.5e18;

    function setUp() public {
        h = new Harness();

        bytes memory buf = new bytes(600);
        for (uint256 i; i < 600; ++i) {
            buf[i] = bytes1(uint8(0x41 + (i % 26)));
        }
        longReason = string(buf);
    }

    function _assertTransferRevertsVerbatim(address token, bytes memory expected) internal {
        (bool ok, bytes memory ret) = address(h).call(abi.encodeCall(Harness.doTransfer, (token, bob, AMOUNT)));
        assertFalse(ok, "safeTransfer should have failed");
        assertEq(ret, expected, "revert payload was not bubbled verbatim");
    }

    function _assertTransferFromRevertsVerbatim(address token, bytes memory expected) internal {
        (bool ok, bytes memory ret) =
            address(h).call(abi.encodeCall(Harness.doTransferFrom, (token, alice, bob, AMOUNT)));
        assertFalse(ok, "safeTransferFrom should have failed");
        assertEq(ret, expected, "revert payload was not bubbled verbatim");
    }

    function _bombPayload(uint256 size) internal pure returns (bytes memory out) {
        out = new bytes(0);
        for (uint256 i; i < size / 32; ++i) {
            out = bytes.concat(out, bytes32(i + 1));
        }
    }

    function test_SafeTransfer_BubblesCustomErrorVerbatim() public {
        CustomErrorToken t = new CustomErrorToken();
        _assertTransferRevertsVerbatim(
            address(t),
            abi.encodeWithSelector(CustomErrorToken.Boom.selector, address(h), AMOUNT, keccak256("boom"))
        );
    }

    function test_SafeTransferFrom_BubblesCustomErrorVerbatim() public {
        CustomErrorToken t = new CustomErrorToken();
        _assertTransferFromRevertsVerbatim(
            address(t),
            abi.encodeWithSelector(CustomErrorToken.Boom.selector, address(h), AMOUNT, keccak256("boom"))
        );
    }

    function test_SafeTransfer_BubblesLongStringVerbatim() public {
        LongStringToken t = new LongStringToken(longReason);
        _assertTransferRevertsVerbatim(address(t), abi.encodeWithSignature("Error(string)", longReason));
    }

    function test_SafeTransferFrom_BubblesLongStringVerbatim() public {
        LongStringToken t = new LongStringToken(longReason);
        _assertTransferFromRevertsVerbatim(address(t), abi.encodeWithSignature("Error(string)", longReason));
    }

    function test_SafeTransfer_BubblesOversizedPayloadVerbatim() public {
        uint256 size = 8192;
        BombToken t = new BombToken(size);
        _assertTransferRevertsVerbatim(address(t), _bombPayload(size));
    }

    function test_SafeTransferFrom_BubblesOversizedPayloadVerbatim() public {
        uint256 size = 8192;
        BombToken t = new BombToken(size);
        _assertTransferFromRevertsVerbatim(address(t), _bombPayload(size));
    }

    function test_SafeTransfer_BubblesDataLessRevertAsEmpty() public {
        NoDataToken t = new NoDataToken();
        _assertTransferRevertsVerbatim(address(t), bytes(""));
    }

    function test_SafeTransferFrom_BubblesDataLessRevertAsEmpty() public {
        NoDataToken t = new NoDataToken();
        _assertTransferFromRevertsVerbatim(address(t), bytes(""));
    }

    function testFuzz_SafeTransfer_BubblesVerbatimForAnyAmount(uint256 amount) public {
        CustomErrorToken t = new CustomErrorToken();
        (bool ok, bytes memory ret) = address(h).call(abi.encodeCall(Harness.doTransfer, (address(t), bob, amount)));
        assertFalse(ok, "safeTransfer should have failed");
        assertEq(
            ret,
            abi.encodeWithSelector(CustomErrorToken.Boom.selector, address(h), amount, keccak256("boom")),
            "revert payload was not bubbled verbatim"
        );
    }

    function test_SafeTransfer_BubblesVerbatimAtZeroAndAtOne() public {
        CustomErrorToken t = new CustomErrorToken();
        for (uint256 i; i < 2; ++i) {
            (bool ok, bytes memory ret) = address(h).call(abi.encodeCall(Harness.doTransfer, (address(t), bob, i)));
            assertFalse(ok, "safeTransfer should have failed");
            assertEq(
                ret,
                abi.encodeWithSelector(CustomErrorToken.Boom.selector, address(h), i, keccak256("boom")),
                "revert payload was not bubbled verbatim"
            );
        }
    }

    function test_SafeTransfer_AcceptsSilentToken() public {
        SilentToken t = new SilentToken();
        t.mint(address(h), AMOUNT);
        h.doTransfer(address(t), bob, AMOUNT);
        assertEq(t.balanceOf(bob), AMOUNT, "silent token should still have moved the balance");
    }

    function test_SafeTransfer_AcceptsExplicitTrue() public {
        TrueToken t = new TrueToken();
        t.mint(address(h), AMOUNT);
        h.doTransfer(address(t), bob, AMOUNT);
        assertEq(t.balanceOf(bob), AMOUNT);
    }

    function test_SafeTransfer_RejectsFalse() public {
        FalseToken t = new FalseToken();
        t.mint(address(h), AMOUNT);
        vm.expectRevert(SafeTransfer.TransferFailed.selector);
        h.doTransfer(address(t), bob, AMOUNT);
        assertEq(t.balanceOf(bob), 0, "nothing should have moved");
    }

    function test_SafeTransferFrom_RejectsFalse() public {
        FalseToken t = new FalseToken();
        t.mint(alice, AMOUNT);
        vm.prank(alice);
        t.approve(address(h), type(uint256).max);
        vm.expectRevert(SafeTransfer.TransferFromFailed.selector);
        h.doTransferFrom(address(t), alice, bob, AMOUNT);
    }

    function test_SafeTransfer_RejectsGarbageWord() public {
        GarbageWordToken t = new GarbageWordToken();
        vm.expectRevert(SafeTransfer.TransferFailed.selector);
        h.doTransfer(address(t), bob, AMOUNT);
    }

    function test_SafeTransferFrom_RejectsGarbageWord() public {
        GarbageWordToken t = new GarbageWordToken();
        vm.expectRevert(SafeTransfer.TransferFromFailed.selector);
        h.doTransferFrom(address(t), alice, bob, AMOUNT);
    }

    function test_SafeTransfer_RejectsShortReturn() public {
        uint256[3] memory lengths = [uint256(1), 4, 31];
        for (uint256 i; i < lengths.length; ++i) {
            ShortReturnToken t = new ShortReturnToken(lengths[i]);
            vm.expectRevert(SafeTransfer.TransferFailed.selector);
            h.doTransfer(address(t), bob, AMOUNT);
        }
    }

    function test_SafeTransferFrom_RejectsShortReturn() public {
        uint256[3] memory lengths = [uint256(1), 4, 31];
        for (uint256 i; i < lengths.length; ++i) {
            ShortReturnToken t = new ShortReturnToken(lengths[i]);
            vm.expectRevert(SafeTransfer.TransferFromFailed.selector);
            h.doTransferFrom(address(t), alice, bob, AMOUNT);
        }
    }

    function test_SafeTransfer_AcceptsOverlongReturnWhoseFirstWordIsTrue() public {
        OverlongToken t = new OverlongToken(1);
        h.doTransfer(address(t), bob, AMOUNT);
    }

    function test_SafeTransfer_RejectsOverlongReturnWhoseFirstWordIsNotTrue() public {
        OverlongToken t = new OverlongToken(2);
        vm.expectRevert(SafeTransfer.TransferFailed.selector);
        h.doTransfer(address(t), bob, AMOUNT);

        OverlongToken f = new OverlongToken(0);
        vm.expectRevert(SafeTransfer.TransferFailed.selector);
        h.doTransfer(address(f), bob, AMOUNT);
    }

    function test_SafeTransferFrom_OverlongReturnParity() public {
        OverlongToken good = new OverlongToken(1);
        h.doTransferFrom(address(good), alice, bob, AMOUNT);

        OverlongToken bad = new OverlongToken(2);
        vm.expectRevert(SafeTransfer.TransferFromFailed.selector);
        h.doTransferFrom(address(bad), alice, bob, AMOUNT);
    }

    function test_SafeTransfer_RejectsCodelessAddress() public {
        assertEq(codeless.code.length, 0, "fixture must really have no code");
        vm.expectRevert(SafeTransfer.TransferFailed.selector);
        h.doTransfer(codeless, bob, AMOUNT);
    }

    function test_SafeTransferFrom_RejectsCodelessAddress() public {
        vm.expectRevert(SafeTransfer.TransferFromFailed.selector);
        h.doTransferFrom(codeless, alice, bob, AMOUNT);
    }

    function test_SafeTransfer_RejectsPrecompile() public {
        vm.expectRevert(SafeTransfer.TransferFailed.selector);
        h.doTransfer(address(0x04), bob, AMOUNT);
    }

    function test_SafeTransfer_RejectsFundedEoa() public {
        vm.deal(alice, 1 ether);
        vm.expectRevert(SafeTransfer.TransferFailed.selector);
        h.doTransfer(alice, bob, AMOUNT);
    }

    function test_SafeTransfer_RestoresFreeMemoryPointerExactly() public {
        AlwaysOkToken t = new AlwaysOkToken();
        Harness.MemProbe memory p = h.probeTransferMemory(address(t), bob, AMOUNT);

        assertEq(p.fmpAfter, p.fmpBefore, "free-memory pointer was not restored exactly");
        assertEq(p.zeroSlotAfter, 0, "the zero slot at 0x60 must be left zero");
        assertGe(p.postStart, p.preStart + 0x20 + p.probeLen, "post-call allocation collided with pre-call memory");
        assertTrue(p.canariesIntact, "canary bytes were overwritten");
    }

    function test_SafeTransferFrom_RestoresFreeMemoryPointerAndZeroSlot() public {
        AlwaysOkToken t = new AlwaysOkToken();
        Harness.MemProbe memory p = h.probeTransferFromMemory(address(t), alice, bob, AMOUNT);

        assertEq(p.fmpAfter, p.fmpBefore, "free-memory pointer was not restored exactly");
        assertEq(p.zeroSlotAfter, 0, "the zero slot at 0x60 must be left zero");
        assertGe(p.postStart, p.preStart + 0x20 + p.probeLen, "post-call allocation collided with pre-call memory");
        assertTrue(p.canariesIntact, "canary bytes were overwritten");
    }

    function testFuzz_SafeTransfer_RestoresFreeMemoryPointerForAnyAmount(uint256 amount) public {
        AlwaysOkToken t = new AlwaysOkToken();
        Harness.MemProbe memory p = h.probeTransferMemory(address(t), bob, amount);

        assertEq(p.fmpAfter, p.fmpBefore, "free-memory pointer was not restored exactly");
        assertEq(p.zeroSlotAfter, 0, "the zero slot at 0x60 must be left zero");
        assertGe(p.postStart, p.preStart + 0x20 + p.probeLen, "post-call allocation collided with pre-call memory");
        assertTrue(p.canariesIntact, "canary bytes were overwritten");
    }

    function testFuzz_SafeTransferFrom_RestoresFreeMemoryPointerForAnyAmount(uint256 amount) public {
        AlwaysOkToken t = new AlwaysOkToken();
        Harness.MemProbe memory p = h.probeTransferFromMemory(address(t), alice, bob, amount);

        assertEq(p.fmpAfter, p.fmpBefore, "free-memory pointer was not restored exactly");
        assertEq(p.zeroSlotAfter, 0, "the zero slot at 0x60 must be left zero");
        assertTrue(p.canariesIntact, "canary bytes were overwritten");
    }

    function test_SafeTransfer_SurvivesReentrancyDuringTheCall() public {
        ReenteringToken outer = new ReenteringToken(h);
        TrueToken inner = new TrueToken();

        outer.mint(address(h), AMOUNT);
        inner.mint(address(h), 3e18);
        outer.setInner(address(inner), alice, 3e18);

        Harness.MemProbe memory p = h.probeTransferMemory(address(outer), bob, AMOUNT);

        assertEq(p.fmpAfter, p.fmpBefore, "re-entrancy perturbed the outer frame's pointer");
        assertEq(p.zeroSlotAfter, 0, "the zero slot at 0x60 must be left zero");
        assertGe(p.postStart, p.preStart + 0x20 + p.probeLen, "post-call allocation collided with pre-call memory");
        assertTrue(p.canariesIntact, "canary bytes were overwritten");
        assertEq(outer.balanceOf(bob), AMOUNT, "outer transfer did not settle");
        assertEq(inner.balanceOf(alice), 3e18, "re-entrant transfer did not settle");
    }

    function test_Regression_FailingTransferKeepsReasonAndDoesNotBurnTheFrame() public {
        CustomErrorToken t = new CustomErrorToken();
        uint256 budget = 2_000_000;

        Harness.FailProbe memory p = h.probeFailingTransfer(address(t), bob, AMOUNT, budget);

        assertEq(
            p.reason,
            abi.encodeWithSelector(CustomErrorToken.Boom.selector, address(h), AMOUNT, keccak256("boom")),
            "the token's revert reason was lost"
        );
        assertGt(p.reason.length, 0, "failing transfer reverted with no data");
        assertLt(p.gasUsed, budget / 4, "the failing frame burned its gas budget");
        assertGe(p.postStart, p.preStart + 0x20 + p.probeLen, "post-call allocation collided with pre-call memory");
        assertTrue(p.canariesIntact, "canary bytes were overwritten across the failing call");
    }

    function test_Regression_FailingTransferFromKeepsReasonAndDoesNotBurnTheFrame() public {
        CustomErrorToken t = new CustomErrorToken();
        uint256 budget = 2_000_000;

        Harness.FailProbe memory p = h.probeFailingTransferFrom(address(t), alice, bob, AMOUNT, budget);

        assertEq(
            p.reason,
            abi.encodeWithSelector(CustomErrorToken.Boom.selector, address(h), AMOUNT, keccak256("boom")),
            "the token's revert reason was lost"
        );
        assertLt(p.gasUsed, budget / 4, "the failing frame burned its gas budget");
        assertTrue(p.canariesIntact, "canary bytes were overwritten across the failing call");
    }

    function testFuzz_Regression_FailingTransferDoesNotBurnTheFrame(uint256 amount) public {
        CustomErrorToken t = new CustomErrorToken();
        uint256 budget = 2_000_000;

        Harness.FailProbe memory p = h.probeFailingTransfer(address(t), bob, amount, budget);

        assertEq(
            p.reason,
            abi.encodeWithSelector(CustomErrorToken.Boom.selector, address(h), amount, keccak256("boom")),
            "the token's revert reason was lost"
        );
        assertLt(p.gasUsed, budget / 4, "the failing frame burned its gas budget");
    }

    function test_SafeTransfer_MasksDirtyRecipientHighBits() public {
        SilentToken t = new SilentToken();
        t.mint(address(h), AMOUNT);

        uint256 dirty = (uint256(type(uint96).max) << 160) | uint256(uint160(bob));
        h.doTransferDirtyTo(address(t), dirty, AMOUNT);

        assertEq(t.balanceOf(bob), AMOUNT, "dirty high bits changed the recipient");
    }

    function test_SafeTransferFrom_MasksDirtyHighBits() public {
        SilentToken t = new SilentToken();
        t.mint(alice, AMOUNT);
        vm.prank(alice);
        t.approve(address(h), type(uint256).max);

        uint256 dirtyFrom = (uint256(type(uint96).max) << 160) | uint256(uint160(alice));
        uint256 dirtyTo = (uint256(type(uint96).max) << 160) | uint256(uint160(bob));
        h.doTransferFromDirty(address(t), dirtyFrom, dirtyTo, AMOUNT);

        assertEq(t.balanceOf(bob), AMOUNT, "dirty high bits changed the parties");
        assertEq(t.balanceOf(alice), 0);
    }

    function test_SafeTransfer_ForwardsAllAvailableGas() public {
        GasReporterToken t = new GasReporterToken();

        uint256 g0 = gasleft();
        h.doTransfer(address(t), bob, AMOUNT);
        uint256 spent = g0 - gasleft();

        assertGt(t.seen(), (spent * 60) / 64, "the token was handed materially less than 63/64");
    }

    function test_GasBurner_IsIndistinguishableFromADataLessRevert() public {
        GasBurnerToken burner = new GasBurnerToken();
        NoDataToken silent = new NoDataToken();

        Harness.FailProbe memory burned = h.probeFailingTransfer(address(burner), bob, AMOUNT, 400_000);
        Harness.FailProbe memory reverted = h.probeFailingTransfer(address(silent), bob, AMOUNT, 400_000);

        assertEq(burned.reason.length, 0, "gas exhaustion yields no return data");
        assertEq(reverted.reason.length, 0, "a data-less revert yields no return data");
        assertEq(burned.reason, reverted.reason, "the two are byte-identical to the caller");

        assertGt(burned.gasUsed, reverted.gasUsed * 4, "the burner should have consumed its budget");
    }

    function test_SafeTransfer_ReportsSuccessForFeeOnTransferToken_ByDesign() public {
        FeeToken t = new FeeToken();
        t.mint(address(h), AMOUNT);

        h.doTransfer(address(t), bob, AMOUNT);

        assertLt(t.balanceOf(bob), AMOUNT, "fixture is not actually skimming");
        assertEq(t.balanceOf(bob), AMOUNT - (AMOUNT * t.FEE_BPS()) / 10_000);
    }

    function test_SafeTransferFrom_ReportsSuccessForFeeOnTransferToken_ByDesign() public {
        FeeToken t = new FeeToken();
        t.mint(alice, AMOUNT);
        vm.prank(alice);
        t.approve(address(h), type(uint256).max);

        h.doTransferFrom(address(t), alice, bob, AMOUNT);

        assertLt(t.balanceOf(bob), AMOUNT, "fixture is not actually skimming");
    }

    function test_SafeTransfer_CannotDetectAFallbackThatSwallowsTheCall() public {
        SwallowingFallback t = new SwallowingFallback();
        h.doTransfer(address(t), bob, AMOUNT);
        assertTrue(true, "accepted, and nothing moved: documented limit of convention (1)");
    }
}

contract SwallowingFallback {
    fallback() external {}
}
