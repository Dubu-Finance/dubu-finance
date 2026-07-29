//! Strategy knobs plus a fair value, in; a packed `updateQuote` word, out.
//!
//! # Every integer step here is `dubu-core`'s
//!
//! This module contains no arithmetic of its own beyond `min`/`max`. The skew is
//! [`LadderBuilder::skewed_mid`], the spread projection is [`mul_div_floor`]/[`mul_div_ceil`],
//! the ladder is [`solve_two_sided`], the validation is [`Ladder::validate`], the round-trip
//! check is [`avg_bid_price`]/[`avg_ask_price`], the fill check is [`amount_out_bid`]/
//! [`amount_in_ask`], and the encoding is [`QuoteWord`]. A second implementation of the curve
//! in this crate is the defect the whole `dubu-core` port exists to prevent, and the way that
//! defect gets reintroduced is one innocent-looking `price * bps / 10_000` written inline
//! because reaching for the library felt heavy.
//!
//! # The pipeline
//!
//! ```text
//! fair value (pool price scale)
//!   -> skewed mid                         LadderBuilder::skewed_mid
//!   -> bid target = mid * (1 - hs)        floored, worse for the taker
//!      ask target = mid * (1 + hs)        ceiled,  worse for the taker
//!   -> SolveInput per side, with bounds   see "the bounds" below
//!   -> solve_two_sided                    widest safe ladder around each target
//!   -> validate_ladder                    the exact check the chain runs
//!   -> round-trip + fill check            the chain's forward path, off-chain
//!   -> QuoteWord::pack                    one calldata word
//! ```
//!
//! # The bounds, and the one thing they are not
//!
//! The bid's ceiling is the skewed mid and the ask's floor is the skewed mid. So `maxBid` can
//! never be lifted above fair value by the solver's impact term, and `minAsk` can never be
//! pushed below it. Combined with the pool's own `minPrice`, that is what the solver's width
//! search is squeezed between, and a `WidthBinding::Boundary` result means the requested width
//! could not fit under that ceiling.
//!
//! **This is a self-consistency bound, not a deviation check.** It constrains the ladder
//! against the very number that produced it, so it cannot catch a wrong fair value — if the
//! feed says 1200 and the market is at 1943, every price here is coherently, confidently wrong.
//! The bound that catches *that* has to come from a source this bot does not price from, which
//! is Pyth, on chain, inside `updateQuote`. It is not built on either side yet. Nothing in this
//! module should be read as providing it.
//!
//! # The epoch the solve assumes
//!
//! [`solve_two_sided`] solves at zero usage: it finds the widest ladder whose *average* price
//! over `[0, capture]` of a fresh epoch is the target. When the live epoch is partly consumed,
//! the row that gets posted therefore executes from `used`, not from zero, and the realised top
//! is below the solved one. That is not corrected here, because correcting it would mean
//! quoting a better price to compensate for depth already sold. It is accounted for where it
//! matters instead: [`crate::policy`] measures drift against the executable top *at the current
//! usage*, so the decision to push is made on the price a taker would really get.

use dubu_core::curve::{
    amount_in_ask, amount_out_bid, avg_ask_price, avg_bid_price, Ladder, AMOUNT_MAX, PRICE_MAX,
};
use dubu_core::inverse::{solve_two_sided, Solution, SolveInput, WidthBinding};
use dubu_core::ladder::{LadderBuilder, BPS_E2};
use dubu_core::math::{mul_div_ceil, mul_div_floor};
use dubu_core::pack::QuoteWord;
use dubu_core::{CurveError, LadderError, PackError};

/// Why a row could not be produced. Every variant means "do not send", and every one is logged.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BuildError {
    /// The fair value, or a price derived from it, is outside `uint56`.
    #[error("price out of range: {0}")]
    Price(&'static str),
    /// The inverse solver refused the inputs.
    #[error("solver: {0}")]
    Solver(LadderError),
    /// The produced row failed the on-chain validator. Unreachable by construction; if it ever
    /// fires, a proof in `dubu-core` is wrong and the row must not be sent.
    #[error("row failed the on-chain validator: {0}")]
    Invalid(CurveError),
    /// The row does not price the size it claims to.
    #[error("round trip: {0}")]
    RoundTrip(String),
    /// The row would not encode.
    #[error("packing: {0}")]
    Pack(PackError),
    /// One side has no capacity, so there is nothing to solve a ladder over.
    #[error("zero {side} capacity")]
    ZeroCapacity {
        /// `"bid"` or `"ask"`.
        side: &'static str,
    },
}

