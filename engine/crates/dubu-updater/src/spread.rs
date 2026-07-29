//! The volatility-scaled half-spread.
//!
//! ```text
//! half_spread = min(s0 + s1 * sigma, cap) + degraded_extra
//! ```
//!
//! `s0` is the pair's configured `half_spread_bps`, `sigma` is the 300 s volatility from
//! [`crate::skew::Volatility`] — the same estimate the Avellaneda–Stoikov skew uses, and the only
//! one in this crate — and `s1` is a dimensionless coefficient in bps of half-spread per bp of
//! sigma.
//!
//! # Why the constant had to go
//!
//! Adverse selection here is **entirely a jump phenomenon**. The simulator's pure-diffusion paths
//! produced zero informed fills: at 1 bp/s with 2 s of quote latency the posted quote is never
//! wrong by more than ~1.4 bp against a 5 bp half-spread, so there is nothing to arbitrage. Every
//! dollar of the measured -$16,580 came from one 100 bp jump. Jumps cluster with volatility, so a
//! half-spread that does not move with volatility is a constant premium against a hazard that is
//! anything but constant — too wide most of the time and far too narrow when it matters.
//!
//! # Deriving `s1` rather than picking it
//!
//! The sweep, against one 100 bp jump on the pool as configured (25 bp width, $2M epoch):
//!
//! ```text
//! half-spread   5 bp      15 bp      30 bp      60 bp
//! result     -$15,695   -$8,697     +$566      +$378
//! ```
//!
//! **30 bp is the smallest half-spread the sweep measures to be positive.** That is the anchor —
//! a measured point, not a zero interpolated between two negative-and-positive neighbours, because
//! the relationship is not linear (`e50` in the arrival hazard is itself tied to the half-spread).
//! In absorption terms it is `30 + 25/2 = 42.5 bp` of absorption against a 100 bp jump.
//!
//! The remaining question is *when* the pool should be paying 30 bp. Not always: a constant 30 bp
//! is UniV2's fee, and a prop AMM that quotes UniV2's fee has no reason to exist. It should be
//! paying it when a 100 bp jump is a live possibility, and the sweep's own conclusion says when
//! that is — jumps cluster with volatility. At ETHUSDT's measured `sigma(300s)` of 10 bp, a 100 bp
//! gap is a 10-sigma move and is not live. At **5x** that, `sigma(300s) = 50 bp`, it is a 2-sigma
//! move and plainly is. So:
//!
//! ```text
//! s0 + s1 * (5 * sigma_measured) = 30
//! 5  + s1 * 50                   = 30
//! s1                             = 25 / 50 = 0.5
//! ```
//!
//! **`s1 = 0.5`** — half a basis point of half-spread per basis point of 300-second sigma.
//!
//! What it produces, on both pairs, at the measured sigma and multiples of it:
//!
//! ```text
//!                 sigma(300s)   half-spread    absorption
//! ETHUSDT  1x        10 bp        10.0 bp        22.5 bp     s0 = 5,  width 25
//!          2x        20 bp        15.0 bp        27.5 bp
//!          5x        50 bp        30.0 bp        42.5 bp     <- the measured-positive point
//! BTCUSDT  1x         3 bp         9.5 bp        29.5 bp     s0 = 8,  width 40
//!          2x         6 bp        11.0 bp        31.0 bp
//!          5x        15 bp        15.5 bp        35.5 bp
//! ```
//!
//! BTC widens far less, and should: it is a third as volatile, and its wider ladder already
//! absorbs 20 bp before the spread contributes anything. One `s1` across both pairs is only
//! possible *because* the term is proportional to each pair's own sigma.
//!
//! **The honest cost.** ETH's ordinary-market half-spread doubles, 5 bp to 10 bp. The simulator
//! cannot see what that costs, because it has no competing venue and therefore no uninformed
//! arrival rate that responds to price — its own documentation says so, and says the spread sweep
//! is biased in favour of wider for exactly that reason. So `s1` is chosen knowing the model
//! flatters it, and it is the first number to back-solve once there is fill data. That is why
//! every row logs `s0`, `sigma`, the sigma term, the result, and whether the cap bound: without
//! those four the next correction would be another guess.
//!
//! # The cap is 30 bp, and past it the answer is not more spread
//!
//! An unbounded spread in a volatility spike is a quoting outage wearing a disguise, so the term
//! is capped — at exactly the 30 bp anchor, which is exactly UniV2's fee. The rule that produces
//! is worth stating plainly: **the pool never quotes worse than the constant-fee AMM it is trying
//! to beat; it simply does not quote 30 bp in a calm market.** Above that point widening has
//! stopped being a defence — the sweep's own 60 bp row earns *less* than the 30 bp row — and the
//! defence that does work is [`crate::jump`], which stops quoting altogether.
//!
//! The cap can never *narrow* a configured spread: if an operator sets `half_spread_bps` above the
//! cap, `s0` wins and the volatility term contributes nothing. [`crate::config`] refuses that
//! configuration at startup rather than silently honouring it.
//!
//! # Staleness-scaled spread: considered, not implemented
//!
//! The chain's ladder is fixed between pushes, so widening in proportion to the expected time to
//! the next push is a real idea. It is not implemented, for two reasons that are measurements
//! rather than opinions.
//!
//! First, **the horizon already pays for it, seventeen times over.** `sigma` is scaled to 300 s and
//! the loop pushes on a 0.5 bp adverse-drift trigger at a 1 s cadence, so realised staleness is one
//! to two blocks. `sigma(300s) = sqrt(300) * sigma(1s)`, so the 5 bp the volatility term already
//! adds for ETH is covering 300 seconds of one-sigma diffusion against an exposure window of one.
//! A staleness term on top of that would be double-counting by more than an order of magnitude.
//!
//! Second, **the one regime where staleness is genuinely long already has its own explicit
//! widening**: `chain.degraded_extra_half_spread_bps`, 25 bp, applied whenever the chain view
//! cannot be refreshed. That is a staleness-scaled spread with a step function instead of a ramp,
//! and it is aimed at the case that actually produces long staleness.
//!
//! What would change the answer: the `quote_age_secs` field already in every `decision` log line
//! showing a fat tail past a few seconds in ordinary operation. Then the exposure window would no
//! longer be one block, the 300 s horizon would stop being generous, and a ramp would earn its
//! place. Until that is measured, adding it would be a third coefficient chosen by feel.

