//! Configuration: parsed from TOML, unknown fields rejected, ranges checked at load.
//!
//! A bad config has to fail at startup rather than at 3am. `deny_unknown_fields` turns a typo into
//! a parse error instead of a knob that silently kept its default; [`Config::validate`] runs every
//! range check that does not need the chain, and [`crate::chain::verify_against_chain`] runs the
//! ones that do before the loop computes anything.
//!
//! Every on-chain amount here is a decimal **string in human units**, scaled by the pair's decimals
//! at load: TOML integers are `i64` and a thousand mWETH is `10^21`.
//!
//! No secret is a literal here. [`KeySource`] names an environment variable or a path, and the
//! endpoint URLs are [`EndpointUrl`] — Nodit puts the API key in the path, so the URL *is* the
//! credential — which expands `${VAR}` at load and redacts in both `Display` and `Debug`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use alloy_primitives::Address;
use serde::Deserialize;

use crate::fair_value::MadParams;
use crate::feed::{Transport, VenueId};
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

// --- Endpoint URLs, which are credentials ---

/// A URL that may carry a credential, and therefore may never be printed.
///
/// `Display` and `Debug` both emit the redacted form, so no spelling of a `tracing` argument logs
/// the key, and [`EndpointUrl::expose`] is the only way to the real string. Redaction keeps scheme
/// and host: over-redacting a key-free path costs nothing, under-redacting one costs the key.
#[derive(Clone, PartialEq, Eq)]
pub struct EndpointUrl {
    raw: String,
    redacted: String,
}

impl EndpointUrl {
    /// Build from an already-resolved URL string, expanding any `${VAR}` references.
    ///
    /// # Errors
    /// [`ConfigError::Invalid`] if a variable is unset or empty, naming it and never its value.
    pub fn resolve(field: &str, template: &str) -> Result<Self, ConfigError> {
        let raw = expand_env(field, template)?;
        let redacted = redact_url(&raw);
        Ok(Self { raw, redacted })
    }

    /// The real URL. The only way out of the wrapper; called by the transport and nowhere else.
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

/// Redacted, always: there is deliberately no un-redacted formatter.
impl std::fmt::Display for EndpointUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.redacted)
    }
}

/// Redacted, always: `?url` and a `{:?}` of any struct holding one both come through here.
impl std::fmt::Debug for EndpointUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.redacted)
    }
}

impl<'de> Deserialize<'de> for EndpointUrl {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        // The field name is not available here; `expand_env` names the variable either way.
        Self::resolve("<url>", &s).map_err(serde::de::Error::custom)
    }
}

/// Expand every `${VAR}` in `template` from the environment.
///
/// An unset or empty variable is an error rather than an empty expansion: a blank key segment 401s
/// at the first request, which is a worse place to discover a missing `.env` than startup.
pub(crate) fn expand_env(field: &str, template: &str) -> Result<String, ConfigError> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after.find('}').ok_or_else(|| {
            invalid(format!(
                "{field}: unterminated `${{` in the URL template; expected `${{VAR}}`"
            ))
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

// --- .env ---

/// Load `KEY=VALUE` pairs from a dotenv-style file into the environment, returning how many were
/// set. A variable already set in the real environment wins: the file is a convenience for local
/// runs, never an override of what an operator set on purpose. The return is a count rather than
/// the keys because nothing from the file may be logged; a missing file is not an error.
pub fn load_dotenv(path: &Path) -> usize {
    let Ok(text) = std::fs::read_to_string(path) else {
        return 0;
    };
    let mut set = 0;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
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

// --- Top level ---

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
    /// The volatility-scaled half-spread. Defaulted **on**, so a config written before it existed
    /// still loads with the term active: a constant half-spread is the defect it fixes.
    #[serde(default)]
    pub spread: SpreadConfig,
    /// Jump detection and withdrawal. Defaulted on, for the same reason.
    #[serde(default)]
    pub jump: JumpConfig,
    /// Killswitches.
    pub risk: RiskConfig,
    /// The RFQ maker endpoint. Absent means off — opt-in rather than defaulted-on like `spread`
    /// and `jump`, because those change how the pool prices and this signs tokens away.
    #[serde(default)]
    pub rfq: Option<RfqConfig>,
    /// The hedge leg. Opt-in for the same reason as `rfq`: it places orders with a live key.
    #[serde(default)]
    pub hedge: Option<HedgeConfig>,
    /// One entry per pair the bot quotes.
    pub pairs: Vec<PairConfig>,
}

/// The hedge leg: where to neutralise inventory, and how patiently. See [`crate::hedge`].
///
/// Absent means no hedging, the state the ladder was priced defensively for: the 25 bp slope
/// exists because inventory had nowhere to go.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HedgeConfig {
    /// REST base, e.g. `https://testnet.binancefuture.com`. Testnet while the pool trades mock
    /// tokens: hedging a fake position live would leave the only real leg in the system unbacked.
    pub base_url: String,
    /// Environment variable holding the venue API key. Never the key itself.
    pub key_env: String,
    /// Environment variable holding the venue secret. Never the secret itself.
    pub secret_env: String,
    /// The venue's taker fee, in hundredths of a basis point, from its rate card. Half of what
    /// derives the crossing interval; the other half is measured sigma.
    pub taker_fee_bps_e2: u32,
    /// Request timeout, milliseconds.
    #[serde(default = "d_hedge_timeout_ms")]
    pub timeout_ms: u64,
    /// Re-measure the venue's clock this often. Binance compares timestamps against its own clock,
    /// so a host that drifts starts failing every signed call at once.
    #[serde(default = "d_hedge_clock_resync_secs")]
    pub clock_resync_secs: u64,
    /// Hyperliquid's read endpoint, for pairs on [`HedgeVenue::HyperliquidPaper`]. Mainnet and
    /// unauthenticated: the equity books are there, and `allMids` needs no key and costs weight 2
    /// against a 1200/minute budget for every market at once.
    #[serde(default = "d_hyperliquid_url")]
    pub hyperliquid_url: String,
    /// One entry per pair to hedge. A pair with no entry is simply not hedged.
    #[serde(default)]
    pub pairs: Vec<HedgePair>,
}

/// Which venue a pair is hedged on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HedgeVenue {
    /// Binance USD-M futures, signed and live against whatever `base_url` points at.
    #[default]
    Binance,
    /// Hyperliquid, read-only: the decision is taken and the book written, but no order is sent.
    /// Paper because the equity markets are mainnet-only — see [`HedgeConfig::base_url`].
    HyperliquidPaper,
    /// Binance spot, read-only, filled into the same paper book. Signs nothing, so no key and no
    /// clock sync.
    ///
    /// The crypto reference is built from Binance/OKX/Bybit **spot**, so booking a paper fill at a
    /// Hyperliquid perp mid carries a -4 to -6 bp basis against the price the pool quotes off,
    /// against an ETH half-spread of 1.75 bp. Filling on the reference's own market removes it.
    BinancePaper,
}

/// How one pair maps onto the venue.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HedgePair {
    /// This crate's pair numbering.
    pub pair_id: u16,
    /// The venue's contract, e.g. `ETHUSDT`, or `xyz:TSLA` on Hyperliquid.
    pub symbol: String,
    /// Where this pair is hedged. Defaults to the signed Binance leg. Per-pair because the venues
    /// do not overlap: Binance USD-M carries the crypto pairs, Hyperliquid the equities via HIP-3.
    #[serde(default)]
    pub venue: HedgeVenue,
    /// The HIP-3 builder book, for [`HedgeVenue::HyperliquidPaper`]. Empty is the main perp book.
    /// Naming the dex names a fair value, not a route: builders attach their own oracles and
    /// disagree, measured in one second at `xyz:TSLA` 307.31 against `flx:TSLA` 395.50.
    #[serde(default)]
    pub dex: String,
    /// Decimals the venue accepts for quantity. Sending more is rejected outright.
    pub qty_decimals: u32,
    /// The venue's minimum order size, in the pool's base units.
    pub qty_base_min: String,
    /// RMS net exposure this pair is willing to carry unhedged, in the pool's base units.
    ///
    /// A RISK BUDGET, not a measurement, and the only input the band `h = sqrt(3) * carry` needs
    /// (see [`crate::hedge::derive_band`]). Exposure inside the band is carried deliberately:
    /// removing it costs more in fees than holding it costs in risk.
    pub carry_base: String,
    /// Largest single order, in the pool's base units. Empty or `"0"` means no clip.
    ///
    /// An EXECUTION limit, not a risk filter -- see [`crate::hedge::Band::order_max`]. Every unit
    /// of exposure is hedged either way; this only bounds how much goes out per order, so a pool
    /// converging on a long-standing position does not move the book against itself in one cycle.
    #[serde(default)]
    pub order_base_max: String,
    /// Don't send again within this many milliseconds. A crossing takes time to fill and to be
    /// reflected; firing again before then doubles the position instead of correcting it.
    #[serde(default = "d_hedge_cooloff_ms")]
    pub cooloff_ms: u64,
}

fn d_hyperliquid_url() -> String {
    "https://api.hyperliquid.xyz".into()
}
fn d_hedge_timeout_ms() -> u64 {
    5_000
}
fn d_hedge_clock_resync_secs() -> u64 {
    300
}
fn d_hedge_cooloff_ms() -> u64 {
    2_000
}

/// The RFQ maker: its own key, the contract it signs for, and how it prices.
///
/// The key is separate from [`TxConfig`]'s because the blast radii differ: a leaked updater key
/// posts a wrong ladder the killswitches notice, a leaked RFQ key signs the maker's balance away
/// up to its standing allowance with nothing to notice in time.
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
    pub half_spread_e2_max: u32,
    /// Largest notional a single order may commit, in whole quote tokens.
    pub notional_per_order_max: String,
    /// Decimals of the quote token these markets are denominated in.
    pub quote_decimals: u8,
    /// How long a signed order stays fillable, how long its inventory stays reserved, and the
    /// window whose option value the spread charges for — one number for all three.
    pub ttl_secs: u64,
    /// Floor on a single fill, in bps of the maker leg.
    #[serde(default)]
    pub fill_bps_min: u16,
}

