//! Inventory skew: the Avellaneda–Stoikov reservation price, and the volatility estimate it
//! needs.
//!
//! # Why there is a skew at all
//!
//! The bot used to quote symmetrically around the reference regardless of what the pool held, so
//! sustained one-way flow accumulated inventory and nothing pushed it back. There is no hedge
//! venue — a Korean corporate real-name exchange account is not available, so there is no account
//! to hedge into — which means this is not one inventory control among several. It is the only
//! one there is.
//!
//! # The model, and only the linear term
//!
//! ```text
//! r = s - q * gamma * sigma^2
//! ```
//!
//! `s` is the reference price, `q` the signed inventory imbalance against target, `gamma` the
//! risk aversion, `sigma` the volatility over a stated horizon. The reservation price `r` is what
//! the book is centred on instead of `s`, so a pool that is long quotes lower on **both** sides
//! and gets hit on its ask sooner than on its bid.
//!
//! The load-bearing detail — and the reason to reach for A–S rather than a hand-rolled constant
//! `kappa * q` — is that the coefficient scales with **`sigma^2`, not with a constant**. The same
//! imbalance is far more dangerous in a fast market than in a calm one, and a fixed `kappa` is
//! wrong in both directions at once: too timid when volatility triples, too aggressive when the
//! market goes quiet and the position could have been worked off at no cost.
//!
//! **The derivative and integral terms are deliberately absent.** Turning this into a PID would
//! need three coefficients, and three coefficients can only be tuned against replay data that
//! does not exist here — so they would be three numbers picked by feel, which archi_v2 §5.4 is
//! explicit about not doing. The linear term is one coefficient and it is provably optimal under
//! the model's own assumptions. What would justify adding the others: a derivative term earns its
//! place once fill data shows the imbalance *oscillating* around target rather than converging
//! (skew overshooting, flow reversing, skew overshooting back), and an integral term once it
//! shows a persistent steady-state offset the proportional term never closes. Both are
//! measurable from the `skew` log line below plus fills, and neither should be added before they
//! have been measured.
//!
//! # Units, so that gamma is a number a human can reason about
//!
//! Everything here is dimensionless and expressed in basis points, which is the only way `gamma`
//! ends up in a tunable range:
//!
//! ```text
//! q      = base share of the book - target share       in [-1, 1], carried as ppm
//! sigma  = relative volatility over `horizon_secs`     carried as bps, and squared as bps^2
//! skew   = gamma * q * sigma^2 / 10_000                in bps of the reference
//! ```
//!
//! The `/ 10_000` is what converts `sigma^2` from "bps squared" back into bps. Measured on the
//! live feed on 2026-07-27, ETHUSDT's `sigma` over the 300s horizon runs about **10 bps** and
//! BTCUSDT's about **3 bps**, so `sigma^2 / 10_000` is 0.01 bp for ETH. A pool 20% away from its
//! target inventory with `gamma = 1000` therefore skews by 2 bp against a 5 bp half-spread —
//! visible, and not dominant. That is the range `gamma` is meant to live in, and it is worth
//! noting how far off a guess can be: this was first written assuming 30 bps, and since the skew
//! is quadratic in `sigma`, that guess was wrong by a factor of nine. `gamma` is a number to
//! back-solve from the `skew` log line, never to pick by feel.
//!
//! Both pairs share one `gamma`, and BTCUSDT's skew comes out near zero — its imbalance is 1%
//! rather than 11% and its volatility is a third of ETH's. That is the model working, not a
//! misconfiguration: `q * sigma^2` is small for that pair right now, so there is little risk to
//! push back against.
//!
//! # Warm-up
//!
//! The EWMA is seeded at zero, so `sigma` — and with it the skew — climbs from nothing over the
//! first few `tau` after a start or a feed outage. That is deliberately the conservative
//! direction: an unknown volatility produces no skew rather than an invented one. `vol_samples`
//! is in every log line so the warm-up is visible rather than looking like a dead feature.
//!
//! # The sign
//!
//! `q > 0` means the pool holds **more** base than it wants, so it wants to sell, so the book
//! must move **down**. `dubu-core`'s convention is that a positive `skew_bps` shifts the book
//! down, so the signs line up and no negation is needed anywhere. A–S agrees: long inventory
//! lowers the reservation price.
//!
//! # The skew can never cross the book
//!
//! Worth stating because it is the first thing to worry about. The skew moves the **mid**, and
//! both targets are derived from the skewed mid with the same half-spread — `bid = mid(1-hs)`,
//! `ask = mid(1+hs)` — so they move together and the spread between them is preserved exactly.
//! With `half_spread_bps` non-zero (which the config validator enforces) `bid < ask` for every
//! skew in range. `minBid` past `minAsk` is not a failure mode of this design; a test pins it
//! across the whole clamp range.
//!
//! The floor is the real constraint, and it is handled by [`min_price_cap_bps`] rather than left
//! to be discovered downstream: a large positive skew can push the bid target under the pair's
//! `minPrice`, at which point `ladder::build` correctly refuses the row — but a refused row is a
//! quoting outage, and clamping the skew to the largest value that still clears the floor keeps
//! the pool quoting a slightly-less-skewed book instead.

use std::time::Instant;

use dubu_core::ladder::{BPS, MAX_BPS};
use dubu_core::math::{mul_div_ceil, mul_div_floor};

/// Relative returns are carried as parts per `10^8`, matching [`crate::units::FEED_SCALE`].
const REL: u128 = 100_000_000;

// ---------------------------------------------------------------------------
// Volatility
// ---------------------------------------------------------------------------

