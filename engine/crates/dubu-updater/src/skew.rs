//! Inventory skew: the Avellaneda–Stoikov reservation price, and the volatility estimate it needs.
//!
//! ```text
//! r = s - q * gamma * sigma^2
//! ```
//!
//! `s` is the reference price, `q` the signed inventory imbalance, `gamma` the risk aversion,
//! `sigma` the volatility over a stated horizon. Centring the book on `r` rather than on `s` means
//! a long pool quotes lower on **both** sides and gets hit on its ask sooner than on its bid.
//!
//! A–S rather than a hand-rolled `kappa * q` because the coefficient scales with `sigma^2` rather
//! than with a constant: the same imbalance is far more dangerous in a fast market, and a fixed
//! `kappa` is too timid when volatility triples and too aggressive when it collapses. The
//! derivative and integral terms are absent because three coefficients can only be tuned against
//! replay data that does not exist here (archi_v2 §5.4).
//!
//! # Units, so that gamma is a number a human can reason about
//!
//! ```text
//! q      = net exposure as a share of the book         in [-1, 1], carried as ppm
//! sigma  = relative volatility over `horizon_secs`     carried as bps, and squared as bps^2
//! skew   = gamma * q * sigma^2 / 10_000                in bps of the reference
//! ```
//!
//! The `/ 10_000` converts `sigma^2` from bps squared back into bps. ETHUSDT's measured
//! `sigma(300s)` is ~10 bps, so a pool 20% off target at `gamma = 1000` skews 2 bp against a 5 bp
//! half-spread — the range `gamma` is meant to live in. Back-solve `gamma` from the `skew` log
//! line rather than picking it: the skew is quadratic in `sigma`, so a first guess is easily wrong
//! by a factor of nine.
//!
//! The EWMA is seeded at zero, so `sigma` and the skew with it climb from nothing over the first
//! few `tau` after a start or a feed outage. That is the conservative direction, and `vol_samples`
//! is logged so the warm-up is distinguishable from a dead feature.
//!
//! # The sign
//!
//! `q > 0` means the pool holds more base than it wants, so it wants to sell, so the book must
//! move **down**. `dubu-core`'s convention is that a positive `skew_bps` shifts the book down, so
//! the signs line up and **no negation is needed anywhere**.
//!
//! The skew cannot cross the book: it moves the mid, and both targets hang off the skewed mid with
//! the same half-spread, so the spread between them is preserved exactly. The pair's `minPrice` is
//! the real constraint, and [`price_cap_bps_min`] handles it here rather than leaving it to be
//! discovered downstream — a large positive skew pushes the bid target under the floor,
//! `ladder::build` then refuses the row, and a refused row is a quoting outage where a
//! slightly-less-skewed book is not.

use std::time::Instant;

use dubu_core::ladder::{BPS, BPS_MAX};
use dubu_core::math::{mul_div_ceil, mul_div_floor};

/// Relative returns are carried as parts per `10^8`, matching [`crate::units::FEED_SCALE`].
const REL: u128 = 100_000_000;

/// The two are the same scale by construction, not by coincidence: a return is a ratio of two feed
/// prices, so carrying it at any other precision would either truncate the ratio or invent digits.
const _: () = assert!(REL == 10u128.pow(crate::units::FEED_SCALE as u32));

/// [`price_cap_bps_min`] divides by `BPS - hs` with `hs` clamped to `BPS_MAX`, so a `BPS_MAX` that
/// reached `BPS` would be a division by zero on the path that keeps the pool quoting.
const _: () = assert!(BPS_MAX < BPS);

// --- Volatility ---

/// How the volatility estimator is parameterised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VolConfig {
    /// EWMA time constant, in milliseconds. The weight on a new sample is `dt / tau`, so the
    /// **half-life** is `tau * ln2`, about 0.69 of this.
    pub tau_ms: u64,
    /// The horizon the reported volatility is scaled to, in seconds. See [`Volatility`].
    pub horizon_secs: u64,
    /// Samples closer together than this are skipped rather than divided by a tiny `dt`.
    pub sample_ms_min: u64,
    /// A gap longer than this is not a return, it is an outage. The estimator re-anchors and
    /// contributes nothing.
    pub sample_ms_max: u64,
}

