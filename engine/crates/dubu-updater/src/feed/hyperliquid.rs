//! Hyperliquid `l2Book`, for the markets no centralised exchange carries.
//!
//! # Why a fifth venue
//!
//! The other four are crypto spot exchanges. None of them lists a share, so the four equity pairs
//! on the pool — AAPL, TSLA, SKHY and SPCX — had no reference at all and the config validator
//! refused to start rather than run a market that could never quote. This is what gives them one.
//!
//! Hyperliquid carries them through HIP-3: builder-deployed perp markets settling on its shared
//! order book. Measured live, `xyz:AAPL` quotes 338.92 / 338.94 — a 0.6 bp spread, on a real book
//! with real depth.
//!
//! # The dex is part of the symbol, and that is not cosmetic
//!
//! HIP-3 lets each builder attach its own oracle, and they disagree. Measured in the same second:
//! `xyz:TSLA` 307.31, `flx:TSLA` 395.50, `cash:TSLA` 400.21 — thirty percent apart. So `xyz:TSLA`
//! and `flx:TSLA` are different markets that happen to share a ticker, and the venue symbol carries
//! the builder for the same reason OKX's carries a hyphen: it is that venue's name for the thing.
//!
//! # One venue is below quorum, and that is the honest state
//!
//! `feed.min_venues` is 2. A pair fed only from here will sit below quorum and quote nothing, which
//! is correct and not a limitation to work around: a single price source has nobody to disagree
//! with it, and the whole cross-venue arrangement exists to stop one venue's malfunction being
//! read as the truth. Quoting equities means a second equity source — another HIP-3 builder is the
//! obvious one, and the disagreement above is exactly why it is worth having.
//!
//! # Perps, priced against spot
//!
//! These are perpetual futures and the pool's tokens are spot. A perp trades near its index but
//! carries a funding basis, so the reference is a few bp off the underlying by construction. That
//! is a real bias and it is not corrected here; correcting it needs the funding rate, which is a
//! separate subscription and a separate decision about whether the pool wants to wear the basis or
//! quote around it.

use std::collections::BTreeMap;

use serde::Deserialize;

use super::ws::{MarketFeed, Update};
use super::{BookTick, VenueId};
use crate::units::{self, FEED_SCALE};

/// One side's level: `{"px": "338.92", "sz": "2.951", "n": 3}`.
#[derive(Debug, Deserialize)]
struct Level {
    px: String,
    sz: String,
}

#[derive(Debug, Deserialize)]
struct BookData {
    coin: String,
    /// `[bids, asks]`, best first. Always two entries, either of which may be empty.
    levels: Vec<Vec<Level>>,
    /// Venue timestamp in milliseconds. Used as the sequence number; see [`Client::parse`].
    time: u64,
}

#[derive(Debug, Deserialize)]
struct Frame {
    channel: String,
    data: serde_json::Value,
}

/// Hyperliquid client.
pub struct Client {
    /// Venue symbol (`xyz:AAPL`) -> canonical symbol (`AAPL`).
    symbols: BTreeMap<String, String>,
}

impl Client {
    /// Build from `(venue symbol, canonical symbol)` pairs.
    #[must_use]
    pub fn new(symbols: &[(String, String)]) -> Self {
        Self {
            symbols: symbols.iter().cloned().collect(),
        }
    }
}

impl MarketFeed for Client {
    fn venue(&self) -> VenueId {
        VenueId::Hyperliquid
    }

    fn subscribe_frames(&self) -> Vec<String> {
        // One frame per market. The venue takes a single subscription per message, unlike OKX's
        // batched `args` array, so a pool quoting four equities sends four frames.
        self.symbols
            .keys()
            .map(|s| {
                format!(
                    r#"{{"method":"subscribe","subscription":{{"type":"l2Book","coin":"{s}"}}}}"#
                )
            })
            .collect()
    }

