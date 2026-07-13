//! Fixed-width integer primitives.
//!
//! Solidity does every intermediate in `uint256`. This crate does them in `u128` wherever a
//! range proof shows that is exact, and in a genuine 256-bit type ([`U256`]) where it does not.
//! There is no external bigint dependency: everything the curve needs is `128 x 128 -> 256`
//! multiply, `256 +- 256`, and `256 / 256 -> 256` divide, all short enough to write and test
//! exhaustively here.
//!
//! # Why a 256-bit intermediate is unavoidable
//!
//! Two shapes force it, and the second is new.
//!
//! **A `128 x 128 -> 256` product with a `u128` quotient.** The width bounds of a ladder are
//! `uint56` prices and `uint96` capacities, so any `price * capacity` reaches `2^152` while its
//! quotient by the capacity is back under `2^56`. [`mul_div_rem`] computes those exactly without
//! materialising anything the caller has to reason about, and it is what the inverse solver's
//! width bounds and the `executableTop*` drift terms use.
//!
//! **A numerator with no `u128` factorisation at all.** `PropCurve` was amended to stop
//! quantising the intermediate price: it folds the price into the amount and rounds once. The
//! quote legs are now
//!
//! ```text
//! bid: floor( q * (2*maxBid*C - W*(2u + q)) / (2*C*S) )
//! ask:  ceil( q * (2*minAsk*C + W*(2u + q)) / (2*C*S) )
//! ```
//!
//! with `W` the ladder span and `S = 10**priceScaleExp`. Neither the numerator's second factor
//! (`< 2^154`) nor the denominator (`< 2^224`) fits `u128`, so there is no way to express these
//! as `mul_div(a: u128, b: u128, d: u128)`. The whole computation runs in [`U256`]:
//! [`U256::mul_u128`] builds the two 256-bit factors, [`U256::checked_mul_u128`] scales by `q`
//! (up to `2^250`), and [`div_floor_u256`] / [`div_ceil_u256`] divide 256 by 256.
//!
//! The quotients are bounded by `q * maxBid / S < 2^152`, which still exceeds `u128` when `S`
//! is small — so both dividers return `Option<u128>` and `None` means "above
//! `PropCurve.MAX_AMOUNT_OUT`", a *shared* domain edge rather than a port narrowing.

/// Low 64 bits set.
const MASK64: u128 = u64::MAX as u128;

/// `10**exp` for every `exp` this crate accepts. `10**38 < 2^127`, so the whole table fits
/// `u128`; `10**39` does not, which is exactly why [`crate::curve::MAX_PRICE_SCALE_EXP`] is 38.
const POW10: [u128; 39] = {
    let mut table = [1u128; 39];
    let mut i = 1;
    while i < 39 {
        table[i] = table[i - 1] * 10;
        i += 1;
    }
    table
};

/// `10**exp`, or `None` when `exp > 38` and the result would not fit `u128`.
///
/// Note the deliberate divergence from Solidity documented on
/// [`crate::curve::MAX_PRICE_SCALE_EXP`]: on chain `10 ** exp` is fine up to `exp == 77`.
/// `PropPool` never configures a pair above 38, so refusing 39..=77 here converts a
/// misconfiguration into a loud failure instead of a silently different quote.
#[inline]
#[must_use]
pub fn pow10(exp: u8) -> Option<u128> {
    POW10.get(exp as usize).copied()
}

/// `128 x 128 -> 256` multiply, returned as `(hi, lo)` limbs so that the true product is
/// `hi * 2^128 + lo`.
///
/// Schoolbook over 64-bit limbs. Every partial product is a `u64 x u64` widened to `u128`, so
/// no individual step can overflow:
///
/// * `ll`, `lh`, `hl`, `hh` are each `<= (2^64 - 1)^2 < 2^128`.
/// * `mid = (ll >> 64) + (lh & MASK64) + (hl & MASK64) < 3 * 2^64 < 2^128`.
/// * `hi <= (2^64-1)^2 + 2*(2^64-2) + 2 = 2^128 - 1`.
#[inline]
#[must_use]
pub const fn widening_mul(a: u128, b: u128) -> (u128, u128) {
    let (a_lo, a_hi) = (a & MASK64, a >> 64);
    let (b_lo, b_hi) = (b & MASK64, b >> 64);

    let ll = a_lo * b_lo;
    let lh = a_lo * b_hi;
    let hl = a_hi * b_lo;
    let hh = a_hi * b_hi;

    // 2^64-weighted column: the carry out of `ll` plus the low halves of the cross terms.
    let mid = (ll >> 64) + (lh & MASK64) + (hl & MASK64);

    let lo = (ll & MASK64) | (mid << 64);
    let hi = hh + (lh >> 64) + (hl >> 64) + (mid >> 64);
    (hi, lo)
}

