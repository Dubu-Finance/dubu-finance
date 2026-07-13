//! The inverse ladder solver: target executable price + capture size -> ladder.
//!
//! This is what the quoter actually runs, and it is the one piece of the engine with no Solidity
//! counterpart. archi_v2 §5.2 sketches it; this module derives it, proves the integer bounds are
//! tight, and implements it exactly.
//!
//! # 0. Which forward curve this is derived against
//!
//! `PropCurve` was amended to stop quantising the intermediate price. It folds the price into the
//! amount and rounds exactly once, so there is no longer an integer `avgBid` for the solver to
//! hit. What the chain evaluates, over the base interval `[0, K]` of a side with capacity `C`, is
//!
//! ```text
//! amountOut = floor( K * (2*maxBid*C - W*K) / (2*C*S) )
//! ```
//!
//! whose implied average price is the **exact rational** `maxBid - W*K/(2C)`. Every previous
//! revision of this module inverted a quantised price and could therefore close the round trip
//! exactly; that is no longer available, and pretending otherwise would be the bug. What is
//! available — and what is derived below — is a round trip that lands *at or below* the target on
//! the bid side and *at or above* it on the ask side, by strictly less than one price unit.
//!
//! # 1. Forward map
//!
//! Write `W = maxBid - minBid` for the ladder width, `C` for the base capacity, and `K` for the
//! capture size, clamped to `C`. The solver quotes from a freshly reset epoch (`u = 0`), so the
//! doubled midpoint numerator `2u + q` is just `K`, and the impact on the average price is the
//! rational `W*K/(2C)`.
//!
//! # 2. What "target" means
//!
//! The strategy hands us a price `T` it wants a taker to actually get on average over a capture
//! chunk of size `K`. So the equation to invert is
//!
//! ```text
//! maxBid - W*K/(2C)  ==  T
//! ```
//!
//! # 3. Inversion, and exactly how exact it is
//!
//! The equation is linear in `maxBid`, and the impact term depends only on `(W, K, C)` — never on
//! `maxBid` — so it inverts in closed form with no search:
//!
//! ```text
//! impact = floor(W*K / (2C))
//! maxBid = T + impact
//! minBid = maxBid - W
//! ```
//!
//! **FLOOR, not ceil.** The realised average is `maxBid - W*K/(2C) = T + floor(x) - x`, i.e.
//! `T - frac(x)`, which lies in `(T - 1, T]`. A lower bid is worse for the taker, so the residual
//! is pool-favourable, and it is exactly zero iff `2C | W*K`. Ceiling the impact instead would put
//! the realised average in `[T, T + 1)` — above the target, i.e. *taker*-favourable — which is the
//! one direction that is not allowed. There is no third option: exactness for arbitrary `W` would
//! require `2C | W*K`, and the only way to force that is to snap `W` down to a multiple of
//! `2C / gcd(2C, K)`, which for a small capture against a large capacity collapses the width to
//! zero and posts no depth at all. A sub-unit pool-favourable residual is strictly better than no
//! ladder.
//!
//! Restated as the invariant the property tests assert: for every input in the domain,
//!
//! ```text
//! T - 1  <  realised_bid  <=  T          and          T  <=  realised_ask  <  T + 1
//! ```
//!
//! and in amount terms the solved ladder never pays a taker more quote than a flat ladder posted
//! at `T` would, nor charges less.
//!
//! The two other places a residual can appear are structural rather than arithmetic, and both are
//! pool-favourable: [`solve_two_sided`]'s crossed-book repair, which only ever raises the ask, and
//! a trade that is not the one solved for (a different size, or a partly consumed epoch), where
//! §1's monotonicity in the consumed interval does the work.
//!
//! # 4. Widest safe width
//!
//! `W` is free — it is the *concentration* knob (a `width_bps` around
//! 0.20, i.e. the price decays 0.2 bp across the entire posted depth). Wider is better for the
//! pool: the same top-of-book price is offered with more depth behind it before the price walks.
//! Two bounds constrain it.
//!
//! Two elementary lemmas do all the work. For integers `a >= 1`, `d >= 1`, `N >= 0`:
//!
//! ```text
//! (A)  ceil(W*a/d)  <= N   <=>  W*a <= N*d          <=>  W <= floor(N*d / a)
//! (B)  floor(W*a/d) <= N   <=>  W*a <  d*(N+1)      <=>  W <= floor((d*(N+1) - 1) / a)
//! ```
//!
//! `(A)` because `ceil(x) <= N <=> x <= N` for integral `N`; `(B)` because
//! `floor(x) <= N <=> x < N + 1`, and `W*a < d*(N+1)` over the integers is `W*a <= d*(N+1) - 1`.
//!
//! **Boundary bound** (the near endpoint must stay under a ceiling `P_hi` — the reference-oracle
//! deviation clamp of archi_v2 §5.4, or simply `MAX_PRICE`). With `H_i = P_hi - T` we need
//! `impact = floor(W*K/(2C)) <= H_i`, which is lemma (B) with `a = K`, `d = 2C`:
//!
//! ```text
//! W_boundary = floor((2C*(H_i + 1) - 1) / K)
//! ```
//!
//! *Safe:* `W*K <= 2C*(H_i+1) - 1 < 2C*(H_i+1)` gives `W*K/(2C) < H_i + 1`, so
//! `floor(W*K/(2C)) <= H_i`. *Tight:* `W+1 > (2C*(H_i+1) - 1)/K` gives `(W+1)*K >= 2C*(H_i+1)`,
//! so `floor((W+1)*K/(2C)) >= H_i + 1`. ∎
//!
//! When `K == 0` the impact is identically zero and the bound is vacuous.
//!
//! **Endpoint bound** (the far endpoint must stay above a floor `P_lo` — `minPrice` or the
//! cost-basis floor). With `H_e = T - P_lo` we need `span(W) = W - floor(W*K/(2C)) <= H_e`. The
//! floor collapses that expression: for integral `W`, `-floor(x) = ceil(-x)`, so
//!
//! ```text
//! span(W) = W - floor(W*K/(2C)) = ceil(W - W*K/(2C)) = ceil(W * (2C - K) / (2C))
//! ```
//!
//! which is lemma (A) with `a = 2C - K =: n`, `d = 2C`:
//!
//! ```text
//! W_endpoint = floor(H_e * 2C / n)
//! ```
//!
//! *Safe:* `W*n <= H_e*2C` gives `W*n/(2C) <= H_e`, so `ceil(W*n/(2C)) <= H_e` as `H_e` is an
//! integer. *Tight:* `W+1 > H_e*2C/n` gives `(W+1)*n/(2C) > H_e`, and the ceiling of that is
//! `>= H_e + 1`. ∎
//!
//! `span` is non-decreasing in `W`, so a single largest-satisfying `W` is exactly what these
//! bounds compute.
//!
//! ## Which bound carries the `-1`, and why it moved back
//!
//! The correction always sits on whichever of the two expressions is a `floor`:
//!
//! ```text
//!                       impact            span              boundary   endpoint
//! floored impact   floor(W*K/2C)   ceil(W*n/2C)        lemma (B)  lemma (A)
//! ceiled  impact    ceil(W*K/2C)  floor(W*n/2C)        lemma (A)  lemma (B)
//! ```
//!
//! The revision before this one ceiled the impact and therefore had the bottom row. Amendment 4
//! made the forward map's implied impact an exact rational and forced the solver to *floor* it
//! (§3), which puts us back on the top row: `W_boundary` carries the `-1` and `W_endpoint` is a
//! bare floor division. Using the wrong row is not merely loose, it is unsound in one direction —
//! a bound one unit too large lets the near endpoint breach the price ceiling it was supposed to
//! respect. `floor_binds_the_boundary_width` pins that case.
//!
//! ## The endpoint denominator is never zero
//!
//! `n = 2C - K` with `K <= C` and `C >= 1` gives `n >= C >= 1`, so the division is always
//! defined. The previous revision needed a special case here (`m == C` was reachable once the
//! midpoint ceiled); working in doubled units removes it.
//!
//! Finally `W = min(W_requested, W_boundary, W_endpoint, MAX_PRICE)`.
//!
//! # 5. Ask side
//!
//! Exactly mirrored. The realised average is `minAsk + W*K/(2C)`, so `minAsk = T - impact` and
//! `maxAsk = minAsk + W` with the *same* floored impact — which puts the realised ask in
//! `[T, T + 1)`, i.e. at or worse for the taker. The floor is the near endpoint and the ceiling is
//! the far endpoint, so the two headrooms swap roles: `H_i = T - P_lo` bounds the impact and
//! `H_e = P_hi - T` bounds the span. [`widest_width`] serves both.
//!
//! # 6. Rounding direction, summarised
//!
//! One rule: every rounding is chosen so that a taker can never do better than the target.
//! The impact floors on both sides (§3, §5); the two width bounds floor, because a width bound
//! must never be overstated. A narrower ladder posts less depth behind the same price, which
//! costs the pool volume, never money.

