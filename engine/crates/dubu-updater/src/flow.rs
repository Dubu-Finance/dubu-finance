//! The own-fill signal: what the trades we just lost money on say about the next price.
//!
//! A market maker sees something nobody else does — who is trading against it, and how those
//! trades worked out. [`crate::markout`] turns that into a per-counterparty score. This turns the
//! score into a shift on fair value.
//!
//! # Why this is not order-flow imbalance
//!
//! The obvious construction is signed order flow: sum the direction of recent fills, shift the mid
//! toward it. It is also the construction that does not work here, for a reason that has nothing
//! to do with implementation. Our fills are not a sample of the market's flow — they are a sample
//! of *the flow that chose to trade against our quote*. A wide quote gets hit only by informed
//! flow; a tight one gets hit by everything. The measured imbalance therefore moves with our own
//! spread, and a signal that responds to our own pricing decision is measuring the thermostat, not
//! the temperature.
//!
//! Conditioning on markout is what breaks that. A fill only contributes in proportion to how badly
//! its counterparty has hurt us before, so the signal is carried by *who* traded rather than by
//! how many did. Widening the spread changes the mix of counterparties, but it does not change
//! what any individual counterparty's history says.
//!
//! # The external book is a control, not an input
//!
//! Flow that merely echoes the public book is not private information, and our reference price has
//! already priced it. Only the part of our flow that the public book does not explain is worth
//! acting on, so the venues' top-of-book imbalance is subtracted before the shift is computed.
//!
//! `beta` is a fixed, configured coefficient, deliberately not fitted online. Fitting it would
//! mean estimating a regression coefficient from a few dozen fills, which produces a confident
//! number and no information.
//!
//! # The attack this must not enable
//!
//! Any signal that lets a counterparty's trades move our price is a signal a counterparty can
//! trade against. Push the mid up with a small buy, then sell large into the distorted quote.
//!
//! Note that markout-conditioning makes this *worse*, not better, and it is worth being blunt
//! about it: the weight rises with how consistently a counterparty beats us, and a successful
//! manipulator is by construction a counterparty that beats us. Informedness weighting is correct
//! for prediction and hands the attacker a lever, and no amount of tuning the weight resolves
//! that. What resolves it is arithmetic.
//!
//! Three bounds, and one of them is checked at construction:
//!
//! 1. **An unknown counterparty carries zero weight.** A fresh address cannot move the signal at
//!    all. Earning weight requires a settled markout history, and earning a *high* weight requires
//!    actually beating us repeatedly — which is expensive, and is the cost that makes the rest of
//!    this hold.
//! 2. **The window must carry real notional before it says anything.** Below
//!    [`Params::min_window_notional`] the shift is zero, so one fill can never dominate.
//! 3. **The shift is capped, and the cap is checked against the epoch.** Manufacturing a shift of
//!    `max_shift_bps` costs the attacker the spread on at least `min_window_notional`; exploiting
//!    it earns at most `max_shift_bps` on the epoch capacity. [`Params::validate`] refuses a
//!    configuration where the second exceeds the first, so an exploitable set of numbers fails at
//!    startup instead of in production.
//!
//! # What this does not fix
//!
//! Not the jump arbitrageur. That is a counterparty seen once, with no history, carrying zero
//! weight by rule 1 — and it is the single largest loss the flow simulator found. This signal
//! addresses persistent informed flow between jumps, which is a different and smaller problem.
//! Claiming otherwise would be the same mistake as reporting that the staleness ramp was a perfect
//! defence.

use std::collections::VecDeque;

use alloy_primitives::Address;

use crate::markout::{Markout, Score};

/// Signal shifts are reported in hundredths of a basis point, matching `markout`.
pub const SHIFT_SCALE: i64 = 10_000;