/// How the volatility estimator is parameterised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VolConfig {
    /// EWMA time constant, in milliseconds. The weight on a new sample is `dt / tau`, so the
    /// **half-life** is `tau * ln2`, about 0.69 of this.
    pub tau_ms: u64,
    /// The horizon the reported volatility is scaled to, in seconds. See [`Volatility`].
    pub horizon_secs: u64,
    /// Samples closer together than this are skipped rather than divided by a tiny `dt`.
    pub min_sample_ms: u64,
    /// A gap longer than this is not a return, it is an outage. The estimator re-anchors and
    /// contributes nothing.
    pub max_sample_ms: u64,
}

/// EWMA of squared relative returns, kept as a **per-second variance** and scaled to a horizon.
///
/// # The horizon, and why 300 seconds
///
/// `sigma` only means something with a horizon attached, and the choice sets the numeric range
/// `gamma` has to live in, so it is worth stating rather than defaulting.
///
/// The default is **300 seconds, the same window as `risk.bleed_window_secs`**. That is not a
/// coincidence and it is the whole argument: the bleed killswitch already defines the horizon
/// over which an adverse move against the inventory is treated as a real loss rather than as
/// noise. The skew exists to push inventory back before that switch has anything to measure, so
/// it should be sized against the volatility over exactly that window. Any other horizon would
/// mean two parts of the same risk system disagreeing about what "soon" means.
///
/// Two horizons that look tempting and are not:
///
/// * *One second*, the cycle cadence — the position the skew is working off survives orders of
///   magnitude longer than one block, so per-second volatility understates the risk being carried
///   by a factor of ~17 and `gamma` would have to absorb it.
/// * *One hour*, the pool's `maxStaleSecs` — that is the staleness backstop, not a holding
///   horizon, and at that scale `sigma^2` moves so slowly that the skew becomes a constant,
///   which is precisely what A–S was chosen over.
///
/// # The estimator
///
/// Returns are sampled once per quote cycle, which the `newHeads` subscription puts at about 1 Hz.
/// Each sample contributes `r^2 / dt` to a per-second variance, so jitter in the cycle cadence —
/// a fallback-timer cycle at 2s, a missed head — does not bias the level. Scaling to the horizon
/// is the usual square-root-of-time: `sigma(T)^2 = var_per_sec * T`.
///
/// The EWMA weight is the standard discretisation `w = 1 - exp(-dt/tau)`, taken to first order as
/// `dt/tau`. At the default `tau` of 60s and `dt` of 1s the two differ by under 1%, and the
/// approximation is what keeps the whole update in integers.
#[derive(Debug, Clone)]
pub struct Volatility {
    cfg: VolConfig,
    /// EWMA variance of relative returns, per second, in `(1/REL)^2` units.
    var_per_sec: u128,
    last: Option<(u128, Instant)>,
    samples: u64,
}

impl Volatility {
    /// A fresh estimator with no history.
    #[must_use]
    pub const fn new(cfg: VolConfig) -> Self {
        Self {
            cfg,
            var_per_sec: 0,
            last: None,
            samples: 0,
        }
    }

    /// How many returns have been folded in. Below a few `tau`s the estimate is still warming up,
    /// which is why it is logged.
    #[must_use]
    pub const fn samples(&self) -> u64 {
        self.samples
    }

    /// Forget the history.
    ///
    /// Called when the reference price is unavailable, so that the first sample after an outage
    /// is not a "return" spanning the whole gap. Without it, a two-minute feed outage during
    /// which the market moved 1% would enter the estimator as a single enormous one-second
    /// return and the skew would be sized off it for the next several minutes.
    pub fn reset(&mut self) {
        self.last = None;
    }

    /// Fold in one observation of the reference price.
    pub fn observe(&mut self, price: u128, now: Instant) {
        let Some((prev, t0)) = self.last else {
            self.last = Some((price, now));
            return;
        };
        let dt_ms =
            u64::try_from(now.saturating_duration_since(t0).as_millis()).unwrap_or(u64::MAX);
        if dt_ms < self.cfg.min_sample_ms {
            // Too close together to divide by. Keep the old anchor so the next sample spans a
            // sensible interval rather than starting over.
            return;
        }
        if dt_ms > self.cfg.max_sample_ms || prev == 0 {
            self.last = Some((price, now));
            return;
        }

        // |r| in parts per REL. The sign is irrelevant: it is squared immediately.
        let r = mul_div_floor(price.abs_diff(prev), REL, prev).unwrap_or(REL);
        let contrib = r
            .checked_mul(r)
            .and_then(|sq| sq.checked_mul(1_000))
            .map_or(u128::MAX, |x| x / u128::from(dt_ms));

        let w_num = u128::from(dt_ms.min(self.cfg.tau_ms));
        let w_den = u128::from(self.cfg.tau_ms.max(1));
        self.var_per_sec = if contrib >= self.var_per_sec {
            let step = mul_div_floor(contrib - self.var_per_sec, w_num, w_den).unwrap_or(0);
            self.var_per_sec.saturating_add(step)
        } else {
            let step = mul_div_floor(self.var_per_sec - contrib, w_num, w_den).unwrap_or(0);
            self.var_per_sec.saturating_sub(step)
        };

        self.last = Some((price, now));
        self.samples = self.samples.saturating_add(1);
    }

    /// `sigma^2` over the configured horizon, in **bps squared scaled by `10^6`**.
    ///
    /// This is the number the skew actually multiplies, and it is deliberately the one with no
    /// square root in it: A–S needs `sigma^2`, so taking a root here only to square it again
    /// would throw away precision for nothing. [`Volatility::sigma_millibps`] exists for the log
    /// line and for humans.
    #[must_use]
    pub const fn sigma_sq_bps_e6(&self) -> u128 {
        // sigma_rel^2 over the horizon is `var_per_sec * horizon` in (1/REL)^2 units. In bps^2
        // that is `* REL^2 / 10^8`, i.e. `* 10^-8`; scaled by 10^6 it is `/ 100`.
        (self
            .var_per_sec
            .saturating_mul(self.cfg.horizon_secs as u128))
            / 100
    }

