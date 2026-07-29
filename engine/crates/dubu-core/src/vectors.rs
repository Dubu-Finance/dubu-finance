//! Differential test-vector generation.
//!
//! Emits a JSON array that `contracts/test/` reads back and replays against `PropCurve.sol`.
//! Both implementations must agree on every record; that agreement is the only evidence
//! either side is right.
//!
//! # Why this module may panic and the rest of the crate may not
//!
//! Everything else here returns a `Result`, and the crate denies `unwrap`, `expect` and `panic` to
//! keep it that way. This module is exempt on purpose.
//!
//! It is a generator, not runtime code — nothing in the quote loop reaches it. It runs when
//! somebody regenerates the fixtures, and what it produces is the evidence that the Rust and the
//! Solidity agree. A vector it could not compute is not a degraded result to carry forward; it is
//! a hole in that evidence, and a fixture file with a hole in it looks exactly like one without.
//! Stopping the generation is the correct failure, and it is the same judgement `markout` makes
//! when it counts an unmarked fill rather than guessing at one.
//!
//! # Wire format
//!
//! A flat array of uniform records — no nesting, no heterogeneous arrays, no tagged union.
//! Every field is present on every record and **every field is a JSON string**, including the
//! numbers, because a JSON number is a double as far as several parsers are concerned and
//! `uint96`/`uint256` values do not survive that.
//!
//! The canonical field list, which is also the record's key order in the emitted file:
//!
//! ```solidity
//! struct Vector {
//!     string amountIn;       // the call's PRIMARY numeric argument, decimal. Base-denominated
//!                            // for `amountOutBid` / `amountInAsk`; QUOTE-denominated for
//!                            // `amountInBid` / `amountOutAsk`, whose argument is the requested
//!                            // output. Dispatch on `kind` before interpreting it.
//!     string capacity;       // uint96 domain, decimal
//!     string expectRevert;   // "" when the call must succeed, else a PropCurve error name
//!     string expected;       // decimal result; "0" and ignored when expectRevert != ""
//!     string kind;           // which PropCurve function to call; DISPATCH ON THIS FIRST
//!     string maxAsk;
//!     string maxBid;
//!     string minAsk;
//!     string minBid;
//!     string minPrice;       // the PAIR's price floor. Only meaningful for validateLadder.
//!                            // NOT a ladder price - see the warning below.
//!     string name;           // label for the failure message
//!     string priceScaleExp;  // uint8, decimal
//!     string used;           // uint96 domain, decimal
//! }
//! ```
//!
//! ## Read it field by field, not with `abi.decode`
//!
//! The obvious consumption path — `abi.decode(vm.parseJson(json), (Vector[]))` — **does not
//! work for this dataset, and cannot be made to.** Measured against Foundry 1.5.1:
//! `vm.parseJson` inspects each JSON string and *coerces it to `uint256`* when it parses as a
//! decimal integer greater than `i64::MAX` (9223372036854775807). At or below that boundary the
//! value stays a `string`. So `"9223372036854775807"` decodes as `string` and
//! `"9223372036854775808"` decodes as `uint256`, from the same schema.
//!
//! That makes the array **heterogeneous per value**, which no single Solidity struct can
//! describe: `capacity` is `"0"` on one record and `"79228162514264337593543950335"` on
//! another, so a `string` member reverts on the second and a `uint256` member reverts on the
//! first. Zero-padding does not help — the coercion tests the parsed *value*, not the text. The
//! dataset genuinely needs values above the boundary (`uint96` capacities, outputs up to the
//! `uint128` ceiling, and the `type(uint256).max` sentinel), so this is not a schema choice
//! that can be tuned away.
//!
//! Explicitly typed reads bypass the coercion entirely and are the supported path:
//!
//! ```solidity
//! string memory json = vm.readFile("testdata/curve_vectors.json");
//! for (uint256 i; ; ++i) {
//!     string memory at = string.concat("$[", vm.toString(i), "].");
//!     // ... break when the index no longer resolves ...
//!     string memory kind = vm.parseJsonString(json, string.concat(at, "kind"));
//!     string memory err  = vm.parseJsonString(json, string.concat(at, "expectRevert"));
//!     uint256 amountIn   = vm.parseUint(vm.parseJsonString(json, string.concat(at, "amountIn")));
//!     // ... dispatch on `kind`, then:
//!     if (bytes(err).length != 0) {
//!         vm.expectRevert(bytes4(keccak256(bytes(string.concat(err, "()")))));
//!         _call(...);                   // must revert; return value irrelevant
//!     } else {
//!         assertEq(_call(...), vm.parseUint(/* expected */), /* name */);
//!     }
//! }
//! ```
//!
//! ## Consumer contract
//!
//! Two traps, both of which have already been hit:
//!
//! * **Dispatch on `kind` before reading any price.** Seven kinds share one flat record shape.
//!   `amountOutBid` / `amountInBid` take `(minBid, maxBid)`; `amountInAsk` / `amountOutAsk` take
//!   `(minAsk, maxAsk)`; `validateLadder` takes all four plus `minPrice`;
//!   `executableTop{Bid,Ask}` take one pair and ignore `amountIn`/`priceScaleExp`. The unused
//!   fields are present and set to `"0"`, not absent, so a decoder that probes candidate key
//!   names instead of reading `kind` will happily pick `minBid` for an ask record and report a
//!   curve defect that is really a schema mismatch.
//! * **`amountIn` is the argument, not necessarily an input amount.** `PropCurve` now exposes
//!   both directions of both sides. For the two exact-output kinds (`amountInBid`, `amountOutAsk`)
//!   the field carries the requested *output*, and `expected` carries the required input. The
//!   field name is kept because the schema is kept; the `kind` dispatch is what disambiguates.
//! * **`capacity` is in BASE units on both sides.** Ask capacity used to be quote-denominated.
//!   A consumer that still scales ask capacity by a price will mis-drive every ask record.
//! * **`minPrice` is the pair's floor, not the low ladder price.** It is an input to
//!   `validateLadder` only, and is `"0"` on every quote record. Reading it as "the low price"
//!   silently prices every bid vector off a zero ladder bottom.
//!
//! ## Key ordering
//!
//! The field order above is **byte-wise ASCII sorted**, which is deliberately *not* the same as
//! case-insensitive alphabetical: `expectRevert` precedes `expected` because `'R'` (0x52) sorts
//! below `'e'` (0x65). It is kept sorted so the file is stable under any key-sorting decoder,
//! diffs cleanly, and would be struct-decodable if the schema were ever restricted to values
//! below the coercion boundary. Swapping that one pair would turn every revert case into a
//! success case expecting the string `"AmountOutOfDomain"` as a decimal, which is exactly the
//! kind of silent misalignment sorted keys are meant to prevent.
//! `field_order_is_byte_wise_sorted` pins it.
//!
//! # Expressing a revert
//!
//! `expectRevert` carries the bare error name (`"ZeroCapacity"`, not `"ZeroCapacity()"`); the
//! consumer appends `()` before hashing. It is set for every [`CurveError`] that has a
//! Solidity counterpart — see [`SOLIDITY_ERRORS`] — and the pairing is what makes the
//! differential test two-sided: agreeing on *where the domain ends* is worth at least as much
//! as agreeing on the numbers inside it. A port that silently returned a wrong number at the
//! edge and a port that silently succeeded where the chain reverts are the same class of bug.
//!
//! [`CurveError::AmountOutOfDomain`] is emitted by the two kinds that *return* a quote amount —
//! `amountOutBid` and `amountInAsk` — at `exp == 0` with `uint56` prices and a `uint96` size,
//! where the product reaches ~2^152: a value `uint256` holds and `u128` does not. `PropCurve`
//! reverts there (`MAX_AMOUNT_OUT == type(uint128).max`) precisely so the two domains coincide,
//! so those are *shared* reverts and must be emitted, not skipped.
//!
//! The two kinds whose *argument* is quote-denominated (`amountInBid`, `amountOutAsk`) also
//! reject above `MAX_AMOUNT_OUT` on chain, but that check is unrepresentable in this port —
//! `MAX_AMOUNT_OUT == u128::MAX`, so the argument's own type enforces it — and therefore emits
//! no vector. The domains still coincide; there is simply nothing to assert.
//!
//! # What is never emitted
//!
//! A record whose Rust result is [`CurveError::ArithmeticPanic`] or
//! [`CurveError::DomainOverflow`]. Those are the only two remaining places this port and the
//! chain genuinely disagree — the chain either panics with an unnamed `Panic(uint256)` or
//! succeeds outright — so asserting on them would assert on the divergence rather than on the
//! agreement. [`generate`] panics if it ever builds one, which keeps the gap visible instead
//! of quietly widening.

