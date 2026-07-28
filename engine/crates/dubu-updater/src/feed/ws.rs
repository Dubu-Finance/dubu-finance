//! The websocket driver every venue shares.
//!
//! Four venues, four wire formats, and exactly one reconnect loop. The loop is the part that is
//! easy to get subtly wrong — backoff that resets at the wrong moment, a read timeout that fires
//! during a quiet period, a disconnect that leaves the last price readable — and four copies of
//! it would drift apart in ways nobody notices until one venue is quietly dead. So the venues
//! implement [`MarketFeed`], which is a parser and a subscribe frame, and this module owns
//! everything else.
//!
//! # Liveness, three mechanisms, because they fail differently
//!
//! * **Read timeout.** Every venue here either pushes continuously or answers a keepalive, so a
//!   socket that produces no frame at all for `read_timeout_ms` is dead regardless of what TCP
//!   believes. Forces a reconnect.
//! * **Keepalive.** OKX closes an idle connection after 30s and wants a literal `ping` text
//!   frame; [`MarketFeed::keepalive`] is how a venue asks for one. Venues that need nothing say
//!   nothing.
//! * **Staleness.** Owned by [`FeedShared`], not here: the socket being up says nothing about a
//!   particular symbol still ticking, and that distinction is what
//!   [`super::VenueStatus::Stale`] exists to express.
//!
//! # Text and binary
//!
//! Every venue here sends text today. Frames arriving as binary are still decoded as UTF-8 and
//! parsed, because RFC 6455 leaves the choice to the application and a client that matches on the
//! opcode instead of the payload silently drops every frame from a peer that changes its mind.
//! That failure gives no signal pointing at the cause — the handshake returns 101, frames arrive
//! on schedule, and it presents as "the venue never sent anything". `chain::heads` learned this
//! the expensive way against Nodit.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

use super::{BookTick, FeedShared, VenueId};
use crate::config::FeedConfig;

/// One parsed book update, and whether the venue said its book was reset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Update {
    /// The **canonical** symbol, already translated out of the venue's own naming.
    pub symbol: String,
    /// The top of book.
    pub tick: BookTick,
    /// The venue's protocol says this frame replaces the book rather than following it, so the
    /// sequence check must be bypassed. Only Bybit's `snapshot` sets it; see
    /// [`FeedShared::record_reset`].
    pub reset: bool,
}

/// What a venue has to supply. Everything else — connecting, backoff, timeouts, the shared
/// state — belongs to [`run`].
pub trait MarketFeed: Send {
    /// Which venue this is.
    fn venue(&self) -> VenueId;

    /// The URL to connect to, given the configured base endpoint.
    ///
    /// Binance carries its subscription list in the query string and overrides this; every other
    /// venue subscribes over the socket and takes the base URL unchanged.
    fn url(&self, base: &str) -> String {
        base.to_string()
    }

    /// Text frames to send immediately after the handshake, in order.
    ///
    /// Binance carries the subscription in the URL and returns an empty list; the others send a
    /// JSON subscribe message.
    fn subscribe_frames(&self) -> Vec<String>;

    /// A keepalive frame and how often to send it, for a venue that closes an idle socket.
    fn keepalive(&self) -> Option<(Duration, String)> {
        None
    }

    /// Drop any per-connection parser state. Called before every subscribe.
    ///
    /// Bybit's `orderbook.1` deltas are merged against a stored top of book, and carrying that
    /// across a reconnect would merge new deltas into a pre-outage book.
    fn on_connect(&mut self) {}

    /// Parse one frame.
    ///
    /// `Ok(None)` for a frame that is well-formed and carries no book update — subscribe
    /// acknowledgements, heartbeats, pongs — so that ignoring them needs no special case here.
    ///
    /// # Errors
    /// A human-readable reason, which is logged and the frame dropped. A parse error is never
    /// fatal: one venue's schema changing must degrade that venue, not the process.
    fn parse(&mut self, text: &str) -> Result<Option<Update>, String>;
}

