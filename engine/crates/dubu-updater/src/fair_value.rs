//! Micro-price, and the outlier filter in front of it.
//!
//! # Why not the mid
//!
//! The plain mid `(bid + ask) / 2` ignores the sizes resting on each side, and the sizes are
//! where the short-horizon information is. If there are 200 ETH bid and 2 ETH offered, the next
//! trade is far more likely to lift the offer than to hit the bid, and the mid says the fair
//! value is halfway between. Quoting a two-sided book around that mid means the side that gets
//! filled is systematically the wrong one: you buy just before the price rises and sell just
//! before it falls. The loss is small per fill and it is *biased*, so it does not average out —
//! it accumulates at the rate you trade.
//!
//! The size-weighted micro-price
//!
//! ```text
//! micro = (bid * askQty + ask * bidQty) / (bidQty + askQty)
//! ```
//!
//! leans toward the thin side, which is the side about to be taken. Note the crossed weighting:
//! `bidQty` multiplies the **ask**. Weighting each price by its own size gets the sign backwards
//! and is worse than the mid.
//!
//! # Arithmetic
//!
//! Exact integers throughout, at [`crate::units::FEED_SCALE`], via [`dubu_core::math::U256`].
//! The numerator is bounded by `2 * maxPrice * maxQty`, which for a six-figure asset against a
//! deep book is around `10^27` — inside `u128` in practice, but the 256-bit intermediate costs
//! nothing here and removes the need to reason about a book we have not seen yet.
//!
//! # Outlier rejection
//!
//! Three unconditional rejections, then one stateful one:
//!
//! * a **crossed or locked** book (`bid >= ask`) — never legitimate on a single venue's top of
//!   book, and it makes the micro-price meaningless rather than merely noisy;
//! * **zero depth** on either side, which either divides by zero or collapses the micro-price
//!   onto one side of the book;
//! * a **zero price**;
//! * a **jump** of more than `max_jump_bps` from the last accepted value.
//!
//! The jump filter has to concede eventually or it turns a genuine fast move into a permanent
//! outage — which is the expensive failure, because a fast move is exactly when a stale quote
//! gets picked off. So after `outlier_tolerance` consecutive rejections the tracker accepts the
//! new level. A single bad print is dropped; a real move costs `outlier_tolerance` ticks, which
//! on a liquid book is tens of milliseconds.

use dubu_core::math::{div_floor_u256, mul_div_floor, U256};

use crate::feed::BookTick;

/// Why a tick did not produce a fair value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Reject {
    /// `bid >= ask`.
    #[error("crossed or locked book: bid {bid} >= ask {ask}")]
    CrossedBook {
        /// Best bid.
        bid: u128,
        /// Best ask.
        ask: u128,
    },
    /// One or both sides had no size.
    #[error("zero depth on one side of the book")]
    ZeroDepth,
    /// A price was zero.
    #[error("zero price")]
    ZeroPrice,
    /// The arithmetic left the domain. Not reachable for any book Binance can publish.
    #[error("micro-price arithmetic left the domain")]
    Domain,
    /// Too far from the last accepted value, and the tracker has not yet conceded.
    #[error("jump of {bps} bps from {from} to {to} exceeds the limit")]
    Jump {
        /// Size of the move.
        bps: u128,
        /// Last accepted micro-price.
        from: u128,
        /// The rejected micro-price.
        to: u128,
    },
}

/// A computed fair value and the book it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FairValue {
    /// The micro-price, at [`crate::units::FEED_SCALE`].
    pub micro: u128,
    /// Best bid at the time.
    pub bid: u128,
    /// Best ask at the time.
    pub ask: u128,
    /// Top-of-book spread in bps of the mid, for logging. The pool's own half-spread should
    /// comfortably exceed half of this or the pool is quoting inside the reference venue.
    pub book_spread_bps: u128,
    /// Whether this observation was accepted only because the tracker conceded a regime shift.
    pub after_concession: bool,
}

/// The size-weighted micro-price of one book tick, with no history involved.
///
/// # Errors
/// [`Reject::CrossedBook`], [`Reject::ZeroDepth`], [`Reject::ZeroPrice`] or [`Reject::Domain`].
pub fn micro_price(t: &BookTick) -> Result<u128, Reject> {
    if t.bid == 0 || t.ask == 0 {
        return Err(Reject::ZeroPrice);
    }
    if t.bid >= t.ask {
        return Err(Reject::CrossedBook { bid: t.bid, ask: t.ask });
    }
    if t.bid_qty == 0 || t.ask_qty == 0 {
        return Err(Reject::ZeroDepth);
    }
    // Note the crossing: bid weighted by ASK size, ask weighted by BID size.
    let num = U256::mul_u128(t.bid, t.ask_qty)
        .checked_add(U256::mul_u128(t.ask, t.bid_qty))
        .ok_or(Reject::Domain)?;
    let den = t.bid_qty.checked_add(t.ask_qty).ok_or(Reject::Domain)?;
    div_floor_u256(num, U256::from_u128(den)).ok_or(Reject::Domain)
}

