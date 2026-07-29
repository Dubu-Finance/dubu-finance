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
    /// What it filled at, after crossing the book and paying the taker fee.
    pub price: f64,
    /// What that cost against the mid, in hundredths of a basis point. Always positive: crossing is
    /// never free, and a paper book that says it is reports a hedge cheaper than any real one.
    pub cost_bps_e2: u32,
    /// Net position after this fill, signed. Positive is long.
    pub position: f64,
}

/// One market as the paper book sees it.
///
/// The curves are what make a paper fill honest about size. A hedge clip is routinely larger than
/// the top of book -- 25 ETH is $47,700 against $37,845 resting, and 20,000 XRP is nearly six times
/// the best bid -- so filling at mid understates the cost of every crossing the pool actually makes.
#[derive(Debug, Clone, Default)]
pub struct Quote {
    /// Mid price.
    pub mid: f64,
    /// Bid side, best first, as a cumulative concession curve. Empty for a mid-only source.
    pub bids: Vec<super::binance::DepthPoint>,
    /// Ask side, same.
    pub asks: Vec<super::binance::DepthPoint>,
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
    /// Public Binance REST, for the pairs that price against spot.
    binance_base: String,
    /// Taker fee charged on every paper fill, in hundredths of a basis point.
    fee_bps_e2: u32,
    /// Latest quote per symbol, across every book polled.
    books: BTreeMap<String, Quote>,
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
            binance_base: "https://api.binance.com".to_string(),
            fee_bps_e2: 0,
            books: BTreeMap::new(),
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
                    // Mid-only: `allMids` carries no depth, so a fill here pays the fee and
                    // nothing for size. Honest and optimistic, and the equity pairs know it.
                    self.books.insert(
                        k.clone(),
                        Quote {
                            mid: px,
                            ..Quote::default()
                        },
                    );
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
        self.books
            .get(symbol)
            .map(|q| q.mid)
            .ok_or_else(|| VenueError::NoPrice(symbol.to_string()))
    }

    /// Charge this taker fee on every paper fill, in hundredths of a basis point.
    pub const fn charge(&mut self, fee_bps_e2: u32) {
        self.fee_bps_e2 = fee_bps_e2;
    }

    /// Point the spot poller somewhere else. Defaults to public Binance.
    pub fn spot_base(&mut self, base: impl Into<String>) {
        self.binance_base = base.into();
    }

    /// Refresh the order book for each symbol from public Binance spot.
    ///
    /// Unsigned, so no key and no clock: `/api/v3/depth` is public. One request per symbol, which is
    /// the cost of having both sides at depth -- `allMids` gives a number, and a number cannot say
    /// what a 25 ETH clip pays.
    ///
    /// A symbol that fails keeps its previous quote rather than being dropped: a stale book prices a
    /// hedge slightly wrong, an absent one refuses it entirely and leaves the exposure standing.
    ///
    /// # Errors
    /// Never returns `Err`; per-symbol failures are counted. Returns how many books were refreshed.
    pub async fn poll_books(&mut self, symbols: &[String]) -> usize {
        let mut n = 0;
        for symbol in symbols {
            let url = format!(
                "{}/api/v3/depth?symbol={symbol}&limit=100",
                self.binance_base
            );
            let Ok(resp) = self.http.get(&url).send().await else {
                self.failures = self.failures.saturating_add(1);
                continue;
            };
            let Ok(v) = resp.json::<serde_json::Value>().await else {
                self.failures = self.failures.saturating_add(1);
                continue;
            };
            let side = |k: &str| -> Vec<(f64, f64)> {
                v.get(k)
                    .and_then(|l| l.as_array())
                    .map(|l| {
                        l.iter()
                            .filter_map(|e| {
                                let px = e.get(0)?.as_str()?.parse::<f64>().ok()?;
                                let sz = e.get(1)?.as_str()?.parse::<f64>().ok()?;
                                Some((px, sz))
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            };
            let (bids, asks) = (side("bids"), side("asks"));
            let (Some(&(best_bid, _)), Some(&(best_ask, _))) = (bids.first(), asks.first()) else {
                self.failures = self.failures.saturating_add(1);
                continue;
            };
            let mid = (best_bid + best_ask) / 2.0;
            let depth = |l: &[(f64, f64)]| {
                let cum: f64 = l.iter().map(|&(_, s)| s).sum();
                super::binance::depth_curve(l, mid, cum)
            };
            self.books.insert(
                symbol.clone(),
                Quote {
                    mid,
                    bids: depth(&bids),
                    asks: depth(&asks),
                },
            );
            n += 1;
        }
        self.last_poll = Some(Instant::now());
        self.polls = self.polls.saturating_add(1);
        n
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
    /// Priced by walking the book, not filled at mid. A hedge clip is routinely larger than the top
    /// of book, so a mid fill quietly reports a hedge cheaper than any that could be executed --
    /// which is exactly the number the whole exercise exists to measure. A mid-only source charges
    /// the fee and nothing for size, which is optimistic and recorded as such in `cost_bps_e2`.
    ///
    /// # Errors
    /// [`VenueError::NoPrice`] if the symbol has no polled quote.
    pub fn write(&mut self, symbol: &str, side: Side, qty: f64) -> Result<PaperFill, VenueError> {
        let quote = self
            .books
            .get(symbol)
            .cloned()
            .ok_or_else(|| VenueError::NoPrice(symbol.to_string()))?;
        let mid = quote.mid;
        // Buying walks the asks, selling walks the bids. Using one curve for both would charge a
        // thin side's concession on the deep side and vice versa.
        let curve = match side {
            Side::Buy => &quote.asks,
            Side::Sell => &quote.bids,
        };
        let concession = super::binance::concession_for(curve, qty).unwrap_or(0);
        let cost_bps_e2 = concession.saturating_add(self.fee_bps_e2);
        let cost = f64::from(cost_bps_e2) / 1_000_000.0;
        // Always against us. A crossing pays in whichever direction it goes.
        let price = match side {
            Side::Buy => mid * (1.0 + cost),
            Side::Sell => mid * (1.0 - cost),
        };
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
            price,
            cost_bps_e2,
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

    fn mid_only(mid: f64) -> Quote {
        Quote {
            mid,
            ..Quote::default()
        }
    }

    fn book() -> Paper {
        let mut p = Paper::new("http://unused", Duration::from_secs(1)).expect("client");
        p.books.insert("ETH".into(), mid_only(1917.95));
        p.books.insert("xyz:TSLA".into(), mid_only(307.305));
        p.books.insert("xyz:SKHY".into(), mid_only(132.77));
        p.last_poll = Some(Instant::now());
        p
    }

    /// A clip larger than the top of book pays for the levels it eats. Filling at mid reports a
    /// hedge nobody could execute, and at these sizes that is the number the exercise is measuring.
    #[test]
    fn a_clip_past_the_top_of_book_pays_for_the_depth_it_eats() {
        let mut p = Paper::new("http://unused", Duration::from_secs(1)).expect("client");
        p.charge(400);
        // Asks 2 bp, 6 bp and 14 bp from a mid of 100, ten units at each.
        p.books.insert(
            "X".into(),
            Quote {
                mid: 100.0,
                bids: super::super::binance::depth_curve(
                    &[(99.98, 10.0), (99.94, 10.0), (99.86, 10.0)],
                    100.0,
                    30.0,
                ),
                asks: super::super::binance::depth_curve(
                    &[(100.02, 10.0), (100.06, 10.0), (100.14, 10.0)],
                    100.0,
                    30.0,
                ),
            },
        );
        p.last_poll = Some(Instant::now());

        let small = p.write("X", Side::Buy, 5.0).expect("priced");
        let large = p.write("X", Side::Buy, 25.0).expect("priced");
        assert!(
            large.cost_bps_e2 > small.cost_bps_e2,
            "25 units walks past the top of book and 5 does not"
        );
        assert!(
            small.cost_bps_e2 >= 400,
            "even the smallest fill pays the taker fee"
        );
        assert!(large.price > large.mid, "a buy always fills above mid");

        let sold = p.write("X", Side::Sell, 25.0).expect("priced");
        assert!(sold.price < sold.mid, "and a sell below it");
    }

    /// A mid-only source has no depth to charge for, so it charges the fee and says so. Optimistic
    /// and legible, rather than silently free.
    #[test]
    fn a_mid_only_source_still_pays_the_fee() {
        let mut p = book();
        p.charge(400);
        let f = p.write("ETH", Side::Sell, 1_000_000.0).expect("priced");
        assert_eq!(f.cost_bps_e2, 400, "no book, so nothing but the fee");
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
        p.books.insert("flx:TSLA".into(), mid_only(395.50));

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
