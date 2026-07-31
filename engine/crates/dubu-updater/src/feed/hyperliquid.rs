//! Hyperliquid `l2Book`, for the markets no centralised exchange carries.
//!
//! # Why a fifth venue
//!
//! The other four are crypto spot exchanges. None of them lists a share, so the four equity pairs
//! on the pool — AAPL, TSLA, SKHY and SPCX — had no reference at all and the config validator
//! refused to start. Hyperliquid carries them through HIP-3: builder-deployed perp markets
//! settling on its shared order book. Measured live, `xyz:AAPL` quotes 338.92 / 338.94 — a 0.6 bp
//! spread, on a real book with real depth.
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
//! `feed.venues_min` is 2, so a pair fed only from here sits below quorum and quotes nothing. That
//! is correct rather than a limitation to work around: a single price source has nobody to
//! disagree with it. Quoting equities means a second equity source — another HIP-3 builder is the
//! obvious one, and the disagreement above is why it is worth having.
//!
//! # Perps, priced against spot
//!
//! These are perpetual futures and the pool's tokens are spot. A perp trades near its index but
//! carries a funding basis, so the reference is a few bp off the underlying by construction. That
//! bias is not corrected here; correcting it needs the funding rate, which is a separate
//! subscription and a separate decision about whether the pool wears the basis or quotes around it.

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

/// `activeAssetCtx.ctx`, the fields this reads.
#[derive(Debug, Deserialize)]
struct Ctx {
    /// The externally derived price. Not `markPx` or `midPx`: those are the perp's own traded
    /// level and carry its funding basis, and the pool quotes spot.
    #[serde(rename = "oraclePx")]
    oracle_px: String,
    /// `[bid, ask]` at the venue's configured notional. Absent on a market with no depth.
    #[serde(rename = "impactPxs")]
    impact_pxs: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct CtxData {
    coin: String,
    ctx: Ctx,
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
    /// Monotone id for `activeAssetCtx` frames, which carry no sequence of their own.
    ctx_seq: u64,
}

impl Client {
    /// Build from `(venue symbol, canonical symbol)` pairs.
    ///
    /// # Panics
    /// If any symbol is empty. A client built from a blank symbol subscribes to nothing and
    /// reads as a healthy silent venue.
    #[must_use]
    pub fn new(symbols: &[(String, String)]) -> Self {
        for (venue, canonical) in symbols {
            assert!(!venue.is_empty(), "a hyperliquid coin must not be blank");
            assert!(
                !canonical.is_empty(),
                "a canonical symbol must not be blank"
            );
        }
        let client = Self {
            symbols: symbols.iter().cloned().collect(),
            ctx_seq: 0,
        };
        debug_assert_eq!(
            client.symbols.len(),
            symbols.len(),
            "two pairs claim the same hyperliquid coin, so one is silently unsubscribed"
        );
        client
    }
}

impl Client {
    /// One `activeAssetCtx` frame into a top of book centred on the oracle.
    ///
    /// This is the channel the venue is obliged to publish on: HIP-3 requires the deployer to call
    /// `setOracle` every three seconds whether or not anything moved, so it ticks about once a
    /// second with no gaps. `l2Book` carries the same market but only when the book itself changes,
    /// which overnight is once every five seconds -- measured 2026-07-31 as 16 gaps past the
    /// staleness window in 90 seconds, which read downstream as the venue being dead for a quarter
    /// of the day.
    ///
    /// `oraclePx` is the centre rather than `midPx` or `markPx`. Those are where the perp trades
    /// and carry its funding basis; the oracle is derived from the underlying equity venues, which
    /// is what a spot pool wants. Sizes are equal so the size-weighted micro-price lands exactly on
    /// it, and the width comes from `impactPxs`, the venue's own two-sided quote at depth.
    fn parse_ctx(&mut self, data: serde_json::Value) -> Result<Option<Update>, String> {
        let Ok(payload) = serde_json::from_value::<CtxData>(data) else {
            return Err("activeAssetCtx payload did not decode".to_string());
        };
        let Some(symbol) = self.symbols.get(&payload.coin) else {
            return Ok(None);
        };
        let px = |field: &str, v: &str| -> Result<u128, String> {
            units::parse_fixed(v, FEED_SCALE).map_err(|e| format!("{field}: {e}"))
        };

        let oracle = px("oraclePx", &payload.ctx.oracle_px)?;
        if oracle == 0 {
            // A zero centre would make the whole book zero, which reads downstream as a price
            // rather than as the absence of one.
            return Ok(None);
        }

        // Half the venue's own impact spread, or one unit when it publishes none. `micro_price`
        // requires bid < ask strictly, so the floor is what keeps a zero-width quote representable.
        let half = match payload.ctx.impact_pxs.as_deref() {
            Some([bid, ask]) => {
                let (b, a) = (px("impact bid", bid)?, px("impact ask", ask)?);
                a.saturating_sub(b) / 2
            }
            _ => 0,
        }
        .max(1);

        self.ctx_seq = self.ctx_seq.saturating_add(1);
        Ok(Some(Update {
            symbol: symbol.clone(),
            tick: BookTick {
                // The frame carries no sequence of its own, so this counter is one. It never shares
                // an id space with `l2Book`'s millisecond timestamps, because `subscribe_frames`
                // asks for this channel alone.
                update_id: self.ctx_seq,
                bid: oracle.saturating_sub(half),
                bid_qty: 1,
                ask: oracle.saturating_add(half),
                ask_qty: 1,
            },
            reset: false,
        }))
    }
}

impl MarketFeed for Client {
    fn venue(&self) -> VenueId {
        VenueId::Hyperliquid
    }