/// Top-of-book spread in bps of the mid.
fn book_spread_bps(bid: u128, ask: u128) -> u128 {
    let mid = (bid / 2).saturating_add(ask / 2);
    if mid == 0 {
        return 0;
    }
    mul_div_floor(ask.saturating_sub(bid), 10_000, mid).unwrap_or(0)
}

/// Stateful outlier filter over a stream of ticks for one symbol.
#[derive(Debug, Clone)]
pub struct FairValueTracker {
    max_jump_bps: u32,
    tolerance: u32,
    last: Option<u128>,
    consecutive_outliers: u32,
    accepted: u64,
    rejected: u64,
}

impl FairValueTracker {
    /// Build a tracker.
    #[must_use]
    pub const fn new(max_jump_bps: u32, tolerance: u32) -> Self {
        Self { max_jump_bps, tolerance, last: None, consecutive_outliers: 0, accepted: 0, rejected: 0 }
    }

    /// Forget the history.
    ///
    /// Called when the feed leaves [`crate::feed::FeedStatus::Live`]. Without it the first tick
    /// after an outage is measured against a price from before the outage and rejected as a
    /// jump — turning a recovered feed into `tolerance` more ticks of downtime at exactly the
    /// moment the market has moved most.
    pub fn reset(&mut self) {
        self.last = None;
        self.consecutive_outliers = 0;
    }

    /// Last accepted micro-price, if any.
    #[must_use]
    pub const fn last(&self) -> Option<u128> {
        self.last
    }

    /// Ticks accepted and rejected since construction.
    #[must_use]
    pub const fn counters(&self) -> (u64, u64) {
        (self.accepted, self.rejected)
    }

