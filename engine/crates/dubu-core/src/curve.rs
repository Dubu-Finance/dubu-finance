//! Exact integer port of `contracts/src/libraries/PropCurve.sol`.
//!
//! `PropCurve.sol` is **authoritative, not frozen**. It has been amended four times since this
//! port was written, and each amendment moved the chain toward this port or fixed a defect this
//! port then mirrored:
//!
//! 1. **`MAX_AMOUNT_OUT`** (`== type(uint128).max`). The quote paths revert with
//!    `AmountOutOfDomain` above it. Before, the chain would settle a trade whose amount this
//!    port cannot represent — the engine would treat a size as unquotable while the pool filled
//!    it, which is a silent-loss bug rather than a crash.
//! 2. **One-directional rounding** — superseded by (4).
//! 3. **Base-denominated capacity on both sides.** Ask capacity used to be quote-denominated,
//!    which made the ask output `amountIn * scale / avgAsk`, the *reciprocal* of the midpoint
//!    price. `1/p` is convex, so by Jensen the midpoint rule under-delivered and every extra
//!    slice of a split integrated the true curve more finely: splitting an ask was strictly
//!    dominant for the taker. Both sides are now linear in the price and additive.
//! 4. **One rounding per trade, and never on the price.** Quantising the price to a whole unit
//!    and then multiplying by the trade size broke monotonicity of `amountOut` in `amountIn`,
//!    which is also the precondition that makes `PropPool.getAmountIn`'s binary search exact.
//!    The price is now folded into the amount and rounded once, at the end.
//!
//! Nothing here may deviate from the amended source except where a deviation is called out
//! below *and* in the doc comment of the function concerned. Every rounding direction, every
//! early return, every revert condition, and the *order* in which they are checked, is load
//! bearing.
//!
//! # The curve
//!
//! A linear-impact single tick. The bid side decays from `maxBid` at zero usage to `minBid` at
//! full capacity; the ask side rises from `minAsk` to `maxAsk`. Usage and capacity are in
//! **base** units on both sides. A trade consuming `[u, u + q]` of a side with capacity `C` is
//! charged the average price over that interval, which for a linear ladder is its value at the
//! midpoint. Doubling clears the half-integer midpoint, so with `W` the span and
//! `S = 10**priceScaleExp` the quote legs are, exactly:
//!
//! ```text
//! bid:  amountOut = floor( q * (2*maxBid*C - W*(2u + q)) / (2*C*S) )
//! ask:  amountIn  =  ceil( q * (2*minAsk*C + W*(2u + q)) / (2*C*S) )
//! ```
//!
//! The numerators are the exact integral of the ladder over `[u, u+q]`; nothing is rounded until
//! the single final division. That is what makes the additivity claims below theorems rather
//! than approximations.
//!
//! # What splitting can and cannot do — now proved, not asserted
//!
//! The exact rational integral is additive: carving `[u, u+q]` into contiguous pieces leaves the
//! sum unchanged. Therefore
//!
//! * **bid**, `sum floor(x_i) <= floor(sum x_i)`: no decomposition ever collects more quote than
//!   the undivided trade, and the shortfall is at most `n - 1` quote units for `n` pieces.
//! * **ask (base out)**, `sum ceil(x_i) >= ceil(sum x_i)`: no decomposition ever pays less quote,
//!   with the same `n - 1` unit bound in the pool's favour.
//! * **ask (quote in)**: [`amount_out_ask`] is the exact inverse of an additive function, hence
//!   super-additive, so splitting cannot buy more base.
//!
//! `splitting_never_beats_one_shot_on_either_side` states all three. The old
//! `splitting_a_bid_costs_at_most_one_price_tick` — which documented a residual worth *one price
//! tick times the trade size* — is gone: that residual came from quantising the price, and there
//! is no longer an intermediate price to quantise.
//!
//! # Monotonicity
//!
//! Both numerators are quadratics in `q`. The ask numerator is an upward parabola through the
//! origin, so it is increasing for all `q >= 0`. The bid numerator `A*q - W*q^2` with
//! `A = 2*maxBid*C - 2*W*u` peaks at `q* = A/(2W)`, and
//!
//! ```text
//! q* >= C - u  <=>  2*maxBid*C - 2*W*u >= 2*W*(C - u)  <=>  maxBid >= W  <=>  minBid >= 0
//! ```
//!
//! so the vertex lies at or beyond the largest admissible `q` for **every** ladder — no side
//! condition, and nothing `validate_ladder` admits can violate it. The bound is tight only at
//! `minBid == 0`, where `N` is still non-decreasing on the closed domain
//! (`N(q+1) - N(q) = A - W*(2q+1) >= W >= 0`).
//!
//! # What the two implementations share
//!
//! Bit-for-bit, on every input in the domain below:
//!
//! * [`amount_out_bid`], [`amount_in_bid`], [`amount_in_ask`], [`amount_out_ask`] — all
//!   arithmetic, the single rounding direction of each, the bisection brackets, and the
//!   `amount == 0` early return that precedes the capacity checks.
//! * [`validate_ladder`] — all three conditions, in order, so the *reason* for a rejection
//!   agrees too.
//! * [`executable_top_bid`] (floors) and [`executable_top_ask`] (ceils).
//! * The reverts [`CurveError::AmountExceedsCapacity`], [`CurveError::ZeroCapacity`],
//!   [`CurveError::ZeroPrice`], [`CurveError::CrossedBook`], [`CurveError::BidBelowMinPrice`]
//!   and [`CurveError::AmountOutOfDomain`] — same conditions, same precedence. All six are
//!   pinned by generated vectors.
//!
//! [`avg_bid_price`] / [`avg_ask_price`] have **no counterpart in the quote path any more**.
//! They are reporting helpers for the inverse solver and the update policy; see their docs.
//!
//! # What they do not share: three divergences, all deliberate
//!
//! 1. [`MAX_PRICE_SCALE_EXP`] is **not enforced on chain**. `10 ** 39` evaluates fine in
//!    `uint256` (up to `exp == 77`), so the chain succeeds where this port returns
//!    [`CurveError::DomainOverflow`]. Safe in one direction only: refusing to quote cannot lose
//!    money. `PropPool.addPair` is where the bound is actually enforced. Not emittable as a
//!    vector.
//! 2. [`NO_ASK`] replaces `type(uint256).max` in [`executable_top_ask`] on zero capacity,
//!    because that value is not representable in `u128`. It is an infinity sentinel, never a
//!    price. The generated vector for this case carries the true `uint256` decimal.
//! 3. [`CurveError::ArithmeticPanic`] stands in for Solidity 0.8's unnamed `Panic(0x11)` /
//!    `Panic(0x12)`, reachable only on inputs `validate_ladder` already rejects, such as
//!    `maxBid < minBid`. Same verdict, different revert encoding.
//!
//! The fourth asymmetry the previous revision flagged — `executableTop*` flooring their drift
//! while the quote path ceiled its impact, leaving both helpers up to one price unit *optimistic
//! for the taker* — has been **resolved on chain**, not merely mirrored, and the resolution is
//! that both sides now CEIL the drift. Under the amended arithmetic the quote path rounds no
//! price at all, so the reference is the exact rational zero-size limit; flooring the drift
//! reports a bid above it and an ask below it, i.e. better for the taker on both sides.
//! Ceiling both moves the reported price away from the taker on both sides, by strictly less
//! than one price unit, and makes these helpers *exactly* the zero-size limit of
//! [`avg_bid_price`] / [`avg_ask_price`]. The asymmetry does not change shape; it disappears.
//! `executable_top_is_exactly_the_zero_size_average` states that as an equality.
//!
//! # Domain
//!
//! Solidity works in `uint256`. This port works in `u128` for every value the pool can hold,
//! with a genuine 256-bit intermediate ([`crate::math::U256`]) where the arithmetic requires it.
//! `IPropPool.PairSnapshot` fixes the widths:
//!
//! | quantity                              | on-chain type | bound          |
//! |---------------------------------------|---------------|----------------|
//! | `minBid` / `maxBid` / `minAsk` / `maxAsk` | `uint56`  | [`MAX_PRICE`]  |
//! | `bidCapacity` / `askCapacity` / `bidUsed` / `askUsed` | `uint96` | [`MAX_AMOUNT`] |
//! | `priceScaleExp`                       | `uint8`       | [`MAX_PRICE_SCALE_EXP`] |
//!
//! The overflow argument is re-derived from scratch, because the previous one described
//! arithmetic that no longer exists (it bounded `amountIn * 10**38` at `2^223`; the new
//! numerators reach `2^250`):
//!
//! * `used + q <= C < 2^96`, `checked_add`ed so a caller outside the domain gets
//!   [`CurveError::AmountExceedsCapacity`] rather than a panic — which is also what the chain
//!   decides, since any `uint256` sum exceeding a `uint96` capacity fails the same check.
//! * `2*maxBid*C` and `2*minAsk*C` reach `2 * 2^56 * 2^96 = 2^153`. **Exceeds `u128`.**
//! * `2u + q <= 2C < 2^97`, so `W*(2u + q) < 2^56 * 2^97 = 2^153`. **Exceeds `u128`.**
//! * The bid numerator's second factor is a difference of the two, so `< 2^153`; the ask's is a
//!   sum, so `< 2^154`.
//! * Times `q < 2^96`: the bid numerator is `< 2^249`, the ask's `< 2^250`. Both inside 256
//!   bits with 6 bits of headroom.
//! * The denominator `2*C*S < 2^97 * 10^38 < 2^97 * 2^127 = 2^224`. **Exceeds `u128`.**
//! * The quotients are bounded by `q * maxBid / S < 2^152`, which still exceeds `u128` for small
//!   `S`. That is exactly [`MAX_AMOUNT_OUT`], so a `None` from the divider is the *shared*
//!   [`CurveError::AmountOutOfDomain`], not a port-only refusal.
//! * The bisection bracket seeds multiply a quote amount by `S`: `2^128 * 10^38 < 2^255`. This
//!   is the single tightest intermediate in the library and it is bounded *only* because
//!   [`MAX_AMOUNT_OUT`] caps the quote-denominated argument at `uint128`.
//!
//! Debug builds additionally `debug_assert!` the input bounds so that a caller drifting out of
//! the domain is caught in tests rather than in production.