impl From<LadderError> for BuildError {
    fn from(e: LadderError) -> Self {
        Self::Solver(e)
    }
}
impl From<PackError> for BuildError {
    fn from(e: PackError) -> Self {
        Self::Pack(e)
    }
}

/// Everything needed to produce one row.
#[derive(Debug, Clone, Copy)]
pub struct RowInputs {
    /// Pair id.
    pub pair_id: u16,
    /// Fair value, already converted into this pair's pool price scale.
    pub fair: u128,
    /// Half-spread in bps, **including** any degraded-mode widening.
    pub half_spread_bps_e2: u32,
    /// Ladder width in bps of the near price — an upper bound on the width, not a target.
    pub width_bps_e2: u32,
    /// Inventory skew in bps. Positive shifts the whole book down.
    pub skew_bps: i16,
    /// Size the average price is guaranteed over.
    pub capture: u128,
    /// Bid capacity that will be in force when this row executes.
    pub bid_capacity: u128,
    /// Ask capacity that will be in force when this row executes.
    pub ask_capacity: u128,
    /// The pool's own `minPrice` floor for this pair.
    pub min_price: u128,
    /// The pair's decimal alignment.
    pub price_scale_exp: u8,
}

/// A row that has passed every off-chain check the chain will re-run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuoteRow {
    /// Pair id.
    pub pair_id: u16,
    /// The fair value it was built from.
    pub fair: u128,
    /// Skewed mid, which is also the bid ceiling and the ask floor.
    pub mid: u128,
    /// Bid target: the average price a taker gets over `[0, capture]` of a fresh epoch.
    pub bid_target: u128,
    /// Ask target, likewise.
    pub ask_target: u128,
    /// The four prices.
    pub ladder: Ladder,
    /// The packed word, ready for `updateQuote`.
    pub word: QuoteWord,
    /// Bid-side solution, for logging: width, impact, and which bound pinned it.
    pub bid: Solution,
    /// Ask-side solution, before any crossed-book repair.
    pub ask: Solution,
    /// Set when the two sides crossed and the ask had to be lifted. Persistently set means the
    /// two targets are being derived from inconsistent inputs, which is worth an alert.
    pub ask_repaired: bool,
    /// Realised average bid over the capture, by the chain's own arithmetic. At or below
    /// [`QuoteRow::bid_target`].
    pub realised_bid: u128,
    /// Realised average ask over the capture. At or above [`QuoteRow::ask_target`].
    pub realised_ask: u128,
    /// Quote the pool would pay for `capture` base on the bid side.
    pub bid_capture_cost: u128,
    /// Quote the pool would collect for `capture` base on the ask side.
    pub ask_capture_cost: u128,
}

impl QuoteRow {
    /// The packed calldata word, as the `uint256` `updateQuote` takes.
    ///
    /// # Errors
    /// [`PackError`] if a price is out of range, which [`build`] has already excluded.
    pub fn packed(&self) -> Result<[u8; 32], PackError> {
        self.word.pack()
    }

    /// Whether this row's four prices are the ones already stored on chain.
    #[must_use]
    pub fn same_as(&self, on_chain: &Ladder) -> bool {
        self.ladder == *on_chain
    }
}