/// EWMA of squared relative returns, kept as a **per-second variance** and scaled to a horizon.
///
/// The horizon defaults to 300 s, matching `risk.bleed_window_secs`: that switch already defines
/// the window over which an adverse move against inventory is a loss rather than noise, and the
/// skew exists to push inventory back before the switch has anything to measure. Any other value
/// is two parts of one risk system disagreeing about "soon".
///
/// Each sample contributes `r^2 / dt` to a per-second variance, so jitter in the ~1 Hz cycle
/// cadence does not bias the level, and scaling to the horizon is square-root-of-time:
/// `sigma(T)^2 = var_per_sec * T`. The EWMA weight is `w = 1 - exp(-dt/tau)` taken to first order
/// as `dt/tau` — under 1% off at the default `tau` of 60 s, and it keeps the update in integers.
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
    ///
    /// # Panics
    ///
    /// If the configuration can never produce a non-zero `sigma`. Each case would leave the skew
    /// at zero AND the half-spread at `s0`, which is a silently under-priced quote rather than a
    /// visible failure. [`crate::config`] refuses all three at startup; this is the second belt.
    #[must_use]
    pub const fn new(cfg: VolConfig) -> Self {
        // A zero tau makes the EWMA weight zero, so the variance never leaves its seed.
        assert!(cfg.tau_ms > 0, "vol tau of zero freezes the estimator");
        // A zero horizon makes `sigma_sq_bps_e6` zero whatever the variance is.
        assert!(cfg.horizon_secs > 0, "vol horizon of zero erases sigma");
        // Inverted bounds mean every sample is either skipped or read as an outage.
        assert!(
            cfg.sample_ms_min < cfg.sample_ms_max,
            "vol sample bounds are inverted; no observation could ever be folded in"
        );
        Self {
            cfg,
            var_per_sec: 0,
            last: None,
            samples: 0,
        }
    }

    /// How many returns have been folded in. Logged, because below a few `tau` the estimate is
    /// still warming up.
    #[must_use]
    pub const fn samples(&self) -> u64 {
        self.samples
    }

    /// Forget the anchor, keeping the variance. Called when the reference is unavailable, so the
    /// first sample after an outage is not a "return" spanning the whole gap: a two-minute outage
    /// across a 1% move would otherwise enter as one enormous one-second return.
    pub fn reset(&mut self) {
        let var_before = self.var_per_sec;
        self.last = None;
        assert!(self.last.is_none(), "the anchor must be gone");
        // Dropping the variance too would restart the warm-up on every brief feed hiccup.
        assert_eq!(self.var_per_sec, var_before, "reset must keep the variance");
    }

    /// Fold in one observation of the reference price.
    pub fn observe(&mut self, price: u128, now: Instant) {
        let Some((prev, t0)) = self.last else {
            self.last = Some((price, now));
            return;
        };
        let samples_before = self.samples;
        let dt_ms =
            u64::try_from(now.saturating_duration_since(t0).as_millis()).unwrap_or(u64::MAX);
        if dt_ms < self.cfg.sample_ms_min {
            // Too close together to divide by. The anchor is kept so the next sample spans a
            // sensible interval rather than starting over.
            assert!(self.last.is_some(), "a skipped sample must keep its anchor");
            return;
        }
        if dt_ms > self.cfg.sample_ms_max || prev == 0 {
            self.last = Some((price, now));
            assert_eq!(self.samples, samples_before, "an outage is not a return");
            return;
        }
        // Both guards are behind us, so the divisors below are real.
        assert!(prev > 0, "a zero anchor is an outage, not a return");
        assert!(dt_ms >= self.cfg.sample_ms_min, "dt below the sample floor");
        assert!(dt_ms > 0, "a zero interval would divide the contribution");

        // |r| in parts per REL. The sign is irrelevant: it is squared immediately.
        let r = mul_div_floor(price.abs_diff(prev), REL, prev).unwrap_or(REL);
        let contrib = r
            .checked_mul(r)
            .and_then(|sq| sq.checked_mul(1_000))
            .map_or(u128::MAX, |x| x / u128::from(dt_ms));
        if price == prev {
            assert_eq!(contrib, 0, "an unmoved reference has no variance in it");
        }

        let w_num = u128::from(dt_ms.min(self.cfg.tau_ms));
        let w_den = u128::from(self.cfg.tau_ms.max(1));
        // A weight above one overshoots rather than approaching, turning the EWMA into an
        // oscillator that prices every quote off the swing.
        assert!(w_num <= w_den, "EWMA weight above 1: {w_num}/{w_den}");
        assert!(w_den > 0, "EWMA weight divides by zero");
        self.var_per_sec = if contrib >= self.var_per_sec {
            let step = mul_div_floor(contrib - self.var_per_sec, w_num, w_den).unwrap_or(0);
            let out = self.var_per_sec.saturating_add(step);
            // An EWMA step never overshoots the sample it is moving toward.
            assert!(out <= contrib, "the estimate stepped past the observation");
            out
        } else {
            let step = mul_div_floor(self.var_per_sec - contrib, w_num, w_den).unwrap_or(0);
            let out = self.var_per_sec.saturating_sub(step);
            assert!(out >= contrib, "the estimate stepped past the observation");
            out
        };

        self.last = Some((price, now));
        self.samples = self.samples.saturating_add(1);
        assert!(self.samples > samples_before, "a folded return must count");
    }

    /// `sigma^2` over the configured horizon, in **bps squared scaled by `10^6`**. Deliberately
    /// the number with no square root in it: A–S needs `sigma^2`, and taking a root here only to
    /// square it again loses precision. [`Volatility::sigma_millibps`] exists for the log line.
    #[must_use]
    pub const fn sigma_sq_bps_e6(&self) -> u128 {
        // sigma_rel^2 over the horizon is `var_per_sec * horizon` in (1/REL)^2 units. In bps^2
        // that is `* REL^2 / 10^8`, i.e. `* 10^-8`; scaled by 10^6 it is `/ 100`.
        let out = (self
            .var_per_sec
            .saturating_mul(self.cfg.horizon_secs as u128))
            / 100;
        // The warm-up contract, both ways.
        if self.var_per_sec == 0 {
            assert!(out == 0, "variance of zero cannot produce a sigma");
        }
        if out > 0 {
            assert!(
                self.var_per_sec > 0,
                "sigma out of a market that never moved"
            );
        }
        out
    }

    /// `sigma` over the configured horizon, in thousandths of a basis point. Milli-bps because a
    /// quiet five-minute window on ETHUSDT is single-digit bps, which integer bps rounds away.
    #[must_use]
    pub fn sigma_millibps(&self) -> u64 {
        let sq = self.sigma_sq_bps_e6();
        let out = u64::try_from(isqrt(sq)).unwrap_or(u64::MAX);
        if sq == 0 {
            assert_eq!(out, 0, "a flat market reports a flat sigma");
        }
        if out > 0 {
            assert!(sq > 0, "a sigma out of a zero variance");
        }
        out
    }

    /// `sigma` over an **arbitrary** interval, in hundredths of a basis point.
    ///
    /// [`crate::jump`] tests a single observation rather than a holding period and needs the same
    /// estimate scaled to that observation's interval; a second, faster estimator would be two
    /// ways to be wrong about how volatile the market is. Square-root-of-time scaling, so a 200 ms
    /// fast-lane scan and a 1 s cycle scan differ by exactly `sqrt(5)` and are interchangeable.
    ///
    /// ```text
    /// sigma_bps(dt)^2 = var_per_sec * dt_ms / (1000 * 10^8)
    /// in hundredths of a bp, that is  * 10^4, i.e.  var_per_sec * dt_ms / 10^7
    /// ```
    #[must_use]
    pub fn sigma_bps_e2_over_ms(&self, dt_ms: u64) -> u32 {
        let v = self.var_per_sec.saturating_mul(u128::from(dt_ms)) / 10_000_000;
        let out = u32::try_from(isqrt(v)).unwrap_or(u32::MAX);
        // Both reach `jump`'s threshold clamp, where a wrong sigma is a missed jump.
        if dt_ms == 0 {
            assert_eq!(out, 0, "no interval, no move to be surprised by");
        }
        if self.var_per_sec == 0 {
            assert_eq!(out, 0, "a dead market is flat over every interval");
        }
        if out > 0 {
            assert!(dt_ms > 0, "a move over no time at all");
            assert!(self.var_per_sec > 0, "a move in a market that never moved");
        }
        out
    }
}