use crate::curve::{amount_out_bid, avg_bid_price, Ladder, MAX_AMOUNT, MAX_PRICE};
use crate::error::LadderError;
use crate::math::{mul_div_floor, mul_div_rem};

/// Which constraint pinned the ladder width. Logged by the quoter: a solver that is permanently
/// `Boundary`-bound is being squeezed by the oracle clamp, which is a different problem from one
/// that is permanently `Requested`-bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WidthBinding {
    /// The strategy's requested width was the smallest.
    Requested,
    /// The price ceiling on the near endpoint bound it (`W_boundary`).
    Boundary,
    /// The price floor on the far endpoint bound it (`W_endpoint`).
    Endpoint,
    /// Nothing bound it; the width saturated at [`MAX_PRICE`].
    Saturated,
}

/// Inputs to one side of the inverse solver.
///
/// `capture` and `capacity` are in **base** units on both sides, matching the amended
/// `PropCurve`, whose ask capacity is no longer quote-denominated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SolveInput {
    /// The average execution price the taker must get over `[0, capture]`.
    pub target: u128,
    /// The trade size, in base units, over which `target` must hold on average.
    pub capture: u128,
    /// Total base capacity posted for this epoch.
    pub capacity: u128,
    /// Width the strategy would like, in price units. Use `MAX_PRICE` for "as wide as safe".
    pub requested_width: u128,
    /// Hard floor: no price in the produced ladder may fall below this.
    pub min_price: u128,
    /// Hard ceiling: no price in the produced ladder may rise above this. Cap it at [`MAX_PRICE`]
    /// at minimum; tighten it with the reference-oracle clamp.
    pub max_price: u128,
}

