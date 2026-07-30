//! Ladder construction from strategy inputs.
//!
//! Where [`crate::inverse`] starts from a target execution price, this starts from the shape the
//! strategy wants: a reference mid, a half-spread, a width, and an inventory skew — the four
//! numbers archi_v2 §4.3 proposes compressing into 14 bytes of calldata.
//!
//! Whatever comes in, what comes out passes `PropCurve.validateLadder`. A rejected `updateQuote`
//! burns the block and leaves the previous quote to go stale, turning a quoting outage into an
//! adverse-selection window.
//!
//! # Clamping order
//!
//! Deterministic, and in this order, because each step's postcondition is what makes the next
//! step's bounds well-formed (`lo <= hi` at every `clamp`).
//!
//! 1. **Sanitise the bps knobs.** `u16`/`i16` admit values past [`BPS_E2_MAX`], and
//!    `BPS_E2 - bps` must stay positive or the whole construction inverts.
//! 2. **Skew the mid**, floored, then clamped to `1..=PRICE_MAX`. Positive skew pushes the book
//!    *down* (the pool is long and wants to sell). The `>= 1` keeps a zero reference from
//!    collapsing the row into the all-zero ladder the validator rejects.
//! 3. **Project the four prices**, bid floored and ask ceiled, both in the pool's favour. §4.3's
//!    on-chain reconstruction floors both, so if that compressed path is ever enabled the two
//!    differ by one unit on the ask; see [`LadderBuilder::round_ask_up`].
//! 4. **Clamp into a monotone chain, bottom up**, each price bounded below by its predecessor and
//!    above by `PRICE_MAX - 1` (`PRICE_MAX` for `max_ask`). One sweep subsumes the floor clamp,
//!    the ceiling clamp and the ordering repair, so no later step can undo an earlier one.
//! 5. **Open the book by one unit** if `max_ask == min_bid`; `validateLadder`'s `maxAsk > minBid`
//!    is strict. Step 4 reserves the room by capping the first three prices at `PRICE_MAX - 1`.
//! 6. **Assert, then validate.** The invariants are checked where the row is built and again by
//!    the port of the on-chain validator, so a construction bug cannot reach the chain through a
//!    validator that shares it.

use crate::curve::{validate_ladder, Ladder, PRICE_MAX};
use crate::error::LadderError;
use crate::math::{mul_div_ceil, mul_div_floor};

/// Basis-point denominator.
pub const BPS: u128 = 10_000;

/// One hundredth of a basis point, the unit the ladder's spread and width are built in.
///
/// Integer basis points are too coarse: with sigma scaled to the quote's real exposure window,
/// ETH's volatility term is 0.6 bp against an `s0` of 1. Nothing on chain changes, since `PropPool`
/// is handed four PRICES, so basis points never leave this crate.
pub const BPS_E2: u128 = 1_000_000;

/// Largest bps value any knob may take. `10_000` would zero out a price outright and anything
/// above it would invert the sign of `10_000 - bps`.
pub const BPS_MAX: u128 = 9_999;

/// [`BPS_MAX`] in hundredths of a basis point.
pub const BPS_E2_MAX: u128 = 999_999;

// Compile-time pins on the relationships every clamp below is derived from: edit one constant
// without the others and this module stops being total.
const _: () = assert!(BPS_E2 == BPS * 100);
const _: () = assert!(BPS_E2_MAX == BPS_MAX * 100 + 99);
// `BPS - skew` and `BPS_E2 - bps` stay strictly positive, so no projection can invert or zero out.
const _: () = assert!(BPS_MAX < BPS);
const _: () = assert!(BPS_E2_MAX < BPS_E2);
// Step 5 needs one price unit of headroom under the ceiling for `max_ask > min_bid`.
const _: () = assert!(PRICE_MAX >= 2);
// The widest step-3 factor (`BPS_E2 + BPS_E2_MAX`) times `PRICE_MAX` must stay inside u128, so
// that the saturation in [`round`] means "too big" rather than "wrapped".
const _: () = assert!(PRICE_MAX < u128::MAX / (BPS_E2 + BPS_E2_MAX));

