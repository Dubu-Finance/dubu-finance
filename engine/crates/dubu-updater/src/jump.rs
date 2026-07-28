//! Jump detection and withdrawal: the defence that does not try to price through a jump.
//!
//! # Why this exists, in one line of arithmetic
//!
//! A linear ladder absorbs a reference error up to
//!
//! ```text
//! absorption = half_spread + width / 2
//! ```
//!
//! and nothing beyond it. Past that point the pool pays
//! `(reference error - absorption) x exposed depth` and there is no spread a market maker would
//! post that makes a 100 bp gap profitable: the simulator's own sweep needs a **30 bp** constant
//! half-spread before the sign flips, which is UniV2's fee and destroys the reason to exist.
//!
//! So the only winning move is not to be there. When the reference moves more than the ladder can
//! absorb in one observation, this module withdraws quotes entirely — `refreshCapacity(pair, 0, 0)`,
//! which makes `PropPool._outFor` return zero from every quote path — holds for a cool-off, and
//! resumes only once the reference has stopped moving.
//!
//! # What counts as a jump
//!
//! Both an absolute move and a move in units of `sigma`, combined as **one threshold with two
//! bounds**, because each arm fails in a way the other one catches:
//!
//! ```text
//! threshold = clamp( k * sigma(dt),  half_spread,  half_spread + width/2 )
//!                    \___________/   \__________/  \____________________/
//!                     the sigma arm   the floor     the absorption ceiling
//! ```
//!
//! **The floor is the pair's own half-spread.** A fixed basis-point threshold is wrong across
//! ETHUSDT and BTCUSDT — their measured 300 s sigmas are 10 bp and 3 bp — but the half-spread is
//! not a fixed number, it is per-pair configuration, and it is the exact point at which the posted
//! quote stops being on the right side of fair value. Below it the pool is still making money on
//! the fill; there is nothing to run away from. It is also comfortably above the measured feed
//! noise: cross-venue MAD on 2026-07-27 ran 0.1-1.0 bps with a largest single-venue deviation of
//! 1.6 bps, against a 5 bp floor on ETH and 8 bp on BTC.
//!
//! **The ceiling is the pair's own absorption limit.** This is the answer to the failure the
//! sigma arm has on its own: one 100 bp return entering a 60 s EWMA at `dt/tau = 1/60` raises
//! ETH's per-second variance from 0.33 to ~167 bps^2/s, i.e. `sigma(1s)` from 0.58 bp to 12.9 bp.
//! At `k = 6` the sigma arm would then ask for a **77 bp** move before it fired again — so the
//! second leg of a two-stage jump would sail straight through the detector, which is precisely
//! the case the defence exists for. Clamping the threshold at `half_spread + width/2` bounds that
//! numbness with a fact about the ladder rather than a fact about the market: above the absorption
//! limit the pool loses money whatever the volatility estimate says, so the detector must fire.
//!
//! **The sigma arm lives between them**, and its job is the opposite of the naive reading: in a
//! calm market the floor binds and sigma does nothing, and as volatility rises the sigma arm
//! *raises* the bar so the bot does not withdraw on every ordinary swing in a fast market. For
//! ETH it starts to bind once `sigma(300s)` passes ~14 bp, i.e. 1.4x the measured level, and it is
//! capped out by ~50 bp, i.e. 5x.
//!
//! The bounds are computed from the pair's **configured** `half_spread_bps`, never from the
//! volatility-scaled one in [`crate::spread`]. Widening the spread widens the real absorption, so
//! feeding the widened value back in here would be defensible — and it would make the detector
//! numb in exactly the regime where jumps cluster, which is the same failure the ceiling exists to
//! prevent. The configured value is the protection the pool *always* has; that is the right thing
//! to be sure of. (The configured `width_bps` is likewise an upper bound the inverse solver may
//! narrow, so the ceiling is very slightly generous; the floor is exact.)
//!
//! # A gap in the reference is a jump
//!
//! An observation separated from the previous one by more than `max_sample` is not a return, it is
//! a hole — the venues went away, or the process was blocked. During that hole the pool was
//! quoting a fixed ladder against a market nobody was watching, and `FeedNotLive` gates *pushes*
//! but does not withdraw *capacity*, so the epoch stayed armed the whole time. Treating the gap as
//! a jump is the conservative reading and closes that hole: the pool comes back withdrawn and
//! re-arms only after the reference has proved it is settled.
//!
//! # The cool-off, and the two-stage move
//!
//! Resuming into the second leg is the failure mode, so the cool-off is **not a timer that runs
//! down**. It ends when all of the following hold:
//!
//! 1. at least `cooloff` has passed **since the most recent trip** — retriggerable, so a second
//!    leg restarts it rather than shortening it;
//! 2. the trailing `cooloff` window of observations has a peak-to-trough **range** within the
//!    current threshold — the reference has actually settled, which catches the staircase that
//!    walks 100 bp in twenty 5 bp steps and never trips the single-observation test;
//! 3. the newest observation is fresh. A dead feed can never satisfy the settle test by having no
//!    observations to contradict it.
//!
//! (1) is a floor and (3) is a liveness check; (2) is the load-bearing one.
//!
//! # False positives, and what they cost
//!
//! A false positive is a quoting outage of one cool-off. The simulator prices both sides of that
//! trade and the ratio is not close:
//!
//! ```text
//! cost of one 30 s outage      ~$122k/h of uninformed flow at 4.70 bp = ~$57/h  ->  $0.48
//! value of one avoided pick-off                                                ->  $16,580
//! break-even false-positive rate                                               ->  ~34,000 : 1
//! ```
//!
//! So the design is biased hard toward firing, and the accepted rate is **a few trips per pair per
//! day** — of order 0.1% of quoting time. The floor is set at the half-spread rather than anywhere
//! higher for exactly this reason; for ETH at the measured `sigma(1s)` of 0.58 bp a 5 bp trip is an
//! 8.7-sigma observation, which is rare without being unreachable.
//!
//! What stops the bias from running away is not the economics, it is the mechanics: each
//! withdrawal is a transaction, an in-flight transaction blocks the pair, and a detector that
//! flapped would keep the pair permanently blocked. The retriggerable cool-off is what makes each
//! trip one transaction rather than a stream.
//!
//! # Per pair or across the book
//!
//! Across the book, by default ([`Scope::Book`]). A BTC jump is information about ETH: the two are
//! 80-90% correlated, and in the market-wide case — a macro print, a liquidation cascade, an
//! exchange outage — ETH's move is *coming* and its feed simply has not printed it yet. That
//! window is the searcher's. The asymmetry decides it: contagion's false positive is one cool-off
//! of foregone spread on the other pair, and its false negative is `(move - absorption) x $2M`.
//!
//! The **threshold** stays per-pair — each pair's own absorption limit and its own sigma. Only the
//! withdrawal is shared.

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

