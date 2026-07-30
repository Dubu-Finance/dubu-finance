//! Chain access: JSON-RPC transport, the runaway guard, and the batched read that produces a
//! [`ChainView`].
//!
//! # The loop is driven by `newHeads`, not by a timer
//!
//! This module used to open with "GIWA has no `eth_subscribe` and no websocket — the endpoint
//! answers 405", and everything below it was shaped by that. A dedicated Nodit endpoint removed
//! the constraint: `eth_subscribe("newHeads")` over its WSS works, and heads arrive at the
//! chain's 1s cadence. [`heads`] owns that subscription and the quote loop wakes on it, doing
//! the reads a cycle needs when there is actually new state rather than on an arbitrary tick.
//!
//! Reads still produce a view with an age, and [`ChainView::at`] still carries it, because that
//! never depended on how the read was triggered.
//!
//! # What survived the rewrite, and why
//!
//! Three things look like leftovers from the polling design and are not. They are here on their
//! own merits, and a later reader should not mistake them for debris:
//!
//! 1. **Multicall3 batching.** One `eth_call` per head instead of six, whatever the pair count:
//!    block number, block timestamp, every pair's `snapshot` and every token balance in one
//!    `aggregate3`. This was *motivated* by the rate limit and is *justified* without it — six
//!    round trips where one would do is worse on latency, and worse on consistency, because a
//!    batch is answered at one block while six separate calls can straddle a block boundary and
//!    produce a view that never existed.
//! 2. **Backoff**, in [`Limiter`]. A dedicated endpoint is not an infinite one. More to the
//!    point, the failure this defends against is *ours*: with no penalty window, any transient
//!    upstream error turns the bot into a retry flood, which is how a small outage becomes a
//!    large one.
//! 3. **The health state machine**, [`ChainHealth`] — same thresholds, one new input. See below.
//!
//! What did **not** survive: the poll timer as the primary driver, demoted to
//! [`crate::config::ChainConfig::fallback_poll_interval_ms`], a floor under the subscription
//! rather than the thing that runs the bot; and the token-bucket *sizing* that assumed a hostile
//! budget, along with the config cross-check that refused a poll interval whose steady-state
//! rate exceeded it. [`Limiter`] remains as a fuse against a runaway loop, not as a budget the
//! normal path is expected to press against.
//!
//! # Liveness has two signals now, and they fail differently
//!
//! [`ChainHealth`] escalates `Healthy -> Degraded -> Down` on the same thresholds it always
//! did. What changed is what feeds it:
//!
//! * **Reads landing.** A failing `eth_call` is a failing endpoint. This is the original signal.
//! * **The block number advancing.** New, and it closes a real hole: an endpoint that answers
//!   every request cheerfully about a chain that has stopped used to read as perfectly healthy
//!   forever, because "the RPC replied" was the only thing being measured. Progress is taken
//!   from the block number the read itself returns, so a frozen chain escalates on exactly the
//!   same ladder as an unreachable one.
//!
//! The head watchdog in [`heads`] deliberately does **not** feed this directly. When heads stop,
//! the loop falls back to its timer and the *next read* answers the question that matters: if
//! the block number is still climbing, only the socket died and quoting continues; if it is
//! frozen too, the chain is genuinely down and the ladder escalates to a halt. Wiring a silent
//! websocket straight to `Down` would withdraw quotes over a quiet socket on a healthy chain,
//! which is the wrong trade — and never noticing would be the worse one.
//!
//! # Which endpoint, and which block tag
//!
//! | use | endpoint | tag |
//! |---|---|---|
//! | `newHeads`, the loop's clock | Nodit WSS | — |
//! | every state read | flashblocks | `pending` |
//! | startup metadata | ordinary (Nodit HTTPS) | `latest` |
//! | nonce, fees, submit, receipts | ordinary (Nodit HTTPS) | `pending` / `latest` |
//!
//! Heads say *when* to look; they are not what is read.
//!
//! **Most of the freshness is the tag, not the host**, and this used to say otherwise. Measured on
//! GIWA by polling `snapshot(1).minBid` — which changes on every push, unlike `updatedAt`, which
//! is `block.timestamp` and quantised to a second — against the moment the updater sent it:
//!
//! ```text
//!   flash `pending`  vs  ordinary `latest`     ordinary lags  ~871ms   <- the TAG
//!   flash `pending`  vs  ordinary `pending`    ordinary lags   ~82ms   <- the HOST
//!   send             ->  included in a preconfirmation   296/440/508ms (min/median/max)
//!   flashblock cadence                          327ms median, 3.0 per 1s block
//! ```
//!
//! So `pending` over `latest` is worth most of a block, and the flashblocks host is worth a
//! further ~82ms on top of that. Both are worth having for a maker, but the second is a tenth of
//! what the first is, and the earlier framing credited the host with all of it.
//!
//! The 440ms is the one that matters for how this bot behaves. It is measured by timing how long
//! after `eth_sendRawTransaction` returns the sender's nonce advances on the `pending` tag, over
//! six transactions — a direct observation rather than a correlation, which an earlier attempt got
//! wrong by matching a send against a state change some *other* send had caused and concluding a
//! physically impossible 5ms. Seoul to the sequencer is ~70ms of round trip on its own.
//!
//! 440ms against a 327ms flashblock is roughly half an interval of waiting plus propagation, which
//! is what arriving at a uniformly random point in the interval predicts.
//!
//! What follows from it: the quote is effective in ~440ms and a confirmed receipt takes about a
//! second, so anything gating on a *confirmed* receipt is waiting about twice as long as the chain
//! requires. See `tx::Sender`'s in-flight gate.
//!
//! Its `latest` **lags the ordinary RPC by about two blocks**, so reading `latest` from it is
//! strictly worse than reading `latest` from the ordinary endpoint. Transactions go to the ordinary endpoint because that is the canonical
//! view, and a nonce read from a preconfirmed state that later reorganises is a stuck
//! transaction.

pub mod heads;
pub mod swaps;
pub mod view;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use alloy_primitives::{Address, Bytes};
use alloy_sol_types::SolCall;
use dubu_core::curve::Ladder;
use serde_json::json;

use crate::config::EndpointUrl;

/// Generated ABI bindings for everything this bot calls.
///
/// Struct field order is load-bearing: these are tuples on the wire, so a field in the wrong
/// place decodes silently into the wrong meaning. They mirror `IPropPool.PairSnapshot` and
/// `PropPool.PairConfig` exactly.
#[allow(missing_docs)]
pub mod abi {
    alloy_sol_types::sol! {
        #[derive(Debug)]
        struct PairSnapshotAbi {
            uint56 minBid;
            uint56 maxBid;
            uint56 minAsk;
            uint56 maxAsk;
            uint32 updatedAt;
            uint96 bidCapacity;
            uint96 askCapacity;
            uint96 bidUsed;
            uint96 askUsed;
            uint32 capGen;
            uint32 usedGen;
            uint16 flags;
            uint8 priceScaleExp;
            uint32 maxStaleSecs;
        }

        #[derive(Debug)]
        struct PairConfigAbi {
            address base;
            address quote;
            uint8 priceScaleExp;
            uint32 maxStaleSecs;
            uint56 minPrice;
            uint96 minBaseReserve;
            uint96 minQuoteReserve;
            bool exists;
        }

        #[derive(Debug)]
        struct Call3 {
            address target;
            bool allowFailure;
            bytes callData;
        }

        #[derive(Debug)]
        struct Call3Result {
            bool success;
            bytes returnData;
        }

        function snapshot(uint16 pairId) external view returns (PairSnapshotAbi);
        function pairConfig(uint16 pairId) external view returns (PairConfigAbi);
        function effectiveCapacity(uint16 pairId)
            external
            view
            returns (uint96 bidCapacity, uint96 askCapacity, uint16 decaySecs);
        function pairCount() external view returns (uint16);
        function updater() external view returns (address);

        function updateQuote(uint256[] packed) external;
        function refreshCapacity(uint16 pairId, uint96 bidCapacity, uint96 askCapacity) external;

        function balanceOf(address account) external view returns (uint256);
        function allowance(address owner, address spender) external view returns (uint256);
        function decimals() external view returns (uint8);

        function aggregate3(Call3[] calls) external payable returns (Call3Result[]);
        function getBlockNumber() external view returns (uint256);
        function getCurrentBlockTimestamp() external view returns (uint256);
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Anything that can go wrong talking to a node.
#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    /// The endpoint answered HTTP 429. A liveness event, not noise.
    #[error("{endpoint}: rate limited (HTTP 429); backing off for {backoff_ms}ms")]
    RateLimited {
        /// Which endpoint.
        endpoint: &'static str,
        /// How long the penalty window is.
        backoff_ms: u64,
    },
    /// A request was refused locally because a rate-limit penalty is still running. No socket
    /// was opened.
    #[error("{endpoint}: in rate-limit backoff for another {remaining_ms}ms; request not sent")]
    BackingOff {
        /// Which endpoint.
        endpoint: &'static str,
        /// Time left in the penalty window.
        remaining_ms: u64,
    },
    /// A request was refused locally because the token bucket is empty.
    #[error("{endpoint}: local request budget exhausted; request not sent")]
    BudgetExhausted {
        /// Which endpoint.
        endpoint: &'static str,
    },
    /// The HTTP request failed outright.
    #[error("{endpoint}: transport failure: {source}")]
    Transport {
        /// Which endpoint.
        endpoint: &'static str,
        /// Underlying reqwest failure.
        source: reqwest::Error,
    },
    /// A non-2xx status that is not 429.
    #[error("{endpoint}: HTTP {status}: {body}")]
    Http {
        /// Which endpoint.
        endpoint: &'static str,
        /// Status code.
        status: u16,
        /// Truncated response body.
        body: String,
    },
    /// A well-formed JSON-RPC error object.
    #[error("{endpoint}: node error {code}: {message}")]
    Node {
        /// Which endpoint.
        endpoint: &'static str,
        /// JSON-RPC error code.
        code: i64,
        /// JSON-RPC error message.
        message: String,
    },
    /// The response did not decode.
    #[error("could not decode {what}: {detail}")]
    Decode {
        /// What was being decoded.
        what: &'static str,
        /// Why it failed.
        detail: String,
    },
}

impl RpcError {
    /// True when the request provably never left this process.
    ///
    /// Both variants are refused by the local limiter before a socket is opened, so nothing the
    /// caller was about to do has happened yet. The transmit path needs this: a send that failed
    /// *after* going out may have consumed a nonce, and a send that never went out cannot have.
    /// Treating the two the same is what made the sender re-read its nonce thousands of times for
    /// transactions it had not sent.
    #[must_use]
    pub const fn never_sent(&self) -> bool {
        matches!(self, Self::BackingOff { .. } | Self::BudgetExhausted { .. })
    }