/// Strategy inputs for one quote row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LadderBuilder {
    /// Reference mid, in the pair's price scale (`quote = base * price / 10**priceScaleExp`).
    pub reference_mid: u128,
    /// Half the bid/ask spread, in bps of the skewed mid.
    pub half_spread_bps_e2: u32,
    /// Ladder width, in bps of the near price. The concentration knob; archi_v2 §5.2 puts
    /// production values below one basis point, so expect 0 for most pairs and the width to come
    /// from [`crate::inverse`] instead.
    pub width_bps_e2: u32,
    /// Inventory skew, in bps. Positive shifts the whole book down.
    pub skew_bps: i16,
    /// The pair's absolute `minPrice` floor, an oracle-independent backstop. `minBid` is
    /// guaranteed at or above it.
    pub min_price: u128,
    /// Round the ask side up rather than down. The default `true` is pool-favourable; set `false`
    /// only to reproduce the §4.3 on-chain reconstruction bit for bit.
    pub round_ask_up: bool,
}

impl LadderBuilder {
    /// A builder with pool-favourable defaults and every knob at zero.
    #[must_use]
    pub const fn new(reference_mid: u128) -> Self {
        Self {
            reference_mid,
            half_spread_bps_e2: 0,
            width_bps_e2: 0,
            skew_bps: 0,
            min_price: 0,
            round_ask_up: true,
        }
    }

    /// The skewed mid, step 2 of the clamping order.
    ///
    /// # Errors
    /// [`LadderError::PriceOutOfRange`] if `reference_mid` is above [`PRICE_MAX`].
    pub fn skewed_mid(&self) -> Result<u128, LadderError> {
        if self.reference_mid > PRICE_MAX {
            return Err(LadderError::PriceOutOfRange);
        }
        let skew = i128::from(self.skew_bps).clamp(-(BPS_MAX as i128), BPS_MAX as i128);
        assert!(skew.unsigned_abs() <= BPS_MAX);
        // `BPS - skew` lands in 1..=19_999, always positive.
        let factor = (BPS as i128 - skew) as u128;
        assert!(factor >= BPS - BPS_MAX);
        assert!(factor <= BPS + BPS_MAX);
        assert!(factor != 0);

        let m = mul_div_floor(self.reference_mid, factor, BPS).unwrap_or(PRICE_MAX);
        let m = m.clamp(1, PRICE_MAX);
        assert!(
            m >= 1,
            "a zero mid collapses the row into the all-zero ladder"
        );
        assert!(m <= PRICE_MAX);
        // Paired statement of the sign convention: positive skew may only push the book down.
        if self.skew_bps > 0 {
            assert!(m <= self.reference_mid.max(1));
        }
        if self.skew_bps < 0 {
            assert!(m >= self.reference_mid.min(PRICE_MAX));
        }
        Ok(m)
    }

    /// Build the row. Always produces something `PropCurve.validateLadder` accepts.
    ///
    /// # Errors
    /// [`LadderError::PriceOutOfRange`] if `reference_mid` exceeds [`PRICE_MAX`];
    /// [`LadderError::InfeasibleBounds`] if `min_price >= PRICE_MAX`, leaving no room for the
    /// strict `maxAsk > minBid`. Never [`LadderError::Rejected`]: that would mean a proof in the
    /// clamping order above is wrong.
    pub fn build(&self) -> Result<Ladder, LadderError> {
        // Step 5 needs one unit of headroom below PRICE_MAX for max_ask to sit above min_bid.
        if self.min_price >= PRICE_MAX {
            return Err(LadderError::InfeasibleBounds);
        }
        let m = self.skewed_mid()?;
        assert!(m >= 1);
        assert!(m <= PRICE_MAX);

        // Step 1. Both in hundredths of a bp -- see [`BPS_E2`].
        let hs = u128::from(self.half_spread_bps_e2).min(BPS_E2_MAX);
        let w = u128::from(self.width_bps_e2).min(BPS_E2_MAX);

        let raw = self.project(m, hs, w);
        let ladder = clamp_chain(raw, self.min_price);

        // Step 6. The construction-side invariants, then the port of the on-chain validator.
        assert_chain(&ladder, self.min_price);
        validate_ladder(
            ladder.min_bid,
            ladder.max_bid,
            ladder.min_ask,
            ladder.max_ask,
            self.min_price,
        )?;
        Ok(ladder)
    }