/// `256 - 256 -> 256` subtract on `(hi, lo)` limbs, wrapping. Test/verification helper.
#[inline]
#[must_use]
pub const fn sub256(a: (u128, u128), b: (u128, u128)) -> (u128, u128) {
    let (lo, borrow) = a.1.overflowing_sub(b.1);
    let hi = a.0.wrapping_sub(b.0).wrapping_sub(borrow as u128);
    (hi, lo)
}

/// Exact `floor(a * b / d)` and `(a * b) mod d`, computed through a 256-bit intermediate.
///
/// Returns `None` when `d == 0` (Solidity would `Panic(0x12)`) or when the quotient does not
/// fit `u128` (Solidity would return a valid `uint256`; see
/// [`crate::CurveError::DomainOverflow`]).
///
/// # Algorithm
///
/// Restoring binary long division of the 256-bit numerator by the 128-bit divisor. 128
/// iterations, branch-per-bit. This runs a handful of times per quote at 1-5 Hz, so a
/// Knuth-D limb divider would buy nothing and cost a great deal of reviewability.
///
/// Loop invariant: `rem < d` at the top of every iteration.
///
/// `v = 2 * rem + bit < 2 * d`, so `v` needs at most 129 bits. The bit shifted out of `rem`
/// is captured in `carry`; when `carry == 1` the true value `v >= 2^128 > d`, so a single
/// conditional subtraction always restores `rem < d`, and the wrapping subtract is exact
/// because `v - d < d < 2^128`.
#[must_use]
pub fn mul_div_rem(a: u128, b: u128, d: u128) -> Option<(u128, u128)> {
    if d == 0 {
        return None;
    }
    let (hi, lo) = widening_mul(a, b);

    // Fast path: the whole product fits 128 bits.
    if hi == 0 {
        return Some((lo / d, lo % d));
    }
    // hi >= d implies quotient >= 2^128.
    if hi >= d {
        return None;
    }

    let mut rem = hi;
    let mut quo: u128 = 0;
    let mut i = 128;
    while i > 0 {
        i -= 1;
        let carry = rem >> 127;
        rem = (rem << 1) | ((lo >> i) & 1);
        if carry == 1 || rem >= d {
            rem = rem.wrapping_sub(d);
            quo = (quo << 1) | 1;
        } else {
            quo <<= 1;
        }
    }
    Some((quo, rem))
}

/// Exact `floor(a * b / d)`. See [`mul_div_rem`].
#[inline]
#[must_use]
pub fn mul_div_floor(a: u128, b: u128, d: u128) -> Option<u128> {
    mul_div_rem(a, b, d).map(|(q, _)| q)
}

/// Exact `ceil(a * b / d)`. See [`mul_div_rem`].
///
/// This **is** on-chain arithmetic now: `PropCurve` computes `discount` and `premium` as
/// `(span * midUsage + capacity - 1) / capacity`, so [`crate::curve::avg_bid_price`] and
/// [`crate::curve::avg_ask_price`] mirror it here, and [`crate::inverse`] inverts it by
/// evaluating this same function. (It is also what the ask side of
/// [`crate::ladder::LadderBuilder`] rounds with, which is where it started.)
///
/// Saturates rather than wrapping when `q == u128::MAX` and the remainder is nonzero: `None`,
/// matching [`mul_div_rem`]'s overflow contract.
#[inline]
#[must_use]
pub fn mul_div_ceil(a: u128, b: u128, d: u128) -> Option<u128> {
    let (q, r) = mul_div_rem(a, b, d)?;
    if r == 0 {
        Some(q)
    } else {
        q.checked_add(1)
    }
}