/// A solved single-sided ladder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Solution {
    /// `minBid` on the bid side, `minAsk` on the ask side.
    pub low: u128,
    /// `maxBid` on the bid side, `maxAsk` on the ask side.
    pub high: u128,
    /// `high - low`.
    pub width: u128,
    /// `min(capture, capacity)` — the interval the target is guaranteed over. This is also the
    /// doubled midpoint numerator `2u + q` at `u == 0`, which is why the previous revision's
    /// separate `mid_usage` field is gone: in doubled units the two coincide.
    pub effective_capture: u128,
    /// `floor(width * effective_capture / (2 * capacity))` — the price impact baked into the
    /// near endpoint. Floored, which is what makes the realised average land on the pool's side
    /// of the target; see §3.
    pub impact: u128,
    /// Which constraint pinned [`Solution::width`].
    pub binding: WidthBinding,
}

/// Largest `W` satisfying both integer bounds of §4.
///
/// * `headroom_impact` bounds `impact = floor(W * K / (2C))` — lemma (B), which carries the `-1`.
/// * `headroom_span` bounds `span = W - floor(W * K / (2C)) = ceil(W * (2C - K) / (2C))` — lemma
///   (A), a bare floor division.
///
/// Which of the two carries the correction is decided by the forward path's rounding, and it is
/// the reverse of what a ceiled impact would need. See §4 of the module docs.
///
/// Preconditions: `twice_capacity == 2 * capacity` with `capacity > 0`, and `capture <= capacity`
/// (so `capture < twice_capacity`).
#[must_use]
pub fn widest_width(
    capture: u128,
    twice_capacity: u128,
    headroom_impact: u128,
    headroom_span: u128,
    requested: u128,
) -> (u128, WidthBinding) {
    debug_assert!(twice_capacity >= 2 && twice_capacity % 2 == 0);
    debug_assert!(capture * 2 <= twice_capacity, "capture must be clamped to capacity");

    // W_boundary = floor((2C*(H_i + 1) - 1) / K), vacuous at K == 0.
    //
    // Lemma (B): bounding a `floor` by `H_i` is the strict `< H_i + 1`, which over the integers
    // is `W*K <= 2C*(H_i+1) - 1`. Under the previous ceiled impact this bound was a bare floor
    // division, and keeping that form here would overstate the safe width by one.
    let boundary = if capture == 0 {
        MAX_PRICE
    } else {
        // 2C*(H_i + 1) reaches 2^97 * 2^56 = 2^153, so it needs the 256-bit intermediate. A
        // quotient above u128 means the bound is far beyond MAX_PRICE.
        match mul_div_rem(twice_capacity, headroom_impact.saturating_add(1), capture) {
            // 2C*(H_i+1) >= 2C > K, so q >= 1 and the -1 borrows cleanly.
            Some((q, 0)) => q - 1,
            Some((q, _)) => q,
            None => MAX_PRICE,
        }
    };

    // W_endpoint = floor(H_e * 2C / n) with n = 2C - K >= C >= 1: always defined, no branch.
    //
    // Lemma (A): the floored impact makes `span(W) = W - floor(W*K/2C)` collapse to the *ceiled*
    // `ceil(W*n/2C)`, and a ceiling is bounded by `H_e` exactly when the real quotient is, so
    // there is no `-1` correction to apply.
    let endpoint = mul_div_floor(headroom_span, twice_capacity, twice_capacity - capture).unwrap_or(MAX_PRICE);

    // Order matters only for the reported binding; ties report the earlier cause, which is the
    // one the operator can act on.
    let mut width = MAX_PRICE;
    let mut binding = WidthBinding::Saturated;
    for (candidate, cause) in [
        (requested, WidthBinding::Requested),
        (boundary, WidthBinding::Boundary),
        (endpoint, WidthBinding::Endpoint),
    ] {
        if candidate < width {
            width = candidate;
            binding = cause;
        }
    }
    (width, binding)
}

