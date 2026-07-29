//! Exact integer mirror of the DuBu prop-AMM curve, plus the off-chain pieces that have no
//! on-chain counterpart.
//!
//! archi_v2 §5.3 states the requirement this crate exists to satisfy:
//!
//! > the same formula must exist in three places: forward (execution), inverse (ladder
//! > solving), validate (row checking).
//!
//! Those three places are [`curve`], [`inverse`], and [`curve::validate_ladder`], and they share
//! one arithmetic core ([`math`]) precisely so they cannot drift. `PropCurve` no longer quantises
//! the intermediate price at all — it folds the price into the amount and rounds exactly once, in
//! the pool's favour — so [`curve`] runs the whole quote leg through [`math::U256`], and
//! [`inverse`] floors the impact it bakes into the ladder so the realised average can only land
//! on the pool's side of the target.
//!
//! # The five things in here
//!
//! | module | what | on-chain twin |
//! |---|---|---|
//! | [`curve`]   | both directions of both sides, validation, executable top | `PropCurve.sol`, exact port |
//! | [`inverse`] | target price + capture size -> widest safe ladder | none |
//! | [`ladder`]  | strategy knobs -> four prices, always valid | none |
//! | [`pack`]    | the `updateQuote` calldata word | the pool's decoder |
//! | [`rfq`]     | the signed-order EIP-712 digest | `PmmSettle.sol`, exact port |
//!
//! [`rfq`] belongs to the *other* path — a firm off-chain quote settled on chain, rather than a
//! ladder the pool prices against — but it is here for the same reason everything else is: the
//! engine computes a value it then believes the chain agrees with, and a disagreement is silent.
//!
//! # Why this has to be exact
//!
//! The engine posts a ladder and then believes it is quoting a particular price. If the two
//! implementations disagree, one of two things happens, and both are silent: either the
//! chain rejects a row the engine thought was valid — leaving the previous quote to go stale
//! into an adverse-selection window — or it accepts a row that executes at a price the
//! engine did not intend. Neither shows up as an error anywhere. The differential vectors in
//! [`vectors`] are the only thing standing between the two.
//!
//! # Numeric domain
//!
//! Solidity computes in `uint256`; this crate computes in `u128` with 256-bit intermediates
//! only where a range proof shows they are needed. See [`curve`] for the bounds table and
//! the two places the narrowing is visible.
//!
//! # Example
//!
//! ```
//! use dubu_core::{curve, inverse::{solve_bid, SolveInput}, pack::QuoteWord, curve::Ladder};
//!
//! // "Pay a taker an average of 3_000.000000 USDC per WETH over the first 5 WETH,
//! //  and give me the widest ladder that does it."
//! let target = 3_000_000_000_000_000u128;           // priceScaleExp = 24
//! let input = SolveInput {
//!     target,
//!     capture: 5_000_000_000_000_000_000,           // 5 WETH
//!     capacity: 20_000_000_000_000_000_000,         // 20 WETH posted this epoch
//!     requested_width: curve::MAX_PRICE,
//!     min_price: target * 99 / 100,
//!     max_price: target * 101 / 100,
//! };
//! let sol = solve_bid(&input).unwrap();
//!
//! // The realised average lands on the pool's side of the target, within one price unit.
//! let realised =
//!     curve::avg_bid_price(sol.low, sol.high, input.capacity, 0, sol.effective_capture).unwrap();
//! assert!(realised <= target && target - realised <= 1);
//!
//! // Both sides of the ask are available too: base out -> quote cost (ceiled), and its exact
//! // integer inverse, quote in -> base out (floored to what the budget strictly pays for).
//! let one_weth = 1_000_000_000_000_000_000u128;
//! let cost = curve::amount_in_ask(one_weth, target, target + 1_000, input.capacity, 0, 24).unwrap();
//! let back = curve::amount_out_ask(cost, target, target + 1_000, input.capacity, 0, 24).unwrap();
//! // At `priceScaleExp = 24` one USDC unit is worth ~3.3e8 wei, so the ceiled cost buys back at
//! // least the WETH it priced — and never more than it pays for, which is the invariant.
//! assert!(back >= one_weth);
//! assert!(curve::amount_in_ask(back, target, target + 1_000, input.capacity, 0, 24).unwrap() <= cost);
//!
//! // And the row packs into a single calldata word.
//! let word = QuoteWord::new(1, Ladder {
//!     min_bid: sol.low,
//!     max_bid: sol.high,
//!     min_ask: sol.high + 1,
//!     max_ask: sol.high + 2,
//! });
//! assert_eq!(QuoteWord::unpack(&word.pack().unwrap()).unwrap(), word);
//! ```

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![warn(clippy::all)]
// Panic discipline, enforced rather than remembered.
//
// Every recoverable condition in this crate returns a `Result`: an RPC that fails, a venue that
// goes quiet, a config that will not parse, a curve solve outside its domain. None of those may
// take the process down, because a maker that dies mid-cycle leaves a ladder on chain that nobody
// is updating.
//
// The exceptions are exempted one at a time, at the site, with the bound that makes each safe
// written next to it — a blanket `allow` at the crate root would leave the next one unexamined.
// Not enabled for `cfg(test)`: an assertion is a panic, and that is what a test is for.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )
)]

pub mod curve;
pub mod error;
pub mod inverse;
pub mod ladder;
pub mod math;
pub mod pack;
pub mod rfq;
pub mod vectors;

pub use curve::{
    amount_in_ask, amount_in_bid, amount_out_ask, amount_out_bid, avg_ask_price, avg_bid_price,
    executable_top_ask, executable_top_bid, validate_ladder, Ladder, MAX_AMOUNT, MAX_AMOUNT_OUT,
    MAX_PRICE, MAX_PRICE_SCALE_EXP, NO_ASK,
};
pub use error::{CurveError, LadderError, PackError};
pub use inverse::{
    solve_ask, solve_bid, solve_two_sided, Solution, SolveInput, TwoSided, WidthBinding,
};
pub use ladder::LadderBuilder;
pub use pack::QuoteWord;
pub use rfq::{Domain as RfqDomain, Order as RfqOrder, ORDER_TYPEHASH, ORDER_TYPE_STRING};