use dubu_core::ladder::BPS_E2_MAX;

/// Rescale a sigma measured over one window to the window a quote is actually exposed for.
///
/// # Why this exists
///
/// The volatility estimator reports over `skew.vol_horizon_secs`, 300 s, because that is roughly
/// how long INVENTORY is held -- the skew's exposure. The half-spread's exposure is something else
/// entirely: how long a posted quote can be hit at a price the market has left behind. The module
/// docs above argued the 300 s horizon "already pays for it, seventeen times over" and named the
/// measurement that would change the answer -- `quote_age_secs` showing a fat tail past a few
/// seconds. It was measured on 2026-07-29 over 360 decisions:
///
/// ```text
/// p50 0.00 s   p90 2.00 s   p99 3.00 s   p100 364.00 s
/// ```
///
/// So the exposure window is two to three seconds, and sizing the spread off a five-minute move was
/// covering it sqrt(300/3) = 10x over. On ETH that is the difference between 5.67 bp and 1.47 bp of
/// half-spread -- the margin was not a rounding error, it was most of the price.
///
/// The p100 is the honest caveat: one quote sat for six minutes, and for that quote the 300 s
/// horizon was right. That case has its own explicit widening (`degraded_extra_half_spread_bps`),
/// which is the mechanism meant to catch it; whether it actually fired there is not established.
///
/// Sigma scales as the square root of time, so this is `sigma * sqrt(to / from)`. Returns the input
/// unchanged when either window is zero -- there is no sensible rescaling of an unmeasured sigma.
#[must_use]
pub fn rescale_sigma(sigma_millibps: u64, from_secs: u64, to_secs: u64) -> u64 {
    if from_secs == 0 || to_secs == 0 || sigma_millibps == 0 {
        return sigma_millibps;
    }
    let ratio = (to_secs as f64 / from_secs as f64).sqrt();
    (sigma_millibps as f64 * ratio) as u64
}

/// The two knobs. Global rather than per-pair: `s1` is dimensionless and multiplies each pair's
/// own sigma, so one value is already per-pair-correct, and the cap is a statement about the worst
/// price the desk is willing to show anywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpreadParams {
    /// `s1`, scaled by 100 so the config can say `0.5`.
    pub vol_coefficient_e2: u32,
    /// The ceiling on `s0 + s1 * sigma`, in bps. The degraded-chain widening is added *after* it;
    /// see [`compute`].
    pub half_spread_bps_e2_max: u32,
}