/// How the signal is sized and what it refuses to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Params {
    /// How far back fills contribute.
    pub window_secs: u64,
    /// Weighted notional the window must hold before the signal is anything but zero. The
    /// single-fill bound, and the denominator of the manipulation arithmetic.
    pub min_window_notional: u128,
    /// Settled fills a counterparty needs before its score carries any weight.
    pub min_fills: u64,
    /// Notional a counterparty needs before its score carries any weight.
    pub min_notional: u128,
    /// Markout, in hundredths of a bp against us, at which a counterparty reaches full weight. A
    /// counterparty costing us more than this is not weighted further; the cap is what stops one
    /// catastrophic fill from owning the signal.
    pub full_weight_at_e2: i64,
    /// How much of the public book's imbalance is subtracted. Fixed, not fitted.
    pub beta_e4: i64,
    /// Shift produced by a residual imbalance of one, before the cap.
    pub gain_bps_e2: i64,
    /// Hard cap on the shift, in hundredths of a bp.
    pub max_shift_bps_e2: i64,
    /// Largest notional a single epoch can be drained for. The exploit side of the inequality.
    pub epoch_notional: u128,
    /// Half-spread the pool charges, in hundredths of a bp. The attacker's cost of manufacturing.
    pub half_spread_e2: i64,
}

/// Why a set of parameters was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParamsError {
    /// A window of zero seconds holds no fills, so the signal would be permanently zero — quietly.
    #[error("window_secs must be non-zero")]
    EmptyWindow,
    /// Without a notional floor, one fill is the whole window and the signal is trivially forged.
    #[error("min_window_notional must be non-zero; without it a single fill owns the signal")]
    NoNotionalFloor,
    /// Weighting divides by this.
    #[error("full_weight_at_e2 must be positive")]
    NoWeightScale,
    /// The shift a counterparty can manufacture is worth more than manufacturing it costs.
    #[error(
        "manipulation is profitable: manufacturing {max_shift_bps_e2}e-2bp costs at most \
         {cost} but exploiting it on an epoch of {epoch_notional} earns up to {gain}. \
         Lower max_shift_bps_e2, raise min_window_notional, or widen the spread"
    )]
    Exploitable {
        /// The cap that would have been allowed.
        max_shift_bps_e2: i64,
        /// Epoch capacity the attacker could drain at the distorted price.
        epoch_notional: u128,
        /// What manufacturing the shift costs the attacker, in quote units.
        cost: u128,
        /// What exploiting it earns, in quote units.
        gain: u128,
    },
}

impl Params {
    /// Refuses a configuration a counterparty could profitably manipulate.
    ///
    /// Manufacturing the maximum shift means trading at least `min_window_notional` against us and
    /// paying the half-spread on it. Exploiting the resulting distortion earns at most the shift
    /// applied to everything the epoch can be drained for. If the second is not strictly smaller
    /// than the first, the signal is a subsidy, and this is the last place that can be noticed
    /// cheaply.
    ///
    /// Deliberately a startup check rather than a runtime one. A runtime guard would leave the
    /// exploitable numbers in the config file, working right up until the epoch is raised.
    pub fn validate(&self) -> Result<(), ParamsError> {
        if self.window_secs == 0 {
            return Err(ParamsError::EmptyWindow);
        }
        if self.min_window_notional == 0 {
            return Err(ParamsError::NoNotionalFloor);
        }
        if self.full_weight_at_e2 <= 0 {
            return Err(ParamsError::NoWeightScale);
        }

        let cost = self
            .min_window_notional
            .saturating_mul(u128::try_from(self.half_spread_e2.max(0)).unwrap_or(0))
            / u128::try_from(SHIFT_SCALE).unwrap_or(1)
            / 100;
        let gain = self
            .epoch_notional
            .saturating_mul(u128::try_from(self.max_shift_bps_e2.max(0)).unwrap_or(0))
            / u128::try_from(SHIFT_SCALE).unwrap_or(1)
            / 100;

        if gain >= cost {
            return Err(ParamsError::Exploitable {
                max_shift_bps_e2: self.max_shift_bps_e2,
                epoch_notional: self.epoch_notional,
                cost,
                gain,
            });
        }
        Ok(())
    }
}