    fn parse(&mut self, text: &str) -> Result<Option<Update>, String> {
        let Ok(frame) = serde_json::from_str::<Frame>(text) else {
            return Ok(None);
        };
        match frame.channel.as_str() {
            "l2Book" => {}
            // A rejected subscription is worth surfacing: it otherwise looks exactly like a market
            // with nothing to say, and the pair silently sits below quorum forever.
            "error" => return Err(format!("venue error: {}", frame.data)),
            // `subscriptionResponse`, `pong`, and anything else that is not a book.
            _ => return Ok(None),
        }

        let Ok(book) = serde_json::from_value::<BookData>(frame.data) else {
            return Err("l2Book payload did not decode".to_string());
        };
        let Some(symbol) = self.symbols.get(&book.coin) else {
            return Ok(None);
        };
        let (Some(bids), Some(asks)) = (book.levels.first(), book.levels.get(1)) else {
            return Err("levels was not [bids, asks]".to_string());
        };
        let (Some(bid), Some(ask)) = (bids.first(), asks.first()) else {
            // A book with a side missing is not a top of book. Dropping it is right: the
            // micro-price would be meaningless and a zero would be worse.
            return Ok(None);
        };

        let f = |field: &str, v: &str| -> Result<u128, String> {
            units::parse_fixed(v, FEED_SCALE).map_err(|e| format!("{field}: {e}"))
        };

        Ok(Some(Update {
            symbol: symbol.clone(),
            tick: BookTick {
                // The venue publishes no update id, so the frame's own millisecond timestamp is the
                // sequence. It is monotone per market, which is all the gap check needs -- and a
                // frame that arrives out of order carries an older time, so it is dropped exactly
                // as a regressed id would be.
                update_id: book.time,
                bid: f("bid px", &bid.px)?,
                bid_qty: f("bid sz", &bid.sz)?,
                ask: f("ask px", &ask.px)?,
                ask_qty: f("ask sz", &ask.sz)?,
            },
            // Every `l2Book` frame carries the whole top of book rather than a delta, so there is
            // no per-connection state to reset and nothing to merge against.
            reset: false,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured verbatim from `wss://api.hyperliquid.xyz/ws` on 2026-07-28.
    const LIVE: &str = r#"{"channel":"l2Book","data":{"coin":"xyz:AAPL","time":1785263000123,"levels":[[{"px":"338.92","sz":"2.951","n":3},{"px":"338.90","sz":"11.2","n":5}],[{"px":"338.94","sz":"20.806","n":7},{"px":"338.96","sz":"4.1","n":2}]]}}"#;

    fn client() -> Client {
        Client::new(&[
            ("xyz:AAPL".to_string(), "AAPL".to_string()),
            ("xyz:TSLA".to_string(), "TSLA".to_string()),
        ])
    }

    #[test]
    fn the_top_of_book_decodes_at_feed_scale() {
        let u = client().parse(LIVE).unwrap().unwrap();
        assert_eq!(u.tick.update_id, 1_785_263_000_123);
        // FEED_SCALE is 8 decimals: 338.92 -> 33_892_000_000.
        assert_eq!(u.tick.bid, 33_892_000_000);
        assert_eq!(u.tick.ask, 33_894_000_000);
        assert_eq!(u.tick.bid_qty, 295_100_000);
        assert_eq!(u.tick.ask_qty, 2_080_600_000);
        assert!(!u.reset, "l2Book is a snapshot, not a delta");
    }

    /// The builder prefix is the venue's name for the market, and the pair is configured under the
    /// plain ticker. Recording under `xyz:AAPL` would leave this venue in its own namespace and the
    /// cross-section permanently empty -- the same mistake OKX's hyphen would cause.
    #[test]
    fn the_venue_symbol_is_translated_to_the_canonical_one() {
        let u = client().parse(LIVE).unwrap().unwrap();
        assert_eq!(u.symbol, "AAPL");
    }

    /// Two builders quote the same ticker at prices thirty percent apart, so only the configured
    /// one may be recorded. An unconfigured book is ignored rather than guessed at.
    #[test]
    fn another_builders_book_for_the_same_ticker_is_not_ours() {
        let other = LIVE.replace("xyz:AAPL", "flx:AAPL");
        assert!(
            client().parse(&other).unwrap().is_none(),
            "flx is a different market that happens to share a ticker"
        );
    }

    #[test]
    fn control_frames_are_ignored_and_a_rejected_subscription_is_not() {
        let mut c = client();
        assert!(c
            .parse(r#"{"channel":"subscriptionResponse","data":{"method":"subscribe"}}"#)
            .unwrap()
            .is_none());
        assert!(c
            .parse(r#"{"channel":"pong","data":{}}"#)
            .unwrap()
            .is_none());
        assert!(
            c.parse(r#"{"channel":"error","data":"Invalid subscription"}"#)
                .is_err(),
            "a rejected subscription looks exactly like a quiet market unless it is surfaced"
        );
    }

    /// A one-sided book has no micro-price. Dropping it beats publishing a zero.
    #[test]
    fn a_book_missing_a_side_is_dropped() {
        let empty_ask = LIVE.replace(
            r#"[{"px":"338.94","sz":"20.806","n":7},{"px":"338.96","sz":"4.1","n":2}]"#,
            "[]",
        );
        assert!(client().parse(&empty_ask).unwrap().is_none());
    }

    /// One subscription frame per market: the venue takes a single subscription per message.
    #[test]
    fn every_market_gets_its_own_subscribe_frame() {
        let frames = client().subscribe_frames();
        assert_eq!(frames.len(), 2);
        assert!(frames.iter().any(|f| f.contains("xyz:AAPL")));
        assert!(frames.iter().any(|f| f.contains("xyz:TSLA")));
    }
}