use dubu_core::math::mul_div_floor;

use crate::skew::Volatility;

/// 100%, in hundredths of a basis point — the unit every move and threshold here is carried in.
///
/// Hundredths rather than whole bps because the thresholds are single-digit bps and a whole-bp
/// measurement would round the interesting range to nothing, exactly as
/// [`crate::policy`] uses deci-bps for the same reason.
pub const BPS_E2: u128 = 1_000_000;

// ---------------------------------------------------------------------------
// Parameters
// ---------------------------------------------------------------------------

/// Whether one pair's jump withdraws one pair or the whole book.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// Withdraw only the pair that tripped.
    Pair,
    /// Withdraw every pair. The default; see the module docs.
    Book,
}

impl Scope {
    /// Short stable string for structured logs.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Pair => "pair",
            Self::Book => "book",
        }
    }
}

/// The knobs that are the same for every pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Params {
    /// The sigma multiplier `k`, scaled by 100 so the config can say `6.5`.
    pub sigma_k_e2: u32,
    /// Minimum time the withdrawal lasts, measured from the **most recent** trip.
    pub cooloff: Duration,
    /// Observations closer together than this are recorded but not tested, so a fast-lane scan
    /// landing 10 ms before a cycle scan does not divide a rounding error by a tiny interval.
    pub min_sample: Duration,
    /// A separation longer than this is a hole in the reference, not a return. See the module
    /// docs: it trips.
    pub max_sample: Duration,
}

/// One pair's economic bounds, in hundredths of a bp.
///
/// Both come from the pair's own configuration, which is what makes a single global `sigma_k`
/// correct across two instruments with very different volatilities and very different ladders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {
    /// The pair's configured half-spread: below this the posted quote is still on the right side
    /// of fair value and there is nothing to run from.
    pub floor_bps_e2: u32,
    /// The pair's absorption limit, `half_spread + width / 2`: above this the pool pays the excess
    /// however calm the volatility estimate claims the market is.
    pub ceiling_bps_e2: u32,
}

impl Bounds {
    /// Derive the bounds from one pair's configured spread and ladder width.
    #[must_use]
    pub const fn from_pair(half_spread_bps: u16, width_bps: u16) -> Self {
        Self::new(half_spread_bps, half_spread_bps, width_bps)
    }

    /// The same bounds with the floor stated separately from the spread.
    ///
    /// The floor used to be `half_spread_bps` and that was right while that field *was* the
    /// posted spread. It is not any more: `spread::compute` posts `s0 + s1 * sigma`, and when `s0`
    /// dropped from 5 bp to 1 the floor followed it down to a level below the ordinary tick noise
    /// of a four-venue combined reference. The detector then tripped on nothing — measured, four
    /// trips in seven minutes, each holding capacity at zero for the 30s cool-off, which is 29% of
    /// the run spent withdrawn with no jump anywhere.
    ///
    /// So the two numbers are separated, because they now answer different questions. `s0` is what
    /// the pool charges in a dead market. This is how large a move it refuses to absorb, and it
    /// belongs to the detector.
    #[must_use]
    pub const fn new(floor_bps: u16, half_spread_bps: u16, width_bps: u16) -> Self {
        // `* 100` converts bps to hundredths of a bp; `* 50` is `width / 2` in the same step.
        let floor = (floor_bps as u32) * 100;
        // A zero floor would let feed noise trip the detector on every tick. The config validator
        // already refuses `half_spread_bps = 0`, so this is a belt on top of braces.
        let floor = if floor == 0 { 100 } else { floor };
        // The ceiling stays the absorption limit -- `half_spread + width / 2` -- because that is a
        // property of the ladder, not of the detector. A move the ladder can absorb is not a jump
        // no matter how the floor is set, so the ceiling is computed from the posted spread and
        // width as it always was, and only the floor was pulled out.
        let ceiling = (half_spread_bps as u32) * 100 + (width_bps as u32) * 50;
        // A floor above the ceiling would make the clamp degenerate silently into always-floor.
        // Raising the ceiling to meet it keeps `threshold` monotone in sigma.
        let ceiling = if ceiling < floor { floor } else { ceiling };
        Self { floor_bps_e2: floor, ceiling_bps_e2: ceiling }
    }