    /// Feed one tick through the filter.
    ///
    /// # Errors
    /// [`Reject`], with the stateful [`Reject::Jump`] last.
    pub fn observe(&mut self, t: &BookTick) -> Result<FairValue, Reject> {
        // `inspect_err` would read better and is stable only from 1.76; the workspace pins
        // its MSRV at 1.75 for `dubu-core`'s sake, so this stays a match.
        let micro = match micro_price(t) {
            Ok(m) => m,
            Err(e) => {
                self.rejected += 1;
                // A structurally broken book says nothing about the price level, so it must
                // not count toward conceding a regime shift.
                self.consecutive_outliers = 0;
                return Err(e);
            }
        };

        let mut after_concession = false;
        if let Some(prev) = self.last {
            let delta = micro.abs_diff(prev);
            let bps = mul_div_floor(delta, 10_000, prev).unwrap_or(u128::MAX);
            if bps > u128::from(self.max_jump_bps) {
                self.consecutive_outliers += 1;
                if self.consecutive_outliers <= self.tolerance {
                    self.rejected += 1;
                    return Err(Reject::Jump { bps, from: prev, to: micro });
                }
                // Conceded: the level really has moved.
                after_concession = true;
            }
        }

        self.consecutive_outliers = 0;
        self.last = Some(micro);
        self.accepted += 1;
        Ok(FairValue { micro, bid: t.bid, ask: t.ask, book_spread_bps: book_spread_bps(t.bid, t.ask), after_concession })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tick(bid: u128, bid_qty: u128, ask: u128, ask_qty: u128) -> BookTick {
        BookTick { update_id: 1, bid, bid_qty, ask, ask_qty }
    }

    #[test]
    fn the_micro_price_leans_toward_the_thin_side() {
        // Prices at FEED_SCALE, as they arrive: 100.00 bid, 102.00 ask, mid 101.00.
        let (bid, ask, mid) = (10_000_000_000u128, 10_200_000_000u128, 10_100_000_000u128);

        // Balanced book: the micro-price is the mid.
        assert_eq!(micro_price(&tick(bid, 10, ask, 10)), Ok(mid));

        // Ten times the size on the bid: the next trade lifts the offer, so fair value moves
        // toward the ask. (bid*1 + ask*10) / 11 = 101.8181...
        assert_eq!(micro_price(&tick(bid, 10, ask, 1)), Ok(10_181_818_181));

        let heavy_bid = micro_price(&tick(bid, 1_000, ask, 1)).unwrap();
        assert!(heavy_bid > mid, "a heavily bid book must price above the mid, got {heavy_bid}");
        assert!(heavy_bid < ask);

        // ... and the mirror.
        let heavy_ask = micro_price(&tick(bid, 1, ask, 1_000)).unwrap();
        assert!(heavy_ask < mid, "a heavily offered book must price below the mid, got {heavy_ask}");
        assert!(heavy_ask > bid);
    }

    #[test]
    fn the_weighting_is_crossed_not_self() {
        // The sign error this pins: weighting each price by its OWN size would put a heavily
        // bid book BELOW the mid. Real numbers off ETHUSDT.
        let t = tick(194_382_000_000, 2_425_800_000, 194_383_000_000, 137_811_000);
        let micro = micro_price(&t).unwrap();
        let mid = (t.bid + t.ask) / 2;
        assert!(micro > mid, "bid size dwarfs ask size, so fair value sits above the mid");
        assert!(micro < t.ask);
    }

    #[test]
    fn structurally_broken_books_are_rejected() {
        assert_eq!(micro_price(&tick(102, 1, 100, 1)), Err(Reject::CrossedBook { bid: 102, ask: 100 }));
        assert_eq!(micro_price(&tick(100, 1, 100, 1)), Err(Reject::CrossedBook { bid: 100, ask: 100 }));
        assert_eq!(micro_price(&tick(100, 0, 102, 1)), Err(Reject::ZeroDepth));
        assert_eq!(micro_price(&tick(100, 1, 102, 0)), Err(Reject::ZeroDepth));
        assert_eq!(micro_price(&tick(0, 1, 102, 1)), Err(Reject::ZeroPrice));
    }

    #[test]
    fn a_single_bad_print_is_dropped_and_the_level_is_kept() {
        let mut t = FairValueTracker::new(200, 3); // 2% jump limit
        let good = tick(194_382_000_000, 100, 194_383_000_000, 100);
        let base = t.observe(&good).unwrap().micro;

        // A print 10% away: rejected, and the tracker still holds the old level.
        let bad = tick(214_382_000_000, 100, 214_383_000_000, 100);
        assert!(matches!(t.observe(&bad), Err(Reject::Jump { .. })));
        assert_eq!(t.last(), Some(base));

        // The next normal print is accepted as if nothing happened.
        let ok = t.observe(&good).unwrap();
        assert!(!ok.after_concession);
        assert_eq!(t.counters(), (2, 1));
    }

    #[test]
    fn a_genuine_fast_move_gets_through_after_the_tolerance() {
        let mut t = FairValueTracker::new(200, 3);
        let from = tick(194_382_000_000, 100, 194_383_000_000, 100);
        t.observe(&from).unwrap();

        // A real 10% move: three ticks rejected, the fourth conceded.
        let to = tick(214_382_000_000, 100, 214_383_000_000, 100);
        for i in 0..3 {
            assert!(matches!(t.observe(&to), Err(Reject::Jump { .. })), "tick {i} should have been rejected");
        }
        let accepted = t.observe(&to).unwrap();
        assert!(accepted.after_concession, "the fourth tick must be flagged as a conceded regime shift");
        assert_eq!(t.last(), Some(accepted.micro));

        // And the counter is cleared, so the next outlier gets the full tolerance again.
        assert!(matches!(t.observe(&from), Err(Reject::Jump { .. })));
    }

    #[test]
    fn a_broken_book_does_not_count_toward_conceding() {
        // Otherwise a burst of crossed books would walk the tracker onto whatever price
        // followed them, which is the opposite of what the filter is for.
        let mut t = FairValueTracker::new(200, 2);
        t.observe(&tick(194_382_000_000, 100, 194_383_000_000, 100)).unwrap();
        for _ in 0..5 {
            assert!(matches!(t.observe(&tick(102, 1, 100, 1)), Err(Reject::CrossedBook { .. })));
        }
        let far = tick(214_382_000_000, 100, 214_383_000_000, 100);
        assert!(matches!(t.observe(&far), Err(Reject::Jump { .. })), "concession counter leaked");
    }

    #[test]
    fn reset_lets_a_recovered_feed_start_clean() {
        let mut t = FairValueTracker::new(200, 3);
        t.observe(&tick(194_382_000_000, 100, 194_383_000_000, 100)).unwrap();
        t.reset();
        assert_eq!(t.last(), None);
        // A price far from the pre-outage one is accepted immediately rather than costing
        // `tolerance` more ticks of downtime.
        let after = t.observe(&tick(300_000_000_000, 100, 300_010_000_000, 100)).unwrap();
        assert!(!after.after_concession);
    }

    #[test]
    fn the_book_spread_is_reported_in_bps() {
        // 1 unit wide on a ~1943.8 book at scale 8 is about 0.00005 bps, which floors to 0.
        assert_eq!(book_spread_bps(194_382_000_000, 194_383_000_000), 0);
        // 1% wide.
        assert_eq!(book_spread_bps(99_500_000_000, 100_500_000_000), 100);
    }
}
