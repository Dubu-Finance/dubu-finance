//! Ladder construction from strategy inputs.
//!
//! The other production path into the four prices. Where [`crate::inverse`] starts from a
//! target execution price, this starts from the shape the strategy wants: a reference mid,
//! a half-spread, a width, and an inventory skew — the same four numbers archi_v2 §4.3
//! proposes compressing into 14 bytes of calldata.
//!
//! The contract this module signs up to is absolute: **whatever comes in, what comes out
//! passes `PropCurve.validateLadder`.** The chain must never reject a row we produced. A
//! rejected `updateQuote` is not a harmless no-op — it burns the block, leaves the previous
//! quote in place to go stale, and is exactly the failure mode that turns an quoting outage
//! into an adverse-selection event.
//!
//! # Clamping order
//!
//! Deterministic, and in this order. Each step's postcondition is what makes the next step's
//! bounds provably well-formed (`lo <= hi` at every `clamp`).
//!
//! 1. **Sanitise the bps knobs.** `half_spread_bps` and `width_bps` are clamped to
//!    `0..=9_999`; `skew_bps` to `-9_999..=9_999`. `u16`/`i16` admit values past 10 000, and
//!    `10_000 - bps` must stay positive or the whole construction inverts.
//! 2. **Skew the mid.** `m = mid * (10_000 - skew) / 10_000`, floored, then clamped to
//!    `1..=MAX_PRICE`. Positive skew pushes the whole book *down* (the pool is long and wants
//!    to sell); negative pushes it up. Clamping to `>= 1` keeps a zero reference from
//!    collapsing the row into the all-zero ladder the validator rejects.
//! 3. **Project the four prices**, bid side floored and ask side ceiled — both directions
//!    are the pool's favour. This is the only asymmetry between the sides and it is
//!    deliberate; §4.3's on-chain reconstruction floors both, and if that compressed path is
//!    ever enabled the two will differ by one unit on the ask. See
//!    [`LadderBuilder::round_ask_up`].
//! 4. **Clamp into a monotone chain, bottom up**, each price bounded below by its
//!    predecessor and above by `MAX_PRICE - 1` (`MAX_PRICE` for `max_ask`). This single sweep
//!    subsumes the floor clamp, the ceiling clamp, and the ordering repair, and it is why no
//!    later step can undo an earlier one.
//! 5. **Open the book by one unit** if `max_ask == min_bid`, because `validateLadder`'s
//!    `maxAsk > minBid` is strict. Step 4 reserves the room for this by capping the first
//!    three prices at `MAX_PRICE - 1`.
//! 6. **Assert.** Run the on-chain validator. A failure here is a bug in this module, and
//!    the row is dropped rather than sent.

use crate::curve::{validate_ladder, Ladder, MAX_PRICE};
use crate::error::LadderError;
use crate::math::{mul_div_ceil, mul_div_floor};

/// Basis-point denominator.
pub const BPS: u128 = 10_000;

/// One in a hundredth of a basis point, the unit the ladder's spread and width are built in.
///
/// Integer basis points were the original unit and they ran out of resolution. ETH's half-spread is
/// `s0 + s1 * sigma` and, once sigma was scaled to the quote's real exposure window rather than the
/// inventory's, the volatility term came to 0.6 bp against an `s0` of 1 -- so the floor is most of
/// the price and a whole basis point is a coarse thing to set it in. Hundredths give 0.5 bp.
///
/// Nothing on chain changes. `PropPool` is handed four PRICES and `validate_ladder` checks their
/// ordering; basis points never leave this crate.
pub const BPS_E2: u128 = 1_000_000;

/// Largest bps value any knob may take. `10_000` would zero out a price outright and
/// anything above it would invert the sign of `10_000 - bps`.
pub const MAX_BPS: u128 = 9_999;

/// [`MAX_BPS`] in hundredths of a basis point.
pub const MAX_BPS_E2: u128 = 999_999;

