// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {IPmmSettle} from "./interfaces/IPmmSettle.sol";
import {SafeTransfer} from "./libraries/SafeTransfer.sol";

contract PmmSettle is IPmmSettle {

    error ZeroReceiver();
    error ZeroMaker();
    error ZeroAsset();
    error IdenticalAssets();
    error ZeroOrderAmount();
    error AmountOutOfDomain();
    error DecayCapExceeded(uint256 decayCap);
    error MinFillOutOfRange(uint256 minFillBps);
    error ZeroFill();
    error OrderExpired(uint256 expiry, uint256 nowTs);
    error NonceCancelled(address maker, uint64 nonce);
    error BadSignature();
    error BadSignatureLength(uint256 length);
    error MalleableSignature();
    error FillExceedsRemaining(uint256 requested, uint256 remaining);
    error DecayTooHigh(uint256 decayPpm, uint256 maxDecayPpm);
    error BelowSettlementFloor(uint256 makerAmountOut, uint256 floorAmount);
    error NothingDelivered();
    error Reentrancy();

    uint256 public constant DECAY_DENOMINATOR = 1e6;

    uint256 public constant MAX_DECAY_CAP = 50_000;

    uint256 public constant BPS_DENOMINATOR = 10_000;

    uint256 public constant MAX_AMOUNT = type(uint128).max;

    bytes32 public constant ORDER_TYPEHASH = keccak256(
        "Order("
            "address maker,"
            "address makerAsset,"
            "address takerAsset,"
            "uint256 makerAmount,"
            "uint256 takerAmount,"
            "uint64 nonce,"
            "uint64 expiry,"
            "uint64 decayStart,"
            "uint32 decayPerSec,"
            "uint32 decayCap,"
            "uint16 minFillBps"
        ")"
    );

    bytes32 private constant _EIP712_DOMAIN_TYPEHASH =
        keccak256("EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)");

    bytes32 private constant _NAME_HASH = keccak256("DuBu PmmSettle");
    bytes32 private constant _VERSION_HASH = keccak256("1");

    bytes32 private constant _HALF_CURVE_ORDER = 0x7FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF5D576E7357A4501DDFE92F46681B20A0;

    mapping(bytes32 => uint256) public override filledTaker;

    mapping(address => mapping(uint256 => uint256)) private _cancelledNonces;

    uint256 private immutable _CACHED_CHAIN_ID;
    bytes32 private immutable _CACHED_DOMAIN_SEPARATOR;

    bool private transient _entered;

    modifier nonReentrant() {
        _lock();
        _;
        _unlock();
    }

    function _lock() private {
        if (_entered) revert Reentrancy();
        _entered = true;
    }

    function _unlock() private {
        _entered = false;
    }

    constructor() {
        _CACHED_CHAIN_ID = block.chainid;
        _CACHED_DOMAIN_SEPARATOR = _computeDomainSeparator();
    }

    function DOMAIN_SEPARATOR() public view override returns (bytes32) {
        return block.chainid == _CACHED_CHAIN_ID ? _CACHED_DOMAIN_SEPARATOR : _computeDomainSeparator();
    }

    function hashOrder(Order calldata order) public view override returns (bytes32) {
        return keccak256(abi.encodePacked("\x19\x01", DOMAIN_SEPARATOR(), _structHash(order)));
    }

    function _computeDomainSeparator() private view returns (bytes32) {
        return keccak256(abi.encode(_EIP712_DOMAIN_TYPEHASH, _NAME_HASH, _VERSION_HASH, block.chainid, address(this)));
    }

    function _structHash(Order calldata order) private pure returns (bytes32) {
        return keccak256(
            abi.encode(
                ORDER_TYPEHASH,
                order.maker,
                order.makerAsset,
                order.takerAsset,
                order.makerAmount,
                order.takerAmount,
                order.nonce,
                order.expiry,
                order.decayStart,
                order.decayPerSec,
                order.decayCap,
                order.minFillBps
            )
        );
    }

    function decayPpmAt(Order calldata order, uint256 atTimestamp) public pure override returns (uint256) {
        uint256 start = order.decayStart;
        uint256 rate = order.decayPerSec;
        uint256 cap = order.decayCap;

        if (start == 0 || rate == 0 || cap == 0) return 0;
        if (atTimestamp <= start) return 0;

        uint256 elapsed = atTimestamp - start;

        if (elapsed >= cap) return cap;

        uint256 accrued = elapsed * rate;
        return accrued > cap ? cap : accrued;
    }

    function previewFill(Order calldata order, uint256 takerAmountIn, uint256 atTimestamp)
        public
        pure
        override
        returns (uint256 quotedMakerOut, uint256 realisedMakerOut, uint256 decayPpm)
    {
        _checkOrderShape(order);
        if (takerAmountIn > MAX_AMOUNT) revert AmountOutOfDomain();

        quotedMakerOut = (order.makerAmount * takerAmountIn) / order.takerAmount;
        decayPpm = decayPpmAt(order, atTimestamp);
        realisedMakerOut = (quotedMakerOut * (DECAY_DENOMINATOR - decayPpm)) / DECAY_DENOMINATOR;
    }

    function remainingTaker(Order calldata order) external view override returns (uint256) {

        return order.takerAmount - filledTaker[hashOrder(order)];
    }

    function isNonceCancelled(address maker, uint64 nonce) public view override returns (bool) {

        return _cancelledNonces[maker][nonce >> 8] & (1 << (nonce & 0xff)) != 0;
    }

    function cancelNonce(uint64 nonce) external override {

        _cancel(nonce >> 8, 1 << (nonce & 0xff));
    }

    function cancelNonceSlot(uint256 slot, uint256 mask) external override {
        _cancel(slot, mask);
    }

    function _cancel(uint256 slot, uint256 mask) private {
        _cancelledNonces[msg.sender][slot] |= mask;
        emit NoncesCancelled(msg.sender, slot, mask);
    }

    function fillOrder(
        Order calldata order,
        bytes calldata signature,
        uint256 takerAmountIn,
        uint32 maxDecayPpm,
        address receiver
    ) external override nonReentrant returns (uint256 makerAmountOut) {
        if (receiver == address(0)) revert ZeroReceiver();
        if (takerAmountIn == 0) revert ZeroFill();
        _checkOrderShape(order);

        if (block.timestamp > order.expiry) revert OrderExpired(order.expiry, block.timestamp);
        if (isNonceCancelled(order.maker, order.nonce)) revert NonceCancelled(order.maker, order.nonce);

        bytes32 orderHash = hashOrder(order);
        _verifySignature(order.maker, orderHash, signature);

        uint256 remaining = _consume(orderHash, order.takerAmount, takerAmountIn);

        uint256 quoted;
        uint256 decayPpm;
        (quoted, makerAmountOut, decayPpm) = _price(order, takerAmountIn, maxDecayPpm);

        SafeTransfer.safeTransferFrom(order.takerAsset, msg.sender, order.maker, takerAmountIn);
        SafeTransfer.safeTransferFrom(order.makerAsset, order.maker, receiver, makerAmountOut);

        _emitFill(order, orderHash, receiver, takerAmountIn, quoted, makerAmountOut, decayPpm, remaining);
    }

    function _consume(bytes32 orderHash, uint256 takerAmount, uint256 takerAmountIn)
        private
        returns (uint256 remaining)
    {
        uint256 filled = filledTaker[orderHash];
        remaining = takerAmount - filled;

        if (takerAmountIn > remaining) revert FillExceedsRemaining(takerAmountIn, remaining);
        filledTaker[orderHash] = filled + takerAmountIn;
        remaining = remaining - takerAmountIn;
    }

    function _price(Order calldata order, uint256 takerAmountIn, uint32 maxDecayPpm)
        private
        view
        returns (uint256 quoted, uint256 makerAmountOut, uint256 decayPpm)
    {

        quoted = (order.makerAmount * takerAmountIn) / order.takerAmount;

        decayPpm = decayPpmAt(order, block.timestamp);
        if (decayPpm > maxDecayPpm) revert DecayTooHigh(decayPpm, maxDecayPpm);

        makerAmountOut = (quoted * (DECAY_DENOMINATOR - decayPpm)) / DECAY_DENOMINATOR;

        if (makerAmountOut == 0) revert NothingDelivered();

        if (order.minFillBps != 0) {
            uint256 floorAmount = (order.makerAmount * order.minFillBps) / BPS_DENOMINATOR;
            if (makerAmountOut < floorAmount) revert BelowSettlementFloor(makerAmountOut, floorAmount);
        }
    }

    function _emitFill(
        Order calldata order,
        bytes32 orderHash,
        address receiver,
        uint256 takerAmountIn,
        uint256 quoted,
        uint256 makerAmountOut,
        uint256 decayPpm,
        uint256 remaining
    ) private {
        emit OrderFilled(
            orderHash,
            order.maker,
            receiver,
            order.makerAsset,
            order.takerAsset,
            takerAmountIn,
            quoted,
            makerAmountOut,
            decayPpm,
            remaining
        );
    }

    function _checkOrderShape(Order calldata order) private pure {
        if (order.maker == address(0)) revert ZeroMaker();
        if (order.makerAsset == address(0) || order.takerAsset == address(0)) revert ZeroAsset();

        if (order.makerAsset == order.takerAsset) revert IdenticalAssets();
        if (order.makerAmount == 0 || order.takerAmount == 0) revert ZeroOrderAmount();
        if (order.makerAmount > MAX_AMOUNT || order.takerAmount > MAX_AMOUNT) revert AmountOutOfDomain();
        if (order.decayCap > MAX_DECAY_CAP) revert DecayCapExceeded(order.decayCap);
        if (order.minFillBps > BPS_DENOMINATOR) revert MinFillOutOfRange(order.minFillBps);
    }

    function _verifySignature(address maker, bytes32 digest, bytes calldata signature) private pure {
        if (signature.length != 65) revert BadSignatureLength(signature.length);

        bytes32 r = bytes32(signature[0:32]);
        bytes32 s = bytes32(signature[32:64]);
        uint8 v = uint8(signature[64]);

        if (uint256(s) > uint256(_HALF_CURVE_ORDER)) revert MalleableSignature();

        if (v != 27 && v != 28) revert MalleableSignature();

        address recovered = ecrecover(digest, v, r, s);
        if (recovered == address(0) || recovered != maker) revert BadSignature();
    }
}
