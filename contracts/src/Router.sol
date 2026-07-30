// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {IAdapter, IERC20Minimal} from "./interfaces/IAdapter.sol";
import {RouteDecoder} from "./libraries/RouteDecoder.sol";
import {SafeTransfer} from "./libraries/SafeTransfer.sol";

/// @notice The slice of canonical Permit2 this router uses.
/// @dev Permit2 is a genesis pre-install on GIWA at 0x000000000022D473030F116dDEE9F6B43aC78BA3,
///      byte-identical to Uniswap's deployment elsewhere, so a minimal local interface is safe:
///      the tuple types fix the selector and the struct names are irrelevant to the ABI.
///
///      Only the **SignatureTransfer** half is declared. AllowanceTransfer would require a standing
///      Permit2 allowance, the persistent approval this design avoids everywhere else.
interface IPermit2 {
    struct TokenPermissions {
        address token;
        uint256 amount;
    }

    struct PermitTransferFrom {
        TokenPermissions permitted;
        uint256 nonce;
        uint256 deadline;
    }

    struct SignatureTransferDetails {
        address to;
        uint256 requestedAmount;
    }

    /// @dev Verifies `signature` over (permitted, spender = msg.sender, nonce, deadline) against
    ///      `owner`, then pulls `transferDetails.requestedAmount` from `owner` to
    ///      `transferDetails.to`. Reverts on a bad signature, a spent nonce, an expired deadline,
    ///      or `requestedAmount > permitted.amount`.
    function permitTransferFrom(
        PermitTransferFrom memory permit,
        SignatureTransferDetails calldata transferDetails,
        address owner,
        bytes calldata signature
    ) external;
}

// --- Route shape ---------------------------------------------------------
//
//   RouteParams
//     └─ Batch[]      parallel, weighted split of the total input
//          └─ Hop[]   sequential; the output of hop k funds hop k+1
//               └─ SwapStep[]  parallel forks inside one hop, weighted
//
// Declared at file level so tests, scripts and the off-chain planner can import them without
// pulling in the contract.

/// @notice One leg: one venue, one adapter, one weight.
struct SwapStep {
    /// @dev Stateless singleton implementing `IAdapter`. Supplied by the caller and NOT
    ///      allowlisted — see the trust note on `IAdapter`.
    address adapter;
    /// @dev Packed per `RouteDecoder`: reverse | fundAdapter | weight | pool.
    uint256 rawData;
    /// @dev Adapter-defined. Empty for `UniV2Adapter`.
    bytes payload;
}

/// @notice One sequential step of a batch. Every fork in a hop consumes `tokenIn`.
struct Hop {
    address tokenIn;
    SwapStep[] steps;
}

/// @notice One independent path from `tokenIn` to `tokenOut`.
struct Batch {
    /// @dev Share of the route's total input, in basis points of `RouteDecoder.WEIGHT_DENOMINATOR`.
    ///      Same at-most-10_000 rule and same remainder handling as fork weights.
    uint16 weightBps;
    Hop[] hops;
}

/// @notice The complete execution plan. Produced off-chain; the router does not alter it.
struct RouteParams {
    address tokenIn;
    address tokenOut;
    address receiver;
    uint256 amountIn;
    /// @dev What the off-chain quoter promised for `amountIn` at plan time. Not a constraint —
    ///      slippage is enforced by `minAmountOut` / `exactAmountOut`. It is emitted so that
    ///      quoted-versus-realised is a matter of public on-chain record. See `RouteExecuted`.
    uint256 quotedAmountOut;
    uint256 deadline;
    Batch[] batches;
}

