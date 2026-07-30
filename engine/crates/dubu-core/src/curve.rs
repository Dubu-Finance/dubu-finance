//! Exact integer port of `contracts/src/libraries/PropCurve.sol`.
//!
//! `PropCurve.sol` is authoritative. Every rounding direction, every early return, every revert
//! condition, and the *order* in which they are checked, is load bearing; generated vectors pin
//! this against the Solidity, and the three deliberate divergences are documented at
//! [`PRICE_SCALE_EXP_MAX`], [`NO_ASK`] and [`CurveError::ArithmeticPanic`].
//!
//! # The curve
//!
//! A linear-impact single tick: the bid decays from `maxBid` at zero usage to `minBid` at full
//! capacity, the ask rises from `minAsk` to `maxAsk`, and usage and capacity are in **base** units
//! on both sides. A trade over `[u, u + q]` is charged the interval's average, which for a linear
//! ladder is its midpoint value; doubling clears the half-integer midpoint. With `W` the span, `C`
//! the capacity and `S = 10**priceScaleExp`:
//!
//! ```text
//! bid:  amountOut = floor( q * (2*maxBid*C - W*(2u + q)) / (2*C*S) )
//! ask:  amountIn  =  ceil( q * (2*minAsk*C + W*(2u + q)) / (2*C*S) )
//! ```
//!
//! Three properties of that shape hold up the rest of the engine and must not be undone:
//!
//! 1. **Base-denominated capacity on both sides.** A quote-denominated ask output would be
//!    `amountIn * scale / avgAsk`, the *reciprocal* of the midpoint price; `1/p` is convex, so by
//!    Jensen the midpoint rule under-delivers and splitting an ask strictly dominates.
//! 2. **Additive**, because the numerators are the exact integral over `[u, u+q]` and nothing
//!    rounds until the single final division: any `n`-way split loses at most `n - 1` units, to
//!    the pool. [`amount_out_ask`] inverts an additive function, so it is super-additive.
//! 3. **Monotone in `q` with no side condition**, which is what makes `PropPool.getAmountIn`'s
//!    bisection exact. The bid numerator `A*q - W*q^2`, `A = 2*maxBid*C - 2*W*u`, peaks at
//!    `q* = A/(2W)` and `q* >= C - u <=> minBid >= 0`, so the vertex lies at or beyond the largest
//!    `q` [`validate_ladder`] admits; the ask numerator is an upward parabola through the origin.
//!
//! [`executable_top_bid`] and [`executable_top_ask`] CEIL the drift, away from the taker on both
//! sides: the quote path rounds no price at all, so the reference is the exact rational zero-size
//! limit, and flooring would report a bid above it and an ask below it.
//!
//! # Domain
//!
//! `IPropPool.PairSnapshot` fixes the widths: `uint56` prices ([`PRICE_MAX`]), `uint96` capacity
//! and usage ([`AMOUNT_MAX`]), a `uint8` `priceScaleExp` bounded by [`PRICE_SCALE_EXP_MAX`]. So
//! `u128` holds every stored value, but the numerators need [`crate::math::U256`]: `2*maxBid*C`
//! and `W*(2u + q)` reach `2^153` and, times `q < 2^96`, `2^250`, against a denominator
//! `2*C*S < 2^224`. The quotients are bounded by `q * maxBid / S < 2^152`, which is exactly
//! [`AMOUNT_OUT_MAX`], so a `None` from the divider is the *shared*
//! [`CurveError::AmountOutOfDomain`] rather than a port-only refusal. The tightest intermediate is
//! the bracket seeds' `2^128 * 10^38 < 2^255`, bounded *only* by that same cap.
//!
//! Input bounds are `debug_assert!`ed, not asserted: a panic on the quoting hot path takes the
//! updater down and the pool then stops quoting at all. The bracket and ordering invariants
//! *inside* the solvers are hard `assert!`s, being properties of this code rather than its inputs.

use crate::error::CurveError;
use crate::math::{mul_div_ceil, mul_div_floor, pow10, U256};

/// Largest representable price. Prices are `uint56` in `IPropPool.PairSnapshot` and in the
/// packed `updateQuote` word.
pub const PRICE_MAX: u128 = (1u128 << 56) - 1;

/// Largest representable amount / capacity. `uint96` in `IPropPool.PairSnapshot`.
pub const AMOUNT_MAX: u128 = (1u128 << 96) - 1;

/// Mirror of `PropCurve.AMOUNT_OUT_MAX` (`type(uint128).max`), the ceiling on every
/// quote-denominated amount this library returns or accepts. It makes the on-chain and off-chain
/// domains coincide, so a trade the engine calls unquotable is one the chain also refuses; without
/// it the pool fills sizes the engine declined and the loss is silent.
pub const AMOUNT_OUT_MAX: u128 = u128::MAX;

/// Mirror of `PropCurve.PRICE_SCALE_EXP_MAX`.
///
/// Deliberate divergence: `PropCurve`'s quote paths do not enforce this and succeed on chain up to
/// `exp == 77`; `PropPool.addPair` enforces it at configuration time instead. Refusing `exp > 38`
/// here turns a misconfiguration into a dropped row rather than a divergent quote.
pub const PRICE_SCALE_EXP_MAX: u8 = 38;

/// What [`executable_top_ask`] returns when `askCapacity == 0`.
///
/// Unavoidable divergence: on chain the function returns `type(uint256).max`, which `u128` cannot
/// hold. Any comparison against this value must treat it as an infinity sentinel, never as a
/// price. The generated vectors emit the true `uint256` decimal so the Solidity differential test
/// still asserts the exact on-chain value.
pub const NO_ASK: u128 = u128::MAX;

// The domain argument above, in compile-checkable form.
const _: () = assert!(PRICE_MAX == (1u128 << 56) - 1);
const _: () = assert!(AMOUNT_MAX == (1u128 << 96) - 1);
const _: () = assert!(PRICE_MAX < AMOUNT_MAX);
// `amount_in_bid` and `amount_out_ask` carry no explicit `amount > AMOUNT_OUT_MAX` check because
// the argument's own type enforces it. Should these stop being equal, both need the check back.
const _: () = assert!(AMOUNT_OUT_MAX == u128::MAX);
// A sentinel outside the price domain, so no comparison can mistake it for a price.
const _: () = assert!(NO_ASK > PRICE_MAX);
// `2*C*S < 2^97 * 10^38 < 2^224` needs the widest scale under 2^127.
const _: () = assert!(10u128.pow(PRICE_SCALE_EXP_MAX as u32) < 1u128 << 127);
// The numerators fit in 256 bits: 1 + 56 + 96 + 1 for the sum, times `q < 2^96`, is 250 bits.
const _: () = assert!(1 + 56 + 96 + 1 + 96 < 256);

/// The four ladder prices for one side-pair, in the order `PropCurve.validateLadder` wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Ladder {
    /// Worst bid, reached when bid capacity is fully consumed.
    pub min_bid: u128,
    /// Best bid, offered at zero usage.
    pub max_bid: u128,
    /// Best ask, offered at zero usage.
    pub min_ask: u128,
    /// Worst ask, reached when ask capacity is fully consumed.
    pub max_ask: u128,
}