#![allow(clippy::expect_used, clippy::panic)]

use serde::{Deserialize, Serialize};

use crate::curve::{
    amount_in_ask, amount_in_bid, amount_out_ask, amount_out_bid, executable_top_ask,
    executable_top_bid, validate_ladder, MAX_AMOUNT, MAX_AMOUNT_OUT, MAX_PRICE,
    MAX_PRICE_SCALE_EXP, NO_ASK,
};
use crate::error::CurveError;

/// `type(uint256).max`, the value `executableTopAsk` returns on zero capacity. See
/// [`crate::curve::NO_ASK`].
pub const UINT256_MAX_DEC: &str =
    "115792089237316195423570985008687907853269984665640564039457584007913129639935";

/// Every value [`Vector::expectRevert`] may take, other than `""`.
///
/// These are exactly the `CurveError` variants with a Solidity counterpart, so a consumer can
/// reject an unknown string rather than silently treating it as a success case. The list is
/// asserted against [`CurveError::solidity_signature`] by `solidity_errors_is_exhaustive`, so
/// adding a custom error to `PropCurve.sol` without extending this fails the build.
pub const SOLIDITY_ERRORS: [&str; 6] = [
    "AmountExceedsCapacity",
    "ZeroCapacity",
    "ZeroPrice",
    "CrossedBook",
    "BidBelowMinPrice",
    "AmountOutOfDomain",
];

/// Which `PropCurve` function a record exercises.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `PropCurve.amountOutBid` — base in, quote out.
    AmountOutBid,
    /// `PropCurve.amountInBid` — quote out requested, base in required.
    AmountInBid,
    /// `PropCurve.amountInAsk` — base out requested, quote in required.
    AmountInAsk,
    /// `PropCurve.amountOutAsk` — quote in, base out.
    AmountOutAsk,
    /// `PropCurve.validateLadder`
    ValidateLadder,
    /// `PropCurve.executableTopBid`
    ExecutableTopBid,
    /// `PropCurve.executableTopAsk`
    ExecutableTopAsk,
}

impl Kind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::AmountOutBid => "amountOutBid",
            Self::AmountInBid => "amountInBid",
            Self::AmountInAsk => "amountInAsk",
            Self::AmountOutAsk => "amountOutAsk",
            Self::ValidateLadder => "validateLadder",
            Self::ExecutableTopBid => "executableTopBid",
            Self::ExecutableTopAsk => "executableTopAsk",
        }
    }
}

/// One differential record. Field order here is the alphabetical order the Solidity struct
/// must declare; do not reorder without updating the consumer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(non_snake_case)]
pub struct Vector {
    /// Trade size, decimal.
    pub amountIn: String,
    /// Side capacity, decimal.
    pub capacity: String,
    /// Expected custom-error name, or `""` for success.
    pub expectRevert: String,
    /// Expected return value, decimal. Meaningless when `expectRevert` is set.
    pub expected: String,
    /// Which function to call; see [`Kind`].
    pub kind: String,
    /// Ladder price, decimal.
    pub maxAsk: String,
    /// Ladder price, decimal.
    pub maxBid: String,
    /// Ladder price, decimal.
    pub minAsk: String,
    /// Ladder price, decimal.
    pub minBid: String,
    /// Pair floor, decimal. Only meaningful for `validateLadder`.
    pub minPrice: String,
    /// Human label, for the failure message.
    pub name: String,
    /// Decimal alignment exponent.
    pub priceScaleExp: String,
    /// Side usage already consumed this epoch, decimal.
    pub used: String,
}

/// Everything a record needs before the expected output is computed.
#[derive(Debug, Clone, Copy)]
struct Inputs {
    kind: Kind,
    amount_in: u128,
    min_bid: u128,
    max_bid: u128,
    min_ask: u128,
    max_ask: u128,
    min_price: u128,
    capacity: u128,
    used: u128,
    exp: u8,
}

impl Default for Inputs {
    fn default() -> Self {
        Self {
            kind: Kind::AmountOutBid,
            amount_in: 0,
            min_bid: 0,
            max_bid: 0,
            min_ask: 0,
            max_ask: 0,
            min_price: 0,
            capacity: 0,
            used: 0,
            exp: 0,
        }
    }
}