/// One fill, reduced to what the signal needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Contribution {
    at_secs: u64,
    /// Positive when the taker bought base from us, which is the direction they are betting on.
    taker_long: bool,
    notional: u128,
    /// Informedness, `0..=SHIFT_SCALE`. Zero for a counterparty with no settled history.
    weight_e4: i64,
}

/// What the signal concluded, and every intermediate value it concluded it from.
///
/// Every stage is reported rather than just the answer. A shift of zero has at least four distinct
/// causes — an empty window, a window under the notional floor, no weighted counterparties, and a
/// residual the public book fully explained — and a single output number cannot tell them apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Tilt {
    /// Fills inside the window.
    pub fills: u64,
    /// Fills that contributed nothing because their counterparty has no settled history.
    pub unweighted: u64,
    /// Weighted notional in the window.
    pub weighted_notional: u128,
    /// Own-flow imbalance, `-SHIFT_SCALE..=SHIFT_SCALE`. Positive means informed takers are buying.
    pub own_e4: i64,
    /// Public top-of-book imbalance over the same convention.
    pub book_e4: i64,
    /// What the public book did not explain.
    pub residual_e4: i64,
    /// The shift, in hundredths of a bp, after gain and cap. Positive raises fair value.
    pub shift_e2: i64,
    /// True when the cap bound the shift, meaning the signal wanted to move further than allowed.
    pub capped: bool,
    /// True when the window held less than the notional floor, so nothing was emitted.
    pub below_floor: bool,
}

/// Accumulates fills and turns them into a shift on fair value.
#[derive(Debug)]
pub struct Flow {
    params: Params,
    recent: VecDeque<Contribution>,
}

impl Flow {
    /// A signal with validated parameters.
    ///
    /// # Errors
    ///
    /// [`ParamsError`] when the parameters are degenerate or manipulable. See [`Params::validate`].
    pub fn new(params: Params) -> Result<Self, ParamsError> {
        params.validate()?;
        Ok(Self {
            params,
            recent: VecDeque::new(),
        })
    }

    /// The parameters in force.
    pub fn params(&self) -> &Params {
        &self.params
    }

    /// Records a fill, weighting it by what `markout` knows about the counterparty.
    ///
    /// `is_bid` follows the pool's convention: true when the pool bought base. The taker is
    /// therefore long exactly when the pool was *not* bidding.
    pub fn observe(
        &mut self,
        markout: &Markout,
        who: &Address,
        is_bid: bool,
        notional: u128,
        at_secs: u64,
    ) {
        let weight_e4 = markout.score_of(who).map_or(0, |s| self.weight_of(s));
        self.recent.push_back(Contribution {
            at_secs,
            taker_long: !is_bid,
            notional,
            weight_e4,
        });
        self.expire(at_secs);
    }

    /// How much a counterparty's history counts, `0..=SHIFT_SCALE`.
    ///
    /// Zero unless the score is seasoned enough to act on, and zero for a counterparty we make
    /// money from — a consistent loser's direction is not a forecast. It rises with how much the
    /// counterparty costs us and saturates at [`Params::full_weight_at_e2`], so no single
    /// catastrophic fill can own the signal.
    fn weight_of(&self, score: &Score) -> i64 {
        if !score.is_actionable(self.params.min_fills, self.params.min_notional) {
            return 0;
        }
        // The longest horizon: the one-second mark is mostly the next tick's noise.
        let Some(m) = score.markout_e2(crate::markout::HORIZONS_SECS.len() - 1) else {
            return 0;
        };
        let against_us = i64::try_from(-m).unwrap_or(i64::MAX);
        if against_us <= 0 {
            return 0;
        }
        (against_us.saturating_mul(SHIFT_SCALE) / self.params.full_weight_at_e2).min(SHIFT_SCALE)
    }

    fn expire(&mut self, now_secs: u64) {
        let cutoff = now_secs.saturating_sub(self.params.window_secs);
        while self.recent.front().is_some_and(|c| c.at_secs < cutoff) {
            self.recent.pop_front();
        }
    }