/// Build and fully validate one row.
///
/// The order of the checks is the order in which a failure is cheapest to diagnose: inputs,
/// solve, validate, round trip, fill, pack.
///
/// # Errors
/// [`BuildError`]. Every one of them means the row is dropped and nothing is sent.
pub fn build(input: &RowInputs) -> Result<QuoteRow, BuildError> {
    if input.bid_capacity == 0 {
        return Err(BuildError::ZeroCapacity { side: "bid" });
    }
    if input.ask_capacity == 0 {
        return Err(BuildError::ZeroCapacity { side: "ask" });
    }
    if input.fair == 0 || input.fair > PRICE_MAX {
        return Err(BuildError::Price("fair value is zero or above uint56"));
    }

    // Step 1: the skew, through `dubu-core`'s own implementation so that the sign convention
    // (positive skew pushes the book DOWN) cannot drift from the one documented there.
    let builder = LadderBuilder {
        skew_bps: input.skew_bps,
        ..LadderBuilder::new(input.fair)
    };
    let mid = builder.skewed_mid()?;

    // Step 2: the two targets. Floor the bid and ceil the ask — both away from the taker, the
    // same direction every rounding in `dubu-core` takes.
    let hs = u128::from(input.half_spread_bps_e2).min(dubu_core::ladder::BPS_E2_MAX);
    let bid_target =
        mul_div_floor(mid, BPS_E2 - hs, BPS_E2).ok_or(BuildError::Price("bid target"))?;
    let ask_target =
        mul_div_ceil(mid, BPS_E2 + hs, BPS_E2).ok_or(BuildError::Price("ask target"))?;
    if ask_target > PRICE_MAX {
        return Err(BuildError::Price("ask target above uint56"));
    }
    // A half-spread that rounds away to nothing on a tiny price would quote a locked book. The
    // validator's strict `maxAsk > minBid` would still pass, but the row is not a spread.
    if bid_target == 0 {
        return Err(BuildError::Price("bid target floored to zero"));
    }

    // Step 3: the width bound, in price units, from bps of each near price.
    let wb = u128::from(input.width_bps_e2);
    let bid_width = mul_div_floor(bid_target, wb, BPS_E2).ok_or(BuildError::Price("bid width"))?;
    let ask_width = mul_div_floor(ask_target, wb, BPS_E2).ok_or(BuildError::Price("ask width"))?;

    // Step 4: bounds. The bid may not be lifted above the mid; the ask may not be pushed below
    // it. See the module docs for what this bound is and — more importantly — is not.
    let bid_in = SolveInput {
        target: bid_target,
        capture: input.capture,
        capacity: input.bid_capacity,
        requested_width: bid_width,
        min_price: input.min_price,
        max_price: mid,
    };
    let ask_in = SolveInput {
        target: ask_target,
        capture: input.capture,
        capacity: input.ask_capacity,
        requested_width: ask_width,
        min_price: input.min_price.max(mid),
        max_price: PRICE_MAX,
    };

    // Step 5: solve. `solve_two_sided` runs `validate_ladder` against `bid_in.min_price`, which
    // is the pool's own floor, so a row that comes back has already passed the chain's check.
    let solved = solve_two_sided(&bid_in, &ask_in)?;
    let ladder = solved.ladder;

    // Step 6: run the validator again, explicitly, at the call site that sends. Redundant with
    // the line above by construction — and the whole point is that a future refactor which
    // makes it non-redundant fails here rather than on chain, where a rejected `updateQuote`
    // burns the block and leaves the previous quote to go stale.
    ladder
        .validate(input.min_price)
        .map_err(BuildError::Invalid)?;

    // Step 7: the round trip. What does the chain's arithmetic say this row actually pays over
    // the capture? `dubu-core` guarantees the residual lands on the pool's side of the target
    // and is under one price unit; asserting it here means a regression in the solver shows up
    // as a dropped row rather than as a quote we believed was something else.
    let cap = input.capture.min(input.bid_capacity);
    let realised_bid = avg_bid_price(ladder.min_bid, ladder.max_bid, input.bid_capacity, 0, cap)
        .map_err(BuildError::Invalid)?;
    let ask_cap = input.capture.min(input.ask_capacity);
    let realised_ask = avg_ask_price(
        ladder.min_ask,
        ladder.max_ask,
        input.ask_capacity,
        0,
        ask_cap,
    )
    .map_err(BuildError::Invalid)?;

    if realised_bid > bid_target {
        return Err(BuildError::RoundTrip(format!(
            "realised bid {realised_bid} is above the target {bid_target}: the row pays a taker more than intended"
        )));
    }
    if realised_ask < ask_target {
        return Err(BuildError::RoundTrip(format!(
            "realised ask {realised_ask} is below the target {ask_target}: the row sells cheaper than intended"
        )));
    }

    // Step 8: the fill check. Push the capture size through the chain's forward quote path in
    // both directions. This is what catches a capacity/price/exponent combination that lands
    // outside the shared `uint128` domain — the pool would revert `AmountOutOfDomain` and the
    // ladder would sit unfillable behind a `QuoteUpdated` event that looked like a success.
    let bid_capture_cost = amount_out_bid(
        cap,
        ladder.min_bid,
        ladder.max_bid,
        input.bid_capacity,
        0,
        input.price_scale_exp,
    )
    .map_err(BuildError::Invalid)?;
    let ask_capture_cost = amount_in_ask(
        ask_cap,
        ladder.min_ask,
        ladder.max_ask,
        input.ask_capacity,
        0,
        input.price_scale_exp,
    )
    .map_err(BuildError::Invalid)?;
    if bid_capture_cost == 0 || ask_capture_cost == 0 {
        return Err(BuildError::RoundTrip(
            "the capture size prices to zero on one side; the row would quote nothing".into(),
        ));
    }

    // Step 9: encode, and prove the encoding round-trips before it is anything's calldata.
    let word = QuoteWord::new(input.pair_id, ladder);
    let bytes = word.pack()?;
    if QuoteWord::unpack(&bytes)? != word {
        return Err(BuildError::Pack(PackError::NonCanonical));
    }

    Ok(QuoteRow {
        pair_id: input.pair_id,
        fair: input.fair,
        mid,
        bid_target,
        ask_target,
        ladder,
        word,
        bid: solved.bid,
        ask: solved.ask,
        ask_repaired: solved.ask_repaired,
        realised_bid,
        realised_ask,
        bid_capture_cost,
        ask_capture_cost,
    })
}

