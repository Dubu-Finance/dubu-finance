//! Jump detection and withdrawal: the defence that does not try to price through a jump.
//!
//! A linear ladder absorbs a reference error up to
//!
//! ```text
//! absorption = half_spread + width / 2
//! ```
//!
//! and nothing beyond it; past that the pool pays `(reference error - absorption) x exposed
//! depth`. No posted spread makes a 100 bp gap profitable — the sweep needs a 30 bp constant
//! half-spread before the sign flips, which is UniV2's fee — so the response is to withdraw rather
//! than to re-price. `refreshCapacity(pair, 0, 0)` makes `PropPool._outFor` return zero from every
//! quote path, held for a cool-off and resumed only once the reference has stopped moving.
//!
//! # What counts as a jump
//!
//! ```text
//! threshold = clamp( k * sigma(dt),  half_spread,  half_spread + width/2 )
//!                    \___________/   \__________/  \____________________/
//!                     the sigma arm   the floor     the absorption ceiling
//! ```
//!
//! The floor is the pair's own half-spread: per-pair configuration rather than a fixed bp
//! threshold, the exact point at which the posted quote stops being on the right side of fair
//! value, and above the measured cross-venue feed noise of 0.1-1.6 bps.
//!
//! The ceiling is the pair's own absorption limit, and it answers the sigma arm's own failure: one
//! 100 bp return entering a 60 s EWMA lifts ETH's `sigma(1s)` from 0.58 bp to 12.9 bp, so at
//! `k = 6` the arm would ask 77 bp before firing again and the second leg of a two-stage jump
//! would sail through. Above the absorption limit the pool loses money whatever sigma says.
//!
//! Between them the sigma arm *raises* the bar as volatility rises, so the bot does not withdraw
//! on every ordinary swing in a fast market.
//!
//! Both bounds come from the pair's **configured** `half_spread_bps`, never the volatility-scaled
//! one in [`crate::spread`]: feeding the widened value back in would make the detector numb in
//! exactly the regime where jumps cluster. (`width_bps` is likewise an upper bound the inverse
//! solver may narrow, so the ceiling is slightly generous; the floor is exact.)
//!
//! An observation separated from the previous one by more than `sample_max` is a hole rather than
//! a return, and trips: `FeedNotLive` gates pushes but does not withdraw capacity, so through that
//! hole the epoch stays armed behind a fixed ladder.
//!
//! # The cool-off
//!
//! Resuming into the second leg is the failure mode, so the cool-off is not a timer that runs
//! down. It ends when all three hold:
//!
//! 1. at least `cooloff` has passed **since the most recent trip**, so a second leg restarts it
//!    rather than shortening it;
//! 2. the trailing `cooloff` window has a peak-to-trough **range** within the current threshold,
//!    which catches the staircase that walks 100 bp in twenty 5 bp steps without ever tripping the
//!    single-observation test. This is the load-bearing condition;
//! 3. the newest observation is fresh, so a dead feed cannot satisfy the settle test by having no
//!    observations to contradict it.
//!
//! One false positive costs ~$0.48 of foregone spread against $16,580 for one avoided pick-off, so
//! the design is biased hard toward firing and a few trips per pair per day are expected. What
//! bounds the bias is mechanical: a withdrawal is a transaction and an in-flight transaction
//! blocks the pair, so the retriggerable cool-off makes each trip one transaction, not a stream.
//!
//! Withdrawal is shared across the correlation group by default ([`Scope::Book`]): in the
//! market-wide case a correlated pair's move is *coming* and its feed has not printed it yet, and
//! that window belongs to the searcher. Only the withdrawal is shared; the threshold stays
//! per-pair.

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

use dubu_core::math::mul_div_floor;

use crate::skew::Volatility;

/// 100%, in hundredths of a basis point — the unit every move and threshold here is carried in.
/// Hundredths rather than whole bps, because the thresholds are single-digit bps.
pub const BPS_E2: u128 = 1_000_000;

