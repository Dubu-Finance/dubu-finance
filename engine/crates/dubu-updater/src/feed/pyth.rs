//! Pyth Hermes, a second reference for the equity pairs.
//!
//! # What it fixes
//!
//! The equity pairs ran on one HIP-3 book, and that book is dark about a quarter of the time —
//! measured per pair, 755 `reference` events against 241 `no_reference`. A reference that only
//! moves in the gaps between outages is a step function: sigma collapses to ~0.10 bp, which makes
//! `jump_floor_bps` the binding threshold, and the catch-up move on the far side of a gap (28 bp
//! measured, against a 15 bp floor) then trips the jump detector and withdraws quotes for 30s. One
//! independent feed removes the gap, the false jump and the withdrawal together.
//!
//! # Two of the four, and no invented mappings
//!
//! Pyth lists AAPL and TSLA. It does not list SPCX, and SKHY is a Korean listing that will not
//! appear either, so those two stay on one venue until a second source for them exists.
//!
//! # A price and a confidence, not a book
//!
//! Every other venue here publishes a two-sided book. Pyth publishes an aggregate price and a
//! **confidence interval** — its own statement of how far the publishers are apart — so the top of
//! book is synthesised as `price ± conf`, with equal size a side. That is the only non-arbitrary
//! width available, and the symmetry makes [`crate::fair_value::micro_price`] collapse to the
//! midpoint, so the reference is exactly the price Pyth published. The confidence then survives as
//! that venue's `book_spread_bps` and is logged with the reference; see
//! [`crate::fair_value::spread_summary`].
//!
//! # Market hours are not an outage
//!
//! These feeds follow the US regular session and simply stop publishing outside it. That is why
//! the publish time is checked here rather than left to [`FeedShared`], which ages a tick from when
//! it **arrived**: a closed market answers instantly with a price hours old. An update past the
//! staleness window is dropped silently, so the venue goes [`super::VenueStatus::Stale`] and leaves
//! the cross-section — it is not an error and must not be reported as one.
//!
//! # This spends the on-chain deviation bound, for these two pairs
//!
//! `PropPool` is designed to grow a deviation check against Pyth on chain, and that check is only
//! worth anything against a price derived independently of it. Pricing mAAPL and mTSLA partly from
//! Pyth means that bound, once built, would be comparing a number against one of its own inputs for
//! those two. It is the right trade today — the bound does not exist and the dark quarter of the
//! Hyperliquid book does — and it is the reason to drop this venue for those pairs if the bound is
//! ever built.
//!
//! # HTTP, not a socket
//!
//! Hermes also streams over server-sent events. Polling is used instead because it is what makes
//! the paragraph above work: a closed market leaves an SSE stream silent, which is indistinguishable
//! from a dead one, so every read timeout would reconnect and the venue would report itself as
//! flapping all night. A poll that returns an unchanged publish time says "no tick" precisely.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::sync::watch;
use tracing::{debug, error, info, warn};

use dubu_core::math::pow10;

use super::ws::{backoff, Update};
use super::{BookTick, FeedShared, VenueId};
use crate::config::FeedConfig;
use crate::units::FEED_SCALE;

/// Symbol suffixes Pyth marks `DEPRECATED FEED`.
///
/// Each is a separate feed covering one session outside regular hours, so selecting one prices a
/// pair off a feed that is dark exactly when the market is open.
pub const DEPRECATED_SUFFIXES: [&str; 3] = [".PRE", ".ON", ".POST"];

/// The size both synthetic sides carry, one unit at [`FEED_SCALE`].
///
/// Only the symmetry matters: equal sizes make the size-weighted micro-price the midpoint. The
/// value itself is never read as depth, because nothing rests on a Pyth book.
const SYNTHETIC_QTY: u128 = 100_000_000;

/// One entry of `/v2/price_feeds`.
#[derive(Debug, Deserialize)]
struct FeedEntry {
    id: String,
    attributes: FeedAttributes,
}

#[derive(Debug, Deserialize)]
struct FeedAttributes {
    symbol: String,
}