    /// `sigma` over the configured horizon, in thousandths of a basis point.
    ///
    /// Milli-bps rather than bps because a calm five-minute window on ETHUSDT is tens of bps and
    /// a quiet one is single digits; integer bps would round the interesting range to nothing.
    #[must_use]
    pub fn sigma_millibps(&self) -> u64 {
        u64::try_from(isqrt(self.sigma_sq_bps_e6())).unwrap_or(u64::MAX)
    }

    /// `sigma` over an **arbitrary** interval, in hundredths of a basis point.
    ///
    /// The horizon above is a property of the skew — it is the window over which an adverse move
    /// against inventory counts as a real loss. [`crate::jump`] asks a different question, about a
    /// single observation, and needs the same estimate scaled to that observation's own interval
    /// instead. This is the *only* way to get it: adding a second, faster estimator would be two
    /// numbers to keep consistent and two ways to be wrong about how volatile the market is.
    ///
    /// The same square-root-of-time scaling as [`Volatility::sigma_sq_bps_e6`], so a fast-lane
    /// scan at 200 ms and a cycle scan at 1 s are compared against thresholds that differ by
    /// exactly `sqrt(5)`, which is what makes the two sampling rates interchangeable.
    ///
    /// ```text
    /// sigma_bps(dt)^2 = var_per_sec * dt_ms / (1000 * 10^8)
    /// in hundredths of a bp, that is  * 10^4, i.e.  var_per_sec * dt_ms / 10^7
    /// ```
    #[must_use]
    pub fn sigma_bps_e2_over_ms(&self, dt_ms: u64) -> u32 {
        let v = self.var_per_sec.saturating_mul(u128::from(dt_ms)) / 10_000_000;
        u32::try_from(isqrt(v)).unwrap_or(u32::MAX)
    }
}

/// Integer square root, by Newton's method. Used only for the human-readable `sigma`.
fn isqrt(n: u128) -> u128 {
    if n < 2 {
        return n;
    }
    let mut x = n;
    let mut y = x.div_ceil(2);
    while y < x {
        x = y;
        y = (x + n / x) / 2;
    }
    x
}

// ---------------------------------------------------------------------------
// Inventory
// ---------------------------------------------------------------------------

/// One pair's inventory position, in quote units, against its configured target.
///
/// # What "the book" is, for a pool with a shared quote token
///
/// The target is a **share of the book**, not an absolute amount, so it stays meaningful as the
/// pool grows or shrinks. The book for one pair is
///
/// ```text
/// book_i = value(base_i) + quote_balance / pairs
/// ```
///
/// The even split of the shared quote token is a simplification and is named as one: both pairs
/// draw their bids from the same mUSDC, and nothing yet caps the sum of their bid liabilities
/// against it. That is archi_v2 §5.4's cross-asset clamp, which the README lists as not built.
/// This makes exactly the same assumption that gap already makes, in exactly the same place, so
/// it adds no new error — but a reader should know that `q` for one pair is not independent of
/// the other's inventory, and that closing the cross-asset clamp is what would make it so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Inventory {
    /// Base holdings valued at the reference, in quote units.
    pub base_value: u128,
    /// The hedge position, valued at the same reference and **signed**: negative when the venue is
    /// short, which is what a hedge against inventory looks like.
    ///
    /// Without this the skew reads the pool's balance alone and fights the hedge. The pool holds
    /// 3,506 ETH and the venue is short 3,506; net exposure is zero and the right quote is
    /// symmetric. Reading the pool alone says "3,506 ETH, too much, price it down" -- so the hedge
    /// flattens the position and the skew immediately prices to rebuild it. Two controls, opposite
    /// directions, both convinced they are correcting.
    ///
    /// Includes what is in flight. A hedge that has been sent but not filled has already committed
    /// the exposure; counting only what has settled double-counts it for as long as the venue takes
    /// to answer, which this afternoon was long enough to turn one 0.04 ETH fill into a 0.08 short.
    pub hedge_value: i128,
    /// This pair's share of the shared quote balance, in quote units.
    pub quote_share: u128,
    /// Target base share of the book, in parts per million. Configuration, never a constant.
    pub target_ppm: u32,
}

impl Inventory {
    /// The signed imbalance `q`, in parts per million.
    ///
    /// `+1_000_000` is "the whole book is base and the target was zero"; `-target_ppm` is "no base
    /// at all". Zero book yields zero imbalance rather than a division by zero — a pool holding
    /// nothing has no inventory to skew against.
    #[must_use]
    pub fn imbalance_ppm(&self) -> i64 {
        // The book is what the pool OWNS -- that is the capital the target share is a share of, and
        // a perp hedge adds none of it. What the hedge changes is the exposure, not the balance
        // sheet, so it belongs in the numerator and nowhere else.
        let book = self.base_value.saturating_add(self.quote_share);
        if book == 0 {
            return 0;
        }
        let net = i128::try_from(self.base_value)
            .unwrap_or(i128::MAX)
            .saturating_add(self.hedge_value);
        // A hedge larger than the holding leaves a net short. That is over-hedged rather than
        // impossible, and the skew should price to unwind it, so the signed path is kept rather
        // than clamped at zero.
        let book_i = i128::try_from(book).unwrap_or(i128::MAX).max(1);
        let current = net.saturating_mul(1_000_000) / book_i;
        i64::try_from(current).unwrap_or(1_000_000) - i64::from(self.target_ppm)
    }
}

// ---------------------------------------------------------------------------
// The skew
// ---------------------------------------------------------------------------

/// The clamps on the skew, and the asymmetry between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SkewParams {
    /// Risk aversion `gamma`, scaled by 100 so the config can say `100.5`.
    pub gamma_e2: u64,
    /// Cap on a **positive** skew, in bps. Positive moves the book down: the pool is long and
    /// wants to sell.
    pub max_positive_bps: u16,
    /// Cap on a **negative** skew, as a magnitude in bps. Negative moves the book up: the pool is
    /// short and wants to buy.
    pub max_negative_bps: u16,
}

