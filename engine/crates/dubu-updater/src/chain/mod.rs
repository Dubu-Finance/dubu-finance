//! Chain access: JSON-RPC transport, the runaway guard, and the batched read that produces a
//! [`ChainView`].
//!
//! [`heads`] holds an `eth_subscribe("newHeads")` and the quote loop wakes on it;
//! [`crate::config::ChainConfig::fallback_poll_interval_ms`] is only a floor under that
//! subscription. Multicall3 batches every read into one `eth_call` per head: six round trips would
//! be worse on latency and worse on consistency, because a batch is answered at one block while six
//! calls can straddle a boundary and produce a view that never existed. [`Limiter`] is a fuse
//! against a runaway retry loop of *ours*, not a budget the normal path presses against.
//!
//! [`ChainHealth`] escalates `Healthy -> Degraded -> Down` on reads landing *and* on the block
//! number advancing, because measuring only the first reads healthy forever against an endpoint
//! that answers cheerfully about a chain that has stopped. The head watchdog in [`heads`]
//! deliberately does **not** feed it: when heads stop, the loop falls back to its timer and the
//! next read distinguishes a dead socket (block number still climbing, so quoting continues) from a
//! dead chain, where wiring a silent websocket straight to `Down` would withdraw quotes over a
//! quiet socket on a healthy chain.
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
//! Heads say *when* to look; they are not what is read. Most of the freshness is the tag rather
//! than the host, measured on GIWA by polling `snapshot(1).minBid` against the moment the updater
//! sent it:
//!
//! ```text
//!   flash `pending`  vs  ordinary `latest`     ordinary lags  ~871ms   <- the TAG
//!   flash `pending`  vs  ordinary `pending`    ordinary lags   ~82ms   <- the HOST
//!   send             ->  included in a preconfirmation   296/440/508ms (min/median/max)
//!   flashblock cadence                          327ms median, 3.0 per 1s block
//! ```
//!
//! 440ms is the number the rest of the bot is sized against and this module is its one home: it is
//! send-to-preconfirmation, timed by watching the sender's nonce advance on the `pending` tag. A
//! confirmed receipt takes about a second, so anything gating on one waits twice as long as the
//! chain requires. Transactions still go to the ordinary endpoint, whose `latest` leads the
//! flashblocks endpoint's by about two blocks: it is the canonical view, and a nonce read from a
//! preconfirmed state that later reorganises is a stuck transaction.

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
/// Struct field order is load-bearing: these are tuples on the wire, so a field out of place
/// decodes silently into the wrong meaning. They mirror `IPropPool.PairSnapshot` and
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

