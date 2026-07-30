//! The inverse ladder solver: target executable price + capture size -> ladder.
//!
//! The one piece of the engine with no Solidity counterpart; archi_v2 §5.2 sketches it. Every
//! rounding here is chosen so a taker can never do better than the target.
//!
//! # 1. Inversion
//!
//! `PropCurve` (see [`crate::curve`]) implies an **exact rational** average price
//! `maxBid - W*K/(2C)`, with `W = maxBid - minBid` the width, `C` the capacity and `K` the capture
//! size clamped to `C`; there is no integer `avgBid` to hit. The solver quotes from a reset epoch
//! (`u = 0`), so the doubled midpoint numerator `2u + q` is just `K`, and setting the average equal
//! to the target `T` is linear in `maxBid`. It inverts in closed form, with no search:
//!
//! ```text
//! impact = floor(W*K / (2C))          maxBid = T + impact          minBid = maxBid - W
//! ```
//!
//! **FLOOR, not ceil.** The realised average is `T - frac(W*K/2C)`, in `(T - 1, T]` and exact when
//! `2C | W*K`. A lower bid is worse for the taker, so that residual is pool-favourable; ceiling
//! lands in `[T, T + 1)`, taker-favourable, the one direction not allowed. Exactness for arbitrary
//! `W` would need snapping `W` to a multiple of `2C / gcd(2C, K)`, which for a small capture
//! against a large capacity collapses the width to zero and posts no depth.
//!
//! The other two residuals are structural and also pool-favourable: [`solve_two_sided`]'s
//! crossed-book repair only ever raises the ask, and a trade that is not the one solved for is
//! covered by the forward map's monotonicity.
//!
//! # 2. Widest safe width
//!
//! `W` is the *concentration* knob, and wider is better for the pool: the same top-of-book price
//! with more depth behind it before the price walks. Two elementary lemmas over integers `a >= 1`,
//! `d >= 1`, `N >= 0` give both bounds, each tight (`W + 1` breaches):
//!
//! ```text
//! (A)  ceil(W*a/d)  <= N   <=>  W*a <= N*d          <=>  W <= floor(N*d / a)
//! (B)  floor(W*a/d) <= N   <=>  W*a <  d*(N+1)      <=>  W <= floor((d*(N+1) - 1) / a)
//!
//! boundary, near endpoint under a ceiling P_hi, H_i = P_hi - T, impact <= H_i, lemma (B):
//!     W_boundary = floor((2C*(H_i + 1) - 1) / K)               (vacuous at K == 0)
//!
//! endpoint, far endpoint above a floor P_lo, H_e = T - P_lo, span <= H_e, lemma (A):
//!     span(W)    = W - floor(W*K/(2C)) = ceil(W * (2C - K) / (2C))
//!     W_endpoint = floor(H_e * 2C / n),   n = 2C - K
//! ```
//!
//! `P_hi` is archi_v2 §5.4's reference-oracle clamp or `PRICE_MAX`; `P_lo` is `minPrice` or the
//! cost-basis floor. The span collapses to a ceiling by `-floor(x) = ceil(-x)`, is non-decreasing
//! in `W`, and `n >= C >= 1` follows from `K <= C`, so the division needs no special case.
//!
//! **Which bound carries the `-1`** follows the rounding: the correction sits on whichever
//! expression is a `floor`.
//!
//! ```text
//!                       impact            span              boundary   endpoint
//! floored impact   floor(W*K/2C)   ceil(W*n/2C)        lemma (B)  lemma (A)
//! ceiled  impact    ceil(W*K/2C)  floor(W*n/2C)        lemma (A)  lemma (B)
//! ```
//!
//! §1 floors, so this module is the top row. Swapping the rows is unsound in one direction: a
//! bound one unit too large lets the near endpoint breach the price ceiling it was meant to
//! respect. Finally `W = min(W_requested, W_boundary, W_endpoint, PRICE_MAX)`.
//!
//! # 3. Ask side
//!
//! Mirrored: `minAsk = T - impact` and `maxAsk = minAsk + W` with the *same* floored impact,
//! putting the realised ask in `[T, T + 1)`. The floor is now the near endpoint and the ceiling the
//! far one, so the headrooms swap roles — `H_i = T - P_lo` bounds the impact and `H_e = P_hi - T`
//! bounds the span. [`widest_width`] serves both.

