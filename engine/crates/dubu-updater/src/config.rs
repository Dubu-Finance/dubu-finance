//! Configuration: parsed from TOML, unknown fields rejected, ranges checked at load.
//!
//! The rule this module exists to enforce is that **a bad config fails at startup, not at 3am**.
//! Two mechanisms do that, and both are worth more than they look:
//!
//! 1. `#[serde(deny_unknown_fields)]` on every struct. A typo'd key is otherwise silently
//!    ignored, and the knob you thought you turned stays at its default. `half_spred_bps = 50`
//!    is a five-basis-point quote that its author believes is fifty.
//! 2. [`Config::validate`], which runs every range check that does not need the chain. The
//!    checks that *do* need the chain — that a pair exists, that the configured token decimals
//!    match the deployed ERC-20s, that the heartbeat fits inside the pool's own `maxStaleSecs`
//!    — run in [`crate::chain::verify_against_chain`] immediately after the first poll, before
//!    the loop is allowed to compute anything.
//!
//! # Amounts are decimal strings, and why
//!
//! TOML integers are `i64`. A thousand mWETH is `10^21`, which does not fit, so every on-chain
//! amount in this file is a **string in human units** — `capacity = "1000"` means 1000 mWETH,
//! scaled by that pair's `base_decimals` at load. That is also the readable form: nobody should
//! be checking a risk limit by counting zeros.
//!
//! # Secrets
//!
//! There is no private key in this file and there is no field that could hold one. [`KeySource`]
//! names either an environment variable or a path; the value is read at startup and never
//! logged. See [`crate::tx`].
//!
//! The endpoint URLs are the second secret, and a less obvious one. Nodit puts the API key in
//! the **path** — `https://giwa-sepolia.nodit.io/<KEY>` — so the URL *is* the credential. Two
//! mechanisms keep it out of everything:
//!
//! 1. The config file holds `${NODIT_API_KEY}`, never a literal. [`EndpointUrl`] expands it from
//!    the environment at load; an unset variable is a startup error naming the *variable*.
//! 2. [`EndpointUrl`]'s `Display` and `Debug` are both **redacted** to `scheme://host/***`. The
//!    real string is reachable only through [`EndpointUrl::expose`], which the transport calls
//!    and nothing else does. That makes "never log the key" a property of the type rather than a
//!    rule every future `info!` has to remember.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use alloy_primitives::Address;
use serde::Deserialize;

use crate::units::{self, UnitsError};

/// Why a config was refused.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file could not be read.
    #[error("cannot read config `{path}`: {source}")]
    Io {
        /// Path that was attempted.
        path: PathBuf,
        /// Underlying IO failure.
        source: std::io::Error,
    },
    /// The file is not valid TOML, or has a field this crate does not know.
    #[error("cannot parse config: {0}")]
    Parse(#[from] toml::de::Error),
    /// A field is syntactically fine and semantically wrong.
    #[error("invalid config: {0}")]
    Invalid(String),
    /// An amount or price string could not be scaled.
    #[error("invalid amount: {0}")]
    Units(#[from] UnitsError),
}

fn invalid(msg: impl Into<String>) -> ConfigError {
    ConfigError::Invalid(msg.into())
}

// ---------------------------------------------------------------------------
// Endpoint URLs, which are credentials
// ---------------------------------------------------------------------------

/// A URL that may carry a credential, and therefore may never be printed.
///
/// Nodit's endpoint is `https://giwa-sepolia.nodit.io/<KEY>`: the API key is a path segment, so
/// the URL is not "a string that happens to contain a secret", it *is* the secret. This wrapper
/// makes that structural instead of a convention:
///
/// * `Display` and `Debug` both emit the **redacted** form, so `url = %cfg.chain.rpc_url` in a
///   `tracing` macro logs `https://giwa-sepolia.nodit.io/***` and there is no spelling of it
///   that logs anything else.
/// * The real string comes out of [`EndpointUrl::expose`] only, which is called by the reqwest
///   client and the websocket connect and nowhere else. Grep for it to audit every use.
///
/// Redaction keeps the scheme and the host — those are the useful half of a log line, and they
/// are not secret — and replaces any path, query or userinfo with `***`. Over-redacting a
/// key-free path costs nothing; under-redacting one costs the key.
#[derive(Clone, PartialEq, Eq)]
pub struct EndpointUrl {
    raw: String,
    redacted: String,
}

impl EndpointUrl {
    /// Build from an already-resolved URL string, expanding any `${VAR}` references.
    ///
    /// # Errors
    /// [`ConfigError::Invalid`] if a referenced variable is unset or empty. The message names
    /// the **variable**, never the value.
    pub fn resolve(field: &str, template: &str) -> Result<Self, ConfigError> {
        let raw = expand_env(field, template)?;
        let redacted = redact_url(&raw);
        Ok(Self { raw, redacted })
    }

    /// The real URL. **The only way to get it**, and the only callers are the HTTP client and
    /// the websocket connect.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.raw
    }

    /// The safe-to-log form: `scheme://host/***`.
    #[must_use]
    pub fn redacted(&self) -> &str {
        &self.redacted
    }

    /// The scheme, lower-cased, or `""` for a string with no `://`.
    #[must_use]
    pub fn scheme(&self) -> &str {
        self.raw.split_once("://").map_or("", |(s, _)| s)
    }

    fn is_http(&self) -> bool {
        matches!(self.scheme(), "http" | "https")
    }

    fn is_ws(&self) -> bool {
        matches!(self.scheme(), "ws" | "wss")
    }
}

/// Redacted, always. See the type docs — there is deliberately no un-redacted formatter.
impl std::fmt::Display for EndpointUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.redacted)
    }
}

/// Redacted, always. `Debug` matters as much as `Display` here: `?url` in a tracing macro and
/// `{:?}` on a struct that contains one both go through this.
impl std::fmt::Debug for EndpointUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.redacted)
    }
}

impl<'de> Deserialize<'de> for EndpointUrl {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        // The field name is not available here, so the error carries the template shape rather
        // than the field. `expand_env` puts the variable name in the message either way.
        Self::resolve("<url>", &s).map_err(serde::de::Error::custom)
    }
}

/// Expand every `${VAR}` in `template` from the environment.
///
/// An unset or empty variable is an error rather than an empty expansion: a URL with a blank key
/// segment produces a 401 at the first request, which is a much worse place to discover a
/// missing `.env` than startup.
fn expand_env(field: &str, template: &str) -> Result<String, ConfigError> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after.find('}').ok_or_else(|| {
            invalid(format!("{field}: unterminated `${{` in the URL template; expected `${{VAR}}`"))
        })?;
        let name = &after[..end];
        match std::env::var(name) {
            Ok(v) if !v.trim().is_empty() => out.push_str(v.trim()),
            _ => {
                return Err(invalid(format!(
                    "{field}: environment variable `{name}` is unset or empty. \
                     Put it in the crate's `.env` (gitignored) or export it; \
                     see `.env.example`. The value is never read from the config file."
                )))
            }
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// `scheme://host/***`, keeping only what is safe and useful in a log.
fn redact_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        // Not a URL shape at all. Whatever it is, it is not going in a log intact.
        return "***".to_string();
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    // `user:pass@host` is userinfo, which is also a credential.
    let authority = match authority.rsplit_once('@') {
        Some((_, host)) => format!("***@{host}"),
        None => authority.to_string(),
    };
    if tail.is_empty() || tail == "/" {
        format!("{scheme}://{authority}{tail}")
    } else {
        format!("{scheme}://{authority}/***")
    }
}