    fn subscribe_frames(&self) -> Vec<String> {
        assert!(
            !self.symbols.is_empty(),
            "subscribing to no market never delivers a book"
        );
        // One frame per market: the venue takes a single subscription per message, unlike OKX's
        // batched `args` array.
        let frames: Vec<String> = self
            .symbols
            .keys()
            .map(|s| {
                format!(
                    r#"{{"method":"subscribe","subscription":{{"type":"activeAssetCtx","coin":"{s}"}}}}"#
                )
            })
            .collect();
        assert_eq!(frames.len(), self.symbols.len());
        debug_assert!(frames
            .iter()
            .all(|f| serde_json::from_str::<serde_json::Value>(f).is_ok()));
        frames
    }

    fn parse(&mut self, text: &str) -> Result<Option<Update>, String> {
        let Ok(frame) = serde_json::from_str::<Frame>(text) else {
            return Ok(None);
        };
        match frame.channel.as_str() {
            "activeAssetCtx" => return self.parse_ctx(frame.data),
            "l2Book" => {}
            // A rejected subscription otherwise looks exactly like a market with nothing to say,
            // and the pair silently sits below quorum forever.
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
        debug_assert!(!symbol.is_empty(), "`new` rejects a blank canonical symbol");
        let (Some(bids), Some(asks)) = (book.levels.first(), book.levels.get(1)) else {
            return Err("levels was not [bids, asks]".to_string());
        };
        assert!(book.levels.len() >= 2, "both sides were just indexed");
        let (Some(bid), Some(ask)) = (bids.first(), asks.first()) else {
            // A book with a side missing is not a top of book. Dropping it is right: the
            // micro-price would be meaningless and a zero would be worse.
            return Ok(None);
        };
        assert!(!bids.is_empty());
        assert!(!asks.is_empty());

        let f = |field: &str, v: &str| -> Result<u128, String> {
            units::parse_fixed(v, FEED_SCALE).map_err(|e| format!("{field}: {e}"))
        };

        Ok(Some(Update {
            symbol: symbol.clone(),
            tick: BookTick {
                // No update id is published, so the frame's own millisecond timestamp is the
                // sequence. It is monotone per market, and a frame that arrives out of order
                // carries an older time, so it is dropped exactly as a regressed id would be.
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

    /// Captured verbatim from `wss://api.hyperliquid.xyz/ws` on 2026-07-31, overnight.
    const CTX: &str = r#"{"channel":"activeAssetCtx","data":{"coin":"xyz:AAPL","ctx":{"funding":"0.0000074304","openInterest":"221413.164","prevDayPx":"337.34","dayNtlVlm":"172946726.7845301032","premium":"0.0002355392","oraclePx":"312.05","markPx":"312.12","midPx":"312.12","impactPxs":["312.094","312.153"],"dayBaseVlm":"534571.812"}}}"#;

    #[test]
    fn the_ctx_frame_centres_the_book_on_the_oracle_not_the_perp() {
        let u = client().parse(CTX).unwrap().unwrap();
        assert_eq!(u.symbol, "AAPL");
        // impact spread is 312.153 - 312.094 = 0.059, so half is 0.0295 -> 2_950_000 at 8 dp.
        assert_eq!(u.tick.bid, 31_205_000_000 - 2_950_000);
        assert_eq!(u.tick.ask, 31_205_000_000 + 2_950_000);
        // Equal sizes, so the size-weighted micro-price is the midpoint, which is the oracle.
        assert_eq!(u.tick.bid_qty, u.tick.ask_qty);
        assert_eq!((u.tick.bid + u.tick.ask) / 2, 31_205_000_000);
        // Deliberately NOT markPx or midPx: both are 312.12 and carry the perp's funding basis,
        // which is 2.2 bp away from the oracle here and would bias a spot pool by that much.
        assert_ne!((u.tick.bid + u.tick.ask) / 2, 31_212_000_000);
    }

    #[test]
    fn the_ctx_id_advances_so_a_repeated_price_is_still_a_new_tick() {
        let mut c = client();
        let a = c.parse(CTX).unwrap().unwrap().tick.update_id;
        let b = c.parse(CTX).unwrap().unwrap().tick.update_id;
        // The oracle republishes every three seconds whether or not the price moved. Reusing an id
        // would have the feed drop the frame as out of order and the venue would look dead again.
        assert!(b > a, "an unchanged oracle must still count as a tick");
    }

    #[test]
    fn a_market_with_no_impact_quote_still_yields_a_representable_book() {
        let no_impact = CTX.replace(r#""impactPxs":["312.094","312.153"],"#, "");
        let u = client().parse(&no_impact).unwrap().unwrap();
        // `micro_price` requires bid < ask strictly, so a missing width floors at one unit rather
        // than collapsing the book to a point.
        assert!(u.tick.bid < u.tick.ask);
        assert_eq!(u.tick.ask - u.tick.bid, 2);
    }

    #[test]
    fn a_coin_we_did_not_subscribe_to_is_ignored() {
        let other = CTX.replace("xyz:AAPL", "xyz:NVDA");
        assert!(client().parse(&other).unwrap().is_none());
    }

    #[test]
    fn the_subscription_asks_for_the_channel_that_does_not_gap() {
        let frames = client().subscribe_frames();
        assert_eq!(frames.len(), 2);
        for f in &frames {
            assert!(f.contains("activeAssetCtx"), "{f}");
            assert!(
                !f.contains("l2Book"),
                "l2Book gaps for five seconds overnight: {f}"
            );
        }
    }

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