    /// Step 3: the four unclamped prices. Bid side down, ask side (by default) up.
    fn project(&self, m: u128, hs: u128, w: u128) -> Ladder {
        assert!(m >= 1);
        assert!(m <= PRICE_MAX);
        assert!(hs <= BPS_E2_MAX);
        assert!(w <= BPS_E2_MAX);
        assert!(
            BPS_E2 - hs >= 1,
            "a full-width half-spread would zero the bid"
        );
        assert!(BPS_E2 - w >= 1);

        let max_bid = mul_div_floor(m, BPS_E2 - hs, BPS_E2).unwrap_or(0);
        let min_bid = mul_div_floor(max_bid, BPS_E2 - w, BPS_E2).unwrap_or(0);
        let up = self.round_ask_up;
        let min_ask = round(up, m, BPS_E2 + hs, BPS_E2);
        let max_ask = round(up, min_ask, BPS_E2 + w, BPS_E2);

        // Already ordered before any clamping, which makes step 4's sweep a repair of the
        // `min_price`/`PRICE_MAX` bounds rather than of the projection itself.
        assert!(min_bid <= max_bid);
        assert!(max_bid <= m);
        assert!(min_ask >= m.min(PRICE_MAX));
        assert!(min_ask <= max_ask);
        Ladder {
            min_bid,
            max_bid,
            min_ask,
            max_ask,
        }
    }
}

/// Step 4 and step 5: the monotone bottom-up sweep, each price bounded below by its predecessor.
///
/// `hi_body = PRICE_MAX - 1` leaves `max_ask` the one unit the strict `maxAsk > minBid` needs.
fn clamp_chain(raw: Ladder, min_price: u128) -> Ladder {
    assert!(
        min_price < PRICE_MAX,
        "no room for the strict maxAsk > minBid"
    );
    let hi_body = PRICE_MAX - 1;
    assert!(min_price <= hi_body, "every clamp below needs lo <= hi");

    let min_bid = raw.min_bid.clamp(min_price, hi_body);
    let max_bid = raw.max_bid.clamp(min_bid, hi_body);
    let min_ask = raw.min_ask.clamp(max_bid, hi_body);
    // Step 5, folded into the lower bound: max_ask >= max(min_ask, min_bid + 1).
    let max_ask = raw.max_ask.clamp(min_ask.max(min_bid + 1), PRICE_MAX);

    assert!(min_bid <= hi_body);
    assert!(max_bid <= hi_body);
    assert!(min_ask <= hi_body);
    assert!(max_ask <= PRICE_MAX);
    Ladder {
        min_bid,
        max_bid,
        min_ask,
        max_ask,
    }
}

/// The invariants `PropCurve.validateLadder` accepts, asserted at the point of construction.
///
/// Paired with [`validate_ladder`] deliberately: that validator is a port and could drift from the
/// chain, so these are stated against the clamping order instead. A bad row has to be wrong in both
/// places to reach the chain.
fn assert_chain(l: &Ladder, min_price: u128) {
    assert!(l.min_bid >= min_price);
    assert!(l.min_bid <= l.max_bid);
    assert!(l.max_bid <= l.min_ask);
    assert!(l.min_ask <= l.max_ask);
    assert!(l.max_ask <= PRICE_MAX);
    // Negative space: `validateLadder`'s `maxAsk > minBid` is strict, so the flat row that passes
    // every ordering check above is still rejected on chain.
    assert!(l.max_ask > l.min_bid);
    assert!(l.max_ask != 0);
}