impl RfqConfig {
    /// Where the signing key lives. Unlike `tx`, a present `[rfq]` section must name a source
    /// rather than defaulting to none.
    ///
    /// # Errors
    /// [`ConfigError`] when neither or both are set, when the variable name is empty, or when it
    /// looks like the key itself pasted where the variable name goes.
    pub fn key_source(&self) -> Result<KeySource, ConfigError> {
        let source =
            match (&self.private_key_env, &self.private_key_file) {
                (Some(_), Some(_)) => {
                    return Err(invalid(
                        "rfq: set exactly one of private_key_env / private_key_file, not both",
                    ))
                }
                (None, None) => return Err(invalid(
                    "rfq: no signing key; set private_key_env or private_key_file, or remove the \
                     [rfq] section to run without the RFQ leg",
                )),
                (Some(v), None) => KeySource::Env(v.clone()),
                (None, Some(p)) => KeySource::File(p.clone()),
            };
        if let KeySource::Env(name) = &source {
            if name.trim().is_empty() {
                return Err(invalid(
                    "rfq.private_key_env: must name an environment variable",
                ));
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

    /// [`Self::notional_per_order_max`] in the quote token's own units.
    ///
    /// # Errors
    /// [`ConfigError::Units`].
    pub fn notional_units_max(&self) -> Result<u128, ConfigError> {
        Ok(units::parse_fixed(
            &self.notional_per_order_max,
            self.quote_decimals,
        )?)
    }

    /// The pricing parameters, as `quoting` wants them.
    ///
    /// # Errors
    /// [`ConfigError::Units`] if the notional cap is not a decimal number.
    pub fn params(
        &self,
        vol_horizon_secs: u64,
    ) -> Result<crate::quoting::MakerParams, ConfigError> {
        Ok(crate::quoting::MakerParams {
            base_half_spread_e2: self.base_half_spread_e2,
            sigma_coefficient_e2: self.sigma_coefficient_e2,
            half_spread_e2_max: self.half_spread_e2_max,
            notional_per_order_max: self.notional_units_max()?,
            ttl_secs: self.ttl_secs,
            // Taken from the skew estimator rather than configured twice: two numbers meaning
            // "the window sigma is measured over" is two ways to misprice the TTL.
            sigma_horizon_secs: vol_horizon_secs,
            fill_bps_min: self.fill_bps_min,
        })
    }
}

impl Config {
    /// Read and validate a config file.
    ///
    /// # Errors
    /// [`ConfigError`] for an unreadable file, a parse failure (including an unknown field), or a
    /// failed range check.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let cfg: Self = toml::from_str(&text)?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Every check that does not need the chain.
    ///
    /// # Errors
    /// [`ConfigError::Invalid`] naming the field, or [`ConfigError::Units`] for a bad amount.
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
            p.validate(self.feed.venues_min)?;
            if !seen_ids.insert(p.pair_id) {
                return Err(invalid(format!(
                    "pairs: pair_id {} appears twice",
                    p.pair_id
                )));
            }
            // Two pairs on one symbol doubles the quote traffic for one price.
            if !seen_symbols.insert(p.symbol.clone()) {
                return Err(invalid(format!(
                    "pairs: symbol `{}` appears twice",
                    p.symbol
                )));
            }
            if f64::from(self.spread.half_spread_bps_max) < p.half_spread_bps {
                return Err(invalid(format!(
                    "spread.half_spread_bps_max ({}) is below pairs[{}].half_spread_bps ({}); \
                     the cap bounds the VOLATILITY TERM and can never narrow the configured \
                     spread, so this combination would silently disable the term for that pair",
                    self.spread.half_spread_bps_max, p.pair_id, p.half_spread_bps
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

    /// Every venue at least one pair names, in a stable order. Enabled by use rather than by a
    /// separate switch: a switched-on venue with no symbol meets no quorum while reading healthy.
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
            .filter_map(|p| {
                p.venues
                    .symbol(venue)
                    .map(|s| (s.to_string(), p.symbol.clone()))
            })
            .collect()
    }
}

// --- Chain ---

/// The three endpoints, the head-subscription watchdog, and the liveness thresholds.
///
/// Three different freshness guarantees, not redundancy:
///
/// * `ws_url` — Nodit WSS, the only endpoint answering `eth_subscribe`. Says *when* to look.
/// * `flashblocks_rpc_url` — GIWA flashblocks under `pending`. Says what is true *now*, including
///   swaps that have already moved `bidUsed`.
/// * `rpc_url` — Nodit HTTPS. Says what is *final*, the only acceptable basis for a nonce.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChainConfig {
    /// Websocket RPC carrying the `newHeads` subscription that drives the quote loop. Must be
    /// `ws(s)://`: an HTTPS endpoint answers `notifications not supported` and never subscribes.
    pub ws_url: EndpointUrl,
    /// Ordinary RPC. Transactions are submitted here, because this is the canonical view.
    pub rpc_url: EndpointUrl,
    /// Flashblocks RPC. **Read only, and only under the `pending` tag**: its `latest` lags the
    /// ordinary endpoint by about two blocks. See [`crate::chain`].
    pub flashblocks_rpc_url: EndpointUrl,
    /// Extra read endpoints, rotated alongside [`Self::flashblocks_rpc_url`].
    ///
    /// Reads only, structurally: a read is a question about one block that any node can answer,
    /// whereas rotating the transmit path would read a nonce from a node that has not seen the
    /// previous send. Several keys multiply the budget, several providers also buy independence.
    #[serde(default)]
    pub read_rpc_urls: Vec<EndpointUrl>,
    /// Fallback endpoints for the **write** path — nonce, submit, receipt — after
    /// [`Self::rpc_url`].
    ///
    /// A quota is a property of a key, not of a node, so a single-key write path is the one place
    /// where one exhausted key stops the bot sending at all. Selected with `Selection::Pin`
    /// rather than `Rotate`: consecutive calls here must reach the same node's view of our nonce.
    #[serde(default)]
    pub write_rpc_urls: Vec<EndpointUrl>,
    /// EIP-155 chain id. 91342 for GIWA Sepolia.
    pub chain_id: u64,
    /// `PropPool` address.
    pub pool: Address,
    /// Multicall3, preinstalled at the canonical address in GIWA's genesis, which is what lets a
    /// whole read cycle be one request.
    pub multicall3: Address,
    /// The chain's block cadence. GIWA is 1s, with heads measured at 904-1050ms. A fact about the
    /// chain rather than a tuning knob: it is the unit the head watchdog is expressed in.
    #[serde(default = "d_block_time_ms")]
    pub block_time_ms: u64,
    /// **The head watchdog.** No head for this many block times means a quiet subscription, which
    /// is worse than a broken one because the bot sits believing the chain has stopped. Tripping
    /// it forces the fallback read and hands the liveness question to
    /// [`crate::chain::ChainHealth`], which decides from the *block number* which one stopped.
    #[serde(default = "d_head_stale_blocks")]
    pub head_stale_blocks: u32,
    /// First reconnect delay for the head subscription. Doubles per consecutive failure.
    /// A subscription that dies must not become a hot reconnect loop.
    #[serde(default = "d_ws_reconnect_initial_ms")]
    pub ws_reconnect_initial_ms: u64,
    /// Reconnect delay ceiling for the head subscription.
    #[serde(default = "d_ws_reconnect_max_ms")]
    pub ws_reconnect_max_ms: u64,
    /// **Fallback only**: the floor under the `newHeads` subscription, so a socket that dies or
    /// goes silent degrades into polling instead of stalling. At a healthy 1s head cadence it
    /// essentially never fires, so sizing it like a primary driver is a misreading.
    #[serde(default = "d_fallback_poll_interval_ms")]
    pub fallback_poll_interval_ms: u64,
    /// How often the quote cycle runs, in milliseconds. Heads wake it early but do not pace it:
    /// the posted spread has to cover the reference's drift over exactly the re-pricing interval,
    /// so pacing on heads would put a one-second floor under that window and let the chain's clock
    /// set the spread. Below the chain reader's own interval the cycle sees the same view twice.
    #[serde(default = "d_quote_interval_ms")]
    pub quote_interval_ms: u64,
    /// Per-request HTTP timeout.
    #[serde(default = "d_request_timeout_ms")]
    pub request_timeout_ms: u64,
    /// Runaway guard, requests per second, across *all* RPC use on one endpoint. A fuse rather
    /// than a budget: normal operation never touches it, and a reconnect storm cannot flood.
    #[serde(default = "d_requests_per_sec")]
    pub requests_per_sec: f64,
    /// Burst allowance before the sustained rate binds. A send needs four or five requests in
    /// quick succession (nonce, submit, receipt polls), so this must be comfortably above one.
    #[serde(default = "d_request_burst")]
    pub request_burst: f64,
    /// First backoff after an HTTP 429, doubling per consecutive 429 up to the ceiling below.
    /// Without it a transient upstream failure becomes a flood.
    #[serde(default = "d_rate_limit_backoff_initial_ms")]
    pub rate_limit_backoff_initial_ms: u64,
    /// Backoff ceiling.
    #[serde(default = "d_rate_limit_backoff_max_ms")]
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
    /// Half-spread widening, in bps, while the chain view is degraded. Quoting into a view that
    /// cannot be refreshed is the adverse-selection case; widening is the cheap partial defence.
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
fn d_quote_interval_ms() -> u64 {
    200
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
fn d_rate_limit_backoff_initial_ms() -> u64 {
    2_000
}
fn d_rate_limit_backoff_max_ms() -> u64 {
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
    /// How long without a `newHeads` delivery counts as a silent subscription. A multiple of the
    /// block time rather than an absolute, so it stays correct if the chain's cadence changes.
    #[must_use]
    pub const fn head_stale_after(&self) -> std::time::Duration {
        std::time::Duration::from_millis(
            self.block_time_ms
                .saturating_mul(self.head_stale_blocks as u64),
        )
    }

    /// Every check that does not need the chain, in four groups run in the order written: a later
    /// group may compare against a field an earlier one has already bounded.
    fn validate(&self) -> Result<(), ConfigError> {
        self.validate_endpoints()?;
        self.validate_cadence()?;
        self.validate_budget()?;
        self.validate_liveness()
    }

    fn validate_endpoints(&self) -> Result<(), ConfigError> {
        for (name, url) in [
            ("rpc_url", &self.rpc_url),
            ("flashblocks_rpc_url", &self.flashblocks_rpc_url),
        ] {
            if !url.is_http() {
                return Err(invalid(format!(
                    "chain.{name}: must be an http(s) URL, got `{url}`"
                )));
            }
        }
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
        Ok(())
    }

    fn validate_cadence(&self) -> Result<(), ConfigError> {
        if !(100..=60_000).contains(&self.block_time_ms) {
            return Err(invalid("chain.block_time_ms: must be 100..=60000"));
        }
        // One missed head is ordinary jitter at the measured 904-1050ms cadence.
        if self.head_stale_blocks < 2 {
            return Err(invalid(
                "chain.head_stale_blocks: must be >= 2; \
                 a one-block window trips on ordinary jitter",
            ));
        }
        if !(250..=600_000).contains(&self.fallback_poll_interval_ms) {
            return Err(invalid(format!(
                "chain.fallback_poll_interval_ms: must be 250..=600000, got {}",
                self.fallback_poll_interval_ms
            )));
        }
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
        Ok(())
    }

    fn validate_budget(&self) -> Result<(), ConfigError> {
        // `is_finite` is tested first because NaN compares false against both bounds and would
        // otherwise pass every range check in this file.
        if !self.requests_per_sec.is_finite()
            || self.requests_per_sec <= 0.0
            || self.requests_per_sec > 1_000.0
        {
            return Err(invalid(
                "chain.requests_per_sec: must be a finite value in (0, 1000]",
            ));
        }
        if !self.request_burst.is_finite()
            || self.request_burst < 1.0
            || self.request_burst > 2_000.0
        {
            return Err(invalid(
                "chain.request_burst: must be a finite value in [1, 2000]",
            ));
        }
        if self.ws_reconnect_initial_ms == 0
            || self.ws_reconnect_max_ms < self.ws_reconnect_initial_ms
        {
            return Err(invalid(
                "chain.ws_reconnect_max_ms must be >= ws_reconnect_initial_ms, \
                 which must be non-zero",
            ));
        }
        if self.rate_limit_backoff_initial_ms == 0
            || self.rate_limit_backoff_max_ms < self.rate_limit_backoff_initial_ms
        {
            return Err(invalid(
                "chain.rate_limit_backoff_max_ms must be >= rate_limit_backoff_initial_ms, \
                 which must be non-zero",
            ));
        }
        Ok(())
    }

    fn validate_liveness(&self) -> Result<(), ConfigError> {
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
        // The watchdog needs room to fire and let the fallback prove whether the chain is down,
        // all before the halt timer expires; at or beyond it the bot withdraws having said nothing.
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
        if self.degraded_extra_half_spread_bps as u128 > dubu_core::ladder::BPS_MAX {
            return Err(invalid(
                "chain.degraded_extra_half_spread_bps: must be <= 9999",
            ));
        }
        Ok(())
    }
}

// --- Feed ---

/// Per-venue endpoint overrides, all optional; the defaults are [`VenueId::default_url`].
///
/// No key or secret field for any venue, because none of these public feeds has one. A venue is
/// enabled by a pair naming a symbol for it in [`PairVenues`], not by appearing here.
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
    /// Override for Hyperliquid's websocket. See [`VenueId::default_url`].
    #[serde(default)]
    pub hyperliquid: Option<String>,
    /// Pyth Hermes base URL, **`https://`**: this is the one polled venue and the paths are
    /// appended to it. See [`VenueId::transport`].
    #[serde(default)]
    pub pyth: Option<String>,
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
            VenueId::Hyperliquid => &self.hyperliquid,
            VenueId::Pyth => &self.pyth,
        };
        over.as_deref().unwrap_or_else(|| venue.default_url())
    }
}

/// Exchange market-data feeds, the quorum rule, and the MAD outlier filter.
///
/// **Market data only**: no key, no order entry. The leg that places orders is [`HedgeConfig`].
///
/// The four quorum fields are the whole policy over the cross-section
/// [`crate::fair_value::combine`] sees: how many venues are enough, how far from the pack is too
/// far, and how far apart the pack may be before there is no single price to quote at all. One
/// exchange would leave the ladder's price bounds checking the number that produced them.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeedConfig {
    /// Endpoint overrides. Omit the table entirely to use the public defaults.
    #[serde(default)]
    pub urls: VenueUrls,
    /// A symbol with no accepted tick for this long is stale on that venue and contributes nothing
    /// to the cross-section. Hard, not a weight: [`crate::feed::FeedSnapshot::live`] drops it.
    #[serde(default = "d_feed_stale_ms")]
    pub stale_after_ms: u64,
    /// First reconnect delay. Doubles per consecutive failure up to the ceiling.
    #[serde(default = "d_reconnect_initial_ms")]
    pub reconnect_initial_ms: u64,
    /// Reconnect delay ceiling.
    #[serde(default = "d_reconnect_max_ms")]
    pub reconnect_max_ms: u64,
    /// No frame at all, not even a keepalive answer, for this long forces a reconnect. These venues
    /// push continuously or answer a ping every 20s, so a silent socket is dead well before TCP
    /// notices. Websocket venues only; the polled ones have nothing to time out on.
    #[serde(default = "d_read_timeout_ms")]
    pub read_timeout_ms: u64,
    /// How often a polled venue is read. Applies to [`VenueId::Pyth`], the only one; see
    /// [`crate::feed::Transport`].
    ///
    /// Must not exceed `stale_after_ms`, or the venue is stale between its own polls by
    /// construction and never contributes to a cross-section.
    #[serde(default = "d_poll_interval_ms")]
    pub poll_interval_ms: u64,
    /// **The quorum**: venues that must survive the MAD filter for a reference price to exist at
    /// all. Below it the bot quotes nothing and says `no_quorum`. Must be at least 2, because one
    /// venue is a single-source oracle.
    #[serde(default = "d_venues_min")]
    pub venues_min: u8,
    /// Multiplier on the median absolute deviation. A venue further than `mad_k * MAD` from the
    /// cross-venue median is an outlier and is dropped.
    #[serde(default = "d_mad_k")]
    pub mad_k: f64,
    /// Floor under the rejection threshold, in bps. Without it, venues agreeing to within a tick
    /// drive the MAD to zero and everything but the median is rejected. The largest ordinary
    /// single-venue deviation on ETHUSDT/BTCUSDT measured 1.6 bps, so the default of 2 clears it.
    #[serde(default = "d_mad_floor_bps")]
    pub mad_floor_bps: f64,
    /// **The regime gate.** Cross-venue MAD above this, in bps, means the venues disagree rather
    /// than that one of them is wrong, so the bot refuses to quote instead of averaging through a
    /// split market. Must exceed `mad_floor_bps`.
    #[serde(default = "d_dispersion_bps_max")]
    pub dispersion_bps_max: f64,
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
/// Pyth's equity feeds publish about once a second, measured; matching that keeps the reference as
/// fresh as the venue can make it without asking for prices that do not exist yet.
fn d_poll_interval_ms() -> u64 {
    1_000
}
fn d_venues_min() -> u8 {
    2
}
fn d_mad_k() -> f64 {
    4.0
}
fn d_mad_floor_bps() -> f64 {
    2.0
}
fn d_dispersion_bps_max() -> f64 {
    25.0
}

/// Round a bps value to deci-bps, the resolution the cross-section is measured at.
fn decibps(v: f64) -> u32 {
    (v * 10.0).round().max(0.0) as u32
}

/// Decimal basis points to hundredths of a basis point, the unit the ladder is built in.
///
/// Whole bps ran out of resolution once sigma was scaled to the quote's real exposure window:
/// ETH's volatility term is 0.61 bp against an `s0` of 1, so rounding would lose most of it.
fn bps_e2(v: f64) -> u32 {
    (v * 100.0).round().max(0.0) as u32
}

impl FeedConfig {
    /// The MAD filter's knobs, converted out of the config's decimal-bps form.
    #[must_use]
    pub fn mad_params(&self) -> MadParams {
        self.mad_params_with(None)
    }

    /// The same, with a pair's [`PairConfig::venues_min`] override applied. Only the quorum is
    /// per-pair: the multiplier and the dispersion ceiling are properties of the filter itself.
    #[must_use]
    pub fn mad_params_with(&self, over: Option<u8>) -> MadParams {
        MadParams {
            venues_min: over.unwrap_or(self.venues_min),
            k_tenths: (self.mad_k * 10.0).round().max(0.0) as u32,
            floor_decibps: decibps(self.mad_floor_bps),
            dispersion_decibps_max: decibps(self.dispersion_bps_max),
        }
    }

    fn validate(&self) -> Result<(), ConfigError> {
        for venue in VenueId::ALL {
            let url = self.urls.get(venue);
            // The scheme follows the venue's transport, not a house rule: a `wss://` check against
            // the polled venue would refuse its only correct endpoint.
            let (ok, want) = match venue.transport() {
                Transport::WebSocket => (
                    url.starts_with("wss://") || url.starts_with("ws://"),
                    "ws(s)",
                ),
                Transport::Http => (
                    url.starts_with("https://") || url.starts_with("http://"),
                    "http(s)",
                ),
            };
            if !ok {
                return Err(invalid(format!(
                    "feed.urls.{venue}: must be a {want} URL, got `{url}`"
                )));
            }
        }
        if !(100..=600_000).contains(&self.stale_after_ms) {
            return Err(invalid("feed.stale_after_ms: must be 100..=600000"));
        }
        if self.poll_interval_ms == 0 || self.poll_interval_ms > self.stale_after_ms {
            return Err(invalid(format!(
                "feed.poll_interval_ms ({}) must be non-zero and <= feed.stale_after_ms ({}); \
                 a venue polled less often than its own staleness window is stale between polls",
                self.poll_interval_ms, self.stale_after_ms
            )));
        }
        if self.reconnect_initial_ms == 0 || self.reconnect_max_ms < self.reconnect_initial_ms {
            return Err(invalid(
                "feed.reconnect_max_ms must be >= reconnect_initial_ms, which must be non-zero",
            ));
        }
        if self.read_timeout_ms < self.stale_after_ms {
            return Err(invalid(format!(
                "feed.read_timeout_ms ({}) must be >= feed.stale_after_ms ({}); \
                 reconnecting sooner than the staleness window makes the window unobservable",
                self.read_timeout_ms, self.stale_after_ms
            )));
        }
        if self.venues_min < 2 {
            return Err(invalid(format!(
                "feed.venues_min is {}, and must be at least 2; quoting off a single venue makes \
                 the ladder's own price bounds a check against the number that produced them",
                self.venues_min
            )));
        }
        if usize::from(self.venues_min) > VenueId::ALL.len() {
            return Err(invalid(format!(
                "feed.venues_min ({}) exceeds the {} venues this crate can speak to",
                self.venues_min,
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
        if !self.dispersion_bps_max.is_finite() || self.dispersion_bps_max <= 0.0 {
            return Err(invalid(
                "feed.dispersion_bps_max: must be finite and non-zero",
            ));
        }
        if self.dispersion_bps_max <= self.mad_floor_bps {
            return Err(invalid(format!(
                "feed.dispersion_bps_max ({}) must exceed feed.mad_floor_bps ({}); \
                 a dispersion limit at or below the rejection floor gates before the filter runs",
                self.dispersion_bps_max, self.mad_floor_bps
            )));
        }
        Ok(())
    }
}

// --- Inventory skew ---

/// The volatility estimator and the Avellaneda–Stoikov knobs.
///
/// Global rather than per-pair: risk aversion and its horizon are properties of the desk, and two
/// pairs disagreeing about how far ahead to look would be two strategies sharing one killswitch.
/// `target_base_share_pct` is the exception and lives on [`PairConfig`].
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

    /// The window the HALF-SPREAD is exposed for, in seconds. See [`crate::spread::rescale_sigma`].
    ///
    /// Deliberately distinct from `vol_horizon_secs`, which is how long inventory is held: a
    /// quote's exposure is how long it can be hit at a price the market has left behind, measured
    /// at p99 = 3s. Sizing the spread off the 300s inventory window instead over-covers by 100x.
    #[serde(default = "d_spread_horizon_secs")]
    pub spread_horizon_secs: u64,
    /// Samples closer together than this are skipped rather than divided by a tiny interval.
    #[serde(default = "d_vol_sample_ms_min")]
    pub vol_min_sample_ms: u64,
    /// A gap longer than this is an outage, not a return. The estimator re-anchors.
    #[serde(default = "d_vol_sample_ms_max")]
    pub vol_max_sample_ms: u64,
    /// Risk aversion `gamma`. `skew_bps = gamma * q * sigma_bps^2 / 10_000`, so with ETHUSDT's
    /// measured 300s `sigma` of about 10 bps, `gamma = 1000` and a 20% imbalance give 2 bp.
    #[serde(default = "d_gamma")]
    pub gamma: f64,
    /// Cap on a **positive** skew (book down, the pool is long and selling), in bps.
    #[serde(default = "d_skew_positive_bps_max")]
    pub positive_bps_max: u16,
    /// Cap on a **negative** skew (book up, the pool is short and buying), as a magnitude in bps.
    /// Deliberately the tighter of the two; [`crate::skew::compute`] has the argument.
    #[serde(default = "d_skew_negative_bps_max")]
    pub negative_bps_max: u16,
}

fn d_vol_tau_ms() -> u64 {
    60_000
}
fn d_vol_horizon_secs() -> u64 {
    300
}
/// The measured p99 of `quote_age_secs`. See [`crate::spread::rescale_sigma`].
const fn d_spread_horizon_secs() -> u64 {
    3
}
fn d_vol_sample_ms_min() -> u64 {
    100
}
fn d_vol_sample_ms_max() -> u64 {
    10_000
}
fn d_gamma() -> f64 {
    1_000.0
}
fn d_skew_positive_bps_max() -> u16 {
    30
}
fn d_skew_negative_bps_max() -> u16 {
    10
}

impl SkewConfig {
    /// The volatility estimator's configuration.
    #[must_use]
    pub const fn vol_config(&self) -> VolConfig {
        VolConfig {
            tau_ms: self.vol_tau_ms,
            horizon_secs: self.vol_horizon_secs,
            sample_ms_min: self.vol_min_sample_ms,
            sample_ms_max: self.vol_max_sample_ms,
        }
    }

    /// The skew's own knobs.
    #[must_use]
    pub fn params(&self) -> SkewParams {
        SkewParams {
            gamma_e2: (self.gamma * 100.0).round().max(0.0) as u64,
            positive_bps_max: self.positive_bps_max,
            negative_bps_max: self.negative_bps_max,
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
        let bps_max = dubu_core::ladder::BPS_MAX;
        for (name, v) in [
            ("positive_bps_max", self.positive_bps_max),
            ("negative_bps_max", self.negative_bps_max),
        ] {
            if u128::from(v) > bps_max {
                return Err(invalid(format!("skew.{name}: must be <= {bps_max}")));
            }
        }
        // A negative skew lifts the pool's bid toward and past fair value, a free option written
        // to whoever notices; a positive one lowers both sides and is defensive. So the lifting
        // direction takes the tighter cap, and a looser one here would invert the argument.
        if self.negative_bps_max > self.positive_bps_max {
            return Err(invalid(format!(
                "skew.negative_bps_max ({}) exceeds skew.positive_bps_max ({}); \
                 the book-LIFTING direction raises the pool's bid toward fair value and is the \
                 pick-off direction, so it must be the tighter cap, not the looser one",
                self.negative_bps_max, self.positive_bps_max
            )));
        }
        Ok(())
    }
}

// --- The volatility-scaled half-spread ---

/// `half_spread = min(s0 + s1 * sigma, cap) + degraded_extra`.
///
/// Global rather than per-pair because `s1` is dimensionless and multiplies each pair's own
/// `sigma`, so one value is already correct across ETHUSDT at 10 bp and BTCUSDT at 3 bp.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpreadConfig {
    /// `s1`, in bps of half-spread per bp of `sigma` at the estimator's horizon. Derived rather
    /// than picked: the sweep's smallest measured-positive half-spread against a 100 bp jump is
    /// 30 bp and should be reached when such a jump is live, which at ETHUSDT's `sigma(300s)` of
    /// 10 bp is 5x that level, so `5 + s1 * 50 = 30` gives `s1 = 0.5`.
    #[serde(default = "d_vol_coefficient")]
    pub vol_coefficient: f64,
    /// The ceiling on `s0 + s1 * sigma`. Past it widening stops being a defence — the sweep's 60 bp
    /// row earns *less* than its 30 bp row — and [`crate::jump`] takes over. 30 bp is UniV2's fee,
    /// so the pool never quotes worse than the constant-fee AMM it is trying to beat. The
    /// degraded-chain widening is deliberately added *after* this cap.
    #[serde(default = "d_half_spread_bps_max")]
    pub half_spread_bps_max: u16,
}

fn d_vol_coefficient() -> f64 {
    0.5
}
fn d_half_spread_bps_max() -> u16 {
    30
}

impl Default for SpreadConfig {
    fn default() -> Self {
        Self {
            vol_coefficient: d_vol_coefficient(),
            half_spread_bps_max: d_half_spread_bps_max(),
        }
    }
}

impl SpreadConfig {
    /// The knobs [`crate::spread::compute`] takes.
    #[must_use]
    pub fn params(&self) -> crate::spread::SpreadParams {
        crate::spread::SpreadParams {
            vol_coefficient_e2: (self.vol_coefficient * 100.0).round().max(0.0) as u32,
            half_spread_bps_e2_max: u32::from(self.half_spread_bps_max) * 100,
        }
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if !self.vol_coefficient.is_finite() || !(0.0..=100.0).contains(&self.vol_coefficient) {
            return Err(invalid(
                "spread.vol_coefficient: must be a finite value in [0, 100]; 0 disables the \
                 volatility term and restores the constant half-spread",
            ));
        }
        if self.half_spread_bps_max == 0 {
            return Err(invalid("spread.half_spread_bps_e2_max: must be non-zero"));
        }
        if u128::from(self.half_spread_bps_max) > dubu_core::ladder::BPS_MAX {
            return Err(invalid("spread.half_spread_bps_e2_max: must be <= 9999"));
        }
        Ok(())
    }
}

// --- Jump detection ---

/// Jump detection and withdrawal. See [`crate::jump`] for the derivations.
///
/// Deliberately no basis-point threshold here: both bounds on the trip threshold come from the
/// pair's own `half_spread_bps` and `width_bps` — the floor is where the posted quote stops being
/// on the right side of fair value, the ceiling `half_spread + width/2` is where the pool pays the
/// excess regardless — so one tunable number would be wrong on some pair.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JumpConfig {
    /// Off means quote through everything.
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
    /// How often the fast lane samples the reference, in milliseconds — the detection latency,
    /// against a mean of half a block if the quote loop's own wake did the check. The fast lane
    /// runs between wakes off the in-memory feed snapshots and costs no RPC unless something trips.
    #[serde(default = "d_jump_scan_interval_ms")]
    pub scan_interval_ms: u64,
    /// `maxPriorityFeePerGas` for a jump withdrawal only, in gwei, as a decimal string.
    ///
    /// GIWA's sequencer has no public mempool and orders by **highest fee first**, so the flat
    /// near-zero tip `tx.rs` pays on quote traffic loses here: the withdrawal races a searcher
    /// willing to outbid a quoting bot, and the auction is won by paying more, not by arriving
    /// earlier. At 0.001 gwei base and ~29k gas, 0.5 gwei costs about five cents.
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
    /// The knobs [`crate::jump::Detector`] takes. `skew` supplies the sampling window so that the
    /// detector and the volatility estimator agree on what a hole in the reference is.
    #[must_use]
    pub fn params(&self, skew: &SkewConfig) -> crate::jump::Params {
        crate::jump::Params {
            sigma_k_e2: (self.sigma_k * 100.0).round().max(0.0) as u32,
            cooloff: std::time::Duration::from_secs(self.cooloff_secs),
            sample_min: std::time::Duration::from_millis(skew.vol_min_sample_ms),
            sample_max: std::time::Duration::from_millis(skew.vol_max_sample_ms),
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
        Ok(units::parse_fixed(
            &self.withdraw_priority_fee_per_gas_gwei,
            9,
        )?)
    }

    fn validate(&self, chain: &ChainConfig) -> Result<(), ConfigError> {
        if !self.sigma_k.is_finite() || !(0.0..=1_000.0).contains(&self.sigma_k) {
            return Err(invalid("jump.sigma_k: must be a finite value in [0, 1000]"));
        }
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
            return Err(invalid(
                "jump.withdraw_max_fee_per_gas_gwei: must be non-zero",
            ));
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

// --- Transactions ---

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
    /// **Must be explicitly `true` to broadcast anything**; absent means dry run.
    #[serde(default)]
    pub transmit_allowed: bool,
    /// Name of the environment variable holding the updater's private key.
    #[serde(default)]
    pub private_key_env: Option<String>,
    /// Path to a file holding the updater's private key. Mutually exclusive with
    /// `private_key_env`.
    #[serde(default)]
    pub private_key_file: Option<PathBuf>,
    /// Gas limit. `updateQuote` for one pair measured 28,747 gas and `refreshCapacity` is cheaper;
    /// this is deliberately several times that, because gas is nearly free on GIWA and a quote that
    /// fails to land is not.
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
    /// How many transactions one pair may have outstanding at once.
    ///
    /// Two by default: at one, roughly a fifth of cycles were held by `PushInFlight` waiting on a
    /// receipt still inside the measured inclusion latency (see [`crate::tx`]). Raising it is safe
    /// because nonce ordering is absolute for one sender and `updateQuote` is an idempotent
    /// overwrite; unbounded is not, since a stall at the head blocks every nonce behind it.
    #[serde(default = "d_in_flight_max")]
    pub in_flight_max: usize,
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
fn d_in_flight_max() -> usize {
    2
}

impl TxConfig {
    /// Where the key is to be read from, if configured at all.
    ///
    /// # Errors
    /// [`ConfigError::Invalid`] if both sources are set.
    pub fn key_source(&self) -> Result<Option<KeySource>, ConfigError> {
        match (&self.private_key_env, &self.private_key_file) {
            (Some(_), Some(_)) => Err(invalid(
                "tx: set exactly one of private_key_env / private_key_file, not both",
            )),
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
                return Err(invalid(
                    "tx.private_key_env: must name an environment variable",
                ));
            }
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
                "tx.max_priority_fee_per_gas_gwei ({tip} wei) exceeds \
                 tx.max_fee_per_gas_gwei ({max} wei)"
            )));
        }
        if !(5..=3_600).contains(&self.pending_timeout_secs) {
            return Err(invalid("tx.pending_timeout_secs: must be 5..=3600"));
        }
        Ok(())
    }
}

// --- Risk ---

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
    /// Measure the switches but do not latch them.
    ///
    /// Everything runs as in enforcement — the window fills, the drawdown is computed, the gross
    /// loss accumulates and is persisted — but a trip logs `event = "halt_shadow"` instead of
    /// stopping the group. It exists because one global `bleed_limit` trips the sparse equity
    /// group on revaluation across reference gaps, with no trade contributing to the drawdown.
    ///
    /// **This disables the drawdown halt**, so it wants an operator watching. Default off, so a
    /// config that forgets the key enforces.
    #[serde(default)]
    pub shadow: bool,
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
            return Err(invalid(
                "risk.bleed_limit: must be non-zero; a zero limit trips instantly",
            ));
        }
        if budget == 0 {
            return Err(invalid(
                "risk.loss_budget: must be non-zero; a zero budget trips instantly",
            ));
        }
        if budget < bleed {
            return Err(invalid(format!(
                "risk.loss_budget ({budget}) is below risk.bleed_limit ({bleed}); \
                 the cumulative budget would always trip first and the \
                 bleed switch is then dead code"
            )));
        }
        Ok(())
    }
}

// --- Pairs ---

/// Which symbol each venue quotes for one pair.
///
/// Every venue spells the same market differently, so the mapping is configuration rather than a
/// transformation guessed in code. Naming a venue here also **enables** it, and unknown fields are
/// a hard error, so `bybbit = "..."` fails at startup rather than leaving the bot a venue short.
///
/// Products are not interchangeable: `ETH-USD` is **not** a second observation of `ETHUSDT`, since
/// the USDT/USD basis holds Coinbase's USD books a persistent 8-9 bps above the three USDT venues
/// — a bias, against a 5 bp half-spread, rather than redundancy.
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
    /// This pair on Hyperliquid, carrying the HIP-3 builder: `xyz:AAPL`. The prefix names a fair
    /// value, not a route — see [`HedgePair::dex`].
    #[serde(default)]
    pub hyperliquid: Option<String>,
    /// This pair's Pyth symbol, in full: `Equity.US.AAPL/USD`. Resolved to a feed id at runtime,
    /// so the id never appears in configuration and cannot drift from the symbol it names.
    ///
    /// The `.PRE` / `.ON` / `.POST` variants are refused; see [`crate::feed::pyth`].
    #[serde(default)]
    pub pyth: Option<String>,
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
            VenueId::Hyperliquid => self.hyperliquid.as_deref(),
            VenueId::Pyth => self.pyth.as_deref(),
        }
    }

    /// Every venue this pair is quoted on, in a stable order.
    pub fn iter(&self) -> impl Iterator<Item = (VenueId, &str)> {
        VenueId::ALL
            .into_iter()
            .filter_map(|v| self.symbol(v).map(|s| (v, s)))
    }

    /// How many venues quote this pair.
    #[must_use]
    pub fn count(&self) -> usize {
        self.iter().count()
    }
}