    /// True when the failure is the *endpoint's*, so another endpoint is worth trying.
    ///
    /// [`Self::Node`] is deliberately excluded. A JSON-RPC error means this node parsed the
    /// request and refused it — "execution reverted", "nonce too low", "already known" — and
    /// every other node holding the same chain would refuse it identically. Retrying it elsewhere
    /// would turn one refusal into several, and on the transmit path it would mean broadcasting
    /// the same intent twice on the guess that the first one did not count.
    #[must_use]
    pub const fn is_endpoint_fault(&self) -> bool {
        match self {
            Self::Transport { .. }
            | Self::Http { .. }
            | Self::RateLimited { .. }
            | Self::BackingOff { .. }
            | Self::BudgetExhausted { .. } => true,
            // A node error usually means the node understood the request and refused it, so
            // another node would refuse it identically and moving on is pointless. These codes are
            // the exception: they say the endpoint refused to serve *this caller*, which is a
            // property of the key, not of the request. A different key answers it fine.
            //
            // This is not theoretical, and it has now happened twice with different codes. First
            // `403 API usage quota has been exceeded` from Nodit; then, after that was whitelisted,
            // `-32016 over rate limit` from GIWA's public endpoint -- a code that was not on the
            // list, so the pool gave up on the first endpoint again while seven alternatives sat
            // behind it. The list is the wrong shape for the problem: anything meaning "not you,
            // not now" belongs on it, and each new venue brings its own spelling.
            //
            // Safe on the transmit path, where a retry would otherwise risk a second broadcast:
            // these are all refusals issued *before* the request was processed, so nothing was
            // submitted and there is nothing to double.
            Self::Node { code, .. } => matches!(code, 401 | 403 | 429 | -32005 | -32016),
            Self::Decode { .. } => false,
        }
    }

    /// Whether this is the rate limit rather than some other failure. Drives the health state
    /// machine's `reason` field and nothing else — both kinds count as a failed poll.
    #[must_use]
    pub const fn is_rate_limit(&self) -> bool {
        matches!(
            self,
            Self::RateLimited { .. } | Self::BackingOff { .. } | Self::BudgetExhausted { .. }
        )
    }

    /// True when the upstream transaction pool refused the transaction because it is **full**.
    ///
    /// This is backpressure, not a fault, and the distinction is the whole reason it has a
    /// predicate of its own. A `-32003` says nothing about the transaction and everything about the
    /// pool it was offered to: the same bytes are perfectly valid a moment later, the nonce was not
    /// consumed, and the correct response is to stop sending rather than to log a failure and try
    /// the next intent. Treating it per-transaction is what makes a brief upstream hiccup produce
    /// ~1600 error lines a minute at 27 tx/s — see [`crate::tx::Sender::send_batch`], which pauses
    /// the phase and reports one episode with a count instead.
    ///
    /// Matched on the code alone. GIWA's sequencer answers `-32003 txpool is full` and reth answers
    /// the same code for the same condition; the message text is the part that varies by node and
    /// version, so keying on it would be keying on the half that is not specified.
    #[must_use]
    pub const fn is_txpool_full(&self) -> bool {
        matches!(self, Self::Node { code: -32003, .. })
    }

    /// True when the node says the nonce has already been used.
    ///
    /// The one error that proves our own counter is *behind* the chain, and therefore the only
    /// per-send outcome that justifies re-reading the account's nonce. See
    /// [`crate::tx::Sender::send_batch`] for why every other outcome must not.
    ///
    /// Matched on the message rather than the code, and that is not a preference: geth, reth and
    /// every gateway in front of them return this under `-32000`, the generic bucket that also
    /// carries "intrinsic gas too low" and "replacement transaction underpriced". The code cannot
    /// distinguish them; the text is the only thing that can.
    #[must_use]
    pub fn is_nonce_too_low(&self) -> bool {
        match self {
            Self::Node { message, .. } => message.to_ascii_lowercase().contains("nonce too low"),
            _ => false,
        }
    }

    /// The node already holds this exact transaction, which is an acceptance wearing an error's
    /// clothes.
    ///
    /// A transaction's hash is a function of its contents, so getting this back means the bytes we
    /// just signed are byte-identical to something already in the pool — and therefore that the
    /// transaction is in flight under the hash we computed, so a receipt lookup finds it. Nothing is
    /// lost, no gas is spent twice, and the nonce is not double-spent. Reporting it as a failed send
    /// produced a stream of operator alerts for transactions that were on their way.
    ///
    /// Matched on the message for the same reason as [`Self::is_nonce_too_low`]: `-32000` is the
    /// generic bucket and the text is the only thing that separates its occupants.
    #[must_use]
    pub fn is_already_known(&self) -> bool {
        match self {
            Self::Node { message, .. } => message.to_ascii_lowercase().contains("already known"),
            _ => false,
        }
    }
}

fn decode_err(what: &'static str, detail: impl std::fmt::Display) -> RpcError {
    RpcError::Decode {
        what,
        detail: detail.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Runaway guard
// ---------------------------------------------------------------------------

/// Token bucket plus a rate-limit penalty window.
///
/// **This is a fuse, not a budget.** It was originally sized to keep the bot underneath a
/// hostile public endpoint's rate limit — the measured 429s during a 203-transaction broadcast.
/// The dedicated endpoint took 20 rapid `eth_blockNumber` calls without one, so that sizing is
/// gone: the defaults in [`crate::config::ChainConfig`] are now loose enough that normal
/// operation never approaches them, and the config no longer refuses an interval on the grounds
/// that its steady-state rate would exhaust the budget.
///
/// What is left is worth keeping on its own terms. A bug — a reconnect storm, a loop that stops
/// awaiting, a retry with no ceiling — should hit a local limit before it hits the endpoint's,
/// and an endpoint that starts failing should not be answered with a flood. Both are about this
/// process misbehaving rather than the provider being stingy, and neither went away.
///
/// Deliberately not a queue, and that reasoning is unchanged. A queue in front of a limited
/// endpoint converts a burst into latency, and latency in a quoting loop converts into adverse
/// selection: the quote we would have sent lands three seconds late, against a market that has
/// moved. Failing the request locally lets the caller decide — the loop skips a cycle, the
/// transmit path aborts the push and logs why — which is always better than a stale action
/// taken on time.
#[derive(Debug)]
struct Limiter {
    tokens: f64,
    burst: f64,
    refill_per_sec: f64,
    last_refill: Instant,
    penalty_until: Option<Instant>,
    consecutive_429: u32,
    rate_limit_events: u64,
}

impl Limiter {
    fn new(requests_per_sec: f64, burst: f64, now: Instant) -> Self {
        Self {
            tokens: burst,
            burst,
            refill_per_sec: requests_per_sec,
            last_refill: now,
            penalty_until: None,
            consecutive_429: 0,
            rate_limit_events: 0,
        }
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now
            .saturating_duration_since(self.last_refill)
            .as_secs_f64();
        if elapsed > 0.0 {
            self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.burst);
            self.last_refill = now;
        }
    }

    fn try_take(&mut self, endpoint: &'static str, now: Instant) -> Result<(), RpcError> {
        if let Some(until) = self.penalty_until {
            if now < until {
                let remaining_ms =
                    u64::try_from(until.duration_since(now).as_millis()).unwrap_or(u64::MAX);
                return Err(RpcError::BackingOff {
                    endpoint,
                    remaining_ms,
                });
            }
            self.penalty_until = None;
        }
        self.refill(now);
        if self.tokens < 1.0 {
            return Err(RpcError::BudgetExhausted { endpoint });
        }
        self.tokens -= 1.0;
        Ok(())
    }

    /// Enter (or extend) the penalty window. Returns its length.
    fn on_rate_limited(&mut self, now: Instant, initial: Duration, max: Duration) -> Duration {
        self.rate_limit_events += 1;
        // Exponential in the number of *consecutive* 429s, so an isolated one costs the initial
        // delay and a sustained squeeze walks up to the ceiling instead of hammering.
        let shift = self.consecutive_429.min(16);
        let backoff = initial.saturating_mul(1u32 << shift).min(max);
        self.consecutive_429 = self.consecutive_429.saturating_add(1);
        self.penalty_until = Some(now + backoff);
        // Spend the bucket too: the endpoint has told us we are over, and the local budget was
        // evidently too generous for the current conditions.
        self.tokens = 0.0;
        backoff
    }

    fn on_success(&mut self) {
        self.consecutive_429 = 0;
    }
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

/// How a pooled client picks which endpoint answers a call.
///
/// The distinction is not a tuning knob. Spreading reads over several keys multiplies the request
/// budget and costs nothing, because a read is a question about one block and any node can answer
/// it. Spreading the *transmit* path over several nodes is a bug: `eth_getTransactionCount` on one
/// node and `eth_sendRawTransaction` on another means the nonce was read from a node that has not
/// seen the previous transaction, which produces a gap and a transaction that never lands. It is
/// the same hazard as reading a nonce from the flashblocks `pending` tag, arriving by a different
/// road.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selection {
    /// Round-robin over healthy endpoints. For reads.
    Rotate,
    /// Stay on the first healthy endpoint, and move only when it is genuinely failing. For the
    /// nonce/submit/receipt path, where consecutive calls must reach the same node's view.
    Pin,
}

/// One endpoint in a pool: a URL and the runaway guard that belongs to it.
///
/// The guard is per-endpoint rather than per-pool because a rate limit is a property of the key
/// that hit it. One shared limiter would let a single throttled key back the whole pool off, which
/// is the opposite of what having several keys is for.
struct Endpoint {
    url: EndpointUrl,
    limiter: Mutex<Limiter>,
}

/// A JSON-RPC client over one or more interchangeable endpoints.
pub struct Rpc {
    name: &'static str,
    endpoints: Vec<Endpoint>,
    selection: Selection,
    cursor: AtomicU64,
    client: reqwest::Client,
    next_id: AtomicU64,
    backoff_initial: Duration,
    backoff_max: Duration,
}

impl Rpc {
    /// Build a client for one endpoint.
    ///
    /// Takes an [`EndpointUrl`] rather than a `String` so that the credential in the path cannot
    /// reach a log line: every error this type produces carries the endpoint's *name*
    /// (`"rpc"`, `"flashblocks"`), never its URL.
    ///
    /// # Errors
    /// [`RpcError::Transport`] if the HTTP client cannot be constructed.
    pub fn new(
        name: &'static str,
        url: &EndpointUrl,
        cfg: &crate::config::ChainConfig,
    ) -> Result<Self, RpcError> {
        Self::pooled(name, std::slice::from_ref(url), Selection::Pin, cfg)
    }

    /// Build a client over several interchangeable endpoints.
    ///
    /// `urls` must all be the same chain — nothing here checks that, and nothing could cheaply.
    /// A mismatched endpoint would answer with another chain's state and the failure would look
    /// like a reorg, so the startup verification in [`verify_against_chain`] is what has to catch
    /// it, and it runs against the pool.
    ///
    /// # Errors
    /// [`RpcError::Transport`] if the HTTP client cannot be constructed, or if `urls` is empty —
    /// a pool with no endpoints answers nothing, and failing at startup beats failing per call.
    pub fn pooled(
        name: &'static str,
        urls: &[EndpointUrl],
        selection: Selection,
        cfg: &crate::config::ChainConfig,
    ) -> Result<Self, RpcError> {
        if urls.is_empty() {
            return Err(decode_err(name, "no endpoints configured"));
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(cfg.request_timeout_ms))
            .user_agent(concat!("dubu-updater/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|source| RpcError::Transport {
                endpoint: name,
                source,
            })?;
        let now = Instant::now();
        Ok(Self {
            name,
            endpoints: urls
                .iter()
                .map(|u| Endpoint {
                    url: u.clone(),
                    limiter: Mutex::new(Limiter::new(cfg.requests_per_sec, cfg.request_burst, now)),
                })
                .collect(),
            selection,
            cursor: AtomicU64::new(0),
            client,
            next_id: AtomicU64::new(1),
            backoff_initial: Duration::from_millis(cfg.rate_limit_backoff_initial_ms),
            backoff_max: Duration::from_millis(cfg.rate_limit_backoff_max_ms),
        })
    }

    /// How many endpoints this client can fall back across.
    #[must_use]
    pub fn endpoint_count(&self) -> usize {
        self.endpoints.len()
    }

    /// This endpoint's name, for logs.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// The first endpoint's URL in its redacted form, for logs. There is no accessor for the real
    /// one.
    #[must_use]
    pub fn url(&self) -> &EndpointUrl {
        // Bounded by the empty check in `pooled`, which is the only constructor.
        #[allow(clippy::indexing_slicing)]
        &self.endpoints[0].url
    }

    /// How many times any endpoint in this pool has rate-limited us.
    #[must_use]
    pub fn rate_limit_events(&self) -> u64 {
        self.endpoints
            .iter()
            .map(|e| Self::lock_of(e).rate_limit_events)
            .sum()
    }

    /// The same count split per endpoint, in pool order.
    ///
    /// The sum above cannot answer the only question worth asking when reads start failing —
    /// *which* endpoint — and neither can the errors, because every variant is built with
    /// `self.name`, the pool's label, under a field documented as "which endpoint". So a pool of
    /// six reported six identical failures, and an afternoon of 429s was unattributable: the
    /// obvious reading was that the endpoint named in the message was the one being throttled, and
    /// nothing in the log could confirm or deny it.
    ///
    /// Positions rather than URLs. An endpoint's URL carries an API key and this crate has exactly
    /// two places that unredact one; the caller matches these against its own configured list.
    #[must_use]
    pub fn rate_limit_events_by_endpoint(&self) -> Vec<u64> {
        self.endpoints
            .iter()
            .map(|e| Self::lock_of(e).rate_limit_events)
            .collect()
    }

    fn lock_of(e: &Endpoint) -> std::sync::MutexGuard<'_, Limiter> {
        e.limiter
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// One JSON-RPC call.
    ///
    /// # Errors
    /// [`RpcError`]. Note that [`RpcError::BackingOff`] and [`RpcError::BudgetExhausted`] are
    /// returned *without opening a socket*, which is the point.
    pub async fn call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, RpcError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let body = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });

        let n = self.endpoints.len();
        let start = match self.selection {
            // Advance the cursor once per call, so consecutive reads land on different keys and
            // the pool's budget is the sum of its keys rather than the smallest of them.
            Selection::Rotate => {
                usize::try_from(self.cursor.fetch_add(1, Ordering::Relaxed)).unwrap_or(0) % n
            }
            Selection::Pin => 0,
        };

        let mut last: Option<RpcError> = None;
        for step in 0..n {
            let idx = (start + step) % n;
            // Bounded by the modulo above; `n` is the length of the vector being indexed.
            #[allow(clippy::indexing_slicing)]
            let endpoint = &self.endpoints[idx];

            // The runaway guard is consulted per endpoint, so one throttled key steps aside
            // instead of backing the whole pool off.
            if let Err(e) = Self::lock_of(endpoint).try_take(self.name, Instant::now()) {
                last = Some(e);
                continue;
            }

            match self.call_one(endpoint, &body).await {
                Ok(v) => return Ok(v),
                Err(e) => {
                    // Only *transport-level* failures are retried elsewhere. A `Node` error means
                    // this node understood the request and refused it, so another node would
                    // refuse it identically — and on the transmit path a retried submit is a
                    // second broadcast of the same intent, which is exactly what must not be
                    // guessed at.
                    if !e.is_endpoint_fault() {
                        return Err(e);
                    }
                    last = Some(e);
                }
            }
        }

        Err(last.unwrap_or_else(|| decode_err(self.name, "no endpoint available")))
    }

    /// One attempt against one endpoint.
    async fn call_one(
        &self,
        endpoint: &Endpoint,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value, RpcError> {
        // One of exactly two `expose()` call sites in the crate; the other is the websocket
        // connect in `heads`. Everything else — logs, errors, Debug output — sees the redaction.
        let resp = self
            .client
            .post(endpoint.url.expose())
            .json(body)
            .send()
            .await
            .map_err(|source| RpcError::Transport {
                endpoint: self.name,
                source,
            })?;

        let status = resp.status();
        let text = resp.text().await.map_err(|source| RpcError::Transport {
            endpoint: self.name,
            source,
        })?;

        // The observed shape is a 429 with an "over rate limit" body, but some gateways answer
        // 200 with the same text, so the body is checked either way.
        let looks_rate_limited =
            status.as_u16() == 429 || text.to_ascii_lowercase().contains("over rate limit");
        if looks_rate_limited {
            let backoff = {
                let mut g = Self::lock_of(endpoint);
                g.on_rate_limited(Instant::now(), self.backoff_initial, self.backoff_max)
            };
            return Err(RpcError::RateLimited {
                endpoint: self.name,
                backoff_ms: u64::try_from(backoff.as_millis()).unwrap_or(u64::MAX),
            });
        }
        if !status.is_success() {
            return Err(RpcError::Http {
                endpoint: self.name,
                status: status.as_u16(),
                body: text.chars().take(300).collect(),
            });
        }

        let v: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| decode_err("json-rpc envelope", e))?;
        if let Some(err) = v.get("error") {
            return Err(RpcError::Node {
                endpoint: self.name,
                code: err
                    .get("code")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(0),
                message: err
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("(none)")
                    .to_string(),
            });
        }
        Self::lock_of(endpoint).on_success();
        Ok(v.get("result").cloned().unwrap_or(serde_json::Value::Null))
    }

    /// `eth_call` against `to` with `data`, at a block tag.
    ///
    /// # Errors
    /// [`RpcError`].
    pub async fn eth_call(&self, to: Address, data: &[u8], tag: &str) -> Result<Vec<u8>, RpcError> {
        let params = json!([{ "to": to.to_string(), "data": hex0x(data) }, tag]);
        let result = self.call("eth_call", params).await?;
        let s = result
            .as_str()
            .ok_or_else(|| decode_err("eth_call result", "not a string"))?;
        unhex(s).ok_or_else(|| decode_err("eth_call result", "not hex"))
    }

    /// A `u64` from a hex-quantity-returning method.
    ///
    /// # Errors
    /// [`RpcError`].
    pub async fn quantity(&self, method: &str, params: serde_json::Value) -> Result<u64, RpcError> {
        let r = self.call(method, params).await?;
        let s = r
            .as_str()
            .ok_or_else(|| decode_err("quantity", "not a string"))?;
        u64::from_str_radix(s.trim_start_matches("0x"), 16).map_err(|e| decode_err("quantity", e))
    }
}