/// Which bound, if any, held the skew back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Clamp {
    /// The model's own number was used unchanged.
    Unbound,
    /// `max_positive_bps` bound it.
    PositiveCap,
    /// `max_negative_bps` bound it.
    NegativeCap,
    /// The pair's `minPrice` floor bound it: any more skew would push the bid target under the
    /// pool's absolute floor and the row would be refused outright.
    MinPriceFloor,
}

impl Clamp {
    /// Short stable string for structured logs.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Unbound => "unbound",
            Self::PositiveCap => "positive_cap",
            Self::NegativeCap => "negative_cap",
            Self::MinPriceFloor => "min_price_floor",
        }
    }

    /// Whether a bound actually bit.
    #[must_use]
    pub const fn bound(self) -> bool {
        !matches!(self, Self::Unbound)
    }
}

/// Everything one skew computation produced. All of it is logged, every row, every cycle.
///
/// This is the input to tuning `gamma` later, and without it `gamma` is guesswork: the pair
/// `(imbalance_ppm, sigma_millibps) -> applied_bps` is exactly the sample a back-solve needs, and
/// `raw_decibps` alongside `applied_bps` is what says whether the clamp has been doing the
/// deciding instead of the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Skew {
    /// What goes into `dubu-core`. Positive shifts the book down.
    pub applied_bps: i16,
    /// The model's number before clamping and before rounding to whole bps, in deci-bps. The
    /// interesting one for tuning, because whole-bps rounding hides everything under 0.5 bp.
    pub raw_decibps: i32,
    /// The signed imbalance that produced it.
    pub imbalance_ppm: i64,
    /// `sigma` over the horizon, in thousandths of a bp.
    pub sigma_millibps: u64,
    /// The largest positive skew the pair's `minPrice` floor left room for, in bps.
    pub floor_cap_bps: u16,
    /// Which bound held it back.
    pub clamp: Clamp,
}

/// The largest positive skew that still leaves the bid target above the pair's `minPrice`.
///
/// A positive skew moves the whole book down, and the bid target is the lowest thing derived from
/// the mid, so it hits the pool's absolute floor first. Past that point `ladder::build` refuses
/// the row — correctly, because `minPrice` is an oracle-independent backstop and quoting through
/// it is exactly what the floor forbids — but a refused row means the pool quotes nothing until
/// the market moves back. Clamping here trades a little skew for staying in the market, which is
/// the right way round: the skew is a preference and quoting is the job.
///
/// Derivation. `ladder::build` computes `mid = floor(fair * (BPS - skew) / BPS)` and then
/// `bid = floor(mid * (BPS - hs) / BPS)`, and needs `bid >= min_price`. It is sufficient that
/// `mid >= ceil(min_price * BPS / (BPS - hs)) + 1`, where the `+ 1` absorbs the floor in the
/// second step. Solving for the skew and rounding conservatively gives the value returned here.
#[must_use]
pub fn min_price_cap_bps(fair: u128, min_price: u128, half_spread_bps: u16) -> u16 {
    let hs = u128::from(half_spread_bps).min(MAX_BPS);
    if fair == 0 {
        return 0;
    }
    let Some(mid_min) = mul_div_ceil(min_price, BPS, BPS - hs) else {
        return 0;
    };
    let mid_min = mid_min.saturating_add(1);
    if fair <= mid_min {
        return 0;
    }
    // skew <= BPS - ceil(mid_min * BPS / fair)
    let Some(needed) = mul_div_ceil(mid_min, BPS, fair) else {
        return 0;
    };
    let cap = BPS.saturating_sub(needed);
    u16::try_from(cap.min(MAX_BPS)).unwrap_or(0)
}