    /// The threshold for one observation: the sigma arm, clamped between the two bounds.
    #[must_use]
    pub const fn threshold(self, sigma_k_e2: u32, sigma_bps_e2: u32) -> (u32, Bound) {
        let arm = (sigma_k_e2 as u64) * (sigma_bps_e2 as u64) / 100;
        if arm <= self.floor_bps_e2 as u64 {
            (self.floor_bps_e2, Bound::Floor)
        } else if arm >= self.ceiling_bps_e2 as u64 {
            (self.ceiling_bps_e2, Bound::Absorption)
        } else {
            (arm as u32, Bound::Sigma)
        }
    }
}

/// Which of the three terms set the threshold. Logged, because "the sigma arm has been pinned at
/// the absorption ceiling for ten minutes" is the shape of a market this bot should not be in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Bound {
    /// The pair's half-spread. The calm-market case, and the one an untested observation reports.
    #[default]
    Floor,
    /// `k * sigma`, strictly between the two bounds.
    Sigma,
    /// The pair's absorption limit. Volatility wanted a looser threshold and was refused.
    Absorption,
}

impl Bound {
    /// Short stable string for structured logs.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Floor => "floor",
            Self::Sigma => "sigma",
            Self::Absorption => "absorption",
        }
    }
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// Why the detector tripped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// One observation moved further than the threshold.
    Move,
    /// The reference disappeared for longer than `max_sample`. See the module docs.
    FeedGap,
    /// Another pair tripped and the scope is [`Scope::Book`].
    Contagion,
}

impl Reason {
    /// Short stable string for structured logs.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Move => "move",
            Self::FeedGap => "feed_gap",
            Self::Contagion => "contagion",
        }
    }
}

/// Whether this pair is quoting or stood aside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Normal.
    Quoting,
    /// Capacity is being held at zero.
    Withdrawn,
}

impl State {
    /// Short stable string for structured logs.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Quoting => "quoting",
            Self::Withdrawn => "withdrawn",
        }
    }

    /// Whether quotes are being held down.
    #[must_use]
    pub const fn withdrawn(self) -> bool {
        matches!(self, Self::Withdrawn)
    }
}

/// Everything one observation produced. All of it is logged on a trip or a state change, and the
/// numeric part is logged every cycle alongside the row, so that `sigma_k` can be back-solved from
/// history rather than guessed at again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Observation {
    /// How far the reference moved since the last tested observation, in hundredths of a bp.
    pub move_bps_e2: u32,
    /// The threshold it was tested against.
    pub threshold_bps_e2: u32,
    /// `sigma` scaled to this observation's own interval, in hundredths of a bp. Straight from
    /// [`Volatility`] — there is exactly one volatility estimator in this crate.
    pub sigma_bps_e2: u32,
    /// Which term set the threshold.
    pub bound: Bound,
    /// The observation interval.
    pub dt_ms: u64,
    /// Peak-to-trough range of the trailing cool-off window, in hundredths of a bp. This is the
    /// settle test's input.
    pub range_bps_e2: u32,
    /// Set when this observation *newly* moved the pair into [`State::Withdrawn`]. A second leg
    /// arriving while already withdrawn re-arms the cool-off and reports `Some` again, so the
    /// caller can re-assert the withdrawal; [`Observation::edge`] distinguishes them.
    pub tripped: Option<Reason>,
    /// `true` when this observation was the transition into [`State::Withdrawn`], as opposed to a
    /// further trip while already there. Only the edge needs a transaction.
    pub edge: bool,
    /// Set when this observation ended the cool-off.
    pub resumed: bool,
    /// State after this observation.
    pub state: State,
}

// ---------------------------------------------------------------------------
// The detector
// ---------------------------------------------------------------------------

/// One pair's jump detector and cool-off state machine.
///
/// It keeps its **own** price anchor, separate from [`Volatility`]'s, because the two are sampled
/// at different rates: the volatility estimator is fed once per quote cycle (~1 Hz) and this is fed
/// by the fast-lane scan (~5 Hz). It reads `sigma` from that one estimator, scaled to whatever
/// interval its own anchor happens to span — reusing the estimate, not duplicating it.
///
/// The sigma it reads is deliberately the estimate **as of before** the observation being tested.
/// Folding a jump's own return into the variance and then asking whether it is surprising relative
/// to that variance is a test that can never fire.
#[derive(Debug, Clone)]
pub struct Detector {
    params: Params,
    bounds: Bounds,
    anchor: Option<(u128, Instant)>,
    /// Trailing `cooloff` window of observations, for the settle test.
    window: VecDeque<(u128, Instant)>,
    state: State,
    tripped_at: Option<Instant>,
    last_reason: Option<Reason>,
    threshold_bps_e2: u32,
    trips: u64,
}

impl Detector {
    /// A fresh detector with no history, quoting.
    #[must_use]
    pub fn new(params: Params, bounds: Bounds) -> Self {
        Self {
            params,
            bounds,
            anchor: None,
            window: VecDeque::new(),
            state: State::Quoting,
            tripped_at: None,
            last_reason: None,
            threshold_bps_e2: bounds.floor_bps_e2,
            trips: 0,
        }
    }

    /// The bounds this detector was built with.
    #[must_use]
    pub const fn bounds(&self) -> Bounds {
        self.bounds
    }

    /// Current state.
    #[must_use]
    pub const fn state(&self) -> State {
        self.state
    }

    /// Whether quotes are being held down.
    #[must_use]
    pub const fn withdrawn(&self) -> bool {
        self.state.withdrawn()
    }

    /// Why the last trip happened.
    #[must_use]
    pub const fn last_reason(&self) -> Option<Reason> {
        self.last_reason
    }

    /// How many times this detector has tripped since start. In every log line, because a detector
    /// that has never fired and a detector that fires hourly need different responses and look
    /// identical otherwise.
    #[must_use]
    pub const fn trips(&self) -> u64 {
        self.trips
    }

