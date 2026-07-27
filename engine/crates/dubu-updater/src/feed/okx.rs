//! OKX v5 public `bbo-tbt`.
//!
//! Read-only public market data. `/ws/v5/public` needs no key, no account and no login frame —
//! the authenticated stream is a different endpoint (`/ws/v5/private`) that this crate never
//! names.
//!
//! # Wire format
//!
//! Subscribe over the socket, then every update arrives as:
//!
//! ```json
//! {"arg":{"channel":"bbo-tbt","instId":"ETH-USDT"},
//!  "data":[{"asks":[["1969.30","34.015588","0","2"]],
//!           "bids":[["1969.29","28.639431","0","15"]],
//!           "ts":"1785136396508","seqId":72498651540}]}
//! ```
//!
//! Each level is `[price, size, liquidatedOrders, orderCount]` and `bbo-tbt` publishes exactly
//! one level a side, which is the whole point of the channel — it is the top of book tick by
//! tick, rather than a depth stream we would have to fold down.
//!
//! `seqId` is the sequence, and it is monotone per instrument but not contiguous, so it detects
//! a regression and nothing more.
//!
//! # Keepalive
//!
//! OKX closes a connection that has been idle for 30 seconds and expects the **literal string**
//! `ping` — not a JSON object, and not a websocket ping frame — answered with `pong`. A liquid
//! `bbo-tbt` subscription pushes far more often than that, so this only matters when a market
//! genuinely stops, which is exactly when a silent disconnect would be most confusing.

use std::collections::BTreeMap;

use serde::Deserialize;

use super::ws::{MarketFeed, Update};
use super::{BookTick, VenueId};
use crate::units::{self, FEED_SCALE};

/// How often to send the keepalive. Comfortably inside OKX's 30s idle timeout.
const KEEPALIVE_SECS: u64 = 20;

#[derive(Debug, Deserialize)]
struct Arg {
    #[serde(rename = "instId")]
    inst_id: String,
}

/// A level is `[price, size, liquidatedOrders, orderCount]`. Held as a `Vec` rather than a
/// four-tuple so that OKX appending a fifth element — which a tuple struct would reject outright,
/// taking the whole venue offline — is a field this parser simply does not read.
#[derive(Debug, Deserialize)]
struct Bbo {
    asks: Vec<Vec<String>>,
    bids: Vec<Vec<String>>,
    #[serde(rename = "seqId")]
    seq_id: u64,
}

#[derive(Debug, Deserialize)]
struct Frame {
    arg: Arg,
    data: Vec<Bbo>,
}

/// OKX client.
pub struct Client {
    /// Venue symbol (`ETH-USDT`) -> canonical symbol.
    symbols: BTreeMap<String, String>,
}

impl Client {
    /// Build from `(venue symbol, canonical symbol)` pairs.
    #[must_use]
    pub fn new(symbols: &[(String, String)]) -> Self {
        Self { symbols: symbols.iter().cloned().collect() }
    }
}

impl MarketFeed for Client {
    fn venue(&self) -> VenueId {
        VenueId::Okx
    }