use crate::error::CurveError;
use crate::math::{mul_div_ceil, mul_div_floor, pow10, U256};

/// Largest representable price. Prices are `uint56` in `IPropPool.PairSnapshot` and in the
/// packed `updateQuote` word.
pub const MAX_PRICE: u128 = (1u128 << 56) - 1;

/// Largest representable amount / capacity. `uint96` in `IPropPool.PairSnapshot`.
pub const MAX_AMOUNT: u128 = (1u128 << 96) - 1;

/// Mirror of `PropCurve.MAX_AMOUNT_OUT` (`type(uint128).max`).
///
/// The ceiling on every quote-denominated amount the library returns or accepts. See the
/// constant's doc comment in `PropCurve.sol`: it exists to make the on-chain and off-chain
/// domains coincide exactly, so a trade the engine cannot represent is one the chain refuses.
pub const MAX_AMOUNT_OUT: u128 = u128::MAX;

/// Mirror of `PropCurve.MAX_PRICE_SCALE_EXP`.
///
/// Divergence, deliberate: `PropCurve`'s quote paths do **not** enforce this — they just
/// evaluate `10 ** priceScaleExp`, which succeeds on chain up to `exp == 77`. This port refuses
/// `exp > 38` with [`CurveError::DomainOverflow`]. `PropPool.addPair` is responsible for
/// enforcing the constant at configuration time; refusing here turns a misconfiguration into a
/// dropped row instead of a divergent quote.
pub const MAX_PRICE_SCALE_EXP: u8 = 38;