    /// How much of the minimum cool-off is left, in milliseconds. Zero once the timer arm is
    /// satisfied — the settle test may still be holding the withdrawal open.
    #[must_use]
    pub fn cooloff_remaining_ms(&self, now: Instant) -> u64 {
        let Some(t) = self.tripped_at else { return 0 };
        let elapsed = now.saturating_duration_since(t);
        u64::try_from(self.params.cooloff.saturating_sub(elapsed).as_millis()).unwrap_or(u64::MAX)
    }

    /// Fold in one reference observation and decide.
    ///
    /// `vol` is the pair's volatility estimator, read but never written: this module adds no
    /// second estimator.
    pub fn observe(&mut self, price: u128, now: Instant, vol: &Volatility) -> Observation {
        // The window first, so the settle test below always sees the newest sample and so a
        // sub-`min_sample` observation still contributes to the range.
        if price > 0 {
            self.window.push_back((price, now));
        }
        while let Some(&(_, t)) = self.window.front() {
            if now.saturating_duration_since(t) > self.params.cooloff {
                self.window.pop_front();
            } else {
                break;
            }
        }

        let Some((prev, t0)) = self.anchor else {
            // First observation after a start. An anchor, not a return.
            self.anchor = Some((price, now));
            return self.report(Measured::default());
        };

        let dt = now.saturating_duration_since(t0);
        let dt_ms = u64::try_from(dt.as_millis()).unwrap_or(u64::MAX);
        // The estimate as of BEFORE this observation: the loop folds the tick into `vol` after the
        // scan, never before it. Testing a jump against a variance that already contains it is a
        // test that can never fire.
        let sigma_bps_e2 = vol.sigma_bps_e2_over_ms(dt_ms.max(1));
        let base = Measured {
            threshold_bps_e2: self.threshold_bps_e2,
            sigma_bps_e2,
            dt_ms,
            ..Measured::default()
        };

        if dt < self.params.min_sample {
            // Too close together to test. Keep the old anchor so the next observation spans a
            // sensible interval rather than starting over.
            let resumed = self.maybe_resume(now);
            return self.report(Measured { resumed, ..base });
        }

        if dt > self.params.max_sample || prev == 0 {
            // A hole in the reference. Not a return, and not safe to assume nothing happened.
            self.anchor = Some((price, now));
            let edge = self.trip(now, Reason::FeedGap);
            return self.report(Measured { tripped: Some(Reason::FeedGap), edge, ..base });
        }

        let (threshold, bound) = self.bounds.threshold(self.params.sigma_k_e2, sigma_bps_e2);
        self.threshold_bps_e2 = threshold;

        let moved = mul_div_floor(price.abs_diff(prev), BPS_E2, prev).unwrap_or(BPS_E2);
        let move_bps_e2 = u32::try_from(moved).unwrap_or(u32::MAX);
        self.anchor = Some((price, now));
        let base = Measured { move_bps_e2, threshold_bps_e2: threshold, bound, ..base };

        if move_bps_e2 >= threshold {
            let edge = self.trip(now, Reason::Move);
            self.report(Measured { tripped: Some(Reason::Move), edge, ..base })
        } else {
            let resumed = self.maybe_resume(now);
            self.report(Measured { resumed, ..base })
        }
    }

    /// Trip this detector because *another* pair did. Returns `true` if this was the edge into
    /// [`State::Withdrawn`], i.e. if a withdrawal transaction is owed.
    pub fn contagion(&mut self, now: Instant) -> bool {
        self.trip(now, Reason::Contagion)
    }

    /// Common trip path. Returns whether this was the transition into `Withdrawn`.
    fn trip(&mut self, now: Instant, reason: Reason) -> bool {
        let edge = !self.state.withdrawn();
        self.state = State::Withdrawn;
        // Retriggerable: the cool-off is measured from the MOST RECENT trip, so the second leg of
        // a two-stage move restarts it rather than being absorbed by the first leg's timer.
        self.tripped_at = Some(now);
        self.last_reason = Some(reason);
        if edge {
            self.trips = self.trips.saturating_add(1);
        }
        edge
    }

    /// End the cool-off if every condition holds. Returns whether it did.
    fn maybe_resume(&mut self, now: Instant) -> bool {
        if !self.state.withdrawn() || !self.settled(now) {
            return false;
        }
        self.state = State::Quoting;
        self.tripped_at = None;
        true
    }

    /// The three conditions, in the order they are cheapest to reject.
    fn settled(&self, now: Instant) -> bool {
        let Some(t) = self.tripped_at else { return true };
        // 1. The minimum, measured from the most recent trip.
        if now.saturating_duration_since(t) < self.params.cooloff {
            return false;
        }
        // 3. A dead feed must never satisfy the settle test by having nothing to contradict it.
        let Some(&(_, last_t)) = self.window.back() else { return false };
        if now.saturating_duration_since(last_t) > self.params.max_sample {
            return false;
        }
        if self.window.len() < 2 {
            return false;
        }
        // 2. The reference has actually settled. The window holds only the last `cooloff` of
        //    observations and the trip was at least that long ago, so it is entirely post-trip.
        self.range_bps_e2() <= self.threshold_bps_e2
    }

    /// Peak-to-trough range of the trailing window, in hundredths of a bp of the low.
    #[must_use]
    pub fn range_bps_e2(&self) -> u32 {
        let mut lo = u128::MAX;
        let mut hi = 0u128;
        for &(p, _) in &self.window {
            lo = lo.min(p);
            hi = hi.max(p);
        }
        if lo == u128::MAX || lo == 0 {
            return 0;
        }
        u32::try_from(mul_div_floor(hi - lo, BPS_E2, lo).unwrap_or(BPS_E2)).unwrap_or(u32::MAX)
    }