/// Compute the Avellaneda–Stoikov linear skew for one row.
///
/// # The clamp, and why it is asymmetric
///
/// `max_positive_bps` is deliberately the looser of the two, and the asymmetry is about which
/// direction writes a free option.
///
/// A **positive** skew moves the book down. The pool is long, wants to sell, and both its bid and
/// its ask fall. The bid falling is defensive — it widens the gap between what the pool pays and
/// what the market says the asset is worth — and the ask falling is the point, because a cheaper
/// ask is what actually works the position off. Neither side of that is a gift to a taker. The
/// bound that matters in this direction is structural rather than economic, and it is the
/// `minPrice` floor, which is why [`min_price_cap_bps`] is folded in here rather than left to
/// downstream refusal.
///
/// A **negative** skew moves the book up. The pool is short base, wants to buy, and its **bid**
/// rises toward and eventually past the reference. A bid above fair value is a free option
/// written to whoever notices first — precisely the adverse-selection direction the whole
/// `adverse_drift_bps` asymmetry in `policy` exists to defend against — and unlike the positive
/// direction there is no structural floor to stop it. So it is capped tighter, and the tighter
/// cap is the *economic* one. The cost of the asymmetry is that a short book is worked off more
/// slowly than a long one; that is the correct trade for a book that cannot hedge, because a
/// slow recovery costs volume and a picked-off bid costs money.
///
/// Both caps also stop a single wild `sigma` print from inverting the strategy. `sigma^2` is
/// quadratic, so a volatility estimate that is 10x too high produces a skew 100x too large, and
/// the cap is what keeps that from becoming a 300 bp quote before anyone reads a log.
#[must_use]
pub fn compute(
    inventory: &Inventory,
    sigma_sq_bps_e6: u128,
    sigma_millibps: u64,
    params: &SkewParams,
    floor_cap_bps: u16,
) -> Skew {
    let q = inventory.imbalance_ppm();

    // skew_decibps = 10 * gamma * q * sigma^2 / 10_000, in two steps so no intermediate is
    // larger than it has to be. `step` is `gamma * sigma^2 * 1_000`.
    let step =
        mul_div_floor(sigma_sq_bps_e6, u128::from(params.gamma_e2), 100_000).unwrap_or(u128::MAX);
    let magnitude = mul_div_floor(step, u128::from(q.unsigned_abs()), 1_000_000_000_000)
        .unwrap_or(u128::from(u32::MAX));
    let magnitude = i64::try_from(magnitude).unwrap_or(i64::from(i32::MAX));
    let raw_decibps_i64 = if q < 0 { -magnitude } else { magnitude };
    let raw_decibps =
        i32::try_from(raw_decibps_i64).unwrap_or(if q < 0 { i32::MIN } else { i32::MAX });

    // Round half away from zero, so a 0.5 bp skew becomes 1 bp rather than 0. `dubu-core` takes
    // whole bps; sub-bp resolution would mean a second implementation of the skew here, which is
    // the one thing this crate must not grow. Saturating, because `raw_decibps` is already
    // saturated when the volatility estimate is absurd and `i32::MAX + 5` would wrap the sign.
    let rounded = if raw_decibps >= 0 {
        raw_decibps.saturating_add(5) / 10
    } else {
        raw_decibps.saturating_sub(5) / 10
    };

    let positive_cap = i32::from(params.max_positive_bps.min(floor_cap_bps));
    let negative_cap = -i32::from(params.max_negative_bps);
    let (applied, clamp) = if rounded > positive_cap {
        let by = if i32::from(floor_cap_bps) < i32::from(params.max_positive_bps) {
            Clamp::MinPriceFloor
        } else {
            Clamp::PositiveCap
        };
        (positive_cap, by)
    } else if rounded < negative_cap {
        (negative_cap, Clamp::NegativeCap)
    } else {
        (rounded, Clamp::Unbound)
    };

    Skew {
        applied_bps: i16::try_from(applied).unwrap_or(0),
        raw_decibps,
        imbalance_ppm: q,
        sigma_millibps,
        floor_cap_bps,
        clamp,
    }
}

#[cfg(test)]
mod tests {
    /// The regression this field exists for: a fully hedged pool must quote symmetrically.
    ///
    /// Before it, skew read the pool's balance alone. Hold 3,506 ETH against a 3,506 short and the
    /// net exposure is zero -- but the balance says "too much base", so the pool priced to sell what
    /// it had already sold. The hedge flattened, the skew rebuilt, and the two ran against each
    /// other indefinitely.
    #[test]
    fn a_fully_hedged_pool_has_no_imbalance_to_skew_against() {
        let unhedged = Inventory {
            base_value: 6_000_000,
            quote_share: 6_000_000,
            hedge_value: 0,
            target_ppm: 500_000,
        };
        assert_eq!(
            unhedged.imbalance_ppm(),
            0,
            "50/50 and flat is the baseline"
        );

        // A swap is an exchange: the pool receives 2,000,000 of base and pays the same in quote, so
        // the book is the size it was and only its composition moved.
        let taken = Inventory {
            base_value: 8_000_000,
            quote_share: 4_000_000,
            ..unhedged
        };
        assert_eq!(taken.imbalance_ppm(), 166_666, "long base, price it down");

        // Hedged, the same holding carries no exposure and the quote goes back to symmetric.
        let hedged = Inventory {
            hedge_value: -2_000_000,
            ..taken
        };
        assert_eq!(
            hedged.imbalance_ppm(),
            0,
            "the hedge cancels the holding, so the skew is what it was before the fill"
        );
    }

    /// Over-hedging is a net short, not an impossibility, and the skew has to price to unwind it.
    #[test]
    fn an_over_hedged_pool_skews_the_other_way() {
        let over = Inventory {
            base_value: 4_000_000,
            quote_share: 6_000_000,
            hedge_value: -6_000_000,
            target_ppm: 500_000,
        };
        assert!(
            over.imbalance_ppm() < -500_000,
            "net short past the target: {}",
            over.imbalance_ppm()
        );
    }

    /// The book is what the pool owns. A perp hedge changes the exposure and adds no capital, so it
    /// must not move the denominator -- doing so would shrink the imbalance of a pool that had
    /// hedged nothing away simply because it had hedged something.
    #[test]
    fn the_hedge_does_not_change_the_size_of_the_book() {
        let a = Inventory {
            base_value: 8_000_000,
            quote_share: 2_000_000,
            hedge_value: 0,
            target_ppm: 500_000,
        };
        let b = Inventory {
            hedge_value: -3_000_000,
            ..a
        };
        // base 8M of a 10M book is 800_000 ppm; net 5M of the same 10M book is 500_000.
        assert_eq!(a.imbalance_ppm(), 300_000);
        assert_eq!(
            b.imbalance_ppm(),
            0,
            "net 5M against a book that is still 10M"
        );
    }

    use super::*;
    use crate::ladder;
    use std::time::Duration;

    fn vol_cfg() -> VolConfig {
        VolConfig {
            tau_ms: 60_000,
            horizon_secs: 300,
            min_sample_ms: 100,
            max_sample_ms: 10_000,
        }
    }

    fn params() -> SkewParams {
        SkewParams {
            gamma_e2: 10_000,
            max_positive_bps: 30,
            max_negative_bps: 10,
        }
    }

    /// A pool holding `base_pct` of its book in base, against a 50% target.
    fn inv(base_pct: u128) -> Inventory {
        Inventory {
            base_value: base_pct * 1_000_000,
            quote_share: (100 - base_pct) * 1_000_000,
            hedge_value: 0,
            target_ppm: 500_000,
        }
    }

    // -----------------------------------------------------------------------
    // Volatility
    // -----------------------------------------------------------------------

