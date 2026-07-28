//! Hyperliquid, read-only: prices for markets Binance does not carry, and a paper book.
//!
//! # Why this venue
//!
//! Binance USD-M covers the crypto pairs. It does not carry equities, and equities are where the
//! pool has no way to lay off what it takes on. Hyperliquid does, through HIP-3 -- builder-deployed
//! markets that settle on its shared order book. Measured on the `xyz` dex: AAPL, TSLA, SKHY and
//! SPCX all quote, alongside 99 others.
//!
//! # Why paper
//!
//! Those markets exist on **mainnet only**. The testnet's HIP-3 list is 243 entries of `test`,
//! `scam` and `TOON` -- experiments, not equities. So hedging an equity for real means real capital
//! against a pool holding mock tokens, which is the incoherent arrangement `binance` documents:
//! drain the mock pool and the only real position left is unbacked.
//!
//! Paper mode takes the same decision, records the same book, and does not send the order. What it
//! proves is everything except the fill: the band, the net exposure, the limits, and the cost the
//! hedge *would* have paid, measured against the venue's actual depth rather than assumed.
//!
//! # Prices are cheap here, unlike everywhere else today
//!
//! `allMids` is weight 2 against a per-IP budget of 1200 a minute, and returns **every** market in
//! one response -- 939 on the main perp book, 103 more per equity dex. Polling once a second is
//! 240 weight, 20% of budget, for all five crypto pairs and every equity at once. After a day spent
//! exhausting seven RPC keys, that is worth stating plainly.
//!
//! # One market, several prices
//!
//! HIP-3 lets each builder attach its own oracle, and they disagree: measured in the same second,
//! `xyz:TSLA` 307.31, `flx:TSLA` 395.50, `cash:TSLA` 400.21. Thirty percent apart. So the dex is
//! part of the market's identity, not a routing detail -- picking one is picking a fair value, and
//! [`Market::dex`] makes that choice explicit rather than implied.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use tracing::{info, warn};

use super::Side;

/// A market on this venue: which builder's book, and which symbol on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Market {
    /// The HIP-3 builder, e.g. `xyz`. Empty for the main perp book.
    ///
    /// Not a routing detail. Builders attach their own oracles and disagree by tens of percent, so
    /// this is part of which price the pool is quoting against.
    pub dex: String,
    /// The venue's symbol, e.g. `xyz:TSLA` or `ETH`.
    pub symbol: String,
}

/// What a would-be hedge cost, and what it did to the book.
#[derive(Debug, Clone, PartialEq)]
pub struct PaperFill {
    /// The market it was written against.
    pub symbol: String,
    /// Direction.
    pub side: Side,
    /// Size, in the venue's own units.
    pub qty: f64,
    /// The mid at the moment it was written.
    pub mid: f64,
    /// Net position after this fill, signed. Positive is long.
    pub position: f64,
}

/// Anything that can go wrong reading this venue.
#[derive(Debug, thiserror::Error)]
pub enum VenueError {
    /// The request never completed.
    #[error("hyperliquid transport: {0}")]
    Transport(String),
    /// The response did not look like what the endpoint documents.
    #[error("hyperliquid response: {0}")]
    Decode(String),
    /// A price was asked for before any poll had landed, or for a symbol the venue does not carry.
    #[error("no price for {0}")]
    NoPrice(String),
}

/// Read-only client plus the paper book it writes into.
#[derive(Debug)]
pub struct Paper {
    base: String,
    http: reqwest::Client,
    /// Latest mid per symbol, across every dex polled.
    mids: BTreeMap<String, f64>,
    /// Signed position per symbol. Positive is long.
    positions: BTreeMap<String, f64>,
    /// Every paper fill, newest last. Bounded so a long run cannot grow without limit.
    fills: Vec<PaperFill>,
    last_poll: Option<Instant>,
    polls: u64,
    failures: u64,
}

/// How many paper fills to keep. Enough to reconstruct a session; not enough to matter for memory.
const FILL_CAPACITY: usize = 4096;

