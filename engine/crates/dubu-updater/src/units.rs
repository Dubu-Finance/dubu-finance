//! Fixed-point decimal parsing, and the one conversion between the exchange's price scale and
//! the pool's.
//!
//! Everything the bot reads from outside itself arrives as a decimal string — Binance sends
//! `"1943.82000000"`, the config file says `capacity = "1000"` — and both have to become integers
//! *exactly*. A float round trip through `f64` is fine for a display value and wrong for a number
//! the chain will settle a `uint56` against, so there is no `f64` in this crate's pricing path and
//! this module is the only place a decimal string becomes a number.
//!
//! # Scales
//!
//! Three different scales appear, and confusing them is the obvious bug:
//!
//! | scale | what | example |
//! |---|---|---|
//! | [`FEED_SCALE`] = 8 | exchange prices and quantities | ETHUSDT `1943.82` -> `194_382_000_000` |
//! | token decimals | on-chain amounts | 1 mWETH -> `10^18` |
//! | pool price scale | `PropPool`'s `uint56` price | see [`price_shift`] |
//!
//! [`FEED_SCALE`] is 8 because Binance never publishes more than eight fractional digits, so the
//! parse is lossless, and because the micro-price forms `bid * askQty + ask * bidQty`: at scale 8
//! that product peaks around `10^27` for a six-figure asset with a large book, six orders of
//! magnitude inside `u128`. At scale 18 the same product overflows.
//!
//! Assertions here are `debug_assert!`: this runs once per feed tick, and a panic in the parser
//! stops the bot quoting altogether, which is worse than the malformed input it would be
//! reporting.

use dubu_core::math::{mul_div_floor, pow10};

/// Decimal places the exchange feed is parsed at. See the module docs for why 8 and not 18.
pub const FEED_SCALE: u8 = 8;

/// What went wrong turning a string into a number.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UnitsError {
    /// The string was empty, signed, in exponent notation, or had a stray character.
    #[error("`{0}` is not a plain non-negative decimal")]
    Malformed(String),
    /// More fractional digits than the target scale can hold.
    ///
    /// Deliberately an error rather than a silent truncation. Truncating is how a config that
    /// says `0.0000000001` becomes zero capacity at 9 decimals without anyone noticing.
    #[error("`{value}` has more than {scale} decimal places")]
    TooPrecise {
        /// The offending string.
        value: String,
        /// The scale it was being parsed at.
        scale: u8,
    },
    /// The scaled value does not fit in `u128`, or the scale exceeds `10^38`.
    #[error("`{0}` overflows a u128 at this scale")]
    Overflow(String),
}

/// Parse a non-negative decimal string into an integer scaled by `10^scale`.
///
/// Accepts `"1000"`, `"1000.5"`, `"0.25"`, `".5"`, `"1_000"` is **not** accepted — underscores
/// are a Rust literal convention, not a decimal one, and accepting them invites `"1_000"` and
/// `"1,000"` to be treated as the same thing.
///
/// # Errors
/// [`UnitsError`], one variant per failure, all of which name the offending string so a bad
/// config line is identifiable without a debugger.
pub fn parse_fixed(s: &str, scale: u8) -> Result<u128, UnitsError> {
    let t = s.trim();
    if t.is_empty() {
        return Err(UnitsError::Malformed(s.to_string()));
    }
    let (int_part, frac_part) = match t.split_once('.') {
        Some((i, f)) => (i, f),
        None => (t, ""),
    };
    // An empty integer part is only legal when there is a fractional part (`.5`), and an empty
    // fractional part is only legal when there is an integer part (`5.`).
    if (int_part.is_empty() && frac_part.is_empty())
        || !int_part.bytes().all(|b| b.is_ascii_digit())
        || !frac_part.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(UnitsError::Malformed(s.to_string()));
    }
    if frac_part.len() > usize::from(scale) {
        return Err(UnitsError::TooPrecise {
            value: s.to_string(),
            scale,
        });
    }
    debug_assert!(frac_part.len() <= usize::from(scale));
    debug_assert!(!int_part.is_empty() || !frac_part.is_empty());

    let unit = pow10(scale).ok_or_else(|| UnitsError::Overflow(s.to_string()))?;
    debug_assert!(unit >= 1);
    let whole: u128 = if int_part.is_empty() {
        0
    } else {
        int_part
            .parse()
            .map_err(|_| UnitsError::Overflow(s.to_string()))?
    };
    // Right-pad the fraction to the full scale: `.5` at scale 8 is 50_000_000, not 5.
    let pad =
        pow10(scale - frac_part.len() as u8).ok_or_else(|| UnitsError::Overflow(s.to_string()))?;
    let frac: u128 = if frac_part.is_empty() {
        0
    } else {
        frac_part
            .parse::<u128>()
            .map_err(|_| UnitsError::Overflow(s.to_string()))?
            * pad
    };
    // The fraction is strictly inside one whole unit, which is what makes the sum below a
    // faithful fixed-point value rather than a carry the caller has to notice.
    debug_assert!(frac < unit);

    let v = whole
        .checked_mul(unit)
        .and_then(|w| w.checked_add(frac))
        .ok_or_else(|| UnitsError::Overflow(s.to_string()))?;
    debug_assert_eq!(v / unit, whole, "the integer part survives the scaling");
    debug_assert_eq!(v % unit, frac);
    // The negative space: a non-empty, non-zero string must not scale to nothing. Truncation to
    // zero is the failure `TooPrecise` exists to make impossible, so it must not sneak back in.
    if v == 0 {
        debug_assert!(t.bytes().all(|b| !b.is_ascii_digit() || b == b'0'));
    }
    Ok(v)
}