/// Which constraint pinned each side's width, for logging.
#[must_use]
pub const fn bindings(row: &QuoteRow) -> (WidthBinding, WidthBinding) {
    (row.bid.binding, row.ask.binding)
}

// ---------------------------------------------------------------------------
// Capacity
// ---------------------------------------------------------------------------

/// A capacity epoch the pool can actually honour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapacityPlan {
    /// Base the pool will buy.
    pub bid: u128,
    /// Base the pool will sell.
    pub ask: u128,
    /// The configured bid capacity was cut because the pool cannot pay for it.
    pub bid_cut_by_inventory: bool,
    /// The configured ask capacity was cut because the pool does not hold the base.
    pub ask_cut_by_inventory: bool,
}

/// Clamp the configured capacities to what the pool's inventory can settle.
///
/// Posting capacity the pool cannot honour is not a harmless overstatement. `PropPool` checks
/// the reserve floor at swap time, so an oversized epoch produces a ladder that quotes size and
/// then reverts `ReserveFloorBreached` when someone takes it — a quote that looks fillable to
/// every aggregator polling `getAmountOut` and is not.
///
/// The two sides are limited by different tokens:
///
/// * **ask** — the pool delivers *base*, so the limit is the base balance above its floor, in
///   the same units as the capacity field. Direct comparison.
/// * **bid** — the pool pays *quote*, so the limit is what the quote balance buys. That
///   conversion is a curve question, and it is answered with a flat ladder at the fair value
///   through [`amount_in_bid`] rather than with a division here. A flat ladder at fair is also
///   the conservative choice: the real `maxBid` sits at or below fair, so the real cost of the
///   epoch is at or below what this budgets for.
///
/// # Errors
/// [`CurveError`] only from the shared domain; a caller treats it as "cannot size an epoch".
pub fn plan_capacity(i: &CapacityInputs) -> Result<CapacityPlan, CurveError> {
    let base_available = i
        .base_balance
        .saturating_sub(i.min_base_reserve)
        .min(AMOUNT_MAX);
    let ask = i.configured_ask.min(base_available);

    let quote_available = i.quote_balance.saturating_sub(i.min_quote_reserve);
    let bid = if i.fair == 0 {
        0
    } else {
        // The least base whose flat-ladder bid quote costs at least the whole budget. One unit
        // below it is, by that minimality, strictly affordable.
        let affordable = dubu_core::curve::amount_in_bid(
            quote_available,
            i.fair,
            i.fair,
            AMOUNT_MAX,
            0,
            i.price_scale_exp,
        )
        .unwrap_or(AMOUNT_MAX)
        .saturating_sub(1);
        i.configured_bid.min(affordable).min(AMOUNT_MAX)
    };

    Ok(CapacityPlan {
        bid,
        ask,
        bid_cut_by_inventory: bid < i.configured_bid,
        ask_cut_by_inventory: ask < i.configured_ask,
    })
}