/// @title Router
/// @notice Aggregation router for GIWA. A pure executor of a plan supplied in calldata.
///
/// **No path finding on-chain.** The router never chooses a venue, compares quotes or re-splits; it
/// receives a plan and executes it. Path finding lives in the off-chain engine, where candidates
/// can be validated with `eth_simulateV1` and ranked on gas-adjusted net output; on chain it would
/// make the router's behaviour depend on state it cannot verify.
///
/// **What the router does guarantee:**
///
///  * *Intermediate amounts are measured, not asserted.* Between hops the router re-reads its own
///    ERC-20 balance; a plan built one block earlier is already wrong about it.
///  * *Slippage and deadline are enforced on the aggregate*, across every batch, at the end — not
///    leg by leg, where a favourable leg could paper over a bad one.
///  * *Routing quality is auditable.* `RouteExecuted` carries realised next to quoted output. A
///    team running both an aggregator and a prop AMM will be accused of self-preferencing, and the
///    answer has to be a public record anyone can difference; 0x measured prop-AMM quote spoofing
///    at 5-10bp per fill.
///
/// **What it does not do:**
///
///  * No commission, no referral fee, no positive-slippage capture. A v1.x fee goes in a separate
///    module in front of this contract.
///  * No owner, no pause, no upgrade, no token rescue: nothing to seize and nothing to stop. The
///    flip side is that dust stranded by a malformed plan is unrecoverable — see
///    `RouteDecoder.validateWeightSum`.
///  * No native ETH; GIWA pre-installs WETH9 at 0x4200...0006, so wrap before calling. A payable
///    path means an ETH refund path, which means a reentrancy surface, for one hop of convenience.
///  * No standing approvals. Tokens are pushed to the venue and the venue measures its own delta.
contract Router {
    using RouteDecoder for uint256;

    // --- Errors ---

    error DeadlineExpired(uint256 deadline, uint256 nowTs);
    error ZeroReceiver();
    error ZeroToken();
    error IdenticalTokens();
    error ZeroAmountIn();
    error EmptyRoute();
    error EmptyBatch();
    error EmptyHop();
    error ZeroAdapter();
    error ZeroShare();
    error HopProducedNothing(uint256 batchIndex, uint256 hopIndex);
    error BatchTokenMismatch(address expected, address actual);
    error NothingReceived();
    error ZeroOutput();
    error InsufficientOutput(uint256 amountOut, uint256 required);
    error ExcessiveInput(uint256 amountIn, uint256 limit);
    error PermitTokenMismatch(address permitted, address tokenIn);
    error PermitAmountTooLow(uint256 permitted, uint256 amountIn);
    error PermitTransferredNothing();
    error TokenCallFailed(address token);
    error Reentrancy();

    // --- Events ---

    /// @notice One completed route.
    /// @param amountIn        input actually consumed, net of any refund.
    /// @param amountOut       output actually delivered, measured as a balance delta.
    /// @param quotedAmountOut what the off-chain quoter promised when the plan was built.
    /// @param planHash        `keccak256` of this call's whole calldata, pinning the exact plan —
    ///                        every venue, weight and payload — to this execution. The calldata is
    ///                        already public; the hash is what an indexer keys on when reconciling
    ///                        `amountOut` against `quotedAmountOut` fill by fill.
    /// @dev `amountOut` and `quotedAmountOut` are emitted side by side so that a persistent
    ///      negative gap on routes through our own prop pool is published by the contract rather
    ///      than discovered by someone else.
    event RouteExecuted(
        address indexed sender,
        address indexed tokenIn,
        address indexed tokenOut,
        address receiver,
        uint256 amountIn,
        uint256 amountOut,
        uint256 quotedAmountOut,
        bytes32 planHash
    );

    // --- Constants ---

    /// @notice Canonical Permit2. Genesis pre-install on GIWA, same address as every other chain.
    IPermit2 public constant PERMIT2 = IPermit2(0x000000000022D473030F116dDEE9F6B43aC78BA3);

    // --- Reentrancy ---

    /// @dev Transient (EIP-1153) rather than a storage slot: the flag is meaningless across
    ///      transactions, and TSTORE costs 100 gas against a warm SSTORE's 2_900 on a path that
    ///      pays it twice per swap. The guard is not optional — adapter addresses come from
    ///      calldata, so every route hands execution to a contract of the caller's choosing while
    ///      the router is mid-flight and holding funds, and every accounting decision here is a
    ///      balance read, so a re-entrant adapter would leave the outer route measuring a delta
    ///      that had already been spent.
    bool private transient _entered;

    /// @dev Body split into functions rather than inline: a modifier is copied into every one of
    ///      the four entry points that uses it.
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

    // --- Entry points — approval based ---

    /// @notice Swap exactly `p.amountIn` for at least `minAmountOut`.
    /// @dev Requires a prior ERC-20 approval to this contract. Use the Permit2 variant to skip it.
    function swapExactIn(RouteParams calldata p, uint256 minAmountOut)
        external
        nonReentrant
        returns (uint256 amountOut)
    {
        (uint256 tokenInBefore, uint256 received) = _pull(p);
        (, amountOut) = _run(p, tokenInBefore, received, minAmountOut, type(uint256).max);
    }

    /// @notice Swap for at least `exactAmountOut`, spending no more than `maxAmountIn`.
    ///
    /// @dev The semantics are weaker than the name suggests, structurally. `IAdapter` has no
    ///      amount parameter, so the router cannot ask a venue for a specific output, only push an
    ///      input and see what comes back. Exact-output therefore means: **the off-chain planner
    ///      computed the input** (`p.amountIn`, inverted through each venue's curve), the router
    ///      spends at most that, then checks output at least `exactAmountOut` and input at most
    ///      `maxAmountIn`. An underestimate reverts rather than under-delivering; an overestimate
    ///      is delivered to `receiver` as extra output, and only unspent *input* is refunded.
    ///
    ///      Inverse math per venue in the router would put venue-specific curve code back on chain,
    ///      which the adapter split exists to avoid, and is not even possible for the prop AMM,
    ///      whose ladder can move between the quote and the fill.
    ///
    /// @return amountIn  input actually consumed.
    /// @return amountOut output delivered; `>= exactAmountOut`.
    function swapExactOut(RouteParams calldata p, uint256 exactAmountOut, uint256 maxAmountIn)
        external
        nonReentrant
        returns (uint256 amountIn, uint256 amountOut)
    {
        // Checked before the pull so an inflated plan cannot even move the user's tokens.
        if (p.amountIn > maxAmountIn) revert ExcessiveInput(p.amountIn, maxAmountIn);
        (uint256 tokenInBefore, uint256 received) = _pull(p);
        (amountIn, amountOut) = _run(p, tokenInBefore, received, exactAmountOut, maxAmountIn);
    }

    // --- Entry points — Permit2 ---

    /// @notice `swapExactIn` funded by a Permit2 signature instead of an ERC-20 approval.
    /// @param permit    the user's `PermitTransferFrom`. `permitted.token` must be `p.tokenIn` and
    ///                  `permitted.amount` must cover `p.amountIn`.
    /// @param signature EIP-712 signature over `permit` with `spender = address(this)`.
    function swapExactInWithPermit2(
        RouteParams calldata p,
        uint256 minAmountOut,
        IPermit2.PermitTransferFrom calldata permit,
        bytes calldata signature
    ) external nonReentrant returns (uint256 amountOut) {
        (uint256 tokenInBefore, uint256 received) = _pullWithPermit2(p, permit, signature);
        (, amountOut) = _run(p, tokenInBefore, received, minAmountOut, type(uint256).max);
    }

    /// @notice `swapExactOut` funded by a Permit2 signature instead of an ERC-20 approval.
    function swapExactOutWithPermit2(
        RouteParams calldata p,
        uint256 exactAmountOut,
        uint256 maxAmountIn,
        IPermit2.PermitTransferFrom calldata permit,
        bytes calldata signature
    ) external nonReentrant returns (uint256 amountIn, uint256 amountOut) {
        if (p.amountIn > maxAmountIn) revert ExcessiveInput(p.amountIn, maxAmountIn);
        (uint256 tokenInBefore, uint256 received) = _pullWithPermit2(p, permit, signature);
        (amountIn, amountOut) = _run(p, tokenInBefore, received, exactAmountOut, maxAmountIn);
    }

    // --- Funding ---

    /// @dev Reports what actually landed, not what was asked for: splitting `p.amountIn` when a
    ///      fee-on-transfer token delivered 99% would have the last leg spend money it lacks.
    function _pull(RouteParams calldata p) private returns (uint256 tokenInBefore, uint256 received) {
        _validate(p);
        tokenInBefore = _balanceOf(p.tokenIn);
        _safeTransferFrom(p.tokenIn, msg.sender, address(this), p.amountIn);
        received = _balanceOf(p.tokenIn) - tokenInBefore;
        if (received == 0) revert NothingReceived();
    }

    /// @dev Two properties make this path safe.
    ///
    ///       1. **`owner` is hard-wired to `msg.sender`.** A bare `PermitTransferFrom` carries no
    ///          witness, so its signature authorises *this spender* to move the tokens anywhere it
    ///          likes; a caller-named `owner` would let anyone observing Alice's signature in the
    ///          mempool replay it inside their own route with themselves as `receiver`. The cost is
    ///          that v1 supports no relayed flow — that needs `permitWitnessTransferFrom` with the
    ///          plan hash as the witness, a frontend-signing change.
    ///
    ///       2. **The transfer is verified by balance delta**, not by the absence of a revert. A
    ///          Permit2 that returned without moving anything is caught here rather than silently
    ///          swapping a stale balance.
    function _pullWithPermit2(
        RouteParams calldata p,
        IPermit2.PermitTransferFrom calldata permit,
        bytes calldata signature
    ) private returns (uint256 tokenInBefore, uint256 received) {
        _validate(p);
        if (permit.permitted.token != p.tokenIn) revert PermitTokenMismatch(permit.permitted.token, p.tokenIn);
        if (permit.permitted.amount < p.amountIn) revert PermitAmountTooLow(permit.permitted.amount, p.amountIn);

        tokenInBefore = _balanceOf(p.tokenIn);
        PERMIT2.permitTransferFrom(
            permit,
            IPermit2.SignatureTransferDetails({to: address(this), requestedAmount: p.amountIn}),
            msg.sender,
            signature
        );
        received = _balanceOf(p.tokenIn) - tokenInBefore;
        if (received == 0) revert PermitTransferredNothing();
    }

    // --- Execution ---

    function _validate(RouteParams calldata p) private view {
        if (block.timestamp > p.deadline) revert DeadlineExpired(p.deadline, block.timestamp);
        if (p.receiver == address(0)) revert ZeroReceiver();
        if (p.tokenIn == address(0) || p.tokenOut == address(0)) revert ZeroToken();
        // Output is measured as a balance delta on `tokenOut`; if it were also `tokenIn`, the pull
        // and the refund would both land inside that delta and the accounting would be nonsense.
        if (p.tokenIn == p.tokenOut) revert IdenticalTokens();
        if (p.amountIn == 0) revert ZeroAmountIn();
        if (p.batches.length == 0) revert EmptyRoute();
    }

    /// @dev Execute, enforce the caller's bounds, pay out, publish.
    /// @param minOut lower bound on realised output (`minAmountOut`, or `exactAmountOut`).
    /// @param maxIn  upper bound on realised input (`type(uint256).max` for exact-in).
    function _run(RouteParams calldata p, uint256 tokenInBefore, uint256 received, uint256 minOut, uint256 maxIn)
        private
        returns (uint256 spent, uint256 amountOut)
    {
        (spent, amountOut) = _execute(p, tokenInBefore, received);

        if (amountOut == 0) revert ZeroOutput();
        if (amountOut < minOut) revert InsufficientOutput(amountOut, minOut);
        if (spent > maxIn) revert ExcessiveInput(spent, maxIn);

        _safeTransfer(p.tokenOut, p.receiver, amountOut);

        emit RouteExecuted(
            msg.sender, p.tokenIn, p.tokenOut, p.receiver, spent, amountOut, p.quotedAmountOut, keccak256(msg.data)
        );
    }

    /// @dev Split `received` across batches, run each, then measure what came back.
    ///
    ///      Output is a balance delta on the router, not on `receiver`, and every hop delivers to
    ///      the router. Delivering the last hop straight to `receiver` would save a transfer, but
    ///      the delta could then be inflated by anything else crediting `receiver` in the same
    ///      call. Paying out once keeps the slippage check's number equal to the receiver's.
    function _execute(RouteParams calldata p, uint256 tokenInBefore, uint256 received)
        private
        returns (uint256 spent, uint256 amountOut)
    {
        uint256 tokenOutBefore = _balanceOf(p.tokenOut);

        Batch[] calldata batches = p.batches;
        uint256 n = batches.length;

        uint256 totalWeight;
        for (uint256 i; i < n; ++i) {
            totalWeight += batches[i].weightBps;
        }
        RouteDecoder.validateWeightSum(totalWeight);

        uint256 allocated;
        for (uint256 i; i < n; ++i) {
            uint256 share = (i + 1 == n && totalWeight == RouteDecoder.WEIGHT_DENOMINATOR)
                ? received - allocated
                : RouteDecoder.shareOf(received, batches[i].weightBps);
            if (share == 0) revert ZeroShare();
            allocated += share;
            _runBatch(batches[i], p.tokenIn, share, i);
        }

        amountOut = _balanceOf(p.tokenOut) - tokenOutBefore;

        // Anything of `tokenIn` above the pre-pull balance is unspent: rounding dust, or a
        // deliberately under-allocated batch weight. It goes back to the payer, never to
        // `receiver`, and never to the router's own balance sheet.
        uint256 leftover = _balanceOf(p.tokenIn) - tokenInBefore;
        if (leftover != 0) _safeTransfer(p.tokenIn, msg.sender, leftover);
        spent = received - leftover;
    }

    /// @dev One path: hops in sequence, each funded by what the previous one actually produced.
    function _runBatch(Batch calldata b, address routeTokenIn, uint256 amountIn, uint256 batchIndex) private {
        Hop[] calldata hops = b.hops;
        uint256 n = hops.length;
        if (n == 0) revert EmptyBatch();
        if (hops[0].tokenIn != routeTokenIn) revert BatchTokenMismatch(routeTokenIn, hops[0].tokenIn);

        uint256 available = amountIn;
        for (uint256 h; h < n; ++h) {
            bool hasNext = h + 1 < n;
            // Measure the delta in the token the *next* hop consumes rather than trusting the
            // plan's intermediate amount: the ladder may have been re-pushed or the V2 pair traded
            // since the plan was built. A delta also passes on only what *this* hop added, so
            // pre-existing dust and other batches do not leak in.
            address nextToken = hasNext ? hops[h + 1].tokenIn : address(0);
            uint256 nextBefore = hasNext ? _balanceOf(nextToken) : 0;

            _runHop(hops[h], available);

            if (hasNext) {
                available = _balanceOf(nextToken) - nextBefore;
                if (available == 0) revert HopProducedNothing(batchIndex, h);
            }
        }
    }

    /// @dev One hop: fund each fork, then hand control to its adapter.
    function _runHop(Hop calldata hop, uint256 available) private {
        SwapStep[] calldata steps = hop.steps;
        uint256 n = steps.length;
        if (n == 0) revert EmptyHop();

        uint256 totalWeight;
        for (uint256 i; i < n; ++i) {
            RouteDecoder.validateStep(steps[i].rawData);
            totalWeight += steps[i].rawData.weight();
        }
        RouteDecoder.validateWeightSum(totalWeight);

        uint256 allocated;
        for (uint256 i; i < n; ++i) {
            uint256 rawData = steps[i].rawData;
            address adapter = steps[i].adapter;
            if (adapter == address(0)) revert ZeroAdapter();

            address pool = rawData.pool();

            // Remainder rule, documented in full on RouteDecoder.validateWeightSum: the last fork
            // sweeps rounding dust only when the caller asked for the whole input to be routed.
            uint256 share = (i + 1 == n && totalWeight == RouteDecoder.WEIGHT_DENOMINATOR)
                ? available - allocated
                : RouteDecoder.shareOf(available, rawData.weight());
            if (share == 0) revert ZeroShare();
            allocated += share;

            // Push first, call second. The venue (or, rarely, the adapter) discovers the size by
            // reading its own balance, so there is no second number to disagree with the transfer.
            _safeTransfer(hop.tokenIn, rawData.fundsAdapter() ? adapter : pool, share);

            if (rawData.isReverse()) {
                IAdapter(adapter).sellQuote(address(this), pool, steps[i].payload);
            } else {
                IAdapter(adapter).sellBase(address(this), pool, steps[i].payload);
            }
        }
    }

    // --- ERC-20 plumbing ---

    function _balanceOf(address token) private view returns (uint256) {
        return IERC20Minimal(token).balanceOf(address(this));
    }

    /// @dev Tolerates tokens that return nothing (USDT-shaped) while still rejecting a `false`
    ///      return. The `code.length` check stops the real failure mode: a call to an address with
    ///      no code succeeds with empty returndata, which the return-value check reads as success.
    function _safeTransfer(address token, address to, uint256 amount) private {
        if (token.code.length == 0) revert TokenCallFailed(token);
        (bool ok, bytes memory ret) = token.call(abi.encodeCall(IERC20Minimal.transfer, (to, amount)));
        if (!ok || (ret.length != 0 && !abi.decode(ret, (bool)))) revert TokenCallFailed(token);
    }

    function _safeTransferFrom(address token, address from, address to, uint256 amount) private {
        if (token.code.length == 0) revert TokenCallFailed(token);
        (bool ok, bytes memory ret) = token.call(abi.encodeCall(IERC20Minimal.transferFrom, (from, to, amount)));
        if (!ok || (ret.length != 0 && !abi.decode(ret, (bool)))) revert TokenCallFailed(token);
    }
}