/// Render a scaled integer back to a decimal string, for logs and error messages.
///
/// Trailing zeros are kept so that the scale is visible in the output — `1943.82000000` tells a
/// reader this came off the feed at scale 8, where `1943.82` does not.
#[must_use]
pub fn format_fixed(v: u128, scale: u8) -> String {
    let Some(unit) = pow10(scale) else {
        return v.to_string();
    };
    if unit == 1 {
        return v.to_string();
    }
    debug_assert!(v % unit < unit);
    let out = format!(
        "{}.{:0width$}",
        v / unit,
        v % unit,
        width = usize::from(scale)
    );
    // The scale has to be visible in the output; that is the entire point of keeping the
    // trailing zeros, and a short fraction would read as a different scale.
    debug_assert!(out.contains('.'));
    debug_assert_eq!(
        out.split_once('.').map(|(_, f)| f.len()),
        Some(usize::from(scale))
    );
    out
}

/// The decimal shift between a human price and the pool's `uint56` price for one pair.
///
/// `PropPool` settles `quoteAmount = baseAmount * price / 10**priceScaleExp`. Substituting one
/// whole unit of each token — `baseAmount = 10^bd`, `quoteAmount = human * 10^qd` — gives
///
/// ```text
/// price = human * 10^(priceScaleExp + quoteDecimals - baseDecimals)
/// ```
///
/// For pairId 1 (18/6, exp 24) that is `10^12`, and for pairId 2 (8/6, exp 12) it is `10^10`.
/// Both are positive, but the expression is signed in general — an 18-decimal base against a
/// 6-decimal quote at a small exponent goes negative — so this returns an `i32` and
/// [`to_pool_price`] handles both directions rather than assuming the pairs we happen to run.
#[must_use]
pub fn price_shift(price_scale_exp: u8, base_decimals: u8, quote_decimals: u8) -> i32 {
    let shift = i32::from(price_scale_exp) + i32::from(quote_decimals) - i32::from(base_decimals);
    // Three `u8`s widened to `i32` cannot leave the range, which is what makes the sign the only
    // interesting thing about the result.
    assert!(shift >= -255);
    assert!(shift <= 510);
    if base_decimals == quote_decimals {
        assert_eq!(shift, i32::from(price_scale_exp));
    }
    shift
}

/// Convert a feed price (scale [`FEED_SCALE`]) into the pool's `uint56` price scale.
///
/// Floors. The residual is one unit of the pool's price scale — `10^-12` of a dollar on pairId 1
/// — against a half-spread measured in basis points, so the direction is immaterial to PnL. It
/// is floored rather than rounded because every other rounding in this system is chosen and
/// documented, and "immaterial" is not a reason to leave one undecided.
///
/// Returns `None` when the shift is outside `10^±38` or the result exceeds `u128`; the caller
/// treats that as an unquotable pair rather than clamping, because a clamped price is a wrong
/// price that looks like a right one.
#[must_use]
pub fn to_pool_price(feed_price: u128, shift: i32) -> Option<u128> {
    // chain = feed * 10^shift / 10^FEED_SCALE, arranged so neither exponent goes negative.
    let (num_exp, den_exp) = if shift >= i32::from(FEED_SCALE) {
        (shift - i32::from(FEED_SCALE), 0)
    } else {
        (0, i32::from(FEED_SCALE) - shift)
    };
    debug_assert!(num_exp >= 0);
    debug_assert!(den_exp >= 0);
    debug_assert!(num_exp == 0 || den_exp == 0, "only one direction at a time");

    let num = pow10(u8::try_from(num_exp).ok()?)?;
    let den = pow10(u8::try_from(den_exp).ok()?)?;
    debug_assert!(num >= 1);
    debug_assert!(den >= 1);
    let p = mul_div_floor(feed_price, num, den)?;
    debug_assert!(feed_price != 0 || p == 0, "zero in, zero out");
    Some(p)
}