/// `/v2/updates/price/latest`. `binary` is the on-chain proof and is deliberately not read.
#[derive(Debug, Deserialize)]
struct Latest {
    parsed: Vec<Parsed>,
}

#[derive(Debug, Deserialize)]
struct Parsed {
    id: String,
    price: PriceFields,
}

/// `{"price":"33223750","conf":"15272","expo":-5,"publish_time":1785424136}`.
///
/// The value is `price * 10^expo`, and `expo` is a property of the feed rather than a constant, so
/// it is applied per message.
#[derive(Debug, Deserialize)]
struct PriceFields {
    price: String,
    conf: String,
    expo: i32,
    publish_time: i64,
}

/// `value * 10^expo`, rescaled to [`FEED_SCALE`]. Floors when the feed is finer than the scale.
fn to_feed_scale(value: &str, expo: i32) -> Result<u128, String> {
    let v: u128 = value
        .parse()
        .map_err(|_| format!("`{value}` is not a non-negative integer"))?;
    let shift = expo
        .checked_add(i32::from(FEED_SCALE))
        .ok_or_else(|| format!("expo {expo} is out of range"))?;
    let mag = u8::try_from(shift.unsigned_abs())
        .ok()
        .and_then(pow10)
        .ok_or_else(|| format!("expo {expo} is out of range"))?;
    if shift >= 0 {
        v.checked_mul(mag)
            .ok_or_else(|| format!("`{value}`e{expo} overflows a u128 at the feed scale"))
    } else {
        Ok(v / mag)
    }
}

/// The synthetic top of book: the confidence interval either side of the price.
///
/// `None` when no two-sided book can be formed, which is a frame to drop rather than an error —
/// a confidence wider than the price itself says nothing about where the price is.
fn book(price: u128, conf: u128, publish_time: u64) -> Option<BookTick> {
    // A zero confidence would lock the book, and `micro_price` rejects a locked one outright.
    let half = conf.max(1);
    let bid = price.checked_sub(half)?;
    if bid == 0 {
        return None;
    }
    let ask = price.checked_add(half)?;
    debug_assert!(bid < ask, "a symmetric interval cannot cross");
    Some(BookTick {
        update_id: publish_time,
        bid,
        bid_qty: SYNTHETIC_QTY,
        ask,
        ask_qty: SYNTHETIC_QTY,
    })
}

/// Pyth Hermes client: symbol resolution, then price decoding.
pub struct Client {
    /// Pyth symbol (`Equity.US.AAPL/USD`) -> canonical symbol (`AAPL`).
    symbols: BTreeMap<String, String>,
    /// Feed id -> canonical symbol. Empty until [`Client::resolve`] has run.
    ids: BTreeMap<String, String>,
    /// Canonical symbol -> newest publish time recorded, per connection.
    seen: BTreeMap<String, u64>,
}

impl Client {
    /// Build from `(venue symbol, canonical symbol)` pairs.
    ///
    /// # Panics
    /// If any symbol is empty. A client built from a blank symbol subscribes to nothing and reads
    /// as a healthy silent venue.
    #[must_use]
    pub fn new(symbols: &[(String, String)]) -> Self {
        for (venue, canonical) in symbols {
            assert!(!venue.is_empty(), "a pyth symbol must not be blank");
            assert!(
                !canonical.is_empty(),
                "a canonical symbol must not be blank"
            );
        }
        let client = Self {
            symbols: symbols.iter().cloned().collect(),
            ids: BTreeMap::new(),
            seen: BTreeMap::new(),
        };
        debug_assert_eq!(
            client.symbols.len(),
            symbols.len(),
            "two pairs claim the same pyth symbol, so one is silently unresolved"
        );
        client
    }

    /// Whether the feed ids still have to be resolved.
    #[must_use]
    pub fn needs_ids(&self) -> bool {
        self.ids.is_empty()
    }