/// Lower-case `0x`-prefixed hex.
#[must_use]
pub fn hex0x(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(2 + bytes.len() * 2);
    s.push_str("0x");
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Parse `0x`-prefixed hex. `None` on an odd length or a non-hex digit.
#[must_use]
pub fn unhex(s: &str) -> Option<Vec<u8>> {
    let t = s.strip_prefix("0x").unwrap_or(s);
    if t.len() % 2 != 0 {
        return None;
    }
    (0..t.len() / 2)
        .map(|i| u8::from_str_radix(&t[i * 2..i * 2 + 2], 16).ok())
        .collect()
}

// ---------------------------------------------------------------------------
// Health
// ---------------------------------------------------------------------------

/// What the chain connection is doing, as the quote loop sees it.
///
/// `stale_secs` measures the *older* of the two liveness signals — the last landed read and the
/// last block-number advance — so either one going quiet escalates on the same ladder. See
/// [`ChainHealth`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChainStatus {
    /// Reads are landing and the chain is advancing.
    Healthy,
    /// A liveness signal has been quiet for a while. Quoting continues with widened spreads.
    Degraded {
        /// How long since the quieter of the two signals last moved.
        stale_secs: u64,
    },
    /// A liveness signal has been quiet long enough to stop. Withdraw quotes and exit.
    Down {
        /// How long since the quieter of the two signals last moved.
        stale_secs: u64,
    },
}

impl ChainStatus {
    /// Short stable string for structured logs.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Degraded { .. } => "degraded",
            Self::Down { .. } => "down",
        }
    }
}

/// Which liveness signal has gone quiet. Purely for the log line that accompanies an escalation
/// — the two cases have completely different diagnoses and saying "chain unhealthy" for both
/// wastes the only signal an operator gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stall {
    /// Neither. Reads are landing and the block number is climbing.
    None,
    /// Reads are failing: timeouts, 429s, node errors. The endpoint is the problem.
    Reads,
    /// Reads succeed and the block number has not moved. **The chain has stopped**, or this
    /// endpoint is serving a frozen view of it. Either way, quoting into it is quoting into a
    /// market whose state cannot change but whose fair value can.
    Progress {
        /// The block number everything is stuck at.
        at_block: u64,
    },
}

impl Stall {
    /// Short stable string for structured logs.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Reads => "reads",
            Self::Progress { .. } => "progress",
        }
    }
}

/// Rolling health of the chain connection, on two signals.
///
/// The thresholds and the `Healthy -> Degraded -> Down` ladder are exactly what they were under
/// polling. The change is that `Down` is now reachable two ways:
///
/// * **reads stop landing** — the original signal, and the one a dead endpoint trips;
/// * **the block number stops advancing** — new. An RPC that answers every call about a chain
///   that has halted used to read `Healthy` indefinitely, because the only thing measured was
///   whether the socket replied. That is the same class of bug as a websocket that reconnects
///   and delivers nothing: everything looks fine and the state is frozen.
///
/// [`ChainHealth::status`] takes the **older** of the two references, so whichever signal has
/// been quiet longest is the one that drives the escalation, and neither can mask the other.
#[derive(Debug)]
pub struct ChainHealth {
    last_read: Option<Instant>,
    last_progress: Option<Instant>,
    best_block: u64,
    since: Instant,
    consecutive_failures: u32,
    last_error: Option<String>,
    degraded_after: Duration,
    halt_after: Duration,
}

impl ChainHealth {
    /// Start a health tracker. The clock runs from construction, so a bot that never manages a
    /// single successful read still halts on schedule instead of waiting forever.
    #[must_use]
    pub fn new(now: Instant, degraded_after_secs: u64, halt_after_secs: u64) -> Self {
        Self {
            last_read: None,
            last_progress: None,
            best_block: 0,
            since: now,
            consecutive_failures: 0,
            last_error: None,
            degraded_after: Duration::from_secs(degraded_after_secs),
            halt_after: Duration::from_secs(halt_after_secs),
        }
    }

    /// Record a successful read at the block number it was answered at.
    ///
    /// The block number is what separates "the endpoint is up" from "the chain is moving". Only
    /// a **strictly greater** number counts as progress: a repeated number is a chain that has
    /// not produced a block, and a lower one is a reorg or a lagging replica, neither of which is
    /// forward motion.
    pub fn on_read(&mut self, now: Instant, block_number: u64) {
        self.last_read = Some(now);
        self.consecutive_failures = 0;
        self.last_error = None;
        if block_number > self.best_block {
            self.best_block = block_number;
            self.last_progress = Some(now);
        }
    }

    /// Record a failed read.
    pub fn on_failure(&mut self, err: &RpcError) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.last_error = Some(err.to_string());
    }

    /// How many reads have failed in a row.
    #[must_use]
    pub const fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }

    /// The highest block number any read has returned.
    #[must_use]
    pub const fn best_block(&self) -> u64 {
        self.best_block
    }

    /// The most recent failure, if the last read failed.
    #[must_use]
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// Which signal is the quiet one, for the escalation log line.
    #[must_use]
    pub fn stall(&self, now: Instant) -> Stall {
        if self.quiet_for(now) < self.degraded_after {
            return Stall::None;
        }
        let read_ref = self.last_read.unwrap_or(self.since);
        let progress_ref = self.last_progress.unwrap_or(self.since);
        if read_ref <= progress_ref {
            Stall::Reads
        } else {
            Stall::Progress {
                at_block: self.best_block,
            }
        }
    }

    /// How long the quieter of the two signals has been quiet.
    fn quiet_for(&self, now: Instant) -> Duration {
        let read_ref = self.last_read.unwrap_or(self.since);
        let progress_ref = self.last_progress.unwrap_or(self.since);
        now.saturating_duration_since(read_ref.min(progress_ref))
    }

    /// Current status.
    #[must_use]
    pub fn status(&self, now: Instant) -> ChainStatus {
        let elapsed = self.quiet_for(now);
        let stale_secs = elapsed.as_secs();
        if elapsed >= self.halt_after {
            ChainStatus::Down { stale_secs }
        } else if elapsed >= self.degraded_after {
            ChainStatus::Degraded { stale_secs }
        } else {
            ChainStatus::Healthy
        }
    }
}

// ---------------------------------------------------------------------------
// Decoded state
// ---------------------------------------------------------------------------

/// Port of `PropPool._decayed`. The staleness ramp: `capacity` at age zero, falling linearly to
/// zero at `decay_secs`, floored. `decay_secs == 0` disables it.
///
/// The pool applies this to `available`, and `_outFor` measures the epoch's remaining room against
/// `available` rather than `capacity` — so a size priced on the nominal number is a size the chain
/// will refuse once the ramp has run. `PropPool.sol` is authoritative; its `_decayed` doc comment
/// records that both ends are exact on purpose and that this port is expected to exist.
#[must_use]
pub const fn decayed(capacity: u128, age_secs: u64, decay_secs: u16) -> u128 {
    debug_assert!(
        capacity <= dubu_core::curve::AMOUNT_MAX,
        "capacity outside uint96; the product below assumes that bound"
    );
    if decay_secs == 0 {
        return capacity;
    }
    let window = decay_secs as u64;
    if age_secs >= window {
        return 0;
    }
    let out = (capacity * (window - age_secs) as u128) / window as u128;
    debug_assert!(out <= capacity, "the ramp may only take depth away");
    out
}

/// One pair's on-chain state, converted out of the ABI types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Snap {
    /// Worst bid.
    pub min_bid: u128,
    /// Best bid.
    pub max_bid: u128,
    /// Best ask.
    pub min_ask: u128,
    /// Worst ask.
    pub max_ask: u128,
    /// Unix seconds of the last `updateQuote`. Zero means never quoted.
    pub updated_at: u64,
    /// Base the pool will buy this epoch.
    pub bid_capacity: u128,
    /// Base the pool will sell this epoch.
    pub ask_capacity: u128,
    /// Raw stored bid usage — meaningless unless the generations match.
    pub bid_used_raw: u128,
    /// Raw stored ask usage — meaningless unless the generations match.
    pub ask_used_raw: u128,
    /// Capacity epoch generation.
    pub cap_gen: u32,
    /// Generation the usage counters were stamped with.
    pub used_gen: u32,
    /// Flags word; bit 0 is paused.
    pub flags: u16,
    /// Decimal alignment for this pair.
    pub price_scale_exp: u8,
    /// How long a quote stays fillable.
    pub stale_secs_max: u32,
}

impl Snap {
    /// Effective bid usage.
    ///
    /// `PropPool` only treats the stored counters as real while `usedGen == capGen`; otherwise
    /// they belong to a superseded epoch and read as zero. Reproducing that rule here rather
    /// than reading `bidUsed` directly is not a detail — on the live pool right now `capGen` is
    /// 14 and `usedGen` is 13, so the raw ask counter says 499 mWETH has been sold this epoch
    /// and the truth is zero. A policy that compared against the raw counter would compute the
    /// executable top at the wrong point on the ladder.
    #[must_use]
    pub const fn bid_used(&self) -> u128 {
        if self.used_gen == self.cap_gen {
            self.bid_used_raw
        } else {
            0
        }
    }

    /// Effective ask usage. See [`Snap::bid_used`].
    #[must_use]
    pub const fn ask_used(&self) -> u128 {
        if self.used_gen == self.cap_gen {
            self.ask_used_raw
        } else {
            0
        }
    }

    /// Bid depth the pool will still expose at `age_secs`, after the staleness ramp.
    ///
    /// Pair with [`Snap::bid_used`], not with [`Snap::bid_capacity`]: the room left is
    /// `available - used`, and `used` is *not* scaled by the ramp.
    #[must_use]
    pub const fn available_bid(&self, age_secs: u64, decay_secs: u16) -> u128 {
        decayed(self.bid_capacity, age_secs, decay_secs)
    }

    /// Ask depth the pool will still expose at `age_secs`. See [`Snap::available_bid`].
    #[must_use]
    pub const fn available_ask(&self, age_secs: u64, decay_secs: u16) -> u128 {
        decayed(self.ask_capacity, age_secs, decay_secs)
    }

    /// Whether this pair is paused.
    #[must_use]
    pub const fn paused(&self) -> bool {
        self.flags & 1 != 0
    }

    /// Whether a ladder has ever been posted.
    #[must_use]
    pub const fn never_quoted(&self) -> bool {
        self.updated_at == 0
    }

    /// The stored four prices.
    #[must_use]
    pub const fn ladder(&self) -> Ladder {
        Ladder {
            min_bid: self.min_bid,
            max_bid: self.max_bid,
            min_ask: self.min_ask,
            max_ask: self.max_ask,
        }
    }

