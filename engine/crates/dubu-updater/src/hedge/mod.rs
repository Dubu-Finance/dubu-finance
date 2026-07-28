//! Neutralising the inventory a fill leaves behind, on a venue that will actually take the other
//! side.
//!
//! # Why this exists, in one number
//!
//! Without it the pool's price schedule has to pay for holding whatever it buys until somebody
//! happens to sell it back. That cost is `gamma * sigma^2 * holding_time`, and `holding_time` with
//! no hedge is *unbounded* — so the schedule is priced defensively and the pool quotes a 25 bp
//! slope where the measured cost of actually unwinding 1,000 ETH on Binance is **3.8 bp**. Six
//! times too steep, and the whole gap is compensation for a risk that a hedge removes.
//!
//! # Not every fill
//!
//! Hedging each fill separately pays the taker fee each time, and small fills mostly cancel: one
//! taker sells, the next buys, and the net inventory never moved. So this tracks the *net* drift
//! since the last hedge and only crosses when it leaves a band.
//!
//! # What is derivable, and what is not
//!
//! The obvious thing to derive is the band *width*, and it cannot be done: the optimal width is
//! `sigma_flow * sqrt(fee / risk_cost)`, and `sigma_flow` -- how fast inventory drifts -- is a
//! property of order flow, which is exactly what there is no data for.
//!
//! What is derivable is the **interval**. Holding a drift for `T` seconds risks `sigma * sqrt(T)`
//! of it; clearing it costs `fee`. Crossing more often than the point where those are equal spends
//! more on fees than the exposure was worth, so:
//!
//! ```text
//!   T* = (fee / sigma_per_sqrt_sec)^2
//! ```
//!
//! Both inputs are known -- fee is a rate card, sigma is measured -- and at a 4 bp taker fee
//! against ETHUSDT's 0.594 bp per root-second that is about **45 seconds**. Whatever drift
//! accumulates in that window is the band, which means the width adapts to flow without anyone
//! having to know what the flow is.
//!
//! A hard cap on drift sits on top, and that one is a risk choice rather than a derivation: it
//! bounds what a burst can build up before the interval elapses.
//!
//! # What a hedge does not fix
//!
//! It removes *variance*, not *expected loss to someone who knew more*. A taker who hits a stale
//! quote costs the pool `reference_move` whether the position is held or crossed out immediately —
//! hedging only decides when that loss is realised. Adverse selection is answered by re-quoting
//! faster, not by hedging faster, and the two are separate levers on separate terms.

use std::time::{Duration, Instant};

pub mod binance;

/// Which way the hedge has to go on the venue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// The pool bought base, so the venue must sell it.
    Sell,
    /// The pool sold base, so the venue must buy it back.
    Buy,
}

impl Side {
    /// Binance's spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sell => "SELL",
            Self::Buy => "BUY",
        }
    }
}

/// An order the band decided to send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Order {
    /// Which pair, in this crate's numbering.
    pub pair_id: u16,
    /// Direction on the venue.
    pub side: Side,
    /// Base units to trade, in the base token's own decimals.
    pub qty: u128,
}

/// How a pair's band is configured.
#[derive(Debug, Clone, Copy)]
pub struct Band {
    /// Don't cross more often than this. Derived -- see [`derive_hedge_interval`].
    pub interval: Duration,
    /// Cross regardless of the interval once drift reaches this, in base units. A risk choice, not
    /// a derivation: it bounds what a burst can accumulate before the interval elapses.
    pub max_drift: u128,
    /// The venue's minimum order size, in base units. A crossing below this is not sent — it would
    /// be rejected and the drift would be counted as hedged when it was not.
    pub min_qty: u128,
    /// Don't send again within this window. A hedge takes time to fill and to be reflected; firing
    /// again before then doubles the position rather than correcting it.
    pub cooloff: Duration,
}

impl Default for Band {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(0),
            max_drift: 0,
            min_qty: 0,
            cooloff: Duration::from_secs(2),
        }
    }
}

/// The net position a pair has drifted since its last hedge, and the rule for acting on it.
///
/// Deliberately signed and *relative*: this tracks drift since the last crossing, not absolute
/// inventory. Absolute inventory is the skew's job and is a slow control on the quoting centre;
/// this is a fast control on the venue position, and conflating them would make one of the two
/// wrong every time the other acted.
#[derive(Debug)]
pub struct Bands {
    pair_id: u16,
    band: Band,
    /// Positive: the pool holds more base than at the last hedge, so the venue must sell.
    drift: i128,
    /// When the current drift started accumulating. See [`Bands::observe`].
    since: Option<Instant>,
    last_sent: Option<Instant>,
    crossings: u64,
    suppressed_small: u64,
    suppressed_cooloff: u64,
}