    /// Turn a `/v2/price_feeds` body into feed ids, returning the configured symbols it did not
    /// find.
    ///
    /// Selection is an **exact** symbol match, which is what excludes the `.PRE` / `.ON` / `.POST`
    /// variants: they are separate feeds under separate symbols.
    ///
    /// # Errors
    /// A human-readable reason if the body is not the array the endpoint documents.
    pub fn resolve(&mut self, body: &str) -> Result<Vec<String>, String> {
        let entries: Vec<FeedEntry> =
            serde_json::from_str(body).map_err(|e| format!("price_feeds did not decode: {e}"))?;
        let mut ids = BTreeMap::new();
        for e in entries {
            // Belt and braces over the exact match above: a config naming a deprecated variant
            // directly must resolve to nothing rather than to a feed Pyth has said it will remove.
            if DEPRECATED_SUFFIXES
                .iter()
                .any(|s| e.attributes.symbol.ends_with(s))
            {
                continue;
            }
            if let Some(canonical) = self.symbols.get(&e.attributes.symbol) {
                ids.insert(e.id, canonical.clone());
            }
        }
        let missing: Vec<String> = self
            .symbols
            .iter()
            .filter(|(_, canonical)| !ids.values().any(|c| c == *canonical))
            .map(|(pyth, _)| pyth.clone())
            .collect();
        assert!(
            ids.len() + missing.len() >= self.symbols.len(),
            "every configured symbol is either resolved or reported missing"
        );
        self.ids = ids;
        Ok(missing)
    }

    /// `ids[]=<id>&ids[]=<id>`, in a stable order. Empty until the ids are resolved.
    #[must_use]
    pub fn ids_query(&self) -> String {
        self.ids
            .keys()
            .map(|id| format!("ids[]={id}"))
            .collect::<Vec<_>>()
            .join("&")
    }