// --- Parameters ---

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
    pub sample_min: Duration,
    /// A separation longer than this is a hole in the reference rather than a return, and trips.
    pub sample_max: Duration,
}

/// One pair's economic bounds, in hundredths of a bp. Both come from the pair's own
/// configuration, which is what makes a single global `sigma_k` correct across two instruments
/// with very different volatilities and ladders.
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

    /// The same bounds with the floor stated separately from the spread. `floor_bps` and
    /// `half_spread_bps` answer different questions: `s0` is what the pool charges in a dead
    /// market, so tying the floor to it puts the floor under the tick noise of a combined
    /// reference whenever `s0` is small. The floor is how large a move the detector refuses to
    /// absorb.
    #[must_use]
    pub const fn new(floor_bps: u16, half_spread_bps: u16, width_bps: u16) -> Self {
        // `* 100` converts bps to hundredths of a bp; `* 50` is `width / 2` in the same step.
        let floor = (floor_bps as u32) * 100;
        // A zero floor would let feed noise trip the detector on every tick. The config validator
        // already refuses `half_spread_bps = 0`, so this is the second belt.
        let floor = if floor == 0 { 100 } else { floor };
        // The ceiling is a property of the ladder, not of the detector: a move the ladder absorbs
        // is not a jump however the floor is set.
        let ceiling = (half_spread_bps as u32) * 100 + (width_bps as u32) * 50;
        // A floor above the ceiling would degenerate the clamp silently into always-floor; raising
        // the ceiling to meet it keeps `threshold` monotone in sigma.
        let ceiling = if ceiling < floor { floor } else { ceiling };
        Self {
            floor_bps_e2: floor,
            ceiling_bps_e2: ceiling,
        }
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

/// Which of the three terms set the threshold. Logged: a sigma arm pinned at the absorption
/// ceiling for minutes is the shape of a market this bot should not be in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Bound {
    /// The pair's half-spread: the calm-market case, and what an untested observation reports.
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

// --- State ---

/// Why the detector tripped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// One observation moved further than the threshold.
    Move,
    /// The reference disappeared for longer than `sample_max`. See the module docs.
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

/// Everything one observation produced. The numeric part is logged every cycle alongside the row,
/// so `sigma_k` can be back-solved from history rather than guessed at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Observation {
    /// How far the reference moved since the last tested observation, in hundredths of a bp.
    pub move_bps_e2: u32,
    /// The threshold it was tested against.
    pub threshold_bps_e2: u32,
    /// `sigma` scaled to this observation's own interval, in hundredths of a bp. Straight from
    /// [`Volatility`]: there is exactly one volatility estimator in this crate.
    pub sigma_bps_e2: u32,
    /// Which term set the threshold.
    pub bound: Bound,
    /// The observation interval.
    pub dt_ms: u64,
    /// Peak-to-trough range of the trailing cool-off window, in hundredths of a bp: the settle
    /// test's input.
    pub range_bps_e2: u32,
    /// Why this observation tripped. A second leg arriving while already withdrawn re-arms the
    /// cool-off and reports `Some` again; [`Observation::edge`] distinguishes the two cases.
    pub tripped: Option<Reason>,
    /// `true` when this observation was the transition into [`State::Withdrawn`], as opposed to a
    /// further trip while already there. Only the edge needs a transaction.
    pub edge: bool,
    /// Set when this observation ended the cool-off.
    pub resumed: bool,
    /// State after this observation.
    pub state: State,
}

// --- The detector ---