// --- Errors ---

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
    /// The transmit path needs the distinction: a send that failed *after* going out may have
    /// consumed a nonce, and one the local limiter refused before opening a socket cannot have.
    #[must_use]
    pub const fn never_sent(&self) -> bool {
        matches!(self, Self::BackingOff { .. } | Self::BudgetExhausted { .. })
    }

    /// True when the failure is the *endpoint's*, so another endpoint is worth trying.
    ///
    /// [`Self::Node`] is excluded by default: a JSON-RPC error means this node parsed the request
    /// and refused it, and every other node holding the same chain refuses it identically, so on
    /// the transmit path a retry is a second broadcast of the same intent.
    #[must_use]
    pub const fn is_endpoint_fault(&self) -> bool {
        match self {
            Self::Transport { .. }
            | Self::Http { .. }
            | Self::RateLimited { .. }
            | Self::BackingOff { .. }
            | Self::BudgetExhausted { .. } => true,
            // The exception: these codes refuse *this caller* rather than the request, which is a
            // property of the key, so a different key answers fine. Each venue spells it
            // differently, so anything meaning "not you, not now" belongs on the list. Safe on the
            // transmit path because all of them are issued before the request is processed.
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
    /// Backpressure, not a fault: the same bytes are valid a moment later and the nonce was not
    /// consumed, so the response is to stop sending — see [`crate::tx::Sender::send_batch`].
    /// Matched on the code alone, because the message text varies by node and version.
    #[must_use]
    pub const fn is_txpool_full(&self) -> bool {
        matches!(self, Self::Node { code: -32003, .. })
    }

    /// True when the node says the nonce has already been used.
    ///
    /// The one error proving our counter is *behind* the chain, and so the only per-send outcome
    /// that justifies re-reading the account's nonce. Matched on the message, not the code: this
    /// shares `-32000` with "intrinsic gas too low" and "replacement transaction underpriced", so
    /// only the text separates them.
    #[must_use]
    pub fn is_nonce_too_low(&self) -> bool {
        match self {
            Self::Node { message, .. } => message.to_ascii_lowercase().contains("nonce too low"),
            _ => false,
        }
    }

    /// The node already holds this exact transaction, which is an acceptance reported as an error.
    ///
    /// A hash is a function of the contents, so the bytes just signed are identical to something
    /// already in the pool: the transaction is in flight under the hash we computed, a receipt
    /// lookup finds it, and the nonce is not double-spent. Matched on the message, as above.
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

// --- Runaway guard ---

/// Token bucket plus a rate-limit penalty window.
///
/// A fuse, not a budget: the defaults in [`crate::config::ChainConfig`] are loose enough that
/// normal operation never approaches them, and what this catches is *this process* misbehaving.
/// Deliberately not a queue, because a queue in front of a limited endpoint converts a burst into
/// latency, and latency in a quoting loop converts into adverse selection; failing locally lets the
/// caller skip a cycle instead of taking a stale action on time.
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
        // Exponential in *consecutive* 429s: an isolated one costs the initial delay and a
        // sustained squeeze walks to the ceiling instead of hammering.
        let shift = self.consecutive_429.min(16);
        let backoff = initial.saturating_mul(1u32 << shift).min(max);
        self.consecutive_429 = self.consecutive_429.saturating_add(1);
        self.penalty_until = Some(now + backoff);
        // Spend the bucket too: the endpoint has said we are over, so the local budget was too
        // generous for current conditions.
        self.tokens = 0.0;
        backoff
    }

    fn on_success(&mut self) {
        self.consecutive_429 = 0;
    }
}

// --- Transport ---