    /// Attach the parts of an [`Observation`] only the detector knows — the range and the
    /// resulting state — to the parts the measurement produced.
    fn report(&self, m: Measured) -> Observation {
        Observation {
            move_bps_e2: m.move_bps_e2,
            threshold_bps_e2: m.threshold_bps_e2,
            sigma_bps_e2: m.sigma_bps_e2,
            bound: m.bound,
            dt_ms: m.dt_ms,
            range_bps_e2: self.range_bps_e2(),
            tripped: m.tripped,
            edge: m.edge,
            resumed: m.resumed,
            state: self.state,
        }
    }
}

/// The measurement half of an [`Observation`], threaded through [`Detector::observe`]'s early
/// returns. A struct rather than eight positional arguments, because five of them are integers in
/// the same units and any two adjacent ones would transpose without a type error.
#[derive(Debug, Clone, Copy, Default)]
struct Measured {
    move_bps_e2: u32,
    threshold_bps_e2: u32,
    sigma_bps_e2: u32,
    bound: Bound,
    dt_ms: u64,
    tripped: Option<Reason>,
    edge: bool,
    resumed: bool,
}

// ---------------------------------------------------------------------------
// The book
// ---------------------------------------------------------------------------

/// Every pair's detector, plus the scope rule that decides whether one pair's jump is the whole
/// book's problem.
///
/// Keyed by pair id rather than positionally: a `Vec` indexed in lock-step with `cfg.pairs` is an
/// invariant nothing enforces, and getting it wrong tests one pair's move against another pair's
/// absorption limit.
#[derive(Debug, Clone)]
pub struct Book {
    detectors: BTreeMap<u16, Detector>,
    scope: Scope,
    enabled: bool,
}

impl Book {
    /// Build from `(pair_id, bounds)` pairs.
    #[must_use]
    pub fn new(pairs: &[(u16, Bounds)], params: Params, scope: Scope, enabled: bool) -> Self {
        Self {
            detectors: pairs.iter().map(|&(id, b)| (id, Detector::new(params, b))).collect(),
            scope,
            enabled,
        }
    }

    /// Whether detection is switched on at all. A disabled book never withdraws and never trips.
    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    /// The configured scope.
    #[must_use]
    pub const fn scope(&self) -> Scope {
        self.scope
    }

    /// One pair's detector.
    #[must_use]
    pub fn detector(&self, pair_id: u16) -> Option<&Detector> {
        self.detectors.get(&pair_id)
    }

    /// Whether this pair's quotes are being held down. `false` for an unknown pair and for a
    /// disabled book, so a caller can use it as a gate without a second check.
    #[must_use]
    pub fn withdrawn(&self, pair_id: u16) -> bool {
        self.enabled && self.detectors.get(&pair_id).is_some_and(Detector::withdrawn)
    }

    /// Every pair currently withdrawn, in id order.
    pub fn withdrawn_pairs(&self) -> impl Iterator<Item = u16> + '_ {
        let enabled = self.enabled;
        self.detectors.iter().filter(move |(_, d)| enabled && d.withdrawn()).map(|(id, _)| *id)
    }

    /// Fold one pair's reference observation in.
    ///
    /// Returns `None` for an unknown pair or a disabled book.
    pub fn observe(
        &mut self,
        pair_id: u16,
        price: u128,
        now: Instant,
        vol: &Volatility,
    ) -> Option<Observation> {
        if !self.enabled {
            return None;
        }
        let d = self.detectors.get_mut(&pair_id)?;
        Some(d.observe(price, now, vol))
    }

    /// Propagate a trip on `origin` to every other pair, if the scope says to.
    ///
    /// Returns the pairs that this *newly* moved into [`State::Withdrawn`] — the ones that owe a
    /// withdrawal transaction. A pair already withdrawn has its cool-off re-armed and is not
    /// returned, because re-sending `refreshCapacity(pair, 0, 0)` against a pair that is already at
    /// zero buys nothing and burns a nonce.
    pub fn contagion(&mut self, origin: u16, now: Instant) -> Vec<u16> {
        if !self.enabled || self.scope == Scope::Pair {
            return Vec::new();
        }
        let mut newly = Vec::new();
        for (&id, d) in &mut self.detectors {
            if id == origin {
                continue;
            }
            if d.contagion(now) {
                newly.push(id);
            }
        }
        newly
    }
}

#[cfg(test)]
mod tests {
    /// The regression that motivated splitting the floor out of the spread.
    ///
    /// Lowering `s0` from 5 bp to 1 used to drag the detector's floor down with it, and a 1 bp
    /// floor is under the tick noise of a four-venue combined reference: a 3.24 bp move over a
    /// 201ms sample -- an ordinary one, taken verbatim from a trip record in the run that exposed
    /// this -- read as a jump and cost a 30s cool-off. With the floor stated on its own it does
    /// not, and a real jump still does.
    #[test]
    fn ordinary_reference_noise_is_not_a_jump_at_a_one_bp_spread() {
        let coupled = Bounds::from_pair(1, 25);
        let (t, bound) = coupled.threshold(600, 3);
        assert_eq!((t, bound), (100, Bound::Floor), "the old floor was s0 itself");
        assert!(324 > t, "so 3.24bp of noise tripped it");

        let split = Bounds::new(5, 1, 25);
        let (t, bound) = split.threshold(600, 3);
        assert_eq!((t, bound), (500, Bound::Floor));
        assert!(324 < t, "the same sample is now inside the floor");

        // The ceiling is still the absorption limit, computed from the posted spread and width and
        // untouched by the floor.
        assert_eq!(split.ceiling_bps_e2, coupled.ceiling_bps_e2);
        // And a move that gaps past the ladder is still a jump.
        assert!(10_000 > split.threshold(600, 3).0);
    }