/// Evaluate the port and render the record. Panics if the port disagrees with the chain in a
/// way that cannot be expressed as a vector; see the module docs.
fn record(name: &str, i: Inputs) -> Vector {
    let outcome: Result<u128, CurveError> = match i.kind {
        Kind::AmountOutBid => {
            amount_out_bid(i.amount_in, i.min_bid, i.max_bid, i.capacity, i.used, i.exp)
        }
        Kind::AmountInBid => {
            amount_in_bid(i.amount_in, i.min_bid, i.max_bid, i.capacity, i.used, i.exp)
        }
        Kind::AmountInAsk => {
            amount_in_ask(i.amount_in, i.min_ask, i.max_ask, i.capacity, i.used, i.exp)
        }
        Kind::AmountOutAsk => {
            amount_out_ask(i.amount_in, i.min_ask, i.max_ask, i.capacity, i.used, i.exp)
        }
        Kind::ValidateLadder => {
            validate_ladder(i.min_bid, i.max_bid, i.min_ask, i.max_ask, i.min_price).map(|()| 0u128)
        }
        Kind::ExecutableTopBid => executable_top_bid(i.min_bid, i.max_bid, i.capacity, i.used),
        Kind::ExecutableTopAsk => executable_top_ask(i.min_ask, i.max_ask, i.capacity, i.used),
    };

    let (expect_revert, expected) = match outcome {
        Ok(v) if i.kind == Kind::ExecutableTopAsk && v == NO_ASK => {
            (String::new(), UINT256_MAX_DEC.to_owned())
        }
        Ok(v) => (String::new(), v.to_string()),
        Err(e) => {
            let sig = e.solidity_signature().unwrap_or_else(|| {
                panic!(
                    "vector `{name}` hits a non-Solidity divergence ({e}); it must not be emitted"
                )
            });
            (sig.trim_end_matches("()").to_owned(), "0".to_owned())
        }
    };

    Vector {
        amountIn: i.amount_in.to_string(),
        capacity: i.capacity.to_string(),
        expectRevert: expect_revert,
        expected,
        kind: i.kind.as_str().to_owned(),
        maxAsk: i.max_ask.to_string(),
        maxBid: i.max_bid.to_string(),
        minAsk: i.min_ask.to_string(),
        minBid: i.min_bid.to_string(),
        minPrice: i.min_price.to_string(),
        name: name.to_owned(),
        priceScaleExp: i.exp.to_string(),
        used: i.used.to_string(),
    }
}

/// Re-evaluate a record's inputs and confirm they still produce its stated output. Used both
/// by the crate's own tests and by anything that wants to check a checked-in file has not
/// drifted from the code.
///
/// # Panics
/// Never; returns `false` on mismatch.
#[must_use]
pub fn verify(v: &Vector) -> bool {
    let parse = |s: &String| -> u128 { s.parse().unwrap_or(u128::MAX) };
    let kind = match v.kind.as_str() {
        "amountOutBid" => Kind::AmountOutBid,
        "amountInBid" => Kind::AmountInBid,
        "amountInAsk" => Kind::AmountInAsk,
        "amountOutAsk" => Kind::AmountOutAsk,
        "validateLadder" => Kind::ValidateLadder,
        "executableTopBid" => Kind::ExecutableTopBid,
        "executableTopAsk" => Kind::ExecutableTopAsk,
        _ => return false,
    };
    let Ok(exp) = v.priceScaleExp.parse::<u8>() else {
        return false;
    };
    let inputs = Inputs {
        kind,
        amount_in: parse(&v.amountIn),
        min_bid: parse(&v.minBid),
        max_bid: parse(&v.maxBid),
        min_ask: parse(&v.minAsk),
        max_ask: parse(&v.maxAsk),
        min_price: parse(&v.minPrice),
        capacity: parse(&v.capacity),
        used: parse(&v.used),
        exp,
    };
    record(&v.name, inputs) == *v
}

/// Realistic pair configurations, as `(label, priceScaleExp, base decimals, quote decimals,
/// price, one base unit)`. `price = human_price * 10**(exp - base_dec + quote_dec)`.
const PAIRS: [(&str, u8, u128, u128); 3] = [
    // WETH/USDC 18/6, ETH at 3_000.
    (
        "weth_usdc_18_6",
        24,
        3_000_000_000_000_000,
        1_000_000_000_000_000_000,
    ),
    // WBTC/USDC 8/6, BTC at 60_000.
    ("wbtc_usdc_8_6", 13, 6_000_000_000_000_000, 100_000_000),
    // USDC/WETH 6/18 — the inverted orientation, where uint56 binds hardest.
    ("usdc_weth_6_18", 8, 33_333_333_333_333_300, 1_000_000),
];