fn check_input(input: &SolveInput) -> Result<u128, LadderError> {
    if input.capacity == 0 {
        return Err(LadderError::ZeroCapacity);
    }
    if input.capacity > MAX_AMOUNT || input.capture > MAX_AMOUNT {
        return Err(LadderError::AmountOutOfRange);
    }
    if input.target > MAX_PRICE || input.max_price > MAX_PRICE || input.min_price > MAX_PRICE {
        return Err(LadderError::PriceOutOfRange);
    }
    if input.min_price > input.max_price {
        return Err(LadderError::InfeasibleBounds);
    }
    if input.target < input.min_price {
        return Err(LadderError::TargetBelowFloor);
    }
    if input.target > input.max_price {
        return Err(LadderError::TargetAboveCeiling);
    }
    Ok(input.capture.min(input.capacity))
}

/// Solve the bid side: find the widest ladder whose average execution price over `[0, capture]`
/// is `target`, or the largest representable price below it.
///
/// Guarantees, all asserted by the property tests:
///
/// * the realised average lies in `(target - 1, target]` — never above, so never favourable to
///   the taker — and equals `target` exactly whenever `2*capacity` divides
///   `width * effective_capture`. See §3 for why exactness is not available in general.
/// * `min_price <= low <= high <= max_price`.
/// * A `W = 0` ladder (`low == high == target`) always exists, so the only failures are
///   input-domain failures.
///
/// # Errors
/// [`LadderError`] variants for out-of-domain or infeasible inputs. Never for a well-formed
/// request inside the domain.
pub fn solve_bid(input: &SolveInput) -> Result<Solution, LadderError> {
    let effective_capture = check_input(input)?;
    let twice_capacity = input.capacity * 2;

    // Bid: the impact raises `maxBid` toward the ceiling; the span lowers `minBid` toward the
    // floor.
    let (width, binding) = widest_width(
        effective_capture,
        twice_capacity,
        input.max_price - input.target,
        input.target - input.min_price,
        input.requested_width,
    );

    // FLOOR — see §3. The realised average is `target - frac(W*K/2C)`, at or below the target.
    let impact = mul_div_floor(width, effective_capture, twice_capacity).ok_or(LadderError::PriceOutOfRange)?;
    let high = input.target + impact; // <= max_price by the boundary bound
    let low = high - width; // >= min_price by the endpoint bound

    debug_assert!(high <= input.max_price);
    debug_assert!(low >= input.min_price);

    Ok(Solution { low, high, width, effective_capture, impact, binding })
}