use crate::curve::{amount_out_bid, avg_bid_price, Ladder, AMOUNT_MAX, PRICE_MAX};
use crate::error::LadderError;
use crate::math::{mul_div_floor, mul_div_rem};

/// Which constraint pinned the ladder width. Logged by the quoter: a permanently `Boundary`-bound
/// solver is being squeezed by the oracle clamp, a different problem from a `Requested`-bound one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WidthBinding {
    /// The strategy's requested width was the smallest.
    Requested,
    /// The price ceiling on the near endpoint bound it (`W_boundary`).
    Boundary,
    /// The price floor on the far endpoint bound it (`W_endpoint`).
    Endpoint,
    /// Nothing bound it; the width saturated at [`PRICE_MAX`].
    Saturated,
}

/// Inputs to one side of the inverse solver. `capture` and `capacity` are in **base** units on
/// both sides, matching `PropCurve`, whose ask capacity is base-denominated too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SolveInput {
    /// The average execution price the taker must get over `[0, capture]`.
    pub target: u128,
    /// The trade size, in base units, over which `target` must hold on average.
    pub capture: u128,
    /// Total base capacity posted for this epoch.
    pub capacity: u128,
    /// Width the strategy would like, in price units. Use `PRICE_MAX` for "as wide as safe".
    pub requested_width: u128,
    /// Hard floor: no price in the produced ladder may fall below this.
    pub min_price: u128,
    /// Hard ceiling: no price in the produced ladder may rise above this. Cap it at [`PRICE_MAX`]
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
    /// `min(capture, capacity)`, the interval the target is guaranteed over. In doubled units it
    /// is also the midpoint numerator `2u + q` at `u == 0`.
    pub effective_capture: u128,
    /// `floor(width * effective_capture / (2 * capacity))`, the price impact baked into the near
    /// endpoint. Floored so the realised average lands on the pool's side of the target; see §1.
    pub impact: u128,
    /// Which constraint pinned [`Solution::width`].
    pub binding: WidthBinding,
}

/// Largest `W` satisfying both integer bounds of §2: `headroom_impact` bounds the impact (lemma
/// (B), which carries the `-1`) and `headroom_span` bounds the span (lemma (A), a bare floor).
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
    debug_assert!(twice_capacity >= 2);
    debug_assert!(twice_capacity % 2 == 0);
    debug_assert!(
        capture * 2 <= twice_capacity,
        "capture must be clamped to capacity"
    );

    // W_boundary = floor((2C*(H_i + 1) - 1) / K), vacuous at K == 0. Dropping the `-1` overstates
    // the safe width by one and breaches the ceiling.
    let boundary = if capture == 0 {
        PRICE_MAX
    } else {
        // 2C*(H_i + 1) reaches 2^153, so this needs the 256-bit intermediate; a quotient above
        // u128 means the bound is far beyond PRICE_MAX.
        match mul_div_rem(twice_capacity, headroom_impact.saturating_add(1), capture) {
            // 2C*(H_i+1) >= 2C > K, so q >= 1 and the -1 borrows cleanly.
            Some((q, 0)) => q - 1,
            Some((q, _)) => q,
            None => PRICE_MAX,
        }
    };

    // W_endpoint = floor(H_e * 2C / n) with n = 2C - K >= C >= 1: always defined, no branch, and
    // no `-1` since lemma (A) bounds a ceiling exactly when it bounds the real quotient.
    let endpoint =
        mul_div_floor(headroom_span, twice_capacity, twice_capacity - capture).unwrap_or(PRICE_MAX);

    // Order matters only for the reported binding; ties report the earlier cause, which is the one
    // the operator can act on.
    let mut width = PRICE_MAX;
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

