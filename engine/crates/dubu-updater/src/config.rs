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

use crate::fair_value::MadParams;
use crate::feed::VenueId;
use crate::skew::{SkewParams, VolConfig};
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
    /// Exchange market-data feeds, the quorum rule and the MAD filter.
    pub feed: FeedConfig,
    /// Transaction construction and the transmit switch.
    pub tx: TxConfig,
    /// Inventory skew: the volatility estimator and the Avellaneda–Stoikov knobs.
    pub skew: SkewConfig,
    /// The volatility-scaled half-spread. Defaulted, so a config written before it existed still
    /// loads — with the term **on**, because a constant half-spread is the defect it fixes.
    #[serde(default)]
    pub spread: SpreadConfig,
    /// Jump detection and withdrawal. Defaulted and **on**, for the same reason.
    #[serde(default)]
    pub jump: JumpConfig,
    /// Killswitches.
    pub risk: RiskConfig,
    /// The RFQ maker endpoint. **Absent means off**, and off is the default.
    ///
    /// Not defaulted-on the way [`SpreadConfig`] and [`JumpConfig`] are. Those change how the pool
    /// prices; this one hands out signatures that move tokens. A config written before this
    /// existed must not acquire a signing endpoint merely by being loaded.
    #[serde(default)]
    pub rfq: Option<RfqConfig>,
    /// One entry per pair the bot quotes.
    pub pairs: Vec<PairConfig>,
}

/// The RFQ maker: its own key, the contract it signs for, and how it prices.
///
/// The key is deliberately separate from [`TxConfig`]'s. See `maker`'s module docs — a leaked
/// updater key posts a wrong ladder the killswitches will notice, and a leaked RFQ key signs away
/// the maker's balance up to its standing allowance with nothing to notice in time.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RfqConfig {
    /// Environment variable holding the RFQ signing key. Exactly one of this and
    /// [`Self::private_key_file`], and never the same key as `tx`.
    #[serde(default)]
    pub private_key_env: Option<String>,
    /// File holding the RFQ signing key. The alternative to [`Self::private_key_env`].
    #[serde(default)]
    pub private_key_file: Option<PathBuf>,
    /// The deployed `PmmSettle`. Pinned into the EIP-712 domain at startup, so a wrong address
    /// here yields orders nobody can fill — which is why it is required rather than defaulted.
    pub pmm_settle: String,
    /// Where the endpoint listens. Loopback by default.
    #[serde(default)]
    pub serve: crate::serve::ServeConfig,
    /// Half-spread at zero volatility, in hundredths of a bp.
    pub base_half_spread_e2: u32,
    /// Added half-spread per millibp of volatility, in hundredths of a bp.
    #[serde(default)]
    pub sigma_coefficient_e2: u32,
    /// Ceiling on the half-spread.
    pub max_half_spread_e2: u32,
    /// Largest notional a single order may commit, in whole quote tokens. A decimal string
    /// because TOML integers are `i64` and this is a `u128` once scaled.
    pub max_notional_per_order: String,
    /// Decimals of the quote token these markets are denominated in.
    pub quote_decimals: u8,
    /// How long a signed order stays fillable, how long its inventory stays reserved, and the
    /// window whose option value the spread charges for. See `quoting::MakerParams`.
    pub ttl_secs: u64,
    /// Floor on a single fill, in bps of the maker leg.
    #[serde(default)]
    pub min_fill_bps: u16,
}

impl RfqConfig {
    /// Where the signing key lives.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] when neither or both are set, when the variable name is empty, or when it
    /// looks like somebody pasted the key itself where the variable name goes. The same three
    /// checks `tx` makes, for the same reasons — and here the key is the more dangerous of the
    /// two, so the section is required to name a source rather than being allowed to default to
    /// none.
    pub fn key_source(&self) -> Result<KeySource, ConfigError> {
        let source = match (&self.private_key_env, &self.private_key_file) {
            (Some(_), Some(_)) => {
                return Err(invalid("rfq: set exactly one of private_key_env / private_key_file, not both"))
            }
            (None, None) => {
                return Err(invalid(
                    "rfq: no signing key; set private_key_env or private_key_file, or remove the \
                     [rfq] section to run without the RFQ leg",
                ))
            }
            (Some(v), None) => KeySource::Env(v.clone()),
            (None, Some(p)) => KeySource::File(p.clone()),
        };
        if let KeySource::Env(name) = &source {
            if name.trim().is_empty() {
                return Err(invalid("rfq.private_key_env: must name an environment variable"));
            }
            if name.starts_with("0x") || name.len() >= 64 {
                return Err(invalid(
                    "rfq.private_key_env looks like a key rather than a variable name; \
                     this field holds the NAME of an environment variable",
                ));
            }
        }
        Ok(source)
    }

    /// [`Self::max_notional_per_order`] in the quote token's own units.
    ///
    /// # Errors
    /// [`ConfigError::Units`].
    pub fn max_notional_units(&self) -> Result<u128, ConfigError> {
        Ok(units::parse_fixed(&self.max_notional_per_order, self.quote_decimals)?)
    }

    /// The pricing parameters, as `quoting` wants them.
    ///
    /// # Errors
    /// [`ConfigError::Units`] if the notional cap is not a decimal number.
    pub fn params(&self, vol_horizon_secs: u64) -> Result<crate::quoting::MakerParams, ConfigError> {
        Ok(crate::quoting::MakerParams {
            base_half_spread_e2: self.base_half_spread_e2,
            sigma_coefficient_e2: self.sigma_coefficient_e2,
            max_half_spread_e2: self.max_half_spread_e2,
            max_notional_per_order: self.max_notional_units()?,
            ttl_secs: self.ttl_secs,
            // Taken from the skew estimator rather than configured twice. Two numbers meaning
            // "the window sigma is measured over" is two things to keep in step and two ways to
            // misprice the TTL.
            sigma_horizon_secs: vol_horizon_secs,
            min_fill_bps: self.min_fill_bps,
        })
    }
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
        self.skew.validate()?;
        self.spread.validate()?;
        self.jump.validate(&self.chain)?;
        self.risk.validate()?;

