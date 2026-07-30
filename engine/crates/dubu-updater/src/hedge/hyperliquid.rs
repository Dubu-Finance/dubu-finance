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
//!
//! # The fetching is not on the quote loop, and the split is not cosmetic
//!
//! This used to be polled from inside `run_hedge`, which `run_cycle` called, and it was **51% of the
//! cycle**: six sequential external HTTPS round trips every pass -- five
//! `api.binance.com/api/v3/depth` calls at 1.274s together plus one `api.hyperliquid.xyz/info`
//! `allMids` at 0.25s -- for a median 1.373s against a 2.695s cycle whose per-pair compute finished
//! in under a millisecond.
//!
//! Two things made that pure waste rather than a cost worth paying. The calls were **sequential**
//! when nothing orders them: no book's price depends on another's, so the whole 1.373s was six
//! copies of one wait. And they poll a **paper** book -- nothing in the quote path reads a mid from
//! here synchronously, so more than half of the price-setting loop was spent on work that sets no
//! price.
//!
//! Both halves therefore moved, and they are separate fixes:
//!
//! * [`Paper::poll_all`] issues all six fetches through one `FuturesUnordered`, so a pass costs one
//!   round trip rather than six.
//! * `main.rs` runs that pass from a spawned hedge task instead of from the cycle, publishing the
//!   position book the cycle actually reads. `feed::ws::run` and `chain::view::run` are the same
//!   shape, and the measurement is why: post-restart the cycle was 1.657s, of which the last-send to
//!   next-cycle gap -- this poll, and nothing else -- was 1.377s.
//!
//! What the cycle gives up is up to one hedge interval of staleness on a paper mid. That is the same
//! trade `chain::view` documents and it is a smaller one here, because under the old arrangement the
//! books were already up to a full cycle old. [`Paper::warn_if_stale`] is unchanged and is still the
//! backstop that says when the staleness has stopped being acceptable.

use std::collections::BTreeMap;
use std::pin::Pin;
use std::time::{Duration, Instant};

use futures_util::stream::{FuturesUnordered, StreamExt};

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

/// What one concurrent pass of [`Paper::poll_all`] refreshed, and what it could not.
#[derive(Debug, Default)]
pub struct PollReport {
    /// Spot books refreshed, with both sides at depth.
    pub books: usize,
    /// Mids refreshed, summed across every dex polled.
    pub mids: usize,
    /// Per-dex failures, named rather than counted. Which book kept its previous mids is the
    /// operator's question, because every paper fill on that book is now priced off them.
    pub dex_failures: Vec<(String, VenueError)>,
}

/// One boxed in-flight fetch.
///
/// Erased because the two request shapes are two distinct opaque future types and a single
/// `FuturesUnordered` cannot hold both otherwise — and holding all six at once is the entire point.
/// Six allocations against six round trips is not a trade worth thinking about.
type Fetch = Pin<Box<dyn std::future::Future<Output = Fetched> + Send>>;

/// One in-flight fetch's result, tagged with what it was.
enum Fetched {
    /// A Binance spot depth response for one symbol. `None` if the request or the decode failed.
    Depth {
        symbol: String,
        body: Option<serde_json::Value>,
    },
    /// A Hyperliquid `allMids` response for one dex.
    Mids {
        dex: String,
        body: Result<serde_json::Value, VenueError>,
    },
}

/// Turn a Binance `/api/v3/depth` body into a quote with both concession curves.
///
/// `None` rather than an error for anything malformed, because the caller's response to every such
/// case is identical: keep the previous book and count a failure.
fn depth_quote(v: &serde_json::Value) -> Option<Quote> {
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
    let (&(best_bid, _), &(best_ask, _)) = (bids.first()?, asks.first()?);
    let mid = (best_bid + best_ask) / 2.0;
    let depth = |l: &[(f64, f64)]| {
        let cum: f64 = l.iter().map(|&(_, s)| s).sum();
        super::binance::depth_curve(l, mid, cum)
    };
    Some(Quote {
        mid,
        bids: depth(&bids),
        asks: depth(&asks),
    })
}