/// Solve the ask side. Mirror of [`solve_bid`]; see §5 of the module docs. The realised average
/// lies in `[target, target + 1)` — never below, so never favourable to the taker.
///
/// # Errors
/// [`LadderError`] variants for out-of-domain or infeasible inputs.
pub fn solve_ask(input: &SolveInput) -> Result<Solution, LadderError> {
    let effective_capture = check_input(input)?;
    let twice_capacity = input.capacity * 2;

    // Ask: the impact lowers `minAsk` toward the floor; the span raises `maxAsk` toward the
    // ceiling. Exactly the bid headrooms, swapped.
    let (width, binding) = widest_width(
        effective_capture,
        twice_capacity,
        input.target - input.min_price,
        input.max_price - input.target,
        input.requested_width,
    );

    let impact = mul_div_floor(width, effective_capture, twice_capacity).ok_or(LadderError::PriceOutOfRange)?;
    let low = input.target - impact; // >= min_price by the boundary bound
    let high = low + width; // <= max_price by the endpoint bound

    debug_assert!(high <= input.max_price);
    debug_assert!(low >= input.min_price);

    Ok(Solution { low, high, width, effective_capture, impact, binding })
}

/// A two-sided row plus the per-side solutions it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TwoSided {
    /// The four prices, guaranteed to pass `PropCurve.validateLadder`.
    pub ladder: Ladder,
    /// The bid-side solution. Always honoured verbatim.
    pub bid: Solution,
    /// The ask-side solution as solved, *before* any repair.
    pub ask: Solution,
    /// Set when the two independently solved sides crossed and the ask side had to be lifted.
    /// When set, `ladder.min_ask != ask.low` and the ask target is no longer honoured — the
    /// realised ask is strictly worse for the taker, never better. The quoter should log this: a
    /// row that repairs every cycle means the two targets are being computed from inconsistent
    /// reference prices.
    pub ask_repaired: bool,
}

/// Solve both sides and assemble a row that `PropCurve.validateLadder` accepts.
///
/// The two sides are solved independently and then reconciled: `min_ask` is raised to `max_bid`
/// if the targets crossed, and `max_ask` is raised to at least `min_bid + 1` to satisfy the
/// validator's strict comparison. Raising the ask is the pool-favourable repair; the
/// alternative — narrowing the bid — would silently deliver a worse-than-requested bid while
/// reporting success.
///
/// # Errors
/// [`LadderError`] from either side, or [`LadderError::InfeasibleBounds`] when the repair would
/// push a price past [`MAX_PRICE`].
pub fn solve_two_sided(bid: &SolveInput, ask: &SolveInput) -> Result<TwoSided, LadderError> {
    let bid_sol = solve_bid(bid)?;
    let ask_sol = solve_ask(ask)?;

    let min_bid = bid_sol.low;
    let max_bid = bid_sol.high;
    let mut min_ask = ask_sol.low;
    let mut max_ask = ask_sol.high;

    if min_ask < max_bid {
        min_ask = max_bid;
    }
    if max_ask < min_ask {
        max_ask = min_ask;
    }
    if max_ask <= min_bid {
        max_ask = min_bid.checked_add(1).ok_or(LadderError::InfeasibleBounds)?;
    }
    if max_ask > MAX_PRICE || min_ask > MAX_PRICE {
        return Err(LadderError::InfeasibleBounds);
    }

    let ladder = Ladder { min_bid, max_bid, min_ask, max_ask };
    ladder.validate(bid.min_price)?;
    Ok(TwoSided {
        ladder,
        bid: bid_sol,
        ask: ask_sol,
        ask_repaired: min_ask != ask_sol.low || max_ask != ask_sol.high,
    })
}