    /// A floor set above the absorption limit must not invert the clamp.
    #[test]
    fn a_floor_above_the_ceiling_does_not_invert_the_clamp() {
        let b = Bounds::new(90, 1, 25);
        assert!(b.ceiling_bps_e2 >= b.floor_bps_e2);
        let (lo, _) = b.threshold(600, 0);
        let (hi, _) = b.threshold(600, 100_000);
        assert!(hi >= lo, "threshold stayed monotone in sigma");
    }

    use super::*;
    use crate::skew::VolConfig;

    fn vol_cfg() -> VolConfig {
        VolConfig { tau_ms: 60_000, horizon_secs: 300, min_sample_ms: 100, max_sample_ms: 10_000 }
    }

    fn params() -> Params {
        Params {
            sigma_k_e2: 600, // k = 6
            cooloff: Duration::from_secs(30),
            min_sample: Duration::from_millis(100),
            max_sample: Duration::from_secs(10),
        }
    }

    /// pairId 1 as deployed: 5 bp half-spread, 25 bp width. Absorption 17.5 bp.
    fn eth() -> Bounds {
        Bounds::from_pair(5, 25)
    }

    /// pairId 2 as deployed: 8 bp half-spread, 40 bp width. Absorption 28 bp.
    fn btc() -> Bounds {
        Bounds::from_pair(8, 40)
    }

    const BASE: u128 = 400_000_000_000; // ETHUSDT at FEED_SCALE = 8.

    /// A scripted path driver: feeds the SAME observations to the one volatility estimator and to
    /// the detector, in the order the live loop does it (read sigma, then update sigma), so the
    /// test exercises the real wiring rather than a mock of it.
    struct Path {
        vol: Volatility,
        det: Detector,
        t: Instant,
    }

    impl Path {
        fn new(bounds: Bounds) -> Self {
            Self { vol: Volatility::new(vol_cfg()), det: Detector::new(params(), bounds), t: Instant::now() }
        }

        /// One second passes and the reference is `price`.
        fn step(&mut self, price: u128) -> Observation {
            self.t += Duration::from_millis(1_000);
            let obs = self.det.observe(price, self.t, &self.vol);
            self.vol.observe(price, self.t);
            obs
        }

        /// `secs` seconds pass with the reference pinned.
        fn quiet(&mut self, price: u128, secs: u64) -> Observation {
            let mut last = self.step(price);
            for _ in 1..secs {
                last = self.step(price);
            }
            last
        }
    }

    // -----------------------------------------------------------------------
    // The threshold itself
    // -----------------------------------------------------------------------

    #[test]
    fn the_bounds_are_the_pairs_own_spread_and_its_absorption_limit() {
        // The whole reason a single global `sigma_k` works across two instruments: neither bound
        // is a number picked here, both come from the pair's configuration.
        assert_eq!(eth(), Bounds { floor_bps_e2: 500, ceiling_bps_e2: 1_750 }); // 5 bp .. 17.5 bp
        assert_eq!(btc(), Bounds { floor_bps_e2: 800, ceiling_bps_e2: 2_800 }); // 8 bp .. 28 bp
    }

    #[test]
    fn the_sigma_arm_is_clamped_at_both_ends() {
        let b = eth();
        // Calm: k * sigma is under the half-spread, so the floor binds and sigma does nothing.
        // ETH's measured sigma(1s) is 0.577 bp; 6 x that is 3.46 bp, under the 5 bp floor.
        assert_eq!(b.threshold(600, 57), (500, Bound::Floor));
        // Fast: strictly between. sigma(1s) = 1.5 bp -> 9 bp.
        assert_eq!(b.threshold(600, 150), (900, Bound::Sigma));
        // THE CASE THE CEILING EXISTS FOR. One 100 bp return raises ETH's sigma(1s) to ~12.9 bp;
        // 6 x that is 77 bp, so the sigma arm alone would let a 50 bp second leg straight through.
        assert_eq!(b.threshold(600, 1_290), (1_750, Bound::Absorption));
    }

    // -----------------------------------------------------------------------
    // Ordinary diffusion must NOT trip it
    // -----------------------------------------------------------------------

    #[test]
    fn ordinary_diffusion_never_trips_the_detector() {
        // The simulator's diffusion path: 1 bp per second, which produced ZERO informed fills and
        // is exactly the regime the pool is built to quote into. A detector that fires here is a
        // quoting outage generator, so this is the test that keeps `sigma_k` honest.
        let mut p = Path::new(eth());
        let bp = BASE / 10_000;
        // A deterministic 1 bp/s sawtooth, 600 samples, plus a slow drift so it is not a pure
        // oscillation the EWMA could special-case.
        for i in 0..600u128 {
            let price = if i % 2 == 0 { BASE + i / 4 * bp } else { BASE + i / 4 * bp + bp };
            let obs = p.step(price);
            assert!(obs.tripped.is_none(), "1 bp/s diffusion tripped at i={i}: {obs:?}");
            assert_eq!(obs.state, State::Quoting);
        }
        assert_eq!(p.det.trips(), 0);
        // ... and the estimator really did see the volatility, so this is not a test that passes
        // because sigma stayed at zero.
        assert!(p.vol.sigma_millibps() > 0);
    }

