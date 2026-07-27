//! The quote updater for the DuBu prop AMM on GIWA.
//!
//! `PropPool` holds inventory and stores a four-point price ladder per pair. It has no strategy
//! of its own — no reserves curve, no spread model, nothing that reacts to a price. Until
//! something pushes a new ladder, the one it holds is the one it quotes, until `maxStaleSecs`
//! expires and it quotes nothing at all. This crate is that something.
//!
//! ```text
//!   Binance bookTicker (ws)                 GIWA (http, polled)
//!            |                                      |
//!            v                                      v
//!   feed ──> fair_value ──> ladder ──> policy ──> tx ──> PropPool.updateQuote
//!            (micro-price)  (dubu-core)  (send?)         PropPool.refreshCapacity
//!                                          ^
//!                                          |
//!                                   risk (killswitches)
//! ```
//!
//! # Two sources, deliberately not one
//!
//! Fair value comes from **public exchange market data**; the on-chain deviation bound is
//! **Pyth's** job, later. Pyth is live on GIWA and `PropPool` is designed to grow a deviation
//! check against it — but if this bot also priced from Pyth, that check would be comparing a
//! value against itself and would catch nothing. The two must stay independent.
//!
//! The market-data connection is read-only: no API key, no account endpoint, no order entry,
//! and no way to add one without a signing step that does not exist here. The book is unhedged
//! because a Korean corporate real-name exchange account is not available, so there is no
//! account to hedge into.
//!
//! # The pool's tokens have no market
//!
//! `mWETH`, `mWBTC` and `mUSDC` are mocks we deployed. Nothing trades them anywhere. Each pair
//! is priced off the **real** asset its mock stands in for — pairId 1 follows `ETHUSDT`, pairId
//! 2 follows `BTCUSDT` — which is what makes the demo move and what will make a markout study
//! mean anything. See [`config::PairConfig`].
//!
//! # Two measured constraints shape the design
//!
//! 1. **No `eth_subscribe`, no websocket.** GIWA's RPC answers 405. Chain state is polled, on a
//!    configurable interval, and every consumer treats the view as something that has an age.
//! 2. **The public RPC rate-limits.** One `eth_call` per poll cycle via Multicall3, a local
//!    token bucket that refuses rather than queues, and a 429 that becomes a liveness state
//!    (widen, then halt) rather than a retry. See [`chain`].
//!
//! # Every integer path is `dubu-core`'s
//!
//! The curve exists in exactly one place. This crate calls `dubu_core` for the skew, the spread
//! projection, the inverse solve, the validator, the round-trip check, the executable top, the
//! inventory mark, and the calldata packing. There is no second implementation of the curve
//! here, and [`ladder`] and [`policy`] both say so at the top for the benefit of whoever is
//! tempted to write `price * bps / 10_000` inline.
//!
//! # Dry run by default
//!
//! [`config::TxConfig::transmit_allowed`] must be explicitly `true` for anything to be
//! broadcast. Absent means dry run, `--dry-run` forces it off, and there is no flag that turns
//! it on.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![warn(clippy::all)]

pub mod chain;
pub mod config;
pub mod fair_value;
pub mod feed;
pub mod ladder;
pub mod policy;
pub mod risk;
pub mod tx;
pub mod units;
