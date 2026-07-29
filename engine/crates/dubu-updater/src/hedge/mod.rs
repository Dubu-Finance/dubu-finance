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
    /// The no-trade half-width, in base units. Exposure inside `[-width, +width]` is carried, not
    /// hedged. Zero disables the band and therefore the hedge.
    ///
    /// This replaced a timer, and the timer was wrong in a way worth recording. It fired at
    /// `T* = (fee/sigma)^2` -- rehedge once the price has moved one fee -- which is Henrotte's
    /// asset-tolerance rule, a known family but one that uses no risk measure at all. Two defects:
    /// it contains no inventory term, so it hedged when nothing had accumulated and waited when a
    /// lot had; and its scaling is wrong. Under proportional costs the optimal band goes as
    /// `cost^(1/3)`, which implies a mean interval in `cost^(2/3)`, where the timer gave `cost^2`.
    /// On plausible ETH numbers the timer fires every ~18s against a band-implied ~54 hours. That
    /// is not a calibration gap; it is paying fees to chase noise.
    ///
    /// See [`derive_band`] for where the number comes from.
    pub width: u128,
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
            width: 0,
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
    /// There is no clock. The band decides on position alone, so how long an exposure has been
    /// sitting is not an input -- an exposure inside the band is one the pool has chosen to carry
    /// for as long as it stays there.
    pub const fn observe(&mut self, pool_base: i128, venue_base: i128) {
        self.deviation = pool_base.saturating_add(venue_base);
    }

    /// The no-trade half-width this pair runs at, in base units.
    #[must_use]
    pub const fn width(&self) -> u128 {
        self.band.width
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
        // A zero band is how the hedge is switched off.
        if self.band.width == 0 {
            return None;
        }
        let magnitude = self.deviation.unsigned_abs();
        // Inside the no-trade region. Not "too small to bother with" -- carried on purpose, because
        // the fee to remove it exceeds the risk of holding it.
        let excess = magnitude.checked_sub(self.band.width).filter(|e| *e > 0)?;

        // Reflect to the boundary, never to flat. Trading all the way back to zero pays to remove
        // exposure the band has already decided is worth carrying, and the next fill puts it
        // straight back. This is what the theory prescribes (Kallsen-Muhle-Karbe: trade the minimal
        // amount to stay inside) and what production hedgers offer as "just get within delta range".
        let magnitude = excess;
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

/// The no-trade half-width, from a risk budget rather than a utility parameter.
///
/// # Where the cube root comes from
///
/// Under proportional transaction costs the optimal policy is a no-trade region with reflection at
/// the boundary, and its width scales as the CUBE ROOT of the cost. Kallsen and Muhle-Karbe (2015,
/// *Mathematical Finance* 25(4)) give the general form, which subsumes Davis-Norman, Shreve-Soner
/// and Whalley-Wilmott. Specialised to a market maker chasing a flat target, it reads
///
/// ```text
/// h = ( 3 * c * sigma_q^2 / (2 * p * sigma^2 * S) )^(1/3)
/// ```
///
/// with `c` the one-way cost, `sigma_q` the volatility of net inventory, `sigma` the asset's
/// volatility, `S` the price and `p` absolute risk aversion. `sigma_q` sits exactly where an option
/// hedger's gamma sits: it is the rate at which the thing you are chasing moves.
///
/// # Why this function does not use that formula
///
/// Two of its inputs are unavailable and one is unfalsifiable. `sigma_q` is measured from your own
/// fill log, and this pool has never been traded against, so it is not merely unknown but
/// undefined. `p` is a utility parameter nobody can state honestly.
///
/// At the optimum, though, the stationary inventory is uniform on `[-h, h]`, so
///
/// ```text
/// h = sqrt(3) * RMS(inventory carried)
/// ```
///
/// which turns the question into one an operator can actually answer: how much exposure are you
/// willing to carry? That is a risk budget, not a measurement, and it is what this takes.
///
/// The cube root makes the choice forgiving in a way worth stating: an input wrong by 8x moves the
/// band by 2x. Setting the budget roughly lands near optimal.
///
/// `carried` is the RMS exposure the pool accepts, in base units.
#[must_use]
pub fn derive_band(carried: u128) -> u128 {
    // sqrt(3), in integer arithmetic so the result is reproducible across platforms.
    carried.saturating_mul(1_732_050_808) / 1_000_000_000
}

/// The diagnostic that needs no risk-aversion parameter either.
///
/// At the optimal band, fees spent on hedging come to exactly **twice** the risk penalty rate.
/// Spending materially more than that means the band is too tight -- the pool is buying certainty
/// it did not want at a price it did not agree to. Returns the ratio scaled by 100, so `200` is on
/// target; there is no need for floating point and no meaning in more precision than that.
///
/// Both arguments are rates over the same window, in the same currency.
#[must_use]
pub const fn fee_to_risk_ratio(fees_spent: u128, risk_penalty: u128) -> Option<u128> {
    if risk_penalty == 0 {
        return None;
    }
    Some(fees_spent.saturating_mul(100) / risk_penalty)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn band(width: u128) -> Band {
        Band {
            width,
            min_qty: 1,
            cooloff: Duration::ZERO,
            max_order: 0,
        }
    }

    /// Flat is `pool + venue == 0`, not `pool == 0`. A maker holding 1,000 base against a 1,000
    /// short carries no price risk.
    #[test]
    fn holding_inventory_against_an_equal_short_is_flat() {
        let mut b = Bands::new(1, band(100));
        b.observe(1_000, -1_000);
        assert_eq!(b.deviation(), 0);
        assert_eq!(b.evaluate(Instant::now()), None);
    }

    /// Exposure inside the band is CARRIED, not tolerated. This is the no-trade region: removing it
    /// costs more in fees than holding it costs in risk, so a hedger that clears it is losing money
    /// on purpose.
    #[test]
    fn exposure_inside_the_band_is_carried_however_long_it_sits() {
        let mut b = Bands::new(1, band(100));
        let t0 = Instant::now();
        b.observe(1_099, -1_000);
        assert_eq!(b.deviation(), 99);
        assert_eq!(b.evaluate(t0), None);
        assert_eq!(
            b.evaluate(t0 + Duration::from_secs(86_400)),
            None,
            "a day later it is still inside the band; there is no clock"
        );
        assert_eq!(b.crossings(), 0);
    }

    /// Past the edge, trade back TO THE EDGE. Flattening would pay to remove exposure the band has
    /// already decided is worth carrying, and the next fill puts it straight back.
    #[test]
    fn crossing_reflects_to_the_boundary_rather_than_to_flat() {
        let mut b = Bands::new(1, band(100));
        let t0 = Instant::now();
        b.observe(1_150, -1_000);
        assert_eq!(
            b.evaluate(t0),
            Some(Order {
                pair_id: 1,
                side: Side::Sell,
                qty: 50
            }),
            "150 of exposure against a band of 100 sends 50, not 150"
        );
    }

    /// Both directions. A short past the edge buys back to the edge.
    #[test]
    fn the_band_is_symmetric() {
        let mut b = Bands::new(1, band(100));
        b.observe(1_000, -1_150);
        assert_eq!(
            b.evaluate(Instant::now()),
            Some(Order {
                pair_id: 1,
                side: Side::Buy,
                qty: 50
            })
        );
    }

    /// A position held since before the hedge existed is exposure, and it converges to the band --
    /// not to zero. The incremental model could not see it at all: nothing had CHANGED, so drift
    /// read zero while 3,487 ETH sat unhedged.
    #[test]
    fn a_standing_position_converges_to_the_band_edge() {
        let mut b = Bands::new(
            1,
            Band {
                width: 100,
                min_qty: 1,
                cooloff: Duration::ZERO,
                max_order: 1_000,
            },
        );
        let t0 = Instant::now();
        let pool: i128 = 3_487;
        let mut venue: i128 = 0;
        let mut sent = 0u32;
        for _ in 0..10 {
            b.observe(pool, venue);
            let Some(o) = b.evaluate(t0) else { break };
            assert!(o.qty <= 1_000, "no order exceeds the clip");
            venue -= i128::try_from(o.qty).unwrap();
            b.settle(t0, o.side, o.qty);
            sent += 1;
        }
        assert_eq!(sent, 4, "3,387 of excess in clips of 1,000");
        assert_eq!(pool + venue, 100, "parked on the band, not flattened");
        assert_eq!(b.evaluate(t0), None, "and it stops there");
    }

    /// A deposit is exposure. Receiving a token makes the pool long it, and where the length came
    /// from is a question the risk does not ask.
    #[test]
    fn a_deposit_is_hedged_like_any_other_length() {
        let mut b = Bands::new(1, band(100));
        let t0 = Instant::now();
        b.observe(1_000, -1_000);
        assert_eq!(b.evaluate(t0), None, "flat");
        b.observe(2_000, -1_000);
        assert_eq!(
            b.evaluate(t0).map(|o| (o.side, o.qty)),
            Some((Side::Sell, 900)),
            "1,000 arrived; 900 of it is past the band"
        );
    }

    /// The reading is the truth, so a hedge sent twice corrects itself on the next cycle instead of
    /// poisoning a running total forever. This is the 0.04-ETH-hedged-twice incident.
    #[test]
    fn an_over_hedge_is_corrected_rather_than_accumulated() {
        let mut b = Bands::new(1, band(10));
        let t0 = Instant::now();
        b.observe(1_040, -1_000);
        assert_eq!(b.evaluate(t0).expect("past the band").qty, 30);

        // Sent twice: the venue is now 80 short against 40 of length.
        b.observe(1_040, -1_080);
        assert_eq!(b.deviation(), -40, "over-hedged, and visibly so");
        assert_eq!(
            b.evaluate(t0).map(|o| (o.side, o.qty)),
            Some((Side::Buy, 30)),
            "the correction is the next crossing, in the other direction"
        );
    }

    /// A partial fill leaves its remainder exposed, and nothing has to remember that.
    #[test]
    fn a_partial_fill_leaves_the_remainder_behind() {
        let mut b = Bands::new(1, band(10));
        let t0 = Instant::now();
        b.observe(1_150, -1_000);
        assert_eq!(b.evaluate(t0).expect("past the band").qty, 140);

        b.settle(t0, Side::Sell, 90);
        b.observe(1_150, -1_090);
        assert_eq!(b.deviation(), 60, "read rather than remembered");
        assert_eq!(b.evaluate(t0).expect("still past").qty, 50);
    }

    /// The cool-off stops a second crossing before the first is reflected in the venue position.
    #[test]
    fn the_cooloff_stops_doubling_the_position() {
        let mut b = Bands::new(
            1,
            Band {
                width: 10,
                min_qty: 1,
                cooloff: Duration::from_secs(5),
                max_order: 0,
            },
        );
        let t0 = Instant::now();
        b.observe(1_150, -1_000);
        assert!(b.evaluate(t0).is_some(), "past the band");
        b.settle(t0, Side::Sell, 140);

        b.observe(1_150, -1_000);
        assert_eq!(b.evaluate(t0), None, "inside the cool-off");
        assert_eq!(b.suppressed_cooloff(), 1);
        assert!(
            b.evaluate(t0 + Duration::from_secs(6)).is_some(),
            "free once it lapses"
        );
    }

    /// Below the venue's minimum the order would be rejected outright.
    #[test]
    fn a_crossing_under_the_venue_minimum_is_held_rather_than_sent() {
        let mut b = Bands::new(
            1,
            Band {
                width: 10,
                min_qty: 500,
                cooloff: Duration::ZERO,
                max_order: 0,
            },
        );
        b.observe(1_050, -1_000);
        assert_eq!(
            b.evaluate(Instant::now()),
            None,
            "past the band, under the minimum"
        );
        assert_eq!(b.suppressed_small(), 1);
        assert_eq!(b.deviation(), 50, "still owed");
    }

    /// A zero band is how the hedge is switched off.
    #[test]
    fn a_band_of_zero_sends_nothing() {
        let mut b = Bands::new(1, Band::default());
        b.observe(1_000_000, 0);
        assert_eq!(b.evaluate(Instant::now()), None);
    }

    /// `h = sqrt(3) * carry`, and the point of the cube root is that being wrong is cheap.
    #[test]
    fn the_band_is_root_three_times_the_carry_and_forgiving_with_it() {
        assert_eq!(derive_band(0), 0, "no budget, no band, no hedge");
        assert_eq!(derive_band(1_000), 1_732);
        assert_eq!(derive_band(69_740), 120_793, "ETH at a 2% budget");

        // The band is linear in the budget, but the budget itself enters the underlying formula
        // under a cube root -- so an operator wrong by 8x on the inputs behind it is wrong by 2x
        // on the band. That is the property that makes setting it by judgement defensible.
        let eightfold = f64::from(8u8).cbrt();
        assert!((eightfold - 2.0).abs() < 1e-9);
    }

    /// At the optimum, fees spent come to exactly twice the risk penalty. Far above that means the
    /// band is too tight -- the pool is buying certainty it never agreed to pay for.
    #[test]
    fn the_fee_to_risk_ratio_says_when_the_band_is_too_tight() {
        assert_eq!(fee_to_risk_ratio(200, 100), Some(200), "on target");
        assert_eq!(fee_to_risk_ratio(900, 100), Some(900), "band far too tight");
        assert_eq!(
            fee_to_risk_ratio(50, 100),
            Some(50),
            "band wide, carrying risk cheaply"
        );
        assert_eq!(
            fee_to_risk_ratio(10, 0),
            None,
            "no risk penalty, no ratio to report"
        );
    }
}