    #[test]
    fn a_flat_market_has_zero_volatility_and_therefore_zero_skew() {
        let mut v = Volatility::new(vol_cfg());
        let mut t = Instant::now();
        for _ in 0..50 {
            v.observe(196_930_000_000, t);
            t += Duration::from_millis(1_000);
        }
        assert_eq!(v.sigma_sq_bps_e6(), 0);
        assert_eq!(v.sigma_millibps(), 0);
        let s = compute(
            &inv(90),
            v.sigma_sq_bps_e6(),
            v.sigma_millibps(),
            &params(),
            9_999,
        );
        assert_eq!(
            s.applied_bps, 0,
            "no volatility means no risk to skew against"
        );
        assert_ne!(
            s.imbalance_ppm, 0,
            "... and it is not because the imbalance is zero"
        );
    }

    #[test]
    fn the_estimate_converges_on_the_volatility_it_is_fed() {
        // A deterministic +-10 bp alternation at 1s. Per-second variance is (10bp)^2 = 100 bps^2,
        // so sigma over 300s should approach sqrt(100 * 300) = ~173 bps.
        let mut v = Volatility::new(vol_cfg());
        let mut t = Instant::now();
        let base = 100_000_000_000u128;
        let bp = base / 10_000;
        for i in 0..2_000 {
            v.observe(if i % 2 == 0 { base } else { base + 10 * bp }, t);
            t += Duration::from_millis(1_000);
        }
        let sigma_bps = v.sigma_millibps() / 1_000;
        assert!(
            (150..=200).contains(&sigma_bps),
            "expected ~173 bps over the 300s horizon, got {sigma_bps}"
        );
    }

    #[test]
    fn the_same_estimator_answers_for_a_single_observation_too() {
        // `jump` tests one observation, not a 300s holding period, so it asks the SAME estimator
        // for sigma over its own interval. Two estimators would be two ways to disagree about how
        // volatile the market is.
        let mut v = Volatility::new(vol_cfg());
        let mut t = Instant::now();
        let base = 100_000_000_000u128;
        let bp = base / 10_000;
        // A +-1 bp per second alternation: per-second sigma is 1 bp, so sigma(300s) is
        // sqrt(300) = 17.3 bp and sigma(1s) is 1 bp = 100 hundredths.
        for i in 0..2_000 {
            v.observe(if i % 2 == 0 { base } else { base + bp }, t);
            t += Duration::from_millis(1_000);
        }
        let one_sec = v.sigma_bps_e2_over_ms(1_000);
        assert!(
            (90..=110).contains(&one_sec),
            "expected ~100 hundredths of a bp, got {one_sec}"
        );
        assert!(
            (16_000..=19_000).contains(&v.sigma_millibps()),
            "sigma(300s) should be ~17.3 bp"
        );

        // Square-root-of-time between the two sampling rates the loop actually uses: a 200ms
        // fast-lane scan and a 1s cycle scan differ by exactly sqrt(5) = 2.236. That is what makes
        // the two interchangeable — the fast lane is not testing against a different threshold, it
        // is testing against the same one scaled to a shorter interval.
        let fast = v.sigma_bps_e2_over_ms(200);
        let scaled = fast * 2_236 / 1_000;
        assert!(
            scaled.abs_diff(one_sec) * 100 <= one_sec * 5,
            "sqrt-of-time broke: sigma(200ms) {fast} scales to {scaled}, sigma(1s) is {one_sec}"
        );

        // And it agrees with the horizon method the skew uses: `sigma(300s)^2 == 300 * sigma(1s)^2`,
        // which is what makes this one estimator rather than two that happen to be near each other.
        // `isqrt(sigma_sq_bps_e6)` is milli-bps, so `/10` puts it in hundredths of a bp.
        let from_horizon = isqrt(v.sigma_sq_bps_e6() / 300) / 10;
        assert!(
            from_horizon.abs_diff(u128::from(one_sec)) <= 2,
            "{from_horizon} vs {one_sec}"
        );
    }

    #[test]
    fn a_dead_market_reports_zero_for_a_single_observation_too() {
        let v = Volatility::new(vol_cfg());
        assert_eq!(v.sigma_bps_e2_over_ms(1_000), 0);
        assert_eq!(v.sigma_bps_e2_over_ms(0), 0);
    }

    #[test]
    fn the_horizon_scales_as_the_square_root_of_time() {
        let feed = |horizon| {
            let mut v = Volatility::new(VolConfig {
                horizon_secs: horizon,
                ..vol_cfg()
            });
            let mut t = Instant::now();
            let base = 100_000_000_000u128;
            for i in 0..1_000 {
                v.observe(
                    if i % 2 == 0 {
                        base
                    } else {
                        base + base / 1_000
                    },
                    t,
                );
                t += Duration::from_millis(1_000);
            }
            v.sigma_millibps()
        };
        // 4x the horizon is 2x the sigma, within integer rounding.
        let (a, b) = (feed(300), feed(1_200));
        assert!(
            b > 19 * a / 10 && b < 21 * a / 10,
            "sqrt-of-time broke: {a} -> {b}"
        );
    }

    #[test]
    fn an_outage_gap_re_anchors_instead_of_becoming_one_enormous_return() {
        // The failure this prevents: a two-minute feed outage across a 1% move entering the
        // estimator as a single one-second 1% return, which would size the skew off it for
        // minutes afterwards.
        let mut v = Volatility::new(vol_cfg());
        let t = Instant::now();
        v.observe(100_000_000_000, t);
        v.observe(101_000_000_000, t + Duration::from_secs(120));
        assert_eq!(v.sigma_sq_bps_e6(), 0, "a gap must contribute nothing");
        assert_eq!(v.samples(), 0);

        // ... and the next ordinary sample is measured from the post-gap anchor.
        v.observe(101_000_000_000, t + Duration::from_secs(121));
        assert_eq!(v.samples(), 1);
        assert_eq!(v.sigma_sq_bps_e6(), 0);
    }