impl Bands {
    /// A pair with no drift recorded.
    #[must_use]
    pub const fn new(pair_id: u16, band: Band) -> Self {
        Self {
            pair_id,
            band,
            drift: 0,
            since: None,
            last_sent: None,
            crossings: 0,
            suppressed_small: 0,
            suppressed_cooloff: 0,
        }
    }

    /// Record a fill's effect on inventory.
    ///
    /// `base_delta` is signed from the **pool's** point of view: positive when the pool received
    /// base. The hedge is always the opposite sign.
    ///
    /// `now` starts the interval clock on the first fill after a crossing. Without it "never sent"
    /// reads as "the interval has already elapsed", and the very first fill -- however trivial --
    /// crosses immediately and pays a fee on nothing. The interval measures how long a drift has
    /// been *sitting*, so it has to begin when the drift does.
    pub fn observe(&mut self, now: Instant, base_delta: i128) {
        if self.drift == 0 && base_delta != 0 {
            self.since = Some(now);
        }
        self.drift = self.drift.saturating_add(base_delta);
    }

    /// Net drift since the last crossing, from the pool's point of view.
    #[must_use]
    pub const fn drift(&self) -> i128 {
        self.drift
    }

    /// Crossings sent since start.
    #[must_use]
    pub const fn crossings(&self) -> u64 {
        self.crossings
    }

    /// Crossings the minimum-size rule held back.
    #[must_use]
    pub const fn suppressed_small(&self) -> u64 {
        self.suppressed_small
    }

    /// Crossings the cool-off held back.
    #[must_use]
    pub const fn suppressed_cooloff(&self) -> u64 {
        self.suppressed_cooloff
    }

    /// Decide whether the drift now warrants crossing.
    ///
    /// Returns the order to send, or `None`. Nothing is deducted here: the caller confirms with
    /// [`Self::settle`] once the venue has actually taken it, because a rejected or unfilled order
    /// that had already been deducted would leave the pool believing it was flat when it was not.
    #[must_use]
    pub fn evaluate(&mut self, now: Instant) -> Option<Order> {
        // Both zero is how the hedge is switched off. Either alone still arms the other trigger.
        if self.band.interval.is_zero() && self.band.max_drift == 0 {
            return None;
        }
        let magnitude = self.drift.unsigned_abs();
        if magnitude == 0 {
            return None;
        }

        // Two triggers, and the cap is the one that does not wait. A burst can build a position
        // worth clearing long before the fee-optimal interval elapses, and the interval exists to
        // stop over-paying fees, not to hold risk through a move.
        let capped = self.band.max_drift > 0 && magnitude >= self.band.max_drift;
        // Measured from when this drift started accumulating, not from the last crossing. `None`
        // here means no drift is outstanding, which the magnitude check above has already ruled
        // out -- so a missing clock is a bug rather than a licence to cross.
        let due = !self.band.interval.is_zero()
            && self
                .since
                .is_some_and(|t| now.saturating_duration_since(t) >= self.band.interval);
        if !capped && !due {
            return None;
        }

        if magnitude < self.band.min_qty {
            self.suppressed_small = self.suppressed_small.saturating_add(1);
            return None;
        }
        if self
            .last_sent
            .is_some_and(|t| now.saturating_duration_since(t) < self.band.cooloff)
        {
            self.suppressed_cooloff = self.suppressed_cooloff.saturating_add(1);
            return None;
        }
        Some(Order {
            pair_id: self.pair_id,
            side: if self.drift > 0 {
                Side::Sell
            } else {
                Side::Buy
            },
            qty: magnitude,
        })
    }

    /// Confirm that the venue took `qty` in `side`, and remove exactly that much from the drift.
    ///
    /// `qty` is what actually filled, which is not always what was asked for. A partial fill leaves
    /// the remainder in the drift, where the next evaluation sees it — which is the whole reason
    /// this is separate from [`Self::evaluate`].
    pub fn settle(&mut self, now: Instant, side: Side, qty: u128) {
        let signed = i128::try_from(qty).unwrap_or(i128::MAX);
        self.drift = match side {
            Side::Sell => self.drift.saturating_sub(signed),
            Side::Buy => self.drift.saturating_add(signed),
        };
        self.last_sent = Some(now);
        self.since = if self.drift == 0 { None } else { Some(now) };
        self.crossings = self.crossings.saturating_add(1);
    }
}