    /// Age of the stored quote against a block timestamp, saturating at zero for a clock skew.
    #[must_use]
    pub const fn quote_age_secs(&self, block_timestamp: u64) -> u64 {
        block_timestamp.saturating_sub(self.updated_at)
    }

    fn from_abi(a: &abi::PairSnapshotAbi) -> Self {
        Self {
            min_bid: a.minBid.to::<u128>(),
            max_bid: a.maxBid.to::<u128>(),
            min_ask: a.minAsk.to::<u128>(),
            max_ask: a.maxAsk.to::<u128>(),
            updated_at: u64::from(a.updatedAt),
            bid_capacity: a.bidCapacity.to::<u128>(),
            ask_capacity: a.askCapacity.to::<u128>(),
            bid_used_raw: a.bidUsed.to::<u128>(),
            ask_used_raw: a.askUsed.to::<u128>(),
            cap_gen: a.capGen,
            used_gen: a.usedGen,
            flags: a.flags,
            price_scale_exp: a.priceScaleExp,
            stale_secs_max: a.maxStaleSecs,
        }
    }
}

/// Immutable per-pair configuration, read once at startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PairMeta {
    /// Base token.
    pub base: Address,
    /// Quote token.
    pub quote: Address,
    /// Decimal alignment.
    pub price_scale_exp: u8,
    /// Freshness window the pool enforces.
    pub stale_secs_max: u32,
    /// Age at which the staleness ramp reaches zero. Zero disables it. Feeds [`decayed`].
    ///
    /// Read here rather than in the per-poll batch because it is manager-set and updater-invisible:
    /// unlike capacity and used, nothing this process does moves it, and nothing that moves it
    /// happens on a 330ms timescale. Polling it would buy one extra call per pair per poll for a
    /// value that changes when a human changes it.
    pub decay_secs: u16,
    /// Absolute floor on `minBid`, oracle-independent.
    pub min_price: u128,
    /// Base-token reserve floor.
    pub min_base_reserve: u128,
    /// Quote-token reserve floor.
    pub min_quote_reserve: u128,
    /// Base token decimals, read from the ERC-20 itself.
    pub base_decimals: u8,
    /// Quote token decimals, read from the ERC-20 itself.
    pub quote_decimals: u8,
}

/// Everything one poll cycle produced.
#[derive(Debug, Clone)]
pub struct ChainView {
    /// Block number the call was answered at.
    pub block_number: u64,
    /// Block timestamp the read was answered at.
    ///
    /// **Do not use this for time arithmetic.** It comes from `getCurrentBlockTimestamp` under the
    /// `pending` tag, and `pending` executes against a block that has not been sealed — the node
    /// projects that block's header, and measured on GIWA it projects the timestamp **about 12
    /// seconds into the future**, on both endpoints:
    ///
    /// ```text
    ///   flash/pending  +11.5s      flash/latest  -2.3s
    ///   canon/pending  +11.0s      canon/latest  -1.8s      block header  -1.4s
    /// ```
    ///
    /// Twelve seconds is Ethereum L1's block interval, which is a strong hint that it is an
    /// `op-geth` default nobody adjusted for a one-second L2.
    ///
    /// The consequences were not theoretical. Quote age came out as a constant 12s, so the
    /// heartbeat fired every cycle — 85% of all pushes — and `markout` stamped its references 12s
    /// ahead of the fills it compares them to, past `reference_at`'s tolerance, so every fill
    /// would have settled `unmarked`.
    ///
    /// Use [`crate::chain::heads::Head::timestamp`] for anything time-based: it is a *sealed*
    /// block's header, so it is real. It lags by about a second, which over-states age slightly —
    /// the safe direction. This field is kept for logging, and because the block number beside it
    /// is genuinely the freshest thing available.
    pub block_timestamp: u64,
    /// One entry per configured pair.
    pub snaps: BTreeMap<u16, Snap>,
    /// Pool balances of every token involved.
    pub balances: BTreeMap<Address, u128>,
    /// What the RFQ maker could actually pay out per token: the **lesser** of its own balance and
    /// its allowance to `PmmSettle`. Empty when no maker is configured.
    ///
    /// Separate from [`Self::balances`], and the separation is the point. `PmmSettle` custodies
    /// nothing and pulls both legs with `transferFrom` from the *maker*, so the pool's inventory
    /// says nothing about what an RFQ order can settle — they are different addresses. Sizing an
    /// RFQ quote against the pool's balance means signing orders that revert at fill time, which
    /// costs the taker gas and tells them nothing useful.
    pub maker_deliverable: BTreeMap<Address, u128>,
    /// The most recent **sealed** block, as `(number, timestamp)`.
    ///
    /// The clock. Not [`Self::block_timestamp`], which comes from the `pending` tag and lands about
    /// twelve seconds in the future; and not the `newHeads` subscription either, which is where it
    /// used to come from and which dies with whatever key it is on. Six of seven keys exhausted
    /// today, the socket went with them, and `chain_now` silently fell back to the host clock --
    /// so quote age, markout stamps and `scan_fills` all quietly stopped meaning anything.
    ///
    /// Read here instead because the reader is already making a round trip on a keyless endpoint
    /// every 330ms. `newHeads` remains as the fast wake signal it is good at; it is no longer the
    /// only source of the time.
    pub sealed: Option<(u64, u64)>,
    /// Our own confirmed nonce, when a sender was configured.
    ///
    /// This is what says a transaction has landed, and it says so exactly: nonce ordering is
    /// absolute for one sender, so a confirmed nonce above a pending transaction's own proves that
    /// transaction is on chain, however many others are in flight beside it.
    ///
    /// It exists because asking for receipts did not scale to the quote cadence. The receipt calls
    /// competed with quote traffic for the same rate limit, and measured, that made transactions
    /// which had actually landed in ~570ms get reported 2.5s later -- so the in-flight gate refused
    /// to re-quote 14.7% of the time over a queue that was already empty. This arrives with a read
    /// the reader was making anyway.
    pub sender_nonce: Option<u64>,
    /// When this view was received locally.
    pub at: Instant,
}

impl ChainView {
    /// How old this view is.
    #[must_use]
    pub fn age(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.at)
    }
}

// ---------------------------------------------------------------------------
// The read
// ---------------------------------------------------------------------------

/// Batches everything one cycle needs into a single `eth_call`.
///
/// Named for what it does rather than when it runs: this used to be `ChainPoller` and it is no
/// longer driven by a poll. It runs on a `newHeads` notification, and on the fallback timer when
/// heads have gone quiet.
pub struct ChainReader {
    pool: Address,
    multicall: Address,
    pair_ids: Vec<u16>,
    tokens: Vec<Address>,
    /// `(maker, settler)` when the RFQ leg is on. Its presence is what puts the two extra reads
    /// per token at the end of the batch, so it is also what `read` dispatches on when decoding.
    maker: Option<(Address, Address)>,
    /// The transaction sender, when the caller wants its confirmed nonce read alongside the state.
    ///
    /// Read on the **`latest`** tag, never `pending`: `pending` counts our own unconfirmed
    /// transactions, so it would report every in-flight send as already landed and free the gate
    /// instantly, which is the exact opposite of what this is for.
    sender: Option<Address>,
    calldata: Bytes,
}

impl ChainReader {
    /// Build the reader and precompute its calldata.
    ///
    /// The call list is fixed for the process lifetime, so it is encoded once: two Multicall3
    /// helpers, then one `snapshot` per pair, then one `balanceOf` per token. Order is the
    /// decode contract.
    #[must_use]
    pub fn new(
        pool: Address,
        multicall: Address,
        pair_ids: Vec<u16>,
        tokens: Vec<Address>,
    ) -> Self {
        Self::build(pool, multicall, pair_ids, tokens, None)
    }

    /// The same reader, plus the two reads that say what the RFQ maker can deliver.
    ///
    /// Folded into the existing batch rather than fetched separately: it is the same block, and a
    /// deliverable read at a different block than the inventory it is compared against would be a
    /// view that never existed.
    #[must_use]
    pub fn with_maker(
        pool: Address,
        multicall: Address,
        pair_ids: Vec<u16>,
        tokens: Vec<Address>,
        maker: Address,
        settler: Address,
    ) -> Self {
        Self::build(pool, multicall, pair_ids, tokens, Some((maker, settler)))
    }

    /// Also read this address's confirmed nonce on every poll.
    ///
    /// Costs one extra request per read and replaces up to `in_flight_max` receipt calls per cycle.
    /// See [`ChainView::sender_nonce`].
    #[must_use]
    pub fn with_sender(mut self, sender: Address) -> Self {
        self.sender = Some(sender);
        self
    }

    fn build(
        pool: Address,
        multicall: Address,
        pair_ids: Vec<u16>,
        tokens: Vec<Address>,
        maker: Option<(Address, Address)>,
    ) -> Self {
        let extra = if maker.is_some() { 2 * tokens.len() } else { 0 };
        let mut calls = Vec::with_capacity(2 + pair_ids.len() + tokens.len() + extra);
        calls.push(abi::Call3 {
            target: multicall,
            allowFailure: false,
            callData: abi::getBlockNumberCall {}.abi_encode().into(),
        });
        calls.push(abi::Call3 {
            target: multicall,
            allowFailure: false,
            callData: abi::getCurrentBlockTimestampCall {}.abi_encode().into(),
        });
        for &id in &pair_ids {
            calls.push(abi::Call3 {
                target: pool,
                allowFailure: false,
                callData: abi::snapshotCall { pairId: id }.abi_encode().into(),
            });
        }
        for &t in &tokens {
            calls.push(abi::Call3 {
                target: t,
                allowFailure: false,
                callData: abi::balanceOfCall { account: pool }.abi_encode().into(),
            });
        }
        if let Some((owner, spender)) = maker {
            for &t in &tokens {
                calls.push(abi::Call3 {
                    target: t,
                    allowFailure: false,
                    callData: abi::balanceOfCall { account: owner }.abi_encode().into(),
                });
                calls.push(abi::Call3 {
                    target: t,
                    allowFailure: false,
                    callData: abi::allowanceCall { owner, spender }.abi_encode().into(),
                });
            }
        }
        let calldata = abi::aggregate3Call { calls }.abi_encode().into();
        Self {
            pool,
            multicall,
            pair_ids,
            tokens,
            maker,
            sender: None,
            calldata,
        }
    }