// ---------------------------------------------------------------------------
// .env
// ---------------------------------------------------------------------------

/// Load `KEY=VALUE` pairs from a dotenv-style file into the process environment.
///
/// Deliberately tiny and deliberately hand-rolled — the same reasoning as `tx.rs` encoding its
/// own EIP-1559 envelope. Three rules, all of which matter:
///
/// * **A variable already set in the real environment always wins.** The file is a convenience
///   for local runs, never an override of what an operator or a systemd unit set on purpose.
/// * **Nothing from the file is ever logged**, including key *names* being fine but values
///   never. The return value is a count, not the contents.
/// * A missing file is not an error. Production sets real environment variables and has no
///   `.env` at all.
///
/// Returns how many variables this file actually set.
pub fn load_dotenv(path: &Path) -> usize {
    let Ok(text) = std::fs::read_to_string(path) else { return 0 };
    let mut set = 0;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else { continue };
        let key = key.trim();
        if key.is_empty() || std::env::var_os(key).is_some() {
            continue;
        }
        let value = value.trim();
        // Strip one layer of matching quotes, so a value with trailing whitespace can be written
        // explicitly. Anything more (escapes, interpolation) is out of scope on purpose.
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
            .unwrap_or(value);
        std::env::set_var(key, value);
        set += 1;
    }
    set
}

// ---------------------------------------------------------------------------
// Top level
// ---------------------------------------------------------------------------

/// The whole configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// RPC endpoints, polling cadence and the request budget.
    pub chain: ChainConfig,
    /// Exchange market-data feed.
    pub feed: FeedConfig,
    /// Transaction construction and the transmit switch.
    pub tx: TxConfig,
    /// Killswitches.
    pub risk: RiskConfig,
    /// One entry per pair the bot quotes.
    pub pairs: Vec<PairConfig>,
}

impl Config {
    /// Read and validate a config file.
    ///
    /// # Errors
    /// [`ConfigError`] for an unreadable file, a parse failure (including an unknown field), or
    /// any failed range check.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path)
            .map_err(|source| ConfigError::Io { path: path.to_path_buf(), source })?;
        let cfg: Self = toml::from_str(&text)?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Every check that does not need the chain.
    ///
    /// # Errors
    /// [`ConfigError::Invalid`] naming the field, or [`ConfigError::Units`] for an unparseable
    /// amount.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.chain.validate()?;
        self.feed.validate()?;
        self.tx.validate()?;
        self.risk.validate()?;

        if self.pairs.is_empty() {
            return Err(invalid("pairs: at least one [[pairs]] entry is required"));
        }
        let mut seen_ids = BTreeSet::new();
        let mut seen_symbols = BTreeSet::new();
        for p in &self.pairs {
            p.validate()?;
            if !seen_ids.insert(p.pair_id) {
                return Err(invalid(format!("pairs: pair_id {} appears twice", p.pair_id)));
            }
            // Two pairs on one symbol is not obviously wrong, but it is never what was meant,
            // and it doubles the quote traffic for one price.
            if !seen_symbols.insert(p.symbol.clone()) {
                return Err(invalid(format!("pairs: symbol `{}` appears twice", p.symbol)));
            }
        }
        Ok(())
    }

    /// The pair entry for an id, if configured.
    #[must_use]
    pub fn pair(&self, pair_id: u16) -> Option<&PairConfig> {
        self.pairs.iter().find(|p| p.pair_id == pair_id)
    }
}

// ---------------------------------------------------------------------------
// Chain
// ---------------------------------------------------------------------------

/// The three endpoints, the head-subscription watchdog, and the liveness thresholds.
///
/// # Which endpoint does what, and why
///
/// | field | endpoint | used for | why that one |
/// |---|---|---|---|
/// | `ws_url` | Nodit WSS | `newHeads`, which **drives the loop** | the only one that answers `eth_subscribe`; 1s confirmed heads |
/// | `flashblocks_rpc_url` | GIWA flashblocks | every state read, `pending` tag | ~200ms preconfirmed state — fresher than any confirmed head |
/// | `rpc_url` | Nodit HTTPS | transactions, nonce, receipts, startup metadata | canonical, and no longer rate-limited |
///
/// The split is not redundancy, it is three different freshness guarantees. Heads say *when* to
/// look, flashblocks says *what is true right now* including preconfirmed swaps that have
/// already moved `bidUsed`, and the ordinary RPC says *what is final* — which is the only
/// acceptable basis for a nonce.
///
/// All three are [`EndpointUrl`], so they are redacted in every log line. Write them in the TOML
/// as `${NODIT_API_KEY}` templates; a literal key in a config file is the thing this type exists
/// to prevent.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChainConfig {
    /// Websocket RPC carrying the `newHeads` subscription that drives the quote loop. Must be a
    /// `ws://` or `wss://` URL — pointing this at an HTTPS endpoint gets
    /// `"notifications not supported"` from the node, which the subscription task logs by name.
    pub ws_url: EndpointUrl,
    /// Ordinary RPC. Transactions are submitted here, because this is the canonical view.
    pub rpc_url: EndpointUrl,
    /// Flashblocks RPC. **Read only, and only under the `pending` tag** — its `latest` lags the
    /// ordinary endpoint by about two blocks, so reading `latest` here is strictly worse than
    /// reading `latest` there. See [`crate::chain`].
    pub flashblocks_rpc_url: EndpointUrl,
    /// EIP-155 chain id. 91342 for GIWA Sepolia.
    pub chain_id: u64,
    /// `PropPool` address.
    pub pool: Address,
    /// Multicall3. Preinstalled at the canonical address in GIWA's genesis, which is what lets
    /// a whole read cycle be one request.
    pub multicall3: Address,
    /// The chain's block cadence. GIWA is 1s, and measured heads arrive at 904-1050ms. This is
    /// the unit the head watchdog is expressed in, so it is a fact about the chain rather than a
    /// tuning knob.
    #[serde(default = "d_block_time_ms")]
    pub block_time_ms: u64,
    /// **The head watchdog.** No head for this many block times means the subscription has gone
    /// quiet, and a quiet subscription is more dangerous than a broken one — the bot would sit
    /// believing the chain had stopped. Tripping it forces the fallback read immediately and
    /// hands the liveness question to [`crate::chain::ChainHealth`], which decides from the
    /// *block number* whether the chain stopped or only the socket did.
    #[serde(default = "d_head_stale_blocks")]
    pub head_stale_blocks: u32,
    /// First reconnect delay for the head subscription. Doubles per consecutive failure.
    /// A subscription that dies must not become a hot reconnect loop.
    #[serde(default = "d_ws_reconnect_initial_ms")]
    pub ws_reconnect_initial_ms: u64,
    /// Reconnect delay ceiling for the head subscription.
    #[serde(default = "d_ws_reconnect_max_ms")]
    pub ws_reconnect_max_ms: u64,
    /// **Fallback only.** The loop is driven by `newHeads`; this timer is the floor underneath
    /// it, so a subscription that dies or goes silent degrades into polling instead of stalling.
    /// It is not the primary driver and sizing it like one is a misreading — at a healthy 1s
    /// head cadence this timer essentially never fires.
    #[serde(default = "d_fallback_poll_interval_ms")]
    pub fallback_poll_interval_ms: u64,
    /// Per-request HTTP timeout.
    #[serde(default = "d_request_timeout_ms")]
    pub request_timeout_ms: u64,
    /// Runaway guard, requests per second, across *all* RPC use on one endpoint.
    ///
    /// This used to be a budget sized against a hostile public endpoint. It is not that any
    /// more — the dedicated endpoint took 20 rapid `eth_blockNumber` calls without a single 429 —
    /// and the default is now loose enough that normal operation never touches it. What it still
    /// buys is a ceiling on a *bug*: a reconnect storm or a spinning loop cannot turn into an
    /// unbounded request flood against the endpoint. Sized as a fuse, not as a budget.
    #[serde(default = "d_requests_per_sec")]
    pub requests_per_sec: f64,
    /// Burst allowance before the sustained rate binds. A send needs four or five requests in
    /// quick succession (nonce, submit, receipt polls), so this must be comfortably above one.
    #[serde(default = "d_request_burst")]
    pub request_burst: f64,
    /// First backoff after an HTTP 429. Doubles per consecutive 429, capped below.
    ///
    /// Kept, and not because 429s are expected: a dedicated endpoint is not an infinite one, and
    /// a bot with no backoff at all turns any transient upstream failure into a flood.
    #[serde(default = "d_rl_backoff_initial_ms")]
    pub rate_limit_backoff_initial_ms: u64,
    /// Backoff ceiling.
    #[serde(default = "d_rl_backoff_max_ms")]
    pub rate_limit_backoff_max_ms: u64,
    /// After this long with no successful read **and no new block**, the chain view is
    /// `Degraded`: quoting continues with [`ChainConfig::degraded_extra_half_spread_bps`] added
    /// to every half-spread.
    #[serde(default = "d_degraded_after_secs")]
    pub degraded_after_secs: u64,
    /// After this long with no successful read and no new block, the bot halts and withdraws
    /// quotes. Must exceed `degraded_after_secs`.
    #[serde(default = "d_halt_after_secs")]
    pub halt_after_secs: u64,
    /// A chain view older than this is stale and blocks a push, even if reads have not yet
    /// been failing long enough to count as degraded.
    #[serde(default = "d_view_stale_secs")]
    pub view_stale_secs: u64,
    /// Half-spread widening, in bps, while the chain view is degraded. Quoting into a view you
    /// cannot refresh is the adverse-selection case; widening is the cheap partial defence and
    /// halting is the complete one.
    #[serde(default = "d_degraded_extra_bps")]
    pub degraded_extra_half_spread_bps: u16,
}