/// Strategy inputs for one quote row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LadderBuilder {
    /// Reference mid, in the pair's price scale (`quote = base * price / 10**priceScaleExp`).
    pub reference_mid: u128,
    /// Half the bid/ask spread, in bps of the skewed mid.
    pub half_spread_bps_e2: u32,
    /// Ladder width, in bps of the near price. This is the concentration knob; archi_v2 §5.2
    /// notes production values in the sub-basis-point range (`0.20 bp`), so expect this to be
    /// 0 for most pairs and the width to come from [`crate::inverse`] instead.
    pub width_bps_e2: u32,
    /// Inventory skew, in bps. Positive shifts the whole book down.
    pub skew_bps: i16,
    /// The pair's absolute `minPrice` floor. Oracle-independent backstop; `minBid` is
    /// guaranteed at or above it.
    pub min_price: u128,
    /// Round the ask side up rather than down. Default (`true`) is pool-favourable. Set
    /// `false` only to reproduce the §4.3 on-chain reconstruction bit for bit.
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
    /// [`LadderError::PriceOutOfRange`] if `reference_mid` is above [`MAX_PRICE`].
    pub fn skewed_mid(&self) -> Result<u128, LadderError> {
        if self.reference_mid > MAX_PRICE {
            return Err(LadderError::PriceOutOfRange);
        }
        let skew = i128::from(self.skew_bps).clamp(-(MAX_BPS as i128), MAX_BPS as i128);
        // 10_000 - skew lands in 1..=19_999, always positive.
        let factor = (BPS as i128 - skew) as u128;
        let m = mul_div_floor(self.reference_mid, factor, BPS).unwrap_or(MAX_PRICE);
        Ok(m.clamp(1, MAX_PRICE))
    }

    /// Build the row. Always produces something `PropCurve.validateLadder` accepts.
    ///
    /// # Errors
    /// [`LadderError::PriceOutOfRange`] if `reference_mid` exceeds [`MAX_PRICE`];
    /// [`LadderError::InfeasibleBounds`] if `min_price` leaves no room for the strict
    /// `maxAsk > minBid` (that is, `min_price >= MAX_PRICE`);
    /// [`LadderError::Rejected`] never — it is the module's own assertion and its firing
    /// would mean a proof above is wrong.
    pub fn build(&self) -> Result<Ladder, LadderError> {
        // Step 5 needs one unit of headroom below MAX_PRICE for max_ask to sit above min_bid.
        if self.min_price >= MAX_PRICE {
            return Err(LadderError::InfeasibleBounds);
        }
        let m = self.skewed_mid()?;

        // Step 1. Both in hundredths of a bp -- see [`BPS_E2`].
        let hs = u128::from(self.half_spread_bps_e2).min(MAX_BPS_E2);
        let w = u128::from(self.width_bps_e2).min(MAX_BPS_E2);

        // Step 3. Bid side down, ask side (by default) up.
        let raw_max_bid = mul_div_floor(m, BPS_E2 - hs, BPS_E2).unwrap_or(0);
        let raw_min_bid = mul_div_floor(raw_max_bid, BPS_E2 - w, BPS_E2).unwrap_or(0);
        let up = self.round_ask_up;
        let raw_min_ask = round(up, m, BPS_E2 + hs, BPS_E2);
        let raw_max_ask = round(up, raw_min_ask, BPS_E2 + w, BPS_E2);

        // Step 4. Monotone bottom-up sweep. `hi_body = MAX_PRICE - 1` is >= min_price by the
        // guard above, so every clamp below has `lo <= hi`.
        let hi_body = MAX_PRICE - 1;
        let min_bid = raw_min_bid.clamp(self.min_price, hi_body);
        let max_bid = raw_max_bid.clamp(min_bid, hi_body);
        let min_ask = raw_min_ask.clamp(max_bid, hi_body);
        // Step 5, folded into the lower bound: max_ask >= max(min_ask, min_bid + 1).
        let max_ask = raw_max_ask.clamp(min_ask.max(min_bid + 1), MAX_PRICE);

        let ladder = Ladder {
            min_bid,
            max_bid,
            min_ask,
            max_ask,
        };
        // Step 6.
        validate_ladder(min_bid, max_bid, min_ask, max_ask, self.min_price)?;
        Ok(ladder)
    }
}

/// `ceil(a*b/d)` when `up`, else `floor`. Saturates at [`MAX_PRICE`] rather than failing:
/// step 4 clamps every price into range anyway, and a saturating projection keeps `build`
/// total.
fn round(up: bool, a: u128, b: u128, d: u128) -> u128 {
    let r = if up {
        mul_div_ceil(a, b, d)
    } else {
        mul_div_floor(a, b, d)
    };
    r.unwrap_or(MAX_PRICE).min(MAX_PRICE)
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
        // Negative skew is the mirror.
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
        // Zero mid, zero knobs: the all-zero ladder would be rejected by the strict
        // maxAsk > minBid, so the builder must not produce it.
        let l = LadderBuilder::new(0).build().unwrap();
        l.validate(0).unwrap();
        assert!(l.max_ask > l.min_bid);

        // Mid at the ceiling with no spread: max_ask must still clear min_bid.
        let l = LadderBuilder::new(MAX_PRICE).build().unwrap();
        l.validate(0).unwrap();
        assert!(l.max_ask > l.min_bid);
        assert!(l.max_ask <= MAX_PRICE);

        // Floor pinned one unit below the ceiling: the tightest feasible row.
        let l = LadderBuilder {
            min_price: MAX_PRICE - 1,
            ..LadderBuilder::new(MAX_PRICE)
        }
        .build()
        .unwrap();
        assert_eq!(l.min_bid, MAX_PRICE - 1);
        assert_eq!(l.max_ask, MAX_PRICE);
        l.validate(MAX_PRICE - 1).unwrap();
    }

    #[test]
    fn out_of_range_inputs_are_rejected() {
        assert_eq!(
            LadderBuilder::new(MAX_PRICE + 1).build(),
            Err(LadderError::PriceOutOfRange)
        );
        assert_eq!(
            LadderBuilder {
                min_price: MAX_PRICE,
                ..LadderBuilder::new(MAX_PRICE)
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