/// Build the full vector set.
///
/// # Panics
/// If any constructed case would produce a non-Solidity divergence. That is a bug in this
/// function, not a legitimate vector.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn generate() -> Vec<Vector> {
    let mut v = Vec::new();
    let cap = 1_000_000u128;
    let (lo, hi) = (1_000_000_000_000u128, 1_001_000_000_000u128);

    // -- amountOutBid: control flow -----------------------------------------------------
    let bid = |name: &str, i: Inputs| {
        record(
            name,
            Inputs {
                kind: Kind::AmountOutBid,
                ..i
            },
        )
    };

    v.push(bid("bid/zero_amount_zero_capacity", Inputs::default()));
    v.push(bid(
        "bid/zero_amount_live_pair",
        Inputs {
            min_bid: lo,
            max_bid: hi,
            capacity: cap,
            exp: 18,
            ..Inputs::default()
        },
    ));
    v.push(bid(
        "bid/zero_capacity_nonzero_amount",
        Inputs {
            amount_in: 1,
            min_bid: lo,
            max_bid: hi,
            exp: 18,
            ..Inputs::default()
        },
    ));
    v.push(bid(
        "bid/one_wei",
        Inputs {
            amount_in: 1,
            min_bid: lo,
            max_bid: hi,
            capacity: cap,
            exp: 18,
            ..Inputs::default()
        },
    ));
    v.push(bid(
        "bid/capacity_boundary_exact",
        Inputs {
            amount_in: cap,
            min_bid: lo,
            max_bid: hi,
            capacity: cap,
            exp: 18,
            ..Inputs::default()
        },
    ));
    v.push(bid(
        "bid/capacity_boundary_plus_one",
        Inputs {
            amount_in: cap + 1,
            min_bid: lo,
            max_bid: hi,
            capacity: cap,
            exp: 18,
            ..Inputs::default()
        },
    ));
    v.push(bid(
        "bid/used_plus_amount_exactly_capacity",
        Inputs {
            amount_in: cap - 999,
            min_bid: lo,
            max_bid: hi,
            capacity: cap,
            used: 999,
            exp: 18,
            ..Inputs::default()
        },
    ));
    v.push(bid(
        "bid/used_plus_amount_one_over",
        Inputs {
            amount_in: cap - 998,
            min_bid: lo,
            max_bid: hi,
            capacity: cap,
            used: 999,
            exp: 18,
            ..Inputs::default()
        },
    ));
    v.push(bid(
        "bid/fully_used_epoch_zero_amount",
        Inputs {
            min_bid: lo,
            max_bid: hi,
            capacity: cap,
            used: cap,
            exp: 18,
            ..Inputs::default()
        },
    ));
    v.push(bid(
        "bid/capacity_one_amount_one",
        Inputs {
            amount_in: 1,
            min_bid: lo,
            max_bid: hi,
            capacity: 1,
            exp: 18,
            ..Inputs::default()
        },
    ));
    v.push(bid(
        "bid/flat_ladder",
        Inputs {
            amount_in: 12_345,
            min_bid: lo,
            max_bid: lo,
            capacity: cap,
            exp: 18,
            ..Inputs::default()
        },
    ));
    // `midUsage = used + (amountIn + 1) / 2` CEILS, so these two share a midpoint of 500_000
    // (the floored halving they were originally named for gave 499_999 and 500_000). The pair
    // is kept adjacent because it pins the direction of that rounding: any drift back to a
    // floor separates them.
    v.push(bid(
        "bid/odd_amount_ceils_the_halving",
        Inputs {
            amount_in: 999_999,
            min_bid: lo,
            max_bid: hi,
            capacity: cap,
            exp: 18,
            ..Inputs::default()
        },
    ));
    v.push(bid(
        "bid/even_amount_shares_the_odd_midpoint",
        Inputs {
            amount_in: 1_000_000,
            min_bid: lo,
            max_bid: hi,
            capacity: cap,
            exp: 18,
            ..Inputs::default()
        },
    ));
    v.push(bid(
        "bid/mid_epoch_usage",
        Inputs {
            amount_in: 100_000,
            min_bid: lo,
            max_bid: hi,
            capacity: cap,
            used: cap / 2,
            exp: 18,
            ..Inputs::default()
        },
    ));
    v.push(bid(
        "bid/min_price_zero_ladder_bottom_zero",
        Inputs {
            amount_in: cap,
            min_bid: 0,
            max_bid: hi,
            capacity: cap,
            exp: 18,
            ..Inputs::default()
        },
    ));

    // -- amountOutBid: every supported priceScaleExp -------------------------------------
    // amountIn * avgBid stays under 2^128 for these, so exp = 0..=7 are representable too.
    for exp in 0..=MAX_PRICE_SCALE_EXP {
        v.push(bid(
            &format!("bid/price_scale_exp_{exp}"),
            Inputs {
                amount_in: 1_000_000,
                min_bid: 999_000_000_000_000,
                max_bid: 1_000_000_000_000_000,
                capacity: 4_000_000,
                used: 1_000_000,
                exp,
                ..Inputs::default()
            },
        ));
    }

    // -- amountOutBid: uint56 / uint96 corners -------------------------------------------
    v.push(bid(
        "bid/max_u56_prices_max_exp",
        Inputs {
            amount_in: MAX_AMOUNT,
            min_bid: MAX_PRICE - 1,
            max_bid: MAX_PRICE,
            capacity: MAX_AMOUNT,
            exp: MAX_PRICE_SCALE_EXP,
            ..Inputs::default()
        },
    ));
    v.push(bid(
        "bid/max_u56_prices_exp_8",
        Inputs {
            amount_in: MAX_AMOUNT,
            min_bid: 0,
            max_bid: MAX_PRICE,
            capacity: MAX_AMOUNT,
            exp: 8,
            ..Inputs::default()
        },
    ));
    v.push(bid(
        "bid/max_u56_prices_full_span_half_used",
        Inputs {
            amount_in: MAX_AMOUNT / 2,
            min_bid: 0,
            max_bid: MAX_PRICE,
            capacity: MAX_AMOUNT,
            used: MAX_AMOUNT / 2,
            exp: 20,
            ..Inputs::default()
        },
    ));

    // -- MAX_AMOUNT_OUT: the shared domain edge, asserted as a revert on both sides ---------
    //
    // These are the vectors the `expectRevert` half of the schema exists for. `exp = 0` means
    // no decimal alignment at all, so `amountIn * avgBid` is the raw ~2^152 product: uint256
    // holds it, `MAX_AMOUNT_OUT` (== type(uint128).max) rejects it, and the port's u128 cannot
    // represent it either. Agreement on *where the domain ends* is the point.
    v.push(bid(
        "bid/amount_out_of_domain_flat_ladder_exp_0",
        Inputs {
            amount_in: MAX_AMOUNT,
            min_bid: MAX_PRICE,
            max_bid: MAX_PRICE,
            capacity: MAX_AMOUNT,
            exp: 0,
            ..Inputs::default()
        },
    ));
    // One unit under the boundary on the same shape, so the pair brackets it: this must
    // succeed. avgBid == 1, so amountOut == amountIn == 2^96 - 1, comfortably inside uint128.
    v.push(bid(
        "bid/amount_out_just_inside_the_domain",
        Inputs {
            amount_in: MAX_AMOUNT,
            min_bid: 1,
            max_bid: 1,
            capacity: MAX_AMOUNT,
            exp: 0,
            ..Inputs::default()
        },
    ));

    // -- amountOutBid / amountOutAsk: realistic pairs ------------------------------------
    for (label, exp, price, one_unit) in PAIRS {
        let span = price / 10_000; // 1 bp of ladder width
        v.push(bid(
            &format!("bid/{label}/one_unit"),
            Inputs {
                amount_in: one_unit,
                min_bid: price - span,
                max_bid: price,
                capacity: one_unit * 100,
                exp,
                ..Inputs::default()
            },
        ));
        v.push(bid(
            &format!("bid/{label}/full_capacity"),
            Inputs {
                amount_in: one_unit * 100,
                min_bid: price - span,
                max_bid: price,
                capacity: one_unit * 100,
                exp,
                ..Inputs::default()
            },
        ));
        let ask = Inputs {
            kind: Kind::AmountInAsk,
            amount_in: one_unit,
            min_ask: price,
            max_ask: price + span,
            capacity: one_unit * 100,
            exp,
            ..Inputs::default()
        };
        v.push(record(&format!("ask_in/{label}/one_unit"), ask));
        // The inversion, seeded with the exact quote cost of that one base unit, so the pair
        // brackets the round trip: `amountOutAsk(amountInAsk(q)) == q`.
        let cost = amount_in_ask(one_unit, price, price + span, one_unit * 100, 0, exp)
            .expect("realistic ask leg must quote");
        v.push(record(
            &format!("ask_out/{label}/one_unit_exact_cost"),
            Inputs {
                kind: Kind::AmountOutAsk,
                amount_in: cost,
                ..ask
            },
        ));
        // One quote unit short must buy strictly less base.
        v.push(record(
            &format!("ask_out/{label}/one_unit_cost_minus_one"),
            Inputs {
                kind: Kind::AmountOutAsk,
                amount_in: cost - 1,
                ..ask
            },
        ));
    }

    // -- amountInAsk / amountOutAsk -------------------------------------------------------
    //
    // Capacity is BASE units on both of these now. `amountInAsk` is the primitive (base out ->
    // quote cost, ceiled); `amountOutAsk` is its exact integer inversion (quote in -> base out,
    // floored to what the budget strictly pays for).
    let ask_in = |name: &str, i: Inputs| {
        record(
            name,
            Inputs {
                kind: Kind::AmountInAsk,
                ..i
            },
        )
    };
    let ask_out = |name: &str, i: Inputs| {
        record(
            name,
            Inputs {
                kind: Kind::AmountOutAsk,
                ..i
            },
        )
    };

    v.push(ask_in(
        "ask_in/zero_amount_zero_capacity",
        Inputs::default(),
    ));
    v.push(ask_out(
        "ask_out/zero_amount_zero_capacity",
        Inputs::default(),
    ));
    v.push(ask_in(
        "ask_in/zero_capacity_nonzero_amount",
        Inputs {
            amount_in: 1,
            min_ask: lo,
            max_ask: hi,
            exp: 18,
            ..Inputs::default()
        },
    ));
    v.push(ask_out(
        "ask_out/zero_capacity_nonzero_amount",
        Inputs {
            amount_in: 1,
            min_ask: lo,
            max_ask: hi,
            exp: 18,
            ..Inputs::default()
        },
    ));
    // The ONLY shape that reverts with ZeroPrice: maxAsk == 0, i.e. a wholly zero ask ladder.
    // `validateLadder`'s strict `maxAsk > minBid` makes it unreachable through `PropPool`, but
    // both directions must agree on the guard, because without it the pool gives base away.
    v.push(ask_in(
        "ask_in/zero_price_flat_zero_ladder",
        Inputs {
            amount_in: 1,
            capacity: cap,
            exp: 18,
            ..Inputs::default()
        },
    ));
    v.push(ask_out(
        "ask_out/zero_price_flat_zero_ladder",
        Inputs {
            amount_in: 1,
            capacity: cap,
            exp: 18,
            ..Inputs::default()
        },
    ));
    // One unit of span over a million units of capacity: the single CEIL lifts the cost of one
    // base unit off zero, so the pool never sells base for nothing.
    v.push(ask_in(
        "ask_in/one_unit_span_costs_at_least_one_unit",
        Inputs {
            amount_in: 1,
            min_ask: 0,
            max_ask: 1,
            capacity: cap,
            exp: 18,
            ..Inputs::default()
        },
    ));
    v.push(ask_in(
        "ask_in/one_base_unit",
        Inputs {
            amount_in: 1,
            min_ask: lo,
            max_ask: hi,
            capacity: cap,
            exp: 18,
            ..Inputs::default()
        },
    ));
    v.push(ask_in(
        "ask_in/capacity_boundary_exact",
        Inputs {
            amount_in: cap,
            min_ask: lo,
            max_ask: hi,
            capacity: cap,
            exp: 18,
            ..Inputs::default()
        },
    ));
    v.push(ask_in(
        "ask_in/capacity_boundary_plus_one",
        Inputs {
            amount_in: cap + 1,
            min_ask: lo,
            max_ask: hi,
            capacity: cap,
            exp: 18,
            ..Inputs::default()
        },
    ));
    // Odd and even sizes over the same interval. The doubled midpoint keeps both exact, so the
    // pair pins that no halving is rounded: the two costs differ by exactly the marginal unit.
    v.push(ask_in(
        "ask_in/odd_size",
        Inputs {
            amount_in: 999_999,
            min_ask: lo,
            max_ask: hi,
            capacity: cap,
            exp: 18,
            ..Inputs::default()
        },
    ));
    v.push(ask_in(
        "ask_in/even_size",
        Inputs {
            amount_in: 1_000_000,
            min_ask: lo,
            max_ask: hi,
            capacity: cap,
            exp: 18,
            ..Inputs::default()
        },
    ));
    v.push(ask_in(
        "ask_in/mid_epoch_usage",
        Inputs {
            amount_in: 100_000,
            min_ask: lo,
            max_ask: hi,
            capacity: cap,
            used: cap / 2,
            exp: 18,
            ..Inputs::default()
        },
    ));
    v.push(ask_in(
        "ask_in/max_u56_prices_max_exp",
        Inputs {
            amount_in: MAX_AMOUNT,
            min_ask: MAX_PRICE - 1,
            max_ask: MAX_PRICE,
            capacity: MAX_AMOUNT,
            exp: MAX_PRICE_SCALE_EXP,
            ..Inputs::default()
        },
    ));
    v.push(ask_in(
        "ask_in/max_u56_prices_full_span",
        Inputs {
            amount_in: MAX_AMOUNT,
            min_ask: 1,
            max_ask: MAX_PRICE,
            capacity: MAX_AMOUNT,
            exp: 30,
            ..Inputs::default()
        },
    ));
    // The shared domain edge on the quote-returning ask path: exp = 0 means no decimal
    // alignment, so the cost is the raw ~2^152 product. uint256 holds it, MAX_AMOUNT_OUT
    // rejects it, u128 cannot represent it.
    v.push(ask_in(
        "ask_in/amount_out_of_domain_flat_ladder_exp_0",
        Inputs {
            amount_in: MAX_AMOUNT,
            min_ask: MAX_PRICE,
            max_ask: MAX_PRICE,
            capacity: MAX_AMOUNT,
            exp: 0,
            ..Inputs::default()
        },
    ));
    // One unit under it on the same shape, so the pair brackets the boundary.
    v.push(ask_in(
        "ask_in/amount_in_just_inside_the_domain",
        Inputs {
            amount_in: MAX_AMOUNT,
            min_ask: 1,
            max_ask: 1,
            capacity: MAX_AMOUNT,
            exp: 0,
            ..Inputs::default()
        },
    ));

    // The inversion's own control flow. `full` is the quote cost of the epoch's whole remaining
    // base, which is the ask side's quote-denominated capacity ceiling.
    let full = amount_in_ask(cap, lo, hi, cap, 0, 18).expect("cost of the whole epoch");
    v.push(ask_out(
        "ask_out/budget_is_the_whole_epoch",
        Inputs {
            amount_in: full,
            min_ask: lo,
            max_ask: hi,
            capacity: cap,
            exp: 18,
            ..Inputs::default()
        },
    ));
    v.push(ask_out(
        "ask_out/budget_one_over_the_whole_epoch",
        Inputs {
            amount_in: full + 1,
            min_ask: lo,
            max_ask: hi,
            capacity: cap,
            exp: 18,
            ..Inputs::default()
        },
    ));
    v.push(ask_out(
        "ask_out/budget_one_under_the_whole_epoch",
        Inputs {
            amount_in: full - 1,
            min_ask: lo,
            max_ask: hi,
            capacity: cap,
            exp: 18,
            ..Inputs::default()
        },
    ));
    v.push(ask_out(
        "ask_out/budget_buys_nothing",
        Inputs {
            amount_in: 1,
            min_ask: lo,
            max_ask: hi,
            capacity: cap,
            exp: 18,
            ..Inputs::default()
        },
    ));
    v.push(ask_out(
        "ask_out/mid_epoch_usage",
        Inputs {
            amount_in: full / 4,
            min_ask: lo,
            max_ask: hi,
            capacity: cap,
            used: cap / 2,
            exp: 18,
            ..Inputs::default()
        },
    ));
    v.push(ask_out(
        "ask_out/fully_used_epoch",
        Inputs {
            amount_in: 1,
            min_ask: lo,
            max_ask: hi,
            capacity: cap,
            used: cap,
            exp: 18,
            ..Inputs::default()
        },
    ));
    v.push(ask_out(
        "ask_out/max_u56_prices_full_span",
        Inputs {
            amount_in: MAX_AMOUNT_OUT,
            min_ask: 1,
            max_ask: MAX_PRICE,
            capacity: MAX_AMOUNT,
            exp: 30,
            ..Inputs::default()
        },
    ));

    // Every supported exponent, on both ask directions, with the inversion seeded by the exact
    // cost the forward direction just produced.
    for exp in 0..=MAX_PRICE_SCALE_EXP {
        let shape = Inputs {
            amount_in: 1_000_000,
            min_ask: 50_000_000_000_000_000,
            max_ask: 50_005_000_000_000_000,
            capacity: 4_000_000,
            used: 1_000_000,
            exp,
            ..Inputs::default()
        };
        v.push(ask_in(&format!("ask_in/price_scale_exp_{exp}"), shape));
        // Above MAX_AMOUNT_OUT the forward direction reverts, so there is no budget to invert;
        // the `ask_in` record above already asserts the shared revert.
        if let Ok(cost) = amount_in_ask(
            shape.amount_in,
            shape.min_ask,
            shape.max_ask,
            shape.capacity,
            shape.used,
            exp,
        ) {
            v.push(ask_out(
                &format!("ask_out/price_scale_exp_{exp}"),
                Inputs {
                    amount_in: cost,
                    ..shape
                },
            ));
        }
    }

    // -- amountInBid: the bid side's exact-output direction ---------------------------------
    //
    // `amountIn` carries the requested QUOTE output here; `expected` is the least base input.
    let bid_in = |name: &str, i: Inputs| {
        record(
            name,
            Inputs {
                kind: Kind::AmountInBid,
                ..i
            },
        )
    };

    v.push(bid_in(
        "bid_in/zero_amount_zero_capacity",
        Inputs::default(),
    ));
    v.push(bid_in(
        "bid_in/zero_capacity_nonzero_amount",
        Inputs {
            amount_in: 1,
            min_bid: lo,
            max_bid: hi,
            exp: 18,
            ..Inputs::default()
        },
    ));
    v.push(bid_in(
        "bid_in/fully_used_epoch",
        Inputs {
            amount_in: 1,
            min_bid: lo,
            max_bid: hi,
            capacity: cap,
            used: cap,
            exp: 18,
            ..Inputs::default()
        },
    ));
    // The whole epoch's output, and one unit more than it can deliver.
    let whole = amount_out_bid(cap, lo, hi, cap, 0, 18).expect("bid quote for the whole epoch");
    v.push(bid_in(
        "bid_in/whole_epoch_output",
        Inputs {
            amount_in: whole,
            min_bid: lo,
            max_bid: hi,
            capacity: cap,
            exp: 18,
            ..Inputs::default()
        },
    ));
    v.push(bid_in(
        "bid_in/one_more_than_the_epoch_can_deliver",
        Inputs {
            amount_in: whole + 1,
            min_bid: lo,
            max_bid: hi,
            capacity: cap,
            exp: 18,
            ..Inputs::default()
        },
    ));
    v.push(bid_in(
        "bid_in/one_quote_unit",
        Inputs {
            amount_in: 1,
            min_bid: lo,
            max_bid: hi,
            capacity: cap,
            exp: 18,
            ..Inputs::default()
        },
    ));
    v.push(bid_in(
        "bid_in/mid_epoch_usage",
        Inputs {
            amount_in: whole / 8,
            min_bid: lo,
            max_bid: hi,
            capacity: cap,
            used: cap / 2,
            exp: 18,
            ..Inputs::default()
        },
    ));
    v.push(bid_in(
        "bid_in/flat_ladder_exact_multiple",
        Inputs {
            amount_in: 1_000_000,
            min_bid: 1_000,
            max_bid: 1_000,
            capacity: cap,
            exp: 0,
            ..Inputs::default()
        },
    ));
    v.push(bid_in(
        "bid_in/flat_ladder_one_over_a_multiple",
        Inputs {
            amount_in: 1_000_001,
            min_bid: 1_000,
            max_bid: 1_000,
            capacity: cap,
            exp: 0,
            ..Inputs::default()
        },
    ));
    v.push(bid_in(
        "bid_in/max_u56_prices_exp_8",
        Inputs {
            amount_in: 1_000_000_000_000_000_000,
            min_bid: MAX_PRICE - 1,
            max_bid: MAX_PRICE,
            capacity: MAX_AMOUNT,
            exp: 8,
            ..Inputs::default()
        },
    ));
    // Every supported exponent, seeded by the forward direction so the round trip is asserted.
    for exp in 0..=MAX_PRICE_SCALE_EXP {
        let shape = Inputs {
            min_bid: 999_000_000_000_000,
            max_bid: 1_000_000_000_000_000,
            capacity: 4_000_000,
            used: 1_000_000,
            exp,
            ..Inputs::default()
        };
        let want = amount_out_bid(
            1_000_000,
            shape.min_bid,
            shape.max_bid,
            shape.capacity,
            shape.used,
            exp,
        )
        .expect("bid quote inside the domain");
        v.push(bid_in(
            &format!("bid_in/price_scale_exp_{exp}"),
            Inputs {
                amount_in: want,
                ..shape
            },
        ));
    }

    // -- validateLadder --------------------------------------------------------------------
    let val = |name: &str, i: Inputs| {
        record(
            name,
            Inputs {
                kind: Kind::ValidateLadder,
                ..i
            },
        )
    };

    v.push(val(
        "validate/ok_strictly_ordered",
        Inputs {
            min_bid: 10,
            max_bid: 20,
            min_ask: 30,
            max_ask: 40,
            min_price: 10,
            ..Inputs::default()
        },
    ));
    v.push(val(
        "validate/ok_touching_book",
        Inputs {
            min_bid: 10,
            max_bid: 20,
            min_ask: 20,
            max_ask: 40,
            min_price: 0,
            ..Inputs::default()
        },
    ));
    v.push(val(
        "validate/ok_one_unit_of_room",
        Inputs {
            min_bid: 10,
            max_bid: 10,
            min_ask: 10,
            max_ask: 11,
            min_price: 10,
            ..Inputs::default()
        },
    ));
    v.push(val(
        "validate/ok_max_u56",
        Inputs {
            min_bid: MAX_PRICE - 1,
            max_bid: MAX_PRICE - 1,
            min_ask: MAX_PRICE - 1,
            max_ask: MAX_PRICE,
            min_price: MAX_PRICE - 1,
            ..Inputs::default()
        },
    ));
    v.push(val(
        "validate/reject_bid_below_min_price",
        Inputs {
            min_bid: 9,
            max_bid: 20,
            min_ask: 30,
            max_ask: 40,
            min_price: 10,
            ..Inputs::default()
        },
    ));
    v.push(val(
        "validate/reject_min_price_beats_ordering_check",
        Inputs {
            min_bid: 0,
            max_bid: 0,
            min_ask: 0,
            max_ask: 0,
            min_price: 1,
            ..Inputs::default()
        },
    ));
    v.push(val(
        "validate/reject_inverted_ask",
        Inputs {
            min_bid: 10,
            max_bid: 20,
            min_ask: 30,
            max_ask: 29,
            min_price: 0,
            ..Inputs::default()
        },
    ));
    v.push(val(
        "validate/reject_crossed_book",
        Inputs {
            min_bid: 10,
            max_bid: 31,
            min_ask: 30,
            max_ask: 40,
            min_price: 0,
            ..Inputs::default()
        },
    ));
    v.push(val(
        "validate/reject_inverted_bid",
        Inputs {
            min_bid: 21,
            max_bid: 20,
            min_ask: 30,
            max_ask: 40,
            min_price: 0,
            ..Inputs::default()
        },
    ));
    v.push(val(
        "validate/reject_flat_ladder_strict_comparison",
        Inputs {
            min_bid: 10,
            max_bid: 10,
            min_ask: 10,
            max_ask: 10,
            min_price: 10,
            ..Inputs::default()
        },
    ));
    v.push(val("validate/reject_all_zero", Inputs::default()));
    v.push(val(
        "validate/reject_max_ask_equals_min_bid_at_u56_max",
        Inputs {
            min_bid: MAX_PRICE,
            max_bid: MAX_PRICE,
            min_ask: MAX_PRICE,
            max_ask: MAX_PRICE,
            min_price: 0,
            ..Inputs::default()
        },
    ));

    // -- executableTopBid / executableTopAsk -----------------------------------------------
    let top_bid = |name: &str, i: Inputs| {
        record(
            name,
            Inputs {
                kind: Kind::ExecutableTopBid,
                ..i
            },
        )
    };
    let top_ask = |name: &str, i: Inputs| {
        record(
            name,
            Inputs {
                kind: Kind::ExecutableTopAsk,
                ..i
            },
        )
    };

    v.push(top_bid(
        "top_bid/zero_capacity",
        Inputs {
            min_bid: lo,
            max_bid: hi,
            ..Inputs::default()
        },
    ));
    v.push(top_bid(
        "top_bid/unused_epoch",
        Inputs {
            min_bid: lo,
            max_bid: hi,
            capacity: cap,
            ..Inputs::default()
        },
    ));
    v.push(top_bid(
        "top_bid/half_used",
        Inputs {
            min_bid: lo,
            max_bid: hi,
            capacity: cap,
            used: cap / 2,
            ..Inputs::default()
        },
    ));
    v.push(top_bid(
        "top_bid/exactly_exhausted",
        Inputs {
            min_bid: lo,
            max_bid: hi,
            capacity: cap,
            used: cap,
            ..Inputs::default()
        },
    ));
    v.push(top_bid(
        "top_bid/over_exhausted",
        Inputs {
            min_bid: lo,
            max_bid: hi,
            capacity: cap,
            used: cap + 1,
            ..Inputs::default()
        },
    ));
    v.push(top_bid(
        "top_bid/one_short_of_exhausted",
        Inputs {
            min_bid: lo,
            max_bid: hi,
            capacity: cap,
            used: cap - 1,
            ..Inputs::default()
        },
    ));
    v.push(top_bid(
        "top_bid/max_u56_span",
        Inputs {
            min_bid: 0,
            max_bid: MAX_PRICE,
            capacity: MAX_AMOUNT,
            used: MAX_AMOUNT / 3,
            ..Inputs::default()
        },
    ));

    v.push(top_ask(
        "top_ask/zero_capacity_is_uint256_max",
        Inputs {
            min_ask: lo,
            max_ask: hi,
            ..Inputs::default()
        },
    ));
    v.push(top_ask(
        "top_ask/unused_epoch",
        Inputs {
            min_ask: lo,
            max_ask: hi,
            capacity: cap,
            ..Inputs::default()
        },
    ));
    v.push(top_ask(
        "top_ask/half_used",
        Inputs {
            min_ask: lo,
            max_ask: hi,
            capacity: cap,
            used: cap / 2,
            ..Inputs::default()
        },
    ));
    v.push(top_ask(
        "top_ask/exactly_exhausted",
        Inputs {
            min_ask: lo,
            max_ask: hi,
            capacity: cap,
            used: cap,
            ..Inputs::default()
        },
    ));
    v.push(top_ask(
        "top_ask/over_exhausted",
        Inputs {
            min_ask: lo,
            max_ask: hi,
            capacity: cap,
            used: cap + 1,
            ..Inputs::default()
        },
    ));
    v.push(top_ask(
        "top_ask/max_u56_span",
        Inputs {
            min_ask: 0,
            max_ask: MAX_PRICE,
            capacity: MAX_AMOUNT,
            used: MAX_AMOUNT / 3,
            ..Inputs::default()
        },
    ));

    v
}