/// What [`executable_top_ask`] returns when `askCapacity == 0`.
///
/// Divergence, unavoidable: on chain the function returns `type(uint256).max`, which is not
/// representable in `u128`. `u128::MAX` carries the same meaning — "no ask is executable" — and
/// is documented as such. Any comparison against this value must treat it as an infinity
/// sentinel, never as a price. The generated test vectors emit the true `uint256` decimal for
/// this case so the Solidity differential test still asserts the exact on-chain value.
pub const NO_ASK: u128 = u128::MAX;

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
        validate_ladder(
            self.min_bid,
            self.max_bid,
            self.min_ask,
            self.max_ask,
            min_price,
        )
    }
}

#[inline]
fn in_price_domain(p: u128) -> bool {
    p <= MAX_PRICE
}

#[inline]
fn in_amount_domain(a: u128) -> bool {
    a <= MAX_AMOUNT
}

#[inline]
fn check_exp(price_scale_exp: u8) -> Result<u128, CurveError> {
    // Returned, not asserted. This runs in the quoting hot path, and a panic here takes the
    // updater process down — after which the pool goes stale and stops quoting at all. A bad
    // input must not escalate into a liveness failure; refusing to quote is recoverable.
    if price_scale_exp > MAX_PRICE_SCALE_EXP {
        return Err(CurveError::DomainOverflow);
    }
    pow10(price_scale_exp).ok_or(CurveError::DomainOverflow)
}

// ---------------------------------------------------------------------------
// The two numerators, as 256-bit quotients
// ---------------------------------------------------------------------------
//
// These mirror `PropCurve._bidGross` / `PropCurve._askCost` and, like them, are **uncapped**:
// they return the raw quotient as a `U256`. That is not laziness, it is required. The bisections
// compare a gross against a target that is itself bounded by `MAX_AMOUNT_OUT`, so narrowing here
// would refuse a request the epoch can in fact fill. Solidity gets this for free from `uint256`;
// the port has to be explicit about it.

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
    let span = max_bid
        .checked_sub(min_bid)
        .ok_or(CurveError::ArithmeticPanic)?;
    let gross = U256::mul_u128(max_bid.checked_mul(2).ok_or(CurveError::DomainOverflow)?, c);
    let impact = U256::mul_u128(span, twice_plus(u, q)?);
    let factor = gross
        .checked_sub(impact)
        .ok_or(CurveError::ArithmeticPanic)?;
    let num = factor
        .checked_mul_u128(q)
        .ok_or(CurveError::ArithmeticPanic)?;
    let den = denominator(c, scale)?;
    Ok(num.div_rem(den).ok_or(CurveError::ArithmeticPanic)?.0)
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
    u.checked_mul(2)
        .and_then(|d| d.checked_add(q))
        .ok_or(CurveError::DomainOverflow)
}

/// `2*C*S`, always `>= 2` for a live pair, so no divider ever sees a zero denominator.
#[inline]
fn denominator(c: u128, scale: u128) -> Result<U256, CurveError> {
    Ok(U256::mul_u128(
        c.checked_mul(2).ok_or(CurveError::DomainOverflow)?,
        scale,
    ))
}

/// `ceil(a * b / d)` clamped into `u128`, for the bisection bracket seeds only. Saturating
/// rather than fallible because every use site immediately clamps the seed into `[lo, hi]`, and
/// the clamp is provably a no-op inside the documented domain.
fn seed_ceil(a: u128, b: u128, d: u128) -> u128 {
    crate::math::div_ceil_u256(U256::mul_u128(a, b), U256::from_u128(d)).unwrap_or(u128::MAX)
}