/// Integer square root, by Newton's method.
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
    // Both halves of the definition. Newton's method converges from above, so an off-by-one here
    // would bias every reported sigma in the same direction rather than cancelling out.
    assert!(x <= n, "isqrt above its own input: {x} for {n}");
    assert!(x * x <= n, "isqrt overshot: {x}^2 > {n}");
    // `(x+1)^2` past `u128` is past `n` too, so the upper half holds trivially there.
    if let Some(next_sq) = (x + 1).checked_mul(x + 1) {
        assert!(next_sq > n, "isqrt undershot: ({x}+1)^2 <= {n}");
    }
    x
}

// --- Inventory ---

/// One pair's inventory position, in quote units. The book is a *share*, so `q` stays meaningful
/// as the pool grows or shrinks:
///
/// ```text
/// book_i = value(base_i) + quote_balance / pairs
/// ```
///
/// The even split of the shared quote token is a simplification: every pair draws its bids from
/// the same mUSDC and nothing yet caps the sum of their bid liabilities against it (archi_v2
/// §5.4's cross-asset clamp, not built). Until it exists, one pair's `q` is not independent of
/// another's inventory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Inventory {
    /// Base holdings valued at the reference, in quote units.
    pub base_value: u128,
    /// The hedge position, valued at the same reference and **signed**: negative when the venue is
    /// short, which is what a hedge against inventory looks like.
    ///
    /// Netted into `q` so the skew and the hedge do not fight: hold 3,506 ETH against a 3,506
    /// short and net exposure is zero, but the balance alone says "too much base, price it down",
    /// so the hedge flattens and the skew immediately prices to rebuild.
    ///
    /// Includes what is in flight: a hedge sent but not filled has already committed the exposure,
    /// and counting only settled fills double-counts it until the venue answers.
    pub hedge_value: i128,
    /// This pair's share of the shared quote balance, in quote units.
    pub quote_share: u128,
}

