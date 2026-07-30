// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {Test} from "forge-std/Test.sol";

import {Router, IPermit2, RouteParams, Batch, Hop, SwapStep} from "../src/Router.sol";
import {RouteDecoder} from "../src/libraries/RouteDecoder.sol";
import {IAdapter, IERC20Minimal} from "../src/interfaces/IAdapter.sol";
import {PropPoolAdapter} from "../src/adapters/PropPoolAdapter.sol";
import {UniV2Adapter} from "../src/adapters/UniV2Adapter.sol";
import {PropPool} from "../src/PropPool.sol";
import {IPropPool} from "../src/interfaces/IPropPool.sol";
import {MockERC20} from "../src/mocks/MockERC20.sol";
import {UniswapV2Factory} from "../src/reference/univ2/UniswapV2Factory.sol";
import {UniswapV2Pair} from "../src/reference/univ2/UniswapV2Pair.sol";

// --- Permit2 doubles ---

/// @notice A faithful re-implementation of the slice of canonical Permit2 the router uses.
///
/// Faithful in the parts that matter here: the real EIP-712 domain (`name = "Permit2"`, no version
/// field), the real `PermitTransferFrom` / `TokenPermissions` type strings, `spender` bound to
/// `msg.sender`, an unordered nonce bitmap, the deadline check, and the
/// `requestedAmount <= permitted.amount` check. Signatures against this mock are byte-identical to
/// what a wallet produces against the genesis pre-install; a mock that skipped signature
/// verification would let the router's Permit2 path pass a test it should not.
///
/// @dev No immutables and no constructor-written storage, because it is installed with `vm.etch`,
///      which copies runtime code only. `DOMAIN_SEPARATOR()` is therefore computed on every call
///      from `block.chainid` and `address(this)`, so it resolves to the canonical address once
///      etched.
contract MockPermit2 {
    error SignatureExpired();
    error InvalidAmount();
    error InvalidNonce();
    error InvalidSigner();
    error InvalidSignatureLength();

    bytes32 private constant _TOKEN_PERMISSIONS_TYPEHASH = keccak256("TokenPermissions(address token,uint256 amount)");
    bytes32 private constant _PERMIT_TRANSFER_FROM_TYPEHASH = keccak256(
        "PermitTransferFrom(TokenPermissions permitted,address spender,uint256 nonce,uint256 deadline)TokenPermissions(address token,uint256 amount)"
    );
    bytes32 private constant _EIP712_DOMAIN_TYPEHASH =
        keccak256("EIP712Domain(string name,uint256 chainId,address verifyingContract)");

    mapping(address => mapping(uint256 => uint256)) public nonceBitmap;

    // forge-lint: disable-next-line(mixed-case-function)
    function DOMAIN_SEPARATOR() public view returns (bytes32) {
        return keccak256(abi.encode(_EIP712_DOMAIN_TYPEHASH, keccak256("Permit2"), block.chainid, address(this)));
    }

    function hashPermit(IPermit2.PermitTransferFrom memory permit, address spender) public view returns (bytes32) {
        bytes32 tokenPermissions =
            keccak256(abi.encode(_TOKEN_PERMISSIONS_TYPEHASH, permit.permitted.token, permit.permitted.amount));
        return keccak256(
            abi.encodePacked(
                "\x19\x01",
                DOMAIN_SEPARATOR(),
                keccak256(
                    abi.encode(_PERMIT_TRANSFER_FROM_TYPEHASH, tokenPermissions, spender, permit.nonce, permit.deadline)
                )
            )
        );
    }

    function permitTransferFrom(
        IPermit2.PermitTransferFrom memory permit,
        IPermit2.SignatureTransferDetails calldata transferDetails,
        address owner,
        bytes calldata signature
    ) external {
        if (block.timestamp > permit.deadline) {
            revert SignatureExpired();
        }
        if (transferDetails.requestedAmount > permit.permitted.amount) revert InvalidAmount();

        _useUnorderedNonce(owner, permit.nonce);

        if (signature.length != 65) revert InvalidSignatureLength();
        bytes32 r = bytes32(signature[0:32]);
        bytes32 s = bytes32(signature[32:64]);
        uint8 v = uint8(signature[64]);
        address signer = ecrecover(hashPermit(permit, msg.sender), v, r, s);
        if (signer == address(0) || signer != owner) revert InvalidSigner();

        IERC20Minimal(permit.permitted.token).transferFrom(owner, transferDetails.to, transferDetails.requestedAmount);
    }

    /// @dev `1 << (nonce & 0xff)` is canonical Permit2's own bitmap indexing, kept verbatim. The
    ///      shift-order lint fires on the pattern but the operands are the right way round: the
    ///      low byte of the nonce is the bit position within the word.
    function _useUnorderedNonce(address from, uint256 nonce) internal {
        uint256 word = nonce >> 8;
        // forge-lint: disable-next-line(incorrect-shift)
        uint256 bit = 1 << (nonce & 0xff);
        if (nonceBitmap[from][word] & bit != 0) revert InvalidNonce();
        nonceBitmap[from][word] |= bit;
    }
}

/// @notice A Permit2 whose `permitTransferFrom` is a bare `return;` — it moves no tokens and does
///         not revert. Installed at the canonical address so the tests can prove our router
///         refuses to swap against whatever balance it happened to be holding.
contract NoOpPermit2 {
    function permitTransferFrom(
        IPermit2.PermitTransferFrom memory,
        IPermit2.SignatureTransferDetails calldata,
        address,
        bytes calldata
    ) external {
        return;
    }
}

/// @notice Transfers a fraction of what was asked for. A "successful" permit that under-delivers
///         is the subtler cousin of the no-op: no revert, tokens moved, but not the amount the
///         plan was built around.
contract ShortPermit2 {
    uint256 public immutable NUMERATOR;
    uint256 public immutable DENOMINATOR;

    constructor(uint256 numerator, uint256 denominator) {
        NUMERATOR = numerator;
        DENOMINATOR = denominator;
    }

    function permitTransferFrom(
        IPermit2.PermitTransferFrom memory permit,
        IPermit2.SignatureTransferDetails calldata transferDetails,
        address owner,
        bytes calldata
    ) external {
        IERC20Minimal(permit.permitted.token)
            .transferFrom(owner, transferDetails.to, (transferDetails.requestedAmount * NUMERATOR) / DENOMINATOR);
    }
}