    /// `AAPL@49f6b65c TSLA@16dad506`, for one startup log line.
    #[must_use]
    pub fn summary(&self) -> String {
        self.ids
            .iter()
            .map(|(id, canonical)| {
                format!("{canonical}@{}", id.chars().take(8).collect::<String>())
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Drop the per-connection publish-time state. Mirrors [`super::ws::MarketFeed::on_connect`].
    pub fn on_connect(&mut self) {
        self.seen.clear();
    }

    /// Decode one `/v2/updates/price/latest` body into the ticks that may be recorded.
    ///
    /// `now_unix` and `max_age` are the publish-time check; see the module docs on why it cannot be
    /// left to [`FeedShared`]. An entry that is stale, unchanged, or unconfigured yields nothing —
    /// none of the three is an error.
    ///
    /// # Errors
    /// A human-readable reason if the body is not the object the endpoint documents, or if a price
    /// does not convert. Never fatal: the caller logs it and drops the poll.
    pub fn parse(
        &mut self,
        body: &str,
        now_unix: u64,
        max_age: Duration,
    ) -> Result<Vec<Update>, String> {
        let latest: Latest = serde_json::from_str(body)
            .map_err(|e| format!("price update did not decode, or carried no `parsed`: {e}"))?;
        let mut out = Vec::new();
        for p in latest.parsed {
            let Some(symbol) = self.ids.get(&p.id).cloned() else {
                continue;
            };
            let Ok(published) = u64::try_from(p.price.publish_time) else {
                continue;
            };
            // The market is closed, not the venue. See the module docs.
            if now_unix.saturating_sub(published) > max_age.as_secs() {
                continue;
            }
            // A repeat carries no new observation, and recording one would be refused as a
            // sequence regression and counted as a gap on every poll.
            if self
                .seen
                .get(&symbol)
                .is_some_and(|&last| published <= last)
            {
                continue;
            }
            let price = to_feed_scale(&p.price.price, p.price.expo)?;
            let conf = to_feed_scale(&p.price.conf, p.price.expo)?;
            let Some(tick) = book(price, conf, published) else {
                continue;
            };
            self.seen.insert(symbol.clone(), published);
            out.push(Update {
                symbol,
                tick,
                // Every poll carries the whole price, so there is no per-connection book to reset.
                reset: false,
            });
        }
        Ok(out)
    }
}

/// One step of the loop below.
enum Step {
    /// The feed ids are now known.
    Resolved,
    /// A price body came back.
    Body(String),
}

/// One GET, with the body only if the endpoint answered 2xx.
///
/// A non-2xx body is a message rather than JSON — an unknown id answers `404 Price ids not found:
/// 0x…` in plain text — so it is carried into the error instead of being decoded, truncated because
/// the discovery response is most of a megabyte.
async fn get(http: &reqwest::Client, url: &str) -> Result<String, String> {
    let resp = http.get(url).send().await.map_err(|e| e.to_string())?;
    let status = resp.status();
    let body = resp.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!(
            "HTTP {status}: {}",
            body.chars().take(200).collect::<String>()
        ));
    }
    Ok(body)
}

/// Resolve every configured symbol to a feed id.
///
/// A symbol that is not listed is named loudly and once, because it can only be a config error and
/// no amount of retrying fixes it. Resolving **none** of them is an error, which is what stops the
/// loop spinning on an endpoint that has nothing for us.
async fn discover(http: &reqwest::Client, base: &str, client: &mut Client) -> Result<(), String> {
    let body = get(http, &format!("{base}/v2/price_feeds?asset_type=equity")).await?;
    let missing = client.resolve(&body)?;
    if !missing.is_empty() {
        error!(target: "feed", event = "feed_unresolved", venue = %VenueId::Pyth,
               symbols = %missing.join(","),
               "PYTH SYMBOL NOT LISTED: this pair gets no price from this venue");
    }
    if client.needs_ids() {
        return Err("no configured symbol resolved to a feed id".to_string());
    }
    info!(target: "feed", event = "feeds_resolved", venue = %VenueId::Pyth,
          feeds = %client.summary(), "resolved pyth feed ids");
    Ok(())
}

/// Hand every decoded tick to the shared state.
fn record(shared: &FeedShared, updates: Vec<Update>) {
    let now = Instant::now();
    for u in updates {
        debug_assert!(!u.symbol.is_empty(), "a tick must name a symbol");
        if let Err(rej) = shared.record(&u.symbol, u.tick, now) {
            debug!(target: "feed", event = "tick_rejected", venue = %VenueId::Pyth,
                   symbol = %u.symbol, reason = ?rej, "dropped a book tick");
        }
    }
}

/// Run the Pyth feed until `shutdown` flips, backing off on every failure.
///
/// The same contract as [`super::ws::run`], and the same [`FeedShared`] underneath: a venue that
/// cannot be reached is a *state* the rest of the system reads off that, not a failure that should
/// take the process down. Only the transport differs.
///
/// # Panics
/// If `cfg` violates the bounds [`FeedConfig`] validates at load.
pub async fn run(
    cfg: FeedConfig,
    base_url: String,
    mut client: Client,
    shared: Arc<FeedShared>,
    mut shutdown: watch::Receiver<bool>,
) {
    assert!(!base_url.is_empty(), "a venue needs an endpoint");
    assert!(cfg.reconnect_initial_ms > 0, "a zero backoff hot-loops");
    assert!(cfg.reconnect_max_ms >= cfg.reconnect_initial_ms);
    assert!(cfg.poll_interval_ms > 0, "a zero poll interval hot-loops");

    let venue = VenueId::Pyth;
    // A response that lands later than the staleness window carries a price already too old to
    // quote, so that window is the request timeout rather than the websocket read timeout.
    let stale_after = Duration::from_millis(cfg.stale_after_ms);
    let http = match reqwest::Client::builder().timeout(stale_after).build() {
        Ok(c) => c,
        Err(e) => {
            error!(target: "feed", event = "client_failed", venue = %venue, error = %e,
                   "could not build the HTTP client; this venue will not run");
            return;
        }
    };

    let base = base_url.trim_end_matches('/').to_string();
    let initial = Duration::from_millis(cfg.reconnect_initial_ms);
    let delay_max = Duration::from_millis(cfg.reconnect_max_ms);
    let poll = Duration::from_millis(cfg.poll_interval_ms);
    let mut delay = initial;
    let mut first = true;
    let mut up = false;

    loop {
        if *shutdown.borrow() {
            return;
        }
        assert!(
            delay <= delay_max,
            "the backoff must stay under its ceiling"
        );

        let step = if client.needs_ids() {
            discover(&http, &base, &mut client)
                .await
                .map(|()| Step::Resolved)
        } else {
            let url = format!("{base}/v2/updates/price/latest?{}", client.ids_query());
            let body = tokio::select! {
                biased;
                _ = shutdown.changed() => return,
                b = get(&http, &url) => b,
            };
            body.map(Step::Body)
        };

        let wait = match step {
            // Straight into the first poll rather than waiting out an interval.
            Ok(Step::Resolved) => {
                delay = initial;
                continue;
            }
            Ok(Step::Body(body)) => {
                // The transition is taken before the body is read, so that clearing the stored
                // ticks cannot discard the ones this poll is about to record.
                if !up {
                    shared.on_connected(first);
                    client.on_connect();
                    first = false;
                    up = true;
                    info!(target: "feed", event = "connected", venue = %venue,
                          "market data venue connected");
                }
                // A schema change degrades this venue rather than the process, exactly as a
                // webserver venue's parse error does.
                match client.parse(&body, crate::now_unix(), stale_after) {
                    Ok(updates) => record(&shared, updates),
                    Err(e) => warn!(target: "feed", event = "parse_error", venue = %venue,
                                    error = %e, "unparseable price body; dropped"),
                }
                delay = initial;
                poll
            }
            Err(e) => {
                if up {
                    shared.on_disconnected();
                    up = false;
                }
                warn!(target: "feed", event = "poll_failed", venue = %venue, error = %e,
                      retry_in_ms = delay.as_millis(), "could not read the venue");
                let wait = delay;
                delay = backoff(delay, delay_max);
                wait
            }
        };

        tokio::select! {
            biased;
            _ = shutdown.changed() => return,
            () = tokio::time::sleep(wait) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fair_value::micro_price;

    /// Captured verbatim from `hermes.pyth.network/v2/updates/price/latest` on 2026-07-31, trimmed
    /// to the fields this module reads.
    const LIVE: &str = r#"{"binary":{"encoding":"hex","data":["504e4155"]},"parsed":[
      {"id":"49f6b65cb1de6b10eaf75e7c03ca029c306d0357e91b5311b175084a5ad55688",
       "price":{"price":"33223750","conf":"15272","expo":-5,"publish_time":1785424136},
       "ema_price":{"price":"33182101","conf":"21716","expo":-5,"publish_time":1785424136},
       "metadata":{"slot":305542333,"proof_available_time":1785424138,"prev_publish_time":1785424136}},
      {"id":"16dad506d7db8da01c87581c87ca897a012a153557d4d578c3b9c9e1bc0632f1",
       "price":{"price":"30557932","conf":"59461","expo":-5,"publish_time":1785424136},
       "ema_price":{"price":"30627819","conf":"31711","expo":-5,"publish_time":1785424136},
       "metadata":{"slot":305542333,"proof_available_time":1785424138,"prev_publish_time":1785424136}}]}"#;

    /// Captured verbatim from `/v2/price_feeds?asset_type=equity`, four of the 1,772 entries: the
    /// live AAPL feed and its three deprecated session variants.
    const FEEDS: &str = r#"[
      {"id":"8c320e4cd87c6c9e2b1b3f5f4a6d8e0f1a2b3c4d5e6f708192a3b4c5d6e7f809",
       "attributes":{"symbol":"Equity.US.AAPL/USD.PRE",
                     "description":"DEPRECATED FEED - APPLE INC / US DOLLAR - PRE MARKET HOURS"}},
      {"id":"241b9a5ce1c3e4a5b6c7d8e9f0a1b2c3d4e5f60718293a4b5c6d7e8f9a0b1c2d",
       "attributes":{"symbol":"Equity.US.AAPL/USD.ON",
                     "description":"DEPRECATED FEED - APPLE INC / US DOLLAR - OVERNIGHT HOURS"}},
      {"id":"5a207c4aa0114b2c3d4e5f60718293a4b5c6d7e8f9a0b1c2d3e4f5061728394a",
       "attributes":{"symbol":"Equity.US.AAPL/USD.POST",
                     "description":"DEPRECATED FEED - APPLE INC / US DOLLAR - POST MARKET HOURS"}},
      {"id":"49f6b65cb1de6b10eaf75e7c03ca029c306d0357e91b5311b175084a5ad55688",
       "attributes":{"symbol":"Equity.US.AAPL/USD","description":"APPLE INC / US DOLLAR"}},
      {"id":"16dad506d7db8da01c87581c87ca897a012a153557d4d578c3b9c9e1bc0632f1",
       "attributes":{"symbol":"Equity.US.TSLA/USD","description":"TESLA INC / US DOLLAR"}}]"#;

