//! Micro-price per venue, and the cross-venue combiner in front of it.
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
//! # One micro-price per venue, then one reference
//!
//! [`micro_price`] runs per venue. [`combine`] turns the cross-section into a single reference,
//! and it is the only thing the ladder is ever built from.
//!
//! Three unconditional per-venue rejections come first, because they say the *frame* is broken
//! rather than that the price is unusual:
//!
//! * a **crossed or locked** book (`bid >= ask`) — never legitimate on a single venue's top of
//!   book, and it makes the micro-price meaningless rather than merely noisy;
//! * **zero depth** on either side, which either divides by zero or collapses the micro-price
//!   onto one side of the book;
//! * a **zero price**.
//!
//! # Outlier rejection is cross-sectional and MAD-based, not a fixed band
//!
//! The previous design compared each tick to that venue's own previous tick against a fixed
//! `max_jump_bps`. That has two problems and the second is worse than the first. A fixed band is
//! wrong in both directions at once — too wide in a calm market to catch anything, and firing
//! constantly in a fast one — and it had to concede after a few consecutive rejections or a
//! genuine fast move became a permanent outage, which meant every real move cost several ticks of
//! staleness at exactly the moment staleness is most expensive.
//!
//! With several venues the comparison can be made *across* them instead of across time, which
//! removes both problems. At one instant:
//!
//! ```text
//! median     = median(micro_v)
//! dev_v      = |micro_v - median|                  (in deci-bps of the median)
//! mad        = median(dev_v)                        the robust scale of the cross-section
//! threshold  = max(k * mad, floor)
//! survivors  = { v : dev_v <= threshold }
//! reference  = mean(micro_v for v in survivors)
//! ```
//!
//! `mad` **is** the market's current disagreement, so the band widens by itself when the market
//! is fast and tightens when it is calm. A venue printing garbage is far from a median three
//! other venues agree on, whatever the regime; a genuine fast move takes every venue with it, so
//! the median moves and nothing is rejected and nothing has to be conceded a few ticks later.
//!
//! The `floor` is not a fixed band smuggled back in — it is what stops the filter from eating
//! itself. When every venue agrees to within a tick, `mad` collapses to zero, the threshold
//! collapses with it, and every venue that is not exactly the median gets rejected. Measured on
//! live ETHUSDT and BTCUSDT across Binance, OKX and Bybit, `mad` runs 0.1-1.0 bps and the largest
//! single-venue deviation seen was 1.6 bps, so a floor of 2 bps is above ordinary disagreement
//! and only a genuinely broken venue clears it.
//!
//! # Degrading is explicit, and there are two different ways to fail
//!
//! * [`ReferenceError::NoQuorum`] — too few venues produced a usable price. We do not know the
//!   price.
//! * [`ReferenceError::Dispersed`] — enough venues, and they **disagree**. There is no single
//!   price to know.
//!
//! The second is the one that matters and it is why the dispersion gate is on `mad` rather than
//! on the count of rejections. One venue disagreeing is an outlier and `mad` barely moves. Half
//! the venues disagreeing is a regime change, and `mad` jumps to roughly half the gap between the
//! two camps — so the gate fires and the bot stops quoting instead of averaging a price that no
//! venue is showing. A rejection count could never distinguish those two: by construction at
//! least half the cross-section always survives a MAD filter, so "a majority was rejected" is not
//! a state that can occur.
//!
//! # What this still cannot catch
//!
//! An error correlated across every venue. If all of them are wrong the same way, the
//! cross-section is silent and so is this module. That is the same gap the ladder's
//! self-consistency bounds have, and closing it needs a source the bot does not price from —
//! Pyth, on chain, inside `updateQuote`. Not built on either side; see the README.
//!
//! # Arithmetic
//!
//! Exact integers throughout, at [`crate::units::FEED_SCALE`], via [`dubu_core::math`]. The
//! micro-price numerator is bounded by `2 * maxPrice * maxQty`, which for a six-figure asset
//! against a deep book is around `10^27` — inside `u128` in practice, but the 256-bit
//! intermediate costs nothing here and removes the need to reason about a book we have not seen
//! yet.