/// Pull every positive mid out of an `allMids` body.
///
/// Prices arrive as decimal strings. Parsing them rather than reading a JSON float keeps the venue's
/// own precision instead of whatever a float round-trip leaves behind.
///
/// # Errors
/// [`VenueError::Decode`] if the body is not the object the endpoint documents.
fn mids_from(v: &serde_json::Value) -> Result<Vec<(String, f64)>, VenueError> {
    let map = v
        .as_object()
        .ok_or_else(|| VenueError::Decode("allMids was not an object".into()))?;
    Ok(map
        .iter()
        .filter_map(|(k, val)| {
            let px = val.as_str()?.parse::<f64>().ok()?;
            (px > 0.0).then(|| (k.clone(), px))
        })
        .collect())
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

    /// Refresh every book this leg uses -- spot depth and per-dex mids -- in one concurrent pass.
    ///
    /// **One `FuturesUnordered` over all of it**, because nothing here orders the requests: no book's
    /// price depends on another's. Sequential, the six were 1.373s of a 2.695s cycle spent on six
    /// copies of the same wait; concurrent, a pass costs one round trip.
    ///
    /// Spot is polled once per symbol and mids once per dex, and that asymmetry is deliberate rather
    /// than an oversight. `allMids` returns a whole book at weight 2, so polling it per symbol would
    /// be the same information at fifty times the cost. `/api/v3/depth` is the other way round: it is
    /// the only call that carries both sides at depth, and depth is what prices a clip larger than the
    /// top of book -- `allMids` gives a number, and a number cannot say what a 25 ETH clip pays.
    ///
    /// A symbol or a book that fails **keeps its previous quote** rather than being dropped. A stale
    /// book prices a hedge slightly wrong; an absent one refuses it entirely and leaves the exposure
    /// standing, which is the worse of the two by a wide margin.
    pub async fn poll_all(&mut self, spot: &[String], dexs: &[String]) -> PollReport {
        let mut report = PollReport::default();
        if spot.is_empty() && dexs.is_empty() {
            return report;
        }

        // Each future owns its payload and borrows nothing: `reqwest::Client` is an `Arc` inside, so
        // cloning it is a refcount bump rather than a second connection pool.
        let mut flight: FuturesUnordered<Fetch> = FuturesUnordered::new();

        for symbol in spot {
            let http = self.http.clone();
            let symbol = symbol.clone();
            let url = format!(
                "{}/api/v3/depth?symbol={symbol}&limit=100",
                self.binance_base
            );
            flight.push(Box::pin(async move {
                // Unsigned, so no key and no clock: `/api/v3/depth` is public.
                let body = match http.get(&url).send().await {
                    Ok(r) => r.json::<serde_json::Value>().await.ok(),
                    Err(_) => None,
                };
                Fetched::Depth { symbol, body }
            }));
        }

        for dex in dexs {
            let http = self.http.clone();
            let dex = dex.clone();
            let url = format!("{}/info", self.base);
            // An empty `dex` polls the main perp book; a builder name polls that builder's.
            let request = if dex.is_empty() {
                serde_json::json!({ "type": "allMids" })
            } else {
                serde_json::json!({ "type": "allMids", "dex": dex })
            };
            flight.push(Box::pin(async move {
                let body = match http.post(&url).json(&request).send().await {
                    Ok(r) => r
                        .json::<serde_json::Value>()
                        .await
                        .map_err(|e| VenueError::Decode(e.to_string())),
                    Err(e) => Err(VenueError::Transport(e.to_string())),
                };
                Fetched::Mids { dex, body }
            }));
        }

        while let Some(fetched) = flight.next().await {
            match fetched {
                Fetched::Depth { symbol, body } => match body.as_ref().and_then(depth_quote) {
                    Some(q) => {
                        self.books.insert(symbol, q);
                        report.books += 1;
                    }
                    None => self.failures = self.failures.saturating_add(1),
                },
                Fetched::Mids { dex, body } => match body.and_then(|v| mids_from(&v)) {
                    Ok(mids) => {
                        for (symbol, px) in mids {
                            // Mid-only: `allMids` carries no depth, so a fill here pays the fee and
                            // nothing for size. Honest and optimistic, and the equity pairs know it.
                            self.books.insert(
                                symbol,
                                Quote {
                                    mid: px,
                                    ..Quote::default()
                                },
                            );
                            report.mids += 1;
                        }
                    }
                    Err(e) => {
                        self.failures = self.failures.saturating_add(1);
                        report.dex_failures.push((dex, e));
                    }
                },
            }
        }

        self.last_poll = Some(Instant::now());
        self.polls = self.polls.saturating_add(1);
        report
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

    /// The depth body decodes to a mid and two curves, and it does so off the wire format Binance
    /// actually sends: prices and sizes are decimal **strings**, not JSON numbers.
    ///
    /// Worth a test of its own now that it is a function rather than a closure inside the poll loop.
    /// It is the one piece of that loop with no I/O in it, so it is the one piece that can be checked
    /// without pretending to be a venue.
    #[test]
    fn a_depth_body_decodes_to_a_mid_and_two_curves() {
        let body = serde_json::json!({
            "bids": [["99.00", "10"], ["98.00", "20"]],
            "asks": [["101.00", "10"], ["102.00", "20"]],
        });
        let q = depth_quote(&body).expect("well formed");
        assert!(
            (q.mid - 100.0).abs() < 1e-9,
            "mid is between the best of each side"
        );
        assert_eq!(q.bids.len(), 2);
        assert_eq!(q.asks.len(), 2);
        // Cumulative, so the far level carries both.
        assert!((q.bids[1].cumulative - 30.0).abs() < 1e-9);
    }

    /// One side empty means no mid, and no mid means no quote at all.
    ///
    /// Filling against a half-book would price a crossing off a number the venue never quoted. The
    /// caller keeps the previous book instead, which is stale but real.
    #[test]
    fn a_depth_body_missing_a_side_yields_no_quote() {
        for body in [
            serde_json::json!({ "bids": [], "asks": [["101.00", "1"]] }),
            serde_json::json!({ "asks": [["101.00", "1"]] }),
            serde_json::json!({ "code": -1121, "msg": "Invalid symbol." }),
        ] {
            assert!(
                depth_quote(&body).is_none(),
                "half a book is not a book: {body}"
            );
        }
    }

    /// `allMids` is an object of decimal strings, and a non-positive one is not a price.
    #[test]
    fn all_mids_parses_strings_and_drops_what_is_not_a_price() {
        let body = serde_json::json!({
            "xyz:TSLA": "307.305",
            "xyz:SKHY": "132.77",
            "xyz:DEAD": "0",
            "xyz:ODD": "not a number",
        });
        let mut mids = mids_from(&body).expect("an object");
        mids.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(mids.len(), 2, "zero and unparseable are dropped: {mids:?}");
        assert_eq!(mids[0].0, "xyz:SKHY");
        assert!((mids[1].1 - 307.305).abs() < 1e-9);
    }

    /// A body that is not the documented object is an error rather than an empty book.
    ///
    /// Empty would read as "this dex has no markets" and silently unhedge every pair on it; an error
    /// names the dex in the log and leaves the previous mids standing.
    #[test]
    fn an_all_mids_body_that_is_not_an_object_is_an_error() {
        let err = mids_from(&serde_json::json!(["ETH", "1917.95"])).expect_err("not an object");
        assert!(matches!(err, VenueError::Decode(_)));
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