/// Domain gate, split one field per check so a rejection points at the field that broke. Returns
/// the effective capture, `min(capture, capacity)`.
fn check_input(input: &SolveInput) -> Result<u128, LadderError> {
    if input.capacity == 0 {
        return Err(LadderError::ZeroCapacity);
    }
    if input.capacity > AMOUNT_MAX {
        return Err(LadderError::AmountOutOfRange);
    }
    if input.capture > AMOUNT_MAX {
        return Err(LadderError::AmountOutOfRange);
    }
    if input.target > PRICE_MAX {
        return Err(LadderError::PriceOutOfRange);
    }
    if input.max_price > PRICE_MAX {
        return Err(LadderError::PriceOutOfRange);
    }
    if input.min_price > PRICE_MAX {
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
/// Guarantees, all pinned by the property tests:
///
/// * the realised average lies in `(target - 1, target]`, never above and so never favourable to
///   the taker, and equals `target` exactly when `2*capacity` divides `width * effective_capture`.
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

    // Bid: the impact raises `maxBid` toward the ceiling, the span lowers `minBid` to the floor.
    let (width, binding) = widest_width(
        effective_capture,
        twice_capacity,
        input.max_price - input.target,
        input.target - input.min_price,
        input.requested_width,
    );

    // FLOOR, see §1: the realised average is `target - frac(W*K/2C)`, at or below the target.
    let impact = mul_div_floor(width, effective_capture, twice_capacity)
        .ok_or(LadderError::PriceOutOfRange)?;
    let high = input.target + impact; // <= max_price by the boundary bound
    let low = high - width; // >= min_price by the endpoint bound

    debug_assert!(high <= input.max_price);
    debug_assert!(low >= input.min_price);

    Ok(Solution {
        low,
        high,
        width,
        effective_capture,
        impact,
        binding,
    })
}

/// Solve the ask side. Mirror of [`solve_bid`]; see §3 of the module docs. The realised average
/// lies in `[target, target + 1)`, never below and so never favourable to the taker.
///
/// # Errors
/// [`LadderError`] variants for out-of-domain or infeasible inputs.
pub fn solve_ask(input: &SolveInput) -> Result<Solution, LadderError> {
    let effective_capture = check_input(input)?;
    let twice_capacity = input.capacity * 2;

    // Ask: the impact lowers `minAsk` to the floor, the span raises `maxAsk` toward the ceiling —
    // the bid headrooms, swapped.
    let (width, binding) = widest_width(
        effective_capture,
        twice_capacity,
        input.target - input.min_price,
        input.max_price - input.target,
        input.requested_width,
    );

    let impact = mul_div_floor(width, effective_capture, twice_capacity)
        .ok_or(LadderError::PriceOutOfRange)?;
    let low = input.target - impact; // >= min_price by the boundary bound
    let high = low + width; // <= max_price by the endpoint bound

    debug_assert!(high <= input.max_price);
    debug_assert!(low >= input.min_price);

    Ok(Solution {
        low,
        high,
        width,
        effective_capture,
        impact,
        binding,
    })
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
    /// Set when the two independently solved sides crossed and the ask had to be lifted, so
    /// `ladder.min_ask != ask.low` and the ask target is no longer honoured — the realised ask is
    /// worse for the taker, never better. A row that repairs every cycle means the two targets come
    /// from inconsistent reference prices, so this is worth logging.
    pub ask_repaired: bool,
}

/// Solve both sides and assemble a row that `PropCurve.validateLadder` accepts.
///
/// The sides are solved independently and then reconciled: `min_ask` rises to `max_bid` if the
/// targets crossed, and `max_ask` rises to at least `min_bid + 1` for the validator's strict
/// comparison. Raising the ask is the pool-favourable repair; narrowing the bid instead would
/// silently deliver a worse-than-requested bid while reporting success.
///
/// # Errors
/// [`LadderError`] from either side, or [`LadderError::InfeasibleBounds`] when the repair would
/// push a price past [`PRICE_MAX`].
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
        max_ask = min_bid
            .checked_add(1)
            .ok_or(LadderError::InfeasibleBounds)?;
    }
    if min_ask > PRICE_MAX {
        return Err(LadderError::InfeasibleBounds);
    }
    if max_ask > PRICE_MAX {
        return Err(LadderError::InfeasibleBounds);
    }

    let ladder = Ladder {
        min_bid,
        max_bid,
        min_ask,
        max_ask,
    };
    ladder.validate(bid.min_price)?;
    Ok(TwoSided {
        ladder,
        bid: bid_sol,
        ask: ask_sol,
        ask_repaired: min_ask != ask_sol.low || max_ask != ask_sol.high,
    })
}