/// Inverse of [`to_pool_price`], for logging a pool price back in human terms.
#[must_use]
pub fn from_pool_price(pool_price: u128, shift: i32) -> Option<u128> {
    let (num_exp, den_exp) = if shift >= i32::from(FEED_SCALE) {
        (0, shift - i32::from(FEED_SCALE))
    } else {
        (i32::from(FEED_SCALE) - shift, 0)
    };
    debug_assert!(num_exp >= 0);
    debug_assert!(den_exp >= 0);
    debug_assert!(num_exp == 0 || den_exp == 0);

    let num = pow10(u8::try_from(num_exp).ok()?)?;
    let den = pow10(u8::try_from(den_exp).ok()?)?;
    // The exponents are exactly [`to_pool_price`]'s, swapped: this is its inverse and not a
    // second, independently derived conversion.
    debug_assert!(num >= 1);
    debug_assert!(den >= 1);
    let p = mul_div_floor(pool_price, num, den)?;
    debug_assert!(pool_price != 0 || p == 0);
    Some(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_what_binance_actually_sends() {
        assert_eq!(
            parse_fixed("1943.82000000", FEED_SCALE),
            Ok(194_382_000_000)
        );
        assert_eq!(parse_fixed("24.25800000", FEED_SCALE), Ok(2_425_800_000));
        assert_eq!(parse_fixed("0.00000001", FEED_SCALE), Ok(1));
        assert_eq!(
            parse_fixed("118000.00000000", FEED_SCALE),
            Ok(11_800_000_000_000)
        );
    }

    #[test]
    fn parses_what_a_config_actually_says() {
        assert_eq!(parse_fixed("1000", 18), Ok(1_000_000_000_000_000_000_000));
        assert_eq!(parse_fixed("20", 8), Ok(2_000_000_000));
        assert_eq!(parse_fixed("0.5", 6), Ok(500_000));
        assert_eq!(parse_fixed(".5", 6), Ok(500_000));
        assert_eq!(parse_fixed("5.", 6), Ok(5_000_000));
        assert_eq!(parse_fixed("0", 18), Ok(0));
    }

    #[test]
    fn excess_precision_is_an_error_not_a_truncation() {
        // The failure this prevents: a 9-decimal capacity silently becoming 0 at 8 decimals.
        assert_eq!(
            parse_fixed("0.000000001", 8),
            Err(UnitsError::TooPrecise {
                value: "0.000000001".into(),
                scale: 8
            })
        );
    }

    #[test]
    fn junk_is_rejected_rather_than_coerced() {
        for bad in [
            "", " ", "-1", "+1", "1e9", "1_000", "1,000", "abc", "1.2.3", ".", "1.2a",
        ] {
            assert!(parse_fixed(bad, 8).is_err(), "`{bad}` was accepted");
        }
    }

    #[test]
    fn round_trips_through_the_formatter() {
        assert_eq!(format_fixed(194_382_000_000, FEED_SCALE), "1943.82000000");
        assert_eq!(format_fixed(1, FEED_SCALE), "0.00000001");
        assert_eq!(format_fixed(0, FEED_SCALE), "0.00000000");
        assert_eq!(format_fixed(7, 0), "7");
    }

    #[test]
    fn the_two_live_pairs_shift_where_deployments_md_says() {
        // pairId 1, mWETH/mUSDC, 18/6, priceScaleExp 24.
        assert_eq!(price_shift(24, 18, 6), 12);
        // 1943.82 USDC per WETH -> 1_943_820_000_000_000 in the pool's scale.
        let p = to_pool_price(parse_fixed("1943.82", FEED_SCALE).unwrap(), 12).unwrap();
        assert_eq!(p, 1_943_820_000_000_000);
        assert!(p <= dubu_core::curve::PRICE_MAX);
        assert_eq!(from_pool_price(p, 12), Some(194_382_000_000));

        // pairId 2, mWBTC/mUSDC, 8/6, priceScaleExp 12.
        assert_eq!(price_shift(12, 8, 6), 10);
        let p = to_pool_price(parse_fixed("118000", FEED_SCALE).unwrap(), 10).unwrap();
        assert_eq!(p, 1_180_000_000_000_000);
        assert!(p <= dubu_core::curve::PRICE_MAX);
    }

    #[test]
    fn a_negative_shift_divides_instead_of_multiplying() {
        // 6-decimal base against an 18-decimal quote at exp 0 would shift by +12; the mirror
        // case, an 18-decimal base against a 6-decimal quote at exp 0, shifts by -12.
        assert_eq!(price_shift(0, 18, 6), -12);
        // 1e8-scaled 1943.82 divided by 10^20 floors to zero, which is the honest answer: that
        // pair cannot represent this price and must not be quoted.
        assert_eq!(to_pool_price(194_382_000_000, -12), Some(0));
        assert_eq!(to_pool_price(194_382_000_000, 0), Some(1_943));
    }
}
