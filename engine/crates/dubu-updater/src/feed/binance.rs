//! Binance `bookTicker` websocket client.
//!
//! Read-only public market data. **No credentials, no account endpoints, no order entry**, and
//! nothing in this file can be extended into any of those without adding a signing step that
//! does not exist. `bookTicker` needs no API key precisely because it is public observation
//! rather than participation — which is also why it is available to a desk that has no exchange
//! account, and this desk has none.
//!
//! # Wire format
//!
//! Combined-stream endpoint, so every frame is wrapped:
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
//! # Liveness
//!
//! Three independent mechanisms, because they fail differently:
//!
//! * **Read timeout.** Binance sends a server ping every ~20s, so a socket that produces no
//!   frame at all for `read_timeout_ms` is dead regardless of what TCP believes. Forces a
//!   reconnect.
//! * **Reconnect backoff.** Exponential from `reconnect_initial_ms` to `reconnect_max_ms`. A
//!   reconnect loop against a rate-limiting endpoint is its own outage.
//! * **Staleness.** Owned by [`super::FeedShared`], not here: the socket being up says nothing
//!   about a particular symbol still ticking.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

use super::{BookTick, FeedShared};
use crate::config::FeedConfig;
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
/// deserialise to `None`, which is how they get ignored without a special case.
#[derive(Debug, Deserialize)]
struct Envelope {
    #[allow(dead_code)]
    stream: String,
    data: BookTickerData,
}

/// Parse one text frame into a symbol and a tick.
///
/// Returns `Ok(None)` for a frame that is well-formed but carries no book update — Binance's
/// subscribe acknowledgements, which arrive on the same socket.
fn parse_frame(text: &str) -> Result<Option<(String, BookTick)>, String> {
    // Try the combined-stream envelope first, then the raw single-stream shape, then give up.
    let data: BookTickerData = match serde_json::from_str::<Envelope>(text) {
        Ok(e) => e.data,
        Err(_) => match serde_json::from_str::<BookTickerData>(text) {
            Ok(d) => d,
            Err(_) => return Ok(None),
        },
    };

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
    Ok(Some((data.s.to_uppercase(), tick)))
}