    fn subscribe_frames(&self) -> Vec<String> {
        let args: Vec<String> = self
            .symbols
            .keys()
            .map(|s| format!(r#"{{"channel":"bbo-tbt","instId":"{s}"}}"#))
            .collect();
        vec![format!(r#"{{"op":"subscribe","args":[{}]}}"#, args.join(","))]
    }

    fn keepalive(&self) -> Option<(std::time::Duration, String)> {
        Some((std::time::Duration::from_secs(KEEPALIVE_SECS), "ping".to_string()))
    }

    fn parse(&mut self, text: &str) -> Result<Option<Update>, String> {
        // `pong`, and the `{"event":"subscribe"|"error"}` control replies, are not book updates.
        // An `event: error` is worth surfacing, because a rejected subscription otherwise looks
        // exactly like a venue that has nothing to say.
        if text == "pong" {
            return Ok(None);
        }
        if let Some(rest) = text.strip_prefix(r#"{"event":"error""#) {
            return Err(format!("subscription rejected: {}", rest.trim_end_matches('}')));
        }
        let Ok(frame) = serde_json::from_str::<Frame>(text) else { return Ok(None) };
        let Some(symbol) = self.symbols.get(&frame.arg.inst_id) else { return Ok(None) };
        let Some(bbo) = frame.data.first() else { return Ok(None) };

        let (Some(bid), Some(ask)) = (bbo.bids.first(), bbo.asks.first()) else {
            // A book with a side missing is not a top of book. Dropping it is right: the
            // micro-price would be meaningless and a zero would be worse.
            return Ok(None);
        };
        if bid.len() < 2 || ask.len() < 2 {
            return Err("a level carried fewer than [price, size]".to_string());
        }
        let f = |field: &str, v: &str| -> Result<u128, String> {
            units::parse_fixed(v, FEED_SCALE).map_err(|e| format!("{field}: {e}"))
        };
        // Both levels were length-checked four lines up; a venue that sends a short level is
        // rejected there with a message rather than indexed into here.
        #[allow(clippy::indexing_slicing)]
        let tick = BookTick {
            update_id: bbo.seq_id,
            bid: f("bids[0][0]", &bid[0])?,
            bid_qty: f("bids[0][1]", &bid[1])?,
            ask: f("asks[0][0]", &ask[0])?,
            ask_qty: f("asks[0][1]", &ask[1])?,
        };
        Ok(Some(Update { symbol: symbol.clone(), tick, reset: false }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> Client {
        Client::new(&[("ETH-USDT".into(), "ETHUSDT".into()), ("BTC-USDT".into(), "BTCUSDT".into())])
    }

    /// Captured verbatim off `wss://ws.okx.com:8443/ws/v5/public`.
    const LIVE: &str = r#"{"arg":{"channel":"bbo-tbt","instId":"ETH-USDT"},"data":[{"asks":[["1969.3","34.015588","0","2"]],"bids":[["1969.29","28.639431","0","15"]],"ts":"1785136396508","seqId":72498651540}]}"#;

    #[test]
    fn parses_a_real_bbo_frame() {
        let u = client().parse(LIVE).unwrap().expect("frame carries a book update");
        assert_eq!(u.symbol, "ETHUSDT");
        assert!(!u.reset);
        assert_eq!(u.tick.update_id, 72_498_651_540);
        assert_eq!(u.tick.bid, 196_929_000_000);
        assert_eq!(u.tick.bid_qty, 2_863_943_100);
        assert_eq!(u.tick.ask, 196_930_000_000);
        assert_eq!(u.tick.ask_qty, 3_401_558_800);
    }

    #[test]
    fn the_venue_symbol_is_translated_to_the_canonical_one() {
        // OKX says `ETH-USDT`; the pool's pair is configured as `ETHUSDT`. Recording under the
        // venue's own spelling would leave every venue in its own namespace and the
        // cross-section permanently empty.
        let u = client().parse(LIVE).unwrap().unwrap();
        assert_eq!(u.symbol, "ETHUSDT");
    }

    #[test]
    fn control_frames_are_ignored_and_a_rejected_subscription_is_not() {
        let mut c = client();
        assert!(c.parse("pong").unwrap().is_none());
        assert!(c
            .parse(r#"{"event":"subscribe","arg":{"channel":"bbo-tbt","instId":"ETH-USDT"},"connId":"d27fab6a"}"#)
            .unwrap()
            .is_none());
        // A rejected subscription must not look like a quiet venue.
        let err = c.parse(r#"{"event":"error","code":"60012","msg":"Invalid request"}"#).unwrap_err();
        assert!(err.contains("60012"), "the error must carry the venue's code: {err}");
    }

    #[test]
    fn a_one_sided_book_is_dropped_rather_than_priced() {
        let text = r#"{"arg":{"channel":"bbo-tbt","instId":"ETH-USDT"},"data":[{"asks":[],"bids":[["1969.29","28.6","0","15"]],"ts":"1","seqId":1}]}"#;
        assert!(client().parse(text).unwrap().is_none());
    }

    #[test]
    fn an_unconfigured_instrument_is_ignored() {
        let text = LIVE.replace("ETH-USDT", "SOL-USDT");
        assert!(client().parse(&text).unwrap().is_none());
    }

    #[test]
    fn the_subscribe_frame_names_every_configured_instrument() {
        let frames = client().subscribe_frames();
        assert_eq!(frames.len(), 1);
        assert!(frames[0].contains(r#"{"channel":"bbo-tbt","instId":"ETH-USDT"}"#));
        assert!(frames[0].contains(r#"{"channel":"bbo-tbt","instId":"BTC-USDT"}"#));
        // It has to be valid JSON, since a malformed frame is answered with a close.
        serde_json::from_str::<serde_json::Value>(&frames[0]).expect("subscribe frame must be JSON");
    }
}
