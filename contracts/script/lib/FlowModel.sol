// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {console2} from "forge-std/console2.sol";

import {IPropPool} from "../../src/interfaces/IPropPool.sol";
import {PropCurve} from "../../src/libraries/PropCurve.sol";

/// @title FlowModel — the taker populations, and the accounting that judges them
///
/// This library is the whole model. `test/Simulation.t.sol` and `script/Simulate.s.sol` are two
/// harnesses over it, so the in-EVM answer and the on-chain answer come from one definition of
/// what a taker is.
///
/// =====================================================================================
/// WHAT THIS EXISTS TO ANSWER
/// =====================================================================================
///
/// One question: **does the 5 bp half-spread cover the adverse selection the pool actually
/// faces?** `test/Integration.t.sol` and `script/Demo.s.sol` both say, in as many words, that
/// they do not measure this — their reference mid is right by fiat, so a stale ladder is never
/// picked off and every fill is profitable by construction.
///
/// A simulator that only sends random uninformed trades reproduces exactly that defect at
/// greater length. Uninformed flow pays the spread *by construction*: it does not look at the
/// price, so it cannot avoid paying it, so the pool books 5 bp on every fill no matter how badly
/// it is configured. The output would be a profit at any spread, any capacity, any latency, and
/// therefore no information at all. So the flow here contains traders who look at the price.
///
/// =====================================================================================
/// THE TWO CLOCKS — read this before reading any latency number this model produces
/// =====================================================================================
///
/// EVM `block.timestamp` is denominated in **whole seconds**. `vm.warp` cannot express 200 ms,
/// and `PropPool`'s staleness cliff and capacity ramp both read `block.timestamp`. A single clock
/// therefore cannot be made sub-second, so this model runs two and never conflates them:
///
///   * **MODEL time** — a tick of [`Params.tickMs`] milliseconds (default 200, matching GIWA's
///     flashblock preconfirmation cadence). The reference path, taker arrivals, maker decisions
///     and *every latency* are denominated in these ticks. This clock is as fine as `tickMs`.
///   * **BLOCK time** — still whole seconds, advanced by the harness once every `1000 / tickMs`
///     model ticks. Everything the *contract* reads — `updatedAt`, the staleness ramp, the epoch
///     — sees this clock and only this clock.
///
/// **The consequence, stated plainly because a reader has to know which of the two they are
/// looking at: this model measures sub-second LATENCY faithfully and sub-second STALENESS not at
/// all.** A ladder pushed at t=1,200 ms and one pushed at t=1,800 ms carry the same on-chain
/// `updatedAt` of 1 s, so `PropPool`'s decay ramp cannot tell them apart and a `decaySecs` of 60
/// is a 60-step staircase no matter how fine the tick is. Two specific residues follow:
///
///   1. **The ramp quantises.** At `tickMs = 200` the depth available to a picker-off falls in
///      five equal jumps per `decaySecs/60`th of the ramp instead of continuously, and a
///      withdrawal that lands 200 ms after the push is indistinguishable on chain from one that
///      lands instantly. Sub-second *decay* is not measured here and no number below should be
///      read as if it were.
///   2. **Same-second pushes collide.** Two `updateQuote` calls inside one block second both
///      write the same `updatedAt`. That is what really happens on a 1 s chain, so it is right —
///      but it means the model cannot represent a quote that is 200 ms fresher than another.
///
/// What *is* faithful at 200 ms is the thing this was built for: how long a wrong quote stays up,
/// who gets to trade against it in the meantime, and whether shortening that window pays.
///
/// **Rescaling.** Changing `tickMs` must change the model's *resolution* and not its *experiment*,
/// which needs two different rescalings that are easy to confuse:
///
///   * **Volatility takes the square root.** `sigma_tick = sigma_sec * sqrt(tickMs / 1000)`,
///     because variance is what adds over independent increments. Applying the linear factor here
///     would make a 200 ms run 2.2x less volatile per second than the 1 s run it is meant to
///     reproduce.
///   * **Arrivals do not.** They scale in tick *duration*. For the uninformed population that is
///     exactly linear in the rate, because it is a counting process and its invariant is the
///     expected number of fills per hour. For the informed population the invariant is different
///     and so is the arithmetic — see [`informedHazardPerTick`].
///
/// [`Params.tickMs`] must divide 1000, and `tickMs = 1000` reproduces the one-second model
/// **bit for bit**: identical amplitude, identical RNG draws, identical run. That is the
/// regression control, and it is exact rather than statistical on purpose.
///
/// =====================================================================================
/// THE THREE POPULATIONS
/// =====================================================================================
///
/// **UNINFORMED** — a Bernoulli arrival on each model tick at
/// `uninformedPerHour * tickMs / 3_600_000`, direction a fair coin, size from the heavy-tailed
/// distribution in [`drawNotional`]. It never looks at the pool's price and never declines a
/// fill. This is the flow a market maker wants and the only flow that pays for anything. Its
/// arrival rate is linear in the tick, so the expected fills per wall-clock hour is invariant to
/// the resolution — see [`uninformedRatePerTick`].
///
/// **INFORMED** — arrives only when the pool's executable quote is wrong relative to the
/// reference, and takes the side that profits from the error. Its arrival rate is
/// [`informedHazard`], a function of how wrong the quote is. Its *size* is not drawn from a
/// distribution: it takes the whole depth that is profitable to take ([`informedAskSize`] /
/// [`informedBidSize`]), because that is what an arbitrageur does.
///
/// **FAST** — the latency-advantaged taker. Identical to INFORMED in what it wants, different
/// in when it gets it: it sees the reference move in the same tick and lands *before* the
/// updater's `updateQuote` for that tick, so it takes the last look at the pre-move ladder. On a
/// chain whose sequencer orders by fee this is not a pathological assumption, it is the ordinary
/// case for anyone willing to outbid a quoting bot that runs at zero priority fee (which
/// `dubu-updater`'s `tx.rs` does, deliberately). It fires with probability `fastHitPct` whenever
/// there is a positive edge, rather than through the hazard.
///
/// =====================================================================================
/// THE ARRIVAL FUNCTION — the single choice that determines the answer
/// =====================================================================================
///
/// ```text
///   netEdge = max(0, edgeBps - informedCostBps)
///   lambda(edge) = informedMaxPerSec * netEdge^2 / (netEdge^2 + e50^2)
/// ```
///
/// A Hill function of order 2 on the edge *net of the taker's own cost*. Four properties, each
/// of which is a claim about the world that can be argued with:
///
/// 1. **Exactly zero below `informedCostBps`.** Not asymptotically zero — zero. An arbitrage
///    that does not cover the arbitrageur's gas plus the slippage of unwinding the position
///    somewhere else is not taken, at any patience. A hazard that is merely small at 1 bp says
///    that a permanent 1 bp error is eventually taken, which over a long enough run is a
///    guaranteed pick-off and is wrong. The default 1 bp is generous on GIWA, where gas is
///    0.001 gwei and the whole cost is the unwind.
///
/// 2. **Convex just above the threshold.** Searchers are heterogeneous — different gas
///    tolerances, different unwind venues, different alertness — so the number of them for whom
///    a given edge clears their own cost is the CDF of a cost distribution, not a step. `n = 2`
///    gives a smooth, roughly logistic CDF shape; `n = 1` would be too flat near the threshold
///    and would have a fat left tail of takers acting on edges that do not pay them.
///
/// 3. **Saturating, at one per second.** `informedMaxPerSec` defaults to 1.0, which at a 1 s
///    block time is one per block. This is a hard fact about the chain, not a taste: the pick-off
///    is a race, the winner takes the whole profitable depth in one transaction, and the
///    second-place finisher finds nothing left. Making the rate unbounded in the edge would claim
///    that a 500 bp error draws ten times the flow of a 50 bp error, which is false — both draw
///    exactly one winner, and the pool loses its whole exposed depth either way. The rate is
///    quoted per *second* and stays that way at every tick size; [`informedHazardPerTick`] does
///    the conversion, and the conversion is deliberately not the obvious one.
///
/// 4. **`e50` is tied to the pool's own half-spread, not picked.** The default is
///    `2 * halfSpreadBps`, i.e. the point at which the error is as large again as the protection
///    against it. Anchoring it to a parameter of the pool rather than to a number chosen by us
///    is what stops the arrival function from being tuned until the answer is flattering.
///    **It is still the assumption that drives the result**, so
///    `test_sensitivityToTheInformedArrivalFunction` re-runs the whole simulation across a range
///    of `e50` and prints every answer, not just the convenient one.
///
/// The edge is measured against `PropCurve.executableTopBid` / `executableTopAsk` — the price a
/// taker gets for an infinitesimal size at the *current* epoch usage. That is the same number
/// `dubu-updater`'s `policy.rs` triggers on, so the taker and the maker are looking at one
/// quantity. It slightly overstates attractiveness, because the informed trader's realised
/// average price is worse than the top; the size solver corrects for that exactly, the hazard
/// does not.
///
/// =====================================================================================
/// SIZE — heavy-tailed, and the tail is stated rather than fitted
/// =====================================================================================
///
/// [`drawNotional`] is a four-decade mixture with probabilities 0.900 / 0.090 / 0.009 / 0.001.
/// Each decade is a tenth as likely as the one below it, which is a Pareto tail index of exactly
/// 1: `P(size > 10^k)` falls by 10x per decade. That makes each decade contribute the same
/// expected notional (~$495 of the ~$2,035 mean), which is the signature of an alpha-1 tail and
/// is roughly what trade-size distributions on liquid venues look like. Stating the mixture
/// beats fitting one, because every number in it is visible and arguable.
///
/// =====================================================================================
/// WHAT IS MEASURED
/// =====================================================================================
///
/// Per fill, from the pool's side, in quote units. `h` is in wall-clock seconds and the index is
/// in model ticks, so the lookup is `k + h * ticksPerSec` — a "+60s" column is sixty seconds at
/// every `tickMs` rather than sixty ticks:
///
/// ```text
///   ask (pool sold q base for x quote):  pnl(h) = x - q * ref[k + h*n] / scale
///   bid (pool bought q base for x quote): pnl(h) = q * ref[k + h*n] / scale - x
/// ```
///
/// `pnl(0)` is the **realised spread** against the reference at fill time. `pnl(1)`, `pnl(10)`
/// and `pnl(60)` are the **markouts**: what the fill was worth once the reference had a chance
/// to reveal whether the taker knew something. A fill that earns 5 bp of spread and marks out at
/// -40 bp after a minute was not a good fill; it was a free option the pool wrote. Markout is
/// the number this whole exercise exists to produce.
///
/// The NAV decomposition mirrors `dubu-updater`'s `risk.rs` exactly, including its integer
/// valuation (`amountOutBid` against a flat ladder at the fair value), so the simulator's P&L
/// and the bot's killswitch are reading the same book:
///
/// ```text
///   NAV         = quoteBalance + value(baseBalance, fair)
///   revaluation = value(basePrev, fairNow) - value(basePrev, fairPrev)
///   tradePnl    = (NAV_now - NAV_prev) - revaluation
/// ```
///
/// =====================================================================================
/// WHAT THIS MODEL CANNOT CAPTURE
/// =====================================================================================
///
/// Stated here rather than in a footnote, because a simulator's limits are part of its output:
///
///  * **The informed trader is a price-taker against a fixed reference.** It does not move the
///    reference, so there is no feedback from the pool's fills back into the price the pool is
///    quoting against. On a venue this small that is right; on a venue that is a meaningful
///    share of the market it would understate the damage.
///  * **No competition between makers.** There is no second venue, so the uninformed arrival
///    rate does not respond to how good the pool's price is. Widening the spread therefore
///    *never* costs volume in this model, which biases every "should the spread be wider"
///    conclusion in favour of wider. Read the spread sweep with that in mind.
///  * **No mempool.** Ordering inside a tick is a fixed rule (fast taker, then the landing
///    update, then informed, then uninformed), not an auction. A real sequencer would sometimes
///    order the update first, which is strictly better for the pool than what is modelled.
///  * **Sub-second STALENESS, as opposed to sub-second latency.** See THE TWO CLOCKS above. The
///    pool's `updatedAt`, its staleness cliff and its capacity ramp all read `block.timestamp`
///    and so quantise to one second at every value of `tickMs`. A result about how fast the
///    updater reacts is measured at the tick; a result about how the ramp decays is not.
///  * **Sub-tick ordering.** Everything inside one model tick is simultaneous, so at
///    `tickMs = 200` the model cannot represent a searcher who is 50 ms faster than another. The
///    fast taker exists precisely to stand in for that, and it is a coin flip rather than a race
///    that anyone could win by spending more.
///  * **Uniform, not Gaussian, diffusion increments.** The random walk's per-tick increment is
///    uniform with the stated standard deviation. A uniform has thinner tails than a normal, so
///    the diffusion paths *understate* the frequency of multi-sigma moves. The jump paths exist
///    because that is where adverse selection actually lives, and putting it in explicitly is
///    more honest than hoping a thin-tailed walk produces it.
///  * **No hedging, and no inventory-driven skew.** The pool quotes symmetrically around the
///    reference forever. `dubu-updater` ships `skew_bps` as a static 0 and its README lists an
///    inventory-driven skew as not built, so this matches the deployed bot rather than an
///    aspirational one.
///  * **Gas is ignored in the pool's P&L.** Every `updateQuote` costs ~29k gas and the busy
///    configurations here push one most seconds. At GIWA's 0.001 gwei that is a rounding error;
///    on a chain where it is not, subtract it.
library FlowModel {
    // =====================================================================
    // Units
    // =====================================================================

    uint256 internal constant BPS = 10_000;

    /// @notice Every bp quantity in this library is carried in **hundredths of a bp**, so a
    ///         report can show two decimals with integer arithmetic. `BPS_E2` is therefore
    ///         "100%" in those units.
    uint256 internal constant BPS_E2 = 1_000_000;

    /// @notice Probability denominator. `PROB_ONE` is certainty.
    uint256 internal constant PROB_ONE = 1_000_000;

    uint256 internal constant SECS_PER_HOUR = 3_600;

    // Populations, as array indices rather than an enum so the accumulators can be plain arrays.
    uint256 internal constant POP_UNINFORMED = 0;
    uint256 internal constant POP_INFORMED = 1;
    uint256 internal constant POP_FAST = 2;
    uint256 internal constant POP_COUNT = 3;

    /// @notice Notional decades, in whole quote tokens: <1k, 1k-10k, 10k-100k, 100k-1M, >=1M.
    uint256 internal constant BUCKET_COUNT = 5;

    /// @notice The three markout horizons, in **wall-clock seconds**. Seconds rather than ticks
    ///         deliberately: a markout is an economic horizon, not a simulation artefact, and
    ///         "+60s" has to mean the same thing at every resolution or two runs at different
    ///         `tickMs` would not be comparable. [`markoutTicks`] converts.
    ///
    /// @dev The path is built with `MARKOUT_LONG` seconds of extra tail so `ref[k + 60s]` is
    ///      defined for every fill.
    uint256 internal constant MARKOUT_SHORT = 1;
    uint256 internal constant MARKOUT_MID = 10;
    uint256 internal constant MARKOUT_LONG = 60;

    /// @notice Milliseconds in a second. The only place the two clocks meet.
    uint256 internal constant MS_PER_SEC = 1_000;

    // =====================================================================
    // Types
    // =====================================================================

    /// @notice Explicit, seeded PRNG state. Held in a struct so a memory reference threads the
    ///         state through every draw without the caller carrying it.
    struct Rng {
        uint256 s;
    }

    /// @notice Every knob. Nothing in this library reads a constant that is not here, so a run
    ///         is fully described by one of these plus a seed.
    struct Params {
        // --- the market ---
        /// @dev Reference price at t=0, in the pair's own price units.
        uint256 mid;
        uint8 priceScaleExp;
        uint8 baseDecimals;
        uint8 quoteDecimals;
        /// @dev The run's length in **wall-clock seconds**, which is what it has always meant.
        ///      Changing `tickMs` changes how many model ticks that is, never how long the run is.
        uint256 horizonSecs;
        uint256 seed;
        /// @dev **The model clock**, in milliseconds. Must divide 1000. Default 200, matching
        ///      GIWA's flashblock preconfirmation cadence; 1000 is the regression control and
        ///      reproduces the one-second model bit for bit. See THE TWO CLOCKS in the header,
        ///      and in particular what it says about what does *not* get finer when this does.
        uint256 tickMs;
        // --- the reference path ---
        /// @dev Per-**second** volatility, hundredths of a bp. ETHUSDT at ~55% annualised is ~1 bp
        ///      per second: 0.55 / sqrt(365*24*3600) = 9.8e-5. Per second at every `tickMs`:
        ///      [`buildPath`] applies the `sqrt(tickMs/1000)` rescaling, so this number keeps one
        ///      meaning and a run at a finer tick is not quietly a calmer market.
        uint256 sigmaE2;
        /// @dev Per-second drift, hundredths of a bp, signed.
        int256 driftE2;
        /// @dev **Second** at which the jump lands. 0 disables it. A jump is a gap, so it lands
        ///      entirely inside one model tick however fine the tick is.
        uint256 jumpAtSecs;
        /// @dev Jump size, hundredths of a bp, signed.
        int256 jumpE2;
        // --- the maker, mirroring dubu-updater's updater.toml ---
        uint256 halfSpreadBps;
        uint256 widthBps;
        uint96 bidCapacity;
        uint96 askCapacity;
        /// @dev **Milliseconds** between the updater deciding to push and the push landing. This
        ///      is the window the free option lives in, it is the parameter the pool controls,
        ///      and it is the reason this model has a sub-second clock at all.
        ///
        ///      Milliseconds rather than seconds because the whole open question is whether the
        ///      2,000 ms round trip the bot has today (wake on `newHeads` at 1 Hz, land a block
        ///      later) is worth replacing with the ~400 ms one that polling GIWA's flashblocks
        ///      `pending` tag at 200 ms would give. A parameter denominated in seconds cannot
        ///      express the alternative, which is exactly why every latency conclusion this file
        ///      produced before now was bounded at one second.
        ///
        ///      Rounded DOWN to whole ticks, but never to zero: a latency below one tick is one
        ///      tick, because a transaction that has to be signed and land cannot take no time.
        uint256 quoteLatencyMs;
        /// @dev **Milliseconds between the updater WAKING UP**, as distinct from how long its
        ///      transaction then takes to land. Default 1,000: `dubu-updater` subscribes to
        ///      `newHeads` and evaluates its triggers once a block.
        ///
        ///      This parameter exists because without it a finer `tickMs` would silently upgrade
        ///      the bot. The old loop evaluated the trigger policy on every tick, which at one
        ///      second a tick happened to be the bot's real cadence; carry that rule to a 200 ms
        ///      tick and the modelled bot starts polling five times a second for free, shortening
        ///      its response by up to 800 ms with no code written and no round trip changed. The
        ///      200 ms run would then beat the 1,000 ms run for a reason that is not latency, and
        ///      it would look exactly like a finding.
        ///
        ///      Keep it at 1,000 to change resolution only. Set it to 200 to model the build that
        ///      polls GIWA's flashblocks `pending` tag instead of waiting for `newHeads` — that is
        ///      a different bot, and it should have to earn its result against this one.
        ///
        ///      Floored at one tick, so a coarse clock cannot represent a bot faster than itself.
        uint256 decisionIntervalMs;
        /// @dev Milliseconds between runs of the jump detector. Default 200, because `jump.rs`
        ///      already scans its in-memory feed snapshots at that cadence rather than waiting for
        ///      the 1 Hz quote cycle — the detector is the one part of the bot that is *already*
        ///      sub-second in production.
        ///
        ///      Floored at one tick, which means the 1,000 ms control necessarily runs the
        ///      detector at 1 Hz and so handicaps it against the real thing. That handicap is not
        ///      a bug in the control, it is the coarse clock's limit showing through, and it is
        ///      one of the reasons the defended run improves at 200 ms. Setting this to 1,000
        ///      isolates how much of that improvement is the detector rather than the clock.
        uint256 detectorIntervalMs;
        uint256 adverseDriftE2;
        uint256 favourableDriftE2;
        uint256 heartbeatSecs;
        uint256 capacityDivergencePct;
        // --- the defences, mirroring updater.toml and PropPool's capacity ramp ---
        /// @dev Volatility coefficient, hundredths of a bp of half-spread per bp of sigma.
        ///      `halfSpread = min(halfSpreadBps + s1 * sigma_300s, capBps)`. 50 == 0.5, the value
        ///      `updater.toml` derives from this simulator's own half-spread sweep. Zero
        ///      reproduces the pre-defence constant spread, which is how the before column of
        ///      every comparison here is produced.
        uint256 s1E2;
        /// @dev Ceiling on the scaled half-spread. An unbounded spread in a volatility spike is a
        ///      quoting outage wearing a disguise.
        uint256 halfSpreadCapBps;
        /// @dev A reference move at or above this over a trailing **one-second** window trips the
        ///      jump detector, in hundredths of a bp. Zero disables it.
        ///
        ///      The window is one second at every `tickMs`, not one tick, and that is a
        ///      correctness requirement rather than a convenience: a threshold applied to
        ///      consecutive 200 ms ticks is a five-times-higher bar in sigma than the same
        ///      threshold applied to consecutive seconds, so a detector defined per-tick would
        ///      get quietly harder to trip every time the clock got finer. Trailing-window also
        ///      matches `jump.rs`, which scans a feed history rather than a pair of samples.
        uint256 jumpThresholdE2;
        /// @dev How long quotes stay withdrawn after a jump, in seconds. The cost of a false
        ///      positive is one of these in foregone spread.
        uint256 cooloffSecs;
        /// @dev **Milliseconds** between the detector firing and the withdrawal LANDING. This is
        ///      the whole question: a withdrawal is a transaction, it races the searcher, and on a
        ///      fee-ordered sequencer the searcher can outbid it. A value of zero would model a
        ///      wish rather than a defence — and note that zero is now *representable but still
        ///      wrong*, because the floor is one tick rather than one second.
        uint256 withdrawLatencyMs;
        /// @dev PropPool's staleness ramp, **seconds, and seconds is not a rounding of something
        ///      finer**. The contract computes the ramp from `block.timestamp - updatedAt`, both
        ///      of which are whole seconds, so this stays quantised at every `tickMs`. Depth falls
        ///      linearly from full at the moment of the push to zero at this age. Zero disables
        ///      it, which is the pool's default.
        uint16 decaySecs;
        // --- the flow ---
        /// @dev Arrivals per wall-clock hour, and per wall-clock hour at every `tickMs`.
        uint256 uninformedPerHour;
        /// @dev Saturation arrival rate, per **second**, scaled by 1e6. 1e6 == one per second,
        ///      which at a 1 s block time is one per block. Never per tick — see
        ///      [`informedHazardPerTick`] for why the two must not be confused.
        uint256 informedMaxPerSecE6;
        /// @dev Net edge at which the hazard reaches half its saturation rate.
        uint256 informedHalfMaxE2;
        /// @dev The dead zone: gas plus unwind cost, below which nothing is taken.
        uint256 informedCostE2;
        /// @dev 100 = a competitive race that clears the whole profitable depth. 50 = a lone
        ///      profit-maximising arbitrageur, which on a linear ladder takes exactly half.
        uint256 informedCapturePct;
        /// @dev Probability the latency-advantaged taker wins the race, in percent.
        uint256 fastHitPct;
    }

    /// @notice One population's, or one size bucket's, running totals.
    struct Accum {
        uint256 fills;
        /// @dev Base leg valued at the reference at fill time, in quote units.
        uint256 notional;
        /// @dev Markout at h = 0: the realised spread against the reference. Quote units, signed.
        int256 spread;
        int256 mo1;
        int256 mo10;
        int256 mo60;
    }

    /// @notice Everything one run produced.
    struct Result {
        Accum[3] byPop;
        Accum[5] byBucket;
        /// @dev Trades the pool refused: over capacity, stale, or against an empty reserve. A
        ///      refusal is a real outcome and is counted, not silently dropped.
        uint256 declinedFills;
        uint256 declinedNotional;
        uint256 quotePushes;
        uint256 capacityRefreshes;
        /// @dev The NAV walk, mirroring risk.rs. `tradePnl` should reconcile with the sum of
        ///      `byPop[*].spread` to within one quote unit per observation.
        uint256 navStart;
        uint256 navEnd;
        int256 revaluation;
        int256 tradePnl;
        uint256 baseStart;
        uint256 baseEnd;
        uint256 quoteStart;
        uint256 quoteEnd;
        uint256 baseMin;
        uint256 baseMax;
        /// @dev Worst single fill by markout at +60s, for the "what did this actually look like"
        ///      line in the report.
        int256 worstFillMo60;
        uint256 worstFillNotional;
        /// @dev MODEL tick, not second and not block. [`tickToMs`] turns it back into wall clock.
        uint256 worstFillAtTick;
        /// @dev How many whole block seconds the run advanced through. `horizonSecs` by
        ///      construction, carried anyway so a report can state the two clocks side by side
        ///      instead of asking the reader to trust that they line up.
        uint256 blockSecsAdvanced;
        uint256 ticksRun;
    }

    // =====================================================================
    // RNG — keccak counter, so a seed reproduces a run exactly
    // =====================================================================

    function rng(uint256 seed) internal pure returns (Rng memory) {
        return Rng({s: uint256(keccak256(abi.encode("dubu.flowmodel", seed)))});
    }

    function next(Rng memory r) internal pure returns (uint256) {
        r.s = uint256(keccak256(abi.encode(r.s)));
        return r.s;
    }

    /// @notice Uniform on `[0, n)`.
    function uniform(Rng memory r, uint256 n) internal pure returns (uint256) {
        return n == 0 ? 0 : next(r) % n;
    }

    /// @notice True with probability `pE6 / PROB_ONE`.
    function bernoulli(Rng memory r, uint256 pE6) internal pure returns (bool) {
        if (pE6 == 0) return false;
        if (pE6 >= PROB_ONE) return true;
        return next(r) % PROB_ONE < pE6;
    }

    // =====================================================================
    // The two clocks — every conversion between them, in one place
    // =====================================================================

    /// @notice Model ticks per whole block second.
    ///
    /// @dev `tickMs` must divide 1000. Not a convenience: if it did not, the number of ticks in a
    ///      second would vary from second to second, the per-second arrival probability the
    ///      informed hazard is defined by would vary with it, and two runs at the same nominal
    ///      volatility would face different flow. A divisor keeps the block clock a strict
    ///      coarsening of the model clock, which is the property everything below assumes.
    function ticksPerSec(Params memory p) internal pure returns (uint256) {
        require(
            p.tickMs != 0 && p.tickMs <= MS_PER_SEC && MS_PER_SEC % p.tickMs == 0, "FlowModel: tickMs must divide 1000"
        );
        return MS_PER_SEC / p.tickMs;
    }

    /// @notice How many model ticks the run's wall-clock horizon is.
    function totalTicks(Params memory p) internal pure returns (uint256) {
        return p.horizonSecs * ticksPerSec(p);
    }

    function secsToTicks(uint256 secs, Params memory p) internal pure returns (uint256) {
        return secs * ticksPerSec(p);
    }

    /// @notice The markout horizon `secs` expressed in ticks.
    function markoutTicks(uint256 secs, Params memory p) internal pure returns (uint256) {
        return secs * ticksPerSec(p);
    }

    /// @notice A latency in milliseconds, as whole model ticks.
    ///
    /// @dev Rounded DOWN, with a floor of one tick for any non-zero latency. Down because a
    ///      latency the clock cannot resolve should not be inflated into one it can; the floor
    ///      because rounding a 200 ms round trip to zero at a 1,000 ms tick would model
    ///      instantaneous requoting, which is the wish this whole exercise exists to avoid. The
    ///      floor is therefore also the honest statement of the coarse clock's limit: at
    ///      `tickMs = 1000`, every latency below a second **is** one second, and that is precisely
    ///      why the sub-second sweep has to be run at a finer tick to mean anything.
    function msToTicks(uint256 ms, Params memory p) internal pure returns (uint256) {
        if (ms == 0) return 0;
        uint256 k = ms / p.tickMs;
        return k == 0 ? 1 : k;
    }

    /// @notice The BLOCK second a model tick falls in — the only thing the contract can see.
    /// @dev Tick `k` is the instant `k * tickMs` milliseconds into the run, so it belongs to block
    ///      second `floor(k * tickMs / 1000)`. The first block second therefore carries one tick
    ///      fewer than the rest (the tick at ms 0 is the pre-loop setup), which is a boundary
    ///      artefact of one tick in the whole horizon and is left visible rather than papered over.
    function tickToBlockSecs(uint256 k, Params memory p) internal pure returns (uint256) {
        return (k * p.tickMs) / MS_PER_SEC;
    }

    /// @notice Wall-clock milliseconds at tick `k`. Exact — this is the clock that is fine.
    function tickToMs(uint256 k, Params memory p) internal pure returns (uint256) {
        return k * p.tickMs;
    }

    /// @notice `floor(sqrt(x))`, Babylonian. Needed because volatility rescales by a square root
    ///         and this library carries no fixed-point maths dependency.
    function isqrt(uint256 x) internal pure returns (uint256 y) {
        if (x == 0) return 0;
        uint256 z = (x + 1) / 2;
        y = x;
        while (z < y) {
            y = z;
            z = (x / z + z) / 2;
        }
    }

    // =====================================================================
    // The reference path
    // =====================================================================

    /// @notice Build the whole reference path up front, on the MODEL clock, including
    ///         `MARKOUT_LONG` seconds of tail.
    ///
    /// @dev Indexed by model tick, so `ref[k]` is the reference `k * tickMs` milliseconds into the
    ///      run. Precomputed rather than evaluated lazily because markout needs `ref[k + 60s]` at
    ///      the moment of the fill, and a random walk has no closed form to look that up with. The
    ///      array is `(horizon + 60) * ticksPerSec + 1` words — 18,300 at the default hour and
    ///      200 ms, still cheap, and it turns every markout into an array read.
    ///
    ///      Increments are multiplicative and **uniform**, not Gaussian: `inc` is uniform on
    ///      `[-A, +A]` with `A = sigma_tick * sqrt(3)`, which has standard deviation exactly
    ///      `sigma_tick`. A uniform has no tail at all beyond `sqrt(3)` sigma, so a diffusion-only
    ///      path here systematically produces *fewer* large moves than a real one. That
    ///      understates adverse selection, which is the safe direction for a tool whose job is to
    ///      find it — and it is why the jump is a separate, explicit term rather than something
    ///      the walk is hoped to generate.
    ///
    ///      **The two rescalings, and why they are not the same one.**
    ///
    ///      *Volatility takes the square root.* `sigma_tick = sigma_sec * sqrt(tickMs / 1000)`,
    ///      because the variance of a sum of independent increments adds while its standard
    ///      deviation does not. Getting this wrong is not a cosmetic error: applying the linear
    ///      factor would make the 200 ms path carry a fifth of the per-second variance of the 1 s
    ///      path, the reference would gap less, the informed hazard would fire less, and the run
    ///      would report that a finer clock is worth thousands of dollars — a conclusion entirely
    ///      manufactured by the rescaling. `test_tickRescalingIsResolutionNotExperiment` measures
    ///      the realised per-second volatility of the path at four tick sizes for exactly this
    ///      reason, rather than trusting the algebra above.
    ///
    ///      *Drift does not.* It is differenced out of an exact cumulative
    ///      (`drift * k * tickMs / 1000`) rather than divided per tick, so a drift too small to
    ///      express in one tick still accumulates at the right rate over the horizon instead of
    ///      truncating to nothing. At 0.20 bp/s and a 200 ms tick a naive per-tick division would
    ///      be exact anyway; at 0.07 bp/s it would lose 29% of the trend.
    ///
    ///      **At `tickMs = 1000` this function is bit-for-bit what it was before it had a tick.**
    ///      `isqrt(1000 * 1000)` is exactly 1000, so the amplitude reduces to `sigma * 1732/1000`
    ///      with the same truncation as before; the differenced drift reduces to `driftE2`; and
    ///      the RNG therefore draws from the same modulus in the same order. That is the control.
    function buildPath(Params memory p) internal pure returns (uint256[] memory ref) {
        uint256 tps = ticksPerSec(p);
        ref = new uint256[]((p.horizonSecs + MARKOUT_LONG) * tps + 1);
        ref[0] = p.mid;

        Rng memory r = rng(p.seed ^ uint256(keccak256("path")));
        // sigma_tick * sqrt(3), in hundredths of a bp. The sqrt is taken over `tickMs * 1000` so
        // that the 1e6 divisor below carries both the 1732/1000 and the 1/1000 of the root's
        // scaling in one integer division, and so that tickMs == 1000 lands on the old constant.
        uint256 amplitude = (p.sigmaE2 * 1_732 * isqrt(p.tickMs * MS_PER_SEC)) / 1_000_000;
        uint256 jumpAtTick = p.jumpAtSecs * tps;

        for (uint256 k = 1; k < ref.length; ++k) {
            int256 incE2 = _cumulativeDrift(p, k) - _cumulativeDrift(p, k - 1);
            if (amplitude != 0) {
                // forge-lint: disable-next-line(unsafe-typecast)
                incE2 += int256(uniform(r, 2 * amplitude + 1)) - int256(amplitude);
            }
            // A jump is a gap: it lands entirely inside one tick at every resolution, which is
            // what makes the 200 ms and 1 s runs face the same shock rather than a smeared one.
            if (p.jumpAtSecs != 0 && k == jumpAtTick) incE2 += p.jumpE2;

            ref[k] = applyBpsE2(ref[k - 1], incE2);
            if (ref[k] == 0) ref[k] = 1; // a zero reference is not a market
        }
    }

    /// @notice Total drift from the start of the run to tick `k`, in hundredths of a bp.
    ///
    /// @dev [`buildPath`] differences this instead of dividing the per-second drift by
    ///      `ticksPerSec`, so that the truncation is taken once against the whole horizon rather
    ///      than once per tick. A drift of 0.07 bp/s at a 200 ms tick is 0.014 bp a tick, which
    ///      truncates to zero and would make the trend path a flat one; differencing the
    ///      cumulative gives the increments 0, 0, 1, 0, 1 and the trend arrives intact.
    ///
    ///      `k * tickMs` is at most a few tens of millions and `driftE2` is a bps figure, so the
    ///      product cannot approach the int256 bound.
    function _cumulativeDrift(Params memory p, uint256 k) private pure returns (int256) {
        // forge-lint: disable-next-line(unsafe-typecast)
        return (p.driftE2 * int256(k * p.tickMs)) / int256(MS_PER_SEC);
    }

    /// @notice `x * (1 + bpsE2 / BPS_E2)`, saturating at zero.
    function applyBpsE2(uint256 x, int256 bpsE2) internal pure returns (uint256) {
        if (bpsE2 >= 0) {
            // forge-lint: disable-next-line(unsafe-typecast)
            return (x * (BPS_E2 + uint256(bpsE2))) / BPS_E2;
        }
        // `bpsE2 < 0` here, and `type(int256).min` is excluded because every caller derives this
        // from a `uint256` volatility bounded by a few thousand.
        // forge-lint: disable-next-line(unsafe-typecast)
        uint256 down = uint256(-bpsE2);
        if (down >= BPS_E2) return 0;
        return (x * (BPS_E2 - down)) / BPS_E2;
    }

    // =====================================================================
    // Sizes
    // =====================================================================

    /// @notice A heavy-tailed uninformed order size, in **whole quote tokens** of notional.
    ///
    /// @dev Four decades, each a tenth as likely as the one below: a Pareto tail index of
    ///      exactly 1. `P(>$1k) = 10%`, `P(>$10k) = 1%`, `P(>$100k) = 0.1%`. Every decade
    ///      therefore carries the same expected notional (~$495 each, ~$2,035 in total), which
    ///      is what an alpha-1 tail means and is the property that makes "most flow is small,
    ///      occasionally something large" quantitative rather than decorative.
    ///
    ///      Within a decade the draw is uniform. That is coarser than a true power law inside
    ///      the decade, and it does not matter: the between-decade weights carry the tail.
    function drawNotional(Rng memory r) internal pure returns (uint256 whole) {
        uint256 u = uniform(r, 1_000);
        if (u < 900) return 100 + uniform(r, 900); // $100 - $999
        if (u < 990) return 1_000 + uniform(r, 9_000); // $1k - $9,999
        if (u < 999) return 10_000 + uniform(r, 90_000); // $10k - $99,999
        return 100_000 + uniform(r, 900_000); // $100k - $999,999
    }

    /// @notice Which of the five reporting decades a notional falls in.
    function bucketOf(uint256 notionalWhole) internal pure returns (uint256) {
        if (notionalWhole < 1_000) return 0;
        if (notionalWhole < 10_000) return 1;
        if (notionalWhole < 100_000) return 2;
        if (notionalWhole < 1_000_000) return 3;
        return 4;
    }

    function bucketLabel(uint256 i) internal pure returns (string memory) {
        if (i == 0) return "< $1k";
        if (i == 1) return "$1k-10k";
        if (i == 2) return "$10k-100k";
        if (i == 3) return "$100k-1M";
        return ">= $1M";
    }

    function popLabel(uint256 i) internal pure returns (string memory) {
        if (i == POP_UNINFORMED) return "uninformed";
        if (i == POP_INFORMED) return "informed";
        return "fast";
    }

    // =====================================================================
    // The informed arrival function
    // =====================================================================

    /// @notice Arrivals per second, scaled by 1e6, for an edge of `edgeE2` hundredths of a bp.
    ///
    /// @dev See the header for the four properties this shape is chosen for. In one line:
    ///      zero below the taker's own cost, convex just above it, saturating at one per block.
    function informedHazard(int256 edgeE2, Params memory p) internal pure returns (uint256) {
        // forge-lint: disable-next-line(unsafe-typecast)
        if (edgeE2 <= int256(p.informedCostE2)) return 0;
        // forge-lint: disable-next-line(unsafe-typecast)
        uint256 net = uint256(edgeE2) - p.informedCostE2;
        uint256 nn = net * net;
        uint256 half = p.informedHalfMaxE2 * p.informedHalfMaxE2;
        if (nn + half == 0) return 0;
        return (p.informedMaxPerSecE6 * nn) / (nn + half);
    }

    /// @notice The probability of an informed arrival in ONE MODEL TICK.
    ///
    /// @dev **This conversion is not `lambda * tickMs / 1000`, and the difference is the single
    ///      place where a careless rescaling would have produced a flattering answer.**
    ///
    ///      The invariant that has to survive a change of clock is *the probability that a given
    ///      wrong quote is picked off at all within a given second*. That is what the hazard is a
    ///      statement about: the header's third property says the pick-off is a race with exactly
    ///      one winner, so what the model claims is `P(somebody takes it)`, not a count. Preserve
    ///      the survival probability and the clock is a resolution knob:
    ///
    ///      ```text
    ///        (1 - q)^n = 1 - lambda        n = ticksPerSec,  q = per-tick probability
    ///      ```
    ///
    ///      Scaling linearly instead — `q = lambda / n` — preserves the expected *count*, which is
    ///      the wrong invariant for a race, and it does real damage at exactly the point the
    ///      headline lives. At the saturated `lambda = 1.0/s` of a 100 bp gap, linear scaling gives
    ///      `q = 0.2` and `P(taken within the second) = 1 - 0.8^5 = 67%`, against 100% at a 1 s
    ///      tick. The 200 ms run would then show a smaller loss than the 1 s run because *the
    ///      searcher had been weakened by a third*, and it would look exactly like a finding about
    ///      latency. The survival form gives `q = 1 - (1-1)^(1/5) = 1`: the gap is taken in the
    ///      first tick after it prints at both resolutions, which is what a saturated race means.
    ///
    ///      For small `lambda` the two agree to first order (`1 - (1-l)^(1/n) ~ l/n`), so this is
    ///      still "linear in tick duration" everywhere except near saturation — it just does not
    ///      stop being right there.
    ///
    ///      At `n == 1` it returns `lambda` unchanged, which is why the one-second control is bit
    ///      identical rather than merely close.
    function informedHazardPerTick(int256 edgeE2, Params memory p) internal pure returns (uint256) {
        uint256 perSec = informedHazard(edgeE2, p);
        uint256 n = ticksPerSec(p);
        // The three cases that carry almost every tick of a real run, and the only ones that are
        // hot: n == 1 is the control, a correct quote has no hazard at all, and a saturated race
        // is certain at any resolution.
        if (n == 1 || perSec == 0) return perSec;
        if (perSec >= PROB_ONE) return PROB_ONE;
        return PROB_ONE - _nthRootE6(PROB_ONE - perSec, n);
    }

    /// @dev Greatest `s` with `s^n <= v`, all in `PROB_ONE` fixed point. Bisected rather than
    ///      closed-form because this library has no fixed-point exp/log and does not need one:
    ///      the call is skipped entirely unless the quote is both wrong and unsaturated, which on
    ///      a jump path is a handful of ticks out of thousands.
    ///
    ///      Flooring the root over-states the per-tick arrival probability by less than 1e-6,
    ///      i.e. it rounds the searcher slightly stronger. That is the direction a risk tool
    ///      should round.
    function _nthRootE6(uint256 v, uint256 n) private pure returns (uint256) {
        uint256 lo;
        uint256 hi = PROB_ONE;
        while (lo < hi) {
            uint256 mid = lo + (hi - lo + 1) / 2;
            if (_powE6(mid, n) <= v) lo = mid;
            else hi = mid - 1;
        }
        return lo;
    }

    function _powE6(uint256 x, uint256 n) private pure returns (uint256 r) {
        r = PROB_ONE;
        for (uint256 i; i < n; ++i) {
            r = (r * x) / PROB_ONE;
        }
    }

    /// @notice Expected uninformed arrivals in one model tick, in `PROB_ONE` units.
    ///
    /// @dev Linear in the tick, and linear is right here for the reason it is wrong for the
    ///      informed hazard: uninformed flow is a counting process whose invariant is the expected
    ///      number of fills per wall-clock hour, not the probability that at least one shows up.
    ///      The caller takes the integer part as a guaranteed count and Bernoullis the remainder,
    ///      so the mean is preserved exactly — `uninformedPerHour` means the same thing at 1,000
    ///      ms and at 200 ms, and the volume that pays for everything does not move when the clock
    ///      does.
    function uninformedRatePerTick(Params memory p) internal pure returns (uint256) {
        return (p.uninformedPerHour * PROB_ONE * p.tickMs) / (SECS_PER_HOUR * MS_PER_SEC);
    }

    // =====================================================================
    // The ladder — byte for byte DubuScript._ladder and IntegrationTest._pushLadder
    // =====================================================================

    /// @notice The half-spread the updater would post right now, given realised volatility.
    ///
    /// @dev Mirrors `spread.rs`. The constant half-spread was the thing the baseline runs showed
    ///      to be indefensible: the absorption limit is `halfSpread + width/2`, it was fixed at
    ///      17.5 bp, and a jump larger than that costs the pool the full excess across the whole
    ///      epoch. Scaling by sigma is what makes the limit move with the risk it is sized for.
    ///
    ///      `sigmaE2` here is the per-second volatility the path was built with, scaled to the
    ///      300 s horizon the updater's estimator reports, so the coefficient means the same
    ///      thing in both places: `sigma_300 = sigma_1s * sqrt(300)`.
    function halfSpreadAt(Params memory p) internal pure returns (uint256) {
        if (p.s1E2 == 0) return p.halfSpreadBps;

        // sqrt(300) in fixed point, x1000, so the whole thing stays in integers.
        uint256 sigma300E2 = (p.sigmaE2 * 17_320) / 1000;
        uint256 scaled = p.halfSpreadBps + (p.s1E2 * sigma300E2) / 10_000;

        uint256 cap = p.halfSpreadCapBps == 0 ? type(uint256).max : p.halfSpreadCapBps;
        return scaled > cap ? cap : scaled;
    }

    /// @notice Did the reference move far enough in one second to count as a jump?
    ///
    /// @dev Mirrors `jump.rs`'s detector, on the same input it sees: a trailing window of the
    ///      reference feed. The threshold is absolute rather than a sigma multiple here because
    ///      the scripted paths hold sigma fixed, so the two are interchangeable within a run and
    ///      an absolute number is the one a reader can check against the jump they configured.
    ///
    ///      **`prevRef` is the reference one WALL-CLOCK SECOND ago, not one tick ago**, and the
    ///      caller is responsible for reaching back `ticksPerSec` entries rather than one. A
    ///      per-tick comparison would make the same `jumpThresholdE2` a 5x harder bar to clear at
    ///      a 200 ms tick than at a 1 s tick — the detector would silently get less sensitive
    ///      every time the clock got finer, and any comparison across `tickMs` would be measuring
    ///      that instead of latency. A gap still trips it on the tick it prints, because a gap is
    ///      inside the trailing window the instant it happens.
    function isJump(uint256 prevRef, uint256 nowRef, uint256 thresholdE2) internal pure returns (bool) {
        if (thresholdE2 == 0 || prevRef == 0) return false;
        uint256 diff = nowRef > prevRef ? nowRef - prevRef : prevRef - nowRef;
        return (diff * 1_000_000) / prevRef >= thresholdE2;
    }

    function ladder(uint256 mid, uint256 halfSpreadBps, uint256 widthBps)
        internal
        pure
        returns (uint256 minBid, uint256 maxBid, uint256 minAsk, uint256 maxAsk)
    {
        maxBid = (mid * (BPS - halfSpreadBps)) / BPS;
        minBid = (maxBid * (BPS - widthBps)) / BPS;
        minAsk = (mid * (BPS + halfSpreadBps)) / BPS;
        maxAsk = (minAsk * (BPS + widthBps)) / BPS;
    }

    function packLadder(uint16 pairId, uint256 mid, uint256 halfSpreadBps, uint256 widthBps)
        internal
        pure
        returns (uint256)
    {
        (uint256 minBid, uint256 maxBid, uint256 minAsk, uint256 maxAsk) = ladder(mid, halfSpreadBps, widthBps);
        return minBid | (maxBid << 56) | (minAsk << 112) | (maxAsk << 168) | (uint256(pairId) << 224);
    }

    // =====================================================================
    // Reading the pool
    // =====================================================================

    /// @notice Effective capacity and usage for one side, applying the generation rule.
    /// @dev `usedGen != capGen` means the used counters belong to a previous epoch and read as
    ///      zero. Reproduced here rather than collapsed by `snapshot`, exactly as `IPropPool`
    ///      documents.
    /// @notice The CURVE's capacity and usage — the numbers the ladder is priced against.
    ///
    /// @dev Not the fillable bound. Once a pair has a staleness ramp the two diverge, and
    ///      substituting one for the other is a real error in both directions: feed the decayed
    ///      number to `PropCurve` and every size reprices, because the ladder's slope is defined
    ///      against the nominal epoch; size a trade against the nominal number and it reverts.
    ///      `IPropPool.PairSnapshot`'s docs state the rule as: age changes how much you can fill,
    ///      never the price of what you can. Use `fillable` below for the bound.
    function room(IPropPool.PairSnapshot memory s, bool isBid) internal pure returns (uint256 capacity, uint256 used) {
        capacity = isBid ? uint256(s.bidCapacity) : uint256(s.askCapacity);
        if (s.usedGen == s.capGen) used = isBid ? uint256(s.bidUsed) : uint256(s.askUsed);
        if (used > capacity) used = capacity;
    }

    /// @notice How much BASE this side will actually accept right now, after the staleness ramp.
    ///
    /// @dev A live call rather than a field, because the ramp is a function of `block.timestamp`
    ///      and `PairSnapshot` is deliberately a static tuple that off-chain consumers mirror
    ///      positionally. This is the number the pool's own capacity guard checks.
    ///
    ///      Reading it is not optional for anyone sizing a trade. Skipping it is how this file
    ///      first "measured" the staleness ramp as a perfect defence: the informed solver bisected
    ///      against the nominal epoch, every candidate size reverted, and the run reported zero
    ///      informed fills at every withdrawal latency including two minutes. The pool was not
    ///      defended, the model was blind.
    ///      Returns the ROOM — what is left after usage — as one value rather than the pair, so a
    ///      caller does not have to spend two stack slots on it. Both harnesses sit at the legacy
    ///      code generator's limit and `via_ir` is off in this repo's default profile.
    function fillableRoom(IPropPool pool, uint16 pairId, IPropPool.PairSnapshot memory s, bool isBid)
        internal
        view
        returns (uint256)
    {
        (uint96 bidCap, uint96 askCap,) = pool.effectiveCapacity(pairId);
        uint256 available = isBid ? uint256(bidCap) : uint256(askCap);
        uint256 used;
        if (s.usedGen == s.capGen) used = isBid ? uint256(s.bidUsed) : uint256(s.askUsed);
        return used >= available ? 0 : available - used;
    }

    /// @notice The price a taker gets right now for an infinitesimal size, per side.
    /// @dev `PropCurve`'s own helpers, which are what `dubu-updater`'s policy module triggers on.
    function executableTop(IPropPool.PairSnapshot memory s, bool isBid) internal pure returns (uint256) {
        (uint256 capacity, uint256 used) = room(s, isBid);
        if (capacity == 0 || used >= capacity) return isBid ? 0 : type(uint256).max;
        return isBid
            ? PropCurve.executableTopBid(s.minBid, s.maxBid, capacity, used)
            : PropCurve.executableTopAsk(s.minAsk, s.maxAsk, capacity, used);
    }

    /// @notice How wrong the quote is, in the taker's favour, on each side.
    ///
    /// @dev Positive `ask` means the pool is selling base below the reference — buying from it
    ///      is profitable. Positive `bid` means the pool is buying base above the reference —
    ///      selling to it is profitable. At a coherent ladder on a correct reference both are
    ///      exactly `-halfSpreadBps`, which is the point: the half-spread is the size of the
    ///      error the pool can absorb before it is writing options.
    function edges(IPropPool.PairSnapshot memory s, uint256 ref) internal pure returns (int256 bid, int256 ask) {
        if (ref == 0) return (type(int256).min, type(int256).min);

        uint256 topBid = executableTop(s, true);
        uint256 topAsk = executableTop(s, false);

        // forge-lint: disable-next-line(unsafe-typecast)
        bid = topBid == 0 ? type(int256).min : (int256(topBid) - int256(ref)) * int256(BPS_E2) / int256(ref);
        ask = topAsk == type(uint256).max
            ? type(int256).min
            // forge-lint: disable-next-line(unsafe-typecast)
            : (int256(ref) - int256(topAsk)) * int256(BPS_E2) / int256(ref);
    }

    // =====================================================================
    // Informed sizing — take the depth that is profitable, and no more
    // =====================================================================

    /// @notice One side's bisection problem, gathered into memory.
    ///
    /// @dev A struct rather than eight arguments because the two solvers below sit right on the
    ///      legacy code generator's 16-slot stack limit — `PropCurve`'s entry points take six
    ///      arguments of their own — and `via_ir` is off in this repo's default profile
    ///      (foundry.toml). Same reason `PropCurve.amountOutAsk` scopes its own locals.
    struct Solve {
        /// @dev Upper bound on the base quantity, already clamped to the epoch's room.
        uint256 hi;
        /// @dev `minAsk` on the ask side, `minBid` on the bid side.
        uint256 pLow;
        uint256 pHigh;
        uint256 capacity;
        uint256 used;
        /// @dev The reference, already haircut by the taker's own cost.
        uint256 refNet;
        uint256 scale;
        /// @dev The pool's inventory of the token it would have to pay out.
        uint256 reserveOut;
        uint8 exp;
    }

    /// @notice Largest base amount whose *average* ask price still beats the reference by at
    ///         least the taker's cost. Bisected on the integer curve, so it is the exact answer
    ///         to the question the taker actually asks.
    ///
    /// @dev The predicate `cost(q) * scale <= q * refNet` is monotone: `amountInAsk` is a convex
    ///      increasing quadratic in `q` and the right-hand side is linear, so the profitable set
    ///      is `[0, q*]` and a greatest-true bisection is exact.
    ///
    ///      `capturePct` scales the answer. 100 is a **competitive race**: several searchers see
    ///      the same error and whoever leaves depth on the table loses it to the next one, so
    ///      the whole profitable interval clears. 50 is a **monopolist**: on a linear ladder the
    ///      profit-maximising size is exactly half the break-even size, because profit is
    ///      `q * (ref - avg(q))` and `avg` is linear in `q`. 100 is the default because a public
    ///      chain with a public mempool is a race, and because it is the assumption that is
    ///      worse for the pool — a risk tool should not round in its own favour.
    function informedAskSize(
        IPropPool.PairSnapshot memory s,
        uint256 fillableRoom,
        uint256 refNet,
        uint256 scale,
        uint256 baseReserve,
        uint256 capturePct
    ) internal pure returns (uint256) {
        Solve memory v;
        (v.capacity, v.used) = room(s, false);
        if (v.capacity == 0 || v.used >= v.capacity || s.maxAsk == 0) return 0;

        v.hi = v.capacity - v.used;
        if (v.hi > fillableRoom) v.hi = fillableRoom; // the ramp, not the epoch
        if (v.hi > baseReserve) v.hi = baseReserve;
        if (v.hi == 0) return 0;

        v.pLow = s.minAsk;
        v.pHigh = s.maxAsk;
        v.refNet = refNet;
        v.scale = scale;
        v.reserveOut = baseReserve;
        v.exp = s.priceScaleExp;

        return (_bisectAsk(v) * capturePct) / 100;
    }

    function _bisectAsk(Solve memory v) private pure returns (uint256) {
        uint256 lo;
        uint256 hi = v.hi;
        while (lo < hi) {
            uint256 mid = lo + (hi - lo + 1) / 2;
            uint256 cost = PropCurve.amountInAsk(mid, v.pLow, v.pHigh, v.capacity, v.used, v.exp);
            if (cost * v.scale <= mid * v.refNet) lo = mid;
            else hi = mid - 1;
        }
        return lo;
    }

    /// @notice Largest base amount whose average bid price still beats the reference by at least
    ///         the taker's cost. Mirror of [`informedAskSize`]; `amountOutBid` is concave
    ///         increasing and the reserve constraint is also a prefix, so the conjunction is
    ///         still monotone and the bisection is still exact.
    function informedBidSize(
        IPropPool.PairSnapshot memory s,
        uint256 fillableRoom,
        uint256 refNet,
        uint256 scale,
        uint256 quoteReserve,
        uint256 capturePct
    ) internal pure returns (uint256) {
        Solve memory v;
        (v.capacity, v.used) = room(s, true);
        if (v.capacity == 0 || v.used >= v.capacity) return 0;

        v.hi = v.capacity - v.used;
        if (v.hi > fillableRoom) v.hi = fillableRoom; // the ramp, not the epoch
        v.pLow = s.minBid;
        v.pHigh = s.maxBid;
        v.refNet = refNet;
        v.scale = scale;
        v.reserveOut = quoteReserve;
        v.exp = s.priceScaleExp;

        return (_bisectBid(v) * capturePct) / 100;
    }

    function _bisectBid(Solve memory v) private pure returns (uint256) {
        uint256 lo;
        uint256 hi = v.hi;
        while (lo < hi) {
            uint256 mid = lo + (hi - lo + 1) / 2;
            uint256 got = PropCurve.amountOutBid(mid, v.pLow, v.pHigh, v.capacity, v.used, v.exp);
            if (got * v.scale >= mid * v.refNet && got <= v.reserveOut) lo = mid;
            else hi = mid - 1;
        }
        return lo;
    }

    /// @notice `ref` haircut by the taker's own cost, in the direction that makes the taker
    ///         demand a real edge rather than a break-even one.
    function refNetForAsk(uint256 ref, uint256 costE2) internal pure returns (uint256) {
        return (ref * (BPS_E2 - costE2)) / BPS_E2;
    }

    function refNetForBid(uint256 ref, uint256 costE2) internal pure returns (uint256) {
        return (ref * (BPS_E2 + costE2)) / BPS_E2;
    }

    // =====================================================================
    // Valuation — the same integer function risk.rs marks the book with
    // =====================================================================

    /// @notice `floor(baseBalance * fair / 10**exp)`, computed through the chain's own quote path.
    ///
    /// @dev `amountOutBid` against a flat ladder (`minBid == maxBid == fair`) over an
    ///      unconstrained epoch. Identical to `dubu-updater`'s `risk::value`, deliberately: a
    ///      mark that differed from a fill by whatever a hand-written division rounded
    ///      differently would make the simulator's P&L and the bot's killswitch disagree about
    ///      the same book.
    function value(uint256 baseBalance, uint256 fair, uint8 priceScaleExp) internal pure returns (uint256) {
        if (baseBalance == 0 || fair == 0) return 0;
        uint256 cap = type(uint96).max;
        uint256 q = baseBalance > cap ? cap : baseBalance;
        return PropCurve.amountOutBid(q, fair, fair, cap, 0, priceScaleExp);
    }

    // =====================================================================
    // Markout
    // =====================================================================

    /// @notice The pool's mark-to-market P&L on one fill, valued at `refAt`, in quote units.
    ///
    /// @param isBid  true when the pool bought base and paid quote.
    /// @param baseQty the base leg.
    /// @param quoteQty the quote leg.
    /// @param markAt the reference price to value the base leg at.
    function fillPnl(bool isBid, uint256 baseQty, uint256 quoteQty, uint256 markAt, uint8 priceScaleExp)
        internal
        pure
        returns (int256)
    {
        // forge-lint: disable-next-line(unsafe-typecast)
        int256 marked = int256(value(baseQty, markAt, priceScaleExp));
        // forge-lint: disable-next-line(unsafe-typecast)
        int256 paid = int256(quoteQty);
        return isBid ? marked - paid : paid - marked;
    }

    /// @notice One fill, as the accountant sees it.
    /// @dev A struct rather than six arguments so that the call sites in the harnesses — which
    ///      already hold a `Result`, a `Params`, the path and a snapshot — stay inside the
    ///      legacy code generator's 16-slot stack limit.
    struct Fill {
        uint256 pop;
        /// @dev MODEL tick, not second. The markout horizons below are converted to ticks so that
        ///      "+60s" is sixty wall-clock seconds at every resolution — otherwise two runs at
        ///      different `tickMs` would be reporting different columns under the same heading.
        uint256 tTick;
        /// @dev True when the POOL bought base. The taker's side is the opposite.
        bool isBid;
        uint256 baseQty;
        uint256 quoteQty;
    }

    /// @notice Fold one fill into a population accumulator and a size-bucket accumulator.
    function record(Result memory res, Fill memory f, uint256[] memory ref, Params memory p) internal pure {
        uint8 exp = p.priceScaleExp;
        uint256 notional = value(f.baseQty, ref[f.tTick], exp);

        int256 s0 = fillPnl(f.isBid, f.baseQty, f.quoteQty, ref[f.tTick], exp);
        int256 s1 = fillPnl(f.isBid, f.baseQty, f.quoteQty, refAt(ref, f.tTick + markoutTicks(MARKOUT_SHORT, p)), exp);
        int256 s10 = fillPnl(f.isBid, f.baseQty, f.quoteQty, refAt(ref, f.tTick + markoutTicks(MARKOUT_MID, p)), exp);
        int256 s60 = fillPnl(f.isBid, f.baseQty, f.quoteQty, refAt(ref, f.tTick + markoutTicks(MARKOUT_LONG, p)), exp);

        _fold(res.byPop[f.pop], notional, s0, s1, s10, s60);
        _fold(res.byBucket[bucketOf(notional / (10 ** uint256(_quoteDecimalsOf(res))))], notional, s0, s1, s10, s60);

        if (s60 < res.worstFillMo60) {
            res.worstFillMo60 = s60;
            res.worstFillNotional = notional;
            res.worstFillAtTick = f.tTick;
        }
    }

    /// @dev The bucketing needs the quote token's decimals to turn raw notional into whole
    ///      tokens, and `Result` has nowhere to put it without widening a struct that is already
    ///      at the stack limit in its consumers. Six is the decimals of every quote token in this
    ///      repo (mUSDC), asserted in `Simulation.t.sol` and in `Simulate.s.sol`'s preflight.
    function _quoteDecimalsOf(Result memory) private pure returns (uint8) {
        return 6;
    }

    function _fold(Accum memory a, uint256 notional, int256 s0, int256 s1, int256 s10, int256 s60) private pure {
        a.fills += 1;
        a.notional += notional;
        a.spread += s0;
        a.mo1 += s1;
        a.mo10 += s10;
        a.mo60 += s60;
    }

    /// @notice `ref[k]` at MODEL tick `k`, clamped to the end of the path.
    function refAt(uint256[] memory ref, uint256 k) internal pure returns (uint256) {
        return k < ref.length ? ref[k] : ref[ref.length - 1];
    }

    // =====================================================================
    // Reporting
    // =====================================================================

    /// @notice `pnl / notional` in hundredths of a bp, signed. Zero notional reports zero.
    function bpsOf(int256 pnl, uint256 notional) internal pure returns (int256) {
        if (notional == 0) return 0;
        // forge-lint: disable-next-line(unsafe-typecast)
        return (pnl * int256(BPS_E2)) / int256(notional);
    }

    function printParams(Params memory p, string memory pathName) internal pure {
        rule();
        console2.log(string.concat(" FLOW MODEL   path: ", pathName));
        rule();
        _printPathParams(p);
        _printMakerParams(p);
        _printFlowParams(p);
    }

    function _printPathParams(Params memory p) private pure {
        string memory head = string.concat("  horizon ", u2s(p.horizonSecs), "s   seed ", u2s(p.seed));
        // forge-lint: disable-next-line(unsafe-typecast)
        console2.log(
            string.concat(head, "   sigma ", sbps(int256(p.sigmaE2)), " bp/s   drift ", sbps(p.driftE2), " bp/s")
        );
        console2.log(
            string.concat(
                "  CLOCKS: model tick ",
                u2s(p.tickMs),
                "ms (",
                u2s(totalTicks(p)),
                " ticks)   block time 1s (",
                u2s(p.horizonSecs),
                " warps, ",
                u2s(ticksPerSec(p)),
                " ticks per second)"
            )
        );
        console2.log("          staleness and the capacity ramp read block.timestamp and stay quantised to 1s.");
        if (p.jumpAtSecs != 0) {
            console2.log(string.concat("  jump ", sbps(p.jumpE2), " bp at t=", u2s(p.jumpAtSecs), "s"));
        }
    }

    function _printMakerParams(Params memory p) private pure {
        string memory head = string.concat(
            "  maker: half-spread ",
            u2s(p.halfSpreadBps),
            p.s1E2 == 0 ? "bp (constant)" : string.concat("->", u2s(halfSpreadAt(p)), "bp at this sigma"),
            "  width ",
            u2s(p.widthBps),
            "bp"
        );
        console2.log(
            string.concat(
                head,
                "  capacity ",
                units(p.askCapacity, p.baseDecimals),
                " base/epoch  quote latency ",
                u2s(p.quoteLatencyMs),
                "ms (",
                u2s(msToTicks(p.quoteLatencyMs, p)),
                " ticks)"
            )
        );
        console2.log(
            string.concat(
                "         adverse trigger ",
                // forge-lint: disable-next-line(unsafe-typecast)
                sbps(int256(p.adverseDriftE2)),
                "bp   favourable trigger ",
                // forge-lint: disable-next-line(unsafe-typecast)
                sbps(int256(p.favourableDriftE2)),
                "bp"
            )
        );
    }

    function _printFlowParams(Params memory p) private pure {
        console2.log(
            string.concat(
                "  flow:  uninformed ", u2s(p.uninformedPerHour), "/hour, Pareto-1 sizes, mean ~$2,035 a fill"
            )
        );
        // forge-lint: disable-next-line(unsafe-typecast)
        string memory head = string.concat("         informed hazard: e50 ", sbps(int256(p.informedHalfMaxE2)), "bp");
        console2.log(
            string.concat(
                head,
                ", cost ",
                // forge-lint: disable-next-line(unsafe-typecast)
                sbps(int256(p.informedCostE2)),
                "bp, saturating at ",
                pct2(p.informedMaxPerSecE6 / 10_000),
                "/s"
            )
        );
        console2.log(
            string.concat(
                "         informed capture ",
                u2s(p.informedCapturePct),
                "% of the profitable depth;  fast taker wins ",
                u2s(p.fastHitPct),
                "% of races"
            )
        );
    }

    /// @notice The per-population table. This is the output the whole exercise is for.
    function printAttribution(Result memory res, uint8 quoteDecimals) internal pure {
        console2.log("");
        console2.log("  population    fills   notional($)    spread     MO +1s     MO +10s    MO +60s    PnL60($)");
        console2.log("  ------------------------------------------------------------------------------------------");

        Accum memory total;
        for (uint256 i; i < POP_COUNT; ++i) {
            _printAccumRow(popLabel(i), res.byPop[i], quoteDecimals);
            total.fills += res.byPop[i].fills;
            total.notional += res.byPop[i].notional;
            total.spread += res.byPop[i].spread;
            total.mo1 += res.byPop[i].mo1;
            total.mo10 += res.byPop[i].mo10;
            total.mo60 += res.byPop[i].mo60;
        }
        console2.log("  ------------------------------------------------------------------------------------------");
        _printAccumRow("ALL", total, quoteDecimals);
        console2.log("  All bp figures are the pool's P&L as a fraction of the fill's notional. Positive is the");
        console2.log("  pool winning. MO is markout: the fill re-marked at the reference h seconds later.");
    }

    function _printAccumRow(string memory label, Accum memory a, uint8 quoteDecimals) private pure {
        string memory left = string.concat(
            "  ",
            pad(label, 14),
            pad(u2s(a.fills), 8),
            pad(thousands(a.notional / (10 ** uint256(quoteDecimals))), 15),
            pad(string.concat(sbps(bpsOf(a.spread, a.notional)), "bp"), 11)
        );
        console2.log(
            string.concat(
                left,
                pad(string.concat(sbps(bpsOf(a.mo1, a.notional)), "bp"), 11),
                pad(string.concat(sbps(bpsOf(a.mo10, a.notional)), "bp"), 11),
                pad(string.concat(sbps(bpsOf(a.mo60, a.notional)), "bp"), 11),
                signedUnits(a.mo60, quoteDecimals)
            )
        );
    }

    /// @notice Markout by fill size. This is the "at what size did the spread stop covering it"
    ///         question, answered with the data rather than asserted.
    function printBuckets(Result memory res, uint8 quoteDecimals) internal pure {
        console2.log("");
        console2.log("  Markout at +60s by fill size");
        console2.log("  size          fills   notional($)    spread     MO +60s    PnL60($)");
        console2.log("  ---------------------------------------------------------------------");
        for (uint256 i; i < BUCKET_COUNT; ++i) {
            Accum memory a = res.byBucket[i];
            if (a.fills == 0) continue;
            console2.log(
                string.concat(
                    "  ",
                    pad(bucketLabel(i), 14),
                    pad(u2s(a.fills), 8),
                    pad(thousands(a.notional / (10 ** uint256(quoteDecimals))), 15),
                    pad(string.concat(sbps(bpsOf(a.spread, a.notional)), "bp"), 11),
                    pad(string.concat(sbps(bpsOf(a.mo60, a.notional)), "bp"), 11),
                    signedUnits(a.mo60, quoteDecimals)
                )
            );
        }
    }

    /// @notice The inventory path and the NAV split, in the same shape `risk.rs` reports it.
    function printNav(Result memory res, uint8 baseDecimals, uint8 quoteDecimals) internal pure {
        console2.log("");
        console2.log("  Inventory and NAV (decomposition identical to dubu-updater risk.rs)");
        string memory head = string.concat(
            "    base   start ", units(res.baseStart, baseDecimals), "  end ", units(res.baseEnd, baseDecimals)
        );
        console2.log(
            string.concat(
                head, "  range [", units(res.baseMin, baseDecimals), ", ", units(res.baseMax, baseDecimals), "]"
            )
        );
        console2.log(
            string.concat(
                "    quote  start ", units(res.quoteStart, quoteDecimals), "  end ", units(res.quoteEnd, quoteDecimals)
            )
        );
        console2.log(
            string.concat(
                "    NAV    start ", units(res.navStart, quoteDecimals), "  end ", units(res.navEnd, quoteDecimals)
            )
        );
        console2.log(
            string.concat(
                "    of which  revaluation ",
                signedUnits(res.revaluation, quoteDecimals),
                "   trade PnL ",
                signedUnits(res.tradePnl, quoteDecimals)
            )
        );
        console2.log(
            string.concat(
                "    clocks   ", u2s(res.ticksRun), " model ticks over ", u2s(res.blockSecsAdvanced), " block seconds"
            )
        );
        string memory pushes = string.concat(
            "    quote pushes ", u2s(res.quotePushes), "   capacity refreshes ", u2s(res.capacityRefreshes)
        );
        console2.log(
            string.concat(
                pushes,
                "   declined fills ",
                u2s(res.declinedFills),
                " ($",
                thousands(res.declinedNotional / (10 ** uint256(quoteDecimals))),
                " notional)"
            )
        );
        console2.log("    revaluation is the unhedged book marked to a moved market; trade PnL is what the fills did.");
    }

    /// @notice The verdict, in the terms the question was asked in.
    function printVerdict(Result memory res, uint8 quoteDecimals) internal pure {
        Accum memory total;
        for (uint256 i; i < POP_COUNT; ++i) {
            total.notional += res.byPop[i].notional;
            total.spread += res.byPop[i].spread;
            total.mo60 += res.byPop[i].mo60;
        }
        uint256 toxic = res.byPop[POP_INFORMED].notional + res.byPop[POP_FAST].notional;
        uint256 toxicPctE2 = total.notional == 0 ? 0 : (toxic * BPS_E2) / total.notional / 100;

        console2.log("");
        rule();
        console2.log(" DID THE SPREAD COVER ADVERSE SELECTION?");
        rule();
        console2.log(
            string.concat(
                "  spread booked at fill time      ",
                signedUnits(total.spread, quoteDecimals),
                "   (",
                sbps(bpsOf(total.spread, total.notional)),
                " bp)"
            )
        );
        console2.log(
            string.concat(
                "  same fills marked out at +60s   ",
                signedUnits(total.mo60, quoteDecimals),
                "   (",
                sbps(bpsOf(total.mo60, total.notional)),
                " bp)"
            )
        );
        console2.log(
            string.concat(
                "  given away to informed + fast   ",
                signedUnits(res.byPop[POP_INFORMED].mo60 + res.byPop[POP_FAST].mo60, quoteDecimals)
            )
        );
        console2.log(
            string.concat(
                "  earned from uninformed          ", signedUnits(res.byPop[POP_UNINFORMED].mo60, quoteDecimals)
            )
        );
        console2.log(string.concat("  toxic share of volume           ", pct2(toxicPctE2), "%"));
        console2.log("");
        if (total.mo60 > 0) {
            console2.log("  VERDICT: YES on this path. The uninformed spread more than paid for the pick-offs.");
        } else if (total.mo60 == 0) {
            console2.log("  VERDICT: exactly break-even, which on a run this size means nothing traded.");
        } else {
            console2.log("  VERDICT: NO on this path. The pool wrote more option value than it collected spread.");
        }
        rule();
    }

    // =====================================================================
    // Formatting — self-contained, so this library needs no cheatcodes
    // =====================================================================

    function rule() internal pure {
        console2.log("  ---------------------------------------------------------------------------------------");
    }

    function u2s(uint256 v) internal pure returns (string memory) {
        if (v == 0) return "0";
        uint256 n = v;
        uint256 digits;
        while (n != 0) {
            ++digits;
            n /= 10;
        }
        bytes memory b = new bytes(digits);
        while (v != 0) {
            b[--digits] = bytes1(uint8(48 + (v % 10)));
            v /= 10;
        }
        return string(b);
    }

    function i2s(int256 v) internal pure returns (string memory) {
        // forge-lint: disable-next-line(unsafe-typecast)
        return v < 0 ? string.concat("-", u2s(uint256(-v))) : u2s(uint256(v));
    }

    /// @dev A signed hundredths-of-a-bp quantity as `[-]NN.NN`.
    function sbps(int256 e2) internal pure returns (string memory) {
        // forge-lint: disable-next-line(unsafe-typecast)
        uint256 a = e2 < 0 ? uint256(-e2) : uint256(e2);
        uint256 frac = a % 100;
        return string.concat(e2 < 0 ? "-" : "", u2s(a / 100), ".", frac < 10 ? "0" : "", u2s(frac));
    }

    /// @dev A percentage carried in hundredths, as `NN.NN`.
    function pct2(uint256 e2) internal pure returns (string memory) {
        uint256 frac = e2 % 100;
        return string.concat(u2s(e2 / 100), ".", frac < 10 ? "0" : "", u2s(frac));
    }

    /// @dev A raw token amount as `whole.ffffff`. Mirrors `DubuScript._units`.
    function units(uint256 amount, uint8 decimals) internal pure returns (string memory) {
        uint256 denom = 10 ** uint256(decimals);
        uint256 frac = amount % denom;
        string memory f =
            decimals >= 6 ? u2s(frac / (10 ** (uint256(decimals) - 6))) : u2s(frac * (10 ** (6 - uint256(decimals))));
        while (bytes(f).length < 6) {
            f = string.concat("0", f);
        }
        return string.concat(u2s(amount / denom), ".", f);
    }

    function signedUnits(int256 amount, uint8 decimals) internal pure returns (string memory) {
        // forge-lint: disable-next-line(unsafe-typecast)
        return amount < 0 ? string.concat("-", units(uint256(-amount), decimals)) : units(uint256(amount), decimals);
    }

    function thousands(uint256 n) internal pure returns (string memory) {
        if (n >= 1_000_000) return string.concat(u2s(n / 1_000_000), ".", u2s((n % 1_000_000) / 100_000), "M");
        if (n >= 1_000) return string.concat(u2s(n / 1_000), ".", u2s((n % 1_000) / 100), "k");
        return u2s(n);
    }

    function pad(string memory s, uint256 width) internal pure returns (string memory) {
        while (bytes(s).length < width) {
            s = string.concat(s, " ");
        }
        return s;
    }
}