    #[test]
    fn a_move_just_under_the_floor_holds_and_one_at_it_fires() {
        // The boundary is a boundary, not a blanket. ETH's floor is 5 bp.
        let mut p = Path::new(eth());
        p.quiet(BASE, 5);
        let just_under = BASE + BASE * 49 / 100_000; // 4.9 bp
        let obs = p.step(just_under);
        assert_eq!(obs.move_bps_e2, 490);
        assert_eq!(obs.threshold_bps_e2, 500);
        assert!(obs.tripped.is_none());

        let mut p = Path::new(eth());
        p.quiet(BASE, 5);
        let at = BASE + BASE * 5 / 10_000; // 5.0 bp
        let obs = p.step(at);
        assert_eq!(obs.move_bps_e2, 500);
        assert_eq!(obs.tripped, Some(Reason::Move));
        assert!(obs.edge);
        assert_eq!(obs.state, State::Withdrawn);
    }

    // -----------------------------------------------------------------------
    // The jump
    // -----------------------------------------------------------------------

    #[test]
    fn a_one_hundred_bp_jump_withdraws_immediately_and_holds_for_the_cooloff() {
        let mut p = Path::new(eth());
        p.quiet(BASE, 60);
        assert!(!p.det.withdrawn());

        // The simulator's jump: +100 bp in one observation.
        let jumped = BASE + BASE / 100;
        let obs = p.step(jumped);
        assert_eq!(obs.tripped, Some(Reason::Move));
        assert!(obs.edge, "the first trip owes a withdrawal transaction");
        assert_eq!(obs.move_bps_e2, 10_000);
        assert_eq!(obs.bound, Bound::Floor, "in a calm market the floor is what fires");
        assert_eq!(obs.state, State::Withdrawn);
        assert_eq!(p.det.trips(), 1);

        // It stays down for the whole cool-off even with the reference pinned at the new level.
        for i in 1..30u64 {
            let obs = p.step(jumped);
            assert!(obs.state.withdrawn(), "resumed after only {i}s");
            assert!(!obs.resumed);
        }
        // ... and comes back once the trailing window is entirely post-jump and quiet.
        let obs = p.step(jumped);
        assert!(obs.resumed, "must resume after the cool-off on a settled reference");
        assert_eq!(obs.state, State::Quoting);
        assert_eq!(p.det.trips(), 1, "resuming is not a second trip");
    }

    #[test]
    fn the_absorption_ceiling_is_what_catches_the_second_leg() {
        // THE TEST THIS MODULE EXISTS FOR. Leg one raises sigma so far that a pure sigma-multiple
        // would ask for ~77 bp before firing again. Leg two is 50 bp. Without the absorption
        // ceiling the detector would be numb exactly when it matters.
        let mut p = Path::new(eth());
        p.quiet(BASE, 60);

        let leg1 = BASE + BASE / 100; // +100 bp
        let o1 = p.step(leg1);
        assert_eq!(o1.tripped, Some(Reason::Move));
        assert!(o1.edge);

        // Sigma is now enormous, and the raw sigma arm is far above the absorption limit.
        let sigma_now = p.vol.sigma_bps_e2_over_ms(1_000);
        assert!(sigma_now > 1_000, "one 100 bp return must move sigma(1s) past 10 bp, got {sigma_now}");
        assert_eq!(eth().threshold(600, sigma_now).1, Bound::Absorption);

        // Ten quiet seconds — not enough to resume, and sigma decays only slowly.
        for _ in 0..10 {
            p.step(leg1);
        }

        // Leg two: +50 bp. Below the numbed sigma arm, above the absorption ceiling.
        let leg2 = leg1 + leg1 / 200;
        let o2 = p.step(leg2);
        assert_eq!(o2.move_bps_e2, 5_000);
        assert_eq!(o2.bound, Bound::Absorption, "the ceiling, not sigma, is what set the threshold");
        assert!(o2.threshold_bps_e2 <= 1_750);
        assert_eq!(o2.tripped, Some(Reason::Move), "the second leg MUST trip");
        assert!(!o2.edge, "already withdrawn, so no second transaction is owed");
    }

    #[test]
    fn the_cooloff_restarts_from_the_second_leg_not_the_first() {
        // Resuming into the second leg is the failure mode, so the timer is retriggerable.
        let mut p = Path::new(eth());
        p.quiet(BASE, 60);
        let leg1 = BASE + BASE / 100;
        p.step(leg1);

        // 20s in, still down; then leg two.
        for _ in 0..20 {
            p.step(leg1);
        }
        let leg2 = leg1 + leg1 / 200;
        assert_eq!(p.step(leg2).tripped, Some(Reason::Move));

        // The first leg's cool-off would have expired 10s from here. It must not.
        for i in 0..29u64 {
            let obs = p.step(leg2);
            assert!(obs.state.withdrawn(), "resumed {i}s after the SECOND leg, cool-off is 30s");
        }
        assert!(p.step(leg2).resumed, "and it does come back once the second leg has settled");
    }

    #[test]
    fn a_staircase_that_never_trips_a_single_observation_still_holds_the_cooloff_open() {
        // The other way a two-stage move arrives: 100 bp delivered in 4 bp steps, none of which
        // clears the 5 bp floor on its own. The single-observation test is blind to it; the
        // settle test's peak-to-trough range is not, so a cool-off already open stays open.
        let mut p = Path::new(eth());
        p.quiet(BASE, 60);
        let jumped = BASE + BASE / 100;
        assert!(p.step(jumped).edge);

        // Now walk 4 bp per second for the whole cool-off. No step trips.
        let mut price = jumped;
        for i in 0..40u64 {
            price += price * 4 / 10_000;
            let obs = p.step(price);
            assert!(obs.tripped.is_none(), "a 4 bp step must not trip the move test (i={i})");
            assert!(obs.state.withdrawn(), "but the range test must hold the withdrawal open (i={i})");
            assert!(obs.range_bps_e2 > obs.threshold_bps_e2);
        }
        // It resumes only once the walking stops.
        let settled = p.quiet(price, 31);
        assert_eq!(settled.state, State::Quoting);
    }

