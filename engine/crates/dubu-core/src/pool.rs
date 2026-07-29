//! Exact integer port of `PropPool`'s view quote path — the gates around [`crate::curve`], not the
//! curve itself.
//!
//! [`crate::curve`] answers what a ladder pays. `PropPool` decides whether it answers at all: a
//! paused pair, a quote the chain considers stale, an exhausted epoch, and a capacity ramp that has
//! run down all produce a zero the curve knows nothing about. Anything pricing this pool from the
//! ladder alone advertises depth the chain will refuse.
//!
//! # Why this exists off-chain at all
//!
//! The aggregator used to ask `getAmountOut` directly. That read has to be preconfirmed to be worth
//! anything — the pool re-quotes several times a second — and GIWA serves pending *state* and
//! pending *timestamp* from different points in time, so the pool computed an age that had never
//! existed and refused every quote as stale. No `maxStaleSecs` fixes that; the inputs disagree
//! before the comparison happens. So the maker serves its own quote, which is what an RFQ maker
//! does anyway, and this is the part that has to agree with the chain.
//!
//! # The split between `capacity` and `available`
//!
//! `PropPool` passes `st.capacity` to the curve and bounds the room by `st.available`. Those differ
//! whenever the staleness ramp is running, and swapping them is a silent mispricing rather than an
//! error: the ladder's price ramp is a function of `used / capacity`, so substituting the decayed
//! value steepens every quote instead of shortening it. `available` is the caller's to compute —
//! it needs a clock, and this module deliberately has none.

use crate::curve::{amount_in_ask, amount_out_ask, amount_out_bid, Ladder};
use crate::error::CurveError;

/// Which side of the book the taker is hitting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// The pool buys base and pays quote. Input is base.
    Bid,
    /// The pool sells base and collects quote. Input is quote.
    Ask,
}

/// Why the pool would refuse to quote at all, whatever the size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unquotable {
    /// Globally or per-pair paused.
    Paused,
    /// No ladder has ever been posted.
    NeverQuoted,
    /// The stored quote is stamped in the future.
    ///
    /// Impossible against a consistent view and routine against an inconsistent one, which is the
    /// whole reason this module exists — see the module note.
    ClockAhead,
    /// Older than the pair's `maxStaleSecs`.
    Stale,
}

/// Port of the status gate in `PropPool._load`. `None` means quotable.
///
/// `paused` is the caller's OR of the global halt and the pair's own flag; the chain checks them
/// in that order and produces the same status from either.
#[must_use]
pub const fn refusal(
    paused: bool,
    updated_at: u64,
    block_timestamp: u64,
    stale_secs_max: u32,
) -> Option<Unquotable> {
    if paused {
        return Some(Unquotable::Paused);
    }
    if updated_at == 0 {
        return Some(Unquotable::NeverQuoted);
    }
    if updated_at > block_timestamp {
        return Some(Unquotable::ClockAhead);
    }
    if block_timestamp - updated_at > stale_secs_max as u64 {
        return Some(Unquotable::Stale);
    }
    None
}

/// Port of `PropPool._maxAmountIn`. Largest input this side's epoch can still take.
fn amount_in_max(
    ladder: &Ladder,
    side: Side,
    capacity: u128,
    available: u128,
    used: u128,
    price_scale_exp: u8,
) -> Result<u128, CurveError> {
    debug_assert!(used < available, "callers test exhaustion before asking for room");
    debug_assert!(available <= capacity, "the ramp only ever removes depth");

    let room = available - used;
    if matches!(side, Side::Bid) {
        return Ok(room);
    }
    if available == capacity {
        return amount_in_ask(
            room,
            ladder.min_ask,
            ladder.max_ask,
            capacity,
            used,
            price_scale_exp,
        );
    }

    // The decayed arm asks for one unit more than the room and steps back, because the exact cost
    // of `room` base can round to an input that buys `room + 1`.
    let cost = amount_in_ask(
        room + 1,
        ladder.min_ask,
        ladder.max_ask,
        capacity,
        used,
        price_scale_exp,
    )?;
    debug_assert!(cost > 0, "a non-zero base amount at a non-zero price ceils to at least one");
    Ok(cost.saturating_sub(1))
}