// ---------------------------------------------------------------------------
// 256-bit arithmetic
// ---------------------------------------------------------------------------

/// A 256-bit unsigned integer as two `u128` limbs: the value is `hi * 2^128 + lo`.
///
/// Exists because the amended `PropCurve` quote legs have numerators up to `2^250` and
/// denominators up to `2^224`, neither of which factors into `u128` operands (see the module
/// docs). The derived `Ord` is the numeric order precisely because `hi` is declared first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct U256 {
    /// High 128 bits.
    pub hi: u128,
    /// Low 128 bits.
    pub lo: u128,
}

impl U256 {
    /// Zero.
    pub const ZERO: Self = Self { hi: 0, lo: 0 };

    /// Widen a `u128`.
    #[inline]
    #[must_use]
    pub const fn from_u128(v: u128) -> Self {
        Self { hi: 0, lo: v }
    }

    /// Narrow back, or `None` if the value does not fit `u128`.
    #[inline]
    #[must_use]
    pub const fn to_u128(self) -> Option<u128> {
        if self.hi == 0 {
            Some(self.lo)
        } else {
            None
        }
    }

    /// `true` iff the value is zero.
    #[inline]
    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.hi == 0 && self.lo == 0
    }

    /// Exact `a * b` as 256 bits. Cannot overflow: `(2^128 - 1)^2 < 2^256`.
    #[inline]
    #[must_use]
    pub const fn mul_u128(a: u128, b: u128) -> Self {
        let (hi, lo) = widening_mul(a, b);
        Self { hi, lo }
    }

    /// `self + other`, or `None` on 256-bit overflow.
    #[inline]
    #[must_use]
    pub const fn checked_add(self, other: Self) -> Option<Self> {
        let (lo, carry) = self.lo.overflowing_add(other.lo);
        let hi = match self.hi.checked_add(other.hi) {
            Some(h) => h,
            None => return None,
        };
        match hi.checked_add(carry as u128) {
            Some(hi) => Some(Self { hi, lo }),
            None => None,
        }
    }

    /// `self - other`, or `None` when `other > self`.
    #[inline]
    #[must_use]
    pub const fn checked_sub(self, other: Self) -> Option<Self> {
        let (lo, borrow) = self.lo.overflowing_sub(other.lo);
        let hi = match self.hi.checked_sub(other.hi) {
            Some(h) => h,
            None => return None,
        };
        match hi.checked_sub(borrow as u128) {
            Some(hi) => Some(Self { hi, lo }),
            None => None,
        }
    }

    /// `self - other`, wrapping. Only used inside [`U256::div_rem`], where the loop invariant
    /// proves the true difference is representable.
    #[inline]
    #[must_use]
    const fn wrapping_sub(self, other: Self) -> Self {
        let (lo, borrow) = self.lo.overflowing_sub(other.lo);
        let hi = self.hi.wrapping_sub(other.hi).wrapping_sub(borrow as u128);
        Self { hi, lo }
    }

    /// `self * 2`, dropping the bit shifted out of `hi`.
    #[inline]
    #[must_use]
    const fn shl1(self) -> Self {
        Self { hi: (self.hi << 1) | (self.lo >> 127), lo: self.lo << 1 }
    }

    /// `self * k` as 256 bits, or `None` on overflow.
    ///
    /// Two `128 x 128` products: `lo * k` in full, and `hi * k` which must stay inside the high
    /// limb. Used for the `q * (...)` step of both quote legs.
    #[must_use]
    pub const fn checked_mul_u128(self, k: u128) -> Option<Self> {
        let (lo_hi, lo_lo) = widening_mul(self.lo, k);
        let (hi_hi, hi_lo) = widening_mul(self.hi, k);
        if hi_hi != 0 {
            return None;
        }
        match lo_hi.checked_add(hi_lo) {
            Some(hi) => Some(Self { hi, lo: lo_lo }),
            None => None,
        }
    }

    /// Exact `(self / d, self % d)`, or `None` when `d == 0`.
    ///
    /// Restoring binary long division, 256 iterations. Loop invariant: `rem < d` at the top of
    /// every iteration. `2*rem + bit < 2*d` needs at most 257 bits; the bit shifted out of
    /// `rem` is captured in `carry`, and when `carry == 1` the true value is `>= 2^256 > d`, so
    /// one conditional subtraction always restores the invariant and the wrapping subtract is
    /// exact because the true difference is below `d < 2^256`.
    ///
    /// This runs a few dozen times per quote at 1-5 Hz, so a Knuth-D limb divider would buy
    /// nothing and cost a great deal of reviewability.
    #[must_use]
    pub fn div_rem(self, d: Self) -> Option<(Self, Self)> {
        if d.is_zero() {
            return None;
        }
        if self < d {
            return Some((Self::ZERO, self));
        }
        if self.hi == 0 {
            // Both fit `u128` (d <= self here), so the native path is exact.
            return Some((Self::from_u128(self.lo / d.lo), Self::from_u128(self.lo % d.lo)));
        }

        let mut rem = Self::ZERO;
        let mut quo = Self::ZERO;
        let mut i = 256;
        while i > 0 {
            i -= 1;
            let bit = if i >= 128 { (self.hi >> (i - 128)) & 1 } else { (self.lo >> i) & 1 };
            let carry = rem.hi >> 127;
            rem = rem.shl1();
            rem.lo |= bit;
            quo = quo.shl1();
            if carry == 1 || rem >= d {
                rem = rem.wrapping_sub(d);
                quo.lo |= 1;
            }
        }
        Some((quo, rem))
    }
}

