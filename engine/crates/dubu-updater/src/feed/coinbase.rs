//! Coinbase Exchange `ticker`.
//!
//! Read-only public market data. The `ticker` channel on `ws-feed.exchange.coinbase.com` takes
//! no key and no `signature` field; the authenticated channels are a different subscription
//! shape this crate never builds.
//!
//! # Wire format
//!
//! ```json
//! {"type":"ticker","sequence":26459104740,"product_id":"ETH-USDT","price":"1969.16",
//!  "best_bid":"1968.88","best_bid_size":"0.25418808",
//!  "best_ask":"1969.17","best_ask_size":"0.10120000", ...}
//! ```
//!
//! `sequence` is per product and monotone, so it detects a regression and nothing more.
//!
//! # Why this venue is implemented and not configured
//!
//! Measured against the other three over 60s of live ETHUSDT and BTCUSDT, on 2026-07-27:
//!
//! | product | mean deviation from the cross-venue median |
//! |---|---|
//! | `ETH-USD` / `BTC-USD` | **+8.4 / +9.3 bps**, persistently one-signed |
//! | `ETH-USDT` / `BTC-USDT` | +0.4 / +1.2 bps, but updated in 4 of 24 samples |
//!
//! Both numbers disqualify it for *these* pairs, for different reasons:
//!
//! * The `-USD` books are deep and continuous, and the entire 9 bp gap is the **USDT/USD basis**.
//!   The pool's pairs track `ETHUSDT` and `BTCUSDT`, so a USD-quoted venue is not a second
//!   observation of the same price — it is a different price, and averaging it in would bias the
//!   reference by roughly `9 / n` bps against a 5 bp half-spread. That is a units error wearing
//!   the costume of redundancy.
//! * The `-USDT` books are the right *unit* but thin (~500 ETH a day), and the `ticker` channel
//!   fires on **trades**, not on book changes, so it sits outside `stale_after_ms` most of the
//!   time. It would count toward the venue list and contribute nothing, which is worse than being
//!   absent because it makes quorum look healthier than it is.
//!
//! So the client exists, is parsed against a captured live frame in the tests, and is left out of
//! `updater.toml`. A pair whose base genuinely trades against USD would enable it by naming a
//! product under `[pairs.venues]`, and nothing else would have to change.

use std::collections::BTreeMap;

use serde::Deserialize;

use super::ws::{MarketFeed, Update};
use super::{BookTick, VenueId};
use crate::units::{self, FEED_SCALE};

#[derive(Debug, Deserialize)]
struct Ticker {
    #[serde(rename = "type")]
    kind: String,
    sequence: u64,
    product_id: String,
    best_bid: String,
    best_bid_size: String,
    best_ask: String,
    best_ask_size: String,
}

/// Coinbase client.
pub struct Client {
    /// Venue product (`ETH-USD`) -> canonical symbol.
    symbols: BTreeMap<String, String>,
}

impl Client {
    /// Build from `(venue product, canonical symbol)` pairs.
    #[must_use]
    pub fn new(symbols: &[(String, String)]) -> Self {
        Self { symbols: symbols.iter().cloned().collect() }
    }
}

impl MarketFeed for Client {
    fn venue(&self) -> VenueId {
        VenueId::Coinbase
    }

    fn subscribe_frames(&self) -> Vec<String> {
        let products: Vec<String> = self.symbols.keys().map(|s| format!("\"{s}\"")).collect();
        vec![format!(
            r#"{{"type":"subscribe","product_ids":[{}],"channels":["ticker"]}}"#,
            products.join(",")
        )]
    }

    fn parse(&mut self, text: &str) -> Result<Option<Update>, String> {
        // A rejected subscription must not look like a quiet venue.
        if text.starts_with(r#"{"type":"error""#) {
            return Err(format!("subscription rejected: {text}"));
        }
        let Ok(t) = serde_json::from_str::<Ticker>(text) else { return Ok(None) };
        if t.kind != "ticker" {
            return Ok(None);
        }
        let Some(symbol) = self.symbols.get(&t.product_id) else { return Ok(None) };

        let f = |field: &str, v: &str| -> Result<u128, String> {
            units::parse_fixed(v, FEED_SCALE).map_err(|e| format!("{field}: {e}"))
        };
        let tick = BookTick {
            update_id: t.sequence,
            bid: f("best_bid", &t.best_bid)?,
            bid_qty: f("best_bid_size", &t.best_bid_size)?,
            ask: f("best_ask", &t.best_ask)?,
            ask_qty: f("best_ask_size", &t.best_ask_size)?,
        };
        Ok(Some(Update { symbol: symbol.clone(), tick, reset: false }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> Client {
        Client::new(&[("ETH-USDT".into(), "ETHUSDT".into()), ("BTC-USD".into(), "BTCUSDT".into())])
    }

    /// Captured verbatim off `wss://ws-feed.exchange.coinbase.com`.
    const LIVE: &str = r#"{"type":"ticker","sequence":26459104740,"product_id":"ETH-USDT","price":"1969.16","open_24h":"1880.47","volume_24h":"511.73577630","low_24h":"1879.01","high_24h":"1980.99","volume_30d":"19731.57018241","best_bid":"1968.88","best_bid_size":"0.25418808","best_ask":"1969.17","best_ask_size":"0.10120000","side":"sell","time":"2026-07-27T07:10:02.486766Z","trade_id":38422555,"last_size":"0.03689"}"#;

    #[test]
    fn parses_a_real_ticker_frame() {
        let u = client().parse(LIVE).unwrap().expect("frame carries a book update");
        assert_eq!(u.symbol, "ETHUSDT");
        assert!(!u.reset);
        assert_eq!(u.tick.update_id, 26_459_104_740);
        assert_eq!(u.tick.bid, 196_888_000_000);
        assert_eq!(u.tick.bid_qty, 25_418_808);
        assert_eq!(u.tick.ask, 196_917_000_000);
        assert_eq!(u.tick.ask_qty, 10_120_000);
    }

    #[test]
    fn a_usd_product_can_stand_in_for_a_usdt_symbol_only_because_the_operator_said_so() {
        // The mapping is configuration, not inference: this client will happily carry `BTC-USD`
        // for canonical `BTCUSDT`, and the module docs are where the 9 bp basis that makes that
        // a bad idea for these pairs is written down.
        let text = LIVE.replace("ETH-USDT", "BTC-USD");
        assert_eq!(client().parse(&text).unwrap().unwrap().symbol, "BTCUSDT");
    }

    #[test]
    fn control_frames_are_ignored_and_a_rejected_subscription_is_not() {
        let mut c = client();
        assert!(c
            .parse(r#"{"type":"subscriptions","channels":[{"name":"ticker","product_ids":["ETH-USDT"]}]}"#)
            .unwrap()
            .is_none());
        assert!(c
            .parse(r#"{"type":"error","message":"Failed to subscribe","reason":"ETH-XYZ is not a valid product"}"#)
            .unwrap_err()
            .contains("not a valid product"));
    }

    #[test]
    fn an_unconfigured_product_is_ignored() {
        let text = LIVE.replace("ETH-USDT", "SOL-USD");
        assert!(client().parse(&text).unwrap().is_none());
    }

    #[test]
    fn the_subscribe_frame_names_every_configured_product() {
        let frames = client().subscribe_frames();
        assert_eq!(frames.len(), 1);
        assert!(frames[0].contains(r#""ETH-USDT""#));
        assert!(frames[0].contains(r#""BTC-USD""#));
        serde_json::from_str::<serde_json::Value>(&frames[0]).expect("subscribe frame must be JSON");
    }
}