/// Port of `PropPool._outFor`. Exact input; `amount_in` is base for a bid and quote for an ask.
///
/// `Ok(0)` is the answer for every unfillable condition, matching the chain's revert-free view
/// path. An `Err` means the *port* disagrees with its own preconditions and is worth surfacing
/// rather than flattening — on chain the equivalent would be a panic inside a view.
///
/// # Errors
///
/// Propagates [`CurveError`] from the underlying curve, which the gates above are meant to exclude.
pub fn amount_out(
    amount_in: u128,
    ladder: &Ladder,
    side: Side,
    capacity: u128,
    available: u128,
    used: u128,
    price_scale_exp: u8,
) -> Result<u128, CurveError> {
    debug_assert!(available <= capacity, "the ramp only ever removes depth");

    if amount_in == 0 || available == 0 || used >= available {
        return Ok(0);
    }
    if amount_in > amount_in_max(ladder, side, capacity, available, used, price_scale_exp)? {
        return Ok(0);
    }

    match side {
        Side::Bid => amount_out_bid(
            amount_in,
            ladder.min_bid,
            ladder.max_bid,
            capacity,
            used,
            price_scale_exp,
        ),
        Side::Ask => amount_out_ask(
            amount_in,
            ladder.min_ask,
            ladder.max_ask,
            capacity,
            used,
            price_scale_exp,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXP: u8 = 24;
    const CAP: u128 = 1_000_000_000_000_000_000_000;

    fn ladder() -> Ladder {
        Ladder {
            min_bid: 1_990_000_000_000_000,
            max_bid: 2_000_000_000_000_000,
            min_ask: 2_000_100_000_000_000,
            max_ask: 2_010_000_000_000_000,
        }
    }

    #[test]
    fn a_paused_pair_is_refused_before_its_clock_is_consulted() {
        assert_eq!(refusal(true, 0, 0, 0), Some(Unquotable::Paused));
    }

    /// The failure that moved this pricing off chain: a view whose state and timestamp came from
    /// different moments stamps the quote in the future.
    #[test]
    fn a_quote_stamped_ahead_of_the_block_is_named_rather_than_read_as_stale() {
        assert_eq!(refusal(false, 101, 100, 5), Some(Unquotable::ClockAhead));
    }

    #[test]
    fn staleness_is_exclusive_at_the_boundary() {
        assert_eq!(refusal(false, 95, 100, 5), None, "exactly at the window");
        assert_eq!(refusal(false, 94, 100, 5), Some(Unquotable::Stale));
    }

    #[test]
    fn a_never_quoted_pair_is_distinguishable_from_a_stale_one() {
        assert_eq!(refusal(false, 0, 100, 5), Some(Unquotable::NeverQuoted));
    }

    #[test]
    fn an_exhausted_epoch_pays_nothing() {
        let out = amount_out(1, &ladder(), Side::Bid, CAP, CAP, CAP, EXP).expect("gated");
        assert_eq!(out, 0, "used == available is exhausted");
    }

    #[test]
    fn zero_in_is_zero_out_on_both_sides() {
        for side in [Side::Bid, Side::Ask] {
            assert_eq!(
                amount_out(0, &ladder(), side, CAP, CAP, 0, EXP).expect("gated"),
                0
            );
        }
    }

    /// Past the epoch's room the answer is a refusal, not a clamped fill.
    #[test]
    fn an_oversized_bid_is_refused_rather_than_truncated() {
        let whole = amount_out(CAP, &ladder(), Side::Bid, CAP, CAP, 0, EXP).expect("whole epoch");
        assert!(whole > 0, "the whole epoch is fillable");
        assert_eq!(
            amount_out(CAP + 1, &ladder(), Side::Bid, CAP, CAP, 0, EXP).expect("gated"),
            0
        );
    }

    /// The ramp bounds the depth on offer without touching the price. Both halves matter: a decayed
    /// epoch must refuse what it can no longer cover, and must still pay the undecayed rate for
    /// what it can.
    #[test]
    fn the_ramp_removes_depth_without_repricing_what_is_left() {
        let half = CAP / 2;
        assert_eq!(
            amount_out(half + 1, &ladder(), Side::Bid, CAP, half, 0, EXP).expect("gated"),
            0,
            "beyond the decayed room"
        );

        let decayed = amount_out(half, &ladder(), Side::Bid, CAP, half, 0, EXP).expect("in room");
        let fresh = amount_out(half, &ladder(), Side::Bid, CAP, CAP, 0, EXP).expect("in room");
        assert_eq!(
            decayed, fresh,
            "the price ramp reads `used / capacity`, which the decay does not move"
        );
    }

    /// `available` is the room and `capacity` is the price. Feeding the decayed value to the curve
    /// instead would steepen the ladder, which is a mispricing no assertion downstream would catch.
    #[test]
    fn substituting_available_for_capacity_would_change_the_price() {
        let half = CAP / 2;
        let correct = amount_out(half, &ladder(), Side::Bid, CAP, half, 0, EXP).expect("in room");
        let wrong = amount_out(half, &ladder(), Side::Bid, half, half, 0, EXP).expect("in room");
        assert_ne!(
            correct, wrong,
            "if these agreed, the capacity/available split would be untestable"
        );
    }

    #[test]
    fn an_ask_takes_quote_in_and_pays_base_out() {
        let cost = amount_in_ask(
            CAP / 1_000,
            ladder().min_ask,
            ladder().max_ask,
            CAP,
            0,
            EXP,
        )
        .expect("cost");
        let base = amount_out(cost, &ladder(), Side::Ask, CAP, CAP, 0, EXP).expect("ask");
        assert!(base > 0, "quote in, base out");
        assert!(base <= CAP / 1_000, "never more base than the input paid for");
    }
}