/// Inputs to [`plan_capacity`].
///
/// A struct rather than eight positional arguments, because six of them are `u128` amounts and
/// two adjacent transpositions — base for quote, or a balance for a floor — both typecheck and
/// both post an epoch the pool cannot honour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapacityInputs {
    /// Bid capacity the operator asked for.
    pub configured_bid: u128,
    /// Ask capacity the operator asked for.
    pub configured_ask: u128,
    /// Base tokens the pool holds.
    pub base_balance: u128,
    /// Quote tokens the pool holds.
    pub quote_balance: u128,
    /// `minBaseReserve` from the pair's on-chain config.
    pub min_base_reserve: u128,
    /// `minQuoteReserve` from the pair's on-chain config.
    pub min_quote_reserve: u128,
    /// Fair value, for converting the quote budget into base.
    pub fair: u128,
    /// The pair's decimal alignment.
    pub price_scale_exp: u8,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// pairId 1 as deployed: mWETH/mUSDC, 18/6, priceScaleExp 24, minPrice 1e15 (= $1000).
    /// Fair value 1943.82 in the pool's scale is `1943.82 * 10^12`.
    fn weth_inputs(fair: u128) -> RowInputs {
        RowInputs {
            pair_id: 1,
            fair,
            half_spread_bps_e2: 500,
            width_bps_e2: 2500,
            skew_bps: 0,
            capture: 20_000_000_000_000_000_000, // 20 mWETH
            bid_capacity: 1_000_000_000_000_000_000_000, // 1000 mWETH
            ask_capacity: 1_000_000_000_000_000_000_000,
            min_price: 1_000_000_000_000_000,
            price_scale_exp: 24,
        }
    }

    const FAIR: u128 = 1_943_820_000_000_000;

    #[test]
    fn a_live_shaped_row_passes_every_check() {
        let row = build(&weth_inputs(FAIR)).expect("a normal row must build");

        // The four prices are ordered and the book is open, which is what the chain checks.
        row.ladder.validate(1_000_000_000_000_000).unwrap();
        let l = row.ladder;
        assert!(l.min_bid <= l.max_bid && l.max_bid <= l.min_ask && l.min_ask <= l.max_ask);
        assert!(l.max_ask > l.min_bid);

        // 5 bp either side of fair.
        assert_eq!(row.bid_target, FAIR * 9_995 / 10_000);
        assert_eq!(row.ask_target, FAIR * 10_005 / 10_000);

        // The bid never sits above fair and the ask never below it.
        assert!(
            l.max_bid <= row.mid,
            "maxBid {} breached the fair-value ceiling {}",
            l.max_bid,
            row.mid
        );
        assert!(
            l.min_ask >= row.mid,
            "minAsk {} breached the fair-value floor {}",
            l.min_ask,
            row.mid
        );

        // And the round trip lands on the pool's side of both targets, within a price unit.
        assert!(row.realised_bid <= row.bid_target);
        assert!(row.bid_target - row.realised_bid <= 1);
        assert!(row.realised_ask >= row.ask_target);
        assert!(row.realised_ask - row.ask_target <= 1);
    }

    #[test]
    fn the_capture_prices_to_the_notional_the_operator_expects() {
        let row = build(&weth_inputs(FAIR)).unwrap();
        // 20 mWETH at 5 bp under 1943.82 is 20 * 1942.848 = 38,856.9618 mUSDC, at 6 decimals.
        assert_eq!(row.bid_capture_cost, 38_856_961_800);
        assert_eq!(row.ask_capture_cost, 38_895_838_200);
        // Sanity in human terms, independent of the exact integer: ~1942.85 and ~1944.79 a coin.
        assert_eq!(row.bid_capture_cost / 20, 1_942_848_090);
        assert_eq!(row.ask_capture_cost / 20, 1_944_791_910);

        // The pool collects more on the ask than it pays on the bid: that is the spread.
        assert!(row.ask_capture_cost > row.bid_capture_cost);
        let edge_bps =
            (row.ask_capture_cost - row.bid_capture_cost) * 10_000 / row.bid_capture_cost;
        assert_eq!(edge_bps, 10, "a 5 bp half-spread is a 10 bp round trip");
    }

    #[test]
    fn the_word_round_trips_through_the_encoder() {
        let row = build(&weth_inputs(FAIR)).unwrap();
        let bytes = row.packed().unwrap();
        let back = QuoteWord::unpack(&bytes).unwrap();
        assert_eq!(back, row.word);
        assert_eq!(back.pair_id, 1);
        assert_eq!(back.ladder, row.ladder);
        // Bits 240..255 must be clear or `updateQuote` decodes a pair id we did not mean.
        assert_eq!(bytes[0], 0);
        assert_eq!(bytes[1], 0);
    }

    #[test]
    fn width_bps_bounds_the_ladder_rather_than_setting_it() {
        let narrow = build(&RowInputs {
            width_bps_e2: 100,
            ..weth_inputs(FAIR)
        })
        .unwrap();
        let wide = build(&RowInputs {
            width_bps_e2: 100000,
            ..weth_inputs(FAIR)
        })
        .unwrap();
        let span = |r: &QuoteRow| r.ladder.max_bid - r.ladder.min_bid;
        assert!(span(&narrow) < span(&wide));
        assert_eq!(narrow.bid.binding, WidthBinding::Requested);

        // Beyond ~500 bp the fair-value ceiling on maxBid binds before the request does, and
        // the arithmetic behind that number is worth writing down because it is the knob an
        // operator will reach for first: the solver lifts maxBid by `W * K / 2C`, the ceiling
        // allows only `half_spread` of lift, so `W <= 2 * half_spread * C / K`. Here that is
        // `2 * 5bp * 1000 / 20 = 500 bp`, and 1000 bp cannot fit under it.
        assert_eq!(wide.bid.binding, WidthBinding::Boundary);
        assert!(wide.ladder.max_bid <= wide.mid);
        assert_eq!(
            build(&RowInputs {
                width_bps_e2: 50000,
                ..weth_inputs(FAIR)
            })
            .unwrap()
            .bid
            .binding,
            WidthBinding::Requested,
            "500 bp is the last width that still fits"
        );
    }

    #[test]
    fn positive_skew_moves_the_whole_book_down() {
        let flat = build(&weth_inputs(FAIR)).unwrap();
        let short = build(&RowInputs {
            skew_bps: 20,
            ..weth_inputs(FAIR)
        })
        .unwrap();
        assert!(short.ladder.max_bid < flat.ladder.max_bid);
        assert!(short.ladder.min_ask < flat.ladder.min_ask);
        // Negative skew is the mirror: the pool is short and wants to buy.
        let long = build(&RowInputs {
            skew_bps: -20,
            ..weth_inputs(FAIR)
        })
        .unwrap();
        assert!(long.ladder.max_bid > flat.ladder.max_bid);
    }

    #[test]
    fn a_fair_value_under_the_pools_floor_is_refused_not_clamped() {
        // minPrice is 1e15, i.e. $1000 a coin. A feed saying $900 must produce no row at all:
        // the pool has an absolute, oracle-independent floor and quoting through it is exactly
        // what the floor forbids. The bid's ceiling is the mid, so the mid landing below the
        // floor is caught as infeasible bounds before the target is even looked at.
        assert_eq!(
            build(&weth_inputs(900_000_000_000_000)),
            Err(BuildError::Solver(LadderError::InfeasibleBounds))
        );

        // Just above the floor, where only the half-spread pushes the bid target through it.
        assert_eq!(
            build(&weth_inputs(1_000_100_000_000_000)),
            Err(BuildError::Solver(LadderError::TargetBelowFloor))
        );

        // And one basis point higher, where it fits, so the boundary is a boundary and not a
        // blanket refusal near the floor.
        let row = build(&weth_inputs(1_001_000_000_000_000)).unwrap();
        assert!(row.ladder.min_bid >= 1_000_000_000_000_000);
    }

    #[test]
    fn zero_capacity_is_refused_before_anything_else() {
        assert_eq!(
            build(&RowInputs {
                bid_capacity: 0,
                ..weth_inputs(FAIR)
            }),
            Err(BuildError::ZeroCapacity { side: "bid" })
        );
        assert_eq!(
            build(&RowInputs {
                ask_capacity: 0,
                ..weth_inputs(FAIR)
            }),
            Err(BuildError::ZeroCapacity { side: "ask" })
        );
    }

    #[test]
    fn a_fair_value_out_of_the_uint56_domain_is_refused() {
        assert!(matches!(build(&weth_inputs(0)), Err(BuildError::Price(_))));
        assert!(matches!(
            build(&weth_inputs(PRICE_MAX + 1)),
            Err(BuildError::Price(_))
        ));
    }

    #[test]
    fn the_wbtc_pair_shape_also_builds() {
        // pairId 2: mWBTC/mUSDC, 8/6, exp 12, minPrice 5e14. 118_000 -> 118_000 * 10^10.
        let row = build(&RowInputs {
            pair_id: 2,
            fair: 1_180_000_000_000_000,
            half_spread_bps_e2: 800,
            width_bps_e2: 4000,
            skew_bps: 0,
            capture: 20_000_000,         // 0.2 mWBTC
            bid_capacity: 2_000_000_000, // 20 mWBTC
            ask_capacity: 2_000_000_000,
            min_price: 500_000_000_000_000,
            price_scale_exp: 12,
        })
        .unwrap();
        row.ladder.validate(500_000_000_000_000).unwrap();
        assert_eq!(row.word.pair_id, 2);
        assert!(row.realised_bid <= row.bid_target);
        assert!(row.realised_ask >= row.ask_target);
    }

    #[test]
    fn capacity_is_cut_to_what_the_inventory_can_settle() {
        let fair = FAIR;
        let exp = 24;
        // Plenty of both: nothing is cut. 4445 mWETH and 11.1M mUSDC, the live balances.
        let p = plan_capacity(&CapacityInputs {
            configured_bid: 1_000_000_000_000_000_000_000,
            configured_ask: 1_000_000_000_000_000_000_000,
            base_balance: 4_445_092_562_514_976_799_824,
            quote_balance: 11_111_000_000_000,
            min_base_reserve: 0,
            min_quote_reserve: 0,
            fair,
            price_scale_exp: exp,
        })
        .unwrap();
        assert_eq!(p.bid, 1_000_000_000_000_000_000_000);
        assert_eq!(p.ask, 1_000_000_000_000_000_000_000);
        assert!(!p.bid_cut_by_inventory && !p.ask_cut_by_inventory);

        // Only 10 mWETH held: the ask epoch is cut to it, the bid is untouched.
        let p = plan_capacity(&CapacityInputs {
            configured_bid: 1_000_000_000_000_000_000_000,
            configured_ask: 1_000_000_000_000_000_000_000,
            base_balance: 10_000_000_000_000_000_000,
            quote_balance: 11_111_000_000_000,
            min_base_reserve: 0,
            min_quote_reserve: 0,
            fair,
            price_scale_exp: exp,
        })
        .unwrap();
        assert_eq!(p.ask, 10_000_000_000_000_000_000);
        assert!(p.ask_cut_by_inventory && !p.bid_cut_by_inventory);
    }

    #[test]
    fn the_bid_epoch_is_cut_to_what_the_quote_balance_can_pay() {
        // 100_000 mUSDC against a 1000 mWETH bid epoch worth ~1.94M: must cut to ~51 mWETH.
        let p = plan_capacity(&CapacityInputs {
            configured_bid: 1_000_000_000_000_000_000_000,
            configured_ask: 1_000_000_000_000_000_000_000,
            base_balance: 4_445_092_562_514_976_799_824,
            quote_balance: 100_000_000_000,
            min_base_reserve: 0,
            min_quote_reserve: 0,
            fair: FAIR,
            price_scale_exp: 24,
        })
        .unwrap();
        assert!(p.bid_cut_by_inventory);
        // And what it costs is genuinely inside the budget — the property that matters.
        let cost = amount_out_bid(p.bid, FAIR, FAIR, p.bid.max(1), 0, 24).unwrap();
        assert!(
            cost <= 100_000_000_000,
            "planned bid epoch costs {cost}, above the 100000 budget"
        );
        // Not needlessly conservative either: one more wei of base would exceed it.
        let over = amount_out_bid(p.bid + 1, FAIR, FAIR, p.bid + 1, 0, 24).unwrap();
        assert!(over >= 100_000_000_000);
    }

    #[test]
    fn reserve_floors_come_off_the_top() {
        let p = plan_capacity(&CapacityInputs {
            configured_bid: 1_000_000_000_000_000_000_000,
            configured_ask: 1_000_000_000_000_000_000_000,
            base_balance: 10_000_000_000_000_000_000,
            quote_balance: 11_111_000_000_000,
            min_base_reserve: 4_000_000_000_000_000_000, /* 4 mWETH floor */
            min_quote_reserve: 0,
            fair: FAIR,
            price_scale_exp: 24,
        })
        .unwrap();
        assert_eq!(
            p.ask, 6_000_000_000_000_000_000,
            "the floor must not be quotable"
        );

        // A floor above the balance yields zero rather than underflowing.
        let p = plan_capacity(&CapacityInputs {
            configured_bid: 1,
            configured_ask: 1,
            base_balance: 1_000,
            quote_balance: 11_111_000_000_000,
            min_base_reserve: 9_999,
            min_quote_reserve: 0,
            fair: FAIR,
            price_scale_exp: 24,
        })
        .unwrap();
        assert_eq!(p.ask, 0);
    }
}