impl Ladder {
    /// Run the on-chain validator against this row.
    ///
    /// # Errors
    /// See [`validate_ladder`].
    pub fn validate(&self, min_price: u128) -> Result<(), CurveError> {
        let verdict = validate_ladder(
            self.min_bid,
            self.max_bid,
            self.min_ask,
            self.max_ask,
            min_price,
        );
        if verdict.is_ok() {
            debug_assert!(
                self.max_ask > self.min_bid,
                "an accepted book is never flat"
            );
        }
        verdict
    }
}

#[inline]
fn in_price_domain(p: u128) -> bool {
    p <= PRICE_MAX
}

#[inline]
fn in_amount_domain(a: u128) -> bool {
    a <= AMOUNT_MAX
}

#[inline]
fn check_exp(price_scale_exp: u8) -> Result<u128, CurveError> {
    // Returned, not asserted: a panic on the quoting hot path takes the updater down and the pool
    // then goes stale. A bad input must not escalate into a liveness failure.
    if price_scale_exp > PRICE_SCALE_EXP_MAX {
        return Err(CurveError::DomainOverflow);
    }
    pow10(price_scale_exp).ok_or(CurveError::DomainOverflow)
}

// --- The two numerators, as 256-bit quotients ---
//
// Mirrors of `PropCurve._bidGross` / `PropCurve._askCost` and, like them, uncapped. The bisections
// compare a gross against a target that is itself bounded by `AMOUNT_OUT_MAX`, so narrowing to
// `u128` here would refuse a request the epoch can in fact fill.

/// `floor( q * (2*maxBid*C - W*(2u + q)) / (2*C*S) )`, uncapped.
///
/// Preconditions, all established by the callers: `C > 0`, `u + q <= C`, `maxBid >= minBid`.
/// Together they make the subtraction underflow-free (`W <= maxBid` and `2u + q <= 2C`).
fn bid_gross(
    q: u128,
    min_bid: u128,
    max_bid: u128,
    c: u128,
    u: u128,
    scale: u128,
) -> Result<U256, CurveError> {
    debug_assert!(c > 0, "callers check zero capacity before reaching here");
    debug_assert!(u <= c);
    debug_assert!(
        q <= c - u,
        "u + q <= C keeps the subtraction underflow-free"
    );
    debug_assert!(scale > 0);
    // `max_bid >= min_bid` is deliberately NOT asserted: an inverted ladder is the documented
    // `ArithmeticPanic` divergence and has to surface as that error, not as a panic.

    let span = max_bid
        .checked_sub(min_bid)
        .ok_or(CurveError::ArithmeticPanic)?;
    debug_assert!(
        span <= max_bid,
        "W <= maxBid is the other half of that proof"
    );

    let gross = U256::mul_u128(max_bid.checked_mul(2).ok_or(CurveError::DomainOverflow)?, c);
    let impact = U256::mul_u128(span, twice_plus(u, q)?);
    let factor = gross
        .checked_sub(impact)
        .ok_or(CurveError::ArithmeticPanic)?;
    let num = factor
        .checked_mul_u128(q)
        .ok_or(CurveError::ArithmeticPanic)?;
    let den = denominator(c, scale)?;
    let quo = num.div_rem(den).ok_or(CurveError::ArithmeticPanic)?.0;
    debug_assert!(q != 0 || quo.is_zero(), "a zero size grosses nothing");
    Ok(quo)
}

/// `ceil( q * (2*minAsk*C + W*(2u + q)) / (2*C*S) )`, uncapped.
fn ask_cost(
    q: u128,
    min_ask: u128,
    max_ask: u128,
    c: u128,
    u: u128,
    scale: u128,
) -> Result<U256, CurveError> {
    debug_assert!(c > 0);
    debug_assert!(u <= c);
    debug_assert!(q <= c - u);
    debug_assert!(scale > 0);
    // See `bid_gross`: an inverted ladder must return `ArithmeticPanic`, not panic.

    let span = max_ask
        .checked_sub(min_ask)
        .ok_or(CurveError::ArithmeticPanic)?;
    let base = U256::mul_u128(min_ask.checked_mul(2).ok_or(CurveError::DomainOverflow)?, c);
    let impact = U256::mul_u128(span, twice_plus(u, q)?);
    let factor = base.checked_add(impact).ok_or(CurveError::DomainOverflow)?;
    let num = factor
        .checked_mul_u128(q)
        .ok_or(CurveError::ArithmeticPanic)?;
    let den = denominator(c, scale)?;
    let (quo, rem) = num.div_rem(den).ok_or(CurveError::ArithmeticPanic)?;
    debug_assert!(q != 0 || quo.is_zero());
    if rem.is_zero() {
        Ok(quo)
    } else {
        quo.checked_add(U256::from_u128(1))
            .ok_or(CurveError::ArithmeticPanic)
    }
}

/// `2u + q`. Cannot overflow in domain (`<= 2C < 2^97`); `checked` for wild inputs.
#[inline]
fn twice_plus(u: u128, q: u128) -> Result<u128, CurveError> {
    let r = u
        .checked_mul(2)
        .and_then(|d| d.checked_add(q))
        .ok_or(CurveError::DomainOverflow)?;
    debug_assert!(r >= q);
    debug_assert!(r >= u);
    Ok(r)
}

/// `2*C*S`, always `>= 2` for a live pair, so no divider ever sees a zero denominator.
#[inline]
fn denominator(c: u128, scale: u128) -> Result<U256, CurveError> {
    debug_assert!(c > 0, "a zero denominator would reach the divider");
    debug_assert!(scale > 0);
    let den = U256::mul_u128(c.checked_mul(2).ok_or(CurveError::DomainOverflow)?, scale);
    debug_assert!(!den.is_zero());
    Ok(den)
}

/// `ceil(a * b / d)` clamped into `u128`, for the bisection bracket seeds only. Saturating rather
/// than fallible because every use site immediately clamps the seed into `[lo, hi]`.
fn seed_ceil(a: u128, b: u128, d: u128) -> u128 {
    debug_assert!(d != 0, "a bracket seed never divides by a zero price");
    crate::math::div_ceil_u256(U256::mul_u128(a, b), U256::from_u128(d)).unwrap_or(u128::MAX)
}

/// `floor(a * b / d)` clamped into `u128`. See [`seed_ceil`].
fn seed_floor(a: u128, b: u128, d: u128) -> u128 {
    debug_assert!(d != 0);
    crate::math::div_floor_u256(U256::mul_u128(a, b), U256::from_u128(d)).unwrap_or(u128::MAX)
}

// --- Bid side: the pool buys base, pays quote ---

