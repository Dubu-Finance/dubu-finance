//! Micro-price per venue, and the cross-venue combiner in front of it.
//!
//! ```text
//! micro = (bid * askQty + ask * bidQty) / (bidQty + askQty)
//! ```
//!
//! Note the crossed weighting: `bidQty` multiplies the **ask**. The sizes carry the short-horizon
//! information the mid ignores — with 200 ETH bid and 2 ETH offered the next trade lifts the offer
//! — so quoting around the mid fills on the wrong side systematically rather than noisily, and the
//! loss accumulates at the rate you trade. Weighting each price by its own size gets the sign
//! backwards and is worse than the mid.
//!
//! # Outlier rejection is cross-sectional and MAD-based, not a fixed band
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
//! A fixed band against each venue's own previous tick is too wide in a calm market and fires
//! constantly in a fast one. `mad` instead **is** the market's current disagreement, so the band
//! widens by itself: a venue printing garbage is far from a median three others agree on whatever
//! the regime, while a genuine fast move takes every venue with it and moves the median. The
//! `floor` stops the filter eating itself, since venues agreeing to within a tick collapse `mad` to
//! zero; measured live across Binance, OKX and Bybit, `mad` runs 0.1-1.0 bps against a largest
//! single-venue deviation of 1.6 bps, so a 2 bps floor sits above ordinary disagreement.
//!
//! [`ReferenceError::Dispersed`] gates on `mad` rather than on a rejection count because at least
//! half the cross-section survives a MAD filter by construction, so "a majority was rejected"
//! cannot occur; a split cross-section instead moves `mad` to half the gap between the camps, and
//! the gate fires rather than averaging a price no venue is showing. Neither it nor
//! [`ReferenceError::NoQuorum`] catches an error correlated across every venue; that needs a source
//! the bot does not price from — Pyth, on chain, inside `updateQuote`. Not built.
//!
//! Exact integers throughout, at [`crate::units::FEED_SCALE`]. The micro-price numerator is bounded
//! by `2 * maxPrice * maxQty`, around `10^27` for a six-figure asset against a deep book — inside
//! `u128` in practice, but the 256-bit intermediate costs nothing.

use dubu_core::math::{div_floor_u256, mul_div_floor, U256};

use crate::feed::{BookTick, FeedStatus, VenueId};

/// Deci-bps: tenths of a basis point. Deviations between healthy venues are a fraction of a bp, so
/// whole bps has no resolution to spare here.
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

/// The size-weighted micro-price of one book tick, with no history and no cross-section.
///
/// The three rejections are unconditional because each says the *frame* is broken rather than that
/// the price is unusual: a crossed book, zero depth on a side, a zero price.
///
/// # Errors
/// [`Reject::CrossedBook`], [`Reject::ZeroDepth`], [`Reject::ZeroPrice`] or [`Reject::Domain`].
pub fn micro_price(t: &BookTick) -> Result<u128, Reject> {
    if t.bid == 0 || t.ask == 0 {
        return Err(Reject::ZeroPrice);
    }
    if t.bid >= t.ask {
        return Err(Reject::CrossedBook {
            bid: t.bid,
            ask: t.ask,
        });
    }
    if t.bid_qty == 0 || t.ask_qty == 0 {
        return Err(Reject::ZeroDepth);
    }
    // The guards above are the only place an untrusted book is judged.
    assert!(t.bid > 0);
    assert!(t.ask > 0);
    assert!(t.bid < t.ask);
    assert!(t.bid_qty > 0);
    assert!(t.ask_qty > 0);

    // Note the crossing: bid weighted by ASK size, ask weighted by BID size.
    let num = U256::mul_u128(t.bid, t.ask_qty)
        .checked_add(U256::mul_u128(t.ask, t.bid_qty))
        .ok_or(Reject::Domain)?;
    let den = t.bid_qty.checked_add(t.ask_qty).ok_or(Reject::Domain)?;
    assert!(den > 0, "both sides carry size, so the sum cannot be zero");
    let micro = div_floor_u256(num, U256::from_u128(den)).ok_or(Reject::Domain)?;

    // A weighted average of two prices lies between them, so a micro-price outside the book means
    // the weighting was crossed the wrong way. Floor division only pulls toward the bid, which is
    // why the lower bound is inclusive and the upper one is not.
    assert!(
        micro >= t.bid,
        "micro-price below the bid: {micro} < {}",
        t.bid
    );
    assert!(
        micro < t.ask,
        "micro-price at or above the ask: {micro} >= {}",
        t.ask
    );
    Ok(micro)
}

