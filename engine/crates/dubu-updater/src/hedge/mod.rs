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
pub mod hyperliquid;

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
    /// Largest single order, in base units. Zero means no clip.
    ///
    /// This is an EXECUTION limit, not a risk filter. Every unit of deviation is real exposure and
    /// deserves a hedge; what this bounds is how much of it may go to the venue in one order. A pool
    /// holding 3,507 ETH that has never hedged needs 3,507 ETH of hedge -- but sending it as one
    /// market order would pay for the privilege of moving the book against itself. Clipping
    /// converges over several crossings instead, one `cooloff` apart.
    pub max_order: u128,
}

impl Default for Band {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(0),
            max_drift: 0,
            min_qty: 0,
            cooloff: Duration::from_secs(2),
            max_order: 0,
        }
    }
}

/// How far a pair's net exposure sits from flat, and the rule for acting on it.
///
/// # Absolute, not incremental, and that is the whole design
///
/// This used to accumulate deltas: every observed fill was added to a running `drift` and every
/// confirmed hedge was subtracted from it. Three separate defects came out of that one choice.
///
/// * **The standing position was invisible.** A pool holding 3,507 ETH that had never hedged read
///   as drift zero, because nothing had changed *since the last crossing*. The exposure the hedge
///   exists to neutralise was exactly the exposure it could not see.
/// * **A deposit had to be special-cased.** Under an incremental model a balance that jumps looks
///   like a giant fill, so the obvious reflex is to filter it out. That reflex is wrong: a pool
///   that receives a token is long that token, and where the exposure came from is a question the
///   risk does not ask.
/// * **Error accumulated forever.** A missed observation, or a fill counted twice, moved the
///   running total permanently -- there was no reading that could correct it. That is precisely
///   how 0.04 ETH was once hedged twice into a 0.08 short that nothing could unwind.
///
/// So the deviation is now recomputed from two absolutes every cycle: what the pool holds, and what
/// the venue is short. It is self-correcting by construction -- a cycle that observes nothing
/// leaves the next cycle reading the truth rather than a stale sum -- and it needs no ledger of
/// what was sent, which is the ledger that used to be wrong.
///
/// The skew still owns absolute inventory as a slow control on the quoting *centre*. This is a fast
/// control on the venue *position*. They read the same balance and act on different things.
#[derive(Debug)]
pub struct Bands {
    pair_id: u16,
    band: Band,
    /// Net exposure: what the pool holds plus what the venue is short. Positive means long overall,
    /// so the venue must sell. Zero is flat, which is the target.
    deviation: i128,
    /// When the deviation last became actionable. See [`Bands::observe`].
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
            deviation: 0,
            since: None,
            last_sent: None,
            crossings: 0,
            suppressed_small: 0,
            suppressed_cooloff: 0,
        }
    }

    /// Record where the pool and the venue actually stand.
    ///
    /// `pool_base` is the pool's holding, read from the chain; `venue_base` is the venue position,
    /// signed (negative when short). Neither is a delta and neither is remembered from last time --
    /// that is the point. No `eth_getLogs`, no cursor, no ledger of what was sent.
    ///
    /// `now` starts the interval clock when the deviation first becomes worth acting on. Without it
    /// "never sent" reads as "the interval has already elapsed", and the first trivial deviation
    /// crosses immediately and pays a fee on nothing. The interval measures how long an exposure has
    /// been *sitting*, so it begins when the exposure does. A deviation below `min_qty` is not
    /// actionable, so it stops the clock rather than holding it open on a quantity that can never be
    /// sent.
    pub fn observe(&mut self, now: Instant, pool_base: i128, venue_base: i128) {
        let next = pool_base.saturating_add(venue_base);
        let was = self.deviation.unsigned_abs() >= self.band.min_qty && self.deviation != 0;
        let is = next.unsigned_abs() >= self.band.min_qty && next != 0;
        if is && !was {
            self.since = Some(now);
        } else if !is {
            self.since = None;
        }
        self.deviation = next;
    }

    /// Net exposure, from the pool's point of view. Positive is long.
    #[must_use]
    pub const fn deviation(&self) -> i128 {
        self.deviation
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
        let magnitude = self.deviation.unsigned_abs();
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
        // Clipped, not skipped. The rest is still real exposure and the next crossing takes the
        // next slice; what this avoids is one order large enough to move the book it is hedging in.
        let qty = if self.band.max_order > 0 {
            magnitude.min(self.band.max_order)
        } else {
            magnitude
        };
        Some(Order {
            pair_id: self.pair_id,
            side: if self.deviation > 0 {
                Side::Sell
            } else {
                Side::Buy
            },
            qty,
        })
    }

    /// Record that a crossing went out, and arm the cool-off.
    ///
    /// Nothing is deducted here, and that is the change the absolute model buys. Under the
    /// incremental one this had to subtract exactly what filled -- so a partial fill, a rejection,
    /// or a reply that never arrived left the running total wrong with no way back. Now the next
    /// [`Self::observe`] reads both sides and the deviation is simply correct again.
    ///
    /// What still matters is the cool-off. A hedge takes time to fill and longer to be reflected in
    /// the venue position, so between sending and seeing it the deviation still reads un-hedged.
    /// Firing again inside that window is how one exposure becomes two.
    pub fn settle(&mut self, now: Instant, _side: Side, _qty: u128) {
        self.last_sent = Some(now);
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
            max_order: 0,
        }
    }

    /// Flat is `pool + venue == 0`, not `pool == 0`. A market maker holding 1,000 base against a
    /// 1,000 short carries no price risk and must not trade.
    #[test]
    fn holding_inventory_against_an_equal_short_is_flat() {
        let mut b = Bands::new(1, band(100));
        let t0 = Instant::now();
        b.observe(t0, 1_000, -1_000);
        assert_eq!(b.deviation(), 0);
        assert_eq!(b.evaluate(t0 + Duration::from_secs(600)), None);
    }

    /// Small fills that cancel must not cross. This is the entire reason for a band: paying the
    /// taker fee on a buy and a sell that net to nothing is pure loss.
    #[test]
    fn offsetting_fills_never_reach_the_venue() {
        let mut b = Bands::new(1, band(100));
        let t0 = Instant::now();
        for _ in 0..20 {
            b.observe(t0, 1_030, -1_000);
            assert_eq!(
                b.evaluate(t0),
                None,
                "30 is under the cap and inside the interval"
            );
            b.observe(t0, 1_000, -1_000);
            assert_eq!(b.evaluate(t0), None);
        }
        assert_eq!(b.deviation(), 0);
        assert_eq!(b.crossings(), 0);
    }

    /// One-directional flow past the cap must cross, in the opposite direction to the pool's fill.
    #[test]
    fn one_sided_flow_crosses_the_other_way() {
        let mut b = Bands::new(1, band(100));
        let t0 = Instant::now();
        b.observe(t0, 1_060, -1_000);
        assert_eq!(
            b.evaluate(t0),
            None,
            "under the cap and inside the interval"
        );
        b.observe(t0, 1_120, -1_000);
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

    /// A pool that has held a position since before the hedge existed is exposed by exactly that
    /// position, and the incremental model could not see it: nothing had *changed*, so drift read
    /// zero while 3,507 ETH sat unhedged. This is the regression that motivated the rewrite.
    #[test]
    fn a_standing_position_is_exposure_even_though_nothing_moved() {
        let mut b = Bands::new(1, band(100));
        let t0 = Instant::now();
        b.observe(t0, 3_507, 0);
        assert_eq!(b.deviation(), 3_507);
        assert_eq!(
            b.evaluate(t0).map(|o| (o.side, o.qty)),
            Some((Side::Sell, 3_507)),
            "never traded, never hedged, fully exposed"
        );
    }

    /// A deposit is exposure. Receiving a token makes the pool long it, and where the length came
    /// from is a question the risk does not ask -- so this must hedge, not filter.
    #[test]
    fn a_deposit_is_hedged_like_any_other_length() {
        let mut b = Bands::new(1, band(100));
        let t0 = Instant::now();
        b.observe(t0, 1_000, -1_000);
        assert_eq!(b.evaluate(t0), None, "flat");

        b.observe(t0, 2_000, -1_000);
        assert_eq!(
            b.evaluate(t0).map(|o| (o.side, o.qty)),
            Some((Side::Sell, 1_000)),
            "1,000 arrived; 1,000 of price risk arrived with it"
        );
    }

    /// The interval fires on its own, without the cap. This is the fee-optimal path: a deviation too
    /// small to be urgent still gets cleared once holding it stops being cheaper than the fee.
    #[test]
    fn the_interval_clears_a_deviation_the_cap_would_never_reach() {
        let mut b = Bands::new(1, band(10_000));
        let t0 = Instant::now();
        b.observe(t0, 1_005, -1_000);
        assert_eq!(
            b.evaluate(t0),
            None,
            "the clock starts here, so nothing is due yet"
        );
        assert!(
            b.evaluate(t0 + Duration::from_secs(46)).is_some(),
            "and it is due once the deviation has sat for the interval"
        );

        // The venue took it, so the next reading is flat -- and the clock stops on its own.
        let t1 = t0 + Duration::from_secs(46);
        b.settle(t1, Side::Sell, 5);
        b.observe(t1, 1_005, -1_005);
        assert_eq!(b.deviation(), 0);
        assert_eq!(
            b.evaluate(t0 + Duration::from_secs(200)),
            None,
            "nothing to cross"
        );
    }

    /// The cap does not wait for the interval. A burst is exactly the case where holding for
    /// fee-efficiency is the wrong trade.
    #[test]
    fn the_cap_overrides_the_interval() {
        let mut b = Bands::new(1, band(100));
        let t0 = Instant::now();
        b.observe(t0, 1_020, -1_000);
        assert_eq!(b.evaluate(t0), None, "small and fresh");
        b.observe(t0, 1_520, -1_000);
        assert!(
            b.evaluate(t0 + Duration::from_millis(1)).is_some(),
            "past the cap, so the interval does not hold it"
        );
    }

    /// A partial fill leaves its remainder exposed, and nothing has to remember that. The venue
    /// moved by what filled, so the next reading simply shows what is left.
    ///
    /// Under the incremental model this was the dangerous case: `settle` had to subtract exactly
    /// what filled, and subtracting what was *requested* instead left the pool believing it was flat
    /// while still carrying the difference.
    #[test]
    fn a_partial_fill_leaves_the_remainder_behind() {
        let mut b = Bands::new(1, band(100));
        let t0 = Instant::now();
        b.observe(t0, 1_150, -1_000);
        assert_eq!(b.evaluate(t0).expect("past the cap").qty, 150);

        b.settle(t0, Side::Sell, 90);
        b.observe(t0, 1_150, -1_090);
        assert_eq!(
            b.deviation(),
            60,
            "60 still unhedged, read rather than remembered"
        );
    }

    /// The reading is the truth, so a hedge sent twice by mistake corrects itself on the next cycle
    /// instead of poisoning a running total forever. This is the 0.04-ETH-hedged-twice incident.
    #[test]
    fn an_over_hedge_is_corrected_rather_than_accumulated() {
        let mut b = Bands::new(1, band(1));
        let t0 = Instant::now();
        b.observe(t0, 1_040, -1_000);
        assert_eq!(b.evaluate(t0).expect("past the cap").qty, 40);

        // Sent twice: the venue is now 80 short against 40 of length.
        b.observe(t0, 1_040, -1_080);
        assert_eq!(b.deviation(), -40, "over-hedged, and visibly so");
        assert_eq!(
            b.evaluate(t0 + Duration::from_secs(46)).map(|o| o.side),
            Some(Side::Buy),
            "the correction is just the next crossing, in the other direction"
        );
    }

    /// The clip bounds the order, never the exposure. What is left over is still owed and comes out
    /// on the following crossings.
    #[test]
    fn a_large_position_converges_in_clipped_orders() {
        let mut b = Bands::new(
            1,
            Band {
                interval: Duration::from_secs(45),
                max_drift: 100,
                min_qty: 1,
                cooloff: Duration::ZERO,
                max_order: 1_000,
            },
        );
        let t0 = Instant::now();
        let pool: i128 = 3_507;
        let mut venue: i128 = 0;
        let mut sent = 0u32;
        for _ in 0..10 {
            b.observe(t0, pool, venue);
            let Some(o) = b.evaluate(t0) else { break };
            assert!(o.qty <= 1_000, "no order may exceed the clip");
            venue -= i128::try_from(o.qty).unwrap();
            b.settle(t0, o.side, o.qty);
            sent += 1;
        }
        assert_eq!(sent, 4, "3,507 in clips of 1,000");
        assert_eq!(pool + venue, 0, "and it lands exactly flat");
    }

    /// The cool-off stops a second crossing before the first is reflected. Without it a slow venue
    /// turns one deviation into two positions in the same direction.
    #[test]
    fn the_cooloff_stops_doubling_the_position() {
        let mut b = Bands::new(
            1,
            Band {
                interval: Duration::from_secs(45),
                max_drift: 100,
                min_qty: 1,
                cooloff: Duration::from_secs(5),
                max_order: 0,
            },
        );
        let t0 = Instant::now();
        b.observe(t0, 1_150, -1_000);
        assert!(b.evaluate(t0).is_some(), "past the cap");
        b.settle(t0, Side::Sell, 150);

        // The venue has not reflected the fill yet, so the deviation still reads un-hedged.
        b.observe(t0, 1_150, -1_000);
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
                max_order: 0,
            },
        );
        let t0 = Instant::now();
        b.observe(t0, 1_050, -1_000);
        assert_eq!(
            b.evaluate(t0),
            None,
            "past the cap, but under the venue minimum"
        );
        assert_eq!(b.suppressed_small(), 1);
        assert_eq!(b.deviation(), 50, "still owed");
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
                max_order: 0,
            },
        );
        b.observe(Instant::now(), 1_000_000, 0);
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