    /// The pool this reader reads.
    #[must_use]
    pub const fn pool(&self) -> Address {
        self.pool
    }

    /// Read everything one cycle needs. Exactly one RPC request, whatever the pair count.
    ///
    /// Uses the `pending` tag, which on the flashblocks endpoint is the ~200ms preconfirmed
    /// state. That is *fresher than the head that triggered this call* — the head is a confirmed
    /// block at 1s, and preconfirmed state at 200ms already reflects swaps that block does not.
    /// It matters concretely: a swap that has been preconfirmed but not yet included has already
    /// moved `bidUsed`/`askUsed`, and quoting against the pre-swap usage would compute the
    /// executable top at a point on the ladder the pool has already walked past.
    ///
    /// # Errors
    /// [`RpcError`] — including the two that never leave the process.
    /// Indexing into `results` is bounded by `decode_batch`, which rejects any length other than
    /// the exact one asked for on the line above. The bound is local and explicit rather than an
    /// invariant held somewhere else, which is why it is exempted here and not suppressed
    /// crate-wide.
    #[allow(clippy::indexing_slicing)]
    pub async fn read(&self, rpc: &Rpc) -> Result<ChainView, RpcError> {
        // Issued together rather than in sequence. The nonce is not part of the multicall -- it is
        // account state, not contract state -- so it costs a second request, and paying for it
        // serially would add a round trip to the interval that decides how fast the gate reopens.
        let nonce_call = async {
            match self.sender {
                Some(a) => rpc
                    .call(
                        "eth_getTransactionCount",
                        serde_json::json!([a.to_string(), "latest"]),
                    )
                    .await
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .and_then(|h| u64::from_str_radix(h.trim_start_matches("0x"), 16).ok()),
                None => None,
            }
        };
        // The sealed head, on the same trip. One more request on a keyless endpoint, against a
        // websocket that costs a key and takes the clock with it when that key runs out.
        let sealed_call = async {
            rpc.call("eth_getBlockByNumber", serde_json::json!(["latest", false]))
                .await
                .ok()
                .and_then(|v| {
                    let hex = |k: &str| {
                        v.get(k)
                            .and_then(serde_json::Value::as_str)
                            .and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok())
                    };
                    Some((hex("number")?, hex("timestamp")?))
                })
        };
        let (raw, sender_nonce, sealed) = tokio::join!(
            rpc.eth_call(self.multicall, &self.calldata, "pending"),
            nonce_call,
            sealed_call
        );
        let raw = raw?;
        let extra = if self.maker.is_some() {
            2 * self.tokens.len()
        } else {
            0
        };
        let results = decode_batch(&raw, 2 + self.pair_ids.len() + self.tokens.len() + extra)?;

        let block_number = abi::getBlockNumberCall::abi_decode_returns(&results[0].returnData)
            .map_err(|e| decode_err("getBlockNumber", e))?;
        let block_timestamp =
            abi::getCurrentBlockTimestampCall::abi_decode_returns(&results[1].returnData)
                .map_err(|e| decode_err("getCurrentBlockTimestamp", e))?;

        let mut snaps = BTreeMap::new();
        for (i, &id) in self.pair_ids.iter().enumerate() {
            let a = abi::snapshotCall::abi_decode_returns(&results[2 + i].returnData)
                .map_err(|e| decode_err("PairSnapshot", e))?;
            snaps.insert(id, Snap::from_abi(&a));
        }

        let mut balances = BTreeMap::new();
        let base = 2 + self.pair_ids.len();
        for (i, &t) in self.tokens.iter().enumerate() {
            let v = abi::balanceOfCall::abi_decode_returns(&results[base + i].returnData)
                .map_err(|e| decode_err("balanceOf", e))?;
            balances.insert(
                t,
                u128::try_from(v).map_err(|_| decode_err("balanceOf", "exceeds u128"))?,
            );
        }

        // What the RFQ maker could pay out, per token: the lesser of what it holds and what it has
        // let `PmmSettle` pull. Either being short is the same failure, because the settler
        // custodies nothing and issues a `transferFrom` against the maker's own balance.
        let mut maker_deliverable = BTreeMap::new();
        if self.maker.is_some() {
            let base = 2 + self.pair_ids.len() + self.tokens.len();
            for (i, &t) in self.tokens.iter().enumerate() {
                let bal = abi::balanceOfCall::abi_decode_returns(&results[base + 2 * i].returnData)
                    .map_err(|e| decode_err("maker balanceOf", e))?;
                let allow =
                    abi::allowanceCall::abi_decode_returns(&results[base + 2 * i + 1].returnData)
                        .map_err(|e| decode_err("maker allowance", e))?;
                let d = bal.min(allow);
                maker_deliverable.insert(
                    t,
                    u128::try_from(d)
                        .map_err(|_| decode_err("maker deliverable", "exceeds u128"))?,
                );
            }
        }

        Ok(ChainView {
            block_number: u64::try_from(block_number).unwrap_or(u64::MAX),
            block_timestamp: u64::try_from(block_timestamp).unwrap_or(u64::MAX),
            snaps,
            balances,
            maker_deliverable,
            sealed,
            sender_nonce,
            at: Instant::now(),
        })
    }
}

/// Decode an `aggregate3` response and insist every sub-call succeeded.
///
/// `allowFailure` is `false` on every call this crate builds, so a revert should surface as a
/// reverted `eth_call` rather than a `false` here — but a partial batch decoding into a
/// zero-filled snapshot is the kind of silent wrong answer that ends up quoted, so the length
/// and the success flags are both checked rather than assumed.
fn decode_batch(raw: &[u8], expected: usize) -> Result<Vec<abi::Call3Result>, RpcError> {
    let results = abi::aggregate3Call::abi_decode_returns(raw)
        .map_err(|e| decode_err("aggregate3 results", e))?;
    if results.len() != expected {
        return Err(decode_err(
            "aggregate3 results",
            format!("expected {expected} entries, got {}", results.len()),
        ));
    }
    if let Some(i) = results.iter().position(|r| !r.success) {
        return Err(decode_err(
            "aggregate3 results",
            format!("sub-call {i} reverted"),
        ));
    }
    Ok(results)
}

// ---------------------------------------------------------------------------
// Startup verification
// ---------------------------------------------------------------------------

/// What the chain says about the configured pairs, and whether the config agrees.
#[derive(Debug, Clone)]
pub struct ChainFacts {
    /// Per-pair immutable configuration.
    pub pairs: BTreeMap<u16, PairMeta>,
    /// The address `PropPool` will accept `updateQuote` from.
    pub updater: Address,
    /// Every token the bot needs a balance for.
    pub tokens: Vec<Address>,
    /// The shared quote token NAV is denominated in.
    pub nav_token: Address,
    /// Decimals of [`ChainFacts::nav_token`].
    pub nav_decimals: u8,
}