fn d_block_time_ms() -> u64 {
    1_000
}
fn d_head_stale_blocks() -> u32 {
    10
}
fn d_ws_reconnect_initial_ms() -> u64 {
    500
}
fn d_ws_reconnect_max_ms() -> u64 {
    30_000
}
fn d_fallback_poll_interval_ms() -> u64 {
    2_000
}
fn d_request_timeout_ms() -> u64 {
    8_000
}
fn d_requests_per_sec() -> f64 {
    25.0
}
fn d_request_burst() -> f64 {
    50.0
}
fn d_rl_backoff_initial_ms() -> u64 {
    2_000
}
fn d_rl_backoff_max_ms() -> u64 {
    120_000
}
fn d_degraded_after_secs() -> u64 {
    30
}
fn d_halt_after_secs() -> u64 {
    600
}
fn d_view_stale_secs() -> u64 {
    20
}
fn d_degraded_extra_bps() -> u16 {
    25
}

impl ChainConfig {
    /// How long without a `newHeads` delivery counts as a silent subscription.
    ///
    /// `block_time_ms * head_stale_blocks`. Expressed as a multiple rather than an absolute so
    /// that it stays correct if the chain's cadence changes.
    #[must_use]
    pub const fn head_stale_after(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.block_time_ms.saturating_mul(self.head_stale_blocks as u64))
    }

    fn validate(&self) -> Result<(), ConfigError> {
        for (name, url) in
            [("rpc_url", &self.rpc_url), ("flashblocks_rpc_url", &self.flashblocks_rpc_url)]
        {
            if !url.is_http() {
                return Err(invalid(format!("chain.{name}: must be an http(s) URL, got `{url}`")));
            }
        }
        // A websocket URL is not optional and an http one will never subscribe: the endpoint
        // answers `notifications not supported` and the loop would fall back to polling forever
        // while looking configured. Caught here instead.
        if !self.ws_url.is_ws() {
            return Err(invalid(format!(
                "chain.ws_url: must be a ws(s) URL, got `{}`; an http(s) endpoint answers \
                 `notifications not supported` to eth_subscribe and would never drive the loop",
                self.ws_url
            )));
        }
        if self.chain_id == 0 {
            return Err(invalid("chain.chain_id: must be non-zero"));
        }
        if self.pool.is_zero() {
            return Err(invalid("chain.pool: must not be the zero address"));
        }
        if self.multicall3.is_zero() {
            return Err(invalid("chain.multicall3: must not be the zero address"));
        }
        if !(100..=60_000).contains(&self.block_time_ms) {
            return Err(invalid("chain.block_time_ms: must be 100..=60000"));
        }
        // One missed head is ordinary jitter — the measured cadence is 904-1050ms, so a window
        // of one block time would trip constantly and a watchdog that cries wolf gets ignored.
        if self.head_stale_blocks < 2 {
            return Err(invalid(
                "chain.head_stale_blocks: must be >= 2; a one-block window trips on ordinary jitter",
            ));
        }
        if !(250..=600_000).contains(&self.fallback_poll_interval_ms) {
            return Err(invalid(format!(
                "chain.fallback_poll_interval_ms: must be 250..=600000, got {}",
                self.fallback_poll_interval_ms
            )));
        }
        // The fallback exists to catch a dead subscription, not to race a live one. Set below
        // the block time it would fire between every pair of heads and quietly become the
        // primary driver again, which is the design this endpoint made unnecessary.
        if self.fallback_poll_interval_ms < self.block_time_ms {
            return Err(invalid(format!(
                "chain.fallback_poll_interval_ms ({}) is below chain.block_time_ms ({}); \
                 the fallback would fire between heads and silently become the primary driver",
                self.fallback_poll_interval_ms, self.block_time_ms
            )));
        }
        if !(500..=120_000).contains(&self.request_timeout_ms) {
            return Err(invalid("chain.request_timeout_ms: must be 500..=120000"));
        }
        if !(self.requests_per_sec.is_finite() && self.requests_per_sec > 0.0 && self.requests_per_sec <= 1_000.0) {
            return Err(invalid("chain.requests_per_sec: must be a finite value in (0, 1000]"));
        }
        if !(self.request_burst.is_finite() && self.request_burst >= 1.0 && self.request_burst <= 2_000.0) {
            return Err(invalid("chain.request_burst: must be a finite value in [1, 2000]"));
        }
        if self.ws_reconnect_initial_ms == 0 || self.ws_reconnect_max_ms < self.ws_reconnect_initial_ms {
            return Err(invalid(
                "chain.ws_reconnect_max_ms must be >= ws_reconnect_initial_ms, which must be non-zero",
            ));
        }
        if self.rate_limit_backoff_initial_ms == 0 || self.rate_limit_backoff_max_ms < self.rate_limit_backoff_initial_ms
        {
            return Err(invalid(
                "chain.rate_limit_backoff_max_ms must be >= rate_limit_backoff_initial_ms, which must be non-zero",
            ));
        }
        if self.degraded_after_secs == 0 {
            return Err(invalid("chain.degraded_after_secs: must be non-zero"));
        }
        if self.halt_after_secs <= self.degraded_after_secs {
            return Err(invalid(format!(
                "chain.halt_after_secs ({}) must exceed chain.degraded_after_secs ({}); \
                 otherwise the bot halts without ever widening",
                self.halt_after_secs, self.degraded_after_secs
            )));
        }
        // The watchdog has to have room to fire, be logged, and let the fallback prove whether
        // the chain is actually down — all before the halt timer expires. A window at or beyond
        // `halt_after_secs` means the bot withdraws quotes without the watchdog ever having said
        // anything, and the operator is left diagnosing a halt with no signal explaining it.
        let watchdog_secs = self.head_stale_after().as_secs();
        if watchdog_secs >= self.halt_after_secs {
            return Err(invalid(format!(
                "chain.head_stale_blocks x block_time_ms is {watchdog_secs}s, at or beyond \
                 chain.halt_after_secs ({}); the head watchdog would never fire before the halt",
                self.halt_after_secs
            )));
        }
        if self.view_stale_secs == 0 {
            return Err(invalid("chain.view_stale_secs: must be non-zero"));
        }
        if self.degraded_extra_half_spread_bps as u128 > dubu_core::ladder::MAX_BPS {
            return Err(invalid("chain.degraded_extra_half_spread_bps: must be <= 9999"));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Feed
// ---------------------------------------------------------------------------

/// Exchange market-data feed.
///
/// **Market data only.** There is no API key field, no secret field, and no order-entry code
/// path anywhere in this crate. The design is unhedged precisely because a Korean corporate
/// real-name exchange account is not available, so there is no account to place an order
/// against even if the code existed. See the crate README.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeedConfig {
    /// Combined-stream endpoint. `wss://stream.binance.com:9443/stream` — the stream list is
    /// built from the per-pair symbols, so this is the bare endpoint.
    pub ws_url: String,
    /// A symbol with no accepted tick for this long is **stale**, and a stale feed blocks every
    /// push. Not a soft signal: [`crate::feed::FeedSnapshot::live`] returns nothing past it.
    #[serde(default = "d_feed_stale_ms")]
    pub stale_after_ms: u64,
    /// First reconnect delay. Doubles per consecutive failure up to the ceiling.
    #[serde(default = "d_reconnect_initial_ms")]
    pub reconnect_initial_ms: u64,
    /// Reconnect delay ceiling.
    #[serde(default = "d_reconnect_max_ms")]
    pub reconnect_max_ms: u64,
    /// No frame at all — not even a server ping — for this long forces a reconnect. Binance
    /// pings every 20s, so a silent socket is a dead socket well before the TCP stack notices.
    #[serde(default = "d_read_timeout_ms")]
    pub read_timeout_ms: u64,
    /// A tick whose micro-price is more than this far from the last accepted one is rejected as
    /// an outlier. See [`crate::fair_value`] for how a genuine fast move gets through anyway.
    #[serde(default = "d_max_jump_bps")]
    pub max_jump_bps: u32,
    /// How many consecutive outliers before the tracker concedes the level has really moved.
    #[serde(default = "d_outlier_tolerance")]
    pub outlier_tolerance: u32,
}

fn d_feed_stale_ms() -> u64 {
    5_000
}
fn d_reconnect_initial_ms() -> u64 {
    500
}
fn d_reconnect_max_ms() -> u64 {
    30_000
}
fn d_read_timeout_ms() -> u64 {
    45_000
}
fn d_max_jump_bps() -> u32 {
    200
}
fn d_outlier_tolerance() -> u32 {
    3
}

impl FeedConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        if !(self.ws_url.starts_with("wss://") || self.ws_url.starts_with("ws://")) {
            return Err(invalid(format!("feed.ws_url: must be a ws(s) URL, got `{}`", self.ws_url)));
        }
        if !(100..=600_000).contains(&self.stale_after_ms) {
            return Err(invalid("feed.stale_after_ms: must be 100..=600000"));
        }
        if self.reconnect_initial_ms == 0 || self.reconnect_max_ms < self.reconnect_initial_ms {
            return Err(invalid("feed.reconnect_max_ms must be >= reconnect_initial_ms, which must be non-zero"));
        }
        if self.read_timeout_ms < self.stale_after_ms {
            return Err(invalid(format!(
                "feed.read_timeout_ms ({}) must be >= feed.stale_after_ms ({}); \
                 reconnecting sooner than the staleness window makes the window unobservable",
                self.read_timeout_ms, self.stale_after_ms
            )));
        }
        if !(1..=10_000).contains(&self.max_jump_bps) {
            return Err(invalid("feed.max_jump_bps: must be 1..=10000"));
        }
        if !(1..=100).contains(&self.outlier_tolerance) {
            return Err(invalid("feed.outlier_tolerance: must be 1..=100"));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Transactions
// ---------------------------------------------------------------------------

/// Where the signing key comes from. Never the config file itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeySource {
    /// Read from this environment variable.
    Env(String),
    /// Read from this file, whitespace trimmed.
    File(PathBuf),
}

/// Transaction construction, and the switch that decides whether anything is broadcast.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TxConfig {
    /// **Must be explicitly `true` to broadcast anything.** Absent means dry run. This is the
    /// one default in the file that is a safety property rather than a convenience.
    #[serde(default)]
    pub transmit_allowed: bool,
    /// Name of the environment variable holding the updater's private key.
    #[serde(default)]
    pub private_key_env: Option<String>,
    /// Path to a file holding the updater's private key. Mutually exclusive with
    /// `private_key_env`.
    #[serde(default)]
    pub private_key_file: Option<PathBuf>,
    /// Gas limit. `updateQuote` for one pair measured 28,747 gas and `refreshCapacity` is
    /// cheaper; this is deliberately several times that, because gas is nearly free here and a
    /// quote that fails to land is not.
    #[serde(default = "d_gas_limit")]
    pub gas_limit: u64,
    /// `maxFeePerGas`, in gwei, as a decimal string.
    #[serde(default = "d_max_fee_gwei")]
    pub max_fee_per_gas_gwei: String,
    /// `maxPriorityFeePerGas`, in gwei, as a decimal string. See [`crate::tx`] for why there is
    /// no escalator behind this number.
    #[serde(default = "d_max_priority_fee_gwei")]
    pub max_priority_fee_per_gas_gwei: String,
    /// How long a submitted transaction may stay unconfirmed before the pair is unblocked and
    /// the nonce resynced. Until then the pair is **not** superseded.
    #[serde(default = "d_pending_timeout_secs")]
    pub pending_timeout_secs: u64,
}