impl Inventory {
    /// The signed imbalance `q`, in parts per million: net exposure as a share of the book.
    /// `+1_000_000` is "the whole book is base and none of it is hedged"; negative is a net short.
    /// A zero book yields zero rather than dividing by zero.
    ///
    /// **The target is zero, and must not become a base-share target again.** The numerator is a
    /// RISK quantity — what the pool loses if the price moves, measured after the hedge — while a
    /// `target_base_share_pct` is a FUNDING quantity that a perp short supplies none of. Both are
    /// shares of the book, so subtracting one from the other compiles and the tests pass, but the
    /// better the hedge the further the numerator falls against a target stuck at 50%, until a
    /// fully hedged pair reads as maximally SHORT and the skew lifts the book to buy base it
    /// already holds. Funding constrains capacity, never the price.
    #[must_use]
    pub fn imbalance_ppm(&self) -> i64 {
        // The book is what the pool OWNS: a perp hedge changes the exposure and not the balance
        // sheet, so it belongs in the numerator and nowhere else.
        let book = self.base_value.saturating_add(self.quote_share);
        if book == 0 {
            return 0;
        }
        assert!(book > 0, "an empty book returns above, never divides");
        let net = i128::try_from(self.base_value)
            .unwrap_or(i128::MAX)
            .saturating_add(self.hedge_value);
        if self.hedge_value == 0 {
            assert!(net >= 0, "an unhedged pool cannot be short: {net}");
        }
        // A hedge larger than the holding leaves a net short: over-hedged rather than impossible,
        // and the skew should price to unwind it, so the signed path is kept rather than clamped.
        let book_i = i128::try_from(book).unwrap_or(i128::MAX).max(1);
        assert!(book_i > 0, "a non-positive book would flip every sign");
        let q = i64::try_from(net.saturating_mul(1_000_000) / book_i).unwrap_or(1_000_000);
        // The sign convention the whole module hangs on: `q > 0` prices the book DOWN. Backwards,
        // it quotes to acquire the exposure the pool is trying to shed.
        if net > 0 {
            assert!(q >= 0, "a net long read as short: net {net} -> q {q}");
        }
        if net < 0 {
            assert!(q <= 0, "a net short read as long: net {net} -> q {q}");
        }
        if net == 0 {
            assert_eq!(q, 0, "a flat position has nothing to skew against");
        }
        q
    }
}

// --- The skew ---

/// The clamps on the skew, and the asymmetry between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SkewParams {
    /// Risk aversion `gamma`, scaled by 100 so the config can say `100.5`.
    pub gamma_e2: u64,
    /// Cap on a **positive** skew, in bps. Positive moves the book down: the pool is long and
    /// wants to sell.
    pub positive_bps_max: u16,
    /// Cap on a **negative** skew, as a magnitude in bps. Negative moves the book up: the pool is
    /// short and wants to buy.
    pub negative_bps_max: u16,
}

