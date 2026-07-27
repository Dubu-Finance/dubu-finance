//! The quote updater for the DuBu prop AMM on GIWA.
//!
//! `PropPool` holds inventory and stores a four-point price ladder per pair. It has no strategy
//! of its own — no reserves curve, no spread model, nothing that reacts to a price. Until
//! something pushes a new ladder, the one it holds is the one it quotes, until `maxStaleSecs`
//! expires and it quotes nothing at all. This crate is that something.
//!
//! ```text
//!   Binance / OKX / Bybit (wss)   Nodit newHeads (wss) ──> wakes the loop, 1s
//!            |                    GIWA flashblocks (http, `pending`) ──> state, ~200ms
//!            v                                      v
//!   feed ──> fair_value ──> spread ──> skew ──> ladder ──> policy ──> tx ──> updateQuote
//!   (n venues) (MAD +      (s0+s1σ)    (A-S)  (dubu-core)  (send?)          refreshCapacity
//!    |         quorum)                                        ^
//!    |                                                        |
//!    └──────> jump ────────────────────────────────────> tx   risk (killswitches)
//!             (fast lane, 200ms, no chain read)
//! ```
//!
//! # Two defences against adverse selection, and they do different jobs
//!
//! The measured problem is not diffusion. A simulator run over the pool as configured found that
//! **pure diffusion produces zero informed fills** — at 1 bp/s with 2 s of quote latency the posted
//! quote is never wrong by more than ~1.4 bp against a 5 bp half-spread — and that one 100 bp jump
//! produced the entire -$16,580. The controlling arithmetic is
//!
//! ```text
//! absorption = half_spread + width / 2
//! loss       = (reference error - absorption) x exposed depth
//! ```
//!
//! [`spread`] widens the absorption limit with volatility, because jumps cluster with it. It
//! cannot solve the problem on its own: the sweep needs a **constant 30 bp** half-spread before the
//! sign flips against a 100 bp jump, and a constant 30 bp is UniV2's fee.
//!
//! [`jump`] is what handles the rest. Above the absorption limit no spread a market maker would
//! post is profitable, so the pool stops quoting: `refreshCapacity(pair, 0, 0)`, held until the
//! reference settles. It runs on a **fast lane** between quote cycles, off the in-memory feed
//! snapshots, with no chain read and none of [`policy`]'s gates in front of it — that is the one
//! place in this crate where latency genuinely matters, because it is a race against a searcher.
//!
//! Both read the **one** volatility estimator in [`skew::Volatility`]. There is deliberately no
//! second one: two estimates of how volatile the market is would be two things to keep consistent
//! and two ways to be wrong.
//!
//! # Several venues, deliberately not one
//!
//! Fair value comes from **several independent public market-data websockets**, combined by
//! [`fair_value::combine`] after a MAD outlier filter and behind a quorum rule. One venue would
//! make the ladder's own price bounds vacuous in the way a single-source oracle is: the row and
//! the limits that are supposed to check it would come from the same number, so a feed that is
//! *confidently* wrong produces a ladder nothing catches. Losing a venue is an explicit logged
//! event, a majority disagreeing stops quoting outright, and neither is ever a silent absence.
//!
//! The on-chain deviation bound is still **Pyth's** job, later. Pyth is live on GIWA and
//! `PropPool` is designed to grow a deviation check against it — but if this bot also priced from
//! Pyth, that check would be comparing a value against itself and would catch nothing. The two
//! must stay independent, and the cross-venue filter is not a substitute: an error correlated
//! across every venue is invisible to it.
//!
//! Every market-data connection is read-only: no API key, no account endpoint, no order entry,
//! and no way to add one without a signing step that does not exist here. The book is unhedged
//! because a Korean corporate real-name exchange account is not available, so there is no
//! account to hedge into — which is also why [`skew`] exists. With no hedge venue, the
//! Avellaneda–Stoikov reservation price is the only inventory control there is.
//!
//! # The pool's tokens have no market
//!
//! `mWETH`, `mWBTC` and `mUSDC` are mocks we deployed. Nothing trades them anywhere. Each pair
//! is priced off the **real** asset its mock stands in for — pairId 1 follows `ETHUSDT`, pairId
//! 2 follows `BTCUSDT` — which is what makes the demo move and what will make a markout study
//! mean anything. Each venue spells that asset differently, so the mapping is configuration:
//! see [`config::PairVenues`].
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
pub mod flow;
pub mod jump;
pub mod ladder;
pub mod markout;
pub mod policy;
pub mod quoting;
pub mod risk;
pub mod skew;
pub mod spread;
pub mod tx;
pub mod units;