/// One quoted pair.
///
/// `symbol` is the canonical name of the **real** asset the pool's mock token tracks, not a market
/// in the mock token: nothing trades `mWETH`, so pairId 1 is priced off `ETHUSDT`. It is also the
/// key every venue's ticks are recorded under, which is why each venue's own spelling is a separate
/// field in [`PairVenues`]: without that translation each venue sits in its own namespace and the
/// cross-section is permanently empty.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairConfig {
    /// `PropPool` pair id. 1-based; id 0 is reserved and never a real pair.
    pub pair_id: u16,
    /// Canonical symbol of the **real** asset this pair's mock base token tracks. See the struct
    /// docs — this is not a market in the mock token.
    pub symbol: String,
    /// Venues that must agree before this pair has a reference, overriding
    /// [`FeedConfig::venues_min`].
    ///
    /// Set it only where a second independent source does not exist: the cross-venue median is
    /// what catches one venue publishing a wrong price confidently. The equity pairs are the case
    /// — every HIP-3 builder other than `xyz` carries ZERO open interest, so its prints are stale
    /// oracle marks 16% (AAPL) to 30% (TSLA) away, and admitting one meets the quorum with a dead
    /// book that drags the median, which is worse than a quorum of one because it looks met.
    #[serde(default)]
    pub venues_min: Option<u8>,

    /// Correlation group, for jump contagion. Pairs in the same group withdraw together, so
    /// `jump.scope = "book"` means the group rather than the whole book. A claim about
    /// correlation, so it is stated rather than inferred: SK Hynix moving says nothing about ETH.
    /// Unset groups a pair with every other unset pair.
    #[serde(default)]
    pub jump_group: String,

    /// Which symbol each venue quotes this pair under. At least as many as the quorum needs.
    pub venues: PairVenues,
    /// Decimals of the base token. Verified against the deployed ERC-20 at startup.
    pub base_decimals: u8,
    /// Decimals of the quote token. Verified against the deployed ERC-20 at startup.
    pub quote_decimals: u8,
    /// Half the bid/ask spread, in bps of the skewed fair value. Decimal: `0.5` is half a bp.
    ///
    /// `s0` in `half_spread = min(s0 + s1 * sigma, cap)`, so it is the FLOOR — what an arbitrarily
    /// small trade pays — and most of the price on the crypto pairs, since sigma at the quote's
    /// exposure window leaves the volatility term well under a bp.
    pub half_spread_bps: f64,
    /// Ladder width, in bps of the near price. This is the concentration knob: the price decays
    /// this far across the whole posted depth. It is an *upper* bound — the inverse solver
    /// narrows it further whenever the price bounds bind.
    pub width_bps: u16,
    /// How large a reference move `jump` refuses to absorb, in bps. Set it explicitly rather than
    /// leaning on the [`Self::half_spread_bps`] default: the posted half-spread is `s0 + s1 *
    /// sigma`, so a low `s0` does not imply low absorption, and a floor below the reference's own
    /// tick noise fires the detector on nothing. See `jump::Bounds::new`.
    #[serde(default)]
    pub jump_floor_bps: Option<u16>,
    /// Push at least this often, in milliseconds, measured from our own last send.
    ///
    /// The chain-derived heartbeat cannot go below a second, because the pool stores `updatedAt`
    /// as a uint32 of seconds and `quote_age_secs` is what it compares. This one is measured
    /// locally, so it can hold the posted quote near the interval the spread is priced for. Unset
    /// leaves only the second-resolution heartbeat. See `policy::Trigger::Cadence`.
    #[serde(default)]
    pub push_interval_ms_max: Option<u64>,
    /// **Target inventory, as a share of this pair's book, in percent.**
    ///
    /// A *share* rather than an amount, so it stays meaningful as the pool grows or shrinks. The
    /// book is this pair's base holdings valued at the reference plus its share of the quote
    /// balance — see [`crate::skew::Inventory`] for what that share means when several pairs draw
    /// bids from one quote token. `50` is balanced; lower prefers quote to base.
    pub target_base_share_pct: f64,
    /// The trade size the half-spread is guaranteed over, in human base units. The solver picks the
    /// widest ladder whose *average* price over `[0, capture]` hits the target, so this is the size
    /// the quote is honest about rather than the size it is limited to.
    pub capture: String,
    /// Base the pool will buy per epoch, in human base units.
    pub bid_capacity: String,
    /// Base the pool will sell per epoch, in human base units.
    pub ask_capacity: String,
    /// Re-post at least this often, in seconds, even with no price move. Must sit inside the
    /// pool's own `maxStaleSecs`, which is checked against the chain at startup.
    pub heartbeat_secs: u64,
    /// Drift threshold, in bps, when the market has moved **against** the posted quote: our bid is
    /// now above fair, or our ask below it. **This must be the TIGHTER of the two thresholds**,
    /// and `PairConfig::validate` refuses a config with them the other way round.
    ///
    /// That inverts the conventional arrangement, deliberately: the prop pool is the only maker of
    /// consequence on GIWA, so there is nobody to lose the flow to. A basis point too conservative
    /// costs a little volume; a basis point too generous after the market has moved is a free
    /// option written to whoever notices first. See [`crate::policy`].
    ///
    /// Fractional to one decimal place; [`Self::adverse_drift_decibps`] is what `policy` compares.
    pub adverse_drift_bps: f64,
    /// Drift threshold, in bps, when the posted quote has merely become conservative, which costs
    /// volume rather than money. **The LOOSER of the two** — see [`Self::adverse_drift_bps`] — and
    /// large enough that a quiet market does not churn gas. Same precision.
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
        (self.target_base_share_pct * 10_000.0)
            .round()
            .clamp(0.0, 1_000_000.0) as u32
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

    /// `half_spread_bps` in hundredths of a bp, the unit the ladder is built in.
    #[must_use]
    pub fn half_spread_bps_e2(&self) -> u32 {
        bps_e2(self.half_spread_bps)
    }

    /// Every per-pair check, in four groups run in the order written.
    fn validate(&self, venues_min: u8) -> Result<(), ConfigError> {
        self.validate_identity(venues_min)?;
        self.validate_pricing()?;
        self.validate_sizes()?;
        self.validate_triggers()
    }

    fn validate_identity(&self, venues_min: u8) -> Result<(), ConfigError> {
        let id = self.pair_id;
        if id == 0 {
            return Err(invalid(
                "pairs.pair_id: 0 is reserved and is never a real pair",
            ));
        }
        if self.symbol.is_empty() || !self.symbol.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return Err(invalid(format!(
                "pairs[{id}].symbol: must be alphanumeric, got `{}`",
                self.symbol
            )));
        }
        let venues = self.venues.count();
        let venues_min = self.venues_min.unwrap_or(venues_min);
        if venues_min == 0 {
            return Err(invalid(format!(
                "pairs[{id}].venues_min: 0 would mean a reference with no venue behind it"
            )));
        }
        if venues < usize::from(venues_min) {
            return Err(invalid(format!(
                "pairs[{id}].venues names {venues} venue(s) but the quorum is {venues_min}; \
                 this pair could never reach quorum and would never quote"
            )));
        }
        for (venue, symbol) in self.venues.iter() {
            if symbol.trim().is_empty() {
                return Err(invalid(format!(
                    "pairs[{id}].venues.{venue}: must not be empty"
                )));
            }
        }
        if let Some(pyth) = self.venues.pyth.as_deref() {
            if let Some(suffix) = crate::feed::pyth::DEPRECATED_SUFFIXES
                .iter()
                .find(|s| pyth.ends_with(**s))
            {
                return Err(invalid(format!(
                    "pairs[{id}].venues.pyth (`{pyth}`) names a `{suffix}` feed, which Pyth marks \
                     DEPRECATED FEED; it covers one session outside regular hours and is dark \
                     while the market is open. Drop the suffix"
                )));
            }
        }
        if self.base_decimals > 30 || self.quote_decimals > 30 {
            return Err(invalid(format!(
                "pairs[{id}]: token decimals must be <= 30"
            )));
        }
        Ok(())
    }

    fn validate_pricing(&self) -> Result<(), ConfigError> {
        let id = self.pair_id;
        let bps_max = dubu_core::ladder::BPS_MAX;
        if u128::from(self.half_spread_bps_e2()) > dubu_core::ladder::BPS_E2_MAX {
            return Err(invalid(format!(
                "pairs[{id}].half_spread_bps: must be <= {bps_max}"
            )));
        }
        if u128::from(self.width_bps) > bps_max {
            return Err(invalid(format!(
                "pairs[{id}].width_bps: must be <= {bps_max}"
            )));
        }
        if !self.target_base_share_pct.is_finite()
            || !(0.0..=100.0).contains(&self.target_base_share_pct)
        {
            return Err(invalid(format!(
                "pairs[{id}].target_base_share_pct: must be a finite value in [0, 100], got {}",
                self.target_base_share_pct
            )));
        }
        if self.half_spread_bps_e2() == 0 {
            return Err(invalid(format!(
                "pairs[{id}].half_spread_bps: must be non-zero; \
                 a zero spread quotes both sides at fair value"
            )));
        }
        Ok(())
    }

    fn validate_sizes(&self) -> Result<(), ConfigError> {
        let id = self.pair_id;
        let capture = self.capture_units()?;
        let bid_cap = self.bid_capacity_units()?;
        let ask_cap = self.ask_capacity_units()?;
        if capture == 0 {
            return Err(invalid(format!("pairs[{id}].capture: must be non-zero")));
        }
        if bid_cap == 0 || ask_cap == 0 {
            return Err(invalid(format!(
                "pairs[{id}]: bid_capacity and ask_capacity must be non-zero; \
                 zero capacity quotes nothing"
            )));
        }
        // `PropPool` holds capacity in a uint96.
        for (name, v) in [("bid_capacity", bid_cap), ("ask_capacity", ask_cap)] {
            if v > dubu_core::curve::AMOUNT_MAX {
                return Err(invalid(format!(
                    "pairs[{id}].{name}: exceeds the pool's uint96 capacity field"
                )));
            }
        }
        if capture > bid_cap.min(ask_cap) {
            return Err(invalid(format!(
                "pairs[{id}].capture ({capture}) exceeds a capacity ({}); \
                 the solver would clamp it and the posted guarantee \
                 would not be the configured one",
                bid_cap.min(ask_cap)
            )));
        }
        Ok(())
    }

    fn validate_triggers(&self) -> Result<(), ConfigError> {
        let id = self.pair_id;
        if self.heartbeat_secs == 0 {
            return Err(invalid(format!(
                "pairs[{id}].heartbeat_secs: must be non-zero"
            )));
        }
        if !self.adverse_drift_bps.is_finite() || !self.favourable_drift_bps.is_finite() {
            return Err(invalid(format!(
                "pairs[{id}]: drift thresholds must be finite"
            )));
        }
        if self.adverse_drift_bps <= 0.0 || self.favourable_drift_bps <= 0.0 {
            return Err(invalid(format!(
                "pairs[{id}]: drift thresholds must be non-zero"
            )));
        }
        // Both fields exist for this asymmetry, and the direction is load-bearing: the pool is the
        // only maker of consequence on GIWA, so a too-generous quote after an adverse move is a
        // free option to whoever notices it while a too-conservative one only costs volume.
        if self.adverse_drift_bps > self.favourable_drift_bps {
            return Err(invalid(format!(
                "pairs[{id}]: adverse_drift_bps ({}) must be <= favourable_drift_bps ({}); \
                 reacting more slowly to an adverse move than to a favourable one is backwards",
                self.adverse_drift_bps, self.favourable_drift_bps
            )));
        }
        if !(1..=100).contains(&self.capacity_divergence_pct) {
            return Err(invalid(format!(
                "pairs[{id}].capacity_divergence_pct: must be 1..=100"
            )));
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
venues_min = 2

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
        assert_eq!(
            cfg.pairs[0].capture_units().unwrap(),
            20_000_000_000_000_000_000
        );
        assert_eq!(cfg.pairs[0].target_base_share_ppm(), 500_000);
    }

    // --- The volatility-scaled spread and the jump withdrawal ---

    #[test]
    fn both_defences_default_to_on_in_a_config_written_before_they_existed() {
        // `good()` has no `[spread]` and no `[jump]` table, and absence must not mean off.
        let cfg = parse(good()).unwrap();
        assert!((cfg.spread.vol_coefficient - 0.5).abs() < 1e-9);
        assert_eq!(cfg.spread.half_spread_bps_max, 30);
        assert_eq!(cfg.spread.params().vol_coefficient_e2, 50);
        assert!(cfg.jump.enabled);
        assert!((cfg.jump.sigma_k - 6.0).abs() < 1e-9);
        assert_eq!(cfg.jump.cooloff_secs, 30);
        assert_eq!(cfg.jump.scope, crate::jump::Scope::Book);
        assert_eq!(cfg.jump.scan_interval_ms, 200);
        assert_eq!(cfg.jump.params(&cfg.skew).sigma_k_e2, 600);
        // The withdrawal fee is 100x the ordinary tip.
        assert_eq!(cfg.jump.withdraw_priority_fee_wei().unwrap(), 500_000_000);
        assert!(
            cfg.jump.withdraw_priority_fee_wei().unwrap() > cfg.tx.max_priority_fee_wei().unwrap()
        );
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
        // `spread::compute` floors at `s0`, so this config does no damage; it does something
        // other than what it says.
        let s = format!("{}\n[spread]\nhalf_spread_bps_max = 3\n", good());
        assert!(
            matches!(parse(&s), Err(ConfigError::Invalid(m)) if m.contains("can never narrow"))
        );
    }

    #[test]
    fn a_cooloff_shorter_than_a_block_is_refused() {
        // The withdrawal would not have confirmed before the resume was due: a flicker.
        let s = format!("{}\n[jump]\ncooloff_secs = 0\n", good());
        assert!(
            matches!(parse(&s), Err(ConfigError::Invalid(m)) if m.contains("below one block time"))
        );
    }

    #[test]
    fn a_fast_lane_slower_than_the_block_time_is_refused() {
        // Above the head cadence the fast lane detects nothing sooner than the cycle would.
        let s = format!("{}\n[jump]\nscan_interval_ms = 2000\n", good());
        assert!(
            matches!(parse(&s), Err(ConfigError::Invalid(m)) if m.contains("exceeds chain.block_time_ms"))
        );
    }

    #[test]
    fn the_volatility_term_can_be_switched_off_without_removing_it() {
        // `0` restores the constant half-spread exactly, so a simulator bisection can attribute
        // a change to this feature rather than another.
        let s = format!("{}\n[spread]\nvol_coefficient = 0.0\n", good());
        let cfg = parse(&s).unwrap();
        assert_eq!(cfg.spread.params().vol_coefficient_e2, 0);
        let sp = crate::spread::compute(5, 10_000, 0, &cfg.spread.params());
        assert_eq!(sp.half_spread_e2, 5);
    }

    #[test]
    fn the_jump_detector_shares_the_volatility_estimators_sampling_window() {
        // A hole in the reference has to be one number: the estimator re-anchors on it and the
        // detector trips on it.
        let cfg = parse(good()).unwrap();
        let p = cfg.jump.params(&cfg.skew);
        assert_eq!(p.sample_max.as_millis() as u64, cfg.skew.vol_max_sample_ms);
        assert_eq!(p.sample_min.as_millis() as u64, cfg.skew.vol_min_sample_ms);
    }

    // --- Venues and the quorum ---

    #[test]
    fn a_venue_is_enabled_by_a_pair_naming_it_and_by_nothing_else() {
        let cfg = parse(good()).unwrap();
        assert_eq!(
            cfg.venues(),
            vec![VenueId::Binance, VenueId::Okx, VenueId::Bybit]
        );
        assert!(
            !cfg.venues().contains(&VenueId::Coinbase),
            "an unnamed venue must not be connected to"
        );
        assert_eq!(
            cfg.venue_symbols(VenueId::Okx),
            vec![("ETH-USDT".to_string(), "ETHUSDT".to_string())]
        );
        assert!(cfg.venue_symbols(VenueId::Coinbase).is_empty());
    }

    #[test]
    fn each_venue_falls_back_to_its_public_endpoint() {
        let cfg = parse(good()).unwrap();
        assert_eq!(
            cfg.feed.urls.get(VenueId::Bybit),
            "wss://stream.bybit.com/v5/public/spot"
        );

        // The override table has to come after the scalar keys or TOML reads them as its own.
        let s = format!(
            "{}\n[feed.urls]\nbybit = \"wss://example.test/spot\"\n",
            good()
        );
        let cfg = parse(&s).unwrap();
        assert_eq!(cfg.feed.urls.get(VenueId::Bybit), "wss://example.test/spot");
        assert_eq!(
            cfg.feed.urls.get(VenueId::Okx),
            "wss://ws.okx.com:8443/ws/v5/public",
            "one override must not disturb the rest"
        );
    }

    /// The scheme check follows [`VenueId::transport`]. A blanket `wss://` rule would refuse the
    /// polled venue's only correct endpoint, and an `http(s)` one would let a websocket venue be
    /// pointed at an endpoint that can never subscribe.
    #[test]
    fn the_url_scheme_each_venue_is_held_to_is_the_one_it_speaks() {
        let cfg = parse(good()).unwrap();
        assert_eq!(
            cfg.feed.urls.get(VenueId::Pyth),
            "https://hermes.pyth.network"
        );

        let s = format!(
            "{}\n[feed.urls]\npyth = \"https://hermes.example.test\"\n",
            good()
        );
        assert_eq!(
            parse(&s).unwrap().feed.urls.get(VenueId::Pyth),
            "https://hermes.example.test"
        );

        let s = format!(
            "{}\n[feed.urls]\npyth = \"wss://hermes.example.test\"\n",
            good()
        );
        assert!(matches!(parse(&s), Err(ConfigError::Invalid(m)) if m.contains("http(s)")));
        let s = format!(
            "{}\n[feed.urls]\nbybit = \"https://example.test\"\n",
            good()
        );
        assert!(matches!(parse(&s), Err(ConfigError::Invalid(m)) if m.contains("ws(s)")));
    }

    /// Pyth's `.PRE` / `.ON` / `.POST` feeds are marked DEPRECATED FEED and cover the sessions the
    /// live feed does not, so one named here would be dark whenever the market is open.
    #[test]
    fn a_deprecated_pyth_session_feed_is_refused_at_startup() {
        let base = good().replace(
            r#"venues = { binance = "ETHUSDT", okx = "ETH-USDT", bybit = "ETHUSDT" }"#,
            r#"venues = { binance = "ETHUSDT", okx = "ETH-USDT", pyth = "PYTHSYM" }"#,
        );
        for bad in [
            "Equity.US.AAPL/USD.PRE",
            "Equity.US.AAPL/USD.ON",
            "Equity.US.AAPL/USD.POST",
        ] {
            let s = base.replace("PYTHSYM", bad);
            assert!(
                matches!(parse(&s), Err(ConfigError::Invalid(m)) if m.contains("DEPRECATED FEED")),
                "`{bad}` was accepted"
            );
        }
        let s = base.replace("PYTHSYM", "Equity.US.AAPL/USD");
        assert_eq!(
            parse(&s).unwrap().venue_symbols(VenueId::Pyth),
            vec![("Equity.US.AAPL/USD".to_string(), "ETHUSDT".to_string())]
        );
    }

    /// A venue polled less often than its own staleness window is stale between polls, which reads
    /// as a venue that is barely there rather than as the misconfiguration it is.
    #[test]
    fn a_poll_interval_past_the_staleness_window_is_refused() {
        let s = good().replace(
            "venues_min = 2",
            "venues_min = 2\nstale_after_ms = 5000\npoll_interval_ms = 6000",
        );
        assert!(
            matches!(parse(&s), Err(ConfigError::Invalid(m)) if m.contains("stale between polls"))
        );
        let s = good().replace(
            "venues_min = 2",
            "venues_min = 2\nstale_after_ms = 5000\npoll_interval_ms = 1000",
        );
        assert_eq!(parse(&s).unwrap().feed.poll_interval_ms, 1_000);
    }

    #[test]
    fn a_misspelled_venue_is_a_hard_error_and_not_one_venue_fewer() {
        // `bybbit` would leave the pair one venue short of the quorum it is written to have,
        // with nothing at run time saying so.
        let s = good().replace(r#"bybit = "ETHUSDT""#, r#"bybbit = "ETHUSDT""#);
        assert!(matches!(parse(&s), Err(ConfigError::Parse(_))));
    }

    #[test]
    fn a_pair_that_could_never_reach_quorum_is_refused_at_startup() {
        let s = good().replace(
            r#"venues = { binance = "ETHUSDT", okx = "ETH-USDT", bybit = "ETHUSDT" }"#,
            r#"venues = { binance = "ETHUSDT" }"#,
        );
        assert!(
            matches!(parse(&s), Err(ConfigError::Invalid(m)) if m.contains("could never reach quorum"))
        );
    }

    #[test]
    fn a_single_venue_quorum_is_refused() {
        // One venue is a single-source oracle, and no config value may reinstate it.
        let s = good().replace("venues_min = 2", "venues_min = 1");
        assert!(matches!(parse(&s), Err(ConfigError::Invalid(m)) if m.contains("at least 2")));
    }

    #[test]
    fn a_dispersion_limit_below_the_rejection_floor_is_refused() {
        // Otherwise the regime gate fires on the ordinary disagreement the floor tolerates.
        let s = good().replace(
            "venues_min = 2",
            "venues_min = 2\nmad_floor_bps = 30.0\ndispersion_bps_max = 25.0",
        );
        assert!(
            matches!(parse(&s), Err(ConfigError::Invalid(m)) if m.contains("gates before the filter runs"))
        );
    }

    #[test]
    fn the_mad_knobs_reach_the_filter_at_deci_bps_resolution() {
        let s = good().replace(
            "venues_min = 2",
            "venues_min = 3\nmad_k = 4.5\nmad_floor_bps = 2.5\ndispersion_bps_max = 25.0",
        );
        let p = parse(&s).unwrap().feed.mad_params();
        assert_eq!(p.venues_min, 3);
        assert_eq!(p.k_tenths, 45);
        assert_eq!(p.floor_decibps, 25);
        assert_eq!(p.dispersion_decibps_max, 250);
    }

    // --- Skew ---

    #[test]
    fn the_skew_section_defaults_to_the_documented_numbers() {
        let cfg = parse(good()).unwrap();
        let p = cfg.skew.params();
        assert_eq!(p.gamma_e2, 100_000, "gamma = 1000");
        assert_eq!(p.positive_bps_max, 30);
        assert_eq!(p.negative_bps_max, 10);
        let v = cfg.skew.vol_config();
        assert_eq!(v.tau_ms, 60_000);
        assert_eq!(
            v.horizon_secs, 300,
            "the same window as risk.bleed_window_secs"
        );
    }

    #[test]
    fn a_looser_cap_on_the_book_lifting_direction_is_refused() {
        // Lifting the book raises the pool's bid toward fair value, the pick-off direction, so
        // a looser cap there inverts the argument in `skew::compute`.
        let s = good().replace(
            "[skew]\n",
            "[skew]\npositive_bps_max = 10\nnegative_bps_max = 30\n",
        );
        assert!(
            matches!(parse(&s), Err(ConfigError::Invalid(m)) if m.contains("must be the tighter cap"))
        );
    }

    #[test]
    fn a_target_inventory_outside_the_book_is_refused() {
        for bad in ["-5", "150"] {
            let s = good().replace(
                "target_base_share_pct = 50",
                &format!("target_base_share_pct = {bad}"),
            );
            assert!(
                matches!(parse(&s), Err(ConfigError::Invalid(m)) if m.contains("target_base_share_pct"))
            );
        }
    }

    #[test]
    fn dry_run_is_the_default_and_needs_no_key() {
        let cfg = parse(good()).unwrap();
        assert!(
            !cfg.tx.transmit_allowed,
            "omitting transmit_allowed must mean dry run"
        );
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
        assert!(
            matches!(parse(&s), Err(ConfigError::Invalid(m)) if m.contains("NAME of an environment variable"))
        );
    }

    #[test]
    fn both_key_sources_at_once_is_refused() {
        let s = good().replace(
            "[tx]\n",
            "[tx]\nprivate_key_env = \"K\"\nprivate_key_file = \"/k\"\n",
        );
        assert!(matches!(parse(&s), Err(ConfigError::Invalid(m)) if m.contains("exactly one")));
    }

    #[test]
    fn an_unknown_field_is_a_hard_error_not_a_silent_default() {
        // Without `deny_unknown_fields` this typo quotes 5 bp while its author believes 50.
        let s = good().replace(
            "half_spread_bps = 5",
            "half_spred_bps = 50\nhalf_spread_bps = 5",
        );
        let err = parse(&s).unwrap_err();
        assert!(
            matches!(err, ConfigError::Parse(_)),
            "expected a parse error, got {err}"
        );
    }

    #[test]
    fn a_fallback_faster_than_the_block_time_is_refused() {
        // Below the block time the fallback fires between heads and becomes the primary driver.
        let s = good().replace(
            "fallback_poll_interval_ms = 2000",
            "fallback_poll_interval_ms = 500",
        );
        assert!(
            matches!(parse(&s), Err(ConfigError::Invalid(m)) if m.contains("silently become the primary driver"))
        );
    }

    #[test]
    fn an_http_ws_url_is_refused_because_it_would_never_subscribe() {
        // The HTTPS endpoint answers `notifications not supported`, so the loop would poll
        // forever while every log line said it was configured for heads.
        let s = good().replace(
            r#"ws_url = "wss://giwa-sepolia.nodit.io/TESTKEY""#,
            r#"ws_url = "https://giwa-sepolia.nodit.io/TESTKEY""#,
        );
        assert!(
            matches!(parse(&s), Err(ConfigError::Invalid(m)) if m.contains("notifications not supported"))
        );
    }

    #[test]
    fn a_one_block_watchdog_window_is_refused() {
        let s = good().replace(
            "fallback_poll_interval_ms = 2000",
            "fallback_poll_interval_ms = 2000\nhead_stale_blocks = 1",
        );
        assert!(matches!(parse(&s), Err(ConfigError::Invalid(m)) if m.contains("ordinary jitter")));
    }

    #[test]
    fn a_watchdog_window_beyond_the_halt_timer_is_refused() {
        // Otherwise the bot withdraws quotes with the watchdog never having said anything.
        let s = good().replace(
            "fallback_poll_interval_ms = 2000",
            "fallback_poll_interval_ms = 2000\nhead_stale_blocks = 900\nhalt_after_secs = 600",
        );
        assert!(
            matches!(parse(&s), Err(ConfigError::Invalid(m)) if m.contains("never fire before the halt"))
        );
    }

    #[test]
    fn the_watchdog_window_is_a_multiple_of_the_block_time() {
        let cfg = parse(good()).unwrap();
        assert_eq!(cfg.chain.block_time_ms, 1_000, "GIWA is a 1s chain");
        assert_eq!(
            cfg.chain.head_stale_after(),
            std::time::Duration::from_secs(10)
        );
    }

    #[test]
    fn reacting_slower_to_adverse_than_favourable_drift_is_refused() {
        let s = good().replace("adverse_drift_bps = 2", "adverse_drift_bps = 20");
        assert!(matches!(parse(&s), Err(ConfigError::Invalid(m)) if m.contains("backwards")));
    }

    #[test]
    fn a_loss_budget_below_the_bleed_limit_is_refused() {
        let s = good().replace("loss_budget = \"10000\"", "loss_budget = \"100\"");
        assert!(
            matches!(parse(&s), Err(ConfigError::Invalid(m)) if m.contains("bleed switch is then dead code"))
        );
    }

    #[test]
    fn a_zero_half_spread_is_refused() {
        let s = good().replace("half_spread_bps = 5", "half_spread_bps = 0");
        assert!(
            matches!(parse(&s), Err(ConfigError::Invalid(m)) if m.contains("quotes both sides at fair value"))
        );
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
        assert!(
            matches!(parse(&two), Err(ConfigError::Invalid(m)) if m.contains("pair_id 1 appears twice"))
        );
    }

    #[test]
    fn a_duplicate_symbol_on_two_pairs_is_refused() {
        // Distinct ids, same symbol: two rows driven by one price, at double the quote traffic.
        let second = pair_block().replace("pair_id = 1", "pair_id = 2");
        let two = format!("{}{second}", good());
        assert!(
            matches!(parse(&two), Err(ConfigError::Invalid(m)) if m.contains("`ETHUSDT` appears twice"))
        );
    }

    #[test]
    fn halting_before_widening_is_refused() {
        let s = good().replace(
            "fallback_poll_interval_ms = 2000",
            "fallback_poll_interval_ms = 2000\ndegraded_after_secs = 30\nhalt_after_secs = 20",
        );
        assert!(matches!(parse(&s), Err(ConfigError::Invalid(m)) if m.contains("must exceed")));
    }

    // --- The API key must not leak ---

    /// A **fake** key shaped like a real Nodit one, including the `~` and `-` a naive URL parser
    /// mishandles. Never a real key: this file is committed.
    const KEY: &str = "EXAMPLEexample0~ExampleKey-000000";
    const KEYED: &str = "https://giwa-sepolia.nodit.io/EXAMPLEexample0~ExampleKey-000000";

    #[test]
    fn a_keyed_url_is_redacted_by_every_formatter() {
        let u = EndpointUrl::resolve("chain.rpc_url", KEYED).unwrap();
        // The two ways a URL reaches a log: `%url` and `?url`.
        let displayed = format!("{u}");
        let debugged = format!("{u:?}");
        for rendered in [&displayed, &debugged] {
            assert!(
                !rendered.contains(KEY),
                "the API key reached a formatter: {rendered}"
            );
        }
        assert_eq!(displayed, "https://giwa-sepolia.nodit.io/***");
        // The host survives: a redaction hiding which endpoint failed is useless for diagnosis.
        assert!(displayed.contains("giwa-sepolia.nodit.io"));
        // The real value is still reachable, but only through the one accessor.
        assert_eq!(u.expose(), KEYED);
    }

    #[test]
    fn redaction_covers_query_strings_and_userinfo_too() {
        // Other providers put the key in a query parameter or in userinfo; neither is used here,
        // and both must be safe if someone points the config at one.
        let q = EndpointUrl::resolve("chain.rpc_url", "https://rpc.example.com/v1?apikey=SECRET")
            .unwrap();
        assert_eq!(q.to_string(), "https://rpc.example.com/***");
        let ui =
            EndpointUrl::resolve("chain.rpc_url", "https://user:SECRET@rpc.example.com").unwrap();
        assert_eq!(ui.to_string(), "https://***@rpc.example.com");
        for u in [&q, &ui] {
            assert!(!u.to_string().contains("SECRET"));
        }
        // A key-free URL is left legible.
        let plain =
            EndpointUrl::resolve("chain.rpc_url", "https://sepolia-rpc-flashblocks.giwa.io")
                .unwrap();
        assert_eq!(plain.to_string(), "https://sepolia-rpc-flashblocks.giwa.io");
    }

    #[test]
    fn a_url_template_expands_from_the_environment() {
        std::env::set_var("DUBU_TEST_KEY_OK", KEY);
        let u = EndpointUrl::resolve(
            "chain.ws_url",
            "wss://giwa-sepolia.nodit.io/${DUBU_TEST_KEY_OK}",
        )
        .unwrap();
        assert_eq!(u.expose(), format!("wss://giwa-sepolia.nodit.io/{KEY}"));
        assert_eq!(u.to_string(), "wss://giwa-sepolia.nodit.io/***");
        assert_eq!(u.scheme(), "wss");
        std::env::remove_var("DUBU_TEST_KEY_OK");
    }

    #[test]
    fn an_unset_variable_names_the_variable_and_never_the_value() {
        std::env::remove_var("DUBU_TEST_KEY_MISSING");
        let err = EndpointUrl::resolve("chain.rpc_url", "https://h/${DUBU_TEST_KEY_MISSING}")
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("DUBU_TEST_KEY_MISSING"),
            "must name the variable: {msg}"
        );
        assert!(msg.contains(".env"), "must say where to put it: {msg}");
    }

    #[test]
    fn an_empty_variable_is_refused_rather_than_expanded_to_nothing() {
        // A blank `NODIT_API_KEY=` yields a URL whose empty key segment 401s at the first
        // request, which is a far worse place to find out.
        std::env::set_var("DUBU_TEST_KEY_BLANK", "   ");
        let err =
            EndpointUrl::resolve("chain.rpc_url", "https://h/${DUBU_TEST_KEY_BLANK}").unwrap_err();
        assert!(err.to_string().contains("unset or empty"));
        std::env::remove_var("DUBU_TEST_KEY_BLANK");
    }

    #[test]
    fn the_config_carries_a_template_and_never_a_literal_key() {
        std::env::set_var("DUBU_TEST_NODIT", KEY);
        let s = good()
            .replace(
                "wss://giwa-sepolia.nodit.io/TESTKEY",
                "wss://giwa-sepolia.nodit.io/${DUBU_TEST_NODIT}",
            )
            .replace(
                "https://giwa-sepolia.nodit.io/TESTKEY",
                "https://giwa-sepolia.nodit.io/${DUBU_TEST_NODIT}",
            );
        let cfg = parse(&s).unwrap();
        assert!(cfg.chain.ws_url.expose().ends_with(KEY));
        // The shape a panic or a `{:?}` dump produces must not contain the key anywhere.
        assert!(
            !format!("{cfg:?}").contains(KEY),
            "the key survived a Debug dump of the config"
        );
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

        // C is already set deliberately, and the file must not silently win.
        std::env::set_var("DUBU_TEST_DOTENV_C", "from_env");
        std::env::remove_var("DUBU_TEST_DOTENV_A");
        std::env::remove_var("DUBU_TEST_DOTENV_B");

        assert_eq!(load_dotenv(&path), 2, "only the two unset variables");
        assert_eq!(std::env::var("DUBU_TEST_DOTENV_A").unwrap(), "from_file");
        assert_eq!(
            std::env::var("DUBU_TEST_DOTENV_B").unwrap(),
            "quoted",
            "one layer of quotes is stripped"
        );
        assert_eq!(std::env::var("DUBU_TEST_DOTENV_C").unwrap(), "from_env");

        // A missing file is not an error: production sets real variables and has no `.env`.
        assert_eq!(load_dotenv(&dir.join("nope.env")), 0);

        for k in [
            "DUBU_TEST_DOTENV_A",
            "DUBU_TEST_DOTENV_B",
            "DUBU_TEST_DOTENV_C",
        ] {
            std::env::remove_var(k);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