/// Serialise the vector set as pretty JSON with a trailing newline.
///
/// # Errors
/// Propagates `serde_json` failures, which cannot occur for this shape.
pub fn to_json(vectors: &[Vector]) -> Result<String, serde_json::Error> {
    let mut s = serde_json::to_string_pretty(vectors)?;
    s.push('\n');
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn every_vector_reproduces_itself() {
        for v in generate() {
            assert!(verify(&v), "vector `{}` does not reproduce", v.name);
        }
    }

    #[test]
    fn names_are_unique() {
        let all = generate();
        let names: HashSet<&str> = all.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names.len(), all.len(), "duplicate vector names");
    }

    #[test]
    fn coverage_is_what_the_spec_asks_for() {
        let all = generate();
        assert!(all.len() >= 200, "only {} vectors", all.len());
        for kind in [
            "amountOutBid",
            "amountInBid",
            "amountInAsk",
            "amountOutAsk",
            "validateLadder",
            "executableTopBid",
            "executableTopAsk",
        ] {
            assert!(all.iter().any(|v| v.kind == kind), "no vectors for {kind}");
        }
        // Every supported exponent, on all four quote paths.
        for exp in 0..=MAX_PRICE_SCALE_EXP {
            let e = exp.to_string();
            for kind in ["amountOutBid", "amountInBid", "amountInAsk"] {
                assert!(
                    all.iter().any(|v| v.kind == kind && v.priceScaleExp == e),
                    "{kind} misses priceScaleExp {exp}"
                );
            }
        }
        // `amountOutAsk` is seeded from `amountInAsk`, so it covers every exponent whose forward
        // cost is inside the shared domain — which is all of them here, but assert it rather
        // than assume it.
        for exp in 0..=MAX_PRICE_SCALE_EXP {
            let e = exp.to_string();
            assert!(
                all.iter()
                    .any(|v| v.kind == "amountOutAsk" && v.priceScaleExp == e),
                "amountOutAsk misses priceScaleExp {exp}"
            );
        }
        // Every named Solidity revert is exercised — including AmountOutOfDomain, which the
        // MAX_AMOUNT_OUT amendment turned from a port-only refusal into a shared revert.
        for err in SOLIDITY_ERRORS {
            assert!(
                all.iter().any(|v| v.expectRevert == err),
                "no vector reverts with {err}"
            );
        }
        // The two kinds that RETURN a quote amount must exercise the shared domain edge. The two
        // whose *argument* is quote-denominated cannot: `MAX_AMOUNT_OUT == u128::MAX`, so their
        // on-chain input bound is enforced by the argument's type here and emits no vector.
        for kind in ["amountOutBid", "amountInAsk"] {
            assert!(
                all.iter()
                    .any(|v| v.kind == kind && v.expectRevert == "AmountOutOfDomain"),
                "{kind} never reaches MAX_AMOUNT_OUT"
            );
        }
        // The ask side's ZeroPrice guard is asserted in both directions.
        for kind in ["amountInAsk", "amountOutAsk"] {
            assert!(
                all.iter()
                    .any(|v| v.kind == kind && v.expectRevert == "ZeroPrice"),
                "{kind} never guards a zero ask ladder"
            );
        }
        // Nothing outside the agreed set can appear in expectRevert.
        for v in &all {
            assert!(
                v.expectRevert.is_empty() || SOLIDITY_ERRORS.contains(&v.expectRevert.as_str()),
                "vector `{}` expects the unknown revert `{}`",
                v.name,
                v.expectRevert
            );
        }
        // The uint256 sentinel is present exactly where it should be.
        assert!(all.iter().any(|v| v.expected == UINT256_MAX_DEC));
    }

    #[test]
    fn solidity_errors_is_exhaustive() {
        // Every CurveError with a Solidity counterpart must be listed, and nothing else.
        let variants = [
            CurveError::AmountExceedsCapacity,
            CurveError::ZeroCapacity,
            CurveError::ZeroPrice,
            CurveError::CrossedBook,
            CurveError::BidBelowMinPrice,
            CurveError::AmountOutOfDomain,
            CurveError::ArithmeticPanic,
            CurveError::DomainOverflow,
        ];
        let listed: Vec<&str> = variants
            .iter()
            .filter_map(|e| e.solidity_signature())
            .map(|s| s.trim_end_matches("()"))
            .collect();
        assert_eq!(listed, SOLIDITY_ERRORS.to_vec());
    }

    /// The members of the Solidity `struct Vector` documented at the top of this module, in
    /// declaration order. Kept as data so the ordering contract is testable.
    const FIELDS: [&str; 13] = [
        "amountIn",
        "capacity",
        "expectRevert",
        "expected",
        "kind",
        "maxAsk",
        "maxBid",
        "minAsk",
        "minBid",
        "minPrice",
        "name",
        "priceScaleExp",
        "used",
    ];

    /// The documented field order must equal the byte-wise sort of the JSON keys, and the
    /// emitted file must use that same order, so the file is stable under any key-sorting
    /// decoder and the `expectRevert`/`expected` pair can never silently transpose.
    #[test]
    fn field_order_is_byte_wise_sorted() {
        let mut sorted = FIELDS;
        sorted.sort_unstable();
        assert_eq!(
            FIELDS, sorted,
            "the documented struct order is not byte-wise sorted"
        );
        // The one pair where byte-wise and case-insensitive alphabetical disagree: 'R' < 'e'.
        // Swapping these two silently turns every revert case into a success case.
        let at = |k: &str| FIELDS.iter().position(|x| *x == k).expect("field listed");
        assert!(at("expectRevert") < at("expected"));

        let json = serde_json::to_string(&generate()[0]).unwrap();
        let mut previous = 0usize;
        for f in FIELDS {
            let found = json
                .find(&format!("\"{f}\":"))
                .unwrap_or_else(|| panic!("field `{f}` is missing from the emitted JSON"));
            assert!(
                found >= previous,
                "field `{f}` is out of order in the emitted JSON"
            );
            previous = found;
        }
        // No field is present that the Solidity struct does not declare.
        let decoded: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&json).unwrap();
        assert_eq!(
            decoded.len(),
            FIELDS.len(),
            "struct arity changed; update the Solidity struct"
        );
    }

    #[test]
    fn json_round_trips() {
        let all = generate();
        let json = to_json(&all).unwrap();
        let back: Vec<Vector> = serde_json::from_str(&json).unwrap();
        assert_eq!(all, back);
    }
}