/// `floor(a * b / d)` clamped into `u128`. See [`seed_ceil`].
fn seed_floor(a: u128, b: u128, d: u128) -> u128 {
    crate::math::div_floor_u256(U256::mul_u128(a, b), U256::from_u128(d)).unwrap_or(u128::MAX)
}

// ---------------------------------------------------------------------------
// Bid side — the pool buys base, pays quote
// ---------------------------------------------------------------------------

/// Port of `PropCurve.amountOutBid`. Base in, quote out; the pool buys base. Exact input.
///
/// Rounds down, once, in the pool's favour, exactly as the chain does.
///
/// The `amount_in == 0` early return comes **before** the capacity checks, so a zero-size quote
/// against a zero-capacity pair returns `Ok(0)` rather than [`CurveError::ZeroCapacity`]. That
/// ordering is deliberate on chain and is mirrored here; a generated vector pins it.
///
/// # Errors
/// [`CurveError::ZeroCapacity`], [`CurveError::AmountExceedsCapacity`],
/// [`CurveError::AmountOutOfDomain`], or [`CurveError::DomainOverflow`] (see module docs).
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
    debug_assert!(
        in_price_domain(min_bid) && in_price_domain(max_bid),
        "price outside uint56 domain"
    );
    debug_assert!(
        in_amount_domain(bid_capacity) && in_amount_domain(bid_used),
        "capacity outside uint96 domain"
    );
    let scale = check_exp(price_scale_exp)?;

    if amount_in == 0 {
        return Ok(0);
    }
    if bid_capacity == 0 {
        return Err(CurveError::ZeroCapacity);
    }
    // `bid_used + amount_in` cannot overflow uint256 on chain. A u128 overflow here means the
    // sum certainly exceeds `bid_capacity <= u128::MAX`, which is the same verdict. This check
    // is also what puts the bid parabola's vertex out of reach and keeps the numerator's
    // subtraction underflow-free.
    let consumed = bid_used
        .checked_add(amount_in)
        .ok_or(CurveError::AmountExceedsCapacity)?;
    if consumed > bid_capacity {
        return Err(CurveError::AmountExceedsCapacity);
    }

    bid_gross(amount_in, min_bid, max_bid, bid_capacity, bid_used, scale)?
        .to_u128()
        .ok_or(CurveError::AmountOutOfDomain)
}

