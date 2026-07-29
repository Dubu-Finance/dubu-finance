//! Neutralising the inventory a fill leaves behind, on a venue that will actually take the other
//! side.
//!
//! Unhedged, holding costs `gamma * sigma^2 * holding_time` with `holding_time` *unbounded*, so the
//! schedule is priced defensively: the pool quotes a 25 bp slope where the measured cost of
//! unwinding 1,000 ETH on Binance is **3.8 bp**. The whole 6x gap pays for a risk a hedge removes.
//! Not per fill, though -- that pays the taker fee each time and small fills mostly cancel, so this
//! tracks *net* exposure and crosses only when it leaves a band.
//!
//! The band *width* is not derivable: the optimum is `sigma_flow * sqrt(fee / risk_cost)`, and
//! `sigma_flow` is a property of order flow, which is exactly what there is no data for. The
//! **interval** is. Holding a drift for `T` seconds risks `sigma * sqrt(T)` of it and clearing it
//! costs `fee`, so crossing more often than `T* = (fee / sigma_per_sqrt_sec)^2` spends more on fees
//! than the exposure was worth. Both inputs are known -- fee is a rate card, sigma is measured --
//! and at a 4 bp taker fee against ETHUSDT's 0.594 bp per root-second that is about **45 seconds**,
//! so the width adapts to flow without anyone knowing what the flow is. The hard cap on drift on
//! top is a risk choice, not a derivation: it bounds what a burst builds up before that elapses.
//!
//! A hedge removes *variance*, not expected loss to someone who knew more: a taker hitting a stale
//! quote costs `reference_move` whether the position is held or crossed out at once. Adverse
//! selection is answered by re-quoting faster, and the two are separate levers.

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
        let spelling = match self {
            Self::Sell => "SELL",
            Self::Buy => "BUY",
        };
        // The venue matches this verbatim; a malformed side is a rejected order, not a wrong one.
        assert!(!spelling.is_empty());
        assert!(spelling.len() <= 4);
        spelling
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
    /// hedged. Zero disables the band and therefore the hedge. See [`derive_band`].
    ///
    /// It replaced a timer at `T* = (fee/sigma)^2` -- Henrotte's asset-tolerance rule -- which had
    /// no inventory term and the wrong scaling: under proportional costs the optimal band goes as
    /// `cost^(1/3)`, implying an interval in `cost^(2/3)` where the timer gave `cost^2`. On
    /// plausible ETH numbers that fires every ~18s against a band-implied ~54 hours.
    pub width: u128,
    /// The venue's minimum order size, in base units. A crossing below this is not sent — it would
    /// be rejected and the drift would be counted as hedged when it was not.
    pub qty_min: u128,
    /// Don't send again within this window. A hedge takes time to fill and to be reflected; firing
    /// again before then doubles the position rather than correcting it.
    pub cooloff: Duration,
    /// Largest single order, in base units. Zero means no clip.
    ///
    /// An EXECUTION limit, not a risk filter: every unit of deviation gets hedged. A pool holding
    /// 3,507 ETH that has never hedged needs 3,507 ETH of hedge, but sending that as one market
    /// order pays to move the book against itself. Clipping converges over several crossings
    /// instead, one `cooloff` apart.
    pub order_max: u128,
}

impl Default for Band {
    fn default() -> Self {
        let band = Self {
            width: 0,
            qty_min: 0,
            cooloff: Duration::from_secs(2),
            order_max: 0,
        };
        // The default is the hedge switched OFF: a default that crossed would trade an unconfigured
        // pair, which is the wrong direction to be wrong in.
        assert!(band.width == 0);
        assert!(!band.cooloff.is_zero());
        band
    }
}