        if self.pairs.is_empty() {
            return Err(invalid("pairs: at least one [[pairs]] entry is required"));
        }
        let mut seen_ids = BTreeSet::new();
        let mut seen_symbols = BTreeSet::new();
        for p in &self.pairs {
            p.validate(self.feed.min_venues)?;
            if !seen_ids.insert(p.pair_id) {
                return Err(invalid(format!("pairs: pair_id {} appears twice", p.pair_id)));
            }
            // Two pairs on one symbol is not obviously wrong, but it is never what was meant,
            // and it doubles the quote traffic for one price.
            if !seen_symbols.insert(p.symbol.clone()) {
                return Err(invalid(format!("pairs: symbol `{}` appears twice", p.symbol)));
            }
            // A cap below a pair's own half-spread would silently NARROW the configured spread —
            // `spread::compute` refuses to do that, so the config would quietly mean something
            // other than what it says. Caught here, naming both numbers.
            if self.spread.max_half_spread_bps < p.half_spread_bps {
                return Err(invalid(format!(
                    "spread.max_half_spread_bps ({}) is below pairs[{}].half_spread_bps ({}); \
                     the cap bounds the VOLATILITY TERM and can never narrow the configured spread, \
                     so this combination would silently disable the term for that pair",
                    self.spread.max_half_spread_bps, p.pair_id, p.half_spread_bps
                )));
            }
        }
        Ok(())
    }

    /// The pair entry for an id, if configured.
    #[must_use]
    pub fn pair(&self, pair_id: u16) -> Option<&PairConfig> {
        self.pairs.iter().find(|p| p.pair_id == pair_id)
    }

    /// Every venue at least one pair names, in a stable order.
    ///
    /// A venue is enabled by being *used*, not by a separate switch. Two places to turn a venue
    /// on is two places for them to disagree, and the failure that produces — a venue configured
    /// but quoting no symbol — is one that counts toward nothing and reads as healthy.
    #[must_use]
    pub fn venues(&self) -> Vec<VenueId> {
        VenueId::ALL
            .into_iter()
            .filter(|v| self.pairs.iter().any(|p| p.venues.symbol(*v).is_some()))
            .collect()
    }

    /// The `(venue symbol, canonical symbol)` pairs one venue should subscribe to.
    #[must_use]
    pub fn venue_symbols(&self, venue: VenueId) -> Vec<(String, String)> {
        self.pairs
            .iter()
            .filter_map(|p| p.venues.symbol(venue).map(|s| (s.to_string(), p.symbol.clone())))
            .collect()
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
    /// Extra read endpoints, rotated alongside [`Self::flashblocks_rpc_url`].
    ///
    /// Reads only, and the restriction is structural rather than a convention: a read is a
    /// question about one block that any node can answer, whereas rotating the *transmit* path
    /// would read a nonce from a node that has not seen the previous transaction. See
    /// [`crate::chain::Selection`]. The transmit client stays pinned to [`Self::rpc_url`] however
    /// many of these there are.
    ///
    /// Each is an [`EndpointUrl`], so each is a `${VAR}` template and each is redacted in logs.
    /// Several keys against one provider multiply the request budget; several providers also buy
    /// independence from one of them being down.
    #[serde(default)]
    pub read_rpc_urls: Vec<EndpointUrl>,
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

/// Per-venue endpoint overrides. Every one is optional; the defaults are the public endpoints
/// on [`VenueId::default_ws_url`].
///
/// There is no API key field and no secret field here, for any venue, because none of these
/// streams has one. A venue is enabled by a pair naming a symbol for it under
/// [`PairVenues`], not by appearing in this table.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VenueUrls {
    /// Binance combined-stream endpoint.
    #[serde(default)]
    pub binance: Option<String>,
    /// OKX v5 public endpoint.
    #[serde(default)]
    pub okx: Option<String>,
    /// Bybit v5 spot public endpoint.
    #[serde(default)]
    pub bybit: Option<String>,
    /// Coinbase Exchange feed endpoint.
    #[serde(default)]
    pub coinbase: Option<String>,
}

impl VenueUrls {
    /// The endpoint for one venue: the override if given, otherwise the public default.
    #[must_use]
    pub fn get(&self, venue: VenueId) -> &str {
        let over = match venue {
            VenueId::Binance => &self.binance,
            VenueId::Okx => &self.okx,
            VenueId::Bybit => &self.bybit,
            VenueId::Coinbase => &self.coinbase,
        };
        over.as_deref().unwrap_or_else(|| venue.default_ws_url())
    }
}

/// Exchange market-data feeds, the quorum rule, and the MAD outlier filter.
///
/// **Market data only.** There is no API key field, no secret field, and no order-entry code
/// path anywhere in this crate, on any venue. The design is unhedged precisely because a Korean
/// corporate real-name exchange account is not available, so there is no account to place an
/// order against even if the code existed. See the crate README.
///
/// # Why the quorum knobs live here and what they are worth
///
/// One exchange makes the ladder's own price bounds vacuous: they would be derived from the same
/// number they are checking. Several venues give [`crate::fair_value::combine`] a cross-section,
/// and these four fields are the entire policy for what to do with it — how many venues are
/// enough, how far from the pack is too far, and how far apart the pack itself may be before
/// there is no single price to quote at all.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeedConfig {
    /// Endpoint overrides. Omit the table entirely to use the public defaults.
    #[serde(default)]
    pub urls: VenueUrls,
    /// A symbol with no accepted tick for this long is **stale on that venue**, and a stale venue
    /// contributes nothing to the cross-section. Not a soft signal:
    /// [`crate::feed::FeedSnapshot::live`] returns nothing past it.
    #[serde(default = "d_feed_stale_ms")]
    pub stale_after_ms: u64,
    /// First reconnect delay. Doubles per consecutive failure up to the ceiling.
    #[serde(default = "d_reconnect_initial_ms")]
    pub reconnect_initial_ms: u64,
    /// Reconnect delay ceiling.
    #[serde(default = "d_reconnect_max_ms")]
    pub reconnect_max_ms: u64,
    /// No frame at all — not even a keepalive answer — for this long forces a reconnect. The
    /// venues here either push continuously or answer a ping every 20s, so a silent socket is a
    /// dead socket well before the TCP stack notices.
    #[serde(default = "d_read_timeout_ms")]
    pub read_timeout_ms: u64,
    /// **The quorum.** Venues that must survive the MAD filter for a reference price to exist at
    /// all. Below it the bot quotes nothing and says `no_quorum`. Must be at least 2: a single
    /// venue is the single-source oracle this whole change exists to stop being.
    #[serde(default = "d_min_venues")]
    pub min_venues: u8,
    /// Multiplier on the median absolute deviation. A venue further than `mad_k * MAD` from the
    /// cross-venue median is an outlier and is dropped.
    #[serde(default = "d_mad_k")]
    pub mad_k: f64,
    /// Floor under the rejection threshold, in bps. Without it, venues agreeing to within a tick
    /// drive the MAD to zero and everything but the median gets rejected. Measured on live
    /// ETHUSDT/BTCUSDT the largest ordinary single-venue deviation was 1.6 bps, so 2 is above
    /// ordinary disagreement and below anything that matters.
    #[serde(default = "d_mad_floor_bps")]
    pub mad_floor_bps: f64,
    /// **The regime gate.** Cross-venue MAD above this, in bps, means the venues do not agree —
    /// not that one of them is wrong. The bot refuses to quote rather than averaging through a
    /// split market. Must exceed `mad_floor_bps`.
    #[serde(default = "d_max_dispersion_bps")]
    pub max_dispersion_bps: f64,
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
fn d_min_venues() -> u8 {
    2
}
fn d_mad_k() -> f64 {
    4.0
}
fn d_mad_floor_bps() -> f64 {
    2.0
}
fn d_max_dispersion_bps() -> f64 {
    25.0
}