/// Port of `PropCurve.amountInBid`. Least base input whose bid quote delivers at least
/// `amount_out` of quote. Exact output.
///
/// Inverts the *integer* function rather than the real-valued radical. [`amount_out_bid`] is
/// non-decreasing in `amount_in` across the whole domain (module docs, unconditionally), so the
/// least `q` with `f(q) >= amount_out` is well defined and the bisection finds it exactly. The
/// answer is rounded up by construction, which is the pool-favourable direction for an
/// exact-output trade.
///
/// # Errors
/// [`CurveError::ZeroCapacity`], [`CurveError::AmountExceedsCapacity`] when the epoch's whole
/// remaining depth cannot deliver `amount_out`, [`CurveError::AmountOutOfDomain`] when
/// `amount_out` is outside the shared domain, or [`CurveError::DomainOverflow`].
pub fn amount_in_bid(
    amount_out: u128,
    min_bid: u128,
    max_bid: u128,
    bid_capacity: u128,
    bid_used: u128,
    price_scale_exp: u8,
) -> Result<u128, CurveError> {
    debug_assert!(
        in_price_domain(min_bid) && in_price_domain(max_bid),
        "price outside uint56 domain"
    );
    debug_assert!(
        in_amount_domain(bid_capacity) && in_amount_domain(bid_used),
        "capacity outside uint96 domain"
    );
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
    // No `amount_out > MAX_AMOUNT_OUT` check: on chain that guards the `amountOut * scale`
    // bracket seed against a `uint256` argument, but here `MAX_AMOUNT_OUT == u128::MAX`, so the
    // argument's own type already enforces it. The domains coincide, which is the whole point of
    // the constant; `amount_out_of_domain_is_a_type_bound_in_the_port` pins the reasoning.

    let target = U256::from_u128(amount_out);
    let mut hi = bid_capacity - bid_used;
    if bid_gross(hi, min_bid, max_bid, bid_capacity, bid_used, scale)? < target {
        return Err(CurveError::AmountExceedsCapacity);
    }

    let mut lo = 1u128;
    // Brackets from the ladder *endpoints*, so they hold regardless of rounding.
    // `f(q) <= floor(q*maxBid/S)`, so `f(q) >= y` forces `q >= ceil(y*S/maxBid)`: a lower bound.
    if max_bid != 0 {
        let seed = seed_ceil(amount_out, scale, max_bid);
        if seed > lo && seed <= hi {
            lo = seed;
        }
    }
    // `f(q) >= floor(q*minBid/S)`, so `q = ceil(y*S/minBid)` already satisfies `f(q) >= y`: an
    // upper bound, i.e. a point known to be feasible.
    if min_bid != 0 {
        let seed = seed_ceil(amount_out, scale, min_bid);
        if seed >= lo && seed < hi {
            hi = seed;
        }
    }

    // Two-sided fixed-point refinement. Mirrors `PropCurve.amountInBid`; see the derivation
    // there. Both ends stay valid bracket ends at every step, so the bisection below is exact
    // regardless of how well this converges — three rounds is a gas constant on chain, not a
    // correctness parameter, and this port must run the same number of them to stay bit-exact.
    let span = max_bid - min_bid; // checked above, inside `bid_gross`
    let den = bid_capacity
        .checked_mul(2)
        .ok_or(CurveError::DomainOverflow)?;
    for _ in 0..3 {
        if lo >= hi {
            break;
        }
        // ceil the price = floor the drift: `>= p(lo)`.
        let drift = mul_div_floor(span, twice_plus(bid_used, lo)?, den)
            .ok_or(CurveError::DomainOverflow)?;
        let p = max_bid
            .checked_sub(drift)
            .ok_or(CurveError::ArithmeticPanic)?;
        if p != 0 {
            let seed = seed_ceil(amount_out, scale, p).min(hi);
            if seed > lo {
                lo = seed;
            }
        }
        // floor the price = ceil the drift: `<= p(hi)`.
        let drift =
            mul_div_ceil(span, twice_plus(bid_used, hi)?, den).ok_or(CurveError::DomainOverflow)?;
        let p = max_bid
            .checked_sub(drift)
            .ok_or(CurveError::ArithmeticPanic)?;
        if p != 0 {
            let seed = seed_ceil(amount_out, scale, p);
            if seed < hi && seed >= lo {
                hi = seed;
            }
        }
    }

    // Least-true bisection. Invariant: `f(hi) >= y` and `f(lo - 1) < y`.
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if bid_gross(mid, min_bid, max_bid, bid_capacity, bid_used, scale)? >= target {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    Ok(hi)
}

// ---------------------------------------------------------------------------
// Ask side — the pool sells base, collects quote
// ---------------------------------------------------------------------------

/// Port of `PropCurve.amountInAsk`. Quote cost of `amount_out` base. Exact output.
///
/// This is the ask side's *primitive*: base in the capacity axis, quote cost out. It is the
/// direction that keeps the curve linear in the price; amendment 3 in the module docs explains
/// why the old quote-denominated parameterisation was convex and therefore split-dominated.
///
/// Rounds up, once, in the pool's favour.
///
/// # Errors
/// [`CurveError::ZeroCapacity`], [`CurveError::AmountExceedsCapacity`],
/// [`CurveError::ZeroPrice`] (only for a wholly zero ask ladder),
/// [`CurveError::AmountOutOfDomain`], or [`CurveError::DomainOverflow`].
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
    debug_assert!(
        in_price_domain(min_ask) && in_price_domain(max_ask),
        "price outside uint56 domain"
    );
    debug_assert!(
        in_amount_domain(ask_capacity) && in_amount_domain(ask_used),
        "capacity outside uint96 domain"
    );
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
    // `maxAsk >= minAsk`, so this is exactly "the ask ladder prices nothing". Any ladder
    // `validate_ladder` accepts has `maxAsk > minBid >= 0`, hence `maxAsk >= 1`, so this is
    // unreachable through `PropPool` — the same reachability the pre-amendment `ZeroPrice` had,
    // which also fired only on an all-zero ask ladder. Without it the pool hands over base for
    // nothing.
    if max_ask == 0 {
        return Err(CurveError::ZeroPrice);
    }

    ask_cost(amount_out, min_ask, max_ask, ask_capacity, ask_used, scale)?
        .to_u128()
        .ok_or(CurveError::AmountOutOfDomain)
}