/// Round-trip check: run the solved bid ladder back through the on-chain quote path and return the
/// realised average bid price, floored as [`avg_bid_price`] floors. The engine calls this as the
/// pre-flight assertion of archi_v2 §5.3.
///
/// # Errors
/// Propagates whatever the on-chain mirror would have reverted with.
pub fn verify_bid_round_trip(
    sol: &Solution,
    capacity: u128,
    price_scale_exp: u8,
) -> Result<u128, LadderError> {
    let realised = avg_bid_price(sol.low, sol.high, capacity, 0, sol.effective_capture)?;
    // Also exercise the full quote path so a divergence in the scaling step cannot hide.
    let _ = amount_out_bid(
        sol.effective_capture,
        sol.low,
        sol.high,
        capacity,
        0,
        price_scale_exp,
    )?;
    Ok(realised)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::curve::{amount_in_ask, amount_out_bid, avg_ask_price};

    fn bid_input(target: u128, capture: u128, capacity: u128) -> SolveInput {
        SolveInput {
            target,
            capture,
            capacity,
            requested_width: PRICE_MAX,
            min_price: 0,
            max_price: PRICE_MAX,
        }
    }

    /// True iff `2C | W*K`, the one case where the realised average equals the target exactly.
    fn bid_residual_is_exact(sol: &Solution, capacity: u128) -> bool {
        mul_div_rem(sol.width, sol.effective_capture, capacity * 2)
            .unwrap()
            .1
            == 0
    }

    #[test]
    fn round_trip_lands_on_the_pools_side_of_the_target() {
        let input = bid_input(3_000_000_000_000_000, 5_000_000, 10_000_000);
        let sol = solve_bid(&input).unwrap();
        let realised =
            avg_bid_price(sol.low, sol.high, input.capacity, 0, sol.effective_capture).unwrap();
        assert!(realised <= input.target && input.target - realised <= 1);
        // Never more quote than a flat ladder at the target would pay.
        let solved = amount_out_bid(
            sol.effective_capture,
            sol.low,
            sol.high,
            input.capacity,
            0,
            18,
        )
        .unwrap();
        let flat = amount_out_bid(
            sol.effective_capture,
            input.target,
            input.target,
            input.capacity,
            0,
            18,
        )
        .unwrap();
        assert!(
            solved <= flat,
            "solved {solved} beat the flat target ladder {flat}"
        );
    }

    #[test]
    fn the_impact_floors_so_the_residual_points_at_the_pool() {
        // W*K = 7*500 = 3_500, 2C = 2_000: 3_500/2_000 = 1.75, impact 1, realised average
        // 1_000_000 - 0.75, i.e. 999_999 once floored.
        let mut input = bid_input(1_000_000, 500, 1_000);
        input.requested_width = 7;
        let sol = solve_bid(&input).unwrap();
        assert_eq!((sol.width, sol.impact), (7, 1));
        assert_eq!((sol.low, sol.high), (999_994, 1_000_001));
        assert!(!bid_residual_is_exact(&sol, input.capacity));
        assert_eq!(
            avg_bid_price(sol.low, sol.high, input.capacity, 0, sol.effective_capture),
            Ok(999_999)
        );
        // Ceiling the impact would put the realised average above the target instead.
        assert_eq!(
            avg_bid_price(
                sol.low + 1,
                sol.high + 1,
                input.capacity,
                0,
                sol.effective_capture
            ),
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
        assert_eq!(
            avg_bid_price(sol.low, sol.high, input.capacity, 0, sol.effective_capture),
            Ok(1_000_000)
        );
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
        assert_eq!(
            (sol.effective_capture, sol.impact, sol.high),
            (0, 0, 1_000_000)
        );
    }

    #[test]
    fn floor_binds_the_endpoint_width() {
        // K = C = 1_000, 2C = 2_000, n = 1_000, H_e = 100. Lemma (A): W = floor(100*2_000/1_000)
        // = 200, impact = floor(200*1_000/2_000) = 100, so low = 900 sits exactly on the floor.
        let input = SolveInput {
            target: 1_000,
            capture: 1_000,
            capacity: 1_000,
            requested_width: PRICE_MAX,
            min_price: 900,
            max_price: PRICE_MAX,
        };
        let sol = solve_bid(&input).unwrap();
        assert_eq!(sol.binding, WidthBinding::Endpoint);
        assert_eq!((sol.width, sol.impact), (200, 100));
        assert_eq!((sol.low, sol.high), (900, 1_100));
        // Tightness, probed with the same FLOOR the forward derivation uses: one more unit of
        // width breaches the floor.
        let w = sol.width + 1;
        let impact = mul_div_floor(w, sol.effective_capture, 2 * input.capacity).unwrap();
        assert!(input.target + impact - w < 900);
    }

    #[test]
    fn floor_binds_the_boundary_width() {
        // K = C = 1_000, 2C = 2_000, H_i = 10. Lemma (B): W = floor((2_000*11 - 1)/1_000) = 21,
        // impact = floor(10.5) = 10, high = 1_010 exactly on the ceiling. Lemma (A) would give 20,
        // loose in the safe direction; applying (A) to the endpoint bound would be unsound.
        let input = SolveInput {
            target: 1_000,
            capture: 1_000,
            capacity: 1_000,
            requested_width: PRICE_MAX,
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
        // K = C = 1_000, 2C = 2_000: impact = floor(7*1_000/2_000) = 3, and 7_000 is not a
        // multiple of 2_000, so the realised average lands one unit under.
        assert_eq!(sol.impact, 3);
        assert_eq!((sol.low, sol.high), (999_996, 1_000_003));
        assert_eq!(
            avg_bid_price(sol.low, sol.high, input.capacity, 0, sol.effective_capture),
            Ok(999_999)
        );
    }

    #[test]
    fn zero_width_is_always_available() {
        let input = SolveInput {
            target: 500,
            capture: 100,
            capacity: 100,
            requested_width: PRICE_MAX,
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
            requested_width: PRICE_MAX,
            min_price: 1,
            max_price: PRICE_MAX,
        };
        let sol = solve_ask(&input).unwrap();
        // K = 4_000_000, 2C = 20_000_000 so 2C/K = 5, H_i = T - 1. Lemma (B): W = 5*T - 1 and
        // impact = floor(W/5) = T - 1, so minAsk = 1, exactly on the floor.
        assert_eq!(sol.binding, WidthBinding::Boundary);
        assert_eq!(sol.width, 15_499_999_999_999_999);
        assert_eq!(sol.impact, 3_099_999_999_999_999);
        assert_eq!((sol.low, sol.high), (1, 15_500_000_000_000_000));
        let realised =
            avg_ask_price(sol.low, sol.high, input.capacity, 0, sol.effective_capture).unwrap();
        assert!(realised >= input.target && realised - input.target <= 1);
        // The solved ladder charges at least what a flat ladder at T charges.
        let solved = amount_in_ask(
            sol.effective_capture,
            sol.low,
            sol.high,
            input.capacity,
            0,
            18,
        )
        .unwrap();
        let flat = amount_in_ask(
            sol.effective_capture,
            input.target,
            input.target,
            input.capacity,
            0,
            18,
        )
        .unwrap();
        assert!(
            solved >= flat,
            "solved {solved} undercharged against the flat target ladder {flat}"
        );
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

        // K = 500, C = 1_000, 2C = 2_000, n = 1_500.
        // Bid (T = 1_000_000, floor 0), lemma (A): W = floor(1_000_000*2_000/1_500) = 1_333_333,
        // impact = 333_333, so low = 0.
        assert_eq!((out.bid.width, out.bid.impact), (1_333_333, 333_333));
        assert_eq!((out.bid.low, out.bid.high), (0, 1_333_333));
        assert_eq!(out.bid.binding, WidthBinding::Endpoint);

        // Ask (T = 1_000_100, floor 0), lemma (B): W = floor((2_000*1_000_101 - 1)/500) =
        // 4_000_403, impact = 1_000_100, so minAsk = 0.
        assert_eq!((out.ask.width, out.ask.impact), (4_000_403, 1_000_100));
        assert_eq!((out.ask.low, out.ask.high), (0, 4_000_403));
        assert_eq!(out.ask.binding, WidthBinding::Boundary);

        // The two sides overlap, so the ask is lifted.
        assert!(out.ask_repaired);
        assert_eq!(
            ladder,
            Ladder {
                min_bid: 0,
                max_bid: 1_333_333,
                min_ask: 1_333_333,
                max_ask: 4_000_403
            }
        );
    }

    #[test]
    fn two_sided_repairs_a_crossed_pair_of_targets() {
        // Ask target below the bid target: the repair must lift the ask, not drop the bid.
        // K = C = 100, 2C = 200, n = 100.
        // Bid (T = 1_000), lemma (A): W = 2_000, impact = 1_000, so 0..2_000.
        // Ask (T =   900), lemma (B): W = 1_801, impact =   900, so 0..1_801.
        let bid = bid_input(1_000, 100, 100);
        let ask = bid_input(900, 100, 100);
        let out = solve_two_sided(&bid, &ask).unwrap();
        assert!(out.ask_repaired);
        assert_eq!(
            (out.bid.width, out.bid.impact, out.bid.low, out.bid.high),
            (2_000, 1_000, 0, 2_000)
        );
        assert_eq!(
            (out.ask.width, out.ask.impact, out.ask.low, out.ask.high),
            (1_801, 900, 0, 1_801)
        );
        assert_eq!(
            out.ladder,
            Ladder {
                min_bid: 0,
                max_bid: 2_000,
                min_ask: 2_000,
                max_ask: 2_000
            }
        );
        // The repair is pool-favourable: the realised ask is worse for the taker than the 900 that
        // was requested, never better.
        let realised = avg_ask_price(
            out.ladder.min_ask,
            out.ladder.max_ask,
            100,
            0,
            out.ask.effective_capture,
        )
        .unwrap();
        assert_eq!(realised, 2_000);
        assert!(realised >= ask.target);
        out.ladder.validate(0).unwrap();
    }

    #[test]
    fn infeasible_inputs_are_rejected_not_clamped() {
        assert_eq!(
            solve_bid(&bid_input(1, 1, 0)),
            Err(LadderError::ZeroCapacity)
        );
        let mut i = bid_input(100, 1, 10);
        i.min_price = 200;
        assert_eq!(solve_bid(&i), Err(LadderError::TargetBelowFloor));
        let mut i = bid_input(100, 1, 10);
        i.max_price = 50;
        assert_eq!(solve_bid(&i), Err(LadderError::TargetAboveCeiling));
        let mut i = bid_input(PRICE_MAX + 1, 1, 10);
        i.max_price = PRICE_MAX;
        assert_eq!(solve_bid(&i), Err(LadderError::PriceOutOfRange));
    }
}