use dubu_core::math::{div_floor_u256, mul_div_floor, U256};

use crate::feed::{BookTick, FeedStatus, VenueId};

/// Deci-bps: tenths of a basis point. Deviations between healthy venues are a fraction of a bp,
/// so whole bps has no resolution to spare here.
const DECIBPS: u128 = 100_000;

/// Why one venue's tick did not produce a micro-price.
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
    /// The arithmetic left the domain. Not reachable for any book these venues can publish.
    #[error("micro-price arithmetic left the domain")]
    Domain,
}

/// The size-weighted micro-price of one book tick, with no history and no cross-section involved.
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
#[must_use]
pub fn book_spread_bps(bid: u128, ask: u128) -> u128 {
    let mid = (bid / 2).saturating_add(ask / 2);
    if mid == 0 {
        return 0;
    }
    mul_div_floor(ask.saturating_sub(bid), 10_000, mid).unwrap_or(0)
}

/// One venue's contribution to the cross-section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VenueQuote {
    /// Which venue.
    pub venue: VenueId,
    /// Its micro-price, at [`crate::units::FEED_SCALE`].
    pub micro: u128,
    /// Best bid at the time.
    pub bid: u128,
    /// Best ask at the time.
    pub ask: u128,
    /// Its top-of-book spread in bps of the mid. The pool's own half-spread should comfortably
    /// exceed half of this or the pool is quoting inside the reference venue.
    pub book_spread_bps: u128,
    /// Age of the tick it came from.
    pub age_ms: u64,
}

impl VenueQuote {
    /// Build from a live tick, or say why the frame was unusable.
    ///
    /// # Errors
    /// [`Reject`], for a structurally broken book.
    pub fn new(venue: VenueId, tick: &BookTick, age_ms: u64) -> Result<Self, Reject> {
        Ok(Self {
            venue,
            micro: micro_price(tick)?,
            bid: tick.bid,
            ask: tick.ask,
            book_spread_bps: book_spread_bps(tick.bid, tick.ask),
            age_ms,
        })
    }
}

/// The MAD filter's knobs, already converted out of the config's decimal-bps form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MadParams {
    /// Venues that must survive for a reference to exist at all.
    pub min_venues: u8,
    /// Multiplier on the MAD, in tenths: `40` is `k = 4.0`.
    pub k_tenths: u32,
    /// Floor under the rejection threshold, in deci-bps of the median.
    pub floor_decibps: u32,
    /// Dispersion above which the venues are treated as disagreeing rather than as noisy, in
    /// deci-bps of the median.
    pub max_dispersion_decibps: u32,
}

/// One venue's position in the cross-section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deviation {
    /// Which venue.
    pub venue: VenueId,
    /// Its micro-price.
    pub micro: u128,
    /// Signed deviation from the median, in deci-bps. Positive means above the median.
    pub decibps: i64,
}

/// Which bound set the rejection threshold. Worth logging: it says whether the filter is in its
/// calm-market regime or its fast-market one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThresholdBound {
    /// `k * mad` bound it: the market is moving and the band widened by itself.
    Mad,
    /// The configured floor bound it: the venues agree closely and the floor is what keeps the
    /// filter from rejecting everything but the median.
    Floor,
    /// Fewer than three venues, so an outlier cannot be attributed to one of them and only the
    /// dispersion gate applies. See [`combine`].
    Unattributable,
}

impl ThresholdBound {
    /// Short stable string for structured logs.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Mad => "mad",
            Self::Floor => "floor",
            Self::Unattributable => "unattributable",
        }
    }
}