    // -----------------------------------------------------------------------
    // The other trips
    // -----------------------------------------------------------------------

    #[test]
    fn a_hole_in_the_reference_is_treated_as_a_jump() {
        // `FeedNotLive` gates pushes but does not withdraw capacity, so during an outage the pool
        // sits with a full epoch behind a fixed ladder. Coming back armed is the hole this closes.
        let mut p = Path::new(eth());
        p.quiet(BASE, 10);
        p.t += Duration::from_secs(120);
        let obs = p.det.observe(BASE, p.t, &p.vol);
        assert_eq!(obs.tripped, Some(Reason::FeedGap));
        assert!(obs.edge);
        assert!(obs.state.withdrawn(), "the price is unchanged, and that is not the point");
    }

    #[test]
    fn samples_closer_than_min_sample_are_recorded_but_not_tested() {
        // The fast-lane scan and the cycle scan can land milliseconds apart. Dividing a rounding
        // error by 10 ms would manufacture a jump out of nothing.
        let mut p = Path::new(eth());
        p.quiet(BASE, 5);
        let t = p.t + Duration::from_millis(10);
        let obs = p.det.observe(BASE * 2, t, &p.vol);
        assert!(obs.tripped.is_none(), "a 10 ms separation is not a return");
        // The anchor is still the pre-gap price, so the next real observation measures the whole
        // move rather than losing it.
        let obs = p.det.observe(BASE * 2, t + Duration::from_millis(1_000), &p.vol);
        assert_eq!(obs.tripped, Some(Reason::Move));
    }

    #[test]
    fn a_dead_feed_can_never_satisfy_the_settle_test() {
        // Silence is not settlement. Without this the cool-off would expire on a feed that stopped
        // delivering and the pool would re-arm blind.
        let mut p = Path::new(eth());
        p.quiet(BASE, 10);
        let jumped = BASE + BASE / 100;
        p.step(jumped);
        // Time passes with no observations at all, well past the cool-off.
        let much_later = p.t + Duration::from_secs(600);
        assert!(!p.det.settled(much_later));
        assert!(p.det.withdrawn());
    }

    // -----------------------------------------------------------------------
    // Scope
    // -----------------------------------------------------------------------

    #[test]
    fn a_btc_jump_withdraws_eth_when_the_scope_is_the_book() {
        let mut book = Book::new(&[(1, eth()), (2, btc())], params(), Scope::Book, true);
        let now = Instant::now();
        let vol = Volatility::new(vol_cfg());

        // pairId 2 jumps; pairId 1 has seen nothing at all.
        book.observe(2, BASE, now, &vol);
        let o = book.observe(2, BASE + BASE / 100, now + Duration::from_secs(1), &vol).unwrap();
        assert_eq!(o.tripped, Some(Reason::Move));

        let newly = book.contagion(2, now + Duration::from_secs(1));
        assert_eq!(newly, vec![1], "the other pair owes a withdrawal");
        assert!(book.withdrawn(1) && book.withdrawn(2));
        assert_eq!(book.detector(1).and_then(Detector::last_reason), Some(Reason::Contagion));

        // Re-propagating does not owe a second transaction for a pair already at zero capacity.
        assert!(book.contagion(2, now + Duration::from_secs(2)).is_empty());
    }

    #[test]
    fn pair_scope_leaves_the_other_pair_quoting() {
        let mut book = Book::new(&[(1, eth()), (2, btc())], params(), Scope::Pair, true);
        let now = Instant::now();
        let vol = Volatility::new(vol_cfg());
        book.observe(2, BASE, now, &vol);
        book.observe(2, BASE + BASE / 100, now + Duration::from_secs(1), &vol);
        assert!(book.contagion(2, now + Duration::from_secs(1)).is_empty());
        assert!(book.withdrawn(2));
        assert!(!book.withdrawn(1));
    }

    #[test]
    fn a_disabled_book_never_withdraws_anything() {
        let mut book = Book::new(&[(1, eth())], params(), Scope::Book, false);
        let now = Instant::now();
        let vol = Volatility::new(vol_cfg());
        assert!(book.observe(1, BASE, now, &vol).is_none());
        assert!(book.observe(1, BASE * 2, now + Duration::from_secs(1), &vol).is_none());
        assert!(!book.withdrawn(1));
        assert_eq!(book.withdrawn_pairs().count(), 0);
    }

    #[test]
    fn btc_needs_a_bigger_move_than_eth_and_that_is_the_point() {
        // A single global `sigma_k` against two instruments. BTC's measured sigma is a third of
        // ETH's, so a fixed bp threshold would either be numb on ETH or hair-triggered on BTC.
        let mut e = Path::new(eth());
        let mut b = Path::new(btc());
        e.quiet(BASE, 30);
        b.quiet(BASE, 30);
        // 6 bp: above ETH's 5 bp floor, below BTC's 8 bp floor.
        let six = BASE + BASE * 6 / 10_000;
        assert_eq!(e.step(six).tripped, Some(Reason::Move));
        assert!(b.step(six).tripped.is_none());
        // 8 bp trips both.
        let mut b = Path::new(btc());
        b.quiet(BASE, 30);
        assert_eq!(b.step(BASE + BASE * 8 / 10_000).tripped, Some(Reason::Move));
    }
}