/// Everything one half-spread computation produced. All of it is logged, every row, every cycle —
/// this is the sample a back-solve for `s1` needs, and `vol_decibps` next to `capped` is what says
/// whether the model or the ceiling has been doing the deciding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spread {
    /// The pair's configured half-spread, `s0`.
    pub s0_bps_e2: u32,
    /// `sigma` over the estimator's horizon, in thousandths of a bp, exactly as the skew logs it.
    pub sigma_millibps: u64,
    /// `s1 * sigma`, in deci-bps. Deci-bps because whole-bps rounding hides everything under
    /// 0.5 bp, which for BTCUSDT is most of the term.
    pub vol_decibps: u32,
    /// `min(s0 + s1 * sigma, cap)`, in bps. What the volatility path produced, before the
    /// degraded-chain widening.
    pub vol_scaled_e2: u32,
    /// Whether [`SpreadParams::half_spread_bps_max`] bound it.
    pub capped: bool,
    /// The degraded-chain widening that was added on top.
    pub degraded_extra_e2: u32,
    /// The half-spread that goes into [`crate::ladder::build`].
    pub half_spread_e2: u32,
}

impl Spread {
    /// How much the volatility term actually added, in bps of the final half-spread. For the log
    /// line, so that "the term is on but rounding to nothing" is distinguishable from "the term is
    /// off".
    #[must_use]
    pub const fn vol_e2(&self) -> u32 {
        self.vol_scaled_e2.saturating_sub(self.s0_bps_e2)
    }
}