    #[test]
    fn samples_too_close_together_are_skipped_without_losing_the_anchor() {
        let mut v = Volatility::new(vol_cfg());
        let t = Instant::now();
        v.observe(100_000_000_000, t);
        v.observe(100_500_000_000, t + Duration::from_millis(10));
        assert_eq!(v.samples(), 0, "10ms is not a per-second return");
        // The anchor is still the original price, so the next real sample measures the whole move.
        v.observe(100_500_000_000, t + Duration::from_millis(1_000));
        assert_eq!(v.samples(), 1);
        assert!(v.sigma_sq_bps_e6() > 0);
    }

    #[test]
    fn reset_drops_the_anchor_so_a_recovered_feed_starts_clean() {
        let mut v = Volatility::new(vol_cfg());
        let t = Instant::now();
        v.observe(100_000_000_000, t);
        v.reset();
        v.observe(200_000_000_000, t + Duration::from_secs(1));
        assert_eq!(
            v.samples(),
            0,
            "the first post-reset observation is an anchor, not a return"
        );
    }

    // -----------------------------------------------------------------------
    // The skew itself
    // -----------------------------------------------------------------------

    #[test]
    fn a_long_book_skews_down_and_a_short_book_skews_up() {
        // sigma = 30 bps over the horizon: sigma^2 = 900, scaled by 1e6.
        let sig = 900 * 1_000_000;
        let long = compute(&inv(70), sig, 30_000, &params(), 9_999);
        let short = compute(&inv(30), sig, 30_000, &params(), 9_999);
        assert!(
            long.imbalance_ppm > 0 && long.applied_bps > 0,
            "long must push the book DOWN"
        );
        assert!(
            short.imbalance_ppm < 0 && short.applied_bps < 0,
            "short must push the book UP"
        );
        assert_eq!(
            long.applied_bps, -short.applied_bps,
            "the model itself is symmetric"
        );

        // At target, no skew at all.
        let flat = compute(&inv(50), sig, 30_000, &params(), 9_999);
        assert_eq!(flat.imbalance_ppm, 0);
        assert_eq!(flat.applied_bps, 0);
        assert_eq!(flat.clamp, Clamp::Unbound);
    }

    #[test]
    fn the_documented_worked_example_comes_out_where_the_docs_say() {
        // 20% away from target, sigma 10 bps (the live ETHUSDT measurement), gamma 1000 -> 2 bp.
        // This is the number the module docs quote to justify gamma's range, so it is pinned.
        let i = Inventory {
            base_value: 700_000,
            quote_share: 300_000,
            hedge_value: 0,
            target_ppm: 500_000,
        };
        assert_eq!(i.imbalance_ppm(), 200_000);
        let live = SkewParams {
            gamma_e2: 100_000,
            ..params()
        };
        let s = compute(&i, 100 * 1_000_000, 10_000, &live, 9_999);
        assert_eq!(s.raw_decibps, 20, "2.0 bp");
        assert_eq!(s.applied_bps, 2);

        // ... and the live book's own state, which the dry-run log shows: 11.2% off target at
        // sigma 9 bps gives just under a bp, so the feature is visibly on rather than rounding to
        // nothing. This is the sample that says gamma is in the right decade.
        let live_book = Inventory {
            base_value: 611_617,
            quote_share: 388_383,
            hedge_value: 0,
            target_ppm: 500_000,
        };
        assert_eq!(live_book.imbalance_ppm(), 111_617);
        let s = compute(&live_book, 81 * 1_000_000, 9_000, &live, 9_999);
        assert_eq!(s.raw_decibps, 9);
        assert_eq!(s.applied_bps, 1, "a live, non-zero, un-clamped skew");
        assert_eq!(s.clamp, Clamp::Unbound);
    }

    #[test]
    fn the_coefficient_scales_with_sigma_squared_and_not_with_a_constant() {
        // The entire reason for reaching for A-S. Doubling volatility must quadruple the skew,
        // not double it — that is what a hand-rolled `kappa * q` gets wrong.
        let i = inv(70);
        let calm = compute(
            &i,
            900 * 1_000_000,
            30_000,
            &SkewParams {
                max_positive_bps: 9_999,
                ..params()
            },
            9_999,
        );
        let fast = compute(
            &i,
            3_600 * 1_000_000,
            60_000,
            &SkewParams {
                max_positive_bps: 9_999,
                ..params()
            },
            9_999,
        );
        assert_eq!(
            fast.raw_decibps,
            4 * calm.raw_decibps,
            "2x sigma must be 4x skew"
        );
    }

    #[test]
    fn the_negative_clamp_is_tighter_than_the_positive_one() {
        // A bid lifted above fair value is a free option; a lowered ask is not. The asymmetry is
        // the point, so it is pinned rather than left to the config file.
        let huge = 10_000 * 1_000_000; // sigma = 100 bps
        let long = compute(&inv(100), huge, 100_000, &params(), 9_999);
        let short = compute(&inv(0), huge, 100_000, &params(), 9_999);
        assert_eq!(long.applied_bps, 30);
        assert_eq!(long.clamp, Clamp::PositiveCap);
        assert_eq!(short.applied_bps, -10);
        assert_eq!(short.clamp, Clamp::NegativeCap);
        assert!(
            short.applied_bps.abs() < long.applied_bps,
            "the book-lifting cap must be tighter"
        );
    }

    #[test]
    fn a_wild_sigma_print_cannot_invert_the_strategy() {
        // sigma^2 is quadratic, so a 10x volatility error is a 100x skew error. The cap is what
        // stops that becoming a 300 bp quote before anyone reads a log line.
        let s = compute(&inv(90), u128::MAX / 2, u64::MAX, &params(), 9_999);
        assert_eq!(s.applied_bps, 30);
        assert!(s.clamp.bound());
    }