/// Run one venue's feed until `shutdown` flips, reconnecting on every failure.
///
/// Never returns an error: a venue that cannot connect is a *state* the rest of the system reads
/// off [`FeedShared`], not a failure that should take the process down. The process still has to
/// withdraw quotes on the way out, and it cannot do that if a feed task panics it first.
pub async fn run(
    cfg: FeedConfig,
    base_url: String,
    mut client: Box<dyn MarketFeed>,
    shared: Arc<FeedShared>,
    mut shutdown: watch::Receiver<bool>,
) {
    let venue = client.venue();
    let url = client.url(&base_url);
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
                client.on_connect();
                let mut subscribe_failed = false;
                for frame in client.subscribe_frames() {
                    if socket.send(Message::Text(frame)).await.is_err() {
                        warn!(target: "feed", event = "subscribe_failed", venue = %venue,
                              "could not send the subscribe frame");
                        subscribe_failed = true;
                        break;
                    }
                }

                if !subscribe_failed {
                    info!(target: "feed", event = "connected", venue = %venue,
                          "market data venue connected");
                    shared.on_connected(first);
                    first = false;
                    delay = Duration::from_millis(cfg.reconnect_initial_ms);
                    read_loop(
                        &mut client,
                        &mut socket,
                        &shared,
                        read_timeout,
                        &mut shutdown,
                    )
                    .await;
                }
                shared.on_disconnected();
                if subscribe_failed {
                    // Fall through to the backoff rather than hot-looping on a broken endpoint.
                } else if *shutdown.borrow() {
                    return;
                }
            }
            Ok(Err(e)) => {
                shared.on_disconnected();
                warn!(target: "feed", event = "connect_failed", venue = %venue, error = %e,
                      retry_in_ms = delay.as_millis(), "could not connect to the venue");
            }
            Err(_) => {
                shared.on_disconnected();
                warn!(target: "feed", event = "connect_timeout", venue = %venue,
                      retry_in_ms = delay.as_millis(), "connection attempt timed out");
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

/// Read frames until something goes wrong. Returns so the caller can reconnect.
async fn read_loop(
    client: &mut Box<dyn MarketFeed>,
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    shared: &Arc<FeedShared>,
    read_timeout: Duration,
    shutdown: &mut watch::Receiver<bool>,
) {
    let venue = client.venue();
    let keepalive = client.keepalive();
    // A period no venue will ever reach, so the `select!` arm below can be unconditional.
    let mut ticker = tokio::time::interval(
        keepalive
            .as_ref()
            .map_or(Duration::from_secs(3_600), |k| k.0),
    );
    ticker.tick().await; // `interval` fires immediately; the connection is fresh.

    loop {
        let next = tokio::select! {
            biased;
            _ = shutdown.changed() => {
                let _ = socket.close(None).await;
                return;
            }
            _ = ticker.tick() => {
                if let Some((_, frame)) = &keepalive {
                    if socket.send(Message::Text(frame.clone())).await.is_err() {
                        warn!(target: "feed", event = "keepalive_failed", venue = %venue,
                              "could not send the keepalive frame");
                        return;
                    }
                }
                continue;
            }
            r = tokio::time::timeout(read_timeout, socket.next()) => r,
        };

        let msg = match next {
            Err(_) => {
                warn!(target: "feed", event = "read_timeout", venue = %venue,
                      timeout_ms = read_timeout.as_millis(),
                      "no frame within the read timeout; reconnecting");
                return;
            }
            Ok(None) => {
                warn!(target: "feed", event = "stream_ended", venue = %venue, "socket closed by peer");
                return;
            }
            Ok(Some(Err(e))) => {
                warn!(target: "feed", event = "read_error", venue = %venue, error = %e,
                      "websocket read failed");
                return;
            }
            Ok(Some(Ok(m))) => m,
        };

        // Text and binary both carry payloads; see the module docs on why the opcode is not the
        // thing to match on.
        let payload: Option<String> = match msg {
            Message::Text(t) => Some(t),
            Message::Binary(b) => String::from_utf8(b).ok(),
            // tungstenite queues its own pong but only flushes it when the sink is polled, and
            // this loop polls the stream. Answering explicitly removes the dependency on that.
            Message::Ping(p) => {
                if socket.send(Message::Pong(p)).await.is_err() {
                    return;
                }
                None
            }
            Message::Close(f) => {
                warn!(target: "feed", event = "close_frame", venue = %venue, frame = ?f,
                      "peer closed the stream");
                return;
            }
            _ => None,
        };
        let Some(text) = payload else { continue };

        match client.parse(&text) {
            Ok(Some(update)) => {
                let now = Instant::now();
                if update.reset {
                    shared.record_reset(&update.symbol, update.tick, now);
                } else if let Err(rej) = shared.record(&update.symbol, update.tick, now) {
                    debug!(target: "feed", event = "tick_rejected", venue = %venue,
                           symbol = %update.symbol, reason = ?rej, "dropped a book tick");
                }
            }
            Ok(None) => {}
            Err(e) => warn!(target: "feed", event = "parse_error", venue = %venue, error = %e,
                            "unparseable frame; dropped"),
        }
    }
}