/// `ceil(a*b/d)` when `up`, else `floor`. Saturates at [`PRICE_MAX`] rather than failing, because
/// step 4 clamps every price into range anyway and a saturating projection keeps `build` total.
fn round(up: bool, a: u128, b: u128, d: u128) -> u128 {
    assert!(d != 0);
    let r = if up {
        mul_div_ceil(a, b, d)
    } else {
        mul_div_floor(a, b, d)
    };
    let r = r.unwrap_or(PRICE_MAX).min(PRICE_MAX);
    assert!(r <= PRICE_MAX);
    // What step 3's ordering assertions rely on: a factor at or above one never moves a price
    // down, and only the saturation can cap it.
    if b >= d {
        assert!(r >= a.min(PRICE_MAX));
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_spread_and_width() {
        let b = LadderBuilder {
            half_spread_bps_e2: 1000, // 0.10%
            width_bps_e2: 10000,      // 1%
            ..LadderBuilder::new(1_000_000)
        };
        let l = b.build().unwrap();
        assert_eq!(l.max_bid, 999_000); // 1_000_000 * 9_990 / 10_000
        assert_eq!(l.min_bid, 989_010); // 999_000 * 9_900 / 10_000
        assert_eq!(l.min_ask, 1_001_000); // ceil(1_000_000 * 10_010 / 10_000)
        assert_eq!(l.max_ask, 1_011_010); // ceil(1_001_000 * 10_100 / 10_000)
        l.validate(0).unwrap();
    }

    #[test]
    fn positive_skew_pushes_the_book_down() {
        let up = LadderBuilder {
            half_spread_bps_e2: 1000,
            skew_bps: 0,
            ..LadderBuilder::new(1_000_000)
        };
        let down = LadderBuilder { skew_bps: 50, ..up };
        let (a, b) = (up.build().unwrap(), down.build().unwrap());
        assert!(b.max_bid < a.max_bid);
        assert!(b.min_ask < a.min_ask);
        let lifted = LadderBuilder {
            skew_bps: -50,
            ..up
        }
        .build()
        .unwrap();
        assert!(lifted.max_bid > a.max_bid);
    }

    #[test]
    fn min_price_floor_wins_over_the_projection() {
        let b = LadderBuilder {
            half_spread_bps_e2: 50000,
            width_bps_e2: 500_000,
            min_price: 999_000,
            ..LadderBuilder::new(1_000_000)
        };
        let l = b.build().unwrap();
        assert!(l.min_bid >= 999_000);
        l.validate(999_000).unwrap();
    }

    #[test]
    fn degenerate_inputs_still_produce_a_valid_row() {
        // Zero mid, zero knobs: the all-zero ladder fails the strict maxAsk > minBid.
        let l = LadderBuilder::new(0).build().unwrap();
        l.validate(0).unwrap();
        assert!(l.max_ask > l.min_bid);

        // Mid at the ceiling with no spread: max_ask must still clear min_bid.
        let l = LadderBuilder::new(PRICE_MAX).build().unwrap();
        l.validate(0).unwrap();
        assert!(l.max_ask > l.min_bid);
        assert!(l.max_ask <= PRICE_MAX);

        // Floor pinned one unit below the ceiling: the tightest feasible row.
        let l = LadderBuilder {
            min_price: PRICE_MAX - 1,
            ..LadderBuilder::new(PRICE_MAX)
        }
        .build()
        .unwrap();
        assert_eq!(l.min_bid, PRICE_MAX - 1);
        assert_eq!(l.max_ask, PRICE_MAX);
        l.validate(PRICE_MAX - 1).unwrap();
    }

    #[test]
    fn out_of_range_inputs_are_rejected() {
        assert_eq!(
            LadderBuilder::new(PRICE_MAX + 1).build(),
            Err(LadderError::PriceOutOfRange)
        );
        assert_eq!(
            LadderBuilder {
                min_price: PRICE_MAX,
                ..LadderBuilder::new(PRICE_MAX)
            }
            .build(),
            Err(LadderError::InfeasibleBounds)
        );
    }

    #[test]
    fn bps_knobs_saturate_instead_of_inverting() {
        // u16 admits 65_535; 10_000 - 65_535 would underflow.
        let l = LadderBuilder {
            half_spread_bps_e2: u32::from(u16::MAX),
            width_bps_e2: u32::from(u16::MAX),
            skew_bps: i16::MIN,
            ..LadderBuilder::new(1_000_000)
        }
        .build()
        .unwrap();
        l.validate(0).unwrap();
    }

    #[test]
    fn ask_rounding_direction_is_selectable() {
        let up = LadderBuilder {
            half_spread_bps_e2: 100,
            ..LadderBuilder::new(999_999)
        };
        let down = LadderBuilder {
            round_ask_up: false,
            ..up
        };
        let (a, b) = (up.build().unwrap(), down.build().unwrap());
        assert!(a.min_ask >= b.min_ask);
        assert_eq!(a.min_ask - b.min_ask, 1);
    }
}
