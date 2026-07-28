//! Bybit v5 spot `orderbook.1`.
//!
//! Read-only public market data. `/v5/public/spot` takes no key and no `auth` frame; the
//! authenticated stream is `/v5/private`, which this crate never names.
//!
//! # Wire format
//!
//! ```json
//! {"topic":"orderbook.1.ETHUSDT","ts":1785136396195,"type":"snapshot",
//!  "data":{"s":"ETHUSDT","b":[["1969.52","7.97197"]],"a":[["1969.53","8.54126"]],
//!          "u":22742692,"seq":177623257056},"cts":1785136396187}
//! ```
//!
//! # The one thing that makes this venue different: `snapshot` versus `delta`
//!
//! Bybit is the only venue here whose top-of-book stream is **incremental**. A `delta` frame
//! carries only the side that changed: an empty `b` means "the bid is unchanged", and a level
//! whose size is `"0"` means that level was **deleted** — which, at depth 1, means there is no
//! best bid at all until the next frame. A client that read an empty `b` as "no bid" would
//! throw away half the updates, and one that read `["1969.52","0"]` as a 1969.52 bid would price
//! against a level that no longer exists. So this module keeps the last top of book per symbol
//! and merges deltas into it, and drops the tick outright whenever a side ends up empty.
//!
//! That state is per connection and [`MarketFeed::on_connect`] clears it. Merging a post-reconnect
//! delta into a pre-outage book is the obvious way to produce a confident, wrong price.
//!
//! A `snapshot` frame is Bybit saying "reset your local book", which it does after a service
//! restart — and after a restart `u` goes back to 1. So a snapshot is applied with the sequence
//! check bypassed ([`super::FeedShared::record_reset`]) and a delta is not. Within one websocket
//! connection frames are ordered, so a snapshot can never be the *older* book; across a
//! reconnect the state is cleared anyway.
//!
//! # Keepalive
//!
//! `{"op":"ping"}` every 20s. `orderbook.1` on a liquid spot pair pushes every few milliseconds,
//! so this only matters if the market genuinely stops — which is exactly when a silent
//! disconnect would be most confusing.

use std::collections::BTreeMap;

use serde::Deserialize;

use super::ws::{MarketFeed, Update};
use super::{BookTick, VenueId};
use crate::units::{self, FEED_SCALE};

/// How often to send the keepalive.
const KEEPALIVE_SECS: u64 = 20;

#[derive(Debug, Deserialize)]
struct Book {
    s: String,
    #[serde(default)]
    b: Vec<[String; 2]>,
    #[serde(default)]
    a: Vec<[String; 2]>,
    u: u64,
}

#[derive(Debug, Deserialize)]
struct Frame {
    #[allow(dead_code)]
    topic: String,
    #[serde(rename = "type")]
    kind: String,
    data: Book,
}

/// The last top of book seen for one symbol. Zero means "that side is empty".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Top {
    bid: u128,
    bid_qty: u128,
    ask: u128,
    ask_qty: u128,
}

/// Bybit client. Holds the per-connection merge state the incremental stream needs.
pub struct Client {
    /// Venue symbol (`ETHUSDT`) -> canonical symbol.
    symbols: BTreeMap<String, String>,
    /// Last merged top of book, canonical symbol -> top. Cleared on every connect.
    tops: BTreeMap<String, Top>,
}

impl Client {
    /// Build from `(venue symbol, canonical symbol)` pairs.
    #[must_use]
    pub fn new(symbols: &[(String, String)]) -> Self {
        Self {
            symbols: symbols.iter().cloned().collect(),
            tops: BTreeMap::new(),
        }
    }
}

/// Parse one `[price, size]` level. `size == 0` means the level was deleted.
fn level(side: &str, l: &[String; 2]) -> Result<(u128, u128), String> {
    let px = units::parse_fixed(&l[0], FEED_SCALE).map_err(|e| format!("{side}[0]: {e}"))?;
    let sz = units::parse_fixed(&l[1], FEED_SCALE).map_err(|e| format!("{side}[1]: {e}"))?;
    if sz == 0 {
        return Ok((0, 0));
    }
    Ok((px, sz))
}