/// Round a bps value to deci-bps, the resolution the cross-section is measured at.
fn decibps(v: f64) -> u32 {
    (v * 10.0).round().max(0.0) as u32
}

impl FeedConfig {
    /// The MAD filter's knobs, converted out of the config's decimal-bps form.
    #[must_use]
    pub fn mad_params(&self) -> MadParams {
        MadParams {
            min_venues: self.min_venues,
            k_tenths: (self.mad_k * 10.0).round().max(0.0) as u32,
            floor_decibps: decibps(self.mad_floor_bps),
            max_dispersion_decibps: decibps(self.max_dispersion_bps),
        }
    }

    fn validate(&self) -> Result<(), ConfigError> {
        for venue in VenueId::ALL {
            let url = self.urls.get(venue);
            if !(url.starts_with("wss://") || url.starts_with("ws://")) {
                return Err(invalid(format!(
                    "feed.urls.{venue}: must be a ws(s) URL, got `{url}`"
                )));
            }
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
        // One venue is a single-source oracle: the ladder and the bounds that are supposed to
        // check it would both come from the same number. That is the shape this change exists to
        // remove, and allowing it back in through a config value would remove it right back.
        if self.min_venues < 2 {
            return Err(invalid(format!(
                "feed.min_venues is {}, and must be at least 2; quoting off a single venue makes \
                 the ladder's own price bounds a check against the number that produced them",
                self.min_venues
            )));
        }
        if usize::from(self.min_venues) > VenueId::ALL.len() {
            return Err(invalid(format!(
                "feed.min_venues ({}) exceeds the {} venues this crate can speak to",
                self.min_venues,
                VenueId::ALL.len()
            )));
        }
        if !self.mad_k.is_finite() || self.mad_k <= 0.0 || self.mad_k > 100.0 {
            return Err(invalid("feed.mad_k: must be a finite value in (0, 100]"));
        }
        if !self.mad_floor_bps.is_finite() || self.mad_floor_bps <= 0.0 {
            return Err(invalid(
                "feed.mad_floor_bps: must be finite and non-zero; a zero floor rejects every \
                 venue but the median as soon as the venues agree closely",
            ));
        }
        if !self.max_dispersion_bps.is_finite() || self.max_dispersion_bps <= 0.0 {
            return Err(invalid("feed.max_dispersion_bps: must be finite and non-zero"));
        }
        // Otherwise the regime gate fires before the outlier filter is ever consulted, and the
        // bot stops quoting on the ordinary disagreement the floor was chosen to tolerate.
        if self.max_dispersion_bps <= self.mad_floor_bps {
            return Err(invalid(format!(
                "feed.max_dispersion_bps ({}) must exceed feed.mad_floor_bps ({}); \
                 a dispersion limit at or below the rejection floor gates before the filter runs",
                self.max_dispersion_bps, self.mad_floor_bps
            )));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Inventory skew
// ---------------------------------------------------------------------------

/// The volatility estimator and the Avellaneda–Stoikov knobs.
///
/// Global rather than per-pair, with one exception: `target_base_share_pct` is a per-pair
/// allocation decision and lives on [`PairConfig`]. Risk aversion and the horizon it is measured
/// over are properties of the desk, not of the instrument, and two pairs disagreeing about how
/// far ahead to look would be two different strategies sharing one killswitch.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkewConfig {
    /// EWMA time constant for the return variance, in milliseconds. The half-life is
    /// `tau * ln2`, about 0.69 of this.
    #[serde(default = "d_vol_tau_ms")]
    pub vol_tau_ms: u64,
    /// The horizon `sigma` is scaled to, in seconds. See [`crate::skew::Volatility`] for why the
    /// default is the same window as `risk.bleed_window_secs`.
    #[serde(default = "d_vol_horizon_secs")]
    pub vol_horizon_secs: u64,
    /// Samples closer together than this are skipped rather than divided by a tiny interval.
    #[serde(default = "d_vol_min_sample_ms")]
    pub vol_min_sample_ms: u64,
    /// A gap longer than this is an outage, not a return. The estimator re-anchors.
    #[serde(default = "d_vol_max_sample_ms")]
    pub vol_max_sample_ms: u64,
    /// Risk aversion `gamma`. `skew_bps = gamma * q * sigma_bps^2 / 10_000`, so with ETHUSDT's
    /// measured 300s `sigma` of about 10 bps, `gamma = 1000` and a 20% imbalance give 2 bp.
    #[serde(default = "d_gamma")]
    pub gamma: f64,
    /// Cap on a **positive** skew (book down, the pool is long and selling), in bps.
    #[serde(default = "d_skew_max_positive_bps")]
    pub max_positive_bps: u16,
    /// Cap on a **negative** skew (book up, the pool is short and buying), as a magnitude in bps.
    /// Deliberately the tighter of the two; [`crate::skew::compute`] argues why at length.
    #[serde(default = "d_skew_max_negative_bps")]
    pub max_negative_bps: u16,
}

fn d_vol_tau_ms() -> u64 {
    60_000
}
fn d_vol_horizon_secs() -> u64 {
    300
}
fn d_vol_min_sample_ms() -> u64 {
    100
}
fn d_vol_max_sample_ms() -> u64 {
    10_000
}
fn d_gamma() -> f64 {
    1_000.0
}
fn d_skew_max_positive_bps() -> u16 {
    30
}
fn d_skew_max_negative_bps() -> u16 {
    10
}

impl SkewConfig {
    /// The volatility estimator's configuration.
    #[must_use]
    pub const fn vol_config(&self) -> VolConfig {
        VolConfig {
            tau_ms: self.vol_tau_ms,
            horizon_secs: self.vol_horizon_secs,
            min_sample_ms: self.vol_min_sample_ms,
            max_sample_ms: self.vol_max_sample_ms,
        }
    }

    /// The skew's own knobs.
    #[must_use]
    pub fn params(&self) -> SkewParams {
        SkewParams {
            gamma_e2: (self.gamma * 100.0).round().max(0.0) as u64,
            max_positive_bps: self.max_positive_bps,
            max_negative_bps: self.max_negative_bps,
        }
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if !(1_000..=3_600_000).contains(&self.vol_tau_ms) {
            return Err(invalid("skew.vol_tau_ms: must be 1000..=3600000"));
        }
        if !(1..=86_400).contains(&self.vol_horizon_secs) {
            return Err(invalid("skew.vol_horizon_secs: must be 1..=86400"));
        }
        if self.vol_min_sample_ms == 0 || self.vol_max_sample_ms <= self.vol_min_sample_ms {
            return Err(invalid(
                "skew.vol_max_sample_ms must exceed skew.vol_min_sample_ms, which must be non-zero",
            ));
        }
        if !self.gamma.is_finite() || self.gamma <= 0.0 || self.gamma > 100_000.0 {
            return Err(invalid("skew.gamma: must be a finite value in (0, 100000]"));
        }
        let max_bps = dubu_core::ladder::MAX_BPS;
        for (name, v) in [("max_positive_bps", self.max_positive_bps), ("max_negative_bps", self.max_negative_bps)] {
            if u128::from(v) > max_bps {
                return Err(invalid(format!("skew.{name}: must be <= {max_bps}")));
            }
        }
        // The asymmetry runs one way for a reason. A negative skew lifts the pool's BID toward
        // and past fair value, which is a free option written to whoever notices; a positive one
        // lowers both sides, which is defensive. Capping the book-lifting direction more loosely
        // than the book-lowering one inverts that and is never what was meant.
        if self.max_negative_bps > self.max_positive_bps {
            return Err(invalid(format!(
                "skew.max_negative_bps ({}) exceeds skew.max_positive_bps ({}); \
                 the book-LIFTING direction raises the pool's bid toward fair value and is the \
                 pick-off direction, so it must be the tighter cap, not the looser one",
                self.max_negative_bps, self.max_positive_bps
            )));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The volatility-scaled half-spread
// ---------------------------------------------------------------------------

/// `half_spread = min(s0 + s1 * sigma, cap) + degraded_extra`.
///
/// Global rather than per-pair, and that is the point: `s1` is dimensionless and multiplies each
/// pair's own `sigma`, so one value is already correct across ETHUSDT at 10 bp and BTCUSDT at 3 bp.
/// See [`crate::spread`] for the derivation of both numbers.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpreadConfig {
    /// `s1`, in bps of half-spread per bp of `sigma` at the estimator's horizon.
    ///
    /// Derived, not picked: the sweep's smallest measured-positive half-spread against a 100 bp
    /// jump is 30 bp, and it should be reached when a 100 bp jump is a live possibility, which at
    /// ETHUSDT's measured `sigma(300s)` of 10 bp means 5x that level. `5 + s1 * 50 = 30` gives
    /// `s1 = 0.5`.
    #[serde(default = "d_vol_coefficient")]
    pub vol_coefficient: f64,
    /// The ceiling on `s0 + s1 * sigma`. Above it, widening has stopped being a defence — the
    /// sweep's 60 bp row earns *less* than its 30 bp row — and [`crate::jump`] is what takes over.
    ///
    /// 30 bp is exactly UniV2's fee, which makes the rule statable: the pool never quotes worse
    /// than the constant-fee AMM it is trying to beat, it simply does not quote 30 bp in a calm
    /// market. The degraded-chain widening is added *after* this cap, deliberately.
    #[serde(default = "d_max_half_spread_bps")]
    pub max_half_spread_bps: u16,
}

fn d_vol_coefficient() -> f64 {
    0.5
}
fn d_max_half_spread_bps() -> u16 {
    30
}

impl Default for SpreadConfig {
    fn default() -> Self {
        Self {
            vol_coefficient: d_vol_coefficient(),
            max_half_spread_bps: d_max_half_spread_bps(),
        }
    }
}

impl SpreadConfig {
    /// The knobs [`crate::spread::compute`] takes.
    #[must_use]
    pub fn params(&self) -> crate::spread::SpreadParams {
        crate::spread::SpreadParams {
            vol_coefficient_e2: (self.vol_coefficient * 100.0).round().max(0.0) as u32,
            max_half_spread_bps: self.max_half_spread_bps,
        }
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if !self.vol_coefficient.is_finite() || !(0.0..=100.0).contains(&self.vol_coefficient) {
            return Err(invalid(
                "spread.vol_coefficient: must be a finite value in [0, 100]; 0 disables the \
                 volatility term and restores the constant half-spread",
            ));
        }
        if self.max_half_spread_bps == 0 {
            return Err(invalid("spread.max_half_spread_bps: must be non-zero"));
        }
        if u128::from(self.max_half_spread_bps) > dubu_core::ladder::MAX_BPS {
            return Err(invalid("spread.max_half_spread_bps: must be <= 9999"));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Jump detection
// ---------------------------------------------------------------------------

/// Jump detection and withdrawal. See [`crate::jump`] for why every one of these is what it is.
///
/// There is no basis-point threshold in this table, and that is deliberate. Both bounds on the
/// trip threshold come from the pair's own `half_spread_bps` and `width_bps` — the floor is the
/// point at which the posted quote stops being on the right side of fair value, and the ceiling is
/// `half_spread + width/2`, the point past which the pool pays the excess whatever the volatility
/// estimate says. A tunable bp threshold would be one more number picked by feel, and it would be
/// wrong on one of the two pairs whichever value it took.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JumpConfig {
    /// Off restores the previous behaviour exactly: quote through everything.
    #[serde(default = "d_true")]
    pub enabled: bool,
    /// The sigma multiplier `k`. It only ever acts *between* the two derived bounds, so raising it
    /// cannot make the detector numb past the absorption limit and lowering it cannot make it fire
    /// inside the half-spread.
    #[serde(default = "d_jump_sigma_k")]
    pub sigma_k: f64,
    /// Minimum withdrawal, in seconds, measured from the **most recent** trip rather than the
    /// first. The cool-off also does not end until the reference has settled; this is its floor.
    #[serde(default = "d_jump_cooloff_secs")]
    pub cooloff_secs: u64,
    /// `"book"` withdraws every pair when one trips; `"pair"` withdraws only the one that did.
    #[serde(default = "d_jump_scope")]
    pub scope: crate::jump::Scope,
    /// How often the fast lane samples the reference, in milliseconds.
    ///
    /// **This is the reaction time, and it is the one place in this crate where latency genuinely
    /// matters.** The quote loop wakes on `newHeads` at 1 Hz, which would put mean detection
    /// latency at 500 ms; the fast lane runs inside the same task between wakes, reads only the
    /// in-memory feed snapshots, and costs zero RPC unless something trips.
    #[serde(default = "d_jump_scan_interval_ms")]
    pub scan_interval_ms: u64,
    /// `maxPriorityFeePerGas` for a jump withdrawal only, in gwei, as a decimal string.
    ///
    /// GIWA's sequencer has no public mempool and orders by **highest fee first**, and `tx.rs`
    /// deliberately pays a flat near-zero tip on ordinary quote traffic. That is the right call for
    /// a quote and the wrong one for a withdrawal: the withdrawal is racing a searcher who is
    /// willing to outbid a quoting bot, and being 200 ms earlier does not win a fee auction —
    /// paying more does. At 0.001 gwei base and ~29k gas, 0.5 gwei costs about five cents.
    #[serde(default = "d_withdraw_priority_fee_gwei")]
    pub withdraw_priority_fee_per_gas_gwei: String,
    /// `maxFeePerGas` for a jump withdrawal only, in gwei. Must be at least the tip above.
    #[serde(default = "d_withdraw_max_fee_gwei")]
    pub withdraw_max_fee_per_gas_gwei: String,
}

fn d_true() -> bool {
    true
}
fn d_jump_sigma_k() -> f64 {
    6.0
}
fn d_jump_cooloff_secs() -> u64 {
    30
}
const fn d_jump_scope() -> crate::jump::Scope {
    crate::jump::Scope::Book
}
fn d_jump_scan_interval_ms() -> u64 {
    200
}
fn d_withdraw_priority_fee_gwei() -> String {
    "0.5".into()
}
fn d_withdraw_max_fee_gwei() -> String {
    "2.0".into()
}

impl Default for JumpConfig {
    fn default() -> Self {
        Self {
            enabled: d_true(),
            sigma_k: d_jump_sigma_k(),
            cooloff_secs: d_jump_cooloff_secs(),
            scope: d_jump_scope(),
            scan_interval_ms: d_jump_scan_interval_ms(),
            withdraw_priority_fee_per_gas_gwei: d_withdraw_priority_fee_gwei(),
            withdraw_max_fee_per_gas_gwei: d_withdraw_max_fee_gwei(),
        }
    }
}

impl JumpConfig {
    /// The knobs [`crate::jump::Detector`] takes. `skew` supplies the sampling window, so the jump
    /// detector and the volatility estimator agree on what a hole in the reference is.
    #[must_use]
    pub fn params(&self, skew: &SkewConfig) -> crate::jump::Params {
        crate::jump::Params {
            sigma_k_e2: (self.sigma_k * 100.0).round().max(0.0) as u32,
            cooloff: std::time::Duration::from_secs(self.cooloff_secs),
            min_sample: std::time::Duration::from_millis(skew.vol_min_sample_ms),
            max_sample: std::time::Duration::from_millis(skew.vol_max_sample_ms),
        }
    }

    /// The fast-lane interval.
    #[must_use]
    pub const fn scan_interval(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.scan_interval_ms)
    }

    /// Withdrawal `maxFeePerGas` in wei.
    ///
    /// # Errors
    /// [`ConfigError::Units`] if the string is not a decimal.
    pub fn withdraw_max_fee_wei(&self) -> Result<u128, ConfigError> {
        Ok(units::parse_fixed(&self.withdraw_max_fee_per_gas_gwei, 9)?)
    }

    /// Withdrawal `maxPriorityFeePerGas` in wei.
    ///
    /// # Errors
    /// [`ConfigError::Units`] if the string is not a decimal.
    pub fn withdraw_priority_fee_wei(&self) -> Result<u128, ConfigError> {
        Ok(units::parse_fixed(&self.withdraw_priority_fee_per_gas_gwei, 9)?)
    }

    fn validate(&self, chain: &ChainConfig) -> Result<(), ConfigError> {
        if !self.sigma_k.is_finite() || !(0.0..=1_000.0).contains(&self.sigma_k) {
            return Err(invalid("jump.sigma_k: must be a finite value in [0, 1000]"));
        }
        // A cool-off shorter than a block is not a withdrawal, it is a flicker: the transaction
        // that posts the zero epoch would not have confirmed before the resume was already due.
        let block_secs = chain.block_time_ms.div_ceil(1_000).max(1);
        if self.cooloff_secs < block_secs {
            return Err(invalid(format!(
                "jump.cooloff_secs ({}) is below one block time ({block_secs}s); \
                 the withdrawal would not have confirmed before the resume was due",
                self.cooloff_secs
            )));
        }
        if self.cooloff_secs > 3_600 {
            return Err(invalid("jump.cooloff_secs: must be <= 3600"));
        }
        if !(20..=10_000).contains(&self.scan_interval_ms) {
            return Err(invalid("jump.scan_interval_ms: must be 20..=10000"));
        }
        // The fast lane exists to beat the head cadence. Set above it, it is strictly worse than
        // doing the check in the cycle and reads as a reaction time it does not deliver.
        if self.scan_interval_ms > chain.block_time_ms {
            return Err(invalid(format!(
                "jump.scan_interval_ms ({}) exceeds chain.block_time_ms ({}); \
                 the quote loop already wakes on every head, so a slower fast lane detects \
                 nothing sooner and only looks like it does",
                self.scan_interval_ms, chain.block_time_ms
            )));
        }
        let max = self.withdraw_max_fee_wei()?;
        let tip = self.withdraw_priority_fee_wei()?;
        if max == 0 {
            return Err(invalid("jump.withdraw_max_fee_per_gas_gwei: must be non-zero"));
        }
        if tip > max {
            return Err(invalid(format!(
                "jump.withdraw_priority_fee_per_gas_gwei ({tip} wei) exceeds \
                 jump.withdraw_max_fee_per_gas_gwei ({max} wei)"
            )));
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

/// Which symbol each venue quotes for one pair.
///
/// Every venue spells the same market differently — `ETHUSDT`, `ETH-USDT`, `ETH-USD` — so the
/// mapping is configuration rather than a transformation guessed in code. Naming a venue here is
/// also what **enables** it: a venue no pair names is never connected to.
///
/// The field names are the venue labels and unknown ones are a hard error, so `bybbit = "..."`
/// fails at startup instead of quietly leaving the bot one venue short of the quorum it thinks it
/// has.
///
/// The choice of product is load-bearing and not interchangeable. `ETH-USD` is **not** a second
/// observation of `ETHUSDT`: measured on 2026-07-27 the USDT/USD basis put Coinbase's USD books
/// a persistent 8-9 bps above the three USDT venues, which against a 5 bp half-spread is a bias,
/// not redundancy. See [`crate::feed::coinbase`].
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairVenues {
    /// Binance symbol, e.g. `ETHUSDT`.
    #[serde(default)]
    pub binance: Option<String>,
    /// OKX instrument id, e.g. `ETH-USDT`.
    #[serde(default)]
    pub okx: Option<String>,
    /// Bybit spot symbol, e.g. `ETHUSDT`.
    #[serde(default)]
    pub bybit: Option<String>,
    /// Coinbase product id, e.g. `ETH-USDT`.
    #[serde(default)]
    pub coinbase: Option<String>,
}

impl PairVenues {
    /// This pair's symbol on one venue, if it is quoted there.
    #[must_use]
    pub fn symbol(&self, venue: VenueId) -> Option<&str> {
        match venue {
            VenueId::Binance => self.binance.as_deref(),
            VenueId::Okx => self.okx.as_deref(),
            VenueId::Bybit => self.bybit.as_deref(),
            VenueId::Coinbase => self.coinbase.as_deref(),
        }
    }

    /// Every venue this pair is quoted on, in a stable order.
    pub fn iter(&self) -> impl Iterator<Item = (VenueId, &str)> {
        VenueId::ALL.into_iter().filter_map(|v| self.symbol(v).map(|s| (v, s)))
    }

    /// How many venues quote this pair.
    #[must_use]
    pub fn count(&self) -> usize {
        self.iter().count()
    }
}

/// One quoted pair.
///
/// # The symbol is not the token
///
/// `symbol` is the **canonical** name for the **real** asset the pool's mock token tracks, not a
/// market in the mock token. `mWETH` has no market anywhere; it is a mock ERC-20 we deployed, and
/// nothing trades it. The bot prices pairId 1 off `ETHUSDT` because that is the asset the mock
/// stands in for, which is what makes the demo live and what makes a later markout study mean
/// anything. A reader who takes `symbol = "ETHUSDT"` to mean "mWETH trades at 1943" has misread
/// it.
///
/// `symbol` is also the key every venue's ticks are recorded under, which is why each venue's own
/// spelling is a separate field in [`PairVenues`]: without the translation each venue would sit
/// in its own namespace and the cross-section would be permanently empty.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairConfig {
    /// `PropPool` pair id. 1-based; id 0 is reserved and never a real pair.
    pub pair_id: u16,
    /// Canonical symbol of the **real** asset this pair's mock base token tracks. See the struct
    /// docs — this is not a market in the mock token.
    pub symbol: String,
    /// Which symbol each venue quotes this pair under. At least `feed.min_venues` of them.
    pub venues: PairVenues,
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
    /// **Target inventory, as a share of this pair's book, in percent.**
    ///
    /// Configuration rather than a constant, and a *share* rather than an amount so that it stays
    /// meaningful as the pool grows or shrinks. The book is this pair's base holdings valued at
    /// the reference plus its share of the quote balance; see [`crate::skew::Inventory`] for what
    /// "its share" means when two pairs draw bids from the same quote token.
    ///
    /// `50` is a balanced book. Lower means the pool would rather hold quote than base, which is
    /// the sensible default for an asset it cannot hedge.
    pub target_base_share_pct: f64,
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
    /// small. See [`crate::policy`]. Fractional bps are supported to one decimal place (e.g.
    /// `0.5`); [`Self::adverse_drift_decibps`] is what `policy` actually compares against.
    pub adverse_drift_bps: f64,
    /// Drift threshold, in bps, when the posted quote has merely become conservative. Costs
    /// volume, not money; keep it larger than `adverse_drift_bps` so a quiet market does not
    /// churn gas. Same one-decimal-place precision as `adverse_drift_bps`.
    pub favourable_drift_bps: f64,
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

    /// [`Self::target_base_share_pct`] in parts per million of the pair's book.
    #[must_use]
    pub fn target_base_share_ppm(&self) -> u32 {
        (self.target_base_share_pct * 10_000.0).round().clamp(0.0, 1_000_000.0) as u32
    }

    /// [`Self::adverse_drift_bps`] in deci-bps (tenths of a bps), which is the precision
    /// [`crate::policy`] actually measures drift at.
    #[must_use]
    pub fn adverse_drift_decibps(&self) -> u32 {
        (self.adverse_drift_bps * 10.0).round() as u32
    }

    /// [`Self::favourable_drift_bps`] in deci-bps. See [`Self::adverse_drift_decibps`].
    #[must_use]
    pub fn favourable_drift_decibps(&self) -> u32 {
        (self.favourable_drift_bps * 10.0).round() as u32
    }

    fn validate(&self, min_venues: u8) -> Result<(), ConfigError> {
        let id = self.pair_id;
        if id == 0 {
            return Err(invalid("pairs.pair_id: 0 is reserved and is never a real pair"));
        }
        if self.symbol.is_empty() || !self.symbol.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return Err(invalid(format!("pairs[{id}].symbol: must be alphanumeric, got `{}`", self.symbol)));
        }
        // A pair that names fewer venues than the quorum needs can never quote. Refusing here
        // rather than discovering it as a permanent `no_quorum` at run time is the difference
        // between a startup error and a bot that looks healthy and quotes nothing.
        let venues = self.venues.count();
        if venues < usize::from(min_venues) {
            return Err(invalid(format!(
                "pairs[{id}].venues names {venues} venue(s) but feed.min_venues is {min_venues}; \
                 this pair could never reach quorum and would never quote"
            )));
        }
        for (venue, symbol) in self.venues.iter() {
            if symbol.trim().is_empty() {
                return Err(invalid(format!("pairs[{id}].venues.{venue}: must not be empty")));
            }
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
        if !self.target_base_share_pct.is_finite() || !(0.0..=100.0).contains(&self.target_base_share_pct) {
            return Err(invalid(format!(
                "pairs[{id}].target_base_share_pct: must be a finite value in [0, 100], got {}",
                self.target_base_share_pct
            )));
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
        if !self.adverse_drift_bps.is_finite() || !self.favourable_drift_bps.is_finite() {
            return Err(invalid(format!("pairs[{id}]: drift thresholds must be finite")));
        }
        if self.adverse_drift_bps <= 0.0 || self.favourable_drift_bps <= 0.0 {
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
min_venues = 2

[tx]

[skew]

[risk]
state_path = "state/killswitch.json"
bleed_window_secs = 300
bleed_limit = "2000"
loss_budget = "10000"

[[pairs]]
pair_id = 1
symbol = "ETHUSDT"
venues = { binance = "ETHUSDT", okx = "ETH-USDT", bybit = "ETHUSDT" }
base_decimals = 18
quote_decimals = 6
half_spread_bps = 5
width_bps = 25
target_base_share_pct = 50
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
        assert_eq!(cfg.pairs[0].target_base_share_ppm(), 500_000);
    }

    // -----------------------------------------------------------------------
    // The volatility-scaled spread and the jump withdrawal
    // -----------------------------------------------------------------------

    #[test]
    fn both_defences_default_to_on_in_a_config_written_before_they_existed() {
        // `good()` has no `[spread]` and no `[jump]` table. A constant half-spread and quoting
        // through a jump are the defects these fix, so absence must not mean off.
        let cfg = parse(good()).unwrap();
        assert!((cfg.spread.vol_coefficient - 0.5).abs() < 1e-9);
        assert_eq!(cfg.spread.max_half_spread_bps, 30);
        assert_eq!(cfg.spread.params().vol_coefficient_e2, 50);
        assert!(cfg.jump.enabled);
        assert!((cfg.jump.sigma_k - 6.0).abs() < 1e-9);
        assert_eq!(cfg.jump.cooloff_secs, 30);
        assert_eq!(cfg.jump.scope, crate::jump::Scope::Book);
        assert_eq!(cfg.jump.scan_interval_ms, 200);
        assert_eq!(cfg.jump.params(&cfg.skew).sigma_k_e2, 600);
        // The withdrawal fee is 100x the ordinary tip, which is the point of it existing.
        assert_eq!(cfg.jump.withdraw_priority_fee_wei().unwrap(), 500_000_000);
        assert!(cfg.jump.withdraw_priority_fee_wei().unwrap() > cfg.tx.max_priority_fee_wei().unwrap());
    }

    #[test]
    fn the_scope_is_a_word_and_a_misspelling_is_a_hard_error() {
        let s = format!("{}\n[jump]\nscope = \"book\"\n", good());
        assert_eq!(parse(&s).unwrap().jump.scope, crate::jump::Scope::Book);
        let s = format!("{}\n[jump]\nscope = \"pair\"\n", good());
        assert_eq!(parse(&s).unwrap().jump.scope, crate::jump::Scope::Pair);
        let s = format!("{}\n[jump]\nscope = \"everything\"\n", good());
        assert!(matches!(parse(&s), Err(ConfigError::Parse(_))));
    }

    #[test]
    fn a_cap_below_a_pairs_own_half_spread_is_refused_rather_than_silently_narrowing_it() {
        // `spread::compute` floors at `s0` and would never narrow the configured spread, so this
        // config does not do damage — it does something other than what it says, which is what
        // this file exists to catch.
        let s = format!("{}\n[spread]\nmax_half_spread_bps = 3\n", good());
        assert!(matches!(parse(&s), Err(ConfigError::Invalid(m)) if m.contains("can never narrow")));
    }

    #[test]
    fn a_cooloff_shorter_than_a_block_is_refused() {
        // The withdrawal transaction would not have confirmed before the resume was already due,
        // which is a flicker rather than a withdrawal.
        let s = format!("{}\n[jump]\ncooloff_secs = 0\n", good());
        assert!(matches!(parse(&s), Err(ConfigError::Invalid(m)) if m.contains("below one block time")));
    }

    #[test]
    fn a_fast_lane_slower_than_the_block_time_is_refused() {
        // The fast lane exists to beat the head cadence. Set above it, it detects nothing sooner
        // than the ordinary cycle would and only looks like it does.
        let s = format!("{}\n[jump]\nscan_interval_ms = 2000\n", good());
        assert!(matches!(parse(&s), Err(ConfigError::Invalid(m)) if m.contains("exceeds chain.block_time_ms")));
    }

    #[test]
    fn the_volatility_term_can_be_switched_off_without_removing_it() {
        // `0` restores the constant half-spread exactly, which is what a bisection against the
        // simulator needs in order to attribute a change to this feature and not to the other one.
        let s = format!("{}\n[spread]\nvol_coefficient = 0.0\n", good());
        let cfg = parse(&s).unwrap();
        assert_eq!(cfg.spread.params().vol_coefficient_e2, 0);
        let sp = crate::spread::compute(5, 10_000, 0, &cfg.spread.params());
        assert_eq!(sp.half_spread_bps, 5);
    }

    #[test]
    fn the_jump_detector_shares_the_volatility_estimators_sampling_window() {
        // What counts as a hole in the reference has to be one number, not two: the estimator
        // re-anchors on it and the detector trips on it, and they must agree about where it is.
        let cfg = parse(good()).unwrap();
        let p = cfg.jump.params(&cfg.skew);
        assert_eq!(p.max_sample.as_millis() as u64, cfg.skew.vol_max_sample_ms);
        assert_eq!(p.min_sample.as_millis() as u64, cfg.skew.vol_min_sample_ms);
    }

    // -----------------------------------------------------------------------
    // Venues and the quorum
    // -----------------------------------------------------------------------

    #[test]
    fn a_venue_is_enabled_by_a_pair_naming_it_and_by_nothing_else() {
        let cfg = parse(good()).unwrap();
        assert_eq!(cfg.venues(), vec![VenueId::Binance, VenueId::Okx, VenueId::Bybit]);
        assert!(!cfg.venues().contains(&VenueId::Coinbase), "an unnamed venue must not be connected to");
        assert_eq!(cfg.venue_symbols(VenueId::Okx), vec![("ETH-USDT".to_string(), "ETHUSDT".to_string())]);
        assert!(cfg.venue_symbols(VenueId::Coinbase).is_empty());
    }

    #[test]
    fn each_venue_falls_back_to_its_public_endpoint() {
        let cfg = parse(good()).unwrap();
        assert_eq!(cfg.feed.urls.get(VenueId::Bybit), "wss://stream.bybit.com/v5/public/spot");

        // The override table has to come after the scalar keys or TOML reads them as its own.
        let s = format!("{}\n[feed.urls]\nbybit = \"wss://example.test/spot\"\n", good());
        let cfg = parse(&s).unwrap();
        assert_eq!(cfg.feed.urls.get(VenueId::Bybit), "wss://example.test/spot");
        assert_eq!(cfg.feed.urls.get(VenueId::Okx), "wss://ws.okx.com:8443/ws/v5/public", "one override must not disturb the rest");
    }

    #[test]
    fn a_misspelled_venue_is_a_hard_error_and_not_one_venue_fewer() {
        // The failure this prevents is the whole reason `deny_unknown_fields` is on this struct:
        // `bybbit` would leave the pair one venue short of the quorum it is written to have, and
        // nothing at run time would say so.
        let s = good().replace(r#"bybit = "ETHUSDT""#, r#"bybbit = "ETHUSDT""#);
        assert!(matches!(parse(&s), Err(ConfigError::Parse(_))));
    }

    #[test]
    fn a_pair_that_could_never_reach_quorum_is_refused_at_startup() {
        let s = good()
            .replace(r#"venues = { binance = "ETHUSDT", okx = "ETH-USDT", bybit = "ETHUSDT" }"#,
                     r#"venues = { binance = "ETHUSDT" }"#);
        assert!(matches!(parse(&s), Err(ConfigError::Invalid(m)) if m.contains("could never reach quorum")));
    }

    #[test]
    fn a_single_venue_quorum_is_refused() {
        // One venue is the single-source oracle this whole design exists to stop being, and a
        // config value must not be able to put it back.
        let s = good().replace("min_venues = 2", "min_venues = 1");
        assert!(matches!(parse(&s), Err(ConfigError::Invalid(m)) if m.contains("at least 2")));
    }

    #[test]
    fn a_dispersion_limit_below_the_rejection_floor_is_refused() {
        // Otherwise the regime gate fires on the ordinary disagreement the floor exists to
        // tolerate, and the bot stops quoting for a reason that is purely arithmetic.
        let s = good().replace("min_venues = 2", "min_venues = 2\nmad_floor_bps = 30.0\nmax_dispersion_bps = 25.0");
        assert!(matches!(parse(&s), Err(ConfigError::Invalid(m)) if m.contains("gates before the filter runs")));
    }

    #[test]
    fn the_mad_knobs_reach_the_filter_at_deci_bps_resolution() {
        let s = good().replace("min_venues = 2", "min_venues = 3\nmad_k = 4.5\nmad_floor_bps = 2.5\nmax_dispersion_bps = 25.0");
        let p = parse(&s).unwrap().feed.mad_params();
        assert_eq!(p.min_venues, 3);
        assert_eq!(p.k_tenths, 45);
        assert_eq!(p.floor_decibps, 25);
        assert_eq!(p.max_dispersion_decibps, 250);
    }

    // -----------------------------------------------------------------------
    // Skew
    // -----------------------------------------------------------------------

    #[test]
    fn the_skew_section_defaults_to_the_documented_numbers() {
        let cfg = parse(good()).unwrap();
        let p = cfg.skew.params();
        assert_eq!(p.gamma_e2, 100_000, "gamma = 1000");
        assert_eq!(p.max_positive_bps, 30);
        assert_eq!(p.max_negative_bps, 10);
        let v = cfg.skew.vol_config();
        assert_eq!(v.tau_ms, 60_000);
        assert_eq!(v.horizon_secs, 300, "the same window as risk.bleed_window_secs");
    }

    #[test]
    fn a_looser_cap_on_the_book_lifting_direction_is_refused() {
        // Lifting the book raises the pool's BID toward fair value, which is the pick-off
        // direction. Capping it more loosely than the book-lowering direction inverts the whole
        // argument in `skew::compute`.
        let s = good().replace("[skew]\n", "[skew]\nmax_positive_bps = 10\nmax_negative_bps = 30\n");
        assert!(matches!(parse(&s), Err(ConfigError::Invalid(m)) if m.contains("must be the tighter cap")));
    }

    #[test]
    fn a_target_inventory_outside_the_book_is_refused() {
        for bad in ["-5", "150"] {
            let s = good().replace("target_base_share_pct = 50", &format!("target_base_share_pct = {bad}"));
            assert!(matches!(parse(&s), Err(ConfigError::Invalid(m)) if m.contains("target_base_share_pct")));
        }
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