    /// The shift fair value should take, given the public book's own imbalance.
    ///
    /// `book_e4` is the venues' top-of-book imbalance on the same sign convention — positive when
    /// there is more size resting on the bid — scaled by [`SHIFT_SCALE`]. See [`book_imbalance`].
    pub fn tilt(&mut self, now_secs: u64, book_e4: i64) -> Tilt {
        self.expire(now_secs);

        let mut out = Tilt {
            fills: self.recent.len() as u64,
            book_e4,
            ..Tilt::default()
        };

        let mut signed: i128 = 0;
        let mut total: u128 = 0;
        for c in &self.recent {
            if c.weight_e4 == 0 {
                out.unweighted += 1;
                continue;
            }
            let w = c
                .notional
                .saturating_mul(u128::try_from(c.weight_e4).unwrap_or(0))
                / u128::try_from(SHIFT_SCALE).unwrap_or(1);
            total = total.saturating_add(w);
            let w = i128::try_from(w).unwrap_or(i128::MAX);
            signed += if c.taker_long { w } else { -w };
        }
        out.weighted_notional = total;

        if total < self.params.min_window_notional {
            out.below_floor = true;
            return out;
        }

        let total_i = i128::try_from(total).unwrap_or(1).max(1);
        out.own_e4 = i64::try_from(signed.saturating_mul(i128::from(SHIFT_SCALE)) / total_i)
            .unwrap_or(0)
            .clamp(-SHIFT_SCALE, SHIFT_SCALE);

        let explained = book_e4.saturating_mul(self.params.beta_e4) / SHIFT_SCALE;
        out.residual_e4 = out
            .own_e4
            .saturating_sub(explained)
            .clamp(-SHIFT_SCALE, SHIFT_SCALE);

        let want = out.residual_e4.saturating_mul(self.params.gain_bps_e2) / SHIFT_SCALE;
        out.shift_e2 = want.clamp(-self.params.max_shift_bps_e2, self.params.max_shift_bps_e2);
        out.capped = want != out.shift_e2;
        out
    }
}