/// Read the immutable facts and check the config against them.
///
/// This is where every check that needs the chain lives, and it runs before the loop starts.
/// The checks are the ones whose failure mode is silent:
///
/// * a `pair_id` that does not exist would poll a zeroed snapshot forever;
/// * `base_decimals` off by one prices the pair by a factor of ten;
/// * a `heartbeat_secs` at or above the pool's own `maxStaleSecs` means the quote expires
///   before the heartbeat re-posts it, so the pool stops quoting between pushes — the exact
///   condition this bot exists to prevent;
/// * pairs with different quote tokens cannot share one NAV, so the killswitches would be
///   measuring an incoherent number.
///
/// # Errors
/// [`RpcError`] for a failed read, or [`RpcError::Decode`] carrying the mismatch.
/// Indexing into `res` is bounded by `decode_batch` on the line that produced it, which rejects
/// any length but the exact one requested. The `BTreeMap` indexes below are over maps this
/// function has just built from the same `pair_ids` and token list it is now iterating.
#[allow(clippy::indexing_slicing)]
pub async fn verify_against_chain(
    rpc: &Rpc,
    pool: Address,
    multicall: Address,
    cfg: &crate::config::Config,
) -> Result<ChainFacts, RpcError> {
    // Round one: pairCount, updater, every pairConfig, then every effectiveCapacity. The last
    // group is only here for its `decaySecs`, which `pairConfig` does not carry.
    let mut calls = vec![
        abi::Call3 {
            target: pool,
            allowFailure: false,
            callData: abi::pairCountCall {}.abi_encode().into(),
        },
        abi::Call3 {
            target: pool,
            allowFailure: false,
            callData: abi::updaterCall {}.abi_encode().into(),
        },
    ];
    for p in &cfg.pairs {
        calls.push(abi::Call3 {
            target: pool,
            allowFailure: false,
            callData: abi::pairConfigCall { pairId: p.pair_id }
                .abi_encode()
                .into(),
        });
    }
    for p in &cfg.pairs {
        calls.push(abi::Call3 {
            target: pool,
            allowFailure: false,
            callData: abi::effectiveCapacityCall { pairId: p.pair_id }
                .abi_encode()
                .into(),
        });
    }
    let raw = rpc
        .eth_call(
            multicall,
            &abi::aggregate3Call { calls }.abi_encode(),
            "latest",
        )
        .await?;
    let res = decode_batch(&raw, 2 + 2 * cfg.pairs.len())?;

    let pair_count = abi::pairCountCall::abi_decode_returns(&res[0].returnData)
        .map_err(|e| decode_err("pairCount", e))?;
    let updater = abi::updaterCall::abi_decode_returns(&res[1].returnData)
        .map_err(|e| decode_err("updater", e))?;

    let mut configs = BTreeMap::new();
    let mut decays = BTreeMap::new();
    for (i, p) in cfg.pairs.iter().enumerate() {
        if p.pair_id > pair_count {
            return Err(decode_err(
                "pair id",
                format!(
                    "config lists pair_id {} but the pool has {pair_count} pairs",
                    p.pair_id
                ),
            ));
        }
        let c = abi::pairConfigCall::abi_decode_returns(&res[2 + i].returnData)
            .map_err(|e| decode_err("PairConfig", e))?;
        if !c.exists {
            return Err(decode_err(
                "pair id",
                format!("pair {} does not exist on chain", p.pair_id),
            ));
        }
        let e = abi::effectiveCapacityCall::abi_decode_returns(
            &res[2 + cfg.pairs.len() + i].returnData,
        )
        .map_err(|e| decode_err("effectiveCapacity", e))?;
        configs.insert(p.pair_id, c);
        decays.insert(p.pair_id, e.decaySecs);
    }

    // Round two: decimals() for every distinct token.
    let mut tokens: Vec<Address> = Vec::new();
    for c in configs.values() {
        for t in [c.base, c.quote] {
            if !tokens.contains(&t) {
                tokens.push(t);
            }
        }
    }
    let calls: Vec<_> = tokens
        .iter()
        .map(|&t| abi::Call3 {
            target: t,
            allowFailure: false,
            callData: abi::decimalsCall {}.abi_encode().into(),
        })
        .collect();
    let raw = rpc
        .eth_call(
            multicall,
            &abi::aggregate3Call { calls }.abi_encode(),
            "latest",
        )
        .await?;
    let res = decode_batch(&raw, tokens.len())?;
    let mut decimals = BTreeMap::new();
    for (i, &t) in tokens.iter().enumerate() {
        let d = abi::decimalsCall::abi_decode_returns(&res[i].returnData)
            .map_err(|e| decode_err("decimals", e))?;
        decimals.insert(t, d);
    }

    // Now the cross-checks.
    let mut pairs = BTreeMap::new();
    let mut nav_token: Option<Address> = None;
    for p in &cfg.pairs {
        let c = &configs[&p.pair_id];
        let bd = decimals[&c.base];
        let qd = decimals[&c.quote];
        if bd != p.base_decimals || qd != p.quote_decimals {
            return Err(decode_err(
                "token decimals",
                format!(
                    "pair {}: config says base/quote = {}/{} but the chain says {bd}/{qd}",
                    p.pair_id, p.base_decimals, p.quote_decimals
                ),
            ));
        }
        if u64::from(c.maxStaleSecs) <= p.heartbeat_secs {
            return Err(decode_err(
                "heartbeat",
                format!(
                    "pair {}: heartbeat_secs {} is not inside the pool's maxStaleSecs {}; \
                     the quote would expire before the heartbeat re-posted it",
                    p.pair_id, p.heartbeat_secs, c.maxStaleSecs
                ),
            ));
        }
        match nav_token {
            None => nav_token = Some(c.quote),
            Some(q) if q != c.quote => return Err(decode_err(
                "quote token",
                "configured pairs do not share a quote token, so a single NAV is not well defined",
            )),
            Some(_) => {}
        }
        pairs.insert(
            p.pair_id,
            PairMeta {
                base: c.base,
                quote: c.quote,
                price_scale_exp: c.priceScaleExp,
                stale_secs_max: c.maxStaleSecs,
                decay_secs: decays[&p.pair_id],
                min_price: c.minPrice.to::<u128>(),
                min_base_reserve: c.minBaseReserve.to::<u128>(),
                min_quote_reserve: c.minQuoteReserve.to::<u128>(),
                base_decimals: bd,
                quote_decimals: qd,
            },
        );
    }

    let nav_token = nav_token.ok_or_else(|| decode_err("quote token", "no pairs"))?;
    let nav_decimals = decimals[&nav_token];
    if nav_decimals != cfg.risk.nav_decimals {
        return Err(decode_err(
            "risk.nav_decimals",
            format!(
                "config says {} but the quote token reports {nav_decimals}",
                cfg.risk.nav_decimals
            ),
        ));
    }

    Ok(ChainFacts {
        pairs,
        updater,
        tokens,
        nav_token,
        nav_decimals,
    })
}

#[cfg(test)]
mod tests {

    /// The three `-32000` occupants must not be confused for one another. All three arrive under the
    /// same code and only the text separates them, so a predicate that drifts here silently turns an
    /// acceptance into an alert, a backpressure signal into a per-transaction failure, or a nonce
    /// resync into neither.
    #[test]
    fn the_generic_error_bucket_is_separated_by_its_text() {
        let node = |m: &str| RpcError::Node {
            code: -32000,
            message: m.to_string(),
            endpoint: "rpc",
        };
        let known = node("already known");
        assert!(known.is_already_known());
        assert!(!known.is_nonce_too_low());

        let low = node("nonce too low: next nonce 41, tx nonce 40");
        assert!(low.is_nonce_too_low());
        assert!(!low.is_already_known());

        // Neither, and specifically not "already known" — this one really is a failed send.
        let under = node("replacement transaction underpriced");
        assert!(!under.is_already_known());
        assert!(!under.is_nonce_too_low());
    }
    use super::*;

    fn snap(cap_gen: u32, used_gen: u32, bid_used: u128, ask_used: u128) -> Snap {
        Snap {
            min_bid: 1_994_002_500_000_000,
            max_bid: 1_999_000_000_000_000,
            min_ask: 2_001_000_000_000_000,
            max_ask: 2_006_002_500_000_000,
            updated_at: 1_785_114_691,
            bid_capacity: 1_000_000_000_000_000_000_000,
            ask_capacity: 1_000_000_000_000_000_000_000,
            bid_used_raw: bid_used,
            ask_used_raw: ask_used,
            cap_gen,
            used_gen,
            flags: 0,
            price_scale_exp: 24,
            stale_secs_max: 3_600,
        }
    }

    #[test]
    fn usage_reads_as_zero_across_a_generation_boundary() {
        // Exactly the live pool's shape: capGen 14, usedGen 13, and a large raw ask counter
        // that the pool itself ignores.
        let stale = snap(14, 13, 0, 499_438_326_634_891_408_781);
        assert_eq!(
            stale.ask_used(),
            0,
            "a superseded epoch's usage must read as zero"
        );
        assert_eq!(stale.bid_used(), 0);

        let current = snap(14, 14, 0, 499_438_326_634_891_408_781);
        assert_eq!(current.ask_used(), 499_438_326_634_891_408_781);
    }

    /// The two ends of the ramp are exact by construction on chain, so they are exact here.
    /// `PropPool._decayed`'s own doc comment says this is worth checking rather than assuming.
    #[test]
    fn the_ramp_is_exact_at_both_of_its_ends() {
        let cap = 1_000_000_000_000_000_000_000;
        assert_eq!(decayed(cap, 0, 30), cap, "age zero loses nothing at all");
        assert_eq!(decayed(cap, 30, 30), 0, "the cliff is `>=`, not `>`");
        assert_eq!(decayed(cap, 31, 30), 0);
        assert_eq!(decayed(cap, u64::MAX, 30), 0);
        // One second short of the cliff is still dust, not zero.
        assert_eq!(decayed(100, 29, 30), 3);
    }

    #[test]
    fn a_zero_window_disables_the_ramp_entirely() {
        // Live pair 3's shape: no ramp, so age must not touch capacity at any age.
        let cap = 1_000_000_000_000_000_000_000;
        assert_eq!(decayed(cap, 0, 0), cap);
        assert_eq!(decayed(cap, 86_400, 0), cap);
        assert_eq!(decayed(cap, u64::MAX, 0), cap);
    }

    #[test]
    fn the_ramp_floors_rather_than_rounds() {
        // 100 * 23 / 30 = 76.66..., and the pool keeps the 0.66.
        assert_eq!(decayed(100, 7, 30), 76);
        // Same shape at the live pair's capacity: 1000e18 * 23 / 30.
        assert_eq!(
            decayed(1_000_000_000_000_000_000_000, 7, 30),
            766_666_666_666_666_666_666
        );
    }

    #[test]
    fn available_applies_the_ramp_to_each_side_separately() {
        let mut s = snap(1, 1, 0, 0);
        s.ask_capacity = 500_000_000_000_000_000_000;
        assert_eq!(s.available_bid(0, 30), s.bid_capacity);
        assert_eq!(s.available_ask(0, 30), s.ask_capacity);
        assert_eq!(s.available_bid(15, 30), 500_000_000_000_000_000_000);
        assert_eq!(s.available_ask(15, 30), 250_000_000_000_000_000_000);
        assert_eq!(s.available_bid(30, 30), 0);
        assert_eq!(s.available_ask(30, 30), 0);
    }

    #[test]
    fn flags_bit_zero_is_paused() {
        let mut s = snap(1, 1, 0, 0);
        assert!(!s.paused());
        s.flags = 1;
        assert!(s.paused());
        // Reserved bits must not be read as paused.
        s.flags = 2;
        assert!(!s.paused());
    }

    #[test]
    fn quote_age_saturates_rather_than_underflowing() {
        let s = snap(1, 1, 0, 0);
        assert_eq!(s.quote_age_secs(1_785_114_691 + 100), 100);
        // A pending-tag timestamp behind the stored one is possible across a reorg.
        assert_eq!(s.quote_age_secs(1_785_114_000), 0);
    }

    #[test]
    fn never_quoted_is_a_zero_timestamp() {
        let mut s = snap(1, 1, 0, 0);
        assert!(!s.never_quoted());
        s.updated_at = 0;
        assert!(s.never_quoted());
    }

    #[test]
    fn the_token_bucket_refuses_rather_than_queues() {
        let t0 = Instant::now();
        let mut l = Limiter::new(1.0, 2.0, t0);
        assert!(l.try_take("rpc", t0).is_ok());
        assert!(l.try_take("rpc", t0).is_ok());
        assert!(matches!(
            l.try_take("rpc", t0),
            Err(RpcError::BudgetExhausted { .. })
        ));
        // One second later, one token.
        let t1 = t0 + Duration::from_secs(1);
        assert!(l.try_take("rpc", t1).is_ok());
        assert!(matches!(
            l.try_take("rpc", t1),
            Err(RpcError::BudgetExhausted { .. })
        ));
    }