/// The band width that balances fee against the volatility of holding the drift.
///
/// Both inputs are known without any flow data, which is what makes this the one tunable in the
/// spread model that does not need markout:
///
/// * `fee_bps_e2` — the venue's taker fee, from its rate card.
/// * `sigma_millibps_per_sqrt_sec` — measured by [`crate::skew::Volatility`].
/// * `seconds_between_hedges` — how long a crossing is expected to be apart from the next, which
///   sets how long the drift sits exposed.
///
/// Widening trades fee for exposure: cross half as often and pay half the fee, but sit on twice
/// the position. The balance is where the marginal fee saved equals the marginal variance taken,
/// which for a random walk is where the band equals `sqrt(fee / (sigma^2 * t))` in relative terms.
///
/// Returned in base units against `epoch_base`, so the caller gets something it can compare to a
/// drift directly rather than a ratio it has to re-scale.
#[must_use]
pub fn derive_hedge_interval(fee_bps_e2: u32, sigma_millibps_per_sqrt_sec: u64) -> Duration {
    if sigma_millibps_per_sqrt_sec == 0 || fee_bps_e2 == 0 {
        return Duration::ZERO;
    }
    let fee = f64::from(fee_bps_e2) / 1_000_000.0;
    let sigma = sigma_millibps_per_sqrt_sec as f64 / 10_000_000.0;
    let secs = (fee / sigma).powi(2);
    // An hour is already far past the point where the interval is the binding constraint; beyond
    // it the drift cap is doing all the work and a larger number only hides that.
    Duration::from_secs_f64(secs.clamp(0.0, 3600.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn band(max_drift: u128) -> Band {
        Band {
            interval: Duration::from_secs(45),
            max_drift,
            min_qty: 1,
            cooloff: Duration::ZERO,
        }
    }

    /// Small fills that cancel must not cross. This is the entire reason for a band: paying the
    /// taker fee on a buy and a sell that net to nothing is pure loss.
    #[test]
    fn offsetting_fills_never_reach_the_venue() {
        let mut b = Bands::new(1, band(100));
        let t0 = Instant::now();
        let t = t0;
        for _ in 0..20 {
            b.observe(t0, 30);
            b.observe(t0, -30);
            assert_eq!(b.evaluate(t), None);
        }
        assert_eq!(b.drift(), 0);
        assert_eq!(b.crossings(), 0);
    }

    /// One-directional flow past the cap must cross, in the opposite direction to the pool's fill.
    #[test]
    fn one_sided_flow_crosses_the_other_way() {
        let mut b = Bands::new(1, band(100));
        let t0 = Instant::now();
        b.observe(t0, 60);
        assert_eq!(
            b.evaluate(t0),
            None,
            "under the cap and inside the interval"
        );
        b.observe(t0, 60);
        assert_eq!(
            b.evaluate(t0),
            Some(Order {
                pair_id: 1,
                side: Side::Sell,
                qty: 120
            }),
            "the pool bought base, so the venue sells"
        );
    }

    /// The interval fires on its own, without the cap. This is the fee-optimal path: a drift too
    /// small to be urgent still gets cleared once holding it stops being cheaper than the fee.
    #[test]
    fn the_interval_clears_a_drift_the_cap_would_never_reach() {
        let mut b = Bands::new(1, band(10_000));
        let t0 = Instant::now();
        b.observe(t0, 5);
        assert_eq!(
            b.evaluate(t0),
            None,
            "the clock starts at this fill, so nothing is due yet"
        );
        assert!(
            b.evaluate(t0 + Duration::from_secs(46)).is_some(),
            "and it is due once the drift has sat for the interval"
        );
        b.settle(t0 + Duration::from_secs(46), Side::Sell, 5);
        assert_eq!(b.drift(), 0);

        // Cleared to flat, so the clock stops until the next fill restarts it.
        assert_eq!(
            b.evaluate(t0 + Duration::from_secs(200)),
            None,
            "no drift, nothing to cross"
        );
    }

    /// The cap does not wait for the interval. A burst is exactly the case where holding for
    /// fee-efficiency is the wrong trade.
    #[test]
    fn the_cap_overrides_the_interval() {
        let mut b = Bands::new(1, band(100));
        let t0 = Instant::now();
        b.observe(t0, 20);
        assert_eq!(b.evaluate(t0), None, "small and fresh");
        b.observe(t0, 500);
        assert!(
            b.evaluate(t0 + Duration::from_millis(1)).is_some(),
            "past the cap, so the interval does not hold it"
        );
    }

    /// A partial fill leaves its remainder in the drift. Deducting the requested amount instead
    /// would leave the pool believing it was flat while still carrying the difference.
    #[test]
    fn a_partial_fill_leaves_the_remainder_behind() {
        let mut b = Bands::new(1, band(100));
        let t0 = Instant::now();
        b.observe(t0, 150);
        assert_eq!(b.evaluate(t0).expect("past the cap").qty, 150);

        b.settle(t0, Side::Sell, 90);
        assert_eq!(b.drift(), 60, "60 still unhedged");
        assert_eq!(
            b.evaluate(t0),
            None,
            "under the cap and inside the interval"
        );
    }

    /// The cool-off stops a second crossing before the first is reflected. Without it a slow venue
    /// turns one drift into two positions in the same direction.
    #[test]
    fn the_cooloff_stops_doubling_the_position() {
        let mut b = Bands::new(
            1,
            Band {
                interval: Duration::from_secs(45),
                max_drift: 100,
                min_qty: 1,
                cooloff: Duration::from_secs(5),
            },
        );
        let t0 = Instant::now();
        b.observe(t0, 150);
        assert!(b.evaluate(t0).is_some(), "past the cap");
        b.settle(t0, Side::Sell, 150);

        b.observe(t0, 150);
        assert_eq!(b.evaluate(t0), None, "inside the cool-off");
        assert_eq!(b.suppressed_cooloff(), 1);
        assert!(
            b.evaluate(t0 + Duration::from_secs(6)).is_some(),
            "and free once it lapses"
        );
    }

    /// Below the venue's minimum the order would be rejected. Sending it anyway and settling on
    /// the request would silently discard real inventory.
    #[test]
    fn a_crossing_under_the_venue_minimum_is_held_rather_than_sent() {
        let mut b = Bands::new(
            1,
            Band {
                interval: Duration::from_secs(45),
                max_drift: 10,
                min_qty: 500,
                cooloff: Duration::ZERO,
            },
        );
        let t0 = Instant::now();
        b.observe(t0, 50);
        assert_eq!(
            b.evaluate(t0),
            None,
            "past the cap, but under the venue minimum"
        );
        assert_eq!(b.suppressed_small(), 1);
        assert_eq!(b.drift(), 50, "still owed");
    }

    /// Both triggers off is how the hedge is disabled.
    #[test]
    fn a_band_with_neither_trigger_sends_nothing() {
        let mut b = Bands::new(
            1,
            Band {
                interval: Duration::ZERO,
                max_drift: 0,
                min_qty: 1,
                cooloff: Duration::ZERO,
            },
        );
        b.observe(Instant::now(), 1_000_000);
        assert_eq!(b.evaluate(Instant::now()), None);
    }

    /// The derived interval is the one number here that needs no flow data, and it has to move the
    /// right way in both inputs or it is decoration.
    ///
    /// At ETHUSDT's measured 0.594 bp per root-second against a 4 bp taker fee it lands near 45
    /// seconds: below that, clearing costs more in fees than the exposure was worth.
    #[test]
    fn the_derived_interval_trades_fee_against_volatility() {
        let base = derive_hedge_interval(400, 594);
        assert!(
            base.as_secs() >= 40 && base.as_secs() <= 50,
            "expected ~45s, got {base:?}"
        );

        let dearer = derive_hedge_interval(800, 594);
        assert!(dearer > base, "a costlier fee is worth waiting longer for");

        let wilder = derive_hedge_interval(400, 1_200);
        assert!(wilder < base, "a wilder market is worth crossing sooner");

        assert_eq!(
            derive_hedge_interval(400, 0),
            Duration::ZERO,
            "no sigma, no interval"
        );
        assert_eq!(
            derive_hedge_interval(0, 594),
            Duration::ZERO,
            "no fee, no interval"
        );
    }
}
