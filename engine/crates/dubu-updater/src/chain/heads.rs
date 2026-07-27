//! The `newHeads` subscription: what drives the quote loop.
//!
//! # Why this replaced a timer
//!
//! The loop used to run on a poll interval because GIWA's public RPC answers 405 to a websocket
//! upgrade and has no `eth_subscribe` at all. A dedicated Nodit endpoint removed that
//! constraint — `eth_subscribe("newHeads")` over its WSS returns a subscription id and then
//! delivers blocks at the chain's own cadence (measured: 904ms, 1050ms, 966ms, 997ms against a
//! 1s block time).
//!
//! Driving off heads is strictly better than driving off a timer, for two independent reasons:
//!
//! * **Fewer requests.** A head costs one inbound frame instead of one outbound `eth_call` that
//!   exists only to ask whether anything happened.
//! * **The reads happen when there is new state.** A timer either fires between blocks — reading
//!   the same state twice and burning a request to learn nothing — or drifts past a block and
//!   quotes against state that is a whole block old. A head is the event itself.
//!
//! # The failure mode this module is mostly about
//!
//! A subscription that **errors** is easy: the socket closes, the task reconnects, the loop
//! keeps running on its fallback timer. A subscription that reconnects and then *silently stops
//! delivering* is the dangerous one, because every visible signal says healthy — the TCP
//! connection is open, no error was raised, the last head we hold looks like a real block — and
//! the bot sits there believing the chain has stopped while it quotes a frozen view into a
//! moving market.
//!
//! So head liveness is a first-class state here, exactly as feed liveness is in
//! [`crate::feed`]. Three mechanisms, and they fail differently on purpose:
//!
//! 1. **Read timeout** on the socket. No frame *at all* — not a head, not a ping — inside the
//!    watchdog window means the socket is dead whatever TCP believes. Forces a reconnect.
//! 2. **Head staleness**, owned by [`HeadShared`] and read by the quote loop. The socket being
//!    up says nothing about heads still arriving on it. This is the silent case, and it is why
//!    [`HeadStatus::Stale`] exists as something distinct from [`HeadStatus::Down`].
//! 3. **Reconnect backoff**, exponential. A dedicated endpoint is not an infinite one, and a
//!    subscription that dies at connect time must not become a hot reconnect loop.
//!
//! What this module deliberately does **not** do is decide that the chain is down. It reports
//! that heads stopped. Whether that means the chain stopped or only this socket did is answered
//! by [`super::ChainHealth`], from the block number the fallback read returns — because those
//! two situations call for opposite responses and only one of them should withdraw quotes.

use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

use crate::config::{ChainConfig, EndpointUrl};

/// One block header, reduced to the two fields the loop actually uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Head {
    /// Block number.
    pub number: u64,
    /// Block timestamp, unix seconds.
    pub timestamp: u64,
}

/// Whether the websocket is up and subscribed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Link {
    /// Connected, `eth_subscribe` acknowledged, reading.
    Up,
    /// Not connected, not subscribed, or backing off between attempts.
    Down,
}

/// What the quote loop is allowed to conclude about the head subscription.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadStatus {
    /// Subscribed, and the newest head is inside the watchdog window.
    Live,
    /// Subscribed, and **no head has arrived for longer than the watchdog allows**.
    ///
    /// The dangerous state. Nothing errored; the socket is open; heads simply stopped. Treated
    /// as loss of the primary driver, which forces the fallback read.
    Stale {
        /// Age of the newest head.
        age_ms: u64,
    },
    /// The socket is down or unsubscribed. Honest failure — the fallback covers it.
    Down,
    /// Subscribed but no head has ever arrived. A fresh start, or an endpoint that accepted the
    /// subscription and never delivered against it.
    NoData,
}

impl HeadStatus {
    /// Short stable string for structured logs.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Stale { .. } => "stale",
            Self::Down => "down",
            Self::NoData => "no_data",
        }
    }

    /// Whether heads are arriving. Anything else means the loop is running on its fallback.
    #[must_use]
    pub const fn is_live(self) -> bool {
        matches!(self, Self::Live)
    }
}