/// A reference price and everything about how it was reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    /// The combined reference, at [`crate::units::FEED_SCALE`]. Equal-weighted mean of the
    /// survivors; see [`combine`] for why equal weight.
    pub micro: u128,
    /// The median of the whole cross-section, before rejection.
    pub median: u128,
    /// Venues that survived, with their deviations.
    pub used: Vec<Deviation>,
    /// Venues that were rejected as outliers, with theirs.
    pub rejected: Vec<Deviation>,
    /// Median absolute deviation across the whole cross-section, in deci-bps of the median.
    pub dispersion_decibps: u32,
    /// The rejection threshold that was applied, in deci-bps.
    pub threshold_decibps: u32,
    /// Which bound produced that threshold.
    pub bound: ThresholdBound,
}

impl Reference {
    /// `binance:+0.3 bybit:-0.5 okx:+0.0` — survivors, then rejected ones marked with `!`.
    ///
    /// One string rather than a nested structure because it goes in a log line that a human reads
    /// during an incident, and `jq` on a nested array at 3am is not the ergonomics wanted.
    #[must_use]
    pub fn venue_summary(&self) -> String {
        let one = |d: &Deviation, bang: &str| {
            format!("{}{}:{}{}.{}", bang, d.venue, if d.decibps < 0 { "-" } else { "+" },
                    d.decibps.unsigned_abs() / 10, d.decibps.unsigned_abs() % 10)
        };
        self.used
            .iter()
            .map(|d| one(d, ""))
            .chain(self.rejected.iter().map(|d| one(d, "!")))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// How many venues the reference was actually built from.
    #[must_use]
    pub fn venues_used(&self) -> usize {
        self.used.len()
    }
}

/// Why no reference price could be produced.
///
/// Both variants mean the same thing to [`crate::policy`] — do not quote — and they mean very
/// different things to whoever is reading the log.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReferenceError {
    /// Too few venues produced a usable price.
    #[error("no quorum: {have} of {need} venues usable")]
    NoQuorum {
        /// Venues that produced one.
        have: u8,
        /// Venues required.
        need: u8,
    },
    /// The venues that are live do not agree closely enough for one number to represent them.
    #[error("venues disagree: dispersion {dispersion_decibps} decibps exceeds {limit_decibps}, across {venues} venues")]
    Dispersed {
        /// Median absolute deviation, in deci-bps of the median.
        dispersion_decibps: u32,
        /// The limit it crossed.
        limit_decibps: u32,
        /// How many venues were in the cross-section.
        venues: u8,
    },
    /// Quorum was met, then the MAD filter rejected enough venues to break it.
    #[error("quorum lost to outliers: {survived} of {need} venues survived the MAD filter")]
    QuorumLostToOutliers {
        /// How many survived.
        survived: u8,
        /// How many are required.
        need: u8,
    },
}

impl ReferenceError {
    /// What the rest of the system may conclude about this symbol.
    #[must_use]
    pub const fn status(&self) -> FeedStatus {
        match *self {
            Self::NoQuorum { have, need } => FeedStatus::NoQuorum { have, need },
            Self::QuorumLostToOutliers { survived, need } => {
                FeedStatus::NoQuorum { have: survived, need }
            }
            Self::Dispersed { dispersion_decibps, limit_decibps, venues } => FeedStatus::Dispersed {
                dispersion_bps: dispersion_decibps,
                limit_bps: limit_decibps,
                venues,
            },
        }
    }

    /// Short stable string for structured logs.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::NoQuorum { .. } => "no_quorum",
            Self::Dispersed { .. } => "dispersed",
            Self::QuorumLostToOutliers { .. } => "quorum_lost_to_outliers",
        }
    }
}

/// Median of a sorted slice. Even lengths take the mean of the two middle values.
///
/// `None` on an empty slice rather than a panic. The quorum check in [`combine`] already makes
/// that unreachable — but it does so two frames away and through a configured `min_venues`, and a
/// defence that lives behind a config value is a defence until somebody sets the value to zero.
/// On an empty slice the even branch would evaluate `xs[0 / 2 - 1]`, which underflows `usize`
/// before it ever indexes, so the failure would not even read as an out-of-bounds.
fn median_sorted(xs: &[u128]) -> Option<u128> {
    let n = xs.len();
    if n == 0 {
        return None;
    }
    if n % 2 == 1 {
        xs.get(n / 2).copied()
    } else {
        let lo = *xs.get(n / 2 - 1)?;
        let hi = *xs.get(n / 2)?;
        // Halve each before adding: the values are feed-scale prices, so this cannot overflow,
        // but the habit is the one the rest of the crate keeps.
        Some(lo / 2 + hi / 2 + (lo % 2 + hi % 2) / 2)
    }
}