/// Port of `PropCurve.amountOutAsk`. Most base that `amount_in` of quote strictly pays for.
/// Exact input, quote-denominated — the taker-facing ask path.
///
/// The inversion of [`amount_in_ask`]. Over the reals this is the positive root of
/// `W*q^2 + (2*minAsk*C + 2*W*u)*q - 2*C*S*X = 0`, but the radical is unusable in a word:
/// `b^2` reaches `2^308` and `2*C*S*X` reaches `2^352`, so a closed form needs 512-bit
/// intermediates *and* a 512-bit square root, and it would invert the real curve rather than the
/// integer one. This bisects the integer function instead, which is exact: `ask_cost` is
/// strictly increasing in `q`, and `ceil(y) <= X` iff `y <= X` for integral `X`, so the integer
/// predicate *is* the rational one.
///
/// Rounding: the answer is the largest `q` the quote covers, so the taker never receives more
/// base than `amount_in` strictly pays for. Any unspent remainder stays with the pool. Because
/// the curve is additive and this is its exact inverse, the result is super-additive in
/// `amount_in`: splitting a quote-denominated ask can never buy more base.
///
/// # Errors
/// [`CurveError::ZeroCapacity`], [`CurveError::AmountExceedsCapacity`] when `amount_in` exceeds
/// the quote cost of the epoch's whole remaining base, [`CurveError::ZeroPrice`],
/// [`CurveError::AmountOutOfDomain`], or [`CurveError::DomainOverflow`].
pub fn amount_out_ask(
    amount_in: u128,
    min_ask: u128,
    max_ask: u128,
    ask_capacity: u128,
    ask_used: u128,
    price_scale_exp: u8,
) -> Result<u128, CurveError> {
    debug_assert!(
        in_price_domain(min_ask) && in_price_domain(max_ask),
        "price outside uint56 domain"
    );
    debug_assert!(
        in_amount_domain(ask_capacity) && in_amount_domain(ask_used),
        "capacity outside uint96 domain"
    );
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
    // On chain there is an `amountIn > MAX_AMOUNT_OUT` check here — an *input* bound, because
    // the bracket seeds multiply the argument by `10**38`. It is unrepresentable as a check in
    // this port: `MAX_AMOUNT_OUT == u128::MAX`, so the argument's type already enforces it.

    let room = ask_capacity - ask_used;
    let budget = U256::from_u128(amount_in);
    // The epoch's quote-denominated ceiling is the cost of all its remaining base. Raising
    // `AmountExceedsCapacity` above it preserves the pre-amendment contract for a
    // quote-denominated `amountIn`, and avoids silently charging more than the base is worth.
    let full = ask_cost(room, min_ask, max_ask, ask_capacity, ask_used, scale)?;
    if budget > full {
        return Err(CurveError::AmountExceedsCapacity);
    }
    if budget == full {
        return Ok(room);
    }

    // `cost(room) > amount_in`, so the answer is in `[0, room - 1]`.
    let mut lo = 0u128;
    let mut hi = room - 1;
    // `cost(q) <= ceil(q*maxAsk/S)`, so every `q <= floor(X*S/maxAsk)` is affordable: a lower
    // bound. It cannot exceed `hi` — affordability is monotone and `room` is not affordable —
    // so the `min` is provably a no-op in domain and exists only to keep this total.
    let seed = seed_floor(amount_in, scale, max_ask).min(hi);
    if seed > lo {
        lo = seed;
    }
    // `cost(q) >= q*minAsk/S`, so affordability forces `q <= floor(X*S/minAsk)`: an upper bound.
    if min_ask != 0 {
        let seed = seed_floor(amount_in, scale, min_ask);
        if seed < hi && seed >= lo {
            hi = seed;
        }
    }

    // Two-sided fixed-point refinement. Mirrors `PropCurve.amountOutAsk`; see the derivation
    // there. Without it the endpoint brackets are a *relative* interval and the bisection costs
    // ~53 iterations on a 20 bps ladder — about 40k gas on the primary ask swap path. Both ends
    // stay valid at every step, so exactness does not depend on convergence.
    let span = max_ask - min_ask; // checked above, inside `ask_cost`
    let den = ask_capacity
        .checked_mul(2)
        .ok_or(CurveError::DomainOverflow)?;
    for _ in 0..3 {
        if lo >= hi {
            break;
        }
        // floor the price: `<= p(lo)`, which makes `X*S / p` an upper bound on the answer.
        let drift = mul_div_floor(span, twice_plus(ask_used, lo)?, den)
            .ok_or(CurveError::DomainOverflow)?;
        let p = min_ask
            .checked_add(drift)
            .ok_or(CurveError::DomainOverflow)?;
        if p != 0 {
            let seed = seed_floor(amount_in, scale, p);
            if seed < hi {
                hi = seed;
            }
        }
        // ceil the price: `>= p(hi)`, which makes every `q <= X*S / p` affordable.
        let drift =
            mul_div_ceil(span, twice_plus(ask_used, hi)?, den).ok_or(CurveError::DomainOverflow)?;
        let p = min_ask
            .checked_add(drift)
            .ok_or(CurveError::DomainOverflow)?;
        if p != 0 {
            let seed = seed_floor(amount_in, scale, p).min(hi);
            if seed > lo {
                lo = seed;
            }
        }
    }

    // Greatest-true bisection. Invariant: `cost(lo) <= X < cost(hi + 1)`.
    while lo < hi {
        // The UPPER midpoint, `lo + ceil((hi - lo) / 2)` — written as Solidity writes it,
        // `lo + (hi - lo + 1) / 2`. A floored midpoint would stall at `hi == lo + 1`.
        let mid = lo + (hi - lo).div_ceil(2);
        if ask_cost(mid, min_ask, max_ask, ask_capacity, ask_used, scale)? <= budget {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    Ok(lo)
}

// ---------------------------------------------------------------------------
// Average price — reporting only
// ---------------------------------------------------------------------------
//
// These have NO counterpart in the quote path any more. Amendment 4 removed the intermediate
// price entirely, so there is no integer `avgBid` for the chain to compute and none for this to
// mirror. What is left is the exact rational average over `[u, u+q]`, which the inverse solver
// targets and the update policy (archi_v2 §5.3) reports — rounded, in each case, so that the
// reported price is never better for the taker than the interval actually executes.

/// The average bid price over `[used, used + q]`, floored:
/// `maxBid - ceil(W * (2*used + q) / (2*C))`.
///
/// Floored, i.e. rounded *down*, because a lower bid is worse for the taker: the reported
/// average is never above what the interval actually pays. The exact rational average lies in
/// `[result, result + 1)`.
///
/// # Errors
/// [`CurveError::ZeroCapacity`] on zero capacity, [`CurveError::ArithmeticPanic`] if
/// `maxBid < minBid` or the ladder is otherwise out of range.
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
    max_bid
        .checked_sub(discount)
        .ok_or(CurveError::ArithmeticPanic)
}

/// The average ask price over `[used, used + q]`, ceiled:
/// `minAsk + ceil(W * (2*used + q) / (2*C))`.
///
/// Ceiled, i.e. rounded *up*, because a higher ask is worse for the taker. The exact rational
/// average lies in `(result - 1, result]`.
///
/// # Errors
/// [`CurveError::ZeroCapacity`] on zero capacity, [`CurveError::ArithmeticPanic`] if
/// `maxAsk < minAsk`.
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
    min_ask
        .checked_add(premium)
        .ok_or(CurveError::DomainOverflow)
}

