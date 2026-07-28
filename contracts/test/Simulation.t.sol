// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {Test} from "forge-std/Test.sol";
import {console2} from "forge-std/console2.sol";

import {FlowModel} from "../script/lib/FlowModel.sol";

import {PropPool} from "../src/PropPool.sol";
import {IPropPool} from "../src/interfaces/IPropPool.sol";
import {PropCurve} from "../src/libraries/PropCurve.sol";
import {MockERC20} from "../src/mocks/MockERC20.sol";

/// @title Simulation — MODE 1: the whole taker flow, in-EVM, deterministic, free
///
/// ```
/// forge test --match-path test/Simulation.t.sol -vv
/// make simulate-dry    # MODE 2: the same model against LIVE GIWA state, sends nothing
/// ```
///
/// This is the mode that belongs in CI. It deploys a `PropPool` shaped exactly like the live
/// pairId 1 (18/6 decimals, `priceScaleExp` 24, 5 bp / 25 bp ladder, 1,000 mWETH of capacity per
/// side per epoch), drives a scripted reference path second by second, runs a quoting bot that
/// mirrors `dubu-updater`'s trigger policy *including its transaction latency*, and lets three
/// taker populations trade against it. Every number is reproducible from a seed.
///
/// =====================================================================================
/// WHY THIS EXISTS
/// =====================================================================================
///
/// `test/Integration.t.sol` and `script/Demo.s.sol` both end with the same disclaimer:
/// "adverse selection is not modelled here, at all". They measure execution quality for an
/// honest taker against a reference mid that is right by fiat. That is a real number and it is
/// the number a taker cares about, but it says nothing about whether the *market maker* makes
/// money, because a reference that is never wrong cannot be picked off.
///
/// The live pool has had 24 fills, all of them ours, all from a deployment demo. There is no
/// markout data. This file manufactures the data it can — from a stated model, with stated
/// parameters — so that the question "does 5 bp cover the adverse selection" has an answer with
/// a derivation attached instead of a shrug.
///
/// =====================================================================================
/// TWO CLOCKS — what `-vv` output from this file does and does not entitle you to say
/// =====================================================================================
///
/// The loop below runs on a **model tick** of `p.tickMs` milliseconds (default 200) and calls
/// `vm.warp` once per `1000 / tickMs` ticks, because `block.timestamp` is denominated in whole
/// seconds and no cheatcode changes that. `FlowModel`'s header states the design; the operational
/// consequence for a reader of this file is one sentence:
///
///   **latency conclusions here are good to 200 ms; staleness conclusions are good to 1 s.**
///
/// Anything that comes out of the harness — how long the updater takes to requote, how long the
/// withdrawal takes to land, when inside a second the searcher gets in — is measured at the tick.
/// Anything the *contract* computes from `block.timestamp` — `maxStaleSecs`, the `decaySecs`
/// capacity ramp, `updatedAt` — is measured at the second, at every `tickMs`, and a run at 200 ms
/// says nothing new about it. The `decaySecs = 60` ramp in `_defended()` is a 60-step staircase
/// whether the tick is 1,000 ms or 200 ms.
///
/// =====================================================================================
/// THE ORDERING INSIDE ONE MODEL TICK, WHICH IS WHERE THE ANSWER LIVES
/// =====================================================================================
///
/// ```text
///   0. block time advances, but only on the ticks that cross a second boundary
///   1. the reference moves to ref[k]
///   2. NAV observation A          -> revaluation only; trade PnL must be exactly 0
///   3. the LATENCY-ADVANTAGED taker acts, but only when an update is landing this
///      tick: it takes the last look at the about-to-be-replaced ladder
///   4. the updater's pending updateQuote lands, if it is due
///   5. the INFORMED taker acts, per the per-tick hazard, against the ladder now on chain
///   6. the UNINFORMED taker acts, per its Bernoulli arrival, indifferent to price
///   7. NAV observation B          -> trade PnL only; revaluation must be exactly 0
///   8. the updater evaluates its triggers and may schedule a push for k + latency ticks
/// ```
///
/// Step 3 is the whole point of separating the fast taker from the informed one, and step 8's
/// latency is the whole point of the exercise: the pool's exposure is the product of how wrong
/// its quote can get and how long it stays wrong. `dubu-updater` runs at zero priority fee by
/// design (`tx.rs` argues why at length, and at 0.001 gwei it is right about the economics), so
/// on a sequencer that orders by fee, being outbid by a searcher is the ordinary case.
///
/// Step 0 is where the second clock shows: at `tickMs = 200` four ticks out of five leave
/// `block.timestamp` exactly where it was, so four fills out of five in a burst are, to the pool,
/// simultaneous. That is not a modelling shortcut. It is what a 1 s chain does.
///
/// =====================================================================================
/// WHAT IS ASSERTED, AS OPPOSED TO PRINTED
/// =====================================================================================
///
/// A simulator whose parameters were chosen to make the pool look good is worse than no
/// simulator, so almost nothing here asserts a P&L. What the tests assert is that the harness
/// measures what it claims:
///
///  * on a flat reference, informed flow is exactly zero and every fill marks out at the spread;
///  * the NAV decomposition reconciles with the per-fill markout, and a quiet book attributes
///    exactly zero trade PnL — the same property `dubu-updater`'s `risk.rs` is built on;
///  * the arrival function is zero below the taker's cost, monotone, and saturating;
///  * the size distribution really has the tail it claims.
///
/// The economics are *reported*, not asserted, and the sweeps report the answer at several
/// values of every parameter that could be accused of having been chosen.
contract SimulationTest is Test {
    // =====================================================================
    // Fixture — shaped like the live pairId 1
    // =====================================================================

    MockERC20 internal baseToken; // 18 decimals, stands in for mWETH
    MockERC20 internal quoteToken; // 6 decimals, stands in for mUSDC
    PropPool internal pool;

    address internal owner = makeAddr("owner");
    address internal manager = makeAddr("manager");
    address internal updater = makeAddr("updater");
    address internal guardian = makeAddr("guardian");
    address internal taker = makeAddr("taker");

    uint16 internal constant PAIR_ID = 1;

    /// @dev The live deployment's exponent for mWETH/mUSDC. DEPLOYMENTS.md, pairId 1.
    uint8 internal constant PRICE_SCALE_EXP = 24;
    uint256 internal constant SCALE = 1e24;

    /// @notice $2,000 per whole base token, encoded in the pool's price units.
    /// @dev `2000 * 10**6 * 10**24 / 10**18`.
    uint256 internal constant REF_MID = 2_000 * 1e12;

    uint256 internal constant TVL_BASE = 5_000e18; // $10M
    uint256 internal constant TVL_QUOTE = 10_000_000e6; // $10M

    uint32 internal constant MAX_STALE_SECS = 3_600;
    uint256 internal constant T0 = 1_800_000_000;

    /// @notice Live `updater.toml` posts 1,000 mWETH per side per epoch — $2M of risk budget.
    uint96 internal constant CAPACITY = 1_000e18;

    // =====================================================================
    // Per-run mutable state
    //
    // In contract storage rather than threaded through the loop: the run function already sits
    // at the legacy code generator's 16-slot stack limit and `via_ir` is off in this repo's
    // default profile (foundry.toml). A test contract's storage costs nobody anything.
    // =====================================================================

    /// @dev **Every instant in here is a MODEL TICK**, not a second and not a block. The
    ///      conversions happen once, at the top of `_run`, so that no schedule inside the loop has
    ///      to know what a millisecond is and so that a unit confusion cannot hide in one branch.
    struct RunState {
        /// @dev The ladder mid the updater has decided on but which has not landed yet.
        uint256 pendingMid;
        /// @dev Tick at which it lands. 0 means nothing is in flight, which is also the
        ///      `PushInFlight` gate `dubu-updater`'s policy module applies: a pair with an
        ///      unconfirmed transaction is not superseded.
        uint256 pendingLandsAt;
        bool pendingRefresh;
        uint256 lastPushAt;
        /// @dev Tick at which a withdrawal triggered by the jump detector LANDS. Zero means none
        ///      is in flight. Deliberately a separate slot from `pendingLandsAt`: a withdrawal is
        ///      not subject to the ordinary update policy and must not be gated behind a push that
        ///      is already in flight, which is how `jump.rs` treats it.
        uint256 withdrawLandsAt;
        /// @dev Tick at which quotes may be reposted. While this is in the future the pool
        ///      quotes nothing, and every arrival in that window is foregone volume — the cost
        ///      side of this defence, counted rather than assumed away.
        uint256 cooloffUntil;
        uint256 jumpsDetected;
        /// @dev Fills that landed after the detector fired but before the withdrawal took effect.
        ///      This is the number that decides whether the defence is a defence or a wish, and it
        ///      is the number a sub-second clock exists to resolve: at a 1 s tick a 2 s withdrawal
        ///      latency is a two-sample window, and "the searcher got in first" and "the searcher
        ///      got in 1,800 ms before the withdrawal" are the same observation.
        uint256 fillsWhileWithdrawing;
        uint256 arrivalsRefusedInCooloff;
        // --- the clock, resolved once per run ---
        uint256 ticksPerSec;
        uint256 quoteLatencyTicks;
        uint256 withdrawLatencyTicks;
        uint256 cooloffTicks;
        uint256 heartbeatTicks;
        uint256 decisionTicks;
        uint256 detectorTicks;
        // The NAV walk's previous observation. Mirrors `risk.rs`'s `prev`.
        uint256 prevQuote;
        uint256 prevBase;
        uint256 prevFair;
        bool navSeeded;
    }

    RunState internal S;

    // =====================================================================
    // Parameter defaults — every one of them justified, none of them free
    // =====================================================================

    /// @notice The base case, taken from the deployed configuration wherever one exists.
    ///
    /// @dev Where a number could not be read off the deployment it is anchored to something
    ///      external and said so here:
    ///
    ///      * `sigmaE2 = 100` — 1 bp per second. ETHUSDT at ~55% annualised realised vol is
    ///        `0.55 / sqrt(365*24*3600) = 9.8e-5` per second, i.e. 0.98 bp. `dubu-updater`
    ///        prices pairId 1 off ETHUSDT, so this is the volatility of the thing the pool is
    ///        actually quoting, not of the mock token.
    ///      * `uninformedPerHour = 60` — one a minute, which at the Pareto-1 size distribution
    ///        (mean ~$2,035) is ~$122k/hour, ~$2.9M/day. That is a healthy mid-tier L2
    ///        WETH/USDC pool. It is **not** measurable on GIWA: the live pool has had 24 fills,
    ///        all of them ours. This single number decides the verdict more than any other, so
    ///        `test_breakEvenUninformedFlow` sweeps it rather than defending it.
    ///      * `quoteLatencyMs = 2000` — one block to decide plus one to land, at GIWA's 1s
    ///        cadence. `updater.toml` measures newHeads delivery at 904-997ms, so the decision
    ///        is made on information up to a second old before the transaction is even signed.
    ///        This is the number `test_sweep_quoteLatencyAcrossTheSecondBoundary` exists to
    ///        question: GIWA preconfirms flashblocks at ~200 ms, and a bot that polled the
    ///        `pending` tag instead of waiting for `newHeads` would be at ~400 ms instead.
    ///      * `tickMs = 200` — the model clock, matching that flashblock cadence. It is a
    ///        RESOLUTION, not a claim: see the two-clocks note above for what stays quantised at
    ///        one second regardless. Set it to 1,000 to reproduce the pre-tick model exactly.
    ///      * `informedHalfMaxE2 = 2 * halfSpread` — see `FlowModel`'s header. Tied to the pool's
    ///        own parameter so it cannot be tuned to flatter, and swept anyway.
    function _defaults() internal view returns (FlowModel.Params memory p) {
        p.mid = REF_MID;
        p.priceScaleExp = PRICE_SCALE_EXP;
        p.baseDecimals = 18;
        p.quoteDecimals = 6;
        p.horizonSecs = 3_600;
        p.seed = 20_260_727;
        p.tickMs = vm.envOr("TICK_MS", uint256(200));

        p.sigmaE2 = 100; // 1.00 bp/s
        p.driftE2 = 0;
        p.jumpAtSecs = 0;
        p.jumpE2 = 0;

        p.halfSpreadBps = 5;
        p.widthBps = 25;
        p.bidCapacity = CAPACITY;
        p.askCapacity = CAPACITY;
        p.quoteLatencyMs = 2_000;
        p.decisionIntervalMs = 1_000; // newHeads, once a block. NOT the tick.
        p.detectorIntervalMs = 200; // jump.rs already scans at 200ms
        p.adverseDriftE2 = 50; // 0.50 bp, from updater.toml
        p.favourableDriftE2 = 800; // 8.00 bp, from updater.toml
        p.heartbeatSecs = 2_400; // from updater.toml
        p.capacityDivergencePct = 30; // from updater.toml

        p.uninformedPerHour = 60;
        p.informedMaxPerSecE6 = 1_000_000; // one per block; the chain's own ceiling
        p.informedHalfMaxE2 = 1_000; // 10.00 bp == 2 x half-spread
        p.informedCostE2 = 100; // 1.00 bp of gas + unwind
        p.informedCapturePct = 100; // a competitive race
        p.fastHitPct = 50;

        // --- the defences, off by default ---
        //
        // Off is deliberate and is what makes every comparison in this file legible: `_defaults`
        // reproduces the pre-defence pool exactly, so the baseline column of a before/after table
        // is a real run rather than a remembered number. `_defended()` turns them on.
        p.s1E2 = 0;
        p.halfSpreadCapBps = 0;
        p.jumpThresholdE2 = 0;
        p.cooloffSecs = 0;
        p.withdrawLatencyMs = 0;
        p.decaySecs = 0;
    }

    /// @notice The pool as it is configured after the defences, and the values are not free.
    ///
    /// @dev Every one is read off the deployed configuration or derived from this file's own
    ///      sweeps, and every one is overridable from the environment so the sweeps below can be
    ///      re-run without editing code:
    ///
    ///        S1_E2=50         hundredths of a bp of half-spread per bp of sigma. `updater.toml`
    ///                         derives 0.5 from this file's half-spread sweep: the sign flips past
    ///                         ~15 bp against a 100 bp jump, and 5 + 0.5*(5*10) = 30.
    ///        SPREAD_CAP_BPS=50  an unbounded spread in a spike is an outage in disguise.
    ///        JUMP_BPS=25      one-second move that trips the detector. Below the absorption limit
    ///                         a jump is already absorbed and withdrawing buys nothing; this sits
    ///                         above ordinary diffusion, which the baseline showed never produces
    ///                         an informed fill at all.
    ///        COOLOFF=30       from `updater.toml`, half of the volatility tau.
    ///        WITHDRAW_LAT_MS=2000  the withdrawal is a transaction on the same 1s chain the push
    ///                         is. Setting this to 0 models a wish; the sweep shows what it is
    ///                         worth. Milliseconds now, because the interesting question is what
    ///                         happens at 400 — it was `WITHDRAW_LAT` in seconds, and a seconds
    ///                         parameter could not ask it.
    ///        DECAY_SECS=60    PropPool's staleness ramp. Stays in SECONDS at every tick size,
    ///                         because the contract computes it from `block.timestamp`.
    ///        TICK_MS=200      the model clock. 1000 reproduces the pre-tick model exactly and is
    ///                         the regression control; anything finer resolves latency and
    ///                         nothing else. Read on `_defaults`, so it applies to every run.
    ///        JUMP_SIZE_BPS    overrides the scripted jump itself, for the size sweep.
    function _defended() internal view returns (FlowModel.Params memory p) {
        p = _defaults();
        p.s1E2 = vm.envOr("S1_E2", uint256(50));
        p.halfSpreadCapBps = vm.envOr("SPREAD_CAP_BPS", uint256(50));
        p.jumpThresholdE2 = vm.envOr("JUMP_BPS", uint256(25)) * 100;
        p.cooloffSecs = vm.envOr("COOLOFF", uint256(30));
        p.withdrawLatencyMs = vm.envOr("WITHDRAW_LAT_MS", uint256(2_000));
        p.decaySecs = uint16(vm.envOr("DECAY_SECS", uint256(60)));
    }

    function _flat() internal view returns (FlowModel.Params memory p) {
        p = _defaults();
        p.sigmaE2 = 0;
    }

    function _jump() internal view returns (FlowModel.Params memory p) {
        p = _defaults();
        p.jumpAtSecs = p.horizonSecs / 2;
        p.jumpE2 = 10_000; // +100 bp in one second, landing inside a single model tick
    }

    function _trend() internal view returns (FlowModel.Params memory p) {
        p = _defaults();
        p.driftE2 = 20; // +0.20 bp/s == +7.2% over the hour
    }

    /// @dev The jump scenario with the defences on. Same seed, same path, same populations, same
    ///      arrival hazard — only the maker's configuration differs, which is what makes the
    ///      before/after a controlled comparison rather than two runs that happen to be adjacent.
    function _jumpDefended() internal view returns (FlowModel.Params memory p) {
        p = _defended();
        p.jumpAtSecs = p.horizonSecs / 2;
        // forge-lint: disable-next-line(unsafe-typecast)
        p.jumpE2 = int256(vm.envOr("JUMP_SIZE_BPS", uint256(100))) * 100;
    }

    // =====================================================================
    // THE MEASUREMENT — what the defences bought, and what they cost
    // =====================================================================

    /// @notice Baseline against defended, same seed and same path.
    ///
    /// @dev Deliberately asserts almost nothing. The defences either move the numbers or they do
    ///      not, and a threshold picked now would just be this run's own output written down as a
    ///      requirement. The two assertions that ARE here are structural: the defended run must
    ///      not lose MORE than the baseline (a defence that backfires is a bug, not a trade-off),
    ///      and the withdrawal must actually have been raced at least sometimes at a non-zero
    ///      latency — if `fillsWhileWithdrawing` is always zero the model has quietly become the
    ///      wish this exercise exists to avoid.
    function test_defences_beforeAndAfter() public {
        FlowModel.Params memory before = _jump();
        FlowModel.printParams(before, "JUMP - BEFORE, no defences");
        FlowModel.Result memory a = _run(before);
        _report(a, before);
        int256 pnlBefore = _totalPnl(a);

        FlowModel.Params memory p = _jumpDefended();
        FlowModel.printParams(p, "JUMP - AFTER: vol-scaled spread, jump withdrawal, capacity ramp");
        console2.log(
            string.concat(
                "  defences: s1=",
                FlowModel.u2s(p.s1E2),
                "e-2  cap=",
                FlowModel.u2s(p.halfSpreadCapBps),
                "bp  jumpTrip=",
                FlowModel.u2s(p.jumpThresholdE2 / 100),
                "bp  cooloff=",
                FlowModel.u2s(p.cooloffSecs),
                "s  withdrawLat=",
                FlowModel.u2s(p.withdrawLatencyMs),
                "ms  decay=",
                FlowModel.u2s(p.decaySecs),
                "s (quantised to blocks)"
            )
        );
        FlowModel.Result memory b = _run(p);
        _report(b, p);
        int256 pnlAfter = _totalPnl(b);

        console2.log("");
        console2.log("=== BEFORE vs AFTER =========================================");
        console2.log(
            string.concat(
                "  informed fills      ",
                FlowModel.u2s(a.byPop[FlowModel.POP_INFORMED].fills),
                " -> ",
                FlowModel.u2s(b.byPop[FlowModel.POP_INFORMED].fills)
            )
        );
        console2.log(
            string.concat(
                "  uninformed fills    ",
                FlowModel.u2s(a.byPop[FlowModel.POP_UNINFORMED].fills),
                " -> ",
                FlowModel.u2s(b.byPop[FlowModel.POP_UNINFORMED].fills)
            )
        );
        console2.log(
            string.concat(
                "  declined (refused)  ", FlowModel.u2s(a.declinedFills), " -> ", FlowModel.u2s(b.declinedFills)
            )
        );
        console2.log(string.concat("  jumps detected      0 -> ", FlowModel.u2s(S.jumpsDetected)));
        console2.log(string.concat("  fills while withdrawing  ", FlowModel.u2s(S.fillsWhileWithdrawing)));
        console2.log("  total PnL (quote units, signed):");
        console2.logInt(pnlBefore);
        console2.logInt(pnlAfter);
        console2.log("=============================================================");

        assertLe(pnlBefore, pnlAfter, "the defences made it worse, which is a bug and not a trade-off");
    }

    function _totalPnl(FlowModel.Result memory res) internal pure returns (int256 total) {
        for (uint256 i; i < FlowModel.POP_COUNT; ++i) {
            total += res.byPop[i].spread;
        }
    }

    /// @notice Each defence on its own, then all three, against the same jump on the same path.
    ///
    /// @dev `test_defences_beforeAndAfter` gives the headline; this gives the attribution, which
    ///      is the only way to tell a defence that works from one that is being carried by the
    ///      others. Run it at both clocks and compare — the two are directly comparable because
    ///      the path, the seed, the populations and the jump are identical and only the
    ///      resolution differs:
    ///
    ///      ```
    ///      TICK_MS=1000 forge test --match-test test_defenceIsolation -vv   # the control
    ///      TICK_MS=200  forge test --match-test test_defenceIsolation -vv   # the finer clock
    ///      ```
    ///
    ///      Expect the withdrawal and the ramp to look *worse* at 200 ms than at 1,000 ms, and
    ///      expect that to be the honest number rather than a regression. Both are defences whose
    ///      value is measured in how much of a race they win, and a one-second clock could only
    ///      score that race to the nearest second — which rounds a searcher who got in 400 ms
    ///      before the withdrawal into a searcher who did not get in at all.
    function test_defenceIsolation() public {
        _sweepHeader(
            "DEFENCE ISOLATION", "defence", "3600s horizon, +100 bp jump at t=1800, $2M epoch, one defence at a time."
        );
        for (uint256 i; i < 5; ++i) {
            FlowModel.Params memory p = _isolate(i);
            FlowModel.Result memory res = _run(p);
            _sweepRow(_isolateLabel(i), "", res, p);
        }
        console2.log("  The spread($) column is the number to read across the two clocks: it is the P&L");
        console2.log("  booked at fill time, unpolluted by the 60s of diffusion that follows a fill.");
        console2.log("");
        console2.log("  ONE SEED PER ROW, so a row that moves by less than a thousand dollars has not");
        console2.log("  moved. Two of these three defences are worth about that much, which is the point:");
        console2.log("");
        console2.log("  * The capacity ramp is arithmetically incapable of more. It haircuts depth by");
        console2.log("    age/decaySecs, the bot requotes every ~2s, and 2/60 is a 3.3% ceiling on a 3.3%");
        console2.log("    ceiling on a $16k pick-off -- about $520 at the very most, and only if the gap");
        console2.log("    lands at the oldest moment of the cycle. Whether a given run gets 0% or 1.7% is");
        console2.log("    decided by whether the pick-off tick coincides with a push landing, which is");
        console2.log("    phase, not defence. DECAY_SECS=3 makes it bite properly (-$11,213 here, a $4,400");
        console2.log("    saving) and is a different, much more expensive trade: it refuses depth to");
        console2.log("    everyone, all the time, to be ready for a gap that arrives once an hour.");
        console2.log("  * The jump withdrawal does nothing at a 2,000ms withdrawal latency, at either");
        console2.log("    clock, because it loses a race it starts level in -- the detector and the");
        console2.log("    searcher both fire on the tick the gap prints. Sub-second is the only version of");
        console2.log("    it that works; test_theWithdrawalRaceAcrossTheSecondBoundary is that measurement");
        console2.log("    and it is the one thing in this file the finer clock changed the answer to.");
        console2.log("  * The vol-scaled spread is the only row that moves for a reason, and it moves the");
        console2.log("    same ~$1,600 at both clocks -- it raises the absorption limit, which is a");
        console2.log("    property of the ladder and has nothing to do with time at all.");
    }

    /// @dev Built by switching defences OFF a fully defended run rather than ON a bare one, so
    ///      that every row uses the same values — including the environment overrides — and a
    ///      tuned `S1_E2` cannot apply to the combined row while the isolated row silently keeps
    ///      the old default.
    function _isolate(uint256 i) internal view returns (FlowModel.Params memory p) {
        p = _jumpDefended();
        if (i != 1 && i != 4) {
            p.s1E2 = 0;
            p.halfSpreadCapBps = 0;
        }
        if (i != 2 && i != 4) {
            p.jumpThresholdE2 = 0;
            p.cooloffSecs = 0;
            p.withdrawLatencyMs = 0;
        }
        if (i != 3 && i != 4) p.decaySecs = 0;
    }

    function _isolateLabel(uint256 i) internal pure returns (string memory) {
        if (i == 0) return "control (none)";
        if (i == 1) return "vol-scaled spread";
        if (i == 2) return "jump withdrawal";
        if (i == 3) return "capacity ramp";
        return "all three";
    }

    /// @notice **The regression control.** At a 1,000 ms tick this model must be the model it was
    ///         before it had a tick at all — not close, identical.
    ///
    /// @dev The two numbers below are today's measured output, hard-coded, which is a thing this
    ///      file otherwise refuses to do on the grounds that writing down a run's own result turns
    ///      it into a requirement. The exception is deliberate and narrow: these are not claims
    ///      about the pool, they are claims about the *instrument*. Their job is to fail if a
    ///      change to `tickMs` handling alters the experiment rather than its resolution, and a
    ///      tolerance would let exactly the errors worth catching through.
    ///
    ///      Exact equality is available because the rescalings were built to collapse at
    ///      `tickMs = 1000`: `isqrt(1000 * 1000)` is exactly 1,000 so the path amplitude reduces
    ///      to the old `sigma * 1732/1000`; the differenced drift reduces to `driftE2`;
    ///      `informedHazardPerTick` returns the per-second hazard unchanged at one tick per
    ///      second; `uninformedRatePerTick` truncates to the same integer; `msToTicks` maps 2,000
    ///      ms to 2 ticks; and the trailing-window jump detector reaches back exactly one entry.
    ///      Every RNG draw therefore comes from the same modulus in the same order.
    ///
    ///      If this test fails, nothing else in this file means anything until it passes again.
    function test_theOneSecondControlReproducesTheOldModel() public {
        FlowModel.Params memory bare = _jump();
        bare.tickMs = 1_000;
        FlowModel.Result memory a = _run(bare);

        FlowModel.Params memory defended = _jumpDefended();
        defended.tickMs = 1_000;
        FlowModel.Result memory b = _run(defended);

        console2.log("");
        FlowModel.rule();
        console2.log(" ONE-SECOND CONTROL: this model at tickMs=1000 vs the model before it had ticks");
        FlowModel.rule();
        console2.log(
            string.concat("  no defences, expected -15757.945680  got ", FlowModel.signedUnits(_totalPnl(a), 6))
        );
        console2.log(
            string.concat("  all defences, expected -13940.431511  got ", FlowModel.signedUnits(_totalPnl(b), 6))
        );
        FlowModel.rule();

        assertEq(_totalPnl(a), -15_757_945_680, "the 1s control no longer reproduces the pre-tick model (undefended)");
        assertEq(_totalPnl(b), -13_940_431_511, "the 1s control no longer reproduces the pre-tick model (defended)");
        assertEq(a.ticksRun, 3_600, "a 3600s horizon at a 1000ms tick is 3600 ticks");
        assertEq(a.blockSecsAdvanced, 3_600, "the block clock did not advance once per tick at tickMs=1000");
    }

    // =====================================================================
    // MODE 1 — the runs
    // =====================================================================

    /// @notice A flat reference. The control: informed flow cannot exist, so every fill is
    ///         uninformed and every markout must equal the spread.
    function test_simulation_flatReference() public {
        FlowModel.Params memory p = _flat();
        FlowModel.printParams(p, "FLAT - the reference never moves");
        FlowModel.Result memory res = _run(p);
        _report(res, p);

        assertEq(res.byPop[FlowModel.POP_INFORMED].fills, 0, "a correct quote cannot be picked off");
        assertEq(res.byPop[FlowModel.POP_FAST].fills, 0, "nor by a faster taker");
        assertGt(res.byPop[FlowModel.POP_UNINFORMED].fills, 0, "the control run traded nothing");

        // Markout at every horizon equals the spread, because the reference never moved.
        FlowModel.Accum memory u = res.byPop[FlowModel.POP_UNINFORMED];
        assertEq(u.mo60, u.spread, "a flat reference must mark out at exactly the spread");

        // And that spread must be the ladder's, plus the ladder's own impact term. It lands a
        // little ABOVE 5 bp, not at it: nothing here triggers a capacity refresh on a flat
        // reference, so the epoch's usage counters accumulate all hour and every later fill
        // executes further down a 25 bp ladder. That is the curve working, not a measurement
        // error, and it is why the bound is one-sided-generous on the upper end.
        int256 bps = FlowModel.bpsOf(u.spread, u.notional);
        assertGt(bps, 490, "uninformed flow paid less than the half-spread");
        assertLt(bps, 700, "uninformed flow paid implausibly more than the half-spread");
    }

    /// @notice The latency-advantaged taker, isolated. `informedMaxPerSecE6 = 0` removes every
    ///         ordinary informed trader from the chain, so the only thing that can pick the pool
    ///         off is a searcher who outbids the quoting bot's transaction.
    ///
    /// @dev This is the case the pool cannot design its way out of by watching Binance faster.
    ///      `dubu-updater` bids 0.005 gwei of priority fee and `tx.rs` is right that at GIWA's
    ///      prices the whole fee ladder is worth less than a rounding error on one quote — but
    ///      "worth nothing to us" and "worth nothing to a searcher holding a five-figure option"
    ///      are different statements, and only the first one is true.
    function test_theLatencyAdvantagedTakerIsTheWorstCase() public {
        FlowModel.Params memory p = _jump();
        p.horizonSecs = 900;
        p.jumpAtSecs = 450;
        p.informedMaxPerSecE6 = 0; // nobody is watching except the fast taker
        p.fastHitPct = 100;

        FlowModel.printParams(p, "JUMP, with ONLY the latency-advantaged taker enabled");
        FlowModel.Result memory res = _run(p);
        _report(res, p);

        assertEq(res.byPop[FlowModel.POP_INFORMED].fills, 0, "the ordinary informed taker was meant to be off");
        assertGt(res.byPop[FlowModel.POP_FAST].fills, 0, "the latency-advantaged taker never got in");
        assertLt(res.byPop[FlowModel.POP_FAST].mo60, 0, "a pick-off that made the pool money is not a pick-off");
    }

    /// @notice Diffusion only, no jump. The quote is never more than `quoteLatencyMs` stale, so
    ///         the pool is picked off only when the walk moves more than the half-spread inside
    ///         that window — a ~3.5 sigma event at these settings.
    function test_simulation_diffusionOnly() public {
        FlowModel.Params memory p = _defaults();
        FlowModel.printParams(p, "DIFFUSION - 1 bp/s random walk, no jump");
        FlowModel.Result memory res = _run(p);
        _report(res, p);
    }

    /// @notice The path that matters. Adverse selection is concentrated in jumps, so a simulator
    ///         without one is measuring the easy half of the problem.
    function test_simulation_jumpPath() public {
        FlowModel.Params memory p = _jump();
        FlowModel.printParams(p, "JUMP - diffusion plus +100 bp in a single second");
        FlowModel.Result memory res = _run(p);
        _report(res, p);

        console2.log("");
        console2.log("  The jump in one line: the reference gaps 100 bp while the pool's ladder is 2s old.");
        console2.log("  Whoever gets there first buys the pool's entire ask epoch at the pre-jump price.");
        console2.log("  Nothing about the half-spread bounds that loss. The per-epoch CAPACITY does, and");
        console2.log("  it is the only thing that does -- which is what archi_v2 5.4 means by a risk budget.");
        _printAbsorptionRule(p);
    }

    /// @notice The closed form the runs keep landing on, printed next to the run that confirms it.
    ///
    /// @dev A taker clearing the whole ask epoch pays the ladder's *average* price, which is
    ///      `minAsk + width/2` — i.e. `halfSpread + width/2` above the mid the ladder was built
    ///      around. So a reference error of `E` basis points leaves the pool short by
    ///
    ///          loss per unit of exposed depth = E - (halfSpread + width/2)
    ///
    ///      and nothing else in the design touches that number. At the deployed 5 bp / 25 bp that
    ///      absorption limit is 17.5 bp: the pool eats any error below it and pays the full excess
    ///      above it, multiplied by however much depth the epoch had on offer. The 100 bp jump
    ///      above lands at ~83 bp of loss, which is `100 - 17.5` and not a coincidence.
    ///
    ///      This is why `test_sweep_halfSpread` only turns the sign somewhere past 15 bp and why
    ///      `test_sweep_capacityIsTheRiskBudget` is almost exactly linear. It also says what
    ///      "quote a wider ladder" really buys: width raises the absorption limit at half the rate
    ///      the half-spread does, and both buy it by quoting worse to the uninformed flow that was
    ///      paying for everything.
    function _printAbsorptionRule(FlowModel.Params memory p) internal pure {
        uint256 absorbE2 = FlowModel.halfSpreadAt(p) * 100 + (p.widthBps * 100) / 2;
        console2.log("");
        console2.log(
            string.concat(
                "  ABSORPTION LIMIT = halfSpread + width/2 = ",
                // forge-lint: disable-next-line(unsafe-typecast)
                FlowModel.sbps(int256(absorbE2)),
                " bp at this ladder."
            )
        );
        console2.log("  A reference error below that is absorbed and the pool still profits. Above it the");
        console2.log("  pool pays the excess on every unit of depth the epoch had posted. Two levers move");
        console2.log("  the limit -- half-spread and width -- and width moves it at half the rate.");
    }

    /// @notice A persistent one-way drift: the pool is on the wrong side of the market all hour
    ///         and its inventory bleeds in one direction. Different failure mode from a jump.
    function test_simulation_trendPath() public {
        FlowModel.Params memory p = _trend();
        FlowModel.printParams(p, "TREND - +0.20 bp/s, a 7.2% one-way hour");
        FlowModel.Result memory res = _run(p);
        _report(res, p);
    }

    // =====================================================================
    // The sweeps — every parameter that could be accused of having been chosen
    // =====================================================================

    /// @notice **The actionable number.** Capacity is what bounds a pick-off, not the spread, so
    ///         this is where "at what size did it stop covering" gets answered.
    function test_sweep_capacityIsTheRiskBudget() public {
        uint96[5] memory caps = [uint96(1_000e18), uint96(250e18), uint96(50e18), uint96(10e18), uint96(2e18)];
        _sweepHeader("PER-EPOCH CAPACITY", "capacity          $/epoch");
        for (uint256 i; i < caps.length; ++i) {
            FlowModel.Params memory p = _jump();
            p.horizonSecs = 900;
            p.jumpAtSecs = 450;
            p.bidCapacity = caps[i];
            p.askCapacity = caps[i];
            FlowModel.Result memory res = _run(p);
            _sweepRow(
                string.concat(FlowModel.u2s(uint256(caps[i]) / 1e18), " base"),
                string.concat("$", FlowModel.thousands(((uint256(caps[i]) * REF_MID) / SCALE) / 1e6)),
                res,
                p
            );
        }
        console2.log("  Same 100 bp jump, same flow, only the risk budget changes. The loss tracks capacity");
        console2.log("  almost linearly, because the pick-off clears whatever depth is exposed and the size");
        console2.log("  of the error decides the price, not the size of the trade. THIS is the lever: the");
        console2.log("  live configuration posts $2M an epoch, and a single 100 bp gap against it is worth");
        console2.log("  five figures to whoever is watching. archi_v2 5.4 calls capacity a risk budget; this");
        console2.log("  is what that sentence costs when the number is set by how much volume you want.");
    }

    /// @notice The three levers that could pay for a narrower quote, swept **together**.
    ///
    /// @dev Each of these has been argued about separately and the separate answers do not compose,
    ///     which is why they are in one table. `s1` is the volatility coefficient, and lowering it
    ///     is the only way to actually quote near the 1 bp floor in a live market — at the measured
    ///     sigma the term contributes 6.5 of the 7.5 bp posted. The standing objection has been
    ///     that `s1` is jump protection and must not be touched. That objection was measured when
    ///     `s0` was 5 bp, the epoch was $2M and the width 25 bp, and **every one of those has since
    ///     changed**, so it is re-measured here rather than re-quoted.
    ///
    ///     Capacity is the lever the arithmetic says should dominate: loss is
    ///     `(jump - absorption) x exposed depth`, absorption moves by a few bp across any
    ///     realistic spread, and depth is set outright by configuration.
    ///
    ///     Width is included because it is the one that looks like a defence and is not. Against a
    ///     100 bp gap every rung of a 25 bp ladder is still profitable to the taker, so the ladder
    ///     is swept whole either way and the width only decides the average price paid — it buys
    ///     nothing back and costs honest size on every ordinary trade.
    function test_sweep_theThreeLeversTogether() public {
        uint256[3] memory s1s = [uint256(58), 30, 0];
        uint96[3] memory caps = [uint96(1_000e18), uint96(250e18), uint96(50e18)];
        uint256[2] memory widths = [uint256(25), 8];

        _sweepHeader("s1 x CAPACITY x WIDTH", "s1 / epoch / width");
        for (uint256 a; a < s1s.length; ++a) {
            for (uint256 b; b < caps.length; ++b) {
                for (uint256 c; c < widths.length; ++c) {
                    FlowModel.Params memory p = _jump();
                    p.horizonSecs = 900;
                    p.jumpAtSecs = 450;
                    p.s1E2 = s1s[a];
                    p.halfSpreadBps = 1;
                    p.bidCapacity = caps[b];
                    p.askCapacity = caps[b];
                    p.widthBps = widths[c];
                    FlowModel.Result memory res = _run(p);
                    _sweepRow(
                        string.concat(
                            "s1=", FlowModel.u2s(s1s[a]),
                            " cap=", FlowModel.u2s(uint256(caps[b]) / 1e18),
                            " w=", FlowModel.u2s(widths[c])
                        ),
                        string.concat("$", FlowModel.thousands(((uint256(caps[b]) * REF_MID) / SCALE) / 1e6)),
                        res,
                        p
                    );
                }
            }
        }
        console2.log("  s0 is 1 bp on every row, so the posted half-spread is 1 + s1 * sigma and the s1 column");
        console2.log("  is the whole difference between quoting near the floor and quoting near the old 7.5.");
        console2.log("  Read the capacity column first: if it dominates, the spread is not what was protecting");
        console2.log("  us and the tightening is affordable. If s1 dominates instead, it is not, and the");
        console2.log("  standing objection survives its own re-measurement.");
    }

    /// @notice **The sub-second deliverable.** How long a wrong quote stays up, swept ACROSS the
    ///         one-second boundary the old model could not see past.
    ///
    /// @dev This test is the reason `tickMs` exists. The previous version of it swept 1/2/5/10
    ///     seconds, reported the total loss as "roughly flat", and that was read as "do not try to
    ///     solve this with speed". **That conclusion was only ever valid at or above one second**,
    ///     because the loop was `for t = 1..horizonSecs` with `vm.warp(T0 + t)` and one second was
    ///     the finest thing the model could represent. A sweep cannot find an effect below its own
    ///     resolution and then be quoted as evidence the effect is absent.
    ///
    ///     The rows below run at a 200 ms model tick so that 200 ms and 400 ms are real points
    ///     rather than rounding artefacts, and they keep the 1 s / 2 s / 5 s points so the new
    ///     numbers sit in the same table as the old conclusion. 2,000 ms is what the bot has
    ///     today: wake on `newHeads` at 1 Hz, land a block later. ~400 ms is what polling GIWA's
    ///     flashblocks `pending` tag at 200 ms would give.
    ///
    ///     **Two columns, because there are two different builds and only one of them is cheap.**
    ///     `decisionIntervalMs` is how often the bot looks; `quoteLatencyMs` is how long its
    ///     transaction then takes to land. Shortening the second without the first is a
    ///     transaction-routing change; shortening both is a new event loop. Reporting one number
    ///     would let the reader attribute the whole effect to whichever they had in mind.
    function test_sweep_quoteLatencyAcrossTheSecondBoundary() public {
        uint256[5] memory latMs = [uint256(200), 400, 1_000, 2_000, 5_000];

        _latencyHeader("LANDING LATENCY ONLY -- 1 Hz newHeads polling held FIXED", LATENCY_SEEDS);
        for (uint256 i; i < latMs.length; ++i) {
            _latencyRow(
                string.concat(FlowModel.u2s(latMs[i]), "ms"), _quoteLatCfg(latMs[i], 1_000, 1_000_000, LATENCY_SEEDS)
            );
        }
        console2.log("  The tx lands faster; the bot still only looks once a second. This row set answers");
        console2.log("  'is a better transaction path worth it on its own'.");

        _latencyHeader("THE WHOLE LOOP AT THAT LATENCY -- polling MOVED WITH the landing latency", LATENCY_SEEDS);
        for (uint256 i; i < latMs.length; ++i) {
            _latencyRow(
                string.concat(FlowModel.u2s(latMs[i]), "ms"), _quoteLatCfg(latMs[i], latMs[i], 1_000_000, LATENCY_SEEDS)
            );
        }
        console2.log("  This is the flashblocks build: poll `pending` at 200ms AND land in a preconf.");

        _printPickOffMechanism();

        console2.log("");
        console2.log("  READ THIS BEFORE QUOTING A ROW. The two tables agree with each other and with the");
        console2.log("  mechanism: crossing the one-second boundary takes the pool from picked off on every");
        console2.log("  seed to picked off on most of them, worth ~$2,200 of the ~$16,500 a gap costs. And");
        console2.log("  200ms is indistinguishable from 400ms, so the whole of the available gain sits AT");
        console2.log("  the boundary and nothing below it buys anything more. Moving the polling loop as");
        console2.log("  well as the landing adds nothing measurable, because the requote is not what the");
        console2.log("  searcher is racing -- it fires in the same tick the gap prints, before any requote");
        console2.log("  decided on that gap could possibly land, at any latency. What a shorter window");
        console2.log("  removes is the searcher's SECOND through Nth attempt, never its first.");
        console2.log("");
        console2.log("  So a 13% expected saving is the ceiling on what requoting faster is worth, it is");
        console2.log("  bounded by a single draw of the hazard, and two other tests are needed before");
        console2.log("  anyone spends a week on it: the withdrawal race, which is worth more, and the");
        console2.log("  searcher's reaction rate, which decides whether either is worth anything at all.");
    }

    /// @notice **Where a sub-second clock actually changes an answer: the withdrawal race.**
    ///
    /// @dev The requote race cannot be won at any latency — the searcher acts in the same tick the
    ///      gap prints. The WITHDRAWAL race is different in one specific way that matters: the
    ///      detector also fires in that tick (`jump.rs` scans at 200 ms), so the two start
    ///      together and the winner is decided by whether `refreshCapacity(id, 0, 0)` lands before
    ///      the searcher's second attempt. At a one-second model tick that question could not be
    ///      asked, because the smallest representable withdrawal latency was 1,000 ms and the
    ///      searcher's first attempt already sat inside it.
    ///
    ///      Two blocks, and the second one is the point. Against a searcher that needs a second to
    ///      react, a 400 ms withdrawal wins often enough to matter. Against one that reacts inside
    ///      200 ms, it never wins, because it is not racing the searcher's second attempt any more
    ///      — the first one already took the epoch.
    function test_theWithdrawalRaceAcrossTheSecondBoundary() public {
        uint256[4] memory wlat = [uint256(200), 400, 1_000, 2_000];
        uint256 seeds = 6;

        _latencyHeader("JUMP WITHDRAWAL vs a searcher reacting at 1.00/s", seeds);
        _latencyRow("none", _quoteLatCfg(2_000, 1_000, 1_000_000, seeds));
        for (uint256 i; i < wlat.length; ++i) {
            _latencyRow(
                string.concat(FlowModel.u2s(wlat[i]), "ms"),
                LatCfg({
                    quoteLatencyMs: 2_000,
                    decisionIntervalMs: 1_000,
                    withdrawLatencyMs: wlat[i],
                    maxPerSecE6: 1_000_000,
                    seeds: seeds
                })
            );
        }

        _latencyHeader("THE SAME, vs a searcher reacting at 5.00/s", seeds);
        _latencyRow("none", _quoteLatCfg(2_000, 1_000, 5_000_000, seeds));
        for (uint256 i; i < wlat.length; ++i) {
            _latencyRow(
                string.concat(FlowModel.u2s(wlat[i]), "ms"),
                LatCfg({
                    quoteLatencyMs: 2_000,
                    decisionIntervalMs: 1_000,
                    withdrawLatencyMs: wlat[i],
                    maxPerSecE6: 5_000_000,
                    seeds: seeds
                })
            );
        }

        console2.log("");
        console2.log("  A single seed of the top block reported the 400ms withdrawal ELIMINATING adverse");
        console2.log("  selection -- toxic share 92.76% -> 0.00%, a $15,600 loss turned into a $66 profit.");
        console2.log("  It is not true, and the shape of the error is the one this file has made before:");
        console2.log("  a number moved a very long way in the desired direction on one sample of a rare");
        console2.log("  binary event. The seeded columns above are what it actually does.");
    }

    /// @dev Eight seeds per row, and the seed count is not decoration. The first cut of this sweep
    ///      ran one seed and produced a 400 ms row showing a +$38 PROFIT sitting between two rows
    ///      that each lost ~$16,000. Nothing about a maker's transaction landing 400 ms after it
    ///      is signed rather than 200 ms makes a searcher walk away from a 100 bp gap on a $2M
    ///      epoch, so the row was not a finding, it was a coin landing on its edge: at these
    ///      settings a searcher gets one Bernoulli draw per 200 ms tick at ~60%, a 400 ms window
    ///      is two draws, and two misses is a 16% event. A single-seed sweep of a rare, enormous,
    ///      binary loss reports its own sampling noise with a straight face.
    ///
    ///      Eight seeds is still few. It is enough to separate "the pool is picked off almost
    ///      always" from "the pool is picked off most of the time", which is the whole size of the
    ///      effect on offer, and the `picked off` column reports the raw count so a reader can see
    ///      the sample rather than infer it from an average.
    uint256 internal constant LATENCY_SEEDS = 8;

    /// @notice One row of a seed-averaged latency sweep.
    /// @dev A struct rather than five arguments, for the reason `FlowModel.Solve` documents.
    struct LatCfg {
        uint256 quoteLatencyMs;
        uint256 decisionIntervalMs;
        /// @dev Non-zero turns the jump-withdrawal defence ON at that latency. Zero leaves the
        ///      pool undefended, which is the baseline every latency row is measured against.
        uint256 withdrawLatencyMs;
        uint256 maxPerSecE6;
        uint256 seeds;
    }

    function _latencyHeader(string memory title, uint256 seeds) internal pure {
        console2.log("");
        FlowModel.rule();
        console2.log(string.concat(" SWEEP: ", title));
        console2.log(
            string.concat(
                "   300s horizon, +100 bp jump at t=150, $2M epoch, 200ms model tick, ",
                FlowModel.u2s(seeds),
                " seeds a row."
            )
        );
        FlowModel.rule();
        console2.log("  latency      picked off   mean spread($)   mean MO+60s($)   worst seed($)");
    }

    function _latencyRow(string memory label, LatCfg memory c) internal {
        int256 sumSpread;
        int256 sumMo60;
        int256 worst;
        uint256 pickedOff;

        for (uint256 s; s < c.seeds; ++s) {
            FlowModel.Params memory p = _jump();
            p.horizonSecs = 300;
            p.jumpAtSecs = 150;
            p.tickMs = 200;
            p.quoteLatencyMs = c.quoteLatencyMs;
            p.decisionIntervalMs = c.decisionIntervalMs;
            p.informedMaxPerSecE6 = c.maxPerSecE6;
            if (c.withdrawLatencyMs != 0) {
                p.jumpThresholdE2 = 2_500; // 25 bp, the deployed trip
                p.cooloffSecs = 30;
                p.withdrawLatencyMs = c.withdrawLatencyMs;
            }
            p.seed = p.seed + s * 7_919; // a prime stride, so the streams do not overlap

            FlowModel.Result memory res = _run(p);
            FlowModel.Accum memory all = _total(res);

            sumSpread += all.spread;
            sumMo60 += all.mo60;
            if (all.spread < worst) worst = all.spread;
            if (res.byPop[FlowModel.POP_INFORMED].fills + res.byPop[FlowModel.POP_FAST].fills != 0) pickedOff += 1;
        }

        // forge-lint: disable-next-line(unsafe-typecast)
        int256 n = int256(c.seeds);
        console2.log(
            string.concat(
                "  ",
                FlowModel.pad(label, 13),
                FlowModel.pad(string.concat(FlowModel.u2s(pickedOff), "/", FlowModel.u2s(c.seeds)), 13),
                FlowModel.pad(FlowModel.signedUnits(sumSpread / n, 6), 17),
                FlowModel.pad(FlowModel.signedUnits(sumMo60 / n, 6), 17),
                FlowModel.signedUnits(worst, 6)
            )
        );
    }

    function _quoteLatCfg(uint256 latMs, uint256 decisionMs, uint256 maxPerSecE6, uint256 seeds)
        internal
        pure
        returns (LatCfg memory)
    {
        return LatCfg({
            quoteLatencyMs: latMs,
            decisionIntervalMs: decisionMs,
            withdrawLatencyMs: 0,
            maxPerSecE6: maxPerSecE6,
            seeds: seeds
        });
    }

    /// @notice The mechanism the tables above are a noisy sample of, computed exactly.
    ///
    /// @dev Printing this next to the simulation is the difference between a result and a number.
    ///      The pick-off is a race between one searcher and one requote, and its outcome is a
    ///      geometric trial: the searcher gets one draw per model tick at the per-tick hazard, the
    ///      window is the latency, so `P(taken) = 1 - (1 - q)^L`. That is what the sweep measures,
    ///      and it says where any sub-second gain would have to come from — not from a smaller
    ///      loss, since the absorption rule fixes the loss GIVEN a pick-off at
    ///      `(error - halfSpread - width/2) x depth` regardless of latency, but from a lower
    ///      PROBABILITY of one happening at all.
    ///
    ///      It also says exactly what that gain is hostage to, which is the next test.
    function _printPickOffMechanism() internal view {
        FlowModel.Params memory p = _jump();
        p.tickMs = 200;

        // ~94 bp: a 100 bp gap measured against a ladder whose top ask sits 5 bp above the
        // pre-jump mid, which is where the executable top lands at zero epoch usage.
        uint256 q = FlowModel.informedHazardPerTick(9_400, p);

        console2.log("");
        FlowModel.rule();
        console2.log(" THE MECHANISM, EXACTLY: P(picked off) = 1 - (1 - q)^L on a 94 bp gap");
        console2.log(
            string.concat(
                "   per-200ms-tick hazard q = ",
                FlowModel.pct2(q / 100),
                "%    per-second = ",
                FlowModel.pct2(FlowModel.informedHazard(9_400, p) / 100),
                "%"
            )
        );
        FlowModel.rule();
        console2.log("   latency     window (ticks)   P(the pool is picked off at all)");

        uint256[5] memory latMs = [uint256(200), 400, 1_000, 2_000, 5_000];
        for (uint256 i; i < latMs.length; ++i) {
            uint256 ticks = latMs[i] / 200;
            uint256 survive = FlowModel.PROB_ONE;
            for (uint256 j; j < ticks; ++j) {
                survive = (survive * (FlowModel.PROB_ONE - q)) / FlowModel.PROB_ONE;
            }
            console2.log(
                string.concat(
                    "   ",
                    FlowModel.pad(string.concat(FlowModel.u2s(latMs[i]), "ms"), 12),
                    FlowModel.pad(FlowModel.u2s(ticks), 17),
                    FlowModel.pct2((FlowModel.PROB_ONE - survive) / 100),
                    "%"
                )
            );
        }
        FlowModel.rule();
    }

    /// @notice **The adversarial reading of the sweep above, and it does not survive it.**
    ///
    /// @dev Any sub-second gain in `test_sweep_quoteLatencyAcrossTheSecondBoundary` comes from one
    ///      place: a 200 ms window is one draw at the per-tick hazard instead of five, so the
    ///      searcher sometimes fails to arrive before the requote. That is entirely a statement
    ///      about `informedMaxPerSecE6`, the ceiling on how fast the fastest searcher can react,
    ///      and its default of 1.0/s means **the quickest arbitrageur on the chain takes about a
    ///      second to notice a 100 bp gap and get a transaction in.**
    ///
    ///      At a one-second tick that assumption was invisible and harmless. It said "the pick-off
    ///      happens in the tick where the gap appears", which is the same thing every faster
    ///      assumption also says at that resolution — the model could not tell them apart, so the
    ///      ceiling never had to be defended. A 200 ms clock makes it load-bearing and makes it
    ///      say something much stronger and much less plausible. `FlowModel`'s own header derives
    ///      the ceiling from "the block time is 1 second", which was a fact about how often a
    ///      pick-off can *settle*, not about how fast a searcher can *see*. GIWA preconfirms
    ///      flashblocks every 200 ms; if the maker is allowed to use them then so is the searcher,
    ///      and the searcher is the one holding a five-figure option.
    ///
    ///      So the sweep is re-run against searchers that react at 1/s, 2/s and 5/s. If the
    ///      sub-second advantage only exists against the slowest of the three, it is not a
    ///      property of latency, it is a property of an unmeasured parameter that happened to be
    ///      calibrated to the old tick size — and the correct engineering conclusion is the
    ///      opposite of the encouraging one.
    ///
    ///      This is the same standard that caught "the defences eliminated adverse selection
    ///      entirely, informed fills 1 -> 0": prefer a result you can explain to one you like.
    function test_theSubSecondGainIsHostageToTheSearchersReactionRate() public {
        uint256[3] memory rates = [uint256(1_000_000), 2_000_000, 5_000_000];
        uint256[3] memory latMs = [uint256(200), 1_000, 5_000];

        uint256 seeds = 6;
        for (uint256 rr; rr < rates.length; ++rr) {
            _latencyHeader(
                string.concat(
                    "SEARCHER REACTS AT ", FlowModel.pct2(rates[rr] / 10_000), "/s -- whole loop at the latency"
                ),
                seeds
            );
            for (uint256 i; i < latMs.length; ++i) {
                _latencyRow(
                    string.concat(FlowModel.u2s(latMs[i]), "ms"), _quoteLatCfg(latMs[i], latMs[i], rates[rr], seeds)
                );
            }
        }

        console2.log("");
        FlowModel.rule();
        console2.log(" WHAT THE THREE BLOCKS SAY, at 200ms of latency:");
        FlowModel.rule();
        console2.log("   1.00/s  the searcher needs a second to see a 100 bp gap. It gets one draw at ~59%");
        console2.log("           inside the window, plus the latency-advantaged taker's coin flip on the");
        console2.log("           tick the requote lands, so the pool escapes about one run in six.");
        console2.log("   2.00/s  the searcher needs half a second. The per-tick hazard saturates and the");
        console2.log("           escape is already gone: 6/6 picked off, and 200ms is worth $79 against");
        console2.log("           1000ms -- a tenth of one percent of the loss, which is nothing.");
        console2.log("   5.00/s  the searcher reacts within 200ms, the same preconfirmation cadence the");
        console2.log("           maker would be buying. Identical to 2.00/s: the first tick after the gap");
        console2.log("           is taken with certainty and latency stops mattering entirely.");
        console2.log("");
        console2.log("   So the sub-second gain does not survive a searcher twice as fast as the default,");
        console2.log("   let alone one that uses the flashblocks the maker is proposing to buy. Nobody has");
        console2.log("   measured this rate on GIWA. It is not a parameter of the pool, it is a property of");
        console2.log("   the people watching it, and it is the entire case for spending money on speed.");
        FlowModel.rule();
    }

    /// @notice The honesty requirement, executed. `e50` is the parameter that most directly sets
    ///         how attractive a given error is, and the answer is printed at every value rather
    ///         than at the convenient one.
    function test_sensitivityToTheInformedArrivalFunction() public {
        uint256[4] memory e50 = [uint256(250), 500, 1_000, 4_000]; // 2.5, 5, 10, 40 bp

        _sweepHeader("INFORMED ARRIVAL FUNCTION (e50), LARGE gap", "e50", "900s horizon, +100 bp jump at t=450.");
        for (uint256 i; i < e50.length; ++i) {
            FlowModel.Params memory p = _jump();
            p.horizonSecs = 900;
            p.jumpAtSecs = 450;
            p.informedHalfMaxE2 = e50[i];
            FlowModel.Result memory res = _run(p);
            // forge-lint: disable-next-line(unsafe-typecast)
            _sweepRow(string.concat(FlowModel.sbps(int256(e50[i])), " bp"), "", res, p);
        }
        console2.log("  Identical. At 100 bp of error the hazard is saturated at every e50 in this range, so");
        console2.log("  the choice changes nothing: somebody takes it, and takes all of it.");

        // The regime where the choice DOES matter: an error small enough that the hazard is on
        // its steep part rather than its ceiling.
        _sweepHeader("INFORMED ARRIVAL FUNCTION (e50), SMALL gap", "e50", "900s horizon, +12 bp jump at t=450.");
        for (uint256 i; i < e50.length; ++i) {
            FlowModel.Params memory p = _jump();
            p.horizonSecs = 900;
            p.jumpAtSecs = 450;
            p.jumpE2 = 1_200; // 12 bp: just past the 5 bp half-spread plus the 1 bp cost
            p.informedHalfMaxE2 = e50[i];
            FlowModel.Result memory res = _run(p);
            // forge-lint: disable-next-line(unsafe-typecast)
            _sweepRow(string.concat(FlowModel.sbps(int256(e50[i])), " bp"), "", res, p);
        }
        console2.log("  Two things to read here, and only one of them is about e50.");
        console2.log("  (1) e50 moves the answer at errors near the half-spread, and that is the model's");
        console2.log("      weakest joint: whether a marginal gap is taken at all depends on a parameter");
        console2.log("      nobody has measured on this chain. The headline does NOT depend on it -- a");
        console2.log("      100 bp gap is taken under every assumption in the table above.");
        console2.log("  (2) MO+60s comes out POSITIVE here while the spread column is negative. That is");
        console2.log("      not the pool winning; it is one fill of ~$1M sitting inside +-$800 of 60-second");
        console2.log("      diffusion. At small gaps the 60s markout is mostly noise, which is exactly why");
        console2.log("      the per-run report prints +1s and +10s alongside it.");
    }

    /// @notice The competitive-race assumption. 100% clears the whole profitable depth; 50% is
    ///         a lone profit-maximising arbitrageur, which on a linear ladder is exactly half.
    function test_sensitivityToTheCaptureAssumption() public {
        uint256[3] memory capture = [uint256(100), 50, 25];
        _sweepHeader("INFORMED CAPTURE (race vs monopolist)", "capture       ");
        for (uint256 i; i < capture.length; ++i) {
            FlowModel.Params memory p = _jump();
            p.horizonSecs = 900;
            p.jumpAtSecs = 450;
            p.informedCapturePct = capture[i];
            FlowModel.Result memory res = _run(p);
            _sweepRow(string.concat(FlowModel.u2s(capture[i]), "%"), "", res, p);
        }
    }

    /// @notice How much uninformed flow it takes to pay for one 100 bp jump per hour.
    ///         The answer is the break-even, and it is reported whether or not it is flattering.
    function test_breakEvenUninformedFlow() public {
        uint256[4] memory rates = [uint256(60), 600, 3_600, 10_800];
        _sweepHeader("UNINFORMED FLOW", "arrivals/hr      notional/hr");
        for (uint256 i; i < rates.length; ++i) {
            FlowModel.Params memory p = _jump();
            p.horizonSecs = 900;
            p.jumpAtSecs = 450;
            p.uninformedPerHour = rates[i];
            FlowModel.Result memory res = _run(p);
            _sweepRow(FlowModel.u2s(rates[i]), string.concat("$", FlowModel.thousands(rates[i] * 2_035)), res, p);
        }
        console2.log("  Non-monotone on purpose, and the reason is worth reading: heavy uninformed flow");
        console2.log("  CONSUMES the ask epoch before the gap arrives, so there is less depth left to pick");
        console2.log("  off. Volume is not only the thing that pays for adverse selection, it is also");
        console2.log("  partly what shields the pool from it. Neither effect is large enough to close the");
        console2.log("  gap at any rate this sweep can afford to simulate, so the arithmetic in");
        console2.log("  test_breakEvenArithmetic finishes the job.");
    }

    /// @notice The break-even, extrapolated out loud from two measured numbers rather than
    ///         asserted. It is an extrapolation and not a measurement because simulating the
    ///         volume it names would be tens of thousands of real swaps per run.
    ///
    /// @dev The two inputs come from *different* runs on purpose. The pick-off cost is measured
    ///      on the jump path, where it is what the run is about. The uninformed edge is measured
    ///      on the FLAT path, where there is no diffusion and therefore no post-fill noise in the
    ///      markout: on the jump path the ~13 uninformed fills sit inside ±$800 of 60-second
    ///      diffusion, so their average markout is mostly sampling error, and dividing by a noisy
    ///      denominator would produce a break-even that moves with the seed.
    ///
    ///      Its own test rather than a tail on the sweep because Solidity never frees memory and
    ///      six full runs in one call expands past the EVM's memory gas limit.
    function test_breakEvenArithmetic() public {
        FlowModel.Params memory jumpRun = _jump();
        jumpRun.horizonSecs = 900;
        jumpRun.jumpAtSecs = 450;
        FlowModel.Result memory jumped = _run(jumpRun);
        int256 toxicLoss = jumped.byPop[FlowModel.POP_INFORMED].mo60 + jumped.byPop[FlowModel.POP_FAST].mo60;

        FlowModel.Result memory quiet = _run(_flat());
        int256 goodBps =
            FlowModel.bpsOf(quiet.byPop[FlowModel.POP_UNINFORMED].mo60, quiet.byPop[FlowModel.POP_UNINFORMED].notional);

        console2.log("");
        console2.log("  BREAK-EVEN, from two measured numbers:");
        console2.log(
            string.concat("    one 100 bp gap against a $2M epoch costs  ", FlowModel.signedUnits(toxicLoss, 6))
        );
        console2.log(string.concat("    uninformed flow earns (flat path, no noise) ", FlowModel.sbps(goodBps), " bp"));
        if (goodBps <= 0 || toxicLoss >= 0) return;

        // forge-lint: disable-next-line(unsafe-typecast)
        uint256 needed = (uint256(-toxicLoss) * FlowModel.BPS_E2) / uint256(goodBps) / 1e6;
        console2.log(
            string.concat(
                "    so covering ONE gap needs                $", FlowModel.thousands(needed), " of uninformed notional"
            )
        );
        console2.log(
            string.concat(
                "    at the default 60 arrivals/hour that is   ",
                FlowModel.u2s(needed / (60 * 2_035)),
                " hours of retail flow, per gap"
            )
        );
    }

    /// @notice The spread sweep. Read it against `FlowModel`'s stated limitation: there is no
    ///         competing venue in this model, so widening the spread never costs volume here and
    ///         the result is biased in favour of wider.
    function test_sweep_halfSpread() public {
        uint256[4] memory hs = [uint256(5), 15, 30, 60];
        _sweepHeader("HALF-SPREAD (biased: no competing venue in this model)", "half-spread   ");
        for (uint256 i; i < hs.length; ++i) {
            FlowModel.Params memory p = _jump();
            p.horizonSecs = 900;
            p.jumpAtSecs = 450;
            p.halfSpreadBps = hs[i];
            p.widthBps = hs[i] * 5;
            p.informedHalfMaxE2 = hs[i] * 200; // e50 stays pinned at 2x the half-spread
            FlowModel.Result memory res = _run(p);
            _sweepRow(string.concat(FlowModel.u2s(hs[i]), " bp"), "", res, p);
        }
        console2.log("  A wider spread raises the bar a move has to clear before the quote is pickable, so");
        console2.log("  it does help -- but it helps by making the pool refuse to trade, and this model");
        console2.log("  cannot see the volume that refusal costs. Do not read a row here as a recommendation.");
    }

    // =====================================================================
    // Harness assertions — is the instrument measuring what it claims?
    // =====================================================================

    /// @notice The NAV walk must reconcile with the per-fill markout, and a quiet book must
    ///         attribute exactly zero trade PnL.
    ///
    /// @dev This is the bridge to `dubu-updater`'s `risk.rs`. That module's whole design rests
    ///      on the property that with no trades, `tradePnl` is exactly zero — not approximately,
    ///      because the balances are identical and the two sums cancel term for term over the
    ///      same integer valuation function. The observation at step 2 of every tick asserts it
    ///      3,600 times inside `_observe`; this test asserts the other half, that the residual
    ///      really is the fills.
    ///
    ///      The tolerance is one quote unit per fill, and it is a rounding bound rather than a
    ///      fudge: the per-fill spread values the base leg as `floor(q * ref / S)` while the NAV
    ///      difference values it as `floor(B * ref / S) - floor((B - q) * ref / S)`, and those
    ///      two differ by at most one unit of a 6-decimal token — a ten-thousandth of a cent.
    function test_navReconcilesWithTheUpdatersRiskModule() public {
        FlowModel.Params memory p = _jump();
        p.horizonSecs = 1_800;
        p.jumpAtSecs = 900;
        FlowModel.Result memory res = _run(p);

        int256 fromFills;
        uint256 fills;
        for (uint256 i; i < FlowModel.POP_COUNT; ++i) {
            fromFills += res.byPop[i].spread;
            fills += res.byPop[i].fills;
        }

        console2.log("");
        FlowModel.rule();
        console2.log(" NAV reconciliation against dubu-updater risk.rs");
        FlowModel.rule();
        console2.log(string.concat("  trade PnL from the NAV walk        ", FlowModel.signedUnits(res.tradePnl, 6)));
        console2.log(string.concat("  sum of per-fill realised spread    ", FlowModel.signedUnits(fromFills, 6)));
        console2.log(string.concat("  revaluation (unhedged inventory)   ", FlowModel.signedUnits(res.revaluation, 6)));
        console2.log(
            string.concat("  NAV start ", FlowModel.units(res.navStart, 6), "  end ", FlowModel.units(res.navEnd, 6))
        );
        FlowModel.rule();

        int256 diff = res.tradePnl - fromFills;
        if (diff < 0) diff = -diff;
        // Two units per fill: the per-fill spread floors the base leg once, the NAV difference
        // floors the whole balance twice, and a tick carrying k fills aggregates k of them.
        // forge-lint: disable-next-line(unsafe-typecast)
        assertLe(uint256(diff), 2 * fills + 4, "NAV trade PnL and per-fill markout disagree beyond rounding");

        // The identity the decomposition is defined by, checked end to end.
        // forge-lint: disable-next-line(unsafe-typecast)
        assertEq(int256(res.navEnd) - int256(res.navStart), res.revaluation + res.tradePnl, "NAV did not decompose");
    }

    /// @notice The arrival function's shape, printed so a reader can argue with it, and asserted
    ///         so it cannot silently change.
    ///
    /// @dev The per-tick column is the one to look at after the two-clock change. It is NOT
    ///      `lambda / 5`: the hazard describes a race with one winner, so what must survive a
    ///      change of clock is the probability the gap is taken at all within the second, and the
    ///      last column shows that it does. See `FlowModel.informedHazardPerTick`.
    function test_informedArrivalFunctionShape() public view {
        FlowModel.Params memory p = _defaults();
        p.tickMs = 200;
        uint256 n = FlowModel.ticksPerSec(p);

        console2.log("");
        FlowModel.rule();
        console2.log(" INFORMED ARRIVAL FUNCTION   lambda(e) = max * n^2/(n^2 + e50^2),  n = max(0, e - cost)");
        console2.log(
            string.concat(
                "   e50 = ",
                FlowModel.sbps(int256(p.informedHalfMaxE2)),
                " bp    cost = ",
                FlowModel.sbps(int256(p.informedCostE2)),
                " bp    max = 1.00/s    tick = 200ms"
            )
        );
        FlowModel.rule();
        console2.log("   edge (bp)   arrivals/sec   P(per 200ms tick)   P(>=1 in one second)   naive lambda/5");

        uint256[9] memory edges = [uint256(0), 50, 100, 200, 500, 1_000, 2_000, 5_000, 20_000];
        for (uint256 i; i < edges.length; ++i) {
            // forge-lint: disable-next-line(unsafe-typecast)
            uint256 lambda = FlowModel.informedHazard(int256(edges[i]), p);
            // forge-lint: disable-next-line(unsafe-typecast)
            uint256 perTick = FlowModel.informedHazardPerTick(int256(edges[i]), p);

            // P(at least one in a second) rebuilt from the per-tick probability. It must come back
            // to lambda, which is the invariant the whole conversion exists to hold.
            uint256 survive = FlowModel.PROB_ONE;
            for (uint256 j; j < n; ++j) {
                survive = (survive * (FlowModel.PROB_ONE - perTick)) / FlowModel.PROB_ONE;
            }
            assertApproxEqAbs(
                FlowModel.PROB_ONE - survive, lambda, 20, "the per-tick hazard did not rebuild the per-second one"
            );

            console2.log(
                string.concat(
                    "   ",
                    // forge-lint: disable-next-line(unsafe-typecast)
                    FlowModel.pad(FlowModel.sbps(int256(edges[i])), 12),
                    FlowModel.pad(FlowModel.pct2(lambda / 10_000), 15),
                    FlowModel.pad(string.concat(FlowModel.pct2(perTick / 100), "%"), 20),
                    FlowModel.pad(string.concat(FlowModel.pct2((FlowModel.PROB_ONE - survive) / 100), "%"), 23),
                    FlowModel.pct2(lambda / n / 100),
                    "%"
                )
            );
        }
        FlowModel.rule();
        console2.log("   The last two columns are the whole point. Column 4 is invariant to the tick by");
        console2.log("   construction; column 5 is what a linear rescaling would have used, and at the");
        console2.log("   saturated top row it is 20.00% per tick -- 67% per second against a true 100%.");
        console2.log("   A 200ms run built on column 5 would show the pool losing less to a jump because");
        console2.log("   the searcher had been quietly disabled, and it would read as a latency finding.");
        FlowModel.rule();

        // Zero at and below the taker's own cost. Not asymptotically zero -- zero.
        // forge-lint: disable-next-line(unsafe-typecast)
        assertEq(FlowModel.informedHazard(int256(p.informedCostE2), p), 0, "an unprofitable edge attracted flow");
        assertEq(FlowModel.informedHazard(-5_000, p), 0, "a quote in the pool's favour attracted flow");

        // Monotone increasing, and saturating below the ceiling.
        uint256 prev;
        for (uint256 e = 100; e <= 20_000; e += 100) {
            // forge-lint: disable-next-line(unsafe-typecast)
            uint256 l = FlowModel.informedHazard(int256(e), p);
            assertGe(l, prev, "the hazard is not monotone in the edge");
            assertLe(l, p.informedMaxPerSecE6, "the hazard exceeded its saturation rate");
            prev = l;
        }

        // Half the maximum at exactly cost + e50, which is what "e50" has to mean.
        // forge-lint: disable-next-line(unsafe-typecast)
        uint256 atHalf = FlowModel.informedHazard(int256(p.informedCostE2 + p.informedHalfMaxE2), p);
        assertEq(atHalf, p.informedMaxPerSecE6 / 2, "e50 is not the half-max point");
    }

    /// @notice **The invariant the whole two-clock change rests on:** changing `tickMs` changes
    ///         the model's resolution and not its experiment.
    ///
    /// @dev Three populations, three different right answers, and confusing any two of them would
    ///      have silently changed what is being measured while looking like a finding about
    ///      latency. So each is measured rather than argued:
    ///
    ///        1. **The reference path.** Per-second realised variance must be the same at every
    ///           tick, which needs the SQUARE ROOT rescaling. Measured on a real path, not on the
    ///           algebra that built it, because the algebra is what is being checked. A linear
    ///           rescaling would show up here as a 200 ms path with a fifth of the variance.
    ///        2. **Uninformed arrivals.** Expected fills per wall-clock hour must be the same at
    ///           every tick, which needs a LINEAR rescaling. It is a counting process; its
    ///           invariant is a mean.
    ///        3. **Informed arrivals.** `P(the gap is taken within a second)` must be the same at
    ///           every tick, which needs NEITHER — see `FlowModel.informedHazardPerTick`. It is a
    ///           race with one winner; its invariant is a probability. Asserted in
    ///           `test_informedArrivalFunctionShape`, where the table that shows it is printed.
    function test_tickRescalingIsResolutionNotExperiment() public view {
        uint256[4] memory ticks = [uint256(1_000), 500, 200, 100];

        console2.log("");
        FlowModel.rule();
        console2.log(" RESCALING INVARIANTS ACROSS tickMs   (sigma 1.00 bp/s, 3600s of path)");
        FlowModel.rule();
        console2.log("   tickMs   ticks/s   realised sigma/s   uninformed fills/hr   informed P(>=1 in 1s) at 100bp");

        for (uint256 i; i < ticks.length; ++i) {
            FlowModel.Params memory p = _defaults();
            p.tickMs = ticks[i];
            p.jumpAtSecs = 0;
            p.driftE2 = 0;

            uint256 sigmaMeasured = _realisedSigmaPerSecE2(p);
            uint256 fillsPerHourE6 = FlowModel.uninformedRatePerTick(p) * FlowModel.ticksPerSec(p) * 3_600;
            uint256 takenInASecond = _informedProbPerSec(p, 10_000);
            uint256 hazardPerSec = FlowModel.informedHazard(10_000, p);

            console2.log(
                string.concat(
                    "   ",
                    FlowModel.pad(FlowModel.u2s(ticks[i]), 9),
                    FlowModel.pad(FlowModel.u2s(FlowModel.ticksPerSec(p)), 10),
                    // forge-lint: disable-next-line(unsafe-typecast)
                    FlowModel.pad(string.concat(FlowModel.sbps(int256(sigmaMeasured)), " bp"), 19),
                    FlowModel.pad(FlowModel.pct2(fillsPerHourE6 / 10_000), 22),
                    FlowModel.pct2(takenInASecond / 100),
                    "%"
                )
            );

            // 1. Volatility. A 10% band around the nominal 1.00 bp/s: wide enough to absorb the
            //    integer truncation of a small per-tick amplitude and the sampling error of 3,600
            //    observations, and nowhere near wide enough to hide a linear-instead-of-sqrt
            //    rescaling, which would land at 0.45 bp/s (500 ms) or 0.32 bp/s (100 ms).
            assertApproxEqAbs(sigmaMeasured, p.sigmaE2, p.sigmaE2 / 10, "per-second volatility moved with the tick");

            // 2. Volume, to a tenth of a percent. The residual is the integer truncation of the
            //    per-tick probability into `PROB_ONE` units — 16,666 rather than 16,666.67 at one
            //    second, which is a rounding the pre-tick model already had and which the control
            //    reproduces exactly. It grows with the number of ticks and is 0.04% at 100 ms.
            assertApproxEqAbs(
                fillsPerHourE6,
                p.uninformedPerHour * FlowModel.PROB_ONE,
                (p.uninformedPerHour * FlowModel.PROB_ONE) / 1_000,
                "uninformed arrivals per hour moved with the tick"
            );

            // 3. The race. `informedHazard` IS the per-second probability by definition, so
            //    rebuilding it from the per-tick draws is a closed loop: if these two disagree,
            //    the per-tick conversion has changed how often the pool gets picked off.
            //    A 100 bp edge is near saturation but not at it (0.9899/s), which makes this a
            //    sharper check than a saturated edge would be — the ceiling would hide an error.
            assertApproxEqAbs(takenInASecond, hazardPerSec, 20, "P(picked off within a second) moved with the tick");
        }
        FlowModel.rule();
        console2.log("   If the volatility column drifts, every latency conclusion downstream of it is");
        console2.log("   measuring the rescaling. That is why it is measured on a built path rather than");
        console2.log("   asserted from the formula that builds it.");
    }

    /// @dev Realised standard deviation of the path's ONE-SECOND returns, in hundredths of a bp.
    ///      Sampled at second boundaries so the statistic means the same thing at every `tickMs`
    ///      — comparing per-tick moves across resolutions would compare different quantities.
    function _realisedSigmaPerSecE2(FlowModel.Params memory p) internal pure returns (uint256) {
        uint256[] memory ref = FlowModel.buildPath(p);
        uint256 tps = FlowModel.ticksPerSec(p);
        uint256 n = p.horizonSecs;

        uint256 sumSq;
        for (uint256 s = 1; s <= n; ++s) {
            uint256 a = ref[(s - 1) * tps];
            uint256 b = ref[s * tps];
            uint256 d = b > a ? b - a : a - b;
            uint256 rel = (d * FlowModel.BPS_E2) / a; // hundredths of a bp
            sumSq += rel * rel;
        }
        // Mean zero by construction (no drift), so the second moment is the variance.
        return FlowModel.isqrt(sumSq / n);
    }

    /// @dev `P(at least one informed arrival within one wall-clock second)`, rebuilt from the
    ///      per-tick probability the run loop actually draws against.
    function _informedProbPerSec(FlowModel.Params memory p, int256 edgeE2) internal pure returns (uint256) {
        uint256 perTick = FlowModel.informedHazardPerTick(edgeE2, p);
        uint256 survive = FlowModel.PROB_ONE;
        for (uint256 j; j < FlowModel.ticksPerSec(p); ++j) {
            survive = (survive * (FlowModel.PROB_ONE - perTick)) / FlowModel.PROB_ONE;
        }
        return FlowModel.PROB_ONE - survive;
    }

    /// @notice The size distribution really is heavy-tailed, and the tail index is the one
    ///         claimed. Empirical, over 20,000 draws.
    function test_sizeDistributionIsHeavyTailed() public pure {
        FlowModel.Rng memory r = FlowModel.rng(1);
        uint256 n = 20_000;
        uint256[4] memory counts;
        uint256[4] memory notional;
        uint256 total;

        for (uint256 i; i < n; ++i) {
            uint256 v = FlowModel.drawNotional(r);
            uint256 b = FlowModel.bucketOf(v);
            counts[b] += 1;
            notional[b] += v;
            total += v;
        }

        console2.log("");
        FlowModel.rule();
        console2.log(string.concat(" UNINFORMED SIZE DISTRIBUTION, ", FlowModel.u2s(n), " draws"));
        FlowModel.rule();
        console2.log("   decade        share of COUNT   share of NOTIONAL");
        for (uint256 b; b < 4; ++b) {
            console2.log(
                string.concat(
                    "   ",
                    FlowModel.pad(FlowModel.bucketLabel(b), 14),
                    FlowModel.pad(string.concat(FlowModel.pct2((counts[b] * 10_000) / n), "%"), 17),
                    FlowModel.pct2((notional[b] * 10_000) / total),
                    "%"
                )
            );
        }
        console2.log(string.concat("   mean notional  $", FlowModel.u2s(total / n)));
        console2.log("   A Pareto tail index of 1: each decade is a tenth as likely and carries the same");
        console2.log("   expected notional. Most flow is small; the top 0.1% of orders is ~a quarter of it.");
        FlowModel.rule();

        // The tail: each decade a tenth as likely as the one below, within sampling error.
        assertApproxEqAbs(counts[0], (n * 900) / 1_000, n / 100, "decade 0 weight");
        assertApproxEqAbs(counts[1], (n * 90) / 1_000, n / 100, "decade 1 weight");
        assertGt(counts[2], 0, "the $10k-100k decade never fired");
        // And the fat-tail property that matters: a handful of orders carry most of the volume.
        assertGt(notional[2] + notional[3], total / 3, "the tail carries too little of the notional");
    }

    /// @notice A quiet book attributes exactly zero trade PnL, and a market move with no fill is
    ///         all revaluation. The property `risk.rs` is built on, reproduced against the real
    ///         contract rather than against the Rust mirror's own arithmetic.
    function test_aQuietBookAttributesExactlyZeroTradePnl() public {
        FlowModel.Params memory p = _defaults();
        p.horizonSecs = 300;
        p.uninformedPerHour = 0;
        p.informedMaxPerSecE6 = 0;
        p.fastHitPct = 0;
        p.jumpAtSecs = 150;
        p.jumpE2 = 20_000; // a 200 bp gap, with nobody there to take it

        FlowModel.Result memory res = _run(p);

        assertEq(res.byPop[FlowModel.POP_UNINFORMED].fills, 0, "the quiet run traded");
        assertEq(res.tradePnl, 0, "a book with no fills attributed non-zero trade PnL");
        assertTrue(res.revaluation != 0, "a 200 bp move produced no revaluation");
        // forge-lint: disable-next-line(unsafe-typecast)
        assertEq(int256(res.navEnd) - int256(res.navStart), res.revaluation, "NAV moved for a reason other than price");

        console2.log("");
        console2.log(
            string.concat(
                "  Quiet book, 200 bp gap: revaluation ",
                FlowModel.signedUnits(res.revaluation, 6),
                ", trade PnL ",
                FlowModel.signedUnits(res.tradePnl, 6),
                " (exactly zero, as risk.rs requires)"
            )
        );
    }

    /// @notice The informed size solver takes the profitable depth and not one unit more.
    function test_informedSizingStopsExactlyWhereTheProfitStops() public {
        FlowModel.Params memory p = _defaults();
        _deployFixture(p);
        _pushLadder(p, REF_MID);
        _refresh(p);

        // The reference is 100 bp above the pool's ladder: the ask is stale and cheap.
        uint256 ref = (REF_MID * 10_100) / 10_000;
        IPropPool.PairSnapshot memory s = pool.snapshot(PAIR_ID);
        uint256 refNet = FlowModel.refNetForAsk(ref, p.informedCostE2);

        uint256 q = FlowModel.informedAskSize(s, s.askCapacity, refNet, SCALE, pool.reserveOf(address(baseToken)), 100);
        assertGt(q, 0, "a 100 bp stale ask attracted no size");

        // At q the average price still clears the taker's cost; at q+1 it does not, unless q is
        // the whole epoch (in which case the constraint is capacity, not profitability).
        uint256 cost = PropCurve.amountInAsk(q, s.minAsk, s.maxAsk, s.askCapacity, 0, PRICE_SCALE_EXP);
        assertLe(cost * SCALE, q * refNet, "the solver returned a size the taker would not take");
        if (q < s.askCapacity) {
            uint256 costNext = PropCurve.amountInAsk(q + 1, s.minAsk, s.maxAsk, s.askCapacity, 0, PRICE_SCALE_EXP);
            assertGt(costNext * SCALE, (q + 1) * refNet, "the solver left profitable depth on the table");
        }

        console2.log("");
        console2.log(
            string.concat(
                "  100 bp stale ask, $2M epoch: the informed taker lifts ",
                FlowModel.units(q, 18),
                " base of the ",
                FlowModel.units(s.askCapacity, 18),
                " on offer."
            )
        );
        console2.log("  It clears the WHOLE epoch, because 100 bp of error dwarfs a 25 bp ladder width.");
        console2.log("  The ladder's shape stops protecting anything once the error exceeds its width.");
    }

    // =====================================================================
    // The run loop
    // =====================================================================

    /// @notice One run, on the model clock, warping the block clock underneath it.
    ///
    /// @dev The loop index `k` is a MODEL TICK and every schedule inside `RunState` is denominated
    ///      in the same unit. `vm.warp` is called with `T0 + tickToBlockSecs(k)`, which at
    ///      `tickMs = 1000` is `T0 + k` — the old loop exactly — and at 200 ms leaves the
    ///      timestamp alone on four ticks out of five.
    ///
    ///      Warping to the value it already holds is a no-op, so the call is made unconditionally
    ///      rather than guarded. `blockSecsAdvanced` counts the transitions, so the report can
    ///      state both clocks and a reader can check they line up instead of taking it on trust.
    function _run(FlowModel.Params memory p) internal returns (FlowModel.Result memory res) {
        // Forge's default per-test gas limit is 2^30, and it is the right default: a test that
        // burns a billion gas is usually a test with a bug in it. This one is not — an hour at a
        // 200 ms tick is 18,000 iterations each driving several real swaps through a real
        // `PropPool`, and the gas it consumes is a fact about the simulation's length rather than
        // about the pool's efficiency. Nothing here measures gas, `PropPool`'s own cost is
        // asserted in `Integration.t.sol` and `PropPool.t.sol` where it means something, and the
        // alternative is raising the limit for all 425 tests in the repo to accommodate one file.
        vm.pauseGasMetering();

        uint256[] memory ref = FlowModel.buildPath(p);
        _deployFixture(p);

        vm.warp(T0);
        _pushLadder(p, ref[0]);
        _refresh(p);
        delete S;
        S.lastPushAt = 0;
        S.ticksPerSec = FlowModel.ticksPerSec(p);
        S.quoteLatencyTicks = FlowModel.msToTicks(p.quoteLatencyMs, p);
        S.withdrawLatencyTicks = FlowModel.msToTicks(p.withdrawLatencyMs, p);
        S.cooloffTicks = FlowModel.secsToTicks(p.cooloffSecs, p);
        S.heartbeatTicks = FlowModel.secsToTicks(p.heartbeatSecs, p);
        S.decisionTicks = FlowModel.msToTicks(p.decisionIntervalMs, p);
        S.detectorTicks = FlowModel.msToTicks(p.detectorIntervalMs, p);

        FlowModel.Rng memory r = FlowModel.rng(p.seed);

        res.baseStart = pool.reserveOf(address(baseToken));
        res.quoteStart = pool.reserveOf(address(quoteToken));
        res.baseMin = res.baseStart;
        res.baseMax = res.baseStart;
        res.navStart = res.quoteStart + FlowModel.value(res.baseStart, ref[0], p.priceScaleExp);

        // Seed the walk at k=0 against `ref[0]`, so the very first revaluation step the loop
        // attributes is `ref[0] -> ref[1]`. Without this the decomposition would silently drop
        // one tick of price move and `navEnd - navStart` would not equal
        // `revaluation + tradePnl` — which is the identity the whole reconciliation rests on.
        _observe(res, ref[0], p, false);

        // ------------------------------------------------------------------
        // The memory arena.
        //
        // Solidity never frees memory, and one tick of this loop allocates a `PairSnapshot`, the
        // return buffers of a dozen external calls and a handful of assertion strings — call it
        // 60 words. At one second a tick that was 3,600 iterations and nobody noticed. At 200 ms
        // it is 18,000, twice over in a before/after test, and the run dies with `MemoryOOG`
        // before it prints a number. Raising `memory_limit` in foundry.toml would fix the symptom
        // for every one of this repo's 425 tests to fix it for one.
        //
        // So the loop body gets an arena: the free memory pointer is snapshotted here and
        // restored at the top of every iteration, which reclaims exactly the garbage and nothing
        // else. That is sound if and only if nothing allocated INSIDE an iteration has to survive
        // it, so here is the audit, because "probably nothing does" is not good enough for a
        // pointer write:
        //
        //   * `ref`, `res`, `r` and `p` are all allocated BEFORE this mark. `res` in particular is
        //     a named return variable, so solc allocates its whole tree — both fixed-size `Accum`
        //     arrays included — at function entry, and `FlowModel.record` only mutates cells that
        //     already exist. `r.s` is the RNG state and is likewise mutated in place.
        //   * `S` is contract storage and is untouched by this.
        //   * Nothing inside an iteration returns a memory reference to its caller: the taker
        //     helpers return nothing, and the snapshots they take are read and dropped.
        //
        // A later run allocates its own path above the previous run's mark, so an earlier run's
        // `Result` — which its caller still holds — is below every subsequent reset and safe.
        // ------------------------------------------------------------------
        uint256 arena;
        // forge-lint: disable-next-line(asm-keccak256)
        assembly ("memory-safe") {
            arena := mload(0x40)
        }

        uint256 nTicks = FlowModel.totalTicks(p);
        uint256 prevSecs;
        for (uint256 k = 1; k <= nTicks; ++k) {
            // Not annotated `memory-safe`, and the annotation would be a lie: this deliberately
            // moves the allocator backwards, which is precisely the assumption that annotation
            // promises not to break.
            assembly {
                mstore(0x40, arena)
            }

            uint256 secs = FlowModel.tickToBlockSecs(k, p);
            if (secs != prevSecs) {
                res.blockSecsAdvanced += 1;
                prevSecs = secs;
            }
            vm.warp(T0 + secs);

            _observe(res, ref[k], p, true); // revaluation window: trade PnL must be zero

            // The detector runs FIRST, on the tick the move prints, because that is where it runs
            // in the bot: `jump.rs` scans the in-memory feed snapshots every 200 ms rather than
            // waiting for the 1 Hz quote cycle. At `tickMs = 200` the model finally runs it at the
            // same cadence the bot does; at 1,000 ms it ran a fifth as often and the model was
            // giving the detector a handicap it does not have in production.
            //
            // But firing only schedules the withdrawal — the ordering below is what makes this
            // honest. Every taker in this tick, including the latency-advantaged one, still gets
            // to act against the live ladder, and they keep getting it until `withdrawLandsAt`.
            // That window is the searcher's, and on a fee-ordered sequencer they can pay to stay
            // inside it.
            _jumpTick(p, ref, k);

            _fastTaker(res, p, r, ref, k);
            _landWithdrawal(p, k);
            _landPending(res, p, k);
            _informed(res, p, r, ref, k);
            _uninformed(res, p, r, ref, k);
            _observe(res, ref[k], p, false); // trade window: revaluation is zero by construction
            _updaterTick(p, ref, k);

            uint256 b = pool.reserveOf(address(baseToken));
            if (b < res.baseMin) res.baseMin = b;
            if (b > res.baseMax) res.baseMax = b;
        }

        res.ticksRun = nTicks;
        res.baseEnd = pool.reserveOf(address(baseToken));
        res.quoteEnd = pool.reserveOf(address(quoteToken));
        res.navEnd = res.quoteEnd + FlowModel.value(res.baseEnd, ref[nTicks], p.priceScaleExp);

        vm.resumeGasMetering();
    }

    // ---------------------------------------------------------------------
    // The maker: dubu-updater's trigger policy, with its transaction latency
    // ---------------------------------------------------------------------

    /// @dev Deliberately mirrors `policy.rs`: measured against the EXECUTABLE TOP at the current
    ///      usage rather than against `maxBid`, with the adverse threshold tight and the
    ///      favourable one loose, and with a pair that has a transaction in flight left alone.
    /// @dev The jump detector, and the withdrawal it schedules.
    ///
    ///      A market maker does not price through a jump, it steps aside — the baseline runs make
    ///      the reason arithmetic rather than folklore: a 100 bp move against a 17.5 bp absorption
    ///      limit costs the excess on the entire epoch, and no spread a maker would post covers
    ///      it. The only winning move is not to be there.
    ///
    ///      What it cannot do is be there instantly. The withdrawal is `refreshCapacity(id, 0, 0)`,
    ///      a transaction, and it lands `withdrawLatencyMs` later. Everything that arrives in
    ///      between is charged to the defence, not hidden by it.
    /// @param k the current MODEL tick.
    function _jumpTick(FlowModel.Params memory p, uint256[] memory ref, uint256 k) internal {
        if (p.jumpThresholdE2 == 0) return;
        if (k % S.detectorTicks != 0) return; // the detector is asleep between scans
        if (S.withdrawLandsAt != 0 || k < S.cooloffUntil) return; // already handling one

        // A TRAILING ONE-SECOND window, not the previous tick. `FlowModel.isJump` documents why at
        // length; in one line, a per-tick comparison would make the same threshold five times
        // harder to trip at 200 ms than at 1 s, and every cross-resolution comparison in this file
        // would then be measuring the detector's sensitivity rather than the pool's latency.
        uint256 back = k > S.ticksPerSec ? k - S.ticksPerSec : 0;
        if (!FlowModel.isJump(ref[back], ref[k], p.jumpThresholdE2)) return;

        S.jumpsDetected += 1;
        S.withdrawLandsAt = k + S.withdrawLatencyTicks;
    }

    /// @dev Zero capacity is a complete withdrawal that stays inside the updater's own authority:
    ///      it cannot call `pause`, and it does not need to. Every quote path returns 0.
    function _landWithdrawal(FlowModel.Params memory, uint256 k) internal {
        if (S.withdrawLandsAt == 0 || k < S.withdrawLandsAt) return;

        vm.prank(updater);
        pool.refreshCapacity(PAIR_ID, 0, 0);

        S.withdrawLandsAt = 0;
        S.cooloffUntil = k + S.cooloffTicks;
        // A push decided before the jump must not resurrect the ladder we just pulled.
        S.pendingLandsAt = 0;
        S.pendingRefresh = false;
    }

    function _updaterTick(FlowModel.Params memory p, uint256[] memory ref, uint256 k) internal {
        // The bot is asleep between `newHeads` deliveries. Without this gate a finer tick would
        // hand the updater a faster polling loop for free — see `Params.decisionIntervalMs`.
        if (k % S.decisionTicks != 0) return;
        if (S.withdrawLandsAt != 0 || k < S.cooloffUntil) return; // stepped aside
        if (S.pendingLandsAt != 0) return; // PushInFlight

        IPropPool.PairSnapshot memory s = pool.snapshot(PAIR_ID);
        bool capacityLow = _capacityIsLow(s, p);

        if (!capacityLow && (k - S.lastPushAt) < S.heartbeatTicks && !_driftTriggers(s, p, ref[k])) return;

        S.pendingMid = ref[k];
        S.pendingLandsAt = k + S.quoteLatencyTicks;
        S.pendingRefresh = capacityLow;
    }

    /// @dev `capacity_divergence_pct` from `updater.toml`: re-open the epoch once remaining
    ///      capacity has fallen this far below the planned one.
    function _capacityIsLow(IPropPool.PairSnapshot memory s, FlowModel.Params memory p) internal pure returns (bool) {
        (uint256 cap, uint256 used) = FlowModel.room(s, true);
        if (cap == 0) return true;
        if ((cap - used) * 100 < cap * (100 - p.capacityDivergencePct)) return true;
        (cap, used) = FlowModel.room(s, false);
        if (cap == 0) return true;
        return (cap - used) * 100 < cap * (100 - p.capacityDivergencePct);
    }

    /// @dev `adverse_drift_bps` / `favourable_drift_bps`, measured on the executable top.
    ///      A positive deviation means the posted quote is more generous than the target, which
    ///      is the pick-off direction and therefore the tight threshold. A side with no capacity
    ///      reports the sentinel and is skipped rather than read as an infinite deviation.
    function _driftTriggers(IPropPool.PairSnapshot memory s, FlowModel.Params memory p, uint256 ref)
        internal
        pure
        returns (bool)
    {
        (int256 bidEdge, int256 askEdge) = FlowModel.edges(s, ref);
        // forge-lint: disable-next-line(unsafe-typecast)
        int256 hs = int256(FlowModel.halfSpreadAt(p) * 100);

        if (bidEdge != type(int256).min && _fires(bidEdge + hs, p)) return true;
        if (askEdge != type(int256).min && _fires(askEdge + hs, p)) return true;
        return false;
    }

    function _fires(int256 devE2, FlowModel.Params memory p) private pure returns (bool) {
        // forge-lint: disable-next-line(unsafe-typecast)
        if (devE2 >= int256(p.adverseDriftE2)) return true;
        // forge-lint: disable-next-line(unsafe-typecast)
        return -devE2 >= int256(p.favourableDriftE2);
    }

    function _landPending(FlowModel.Result memory res, FlowModel.Params memory p, uint256 t) internal {
        if (S.pendingLandsAt == 0 || t < S.pendingLandsAt) return;

        _pushLadder(p, S.pendingMid);
        res.quotePushes += 1;
        if (S.pendingRefresh) {
            _refresh(p);
            res.capacityRefreshes += 1;
        }
        S.pendingLandsAt = 0;
        S.pendingRefresh = false;
        S.lastPushAt = t;
    }

    // ---------------------------------------------------------------------
    // The three populations
    // ---------------------------------------------------------------------

    /// @notice The latency-advantaged taker: it acts **only** on the tick an update is landing,
    ///         and it acts before it. That is the whole of its advantage, and it is the realistic
    ///         worst case on a sequencer that orders by fee against a bot bidding zero.
    function _fastTaker(
        FlowModel.Result memory res,
        FlowModel.Params memory p,
        FlowModel.Rng memory r,
        uint256[] memory ref,
        uint256 k
    ) internal {
        if (p.fastHitPct == 0) return;
        // Conditioned on a push landing, not on a rate, so it needs no rescaling: there are the
        // same number of pushes per wall-clock hour at every tick size, because the push cadence
        // is gated by the latency and the in-flight rule rather than by the clock.
        if (S.pendingLandsAt != k) return;
        if (!FlowModel.bernoulli(r, p.fastHitPct * 10_000)) return;
        _takeTheEdge(res, p, ref, k, FlowModel.POP_FAST);
    }

    /// @notice The ordinary informed taker, arriving per the hazard against whatever ladder is
    ///         on chain right now.
    ///
    /// @dev The hazard is quoted per second and converted to a per-tick probability by
    ///      `FlowModel.informedHazardPerTick`, which preserves the probability that the gap is
    ///      taken *within a second* rather than the expected count. That is the invariant a race
    ///      has, and choosing the other one would have weakened the searcher by a third at
    ///      saturation the moment the clock got finer. The rest of the function is unchanged: a
    ///      finer tick gives the searcher more chances to arrive, each correspondingly less
    ///      likely, arriving on average sooner within the second — which is the resolution
    ///      improvement, and is the only thing that should differ.
    function _informed(
        FlowModel.Result memory res,
        FlowModel.Params memory p,
        FlowModel.Rng memory r,
        uint256[] memory ref,
        uint256 k
    ) internal {
        if (p.informedMaxPerSecE6 == 0) return;

        IPropPool.PairSnapshot memory s = pool.snapshot(PAIR_ID);
        (int256 bidEdge, int256 askEdge) = FlowModel.edges(s, ref[k]);
        int256 best = bidEdge > askEdge ? bidEdge : askEdge;
        if (!FlowModel.bernoulli(r, FlowModel.informedHazardPerTick(best, p))) return;

        _takeTheEdge(res, p, ref, k, FlowModel.POP_INFORMED);
    }

    /// @dev Shared by both informed populations: pick the profitable side, size it exactly, take
    ///      it. Returns quietly when the pool cannot fill, which is the pool's capacity bound
    ///      doing its job — and the reason `declinedFills` is not incremented here: an informed
    ///      taker that finds nothing worth taking has not been refused, it has been deterred.
    function _takeTheEdge(
        FlowModel.Result memory res,
        FlowModel.Params memory p,
        uint256[] memory ref,
        uint256 k,
        uint256 pop
    ) internal {
        IPropPool.PairSnapshot memory s = pool.snapshot(PAIR_ID);
        (int256 bidEdge, int256 askEdge) = FlowModel.edges(s, ref[k]);
        // forge-lint: disable-next-line(unsafe-typecast)
        if (bidEdge <= int256(p.informedCostE2) && askEdge <= int256(p.informedCostE2)) return;

        if (askEdge >= bidEdge) _liftTheAsk(res, p, ref, k, pop, s);
        else _hitTheBid(res, p, ref, k, pop, s);
    }

    /// @dev The pool is selling base below the reference. Buy the whole profitable interval.
    function _liftTheAsk(
        FlowModel.Result memory res,
        FlowModel.Params memory p,
        uint256[] memory ref,
        uint256 k,
        uint256 pop,
        IPropPool.PairSnapshot memory s
    ) internal {
        uint256 q = FlowModel.informedAskSize(
            s,
            FlowModel.fillableRoom(pool, PAIR_ID, s, false),
            FlowModel.refNetForAsk(ref[k], p.informedCostE2),
            SCALE,
            pool.reserveOf(address(baseToken)),
            p.informedCapturePct
        );
        if (q == 0) return;
        uint256 cost = pool.getAmountIn(address(quoteToken), address(baseToken), q);
        if (cost == 0) return;

        _mintTo(taker, address(quoteToken), cost);
        vm.prank(taker);
        // forge-lint: disable-next-line(unsafe-typecast)
        pool.swap(address(quoteToken), address(baseToken), -int256(q), type(uint256).max, taker, 0, block.timestamp);

        _record(res, FlowModel.Fill({pop: pop, tTick: k, isBid: false, baseQty: q, quoteQty: cost}), ref, p);
    }

    /// @dev The pool is buying base above the reference. Sell it the whole profitable interval.
    function _hitTheBid(
        FlowModel.Result memory res,
        FlowModel.Params memory p,
        uint256[] memory ref,
        uint256 k,
        uint256 pop,
        IPropPool.PairSnapshot memory s
    ) internal {
        uint256 q = FlowModel.informedBidSize(
            s,
            FlowModel.fillableRoom(pool, PAIR_ID, s, true),
            FlowModel.refNetForBid(ref[k], p.informedCostE2),
            SCALE,
            pool.reserveOf(address(quoteToken)),
            p.informedCapturePct
        );
        if (q == 0) return;
        uint256 proceeds = pool.getAmountOut(address(baseToken), address(quoteToken), q);
        if (proceeds == 0) return;

        _mintTo(taker, address(baseToken), q);
        vm.prank(taker);
        // forge-lint: disable-next-line(unsafe-typecast)
        pool.swap(address(baseToken), address(quoteToken), int256(q), 0, taker, 0, block.timestamp);

        _record(res, FlowModel.Fill({pop: pop, tTick: k, isBid: true, baseQty: q, quoteQty: proceeds}), ref, p);
    }

    /// @notice `FlowModel.record`, plus the one thing the library cannot see: whether this fill
    ///         landed inside the window between the jump detector firing and its withdrawal
    ///         taking effect.
    ///
    /// @dev Every fill site goes through here so the count cannot drift out of step with reality
    ///      by someone adding a fifth one. `fillsWhileWithdrawing` was declared and printed but
    ///      never incremented before this change, which made the one line of output that would
    ///      have caught a defence that is secretly a wish read a constant zero. The file's own
    ///      history has an instance of exactly that failure — informed fills reported as 1 -> 0
    ///      because the size solver was bisecting against the wrong capacity — and the thing that
    ///      caught it was a number that refused to move when it should have. A counter that is
    ///      always zero cannot do that job.
    function _record(
        FlowModel.Result memory res,
        FlowModel.Fill memory f,
        uint256[] memory ref,
        FlowModel.Params memory p
    ) internal {
        if (S.withdrawLandsAt != 0) S.fillsWhileWithdrawing += 1;
        FlowModel.record(res, f, ref, p);
    }

    /// @notice Uninformed flow: arrivals on the 1-second grid, a fair-coin direction, and a
    ///         heavy-tailed size. It does not look at the price and does not decline.
    ///
    /// @dev The arrival count per tick is the integer part of the rate plus a Bernoulli draw on
    ///      the fraction, which has the right mean and is the discrete-time reading of a Poisson
    ///      process. Below one arrival per second — every configuration except the top of the
    ///      break-even sweep — it degenerates to a plain Bernoulli, and its only departure from a
    ///      true Poisson process is that it cannot produce a burst well above the mean in a
    ///      single second. That matters for queueing, and not at all for the annual P&L question
    ///      this file asks.
    ///
    ///      Getting this wrong is not cosmetic. A plain `bernoulli(rate)` silently saturates at
    ///      one arrival per tick, so every configuration above 3,600 arrivals/hour would have
    ///      produced *the same* run and the break-even sweep would have reported a floor that is
    ///      an artefact of the grid rather than a fact about the pool.
    function _uninformed(
        FlowModel.Result memory res,
        FlowModel.Params memory p,
        FlowModel.Rng memory r,
        uint256[] memory ref,
        uint256 k
    ) internal {
        uint256 rate = FlowModel.uninformedRatePerTick(p);
        uint256 n = rate / FlowModel.PROB_ONE;
        if (FlowModel.bernoulli(r, rate % FlowModel.PROB_ONE)) n += 1;

        for (uint256 i; i < n; ++i) {
            uint256 raw = FlowModel.drawNotional(r) * (10 ** uint256(p.quoteDecimals));
            if (FlowModel.next(r) % 2 == 0) _uninformedBuysBase(res, p, ref, k, raw);
            else _uninformedSellsBase(res, p, ref, k, raw);
        }
    }

    /// @dev Split in two purely for the stack; see `FlowModel.Solve` for the same reason.
    function _uninformedBuysBase(
        FlowModel.Result memory res,
        FlowModel.Params memory p,
        uint256[] memory ref,
        uint256 k,
        uint256 raw
    ) internal {
        uint256 out = pool.getAmountOut(address(quoteToken), address(baseToken), raw);
        if (out == 0) {
            res.declinedFills += 1;
            res.declinedNotional += raw;
            return;
        }
        _mintTo(taker, address(quoteToken), raw);
        vm.prank(taker);
        // forge-lint: disable-next-line(unsafe-typecast)
        pool.swap(address(quoteToken), address(baseToken), int256(raw), 0, taker, 0, block.timestamp);
        _record(
            res,
            FlowModel.Fill({pop: FlowModel.POP_UNINFORMED, tTick: k, isBid: false, baseQty: out, quoteQty: raw}),
            ref,
            p
        );
    }

    function _uninformedSellsBase(
        FlowModel.Result memory res,
        FlowModel.Params memory p,
        uint256[] memory ref,
        uint256 k,
        uint256 raw
    ) internal {
        uint256 baseIn = (raw * SCALE) / ref[k];
        uint256 out = pool.getAmountOut(address(baseToken), address(quoteToken), baseIn);
        if (out == 0) {
            res.declinedFills += 1;
            res.declinedNotional += raw;
            return;
        }
        _mintTo(taker, address(baseToken), baseIn);
        vm.prank(taker);
        // forge-lint: disable-next-line(unsafe-typecast)
        pool.swap(address(baseToken), address(quoteToken), int256(baseIn), 0, taker, 0, block.timestamp);
        _record(
            res,
            FlowModel.Fill({pop: FlowModel.POP_UNINFORMED, tTick: k, isBid: true, baseQty: baseIn, quoteQty: out}),
            ref,
            p
        );
    }

    // ---------------------------------------------------------------------
    // The NAV walk — identical in shape to dubu-updater's risk.rs
    // ---------------------------------------------------------------------

    /// @param expectNoTrade set on the observation taken before any fill, where the balances
    ///        cannot have moved since the last one. `risk.rs`'s whole design rests on trade PnL
    ///        being *exactly* zero in that case, so it is asserted rather than assumed.
    function _observe(FlowModel.Result memory res, uint256 fair, FlowModel.Params memory p, bool expectNoTrade)
        internal
    {
        uint256 q = pool.reserveOf(address(quoteToken));
        uint256 b = pool.reserveOf(address(baseToken));

        if (!S.navSeeded) {
            S.prevQuote = q;
            S.prevBase = b;
            S.prevFair = fair;
            S.navSeeded = true;
            return;
        }

        // forge-lint: disable-next-line(unsafe-typecast)
        int256 reval = int256(FlowModel.value(S.prevBase, fair, p.priceScaleExp))
            // forge-lint: disable-next-line(unsafe-typecast)
            - int256(FlowModel.value(S.prevBase, S.prevFair, p.priceScaleExp));
        // forge-lint: disable-next-line(unsafe-typecast)
        int256 prevNav = int256(S.prevQuote + FlowModel.value(S.prevBase, S.prevFair, p.priceScaleExp));
        // forge-lint: disable-next-line(unsafe-typecast)
        int256 nav = int256(q + FlowModel.value(b, fair, p.priceScaleExp));

        int256 tradePnl = (nav - prevNav) - reval;
        if (expectNoTrade) assertEq(tradePnl, 0, "a market move with no fill attributed trade PnL");

        res.revaluation += reval;
        res.tradePnl += tradePnl;

        S.prevQuote = q;
        S.prevBase = b;
        S.prevFair = fair;
    }

    // ---------------------------------------------------------------------
    // Fixture
    // ---------------------------------------------------------------------

    function _deployFixture(FlowModel.Params memory p) internal {
        baseToken = new MockERC20("Base", "BASE", p.baseDecimals);
        quoteToken = new MockERC20("Quote", "QUOTE", p.quoteDecimals);
        pool = new PropPool(owner, manager, updater, guardian);

        vm.prank(owner);
        // A floor at a tenth of the starting mid rather than the deployment's half. The scripted
        // paths here deliberately move further than a live pair's floor is sized for, and the
        // floor is not what this file is testing -- a `BidBelowMinPrice` revert mid-run would be
        // the harness failing, not the pool.
        // forge-lint: disable-next-line(unsafe-typecast)
        pool.addPair(address(baseToken), address(quoteToken), p.priceScaleExp, MAX_STALE_SECS, uint56(p.mid / 10));

        // The staleness ramp lives on chain, so the fixture gets it for free from the real
        // PropPool -- nothing in this file models it, the pool applies it. Manager-set, because
        // the updater must not be able to widen its own leash.
        if (p.decaySecs != 0) {
            vm.prank(manager);
            pool.setPairDecay(PAIR_ID, p.decaySecs);
        }

        baseToken.mint(manager, TVL_BASE);
        quoteToken.mint(manager, TVL_QUOTE);
        vm.startPrank(manager);
        baseToken.approve(address(pool), type(uint256).max);
        quoteToken.approve(address(pool), type(uint256).max);
        pool.deposit(address(baseToken), TVL_BASE);
        pool.deposit(address(quoteToken), TVL_QUOTE);
        vm.stopPrank();

        vm.startPrank(taker);
        baseToken.approve(address(pool), type(uint256).max);
        quoteToken.approve(address(pool), type(uint256).max);
        vm.stopPrank();
    }

    function _pushLadder(FlowModel.Params memory p, uint256 mid) internal {
        (,,, uint256 maxAsk) = FlowModel.ladder(mid, FlowModel.halfSpreadAt(p), p.widthBps);
        require(maxAsk <= type(uint56).max, "Simulation: the path took the ladder outside uint56");

        uint256[] memory packed = new uint256[](1);
        packed[0] = FlowModel.packLadder(PAIR_ID, mid, FlowModel.halfSpreadAt(p), p.widthBps);
        vm.prank(updater);
        pool.updateQuote(packed);
    }

    function _refresh(FlowModel.Params memory p) internal {
        vm.prank(updater);
        pool.refreshCapacity(PAIR_ID, p.bidCapacity, p.askCapacity);
    }

    /// @dev The mocks are freely mintable, so the taker never runs out. What it holds is not part
    ///      of the model — a taker constrained by its own balance would be a fourth, accidental
    ///      population.
    function _mintTo(address who, address token, uint256 amount) internal {
        MockERC20(token).mint(who, amount);
    }

    // ---------------------------------------------------------------------
    // Output
    // ---------------------------------------------------------------------

    function _report(FlowModel.Result memory res, FlowModel.Params memory p) internal pure {
        FlowModel.printAttribution(res, p.quoteDecimals);
        FlowModel.printBuckets(res, p.quoteDecimals);
        FlowModel.printNav(res, p.baseDecimals, p.quoteDecimals);
        if (res.worstFillNotional != 0) {
            console2.log(
                string.concat(
                    "    worst single fill: ",
                    FlowModel.signedUnits(res.worstFillMo60, p.quoteDecimals),
                    " at +60s on ",
                    FlowModel.units(res.worstFillNotional, p.quoteDecimals),
                    " of notional, at t=",
                    FlowModel.u2s(FlowModel.tickToMs(res.worstFillAtTick, p)),
                    "ms"
                )
            );
        }
        FlowModel.printVerdict(res, p.quoteDecimals);
    }

    function _sweepHeader(string memory title, string memory firstCols) internal pure {
        _sweepHeader(title, firstCols, "900s horizon, +100 bp jump at t=450, everything else at the defaults.");
    }

    function _sweepHeader(string memory title, string memory firstCols, string memory setup) internal pure {
        console2.log("");
        FlowModel.rule();
        console2.log(string.concat(" SWEEP: ", title));
        console2.log(string.concat("   ", setup));
        FlowModel.rule();
        console2.log(
            string.concat("  ", FlowModel.pad(firstCols, 30), "fills  toxic%   spread($)      MO+60s($)     MO+60s(bp)")
        );
    }

    function _sweepRow(string memory col1, string memory col2, FlowModel.Result memory res, FlowModel.Params memory p)
        internal
        pure
    {
        FlowModel.Accum memory all = _total(res);
        uint256 toxic = res.byPop[FlowModel.POP_INFORMED].notional + res.byPop[FlowModel.POP_FAST].notional;

        string memory left = string.concat(
            "  ",
            FlowModel.pad(col1, 18),
            FlowModel.pad(col2, 12),
            FlowModel.pad(FlowModel.u2s(all.fills), 7),
            FlowModel.pad(FlowModel.pct2(all.notional == 0 ? 0 : (toxic * 10_000) / all.notional), 9)
        );
        console2.log(
            string.concat(
                left,
                FlowModel.pad(FlowModel.signedUnits(all.spread, p.quoteDecimals), 15),
                FlowModel.pad(FlowModel.signedUnits(all.mo60, p.quoteDecimals), 14),
                FlowModel.sbps(FlowModel.bpsOf(all.mo60, all.notional))
            )
        );
    }

    function _total(FlowModel.Result memory res) internal pure returns (FlowModel.Accum memory all) {
        for (uint256 i; i < FlowModel.POP_COUNT; ++i) {
            all.fills += res.byPop[i].fills;
            all.notional += res.byPop[i].notional;
            all.spread += res.byPop[i].spread;
            all.mo1 += res.byPop[i].mo1;
            all.mo10 += res.byPop[i].mo10;
            all.mo60 += res.byPop[i].mo60;
        }
    }
}
