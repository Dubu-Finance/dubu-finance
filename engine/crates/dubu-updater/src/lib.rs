//! The quote updater for the DuBu prop AMM on GIWA.
//!
//! `PropPool` holds inventory and stores a four-point price ladder per pair. It has no strategy
//! of its own — no reserves curve, no spread model, nothing that reacts to a price. Until
//! something pushes a new ladder, the one it holds is the one it quotes, until `maxStaleSecs`
//! expires and it quotes nothing at all. This crate is that something.
//!
//! ```text
//!   Binance bookTicker (wss)      Nodit newHeads (wss) ──> wakes the loop, 1s
//!            |                    GIWA flashblocks (http, `pending`) ──> state, ~200ms
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
//! # The loop is event-driven
//!
//! A `newHeads` subscription over a dedicated Nodit websocket wakes the loop at the chain's 1s
//! cadence; the reads a cycle needs then happen against the flashblocks endpoint's `pending`
//! tag, which is ~200ms preconfirmed state and therefore fresher than the head that triggered
//! it. See [`chain`] for the endpoint table and [`chain::heads`] for the subscription.
//!
//! This replaced a polling timer, which existed only because GIWA's public RPC answers 405 to a
//! websocket upgrade and rate-limits. What remains of that design is kept on its own merits and
//! labelled as such: Multicall3 batching (one `eth_call` per head, whatever the pair count),
//! backoff (a dedicated endpoint is not an infinite one), and a fallback timer underneath the
//! subscription so a dead or *silent* socket degrades into polling instead of stalling.
//!
//! Liveness is two signals on one ladder — reads landing, and the block number advancing. The
//! second is what stops an endpoint that answers cheerfully about a stopped chain from reading
//! as healthy forever.
//!
//! # No secret is ever printed
//!
//! The endpoint URLs carry the API key in their path, so [`config::EndpointUrl`] makes them
//! unprintable: `Display` and `Debug` are both redacted to `scheme://host/***`, and the real
//! string is reachable only through `expose()`. The config file holds `${NODIT_API_KEY}`
//! templates, never a literal, and the value comes from the environment or a gitignored `.env`.
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