/// A point-in-time read of the subscription.
#[derive(Debug, Clone, Copy)]
pub struct HeadSnapshot {
    /// What the subscription is doing.
    pub status: HeadStatus,
    /// The newest head, whatever the status. Diagnostics only — the quote loop reads chain state
    /// from the flashblocks endpoint, never from a header.
    pub last: Option<Head>,
    /// Age of the newest head, if there has ever been one.
    pub age_ms: Option<u64>,
    /// Reconnects since process start. A climbing count with a `Live` status is still a warning.
    pub reconnects: u64,
    /// Successful `eth_subscribe` acknowledgements since process start.
    pub subscriptions: u64,
}

struct Inner {
    link: Link,
    last: Option<Head>,
    at: Option<Instant>,
    reconnects: u64,
    subscriptions: u64,
}

/// Subscription state shared between the websocket task and the quote loop.
pub struct HeadShared {
    inner: Mutex<Inner>,
    stale_after: Duration,
}

impl HeadShared {
    /// Create the shared state with the watchdog window from
    /// [`ChainConfig::head_stale_after`].
    #[must_use]
    pub fn new(stale_after: Duration) -> Self {
        Self {
            inner: Mutex::new(Inner {
                link: Link::Down,
                last: None,
                at: None,
                reconnects: 0,
                subscriptions: 0,
            }),
            stale_after,
        }
    }

    /// The watchdog window, for logging it alongside a trip.
    #[must_use]
    pub const fn stale_after(&self) -> Duration {
        self.stale_after
    }

    /// Record a head.
    ///
    /// A head whose number is at or below the newest one is dropped: a reorg or a duplicate
    /// notification must not look like fresh liveness. It is still *a frame*, so the socket-level
    /// read timeout is satisfied, but it does not reset the staleness clock — which is right,
    /// because an endpoint replaying one header forever is exactly the silent failure this
    /// module exists to catch.
    pub fn record(&self, head: Head, now: Instant) -> bool {
        let mut g = self.lock();
        if let Some(prev) = g.last {
            if head.number <= prev.number {
                return false;
            }
        }
        g.last = Some(head);
        g.at = Some(now);
        true
    }

    /// The socket connected and `eth_subscribe` was acknowledged.
    ///
    /// The stored head is **cleared**. The first status after a reconnect is therefore
    /// [`HeadStatus::NoData`] rather than a resurrected pre-outage header, for the same reason
    /// [`crate::feed::FeedShared::on_connected`] clears prices: a stale value that survives an
    /// outage is indistinguishable from a fresh one at the point it gets used.
    pub fn on_subscribed(&self, first: bool) {
        let mut g = self.lock();
        g.link = Link::Up;
        g.last = None;
        g.at = None;
        g.subscriptions += 1;
        if !first {
            g.reconnects += 1;
        }
    }

    /// The socket went down or the subscription was lost.
    pub fn on_disconnected(&self) {
        self.lock().link = Link::Down;
    }

    /// Read the current state.
    #[must_use]
    pub fn snapshot(&self, now: Instant) -> HeadSnapshot {
        let g = self.lock();
        let age = g.at.map(|t| now.saturating_duration_since(t));
        let age_ms = age.map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
        let status = match (g.link, g.last, age) {
            (Link::Down, _, _) => HeadStatus::Down,
            (Link::Up, None, _) => HeadStatus::NoData,
            (Link::Up, Some(_), Some(d)) if d <= self.stale_after => HeadStatus::Live,
            (Link::Up, _, _) => HeadStatus::Stale { age_ms: age_ms.unwrap_or(u64::MAX) },
        };
        HeadSnapshot {
            status,
            last: g.last,
            age_ms,
            reconnects: g.reconnects,
            subscriptions: g.subscriptions,
        }
    }

