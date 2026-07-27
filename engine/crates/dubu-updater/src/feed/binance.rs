//! Binance spot `bookTicker`.
//!
//! Read-only public market data. **No credentials, no account endpoints, no order entry**, and
//! nothing in this file can be extended into any of those without adding a signing step that
//! does not exist. `bookTicker` needs no API key precisely because it is public observation
//! rather than participation — which is also why it is available to a desk that has no exchange
//! account, and this desk has none.
//!
//! # Wire format
//!
//! Combined-stream endpoint, so the subscription is in the URL and every frame is wrapped:
//!
//! ```json
//! {"stream":"ethusdt@bookTicker",
//!  "data":{"u":400900217,"s":"ETHUSDT","b":"1943.82000000","B":"24.25800000",
//!                                      "a":"1943.83000000","A":"13.78110000"}}
//! ```
//!
//! `u` is the order-book update id, `b`/`B` the best bid and its size, `a`/`A` the best ask and
//! its size. All five numeric fields are decimal strings and are parsed as exact fixed-point at
//! [`crate::units::FEED_SCALE`]; see that module for why there is no `f64` on this path.
//!
//! `u` is monotone but **not contiguous**, so a dropped message is undetectable here; see the
//! `feed` module docs.

use std::collections::BTreeMap;

use serde::Deserialize;

use super::ws::{MarketFeed, Update};
use super::{BookTick, VenueId};
use crate::units::{self, FEED_SCALE};

/// The `data` object of a `bookTicker` frame.
#[derive(Debug, Deserialize)]
struct BookTickerData {
    /// Order-book update id.
    u: u64,
    /// Symbol, upper case.
    s: String,
    /// Best bid price.
    b: String,
    /// Best bid quantity.
    #[serde(rename = "B")]
    bid_qty: String,
    /// Best ask price.
    a: String,
    /// Best ask quantity.
    #[serde(rename = "A")]
    ask_qty: String,
}

/// Combined-stream envelope. Control replies (`{"result":null,"id":1}`) have neither field and
/// fail to deserialise, which is how they get ignored without a special case.
#[derive(Debug, Deserialize)]
struct Envelope {
    #[allow(dead_code)]
    stream: String,
    data: BookTickerData,
}

/// Binance client. Holds only the symbol translation.
pub struct Client {
    /// Venue symbol (upper case) -> canonical symbol.
    symbols: BTreeMap<String, String>,
}

impl Client {
    /// Build from `(venue symbol, canonical symbol)` pairs.
    #[must_use]
    pub fn new(symbols: &[(String, String)]) -> Self {
        Self {
            symbols: symbols.iter().map(|(v, c)| (v.to_uppercase(), c.clone())).collect(),
        }
    }
}

impl MarketFeed for Client {
    fn venue(&self) -> VenueId {
        VenueId::Binance
    }

    fn url(&self, base: &str) -> String {
        let streams: Vec<String> =
            self.symbols.keys().map(|s| format!("{}@bookTicker", s.to_lowercase())).collect();
        format!("{}?streams={}", base.trim_end_matches('/'), streams.join("/"))
    }

    fn subscribe_frames(&self) -> Vec<String> {
        Vec::new()
    }

    fn parse(&mut self, text: &str) -> Result<Option<Update>, String> {
        // Try the combined-stream envelope first, then the raw single-stream shape, then give up.
        let data: BookTickerData = match serde_json::from_str::<Envelope>(text) {
            Ok(e) => e.data,
            Err(_) => match serde_json::from_str::<BookTickerData>(text) {
                Ok(d) => d,
                Err(_) => return Ok(None),
            },
        };
        let Some(symbol) = self.symbols.get(&data.s.to_uppercase()) else { return Ok(None) };

        let f = |field: &str, v: &str| -> Result<u128, String> {
            units::parse_fixed(v, FEED_SCALE).map_err(|e| format!("{field}: {e}"))
        };
        let tick = BookTick {
            update_id: data.u,
            bid: f("b", &data.b)?,
            bid_qty: f("B", &data.bid_qty)?,
            ask: f("a", &data.a)?,
            ask_qty: f("A", &data.ask_qty)?,
        };
        Ok(Some(Update { symbol: symbol.clone(), tick, reset: false }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> Client {
        Client::new(&[("ETHUSDT".into(), "ETHUSDT".into()), ("BTCUSDT".into(), "BTCUSDT".into())])
    }

    #[test]
    fn parses_a_real_combined_stream_frame() {
        // Captured off the live endpoint.
        let text = r#"{"stream":"ethusdt@bookTicker","data":{"u":400900217,"s":"ETHUSDT",
            "b":"1943.82000000","B":"24.25800000","a":"1943.83000000","A":"13.78110000"}}"#;
        let u = client().parse(text).unwrap().expect("frame carries a book update");
        assert_eq!(u.symbol, "ETHUSDT");
        assert!(!u.reset);
        assert_eq!(u.tick.update_id, 400_900_217);
        assert_eq!(u.tick.bid, 194_382_000_000);
        assert_eq!(u.tick.bid_qty, 2_425_800_000);
        assert_eq!(u.tick.ask, 194_383_000_000);
        assert_eq!(u.tick.ask_qty, 1_378_110_000);
    }

    #[test]
    fn parses_the_raw_single_stream_shape_too() {
        let text = r#"{"u":1,"s":"BTCUSDT","b":"118000.10","B":"0.5","a":"118000.20","A":"1.25"}"#;
        let u = client().parse(text).unwrap().unwrap();
        assert_eq!(u.symbol, "BTCUSDT");
        assert_eq!(u.tick.bid, 11_800_010_000_000);
        assert_eq!(u.tick.ask_qty, 125_000_000);
    }

    #[test]
    fn subscribe_acknowledgements_are_ignored_not_errors() {
        assert!(client().parse(r#"{"result":null,"id":1}"#).unwrap().is_none());
        assert!(client().parse("not json at all").unwrap().is_none());
    }

    #[test]
    fn an_unconfigured_symbol_is_ignored_rather_than_recorded() {
        let text = r#"{"u":1,"s":"SOLUSDT","b":"100","B":"1","a":"101","A":"1"}"#;
        assert!(client().parse(text).unwrap().is_none());
    }

    #[test]
    fn a_malformed_number_is_an_error_rather_than_a_zero() {
        // The failure this prevents: `bid = 0` sailing through into a fair value.
        let text = r#"{"u":1,"s":"ETHUSDT","b":"1.9e3","B":"1","a":"2000","A":"1"}"#;
        let err = client().parse(text).unwrap_err();
        assert!(err.starts_with("b:"), "error must name the offending field, got `{err}`");
    }

    #[test]
    fn the_subscription_is_in_the_url() {
        let url = client().url("wss://stream.binance.com:9443/stream");
        assert_eq!(url, "wss://stream.binance.com:9443/stream?streams=btcusdt@bookTicker/ethusdt@bookTicker");
        assert!(client().subscribe_frames().is_empty());
    }
}