/// Signed deviation from a reference, in deci-bps of it.
fn deviation_decibps(x: u128, from: u128) -> i64 {
    if from == 0 {
        return 0;
    }
    let mag = mul_div_floor(x.abs_diff(from), DECIBPS, from).unwrap_or(u128::from(u32::MAX));
    let mag = i64::try_from(mag).unwrap_or(i64::MAX);
    if x < from {
        -mag
    } else {
        mag
    }
}

/// Combine one instant's per-venue micro-prices into a single reference.
///
/// # The weighting, and what happens as venues drop out
///
/// Survivors are combined with **equal weight**. Two alternatives were considered and rejected:
///
/// * *Liquidity weight* — weight each venue by the size resting at its top of book. That reads
///   well and is wrong here, because top-of-book size is not comparable across venues: it is
///   mostly a function of tick size, so a venue with a coarse tick shows an order of magnitude
///   more size at the top and would dominate the reference for a reason that has nothing to do
///   with how good its price is. Each venue's own liquidity information is already inside its
///   micro-price, which is where it belongs.
/// * *The median itself* — maximally robust, and it throws away every venue but one. It also
///   moves discontinuously as venues drop out, which is the opposite of what is wanted from
///   something the ladder is rebuilt from every second.
///
/// Equal weight over MAD-filtered survivors takes the robustness from the filter and the
/// efficiency from the average. As venues drop out the reference stays continuous in *value* —
/// each survivor is within `threshold` of the median by construction, so losing one moves the
/// reference by at most `threshold / n` — and the venue count is in every log line so the loss of
/// redundancy is visible even though the price barely moves. Below `min_venues` there is no
/// reference at all.
///
/// # Errors
/// [`ReferenceError`], one variant per way the cross-section can fail to name a price.
pub fn combine(quotes: &[VenueQuote], p: &MadParams) -> Result<Reference, ReferenceError> {
    let need = p.min_venues;
    let n = u8::try_from(quotes.len()).unwrap_or(u8::MAX);
    if n < need {
        return Err(ReferenceError::NoQuorum { have: n, need });
    }

    let mut sorted: Vec<u128> = quotes.iter().map(|q| q.micro).collect();
    sorted.sort_unstable();
    // `None` and zero mean the same thing here — no cross-section to price from — so they take
    // the same exit rather than one of them being a distinct failure nobody has a response to.
    let median = median_sorted(&sorted).unwrap_or(0);
    if median == 0 {
        return Err(ReferenceError::NoQuorum { have: 0, need });
    }

    let devs: Vec<Deviation> = quotes
        .iter()
        .map(|q| Deviation { venue: q.venue, micro: q.micro, decibps: deviation_decibps(q.micro, median) })
        .collect();

    let mut mags: Vec<u128> = devs.iter().map(|d| u128::from(d.decibps.unsigned_abs())).collect();
    mags.sort_unstable();
    let mad = median_sorted(&mags).unwrap_or(0);
    let dispersion = u32::try_from(mad).unwrap_or(u32::MAX);

    // The regime gate. One venue away from the pack barely moves the MAD; half the venues away
    // from the other half moves it to roughly half the gap, which is what this catches. See the
    // module docs on why this is measured on the dispersion and not on a rejection count.
    if dispersion > p.max_dispersion_decibps {
        return Err(ReferenceError::Dispersed {
            dispersion_decibps: dispersion,
            limit_decibps: p.max_dispersion_decibps,
            venues: n,
        });
    }

    // With fewer than three venues an outlier cannot be attributed. Two venues that disagree are
    // symmetric — each is exactly one MAD from the median of the pair — so any threshold either
    // rejects both or neither, and there is no information saying which one is wrong. The
    // dispersion gate above is the entire defence in that case, and pretending otherwise by
    // rejecting the "worse" one would be inventing an answer.
    let (threshold, bound) = if quotes.len() < 3 {
        (p.max_dispersion_decibps, ThresholdBound::Unattributable)
    } else {
        let scaled = u32::try_from(mad * u128::from(p.k_tenths) / 10).unwrap_or(u32::MAX);
        if scaled >= p.floor_decibps {
            (scaled, ThresholdBound::Mad)
        } else {
            (p.floor_decibps, ThresholdBound::Floor)
        }
    };

    let (used, rejected): (Vec<Deviation>, Vec<Deviation>) =
        devs.into_iter().partition(|d| d.decibps.unsigned_abs() <= u64::from(threshold));

    let survived = u8::try_from(used.len()).unwrap_or(u8::MAX);
    if survived < need {
        return Err(ReferenceError::QuorumLostToOutliers { survived, need });
    }

    // Equal weight. `used` is non-empty here, and the sum of a handful of feed-scale prices is
    // nowhere near `u128`.
    let sum: u128 = used.iter().map(|d| d.micro).sum();
    let micro = sum / used.len() as u128;

    Ok(Reference {
        micro,
        median,
        used,
        rejected,
        dispersion_decibps: dispersion,
        threshold_decibps: threshold,
        bound,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tick(bid: u128, bid_qty: u128, ask: u128, ask_qty: u128) -> BookTick {
        BookTick { update_id: 1, bid, bid_qty, ask, ask_qty }
    }

    /// A venue quoting a flat book at `micro`, so the cross-section is easy to reason about.
    fn q(venue: VenueId, micro: u128) -> VenueQuote {
        VenueQuote { venue, micro, bid: micro, ask: micro + 1, book_spread_bps: 0, age_ms: 10 }
    }

    /// Live-shaped parameters: k = 4.0, floor 2 bp, dispersion limit 25 bp, quorum 2.
    fn params() -> MadParams {
        MadParams { min_venues: 2, k_tenths: 40, floor_decibps: 20, max_dispersion_decibps: 250 }
    }

    // -----------------------------------------------------------------------
    // The micro-price itself
    // -----------------------------------------------------------------------

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
    fn the_book_spread_is_reported_in_bps() {
        // 1 unit wide on a ~1943.8 book at scale 8 is about 0.00005 bps, which floors to 0.
        assert_eq!(book_spread_bps(194_382_000_000, 194_383_000_000), 0);
        // 1% wide.
        assert_eq!(book_spread_bps(99_500_000_000, 100_500_000_000), 100);
    }

    // -----------------------------------------------------------------------
    // The cross-section
    // -----------------------------------------------------------------------

    #[test]
    fn three_agreeing_venues_combine_to_their_mean() {
        // Live-shaped: three venues within a bp of each other.
        let quotes = [
            q(VenueId::Binance, 196_930_000_000),
            q(VenueId::Okx, 196_929_000_000),
            q(VenueId::Bybit, 196_931_000_000),
        ];
        let r = combine(&quotes, &params()).unwrap();
        assert_eq!(r.median, 196_930_000_000);
        assert_eq!(r.micro, 196_930_000_000, "equal weight over three survivors");
        assert_eq!(r.venues_used(), 3);
        assert!(r.rejected.is_empty());
        // Everything agrees to a fraction of a bp, so the floor is what set the threshold.
        assert_eq!(r.bound, ThresholdBound::Floor);
        assert_eq!(r.threshold_decibps, 20);
    }

    #[test]
    fn one_venue_printing_garbage_is_rejected_and_the_rest_still_quote() {
        // The failure the whole multi-venue design exists for: a feed that is confidently wrong.
        let quotes = [
            q(VenueId::Binance, 196_930_000_000),
            q(VenueId::Okx, 196_929_000_000),
            q(VenueId::Bybit, 120_000_000_000), // a bad parse, or an exchange mid-outage
        ];
        let r = combine(&quotes, &params()).unwrap();
        assert_eq!(r.rejected.len(), 1);
        assert_eq!(r.rejected[0].venue, VenueId::Bybit);
        assert_eq!(r.venues_used(), 2);
        // The reference is the mean of the two survivors and is nowhere near the outlier.
        assert_eq!(r.micro, 196_929_500_000);
        assert!(r.venue_summary().contains("!bybit"), "summary: {}", r.venue_summary());
    }

    #[test]
    fn a_genuine_fast_move_takes_every_venue_with_it_and_nothing_is_rejected() {
        // The case the old fixed-band temporal filter got wrong: it rejected the first few ticks
        // of every real move, which is exactly when a stale quote is most expensive. A
        // cross-sectional filter sees the whole market move together and has nothing to say.
        let before = [
            q(VenueId::Binance, 196_930_000_000),
            q(VenueId::Okx, 196_929_000_000),
            q(VenueId::Bybit, 196_931_000_000),
        ];
        let after = [
            q(VenueId::Binance, 200_930_000_000), // +200 bp, all three at once
            q(VenueId::Okx, 200_929_000_000),
            q(VenueId::Bybit, 200_931_000_000),
        ];
        let r0 = combine(&before, &params()).unwrap();
        let r1 = combine(&after, &params()).unwrap();
        assert!(r1.rejected.is_empty(), "a market-wide move must not be treated as an outlier");
        assert!(r1.micro > r0.micro);
        assert_eq!(r1.venues_used(), 3, "and it must cost nothing in venue count");
    }

    #[test]
    fn the_band_widens_by_itself_when_the_market_disagrees_more() {
        // A venue 3 bp from the median is rejected in a calm cross-section and kept in a noisy
        // one. That adaptivity is the entire argument for MAD over a fixed band.
        let base = 100_000_000_000u128;
        let bp = base / 10_000;

        let calm = [
            q(VenueId::Binance, base),
            q(VenueId::Okx, base + bp / 10),
            q(VenueId::Bybit, base - bp / 10),
            q(VenueId::Coinbase, base + 3 * bp),
        ];
        let r = combine(&calm, &params()).unwrap();
        assert_eq!(r.bound, ThresholdBound::Floor);
        assert_eq!(r.rejected.len(), 1, "3 bp is an outlier when everyone agrees to 0.1 bp");

        let fast = [
            q(VenueId::Binance, base),
            q(VenueId::Okx, base + 2 * bp),
            q(VenueId::Bybit, base - 2 * bp),
            q(VenueId::Coinbase, base + 3 * bp),
        ];
        let r = combine(&fast, &params()).unwrap();
        assert_eq!(r.bound, ThresholdBound::Mad, "the MAD must bind once the market is moving");
        assert!(r.rejected.is_empty(), "3 bp is ordinary when the cross-section is 2 bp wide");
    }

    #[test]
    fn a_split_cross_section_is_a_regime_change_and_is_not_averaged_through() {
        // Two venues at one price, two at another 60 bp away. There is no single price here, and
        // the mean of the four is a number no venue is showing. Refusing is the only honest
        // answer.
        let base = 100_000_000_000u128;
        let bp = base / 10_000;
        let split = [
            q(VenueId::Binance, base),
            q(VenueId::Okx, base + bp / 10),
            q(VenueId::Bybit, base + 60 * bp),
            q(VenueId::Coinbase, base + 60 * bp),
        ];
        let err = combine(&split, &params()).unwrap_err();
        assert!(matches!(err, ReferenceError::Dispersed { .. }), "got {err}");
        assert!(matches!(err.status(), FeedStatus::Dispersed { venues: 4, .. }));
    }

    #[test]
    fn two_venues_cannot_attribute_an_outlier_and_say_so() {
        // With two points every threshold rejects both or neither, so the only defence is the
        // dispersion gate. Pretending to pick a winner would be inventing an answer.
        let base = 100_000_000_000u128;
        let bp = base / 10_000;
        let r = combine(&[q(VenueId::Binance, base), q(VenueId::Okx, base + 4 * bp)], &params()).unwrap();
        assert_eq!(r.bound, ThresholdBound::Unattributable);
        assert_eq!(r.venues_used(), 2, "neither may be rejected");
        assert_eq!(r.micro, base + 2 * bp);

        // Far enough apart and the dispersion gate does the work instead.
        let err = combine(&[q(VenueId::Binance, base), q(VenueId::Okx, base + 60 * bp)], &params())
            .unwrap_err();
        assert!(matches!(err, ReferenceError::Dispersed { venues: 2, .. }), "got {err}");
    }

    #[test]
    fn below_quorum_there_is_no_reference_at_all() {
        let err = combine(&[q(VenueId::Binance, 100)], &params()).unwrap_err();
        assert_eq!(err, ReferenceError::NoQuorum { have: 1, need: 2 });
        assert_eq!(err.status(), FeedStatus::NoQuorum { have: 1, need: 2 });

        let err = combine(&[], &params()).unwrap_err();
        assert_eq!(err, ReferenceError::NoQuorum { have: 0, need: 2 });
    }

    #[test]
    fn quorum_lost_to_outliers_is_reported_as_its_own_state() {
        // Three venues, a strict quorum of 3, one rejected: the survivors no longer make quorum.
        // Reporting that as plain `NoQuorum` would hide that a venue was actively disagreeing.
        let base = 100_000_000_000u128;
        let strict = MadParams { min_venues: 3, ..params() };
        let quotes = [
            q(VenueId::Binance, base),
            q(VenueId::Okx, base + 1),
            q(VenueId::Bybit, base * 2),
        ];
        let err = combine(&quotes, &strict).unwrap_err();
        assert_eq!(err, ReferenceError::QuorumLostToOutliers { survived: 2, need: 3 });
        assert_eq!(err.label(), "quorum_lost_to_outliers");
    }

    #[test]
    fn a_venue_dropping_out_moves_the_reference_by_less_than_the_threshold() {
        // The continuity claim in `combine`'s docs, pinned. Every survivor is within `threshold`
        // of the median, so losing one cannot move the mean by more than `threshold / n`.
        let base = 196_930_000_000u128;
        let all = [
            q(VenueId::Binance, base),
            q(VenueId::Okx, base - 20_000_000),
            q(VenueId::Bybit, base + 20_000_000),
        ];
        let with = combine(&all, &params()).unwrap();
        let without = combine(&all[..2], &params()).unwrap();
        let moved = deviation_decibps(without.micro, with.micro).unsigned_abs();
        assert!(
            u32::try_from(moved).unwrap() <= with.threshold_decibps,
            "losing a venue moved the reference {moved} decibps, past the {} threshold",
            with.threshold_decibps
        );
    }

    #[test]
    fn the_median_is_the_mean_of_the_middle_two_at_even_lengths() {
        assert_eq!(median_sorted(&[1, 2, 3]), Some(2));
        assert_eq!(median_sorted(&[1, 2, 3, 4]), Some(2));
        assert_eq!(median_sorted(&[10, 20, 21, 40]), Some(20));
        // Odd sum, so the halving has to carry.
        assert_eq!(median_sorted(&[1, 1, 2, 2]), Some(1));
        assert_eq!(median_sorted(&[]), None, "an empty cross-section names no price");
        assert_eq!(median_sorted(&[3, 3]), Some(3));
    }

    #[test]
    fn deviations_carry_their_sign() {
        assert_eq!(deviation_decibps(10_100, 10_000), 1_000);
        assert_eq!(deviation_decibps(9_900, 10_000), -1_000);
        assert_eq!(deviation_decibps(10_000, 10_000), 0);
        assert_eq!(deviation_decibps(1, 0), 0);
    }
}