    /// A poisoned mutex here guards nothing but a cached header and two counters. Recovering the
    /// guard is correct: taking the process down would turn a cosmetic panic into a quoting
    /// outage, and every consumer re-derives staleness from the timestamp anyway.
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

// ---------------------------------------------------------------------------
// Wire format
// ---------------------------------------------------------------------------

/// The JSON payload of a frame, whether it arrived as **text or binary**.
///
/// This is not defensive coding, it is a measured requirement. Nodit sends JSON-RPC over
/// **binary** frames (opcode 0x2); Binance's market-data feed sends **text** (0x1). Both are
/// correct — RFC 6455 leaves the choice to the application and has nothing to say about JSON —
/// and a client written against one of them silently ignores every frame from the other.
///
/// "Silently" is the operative word, and it cost a debugging session here: the TLS handshake
/// returns 101, the socket stays open, frames arrive on schedule, `eth_subscribe` is
/// acknowledged — and with a text-only match arm not one byte of it is ever parsed, so the whole
/// thing presents as "the endpoint never replied". Matching on the opcode instead of the payload
/// is the bug; JSON is JSON.
fn payload(msg: &Message) -> Option<&str> {
    match msg {
        Message::Text(t) => Some(t.as_str()),
        Message::Binary(b) => std::str::from_utf8(b).ok(),
        _ => None,
    }
}

/// Parse a `0x`-prefixed hex quantity.
fn quantity(v: Option<&serde_json::Value>) -> Option<u64> {
    let s = v?.as_str()?;
    u64::from_str_radix(s.trim_start_matches("0x"), 16).ok()
}

/// The `eth_subscribe` reply. `Ok(Some(id))` on success, `Ok(None)` for a frame that is not a
/// reply to our request, `Err` for a JSON-RPC error object.
///
/// The error case is worth naming: pointing `ws_url` at the HTTPS endpoint gets
/// `-32601 "notifications not supported"` back, which is a configuration mistake rather than an
/// outage and deserves to be logged as one.
fn parse_subscribe_ack(text: &str, id: u64) -> Result<Option<String>, String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else { return Ok(None) };
    if v.get("id").and_then(serde_json::Value::as_u64) != Some(id) {
        return Ok(None);
    }
    if let Some(err) = v.get("error") {
        let code = err.get("code").and_then(serde_json::Value::as_i64).unwrap_or(0);
        let msg = err.get("message").and_then(serde_json::Value::as_str).unwrap_or("(none)");
        return Err(format!("node refused eth_subscribe: {code}: {msg}"));
    }
    Ok(v.get("result").and_then(serde_json::Value::as_str).map(ToString::to_string))
}

/// Parse a `eth_subscription` notification into a head.
///
/// `Ok(None)` for any frame that is not a head notification for **our** subscription id: pings,
/// acknowledgements, and — the reason the id is checked rather than assumed — notifications from
/// some other subscription, which would otherwise be counted as head liveness.
fn parse_head_notification(text: &str, subscription: &str) -> Result<Option<Head>, String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else { return Ok(None) };
    if v.get("method").and_then(serde_json::Value::as_str) != Some("eth_subscription") {
        return Ok(None);
    }
    let Some(params) = v.get("params") else { return Ok(None) };
    if params.get("subscription").and_then(serde_json::Value::as_str) != Some(subscription) {
        return Ok(None);
    }
    let Some(result) = params.get("result") else { return Ok(None) };
    let number = quantity(result.get("number")).ok_or("head has no usable `number`")?;
    let timestamp = quantity(result.get("timestamp")).ok_or("head has no usable `timestamp`")?;
    Ok(Some(Head { number, timestamp }))
}

// ---------------------------------------------------------------------------
// The task
// ---------------------------------------------------------------------------

