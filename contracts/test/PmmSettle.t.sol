// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {Test} from "forge-std/Test.sol";

import {PmmSettle} from "../src/PmmSettle.sol";
import {IPmmSettle} from "../src/interfaces/IPmmSettle.sol";
import {MockERC20} from "../src/mocks/MockERC20.sol";

/*//////////////////////////////////////////////////////////////////////////////
                                 TOKEN DOUBLES
//////////////////////////////////////////////////////////////////////////////*/

/// @notice USDT-shaped: `transfer`/`transferFrom` succeed and return **nothing**.
/// @dev `SafeTransfer` treats empty returndata as success, and this is the token that proves it
///      rather than the comment claiming it. It is also the shape that would break a naive
///      `IERC20(t).transferFrom(...)`, which demands 32 bytes back.
contract NoReturnToken is MockERC20 {
    constructor() MockERC20("NoReturn", "NRT", 18) {}

    function transfer(address to, uint256 amount) public override returns (bool) {
        _transfer(msg.sender, to, amount);
        assembly {
            return(0, 0)
        }
    }

    function transferFrom(address from, address to, uint256 amount) public override returns (bool) {
        _spendAllowance(from, msg.sender, amount);
        _transfer(from, to, amount);
        assembly {
            return(0, 0)
        }
    }
}

/// @notice Returns `false` instead of reverting. The failure mode `SafeTransfer` exists for.
contract FalseReturnToken is MockERC20 {
    constructor() MockERC20("False", "FLS", 18) {}

    function transferFrom(address, address, uint256) public pure override returns (bool) {
        return false;
    }
}

/// @notice Re-enters `PmmSettle.fillOrder` from inside `transferFrom`.
///
/// @dev A token is arbitrary code that `PmmSettle` calls with a live call frame and a signed
///      order in scope. This is the only thing standing between "the guard is documented" and
///      "the guard works". The re-entry is not wrapped in `try`, so the inner `Reentrancy()`
///      bubbles all the way out and the outer call is what the test observes.
contract ReentrantToken is MockERC20 {
    PmmSettle public settle;
    IPmmSettle.Order internal _order;
    bytes internal _signature;
    bool public armed;

    constructor() MockERC20("Reentrant", "RNT", 18) {}

    function arm(PmmSettle settle_, IPmmSettle.Order calldata order, bytes calldata signature) external {
        settle = settle_;
        _order = order;
        _signature = signature;
        armed = true;
    }

    function transferFrom(address from, address to, uint256 amount) public override returns (bool) {
        if (armed) {
            armed = false;
            settle.fillOrder(_order, _signature, 1, type(uint32).max, address(this));
        }
        _spendAllowance(from, msg.sender, amount);
        _transfer(from, to, amount);
        return true;
    }
}

/*//////////////////////////////////////////////////////////////////////////////
                                     BASE
//////////////////////////////////////////////////////////////////////////////*/

/// @notice Shared fixture: one maker with a known key, two tokens, and an order builder.
abstract contract PmmSettleBase is Test {
    PmmSettle internal settle;
    MockERC20 internal makerAsset;
    MockERC20 internal takerAsset;

    uint256 internal constant MAKER_PK = 0xA11CE;
    uint256 internal constant OTHER_PK = 0xB0B;
    address internal maker;
    address internal other;
    address internal taker = address(0x7A4E);
    address internal receiver = address(0x4EC0);

    /// @dev `secp256k1n`. Used to build the malleable twin of a valid signature.
    uint256 internal constant CURVE_ORDER = 0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEBAAEDCE6AF48A03BBFD25E8CD0364141;

    uint256 internal constant MAKER_SIZE = 3_000e6;
    uint256 internal constant TAKER_SIZE = 1e18;
    /// @dev The smallest taker leg that buys at least one unit of the maker leg at this rate:
    ///      `ceil(TAKER_SIZE / MAKER_SIZE)` is ~3.34e8, so 1e12 clears it with room to spare.
    uint256 internal constant MIN_SLICE = 1e12;

    /// @dev `keccak256(ORDER_TYPE_STRING)`, written out so the test's own encoder does not read
    ///      it from the contract under test. `PmmSettleEip712Test` checks it against both the
    ///      type string and the contract.
    bytes32 internal constant TYPEHASH = 0x23e655e78a91115e92aff2d730688fc421a3773ea96b4afcd69c21acf9e8be56;

    function setUp() public virtual {
        maker = vm.addr(MAKER_PK);
        other = vm.addr(OTHER_PK);

        settle = new PmmSettle();
        makerAsset = new MockERC20("Maker", "MKR", 6);
        takerAsset = new MockERC20("Taker", "TKR", 18);

        // The maker's standing allowance. This contract custodies nothing, so this approval is
        // the funding model — see the note at the top of PmmSettle.
        makerAsset.mint(maker, 1_000_000e6);
        vm.prank(maker);
        makerAsset.approve(address(settle), type(uint256).max);

        takerAsset.mint(taker, 1_000e18);
        vm.prank(taker);
        takerAsset.approve(address(settle), type(uint256).max);

        // Deterministic, and comfortably past any `decayStart` the tests choose.
        vm.warp(1_800_000_000);
    }

    // ---------------------------------------------------------------------
    // Builders
    // ---------------------------------------------------------------------

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
        return _signWith(MAKER_PK, o);
    }

    function _signWith(uint256 pk, IPmmSettle.Order memory o) internal view returns (bytes memory) {
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(pk, _digest(o, address(settle)));
        return abi.encodePacked(r, s, v);
    }

    /// @dev The digest, computed locally rather than by calling `settle.hashOrder`.
    ///
    ///      Not a style preference — a helper that makes a real external call cannot be used
    ///      inside an armed `vm.prank` or `vm.expectRevert`, because Solidity evaluates call
    ///      arguments after the cheatcode is set up and `hashOrder` would then be the call the
    ///      cheatcode applies to. Every negative test in this file would silently degrade into
    ///      "some other call did not revert". Computing it here keeps the arrangement legible and,
    ///      because it is a second implementation, also means a `hashOrder` that drifted from the
    ///      spec would break every fill test rather than none.
    ///
    ///      `PmmSettleEip712Test.test_hashOrderMatchesTheTestHelper` pins the two together.
    function _digest(IPmmSettle.Order memory o, address verifying) internal view returns (bytes32) {
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
                verifying
            )
        );
        return keccak256(abi.encodePacked("\x19\x01", separator, structHash));
    }

    /// @dev An address whose twenty bytes are all `b`, matching `[b; 20]` in the Rust mirror.
    function _addr(uint8 b) internal pure returns (address) {
        bytes20 out;
        for (uint256 i; i < 20; ++i) {
            out |= bytes20(bytes1(b)) >> (8 * i);
        }
        return address(out);
    }
}

/*//////////////////////////////////////////////////////////////////////////////
                                    EIP-712
//////////////////////////////////////////////////////////////////////////////*/