    const PUBLISHED: u64 = 1_785_424_136;
    const WINDOW: Duration = Duration::from_millis(5_000);

    fn client() -> Client {
        let mut c = Client::new(&[
            ("Equity.US.AAPL/USD".to_string(), "AAPL".to_string()),
            ("Equity.US.TSLA/USD".to_string(), "TSLA".to_string()),
        ]);
        assert!(c.resolve(FEEDS).unwrap().is_empty());
        c
    }

    #[test]
    fn a_live_body_decodes_to_a_price_at_feed_scale() {
        let updates = client().parse(LIVE, PUBLISHED, WINDOW).unwrap();
        assert_eq!(updates.len(), 2);
        let aapl = updates.iter().find(|u| u.symbol == "AAPL").unwrap();
        // 33223750 at expo -5 is 332.2375; FEED_SCALE is 8, so 33_223_750_000.
        assert_eq!(micro_price(&aapl.tick).unwrap(), 33_223_750_000);
        assert_eq!(aapl.tick.update_id, PUBLISHED);
        assert!(!aapl.reset);

        let tsla = updates.iter().find(|u| u.symbol == "TSLA").unwrap();
        assert_eq!(micro_price(&tsla.tick).unwrap(), 30_557_932_000);
    }

    /// The confidence is the synthetic book's half-width, and the symmetry is what makes the
    /// micro-price the published price rather than something weighted off it.
    #[test]
    fn the_confidence_interval_becomes_the_book_and_not_the_price() {
        let updates = client().parse(LIVE, PUBLISHED, WINDOW).unwrap();
        let aapl = updates.iter().find(|u| u.symbol == "AAPL").unwrap();
        // conf 15272 at expo -5 is 0.15272 -> 15_272_000 at FEED_SCALE.
        assert_eq!(aapl.tick.bid, 33_223_750_000 - 15_272_000);
        assert_eq!(aapl.tick.ask, 33_223_750_000 + 15_272_000);
        assert_eq!(aapl.tick.bid_qty, aapl.tick.ask_qty);
        assert_eq!(
            crate::fair_value::book_spread_bps(aapl.tick.bid, aapl.tick.ask),
            9,
            "0.30544 either side of 332.2375 is a 9 bp book"
        );
    }

