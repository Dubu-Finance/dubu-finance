// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {IPropPool} from "./interfaces/IPropPool.sol";
import {IPyth, PythStructs} from "./interfaces/IPyth.sol";
import {PropCurve} from "./libraries/PropCurve.sol";
import {ReentrancyLock} from "./libraries/ReentrancyLock.sol";
import {SafeTransfer} from "./libraries/SafeTransfer.sol";

interface IERC20Balance {
    function balanceOf(address account) external view returns (uint256);
}

contract PropPool is IPropPool {
    using SafeTransfer for address;

    error NotOwner();
    error NotManager();
    error NotUpdater();
    error NotGuardian();
    error ZeroAddress();
    error IdenticalTokens();
    error PairExists();
    error UnknownPair();
    error PriceScaleTooLarge();
    error ZeroMinPrice();
    error ZeroStaleWindow();
    error MinPriceStrandsQuote();
    error DeadlineExpired();
    error ZeroAmount();
    error AmountOverflow();
    error PoolPaused();
    error StaleQuote();
    error InsufficientCapacity();
    error ReserveFloorBreached();
    error SlippageExceeded();
    error ZeroOutput();
    error LengthMismatch();
    error CapacityOutOfDomain();
    error PythNotSet();
    error DeviationTooLarge();
    error ZeroPythStaleWindow();

    error BidCeilingExceeded(uint16 pairId, uint256 referencePrice, uint256 maxBid);

    error AskFloorBreached(uint16 pairId, uint256 referencePrice, uint256 minAsk);

    error ReferenceUnavailable(uint16 pairId, uint8 status);

    event OwnershipTransferStarted(address indexed previousOwner, address indexed newOwner);
    event OwnershipTransferred(address indexed previousOwner, address indexed newOwner);
    event ManagerUpdated(address indexed previousManager, address indexed newManager);
    event UpdaterUpdated(address indexed previousUpdater, address indexed newUpdater);
    event GuardianUpdated(address indexed previousGuardian, address indexed newGuardian);
    event PairConfigUpdated(
        uint16 indexed pairId, uint32 maxStaleSecs, uint56 minPrice, uint96 minBaseReserve, uint96 minQuoteReserve
    );
    event ReserveSynced(address indexed token, uint256 reserve);
    event PythUpdated(address indexed previousPyth, address indexed newPyth);
    event PairOracleUpdated(
        uint16 indexed pairId, bytes32 feedId, uint16 maxDeviationBps, uint32 maxPythStaleSecs, int8 refExpo
    );

    event PairDecayUpdated(uint16 indexed pairId, uint16 decaySecs);

    uint256 private constant MASK_16 = 0xffff;
    uint256 private constant MASK_32 = 0xffffffff;
    uint256 private constant MASK_56 = 0xffffffffffffff;
    uint256 private constant MASK_96 = 0xffffffffffffffffffffffff;

    uint256 private constant MAX_PRICE = MASK_56;

    uint256 private constant LADDER_MASK = (uint256(1) << 224) - 1;

    uint256 private constant SHIFT_MAX_BID = 56;
    uint256 private constant SHIFT_MIN_ASK = 112;
    uint256 private constant SHIFT_MAX_ASK = 168;
    uint256 private constant SHIFT_UPDATED_AT = 224;

    uint256 private constant SHIFT_ASK_CAPACITY = 96;
    uint256 private constant SHIFT_GEN = 192;
    uint256 private constant SHIFT_FLAGS = 224;

    uint256 private constant SHIFT_DECAY = 240;

    uint256 private constant CAP_WORD_PRESERVED = ~((uint256(1) << SHIFT_FLAGS) - 1);

    uint256 private constant FLAG_PAUSED = uint256(1) << SHIFT_FLAGS;

    uint16 internal constant FLAG_SNAPSHOT_BOUNDED = uint16(1) << 15;

    uint16 internal constant FLAG_SNAPSHOT_DECAYING = uint16(1) << 14;

    uint256 private constant ROUTE_IS_BID = uint256(1) << 16;

    struct PairConfig {
        address base;
        address quote;
        uint8 priceScaleExp;
        uint32 maxStaleSecs;
        uint56 minPrice;
        uint96 minBaseReserve;
        uint96 minQuoteReserve;
        bool exists;
    }

    struct PairState {
        address tokenOut;
        uint8 priceScaleExp;
        uint256 pLow;
        uint256 pHigh;
        uint256 capacity;
        uint256 available;
        uint256 used;
        uint256 reserveOut;
        uint256 minReserveOut;
    }

    struct PairOracle {

        bytes32 feedId;

        uint16 maxDeviationBps;

        uint32 maxPythStaleSecs;

        int8 refExpo;
    }

    uint256 private constant STATUS_OK = 0;
    uint256 private constant STATUS_UNKNOWN = 1;
    uint256 private constant STATUS_PAUSED = 2;
    uint256 private constant STATUS_STALE = 3;

    uint256 private constant BPS = 10_000;

    uint8 public constant REF_OK = 0;

    uint8 public constant REF_DISABLED = 1;

    uint8 public constant REF_UNAVAILABLE = 2;

    uint8 public constant REF_STALE = 3;

    uint8 public constant REF_INVALID = 4;

    uint256 private constant MAX_REFERENCE_PRICE = type(uint64).max;

    int256 private constant MAX_NET_EXPONENT = 58;

    address public owner;

    uint16 public pairCount;

    address public pendingOwner;
    address public manager;
    address public updater;

    address public guardian;

    bool public allPaused;

    address public pyth;

    mapping(uint16 => PairConfig) private _config;
    mapping(uint16 => PairOracle) private _oracle;
    mapping(uint16 => uint256) private _quoteWord;
    mapping(uint16 => uint256) private _capacityWord;
    mapping(uint16 => uint256) private _usedWord;

    mapping(address => mapping(address => uint256)) private _route;

    mapping(address => uint256) private _reserve;

    modifier onlyOwner() {
        if (msg.sender != owner) revert NotOwner();
        _;
    }

    modifier onlyManager() {
        if (msg.sender != manager) revert NotManager();
        _;
    }

    modifier onlyUpdater() {
        if (msg.sender != updater) revert NotUpdater();
        _;
    }

    modifier onlyGuardian() {
        if (msg.sender != guardian) revert NotGuardian();
        _;
    }

    modifier locked() {
        ReentrancyLock.acquire();
        _;
        ReentrancyLock.release();
    }

    constructor(address owner_, address manager_, address updater_, address guardian_) {
        if (owner_ == address(0) || manager_ == address(0) || updater_ == address(0) || guardian_ == address(0)) {
            revert ZeroAddress();
        }
        owner = owner_;
        manager = manager_;
        updater = updater_;
        guardian = guardian_;
        emit OwnershipTransferred(address(0), owner_);
        emit ManagerUpdated(address(0), manager_);
        emit UpdaterUpdated(address(0), updater_);
        emit GuardianUpdated(address(0), guardian_);
    }

    function transferOwnership(address newOwner) external onlyOwner {
        pendingOwner = newOwner;
        emit OwnershipTransferStarted(owner, newOwner);
    }

    function acceptOwnership() external {
        address pending = pendingOwner;
        if (msg.sender != pending) revert NotOwner();
        emit OwnershipTransferred(owner, pending);
        owner = pending;
        pendingOwner = address(0);
    }

    function setManager(address newManager) external onlyOwner {
        if (newManager == address(0)) revert ZeroAddress();
        emit ManagerUpdated(manager, newManager);
        manager = newManager;
    }

    function setUpdater(address newUpdater) external onlyOwner {
        if (newUpdater == address(0)) revert ZeroAddress();
        emit UpdaterUpdated(updater, newUpdater);
        updater = newUpdater;
    }

    function setGuardian(address newGuardian) external onlyOwner {
        if (newGuardian == address(0)) revert ZeroAddress();
        emit GuardianUpdated(guardian, newGuardian);
        guardian = newGuardian;
    }

    function addPair(address base, address quote, uint8 priceScaleExp, uint32 maxStaleSecs, uint56 minPrice)
        external
        onlyOwner
        returns (uint16 pairId)
    {
        if (base == address(0) || quote == address(0)) revert ZeroAddress();
        if (base == quote) revert IdenticalTokens();
        if (priceScaleExp > PropCurve.MAX_PRICE_SCALE_EXP) revert PriceScaleTooLarge();
        if (minPrice == 0) revert ZeroMinPrice();
        if (maxStaleSecs == 0) revert ZeroStaleWindow();
        if (_route[base][quote] != 0 || _route[quote][base] != 0) revert PairExists();

        pairId = ++pairCount;

        _config[pairId] = PairConfig({
            base: base,
            quote: quote,
            priceScaleExp: priceScaleExp,
            maxStaleSecs: maxStaleSecs,
            minPrice: minPrice,
            minBaseReserve: 0,
            minQuoteReserve: 0,
            exists: true
        });

        _route[base][quote] = uint256(pairId) | ROUTE_IS_BID;
        _route[quote][base] = uint256(pairId);

        emit PairAdded(pairId, base, quote);
    }

    function setPairConfig(
        uint16 pairId,
        uint32 maxStaleSecs,
        uint56 minPrice,
        uint96 minBaseReserve,
        uint96 minQuoteReserve
    ) external onlyManager {
        if (pairId == 0 || pairId > pairCount) revert UnknownPair();
        if (minPrice == 0) revert ZeroMinPrice();
        if (maxStaleSecs == 0) revert ZeroStaleWindow();

        uint256 word = _quoteWord[pairId];

        if (word != 0 && (word & MASK_56) < minPrice) revert MinPriceStrandsQuote();

        PairConfig storage cfg = _config[pairId];
        cfg.maxStaleSecs = maxStaleSecs;
        cfg.minPrice = minPrice;
        cfg.minBaseReserve = minBaseReserve;
        cfg.minQuoteReserve = minQuoteReserve;

        emit PairConfigUpdated(pairId, maxStaleSecs, minPrice, minBaseReserve, minQuoteReserve);
    }

    function setPairDecay(uint16 pairId, uint16 decaySecs) external onlyManager {
        if (pairId == 0 || pairId > pairCount) revert UnknownPair();

        uint256 word = _capacityWord[pairId];
        _capacityWord[pairId] = (word & ~(MASK_16 << SHIFT_DECAY)) | (uint256(decaySecs) << SHIFT_DECAY);

        emit PairDecayUpdated(pairId, decaySecs);
    }

    function setPyth(address newPyth) external onlyOwner {
        if (newPyth == address(0)) revert ZeroAddress();
        emit PythUpdated(pyth, newPyth);
        pyth = newPyth;
    }

    function setPairOracle(uint16 pairId, bytes32 feedId, uint16 maxDeviationBps, uint32 maxPythStaleSecs, int8 refExpo)
        external
        onlyManager
    {
        if (pairId == 0 || pairId > pairCount) revert UnknownPair();

        if (feedId != bytes32(0)) {
            if (pyth == address(0)) revert PythNotSet();

            if (maxDeviationBps > BPS) revert DeviationTooLarge();

            if (maxPythStaleSecs == 0) revert ZeroPythStaleWindow();
        }

        _oracle[pairId] = PairOracle({
            feedId: feedId, maxDeviationBps: maxDeviationBps, maxPythStaleSecs: maxPythStaleSecs, refExpo: refExpo
        });

        emit PairOracleUpdated(pairId, feedId, maxDeviationBps, maxPythStaleSecs, refExpo);
    }

    function pairOracle(uint16 pairId) external view returns (PairOracle memory) {
        return _oracle[pairId];
    }

    function referencePrice(uint16 pairId) external view returns (uint256 price, uint8 status) {
        return _referencePrice(pairId);
    }

    function updateQuote(uint256[] calldata packed) external onlyUpdater {
        uint256 n = pairCount;
        uint256 stamp = uint256(uint32(block.timestamp)) << SHIFT_UPDATED_AT;
        uint256 len = packed.length;

        for (uint256 i; i < len;) {
            uint256 word = packed[i];
            uint256 pairId = (word >> SHIFT_UPDATED_AT) & MASK_16;
            if (pairId == 0 || pairId > n) revert UnknownPair();

            uint256 minBid = word & MASK_56;
            uint256 maxBid = (word >> SHIFT_MAX_BID) & MASK_56;
            uint256 minAsk = (word >> SHIFT_MIN_ASK) & MASK_56;
            uint256 maxAsk = (word >> SHIFT_MAX_ASK) & MASK_56;

            PropCurve.validateLadder(minBid, maxBid, minAsk, maxAsk, _config[uint16(pairId)].minPrice);
            _checkReferenceBound(uint16(pairId), maxBid, minAsk);

            _quoteWord[uint16(pairId)] = (word & LADDER_MASK) | stamp;

            emit QuoteUpdated(uint16(pairId), uint56(minBid), uint56(maxBid), uint56(minAsk), uint56(maxAsk));

            unchecked {
                ++i;
            }
        }
    }

    function refreshCapacity(uint16 pairId, uint96 bidCapacity, uint96 askCapacity) external onlyUpdater {
        if (pairId == 0 || pairId > pairCount) revert UnknownPair();
        _refreshCapacity(pairId, bidCapacity, askCapacity);
    }

    function refreshCapacityBatch(uint256[] calldata packed) external onlyUpdater {
        uint256 n = pairCount;
        uint256 len = packed.length;
        for (uint256 i; i < len;) {
            uint256 word = packed[i];
            uint256 pairId = (word >> SHIFT_FLAGS) & MASK_16;
            if (pairId == 0 || pairId > n) revert UnknownPair();
            _refreshCapacity(uint16(pairId), uint96(word & MASK_96), uint96((word >> SHIFT_ASK_CAPACITY) & MASK_96));
            unchecked {
                ++i;
            }
        }
    }

    function _refreshCapacity(uint16 pairId, uint96 bidCapacity, uint96 askCapacity) private {
        uint256 largest = bidCapacity > askCapacity ? bidCapacity : askCapacity;

        if (largest * MAX_PRICE > PropCurve.MAX_AMOUNT_OUT * (10 ** uint256(_config[pairId].priceScaleExp))) {
            revert CapacityOutOfDomain();
        }

        uint256 word = _capacityWord[pairId];
        uint32 gen;
        unchecked {
            gen = uint32(((word >> SHIFT_GEN) & MASK_32) + 1);
        }
        _capacityWord[pairId] = uint256(bidCapacity) | (uint256(askCapacity) << SHIFT_ASK_CAPACITY)
            | (uint256(gen) << SHIFT_GEN) | (word & CAP_WORD_PRESERVED);

        emit CapacityRefreshed(pairId, bidCapacity, askCapacity, gen);
    }

    function pause(uint16 pairId) external onlyGuardian {
        _setPaused(pairId, true);
    }

    function unpause(uint16 pairId) external onlyGuardian {
        _setPaused(pairId, false);
    }

    function pauseAll() external onlyGuardian {
        allPaused = true;
        emit Paused(0, true);
    }

    function unpauseAll() external onlyGuardian {
        allPaused = false;
        emit Paused(0, false);
    }

    function _setPaused(uint16 pairId, bool paused) private {
        if (pairId == 0 || pairId > pairCount) revert UnknownPair();
        uint256 word = _capacityWord[pairId];
        _capacityWord[pairId] = paused ? (word | FLAG_PAUSED) : (word & ~FLAG_PAUSED);
        emit Paused(pairId, paused);
    }

    function deposit(address token, uint256 amount) external onlyManager locked {
        token.safeTransferFrom(msg.sender, address(this), amount);
        uint256 reserve = _reserve[token] + amount;
        _reserve[token] = reserve;
        emit ReserveSynced(token, reserve);
    }

    function withdraw(address token, uint256 amount, address to) external onlyOwner locked {
        if (to == address(0)) revert ZeroAddress();
        _reserve[token] -= amount;
        token.safeTransfer(to, amount);
        emit ReserveSynced(token, _reserve[token]);
    }

    function sync(address token) external onlyManager {
        uint256 balance = IERC20Balance(token).balanceOf(address(this));
        _reserve[token] = balance;
        emit ReserveSynced(token, balance);
    }

    function reserveOf(address token) external view returns (uint256) {
        return _reserve[token];
    }

    function pairConfig(uint16 pairId) external view returns (PairConfig memory) {
        return _config[pairId];
    }

    function getAmountOut(address tokenIn, address tokenOut, uint256 amountIn)
        external
        view
        returns (uint256 amountOut)
    {
        (uint16 pairId, bool isBid) = pairIdFor(tokenIn, tokenOut);
        return quoteByPair(pairId, isBid, amountIn);
    }

    function quoteByPair(uint16 pairId, bool isBid, uint256 amountIn) public view returns (uint256 amountOut) {
        (uint256 status, PairState memory st) = _load(pairId, isBid);
        if (status != STATUS_OK) return 0;

        amountOut = _outFor(st, isBid, amountIn);
        if (amountOut == 0) return 0;
        if (_breachesFloor(st, amountOut)) return 0;
    }

    function getAmountIn(address tokenIn, address tokenOut, uint256 amountOut)
        external
        view
        returns (uint256 amountIn)
    {
        (uint16 pairId, bool isBid) = pairIdFor(tokenIn, tokenOut);
        (uint256 status, PairState memory st) = _load(pairId, isBid);
        if (status != STATUS_OK) return 0;
        if (amountOut == 0) return 0;
        if (_breachesFloor(st, amountOut)) return 0;
        return _inFor(st, isBid, amountOut);
    }

    function snapshot(uint16 pairId) external view returns (PairSnapshot memory s) {
        if (pairId == 0 || pairId > pairCount) return s;

        uint256 q = _quoteWord[pairId];
        uint256 c = _capacityWord[pairId];
        uint256 u = _usedWord[pairId];
        PairConfig storage cfg = _config[pairId];

        s.minBid = uint56(q & MASK_56);
        s.maxBid = uint56((q >> SHIFT_MAX_BID) & MASK_56);
        s.minAsk = uint56((q >> SHIFT_MIN_ASK) & MASK_56);
        s.maxAsk = uint56((q >> SHIFT_MAX_ASK) & MASK_56);
        s.updatedAt = uint32(q >> SHIFT_UPDATED_AT);

        s.bidCapacity = uint96(c & MASK_96);
        s.askCapacity = uint96((c >> SHIFT_ASK_CAPACITY) & MASK_96);
        s.capGen = uint32((c >> SHIFT_GEN) & MASK_32);
        s.flags = uint16((c >> SHIFT_FLAGS) & MASK_16);

        if (_oracle[pairId].feedId != bytes32(0)) s.flags |= FLAG_SNAPSHOT_BOUNDED;

        if ((c >> SHIFT_DECAY) & MASK_16 != 0) s.flags |= FLAG_SNAPSHOT_DECAYING;

        s.bidUsed = uint96(u & MASK_96);
        s.askUsed = uint96((u >> SHIFT_ASK_CAPACITY) & MASK_96);
        s.usedGen = uint32((u >> SHIFT_GEN) & MASK_32);

        s.priceScaleExp = cfg.priceScaleExp;
        s.maxStaleSecs = cfg.maxStaleSecs;
    }

    function effectiveCapacity(uint16 pairId)
        external
        view
        returns (uint96 bidCapacity, uint96 askCapacity, uint16 decaySecs)
    {
        decaySecs = uint16((_capacityWord[pairId] >> SHIFT_DECAY) & MASK_16);

        (uint256 bidStatus, PairState memory bid) = _load(pairId, true);
        if (bidStatus == STATUS_OK) bidCapacity = uint96(bid.available);

        (uint256 askStatus, PairState memory ask) = _load(pairId, false);
        if (askStatus == STATUS_OK) askCapacity = uint96(ask.available);
    }

    function pairIdFor(address tokenIn, address tokenOut) public view returns (uint16 pairId, bool isBid) {
        uint256 route = _route[tokenIn][tokenOut];
        pairId = uint16(route & MASK_16);
        isBid = route & ROUTE_IS_BID != 0;
    }

    function getSupportedPairs() external view returns (Pair[] memory pairs) {
        uint256 n = pairCount;
        pairs = new Pair[](n);
        for (uint256 i; i < n;) {
            uint16 id = uint16(i + 1);
            PairConfig storage cfg = _config[id];
            pairs[i] = Pair({pairId: id, base: cfg.base, quote: cfg.quote});
            unchecked {
                ++i;
            }
        }
    }

    function swap(
        address tokenIn,
        address tokenOut,
        int256 specifiedAmount,
        uint256 limitAmount,
        address receiver,
        uint256 partnerId,
        uint256 deadline
    ) external locked returns (uint256 result) {
        if (block.timestamp > deadline) revert DeadlineExpired();
        if (receiver == address(0)) revert ZeroAddress();
        if (specifiedAmount == 0) revert ZeroAmount();
        if (specifiedAmount == type(int256).min) revert AmountOverflow();

        (uint16 pairId, bool isBid) = pairIdFor(tokenIn, tokenOut);

        uint256 amountIn;
        uint256 amountOut;
        if (specifiedAmount > 0) {
            amountIn = uint256(specifiedAmount);
            amountOut = _resolveExactIn(pairId, isBid, amountIn);
            if (amountOut < limitAmount) revert SlippageExceeded();
            result = amountOut;
        } else {

            amountOut = uint256(-specifiedAmount);
            amountIn = _resolveExactOut(pairId, isBid, amountOut);
            if (amountIn > limitAmount) revert SlippageExceeded();
            result = amountIn;
        }

        tokenIn.safeTransferFrom(msg.sender, address(this), amountIn);
        tokenOut.safeTransfer(receiver, amountOut);

        _settle(pairId, isBid, tokenIn, tokenOut, amountIn, amountOut);

        emit Swap(pairId, msg.sender, receiver, isBid, amountIn, amountOut, partnerId);
    }

    function swapWithContractBalance(
        address tokenIn,
        address tokenOut,
        uint256 limitAmount,
        address receiver,
        uint256 partnerId,
        uint256 deadline
    ) external locked returns (uint256 amountOut) {
        if (block.timestamp > deadline) revert DeadlineExpired();
        if (receiver == address(0)) revert ZeroAddress();

        (uint16 pairId, bool isBid) = pairIdFor(tokenIn, tokenOut);

        uint256 amountIn = IERC20Balance(tokenIn).balanceOf(address(this)) - _reserve[tokenIn];
        if (amountIn == 0) revert ZeroAmount();

        amountOut = _resolveExactIn(pairId, isBid, amountIn);
        if (amountOut < limitAmount) revert SlippageExceeded();

        tokenOut.safeTransfer(receiver, amountOut);

        _settle(pairId, isBid, tokenIn, tokenOut, amountIn, amountOut);

        emit Swap(pairId, msg.sender, receiver, isBid, amountIn, amountOut, partnerId);
    }

    function _load(uint16 pairId, bool isBid) private view returns (uint256 status, PairState memory st) {
        if (pairId == 0 || pairId > pairCount) return (STATUS_UNKNOWN, st);
        if (allPaused) return (STATUS_PAUSED, st);

        uint256 capWord = _capacityWord[pairId];
        if (capWord & FLAG_PAUSED != 0) return (STATUS_PAUSED, st);

        PairConfig storage cfg = _config[pairId];
        uint256 qWord = _quoteWord[pairId];
        uint256 updatedAt = qWord >> SHIFT_UPDATED_AT;

        if (updatedAt == 0) return (STATUS_STALE, st);
        if (updatedAt > block.timestamp) return (STATUS_STALE, st);
        uint256 age;
        unchecked {
            age = block.timestamp - updatedAt;
        }
        if (age > cfg.maxStaleSecs) return (STATUS_STALE, st);

        st.priceScaleExp = cfg.priceScaleExp;
        if (isBid) {
            st.pLow = qWord & MASK_56;
            st.pHigh = (qWord >> SHIFT_MAX_BID) & MASK_56;
            st.capacity = capWord & MASK_96;
            st.tokenOut = cfg.quote;
            st.minReserveOut = cfg.minQuoteReserve;
        } else {
            st.pLow = (qWord >> SHIFT_MIN_ASK) & MASK_56;
            st.pHigh = (qWord >> SHIFT_MAX_ASK) & MASK_56;
            st.capacity = (capWord >> SHIFT_ASK_CAPACITY) & MASK_96;
            st.tokenOut = cfg.base;
            st.minReserveOut = cfg.minBaseReserve;
        }

        st.available = _decayed(st.capacity, age, (capWord >> SHIFT_DECAY) & MASK_16);

        uint256 uWord = _usedWord[pairId];
        if (((uWord >> SHIFT_GEN) & MASK_32) == ((capWord >> SHIFT_GEN) & MASK_32)) {
            st.used = isBid ? (uWord & MASK_96) : ((uWord >> SHIFT_ASK_CAPACITY) & MASK_96);
        }

        st.reserveOut = _reserve[st.tokenOut];
    }

    function _decayed(uint256 capacity, uint256 age, uint256 decaySecs) private pure returns (uint256) {
        if (decaySecs == 0) return capacity;
        if (age >= decaySecs) return 0;
        return (capacity * (decaySecs - age)) / decaySecs;
    }

    function _referencePrice(uint16 pairId) private view returns (uint256 price, uint8 status) {
        PairOracle storage o = _oracle[pairId];

        bytes32 feedId = o.feedId;
        if (feedId == bytes32(0)) return (0, REF_DISABLED);

        address p = pyth;
        if (p == address(0) || p.code.length == 0) return (0, REF_UNAVAILABLE);

        PythStructs.Price memory quoted;
        try IPyth(p).getPriceUnsafe(feedId) returns (PythStructs.Price memory r) {
            quoted = r;
        } catch {
            return (0, REF_UNAVAILABLE);
        }

        uint256 publishTime = quoted.publishTime;
        uint256 age = publishTime > block.timestamp ? publishTime - block.timestamp : block.timestamp - publishTime;
        if (age > o.maxPythStaleSecs) return (0, REF_STALE);

        if (quoted.price <= 0) return (0, REF_INVALID);

        int256 net = int256(quoted.expo) + int256(o.refExpo);
        if (net > MAX_NET_EXPONENT || net < -MAX_NET_EXPONENT) return (0, REF_INVALID);

        uint256 raw = uint256(int256(quoted.price));
        if (net >= 0) {
            price = raw * (10 ** uint256(net));
        } else {

            price = raw / (10 ** uint256(-net));
        }

        if (price == 0 || price > MAX_REFERENCE_PRICE) return (0, REF_INVALID);

        status = REF_OK;
    }

    function _checkReferenceBound(uint16 pairId, uint256 maxBid, uint256 minAsk) private view {
        (uint256 ref, uint8 status) = _referencePrice(pairId);
        if (status == REF_DISABLED) return;
        if (status != REF_OK) revert ReferenceUnavailable(pairId, status);

        uint256 dev = _oracle[pairId].maxDeviationBps;

        if (maxBid * BPS > ref * (BPS + dev)) revert BidCeilingExceeded(pairId, ref, maxBid);

        if (minAsk * BPS < ref * (BPS - dev)) revert AskFloorBreached(pairId, ref, minAsk);
    }

    function _loadChecked(uint16 pairId, bool isBid) private view returns (PairState memory st) {
        uint256 status;
        (status, st) = _load(pairId, isBid);
        if (status == STATUS_UNKNOWN) revert UnknownPair();
        if (status == STATUS_PAUSED) revert PoolPaused();
        if (status == STATUS_STALE) revert StaleQuote();
    }

    function _maxAmountIn(PairState memory st, bool isBid) private pure returns (uint256) {
        uint256 room = st.available - st.used;
        if (isBid) return room;
        if (st.available == st.capacity) {
            return PropCurve.amountInAsk(room, st.pLow, st.pHigh, st.capacity, st.used, st.priceScaleExp);
        }
        return PropCurve.amountInAsk(room + 1, st.pLow, st.pHigh, st.capacity, st.used, st.priceScaleExp) - 1;
    }

    function _maxAmountOut(PairState memory st, bool isBid) private pure returns (uint256) {
        uint256 room = st.available - st.used;
        if (!isBid) return room;
        return PropCurve.amountOutBid(room, st.pLow, st.pHigh, st.capacity, st.used, st.priceScaleExp);
    }

    function _breachesFloor(PairState memory st, uint256 amount) private pure returns (bool) {
        return st.reserveOut < st.minReserveOut || amount > st.reserveOut - st.minReserveOut;
    }

    function _outFor(PairState memory st, bool isBid, uint256 amountIn) private pure returns (uint256) {
        if (amountIn == 0) return 0;
        if (st.available == 0 || st.used >= st.available) return 0;
        if (amountIn > _maxAmountIn(st, isBid)) return 0;

        return isBid
            ? PropCurve.amountOutBid(amountIn, st.pLow, st.pHigh, st.capacity, st.used, st.priceScaleExp)
            : PropCurve.amountOutAsk(amountIn, st.pLow, st.pHigh, st.capacity, st.used, st.priceScaleExp);
    }

    function _inFor(PairState memory st, bool isBid, uint256 amountOut) private pure returns (uint256) {
        if (amountOut == 0) return 0;
        if (st.available == 0 || st.used >= st.available) return 0;

        if (amountOut > _maxAmountOut(st, isBid)) return 0;

        return isBid
            ? PropCurve.amountInBid(amountOut, st.pLow, st.pHigh, st.capacity, st.used, st.priceScaleExp)
            : PropCurve.amountInAsk(amountOut, st.pLow, st.pHigh, st.capacity, st.used, st.priceScaleExp);
    }

    function _resolveExactIn(uint16 pairId, bool isBid, uint256 amountIn) private view returns (uint256 amountOut) {
        PairState memory st = _loadChecked(pairId, isBid);

        if (st.available == 0 || st.used >= st.available || amountIn > _maxAmountIn(st, isBid)) {
            revert InsufficientCapacity();
        }
        amountOut = isBid
            ? PropCurve.amountOutBid(amountIn, st.pLow, st.pHigh, st.capacity, st.used, st.priceScaleExp)
            : PropCurve.amountOutAsk(amountIn, st.pLow, st.pHigh, st.capacity, st.used, st.priceScaleExp);
        if (amountOut == 0) revert ZeroOutput();
        if (_breachesFloor(st, amountOut)) revert ReserveFloorBreached();
    }

    function _resolveExactOut(uint16 pairId, bool isBid, uint256 amountOut) private view returns (uint256 amountIn) {
        PairState memory st = _loadChecked(pairId, isBid);
        if (_breachesFloor(st, amountOut)) revert ReserveFloorBreached();
        amountIn = _inFor(st, isBid, amountOut);
        if (amountIn == 0) revert InsufficientCapacity();
    }

    function _settle(uint16 pairId, bool isBid, address tokenIn, address tokenOut, uint256 amountIn, uint256 amountOut)
        private
    {
        uint256 gen = (_capacityWord[pairId] >> SHIFT_GEN) & MASK_32;
        uint256 uWord = _usedWord[pairId];

        uint256 bidUsed;
        uint256 askUsed;
        if (((uWord >> SHIFT_GEN) & MASK_32) == gen) {
            bidUsed = uWord & MASK_96;
            askUsed = (uWord >> SHIFT_ASK_CAPACITY) & MASK_96;
        }

        if (isBid) bidUsed += amountIn;
        else askUsed += amountOut;

        _usedWord[pairId] = bidUsed | (askUsed << SHIFT_ASK_CAPACITY) | (gen << SHIFT_GEN);

        _reserve[tokenIn] += amountIn;
        _reserve[tokenOut] -= amountOut;
    }
}