contract PmmSettleEip712Test is PmmSettleBase {
    /// @dev The literal every off-chain signer must reproduce. Duplicated here rather than
    ///      imported from the contract on purpose: if it were read from `PmmSettle` this test
    ///      would assert `x == x`.
    string internal constant TYPE_STRING = "Order(address maker,address makerAsset,address takerAsset,"
        "uint256 makerAmount,uint256 takerAmount,uint64 nonce,uint64 expiry,uint64 decayStart,"
        "uint32 decayPerSec,uint32 decayCap,uint16 minFillBps)";

    function test_typehashMatchesTheTypeString() public view {
        assertEq(settle.ORDER_TYPEHASH(), keccak256(bytes(TYPE_STRING)));
    }

    /// @notice The exact 32 bytes `dubu_core::rfq::ORDER_TYPEHASH` holds.
    /// @dev Written out rather than derived so the two files can be diffed by eye. A change to
    ///      any field name, type or position moves this and fails on both sides at once.
    function test_typehashMatchesTheRustMirror() public view {
        assertEq(settle.ORDER_TYPEHASH(), 0x23e655e78a91115e92aff2d730688fc421a3773ea96b4afcd69c21acf9e8be56);
    }

    function test_domainSeparatorIsTheEip712Encoding() public view {
        bytes32 expected = keccak256(
            abi.encode(
                keccak256("EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)"),
                keccak256("DuBu PmmSettle"),
                keccak256("1"),
                block.chainid,
                address(settle)
            )
        );
        assertEq(settle.DOMAIN_SEPARATOR(), expected);
    }

    /// @notice The cached separator must be abandoned when the chain forks under the contract.
    function test_domainSeparatorIsRederivedOnChainIdChange() public {
        bytes32 before = settle.DOMAIN_SEPARATOR();
        vm.chainId(block.chainid + 1);
        bytes32 afterFork = settle.DOMAIN_SEPARATOR();

        assertTrue(before != afterFork);
        assertEq(
            afterFork,
            keccak256(
                abi.encode(
                    keccak256("EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)"),
                    keccak256("DuBu PmmSettle"),
                    keccak256("1"),
                    block.chainid,
                    address(settle)
                )
            )
        );
    }

    /// @notice And an order signed before the fork must not be fillable after it — the actual
    ///         consequence the re-derivation exists to produce.
    function test_signatureDoesNotSurviveAChainIdChange() public {
        IPmmSettle.Order memory o = _order();
        bytes memory sig = _sign(o);

        vm.chainId(block.chainid + 1);

        vm.prank(taker);
        vm.expectRevert(PmmSettle.BadSignature.selector);
        settle.fillOrder(o, sig, TAKER_SIZE, type(uint32).max, receiver);
    }

    /// @notice Two deployments on one chain must not share signatures.
    function test_signatureIsBoundToTheVerifyingContract() public {
        IPmmSettle.Order memory o = _order();
        bytes memory sig = _sign(o);

        PmmSettle twin = new PmmSettle();
        assertTrue(twin.DOMAIN_SEPARATOR() != settle.DOMAIN_SEPARATOR());

        vm.prank(maker);
        makerAsset.approve(address(twin), type(uint256).max);
        vm.prank(taker);
        takerAsset.approve(address(twin), type(uint256).max);

        vm.prank(taker);
        vm.expectRevert(PmmSettle.BadSignature.selector);
        twin.fillOrder(o, sig, TAKER_SIZE, type(uint32).max, receiver);
    }

    /// @notice The base fixture computes digests itself; this pins its encoder to the contract's.
    function test_hashOrderMatchesTheTestHelper() public view {
        assertEq(settle.hashOrder(_order()), _digest(_order(), address(settle)));
    }

    function test_hashOrderIsTheStandardTypedDataDigest() public view {
        IPmmSettle.Order memory o = _order();
        bytes32 structHash = keccak256(
            abi.encode(
                settle.ORDER_TYPEHASH(),
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
        assertEq(settle.hashOrder(o), keccak256(abi.encodePacked("\x19\x01", settle.DOMAIN_SEPARATOR(), structHash)));
    }

    // ---------------------------------------------------------------------
    // The shared vectors
    // ---------------------------------------------------------------------

    /// @notice The five fixed vectors, byte-identical to `dubu_core::rfq::vectors::all()`.
    ///
    /// @dev Neither side computes these; both assert them. That is the only arrangement under
    ///      which they are evidence rather than a tautology.
    ///
    ///      The digest is checked against a *literal* domain separator built from the vector's
    ///      chain id and verifying contract rather than against `settle.DOMAIN_SEPARATOR()`,
    ///      because the vectors deliberately name chains and addresses this test is not deployed
    ///      on — including chain id `0` and `type(uint64).max`, which `vm.chainId` will not accept.
    function test_eip712VectorsMatchTheRustMirror() public view {
        _checkVector(
            "zero",
            IPmmSettle.Order({
                maker: address(0),
                makerAsset: address(0),
                takerAsset: address(0),
                makerAmount: 0,
                takerAmount: 0,
                nonce: 0,
                expiry: 0,
                decayStart: 0,
                decayPerSec: 0,
                decayCap: 0,
                minFillBps: 0
            }),
            0,
            address(0),
            0xe4f4bdaa8c4f12a4f520a975132dc664c5289e80fab437a5ba98fd51dc2c7b29,
            0xbcc0c92d5f4d665e0ad5acda83ccc00e485acfac544532beed93b3df0689d84f
        );

        _checkVector(
            "weth_usdc",
            _vectorWethUsdc(0),
            91_342,
            _addr(0x11),
            0x9dc8b4546d5a28923f50a4374402e8b0774db53704d9d668aa1be1a75a30259e,
            0xfd8b2f15795e0412c2494fb162b8e8c84ae531843730ad9fd27e78dc6b5006dc
        );

        _checkVector(
            "domain_ceiling",
            IPmmSettle.Order({
                maker: _addr(0xff),
                makerAsset: _addr(0x01),
                takerAsset: _addr(0x02),
                makerAmount: type(uint128).max,
                takerAmount: type(uint128).max,
                nonce: 0,
                expiry: type(uint64).max,
                decayStart: 0,
                decayPerSec: 0,
                decayCap: 0,
                minFillBps: 10_000
            }),
            1,
            _addr(0xde),
            0xe5bafa60feed25b42a0e0ca9ef1230af79d5ecdafed4eb60cc93953256305b7b,
            0xd076d897e2cef09edf1c915695a37b4ccb28d6ef230cb3e44c797cecda588415
        );

        _checkVector(
            "narrow_fields_max",
            IPmmSettle.Order({
                maker: _addr(0x0a),
                makerAsset: _addr(0x0b),
                takerAsset: _addr(0x0c),
                makerAmount: 1,
                takerAmount: 1,
                nonce: type(uint64).max,
                expiry: type(uint64).max,
                decayStart: type(uint64).max,
                decayPerSec: type(uint32).max,
                decayCap: type(uint32).max,
                minFillBps: type(uint16).max
            }),
            type(uint64).max,
            _addr(0xee),
            0xa754907b3a90f7c66922923dc858007d4e2235aaf44c56bec298937ad69134ac,
            0xf4a14b260933d10c42a3e7f80909e898e396b8e92accc08c1c72466797bb581e
        );

        // Identical to `weth_usdc` but for one `uint16`. If the narrowest member were dropped
        // from `encodeData` the two would collide, and the assertion below would still pass on
        // both — which is why the collision itself is checked separately.
        _checkVector(
            "weth_usdc_min_fill",
            _vectorWethUsdc(1),
            91_342,
            _addr(0x11),
            0x83fe65ecbd2f178813110780f7fea08b239bc0192296d6afa13218dd060bf5a7,
            0xe332ed6b3fb00d5917406e37e578c2f22363b216ab21f4389ae463aeebbc2e64
        );
    }

    function test_vectorsDifferingOnlyInMinFillBpsDoNotCollide() public view {
        assertTrue(_structHash(_vectorWethUsdc(0)) != _structHash(_vectorWethUsdc(1)));
    }

    function _vectorWethUsdc(uint16 minFillBps) internal pure returns (IPmmSettle.Order memory) {
        return IPmmSettle.Order({
            maker: _addr(0xa1),
            makerAsset: _addr(0xb2),
            takerAsset: _addr(0xc3),
            makerAmount: 3_000_000_000,
            takerAmount: 1_000_000_000_000_000_000,
            nonce: 7,
            expiry: 1_800_000_060,
            decayStart: 1_800_000_000,
            decayPerSec: 250,
            decayCap: 50_000,
            minFillBps: minFillBps
        });
    }

    function _checkVector(
        string memory name,
        IPmmSettle.Order memory o,
        uint256 chainId,
        address verifying,
        bytes32 expectedStructHash,
        bytes32 expectedDigest
    ) internal view {
        assertEq(_structHash(o), expectedStructHash, string.concat(name, " struct hash"));

        bytes32 separator = keccak256(
            abi.encode(
                keccak256("EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)"),
                keccak256("DuBu PmmSettle"),
                keccak256("1"),
                chainId,
                verifying
            )
        );
        assertEq(
            keccak256(abi.encodePacked("\x19\x01", separator, expectedStructHash)),
            expectedDigest,
            string.concat(name, " digest")
        );
    }

    function _structHash(IPmmSettle.Order memory o) internal view returns (bytes32) {
        return keccak256(
            abi.encode(
                settle.ORDER_TYPEHASH(),
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
    }
}

/*//////////////////////////////////////////////////////////////////////////////
                                  SIGNATURES
//////////////////////////////////////////////////////////////////////////////*/

contract PmmSettleSignatureTest is PmmSettleBase {
    function test_validSignatureFills() public {
        IPmmSettle.Order memory o = _order();
        vm.prank(taker);
        uint256 out = settle.fillOrder(o, _sign(o), TAKER_SIZE, type(uint32).max, receiver);
        assertEq(out, MAKER_SIZE);
        assertEq(makerAsset.balanceOf(receiver), MAKER_SIZE);
        assertEq(takerAsset.balanceOf(maker), TAKER_SIZE);
    }

    function test_signatureFromAnotherKeyReverts() public {
        IPmmSettle.Order memory o = _order();
        vm.prank(taker);
        vm.expectRevert(PmmSettle.BadSignature.selector);
        settle.fillOrder(o, _signWith(OTHER_PK, o), TAKER_SIZE, type(uint32).max, receiver);
    }

    /// @notice Every signed field, one at a time. A field that fell out of `encodeData` would
    ///         let the corresponding mutation through, and this is the test that would catch it.
    function test_everySignedFieldIsCovered() public {
        IPmmSettle.Order memory signed = _order();
        bytes memory sig = _sign(signed);

        for (uint256 i; i < 11; ++i) {
            IPmmSettle.Order memory tampered = _order();
            if (i == 0) tampered.maker = other;
            else if (i == 1) tampered.makerAsset = address(0xBEEF01);
            else if (i == 2) tampered.takerAsset = address(0xBEEF02);
            else if (i == 3) tampered.makerAmount = MAKER_SIZE + 1;
            else if (i == 4) tampered.takerAmount = TAKER_SIZE + 1;
            else if (i == 5) tampered.nonce = 2;
            else if (i == 6) tampered.expiry = signed.expiry + 1;
            else if (i == 7) tampered.decayStart = uint64(block.timestamp);
            else if (i == 8) tampered.decayPerSec = 1;
            else if (i == 9) tampered.decayCap = 1;
            else tampered.minFillBps = 1;

            vm.prank(taker);
            // `maker` is the recovery target, so tampering with it yields "recovered someone
            // else" just like the rest; every mutation lands on the same error.
            vm.expectRevert(PmmSettle.BadSignature.selector);
            settle.fillOrder(tampered, sig, 1e17, type(uint32).max, receiver);
        }
    }

    function test_signatureMustBe65Bytes() public {
        IPmmSettle.Order memory o = _order();
        bytes memory sig = _sign(o);

        bytes memory short_ = new bytes(64);
        for (uint256 i; i < 64; ++i) {
            short_[i] = sig[i];
        }
        vm.prank(taker);
        vm.expectRevert(abi.encodeWithSelector(PmmSettle.BadSignatureLength.selector, 64));
        settle.fillOrder(o, short_, TAKER_SIZE, type(uint32).max, receiver);

        bytes memory long_ = abi.encodePacked(sig, bytes1(0x00));
        vm.prank(taker);
        vm.expectRevert(abi.encodeWithSelector(PmmSettle.BadSignatureLength.selector, 66));
        settle.fillOrder(o, long_, TAKER_SIZE, type(uint32).max, receiver);

        vm.prank(taker);
        vm.expectRevert(abi.encodeWithSelector(PmmSettle.BadSignatureLength.selector, 0));
        settle.fillOrder(o, "", TAKER_SIZE, type(uint32).max, receiver);
    }

    /// @notice The high-`s` twin of a valid signature recovers the same address and is rejected.
    function test_malleableSignatureReverts() public {
        IPmmSettle.Order memory o = _order();
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(MAKER_PK, settle.hashOrder(o));

        bytes32 flippedS = bytes32(CURVE_ORDER - uint256(s));
        uint8 flippedV = v == 27 ? 28 : 27;
        // Sanity: the twin really is a valid signature by the same maker. Without this the test
        // could pass because the twin is simply garbage.
        assertEq(ecrecover(settle.hashOrder(o), flippedV, r, flippedS), maker);

        vm.prank(taker);
        vm.expectRevert(PmmSettle.MalleableSignature.selector);
        settle.fillOrder(o, abi.encodePacked(r, flippedS, flippedV), TAKER_SIZE, type(uint32).max, receiver);
    }

    function test_vOutsideTwentySevenTwentyEightReverts() public {
        IPmmSettle.Order memory o = _order();
        (, bytes32 r, bytes32 s) = vm.sign(MAKER_PK, settle.hashOrder(o));

        for (uint256 i; i < 3; ++i) {
            uint8 badV = [uint8(0), 1, 29][i];
            vm.prank(taker);
            vm.expectRevert(PmmSettle.MalleableSignature.selector);
            settle.fillOrder(o, abi.encodePacked(r, s, badV), TAKER_SIZE, type(uint32).max, receiver);
        }
    }

    /// @notice A signature `ecrecover` cannot resolve returns `address(0)`; it must not match.
    function test_unrecoverableSignatureReverts() public {
        IPmmSettle.Order memory o = _order();
        // `r == 0` is not a point on the curve, so `ecrecover` yields the zero address.
        vm.prank(taker);
        vm.expectRevert(PmmSettle.BadSignature.selector);
        settle.fillOrder(o, abi.encodePacked(bytes32(0), bytes32(uint256(1)), uint8(27)), 1e17, 0, receiver);
    }

    /// @notice Anyone may present a signature — with their own tokens. The maker is unharmed,
    ///         which is why this is a documented property rather than a bug.
    function test_anyoneCanFillButPaysThemselves() public {
        IPmmSettle.Order memory o = _order();
        address stranger = address(0xDEAD01);
        takerAsset.mint(stranger, TAKER_SIZE);
        vm.prank(stranger);
        takerAsset.approve(address(settle), TAKER_SIZE);

        vm.prank(stranger);
        settle.fillOrder(o, _sign(o), TAKER_SIZE, type(uint32).max, stranger);

        assertEq(takerAsset.balanceOf(stranger), 0);
        assertEq(makerAsset.balanceOf(stranger), MAKER_SIZE);
        assertEq(takerAsset.balanceOf(maker), TAKER_SIZE);
    }
}

/*//////////////////////////////////////////////////////////////////////////////
                              EXPIRY & CANCELLATION
//////////////////////////////////////////////////////////////////////////////*/

contract PmmSettleLifetimeTest is PmmSettleBase {
    function test_fillAtExactExpirySucceeds() public {
        IPmmSettle.Order memory o = _order();
        bytes memory sig = _sign(o);

        vm.warp(o.expiry);
        vm.prank(taker);
        settle.fillOrder(o, sig, TAKER_SIZE, type(uint32).max, receiver);
        assertEq(makerAsset.balanceOf(receiver), MAKER_SIZE);
    }

    function test_fillOneSecondPastExpiryReverts() public {
        IPmmSettle.Order memory o = _order();
        bytes memory sig = _sign(o);

        vm.warp(uint256(o.expiry) + 1);
        vm.prank(taker);
        vm.expectRevert(
            abi.encodeWithSelector(PmmSettle.OrderExpired.selector, uint256(o.expiry), uint256(o.expiry) + 1)
        );
        settle.fillOrder(o, sig, TAKER_SIZE, type(uint32).max, receiver);
    }

    function test_cancelNonceKillsTheOrder() public {
        IPmmSettle.Order memory o = _order();
        bytes memory sig = _sign(o);

        vm.prank(maker);
        settle.cancelNonce(o.nonce);
        assertTrue(settle.isNonceCancelled(maker, o.nonce));

        vm.prank(taker);
        vm.expectRevert(abi.encodeWithSelector(PmmSettle.NonceCancelled.selector, maker, o.nonce));
        settle.fillOrder(o, sig, TAKER_SIZE, type(uint32).max, receiver);
    }

    /// @notice Cancellation kills a partly filled order too — the remaining size is forfeited,
    ///         which is exactly what a kill switch is supposed to do.
    function test_cancelKillsAPartlyFilledOrder() public {
        IPmmSettle.Order memory o = _order();
        bytes memory sig = _sign(o);

        vm.prank(taker);
        settle.fillOrder(o, sig, TAKER_SIZE / 2, type(uint32).max, receiver);
        assertEq(settle.remainingTaker(o), TAKER_SIZE / 2);

        vm.prank(maker);
        settle.cancelNonce(o.nonce);

        vm.prank(taker);
        vm.expectRevert(abi.encodeWithSelector(PmmSettle.NonceCancelled.selector, maker, o.nonce));
        settle.fillOrder(o, sig, TAKER_SIZE / 2, type(uint32).max, receiver);
    }

    /// @notice The whole point of a shared nonce: one `SSTORE` retires a streamed quote's
    ///         siblings along with it.
    function test_cancelNonceKillsEverySiblingSharingIt() public {
        IPmmSettle.Order memory a = _order();
        IPmmSettle.Order memory b = _order();
        b.makerAmount = MAKER_SIZE + 1; // a different order, same nonce
        bytes memory sigA = _sign(a);
        bytes memory sigB = _sign(b);

        // Independent accounting while both are live.
        assertEq(settle.remainingTaker(a), TAKER_SIZE);
        assertEq(settle.remainingTaker(b), TAKER_SIZE);

        vm.prank(maker);
        settle.cancelNonce(a.nonce);

        vm.prank(taker);
        vm.expectRevert(abi.encodeWithSelector(PmmSettle.NonceCancelled.selector, maker, a.nonce));
        settle.fillOrder(a, sigA, 1e17, type(uint32).max, receiver);

        vm.prank(taker);
        vm.expectRevert(abi.encodeWithSelector(PmmSettle.NonceCancelled.selector, maker, b.nonce));
        settle.fillOrder(b, sigB, 1e17, type(uint32).max, receiver);
    }

    /// @notice A fully consumed order does NOT burn its nonce, precisely so its siblings live on.
    function test_fullFillDoesNotBurnTheNonce() public {
        IPmmSettle.Order memory a = _order();
        vm.prank(taker);
        settle.fillOrder(a, _sign(a), TAKER_SIZE, type(uint32).max, receiver);

        assertFalse(settle.isNonceCancelled(maker, a.nonce));

        IPmmSettle.Order memory b = _order();
        b.makerAmount = MAKER_SIZE + 1;
        vm.prank(taker);
        settle.fillOrder(b, _sign(b), TAKER_SIZE, type(uint32).max, receiver);
        assertEq(settle.remainingTaker(b), 0);
    }

    function test_cancelNonceSlotRetires256AtOnce() public {
        vm.prank(maker);
        settle.cancelNonceSlot(0, type(uint256).max);

        for (uint256 i; i < 256; ++i) {
            // casting to 'uint64' is safe because the loop bound is 256
            // forge-lint: disable-next-line(unsafe-typecast)
            assertTrue(settle.isNonceCancelled(maker, uint64(i)));
        }
        assertFalse(settle.isNonceCancelled(maker, 256));
    }

    function test_cancellationIsPerMaker() public {
        vm.prank(other);
        settle.cancelNonce(1);
        assertTrue(settle.isNonceCancelled(other, 1));
        assertFalse(settle.isNonceCancelled(maker, 1));

        IPmmSettle.Order memory o = _order();
        vm.prank(taker);
        settle.fillOrder(o, _sign(o), TAKER_SIZE, type(uint32).max, receiver);
    }

    /// @notice Idempotent, by design: a maker tripping a kill switch does not know which quotes
    ///         are already dead, and a revert would abort the batch and leave live ones standing.
    function test_cancellingTwiceIsANoOp() public {
        vm.startPrank(maker);
        settle.cancelNonce(9);
        settle.cancelNonce(9);
        settle.cancelNonceSlot(0, 1 << 9);
        vm.stopPrank();
        assertTrue(settle.isNonceCancelled(maker, 9));
    }

    function test_nonceBitmapAddressesHighNonces() public {
        uint64 n = type(uint64).max;
        vm.prank(maker);
        settle.cancelNonce(n);
        assertTrue(settle.isNonceCancelled(maker, n));
        assertFalse(settle.isNonceCancelled(maker, n - 1));
    }
}

/*//////////////////////////////////////////////////////////////////////////////
                            PARTIAL FILLS & REPLAY
//////////////////////////////////////////////////////////////////////////////*/

contract PmmSettlePartialFillTest is PmmSettleBase {
    function test_remainingStartsFullAndTracksEveryFill() public {
        IPmmSettle.Order memory o = _order();
        bytes memory sig = _sign(o);
        assertEq(settle.remainingTaker(o), TAKER_SIZE);

        vm.prank(taker);
        settle.fillOrder(o, sig, 3e17, type(uint32).max, receiver);
        assertEq(settle.remainingTaker(o), TAKER_SIZE - 3e17);
        assertEq(settle.filledTaker(settle.hashOrder(o)), 3e17);

        vm.prank(taker);
        settle.fillOrder(o, sig, 7e17, type(uint32).max, receiver);
        assertEq(settle.remainingTaker(o), 0);
    }

    /// @notice The headline property: a sequence of partial fills sums to exactly the order size
    ///         and **not one unit more**.
    function test_partialFillsSumToTheOrderSizeAndNotOneUnitMore() public {
        IPmmSettle.Order memory o = _order();
        bytes memory sig = _sign(o);

        uint256[5] memory slices = [uint256(1e17), 2e17, 3e17, 35e16, 5e16];
        uint256 total;
        for (uint256 i; i < slices.length; ++i) {
            vm.prank(taker);
            settle.fillOrder(o, sig, slices[i], type(uint32).max, receiver);
            total += slices[i];
        }
        assertEq(total, TAKER_SIZE);
        assertEq(settle.remainingTaker(o), 0);
        assertEq(takerAsset.balanceOf(maker), TAKER_SIZE);

        vm.prank(taker);
        vm.expectRevert(abi.encodeWithSelector(PmmSettle.FillExceedsRemaining.selector, 1, 0));
        settle.fillOrder(o, sig, 1, type(uint32).max, receiver);
    }

    function test_overRequestingRevertsRatherThanClamping() public {
        IPmmSettle.Order memory o = _order();
        bytes memory sig = _sign(o);

        vm.prank(taker);
        settle.fillOrder(o, sig, 6e17, type(uint32).max, receiver);

        vm.prank(taker);
        vm.expectRevert(abi.encodeWithSelector(PmmSettle.FillExceedsRemaining.selector, 4e17 + 1, 4e17));
        settle.fillOrder(o, sig, 4e17 + 1, type(uint32).max, receiver);

        // ...and the exact remainder still works, so the revert was a bound and not a fence.
        vm.prank(taker);
        settle.fillOrder(o, sig, 4e17, type(uint32).max, receiver);
    }

    function test_replayAfterFullFillReverts() public {
        IPmmSettle.Order memory o = _order();
        bytes memory sig = _sign(o);

        vm.prank(taker);
        settle.fillOrder(o, sig, TAKER_SIZE, type(uint32).max, receiver);

        vm.prank(taker);
        vm.expectRevert(abi.encodeWithSelector(PmmSettle.FillExceedsRemaining.selector, TAKER_SIZE, 0));
        settle.fillOrder(o, sig, TAKER_SIZE, type(uint32).max, receiver);
    }

    /// @notice One quote, several takers. This is what caveat 2 asked for.
    function test_oneQuoteServesSeveralTakers() public {
        IPmmSettle.Order memory o = _order();
        bytes memory sig = _sign(o);

        address[3] memory takers = [address(0x7A1E), address(0x7A2E), address(0x7A3E)];
        for (uint256 i; i < takers.length; ++i) {
            takerAsset.mint(takers[i], 3e17);
            vm.startPrank(takers[i]);
            takerAsset.approve(address(settle), 3e17);
            settle.fillOrder(o, sig, 3e17, type(uint32).max, takers[i]);
            vm.stopPrank();
            assertEq(makerAsset.balanceOf(takers[i]), (MAKER_SIZE * 3e17) / TAKER_SIZE);
        }
        assertEq(settle.remainingTaker(o), TAKER_SIZE - 9e17);
    }

    /// @notice Two orders differing only in the nonce have independent accounting, because the
    ///         key is the digest and the nonce is inside it.
    function test_ordersDifferingOnlyInNonceAccountSeparately() public {
        IPmmSettle.Order memory a = _order();
        IPmmSettle.Order memory b = _order();
        b.nonce = 2;

        vm.prank(taker);
        settle.fillOrder(a, _sign(a), TAKER_SIZE, type(uint32).max, receiver);
        assertEq(settle.remainingTaker(a), 0);
        assertEq(settle.remainingTaker(b), TAKER_SIZE);

        vm.prank(taker);
        settle.fillOrder(b, _sign(b), TAKER_SIZE, type(uint32).max, receiver);
    }

    /// @notice Splitting can never draw more maker asset than filling whole, and the shortfall is
    ///         bounded by one unit per extra piece. Same statement, same bound and same direction
    ///         as `PropCurve`'s splitting note.
    function testFuzz_splittingNeverBeatsAWholeFill(uint96 a, uint96 b, uint96 c) public {
        // Bounded below by MIN_SLICE rather than by 1. At this rate a taker leg under
        // `ceil(TAKER_SIZE / MAKER_SIZE)` buys zero maker units and is rejected outright by
        // `NothingDelivered`, which is correct behaviour and simply not what this property is
        // about; bounding is honest where a `vm.assume` on the same condition would be noise.
        uint256 s1 = bound(uint256(a), MIN_SLICE, TAKER_SIZE / 3);
        uint256 s2 = bound(uint256(b), MIN_SLICE, TAKER_SIZE / 3);
        uint256 s3 = bound(uint256(c), MIN_SLICE, TAKER_SIZE / 3);

        IPmmSettle.Order memory o = _order();
        bytes memory sig = _sign(o);

        uint256 collected;
        uint256[3] memory slices = [s1, s2, s3];
        for (uint256 i; i < 3; ++i) {
            vm.prank(taker);
            collected += settle.fillOrder(o, sig, slices[i], type(uint32).max, receiver);
        }

        uint256 whole = (MAKER_SIZE * (s1 + s2 + s3)) / TAKER_SIZE;
        assertLe(collected, whole);
        assertGe(collected + 2, whole);
        assertEq(settle.remainingTaker(o), TAKER_SIZE - s1 - s2 - s3);
    }

    /// @notice However an order is decomposed, the maker never pays out more than `makerAmount`.
    function testFuzz_totalMakerLegNeverExceedsTheOrder(uint8 pieces, uint96 seed) public {
        uint256 n = (uint256(pieces) % 8) + 1;
        IPmmSettle.Order memory o = _order();
        bytes memory sig = _sign(o);

        uint256 remaining = TAKER_SIZE;
        uint256 paid;
        for (uint256 i; i < n; ++i) {
            // Every piece is at least MIN_SLICE, and enough is held back for the pieces still to
            // come, so the decomposition always reaches exactly `TAKER_SIZE` in exactly `n` fills.
            uint256 slice = i + 1 == n
                ? remaining
                : bound(uint256(keccak256(abi.encode(seed, i))), MIN_SLICE, remaining - (n - 1 - i) * MIN_SLICE);
            vm.prank(taker);
            paid += settle.fillOrder(o, sig, slice, type(uint32).max, receiver);
            remaining -= slice;
        }
        assertEq(remaining, 0);
        assertLe(paid, MAKER_SIZE);
        assertEq(makerAsset.balanceOf(receiver), paid);
    }
}

/*//////////////////////////////////////////////////////////////////////////////
                                     DECAY
//////////////////////////////////////////////////////////////////////////////*/

contract PmmSettleDecayTest is PmmSettleBase {
    uint64 internal decayStart;

    function _decaying() internal view returns (IPmmSettle.Order memory o) {
        o = _order();
        o.decayStart = decayStart;
        o.decayPerSec = 1_000; // 0.1% per second
        o.decayCap = 50_000; // 5%
        o.expiry = decayStart + 3600;
    }

    function setUp() public override {
        super.setUp();
        decayStart = uint64(block.timestamp + 10);
    }

    function test_noDecayBeforeStart() public {
        IPmmSettle.Order memory o = _decaying();
        assertEq(settle.decayPpmAt(o, decayStart - 1), 0);

        vm.warp(decayStart - 1);
        vm.prank(taker);
        assertEq(settle.fillOrder(o, _sign(o), TAKER_SIZE, 0, receiver), MAKER_SIZE);
    }

    /// @notice Exactly at `decayStart` the decay is still zero. The boundary is `<=`, and it is
    ///         pinned because "the quote is firm until T" and "the quote is firm before T" differ
    ///         by one second that a maker cannot see in production.
    function test_noDecayExactlyAtStart() public {
        IPmmSettle.Order memory o = _decaying();
        assertEq(settle.decayPpmAt(o, decayStart), 0);

        vm.warp(decayStart);
        vm.prank(taker);
        assertEq(settle.fillOrder(o, _sign(o), TAKER_SIZE, 0, receiver), MAKER_SIZE);
    }

    function test_decayAccruesPerSecond() public {
        IPmmSettle.Order memory o = _decaying();
        assertEq(settle.decayPpmAt(o, decayStart + 1), 1_000);
        assertEq(settle.decayPpmAt(o, decayStart + 17), 17_000);

        vm.warp(decayStart + 1);
        vm.prank(taker);
        uint256 out = settle.fillOrder(o, _sign(o), TAKER_SIZE, type(uint32).max, receiver);
        assertEq(out, (MAKER_SIZE * (1e6 - 1_000)) / 1e6);
    }

    function test_decayStopsAtTheCap() public {
        IPmmSettle.Order memory o = _decaying();
        assertEq(settle.decayPpmAt(o, decayStart + 50), 50_000);
        assertEq(settle.decayPpmAt(o, decayStart + 51), 50_000);
        assertEq(settle.decayPpmAt(o, decayStart + 1_000_000), 50_000);

        vm.warp(decayStart + 600);
        vm.prank(taker);
        uint256 out = settle.fillOrder(o, _sign(o), TAKER_SIZE, type(uint32).max, receiver);
        assertEq(out, (MAKER_SIZE * (1e6 - 50_000)) / 1e6);
    }

    /// @notice `decayPpmAt` is `pure` and takes a caller-chosen timestamp, so absurd input is
    ///         reachable rather than hypothetical. It must answer, not panic — an aggregator
    ///         batching previews cannot have one order take the whole multicall down.
    function test_decayPpmAtIsTotalForAbsurdTimestamps() public view {
        IPmmSettle.Order memory o = _decaying();
        assertEq(settle.decayPpmAt(o, type(uint256).max), 50_000);
    }

    function test_anyZeroParameterDisablesTheDecay() public view {
        IPmmSettle.Order memory o = _decaying();

        IPmmSettle.Order memory noStart = o;
        noStart.decayStart = 0;
        assertEq(settle.decayPpmAt(noStart, type(uint64).max), 0);

        IPmmSettle.Order memory noRate = o;
        noRate.decayPerSec = 0;
        assertEq(settle.decayPpmAt(noRate, decayStart + 100), 0);

        IPmmSettle.Order memory noCap = o;
        noCap.decayCap = 0;
        assertEq(settle.decayPpmAt(noCap, decayStart + 100), 0);
    }

    function test_decayCapAboveTheHardCeilingReverts() public {
        IPmmSettle.Order memory o = _decaying();
        o.decayCap = 50_001;

        vm.prank(taker);
        vm.expectRevert(abi.encodeWithSelector(PmmSettle.DecayCapExceeded.selector, 50_001));
        settle.fillOrder(o, _sign(o), TAKER_SIZE, type(uint32).max, receiver);

        vm.expectRevert(abi.encodeWithSelector(PmmSettle.DecayCapExceeded.selector, 50_001));
        settle.previewFill(o, TAKER_SIZE, block.timestamp);
    }

    /// @notice The taker's half of caveat 1: bound the decay, or revert.
    function test_maxDecayPpmBoundsTheFill() public {
        IPmmSettle.Order memory o = _decaying();
        bytes memory sig = _sign(o);
        vm.warp(decayStart + 3);

        vm.prank(taker);
        vm.expectRevert(abi.encodeWithSelector(PmmSettle.DecayTooHigh.selector, 3_000, 2_999));
        settle.fillOrder(o, sig, TAKER_SIZE, 2_999, receiver);

        // Equality is accepted: the bound is a ceiling, not a strict one.
        vm.prank(taker);
        settle.fillOrder(o, sig, TAKER_SIZE, 3_000, receiver);
    }

    /// @notice The decay is a function of time only — it does not deepen as the order is
    ///         consumed, so an early fill is never made worse by a later one.
    function test_decayDoesNotDependOnHowMuchIsAlreadyFilled() public {
        IPmmSettle.Order memory o = _decaying();
        bytes memory sig = _sign(o);
        vm.warp(decayStart + 4);

        vm.prank(taker);
        uint256 first = settle.fillOrder(o, sig, 2e17, type(uint32).max, receiver);
        vm.prank(taker);
        uint256 second = settle.fillOrder(o, sig, 2e17, type(uint32).max, receiver);
        assertEq(first, second);
    }

    /// @notice Rounding goes to the maker: one `floor`, applied to the result and not the cut.
    function test_decayRoundsInTheMakersFavour() public {
        IPmmSettle.Order memory o = _decaying();
        o.makerAmount = 1_001;
        o.takerAmount = 1_000;
        bytes memory sig = _sign(o);
        vm.warp(decayStart + 1);

        // quoted = floor(1001 * 999 / 1000) = 999; realised = floor(999 * 999000 / 1e6) = 998.
        // The alternative form, `999 - floor(999 * 1000 / 1e6)`, would give 999.
        vm.prank(taker);
        assertEq(settle.fillOrder(o, sig, 999, type(uint32).max, receiver), 998);
    }

    function testFuzz_previewMatchesTheRealisedFill(uint96 slice, uint32 age) public {
        uint256 t = (uint256(slice) % TAKER_SIZE) + 1;
        uint256 at = uint256(decayStart) + (uint256(age) % 3_000);

        IPmmSettle.Order memory o = _decaying();
        bytes memory sig = _sign(o);

        (uint256 quoted, uint256 realised, uint256 decayPpm) = settle.previewFill(o, t, at);
        assertEq(quoted, (MAKER_SIZE * t) / TAKER_SIZE);
        assertEq(realised, (quoted * (1e6 - decayPpm)) / 1e6);

        vm.warp(at);
        if (realised == 0) {
            vm.prank(taker);
            vm.expectRevert(PmmSettle.NothingDelivered.selector);
            settle.fillOrder(o, sig, t, type(uint32).max, receiver);
        } else {
            vm.prank(taker);
            assertEq(settle.fillOrder(o, sig, t, type(uint32).max, receiver), realised);
        }
    }
}

/*//////////////////////////////////////////////////////////////////////////////
                       THE SETTLEMENT FLOOR — CAVEAT 3
//////////////////////////////////////////////////////////////////////////////*/

contract PmmSettleFloorTest is PmmSettleBase {
    /// @notice The regression test for caveat 3, written as the forked design's own numbers.
    ///
    /// @dev An order with a 60% floor and the maximum 5% decay. A 63% fill quotes 630 of a
    ///      1000-unit maker leg, which clears 600 — so under the forked ordering, where the floor
    ///      is checked before the decay, this fill is accepted and then delivers
    ///      `floor(630 * 0.95) = 598`, i.e. 59.8% of an order advertising a 60% minimum. That is
    ///      the "effective minimum delivery is 57%" arithmetic archi_v2 §4.6 recorded.
    ///
    ///      Checking after the decay rejects it. If someone hoists the floor above the decay, this
    ///      is the test that fails and nothing else in this repo does.
    function test_floorIsAppliedAfterTheDecayNotBefore() public {
        IPmmSettle.Order memory o = _order();
        o.makerAmount = 1_000;
        o.takerAmount = 1_000;
        o.minFillBps = 6_000;
        o.decayStart = uint64(block.timestamp);
        o.decayPerSec = 50_000;
        o.decayCap = 50_000;
        o.expiry = uint64(block.timestamp + 600);
        bytes memory sig = _sign(o);

        vm.warp(uint256(o.decayStart) + 1);
        (uint256 quoted, uint256 realised, uint256 decayPpm) = settle.previewFill(o, 630, block.timestamp);
        // The pre-decay amount clears the floor and the post-decay amount does not. Without that
        // gap the test would prove nothing about the ordering.
        assertEq(decayPpm, 50_000);
        assertEq(quoted, 630);
        assertEq(realised, 598);
        assertGe(quoted, 600);

        vm.prank(taker);
        vm.expectRevert(abi.encodeWithSelector(PmmSettle.BelowSettlementFloor.selector, 598, 600));
        settle.fillOrder(o, sig, 630, type(uint32).max, receiver);
    }

    /// @notice The same order, same size, filled before the decay starts: accepted. So the revert
    ///         above is the decay's doing and not the floor rejecting a legitimate size.
    function test_theSameFillClearsTheFloorWithNoDecay() public {
        IPmmSettle.Order memory o = _order();
        o.makerAmount = 1_000;
        o.takerAmount = 1_000;
        o.minFillBps = 6_000;
        o.decayStart = uint64(block.timestamp + 100);
        o.decayPerSec = 50_000;
        o.decayCap = 50_000;
        o.expiry = uint64(block.timestamp + 600);

        vm.prank(taker);
        assertEq(settle.fillOrder(o, _sign(o), 630, type(uint32).max, receiver), 630);
    }

    function test_floorRejectsAnUndersizedFill() public {
        IPmmSettle.Order memory o = _order();
        o.makerAmount = 1_000;
        o.takerAmount = 1_000;
        o.minFillBps = 6_000;

        vm.prank(taker);
        vm.expectRevert(abi.encodeWithSelector(PmmSettle.BelowSettlementFloor.selector, 599, 600));
        settle.fillOrder(o, _sign(o), 599, type(uint32).max, receiver);
    }

    /// @notice "All of it at the price I quoted, or nothing" — and, because the check is after
    ///         the decay, such an order genuinely becomes unfillable once the decay bites. That
    ///         is the correct reading of a 100% floor, and the ordering the fork inherited broke
    ///         it silently.
    function test_aFullSizeFloorIsUnfillableOnceTheDecayBites() public {
        IPmmSettle.Order memory o = _order();
        o.makerAmount = 1_000;
        o.takerAmount = 1_000;
        o.minFillBps = 10_000;
        o.decayStart = uint64(block.timestamp);
        o.decayPerSec = 1_000;
        o.decayCap = 50_000;
        o.expiry = uint64(block.timestamp + 600);
        bytes memory sig = _sign(o);

        vm.prank(taker);
        assertEq(settle.fillOrder(o, sig, 1_000, type(uint32).max, receiver), 1_000);

        IPmmSettle.Order memory twin = o;
        twin.nonce = 2;
        bytes memory twinSig = _sign(twin);
        vm.warp(uint256(o.decayStart) + 1);
        vm.prank(taker);
        vm.expectRevert(abi.encodeWithSelector(PmmSettle.BelowSettlementFloor.selector, 999, 1_000));
        settle.fillOrder(twin, twinSig, 1_000, type(uint32).max, receiver);
    }

    /// @notice Zero disables it, which is what a streamed quote wants — the forked design's
    ///         hard-coded 60% is precisely what made its orders one-shot.
    function test_zeroMinFillBpsPermitsDustSizedFills() public {
        IPmmSettle.Order memory o = _order();
        assertEq(o.minFillBps, 0);
        vm.prank(taker);
        settle.fillOrder(o, _sign(o), 1e12, type(uint32).max, receiver);
    }

    function test_minFillBpsAboveTenThousandReverts() public {
        IPmmSettle.Order memory o = _order();
        o.minFillBps = 10_001;
        vm.prank(taker);
        vm.expectRevert(abi.encodeWithSelector(PmmSettle.MinFillOutOfRange.selector, 10_001));
        settle.fillOrder(o, _sign(o), TAKER_SIZE, type(uint32).max, receiver);
    }
}

/*//////////////////////////////////////////////////////////////////////////////
                        THE EVENT — THE ANTI-SPOOFING RECORD
//////////////////////////////////////////////////////////////////////////////*/

contract PmmSettleEventTest is PmmSettleBase {
    function test_orderFilledRecordsQuotedNextToRealised() public {
        IPmmSettle.Order memory o = _order();
        o.decayStart = uint64(block.timestamp);
        o.decayPerSec = 2_000;
        o.decayCap = 50_000;
        o.expiry = uint64(block.timestamp + 600);
        bytes memory sig = _sign(o);

        uint256 slice = 4e17;
        vm.warp(uint256(o.decayStart) + 6);

        uint256 quoted = (MAKER_SIZE * slice) / TAKER_SIZE;
        uint256 realised = (quoted * (1e6 - 12_000)) / 1e6;
        // The gap is the whole point of the event: if these were equal the assertion would be
        // vacuous, so the test insists there is something to record.
        assertGt(quoted, realised);

        vm.expectEmit(true, true, true, true, address(settle));
        emit IPmmSettle.OrderFilled(
            settle.hashOrder(o),
            maker,
            receiver,
            address(makerAsset),
            address(takerAsset),
            slice,
            quoted,
            realised,
            12_000,
            TAKER_SIZE - slice
        );

        vm.prank(taker);
        assertEq(settle.fillOrder(o, sig, slice, type(uint32).max, receiver), realised);
    }

    /// @notice With no decay the two fields are equal, which is the baseline an indexer
    ///         differences against.
    function test_quotedEqualsRealisedWhenNothingDecayed() public {
        IPmmSettle.Order memory o = _order();

        vm.expectEmit(true, true, true, true, address(settle));
        emit IPmmSettle.OrderFilled(
            settle.hashOrder(o),
            maker,
            receiver,
            address(makerAsset),
            address(takerAsset),
            TAKER_SIZE,
            MAKER_SIZE,
            MAKER_SIZE,
            0,
            0
        );

        vm.prank(taker);
        settle.fillOrder(o, _sign(o), TAKER_SIZE, type(uint32).max, receiver);
    }

    function test_cancellationIsPublished() public {
        vm.expectEmit(true, true, true, true, address(settle));
        emit IPmmSettle.NoncesCancelled(maker, 0, 1 << 5);
        vm.prank(maker);
        settle.cancelNonce(5);
    }
}

/*//////////////////////////////////////////////////////////////////////////////
                         SHAPE VALIDATION & BAD TOKENS
//////////////////////////////////////////////////////////////////////////////*/

contract PmmSettleGuardTest is PmmSettleBase {
    function test_zeroReceiverReverts() public {
        IPmmSettle.Order memory o = _order();
        vm.prank(taker);
        vm.expectRevert(PmmSettle.ZeroReceiver.selector);
        settle.fillOrder(o, _sign(o), TAKER_SIZE, type(uint32).max, address(0));
    }

    function test_zeroFillReverts() public {
        IPmmSettle.Order memory o = _order();
        vm.prank(taker);
        vm.expectRevert(PmmSettle.ZeroFill.selector);
        settle.fillOrder(o, _sign(o), 0, type(uint32).max, receiver);
    }

    function test_shapeViolationsRevert() public {
        IPmmSettle.Order memory o;

        o = _order();
        o.maker = address(0);
        _expectShapeRevert(o, PmmSettle.ZeroMaker.selector);

        o = _order();
        o.makerAsset = address(0);
        _expectShapeRevert(o, PmmSettle.ZeroAsset.selector);

        o = _order();
        o.takerAsset = address(0);
        _expectShapeRevert(o, PmmSettle.ZeroAsset.selector);

        o = _order();
        o.takerAsset = o.makerAsset;
        _expectShapeRevert(o, PmmSettle.IdenticalAssets.selector);

        o = _order();
        o.makerAmount = 0;
        _expectShapeRevert(o, PmmSettle.ZeroOrderAmount.selector);

        o = _order();
        o.takerAmount = 0;
        _expectShapeRevert(o, PmmSettle.ZeroOrderAmount.selector);

        o = _order();
        o.makerAmount = uint256(type(uint128).max) + 1;
        _expectShapeRevert(o, PmmSettle.AmountOutOfDomain.selector);

        o = _order();
        o.takerAmount = uint256(type(uint128).max) + 1;
        _expectShapeRevert(o, PmmSettle.AmountOutOfDomain.selector);
    }

    /// @notice The preview and the fill must agree on which orders exist, or a taker is shown a
    ///         price no transaction can realise.
    function test_previewRejectsExactlyWhatTheFillRejects() public {
        IPmmSettle.Order memory o = _order();
        o.makerAsset = o.takerAsset;

        vm.expectRevert(PmmSettle.IdenticalAssets.selector);
        settle.previewFill(o, 1, block.timestamp);

        vm.prank(taker);
        vm.expectRevert(PmmSettle.IdenticalAssets.selector);
        settle.fillOrder(o, _sign(o), 1, type(uint32).max, receiver);
    }

    function test_previewRejectsAnOutOfDomainSize() public {
        IPmmSettle.Order memory o = _order();
        vm.expectRevert(PmmSettle.AmountOutOfDomain.selector);
        settle.previewFill(o, uint256(type(uint128).max) + 1, block.timestamp);
    }

    /// @notice A fill too small to buy one unit of the maker leg takes the taker's tokens and
    ///         delivers nothing. It must revert instead.
    function test_aFillThatDeliversNothingReverts() public {
        IPmmSettle.Order memory o = _order();
        o.makerAmount = 1;
        o.takerAmount = 1e18;

        vm.prank(taker);
        vm.expectRevert(PmmSettle.NothingDelivered.selector);
        settle.fillOrder(o, _sign(o), 1e17, type(uint32).max, receiver);
    }

    function test_makerWithoutAllowanceCannotSettle() public {
        vm.prank(maker);
        makerAsset.approve(address(settle), 0);

        IPmmSettle.Order memory o = _order();
        vm.prank(taker);
        vm.expectRevert(MockERC20.InsufficientAllowance.selector);
        settle.fillOrder(o, _sign(o), TAKER_SIZE, type(uint32).max, receiver);
    }

    // ---------------------------------------------------------------------
    // Tokens
    // ---------------------------------------------------------------------

    /// @notice A token that returns nothing settles, because `SafeTransfer` reads empty
    ///         returndata as success.
    function test_usdtShapedTokenSettles() public {
        NoReturnToken quiet = new NoReturnToken();
        quiet.mint(maker, 1_000e18);
        vm.prank(maker);
        quiet.approve(address(settle), type(uint256).max);

        IPmmSettle.Order memory o = _order();
        o.makerAsset = address(quiet);
        o.makerAmount = 500e18;

        vm.prank(taker);
        settle.fillOrder(o, _sign(o), TAKER_SIZE, type(uint32).max, receiver);
        assertEq(quiet.balanceOf(receiver), 500e18);
    }

    /// @notice A token that returns `false` instead of reverting must not be read as success.
    function test_tokenReturningFalseReverts() public {
        FalseReturnToken liar = new FalseReturnToken();
        liar.mint(maker, 1_000e18);

        IPmmSettle.Order memory o = _order();
        o.makerAsset = address(liar);
        o.makerAmount = 500e18;

        vm.prank(taker);
        vm.expectRevert(bytes4(keccak256("TransferFromFailed()")));
        settle.fillOrder(o, _sign(o), TAKER_SIZE, type(uint32).max, receiver);
    }

    /// @notice An address with no code answers every call successfully with empty returndata.
    ///         Without the `extcodesize` check that would read as a settled transfer.
    function test_codelessAssetReverts() public {
        IPmmSettle.Order memory o = _order();
        o.makerAsset = address(0xC0DE1E55);

        vm.prank(taker);
        vm.expectRevert(bytes4(keccak256("TransferFromFailed()")));
        settle.fillOrder(o, _sign(o), TAKER_SIZE, type(uint32).max, receiver);
    }

    // ---------------------------------------------------------------------
    // Reentrancy
    // ---------------------------------------------------------------------

    function test_reentrancyThroughTheTakerAssetReverts() public {
        ReentrantToken evil = new ReentrantToken();
        evil.mint(taker, 100e18);
        vm.prank(taker);
        evil.approve(address(settle), type(uint256).max);

        IPmmSettle.Order memory o = _order();
        o.takerAsset = address(evil);
        bytes memory sig = _sign(o);
        evil.arm(settle, o, sig);

        vm.prank(taker);
        vm.expectRevert(PmmSettle.Reentrancy.selector);
        settle.fillOrder(o, sig, TAKER_SIZE, type(uint32).max, receiver);
    }

    function test_reentrancyThroughTheMakerAssetReverts() public {
        ReentrantToken evil = new ReentrantToken();
        evil.mint(maker, 1_000e18);
        vm.prank(maker);
        evil.approve(address(settle), type(uint256).max);

        IPmmSettle.Order memory o = _order();
        o.makerAsset = address(evil);
        o.makerAmount = 500e18;
        bytes memory sig = _sign(o);
        evil.arm(settle, o, sig);

        vm.prank(taker);
        vm.expectRevert(PmmSettle.Reentrancy.selector);
        settle.fillOrder(o, sig, TAKER_SIZE, type(uint32).max, receiver);
    }

    /// @notice Disarmed, the same token settles — so the revert above is the guard and not the
    ///         token simply being broken.
    function test_theSameTokenSettlesWhenItDoesNotReenter() public {
        ReentrantToken quiet = new ReentrantToken();
        quiet.mint(maker, 1_000e18);
        vm.prank(maker);
        quiet.approve(address(settle), type(uint256).max);

        IPmmSettle.Order memory o = _order();
        o.makerAsset = address(quiet);
        o.makerAmount = 500e18;

        vm.prank(taker);
        settle.fillOrder(o, _sign(o), TAKER_SIZE, type(uint32).max, receiver);
        assertEq(quiet.balanceOf(receiver), 500e18);
    }

    function _expectShapeRevert(IPmmSettle.Order memory o, bytes4 selector) private {
        vm.prank(taker);
        vm.expectRevert(selector);
        settle.fillOrder(o, _sign(o), 1e17, type(uint32).max, receiver);
    }
}
