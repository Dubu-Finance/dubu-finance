// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {Test} from "forge-std/Test.sol";
import {SafeTransfer} from "../src/libraries/SafeTransfer.sol";

/*//////////////////////////////////////////////////////////////////////////////
                                    HARNESS
//////////////////////////////////////////////////////////////////////////////*/

interface ITestToken {
    function mint(address to, uint256 amount) external;
    function approve(address spender, uint256 amount) external returns (bool);
    function balanceOf(address who) external view returns (uint256);
}

/// @notice Wraps the internal library so tests can observe reverts across a call boundary, and
///         exposes memory probes that read 0x40/0x60 in the same frame that runs the assembly.
contract Harness {
    /// @dev Byte length of the canary buffers the probes allocate on either side of a transfer.
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

    /// @notice `to` is handed to the library with its upper 96 bits deliberately dirty.
    function doTransferDirtyTo(address token, uint256 dirtyTo, uint256 amount) external {
        address to;
        assembly {
            to := dirtyTo
        }
        SafeTransfer.safeTransfer(token, to, amount);
    }

    /// @notice `from` and `to` are handed to the library with their upper 96 bits dirty.
    function doTransferFromDirty(address token, uint256 dirtyFrom, uint256 dirtyTo, uint256 amount) external {
        address from;
        address to;
        assembly {
            from := dirtyFrom
            to := dirtyTo
        }
        SafeTransfer.safeTransferFrom(token, from, to, amount);
    }

    /// @notice Allocate a canary, run a SUCCEEDING `safeTransfer`, allocate another canary.
    /// @dev The success path is the only path on which the caller can observe the reserved region
    ///      at all: every failure path terminates the frame, so its memory is discarded. That is
    ///      why the failure path is pinned by payload + gas (see `probeFailingTransfer`) instead.
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

    /// @notice Same probe around a succeeding `safeTransferFrom` (the control implementation).
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

    /// @notice Allocate a canary, run a FAILING `safeTransfer` under a gas cap, allocate another.
    /// @dev Reports the bubbled payload and the gas the failing frame actually consumed. The bug
    ///      this file regression-tests made the payload empty and the gas consumption total,
    ///      because the failure path read a free-memory pointer it had already overwritten and fed
    ///      the result to `returndatacopy`.
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

    /// @notice Same, for the control implementation.
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

/*//////////////////////////////////////////////////////////////////////////////
                              ADVERSARIAL MOCKS
//////////////////////////////////////////////////////////////////////////////*/

/// @notice Book-keeping shared by the mocks that actually move balances.
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

/// @notice Convention (2): returns a 32-byte `true`.
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

/// @notice Convention (1): returns nothing at all, USDT style.
contract SilentToken is Ledger {
    function transfer(address to, uint256 amount) external {
        _move(msg.sender, to, amount);
    }

    function transferFrom(address from, address to, uint256 amount) external {
        _spend(from, msg.sender, amount);
        _move(from, to, amount);
    }
}

/// @notice Convention (3): returns a 32-byte `false` instead of reverting, and moves nothing.
contract FalseToken is Ledger {
    function transfer(address, uint256) external pure returns (bool) {
        return false;
    }

    function transferFrom(address, address, uint256) external pure returns (bool) {
        return false;
    }
}

/// @notice Returns exactly 32 bytes that are neither 0 nor 1.
contract GarbageWordToken {
    fallback() external {
        assembly {
            mstore(0x00, 2)
            return(0x00, 0x20)
        }
    }
}

/// @notice Returns fewer than 32 bytes, so the first word of the output buffer is only partly
///         overwritten and still holds the library's own selector constant in its tail.
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

/// @notice Returns 96 bytes; the first word is whatever the constructor was given.
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

/// @notice Reverts with a custom error carrying arguments.
contract CustomErrorToken {
    error Boom(address caller, uint256 amount, bytes32 tag);

    function transfer(address, uint256 amount) external view returns (bool) {
        revert Boom(msg.sender, amount, keccak256("boom"));
    }

    function transferFrom(address, address, uint256 amount) external view returns (bool) {
        revert Boom(msg.sender, amount, keccak256("boom"));
    }
}

/// @notice Reverts with a long `Error(string)`.
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

/// @notice Reverts with no return data at all.
contract NoDataToken {
    fallback() external {
        assembly {
            revert(0x00, 0x00)
        }
    }
}

/// @notice Reverts with a large payload of a deterministic pattern: word `i` holds `i + 1`.
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

/// @notice Consumes every unit of gas forwarded to it and returns no data.
contract GasBurnerToken {
    fallback() external {
        assembly {
            invalid()
        }
    }
}

/// @notice Reports how much gas the caller forwarded, then succeeds.
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

/// @notice Returns a 32-byte `true` for anything, keeping no books. Used where the point of the
///         test is the caller's memory rather than the token's accounting.
contract AlwaysOkToken {
    fallback() external {
        assembly {
            mstore(0x00, 1)
            return(0x00, 0x20)
        }
    }
}

/// @notice Skims a fee: reports success while delivering less than requested.
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

/// @notice Calls back into the harness mid-transfer, so the outer frame is suspended with its
///         free-memory-pointer word clobbered while a second `safeTransfer` runs in a new frame.
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

/*//////////////////////////////////////////////////////////////////////////////
                                     TESTS
//////////////////////////////////////////////////////////////////////////////*/

contract SafeTransferTest is Test {
    Harness internal h;

    address internal alice = address(0xA11CE);
    address internal bob = address(0xB0B);

    /// @dev An address that has never been deployed to: the `extcodesize` case.
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

    // ------------------------------------------------------------------ helpers

    /// @dev Byte-for-byte assertion that a failing `safeTransfer` produced exactly `expected`.
    function _assertTransferRevertsVerbatim(address token, bytes memory expected) internal {
        (bool ok, bytes memory ret) = address(h).call(abi.encodeCall(Harness.doTransfer, (token, bob, AMOUNT)));
        assertFalse(ok, "safeTransfer should have failed");
        assertEq(ret, expected, "revert payload was not bubbled verbatim");
    }

    /// @dev Same, for `safeTransferFrom`.
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

    // =====================================================================
    // Revert bubbling — the property the free-memory-pointer bug destroyed
    // =====================================================================

    /// @notice A custom error with arguments must arrive at the caller unaltered.
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

    /// @notice A 600-character `Error(string)` is far longer than one word, so the bubble has to
    ///         expand memory at the cached pointer rather than assume a fixed buffer.
    function test_SafeTransfer_BubblesLongStringVerbatim() public {
        LongStringToken t = new LongStringToken(longReason);
        _assertTransferRevertsVerbatim(address(t), abi.encodeWithSignature("Error(string)", longReason));
    }

    function test_SafeTransferFrom_BubblesLongStringVerbatim() public {
        LongStringToken t = new LongStringToken(longReason);
        _assertTransferFromRevertsVerbatim(address(t), abi.encodeWithSignature("Error(string)", longReason));
    }

    /// @notice 8 KiB of revert data still arrives whole. `returndatacopy` is unbounded by design:
    ///         "verbatim" and "bounded" cannot both hold, and this library chose verbatim.
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

    /// @notice A data-less revert bubbles as a data-less revert. This is the one failure mode the
    ///         library cannot make diagnosable, and it is indistinguishable from the callee running
    ///         out of gas (see `test_GasBurner_IsIndistinguishableFromADataLessRevert`).
    function test_SafeTransfer_BubblesDataLessRevertAsEmpty() public {
        NoDataToken t = new NoDataToken();
        _assertTransferRevertsVerbatim(address(t), bytes(""));
    }

    function test_SafeTransferFrom_BubblesDataLessRevertAsEmpty() public {
        NoDataToken t = new NoDataToken();
        _assertTransferFromRevertsVerbatim(address(t), bytes(""));
    }

    /// @notice The amount is what lands on top of the free-memory pointer, so the bubble has to
    ///         hold for every amount — not just the one a hand-written test happens to pick.
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

    /// @notice Why the defect survived review: at `amount == 0` the clobber writes zeros over the
    ///         pointer's high bytes, which is exactly what the old restore did, so the corrupted
    ///         pointer read back as the correct `0x80` and the bubble worked by accident. Any
    ///         non-zero amount broke it. Pinned so a future refactor cannot re-introduce a fix
    ///         that only holds at zero.
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

    // =====================================================================
    // Return-data conventions
    // =====================================================================

    /// @notice Convention (1): no return data plus code at the address is success.
    function test_SafeTransfer_AcceptsSilentToken() public {
        SilentToken t = new SilentToken();
        t.mint(address(h), AMOUNT);
        h.doTransfer(address(t), bob, AMOUNT);
        assertEq(t.balanceOf(bob), AMOUNT, "silent token should still have moved the balance");
    }

    /// @notice Convention (2): 32 bytes of exactly 1 is success.
    function test_SafeTransfer_AcceptsExplicitTrue() public {
        TrueToken t = new TrueToken();
        t.mint(address(h), AMOUNT);
        h.doTransfer(address(t), bob, AMOUNT);
        assertEq(t.balanceOf(bob), AMOUNT);
    }

    /// @notice Convention (3): 32 bytes of 0 must revert, not silently pass.
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

    /// @notice 32 bytes of anything other than 1 is garbage, not success.
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

    /// @notice A return shorter than a word must be rejected. The `gt(returndatasize(), 31)` guard
    ///         is what stops the library from decoding its own selector constant as a return value:
    ///         the `call` only overwrites the first `returndatasize()` bytes of the output buffer,
    ///         and the rest of that word still holds `0xa9059cbb` from the calldata build.
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

    /// @notice An over-long return whose first word is `true` is accepted — trailing bytes are
    ///         ignored, which is what a strict ABI decoder does too.
    function test_SafeTransfer_AcceptsOverlongReturnWhoseFirstWordIsTrue() public {
        OverlongToken t = new OverlongToken(1);
        h.doTransfer(address(t), bob, AMOUNT);
    }

    /// @notice An over-long return whose first word is not `true` is still a failure. Length alone
    ///         must never be taken as evidence of success.
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

    // =====================================================================
    // extcodesize — a typo'd token address must not silently "succeed"
    // =====================================================================

    /// @notice A `call` to an account with no code returns success with empty return data, which is
    ///         byte-identical to convention (1). Without the code check this would move nothing and
    ///         report success.
    function test_SafeTransfer_RejectsCodelessAddress() public {
        assertEq(codeless.code.length, 0, "fixture must really have no code");
        vm.expectRevert(SafeTransfer.TransferFailed.selector);
        h.doTransfer(codeless, bob, AMOUNT);
    }

    function test_SafeTransferFrom_RejectsCodelessAddress() public {
        vm.expectRevert(SafeTransfer.TransferFromFailed.selector);
        h.doTransferFrom(codeless, alice, bob, AMOUNT);
    }

    /// @notice A precompile is the sharpest form of the same trap: it succeeds, it returns data,
    ///         and `extcodesize` is 0. `identity` (0x04) echoes our calldata back, so the return is
    ///         68 bytes whose first word is the padded recipient — not `true`. Rejected twice over.
    function test_SafeTransfer_RejectsPrecompile() public {
        vm.expectRevert(SafeTransfer.TransferFailed.selector);
        h.doTransfer(address(0x04), bob, AMOUNT);
    }

    /// @notice An EOA with a balance is codeless too.
    function test_SafeTransfer_RejectsFundedEoa() public {
        vm.deal(alice, 1 ether);
        vm.expectRevert(SafeTransfer.TransferFailed.selector);
        h.doTransfer(alice, bob, AMOUNT);
    }

    // =====================================================================
    // Memory discipline below 0x80
    // =====================================================================

    /// @notice `safeTransfer` lays its calldata across 0x00..0x53, and 0x40..0x53 is the high 20
    ///         bytes of the free-memory pointer. With live memory on both sides of the call, the
    ///         pointer must come back byte-identical, the zero slot at 0x60 must still be zero, and
    ///         a buffer allocated afterwards must not land on top of one allocated before.
    function test_SafeTransfer_RestoresFreeMemoryPointerExactly() public {
        AlwaysOkToken t = new AlwaysOkToken();
        Harness.MemProbe memory p = h.probeTransferMemory(address(t), bob, AMOUNT);

        assertEq(p.fmpAfter, p.fmpBefore, "free-memory pointer was not restored exactly");
        assertEq(p.zeroSlotAfter, 0, "the zero slot at 0x60 must be left zero");
        assertGe(p.postStart, p.preStart + 0x20 + p.probeLen, "post-call allocation collided with pre-call memory");
        assertTrue(p.canariesIntact, "canary bytes were overwritten");
    }

    /// @notice The control implementation, which additionally borrows the zero slot at 0x60.
    function test_SafeTransferFrom_RestoresFreeMemoryPointerAndZeroSlot() public {
        AlwaysOkToken t = new AlwaysOkToken();
        Harness.MemProbe memory p = h.probeTransferFromMemory(address(t), alice, bob, AMOUNT);

        assertEq(p.fmpAfter, p.fmpBefore, "free-memory pointer was not restored exactly");
        assertEq(p.zeroSlotAfter, 0, "the zero slot at 0x60 must be left zero");
        assertGe(p.postStart, p.preStart + 0x20 + p.probeLen, "post-call allocation collided with pre-call memory");
        assertTrue(p.canariesIntact, "canary bytes were overwritten");
    }

    /// @notice The clobbered bytes are bytes 12..31 of `amount`, so pointer restoration has to hold
    ///         across the whole range — including amounts whose high bits are set.
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

    /// @notice The reserved region is clobbered for the whole duration of the `call`. A token that
    ///         re-enters us runs in a fresh frame with its own memory, so the outer frame's pointer
    ///         survives — this pins that the assumption holds rather than merely looking plausible.
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

    /// @notice Regression pin for the bug this file exists for. A failing `safeTransfer` with live
    ///         memory on both sides must (a) bubble the token's reason verbatim, (b) not disturb the
    ///         caller's memory, and (c) not consume the frame's gas.
    ///
    ///         (c) is the part that has to be measured rather than inspected: every failure path
    ///         terminates the frame, so the reverting frame's own pointer cannot be read back.
    ///         Feeding a corrupted pointer to `returndatacopy` costs the entire remaining gas via
    ///         `InvalidOperandOOG`, so consumption is a faithful proxy — under the bug this frame
    ///         burned essentially the whole budget and returned nothing.
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

    /// @notice The same pin on the control implementation, so the two stay symmetric.
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

    /// @notice The corrupted pointer was `(amount << 96) | 0x80`, so its blast radius scaled with
    ///         the amount. Fuzzed so no single amount can hide a partial fix.
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

    // =====================================================================
    // Dirty argument bits
    // =====================================================================

    /// @notice `mstore(0x14, to)` writes the full 32-byte word, so a dirty `to` would spill its
    ///         upper bits into 0x14..0x1f — which is the recipient argument's ABI padding. The
    ///         selector store lands afterwards and re-zeroes that range, so the token still sees a
    ///         clean address.
    function test_SafeTransfer_MasksDirtyRecipientHighBits() public {
        SilentToken t = new SilentToken();
        t.mint(address(h), AMOUNT);

        uint256 dirty = (uint256(type(uint96).max) << 160) | uint256(uint160(bob));
        h.doTransferDirtyTo(address(t), dirty, AMOUNT);

        assertEq(t.balanceOf(bob), AMOUNT, "dirty high bits changed the recipient");
    }

    /// @notice Same for `safeTransferFrom`, where `from` is placed with `shl(96, from)` (which
    ///         shifts dirty bits out) and `to`'s padding is re-zeroed by the `from` store.
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

    // =====================================================================
    // Gas
    // =====================================================================

    /// @notice No stipend or artificial cap: the token gets the 63/64 the EVM allows. Tokens with
    ///         transfer hooks depend on this, so a future "gas safety" cap would be a behaviour
    ///         change, not a hardening.
    function test_SafeTransfer_ForwardsAllAvailableGas() public {
        GasReporterToken t = new GasReporterToken();

        uint256 g0 = gasleft();
        h.doTransfer(address(t), bob, AMOUNT);
        uint256 spent = g0 - gasleft();

        assertGt(t.seen(), (spent * 60) / 64, "the token was handed materially less than 63/64");
    }

    /// @notice A token that eats every forwarded unit of gas is observationally identical to one
    ///         that reverts with no data: `call` returns 0 and `returndatasize()` is 0 in both
    ///         cases, so the bubble is empty either way.
    ///
    ///         This is a property of `CALL`, not a defect in the library, and it is the residual
    ///         after the pointer fix: `TransferFailed()` cannot be substituted for the empty bubble
    ///         without giving up the documented "bubble verbatim" contract. The consequence worth
    ///         knowing is the 63/64 rule — a caller wrapping the pool in `try/catch` resumes with
    ///         only 1/64 of the gas it had, so the griefing vector is the gas, not the ambiguity.
    function test_GasBurner_IsIndistinguishableFromADataLessRevert() public {
        GasBurnerToken burner = new GasBurnerToken();
        NoDataToken silent = new NoDataToken();

        Harness.FailProbe memory burned = h.probeFailingTransfer(address(burner), bob, AMOUNT, 400_000);
        Harness.FailProbe memory reverted = h.probeFailingTransfer(address(silent), bob, AMOUNT, 400_000);

        assertEq(burned.reason.length, 0, "gas exhaustion yields no return data");
        assertEq(reverted.reason.length, 0, "a data-less revert yields no return data");
        assertEq(burned.reason, reverted.reason, "the two are byte-identical to the caller");

        // The distinguishing signal is gas, and only the caller can see it.
        assertGt(burned.gasUsed, reverted.gasUsed * 4, "the burner should have consumed its budget");
    }

    // =====================================================================
    // Documented non-goals
    // =====================================================================

    /// @notice Fee-on-transfer is out of scope by design: the library reports success/failure, not
    ///         the amount delivered. Pinned so the gap stays a decision instead of a surprise —
    ///         `PropPool` must not list such a token.
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

    /// @notice A contract whose fallback swallows unknown selectors is indistinguishable from USDT:
    ///         both return no data and both have code. There is no signal left to check, so this is
    ///         a listing-time concern, not something the primitive can catch.
    function test_SafeTransfer_CannotDetectAFallbackThatSwallowsTheCall() public {
        SwallowingFallback t = new SwallowingFallback();
        h.doTransfer(address(t), bob, AMOUNT);
        assertTrue(true, "accepted, and nothing moved: documented limit of convention (1)");
    }
}

/// @notice Has code, accepts anything, returns nothing, moves nothing.
contract SwallowingFallback {
    fallback() external {}
}