/// Top-of-book spread in bps of the mid.
#[must_use]
pub fn book_spread_bps(bid: u128, ask: u128) -> u128 {
    let mid = (bid / 2).saturating_add(ask / 2);
    if mid == 0 {
        return 0;
    }
    assert!(mid > 0);
    let bps = mul_div_floor(ask.saturating_sub(bid), 10_000, mid).unwrap_or(0);
    if bid >= ask {
        assert_eq!(bps, 0, "a crossed or locked book has no positive spread");
    }
    bps
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
    /// Its top-of-book spread in bps of the mid. The pool's own half-spread must comfortably exceed
    /// half of this, or the pool is quoting inside the reference venue.
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
        let quote = Self {
            venue,
            micro: micro_price(tick)?,
            bid: tick.bid,
            ask: tick.ask,
            book_spread_bps: book_spread_bps(tick.bid, tick.ask),
            age_ms,
        };
        // Paired with `micro_price`'s postcondition: everything downstream reads these fields
        // rather than re-deriving them from the tick.
        assert!(quote.bid < quote.ask);
        assert!(quote.micro >= quote.bid);
        assert!(quote.micro < quote.ask);
        Ok(quote)
    }
}

/// The MAD filter's knobs, already converted out of the config's decimal-bps form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MadParams {
    /// Venues that must survive for a reference to exist at all.
    pub venues_min: u8,
    /// Multiplier on the MAD, in tenths: `40` is `k = 4.0`.
    pub k_tenths: u32,
    /// Floor under the rejection threshold, in deci-bps of the median.
    pub floor_decibps: u32,
    /// Dispersion above which venues are disagreeing rather than noisy, in deci-bps of the median.
    pub dispersion_decibps_max: u32,
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