/// How far a pair's net exposure sits from flat, and the rule for acting on it.
///
/// Recomputed from two absolutes every cycle -- what the pool holds and what the venue is short --
/// rather than accumulated from deltas. Accumulating hid the standing position (3,507 ETH that had
/// never been hedged read as drift zero, because nothing had changed *since the last crossing*),
/// needed a special case for deposits (a pool that receives a token is long it, and where the
/// exposure came from is a question the risk does not ask), and let error persist: one fill counted
/// twice moved the running total forever, which is how 0.04 ETH became a 0.08 short nothing could
/// unwind. Absolute is self-correcting instead, and needs no ledger of what was sent.
///
/// The skew owns absolute inventory as a slow control on the quoting *centre*; this is a fast
/// control on the venue *position*. Same balance, different things.
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
        let bands = Self {
            pair_id,
            band,
            deviation: 0,
            last_sent: None,
            crossings: 0,
            suppressed_small: 0,
            suppressed_cooloff: 0,
        };
        // A fresh pair is flat: the first `observe` sets the exposure, never construction.
        assert!(bands.deviation == 0);
        assert!(bands.crossings == 0);
        assert!(bands.last_sent.is_none());
        bands
    }

    /// Record where the pool and the venue actually stand.
    ///
    /// `pool_base` is the pool's holding; `venue_base` is the venue position, signed (negative when
    /// short). Neither is a delta and neither is remembered -- that is the point: no `eth_getLogs`,
    /// no cursor, no ledger. There is no clock either, so an exposure inside the band is one the
    /// pool carries for as long as it stays there.
    pub const fn observe(&mut self, pool_base: i128, venue_base: i128) {
        self.deviation = pool_base.saturating_add(venue_base);
        // Saturating cannot wrap, and the sign is the part the venue side depends on: a deviation
        // whose sign flipped would hedge the wrong way and double the exposure.
        if pool_base >= 0 && venue_base >= 0 {
            assert!(self.deviation >= 0);
        }
        if pool_base <= 0 && venue_base <= 0 {
            assert!(self.deviation <= 0);
        }
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
    /// Returns the order to send, or `None`. Nothing is deducted here: a rejected or unfilled order
    /// already deducted would leave the pool believing it was flat when it was not.
    #[must_use]
    pub fn evaluate(&mut self, now: Instant) -> Option<Order> {
        // A zero band is how the hedge is switched off.
        if self.band.width == 0 {
            return None;
        }
        let exposure = self.deviation.unsigned_abs();
        // Inside the band is carried on purpose: the fee to remove it exceeds the risk of holding
        // it. Past the edge, reflect TO the boundary rather than to flat -- trading back to zero
        // pays to remove exposure the band already decided is worth carrying, and the next fill
        // puts it straight back. Kallsen-Muhle-Karbe prescribe the minimal trade that stays inside;
        // production hedgers sell the same thing as "just get within delta range".
        let magnitude = exposure.checked_sub(self.band.width).filter(|e| *e > 0)?;
        assert!(magnitude > 0);
        assert!(magnitude <= exposure);
        // Past the band in magnitude means off flat in sign, and the sign picks the venue side.
        assert!(self.deviation != 0);

        if magnitude < self.band.qty_min {
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
        let qty = clip(magnitude, self.band.order_max);
        // The bound that matters, restated on the value that actually leaves: an order can never
        // exceed the deviation that justified it.
        assert!(qty > 0);
        assert!(qty <= exposure);
        // Guarded, because a clip below the venue minimum is a misconfiguration to be read in the
        // rejection, not a reason to take the process down mid-cycle.
        if self.band.order_max == 0 || self.band.order_max >= self.band.qty_min {
            assert!(qty >= self.band.qty_min);
        }
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
    /// Nothing is deducted here, and that is what the absolute model buys: a partial fill, a
    /// rejection, or a reply that never arrived all correct themselves on the next
    /// [`Self::observe`], which reads both sides rather than adjusting a running total.
    ///
    /// What still matters is the cool-off. A hedge takes time to fill and longer to be reflected in
    /// the venue position, so between sending and seeing it the deviation still reads un-hedged.
    /// Firing again inside that window is how one exposure becomes two.
    pub fn settle(&mut self, now: Instant, _side: Side, _qty: u128) {
        let before = self.crossings;
        self.last_sent = Some(now);
        self.crossings = self.crossings.saturating_add(1);
        // `last_sent` arms the cool-off; unset, the next cycle crosses the same exposure again.
        assert!(self.last_sent.is_some());
        if before < u64::MAX {
            assert!(self.crossings == before + 1);
        }
    }
}

/// Bound one order to the venue's largest single order. Zero means no clip.
///
/// The remainder is not skipped: it is still real exposure and the next crossing takes the next
/// slice. What this avoids is one order large enough to move the book it is hedging in.
fn clip(magnitude: u128, order_max: u128) -> u128 {
    assert!(magnitude > 0);
    let qty = if order_max > 0 {
        magnitude.min(order_max)
    } else {
        magnitude
    };
    assert!(qty > 0);
    assert!(qty <= magnitude);
    if order_max > 0 {
        assert!(qty <= order_max);
    }
    qty
}

/// The no-trade half-width, from a risk budget rather than a utility parameter.
///
/// Under proportional transaction costs the optimal policy is a no-trade region with reflection at
/// the boundary, and its width scales as the CUBE ROOT of the cost. Kallsen and Muhle-Karbe (2015,
/// *Mathematical Finance* 25(4)) give the general form -- subsuming Davis-Norman, Shreve-Soner and
/// Whalley-Wilmott -- which for a maker chasing a flat target reads
///
/// ```text
/// h = ( 3 * c * sigma_q^2 / (2 * p * sigma^2 * S) )^(1/3)
/// ```
///
/// with `c` the one-way cost, `sigma_q` the volatility of net inventory, `sigma` the asset's
/// volatility, `S` the price and `p` absolute risk aversion.
///
/// That formula is unusable here: `sigma_q` is measured from your own fill log and this pool has
/// never been traded against, so it is undefined rather than unknown, and `p` is a utility
/// parameter nobody can state honestly. At the optimum the stationary inventory is uniform on
/// `[-h, h]`, so
///
/// ```text
/// h = sqrt(3) * RMS(inventory carried)
/// ```
///
/// which asks instead how much exposure the operator is willing to carry -- a risk budget, and what
/// `carried` is, in base units. The cube root makes that forgiving: an input wrong by 8x moves the
/// band by 2x.
#[must_use]
pub fn derive_band(carried: u128) -> u128 {
    // sqrt(3), in integer arithmetic so the result is reproducible across platforms. The bracket
    // is checked at compile time, because the bounds asserted on `width` below rest on it.
    const ROOT_3_E9: u128 = 1_732_050_808;
    const ONE_E9: u128 = 1_000_000_000;
    const _: () = assert!(ROOT_3_E9 > ONE_E9);
    const _: () = assert!(ROOT_3_E9 < 2 * ONE_E9);

    let width = carried.saturating_mul(ROOT_3_E9) / ONE_E9;
    // No budget, no band, no hedge -- and a budget of any size must produce a band it brackets,
    // because sqrt(3) lies between 1 and 2. Skipped where the multiply saturated, which is a
    // budget no pool holds.
    if carried == 0 {
        assert!(width == 0);
    }
    if carried <= u128::MAX / ROOT_3_E9 {
        assert!(width >= carried);
        assert!(width <= carried.saturating_mul(2));
    }
    width
}

/// The diagnostic that needs no risk-aversion parameter either.
///
/// At the optimal band, fees spent on hedging come to exactly **twice** the risk penalty rate.
/// Spending materially more than that means the band is too tight -- the pool is buying certainty
/// it did not want at a price it did not agree to. Returns the ratio scaled by 100, so `200` is on
/// target; there is no meaning in more precision than that.
///
/// Both arguments are rates over the same window, in the same currency.
#[must_use]
pub const fn fee_to_risk_ratio(fees_spent: u128, risk_penalty: u128) -> Option<u128> {
    if risk_penalty == 0 {
        return None;
    }
    // The zero case returned above, so the divisor below cannot be zero.
    assert!(risk_penalty > 0);
    let ratio = fees_spent.saturating_mul(100) / risk_penalty;
    // Spending nothing must read as nothing, or an operator sees a band that looks too tight when
    // the hedge has not crossed at all.
    if fees_spent == 0 {
        assert!(ratio == 0);
    }
    Some(ratio)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn band(width: u128) -> Band {
        Band {
            width,
            qty_min: 1,
            cooloff: Duration::ZERO,
            order_max: 0,
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
                qty_min: 1,
                cooloff: Duration::ZERO,
                order_max: 1_000,
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
                qty_min: 1,
                cooloff: Duration::from_secs(5),
                order_max: 0,
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
                qty_min: 500,
                cooloff: Duration::ZERO,
                order_max: 0,
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