    #[test]
    fn a_429_opens_no_further_sockets_until_the_penalty_expires() {
        let t0 = Instant::now();
        let mut l = Limiter::new(100.0, 100.0, t0);
        let b = l.on_rate_limited(
            t0,
            Duration::from_millis(1_000),
            Duration::from_millis(60_000),
        );
        assert_eq!(b, Duration::from_millis(1_000));
        assert!(matches!(
            l.try_take("rpc", t0),
            Err(RpcError::BackingOff { .. })
        ));
        assert!(matches!(
            l.try_take("rpc", t0 + Duration::from_millis(999)),
            Err(RpcError::BackingOff { .. })
        ));
        assert!(l.try_take("rpc", t0 + Duration::from_millis(1_001)).is_ok());
    }

    #[test]
    fn consecutive_rate_limits_back_off_exponentially_and_cap() {
        let t0 = Instant::now();
        let mut l = Limiter::new(100.0, 100.0, t0);
        let init = Duration::from_millis(1_000);
        let max = Duration::from_millis(8_000);
        assert_eq!(
            l.on_rate_limited(t0, init, max),
            Duration::from_millis(1_000)
        );
        assert_eq!(
            l.on_rate_limited(t0, init, max),
            Duration::from_millis(2_000)
        );
        assert_eq!(
            l.on_rate_limited(t0, init, max),
            Duration::from_millis(4_000)
        );
        assert_eq!(
            l.on_rate_limited(t0, init, max),
            Duration::from_millis(8_000)
        );
        assert_eq!(
            l.on_rate_limited(t0, init, max),
            Duration::from_millis(8_000),
            "must cap"
        );
        // A success resets the ladder, so an isolated 429 an hour later is cheap again.
        l.on_success();
        assert_eq!(
            l.on_rate_limited(t0, init, max),
            Duration::from_millis(1_000)
        );
        assert_eq!(l.rate_limit_events, 6);
    }

    /// A quota refusal must move the pool to the next key, not end the call.
    ///
    /// This is the regression that stopped the bot starting at all: `403 API usage quota has been
    /// exceeded` arrives as a JSON-RPC error object, so it read as "the node refused this request"
    /// -- the one class the pool deliberately does not retry, because another node would refuse it
    /// identically. For a quota that reasoning is exactly inverted: the refusal is about the key,
    /// and six working keys were sitting behind the one that hit its limit.
    #[test]
    fn a_quota_refusal_is_the_endpoints_fault_not_the_requests() {
        let quota = RpcError::Node {
            endpoint: "rpc",
            code: 403,
            message: "API usage quota has been exceeded".into(),
        };
        assert!(quota.is_endpoint_fault(), "must fail over to the next key");

        for code in [401, 429, -32005] {
            let e = RpcError::Node {
                endpoint: "rpc",
                code,
                message: "refused".into(),
            };
            assert!(
                e.is_endpoint_fault(),
                "code {code} refuses the caller, not the request"
            );
        }

        // And the ordinary case is unchanged: a malformed request is malformed everywhere, and
        // retrying it on the transmit path would be a second broadcast of the same intent.
        let bad = RpcError::Node {
            endpoint: "rpc",
            code: -32000,
            message: "nonce too low".into(),
        };
        assert!(
            !bad.is_endpoint_fault(),
            "every node answers this identically"
        );
    }

    /// The same bug, second spelling. `403` was whitelisted this morning; GIWA's public endpoint
    /// answers `-32016 over rate limit`, which was not, so the pool gave up on the first endpoint
    /// again -- with seven alternatives configured and idle.
    #[test]
    fn a_rate_limit_refusal_fails_over_whatever_the_venue_calls_it() {
        for code in [-32016, -32005, 429] {
            let e = RpcError::Node {
                endpoint: "rpc",
                code,
                message: "over rate limit".into(),
            };
            assert!(
                e.is_endpoint_fault(),
                "code {code} means not-you-not-now, so another endpoint is worth trying"
            );
        }
    }

    #[test]
    fn health_walks_healthy_to_degraded_to_down() {
        let t0 = Instant::now();
        let mut h = ChainHealth::new(t0, 30, 300);
        assert_eq!(h.status(t0), ChainStatus::Healthy);
        h.on_read(t0, 1_000);

        assert_eq!(h.status(t0 + Duration::from_secs(29)), ChainStatus::Healthy);
        assert_eq!(
            h.status(t0 + Duration::from_secs(30)),
            ChainStatus::Degraded { stale_secs: 30 }
        );
        assert_eq!(
            h.status(t0 + Duration::from_secs(299)),
            ChainStatus::Degraded { stale_secs: 299 }
        );
        assert_eq!(
            h.status(t0 + Duration::from_secs(300)),
            ChainStatus::Down { stale_secs: 300 }
        );

        // A read at a new block at any point returns to healthy.
        h.on_read(t0 + Duration::from_secs(300), 1_001);
        assert_eq!(
            h.status(t0 + Duration::from_secs(301)),
            ChainStatus::Healthy
        );
    }

    #[test]
    fn health_counts_from_start_when_no_read_has_ever_succeeded() {
        // Otherwise a bot that never reaches the node would sit `Healthy` forever.
        let t0 = Instant::now();
        let h = ChainHealth::new(t0, 30, 300);
        assert_eq!(
            h.status(t0 + Duration::from_secs(300)),
            ChainStatus::Down { stale_secs: 300 }
        );
    }

    #[test]
    fn a_frozen_chain_escalates_even_though_every_read_succeeds() {
        // The hole the block-number signal closes. Every `eth_call` lands, so the old
        // "did the RPC reply" test says Healthy forever, while the chain has not produced a
        // block in ten minutes and the bot happily quotes into it.
        let t0 = Instant::now();
        let mut h = ChainHealth::new(t0, 30, 300);
        h.on_read(t0, 5_000);
        assert_eq!(h.status(t0), ChainStatus::Healthy);

        // Reads keep succeeding, once a second, at the SAME block.
        for s in 1..=310 {
            h.on_read(t0 + Duration::from_secs(s), 5_000);
        }
        let now = t0 + Duration::from_secs(310);
        assert_eq!(
            h.consecutive_failures(),
            0,
            "nothing failed; that is the point"
        );
        assert_eq!(h.status(now), ChainStatus::Down { stale_secs: 310 });
        assert_eq!(h.stall(now), Stall::Progress { at_block: 5_000 });

        // One new block clears it immediately.
        h.on_read(now, 5_001);
        assert_eq!(h.status(now), ChainStatus::Healthy);
        assert_eq!(h.stall(now), Stall::None);
    }

    #[test]
    fn a_failing_endpoint_escalates_on_the_read_signal_and_says_so() {
        let t0 = Instant::now();
        let mut h = ChainHealth::new(t0, 30, 300);
        h.on_read(t0, 5_000);
        h.on_failure(&RpcError::BudgetExhausted { endpoint: "rpc" });

        let now = t0 + Duration::from_secs(40);
        assert_eq!(h.status(now), ChainStatus::Degraded { stale_secs: 40 });
        // Both references are the same instant here, so the tie must resolve to the endpoint
        // rather than accusing the chain of stopping.
        assert_eq!(h.stall(now), Stall::Reads);
        assert_eq!(h.consecutive_failures(), 1);
        assert!(h.last_error().is_some());
    }

    #[test]
    fn a_reorg_backwards_is_not_progress_and_does_not_lower_the_best_block() {
        let t0 = Instant::now();
        let mut h = ChainHealth::new(t0, 30, 300);
        h.on_read(t0, 5_000);
        h.on_read(t0 + Duration::from_secs(1), 4_998);
        assert_eq!(
            h.best_block(),
            5_000,
            "a lagging replica must not rewind the high-water mark"
        );
        // Staleness runs from the QUIETER signal, which is progress at t0 rather than the read
        // at t0+1s — so a stream of reads that never advance cannot mask a stalled chain.
        assert_eq!(
            h.status(t0 + Duration::from_secs(40)),
            ChainStatus::Degraded { stale_secs: 40 }
        );
        assert_eq!(
            h.stall(t0 + Duration::from_secs(40)),
            Stall::Progress { at_block: 5_000 }
        );
    }

    #[test]
    fn hex_round_trips() {
        assert_eq!(hex0x(&[0x00, 0xff, 0x10]), "0x00ff10");
        assert_eq!(hex0x(&[]), "0x");
        assert_eq!(unhex("0x00ff10"), Some(vec![0x00, 0xff, 0x10]));
        assert_eq!(unhex("00ff10"), Some(vec![0x00, 0xff, 0x10]));
        assert_eq!(unhex("0xfff"), None);
        assert_eq!(unhex("0xzz"), None);
    }

    #[test]
    fn the_read_encodes_one_call_per_thing_it_needs() {
        let pool = Address::repeat_byte(0xaa);
        let mc = Address::repeat_byte(0xbb);
        let p = ChainReader::new(
            pool,
            mc,
            vec![1, 2],
            vec![Address::repeat_byte(1), Address::repeat_byte(2)],
        );
        // Decoding our own calldata proves the layout the read's decoder assumes.
        let decoded = abi::aggregate3Call::abi_decode(&p.calldata).unwrap();
        assert_eq!(decoded.calls.len(), 6, "2 helpers + 2 pairs + 2 tokens");
        assert_eq!(decoded.calls[0].target, mc);
        assert_eq!(decoded.calls[2].target, pool);
        assert_eq!(decoded.calls[4].target, Address::repeat_byte(1));
        assert!(
            decoded.calls.iter().all(|c| !c.allowFailure),
            "a reverting sub-call must fail the batch"
        );
        // And the pair ids landed in the right slots.
        assert_eq!(
            abi::snapshotCall::abi_decode(&decoded.calls[2].callData)
                .unwrap()
                .pairId,
            1
        );
        assert_eq!(
            abi::snapshotCall::abi_decode(&decoded.calls[3].callData)
                .unwrap()
                .pairId,
            2
        );
    }
}