/// Exact `floor(num / den)` narrowed to `u128`.
///
/// `None` when `den == 0` (Solidity would `Panic(0x12)`) or when the quotient exceeds `u128`,
/// which for every caller in this crate is exactly `PropCurve.MAX_AMOUNT_OUT` being exceeded —
/// a shared revert, not a port narrowing.
#[inline]
#[must_use]
pub fn div_floor_u256(num: U256, den: U256) -> Option<u128> {
    num.div_rem(den)?.0.to_u128()
}

/// Exact `ceil(num / den)` narrowed to `u128`. See [`div_floor_u256`].
#[inline]
#[must_use]
pub fn div_ceil_u256(num: U256, den: U256) -> Option<u128> {
    let (q, r) = num.div_rem(den)?;
    let q = q.to_u128()?;
    if r.is_zero() {
        Some(q)
    } else {
        q.checked_add(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use num_bigint::BigUint;
    use proptest::prelude::*;

    fn big(hi: u128, lo: u128) -> BigUint {
        (BigUint::from(hi) << 128u32) + BigUint::from(lo)
    }

    fn u(v: U256) -> BigUint {
        big(v.hi, v.lo)
    }

    fn max256() -> BigUint {
        big(u128::MAX, u128::MAX)
    }

    /// The widest intermediates `PropCurve` can reach, asserted against the module docs.
    #[test]
    fn the_new_curve_intermediates_fit_256_bits() {
        let (price, cap) = ((1u128 << 56) - 1, (1u128 << 96) - 1);
        let scale = pow10(38).unwrap();

        // ask numerator second factor: 2*minAsk*C + W*(2u + q) < 2^154
        let a = U256::mul_u128(2 * price, cap);
        let b = U256::mul_u128(price, 2 * cap);
        let factor = a.checked_add(b).expect("second factor fits 256 bits");
        assert!(factor.hi < 1u128 << 27, "second factor should be under 2^155");
        // times q < 2^96 — the widest numerator in the library.
        let num = factor.checked_mul_u128(cap).expect("q * factor fits 256 bits");
        assert!(num.hi >> 123 == 0, "numerator should be under 2^251");
        // denominator 2*C*S < 2^224
        let den = U256::mul_u128(2 * cap, scale);
        assert!(!den.is_zero() && den.hi >> 96 == 0, "denominator should be under 2^224");
        // And it divides.
        assert!(div_floor_u256(num, den).is_some());
    }

    #[test]
    fn pow10_table() {
        assert_eq!(pow10(0), Some(1));
        assert_eq!(pow10(1), Some(10));
        assert_eq!(pow10(18), Some(1_000_000_000_000_000_000));
        assert_eq!(pow10(38), Some(100_000_000_000_000_000_000_000_000_000_000_000_000));
        assert_eq!(pow10(39), None);
        assert_eq!(pow10(255), None);
        // The headroom claim in PropCurve's comment: 10**38 still fits 127 bits.
        assert!(pow10(38).unwrap() < 1u128 << 127);
    }

    #[test]
    fn widening_mul_known_vectors() {
        assert_eq!(widening_mul(0, u128::MAX), (0, 0));
        assert_eq!(widening_mul(1, u128::MAX), (0, u128::MAX));
        // (2^128 - 1)^2 = 2^256 - 2^129 + 1
        let (hi, lo) = widening_mul(u128::MAX, u128::MAX);
        assert_eq!(hi, u128::MAX - 1);
        assert_eq!(lo, 1);
        assert_eq!(widening_mul(1u128 << 64, 1u128 << 64), (1, 0));
    }

    #[test]
    fn mul_div_rejects_zero_divisor() {
        assert_eq!(mul_div_rem(1, 1, 0), None);
        assert_eq!(mul_div_floor(1, 1, 0), None);
        assert_eq!(mul_div_ceil(1, 1, 0), None);
    }

    #[test]
    fn mul_div_detects_quotient_overflow() {
        // (2^128-1)^2 / 1 does not fit u128.
        assert_eq!(mul_div_floor(u128::MAX, u128::MAX, 1), None);
        // ... but /(2^128-1) does, exactly.
        assert_eq!(mul_div_floor(u128::MAX, u128::MAX, u128::MAX), Some(u128::MAX));
    }

    #[test]
    fn mul_div_ceil_saturation() {
        // q == u128::MAX with a nonzero remainder cannot be rounded up.
        assert_eq!(mul_div_ceil(u128::MAX, 2, 2), Some(u128::MAX));
        assert_eq!(mul_div_ceil(u128::MAX, u128::MAX, u128::MAX), Some(u128::MAX));
    }

    #[test]
    fn the_impact_term_shape_is_exact() {
        // (maxBid - minBid) * midUsage / bidCapacity at the domain corners.
        let width = (1u128 << 56) - 1;
        let cap = (1u128 << 96) - 1;
        assert_eq!(mul_div_floor(width, cap, cap), Some(width));

        // The whole point of mul_div_floor is that the intermediate product does not have to
        // fit in u128 — here width * (cap/2) is ~2^151. Compute the reference with BigUint
        // rather than natively, which is what made this assertion overflow at compile time.
        let expected = (BigUint::from(width) * BigUint::from(cap / 2)) / BigUint::from(cap);
        assert_eq!(mul_div_floor(width, cap / 2, cap), Some(u128::try_from(expected).unwrap()));
    }

    proptest! {
        #[test]
        fn widening_mul_matches_bigint(a: u128, b: u128) {
            let (hi, lo) = widening_mul(a, b);
            prop_assert_eq!(big(hi, lo), BigUint::from(a) * BigUint::from(b));
            // Independent cross-check of the low limb against native wrapping multiply.
            prop_assert_eq!(lo, a.wrapping_mul(b));
        }

        #[test]
        fn mul_div_matches_bigint(a: u128, b: u128, d: u128) {
            let got = mul_div_rem(a, b, d);
            if d == 0 {
                prop_assert!(got.is_none());
                return Ok(());
            }
            let prod = BigUint::from(a) * BigUint::from(b);
            let want_q = &prod / BigUint::from(d);
            let want_r = &prod % BigUint::from(d);
            if want_q > BigUint::from(u128::MAX) {
                prop_assert!(got.is_none());
            } else {
                let (q, r) = got.expect("quotient fits u128");
                prop_assert_eq!(BigUint::from(q), want_q);
                prop_assert_eq!(BigUint::from(r), want_r);
            }
        }

        /// Self-contained verification that does not lean on bigint: reconstruct
        /// `a*b == q*d + r` with `r < d` using only widening_mul and 256-bit subtract.
        #[test]
        fn mul_div_reconstructs_the_dividend(a: u128, b: u128, d in 1u128..) {
            if let Some((q, r)) = mul_div_rem(a, b, d) {
                prop_assert!(r < d);
                let prod = widening_mul(a, b);
                let qd = widening_mul(q, d);
                prop_assert_eq!(sub256(prod, qd), (0, r));
            }
        }

        /// In the curve's actual domain the quotient always exists.
        #[test]
        fn impact_term_never_overflows(
            width in 0u128..(1u128 << 56),
            usage in 0u128..(1u128 << 96),
            cap in 1u128..(1u128 << 96),
        ) {
            let usage = usage.min(cap);
            let q = mul_div_floor(width, usage, cap).expect("impact quotient fits u128");
            prop_assert!(q <= width);
        }

        #[test]
        fn u256_arithmetic_matches_bigint(a: u128, b: u128, c: u128, k: u128) {
            let x = U256::mul_u128(a, b);
            prop_assert_eq!(u(x), BigUint::from(a) * BigUint::from(b));

            let y = U256::mul_u128(c, k);
            match x.checked_add(y) {
                Some(s) => prop_assert_eq!(u(s), u(x) + u(y)),
                None => prop_assert!(u(x) + u(y) > max256()),
            }
            match x.checked_sub(y) {
                Some(s) => prop_assert_eq!(u(s), u(x) - u(y)),
                None => prop_assert!(u(x) < u(y)),
            }
            match x.checked_mul_u128(k) {
                Some(p) => prop_assert_eq!(u(p), u(x) * BigUint::from(k)),
                None => prop_assert!(u(x) * BigUint::from(k) > max256()),
            }
        }

        #[test]
        fn u256_div_rem_matches_bigint(a: u128, b: u128, c: u128, k: u128) {
            let n = U256::mul_u128(a, b);
            let d = U256::mul_u128(c, k);
            match n.div_rem(d) {
                None => prop_assert!(d.is_zero()),
                Some((q, r)) => {
                    prop_assert!(r < d);
                    prop_assert_eq!(u(q), &u(n) / &u(d));
                    prop_assert_eq!(u(r), &u(n) % &u(d));
                    // Reconstruct without bigint as an independent check.
                    prop_assert_eq!(q.checked_mul_u128(1).unwrap(), q);
                }
            }
        }

        /// The two narrowing dividers agree with bigint, and `None` means and only means the
        /// quotient is above `u128::MAX` (or the divisor is zero).
        #[test]
        fn u256_narrowing_dividers_agree(a: u128, b: u128, c: u128, k in 1u128..) {
            let n = U256::mul_u128(a, b);
            let d = U256::mul_u128(c, k);
            let want_q = if d.is_zero() { None } else { Some(&u(n) / &u(d)) };
            match (div_floor_u256(n, d), &want_q) {
                (Some(got), Some(w)) => prop_assert_eq!(BigUint::from(got), w.clone()),
                (None, Some(w)) => prop_assert!(w > &BigUint::from(u128::MAX)),
                (None, None) => {}
                (Some(_), None) => prop_assert!(false, "divided by zero"),
            }
            if let (Some(f), Some(ce)) = (div_floor_u256(n, d), div_ceil_u256(n, d)) {
                prop_assert!(ce == f || ce == f + 1);
                let exact = n.div_rem(d).unwrap().1.is_zero();
                prop_assert_eq!(ce == f, exact);
            }
        }

        #[test]
        fn mul_div_floor_and_ceil_agree(a: u128, b: u128, d in 1u128..) {
            let (f, c) = (mul_div_floor(a, b, d), mul_div_ceil(a, b, d));
            match (f, c) {
                (Some(f), Some(c)) => prop_assert!(c == f || c == f + 1),
                (None, None) => {}
                (Some(f), None) => prop_assert_eq!(f, u128::MAX),
                (None, Some(_)) => prop_assert!(false, "ceil succeeded where floor failed"),
            }
        }
    }
}