/// Which bound, if any, held the skew back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Clamp {
    /// The model's own number was used unchanged.
    Unbound,
    /// `positive_bps_max` bound it.
    PositiveCap,
    /// `negative_bps_max` bound it.
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
        let out = !matches!(self, Self::Unbound);
        // Asserted both ways: the log line reads `clamp` and `bound` as one claim.
        if matches!(self, Self::Unbound) {
            assert!(!out, "the unbound case is the one where nothing bit");
        } else {
            assert!(out, "every other case is a bound that bit");
        }
        out
    }
}

/// Everything one skew computation produced. All of it is logged every row:
/// `(imbalance_ppm, sigma_millibps) -> applied_bps` is the sample a back-solve for `gamma` needs,
/// and `raw_decibps` beside `applied_bps` says whether the clamp or the model has been deciding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Skew {
    /// What goes into `dubu-core`. Positive shifts the book down.
    pub applied_bps: i16,
    /// The model's number before clamping and rounding, in deci-bps. The one to tune against,
    /// because whole-bps rounding hides everything under 0.5 bp.
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
/// A positive skew moves the whole book down and the bid target is the lowest thing derived from
/// the mid, so it hits the floor first, after which `ladder::build` refuses the row and the pool
/// quotes nothing. Clamping here trades a little skew for staying in the market.
///
/// Derivation: `ladder::build` computes `mid = floor(fair * (BPS - skew) / BPS)` then
/// `bid = floor(mid * (BPS - hs) / BPS)`, and needs `bid >= min_price`. It is sufficient that
/// `mid >= ceil(min_price * BPS / (BPS - hs)) + 1`, the `+ 1` absorbing the floor in the second
/// step; solving for the skew and rounding conservatively gives the value returned here.
#[must_use]
pub fn price_cap_bps_min(fair: u128, min_price: u128, half_spread_bps: u16) -> u16 {
    let hs = u128::from(half_spread_bps).min(BPS_MAX);
    assert!(hs <= BPS_MAX, "half-spread outside the ladder's domain");
    assert!(BPS > hs, "the divisor below would be zero or wrap");
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
    assert!(
        fair > mid_min,
        "a fair value at the floor has no room to skew"
    );
    // skew <= BPS - ceil(mid_min * BPS / fair)
    let Some(needed) = mul_div_ceil(mid_min, BPS, fair) else {
        return 0;
    };
    let cap = BPS.saturating_sub(needed);
    let out = u16::try_from(cap.min(BPS_MAX)).unwrap_or(0);
    // Past `BPS_MAX` the skew inverts the price rather than merely flooring it, and
    // `ladder::build` refuses the row this exists to keep quoting.
    assert!(
        u128::from(out) <= BPS_MAX,
        "floor cap outside the domain: {out}"
    );
    out
}