/// Subscribe to `newHeads` and keep the subscription alive until `shutdown` flips.
///
/// Never returns an error, for the same reason [`crate::feed::binance::run`] does not: a
/// subscription that cannot connect is a *state* the quote loop reads off [`HeadShared`] and
/// survives by falling back to its timer, not a failure that should take the process down. The
/// process still has to withdraw quotes on the way out and it cannot do that if this task
/// panics it first.
///
/// `wake` carries the newest head number to the loop. It is a [`watch`] channel and not an
/// `mpsc` on purpose: `watch` **coalesces**. If two heads land while a cycle is still running,
/// the loop should run once against the newer state, not twice — the second run against the
/// older head would be computing a ladder for state that is already superseded, and a queue
/// would guarantee exactly that under any load that matters.
pub async fn run(
    cfg: ChainConfig,
    shared: Arc<HeadShared>,
    wake: watch::Sender<u64>,
    mut shutdown: watch::Receiver<bool>,
) {
    let url: EndpointUrl = cfg.ws_url.clone();
    let mut delay = Duration::from_millis(cfg.ws_reconnect_initial_ms);
    let max_delay = Duration::from_millis(cfg.ws_reconnect_max_ms);
    // No frame at all inside the watchdog window means the socket is dead. A head is due every
    // block time, so this is many blocks of silence.
    let read_timeout = cfg.head_stale_after();
    let connect_timeout = Duration::from_millis(cfg.request_timeout_ms);
    let mut first = true;

    loop {
        if *shutdown.borrow() {
            return;
        }

        // `expose()` is one of exactly two call sites; everything else sees the redacted form.
        let connect = tokio_tungstenite::connect_async(url.expose());
        let outcome = tokio::select! {
            biased;
            _ = shutdown.changed() => return,
            r = tokio::time::timeout(connect_timeout, connect) => r,
        };

        match outcome {
            Ok(Ok((mut socket, _resp))) => {
                let sub_id = 1_u64;
                let req = json!({
                    "jsonrpc": "2.0", "id": sub_id,
                    "method": "eth_subscribe", "params": ["newHeads"],
                });
                if socket.send(Message::Text(req.to_string())).await.is_err() {
                    warn!(target: "heads", event = "subscribe_send_failed", url = %url,
                          "connected but could not send eth_subscribe");
                    shared.on_disconnected();
                } else {
                    match await_subscription(&mut socket, sub_id, read_timeout, &mut shutdown).await {
                        Sub::Shutdown => {
                            let _ = socket.close(None).await;
                            shared.on_disconnected();
                            return;
                        }
                        Sub::Failed(reason) => {
                            warn!(target: "heads", event = "subscribe_failed", url = %url,
                                  reason = %reason, retry_in_ms = delay.as_millis(),
                                  "eth_subscribe did not succeed");
                            shared.on_disconnected();
                        }
                        Sub::Ok(subscription) => {
                            // The subscription id is not a credential, but it is an opaque
                            // session handle and there is nothing to gain from logging it.
                            info!(target: "heads", event = "subscribed", url = %url,
                                  block_time_ms = cfg.block_time_ms,
                                  watchdog_ms = read_timeout.as_millis(),
                                  "newHeads subscription established; this now drives the quote loop");
                            shared.on_subscribed(first);
                            first = false;
                            delay = Duration::from_millis(cfg.ws_reconnect_initial_ms);

                            read_heads(&mut socket, &subscription, &shared, &wake, read_timeout, &mut shutdown)
                                .await;

                            if *shutdown.borrow() {
                                let _ = socket.close(None).await;
                                shared.on_disconnected();
                                return;
                            }
                            shared.on_disconnected();
                        }
                    }
                }
            }
            Ok(Err(e)) => {
                shared.on_disconnected();
                warn!(target: "heads", event = "connect_failed", url = %url, error = %e,
                      retry_in_ms = delay.as_millis(), "could not connect the head subscription");
            }
            Err(_) => {
                shared.on_disconnected();
                warn!(target: "heads", event = "connect_timeout", url = %url,
                      retry_in_ms = delay.as_millis(), "head subscription connect timed out");
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

/// Outcome of waiting for the `eth_subscribe` acknowledgement.
enum Sub {
    Ok(String),
    Failed(String),
    Shutdown,
}

type Socket = tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
>;

/// Read frames until the subscribe acknowledgement arrives, times out, or errors.
async fn await_subscription(
    socket: &mut Socket,
    id: u64,
    timeout: Duration,
    shutdown: &mut watch::Receiver<bool>,
) -> Sub {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Sub::Failed("no eth_subscribe reply inside the watchdog window".into());
        }
        let next = tokio::select! {
            biased;
            _ = shutdown.changed() => return Sub::Shutdown,
            r = tokio::time::timeout(remaining, socket.next()) => r,
        };
        let msg = match next {
            Err(_) => return Sub::Failed("no eth_subscribe reply inside the watchdog window".into()),
            Ok(None) => return Sub::Failed("socket closed before the subscription was acknowledged".into()),
            Ok(Some(Err(e))) => return Sub::Failed(format!("websocket read failed: {e}")),
            Ok(Some(Ok(m))) => m,
        };

        // Payload first, opcode second: the reply may arrive as either text or binary.
        if let Some(text) = payload(&msg) {
            match parse_subscribe_ack(text, id) {
                Ok(Some(sub)) => return Sub::Ok(sub),
                Ok(None) => {}
                Err(e) => return Sub::Failed(e),
            }
        } else if let Message::Ping(p) = msg {
            if socket.send(Message::Pong(p)).await.is_err() {
                return Sub::Failed("could not answer a ping".into());
            }
        } else if let Message::Close(f) = msg {
            return Sub::Failed(format!("peer closed before acknowledging: {f:?}"));
        }
    }
}

/// Read head notifications until the socket dies, goes silent, or shutdown flips.
async fn read_heads(
    socket: &mut Socket,
    subscription: &str,
    shared: &HeadShared,
    wake: &watch::Sender<u64>,
    read_timeout: Duration,
    shutdown: &mut watch::Receiver<bool>,
) {
    loop {
        let next = tokio::select! {
            biased;
            _ = shutdown.changed() => return,
            r = tokio::time::timeout(read_timeout, socket.next()) => r,
        };
        let msg = match next {
            Err(_) => {
                warn!(target: "heads", event = "read_timeout", timeout_ms = read_timeout.as_millis(),
                      "no frame at all inside the watchdog window; the socket is dead. Reconnecting");
                return;
            }
            Ok(None) => {
                warn!(target: "heads", event = "stream_ended", "head socket closed by peer");
                return;
            }
            Ok(Some(Err(e))) => {
                warn!(target: "heads", event = "read_error", error = %e, "head websocket read failed");
                return;
            }
            Ok(Some(Ok(m))) => m,
        };

        // Payload first, opcode second. Heads arrive as binary frames from this endpoint and as
        // text from others; both are JSON and both must be parsed. See `payload`.
        if let Some(text) = payload(&msg) {
            match parse_head_notification(text, subscription) {
                Ok(Some(head)) => {
                    if shared.record(head, Instant::now()) {
                        debug!(target: "heads", event = "head", number = head.number,
                               timestamp = head.timestamp, "new head");
                        // Ignore a send error: it only means the quote loop has already gone
                        // away, and this task's own shutdown path handles that.
                        let _ = wake.send(head.number);
                    } else {
                        debug!(target: "heads", event = "head_not_newer", number = head.number,
                               "head at or below the newest already seen; not counted as liveness");
                    }
                }
                Ok(None) => {}
                Err(e) => warn!(target: "heads", event = "parse_error", error = %e,
                                "unparseable head notification; dropped"),
            }
        // tungstenite queues a pong but only flushes it when the sink is polled, and this loop
        // polls the stream. Answering explicitly removes the dependency on that detail.
        } else if let Message::Ping(p) = msg {
            if socket.send(Message::Pong(p)).await.is_err() {
                return;
            }
        } else if let Message::Close(f) = msg {
            warn!(target: "heads", event = "close_frame", frame = ?f, "peer closed the head stream");
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real `newHeads` notification from the Nodit endpoint, trimmed to the fields that are
    /// parsed plus enough neighbours to prove the parser is not position-dependent.
    const REAL_HEAD: &str = r#"{"jsonrpc":"2.0","method":"eth_subscription","params":{
        "subscription":"0x1bfc0686fe54594315fcc5bcb6ffd037",
        "result":{"parentHash":"0xabc","number":"0x1e4f5e4","timestamp":"0x686e5cc0",
                  "gasLimit":"0x3938700","gasUsed":"0x5208","baseFeePerGas":"0x3e8",
                  "difficulty":"0x0","extraData":"0x","blobGasUsed":"0x0"}}}"#;

    const SUB: &str = "0x1bfc0686fe54594315fcc5bcb6ffd037";

    fn shared() -> HeadShared {
        HeadShared::new(Duration::from_millis(10_000))
    }

    #[test]
    fn parses_a_real_new_heads_notification() {
        let head = parse_head_notification(REAL_HEAD, SUB).unwrap().expect("carries a head");
        assert_eq!(head.number, 0x1e4_f5e4);
        assert_eq!(head.timestamp, 0x686e_5cc0);
    }

    #[test]
    fn json_is_read_out_of_binary_frames_as_well_as_text_ones() {
        // MEASURED: Nodit sends JSON-RPC as BINARY frames (opcode 0x2). Binance's feed sends
        // TEXT (0x1). A text-only match arm here reads as "the endpoint never replied" — the
        // handshake returns 101, frames keep arriving, and every one is dropped unparsed. This
        // is pinned because the failure gives no signal at all pointing at the opcode.
        let as_text = Message::Text(REAL_HEAD.to_string());
        let as_binary = Message::Binary(REAL_HEAD.as_bytes().to_vec());
        assert_eq!(payload(&as_text), Some(REAL_HEAD));
        assert_eq!(payload(&as_binary), Some(REAL_HEAD));
        for m in [&as_text, &as_binary] {
            let head = parse_head_notification(payload(m).unwrap(), SUB).unwrap().unwrap();
            assert_eq!(head.number, 0x1e4_f5e4);
        }
        // Control frames carry no payload and must not be mistaken for one.
        assert_eq!(payload(&Message::Ping(vec![1, 2, 3])), None);
        // Non-UTF-8 in a binary frame is not JSON and must not panic.
        assert_eq!(payload(&Message::Binary(vec![0xff, 0xfe])), None);
    }

    #[test]
    fn a_notification_for_another_subscription_is_not_our_liveness() {
        // The failure this prevents: some other subscription's traffic resetting the head
        // watchdog, so a dead `newHeads` looks alive.
        assert!(parse_head_notification(REAL_HEAD, "0xsomethingelse").unwrap().is_none());
    }

    #[test]
    fn acknowledgements_and_noise_are_ignored_rather_than_errors() {
        assert!(parse_head_notification(r#"{"jsonrpc":"2.0","id":1,"result":"0xdead"}"#, SUB).unwrap().is_none());
        assert!(parse_head_notification("not json at all", SUB).unwrap().is_none());
    }

    #[test]
    fn a_head_without_a_number_is_an_error_not_a_zero() {
        // A zero here would look like a block number that never advances, which is the exact
        // signal `ChainHealth` reads to decide the chain has stopped.
        let text = REAL_HEAD.replace(r#""number":"0x1e4f5e4","#, "");
        let err = parse_head_notification(&text, SUB).unwrap_err();
        assert!(err.contains("number"), "error must name the missing field, got `{err}`");
    }

    #[test]
    fn the_subscribe_ack_yields_the_subscription_id() {
        let ack = r#"{"jsonrpc":"2.0","id":1,"result":"0x1bfc0686fe54594315fcc5bcb6ffd037"}"#;
        assert_eq!(parse_subscribe_ack(ack, 1).unwrap().as_deref(), Some(SUB));
        // A reply to somebody else's request is not ours.
        assert!(parse_subscribe_ack(ack, 2).unwrap().is_none());
    }

    #[test]
    fn pointing_the_ws_url_at_an_http_endpoint_is_reported_verbatim() {
        // Measured: this is exactly what the HTTPS endpoint answers. Surfacing the node's own
        // words is what makes the misconfiguration diagnosable from one log line.
        let err = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"notifications not supported"}}"#;
        let e = parse_subscribe_ack(err, 1).unwrap_err();
        assert!(e.contains("notifications not supported"), "got `{e}`");
    }

    #[test]
    fn a_silent_subscription_goes_stale_while_still_connected() {
        // THE case this module exists for: link up, no error, and no heads. It must not read as
        // `Live` and it must not read as `Down` either — `Down` would understate it, because a
        // down socket is at least trying to reconnect.
        let s = shared();
        let t0 = Instant::now();
        s.on_subscribed(true);
        s.record(Head { number: 100, timestamp: 1 }, t0);
        assert_eq!(s.snapshot(t0).status, HeadStatus::Live);

        let inside = t0 + Duration::from_millis(10_000);
        assert_eq!(s.snapshot(inside).status, HeadStatus::Live);

        let outside = t0 + Duration::from_millis(10_001);
        assert_eq!(s.snapshot(outside).status, HeadStatus::Stale { age_ms: 10_001 });
        assert!(!s.snapshot(outside).status.is_live());
        // ... and the last head is still readable for the diagnostic log line.
        assert_eq!(s.snapshot(outside).last.map(|h| h.number), Some(100));
    }

    #[test]
    fn a_replayed_head_does_not_reset_the_watchdog() {
        // An endpoint that re-sends one header forever keeps the socket busy and delivers no
        // new state. Counting that as liveness would defeat the whole watchdog.
        let s = shared();
        let t0 = Instant::now();
        s.on_subscribed(true);
        assert!(s.record(Head { number: 100, timestamp: 1 }, t0));

        let later = t0 + Duration::from_millis(9_000);
        assert!(!s.record(Head { number: 100, timestamp: 1 }, later), "a duplicate is not liveness");
        assert!(!s.record(Head { number: 99, timestamp: 1 }, later), "a reorg backwards is not liveness");

        // The clock still runs from the ORIGINAL arrival, so the watchdog still trips on time.
        assert_eq!(s.snapshot(t0 + Duration::from_millis(10_001)).status, HeadStatus::Stale { age_ms: 10_001 });

        // A genuinely newer head does reset it.
        assert!(s.record(Head { number: 101, timestamp: 2 }, later));
        assert_eq!(s.snapshot(later).status, HeadStatus::Live);
    }

    #[test]
    fn a_reconnect_clears_the_head_rather_than_resurrecting_it() {
        let s = shared();
        let t0 = Instant::now();
        s.on_subscribed(true);
        s.record(Head { number: 100, timestamp: 1 }, t0);

        s.on_disconnected();
        assert_eq!(s.snapshot(t0).status, HeadStatus::Down);

        s.on_subscribed(false);
        let snap = s.snapshot(t0);
        assert_eq!(snap.status, HeadStatus::NoData, "a pre-outage head must not survive a reconnect");
        assert_eq!(snap.reconnects, 1);
        assert_eq!(snap.subscriptions, 2);

        // An endpoint whose head numbering is behind the pre-outage one must still be accepted,
        // since the sequence state was cleared with the head.
        assert!(s.record(Head { number: 7, timestamp: 3 }, t0));
        assert_eq!(s.snapshot(t0).status, HeadStatus::Live);
    }

    #[test]
    fn a_never_delivering_subscription_is_no_data_not_live() {
        let s = shared();
        s.on_subscribed(true);
        let snap = s.snapshot(Instant::now());
        assert_eq!(snap.status, HeadStatus::NoData);
        assert!(!snap.status.is_live(), "an acknowledged subscription that delivers nothing is not live");
    }
}