/// Which bound set the rejection threshold. Worth logging: it says which regime the filter is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThresholdBound {
    /// `k * mad` bound it: the market is moving and the band widened by itself.
    Mad,
    /// The configured floor bound it: without it, close agreement rejects all but the median.
    Floor,
    /// Fewer than three venues, so only the dispersion gate applies; see [`threshold`].
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
    /// The combined reference, at [`crate::units::FEED_SCALE`]; see [`combine`] on equal weight.
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
    /// `binance:+0.3 bybit:-0.5 okx:+0.0` — survivors, then rejected ones marked with `!`. One flat
    /// string, because it goes in a log line a human reads during an incident.
    #[must_use]
    pub fn venue_summary(&self) -> String {
        let one = |d: &Deviation, bang: &str| {
            format!(
                "{}{}:{}{}.{}",
                bang,
                d.venue,
                if d.decibps < 0 { "-" } else { "+" },
                d.decibps.unsigned_abs() / 10,
                d.decibps.unsigned_abs() % 10
            )
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

/// Why no reference price could be produced. Every variant means the same thing to
/// [`crate::policy`] — do not quote — and different things to whoever reads the log.
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
    #[error(
        "venues disagree: dispersion {dispersion_decibps} decibps exceeds {limit_decibps}, \
         across {venues} venues"
    )]
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
            Self::QuorumLostToOutliers { survived, need } => FeedStatus::NoQuorum {
                have: survived,
                need,
            },
            Self::Dispersed {
                dispersion_decibps,
                limit_decibps,
                venues,
            } => FeedStatus::Dispersed {
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
/// `None` on an empty slice rather than a panic: [`combine`]'s quorum check leaves that reachable
/// through a configured `venues_min` of zero, and the even branch would evaluate `xs[0 / 2 - 1]`,
/// underflowing `usize` before it ever indexes — so the failure would not read as out-of-bounds.
fn median_sorted(xs: &[u128]) -> Option<u128> {
    let n = xs.len();
    if n == 0 {
        return None;
    }
    assert!(n > 0);
    // `slice::is_sorted` is stable since 1.82 and this crate's MSRV is 1.75.
    debug_assert!(
        xs.windows(2).all(|w| w.first() <= w.last()),
        "median_sorted was handed an unsorted slice"
    );
    let median = if n % 2 == 1 {
        xs.get(n / 2).copied()
    } else {
        let lo = *xs.get(n / 2 - 1)?;
        let hi = *xs.get(n / 2)?;
        assert!(
            lo <= hi,
            "the slice is sorted, so the lower middle is the lower"
        );
        // Halve each before adding: the crate's standard overflow-free form, and the carry term
        // keeps it exact.
        Some(lo / 2 + hi / 2 + (lo % 2 + hi % 2) / 2)
    };
    // A median lies inside the sample; outside means the carry is wrong or the slice was unsorted.
    if let (Some(m), Some(lo), Some(hi)) = (median, xs.first(), xs.last()) {
        assert!(m >= *lo);
        assert!(m <= *hi);
    }
    median
}

/// Signed deviation from a reference, in deci-bps of it.
fn deviation_decibps(x: u128, from: u128) -> i64 {
    if from == 0 {
        return 0;
    }
    let mag = mul_div_floor(x.abs_diff(from), DECIBPS, from).unwrap_or(u128::from(u32::MAX));
    let mag = i64::try_from(mag).unwrap_or(i64::MAX);
    assert!(
        mag >= 0,
        "a magnitude is unsigned before the sign is applied"
    );
    let signed = if x < from { -mag } else { mag };
    if x == from {
        assert_eq!(signed, 0, "no distance means no deviation");
    }
    if x < from {
        assert!(signed <= 0, "below the reference must read negative");
    }
    if x > from {
        assert!(signed >= 0, "above the reference must read positive");
    }
    signed
}

/// Each venue's signed distance from the median, in deci-bps.
fn deviations(quotes: &[VenueQuote], median: u128) -> Vec<Deviation> {
    assert!(
        median > 0,
        "a zero median names no reference to deviate from"
    );
    let devs: Vec<Deviation> = quotes
        .iter()
        .map(|q| Deviation {
            venue: q.venue,
            micro: q.micro,
            decibps: deviation_decibps(q.micro, median),
        })
        .collect();
    assert_eq!(devs.len(), quotes.len(), "every venue keeps its place");
    devs
}

/// Median absolute deviation across the whole cross-section, in deci-bps of the median. This **is**
/// the market's current disagreement, which is what makes the band adaptive rather than fixed.
fn mad_decibps(devs: &[Deviation]) -> u128 {
    assert!(!devs.is_empty(), "an empty cross-section has no scale");
    let mut mags: Vec<u128> = devs
        .iter()
        .map(|d| u128::from(d.decibps.unsigned_abs()))
        .collect();
    mags.sort_unstable();
    let mad = median_sorted(&mags).unwrap_or(0);
    debug_assert!(mags.last().is_some_and(|worst| mad <= *worst));
    mad
}

/// The rejection threshold, and which bound produced it.
///
/// With fewer than three venues an outlier cannot be attributed: two that disagree are symmetric,
/// each exactly one MAD from the median of the pair, so any threshold rejects both or neither. The
/// dispersion gate is the entire defence in that case.
fn threshold(venues: usize, mad: u128, p: &MadParams) -> (u32, ThresholdBound) {
    if venues < 3 {
        return (p.dispersion_decibps_max, ThresholdBound::Unattributable);
    }
    assert!(venues >= 3, "the unattributable case returned above");
    let scaled = u32::try_from(mad * u128::from(p.k_tenths) / 10).unwrap_or(u32::MAX);
    let chosen = if scaled >= p.floor_decibps {
        (scaled, ThresholdBound::Mad)
    } else {
        (p.floor_decibps, ThresholdBound::Floor)
    };
    // The floor is what stops the filter eating itself when the MAD collapses to zero.
    assert!(
        chosen.0 >= p.floor_decibps,
        "the threshold may never fall under the floor"
    );
    if chosen.1 == ThresholdBound::Mad {
        assert_eq!(chosen.0, scaled);
    }
    chosen
}

/// Split the cross-section at the rejection threshold. At least half of it is within one MAD of the
/// median by construction, so a majority can never be rejected.
fn combine_partition(devs: Vec<Deviation>, threshold: u32) -> (Vec<Deviation>, Vec<Deviation>) {
    let devs_len = devs.len();
    let (used, rejected): (Vec<Deviation>, Vec<Deviation>) = devs
        .into_iter()
        .partition(|d| d.decibps.unsigned_abs() <= u64::from(threshold));
    assert_eq!(
        used.len() + rejected.len(),
        devs_len,
        "a venue was lost in the partition"
    );
    assert!(
        rejected.len() <= devs_len,
        "more venues rejected than existed"
    );
    debug_assert!(
        used.iter()
            .all(|d| d.decibps.unsigned_abs() <= u64::from(threshold)),
        "a rejected venue survived the partition"
    );
    (used, rejected)
}

/// Equal-weighted mean of the survivors, at [`crate::units::FEED_SCALE`]. The sum of a handful of
/// feed-scale prices is nowhere near `u128`. A mean lies inside its sample; paired with
/// [`combine_partition`], that bounds how far losing one venue can move the reference.
fn combine_mean(used: &[Deviation]) -> u128 {
    let sum: u128 = used.iter().map(|d| d.micro).sum();
    let micro = sum / used.len() as u128;
    if let (Some(lo), Some(hi)) = (
        used.iter().map(|d| d.micro).min(),
        used.iter().map(|d| d.micro).max(),
    ) {
        assert!(
            micro >= lo,
            "the reference sits below every venue it averaged"
        );
        assert!(
            micro <= hi,
            "the reference sits above every venue it averaged"
        );
    }
    micro
}

/// Combine one instant's per-venue micro-prices into a single reference.
///
/// **Equal weight**, not liquidity weight: top-of-book size is mostly a function of tick size, so a
/// coarse-tick venue would dominate for a reason unrelated to its price, and each venue's own
/// liquidity is already inside its micro-price. Not the median either — that discards every venue
/// but one and moves discontinuously as venues drop out. Every survivor is within `threshold` of
/// the median, so losing one moves the reference by at most `threshold / n`; the venue count is
/// logged so the lost redundancy is visible even though the price barely moves.
///
/// # Errors
/// [`ReferenceError`], one variant per way the cross-section can fail to name a price.
pub fn combine(quotes: &[VenueQuote], p: &MadParams) -> Result<Reference, ReferenceError> {
    let need = p.venues_min;
    let n = u8::try_from(quotes.len()).unwrap_or(u8::MAX);
    if n < need {
        return Err(ReferenceError::NoQuorum { have: n, need });
    }
    assert!(
        n >= need,
        "the quorum check is the only way past this point"
    );
    assert!(!quotes.is_empty() || need == 0);

    let mut sorted: Vec<u128> = quotes.iter().map(|q| q.micro).collect();
    sorted.sort_unstable();
    // `None` and zero both mean no cross-section to price from, so they take the same exit.
    let median = median_sorted(&sorted).unwrap_or(0);
    if median == 0 {
        return Err(ReferenceError::NoQuorum { have: 0, need });
    }
    assert!(median > 0);

    let devs = deviations(quotes, median);
    let mad = mad_decibps(&devs);
    let dispersion = u32::try_from(mad).unwrap_or(u32::MAX);

    // The regime gate: one venue away from the pack barely moves the MAD, half the venues away from
    // the other half moves it to roughly half the gap.
    if dispersion > p.dispersion_decibps_max {
        return Err(ReferenceError::Dispersed {
            dispersion_decibps: dispersion,
            limit_decibps: p.dispersion_decibps_max,
            venues: n,
        });
    }
    assert!(dispersion <= p.dispersion_decibps_max);

    let (threshold, bound) = threshold(quotes.len(), mad, p);
    if quotes.len() < 3 {
        assert_eq!(bound, ThresholdBound::Unattributable);
    }

    let (used, rejected) = combine_partition(devs, threshold);
    let survived = u8::try_from(used.len()).unwrap_or(u8::MAX);
    if survived < need {
        return Err(ReferenceError::QuorumLostToOutliers { survived, need });
    }
    assert!(!used.is_empty() || need == 0);

    Ok(Reference {
        micro: combine_mean(&used),
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
        BookTick {
            update_id: 1,
            bid,
            bid_qty,
            ask,
            ask_qty,
        }
    }

    /// A venue quoting a flat book at `micro`, so the cross-section is easy to reason about.
    fn q(venue: VenueId, micro: u128) -> VenueQuote {
        VenueQuote {
            venue,
            micro,
            bid: micro,
            ask: micro + 1,
            book_spread_bps: 0,
            age_ms: 10,
        }
    }

    /// Live-shaped parameters: k = 4.0, floor 2 bp, dispersion limit 25 bp, quorum 2.
    fn params() -> MadParams {
        MadParams {
            venues_min: 2,
            k_tenths: 40,
            floor_decibps: 20,
            dispersion_decibps_max: 250,
        }
    }

    // --- the micro-price itself ---

    #[test]
    fn the_micro_price_leans_toward_the_thin_side() {
        // Prices at FEED_SCALE, as they arrive: 100.00 bid, 102.00 ask, mid 101.00.
        let (bid, ask, mid) = (10_000_000_000u128, 10_200_000_000u128, 10_100_000_000u128);

        // Balanced book: the micro-price is the mid.
        assert_eq!(micro_price(&tick(bid, 10, ask, 10)), Ok(mid));

        // Ten times the size on the bid: (bid*1 + ask*10) / 11 = 101.8181...
        assert_eq!(micro_price(&tick(bid, 10, ask, 1)), Ok(10_181_818_181));

        let heavy_bid = micro_price(&tick(bid, 1_000, ask, 1)).unwrap();
        assert!(
            heavy_bid > mid,
            "a heavily bid book must price above the mid, got {heavy_bid}"
        );
        assert!(heavy_bid < ask);

        // ... and the mirror.
        let heavy_ask = micro_price(&tick(bid, 1, ask, 1_000)).unwrap();
        assert!(
            heavy_ask < mid,
            "a heavily offered book must price below the mid, got {heavy_ask}"
        );
        assert!(heavy_ask > bid);
    }

    #[test]
    fn the_weighting_is_crossed_not_self() {
        // Weighting each price by its OWN size would put a heavily bid book BELOW the mid.
        let t = tick(194_382_000_000, 2_425_800_000, 194_383_000_000, 137_811_000);
        let micro = micro_price(&t).unwrap();
        let mid = (t.bid + t.ask) / 2;
        assert!(
            micro > mid,
            "bid size dwarfs ask size, so fair value sits above the mid"
        );
        assert!(micro < t.ask);
    }

    #[test]
    fn structurally_broken_books_are_rejected() {
        assert_eq!(
            micro_price(&tick(102, 1, 100, 1)),
            Err(Reject::CrossedBook { bid: 102, ask: 100 })
        );
        assert_eq!(
            micro_price(&tick(100, 1, 100, 1)),
            Err(Reject::CrossedBook { bid: 100, ask: 100 })
        );
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

    // --- the cross-section ---

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
        assert_eq!(
            r.micro, 196_930_000_000,
            "equal weight over three survivors"
        );
        assert_eq!(r.venues_used(), 3);
        assert!(r.rejected.is_empty());
        // Everything agrees to a fraction of a bp, so the floor is what set the threshold.
        assert_eq!(r.bound, ThresholdBound::Floor);
        assert_eq!(r.threshold_decibps, 20);
    }

    #[test]
    fn one_venue_printing_garbage_is_rejected_and_the_rest_still_quote() {
        // The failure the multi-venue design exists for: a feed that is confidently wrong.
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
        assert!(
            r.venue_summary().contains("!bybit"),
            "summary: {}",
            r.venue_summary()
        );
    }

    #[test]
    fn a_genuine_fast_move_takes_every_venue_with_it_and_nothing_is_rejected() {
        // What a temporal filter gets wrong: it rejects the first ticks of every real move, which
        // is when a stale quote is most expensive.
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
        assert!(
            r1.rejected.is_empty(),
            "a market-wide move must not be treated as an outlier"
        );
        assert!(r1.micro > r0.micro);
        assert_eq!(
            r1.venues_used(),
            3,
            "and it must cost nothing in venue count"
        );
    }

    #[test]
    fn the_band_widens_by_itself_when_the_market_disagrees_more() {
        // A venue 3 bp from the median is rejected in a calm cross-section and kept in a noisy one.
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
        assert_eq!(
            r.rejected.len(),
            1,
            "3 bp is an outlier when everyone agrees to 0.1 bp"
        );

        let fast = [
            q(VenueId::Binance, base),
            q(VenueId::Okx, base + 2 * bp),
            q(VenueId::Bybit, base - 2 * bp),
            q(VenueId::Coinbase, base + 3 * bp),
        ];
        let r = combine(&fast, &params()).unwrap();
        assert_eq!(
            r.bound,
            ThresholdBound::Mad,
            "the MAD must bind once the market is moving"
        );
        assert!(
            r.rejected.is_empty(),
            "3 bp is ordinary when the cross-section is 2 bp wide"
        );
    }

    #[test]
    fn a_split_cross_section_is_a_regime_change_and_is_not_averaged_through() {
        // Two venues at one price, two at another 60 bp away. The mean of the four is a number no
        // venue is showing, so there is nothing to quote.
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
        assert!(matches!(
            err.status(),
            FeedStatus::Dispersed { venues: 4, .. }
        ));
    }

    #[test]
    fn two_venues_cannot_attribute_an_outlier_and_say_so() {
        // With two points every threshold rejects both or neither, so the dispersion gate is the
        // only defence.
        let base = 100_000_000_000u128;
        let bp = base / 10_000;
        let r = combine(
            &[q(VenueId::Binance, base), q(VenueId::Okx, base + 4 * bp)],
            &params(),
        )
        .unwrap();
        assert_eq!(r.bound, ThresholdBound::Unattributable);
        assert_eq!(r.venues_used(), 2, "neither may be rejected");
        assert_eq!(r.micro, base + 2 * bp);

        // Far enough apart and the dispersion gate does the work instead.
        let err = combine(
            &[q(VenueId::Binance, base), q(VenueId::Okx, base + 60 * bp)],
            &params(),
        )
        .unwrap_err();
        assert!(
            matches!(err, ReferenceError::Dispersed { venues: 2, .. }),
            "got {err}"
        );
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
        // Three venues, a strict quorum of 3, one rejected. Plain `NoQuorum` would hide that a
        // venue was actively disagreeing.
        let base = 100_000_000_000u128;
        let strict = MadParams {
            venues_min: 3,
            ..params()
        };
        let quotes = [
            q(VenueId::Binance, base),
            q(VenueId::Okx, base + 1),
            q(VenueId::Bybit, base * 2),
        ];
        let err = combine(&quotes, &strict).unwrap_err();
        assert_eq!(
            err,
            ReferenceError::QuorumLostToOutliers {
                survived: 2,
                need: 3
            }
        );
        assert_eq!(err.label(), "quorum_lost_to_outliers");
    }

    #[test]
    fn a_venue_dropping_out_moves_the_reference_by_less_than_the_threshold() {
        // The continuity claim in `combine`'s docs, pinned.
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
        assert_eq!(
            median_sorted(&[]),
            None,
            "an empty cross-section names no price"
        );
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