    /// A body with no `parsed` is a schema change or an error page, and must not read as a venue
    /// with nothing to say.
    #[test]
    fn a_body_without_parsed_is_an_error() {
        assert!(client()
            .parse(
                r#"{"binary":{"encoding":"hex","data":[]}}"#,
                PUBLISHED,
                WINDOW
            )
            .is_err());
        assert!(client()
            .parse("Price ids not found: 0x49f6", PUBLISHED, WINDOW)
            .is_err());
        // ... but an empty list is a well-formed answer carrying no update.
        assert!(client()
            .parse(r#"{"parsed":[]}"#, PUBLISHED, WINDOW)
            .unwrap()
            .is_empty());
    }

    /// 16:00 ET: the feed stops publishing and the endpoint keeps answering. That is this venue
    /// having no tick, not an error, and the distinction is the whole point of checking here.
    #[test]
    fn a_stale_publish_time_is_no_tick_rather_than_an_error() {
        let updates = client()
            .parse(LIVE, PUBLISHED + 6, WINDOW)
            .expect("a closed market is not a parse failure");
        assert!(updates.is_empty());
        // Inside the window it is still a tick, so the boundary is the window and nothing else.
        assert_eq!(
            client().parse(LIVE, PUBLISHED + 5, WINDOW).unwrap().len(),
            2
        );
    }

    /// The `.PRE` / `.ON` / `.POST` feeds carry the same ticker and are marked DEPRECATED FEED.
    /// Selecting one would price the pair off a feed that is dark during the session.
    #[test]
    fn the_deprecated_session_feeds_are_not_selected() {
        let c = client();
        assert_eq!(
            c.ids_query(),
            "ids[]=16dad506d7db8da01c87581c87ca897a012a153557d4d578c3b9c9e1bc0632f1\
             &ids[]=49f6b65cb1de6b10eaf75e7c03ca029c306d0357e91b5311b175084a5ad55688"
        );
        assert!(
            !c.ids_query().contains("8c320e4c"),
            "the .PRE feed was selected"
        );
        assert!(
            !c.ids_query().contains("241b9a5c"),
            "the .ON feed was selected"
        );
        assert!(
            !c.ids_query().contains("5a207c4a"),
            "the .POST feed was selected"
        );

        // And a config that names one directly resolves to nothing rather than to that feed.
        let mut direct = Client::new(&[("Equity.US.AAPL/USD.PRE".to_string(), "AAPL".to_string())]);
        assert_eq!(
            direct.resolve(FEEDS).unwrap(),
            vec!["Equity.US.AAPL/USD.PRE"]
        );
        assert!(direct.needs_ids());
    }

    /// The publish time is the sequence number, so an unchanged one must yield nothing at all:
    /// emitting it would be refused as a regression and counted as a gap on every poll.
    #[test]
    fn an_unchanged_publish_time_yields_no_second_tick() {
        let mut c = client();
        assert_eq!(c.parse(LIVE, PUBLISHED, WINDOW).unwrap().len(), 2);
        assert!(c.parse(LIVE, PUBLISHED, WINDOW).unwrap().is_empty());

        let newer = LIVE.replace("1785424136", "1785424137");
        assert_eq!(c.parse(&newer, PUBLISHED + 1, WINDOW).unwrap().len(), 2);

        // A reconnect drops that state, because the ticks it was guarding are gone with it.
        c.on_connect();
        assert_eq!(c.parse(&newer, PUBLISHED + 1, WINDOW).unwrap().len(), 2);
    }

    /// An unresolved id in the body belongs to somebody else's subscription, not to us.
    #[test]
    fn an_unconfigured_feed_id_is_ignored() {
        let other = LIVE.replace("49f6b65c", "deadbeef");
        let updates = client().parse(&other, PUBLISHED, WINDOW).unwrap();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].symbol, "TSLA");
    }

    #[test]
    fn the_exponent_is_applied_per_message_and_both_directions() {
        assert_eq!(to_feed_scale("33223750", -5), Ok(33_223_750_000));
        assert_eq!(to_feed_scale("332", 0), Ok(33_200_000_000));
        // Finer than the feed scale: floored, not refused. 332.2375012345 -> 332.23750123.
        assert_eq!(to_feed_scale("3322375012345", -10), Ok(33_223_750_123));
        assert!(
            to_feed_scale("-1", -5).is_err(),
            "a negative price is not a price"
        );
        assert!(to_feed_scale("1", i32::MIN).is_err());
        assert!(to_feed_scale("1", i32::MAX).is_err());
    }

    /// A confidence at or beyond the price leaves no two-sided book. Dropping beats publishing a
    /// zero bid, which the ladder would take as a real price.
    #[test]
    fn a_confidence_wider_than_the_price_is_dropped() {
        assert!(book(100, 100, 1).is_none());
        assert!(book(100, 500, 1).is_none());
        // A zero confidence still has to produce a book that is not locked.
        let t = book(100, 0, 1).unwrap();
        assert!(t.bid < t.ask);
        assert!(micro_price(&t).is_ok());
    }
}