/// One pair's jump detector and cool-off state machine.
///
/// It keeps its **own** price anchor, separate from [`Volatility`]'s, because the two are sampled
/// at different rates — the estimator once per quote cycle (~1 Hz), this by the fast-lane scan
/// (~5 Hz) — and reads `sigma` from that one estimator scaled to its own anchor's interval. The
/// sigma it reads is the estimate **as of before** the observation being tested: folding a jump's
/// own return into the variance and then asking whether it is surprising is a test that can never
/// fire.
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

    /// How many times this detector has tripped since start. In every log line: a detector that
    /// has never fired and one that fires hourly need different responses.
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

    /// Fold in one reference observation and decide. `vol` is read but never written: this module
    /// adds no second estimator.
    pub fn observe(&mut self, price: u128, now: Instant, vol: &Volatility) -> Observation {
        // The window first, so the settle test below sees the newest sample and a sub-`sample_min`
        // observation still contributes to the range.
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
        // The estimate as of BEFORE this observation: the loop folds the tick in after the scan.
        let sigma_bps_e2 = vol.sigma_bps_e2_over_ms(dt_ms.max(1));
        let base = Measured {
            threshold_bps_e2: self.threshold_bps_e2,
            sigma_bps_e2,
            dt_ms,
            ..Measured::default()
        };

        if dt < self.params.sample_min {
            // Too close together to test. The anchor is kept so the next observation spans a
            // sensible interval rather than starting over.
            let resumed = self.maybe_resume(now);
            return self.report(Measured { resumed, ..base });
        }

        if dt > self.params.sample_max || prev == 0 {
            // A hole in the reference: not a return, and not safe to assume nothing happened.
            self.anchor = Some((price, now));
            let edge = self.trip(now, Reason::FeedGap);
            return self.report(Measured {
                tripped: Some(Reason::FeedGap),
                edge,
                ..base
            });
        }

        let (threshold, bound) = self.bounds.threshold(self.params.sigma_k_e2, sigma_bps_e2);
        self.threshold_bps_e2 = threshold;

        let moved = mul_div_floor(price.abs_diff(prev), BPS_E2, prev).unwrap_or(BPS_E2);
        let move_bps_e2 = u32::try_from(moved).unwrap_or(u32::MAX);
        self.anchor = Some((price, now));
        let base = Measured {
            move_bps_e2,
            threshold_bps_e2: threshold,
            bound,
            ..base
        };

        if move_bps_e2 >= threshold {
            let edge = self.trip(now, Reason::Move);
            self.report(Measured {
                tripped: Some(Reason::Move),
                edge,
                ..base
            })
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
        // Retriggerable: measured from the MOST RECENT trip, so a second leg restarts the cool-off
        // rather than being absorbed by the first leg's timer.
        self.tripped_at = Some(now);
        self.last_reason = Some(reason);
        if edge {
            self.trips = self.trips.saturating_add(1);
        }
        edge
    }

    /// End the cool-off if every condition holds. Returns whether it did.
    fn maybe_resume(&mut self, now: Instant) -> bool {
        if !self.state.withdrawn() {
            return false;
        }
        if !self.settled(now) {
            return false;
        }
        self.state = State::Quoting;
        self.tripped_at = None;
        true
    }

    /// The three conditions, in the order they are cheapest to reject.
    fn settled(&self, now: Instant) -> bool {
        let Some(t) = self.tripped_at else {
            return true;
        };
        // 1. The minimum, measured from the most recent trip.
        if now.saturating_duration_since(t) < self.params.cooloff {
            return false;
        }
        // 3. A dead feed must never satisfy the settle test by having nothing to contradict it.
        let Some(&(_, last_t)) = self.window.back() else {
            return false;
        };
        if now.saturating_duration_since(last_t) > self.params.sample_max {
            return false;
        }
        if self.window.len() < 2 {
            return false;
        }
        // 2. The reference has settled. The window holds only the last `cooloff` of observations
        //    and the trip was at least that long ago, so it is entirely post-trip.
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
/// returns. A struct rather than eight positional arguments, because five are integers in the
/// same units and any two adjacent ones would transpose without a type error.
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

// --- The book ---

/// Every pair's detector, plus the scope rule that decides how far one pair's jump travels. Keyed
/// by pair id rather than positionally: a `Vec` indexed in lock-step with `cfg.pairs` is an
/// invariant nothing enforces, and getting it wrong tests one pair's move against another pair's
/// absorption limit.
#[derive(Debug, Clone)]
pub struct Book {
    detectors: BTreeMap<u16, Detector>,
    /// Which correlation group each pair belongs to. Contagion travels inside a group and never
    /// across: [`Scope::Book`] meaning the literal whole book holds only while every pair is
    /// correlated, and an equity's move says nothing about ETH while being the most volatile
    /// market here. A group is a claim about correlation, so it is configuration rather than
    /// inferred; pairs with no group named share one default group.
    groups: BTreeMap<u16, String>,
    scope: Scope,
    enabled: bool,
}

impl Book {
    /// Build from `(pair_id, bounds)` pairs.
    #[must_use]
    pub fn new(pairs: &[(u16, Bounds)], params: Params, scope: Scope, enabled: bool) -> Self {
        Self::grouped(pairs, params, scope, enabled, &BTreeMap::new())
    }

    /// The same, with each pair's correlation group. See [`Self::groups`].
    #[must_use]
    pub fn grouped(
        pairs: &[(u16, Bounds)],
        params: Params,
        scope: Scope,
        enabled: bool,
        groups: &BTreeMap<u16, String>,
    ) -> Self {
        Self {
            detectors: pairs
                .iter()
                .map(|&(id, b)| (id, Detector::new(params, b)))
                .collect(),
            groups: groups.clone(),
            scope,
            enabled,
        }
    }

    /// The group a pair belongs to. Unnamed pairs share one default group.
    fn group_of(&self, id: u16) -> &str {
        self.groups.get(&id).map_or("", String::as_str)
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
        if self.enabled {
            self.detectors
                .get(&pair_id)
                .is_some_and(Detector::withdrawn)
        } else {
            false
        }
    }

    /// Every pair currently withdrawn, in id order.
    pub fn withdrawn_pairs(&self) -> impl Iterator<Item = u16> + '_ {
        let enabled = self.enabled;
        self.detectors
            .iter()
            .filter(move |(_, d)| enabled && d.withdrawn())
            .map(|(id, _)| *id)
    }

    /// Fold one pair's reference observation in. `None` for an unknown pair or a disabled book.
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

    /// Propagate a trip on `origin` to every other pair in its group, if the scope says to.
    ///
    /// Returns only the pairs this *newly* withdrew, i.e. those that owe a withdrawal transaction.
    /// A pair already withdrawn has its cool-off re-armed and is not returned, because re-sending
    /// `refreshCapacity(pair, 0, 0)` against a zero epoch only burns a nonce.
    pub fn contagion(&mut self, origin: u16, now: Instant) -> Vec<u16> {
        if !self.enabled {
            return Vec::new();
        }
        if self.scope == Scope::Pair {
            return Vec::new();
        }
        // Resolved before the mutable borrow and cloned rather than held: `group_of` borrows
        // `self`, and the loop below needs it mutably.
        let origin_group = self.group_of(origin).to_string();
        let peers: Vec<u16> = self
            .groups
            .iter()
            .map(|(&id, g)| (id, g.as_str() == origin_group))
            .chain(
                self.detectors
                    .keys()
                    .filter(|id| !self.groups.contains_key(id))
                    .map(|&id| (id, origin_group.is_empty())),
            )
            .filter(|&(id, same)| same && id != origin)
            .map(|(id, _)| id)
            .collect();
        let mut newly = Vec::new();
        for id in peers {
            if let Some(d) = self.detectors.get_mut(&id) {
                if d.contagion(now) {
                    newly.push(id);
                }
            }
        }
        newly
    }
}

#[cfg(test)]
mod tests {
    /// A 1 bp floor sits under the tick noise of a four-venue combined reference, so tying the
    /// floor to `s0` turns an ordinary 3.24 bp move into a 30s cool-off.
    #[test]
    fn ordinary_reference_noise_is_not_a_jump_at_a_one_bp_spread() {
        let coupled = Bounds::from_pair(1, 25);
        let (t, bound) = coupled.threshold(600, 3);
        assert_eq!(
            (t, bound),
            (100, Bound::Floor),
            "the old floor was s0 itself"
        );
        assert!(324 > t, "so 3.24bp of noise tripped it");

        let split = Bounds::new(5, 1, 25);
        let (t, bound) = split.threshold(600, 3);
        assert_eq!((t, bound), (500, Bound::Floor));
        assert!(324 < t, "the same sample is now inside the floor");

        // The ceiling is still the absorption limit, untouched by the floor.
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
        VolConfig {
            tau_ms: 60_000,
            horizon_secs: 300,
            sample_ms_min: 100,
            sample_ms_max: 10_000,
        }
    }

    fn params() -> Params {
        Params {
            sigma_k_e2: 600, // k = 6
            cooloff: Duration::from_secs(30),
            sample_min: Duration::from_millis(100),
            sample_max: Duration::from_secs(10),
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

    /// Feeds the SAME observations to the one volatility estimator and to the detector, in the
    /// order the live loop does (read sigma, then update sigma), so this exercises the real wiring.
    struct Path {
        vol: Volatility,
        det: Detector,
        t: Instant,
    }

    impl Path {
        fn new(bounds: Bounds) -> Self {
            Self {
                vol: Volatility::new(vol_cfg()),
                det: Detector::new(params(), bounds),
                t: Instant::now(),
            }
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

    // --- The threshold itself ---

    #[test]
    fn the_bounds_are_the_pairs_own_spread_and_its_absorption_limit() {
        // Why one global `sigma_k` works across two instruments: both bounds come from the pair.
        assert_eq!(
            eth(),
            Bounds {
                floor_bps_e2: 500,
                ceiling_bps_e2: 1_750
            }
        ); // 5 bp .. 17.5 bp
        assert_eq!(
            btc(),
            Bounds {
                floor_bps_e2: 800,
                ceiling_bps_e2: 2_800
            }
        ); // 8 bp .. 28 bp
    }

    #[test]
    fn the_sigma_arm_is_clamped_at_both_ends() {
        let b = eth();
        // Calm: ETH's sigma(1s) is 0.577 bp, so 6x that is 3.46 bp, under the 5 bp floor.
        assert_eq!(b.threshold(600, 57), (500, Bound::Floor));
        // Fast: strictly between. sigma(1s) = 1.5 bp -> 9 bp.
        assert_eq!(b.threshold(600, 150), (900, Bound::Sigma));
        // The case the ceiling exists for: sigma(1s) at ~12.9 bp asks 77 bp on the arm alone.
        assert_eq!(b.threshold(600, 1_290), (1_750, Bound::Absorption));
    }

    // --- Ordinary diffusion must NOT trip it ---

    #[test]
    fn ordinary_diffusion_never_trips_the_detector() {
        // 1 bp/s is the regime the pool is built to quote into, so a detector firing here is a
        // quoting-outage generator. This is what keeps `sigma_k` honest.
        let mut p = Path::new(eth());
        let bp = BASE / 10_000;
        // A 1 bp/s sawtooth plus a slow drift, so it is not an oscillation the EWMA absorbs.
        for i in 0..600u128 {
            let price = if i % 2 == 0 {
                BASE + i / 4 * bp
            } else {
                BASE + i / 4 * bp + bp
            };
            let obs = p.step(price);
            assert!(
                obs.tripped.is_none(),
                "1 bp/s diffusion tripped at i={i}: {obs:?}"
            );
            assert_eq!(obs.state, State::Quoting);
        }
        assert_eq!(p.det.trips(), 0);
        // ... and the estimator did see the volatility, so this does not pass on a zero sigma.
        assert!(p.vol.sigma_millibps() > 0);
    }

    #[test]
    fn a_move_just_under_the_floor_holds_and_one_at_it_fires() {
        // ETH's floor is 5 bp, and it is a boundary rather than a blanket.
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

    // --- The jump ---

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
        assert_eq!(
            obs.bound,
            Bound::Floor,
            "in a calm market the floor is what fires"
        );
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
        assert!(
            obs.resumed,
            "must resume after the cool-off on a settled reference"
        );
        assert_eq!(obs.state, State::Quoting);
        assert_eq!(p.det.trips(), 1, "resuming is not a second trip");
    }

    #[test]
    fn the_absorption_ceiling_is_what_catches_the_second_leg() {
        // Leg one raises sigma so far that a pure sigma-multiple would ask ~77 bp; leg two is
        // 50 bp, so without the ceiling the detector is numb when it matters most.
        let mut p = Path::new(eth());
        p.quiet(BASE, 60);

        let leg1 = BASE + BASE / 100; // +100 bp
        let o1 = p.step(leg1);
        assert_eq!(o1.tripped, Some(Reason::Move));
        assert!(o1.edge);

        // Sigma is now enormous, and the raw sigma arm is far above the absorption limit.
        let sigma_now = p.vol.sigma_bps_e2_over_ms(1_000);
        assert!(
            sigma_now > 1_000,
            "one 100 bp return must move sigma(1s) past 10 bp, got {sigma_now}"
        );
        assert_eq!(eth().threshold(600, sigma_now).1, Bound::Absorption);

        // Ten quiet seconds — not enough to resume, and sigma decays only slowly.
        for _ in 0..10 {
            p.step(leg1);
        }

        // Leg two: +50 bp. Below the numbed sigma arm, above the absorption ceiling.
        let leg2 = leg1 + leg1 / 200;
        let o2 = p.step(leg2);
        assert_eq!(o2.move_bps_e2, 5_000);
        assert_eq!(
            o2.bound,
            Bound::Absorption,
            "the ceiling, not sigma, is what set the threshold"
        );
        assert!(o2.threshold_bps_e2 <= 1_750);
        assert_eq!(o2.tripped, Some(Reason::Move), "the second leg MUST trip");
        assert!(
            !o2.edge,
            "already withdrawn, so no second transaction is owed"
        );
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
            assert!(
                obs.state.withdrawn(),
                "resumed {i}s after the SECOND leg, cool-off is 30s"
            );
        }
        assert!(
            p.step(leg2).resumed,
            "and it does come back once the second leg has settled"
        );
    }

    #[test]
    fn a_staircase_that_never_trips_a_single_observation_still_holds_the_cooloff_open() {
        // 100 bp in 4 bp steps, none clearing the 5 bp floor: the single-observation test is
        // blind to it, and the settle test's peak-to-trough range is not.
        let mut p = Path::new(eth());
        p.quiet(BASE, 60);
        let jumped = BASE + BASE / 100;
        assert!(p.step(jumped).edge);

        // Now walk 4 bp per second for the whole cool-off. No step trips.
        let mut price = jumped;
        for i in 0..40u64 {
            price += price * 4 / 10_000;
            let obs = p.step(price);
            assert!(
                obs.tripped.is_none(),
                "a 4 bp step must not trip the move test (i={i})"
            );
            assert!(
                obs.state.withdrawn(),
                "but the range test must hold the withdrawal open (i={i})"
            );
            assert!(obs.range_bps_e2 > obs.threshold_bps_e2);
        }
        // It resumes only once the walking stops.
        let settled = p.quiet(price, 31);
        assert_eq!(settled.state, State::Quoting);
    }

    // --- The other trips ---

    #[test]
    fn a_hole_in_the_reference_is_treated_as_a_jump() {
        // `FeedNotLive` gates pushes but not capacity, so an outage leaves a full epoch behind a
        // fixed ladder. Coming back armed is the hole this closes.
        let mut p = Path::new(eth());
        p.quiet(BASE, 10);
        p.t += Duration::from_secs(120);
        let obs = p.det.observe(BASE, p.t, &p.vol);
        assert_eq!(obs.tripped, Some(Reason::FeedGap));
        assert!(obs.edge);
        assert!(
            obs.state.withdrawn(),
            "the price is unchanged, and that is not the point"
        );
    }

    #[test]
    fn samples_closer_than_min_sample_are_recorded_but_not_tested() {
        // The two scans can land milliseconds apart, and a rounding error over 10 ms is a jump.
        let mut p = Path::new(eth());
        p.quiet(BASE, 5);
        let t = p.t + Duration::from_millis(10);
        let obs = p.det.observe(BASE * 2, t, &p.vol);
        assert!(obs.tripped.is_none(), "a 10 ms separation is not a return");
        // The anchor is still the pre-gap price, so the next observation measures the whole move.
        let obs = p
            .det
            .observe(BASE * 2, t + Duration::from_millis(1_000), &p.vol);
        assert_eq!(obs.tripped, Some(Reason::Move));
    }

    #[test]
    fn a_dead_feed_can_never_satisfy_the_settle_test() {
        // Silence is not settlement: otherwise the cool-off expires on a dead feed and the pool
        // re-arms blind.
        let mut p = Path::new(eth());
        p.quiet(BASE, 10);
        let jumped = BASE + BASE / 100;
        p.step(jumped);
        // Time passes with no observations at all, well past the cool-off.
        let much_later = p.t + Duration::from_secs(600);
        assert!(!p.det.settled(much_later));
        assert!(p.det.withdrawn());
    }

    // --- Contagion ---

    /// Contagion stops at the group edge: the most volatile market in the book is also the least
    /// related to the rest of it.
    #[test]
    fn a_jump_in_one_group_does_not_withdraw_another() {
        let b = Bounds::from_pair(1, 8);
        let mut book = Book::grouped(
            &[(1, b), (2, b), (8, b), (9, b)],
            params(),
            Scope::Book,
            true,
            &[
                (1, "crypto".to_string()),
                (2, "crypto".to_string()),
                (8, "equity".to_string()),
                (9, "equity".to_string()),
            ]
            .into_iter()
            .collect(),
        );
        let hit = book.contagion(8, Instant::now());
        assert_eq!(
            hit,
            vec![9],
            "only the other equity; ETH and BTC are untouched"
        );

        let mut book2 = Book::grouped(
            &[(1, b), (2, b), (8, b), (9, b)],
            params(),
            Scope::Book,
            true,
            &[
                (1, "crypto".to_string()),
                (2, "crypto".to_string()),
                (8, "equity".to_string()),
                (9, "equity".to_string()),
            ]
            .into_iter()
            .collect(),
        );
        assert_eq!(
            book2.contagion(1, Instant::now()),
            vec![2],
            "and a crypto jump stays in crypto"
        );
    }

    /// No groups named means one default group holding everything.
    #[test]
    fn ungrouped_pairs_share_one_group() {
        let b = Bounds::from_pair(1, 8);
        let mut book = Book::new(&[(1, b), (2, b), (8, b)], params(), Scope::Book, true);
        assert_eq!(book.contagion(1, Instant::now()), vec![2, 8]);
    }

    // --- Scope ---

    #[test]
    fn a_btc_jump_withdraws_eth_when_the_scope_is_the_book() {
        let mut book = Book::new(&[(1, eth()), (2, btc())], params(), Scope::Book, true);
        let now = Instant::now();
        let vol = Volatility::new(vol_cfg());

        // pairId 2 jumps; pairId 1 has seen nothing at all.
        book.observe(2, BASE, now, &vol);
        let o = book
            .observe(2, BASE + BASE / 100, now + Duration::from_secs(1), &vol)
            .unwrap();
        assert_eq!(o.tripped, Some(Reason::Move));

        let newly = book.contagion(2, now + Duration::from_secs(1));
        assert_eq!(newly, vec![1], "the other pair owes a withdrawal");
        assert!(book.withdrawn(1) && book.withdrawn(2));
        assert_eq!(
            book.detector(1).and_then(Detector::last_reason),
            Some(Reason::Contagion)
        );

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
        assert!(book
            .observe(1, BASE * 2, now + Duration::from_secs(1), &vol)
            .is_none());
        assert!(!book.withdrawn(1));
        assert_eq!(book.withdrawn_pairs().count(), 0);
    }

    #[test]
    fn btc_needs_a_bigger_move_than_eth() {
        // BTC's sigma is a third of ETH's, so a fixed bp threshold is numb on one or the other.
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