/// How a pooled client picks which endpoint answers a call.
///
/// Not a tuning knob. Spreading *reads* over several keys multiplies the request budget for free,
/// because a read is a question about one block that any node can answer. Spreading the *transmit*
/// path is a bug: a nonce from one node and a send to another means the count came from a node that
/// has not seen the previous transaction, which produces a gap and a transaction that never lands.
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
/// Per-endpoint rather than per-pool because a rate limit is a property of the key that hit it; one
/// shared limiter would let a single throttled key back the whole pool off.
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
    /// Takes an [`EndpointUrl`] rather than a `String` so the credential in the path cannot reach a
    /// log line: every error carries the endpoint's *name* (`"rpc"`), never its URL.
    ///
    /// # Errors
    /// [`RpcError::Transport`] if the client cannot be constructed.
    pub fn new(
        name: &'static str,
        url: &EndpointUrl,
        cfg: &crate::config::ChainConfig,
    ) -> Result<Self, RpcError> {
        Self::pooled(name, std::slice::from_ref(url), Selection::Pin, cfg)
    }

    /// Build a client over several interchangeable endpoints.
    ///
    /// `urls` must all be the same chain and nothing here checks it: a mismatched endpoint answers
    /// with another chain's state and the failure looks like a reorg, which is why
    /// [`verify_against_chain`] runs against the pool.
    ///
    /// # Errors
    /// [`RpcError::Transport`] if the client cannot be constructed, or if `urls` is empty — a pool
    /// with no endpoints answers nothing, and failing at startup beats failing per call.
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
    /// Neither the sum nor the errors can say *which* endpoint is failing: every error carries the
    /// pool's label, so a pool of six reports six identical failures. Positions rather than URLs,
    /// because a URL carries an API key; the caller matches them against its configured list.
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
    /// [`RpcError`]. [`RpcError::BackingOff`] and [`RpcError::BudgetExhausted`] are returned
    /// *without opening a socket*, which the transmit path relies on.
    pub async fn call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, RpcError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let body = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });

        let n = self.endpoints.len();
        let start = match self.selection {
            // Advance once per call, so the pool's budget is the sum of its keys rather than the
            // smallest of them.
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

            // Per endpoint, so one throttled key steps aside instead of backing the pool off.
            if let Err(e) = Self::lock_of(endpoint).try_take(self.name, Instant::now()) {
                last = Some(e);
                continue;
            }

            match self.call_one(endpoint, &body).await {
                Ok(v) => return Ok(v),
                Err(e) => {
                    // Another node refuses a `Node` error identically, and on the transmit path a
                    // retried submit is a second broadcast of the same intent.
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
        // One of exactly two `expose()` call sites in the crate, the other being the websocket
        // connect in `heads`. Logs, errors and Debug output all see the redaction.
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

        // Some gateways answer 200 with an "over rate limit" body, so the body is checked either
        // way rather than only on a 429.
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

// --- Health ---

/// What the chain connection is doing, as the quote loop sees it.
///
/// `stale_secs` measures the *older* of the two liveness signals — the last landed read and the
/// last block-number advance — so either going quiet escalates on the same ladder.
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

/// Which liveness signal has gone quiet. Only for the escalation's log line, because the two cases
/// have different diagnoses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stall {
    /// Neither. Reads are landing and the block number is climbing.
    None,
    /// Reads are failing: timeouts, 429s, node errors. The endpoint is the problem.
    Reads,
    /// Reads succeed and the block number has not moved: the chain has stopped, or this endpoint
    /// serves a frozen view of it. Either way the pool's state cannot change while its fair value
    /// can.
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
/// `Down` is reachable two ways: reads stop landing, or the block number stops advancing.
/// [`ChainHealth::status`] takes the **older** of the two references, so whichever signal has been
/// quiet longest drives the escalation and neither can mask the other.
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
    /// Start a health tracker. The clock runs from construction, so a bot that never manages one
    /// successful read still halts on schedule.
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
    /// Only a **strictly greater** number counts as progress: a repeated one is a chain that has
    /// produced no block, and a lower one is a reorg or a lagging replica.
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

// --- Decoded state ---

/// Port of `PropPool._decayed`. The staleness ramp: `capacity` at age zero, falling linearly to
/// zero at `decay_secs`, floored. `decay_secs == 0` disables it.
///
/// `_outFor` measures the epoch's remaining room against `available` rather than `capacity`, so a
/// size priced on the nominal number is one the chain refuses once the ramp has run. Both ends are
/// exact on purpose; `PropPool.sol` is authoritative.
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
    /// `PropPool` treats the stored counters as real only while `usedGen == capGen`; otherwise they
    /// belong to a superseded epoch and read as zero. Reproduced here rather than reading `bidUsed`
    /// directly because the generations diverge routinely, and a stale raw counter puts the
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
    /// Read once at startup rather than per poll: it is manager-set, so nothing this process does
    /// moves it and nothing that does moves it on a 330ms timescale.
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
    /// **Do not use this for time arithmetic.** `getCurrentBlockTimestamp` under the `pending` tag
    /// executes against an unsealed block whose header the node projects ~12s ahead on GIWA (L1's
    /// block interval, most likely an `op-geth` default nobody adjusted for a one-second L2), which
    /// pins quote age at a constant 12s and stamps `markout` references past `reference_at`'s
    /// tolerance. Use [`crate::chain::heads::Head::timestamp`] for anything time-based: a *sealed*
    /// header, lagging about a second, which over-states age in the safe direction. Kept for
    /// logging, and because the block number beside it is the freshest available.
    pub block_timestamp: u64,
    /// One entry per configured pair.
    pub snaps: BTreeMap<u16, Snap>,
    /// Pool balances of every token involved.
    pub balances: BTreeMap<Address, u128>,
    /// What the RFQ maker could actually pay out per token: the **lesser** of its own balance and
    /// its allowance to `PmmSettle`. Empty when no maker is configured.
    ///
    /// Separate from [`Self::balances`] because `PmmSettle` custodies nothing and pulls both legs
    /// with `transferFrom` from the *maker*, a different address from the pool. Sizing an RFQ quote
    /// against the pool's balance signs orders that revert at fill time.
    pub maker_deliverable: BTreeMap<Address, u128>,
    /// The most recent **sealed** block, as `(number, timestamp)`. The clock.
    ///
    /// Not [`Self::block_timestamp`], which is a projection into the future; and not the `newHeads`
    /// subscription, which dies with whatever key it is on and takes quote age, markout stamps and
    /// `scan_fills` with it. Read here because the reader is already making a keyless round trip
    /// every 330ms, which leaves `newHeads` as the fast wake signal it is good at.
    pub sealed: Option<(u64, u64)>,
    /// Our own confirmed nonce, when a sender was configured.
    ///
    /// What says a transaction has landed, exactly: nonce ordering is absolute for one sender, so a
    /// confirmed nonce above a pending transaction's proves that transaction is on chain however
    /// many others are in flight beside it. Receipts do not scale to the quote cadence — they
    /// compete for the same rate limit, reporting a ~570ms landing 2.5s later — whereas this
    /// arrives with a read the reader was making anyway.
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

// --- The read ---

/// Batches everything one cycle needs into a single `eth_call`. Runs on a `newHeads` notification,
/// and on the fallback timer when heads have gone quiet.
pub struct ChainReader {
    pool: Address,
    multicall: Address,
    pair_ids: Vec<u16>,
    tokens: Vec<Address>,
    /// `(maker, settler)` when the RFQ leg is on. Its presence appends two reads per token to the
    /// batch, so it is also what `read` dispatches on when decoding.
    maker: Option<(Address, Address)>,
    /// The transaction sender, when the caller wants its confirmed nonce read alongside the state.
    ///
    /// Read on the **`latest`** tag, never `pending`: `pending` counts our own unconfirmed
    /// transactions, so it would report every in-flight send as landed and free the gate instantly.
    sender: Option<Address>,
    calldata: Bytes,
}

impl ChainReader {
    /// Build the reader and precompute its calldata.
    ///
    /// The call list is fixed for the process lifetime, so it is encoded once: two Multicall3
    /// helpers, one `snapshot` per pair, one `balanceOf` per token. That order is the decode
    /// contract.
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
    /// Folded into the existing batch rather than fetched separately: a deliverable read at a
    /// different block from the inventory it is compared against is a view that never existed.
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
    /// Uses the `pending` tag, which on the flashblocks endpoint is the ~200ms preconfirmed state,
    /// *fresher than the head that triggered this call*: a preconfirmed swap has already moved
    /// `bidUsed`/`askUsed`, and quoting against pre-swap usage puts the executable top at a point
    /// on the ladder the pool has already walked past.
    ///
    /// # Errors
    /// [`RpcError`]. Indexing into `results` is bounded by the `decode_batch` above, which rejects
    /// any length but the one asked for.
    #[allow(clippy::indexing_slicing)]
    pub async fn read(&self, rpc: &Rpc) -> Result<ChainView, RpcError> {
        // Concurrent, not serial: the nonce is account state so it cannot join the multicall, and
        // a serial round trip lands in the interval that decides how fast the gate reopens.
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
        // The sealed head, on the same trip: one keyless request, against a websocket that costs a
        // key and takes the clock with it when that key runs out.
        let sealed_call = async {
            rpc.call("eth_getBlockByNumber", serde_json::json!(["latest", false]))
                .await
                .ok()
                .and_then(|v| read_sealed(&v))
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

        // The lesser of what the maker holds and what it has let `PmmSettle` pull: either being
        // short is the same failure, since the settler custodies nothing and pulls with
        // `transferFrom` against the maker's own balance.
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

/// `(number, timestamp)` out of an `eth_getBlockByNumber` result. `None` unless both are present.
fn read_sealed(block: &serde_json::Value) -> Option<(u64, u64)> {
    let hex = |k: &str| {
        block
            .get(k)
            .and_then(serde_json::Value::as_str)
            .and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok())
    };
    Some((hex("number")?, hex("timestamp")?))
}

/// Decode an `aggregate3` response and insist every sub-call succeeded.
///
/// `allowFailure` is `false` on every call this crate builds, so a revert should surface as a
/// reverted `eth_call` rather than a `false` here. Length and flags are checked anyway, because a
/// partial batch decodes into a zero-filled snapshot and gets quoted.
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

// --- Startup verification ---

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

/// Round one's call list: `pairCount`, `updater`, every `pairConfig`, then every
/// `effectiveCapacity`. The last group is here only for its `decaySecs`, which `pairConfig` does
/// not carry; the order is what [`verify_against_chain`]'s indexes decode against.
fn verify_against_chain_calls(
    pool: Address,
    pairs: &[crate::config::PairConfig],
) -> Vec<abi::Call3> {
    let mut calls = Vec::with_capacity(2 + 2 * pairs.len());
    for call_data in [
        abi::pairCountCall {}.abi_encode(),
        abi::updaterCall {}.abi_encode(),
    ] {
        calls.push(abi::Call3 {
            target: pool,
            allowFailure: false,
            callData: call_data.into(),
        });
    }
    for p in pairs {
        calls.push(abi::Call3 {
            target: pool,
            allowFailure: false,
            callData: abi::pairConfigCall { pairId: p.pair_id }
                .abi_encode()
                .into(),
        });
    }
    for p in pairs {
        calls.push(abi::Call3 {
            target: pool,
            allowFailure: false,
            callData: abi::effectiveCapacityCall { pairId: p.pair_id }
                .abi_encode()
                .into(),
        });
    }
    calls
}

/// Every distinct token across the configured pairs, in first-seen order.
fn verify_against_chain_tokens(configs: &BTreeMap<u16, abi::PairConfigAbi>) -> Vec<Address> {
    let mut tokens: Vec<Address> = Vec::new();
    for c in configs.values() {
        for t in [c.base, c.quote] {
            if !tokens.contains(&t) {
                tokens.push(t);
            }
        }
    }
    tokens
}

/// Read the immutable facts and check the config against them, before the loop starts.
///
/// Every check has a silent failure mode: a `pair_id` that does not exist polls a zeroed snapshot
/// forever; `base_decimals` off by one prices the pair by a factor of ten; a `heartbeat_secs` at or
/// above the pool's `maxStaleSecs` lets the quote expire between pushes; pairs with different quote
/// tokens cannot share one NAV.
///
/// # Errors
/// [`RpcError`] for a failed read, or [`RpcError::Decode`] carrying the mismatch. Indexing into
/// `res` is bounded by the `decode_batch` that produced it, and the `BTreeMap` indexes are over
/// maps this function just built from the list it is iterating.
#[allow(clippy::indexing_slicing)]
pub async fn verify_against_chain(
    rpc: &Rpc,
    pool: Address,
    multicall: Address,
    cfg: &crate::config::Config,
) -> Result<ChainFacts, RpcError> {
    let calls = verify_against_chain_calls(pool, &cfg.pairs);
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
    let tokens = verify_against_chain_tokens(&configs);
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

    /// The three `-32000` occupants share a code and only the text separates them, so a predicate
    /// that drifts turns an acceptance into an alert, or a resync into neither.
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

        // Neither, and specifically not "already known": this one is a failed send.
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
        // A live-pool shape: capGen 14, usedGen 13, and a raw ask counter the pool ignores.
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

    /// Both ends of the ramp are exact by construction on chain, so they are exact here.
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
        // No ramp, so age must not touch capacity at any age.
        let cap = 1_000_000_000_000_000_000_000;
        assert_eq!(decayed(cap, 0, 0), cap);
        assert_eq!(decayed(cap, 86_400, 0), cap);
        assert_eq!(decayed(cap, u64::MAX, 0), cap);
    }

    #[test]
    fn the_ramp_floors_rather_than_rounds() {
        // 100 * 23 / 30 = 76.66..., and the pool keeps the 0.66.
        assert_eq!(decayed(100, 7, 30), 76);
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

    /// A quota refusal arrives as a JSON-RPC error object, which the pool otherwise treats as "the
    /// node refused this request" — but it is about the key, so the other keys answer fine.
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

        // The ordinary case is unchanged: a malformed request is malformed everywhere, and a
        // retry on the transmit path would be a second broadcast of the same intent.
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

    /// The same rule in other spellings: a code off the list makes the pool give up on the first
    /// endpoint with the rest sitting idle.
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

        // A read at a new block returns to healthy.
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
        // The hole the block-number signal closes: every `eth_call` lands, so "did the RPC reply"
        // says Healthy forever while the chain has produced no block.
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
        // Both references are the same instant, so the tie must resolve to the endpoint rather
        // than accuse the chain of stopping.
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
        // Staleness runs from the quieter signal, so a stream of reads that never advance cannot
        // mask a stalled chain.
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
        // Decoding our own calldata pins the layout the read's decoder assumes.
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