impl MarketFeed for Client {
    fn venue(&self) -> VenueId {
        VenueId::Bybit
    }

    fn subscribe_frames(&self) -> Vec<String> {
        let args: Vec<String> = self
            .symbols
            .keys()
            .map(|s| format!(r#""orderbook.1.{s}""#))
            .collect();
        vec![format!(
            r#"{{"op":"subscribe","args":[{}]}}"#,
            args.join(",")
        )]
    }

    fn keepalive(&self) -> Option<(std::time::Duration, String)> {
        Some((
            std::time::Duration::from_secs(KEEPALIVE_SECS),
            r#"{"op":"ping"}"#.to_string(),
        ))
    }

    fn on_connect(&mut self) {
        self.tops.clear();
    }

    fn parse(&mut self, text: &str) -> Result<Option<Update>, String> {
        // A failed subscription must not look like a quiet venue.
        if text.starts_with(r#"{"success":false"#) {
            return Err(format!("subscription rejected: {text}"));
        }
        let Ok(frame) = serde_json::from_str::<Frame>(text) else {
            return Ok(None);
        };
        let Some(symbol) = self.symbols.get(&frame.data.s).cloned() else {
            return Ok(None);
        };

        let snapshot = frame.kind == "snapshot";
        // A delta before any snapshot has nothing to merge into.
        let mut top = if snapshot {
            Top::default()
        } else {
            match self.tops.get(&symbol) {
                Some(t) => *t,
                None => return Ok(None),
            }
        };

        if let Some(b) = frame.data.b.first() {
            let (px, sz) = level("b", b)?;
            top.bid = px;
            top.bid_qty = sz;
        } else if snapshot {
            // A snapshot with no bid side really is an empty bid side.
            top.bid = 0;
            top.bid_qty = 0;
        }
        if let Some(a) = frame.data.a.first() {
            let (px, sz) = level("a", a)?;
            top.ask = px;
            top.ask_qty = sz;
        } else if snapshot {
            top.ask = 0;
            top.ask_qty = 0;
        }

        self.tops.insert(symbol.clone(), top);
        if top.bid == 0 || top.ask == 0 {
            // One side is empty. The merge state is kept — the next delta may refill it — but
            // there is no top of book to price from right now.
            return Ok(None);
        }

        let tick = BookTick {
            update_id: frame.data.u,
            bid: top.bid,
            bid_qty: top.bid_qty,
            ask: top.ask,
            ask_qty: top.ask_qty,
        };
        Ok(Some(Update {
            symbol,
            tick,
            reset: snapshot,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> Client {
        Client::new(&[
            ("ETHUSDT".into(), "ETHUSDT".into()),
            ("BTCUSDT".into(), "BTCUSDT".into()),
        ])
    }

    /// Captured verbatim off `wss://stream.bybit.com/v5/public/spot`.
    const LIVE: &str = r#"{"topic":"orderbook.1.ETHUSDT","ts":1785136396195,"type":"snapshot","data":{"s":"ETHUSDT","b":[["1969.52","7.97197"]],"a":[["1969.53","8.54126"]],"u":22742692,"seq":177623257056},"cts":1785136396187}"#;

    #[test]
    fn parses_a_real_snapshot_frame() {
        let u = client()
            .parse(LIVE)
            .unwrap()
            .expect("frame carries a book update");
        assert_eq!(u.symbol, "ETHUSDT");
        assert_eq!(u.tick.update_id, 22_742_692);
        assert_eq!(u.tick.bid, 196_952_000_000);
        assert_eq!(u.tick.bid_qty, 797_197_000);
        assert_eq!(u.tick.ask, 196_953_000_000);
        assert_eq!(u.tick.ask_qty, 854_126_000);
        assert!(u.reset, "a snapshot must bypass the sequence check");
    }

    #[test]
    fn a_delta_carrying_one_side_leaves_the_other_alone() {
        // The bug this pins: reading an empty `b` as "no bid" and throwing the update away, or
        // worse, emitting a zero bid.
        let mut c = client();
        c.parse(LIVE).unwrap().unwrap();

        let delta = r#"{"topic":"orderbook.1.ETHUSDT","ts":1,"type":"delta","data":{"s":"ETHUSDT","b":[],"a":[["1969.60","3.0"]],"u":22742693},"cts":1}"#;
        let u = c
            .parse(delta)
            .unwrap()
            .expect("a one-sided delta is still a book update");
        assert!(!u.reset, "a delta must stay sequence-checked");
        assert_eq!(
            u.tick.bid, 196_952_000_000,
            "the untouched bid must survive the delta"
        );
        assert_eq!(u.tick.bid_qty, 797_197_000);
        assert_eq!(u.tick.ask, 196_960_000_000);
        assert_eq!(u.tick.ask_qty, 300_000_000);
    }

    #[test]
    fn a_zero_size_level_is_a_deletion_and_not_a_price() {
        // `["1969.52","0"]` means the level is gone. Pricing against it would quote a book that
        // does not exist.
        let mut c = client();
        c.parse(LIVE).unwrap().unwrap();
        let deleted = r#"{"topic":"orderbook.1.ETHUSDT","ts":1,"type":"delta","data":{"s":"ETHUSDT","b":[["1969.52","0"]],"a":[],"u":22742694},"cts":1}"#;
        assert!(
            c.parse(deleted).unwrap().is_none(),
            "an empty bid side must not produce a tick"
        );

        // ... and the next delta refills it.
        let refill = r#"{"topic":"orderbook.1.ETHUSDT","ts":1,"type":"delta","data":{"s":"ETHUSDT","b":[["1969.40","2.0"]],"a":[],"u":22742695},"cts":1}"#;
        let u = c.parse(refill).unwrap().unwrap();
        assert_eq!(u.tick.bid, 196_940_000_000);
        assert_eq!(
            u.tick.ask, 196_953_000_000,
            "the ask survived the whole exchange"
        );
    }

    #[test]
    fn a_delta_before_any_snapshot_has_nothing_to_merge_into() {
        let delta = r#"{"topic":"orderbook.1.ETHUSDT","ts":1,"type":"delta","data":{"s":"ETHUSDT","b":[["1969.52","1"]],"a":[],"u":1},"cts":1}"#;
        assert!(client().parse(delta).unwrap().is_none());
    }

    #[test]
    fn a_reconnect_clears_the_merge_state() {
        // Merging a post-reconnect delta into a pre-outage book is how a confidently wrong price
        // gets built out of two individually correct frames.
        let mut c = client();
        c.parse(LIVE).unwrap().unwrap();
        c.on_connect();
        let delta = r#"{"topic":"orderbook.1.ETHUSDT","ts":1,"type":"delta","data":{"s":"ETHUSDT","b":[],"a":[["1969.60","3.0"]],"u":22742693},"cts":1}"#;
        assert!(c.parse(delta).unwrap().is_none());
    }

    #[test]
    fn control_frames_are_ignored_and_a_rejected_subscription_is_not() {
        let mut c = client();
        assert!(c
            .parse(r#"{"success":true,"ret_msg":"subscribe","conn_id":"abc","op":"subscribe"}"#)
            .unwrap()
            .is_none());
        assert!(c
            .parse(r#"{"op":"pong","success":true}"#)
            .unwrap()
            .is_none());
        assert!(c
            .parse(r#"{"success":false,"ret_msg":"Invalid symbol","op":"subscribe"}"#)
            .unwrap_err()
            .contains("Invalid symbol"));
    }

    #[test]
    fn an_unconfigured_symbol_is_ignored() {
        let text = LIVE.replace("ETHUSDT", "SOLUSDT");
        assert!(client().parse(&text).unwrap().is_none());
    }

    #[test]
    fn the_subscribe_frame_names_every_configured_topic() {
        let frames = client().subscribe_frames();
        assert_eq!(frames.len(), 1);
        assert!(frames[0].contains(r#""orderbook.1.ETHUSDT""#));
        assert!(frames[0].contains(r#""orderbook.1.BTCUSDT""#));
        serde_json::from_str::<serde_json::Value>(&frames[0])
            .expect("subscribe frame must be JSON");
    }
}