    #[test]
    fn the_min_price_floor_clamps_before_the_row_is_refused() {
        // pairId 1 as deployed: minPrice 1e15 (= $1000 a coin), half-spread 5 bp. At a fair value
        // of $1010 there is only ~94 bp of room before the bid target breaches the floor.
        let fair = 1_010_000_000_000_000u128;
        let min_price = 1_000_000_000_000_000u128;
        let cap = min_price_cap_bps(fair, min_price, 5);
        assert!(
            (90..=99).contains(&cap),
            "expected ~94 bp of headroom, got {cap}"
        );

        // A skew the model wants to push past it is held at the floor and says so. sigma of
        // 200 bp over the horizon on a fully-long book wants 200 bp of skew; the floor allows 94.
        let s = compute(
            &inv(100),
            40_000 * 1_000_000,
            200_000,
            &SkewParams {
                max_positive_bps: 9_999,
                ..params()
            },
            cap,
        );
        assert_eq!(s.raw_decibps, 2_000, "the model wants 200 bp");
        assert_eq!(s.applied_bps, i16::try_from(cap).unwrap());
        assert_eq!(s.clamp, Clamp::MinPriceFloor);
        assert_eq!(s.floor_cap_bps, cap);

        // Verify against `dubu-core` rather than against the derivation: the clamped skew really
        // does still build a row, and one bp more really would not.
        let row = |skew| {
            ladder::build(&crate::ladder::RowInputs {
                pair_id: 1,
                fair,
                half_spread_bps: 5,
                width_bps: 25,
                skew_bps: skew,
                capture: 20_000_000_000_000_000_000,
                bid_capacity: 1_000_000_000_000_000_000_000,
                ask_capacity: 1_000_000_000_000_000_000_000,
                min_price,
                price_scale_exp: 24,
            })
        };
        assert!(
            row(s.applied_bps).is_ok(),
            "the clamped skew must still produce a row"
        );
        assert!(
            row(s.applied_bps + 1).is_err(),
            "and one bp more must not — the clamp is tight"
        );
    }

    #[test]
    fn a_fair_value_at_the_floor_leaves_no_room_to_skew_down_at_all() {
        let min_price = 1_000_000_000_000_000u128;
        assert_eq!(min_price_cap_bps(min_price, min_price, 5), 0);
        assert_eq!(min_price_cap_bps(min_price / 2, min_price, 5), 0);
        assert_eq!(min_price_cap_bps(0, min_price, 5), 0);

        // Far above the floor — $1943 against a $1000 floor — the floor allows roughly half the
        // price in skew, so it is nowhere near the binding constraint at the configured 30 bp cap.
        let cap = min_price_cap_bps(1_943_820_000_000_000, min_price, 5);
        assert_eq!(cap, 4_852);
        assert!(u128::from(cap) < MAX_BPS);
        assert!(
            cap > params().max_positive_bps * 100,
            "the configured cap binds first"
        );
    }

    #[test]
    fn the_clamp_reason_distinguishes_the_configured_cap_from_the_floor() {
        let huge = 10_000 * 1_000_000;
        // Floor is looser than the configured cap: the cap is what bound.
        let s = compute(&inv(100), huge, 100_000, &params(), 9_999);
        assert_eq!(s.clamp, Clamp::PositiveCap);
        // Floor is tighter: the floor is what bound, and the operator needs to see which.
        let s = compute(&inv(100), huge, 100_000, &params(), 7);
        assert_eq!(s.clamp, Clamp::MinPriceFloor);
        assert_eq!(s.applied_bps, 7);
    }

    #[test]
    fn an_empty_book_has_no_imbalance_rather_than_a_division_by_zero() {
        let i = Inventory {
            base_value: 0,
            quote_share: 0,
            hedge_value: 0,
            target_ppm: 500_000,
        };
        assert_eq!(i.imbalance_ppm(), 0);
        assert_eq!(
            compute(&i, 900 * 1_000_000, 30_000, &params(), 9_999).applied_bps,
            0
        );
    }

    #[test]
    fn the_target_is_configuration_and_moving_it_moves_the_imbalance() {
        // The knob is a share of the book, so the same balances read differently against
        // different targets — which is the whole point of it not being a constant.
        let holdings = |target| Inventory {
            base_value: 600,
            quote_share: 400,
            hedge_value: 0,
            target_ppm: target,
        };
        assert_eq!(holdings(500_000).imbalance_ppm(), 100_000);
        assert_eq!(holdings(600_000).imbalance_ppm(), 0);
        assert_eq!(holdings(800_000).imbalance_ppm(), -200_000);
    }

    #[test]
    fn no_skew_in_range_can_cross_the_book() {
        // The first thing to worry about, and it is structurally impossible: the skew moves the
        // mid, and both targets hang off the skewed mid with the same half-spread, so the spread
        // between them is preserved exactly. Checked across the whole legal range against
        // `dubu-core`'s own builder rather than by argument.
        use dubu_core::ladder::LadderBuilder;
        let fair = 1_943_820_000_000_000u128;
        for skew in [-9_999i16, -1_000, -30, -1, 0, 1, 30, 1_000, 9_999] {
            for hs in [1u16, 5, 8, 100] {
                let b = LadderBuilder {
                    skew_bps: skew,
                    half_spread_bps: hs,
                    ..LadderBuilder::new(fair)
                };
                let mid = b.skewed_mid().unwrap();
                let bid = mul_div_floor(mid, BPS - u128::from(hs), BPS).unwrap();
                let ask = mul_div_ceil(mid, BPS + u128::from(hs), BPS).unwrap();
                assert!(
                    bid < ask,
                    "skew {skew} / hs {hs} crossed the book: {bid} >= {ask}"
                );
                let l = b.build().unwrap();
                l.validate(0)
                    .expect("every skew in range must still validate on chain");
            }
        }
    }
}