/// Port of `PropCurve.amountOutBid`. Base in, quote out. Exact input.
///
/// Rounds down, once, in the pool's favour, exactly as the chain does. The `amount_in == 0` early
/// return comes **before** the capacity checks, so a zero-size quote against a zero-capacity pair
/// returns `Ok(0)` rather than [`CurveError::ZeroCapacity`]; a generated vector pins that order.
///
/// # Errors
/// `ZeroCapacity`, `AmountExceedsCapacity`, `AmountOutOfDomain`, `DomainOverflow`.
pub fn amount_out_bid(
    amount_in: u128,
    min_bid: u128,
    max_bid: u128,
    bid_capacity: u128,
    bid_used: u128,
    price_scale_exp: u8,
) -> Result<u128, CurveError> {
    debug_assert!(
        in_amount_domain(amount_in),
        "amountIn outside uint96 domain"
    );
    debug_assert!(in_price_domain(min_bid), "minBid outside uint56 domain");
    debug_assert!(in_price_domain(max_bid), "maxBid outside uint56 domain");
    debug_assert!(in_amount_domain(bid_capacity), "capacity outside uint96");
    debug_assert!(in_amount_domain(bid_used), "used outside uint96 domain");
    let scale = check_exp(price_scale_exp)?;
    debug_assert!(scale >= 1);

    if amount_in == 0 {
        return Ok(0);
    }
    if bid_capacity == 0 {
        return Err(CurveError::ZeroCapacity);
    }
    // An overflow here means the sum exceeds `bid_capacity <= u128::MAX` anyway, the verdict the
    // chain's uint256 sum also reaches. `consumed <= capacity` is what puts the bid parabola's
    // vertex out of reach and keeps the numerator's subtraction underflow-free.
    let consumed = bid_used
        .checked_add(amount_in)
        .ok_or(CurveError::AmountExceedsCapacity)?;
    if consumed > bid_capacity {
        return Err(CurveError::AmountExceedsCapacity);
    }
    debug_assert!(consumed <= bid_capacity);
    debug_assert!(amount_in <= bid_capacity - bid_used);

    let out = bid_gross(amount_in, min_bid, max_bid, bid_capacity, bid_used, scale)?
        .to_u128()
        .ok_or(CurveError::AmountOutOfDomain)?;
    // Negative space: a zero ladder pays nothing, whatever the size.
    if max_bid == 0 {
        debug_assert_eq!(out, 0, "a zero bid ladder pays nothing");
    }
    Ok(out)
}

/// Port of `PropCurve.amountInBid`. Least base input whose bid quote delivers at least
/// `amount_out` of quote. Exact output.
///
/// Inverts the *integer* function rather than the real-valued radical: [`amount_out_bid`] is
/// non-decreasing in `amount_in` across the whole domain, so the least `q` with
/// `f(q) >= amount_out` is well defined and the bisection finds it exactly, rounded up by
/// construction into the pool's favour.
///
/// # Errors
/// `ZeroCapacity`, `AmountOutOfDomain`, `DomainOverflow`, or `AmountExceedsCapacity` when the
/// epoch's whole remaining depth cannot deliver `amount_out`.
pub fn amount_in_bid(
    amount_out: u128,
    min_bid: u128,
    max_bid: u128,
    bid_capacity: u128,
    bid_used: u128,
    price_scale_exp: u8,
) -> Result<u128, CurveError> {
    debug_assert!(in_price_domain(min_bid), "minBid outside uint56 domain");
    debug_assert!(in_price_domain(max_bid), "maxBid outside uint56 domain");
    debug_assert!(in_amount_domain(bid_capacity), "capacity outside uint96");
    debug_assert!(in_amount_domain(bid_used), "used outside uint96 domain");
    let scale = check_exp(price_scale_exp)?;

    if amount_out == 0 {
        return Ok(0);
    }
    if bid_capacity == 0 {
        return Err(CurveError::ZeroCapacity);
    }
    if bid_used >= bid_capacity {
        return Err(CurveError::AmountExceedsCapacity);
    }
    // No `amount_out > AMOUNT_OUT_MAX` check: on chain that guards the `amountOut * scale` bracket
    // seed against a `uint256` argument, but here `AMOUNT_OUT_MAX == u128::MAX` and the argument's
    // own type enforces it.

    let leg = BidLeg {
        min_bid,
        max_bid,
        capacity: bid_capacity,
        used: bid_used,
        scale,
    };
    let target = U256::from_u128(amount_out);
    let room = bid_capacity - bid_used;
    debug_assert!(room >= 1);
    if leg.gross(room)? < target {
        return Err(CurveError::AmountExceedsCapacity);
    }

    let bracket = seed_bid_bracket(&leg, amount_out, (1, room));
    let bracket = refine_bid_bracket(&leg, amount_out, bracket)?;
    let q = bisect_least_bid(&leg, &target, bracket)?;

    debug_assert!(
        q >= 1,
        "a non-zero exact-output trade needs a non-zero input"
    );
    debug_assert!(q <= room);
    Ok(q)
}

/// The ladder and epoch one bid solve runs against. Grouped so the bracket search, the refinement
/// and the bisection cannot be handed three different curves: five loose `u128`s in a row is a
/// transposition that would typecheck.
#[derive(Clone, Copy)]
struct BidLeg {
    min_bid: u128,
    max_bid: u128,
    capacity: u128,
    used: u128,
    scale: u128,
}

impl BidLeg {
    #[inline]
    fn gross(&self, q: u128) -> Result<U256, CurveError> {
        bid_gross(
            q,
            self.min_bid,
            self.max_bid,
            self.capacity,
            self.used,
            self.scale,
        )
    }
}

/// Brackets from the ladder *endpoints*, so they hold regardless of rounding.
///
/// `f(q) <= floor(q*maxBid/S)`, so `f(q) >= y` forces `q >= ceil(y*S/maxBid)`: a lower bound.
/// `f(q) >= floor(q*minBid/S)`, so `q = ceil(y*S/minBid)` already satisfies `f(q) >= y`: an
/// upper bound, i.e. a point known to be feasible.
fn seed_bid_bracket(leg: &BidLeg, amount_out: u128, bracket: (u128, u128)) -> (u128, u128) {
    let (mut lo, mut hi) = bracket;
    debug_assert!(lo <= hi);
    if leg.max_bid != 0 {
        let seed = seed_ceil(amount_out, leg.scale, leg.max_bid);
        if seed > lo && seed <= hi {
            lo = seed;
        }
    }
    if leg.min_bid != 0 {
        let seed = seed_ceil(amount_out, leg.scale, leg.min_bid);
        if seed >= lo && seed < hi {
            hi = seed;
        }
    }
    assert!(
        lo <= hi,
        "a seed may narrow the bracket but never invert it"
    );
    assert!(hi <= bracket.1, "the feasible end may only move down");
    assert!(lo >= bracket.0);
    (lo, hi)
}

/// Two-sided fixed-point refinement, three rounds. Mirrors `PropCurve.amountInBid`.
///
/// Both ends stay valid bracket ends at every step, so the bisection is exact regardless of how
/// well this converges. Three rounds is a gas constant on chain, not a correctness parameter, and
/// this port must run the same count to stay bit-exact.
fn refine_bid_bracket(
    leg: &BidLeg,
    amount_out: u128,
    bracket: (u128, u128),
) -> Result<(u128, u128), CurveError> {
    let (mut lo, mut hi) = bracket;
    debug_assert!(lo <= hi);
    debug_assert!(
        leg.max_bid >= leg.min_bid,
        "established by the feasibility gross"
    );

    let span = leg.max_bid - leg.min_bid;
    let den = leg
        .capacity
        .checked_mul(2)
        .ok_or(CurveError::DomainOverflow)?;
    debug_assert!(den != 0);
    for _ in 0..3 {
        if lo >= hi {
            break;
        }
        // ceil the price = floor the drift: `>= p(lo)`.
        let drift = mul_div_floor(span, twice_plus(leg.used, lo)?, den)
            .ok_or(CurveError::DomainOverflow)?;
        let p = leg
            .max_bid
            .checked_sub(drift)
            .ok_or(CurveError::ArithmeticPanic)?;
        if p != 0 {
            let seed = seed_ceil(amount_out, leg.scale, p).min(hi);
            if seed > lo {
                lo = seed;
            }
        }
        // floor the price = ceil the drift: `<= p(hi)`.
        let drift =
            mul_div_ceil(span, twice_plus(leg.used, hi)?, den).ok_or(CurveError::DomainOverflow)?;
        let p = leg
            .max_bid
            .checked_sub(drift)
            .ok_or(CurveError::ArithmeticPanic)?;
        if p != 0 {
            let seed = seed_ceil(amount_out, leg.scale, p);
            if seed < hi && seed >= lo {
                hi = seed;
            }
        }
        assert!(lo <= hi, "refinement must never invert the bracket");
    }
    assert!(lo >= bracket.0);
    assert!(hi <= bracket.1);
    Ok((lo, hi))
}