fn d_gas_limit() -> u64 {
    400_000
}
fn d_max_fee_gwei() -> String {
    "0.05".into()
}
fn d_max_priority_fee_gwei() -> String {
    "0.005".into()
}
fn d_pending_timeout_secs() -> u64 {
    120
}

impl TxConfig {
    /// Where the key is to be read from, if configured at all.
    ///
    /// # Errors
    /// [`ConfigError::Invalid`] if both sources are set.
    pub fn key_source(&self) -> Result<Option<KeySource>, ConfigError> {
        match (&self.private_key_env, &self.private_key_file) {
            (Some(_), Some(_)) => {
                Err(invalid("tx: set exactly one of private_key_env / private_key_file, not both"))
            }
            (Some(v), None) => Ok(Some(KeySource::Env(v.clone()))),
            (None, Some(p)) => Ok(Some(KeySource::File(p.clone()))),
            (None, None) => Ok(None),
        }
    }

    /// `maxFeePerGas` in wei.
    ///
    /// # Errors
    /// [`ConfigError::Units`] if the string is not a decimal.
    pub fn max_fee_wei(&self) -> Result<u128, ConfigError> {
        Ok(units::parse_fixed(&self.max_fee_per_gas_gwei, 9)?)
    }

    /// `maxPriorityFeePerGas` in wei.
    ///
    /// # Errors
    /// [`ConfigError::Units`] if the string is not a decimal.
    pub fn max_priority_fee_wei(&self) -> Result<u128, ConfigError> {
        Ok(units::parse_fixed(&self.max_priority_fee_per_gas_gwei, 9)?)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        let key = self.key_source()?;
        if self.transmit_allowed && key.is_none() {
            return Err(invalid(
                "tx.transmit_allowed is true but no key source is set; \
                 set private_key_env or private_key_file",
            ));
        }
        if let Some(KeySource::Env(name)) = &key {
            if name.trim().is_empty() {
                return Err(invalid("tx.private_key_env: must name an environment variable"));
            }
            // A hex string here means someone pasted the key where the variable name goes.
            if name.starts_with("0x") || name.len() >= 64 {
                return Err(invalid(
                    "tx.private_key_env looks like a key rather than a variable name; \
                     this field holds the NAME of an environment variable",
                ));
            }
        }
        if !(21_000..=30_000_000).contains(&self.gas_limit) {
            return Err(invalid("tx.gas_limit: must be 21000..=30000000"));
        }
        let max = self.max_fee_wei()?;
        let tip = self.max_priority_fee_wei()?;
        if max == 0 {
            return Err(invalid("tx.max_fee_per_gas_gwei: must be non-zero"));
        }
        if tip > max {
            return Err(invalid(format!(
                "tx.max_priority_fee_per_gas_gwei ({tip} wei) exceeds tx.max_fee_per_gas_gwei ({max} wei)"
            )));
        }
        if !(5..=3_600).contains(&self.pending_timeout_secs) {
            return Err(invalid("tx.pending_timeout_secs: must be 5..=3600"));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Risk
// ---------------------------------------------------------------------------

/// The two killswitches. Both latch, and the latch survives a restart.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RiskConfig {
    /// Where the latch lives. Written atomically (temp file plus rename) after every change.
    pub state_path: PathBuf,
    /// Decimals of the token NAV is denominated in — the pairs' shared quote token, mUSDC, so
    /// 6. Verified against the chain at startup.
    #[serde(default = "d_nav_decimals")]
    pub nav_decimals: u8,
    /// Window for the short-horizon bleed limit, in seconds.
    pub bleed_window_secs: u64,
    /// Peak-to-current NAV drawdown inside the window that trips the bleed switch. Human units
    /// of the NAV token, so `"2000"` is 2000 mUSDC.
    pub bleed_limit: String,
    /// Cumulative gross loss that trips the budget switch. Gross: every negative NAV step is
    /// added, and a subsequent recovery does **not** give the budget back. Human units.
    pub loss_budget: String,
}

fn d_nav_decimals() -> u8 {
    6
}

impl RiskConfig {
    /// [`RiskConfig::bleed_limit`] scaled to the NAV token's smallest unit.
    ///
    /// # Errors
    /// [`ConfigError::Units`] if the string is not a decimal.
    pub fn bleed_limit_units(&self) -> Result<u128, ConfigError> {
        Ok(units::parse_fixed(&self.bleed_limit, self.nav_decimals)?)
    }

    /// [`RiskConfig::loss_budget`] scaled to the NAV token's smallest unit.
    ///
    /// # Errors
    /// [`ConfigError::Units`] if the string is not a decimal.
    pub fn loss_budget_units(&self) -> Result<u128, ConfigError> {
        Ok(units::parse_fixed(&self.loss_budget, self.nav_decimals)?)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.state_path.as_os_str().is_empty() {
            return Err(invalid("risk.state_path: must be a path"));
        }
        if self.nav_decimals > 30 {
            return Err(invalid("risk.nav_decimals: must be <= 30"));
        }
        if !(1..=86_400).contains(&self.bleed_window_secs) {
            return Err(invalid("risk.bleed_window_secs: must be 1..=86400"));
        }
        let bleed = self.bleed_limit_units()?;
        let budget = self.loss_budget_units()?;
        if bleed == 0 {
            return Err(invalid("risk.bleed_limit: must be non-zero; a zero limit trips instantly"));
        }
        if budget == 0 {
            return Err(invalid("risk.loss_budget: must be non-zero; a zero budget trips instantly"));
        }
        if budget < bleed {
            return Err(invalid(format!(
                "risk.loss_budget ({budget}) is below risk.bleed_limit ({bleed}); \
                 the cumulative budget would always trip first and the bleed switch is then dead code"
            )));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Pairs
// ---------------------------------------------------------------------------

/// One quoted pair.
///
/// # The symbol is not the token
///
/// `symbol` is the **real** asset the pool's mock token tracks, not a market in the mock token.
/// `mWETH` has no market anywhere; it is a mock ERC-20 we deployed, and nothing trades it. The
/// bot prices pairId 1 off Binance's `ETHUSDT` because that is the asset the mock stands in for,
/// which is what makes the demo live and what makes a later markout study mean anything. A
/// reader who takes `symbol = "ETHUSDT"` to mean "mWETH trades at 1943" has misread it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairConfig {
    /// `PropPool` pair id. 1-based; id 0 is reserved and never a real pair.
    pub pair_id: u16,
    /// Exchange symbol of the **real** asset this pair's mock base token tracks. See the struct
    /// docs — this is not a market in the mock token.
    pub symbol: String,
    /// Decimals of the base token. Verified against the deployed ERC-20 at startup.
    pub base_decimals: u8,
    /// Decimals of the quote token. Verified against the deployed ERC-20 at startup.
    pub quote_decimals: u8,
    /// Half the bid/ask spread, in bps of the skewed fair value.
    pub half_spread_bps: u16,
    /// Ladder width, in bps of the near price. This is the concentration knob: the price decays
    /// this far across the whole posted depth. It is an *upper* bound — the inverse solver
    /// narrows it further whenever the price bounds bind.
    pub width_bps: u16,
    /// Inventory skew, in bps. Positive shifts the whole book down. Static here; see the README
    /// for what an inventory-driven skew would need.
    #[serde(default)]
    pub skew_bps: i16,
    /// The trade size the half-spread is guaranteed over, in human base units. The solver picks
    /// the widest ladder whose *average* price over `[0, capture]` hits the target, so this is
    /// the size the quote is honest about rather than the size it is limited to.
    pub capture: String,
    /// Base the pool will buy per epoch, in human base units.
    pub bid_capacity: String,
    /// Base the pool will sell per epoch, in human base units.
    pub ask_capacity: String,
    /// Re-post at least this often, in seconds, even with no price move. Must sit inside the
    /// pool's own `maxStaleSecs`, which is checked against the chain at startup.
    pub heartbeat_secs: u64,
    /// Drift threshold, in bps, when the market has moved **against** the posted quote — our
    /// bid is now above fair, or our ask below it. This is the pick-off direction; keep it
    /// small. See [`crate::policy`].
    pub adverse_drift_bps: u32,
    /// Drift threshold, in bps, when the posted quote has merely become conservative. Costs
    /// volume, not money; keep it larger than `adverse_drift_bps` so a quiet market does not
    /// churn gas.
    pub favourable_drift_bps: u32,
    /// Refresh the capacity epoch once remaining capacity has fallen this far, in percent,
    /// below the configured capacity.
    pub capacity_divergence_pct: u32,
}

impl PairConfig {
    /// [`PairConfig::capture`] in base units.
    ///
    /// # Errors
    /// [`ConfigError::Units`].
    pub fn capture_units(&self) -> Result<u128, ConfigError> {
        Ok(units::parse_fixed(&self.capture, self.base_decimals)?)
    }

    /// [`PairConfig::bid_capacity`] in base units.
    ///
    /// # Errors
    /// [`ConfigError::Units`].
    pub fn bid_capacity_units(&self) -> Result<u128, ConfigError> {
        Ok(units::parse_fixed(&self.bid_capacity, self.base_decimals)?)
    }

    /// [`PairConfig::ask_capacity`] in base units.
    ///
    /// # Errors
    /// [`ConfigError::Units`].
    pub fn ask_capacity_units(&self) -> Result<u128, ConfigError> {
        Ok(units::parse_fixed(&self.ask_capacity, self.base_decimals)?)
    }

    /// Lower-case Binance stream name, e.g. `ethusdt@bookTicker`.
    #[must_use]
    pub fn stream_name(&self) -> String {
        format!("{}@bookTicker", self.symbol.to_lowercase())
    }

    fn validate(&self) -> Result<(), ConfigError> {
        let id = self.pair_id;
        if id == 0 {
            return Err(invalid("pairs.pair_id: 0 is reserved and is never a real pair"));
        }
        if self.symbol.is_empty() || !self.symbol.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return Err(invalid(format!("pairs[{id}].symbol: must be alphanumeric, got `{}`", self.symbol)));
        }
        if self.base_decimals > 30 || self.quote_decimals > 30 {
            return Err(invalid(format!("pairs[{id}]: token decimals must be <= 30")));
        }
        let max_bps = dubu_core::ladder::MAX_BPS;
        if u128::from(self.half_spread_bps) > max_bps {
            return Err(invalid(format!("pairs[{id}].half_spread_bps: must be <= {max_bps}")));
        }
        if u128::from(self.width_bps) > max_bps {
            return Err(invalid(format!("pairs[{id}].width_bps: must be <= {max_bps}")));
        }
        if i128::from(self.skew_bps).unsigned_abs() > max_bps {
            return Err(invalid(format!("pairs[{id}].skew_bps: must be within +/-{max_bps}")));
        }
        // A zero half-spread quotes both sides at fair value and loses the spread to every
        // taker. It is never a configuration, only a typo.
        if self.half_spread_bps == 0 {
            return Err(invalid(format!(
                "pairs[{id}].half_spread_bps: must be non-zero; a zero spread quotes both sides at fair value"
            )));
        }

        let capture = self.capture_units()?;
        let bid_cap = self.bid_capacity_units()?;
        let ask_cap = self.ask_capacity_units()?;
        if capture == 0 {
            return Err(invalid(format!("pairs[{id}].capture: must be non-zero")));
        }
        if bid_cap == 0 || ask_cap == 0 {
            return Err(invalid(format!(
                "pairs[{id}]: bid_capacity and ask_capacity must be non-zero; zero capacity quotes nothing"
            )));
        }
        // `PropPool` holds capacity in a uint96.
        for (name, v) in [("bid_capacity", bid_cap), ("ask_capacity", ask_cap)] {
            if v > dubu_core::curve::MAX_AMOUNT {
                return Err(invalid(format!("pairs[{id}].{name}: exceeds the pool's uint96 capacity field")));
            }
        }
        // The solver clamps capture to capacity anyway, but a capture above capacity means the
        // configured guarantee is not the one that will be posted, and silently honouring a
        // smaller one is the kind of surprise this file exists to prevent.
        if capture > bid_cap.min(ask_cap) {
            return Err(invalid(format!(
                "pairs[{id}].capture ({capture}) exceeds a capacity ({}); \
                 the solver would clamp it and the posted guarantee would not be the configured one",
                bid_cap.min(ask_cap)
            )));
        }

        if self.heartbeat_secs == 0 {
            return Err(invalid(format!("pairs[{id}].heartbeat_secs: must be non-zero")));
        }
        if self.adverse_drift_bps == 0 || self.favourable_drift_bps == 0 {
            return Err(invalid(format!("pairs[{id}]: drift thresholds must be non-zero")));
        }
        if self.adverse_drift_bps > self.favourable_drift_bps {
            return Err(invalid(format!(
                "pairs[{id}]: adverse_drift_bps ({}) must be <= favourable_drift_bps ({}); \
                 reacting more slowly to an adverse move than to a favourable one is backwards",
                self.adverse_drift_bps, self.favourable_drift_bps
            )));
        }
        if !(1..=100).contains(&self.capacity_divergence_pct) {
            return Err(invalid(format!("pairs[{id}].capacity_divergence_pct: must be 1..=100")));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A config that passes, used as the base for the negative cases below.
    fn good() -> &'static str {
        r#"
[chain]
ws_url = "wss://giwa-sepolia.nodit.io/TESTKEY"
rpc_url = "https://giwa-sepolia.nodit.io/TESTKEY"
flashblocks_rpc_url = "https://sepolia-rpc-flashblocks.giwa.io"
chain_id = 91342
pool = "0xA629071E606F425dB93310c3ecc35E00Fbe16358"
multicall3 = "0xcA11bde05977b3631167028862bE2a173976CA11"
fallback_poll_interval_ms = 2000

[feed]
ws_url = "wss://stream.binance.com:9443/stream"

[tx]

[risk]
state_path = "state/killswitch.json"
bleed_window_secs = 300
bleed_limit = "2000"
loss_budget = "10000"

[[pairs]]
pair_id = 1
symbol = "ETHUSDT"
base_decimals = 18
quote_decimals = 6
half_spread_bps = 5
width_bps = 25
capture = "20"
bid_capacity = "1000"
ask_capacity = "1000"
heartbeat_secs = 2400
adverse_drift_bps = 2
favourable_drift_bps = 8
capacity_divergence_pct = 30
"#
    }

    fn parse(s: &str) -> Result<Config, ConfigError> {
        let cfg: Config = toml::from_str(s)?;
        cfg.validate()?;
        Ok(cfg)
    }

    #[test]
    fn the_reference_config_loads() {
        let cfg = parse(good()).expect("reference config must validate");
        assert_eq!(cfg.pairs.len(), 1);
        assert_eq!(cfg.pairs[0].capture_units().unwrap(), 20_000_000_000_000_000_000);
        assert_eq!(cfg.pairs[0].stream_name(), "ethusdt@bookTicker");
    }

    #[test]
    fn dry_run_is_the_default_and_needs_no_key() {
        let cfg = parse(good()).unwrap();
        assert!(!cfg.tx.transmit_allowed, "omitting transmit_allowed must mean dry run");
        assert_eq!(cfg.tx.key_source().unwrap(), None);
    }

    #[test]
    fn transmitting_without_a_key_source_is_refused() {
        let s = good().replace("[tx]\n", "[tx]\ntransmit_allowed = true\n");
        assert!(matches!(parse(&s), Err(ConfigError::Invalid(m)) if m.contains("no key source")));
    }

    #[test]
    fn a_key_pasted_into_the_variable_name_field_is_refused() {
        let s = good().replace(
            "[tx]\n",
            "[tx]\nprivate_key_env = \"0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d\"\n",
        );
        assert!(matches!(parse(&s), Err(ConfigError::Invalid(m)) if m.contains("NAME of an environment variable")));
    }

    #[test]
    fn both_key_sources_at_once_is_refused() {
        let s = good().replace("[tx]\n", "[tx]\nprivate_key_env = \"K\"\nprivate_key_file = \"/k\"\n");
        assert!(matches!(parse(&s), Err(ConfigError::Invalid(m)) if m.contains("exactly one")));
    }

    #[test]
    fn an_unknown_field_is_a_hard_error_not_a_silent_default() {
        // The whole point of deny_unknown_fields: this typo would otherwise quote 5 bp while
        // its author believed it quoted 50.
        let s = good().replace("half_spread_bps = 5", "half_spred_bps = 50\nhalf_spread_bps = 5");
        let err = parse(&s).unwrap_err();
        assert!(matches!(err, ConfigError::Parse(_)), "expected a parse error, got {err}");
    }

    #[test]
    fn a_fallback_faster_than_the_block_time_is_refused() {
        // The fallback exists to catch a dead subscription, not to race a live one. Below the
        // block time it fires between heads and quietly becomes the primary driver again —
        // which is the design the dedicated endpoint made unnecessary.
        let s = good().replace("fallback_poll_interval_ms = 2000", "fallback_poll_interval_ms = 500");
        assert!(matches!(parse(&s), Err(ConfigError::Invalid(m)) if m.contains("silently become the primary driver")));
    }

    #[test]
    fn an_http_ws_url_is_refused_because_it_would_never_subscribe() {
        // Measured: the HTTPS endpoint answers `notifications not supported`. The loop would
        // then run on its fallback forever while every log line said it was configured for
        // heads, which is exactly the silent degradation this rewrite is about.
        let s = good().replace(
            r#"ws_url = "wss://giwa-sepolia.nodit.io/TESTKEY""#,
            r#"ws_url = "https://giwa-sepolia.nodit.io/TESTKEY""#,
        );
        assert!(matches!(parse(&s), Err(ConfigError::Invalid(m)) if m.contains("notifications not supported")));
    }

    #[test]
    fn a_one_block_watchdog_window_is_refused() {
        let s = good().replace("fallback_poll_interval_ms = 2000", "fallback_poll_interval_ms = 2000\nhead_stale_blocks = 1");
        assert!(matches!(parse(&s), Err(ConfigError::Invalid(m)) if m.contains("ordinary jitter")));
    }

    #[test]
    fn a_watchdog_window_beyond_the_halt_timer_is_refused() {
        // Otherwise the bot withdraws quotes without the watchdog ever having said anything,
        // and the operator diagnoses a halt with no signal explaining it.
        let s = good().replace(
            "fallback_poll_interval_ms = 2000",
            "fallback_poll_interval_ms = 2000\nhead_stale_blocks = 900\nhalt_after_secs = 600",
        );
        assert!(matches!(parse(&s), Err(ConfigError::Invalid(m)) if m.contains("never fire before the halt")));
    }

    #[test]
    fn the_watchdog_window_is_a_multiple_of_the_block_time() {
        let cfg = parse(good()).unwrap();
        assert_eq!(cfg.chain.block_time_ms, 1_000, "GIWA is a 1s chain");
        assert_eq!(cfg.chain.head_stale_after(), std::time::Duration::from_secs(10));
    }

    #[test]
    fn reacting_slower_to_adverse_than_favourable_drift_is_refused() {
        let s = good().replace("adverse_drift_bps = 2", "adverse_drift_bps = 20");
        assert!(matches!(parse(&s), Err(ConfigError::Invalid(m)) if m.contains("backwards")));
    }

    #[test]
    fn a_loss_budget_below_the_bleed_limit_is_refused() {
        let s = good().replace("loss_budget = \"10000\"", "loss_budget = \"100\"");
        assert!(matches!(parse(&s), Err(ConfigError::Invalid(m)) if m.contains("bleed switch is then dead code")));
    }

    #[test]
    fn a_zero_half_spread_is_refused() {
        let s = good().replace("half_spread_bps = 5", "half_spread_bps = 0");
        assert!(matches!(parse(&s), Err(ConfigError::Invalid(m)) if m.contains("quotes both sides at fair value")));
    }

    #[test]
    fn a_capture_above_capacity_is_refused_rather_than_clamped() {
        let s = good().replace("capture = \"20\"", "capture = \"5000\"");
        assert!(matches!(parse(&s), Err(ConfigError::Invalid(m)) if m.contains("would clamp it")));
    }

    /// The `[[pairs]]` block of [`good`], verbatim, for building duplicates.
    fn pair_block() -> String {
        format!("[[pairs]]{}", good().split_once("[[pairs]]").unwrap().1)
    }

    #[test]
    fn a_duplicate_pair_id_is_refused() {
        let two = format!("{}{}", good(), pair_block());
        assert!(matches!(parse(&two), Err(ConfigError::Invalid(m)) if m.contains("pair_id 1 appears twice")));
    }

    #[test]
    fn a_duplicate_symbol_on_two_pairs_is_refused() {
        // Distinct ids, same symbol: two rows driven by one price, at double the quote traffic.
        let second = pair_block().replace("pair_id = 1", "pair_id = 2");
        let two = format!("{}{second}", good());
        assert!(matches!(parse(&two), Err(ConfigError::Invalid(m)) if m.contains("`ETHUSDT` appears twice")));
    }

    #[test]
    fn halting_before_widening_is_refused() {
        let s = good().replace(
            "fallback_poll_interval_ms = 2000",
            "fallback_poll_interval_ms = 2000\ndegraded_after_secs = 30\nhalt_after_secs = 20",
        );
        assert!(matches!(parse(&s), Err(ConfigError::Invalid(m)) if m.contains("must exceed")));
    }

    // -----------------------------------------------------------------------
    // The API key must not leak
    // -----------------------------------------------------------------------

    /// A **fake** key with the same shape as a real Nodit one — 32 characters of base64-ish
    /// alphabet including the `~` and `-` that appear in real ones, since those are exactly the
    /// characters a naive URL parser mishandles. Never a real key: this file is committed, and
    /// the whole point of `EndpointUrl` is that a credential never lands in source.
    const KEY: &str = "EXAMPLEexample0~ExampleKey-000000";
    const KEYED: &str = "https://giwa-sepolia.nodit.io/EXAMPLEexample0~ExampleKey-000000";

    #[test]
    fn a_keyed_url_is_redacted_by_every_formatter() {
        let u = EndpointUrl::resolve("chain.rpc_url", KEYED).unwrap();
        // The two ways a URL reaches a log: `%url` and `?url`.
        let displayed = format!("{u}");
        let debugged = format!("{u:?}");
        for rendered in [&displayed, &debugged] {
            assert!(!rendered.contains(KEY), "the API key reached a formatter: {rendered}");
        }
        assert_eq!(displayed, "https://giwa-sepolia.nodit.io/***");
        // ... and the host survives, because a redaction that hides which endpoint failed is
        // useless for diagnosis.
        assert!(displayed.contains("giwa-sepolia.nodit.io"));
        // The real value is still reachable, but only through the one accessor.
        assert_eq!(u.expose(), KEYED);
    }

    #[test]
    fn redaction_covers_query_strings_and_userinfo_too() {
        // Other providers put the key in a query parameter or in userinfo. Neither shape is
        // used here, and both must still be safe if someone points the config at one.
        let q = EndpointUrl::resolve("chain.rpc_url", "https://rpc.example.com/v1?apikey=SECRET").unwrap();
        assert_eq!(q.to_string(), "https://rpc.example.com/***");
        let ui = EndpointUrl::resolve("chain.rpc_url", "https://user:SECRET@rpc.example.com").unwrap();
        assert_eq!(ui.to_string(), "https://***@rpc.example.com");
        for u in [&q, &ui] {
            assert!(!u.to_string().contains("SECRET"));
        }
        // A key-free URL is left legible.
        let plain = EndpointUrl::resolve("chain.rpc_url", "https://sepolia-rpc-flashblocks.giwa.io").unwrap();
        assert_eq!(plain.to_string(), "https://sepolia-rpc-flashblocks.giwa.io");
    }

    #[test]
    fn a_url_template_expands_from_the_environment() {
        std::env::set_var("DUBU_TEST_KEY_OK", KEY);
        let u = EndpointUrl::resolve("chain.ws_url", "wss://giwa-sepolia.nodit.io/${DUBU_TEST_KEY_OK}").unwrap();
        assert_eq!(u.expose(), format!("wss://giwa-sepolia.nodit.io/{KEY}"));
        assert_eq!(u.to_string(), "wss://giwa-sepolia.nodit.io/***");
        assert_eq!(u.scheme(), "wss");
        std::env::remove_var("DUBU_TEST_KEY_OK");
    }

    #[test]
    fn an_unset_variable_names_the_variable_and_never_the_value() {
        std::env::remove_var("DUBU_TEST_KEY_MISSING");
        let err = EndpointUrl::resolve("chain.rpc_url", "https://h/${DUBU_TEST_KEY_MISSING}").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("DUBU_TEST_KEY_MISSING"), "must name the variable: {msg}");
        assert!(msg.contains(".env"), "must say where to put it: {msg}");
    }

    #[test]
    fn an_empty_variable_is_refused_rather_than_expanded_to_nothing() {
        // `.env` with a blank `NODIT_API_KEY=` is the likely mistake, and a URL with an empty
        // key segment fails as a 401 at the first request — a much worse place to find out.
        std::env::set_var("DUBU_TEST_KEY_BLANK", "   ");
        let err = EndpointUrl::resolve("chain.rpc_url", "https://h/${DUBU_TEST_KEY_BLANK}").unwrap_err();
        assert!(err.to_string().contains("unset or empty"));
        std::env::remove_var("DUBU_TEST_KEY_BLANK");
    }

    #[test]
    fn the_config_carries_a_template_and_never_a_literal_key() {
        std::env::set_var("DUBU_TEST_NODIT", KEY);
        let s = good()
            .replace("wss://giwa-sepolia.nodit.io/TESTKEY", "wss://giwa-sepolia.nodit.io/${DUBU_TEST_NODIT}")
            .replace("https://giwa-sepolia.nodit.io/TESTKEY", "https://giwa-sepolia.nodit.io/${DUBU_TEST_NODIT}");
        let cfg = parse(&s).unwrap();
        assert!(cfg.chain.ws_url.expose().ends_with(KEY));
        // The whole config Debug-printed — the shape a panic or a `{:?}` dump would produce —
        // must not contain the key anywhere.
        assert!(!format!("{cfg:?}").contains(KEY), "the key survived a Debug dump of the config");
        std::env::remove_var("DUBU_TEST_NODIT");
    }

    #[test]
    fn dotenv_never_overrides_the_real_environment() {
        let dir = std::env::temp_dir().join(format!("dubu-dotenv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".env");
        std::fs::write(
            &path,
            "# a comment\n\nexport DUBU_TEST_DOTENV_A=from_file\nDUBU_TEST_DOTENV_B=\"quoted\"\nDUBU_TEST_DOTENV_C=from_file\nnot_a_pair\n",
        )
        .unwrap();

        // C is already set: an operator or a systemd unit set it deliberately and the file must
        // not silently win.
        std::env::set_var("DUBU_TEST_DOTENV_C", "from_env");
        std::env::remove_var("DUBU_TEST_DOTENV_A");
        std::env::remove_var("DUBU_TEST_DOTENV_B");

        assert_eq!(load_dotenv(&path), 2, "only the two unset variables");
        assert_eq!(std::env::var("DUBU_TEST_DOTENV_A").unwrap(), "from_file");
        assert_eq!(std::env::var("DUBU_TEST_DOTENV_B").unwrap(), "quoted", "one layer of quotes is stripped");
        assert_eq!(std::env::var("DUBU_TEST_DOTENV_C").unwrap(), "from_env");

        // A missing file is not an error: production sets real variables and has no `.env`.
        assert_eq!(load_dotenv(&dir.join("nope.env")), 0);

        for k in ["DUBU_TEST_DOTENV_A", "DUBU_TEST_DOTENV_B", "DUBU_TEST_DOTENV_C"] {
            std::env::remove_var(k);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