// ---------------------------------------------------------------------------
// Ladder validation
// ---------------------------------------------------------------------------

/// Port of `PropCurve.validateLadder`.
///
/// Accepts exactly `maxAsk >= minAsk >= maxBid >= minBid >= minPrice` **and** the strict
/// `maxAsk > minBid`. The strict comparison is easy to miss: an entirely flat ladder (all four
/// prices equal) is *rejected*, so the off-chain builder must always leave at least one price
/// unit between `minBid` and `maxAsk`. It is also what makes [`CurveError::ZeroPrice`]
/// unreachable on the ask side, by forcing `maxAsk >= 1`.
///
/// Check order matters for which error surfaces: the floor check runs first.
///
/// # Errors
/// [`CurveError::BidBelowMinPrice`] or [`CurveError::CrossedBook`].
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
    if !(max_ask >= min_ask && min_ask >= max_bid && max_bid >= min_bid) {
        return Err(CurveError::CrossedBook);
    }
    if max_ask <= min_bid {
        return Err(CurveError::CrossedBook);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Introspection
// ---------------------------------------------------------------------------

/// Port of `PropCurve.executableTopBid`.
///
/// The price a taker receives right now for an infinitesimal bid. This — not `maxBid` — is what
/// the update policy compares against (archi_v2 §5.3), because once the epoch is partly consumed
/// the executable top has already moved.
///
/// CEILS the drift, so the reported bid is at or below the exact rational zero-size limit
/// `maxBid - W*used/C`: never better for the taker than the book executes. This is exactly
/// `avg_bid_price(min_bid, max_bid, capacity, used, 0)`.
///
/// # Errors
/// [`CurveError::ArithmeticPanic`] if `maxBid < minBid` (`Panic(0x11)` on chain).
pub fn executable_top_bid(
    min_bid: u128,
    max_bid: u128,
    bid_capacity: u128,
    bid_used: u128,
) -> Result<u128, CurveError> {
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
    max_bid
        .checked_sub(drift)
        .ok_or(CurveError::ArithmeticPanic)
}

/// Port of `PropCurve.executableTopAsk`.
///
/// CEILS the drift, for the same reason and in the same direction as [`executable_top_bid`]:
/// away from the taker. This is exactly `avg_ask_price(min_ask, max_ask, capacity, used, 0)`.
///
/// Returns [`NO_ASK`] when `askCapacity == 0`. See that constant for the one place this port
/// cannot be bit-identical to `uint256`.
///
/// # Errors
/// [`CurveError::ArithmeticPanic`] if `maxAsk < minAsk`.
pub fn executable_top_ask(
    min_ask: u128,
    max_ask: u128,
    ask_capacity: u128,
    ask_used: u128,
) -> Result<u128, CurveError> {
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
    min_ask.checked_add(drift).ok_or(CurveError::DomainOverflow)
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
        // Consuming the whole 100-unit ladder from 2_000 down to 1_000 must charge 1_500.
        assert_eq!(
            amount_out_bid(100, 1_000, 2_000, 100, 0, 0),
            Ok(100 * 1_500)
        );
        // Consuming the first half charges the midpoint of the first half: 1_750.
        assert_eq!(amount_out_bid(50, 1_000, 2_000, 100, 0, 0), Ok(50 * 1_750));
        // ... and the second half charges 1_250. Halves sum to the whole exactly.
        assert_eq!(amount_out_bid(50, 1_000, 2_000, 100, 50, 0), Ok(50 * 1_250));
        assert_eq!(50 * 1_750 + 50 * 1_250, 100 * 1_500);
    }

    #[test]
    fn ask_is_the_mirror_of_the_bid_now_that_both_are_base_denominated() {
        // The same ladder walked upward: 100 base costs the 1_500 midpoint, not `100 / 1_500`.
        assert_eq!(amount_in_ask(100, 1_000, 2_000, 100, 0, 0), Ok(100 * 1_500));
        assert_eq!(amount_in_ask(50, 1_000, 2_000, 100, 0, 0), Ok(50 * 1_250));
        assert_eq!(amount_in_ask(50, 1_000, 2_000, 100, 50, 0), Ok(50 * 1_750));
        // And the inversion recovers the size exactly when the budget is exact.
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

    /// DEFECT 2's witness, reproduced in the units the Foundry test used. An 18/6 pair at price
    /// 3e9 (`priceScaleExp = 12`... here 24 with the ladder scaled), 100 bps of ladder, 30 base
    /// of capacity: one extra wei of input used to return LESS quote.
    #[test]
    fn one_extra_wei_never_returns_less() {
        let price = 3_000_000_000u128;
        let (min_bid, max_bid) = (price - price / 100, price);
        let cap = 30_000_000_000_000_000_000u128;
        let a = amount_out_bid(2_000_000_000_000_000_000, min_bid, max_bid, cap, 0, 12).unwrap();
        let b = amount_out_bid(2_000_000_000_000_000_001, min_bid, max_bid, cap, 0, 12).unwrap();
        assert!(b >= a, "{b} < {a}: monotonicity broken again");
        // And exhaustively across the wei boundary that used to trip it.
        let mut prev = 0u128;
        for q in (0..40u128).map(|i| 2_000_000_000_000_000_000 - 20 + i) {
            let out = amount_out_bid(q, min_bid, max_bid, cap, 0, 12).unwrap();
            assert!(out >= prev, "q={q} out={out} prev={prev}");
            prev = out;
        }
    }

    #[test]
    fn splitting_an_ask_never_beats_executing_it_whole() {
        // The Jensen defect, in the shape that reproduced it: a wide ladder and a big trade.
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
            amount_in_ask(MAX_AMOUNT, 0, 0, MAX_AMOUNT, 0, E),
            Err(CurveError::ZeroPrice)
        );
        // One unit of span is enough to price the interval: ceil lifts any nonzero cost to 1.
        assert_eq!(amount_in_ask(1, 0, 1, 1_000_000, 0, E), Ok(1));
        assert_eq!(amount_in_ask(1, 0, MAX_PRICE, MAX_AMOUNT, 0, 0), Ok(1));
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
        // The epoch's whole remaining base costs ~1_005_000 quote here, so every budget below
        // must sit under that; `ask_inversion_reverts_above_the_epoch_ceiling` covers the rest.
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
        // The amended CEIL, on both sides: a drift of 1/3 of a unit reports the worse price.
        assert_eq!(executable_top_ask(1_000, 1_001, 3, 1), Ok(1_001));
        assert_eq!(executable_top_bid(1_000, 1_001, 3, 1), Ok(1_000));
        // ... and each helper is exactly the zero-size limit of its average-price counterpart.
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
                                        // The ask leg of the same flat ladder costs the same 3_000 USDC for 1 WETH.
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
        assert!(px <= MAX_PRICE);
        assert_eq!(
            amount_out_bid(100_000_000, px, px, 1_000_000_000, 0, 13),
            Ok(60_000_000_000)
        );
    }

    #[test]
    fn realistic_usdc_weth_6_18() {
        let px = 33_333_333_333_333_300u128;
        assert!(px <= MAX_PRICE);
        assert_eq!(
            amount_out_bid(1_000_000, px, px, 1_000_000_000_000, 0, 8),
            Ok(333_333_333_333_333)
        );
    }

    #[test]
    fn domain_edges_are_reported_not_wrapped() {
        // exp = 0 at the corner: `q * maxBid` is ~2^152, above `MAX_AMOUNT_OUT`. Shared revert.
        assert_eq!(
            amount_out_bid(MAX_AMOUNT, MAX_PRICE, MAX_PRICE, MAX_AMOUNT, 0, 0),
            Err(CurveError::AmountOutOfDomain)
        );
        assert_eq!(
            amount_in_ask(MAX_AMOUNT, MAX_PRICE, MAX_PRICE, MAX_AMOUNT, 0, 0),
            Err(CurveError::AmountOutOfDomain)
        );
        // One unit under the boundary on the same shape: avgBid == 1, so out == q < 2^96.
        assert_eq!(
            amount_out_bid(MAX_AMOUNT, 1, 1, MAX_AMOUNT, 0, 0),
            Ok(MAX_AMOUNT)
        );
        // exp above MAX_PRICE_SCALE_EXP: still port-only, and it is checked first.
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
        // Flat ladder: exact, no rounding to observe.
        assert_eq!(avg_bid_price(1_000, 1_000, 100, 0, 100), Ok(1_000));
        // Half-consumed 1_000..2_000 ladder over [0, 100]: exact midpoint 1_500.
        assert_eq!(avg_bid_price(1_000, 2_000, 100, 0, 100), Ok(1_500));
        assert_eq!(avg_ask_price(1_000, 2_000, 100, 0, 100), Ok(1_500));
        // A ladder whose midpoint is not representable: bid floors, ask ceils.
        assert_eq!(avg_bid_price(1_000, 1_001, 3, 0, 1), Ok(1_000));
        assert_eq!(avg_ask_price(1_000, 1_001, 3, 0, 1), Ok(1_001));
    }
}