// --- Misbehaving doubles ---

/// @notice `transferFrom` returns `true` and moves nothing. The approval-path twin of the no-op
///         Permit2, and the reason `_pull` measures a balance delta instead of trusting the call.
contract SilentToken is MockERC20 {
    constructor() MockERC20("Silent", "SIL", 18) {}

    function transferFrom(address, address, uint256) public pure override returns (bool) {
        return true;
    }
}

/// @notice An adapter that performs a real prop-pool swap but keeps a slice of the output — the
///         shape of "an adapter returned less than promised", where the venue filled, the plan's
///         quote was correct, and the shortfall appears only in the router's own balance delta.
contract SkimmingAdapter is IAdapter {
    uint256 public immutable KEEP_BPS;

    constructor(uint256 keepBps) {
        KEEP_BPS = keepBps;
    }

    function sellBase(address to, address pool, bytes calldata data) external override {
        _run(to, pool, data, true);
    }

    function sellQuote(address to, address pool, bytes calldata data) external override {
        _run(to, pool, data, false);
    }

    function _run(address to, address pool, bytes calldata data, bool bid) private {
        (address base, address quote, uint256 limitAmount, uint256 partnerId, uint256 deadline) =
            abi.decode(data, (address, address, uint256, uint256, uint256));
        (address tokenIn, address tokenOut) = bid ? (base, quote) : (quote, base);
        uint256 out =
            IPropPool(pool).swapWithContractBalance(tokenIn, tokenOut, limitAmount, address(this), partnerId, deadline);
        IERC20Minimal(tokenOut).transfer(to, out - (out * KEEP_BPS) / 10_000);
    }
}

// --- Fixture ---