/// Compute the Avellaneda–Stoikov linear skew for one row.
///
/// `positive_bps_max` is deliberately the looser of the two caps. A positive skew moves the book
/// down: both sides fall and the cheaper ask works the position off, so neither side is a gift to
/// a taker and the binding constraint is structural — the `minPrice` floor, which is why
/// [`price_cap_bps_min`] is folded in here rather than left to downstream refusal. A negative skew
/// moves the book up, lifting the **bid** toward and past the reference, which is a free option
/// written to whoever notices first (the direction `policy`'s `adverse_drift_bps` asymmetry also
/// defends). Nothing structural stops that, so it is capped tighter: a slow recovery costs volume
/// and a picked-off bid costs money.
///
/// Both caps also stop a single wild `sigma` print from inverting the strategy, since `sigma^2` is
/// quadratic and a 10x volatility error is a 100x skew error.
#[must_use]
pub fn compute(
    inventory: &Inventory,
    sigma_sq_bps_e6: u128,
    sigma_millibps: u64,
    params: &SkewParams,
    floor_cap_bps: u16,
) -> Skew {
    let q = inventory.imbalance_ppm();
    let raw_decibps = compute_raw_decibps(q, sigma_sq_bps_e6, params);

    let rounded = compute_rounded_bps(raw_decibps);

    let positive_cap = i32::from(params.positive_bps_max.min(floor_cap_bps));
    let negative_cap = -i32::from(params.negative_bps_max);
    assert!(positive_cap >= 0, "the book-lowering cap cannot lift it");
    assert!(negative_cap <= 0, "the book-lifting cap cannot lower it");
    let (applied, clamp) = if rounded > positive_cap {
        let by = if i32::from(floor_cap_bps) < i32::from(params.positive_bps_max) {
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
    assert!(applied <= positive_cap, "skew past its own positive cap");
    assert!(applied >= negative_cap, "skew past its own negative cap");
    // `clamp` is what tells the model's decision from the cap's, so it is asserted both ways.
    if clamp == Clamp::Unbound {
        assert_eq!(
            applied, rounded,
            "an unbound skew is the model's own number"
        );
    } else {
        assert_ne!(
            applied, rounded,
            "a bound that changed nothing is not a bound"
        );
    }

    Skew {
        applied_bps: i16::try_from(applied).unwrap_or(0),
        raw_decibps,
        imbalance_ppm: q,
        sigma_millibps,
        floor_cap_bps,
        clamp,
    }
}

/// Deci-bps to whole bps, rounding half away from zero so a 0.5 bp skew becomes 1 bp rather than
/// 0; `dubu-core` takes whole bps. Saturating, because `raw_decibps` is already saturated when the
/// volatility estimate is absurd and `i32::MAX + 5` would wrap the sign.
fn compute_rounded_bps(raw_decibps: i32) -> i32 {
    let out = if raw_decibps >= 0 {
        raw_decibps.saturating_add(5) / 10
    } else {
        raw_decibps.saturating_sub(5) / 10
    };
    if raw_decibps > 0 {
        assert!(out >= 0, "rounding flipped a long book short: {out}");
    }
    if raw_decibps < 0 {
        assert!(out <= 0, "rounding flipped a short book long: {out}");
    }
    out
}

/// The model's own number, in deci-bps, before any clamp: `10 * gamma * q * sigma^2 / 10_000`.
///
/// In two steps so no intermediate is larger than it has to be; `step` is `gamma * sigma^2 * 1000`.
fn compute_raw_decibps(q: i64, sigma_sq_bps_e6: u128, params: &SkewParams) -> i32 {
    let step =
        mul_div_floor(sigma_sq_bps_e6, u128::from(params.gamma_e2), 100_000).unwrap_or(u128::MAX);
    let magnitude = mul_div_floor(step, u128::from(q.unsigned_abs()), 1_000_000_000_000)
        .unwrap_or(u128::from(u32::MAX));
    let magnitude = i64::try_from(magnitude).unwrap_or(i64::from(i32::MAX));
    assert!(magnitude >= 0, "a magnitude carries no sign: {magnitude}");
    let raw_decibps_i64 = if q < 0 { -magnitude } else { magnitude };
    let out = i32::try_from(raw_decibps_i64).unwrap_or(if q < 0 { i32::MIN } else { i32::MAX });
    // The sign comes from the inventory and nothing else: a skew disagreeing with `q` quotes to
    // acquire exactly the exposure the pool is trying to shed.
    if q > 0 {
        assert!(out >= 0, "a long book skewed the book up: q {q} -> {out}");
    }
    if q < 0 {
        assert!(
            out <= 0,
            "a short book skewed the book down: q {q} -> {out}"
        );
    }
    if q == 0 {
        assert_eq!(out, 0, "a flat book has nothing to skew against");
    }
    if sigma_sq_bps_e6 == 0 {
        assert_eq!(out, 0, "no volatility means no risk to skew against");
    }
    out
}

#[cfg(test)]
mod tests {
    /// A fully hedged pool has no exposure and must quote symmetrically. Reading the balance
    /// prices to sell what is already sold; reading net exposure against a 50% base-share target
    /// reads a fully hedged pair as maximally short. Neither mistake fails to compile.
    #[test]
    fn a_fully_hedged_pool_has_no_imbalance_to_skew_against() {
        let flat = Inventory {
            base_value: 6_000_000,
            quote_share: 6_000_000,
            hedge_value: -6_000_000,
        };
        assert_eq!(
            flat.imbalance_ppm(),
            0,
            "every unit of base is hedged; nothing to price against"
        );

        // The same pool with no hedge is long its entire base holding, and says so.
        let unhedged = Inventory {
            hedge_value: 0,
            ..flat
        };
        assert_eq!(
            unhedged.imbalance_ppm(),
            500_000,
            "half the book is base and none of it is hedged"
        );
    }

    /// No funding target on this path: a 50/50 pool with no hedge is NOT flat.
    #[test]
    fn a_balanced_balance_sheet_is_not_a_flat_position() {
        let half = Inventory {
            base_value: 5_000_000,
            quote_share: 5_000_000,
            hedge_value: 0,
        };
        assert_eq!(
            half.imbalance_ppm(),
            500_000,
            "50% of the book in base is 50% of the book exposed, whatever the funding target says"
        );
    }

    #[test]
    fn an_over_hedged_pool_skews_the_other_way() {
        let over = Inventory {
            base_value: 4_000_000,
            quote_share: 6_000_000,
            hedge_value: -6_000_000,
        };
        assert!(
            over.imbalance_ppm() < 0,
            "short more than it holds: {}",
            over.imbalance_ppm()
        );
    }

    /// The book is what the pool owns. A perp hedge adds no capital, so it must not move the
    /// denominator: that would shrink the imbalance of a pool that had hedged nothing away.
    #[test]
    fn the_hedge_does_not_change_the_size_of_the_book() {
        let a = Inventory {
            base_value: 8_000_000,
            quote_share: 2_000_000,
            hedge_value: 0,
        };
        let b = Inventory {
            hedge_value: -3_000_000,
            ..a
        };
        assert_eq!(a.imbalance_ppm(), 800_000, "base 8M of a 10M book");
        assert_eq!(
            b.imbalance_ppm(),
            500_000,
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
            sample_ms_min: 100,
            sample_ms_max: 10_000,
        }
    }

    fn params() -> SkewParams {
        SkewParams {
            gamma_e2: 10_000,
            positive_bps_max: 30,
            negative_bps_max: 10,
        }
    }

    /// A pool holding `base_pct` of a fixed 100M book in base with half of it hedged away, so NET
    /// exposure is `base_pct - 50` points. An unhedged pool can never be short.
    fn inv(base_pct: u128) -> Inventory {
        Inventory {
            base_value: base_pct * 1_000_000,
            quote_share: (100 - base_pct) * 1_000_000,
            hedge_value: -50_000_000,
        }
    }

    // --- Volatility ---

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
        // +-10 bp at 1s: per-second variance (10bp)^2, so sigma(300s) approaches ~173 bps.
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
        // `jump` asks the SAME estimator over its own interval: two would be two ways to disagree.
        let mut v = Volatility::new(vol_cfg());
        let mut t = Instant::now();
        let base = 100_000_000_000u128;
        let bp = base / 10_000;
        // +-1 bp/s: sigma(1s) is 1 bp = 100 hundredths, sigma(300s) is sqrt(300) = 17.3 bp.
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

        // The two sampling rates the loop uses differ by exactly sqrt(5), which is what makes the
        // fast lane the same threshold scaled to a shorter interval rather than a different one.
        let fast = v.sigma_bps_e2_over_ms(200);
        let scaled = fast * 2_236 / 1_000;
        assert!(
            scaled.abs_diff(one_sec) * 100 <= one_sec * 5,
            "sqrt-of-time broke: sigma(200ms) {fast} scales to {scaled}, sigma(1s) is {one_sec}"
        );

        // And it agrees with the horizon method the skew uses, `sigma(300s)^2 == 300*sigma(1s)^2`.
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
        // Otherwise a two-minute outage across a 1% move enters as one 1% per-second return.
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

    // --- The skew itself ---

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
        // 20% exposed, sigma 10 bps, gamma 1000 -> 2 bp: the module docs quote this for gamma.
        let i = Inventory {
            base_value: 700_000,
            quote_share: 300_000,
            hedge_value: -500_000,
        };
        assert_eq!(i.imbalance_ppm(), 200_000);
        let live = SkewParams {
            gamma_e2: 100_000,
            ..params()
        };
        let s = compute(&i, 100 * 1_000_000, 10_000, &live, 9_999);
        assert_eq!(s.raw_decibps, 20, "2.0 bp");
        assert_eq!(s.applied_bps, 2);

        // ... and the live book: 11.2% off target at sigma 9 bps gives just under a bp, so the
        // feature is visibly on rather than rounding to nothing.
        let live_book = Inventory {
            base_value: 611_617,
            quote_share: 388_383,
            hedge_value: -500_000,
        };
        assert_eq!(live_book.imbalance_ppm(), 111_617);
        let s = compute(&live_book, 81 * 1_000_000, 9_000, &live, 9_999);
        assert_eq!(s.raw_decibps, 9);
        assert_eq!(s.applied_bps, 1, "a live, non-zero, un-clamped skew");
        assert_eq!(s.clamp, Clamp::Unbound);
    }

    #[test]
    fn the_coefficient_scales_with_sigma_squared_and_not_with_a_constant() {
        // The reason for reaching for A-S: doubling volatility must quadruple the skew.
        let i = inv(70);
        let calm = compute(
            &i,
            900 * 1_000_000,
            30_000,
            &SkewParams {
                positive_bps_max: 9_999,
                ..params()
            },
            9_999,
        );
        let fast = compute(
            &i,
            3_600 * 1_000_000,
            60_000,
            &SkewParams {
                positive_bps_max: 9_999,
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
        // A lifted bid is a free option and a lowered ask is not, so the asymmetry is pinned here.
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
        // A 10x volatility error is a 100x skew error; the cap stops that becoming a 300 bp quote.
        let s = compute(&inv(90), u128::MAX / 2, u64::MAX, &params(), 9_999);
        assert_eq!(s.applied_bps, 30);
        assert!(s.clamp.bound());
    }

    #[test]
    fn the_min_price_floor_clamps_before_the_row_is_refused() {
        // pairId 1 as deployed: minPrice 1e15, half-spread 5 bp. At a fair of $1010 there is only
        // ~94 bp of room before the bid target breaches the floor.
        let fair = 1_010_000_000_000_000u128;
        let min_price = 1_000_000_000_000_000u128;
        let cap = price_cap_bps_min(fair, min_price, 5);
        assert!(
            (90..=99).contains(&cap),
            "expected ~94 bp of headroom, got {cap}"
        );

        // sigma of 200 bp on a fully-long book wants 200 bp of skew; the floor allows 94.
        let s = compute(
            &inv(100),
            40_000 * 1_000_000,
            200_000,
            &SkewParams {
                positive_bps_max: 9_999,
                ..params()
            },
            cap,
        );
        assert_eq!(s.raw_decibps, 2_000, "the model wants 200 bp");
        assert_eq!(s.applied_bps, i16::try_from(cap).unwrap());
        assert_eq!(s.clamp, Clamp::MinPriceFloor);
        assert_eq!(s.floor_cap_bps, cap);

        // Verified against `dubu-core` rather than against the derivation above.
        let row = |skew| {
            ladder::build(&crate::ladder::RowInputs {
                pair_id: 1,
                fair,
                half_spread_bps_e2: 500,
                width_bps_e2: 2500,
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
        assert_eq!(price_cap_bps_min(min_price, min_price, 5), 0);
        assert_eq!(price_cap_bps_min(min_price / 2, min_price, 5), 0);
        assert_eq!(price_cap_bps_min(0, min_price, 5), 0);

        // Far above the floor the headroom is roughly half the price, so the configured cap binds.
        let cap = price_cap_bps_min(1_943_820_000_000_000, min_price, 5);
        assert_eq!(cap, 4_852);
        assert!(u128::from(cap) < BPS_MAX);
        assert!(
            cap > params().positive_bps_max * 100,
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
        };
        assert_eq!(i.imbalance_ppm(), 0);
        assert_eq!(
            compute(&i, 900 * 1_000_000, 30_000, &params(), 9_999).applied_bps,
            0
        );
    }

    /// The hedge is the only thing that moves the imbalance; there is no funding-target knob here.
    #[test]
    fn the_hedge_is_what_moves_the_imbalance() {
        let holdings = |hedge| Inventory {
            base_value: 600,
            quote_share: 400,
            hedge_value: hedge,
        };
        assert_eq!(holdings(0).imbalance_ppm(), 600_000, "long the lot");
        assert_eq!(
            holdings(-300).imbalance_ppm(),
            300_000,
            "half of it covered"
        );
        assert_eq!(holdings(-600).imbalance_ppm(), 0, "flat");
        assert_eq!(
            holdings(-900).imbalance_ppm(),
            -300_000,
            "short past its holding"
        );
    }

    #[test]
    fn no_skew_in_range_can_cross_the_book() {
        // Structurally impossible, but checked across the whole legal range against `dubu-core`'s
        // own builder rather than by argument.
        use dubu_core::ladder::LadderBuilder;
        let fair = 1_943_820_000_000_000u128;
        for skew in [-9_999i16, -1_000, -30, -1, 0, 1, 30, 1_000, 9_999] {
            for hs in [1u16, 5, 8, 100] {
                let b = LadderBuilder {
                    skew_bps: skew,
                    half_spread_bps_e2: u32::from(hs) * 100,
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