/// Compute one row's half-spread.
///
/// # The order of operations, which is a decision and not an accident
///
/// The cap is applied to `s0 + s1 * sigma` and the degraded-chain widening is added **after** it.
/// The two defend against different failures — one against a market that is moving, the other
/// against a chain view that cannot be refreshed — and letting the volatility cap swallow the
/// degraded widening would silently disable the second whenever the first was already at its
/// ceiling, which is precisely when both are needed. The final value is clamped to `dubu-core`'s
/// `BPS_MAX` so it can never leave the domain [`crate::ladder::build`] validates in.
///
/// # Composition with the inventory skew
///
/// The result is the half-spread that [`crate::skew::price_cap_bps_min`] is asked about, so the
/// skew's floor clamp is computed against the spread that will actually be posted. A wider spread
/// pushes the bid target lower and therefore leaves *less* room to skew down; feeding the
/// unwidened value in would let the skew push a row through the pair's `minPrice` floor and have
/// it refused. Both then go into `dubu-core`'s `skewed_mid` and through `validateLadder`, so
/// nothing reaches the chain unvalidated.
#[must_use]
pub fn compute(
    s0_bps_e2: u32,
    sigma_millibps: u64,
    degraded_extra_e2: u32,
    params: &SpreadParams,
) -> Spread {
    // s1 * sigma, in HUNDREDTHS of a bp: (s1_e2 / 100) * (sigma_millibps / 1000) * 100.
    //
    // This used to round up to whole bps, because `dubu-core` took whole bps. It no longer does,
    // and the rounding was not free: once sigma was scaled to the quote's real exposure window the
    // volatility term came to 0.61 bp on ETH, which rounded to 1 -- a 64% overcharge on the term,
    // and on a 1 bp `s0` that is a third of the whole price.
    let vol_decibps =
        u32::try_from(u64::from(params.vol_coefficient_e2).saturating_mul(sigma_millibps) / 10_000)
            .unwrap_or(u32::MAX);
    let vol_e2 = vol_decibps.saturating_mul(10);

    let wanted = s0_bps_e2.saturating_add(vol_e2);
    let cap = params.half_spread_bps_e2_max;
    // `.max(s0_bps_e2)`: a cap below the configured spread must never *narrow* it. `s0` is the
    // operator's floor, not a suggestion — the volatility term simply contributes nothing there.
    // `config` refuses that combination at startup, so this is the second belt.
    let (vol_scaled_e2, capped) = if wanted > cap {
        (cap.max(s0_bps_e2), true)
    } else {
        (wanted, false)
    };

    let half_spread_e2 = vol_scaled_e2
        .saturating_add(degraded_extra_e2)
        .min(u32::try_from(BPS_E2_MAX).unwrap_or(u32::MAX));

    Spread {
        s0_bps_e2,
        sigma_millibps,
        vol_decibps,
        vol_scaled_e2,
        capped,
        degraded_extra_e2,
        half_spread_e2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ladder::{self, RowInputs};

    /// The derived configuration: s1 = 0.5, cap 30 bp.
    fn params() -> SpreadParams {
        SpreadParams {
            vol_coefficient_e2: 50,
            half_spread_bps_e2_max: 3000,
        }
    }

    /// sigma in whole bps -> the milli-bps the estimator reports.
    const fn sigma(bps: u64) -> u64 {
        bps * 1_000
    }

    #[test]
    fn the_documented_table_is_what_the_code_produces() {
        // The half-spread at 1x, 2x and 5x the MEASURED sigma on both pairs. These six numbers are
        // the derivation's output and the thing a reader would check first, so they are pinned
        // rather than left to be recomputed by hand.
        // In hundredths of a bp now, and no longer rounded up to whole bps -- 9.5 bp is 950,
        // where it used to be charged as 10.
        let eth = |m: u64| compute(500, sigma(10 * m), 0, &params()).half_spread_e2;
        assert_eq!(
            eth(1),
            1_000,
            "ETH at the measured sigma of 10 bp: 5 + 0.5*10"
        );
        assert_eq!(eth(2), 1_500);
        assert_eq!(eth(5), 3_000, "the measured-positive point in the sweep");

        let btc = |m: u64| compute(800, sigma(3 * m), 0, &params()).half_spread_e2;
        assert_eq!(btc(1), 950, "8 + 0.5*3 = 9.5, and 9.5 is now representable");
        assert_eq!(btc(2), 1_100);
        assert_eq!(btc(5), 1_550, "8 + 0.5*15 = 15.5");

        // And the absorption limits those imply, which is the number that actually decides the
        // outcome: absorption = half_spread + width/2.
        assert_eq!(eth(1) + 2_500 / 2, 2_250, "10 bp half + 12.5 bp of width");
        assert_eq!(eth(5) + 2_500 / 2, 4_250);
        assert_eq!(btc(1) + 4_000 / 2, 2_950);
    }

    #[test]
    fn a_flat_market_leaves_the_configured_spread_exactly_where_it_was() {
        // Warm-up matters: the estimator seeds at zero, so the first cycles after a start or a
        // feed outage must produce the configured spread and not an invented one.
        let s = compute(500, 0, 0, &params());
        assert_eq!(s.half_spread_e2, 500);
        assert_eq!(s.vol_decibps, 0);
        assert_eq!(s.vol_e2(), 0);
        assert!(!s.capped);
    }

    #[test]
    fn the_term_is_linear_in_sigma_and_the_skew_is_quadratic() {
        // Worth pinning together: the two consumers of the same sigma respond to it differently on
        // purpose. Doubling sigma doubles the spread term and quadruples the skew.
        let one = compute(500, sigma(10), 0, &params()).vol_decibps;
        let two = compute(500, sigma(20), 0, &params()).vol_decibps;
        assert_eq!(two, 2 * one);
    }

    #[test]
    fn the_cap_binds_and_says_so() {
        // 10x the measured sigma wants 5 + 50 = 55 bp.
        let s = compute(500, sigma(100), 0, &params());
        assert_eq!(s.vol_decibps, 500);
        assert_eq!(s.vol_scaled_e2, 3000);
        assert!(s.capped, "without this flag, tuning s1 later is guesswork");
        assert_eq!(s.half_spread_e2, 3000);

        // An absurd sigma cannot walk it any further. Past this point the defence is `jump`, not
        // a wider quote.
        let s = compute(500, u64::MAX, 0, &params());
        assert_eq!(s.half_spread_e2, 3000);
        assert!(s.capped);
    }

    #[test]
    fn the_degraded_widening_survives_the_cap() {
        // The two defences are for different failures. A volatility spike that pins the cap must
        // not silently disable the widening that exists for a chain view we cannot refresh —
        // which is exactly when both are needed at once.
        let s = compute(500, sigma(100), 2_500, &params());
        assert_eq!(s.vol_scaled_e2, 3_000);
        assert!(s.capped);
        assert_eq!(s.degraded_extra_e2, 2_500);
        assert_eq!(s.half_spread_e2, 5_500);
    }

    #[test]
    fn a_cap_below_the_configured_spread_never_narrows_it() {
        // `s0` is the operator's floor, not a suggestion. `config::validate` refuses this
        // combination at startup; this is the behaviour if it ever gets past.
        let tight = SpreadParams {
            vol_coefficient_e2: 50,
            half_spread_bps_e2_max: 300,
        };
        let s = compute(500, sigma(10), 0, &tight);
        assert_eq!(
            s.half_spread_e2, 500,
            "the cap floors at s0, it never cuts below it"
        );
        assert_eq!(
            s.vol_e2(),
            0,
            "and the volatility term contributes nothing there"
        );
        assert!(
            s.capped,
            "but the cap did bind, and the log line must say so"
        );
    }

    #[test]
    fn every_half_spread_this_can_produce_still_builds_a_row_the_chain_accepts() {
        // The composition requirement: nothing here may produce a ladder `dubu-core`'s validator —
        // which is the chain's own check — refuses. Swept across the whole reachable range of the
        // volatility term, at the live pair shape, with the skew at both of its clamps.
        const FAIR: u128 = 1_943_820_000_000_000;
        for m in [0u64, 1, 2, 5, 10, 50, 1_000] {
            let s = compute(500, sigma(10 * m), 0, &params());
            for skew_bps in [-10i16, 0, 30] {
                let row = ladder::build(&RowInputs {
                    pair_id: 1,
                    fair: FAIR,
                    half_spread_bps_e2: s.half_spread_e2,
                    width_bps_e2: 2500,
                    skew_bps,
                    capture: 20_000_000_000_000_000_000,
                    bid_capacity: 1_000_000_000_000_000_000_000,
                    ask_capacity: 1_000_000_000_000_000_000_000,
                    min_price: 1_000_000_000_000_000,
                    price_scale_exp: 24,
                })
                .unwrap_or_else(|e| panic!("hs {} skew {skew_bps} failed: {e}", s.half_spread_e2));
                row.ladder
                    .validate(1_000_000_000_000_000)
                    .expect("every reachable half-spread must still pass the chain's validator");
                // The spread really is what was asked for, and it really does widen with sigma.
                assert_eq!(
                    row.bid_target,
                    row.mid * (1_000_000 - u128::from(s.half_spread_e2)) / 1_000_000
                );
            }
        }

        // And the degraded case on top of the cap, which is the widest thing reachable at all.
        let s = compute(500, sigma(1_000), 2_500, &params());
        assert_eq!(s.half_spread_e2, 5_500);
        assert!(ladder::build(&RowInputs {
            pair_id: 1,
            fair: FAIR,
            half_spread_bps_e2: s.half_spread_e2,
            width_bps_e2: 2500,
            skew_bps: 30,
            capture: 20_000_000_000_000_000_000,
            bid_capacity: 1_000_000_000_000_000_000_000,
            ask_capacity: 1_000_000_000_000_000_000_000,
            min_price: 1_000_000_000_000_000,
            price_scale_exp: 24,
        })
        .is_ok());
    }

    #[test]
    fn a_wider_spread_leaves_the_skew_less_room_before_the_floor_and_that_is_handled() {
        // The composition failure this ordering prevents: computing the skew's floor clamp against
        // the UNwidened spread would let it push a row under the pair's minPrice and have it
        // refused outright, which is a quoting outage rather than a slightly-less-skewed book.
        let min_price = 1_000_000_000_000_000u128;
        let fair = 1_010_000_000_000_000u128;
        let narrow = crate::skew::price_cap_bps_min(fair, min_price, 5);
        let wide = crate::skew::price_cap_bps_min(fair, min_price, 30);
        assert!(
            wide < narrow,
            "a wider spread must leave less headroom, got {wide} vs {narrow}"
        );
    }

    /// The quote's exposure window is not the inventory's, and sizing one off the other was most of
    /// the half-spread. Measured 2026-07-29: `quote_age_secs` p90 = 2 s, p99 = 3 s, against a
    /// 300 s estimator horizon.
    #[test]
    fn sigma_rescales_to_the_window_the_quote_is_actually_exposed_for() {
        // ETH's measured sigma over 300 s, in thousandths of a bp.
        let sigma_300 = 16_090;
        let three = rescale_sigma(sigma_300, 300, 3);
        assert!(
            (1_550..=1_650).contains(&three),
            "sqrt(3/300) = 1/10, so ~1.61 bp, got {three}"
        );
        // s0 = 1 bp, s1 = 0.29: 1 + 0.29 * 1.61 = 1.47 bp against 1 + 0.29 * 16.09 = 5.67 bp.
        assert!(
            rescale_sigma(sigma_300, 300, 300) == sigma_300,
            "same window, same sigma"
        );
        assert!(
            rescale_sigma(sigma_300, 300, 1200) > sigma_300,
            "a longer window is a bigger move"
        );
    }

    /// An unmeasured sigma has no rescaling, and neither does a zero window. Returning the input
    /// keeps the caller's behaviour identical to before the horizon was split out.
    #[test]
    fn a_zero_anything_passes_through() {
        assert_eq!(rescale_sigma(0, 300, 3), 0);
        assert_eq!(rescale_sigma(5_000, 0, 3), 5_000);
        assert_eq!(rescale_sigma(5_000, 300, 0), 5_000);
    }
}