/// Round-trip check: run the solved bid ladder back through the on-chain quote path and confirm
/// the realised average is at or below the target — never better for the taker.
///
/// Returns the realised average bid price, floored as [`avg_bid_price`] floors. The engine calls
/// this as the pre-flight assertion described in archi_v2 §5.3 ("forward, inverse, validate — the
/// same arithmetic in three places").
///
/// # Errors
/// Propagates whatever the on-chain mirror would have reverted with.
pub fn verify_bid_round_trip(sol: &Solution, capacity: u128, price_scale_exp: u8) -> Result<u128, LadderError> {
    let realised = avg_bid_price(sol.low, sol.high, capacity, 0, sol.effective_capture)?;
    // Also exercise the full quote path so a divergence in the scaling step cannot hide.
    let _ = amount_out_bid(sol.effective_capture, sol.low, sol.high, capacity, 0, price_scale_exp)?;
    Ok(realised)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::curve::{amount_in_ask, amount_out_bid, avg_ask_price};

    fn bid_input(target: u128, capture: u128, capacity: u128) -> SolveInput {
        SolveInput { target, capture, capacity, requested_width: MAX_PRICE, min_price: 0, max_price: MAX_PRICE }
    }

    /// The realised average, as the chain's own arithmetic implies it, expressed exactly:
    /// `target` iff `2C | W*K`, else strictly between `target - 1` and `target`.
    fn bid_residual_is_exact(sol: &Solution, capacity: u128) -> bool {
        mul_div_rem(sol.width, sol.effective_capture, capacity * 2).unwrap().1 == 0
    }

    #[test]
    fn round_trip_lands_on_the_pools_side_of_the_target() {
        let input = bid_input(3_000_000_000_000_000, 5_000_000, 10_000_000);
        let sol = solve_bid(&input).unwrap();
        let realised = avg_bid_price(sol.low, sol.high, input.capacity, 0, sol.effective_capture).unwrap();
        assert!(realised <= input.target && input.target - realised <= 1);
        // The quote path never pays a taker more than a flat ladder at the target would.
        let solved = amount_out_bid(sol.effective_capture, sol.low, sol.high, input.capacity, 0, 18).unwrap();
        let flat = amount_out_bid(sol.effective_capture, input.target, input.target, input.capacity, 0, 18).unwrap();
        assert!(solved <= flat, "solved {solved} beat the flat target ladder {flat}");
    }

    #[test]
    fn the_impact_floors_so_the_residual_points_at_the_pool() {
        // W*K = 7*500 = 3_500, 2C = 2_000: 3_500/2_000 = 1.75, so the impact is 1 and the
        // realised average is 1_000_000 - 0.75, i.e. 999_999 once floored.
        let mut input = bid_input(1_000_000, 500, 1_000);
        input.requested_width = 7;
        let sol = solve_bid(&input).unwrap();
        assert_eq!((sol.width, sol.impact), (7, 1));
        assert_eq!((sol.low, sol.high), (999_994, 1_000_001));
        assert!(!bid_residual_is_exact(&sol, input.capacity));
        assert_eq!(avg_bid_price(sol.low, sol.high, input.capacity, 0, sol.effective_capture), Ok(999_999));
        // Ceiling the impact would have put the realised average at 1_000_000 + something, i.e.
        // above the target — the one direction that is not allowed.
        assert_eq!(
            avg_bid_price(sol.low + 1, sol.high + 1, input.capacity, 0, sol.effective_capture),
            Ok(1_000_000)
        );
    }

    #[test]
    fn round_trip_is_exact_when_the_division_is() {
        // W*K divisible by 2C: K = C = 1_000, so 2C = 2_000 and any even W works.
        let mut input = bid_input(1_000_000, 1_000, 1_000);
        input.requested_width = 8;
        let sol = solve_bid(&input).unwrap();
        assert_eq!((sol.width, sol.impact), (8, 4));
        assert!(bid_residual_is_exact(&sol, input.capacity));
        assert_eq!(avg_bid_price(sol.low, sol.high, input.capacity, 0, sol.effective_capture), Ok(1_000_000));
    }

    #[test]
    fn capture_is_clamped_to_capacity() {
        let input = bid_input(1_000_000, u128::from(u32::MAX), 1_000);
        let sol = solve_bid(&input).unwrap();
        assert_eq!(sol.effective_capture, 1_000);
    }

    #[test]
    fn zero_capture_puts_the_target_at_the_top() {
        let input = bid_input(1_000_000, 0, 1_000);
        let sol = solve_bid(&input).unwrap();
        assert_eq!((sol.effective_capture, sol.impact, sol.high), (0, 0, 1_000_000));
    }

    #[test]
    fn floor_binds_the_endpoint_width() {
        // target 1_000, floor 900 => the bottom may drop at most 100.
        //
        // K = C = 1_000, 2C = 2_000, n = 2C - K = 1_000, H_e = 100.
        // Lemma (A): W_endpoint = floor(100 * 2_000 / 1_000) = 200.
        // impact = floor(200 * 1_000 / 2_000) = 100, so high = 1_100 and low = 900, exactly on
        // the floor; span = 200 - 100 = 100 = H_e. ✓
        let input = SolveInput {
            target: 1_000,
            capture: 1_000,
            capacity: 1_000,
            requested_width: MAX_PRICE,
            min_price: 900,
            max_price: MAX_PRICE,
        };
        let sol = solve_bid(&input).unwrap();
        assert_eq!(sol.binding, WidthBinding::Endpoint);
        assert_eq!((sol.width, sol.impact), (200, 100));
        assert_eq!((sol.low, sol.high), (900, 1_100));
        // Tightness: one more unit of width would breach the floor. Probe with the same FLOOR the
        // forward derivation uses, or the probe proves nothing about the real solver.
        let w = sol.width + 1;
        let impact = mul_div_floor(w, sol.effective_capture, 2 * input.capacity).unwrap();
        assert!(input.target + impact - w < 900);
    }

    #[test]
    fn floor_binds_the_boundary_width() {
        // K = C = 1_000, 2C = 2_000, H_i = 1_010 - 1_000 = 10.
        // Lemma (B): W_boundary = floor((2_000*11 - 1)/1_000) = floor(21_999/1_000) = 21,
        // impact = floor(21*1_000/2_000) = floor(10.5) = 10, high = 1_010 exactly on the ceiling.
        //
        // This is the case that shows using lemma (A) here — the form the *previous*, ceiled
        // revision needed — is now merely loose in the safe direction (it would give
        // floor(10*2_000/1_000) = 20, one unit narrower), whereas using lemma (A) for the
        // endpoint bound while the impact ceiled was unsound. The correction follows the floor.
        let input = SolveInput {
            target: 1_000,
            capture: 1_000,
            capacity: 1_000,
            requested_width: MAX_PRICE,
            min_price: 0,
            max_price: 1_010,
        };
        let sol = solve_bid(&input).unwrap();
        assert_eq!(sol.binding, WidthBinding::Boundary);
        assert_eq!((sol.width, sol.impact), (21, 10));
        assert_eq!((sol.low, sol.high), (989, 1_010));
        // Tightness: W + 1 breaches the ceiling.
        let w = sol.width + 1;
        let impact = mul_div_floor(w, sol.effective_capture, 2 * input.capacity).unwrap();
        assert_eq!((w, impact), (22, 11));
        assert!(input.target + impact > 1_010);
    }

    #[test]
    fn requested_width_wins_when_smallest() {
        let mut input = bid_input(1_000_000, 1_000, 1_000);
        input.requested_width = 7;
        let sol = solve_bid(&input).unwrap();
        assert_eq!((sol.width, sol.binding), (7, WidthBinding::Requested));
        // K = C = 1_000, 2C = 2_000: impact = floor(7*1_000/2_000) = 3.
        assert_eq!(sol.impact, 3);
        assert_eq!((sol.low, sol.high), (999_996, 1_000_003));
        // 7*1_000 = 7_000 is not a multiple of 2_000, so the realised average is one unit under.
        assert_eq!(avg_bid_price(sol.low, sol.high, input.capacity, 0, sol.effective_capture), Ok(999_999));
    }

    #[test]
    fn zero_width_is_always_available() {
        let input = SolveInput {
            target: 500,
            capture: 100,
            capacity: 100,
            requested_width: MAX_PRICE,
            min_price: 500,
            max_price: 500,
        };
        let sol = solve_bid(&input).unwrap();
        assert_eq!((sol.low, sol.high, sol.width), (500, 500, 0));
    }

    #[test]
    fn ask_round_trip_never_favours_the_taker() {
        let input = SolveInput {
            target: 3_100_000_000_000_000,
            capture: 4_000_000,
            capacity: 10_000_000,
            requested_width: MAX_PRICE,
            min_price: 1,
            max_price: MAX_PRICE,
        };
        let sol = solve_ask(&input).unwrap();
        // K = 4_000_000, 2C = 20_000_000, so 2C/K = 5 and H_i = T - 1.
        // Lemma (B): W = floor((20_000_000*T - 1)/4_000_000) = 5*T - 1 = 15_499_999_999_999_999.
        // impact = floor(W*K/2C) = floor(W/5) = T - 1, so minAsk = 1, exactly on the floor.
        assert_eq!(sol.binding, WidthBinding::Boundary);
        assert_eq!(sol.width, 15_499_999_999_999_999);
        assert_eq!(sol.impact, 3_099_999_999_999_999);
        assert_eq!((sol.low, sol.high), (1, 15_500_000_000_000_000));
        let realised = avg_ask_price(sol.low, sol.high, input.capacity, 0, sol.effective_capture).unwrap();
        assert!(realised >= input.target && realised - input.target <= 1);
        // In amount terms: the solved ladder charges at least what a flat ladder at T charges.
        let solved = amount_in_ask(sol.effective_capture, sol.low, sol.high, input.capacity, 0, 18).unwrap();
        let flat = amount_in_ask(sol.effective_capture, input.target, input.target, input.capacity, 0, 18).unwrap();
        assert!(solved >= flat, "solved {solved} undercharged against the flat target ladder {flat}");
    }

    #[test]
    fn two_sided_row_passes_the_on_chain_validator() {
        let bid = bid_input(1_000_000, 500, 1_000);
        let ask = bid_input(1_000_100, 500, 1_000);
        let out = solve_two_sided(&bid, &ask).unwrap();
        let ladder = out.ladder;
        ladder.validate(0).unwrap();
        assert!(ladder.min_bid <= ladder.max_bid);
        assert!(ladder.max_bid <= ladder.min_ask);
        assert!(ladder.min_ask <= ladder.max_ask);
        assert!(ladder.max_ask > ladder.min_bid);

        // K = 500, C = 1_000, 2C = 2_000, n = 2C - K = 1_500.
        //
        // Bid (T = 1_000_000, floor 0): lemma (A) endpoint bound is
        //   W = floor(1_000_000 * 2_000 / 1_500) = 1_333_333,
        //   impact = floor(1_333_333*500/2_000) = 333_333, high = 1_333_333, low = 0.
        assert_eq!((out.bid.width, out.bid.impact), (1_333_333, 333_333));
        assert_eq!((out.bid.low, out.bid.high), (0, 1_333_333));
        assert_eq!(out.bid.binding, WidthBinding::Endpoint);

        // Ask (T = 1_000_100, floor 0): lemma (B) boundary bound is
        //   W = floor((2_000*1_000_101 - 1)/500) = floor(2_000_201_999/500) = 4_000_403,
        //   impact = floor(4_000_403*500/2_000) = 1_000_100, so minAsk = 0.
        assert_eq!((out.ask.width, out.ask.impact), (4_000_403, 1_000_100));
        assert_eq!((out.ask.low, out.ask.high), (0, 4_000_403));
        assert_eq!(out.ask.binding, WidthBinding::Boundary);

        // The two sides overlap, so the ask is lifted.
        assert!(out.ask_repaired);
        assert_eq!(ladder, Ladder { min_bid: 0, max_bid: 1_333_333, min_ask: 1_333_333, max_ask: 4_000_403 });
    }

    #[test]
    fn two_sided_repairs_a_crossed_pair_of_targets() {
        // Ask target below the bid target: the repair must lift the ask, not drop the bid.
        //
        // K = C = 100, 2C = 200, n = 100.
        // Bid  (T = 1_000): lemma (A) => W = floor(1_000*200/100) = 2_000,
        //                     impact = floor(2_000*100/200) = 1_000, high = 2_000, low = 0.
        // Ask  (T =   900): lemma (B) => W = floor((200*901 - 1)/100) = 1_801,
        //                     impact = floor(1_801*100/200) = 900, low = 0, high = 1_801.
        let bid = bid_input(1_000, 100, 100);
        let ask = bid_input(900, 100, 100);
        let out = solve_two_sided(&bid, &ask).unwrap();
        assert!(out.ask_repaired);
        assert_eq!((out.bid.width, out.bid.impact, out.bid.low, out.bid.high), (2_000, 1_000, 0, 2_000));
        assert_eq!((out.ask.width, out.ask.impact, out.ask.low, out.ask.high), (1_801, 900, 0, 1_801));
        assert_eq!(out.ladder, Ladder { min_bid: 0, max_bid: 2_000, min_ask: 2_000, max_ask: 2_000 });
        // The repair is pool-favourable: the realised ask is strictly worse for the taker than
        // the 900 that was requested, never better.
        let realised = avg_ask_price(out.ladder.min_ask, out.ladder.max_ask, 100, 0, out.ask.effective_capture).unwrap();
        assert_eq!(realised, 2_000);
        assert!(realised >= ask.target);
        out.ladder.validate(0).unwrap();
    }

    #[test]
    fn infeasible_inputs_are_rejected_not_clamped() {
        assert_eq!(solve_bid(&bid_input(1, 1, 0)), Err(LadderError::ZeroCapacity));
        let mut i = bid_input(100, 1, 10);
        i.min_price = 200;
        assert_eq!(solve_bid(&i), Err(LadderError::TargetBelowFloor));
        let mut i = bid_input(100, 1, 10);
        i.max_price = 50;
        assert_eq!(solve_bid(&i), Err(LadderError::TargetAboveCeiling));
        let mut i = bid_input(MAX_PRICE + 1, 1, 10);
        i.max_price = MAX_PRICE;
        assert_eq!(solve_bid(&i), Err(LadderError::PriceOutOfRange));
    }
}