/// Top-of-book imbalance across the live venues, scaled by [`SHIFT_SCALE`].
///
/// Positive when more size rests on the bid than the ask. Sizes are summed across venues rather
/// than averaged per venue, so a venue with real depth counts for more than one quoting dust —
/// averaging would let the smallest venue outvote the largest.
///
/// `None` when nothing is resting, which is a different statement from balanced and must not be
/// folded into zero: a balanced book explains flow, an absent one explains nothing.
pub fn book_imbalance(sizes: impl IntoIterator<Item = (u128, u128)>) -> Option<i64> {
    let (mut bid, mut ask) = (0u128, 0u128);
    for (b, a) in sizes {
        bid = bid.saturating_add(b);
        ask = ask.saturating_add(a);
    }
    let total = bid.checked_add(ask)?;
    if total == 0 {
        return None;
    }
    let diff = i128::try_from(bid).ok()? - i128::try_from(ask).ok()?;
    i64::try_from(diff.checked_mul(i128::from(SHIFT_SCALE))? / i128::try_from(total).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markout::Fill;
    use alloy_primitives::address;

    fn params() -> Params {
        Params {
            window_secs: 300,
            min_window_notional: 1_000_000_000,
            min_fills: 3,
            min_notional: 1_000_000,
            full_weight_at_e2: 1_000, // 10 bp against us is full weight
            beta_e4: 5_000,           // half the public book's imbalance is subtracted
            gain_bps_e2: 500,         // a residual of 1 wants 5 bp
            max_shift_bps_e2: 200,    // capped at 2 bp
            epoch_notional: 1_000_000_000,
            half_spread_e2: 1_750, // 17.5 bp, the absorption limit
        }
    }

    fn informed() -> Address {
        address!("00000000000000000000000000000000000000A1")
    }

    fn stranger() -> Address {
        address!("00000000000000000000000000000000000000B2")
    }

    /// Gives `who` a settled, losing-for-us history so their weight is non-zero.
    fn season(m: &mut Markout, who: Address, fills: u64) {
        for i in 0..fills {
            let at = 1_000 + i * 100;
            // Reference rises after the fill, so a taker buying base from us wins.
            m.observe_reference(1, at, 1_000_000);
            m.observe_reference(1, at + 1, 1_010_000);
            m.observe_reference(1, at + 10, 1_010_000);
            m.observe_reference(1, at + 60, 1_010_000);
            m.observe_fill(Fill {
                pair_id: 1,
                receiver: who,
                partner_id: 0,
                is_bid: false, // pool sold base
                amount_in: 1_000_000,
                amount_out: 1_000_000,
                at_secs: at,
                ref_at_fill: 1_000_000,
                price_scale_exp: 6,
            });
            m.settle(at + 100);
        }
    }

    #[test]
    fn a_manipulable_configuration_is_refused_at_construction() {
        let bad = Params {
            max_shift_bps_e2: 5_000,
            ..params()
        };
        assert!(matches!(
            Flow::new(bad),
            Err(ParamsError::Exploitable { .. })
        ));
    }

    /// The bound is an inequality between two products, so raising the epoch alone can break a
    /// configuration that was safe. That is exactly the case the startup check exists for.
    #[test]
    fn raising_the_epoch_can_invalidate_a_safe_configuration() {
        assert!(params().validate().is_ok());
        let bigger = Params {
            epoch_notional: params().epoch_notional * 100,
            ..params()
        };
        assert!(matches!(
            bigger.validate(),
            Err(ParamsError::Exploitable { .. })
        ));
    }

    #[test]
    fn a_window_without_a_notional_floor_is_refused() {
        let bad = Params {
            min_window_notional: 0,
            ..params()
        };
        assert_eq!(bad.validate(), Err(ParamsError::NoNotionalFloor));
    }

    /// Rule 1. A fresh address is the jump arbitrageur's shape and the manipulator's shape alike.
    #[test]
    fn an_unknown_counterparty_moves_nothing() {
        let m = Markout::new();
        let mut f = Flow::new(params()).expect("valid");
        f.observe(&m, &stranger(), false, 100_000_000_000, 2_000);
        let t = f.tilt(2_000, 0);
        assert_eq!(t.fills, 1);
        assert_eq!(t.unweighted, 1);
        assert_eq!(t.shift_e2, 0);
        assert_eq!(t.weighted_notional, 0);
    }

    /// A counterparty we consistently make money from is not a forecast.
    #[test]
    fn a_counterparty_we_beat_carries_no_weight() {
        let mut m = Markout::new();
        for i in 0..5u64 {
            let at = 1_000 + i * 100;
            m.observe_reference(1, at, 1_000_000);
            for h in crate::markout::HORIZONS_SECS {
                m.observe_reference(1, at + h, 990_000); // moves our way after we sell
            }
            m.observe_fill(Fill {
                pair_id: 1,
                receiver: stranger(),
                partner_id: 0,
                is_bid: false,
                amount_in: 1_000_000,
                amount_out: 1_000_000,
                at_secs: at,
                ref_at_fill: 1_000_000,
                price_scale_exp: 6,
            });
            m.settle(at + 100);
        }
        let s = m.score_of(&stranger()).expect("seasoned");
        assert!(
            s.markout_e2(2).expect("marked") > 0,
            "the pool won against them"
        );

        let mut f = Flow::new(params()).expect("valid");
        f.observe(&m, &stranger(), false, 100_000_000_000, 2_000);
        assert_eq!(f.tilt(2_000, 0).shift_e2, 0);
    }

    /// Rule 2. One fill below the floor says nothing however informed its counterparty.
    #[test]
    fn a_window_below_the_notional_floor_stays_silent() {
        let mut m = Markout::new();
        season(&mut m, informed(), 5);
        let mut f = Flow::new(params()).expect("valid");
        f.observe(&m, &informed(), false, 1_000, 2_000);
        let t = f.tilt(2_000, 0);
        assert!(t.below_floor);
        assert_eq!(t.shift_e2, 0);
    }

    #[test]
    fn informed_buying_raises_fair_value_and_informed_selling_lowers_it() {
        let mut m = Markout::new();
        season(&mut m, informed(), 5);

        let mut up = Flow::new(params()).expect("valid");
        up.observe(&m, &informed(), false, 100_000_000_000, 2_000); // taker bought base
        let t = up.tilt(2_000, 0);
        assert!(
            t.shift_e2 > 0,
            "informed takers buying means the price is going up: {t:?}"
        );

        let mut down = Flow::new(params()).expect("valid");
        down.observe(&m, &informed(), true, 100_000_000_000, 2_000); // taker sold base
        assert_eq!(down.tilt(2_000, 0).shift_e2, -t.shift_e2);
    }

    /// Rule 3. However lopsided the flow, the shift stops at the cap.
    #[test]
    fn the_shift_is_capped_and_says_so() {
        let mut m = Markout::new();
        season(&mut m, informed(), 5);
        let mut f = Flow::new(params()).expect("valid");
        for i in 0..20 {
            f.observe(&m, &informed(), false, 100_000_000_000, 2_000 + i);
        }
        let t = f.tilt(2_020, 0);
        assert_eq!(t.shift_e2, params().max_shift_bps_e2);
        assert!(t.capped);
    }

    /// Flow the public book already explains is not private information.
    #[test]
    fn a_public_book_leaning_the_same_way_shrinks_the_signal() {
        let mut m = Markout::new();
        season(&mut m, informed(), 5);

        // A gain low enough that the cap does not bind: with both runs capped, the control would
        // be invisible in the output even though it had worked.
        let uncapped = Params {
            gain_bps_e2: 200,
            ..params()
        };

        let mut alone = Flow::new(uncapped).expect("valid");
        alone.observe(&m, &informed(), false, 100_000_000_000, 2_000);
        let quiet = alone.tilt(2_000, 0);

        let mut controlled = Flow::new(uncapped).expect("valid");
        controlled.observe(&m, &informed(), false, 100_000_000_000, 2_000);
        // A bid-heavy public book explains buying pressure.
        let explained = controlled.tilt(2_000, SHIFT_SCALE);

        assert!(
            !quiet.capped && !explained.capped,
            "the cap must not mask the control"
        );
        assert!(explained.residual_e4 < quiet.residual_e4);
        assert!(explained.shift_e2 < quiet.shift_e2);
    }

    #[test]
    fn fills_outside_the_window_stop_counting() {
        let mut m = Markout::new();
        season(&mut m, informed(), 5);
        let mut f = Flow::new(params()).expect("valid");
        f.observe(&m, &informed(), false, 100_000_000_000, 2_000);
        assert!(f.tilt(2_000, 0).shift_e2 > 0);
        assert_eq!(f.tilt(2_000 + params().window_secs + 1, 0).fills, 0);
    }

    #[test]
    fn book_imbalance_signs_toward_the_heavier_side() {
        assert_eq!(book_imbalance([(300u128, 100u128)]), Some(5_000));
        assert_eq!(book_imbalance([(100u128, 300u128)]), Some(-5_000));
        assert_eq!(book_imbalance([(100u128, 100u128)]), Some(0));
    }

    /// An absent book is not a balanced one. Folding it into zero would let a dead feed read as
    /// "the public book explains nothing", which is exactly what it cannot testify to.
    #[test]
    fn an_empty_book_is_not_a_balanced_book() {
        assert_eq!(book_imbalance(std::iter::empty()), None);
        assert_eq!(book_imbalance([(0u128, 0u128)]), None);
    }

    /// Summed, not averaged: a venue quoting dust must not outvote one with real depth.
    #[test]
    fn venues_are_weighted_by_the_depth_they_actually_show() {
        // One deep venue leaning bid, one tiny venue leaning ask the other way.
        let i = book_imbalance([(1_000_000u128, 0u128), (0u128, 1u128)]).expect("sized");
        assert!(i > 9_990, "the dust venue barely moves it: {i}");
    }
}