/// Least-true bisection: the smallest `q` in the bracket with `gross(q) >= target`.
///
/// Invariant at every step: `f(hi) >= y` and `f(lo - 1) < y`.
fn bisect_least_bid(
    leg: &BidLeg,
    target: &U256,
    bracket: (u128, u128),
) -> Result<u128, CurveError> {
    let (mut lo, mut hi) = bracket;
    assert!(lo <= hi);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        debug_assert!(mid >= lo);
        debug_assert!(
            mid < hi,
            "a floored midpoint below hi is what terminates this"
        );
        if leg.gross(mid)? >= *target {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    assert_eq!(lo, hi);
    Ok(hi)
}

// --- Ask side: the pool sells base, collects quote ---

/// Port of `PropCurve.amountInAsk`. Quote cost of `amount_out` base. Exact output.
///
/// The ask side's *primitive*: base on the capacity axis, quote cost out, which is the direction
/// that keeps the curve linear in the price (property 1 in the module docs). Rounds up, once, in
/// the pool's favour.
///
/// # Errors
/// `ZeroCapacity`, `AmountExceedsCapacity`, `AmountOutOfDomain`, `DomainOverflow`, or `ZeroPrice`
/// for a wholly zero ask ladder.
pub fn amount_in_ask(
    amount_out: u128,
    min_ask: u128,
    max_ask: u128,
    ask_capacity: u128,
    ask_used: u128,
    price_scale_exp: u8,
) -> Result<u128, CurveError> {
    debug_assert!(
        in_amount_domain(amount_out),
        "amountOut outside uint96 domain"
    );
    debug_assert!(in_price_domain(min_ask), "minAsk outside uint56 domain");
    debug_assert!(in_price_domain(max_ask), "maxAsk outside uint56 domain");
    debug_assert!(in_amount_domain(ask_capacity), "capacity outside uint96");
    debug_assert!(in_amount_domain(ask_used), "used outside uint96 domain");
    let scale = check_exp(price_scale_exp)?;

    if amount_out == 0 {
        return Ok(0);
    }
    if ask_capacity == 0 {
        return Err(CurveError::ZeroCapacity);
    }
    let consumed = ask_used
        .checked_add(amount_out)
        .ok_or(CurveError::AmountExceedsCapacity)?;
    if consumed > ask_capacity {
        return Err(CurveError::AmountExceedsCapacity);
    }
    debug_assert!(amount_out <= ask_capacity - ask_used);
    // With `maxAsk >= minAsk` this is exactly "the ask ladder prices nothing", which would hand
    // over base for free. Unreachable through `PropPool`, whose accepted ladders have `maxAsk > 0`.
    if max_ask == 0 {
        return Err(CurveError::ZeroPrice);
    }

    let cost = ask_cost(amount_out, min_ask, max_ask, ask_capacity, ask_used, scale)?
        .to_u128()
        .ok_or(CurveError::AmountOutOfDomain)?;
    // Negative space: base handed over for no quote at all. The `max_ask == 0` gate forbids it and
    // `ask_cost` ceils, so a priced ladder cannot round a non-zero size down to nothing.
    debug_assert!(
        cost >= 1,
        "a non-zero size against a priced ask costs something"
    );
    Ok(cost)
}

/// Port of `PropCurve.amountOutAsk`. Most base that `amount_in` of quote strictly pays for.
/// Exact input, quote-denominated — the taker-facing ask path.
///
/// The inversion of [`amount_in_ask`], by bisection rather than the quadratic's positive root:
/// `b^2` reaches `2^308` and `2*C*S*X` reaches `2^352`, so a closed form needs 512-bit
/// intermediates and a 512-bit square root, and it would invert the real curve rather than the
/// integer one. Bisection is exact because `ask_cost` is strictly increasing in `q` and
/// `ceil(y) <= X` iff `y <= X` for integral `X`, so the integer predicate *is* the rational one.
///
/// The answer is the largest `q` the quote covers, so the taker never receives more base than
/// `amount_in` strictly pays for and any unspent remainder stays with the pool.
///
/// # Errors
/// `ZeroCapacity`, `ZeroPrice`, `AmountOutOfDomain`, `DomainOverflow`, or
/// `AmountExceedsCapacity` when `amount_in` exceeds the quote cost of the epoch's remaining base.
pub fn amount_out_ask(
    amount_in: u128,
    min_ask: u128,
    max_ask: u128,
    ask_capacity: u128,
    ask_used: u128,
    price_scale_exp: u8,
) -> Result<u128, CurveError> {
    debug_assert!(in_price_domain(min_ask), "minAsk outside uint56 domain");
    debug_assert!(in_price_domain(max_ask), "maxAsk outside uint56 domain");
    debug_assert!(in_amount_domain(ask_capacity), "capacity outside uint96");
    debug_assert!(in_amount_domain(ask_used), "used outside uint96 domain");
    let scale = check_exp(price_scale_exp)?;

    if amount_in == 0 {
        return Ok(0);
    }
    if ask_capacity == 0 {
        return Err(CurveError::ZeroCapacity);
    }
    if ask_used >= ask_capacity {
        return Err(CurveError::AmountExceedsCapacity);
    }
    if max_ask == 0 {
        return Err(CurveError::ZeroPrice);
    }
    // On chain there is an `amountIn > AMOUNT_OUT_MAX` check here, an *input* bound because the
    // bracket seeds multiply the argument by `10**38`. With `AMOUNT_OUT_MAX == u128::MAX` the
    // argument's own type enforces it.

    let leg = AskLeg {
        min_ask,
        max_ask,
        capacity: ask_capacity,
        used: ask_used,
        scale,
    };
    let room = ask_capacity - ask_used;
    debug_assert!(room >= 1);
    let budget = U256::from_u128(amount_in);
    // The epoch's quote-denominated ceiling is the cost of all its remaining base; refusing above
    // it avoids silently charging more than the base on offer is worth.
    let full = leg.cost(room)?;
    if budget > full {
        return Err(CurveError::AmountExceedsCapacity);
    }
    if budget == full {
        return Ok(room);
    }

    // `cost(room) > amount_in`, so the answer is in `[0, room - 1]`.
    let bracket = seed_ask_bracket(&leg, amount_in, (0, room - 1));
    let bracket = refine_ask_bracket(&leg, amount_in, bracket)?;
    let q = bisect_greatest_ask(&leg, &budget, bracket)?;

    assert!(
        q < room,
        "the whole remaining epoch was already ruled unaffordable"
    );
    Ok(q)
}

/// The ladder and epoch one ask solve runs against. See [`BidLeg`].
#[derive(Clone, Copy)]
struct AskLeg {
    min_ask: u128,
    max_ask: u128,
    capacity: u128,
    used: u128,
    scale: u128,
}

impl AskLeg {
    #[inline]
    fn cost(&self, q: u128) -> Result<U256, CurveError> {
        ask_cost(
            q,
            self.min_ask,
            self.max_ask,
            self.capacity,
            self.used,
            self.scale,
        )
    }
}

/// Brackets from the ladder *endpoints*.
///
/// `cost(q) <= ceil(q*maxAsk/S)`, so every `q <= floor(X*S/maxAsk)` is affordable: a lower bound.
/// In domain it cannot exceed `hi`, since affordability is monotone and `room` is not affordable,
/// so the `min` only keeps this total.
/// `cost(q) >= q*minAsk/S`, so affordability forces `q <= floor(X*S/minAsk)`: an upper bound.
fn seed_ask_bracket(leg: &AskLeg, amount_in: u128, bracket: (u128, u128)) -> (u128, u128) {
    let (mut lo, mut hi) = bracket;
    debug_assert!(lo <= hi);
    let seed = seed_floor(amount_in, leg.scale, leg.max_ask).min(hi);
    if seed > lo {
        lo = seed;
    }
    if leg.min_ask != 0 {
        let seed = seed_floor(amount_in, leg.scale, leg.min_ask);
        if seed < hi && seed >= lo {
            hi = seed;
        }
    }
    assert!(lo <= hi, "both seed updates are guarded against inverting");
    assert!(lo >= bracket.0);
    assert!(hi <= bracket.1);
    (lo, hi)
}

/// Two-sided fixed-point refinement, three rounds. Mirrors `PropCurve.amountOutAsk`.
///
/// Without it the endpoint brackets are a *relative* interval and the bisection costs ~53
/// iterations on a 20 bps ladder, about 40k gas on the primary ask swap path. Both ends stay valid
/// at every step, so exactness does not depend on convergence.
///
/// Unlike [`refine_bid_bracket`], the `hi` update carries no `seed >= lo` guard, mirroring
/// Solidity, so `lo > hi` is reachable here. The loop's `lo >= hi` break and the bisection's
/// `while lo < hi` both handle it by returning `lo`, a point already known to be affordable.
fn refine_ask_bracket(
    leg: &AskLeg,
    amount_in: u128,
    bracket: (u128, u128),
) -> Result<(u128, u128), CurveError> {
    let (mut lo, mut hi) = bracket;
    debug_assert!(lo <= hi);
    debug_assert!(
        leg.max_ask >= leg.min_ask,
        "established by the feasibility cost"
    );

    let span = leg.max_ask - leg.min_ask;
    let den = leg
        .capacity
        .checked_mul(2)
        .ok_or(CurveError::DomainOverflow)?;
    debug_assert!(den != 0);
    for _ in 0..3 {
        if lo >= hi {
            break;
        }
        // floor the price: `<= p(lo)`, which makes `X*S / p` an upper bound on the answer.
        let drift = mul_div_floor(span, twice_plus(leg.used, lo)?, den)
            .ok_or(CurveError::DomainOverflow)?;
        let p = leg
            .min_ask
            .checked_add(drift)
            .ok_or(CurveError::DomainOverflow)?;
        if p != 0 {
            let seed = seed_floor(amount_in, leg.scale, p);
            if seed < hi {
                hi = seed;
            }
        }
        // ceil the price: `>= p(hi)`, which makes every `q <= X*S / p` affordable.
        let drift =
            mul_div_ceil(span, twice_plus(leg.used, hi)?, den).ok_or(CurveError::DomainOverflow)?;
        let p = leg
            .min_ask
            .checked_add(drift)
            .ok_or(CurveError::DomainOverflow)?;
        if p != 0 {
            let seed = seed_floor(amount_in, leg.scale, p).min(hi);
            if seed > lo {
                lo = seed;
            }
        }
    }
    assert!(lo >= bracket.0, "the affordable end may only move up");
    assert!(hi <= bracket.1, "the unaffordable end may only move down");
    Ok((lo, hi))
}

/// Greatest-true bisection: the largest `q` in the bracket whose cost the budget covers.
///
/// Invariant at every step: `cost(lo) <= X < cost(hi + 1)`.
fn bisect_greatest_ask(
    leg: &AskLeg,
    budget: &U256,
    bracket: (u128, u128),
) -> Result<u128, CurveError> {
    let (mut lo, mut hi) = bracket;
    while lo < hi {
        // The UPPER midpoint, `lo + ceil((hi - lo) / 2)`. A floored one stalls at `hi == lo + 1`.
        let mid = lo + (hi - lo).div_ceil(2);
        assert!(mid > lo, "a floored midpoint here would not terminate");
        debug_assert!(mid <= hi);
        if leg.cost(mid)? <= *budget {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    Ok(lo)
}

// --- Average price: reporting only ---
//
// No counterpart in the quote path, which folds the price into the amount and computes no
// intermediate price at all. These report the exact rational average over `[u, u+q]` for the
// inverse solver and the update policy (archi_v2 §5.3), rounded in each case so the reported price
// is never better for the taker than the interval executes.

/// The average bid price over `[used, used + q]`, floored:
/// `maxBid - ceil(W * (2*used + q) / (2*C))`.
///
/// Floored because a lower bid is worse for the taker. The exact rational average lies in
/// `[result, result + 1)`.
///
/// # Errors
/// `ZeroCapacity`, or `ArithmeticPanic` if `maxBid < minBid`.
pub fn avg_bid_price(
    min_bid: u128,
    max_bid: u128,
    capacity: u128,
    used: u128,
    q: u128,
) -> Result<u128, CurveError> {
    if capacity == 0 {
        return Err(CurveError::ZeroCapacity);
    }
    let span = max_bid
        .checked_sub(min_bid)
        .ok_or(CurveError::ArithmeticPanic)?;
    let discount = mul_div_ceil(
        span,
        twice_plus(used, q)?,
        capacity.checked_mul(2).ok_or(CurveError::DomainOverflow)?,
    )
    .ok_or(CurveError::DomainOverflow)?;
    let avg = max_bid
        .checked_sub(discount)
        .ok_or(CurveError::ArithmeticPanic)?;
    debug_assert!(avg <= max_bid, "the average can never beat the best bid");
    // Bounded below by the ladder only for an interval the epoch contains: past capacity the
    // discount runs off the bottom and the answer stops being a price on this book.
    if used + q <= capacity {
        debug_assert!(avg >= min_bid);
    }
    Ok(avg)
}

/// The average ask price over `[used, used + q]`, ceiled:
/// `minAsk + ceil(W * (2*used + q) / (2*C))`.
///
/// Ceiled because a higher ask is worse for the taker. The exact rational average lies in
/// `(result - 1, result]`.
///
/// # Errors
/// `ZeroCapacity`, or `ArithmeticPanic` if `maxAsk < minAsk`.
pub fn avg_ask_price(
    min_ask: u128,
    max_ask: u128,
    capacity: u128,
    used: u128,
    q: u128,
) -> Result<u128, CurveError> {
    if capacity == 0 {
        return Err(CurveError::ZeroCapacity);
    }
    let span = max_ask
        .checked_sub(min_ask)
        .ok_or(CurveError::ArithmeticPanic)?;
    let premium = mul_div_ceil(
        span,
        twice_plus(used, q)?,
        capacity.checked_mul(2).ok_or(CurveError::DomainOverflow)?,
    )
    .ok_or(CurveError::DomainOverflow)?;
    let avg = min_ask
        .checked_add(premium)
        .ok_or(CurveError::DomainOverflow)?;
    debug_assert!(avg >= min_ask, "the average can never beat the best ask");
    // See [`avg_bid_price`]: bounded by the ladder only for an interval the epoch contains.
    if used + q <= capacity {
        debug_assert!(avg <= max_ask);
    }
    Ok(avg)
}

// --- Ladder validation ---

/// Port of `PropCurve.validateLadder`.
///
/// Accepts exactly `maxAsk >= minAsk >= maxBid >= minBid >= minPrice` **and** the strict
/// `maxAsk > minBid`. That strict comparison rejects an entirely flat ladder, so the off-chain
/// builder must always leave one price unit between `minBid` and `maxAsk`; it is also what forces
/// `maxAsk >= 1` and makes [`CurveError::ZeroPrice`] unreachable on the ask side. Check order
/// decides which error surfaces, and the floor check runs first.
///
/// # Errors
/// `BidBelowMinPrice` or `CrossedBook`.
pub fn validate_ladder(
    min_bid: u128,
    max_bid: u128,
    min_ask: u128,
    max_ask: u128,
    min_price: u128,
) -> Result<(), CurveError> {
    if min_bid < min_price {
        return Err(CurveError::BidBelowMinPrice);
    }
    // Split so a failure points at which relation broke. Solidity states the same boolean as one
    // De Morgan'd `&&` chain, with the same error and the same precedence.
    if max_ask < min_ask {
        return Err(CurveError::CrossedBook);
    }
    if min_ask < max_bid {
        return Err(CurveError::CrossedBook);
    }
    if max_bid < min_bid {
        return Err(CurveError::CrossedBook);
    }
    if max_ask <= min_bid {
        return Err(CurveError::CrossedBook);
    }
    // Restated positively on the accepting path: this is the gate the four on-chain prices are
    // admitted by. The paired statement at the point of construction is `ladder::assert_chain`.
    debug_assert!(min_bid >= min_price);
    debug_assert!(min_bid <= max_bid);
    debug_assert!(max_bid <= min_ask);
    debug_assert!(min_ask <= max_ask);
    debug_assert!(max_ask > min_bid, "a flat ladder is rejected, not accepted");
    debug_assert!(
        max_ask >= 1,
        "which is what makes ZeroPrice unreachable on the ask"
    );
    Ok(())
}

// --- Introspection ---

/// Port of `PropCurve.executableTopBid`.
///
/// The price a taker receives right now for an infinitesimal bid. This, not `maxBid`, is what the
/// update policy compares against (archi_v2 §5.3): once the epoch is partly consumed the
/// executable top has already moved. CEILS the drift, so the reported bid is at or below the exact
/// rational zero-size limit `maxBid - W*used/C`. Exactly
/// `avg_bid_price(min_bid, max_bid, capacity, used, 0)`.
///
/// # Errors
/// `ArithmeticPanic` if `maxBid < minBid` (`Panic(0x11)` on chain).
pub fn executable_top_bid(
    min_bid: u128,
    max_bid: u128,
    bid_capacity: u128,
    bid_used: u128,
) -> Result<u128, CurveError> {
    debug_assert!(in_price_domain(min_bid), "minBid outside uint56 domain");
    debug_assert!(in_price_domain(max_bid), "maxBid outside uint56 domain");
    if bid_capacity == 0 {
        return Ok(0);
    }
    if bid_used >= bid_capacity {
        return Ok(min_bid);
    }
    let span = max_bid
        .checked_sub(min_bid)
        .ok_or(CurveError::ArithmeticPanic)?;
    let drift = mul_div_ceil(span, bid_used, bid_capacity).ok_or(CurveError::DomainOverflow)?;
    debug_assert!(drift <= span, "the drift cannot walk past minBid");
    let top = max_bid
        .checked_sub(drift)
        .ok_or(CurveError::ArithmeticPanic)?;
    debug_assert!(
        top <= max_bid,
        "the top only ever walks down as usage grows"
    );
    debug_assert!(top >= min_bid);
    Ok(top)
}

/// Port of `PropCurve.executableTopAsk`.
///
/// CEILS the drift for the same reason as [`executable_top_bid`], so it is exactly
/// `avg_ask_price(min_ask, max_ask, capacity, used, 0)`. Returns [`NO_ASK`] when
/// `askCapacity == 0`; see that constant for the one place this port cannot be bit-identical.
///
/// # Errors
/// `ArithmeticPanic` if `maxAsk < minAsk`.
pub fn executable_top_ask(
    min_ask: u128,
    max_ask: u128,
    ask_capacity: u128,
    ask_used: u128,
) -> Result<u128, CurveError> {
    debug_assert!(in_price_domain(min_ask), "minAsk outside uint56 domain");
    debug_assert!(in_price_domain(max_ask), "maxAsk outside uint56 domain");
    if ask_capacity == 0 {
        return Ok(NO_ASK);
    }
    if ask_used >= ask_capacity {
        return Ok(max_ask);
    }
    let span = max_ask
        .checked_sub(min_ask)
        .ok_or(CurveError::ArithmeticPanic)?;
    let drift = mul_div_ceil(span, ask_used, ask_capacity).ok_or(CurveError::DomainOverflow)?;
    debug_assert!(drift <= span);
    let top = min_ask
        .checked_add(drift)
        .ok_or(CurveError::DomainOverflow)?;
    debug_assert!(top >= min_ask, "the top only ever walks up as usage grows");
    debug_assert!(top <= max_ask);
    // Negative space: the zero-capacity sentinel is returned above and only above.
    debug_assert!(top != NO_ASK);
    Ok(top)
}

#[cfg(test)]
mod tests {
    use super::*;

    const E: u8 = 18;

    #[test]
    fn zero_amount_returns_zero_before_any_other_check() {
        // Ordering pin: zero amount beats zero capacity, on all four paths.
        assert_eq!(amount_out_bid(0, 0, 0, 0, 0, 0), Ok(0));
        assert_eq!(amount_in_bid(0, 0, 0, 0, 0, 0), Ok(0));
        assert_eq!(amount_in_ask(0, 0, 0, 0, 0, 0), Ok(0));
        assert_eq!(amount_out_ask(0, 0, 0, 0, 0, 0), Ok(0));
    }

    #[test]
    fn zero_capacity_reverts_for_nonzero_amount() {
        assert_eq!(
            amount_out_bid(1, 1, 2, 0, 0, E),
            Err(CurveError::ZeroCapacity)
        );
        assert_eq!(
            amount_in_bid(1, 1, 2, 0, 0, E),
            Err(CurveError::ZeroCapacity)
        );
        assert_eq!(
            amount_in_ask(1, 1, 2, 0, 0, E),
            Err(CurveError::ZeroCapacity)
        );
        assert_eq!(
            amount_out_ask(1, 1, 2, 0, 0, E),
            Err(CurveError::ZeroCapacity)
        );
    }

    #[test]
    fn capacity_boundary_is_inclusive() {
        assert!(amount_out_bid(100, 1_000, 2_000, 100, 0, 0).is_ok());
        assert_eq!(
            amount_out_bid(101, 1_000, 2_000, 100, 0, 0),
            Err(CurveError::AmountExceedsCapacity)
        );
        assert!(amount_out_bid(40, 1_000, 2_000, 100, 60, 0).is_ok());
        assert_eq!(
            amount_out_bid(41, 1_000, 2_000, 100, 60, 0),
            Err(CurveError::AmountExceedsCapacity)
        );
        // The ask side's capacity axis is base too, so `amount_in_ask` checks the same way.
        assert!(amount_in_ask(100, 1_000, 2_000, 100, 0, 0).is_ok());
        assert_eq!(
            amount_in_ask(101, 1_000, 2_000, 100, 0, 0),
            Err(CurveError::AmountExceedsCapacity)
        );
    }

    #[test]
    fn midpoint_pricing_is_the_average() {
        // The whole 100-unit ladder from 2_000 down to 1_000 charges its midpoint, 1_500.
        assert_eq!(
            amount_out_bid(100, 1_000, 2_000, 100, 0, 0),
            Ok(100 * 1_500)
        );
        // Each half charges its own midpoint, and the halves sum to the whole exactly.
        assert_eq!(amount_out_bid(50, 1_000, 2_000, 100, 0, 0), Ok(50 * 1_750));
        assert_eq!(amount_out_bid(50, 1_000, 2_000, 100, 50, 0), Ok(50 * 1_250));
        assert_eq!(50 * 1_750 + 50 * 1_250, 100 * 1_500);
    }

    #[test]
    fn ask_is_the_mirror_of_the_bid_now_that_both_are_base_denominated() {
        // The same ladder walked upward: 100 base costs the 1_500 midpoint, not `100 / 1_500`.
        assert_eq!(amount_in_ask(100, 1_000, 2_000, 100, 0, 0), Ok(100 * 1_500));
        assert_eq!(amount_in_ask(50, 1_000, 2_000, 100, 0, 0), Ok(50 * 1_250));
        assert_eq!(amount_in_ask(50, 1_000, 2_000, 100, 50, 0), Ok(50 * 1_750));
        // The inversion recovers the size exactly when the budget is exact.
        assert_eq!(
            amount_out_ask(100 * 1_500, 1_000, 2_000, 100, 0, 0),
            Ok(100)
        );
        assert_eq!(amount_out_ask(50 * 1_250, 1_000, 2_000, 100, 0, 0), Ok(50));
        // One quote unit short buys one base unit less at this ladder.
        assert_eq!(
            amount_out_ask(50 * 1_250 - 1, 1_000, 2_000, 100, 0, 0),
            Ok(49)
        );
    }

    /// Monotonicity across the wei boundary on an 18/6 pair at 3e9, 100 bps of ladder, 30 base of
    /// capacity — the shape that broke when prices were still quantised.
    #[test]
    fn one_extra_wei_never_returns_less() {
        let price = 3_000_000_000u128;
        let (min_bid, max_bid) = (price - price / 100, price);
        let cap = 30_000_000_000_000_000_000u128;
        let a = amount_out_bid(2_000_000_000_000_000_000, min_bid, max_bid, cap, 0, 12).unwrap();
        let b = amount_out_bid(2_000_000_000_000_000_001, min_bid, max_bid, cap, 0, 12).unwrap();
        assert!(b >= a, "{b} < {a}: monotonicity broken again");
        let mut prev = 0u128;
        for q in (0..40u128).map(|i| 2_000_000_000_000_000_000 - 20 + i) {
            let out = amount_out_bid(q, min_bid, max_bid, cap, 0, 12).unwrap();
            assert!(out >= prev, "q={q} out={out} prev={prev}");
            prev = out;
        }
    }

    #[test]
    fn splitting_an_ask_never_beats_executing_it_whole() {
        // The Jensen defect's shape: a wide ladder and a big trade.
        let (min_ask, max_ask) = (1_000_000_000_000u128, 1_200_000_000_000u128); // 2000 bps
        let cap = 1_000_000_000u128;
        let whole = amount_in_ask(cap, min_ask, max_ask, cap, 0, 12).unwrap();
        for pieces in [2u128, 4, 8, 64] {
            let mut total = 0u128;
            let mut used = 0u128;
            for i in 0..pieces {
                let piece = cap / pieces + u128::from(i < cap % pieces);
                total += amount_in_ask(piece, min_ask, max_ask, cap, used, 12).unwrap();
                used += piece;
            }
            assert_eq!(used, cap);
            assert!(total >= whole, "{pieces} pieces paid {total} < {whole}");
            assert!(
                total - whole < pieces,
                "residual {} exceeds one unit per piece",
                total - whole
            );
        }
    }

    #[test]
    fn zero_price_only_on_a_wholly_zero_ask_ladder() {
        // `maxAsk == 0` is the only shape, and it is unreachable through `validate_ladder`.
        assert_eq!(amount_in_ask(1, 0, 0, 10, 0, E), Err(CurveError::ZeroPrice));
        assert_eq!(
            amount_out_ask(1, 0, 0, 10, 0, E),
            Err(CurveError::ZeroPrice)
        );
        assert_eq!(
            amount_in_ask(AMOUNT_MAX, 0, 0, AMOUNT_MAX, 0, E),
            Err(CurveError::ZeroPrice)
        );
        // One unit of span is enough to price the interval: ceil lifts any nonzero cost to 1.
        assert_eq!(amount_in_ask(1, 0, 1, 1_000_000, 0, E), Ok(1));
        assert_eq!(amount_in_ask(1, 0, PRICE_MAX, AMOUNT_MAX, 0, 0), Ok(1));
    }

    #[test]
    fn exact_output_paths_round_toward_the_pool() {
        // Least base input that yields >= 1_000 quote on a flat ladder at price 10, exp 0.
        assert_eq!(amount_in_bid(1_000, 10, 10, 1_000, 0, 0), Ok(100));
        assert_eq!(amount_in_bid(1_001, 10, 10, 1_000, 0, 0), Ok(101));
        assert_eq!(amount_out_bid(100, 10, 10, 1_000, 0, 0), Ok(1_000));
        // Round trip: the least input's output is never below the request.
        for want in [1u128, 7, 99, 1_000, 9_999] {
            let q = amount_in_bid(want, 9, 11, 10_000, 0, 0).unwrap();
            assert!(amount_out_bid(q, 9, 11, 10_000, 0, 0).unwrap() >= want);
            if q > 1 {
                assert!(
                    amount_out_bid(q - 1, 9, 11, 10_000, 0, 0).unwrap() < want,
                    "not minimal at {want}"
                );
            }
        }
    }

    #[test]
    fn ask_inversion_never_overpays_the_taker() {
        let (min_ask, max_ask, cap) = (1_000_000u128, 1_010_000u128, 1_000_000u128);
        // The epoch's remaining base costs ~1_005_000 quote here, so every budget below sits under
        // it; `ask_inversion_reverts_above_the_epoch_ceiling` covers the rest.
        for budget in [1u128, 1_000, 999_999, 1_004_999, 1_005_000] {
            let q = amount_out_ask(budget, min_ask, max_ask, cap, 0, 6).unwrap();
            let cost = amount_in_ask(q, min_ask, max_ask, cap, 0, 6).unwrap_or(0);
            assert!(cost <= budget, "q={q} cost {cost} exceeds budget {budget}");
            // Maximal: one more base unit would cost more than the budget.
            if q < cap {
                let more = amount_in_ask(q + 1, min_ask, max_ask, cap, 0, 6).unwrap();
                assert!(more > budget, "q={q} was not maximal for budget {budget}");
            }
        }
    }

    #[test]
    fn ask_inversion_reverts_above_the_epoch_ceiling() {
        let (min_ask, max_ask, cap) = (1_000u128, 2_000u128, 100u128);
        let full = amount_in_ask(cap, min_ask, max_ask, cap, 0, 0).unwrap();
        assert_eq!(amount_out_ask(full, min_ask, max_ask, cap, 0, 0), Ok(cap));
        assert_eq!(
            amount_out_ask(full + 1, min_ask, max_ask, cap, 0, 0),
            Err(CurveError::AmountExceedsCapacity)
        );
    }

    #[test]
    fn validator_matches_solidity_ordering() {
        assert_eq!(validate_ladder(10, 20, 30, 40, 10), Ok(()));
        assert_eq!(
            validate_ladder(9, 20, 30, 40, 10),
            Err(CurveError::BidBelowMinPrice)
        );
        assert_eq!(
            validate_ladder(10, 20, 30, 29, 10),
            Err(CurveError::CrossedBook)
        );
        assert_eq!(
            validate_ladder(10, 31, 30, 40, 10),
            Err(CurveError::CrossedBook)
        );
        assert_eq!(
            validate_ladder(21, 20, 30, 40, 10),
            Err(CurveError::CrossedBook)
        );
        // Flat ladder: ordering passes, the strict maxAsk > minBid does not.
        assert_eq!(
            validate_ladder(10, 10, 10, 10, 10),
            Err(CurveError::CrossedBook)
        );
        assert_eq!(validate_ladder(10, 10, 10, 11, 10), Ok(()));
        // Floor check runs before the ordering check.
        assert_eq!(
            validate_ladder(0, 0, 0, 0, 1),
            Err(CurveError::BidBelowMinPrice)
        );
    }

    #[test]
    fn executable_tops() {
        assert_eq!(executable_top_bid(1_000, 2_000, 0, 0), Ok(0));
        assert_eq!(executable_top_bid(1_000, 2_000, 100, 0), Ok(2_000));
        assert_eq!(executable_top_bid(1_000, 2_000, 100, 50), Ok(1_500));
        assert_eq!(executable_top_bid(1_000, 2_000, 100, 100), Ok(1_000));
        assert_eq!(executable_top_bid(1_000, 2_000, 100, 999), Ok(1_000));

        assert_eq!(executable_top_ask(1_000, 2_000, 0, 0), Ok(NO_ASK));
        assert_eq!(executable_top_ask(1_000, 2_000, 100, 0), Ok(1_000));
        assert_eq!(executable_top_ask(1_000, 2_000, 100, 50), Ok(1_500));
        assert_eq!(executable_top_ask(1_000, 2_000, 100, 100), Ok(2_000));
        // The CEIL, on both sides: a drift of 1/3 of a unit reports the worse price, and each
        // helper is exactly the zero-size limit of its average-price counterpart.
        assert_eq!(executable_top_ask(1_000, 1_001, 3, 1), Ok(1_001));
        assert_eq!(executable_top_bid(1_000, 1_001, 3, 1), Ok(1_000));
        assert_eq!(
            executable_top_bid(1_000, 1_001, 3, 1),
            avg_bid_price(1_000, 1_001, 3, 1, 0)
        );
        assert_eq!(
            executable_top_ask(1_000, 1_001, 3, 1),
            avg_ask_price(1_000, 1_001, 3, 1, 0)
        );
    }

    #[test]
    fn realistic_weth_usdc_18_6() {
        // priceScaleExp = 24: price = human * 10**(24 - 18 + 6) = human * 10**12.
        let px = 3_000 * 1_000_000_000_000u128;
        let out = amount_out_bid(
            1_000_000_000_000_000_000,
            px,
            px,
            10_000_000_000_000_000_000,
            0,
            24,
        )
        .unwrap();
        assert_eq!(out, 3_000_000_000); // 3_000 USDC at 6dp

        let cost = amount_in_ask(
            1_000_000_000_000_000_000,
            px,
            px,
            10_000_000_000_000_000_000,
            0,
            24,
        )
        .unwrap();
        assert_eq!(cost, 3_000_000_000);
        assert_eq!(
            amount_out_ask(cost, px, px, 10_000_000_000_000_000_000, 0, 24),
            Ok(1_000_000_000_000_000_000)
        );
    }

    #[test]
    fn realistic_wbtc_usdc_8_6() {
        let px = 60_000 * 100_000_000_000u128;
        assert!(px <= PRICE_MAX);
        assert_eq!(
            amount_out_bid(100_000_000, px, px, 1_000_000_000, 0, 13),
            Ok(60_000_000_000)
        );
    }

    #[test]
    fn realistic_usdc_weth_6_18() {
        let px = 33_333_333_333_333_300u128;
        assert!(px <= PRICE_MAX);
        assert_eq!(
            amount_out_bid(1_000_000, px, px, 1_000_000_000_000, 0, 8),
            Ok(333_333_333_333_333)
        );
    }

    #[test]
    fn domain_edges_are_reported_not_wrapped() {
        // exp = 0 at the corner: `q * maxBid` is ~2^152, above `AMOUNT_OUT_MAX`. Shared revert.
        assert_eq!(
            amount_out_bid(AMOUNT_MAX, PRICE_MAX, PRICE_MAX, AMOUNT_MAX, 0, 0),
            Err(CurveError::AmountOutOfDomain)
        );
        assert_eq!(
            amount_in_ask(AMOUNT_MAX, PRICE_MAX, PRICE_MAX, AMOUNT_MAX, 0, 0),
            Err(CurveError::AmountOutOfDomain)
        );
        // One unit under the boundary on the same shape: avgBid == 1, so out == q < 2^96.
        assert_eq!(
            amount_out_bid(AMOUNT_MAX, 1, 1, AMOUNT_MAX, 0, 0),
            Ok(AMOUNT_MAX)
        );
        // exp above PRICE_SCALE_EXP_MAX: still port-only, and it is checked first.
        assert_eq!(
            amount_out_bid(1, 1, 1, 1, 0, 39),
            Err(CurveError::DomainOverflow)
        );
    }

    #[test]
    fn arithmetic_panic_on_inverted_ladder() {
        assert_eq!(
            amount_out_bid(1, 2_000, 1_000, 10, 0, 0),
            Err(CurveError::ArithmeticPanic)
        );
        assert_eq!(
            amount_in_ask(1, 2_000, 1_000, 10, 0, 0),
            Err(CurveError::ArithmeticPanic)
        );
        assert_eq!(
            amount_out_ask(1, 2_000, 1_000, 10, 0, 0),
            Err(CurveError::ArithmeticPanic)
        );
        assert_eq!(
            amount_in_bid(1, 2_000, 1_000, 10, 0, 0),
            Err(CurveError::ArithmeticPanic)
        );
    }

    #[test]
    fn average_price_helpers_round_toward_the_pool() {
        // Flat and exactly-representable ladders leave no rounding to observe.
        assert_eq!(avg_bid_price(1_000, 1_000, 100, 0, 100), Ok(1_000));
        assert_eq!(avg_bid_price(1_000, 2_000, 100, 0, 100), Ok(1_500));
        assert_eq!(avg_ask_price(1_000, 2_000, 100, 0, 100), Ok(1_500));
        // A ladder whose midpoint is not representable: bid floors, ask ceils.
        assert_eq!(avg_bid_price(1_000, 1_001, 3, 0, 1), Ok(1_000));
        assert_eq!(avg_ask_price(1_000, 1_001, 3, 0, 1), Ok(1_001));
    }
}