/// Run the feed until `shutdown` flips, reconnecting on every failure.
///
/// Never returns an error: a feed that cannot connect is a *state* the rest of the system reads
/// off [`FeedShared`], not a failure that should take the process down. The process still has to
/// withdraw quotes on the way out, and it cannot do that if the feed task panics it first.
pub async fn run(cfg: FeedConfig, streams: Vec<String>, shared: Arc<FeedShared>, mut shutdown: watch::Receiver<bool>) {
    let url = format!("{}?streams={}", cfg.ws_url.trim_end_matches('/'), streams.join("/"));
    let mut delay = Duration::from_millis(cfg.reconnect_initial_ms);
    let max_delay = Duration::from_millis(cfg.reconnect_max_ms);
    let read_timeout = Duration::from_millis(cfg.read_timeout_ms);
    let mut first = true;

    loop {
        if *shutdown.borrow() {
            return;
        }

        let connect = tokio_tungstenite::connect_async(&url);
        let outcome = tokio::select! {
            biased;
            _ = shutdown.changed() => return,
            r = tokio::time::timeout(read_timeout, connect) => r,
        };

        match outcome {
            Ok(Ok((mut socket, _resp))) => {
                info!(target: "feed", event = "connected", streams = %streams.join(","), "market data feed connected");
                shared.on_connected(first);
                first = false;
                delay = Duration::from_millis(cfg.reconnect_initial_ms);

                loop {
                    let next = tokio::select! {
                        biased;
                        _ = shutdown.changed() => {
                            let _ = socket.close(None).await;
                            shared.on_disconnected();
                            return;
                        }
                        r = tokio::time::timeout(read_timeout, socket.next()) => r,
                    };
                    let msg = match next {
                        Err(_) => {
                            warn!(target: "feed", event = "read_timeout", timeout_ms = cfg.read_timeout_ms,
                                  "no frame within the read timeout; reconnecting");
                            break;
                        }
                        Ok(None) => {
                            warn!(target: "feed", event = "stream_ended", "socket closed by peer");
                            break;
                        }
                        Ok(Some(Err(e))) => {
                            warn!(target: "feed", event = "read_error", error = %e, "websocket read failed");
                            break;
                        }
                        Ok(Some(Ok(m))) => m,
                    };

                    match msg {
                        Message::Text(text) => match parse_frame(&text) {
                            Ok(Some((symbol, tick))) => {
                                if let Err(rej) = shared.record(&symbol, tick, Instant::now()) {
                                    debug!(target: "feed", event = "tick_rejected", symbol = %symbol,
                                           reason = ?rej, "dropped a book tick");
                                }
                            }
                            Ok(None) => {}
                            Err(e) => warn!(target: "feed", event = "parse_error", error = %e,
                                            "unparseable frame; dropped"),
                        },
                        // tungstenite queues its own pong, but only flushes it when the sink is
                        // polled, and this loop polls the stream. Answering explicitly is one
                        // frame every 20s and removes the dependency on that detail.
                        Message::Ping(p) => {
                            if socket.send(Message::Pong(p)).await.is_err() {
                                break;
                            }
                        }
                        Message::Close(f) => {
                            warn!(target: "feed", event = "close_frame", frame = ?f, "peer closed the stream");
                            break;
                        }
                        _ => {}
                    }
                }
                shared.on_disconnected();
            }
            Ok(Err(e)) => {
                shared.on_disconnected();
                warn!(target: "feed", event = "connect_failed", error = %e, retry_in_ms = delay.as_millis(),
                      "could not connect to the market data feed");
            }
            Err(_) => {
                shared.on_disconnected();
                warn!(target: "feed", event = "connect_timeout", retry_in_ms = delay.as_millis(),
                      "connection attempt timed out");
            }
        }

        tokio::select! {
            biased;
            _ = shutdown.changed() => return,
            () = tokio::time::sleep(delay) => {}
        }
        delay = (delay * 2).min(max_delay);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_real_combined_stream_frame() {
        let text = r#"{"stream":"ethusdt@bookTicker","data":{"u":400900217,"s":"ETHUSDT",
            "b":"1943.82000000","B":"24.25800000","a":"1943.83000000","A":"13.78110000"}}"#;
        let (symbol, tick) = parse_frame(text).unwrap().expect("frame carries a book update");
        assert_eq!(symbol, "ETHUSDT");
        assert_eq!(tick.update_id, 400_900_217);
        assert_eq!(tick.bid, 194_382_000_000);
        assert_eq!(tick.bid_qty, 2_425_800_000);
        assert_eq!(tick.ask, 194_383_000_000);
        assert_eq!(tick.ask_qty, 1_378_110_000);
    }

    #[test]
    fn parses_the_raw_single_stream_shape_too() {
        let text = r#"{"u":1,"s":"BTCUSDT","b":"118000.10","B":"0.5","a":"118000.20","A":"1.25"}"#;
        let (symbol, tick) = parse_frame(text).unwrap().unwrap();
        assert_eq!(symbol, "BTCUSDT");
        assert_eq!(tick.bid, 11_800_010_000_000);
        assert_eq!(tick.ask_qty, 125_000_000);
    }

    #[test]
    fn subscribe_acknowledgements_are_ignored_not_errors() {
        assert!(parse_frame(r#"{"result":null,"id":1}"#).unwrap().is_none());
        assert!(parse_frame("not json at all").unwrap().is_none());
    }

    #[test]
    fn a_malformed_number_is_an_error_rather_than_a_zero() {
        // The failure this prevents: `bid = 0` sailing through into a fair value.
        let text = r#"{"u":1,"s":"ETHUSDT","b":"1.9e3","B":"1","a":"2000","A":"1"}"#;
        let err = parse_frame(text).unwrap_err();
        assert!(err.starts_with("b:"), "error must name the offending field, got `{err}`");
    }
}