contract RouterTest is Test {
    // Canonical Permit2. Genesis pre-install on GIWA; the router hard-codes it.
    address internal constant PERMIT2 = 0x000000000022D473030F116dDEE9F6B43aC78BA3;

    Router internal router;
    PropPool internal pool;
    PropPoolAdapter internal propAdapter;
    UniV2Adapter internal uniAdapter;
    UniswapV2Factory internal factory;

    MockERC20 internal tokenA; // 18 dec, prop pair base
    MockERC20 internal tokenB; // 6 dec,  prop pair quote
    MockERC20 internal tokenC; // 18 dec, second leg of the 2-hop route

    // Named after the token pair, not the lint's preferred casing.
    // forge-lint: disable-next-line(mixed-case-variable)
    UniswapV2Pair internal pairAB;
    // forge-lint: disable-next-line(mixed-case-variable)
    UniswapV2Pair internal pairBC;

    address internal owner = makeAddr("owner");
    address internal manager = makeAddr("manager");
    address internal updater = makeAddr("updater");
    address internal guardian = makeAddr("guardian");
    address internal user;
    uint256 internal userPk;
    address internal receiver = makeAddr("receiver");

    uint16 internal constant PAIR_ID = 1;
    uint8 internal constant PRICE_SCALE_EXP = 18;
    /// @dev `quoteUnits = baseUnits * price / 1e18`. With A at 18 decimals and B at 6, a price of
    ///      2e9 means one whole A is worth 2000 whole B.
    uint256 internal constant MID = 2e9;

    uint256 internal constant PROP_BASE_INVENTORY = 5_000e18;
    uint256 internal constant PROP_QUOTE_INVENTORY = 10_000_000e6;
    uint96 internal constant BID_CAPACITY = 1_000e18;
    /// @notice Base the pool will SELL this epoch. BASE units — see `PropCurve` amendment 1 and
    ///         `IPropPool.PairSnapshot`.
    /// @dev Symmetric with `BID_CAPACITY`, both counters sharing a unit. An ask `amountIn` is quote
    ///      denominated and is NOT comparable with this number: the epoch's quote ceiling is
    ///      `getAmountIn(quote, base, ASK_CAPACITY)`.
    uint96 internal constant ASK_CAPACITY = 1_000e18;

    function setUp() public {
        vm.warp(1_800_000_000);
        (user, userPk) = makeAddrAndKey("permitUser");

        tokenA = new MockERC20("Alpha", "A", 18);
        tokenB = new MockERC20("Bravo", "B", 6);
        tokenC = new MockERC20("Charlie", "C", 18);

        router = new Router();
        propAdapter = new PropPoolAdapter();
        uniAdapter = new UniV2Adapter();

        // --- prop pool: pair A/B ---
        pool = new PropPool(owner, manager, updater, guardian);
        vm.prank(owner);
        pool.addPair(address(tokenA), address(tokenB), PRICE_SCALE_EXP, 60, 1e9);
        vm.prank(manager);
        pool.setPairConfig(PAIR_ID, 60, 1e9, 0, 0);

        tokenA.mint(manager, PROP_BASE_INVENTORY);
        tokenB.mint(manager, PROP_QUOTE_INVENTORY);
        vm.startPrank(manager);
        tokenA.approve(address(pool), type(uint256).max);
        tokenB.approve(address(pool), type(uint256).max);
        pool.deposit(address(tokenA), PROP_BASE_INVENTORY);
        pool.deposit(address(tokenB), PROP_QUOTE_INVENTORY);
        vm.stopPrank();

        _pushLadder(MID, 5, 25);
        vm.prank(updater);
        pool.refreshCapacity(PAIR_ID, BID_CAPACITY, ASK_CAPACITY);

        // --- univ2: pairs A/B and B/C, both priced at the same mid as the prop pool ---
        factory = new UniswapV2Factory(address(this));
        pairAB = UniswapV2Pair(factory.createPair(address(tokenA), address(tokenB)));
        pairBC = UniswapV2Pair(factory.createPair(address(tokenB), address(tokenC)));

        tokenA.mint(address(pairAB), 5_000e18);
        tokenB.mint(address(pairAB), 10_000_000e6);
        pairAB.mint(address(this));

        tokenB.mint(address(pairBC), 10_000_000e6);
        tokenC.mint(address(pairBC), 10_000e18);
        pairBC.mint(address(this));

        // --- the payer ---
        tokenA.mint(user, 1_000_000e18);
        tokenB.mint(user, 1_000_000_000e6);
        vm.startPrank(user);
        tokenA.approve(address(router), type(uint256).max);
        tokenB.approve(address(router), type(uint256).max);
        vm.stopPrank();

        // --- canonical Permit2, installed at its canonical address ---
        _installPermit2(address(new MockPermit2()));
        vm.startPrank(user);
        tokenA.approve(PERMIT2, type(uint256).max);
        tokenB.approve(PERMIT2, type(uint256).max);
        vm.stopPrank();
    }

    function _installPermit2(address implementation) internal {
        vm.etch(PERMIT2, implementation.code);
    }

    function _pushLadder(uint256 mid, uint256 halfBps, uint256 widthBps) internal {
        uint256 maxBid = (mid * (10_000 - halfBps)) / 10_000;
        uint256 minBid = (maxBid * (10_000 - widthBps)) / 10_000;
        uint256 minAsk = (mid * (10_000 + halfBps)) / 10_000;
        uint256 maxAsk = (minAsk * (10_000 + widthBps)) / 10_000;
        uint256[] memory packed = new uint256[](1);
        packed[0] = minBid | (maxBid << 56) | (minAsk << 112) | (maxAsk << 168) | (uint256(PAIR_ID) << 224);
        vm.prank(updater);
        pool.updateQuote(packed);
    }

    // --- Route builders ---

    function _propPayload(address base, address quote, uint256 limitAmount) internal view returns (bytes memory) {
        return propAdapter.encodePayload(base, quote, limitAmount, 0, block.timestamp + 1);
    }

    /// @dev `reverse` selects `sellQuote`. For the prop pool that means quote -> base; for a V2
    ///      pair it means `token1 -> token0`, which depends on address ordering and is therefore
    ///      computed rather than hard-coded.
    function _step(address adapter, address venue, uint16 weightBps, bool reverse, bytes memory payload)
        internal
        pure
        returns (SwapStep memory)
    {
        return SwapStep({
            adapter: adapter, rawData: RouteDecoder.encode(venue, weightBps, reverse, false), payload: payload
        });
    }

    function _uniReverse(UniswapV2Pair p, address tokenIn) internal view returns (bool) {
        return tokenIn == p.token1();
    }

    function _oneStepRoute(address tokenIn, address tokenOut, uint256 amountIn, SwapStep memory step)
        internal
        view
        returns (RouteParams memory p)
    {
        SwapStep[] memory steps = new SwapStep[](1);
        steps[0] = step;

        Hop[] memory hops = new Hop[](1);
        hops[0] = Hop({tokenIn: tokenIn, steps: steps});

        Batch[] memory batches = new Batch[](1);
        batches[0] = Batch({weightBps: 10_000, hops: hops});

        p = RouteParams({
            tokenIn: tokenIn,
            tokenOut: tokenOut,
            receiver: receiver,
            amountIn: amountIn,
            quotedAmountOut: 0,
            deadline: block.timestamp + 1,
            batches: batches
        });
    }

    function _propRoute(uint256 amountIn, bool bid) internal view returns (RouteParams memory) {
        (address tokenIn, address tokenOut) =
            bid ? (address(tokenA), address(tokenB)) : (address(tokenB), address(tokenA));
        return _oneStepRoute(
            tokenIn,
            tokenOut,
            amountIn,
            _step(address(propAdapter), address(pool), 10_000, !bid, _propPayload(address(tokenA), address(tokenB), 0))
        );
    }

    function _uniRoute(uint256 amountIn, bool aToB) internal view returns (RouteParams memory) {
        (address tokenIn, address tokenOut) =
            aToB ? (address(tokenA), address(tokenB)) : (address(tokenB), address(tokenA));
        return _oneStepRoute(
            tokenIn,
            tokenOut,
            amountIn,
            _step(address(uniAdapter), address(pairAB), 10_000, _uniReverse(pairAB, tokenIn), "")
        );
    }

    // --- Off-chain-style quoting helpers ---

    function _uniQuote(UniswapV2Pair p, address tokenIn, uint256 amountIn) internal view returns (uint256) {
        (uint112 r0, uint112 r1,) = p.getReserves();
        return
            tokenIn == p.token0()
                ? uniAdapter.getAmountOut(amountIn, r0, r1)
                : uniAdapter.getAmountOut(amountIn, r1, r0);
    }

    // --- Single hop — prop pool ---

    function test_singleHopViaPropPoolAdapter() public {
        uint256 amountIn = 100e18;
        uint256 quoted = pool.getAmountOut(address(tokenA), address(tokenB), amountIn);
        assertGt(quoted, 0);

        RouteParams memory p = _propRoute(amountIn, true);
        p.quotedAmountOut = quoted;

        uint256 before = tokenB.balanceOf(receiver);
        vm.prank(user);
        uint256 out = router.swapExactIn(p, quoted);

        // The adapter drives `swapWithContractBalance`, which prices against live storage in the
        // same frame that moves the tokens, so the realised fill must equal the quote to the unit.
        assertEq(out, quoted, "routed output diverged from the pool's own quote");
        assertEq(tokenB.balanceOf(receiver) - before, out, "receiver was not paid the measured output");
        assertEq(tokenB.balanceOf(address(router)), 0, "router retained output");
        assertEq(tokenA.balanceOf(address(router)), 0, "router retained input");

        IPropPool.PairSnapshot memory snap = pool.snapshot(PAIR_ID);
        assertEq(uint256(snap.bidUsed), amountIn, "the pool did not account the full input");
    }

    function test_singleHopViaPropPoolAdapter_askSide() public {
        uint256 amountIn = 200_000e6;
        uint256 quoted = pool.getAmountOut(address(tokenB), address(tokenA), amountIn);

        RouteParams memory p = _propRoute(amountIn, false);
        vm.prank(user);
        uint256 out = router.swapExactIn(p, quoted);

        assertEq(out, quoted);
        assertEq(tokenA.balanceOf(receiver), out);
    }

    // --- Single hop — UniV2 ---

    function test_singleHopViaUniV2Adapter() public {
        uint256 amountIn = 100e18;
        uint256 expected = _uniQuote(pairAB, address(tokenA), amountIn);

        RouteParams memory p = _uniRoute(amountIn, true);
        vm.prank(user);
        uint256 out = router.swapExactIn(p, expected);

        // The adapter must reproduce `UniswapV2Library.getAmountOut` bit for bit; a one-unit
        // error either reverts the pair's k-check or donates to the LPs.
        assertEq(out, expected, "adapter output != library output");
        assertEq(tokenB.balanceOf(receiver), out);
    }

    function test_singleHopViaUniV2Adapter_reverseDirection() public {
        uint256 amountIn = 200_000e6;
        uint256 expected = _uniQuote(pairAB, address(tokenB), amountIn);

        RouteParams memory p = _uniRoute(amountIn, false);
        vm.prank(user);
        uint256 out = router.swapExactIn(p, expected);

        assertEq(out, expected);
        assertEq(tokenA.balanceOf(receiver), out);
    }

    // --- Two hops ---

    /// @notice A -> B on the prop pool, then B -> C on a V2 pair. The second hop is funded by
    ///         what the first one actually produced, measured on-chain.
    function test_twoHopRoute() public {
        uint256 amountIn = 50e18;

        uint256 midAmount = pool.getAmountOut(address(tokenA), address(tokenB), amountIn);
        uint256 expected = _uniQuote(pairBC, address(tokenB), midAmount);

        SwapStep[] memory s0 = new SwapStep[](1);
        s0[0] = _step(
            address(propAdapter), address(pool), 10_000, false, _propPayload(address(tokenA), address(tokenB), 0)
        );
        SwapStep[] memory s1 = new SwapStep[](1);
        s1[0] = _step(address(uniAdapter), address(pairBC), 10_000, _uniReverse(pairBC, address(tokenB)), "");

        Hop[] memory hops = new Hop[](2);
        hops[0] = Hop({tokenIn: address(tokenA), steps: s0});
        hops[1] = Hop({tokenIn: address(tokenB), steps: s1});

        Batch[] memory batches = new Batch[](1);
        batches[0] = Batch({weightBps: 10_000, hops: hops});

        RouteParams memory p = RouteParams({
            tokenIn: address(tokenA),
            tokenOut: address(tokenC),
            receiver: receiver,
            amountIn: amountIn,
            quotedAmountOut: expected,
            deadline: block.timestamp + 1,
            batches: batches
        });

        vm.prank(user);
        uint256 out = router.swapExactIn(p, expected);

        assertEq(out, expected, "two-hop output != composed quote");
        assertEq(tokenC.balanceOf(receiver), out);
        assertEq(tokenB.balanceOf(address(router)), 0, "intermediate token stranded in the router");
    }

    function test_twoHopRoute_revertsWhenTheFirstHopProducesNothing() public {
        // Pause the prop pair so hop 0 cannot fill. The adapter's own call reverts, which is what
        // the caller sees; `HopProducedNothing` is reachable only for a venue that silently
        // delivers zero, which the prop pool does not do.
        vm.prank(guardian);
        pool.pause(PAIR_ID);

        SwapStep[] memory s0 = new SwapStep[](1);
        s0[0] = _step(
            address(propAdapter), address(pool), 10_000, false, _propPayload(address(tokenA), address(tokenB), 0)
        );
        SwapStep[] memory s1 = new SwapStep[](1);
        s1[0] = _step(address(uniAdapter), address(pairBC), 10_000, _uniReverse(pairBC, address(tokenB)), "");

        Hop[] memory hops = new Hop[](2);
        hops[0] = Hop({tokenIn: address(tokenA), steps: s0});
        hops[1] = Hop({tokenIn: address(tokenB), steps: s1});

        Batch[] memory batches = new Batch[](1);
        batches[0] = Batch({weightBps: 10_000, hops: hops});

        RouteParams memory p = RouteParams({
            tokenIn: address(tokenA),
            tokenOut: address(tokenC),
            receiver: receiver,
            amountIn: 50e18,
            quotedAmountOut: 0,
            deadline: block.timestamp + 1,
            batches: batches
        });

        vm.prank(user);
        vm.expectRevert(PropPool.PoolPaused.selector);
        router.swapExactIn(p, 0);
    }

    // --- Weighted split across two venues ---

    /// @notice 60% prop pool, 40% V2, in one hop. Asserts the weights were *honoured*, not merely
    ///         that the total came out plausible: each venue's own accounting is checked against
    ///         the share the decoder should have handed it.
    function test_weightedSplitAcrossTwoVenuesHonoursTheWeights() public {
        uint256 amountIn = 100e18;
        uint256 propShare = (amountIn * 6_000) / 10_000;
        uint256 uniShare = amountIn - propShare; // last leg sweeps the remainder

        uint256 propQuote = pool.getAmountOut(address(tokenA), address(tokenB), propShare);
        uint256 uniQuote = _uniQuote(pairAB, address(tokenA), uniShare);

        SwapStep[] memory steps = new SwapStep[](2);
        steps[0] =
            _step(address(propAdapter), address(pool), 6_000, false, _propPayload(address(tokenA), address(tokenB), 0));
        steps[1] = _step(address(uniAdapter), address(pairAB), 4_000, _uniReverse(pairAB, address(tokenA)), "");

        Hop[] memory hops = new Hop[](1);
        hops[0] = Hop({tokenIn: address(tokenA), steps: steps});
        Batch[] memory batches = new Batch[](1);
        batches[0] = Batch({weightBps: 10_000, hops: hops});

        RouteParams memory p = RouteParams({
            tokenIn: address(tokenA),
            tokenOut: address(tokenB),
            receiver: receiver,
            amountIn: amountIn,
            quotedAmountOut: propQuote + uniQuote,
            deadline: block.timestamp + 1,
            batches: batches
        });

        uint256 uniABefore = _pairABReserveOfA();

        vm.prank(user);
        uint256 out = router.swapExactIn(p, propQuote + uniQuote);

        // The pool's `used` counter and the pair's reserve delta are the authoritative records of
        // what each venue received. Neither is a number the test supplied.
        assertEq(uint256(pool.snapshot(PAIR_ID).bidUsed), propShare, "prop leg did not receive its 60% weight");
        assertEq(_pairABReserveOfA() - uniABefore, uniShare, "V2 leg did not receive its 40% weight");

        assertEq(out, propQuote + uniQuote, "split output != sum of the two leg quotes");
        assertEq(tokenA.balanceOf(address(router)), 0, "split left input dust in the router");
    }

    // forge-lint: disable-next-line(mixed-case-function)
    function _pairABReserveOfA() internal view returns (uint256) {
        (uint112 r0, uint112 r1,) = pairAB.getReserves();
        return address(tokenA) == pairAB.token0() ? uint256(r0) : uint256(r1);
    }

    /// @notice Batch-level weights (parallel paths) are honoured the same way, and a deliberate
    ///         under-allocation is refunded to the payer rather than kept or silently topped up.
    function test_batchWeightsUnderAllocateAndRefundTheRemainder() public {
        uint256 amountIn = 100e18;
        uint256 routed = (amountIn * 7_000) / 10_000;

        SwapStep[] memory steps = new SwapStep[](1);
        steps[0] = _step(
            address(propAdapter), address(pool), 10_000, false, _propPayload(address(tokenA), address(tokenB), 0)
        );
        Hop[] memory hops = new Hop[](1);
        hops[0] = Hop({tokenIn: address(tokenA), steps: steps});
        Batch[] memory batches = new Batch[](1);
        batches[0] = Batch({weightBps: 7_000, hops: hops}); // deliberately < 10_000

        RouteParams memory p = RouteParams({
            tokenIn: address(tokenA),
            tokenOut: address(tokenB),
            receiver: receiver,
            amountIn: amountIn,
            quotedAmountOut: 0,
            deadline: block.timestamp + 1,
            batches: batches
        });

        uint256 userBefore = tokenA.balanceOf(user);
        vm.prank(user);
        router.swapExactIn(p, 0);

        IPropPool.PairSnapshot memory snap = pool.snapshot(PAIR_ID);
        assertEq(uint256(snap.bidUsed), routed, "under-allocated batch did not route exactly 70%");
        assertEq(userBefore - tokenA.balanceOf(user), routed, "withheld input was not refunded to the payer");
        assertEq(tokenA.balanceOf(address(router)), 0, "router kept the withheld input");
    }

    function test_overAllocatedWeightsAreRejected() public {
        SwapStep[] memory steps = new SwapStep[](2);
        steps[0] =
            _step(address(propAdapter), address(pool), 6_000, false, _propPayload(address(tokenA), address(tokenB), 0));
        steps[1] = _step(address(uniAdapter), address(pairAB), 5_000, _uniReverse(pairAB, address(tokenA)), "");

        Hop[] memory hops = new Hop[](1);
        hops[0] = Hop({tokenIn: address(tokenA), steps: steps});
        Batch[] memory batches = new Batch[](1);
        batches[0] = Batch({weightBps: 10_000, hops: hops});

        RouteParams memory p = RouteParams({
            tokenIn: address(tokenA),
            tokenOut: address(tokenB),
            receiver: receiver,
            amountIn: 100e18,
            quotedAmountOut: 0,
            deadline: block.timestamp + 1,
            batches: batches
        });

        vm.prank(user);
        vm.expectRevert(abi.encodeWithSelector(RouteDecoder.WeightSumExceeded.selector, 11_000));
        router.swapExactIn(p, 0);
    }

    // --- Exact output ---

    function test_exactOut() public {
        uint256 exactOut = 100_000e6;
        // The planner inverts the venue's curve off-chain; the router only enforces the bounds.
        uint256 plannedIn = pool.getAmountIn(address(tokenA), address(tokenB), exactOut);
        assertGt(plannedIn, 0);

        RouteParams memory p = _propRoute(plannedIn, true);
        p.quotedAmountOut = exactOut;

        vm.prank(user);
        (uint256 spent, uint256 out) = router.swapExactOut(p, exactOut, plannedIn);

        assertGe(out, exactOut, "exact-out under-delivered");
        assertLe(spent, plannedIn, "exact-out overspent");
        assertEq(tokenB.balanceOf(receiver), out, "overshoot was not delivered to the receiver");
    }

    /// @notice An overshooting plan delivers the surplus as extra *output*; only unspent input is
    ///         refunded. Pinned because the alternative reading — refund the overshoot in the
    ///         output token — would be a different contract.
    function test_exactOut_overshootIsDeliveredNotRefunded() public {
        uint256 exactOut = 100_000e6;
        uint256 plannedIn = pool.getAmountIn(address(tokenA), address(tokenB), exactOut) * 2;

        RouteParams memory p = _propRoute(plannedIn, true);
        vm.prank(user);
        (uint256 spent, uint256 out) = router.swapExactOut(p, exactOut, plannedIn);

        assertGt(out, exactOut, "doubling the input should overshoot");
        assertEq(spent, plannedIn, "the whole planned input should have been consumed");
        assertEq(tokenB.balanceOf(receiver), out);
    }

    function test_exactOut_revertsWhenThePlannerUnderestimated() public {
        uint256 exactOut = 100_000e6;
        uint256 plannedIn = pool.getAmountIn(address(tokenA), address(tokenB), exactOut) - 1;

        RouteParams memory p = _propRoute(plannedIn, true);
        vm.prank(user);
        vm.expectRevert();
        router.swapExactOut(p, exactOut, plannedIn);
    }

    /// @notice The `maxAmountIn` check runs before the pull, so an inflated plan cannot even move
    ///         the payer's tokens.
    function test_exactOut_revertsBeforePullingWhenPlanExceedsMaxIn() public {
        RouteParams memory p = _propRoute(100e18, true);
        uint256 userBefore = tokenA.balanceOf(user);

        vm.prank(user);
        vm.expectRevert(abi.encodeWithSelector(Router.ExcessiveInput.selector, 100e18, 99e18));
        router.swapExactOut(p, 1, 99e18);

        assertEq(tokenA.balanceOf(user), userBefore, "tokens moved despite an over-limit plan");
    }

    // --- Deadline and slippage ---

    function test_deadlineExpired() public {
        RouteParams memory p = _propRoute(100e18, true);
        p.deadline = block.timestamp - 1;

        vm.prank(user);
        vm.expectRevert(abi.encodeWithSelector(Router.DeadlineExpired.selector, block.timestamp - 1, block.timestamp));
        router.swapExactIn(p, 0);
    }

    function test_deadlineAtExactlyNowStillPasses() public {
        RouteParams memory p = _propRoute(100e18, true);
        p.deadline = block.timestamp;
        vm.prank(user);
        assertGt(router.swapExactIn(p, 0), 0);
    }

    function test_slippageRevert() public {
        uint256 amountIn = 100e18;
        uint256 quoted = pool.getAmountOut(address(tokenA), address(tokenB), amountIn);

        RouteParams memory p = _propRoute(amountIn, true);
        vm.prank(user);
        vm.expectRevert(abi.encodeWithSelector(Router.InsufficientOutput.selector, quoted, quoted + 1));
        router.swapExactIn(p, quoted + 1);
    }

    /// @notice Slippage is enforced on the *aggregate*, so a good leg cannot paper over a bad one.
    /// @dev Both legs go to the prop pool, but the second one is a `SkimmingAdapter` that pockets
    ///      half its output. The honest total is met by the first leg plus a full second leg; with
    ///      the skim it is not, and the router must catch it at the end rather than per leg.
    function test_slippageIsEnforcedOnTheAggregateNotPerLeg() public {
        SkimmingAdapter skimmer = new SkimmingAdapter(5_000);
        uint256 amountIn = 100e18;

        uint256 half = (amountIn * 5_000) / 10_000;
        // Both legs are simulated and rolled back rather than quoted: the second leg prices from a
        // ladder the first has already partly consumed, so quoting it from the current state would
        // be the wrong number.
        (uint256 honestFirst, uint256 honestSecond) = _simulateTwoLegs(half, amountIn - half);

        SwapStep[] memory steps = new SwapStep[](2);
        steps[0] =
            _step(address(propAdapter), address(pool), 5_000, false, _propPayload(address(tokenA), address(tokenB), 0));
        steps[1] =
            _step(address(skimmer), address(pool), 5_000, false, _propPayload(address(tokenA), address(tokenB), 0));

        Hop[] memory hops = new Hop[](1);
        hops[0] = Hop({tokenIn: address(tokenA), steps: steps});
        Batch[] memory batches = new Batch[](1);
        batches[0] = Batch({weightBps: 10_000, hops: hops});

        RouteParams memory p = RouteParams({
            tokenIn: address(tokenA),
            tokenOut: address(tokenB),
            receiver: receiver,
            amountIn: amountIn,
            quotedAmountOut: honestFirst + honestSecond,
            deadline: block.timestamp + 1,
            batches: batches
        });

        uint256 honestTotal = honestFirst + honestSecond;
        uint256 skimmedTotal = honestFirst + honestSecond - honestSecond / 2;

        vm.prank(user);
        vm.expectRevert(abi.encodeWithSelector(Router.InsufficientOutput.selector, skimmedTotal, honestTotal));
        router.swapExactIn(p, honestTotal);

        // Without the guard it would have "succeeded" at the lower number, the shortfall being
        // visible only in the aggregate balance delta.
        vm.prank(user);
        assertEq(router.swapExactIn(p, skimmedTotal), skimmedTotal, "the skimmed total is what actually arrives");
        assertEq(tokenB.balanceOf(address(skimmer)), honestSecond / 2, "the adapter kept exactly what it skimmed");
    }

    /// @dev Runs two sequential bid legs against the prop pool and rolls the state back, so the
    ///      caller learns what an unskimmed two-leg split would have produced.
    function _simulateTwoLegs(uint256 legOne, uint256 legTwo) internal returns (uint256 outOne, uint256 outTwo) {
        uint256 snapId = vm.snapshotState();
        tokenA.mint(address(pool), legOne);
        outOne = pool.swapWithContractBalance(address(tokenA), address(tokenB), 0, user, 0, block.timestamp + 1);
        tokenA.mint(address(pool), legTwo);
        outTwo = pool.swapWithContractBalance(address(tokenA), address(tokenB), 0, user, 0, block.timestamp + 1);
        vm.revertToState(snapId);
    }

    /// @notice An adapter that delivers nothing at all trips `ZeroOutput`, not a silent success.
    function test_adapterDeliveringNothingRevertsRatherThanSucceeding() public {
        SkimmingAdapter blackHole = new SkimmingAdapter(10_000);
        RouteParams memory p = _oneStepRoute(
            address(tokenA),
            address(tokenB),
            100e18,
            _step(address(blackHole), address(pool), 10_000, false, _propPayload(address(tokenA), address(tokenB), 0))
        );

        vm.prank(user);
        vm.expectRevert(Router.ZeroOutput.selector);
        router.swapExactIn(p, 0);
    }

    // --- Permit2 ---

    function _signPermit(address token, uint256 amount, uint256 nonce, uint256 deadline)
        internal
        view
        returns (IPermit2.PermitTransferFrom memory permit, bytes memory signature)
    {
        permit = IPermit2.PermitTransferFrom({
            permitted: IPermit2.TokenPermissions({token: token, amount: amount}), nonce: nonce, deadline: deadline
        });
        bytes32 digest = MockPermit2(PERMIT2).hashPermit(permit, address(router));
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(userPk, digest);
        signature = abi.encodePacked(r, s, v);
    }

    function test_permit2HappyPath() public {
        uint256 amountIn = 100e18;
        uint256 quoted = pool.getAmountOut(address(tokenA), address(tokenB), amountIn);

        RouteParams memory p = _propRoute(amountIn, true);
        (IPermit2.PermitTransferFrom memory permit, bytes memory sig) =
            _signPermit(address(tokenA), amountIn, 0, block.timestamp + 1);

        uint256 userBefore = tokenA.balanceOf(user);
        vm.prank(user);
        uint256 out = router.swapExactInWithPermit2(p, quoted, permit, sig);

        assertEq(out, quoted);
        assertEq(userBefore - tokenA.balanceOf(user), amountIn, "permit did not move exactly the planned input");
        assertEq(tokenB.balanceOf(receiver), out);
    }

    function test_permit2ExactOut() public {
        uint256 exactOut = 100_000e6;
        uint256 plannedIn = pool.getAmountIn(address(tokenA), address(tokenB), exactOut);

        RouteParams memory p = _propRoute(plannedIn, true);
        (IPermit2.PermitTransferFrom memory permit, bytes memory sig) =
            _signPermit(address(tokenA), plannedIn, 1, block.timestamp + 1);

        vm.prank(user);
        (uint256 spent, uint256 out) = router.swapExactOutWithPermit2(p, exactOut, plannedIn, permit, sig);
        assertGe(out, exactOut);
        assertLe(spent, plannedIn);
    }

    /// @notice `owner` is hard-wired to `msg.sender`, so an observed signature cannot be replayed
    ///         by a third party into their own route with themselves as receiver.
    function test_permit2SignatureCannotBeReplayedByAnotherCaller() public {
        uint256 amountIn = 100e18;
        (IPermit2.PermitTransferFrom memory permit, bytes memory sig) =
            _signPermit(address(tokenA), amountIn, 2, block.timestamp + 1);

        address attacker = makeAddr("attacker");
        RouteParams memory p = _propRoute(amountIn, true);
        p.receiver = attacker;

        vm.prank(attacker);
        vm.expectRevert(MockPermit2.InvalidSigner.selector);
        router.swapExactInWithPermit2(p, 0, permit, sig);
    }

    function test_permit2RejectsAMismatchedToken() public {
        (IPermit2.PermitTransferFrom memory permit, bytes memory sig) =
            _signPermit(address(tokenB), 100e18, 3, block.timestamp + 1);

        RouteParams memory p = _propRoute(100e18, true); // tokenIn == tokenA
        vm.prank(user);
        vm.expectRevert(abi.encodeWithSelector(Router.PermitTokenMismatch.selector, address(tokenB), address(tokenA)));
        router.swapExactInWithPermit2(p, 0, permit, sig);
    }

    function test_permit2RejectsAnUndersizedPermit() public {
        (IPermit2.PermitTransferFrom memory permit, bytes memory sig) =
            _signPermit(address(tokenA), 99e18, 4, block.timestamp + 1);

        RouteParams memory p = _propRoute(100e18, true);
        vm.prank(user);
        vm.expectRevert(abi.encodeWithSelector(Router.PermitAmountTooLow.selector, 99e18, 100e18));
        router.swapExactInWithPermit2(p, 0, permit, sig);
    }

    /// @notice A Permit2 that moves nothing and does not revert cannot fund a swap, even when the
    ///         router already holds a stray balance of the input token. The no-op is installed at
    ///         the canonical address and the stray balance is seeded, so the only thing standing
    ///         between the plan and someone else's money is `_pullWithPermit2` verifying the
    ///         transfer by balance delta rather than by the absence of a revert.
    function test_permit2NoOpCannotFundASwapEvenWithAStrayRouterBalance() public {
        _installPermit2(address(new NoOpPermit2()));

        // The stray balance is what makes this test meaningful rather than tautological.
        tokenA.mint(address(router), 500e18);
        uint256 strayBefore = tokenA.balanceOf(address(router));

        RouteParams memory p = _propRoute(100e18, true);
        IPermit2.PermitTransferFrom memory permit = IPermit2.PermitTransferFrom({
            permitted: IPermit2.TokenPermissions({token: address(tokenA), amount: 100e18}),
            nonce: 5,
            deadline: block.timestamp + 1
        });

        vm.prank(user);
        vm.expectRevert(Router.PermitTransferredNothing.selector);
        router.swapExactInWithPermit2(p, 0, permit, hex"");

        assertEq(tokenA.balanceOf(address(router)), strayBefore, "the stray balance was touched");
        assertEq(tokenB.balanceOf(receiver), 0, "a swap happened without any input being pulled");

        // Same for the exact-out entry point, which shares `_pullWithPermit2`.
        vm.prank(user);
        vm.expectRevert(Router.PermitTransferredNothing.selector);
        router.swapExactOutWithPermit2(p, 1, type(uint256).max, permit, hex"");
    }

    /// @notice The approval path is closed against the same class of failure: a token whose
    ///         `transferFrom` returns `true` and moves nothing.
    function test_approvalPathSilentTokenCannotFundASwap() public {
        SilentToken silent = new SilentToken();
        silent.mint(user, 1_000e18);
        vm.prank(user);
        silent.approve(address(router), type(uint256).max);

        // Pre-seed the router, again so the test cannot pass for the wrong reason.
        MockERC20(address(silent)).mint(address(router), 500e18);

        vm.prank(owner);
        pool.addPair(address(silent), address(tokenC), 18, 60, 1e9);

        RouteParams memory p = _oneStepRoute(
            address(silent),
            address(tokenC),
            100e18,
            _step(address(propAdapter), address(pool), 10_000, false, _propPayload(address(silent), address(tokenC), 0))
        );

        vm.prank(user);
        vm.expectRevert(Router.NothingReceived.selector);
        router.swapExactIn(p, 0);
    }

    /// @notice A Permit2 that moves *less* than requested is not treated as if it had moved the
    ///         full amount. Everything downstream is sized from the measured delta, so the route
    ///         executes correctly at the smaller size and the payer is charged only that.
    function test_permit2PartialTransferSizesTheRouteFromWhatActuallyArrived() public {
        _installPermit2(address(new ShortPermit2(1, 2)));
        vm.prank(user);
        tokenA.approve(PERMIT2, type(uint256).max);

        uint256 planned = 100e18;
        uint256 arriving = planned / 2;
        uint256 quoteForArriving = pool.getAmountOut(address(tokenA), address(tokenB), arriving);

        RouteParams memory p = _propRoute(planned, true);
        IPermit2.PermitTransferFrom memory permit = IPermit2.PermitTransferFrom({
            permitted: IPermit2.TokenPermissions({token: address(tokenA), amount: planned}),
            nonce: 6,
            deadline: block.timestamp + 1
        });

        uint256 userBefore = tokenA.balanceOf(user);
        vm.prank(user);
        uint256 out = router.swapExactInWithPermit2(p, quoteForArriving, permit, hex"");

        assertEq(out, quoteForArriving, "route was not sized from the measured delta");
        assertEq(userBefore - tokenA.balanceOf(user), arriving, "payer was charged more than arrived");

        IPropPool.PairSnapshot memory snap = pool.snapshot(PAIR_ID);
        assertEq(uint256(snap.bidUsed), arriving, "the venue received an amount that never arrived");
    }

    /// @notice The complete enumeration behind the claim in the test above: every funding path in
    ///         the router, driven with a funder that moves nothing, must revert. If a new funding
    ///         mode is ever added, this test is where it has to be listed.
    function test_noFundingPathCanReturnWithoutMovingTokens() public {
        RouteParams memory p = _propRoute(100e18, true);
        IPermit2.PermitTransferFrom memory permit = IPermit2.PermitTransferFrom({
            permitted: IPermit2.TokenPermissions({token: address(tokenA), amount: 100e18}),
            nonce: 7,
            deadline: block.timestamp + 1
        });

        // There are exactly four external entry points, funded by exactly two paths.
        // 1 & 2: Permit2 that moves nothing.
        _installPermit2(address(new NoOpPermit2()));
        vm.prank(user);
        vm.expectRevert(Router.PermitTransferredNothing.selector);
        router.swapExactInWithPermit2(p, 0, permit, hex"");
        vm.prank(user);
        vm.expectRevert(Router.PermitTransferredNothing.selector);
        router.swapExactOutWithPermit2(p, 1, type(uint256).max, permit, hex"");

        // 3 & 4: `transferFrom` that moves nothing. Covered against a real Permit2 install so
        // the two paths cannot be confused.
        _installPermit2(address(new MockPermit2()));
        SilentToken silent = new SilentToken();
        silent.mint(user, 1_000e18);
        vm.startPrank(user);
        silent.approve(address(router), type(uint256).max);
        vm.stopPrank();
        vm.prank(owner);
        pool.addPair(address(silent), address(tokenC), 18, 60, 1e9);

        RouteParams memory q = _oneStepRoute(
            address(silent),
            address(tokenC),
            100e18,
            _step(address(propAdapter), address(pool), 10_000, false, _propPayload(address(silent), address(tokenC), 0))
        );
        vm.prank(user);
        vm.expectRevert(Router.NothingReceived.selector);
        router.swapExactIn(q, 0);
        vm.prank(user);
        vm.expectRevert(Router.NothingReceived.selector);
        router.swapExactOut(q, 1, type(uint256).max);

        // An address with no code cannot fund anything either, and it fails earlier and with no
        // error data: `_pull` reads `balanceOf` first, a `call` to a codeless account succeeds with
        // empty returndata, and the ABI decoder rejects it. The revert is pinned rather than a
        // zero, because a `balanceOf` decoding to 0 lands in `NothingReceived` — safe, but a
        // different path — and a `staticcall` variant swallowing the decode error would not be.
        RouteParams memory r = _oneStepRoute(
            makeAddr("notAToken"),
            address(tokenC),
            100e18,
            _step(address(propAdapter), address(pool), 10_000, false, _propPayload(address(tokenA), address(tokenC), 0))
        );
        vm.prank(user);
        vm.expectRevert();
        router.swapExactIn(r, 0);
    }

    // --- Route validation ---

    function test_routeValidation() public {
        vm.startPrank(user);

        RouteParams memory p = _propRoute(100e18, true);
        p.receiver = address(0);
        vm.expectRevert(Router.ZeroReceiver.selector);
        router.swapExactIn(p, 0);

        p = _propRoute(100e18, true);
        p.tokenOut = p.tokenIn;
        vm.expectRevert(Router.IdenticalTokens.selector);
        router.swapExactIn(p, 0);

        p = _propRoute(100e18, true);
        p.amountIn = 0;
        vm.expectRevert(Router.ZeroAmountIn.selector);
        router.swapExactIn(p, 0);

        p = _propRoute(100e18, true);
        p.batches = new Batch[](0);
        vm.expectRevert(Router.EmptyRoute.selector);
        router.swapExactIn(p, 0);

        p = _propRoute(100e18, true);
        p.batches[0].hops = new Hop[](0);
        vm.expectRevert(Router.EmptyBatch.selector);
        router.swapExactIn(p, 0);

        p = _propRoute(100e18, true);
        p.batches[0].hops[0].steps = new SwapStep[](0);
        vm.expectRevert(Router.EmptyHop.selector);
        router.swapExactIn(p, 0);

        p = _propRoute(100e18, true);
        p.batches[0].hops[0].tokenIn = address(tokenC);
        vm.expectRevert(abi.encodeWithSelector(Router.BatchTokenMismatch.selector, address(tokenA), address(tokenC)));
        router.swapExactIn(p, 0);

        vm.stopPrank();
    }

    function test_reservedBitsInAStepAreRejected() public {
        RouteParams memory p = _propRoute(100e18, true);
        p.batches[0].hops[0].steps[0].rawData |= uint256(1) << 200; // inside 253..176

        vm.prank(user);
        vm.expectRevert(
            abi.encodeWithSelector(RouteDecoder.ReservedBitsSet.selector, p.batches[0].hops[0].steps[0].rawData)
        );
        router.swapExactIn(p, 0);
    }

    function test_zeroAdapterIsRejected() public {
        RouteParams memory p = _propRoute(100e18, true);
        p.batches[0].hops[0].steps[0].adapter = address(0);

        vm.prank(user);
        vm.expectRevert(Router.ZeroAdapter.selector);
        router.swapExactIn(p, 0);
    }

    // --- Auditability ---

    /// @notice `RouteExecuted` publishes realised next to quoted, which is the whole neutrality
    ///         argument in archi_v2.md §6.3. Pin the field order and the plan hash.
    function test_routeExecutedPublishesRealisedNextToQuoted() public {
        uint256 amountIn = 100e18;
        uint256 quoted = pool.getAmountOut(address(tokenA), address(tokenB), amountIn);

        RouteParams memory p = _propRoute(amountIn, true);
        p.quotedAmountOut = quoted;

        bytes memory callData = abi.encodeCall(Router.swapExactIn, (p, quoted));

        vm.expectEmit(true, true, true, true, address(router));
        emit Router.RouteExecuted(
            user, address(tokenA), address(tokenB), receiver, amountIn, quoted, quoted, keccak256(callData)
        );

        vm.prank(user);
        (bool ok,) = address(router).call(callData);
        assertTrue(ok);
    }
}