impl Paper {
    /// Build against `base`, e.g. `https://api.hyperliquid.xyz`.
    ///
    /// # Errors
    /// [`VenueError::Transport`] if the HTTP client cannot be constructed.
    pub fn new(base: impl Into<String>, timeout: Duration) -> Result<Self, VenueError> {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| VenueError::Transport(e.to_string()))?;
        Ok(Self {
            base: base.into(),
            http,
            mids: BTreeMap::new(),
            positions: BTreeMap::new(),
            fills: Vec::new(),
            last_poll: None,
            polls: 0,
            failures: 0,
        })
    }

    /// Refresh every mid on one book.
    ///
    /// `dex` empty polls the main perp book; a builder name polls that builder's. One call returns
    /// the whole book at weight 2, so this is called once per dex rather than once per symbol --
    /// per-symbol polling would be the same information at fifty times the cost.
    ///
    /// # Errors
    /// [`VenueError`].
    pub async fn poll_mids(&mut self, dex: &str) -> Result<usize, VenueError> {
        let body = if dex.is_empty() {
            serde_json::json!({ "type": "allMids" })
        } else {
            serde_json::json!({ "type": "allMids", "dex": dex })
        };
        let url = format!("{}/info", self.base);
        let resp = self.http.post(&url).json(&body).send().await.map_err(|e| {
            self.failures = self.failures.saturating_add(1);
            VenueError::Transport(e.to_string())
        })?;
        let v: serde_json::Value = resp.json().await.map_err(|e| {
            self.failures = self.failures.saturating_add(1);
            VenueError::Decode(e.to_string())
        })?;
        let Some(map) = v.as_object() else {
            self.failures = self.failures.saturating_add(1);
            return Err(VenueError::Decode("allMids was not an object".into()));
        };

        let mut n = 0;
        for (k, val) in map {
            // Prices arrive as decimal strings. Parsing rather than reading a float keeps the
            // venue's own precision instead of whatever a JSON float round-trip leaves.
            if let Some(px) = val.as_str().and_then(|s| s.parse::<f64>().ok()) {
                if px > 0.0 {
                    self.mids.insert(k.clone(), px);
                    n += 1;
                }
            }
        }
        self.last_poll = Some(Instant::now());
        self.polls = self.polls.saturating_add(1);
        Ok(n)
    }

    /// The latest mid for a symbol.
    ///
    /// # Errors
    /// [`VenueError::NoPrice`] if nothing has been polled for it.
    pub fn mid(&self, symbol: &str) -> Result<f64, VenueError> {
        self.mids
            .get(symbol)
            .copied()
            .ok_or_else(|| VenueError::NoPrice(symbol.to_string()))
    }

    /// How stale the book is, or `None` before the first poll.
    #[must_use]
    pub fn age(&self, now: Instant) -> Option<Duration> {
        self.last_poll.map(|t| now.saturating_duration_since(t))
    }

    /// Successful polls, and failed ones.
    #[must_use]
    pub const fn counters(&self) -> (u64, u64) {
        (self.polls, self.failures)
    }

    /// Net position on a symbol. Positive is long.
    #[must_use]
    pub fn position(&self, symbol: &str) -> f64 {
        self.positions.get(symbol).copied().unwrap_or(0.0)
    }

    /// Every symbol currently carrying a position.
    #[must_use]
    pub fn open_positions(&self) -> Vec<(String, f64)> {
        self.positions
            .iter()
            .filter(|(_, v)| v.abs() > f64::EPSILON)
            .map(|(k, v)| (k.clone(), *v))
            .collect()
    }

    /// Paper fills recorded, newest last.
    #[must_use]
    pub fn fills(&self) -> &[PaperFill] {
        &self.fills
    }

    /// Write a hedge into the book without sending it.
    ///
    /// Refuses without a price rather than assuming one: a paper book filled at a made-up mid
    /// reports a cost that never existed, which is worse than reporting nothing. The caller sees
    /// the error and leaves the drift outstanding, exactly as it would for a rejected live order.
    ///
    /// # Errors
    /// [`VenueError::NoPrice`] if the symbol has no polled mid.
    pub fn write(&mut self, symbol: &str, side: Side, qty: f64) -> Result<PaperFill, VenueError> {
        let mid = self.mid(symbol)?;
        let signed = match side {
            Side::Buy => qty,
            Side::Sell => -qty,
        };
        let position = self
            .positions
            .entry(symbol.to_string())
            .and_modify(|p| *p += signed)
            .or_insert(signed);
        let fill = PaperFill {
            symbol: symbol.to_string(),
            side,
            qty,
            mid,
            position: *position,
        };
        self.fills.push(fill.clone());
        if self.fills.len() > FILL_CAPACITY {
            self.fills.remove(0);
        }
        info!(
            target: "hedge", event = "paper_fill", symbol, side = side.as_str(), qty, mid,
            position = fill.position,
            "hedge written to the paper book; no order was sent"
        );
        Ok(fill)
    }

    /// Warn once if the book has gone stale enough that a paper fill would be priced off old data.
    pub fn warn_if_stale(&self, now: Instant, limit: Duration) {
        if let Some(age) = self.age(now) {
            if age > limit {
                warn!(
                    target: "hedge", event = "paper_stale", age_ms = age.as_millis() as u64,
                    limit_ms = limit.as_millis() as u64, polls = self.polls, failures = self.failures,
                    "the paper book is stale; hedges written now are priced off old mids"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book() -> Paper {
        let mut p = Paper::new("http://unused", Duration::from_secs(1)).expect("client");
        p.mids.insert("ETH".into(), 1917.95);
        p.mids.insert("xyz:TSLA".into(), 307.305);
        p.mids.insert("xyz:SKHY".into(), 132.77);
        p.last_poll = Some(Instant::now());
        p
    }

    /// A sell leaves a short and a buy closes it, which is the whole arithmetic the band relies on.
    #[test]
    fn the_paper_book_tracks_a_signed_position() {
        let mut p = book();
        p.write("ETH", Side::Sell, 40.0).expect("priced");
        assert!((p.position("ETH") + 40.0).abs() < 1e-9, "short 40");

        p.write("ETH", Side::Buy, 15.0).expect("priced");
        assert!(
            (p.position("ETH") + 25.0).abs() < 1e-9,
            "short 25 after buying 15 back"
        );

        p.write("ETH", Side::Buy, 25.0).expect("priced");
        assert!(p.position("ETH").abs() < 1e-9, "flat");
        assert!(p.open_positions().is_empty(), "and nothing left open");
    }

    /// Refusing without a price is the point. A paper fill at an invented mid reports a cost that
    /// never existed, and the caller would settle the drift against it as though it were real.
    #[test]
    fn a_symbol_with_no_price_is_refused_rather_than_guessed() {
        let mut p = book();
        let err = p.write("xyz:NOSUCH", Side::Sell, 1.0).expect_err("no mid");
        assert!(matches!(err, VenueError::NoPrice(s) if s == "xyz:NOSUCH"));
        assert_eq!(p.fills().len(), 0, "and nothing was recorded");
    }

    /// Equities are why this venue exists: Binance carries none of these.
    #[test]
    fn equity_markets_price_like_any_other() {
        let mut p = book();
        let f = p.write("xyz:TSLA", Side::Sell, 100.0).expect("priced");
        assert!((f.mid - 307.305).abs() < 1e-9);
        assert!((p.position("xyz:TSLA") + 100.0).abs() < 1e-9);

        // SK Hynix, on the same book. Binance has no equivalent at any price.
        p.write("xyz:SKHY", Side::Buy, 50.0).expect("priced");
        assert!((p.position("xyz:SKHY") - 50.0).abs() < 1e-9);
    }

    /// The dex is part of the identity, not a routing detail: measured in one second, `xyz:TSLA`
    /// was 307.31 while `flx:TSLA` was 395.50. Treating them as one market would price the pool
    /// against a fair value it never chose.
    #[test]
    fn the_same_ticker_on_two_dexs_is_two_markets() {
        let mut p = book();
        p.mids.insert("flx:TSLA".into(), 395.50);

        p.write("xyz:TSLA", Side::Sell, 10.0).expect("priced");
        p.write("flx:TSLA", Side::Sell, 10.0).expect("priced");

        assert_eq!(p.open_positions().len(), 2, "separate books");
        let mids: Vec<f64> = p.fills().iter().map(|f| f.mid).collect();
        assert!(
            (mids[0] - mids[1]).abs() > 80.0,
            "and they disagree by tens of percent: {mids:?}"
        );
    }

    /// The ring buffer must bound memory without losing the newest.
    #[test]
    fn the_fill_log_is_bounded_and_keeps_the_newest() {
        let mut p = book();
        for _ in 0..(FILL_CAPACITY + 10) {
            p.write("ETH", Side::Sell, 1.0).expect("priced");
        }
        assert_eq!(p.fills().len(), FILL_CAPACITY);
        let last = p.fills().last().expect("non-empty");
        assert!(
            (last.position + (FILL_CAPACITY + 10) as f64).abs() < 1e-6,
            "the position is cumulative even though the log is not"
        );
    }
}
