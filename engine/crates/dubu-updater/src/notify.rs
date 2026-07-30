//! Telling the operator, on Telegram, that something happened.
//!
//! # Why this exists
//!
//! Two incidents, and neither of them was a failure of the log.
//!
//! On 2026-07-29 a ten-minute flashblocks outage latched `Halt::Liveness`, the stay-down check
//! re-asserted the withdrawals and exited `EXIT_HALTED`, and systemd fed the process back into the
//! same wall six times until `StartLimitBurst` was exhausted. **The bot was then dead for 59
//! minutes**, and the only thing that ended it was a human happening to look. Separately, the
//! equity group's killswitch latched and stayed latched for over fourteen hours, silently: the
//! record of it was one `INFO` line in a stream that emits several a second and reads, at a
//! glance, exactly like routine bookkeeping.
//!
//! Both were fully described in the log at the moment they happened. Nobody was reading the log at
//! the moment they happened, which is the whole problem — a log is a thing you consult once you
//! already suspect something, and both of these are the kind of failure that removes the reason to
//! suspect anything. So this pushes.
//!
//! # What it must never do
//!
//! This is alerting bolted onto a live trading system, so the ordering of its obligations is not
//! the obvious one. It is more important that this **cannot affect a quote** than that any
//! particular message arrives:
//!
//! * **It never blocks the quote loop.** [`Notifier::send`] is a `try_send` into a bounded channel
//!   and returns immediately; on a full channel the message is dropped and counted. There is no
//!   spelling of it that awaits, which is enforced by it not being `async` at all. The cycle is
//!   already 2.7s against a 200ms target and is about to be optimised hard — a millisecond spent
//!   here would be a millisecond taken from that.
//! * **A missing credential is not a failure.** With `TELEGRAM_BOT_TOKEN` or `TELEGRAM_CHAT_ID`
//!   absent, [`Notifier::from_env`] returns a notifier that spawns nothing and whose `send` is a
//!   no-op. Nothing else in the process behaves differently. An alerting credential that can stop
//!   a market maker from quoting is a worse liability than no alerting.
//! * **A Telegram failure is not a trading failure.** A 429, a revoked token, a DNS outage: each
//!   is logged once and the batch is dropped. Nothing is retried, because the only retry policy
//!   that is safe here is none — see [`deliver`].
//! * **The token is never printed.** Not at startup, not in an error, not redacted. It is a path
//!   segment of the API URL, which is exactly the shape [`crate::config::EndpointUrl`] already
//!   exists to keep out of logs, and `reqwest`'s own errors carry the URL they failed on — see
//!   [`post`] for the one line that handles that.
//!
//! # Coalescing, and why the window is what it is
//!
//! Telegram's per-chat limits are far tighter than its headline ~30 messages/second: sustained
//! traffic to a single group is documented at **20 messages per minute**, and bursts past it are
//! answered with 429 and a `retry_after`. A flow simulator is about to start generating trades, so
//! fills will arrive in bursts of tens rather than the current zero.
//!
//! So the task collects into a [`Batch`] and sends one message per window. The window opens on the
//! *first* event and closes [`BATCH_WINDOW`] later, which bounds the send rate at one message per
//! window plus one round trip — at three seconds that is at most 20 per minute, exactly Telegram's
//! per-group ceiling, and strictly under it in practice because the next window cannot open until
//! the previous send has returned. Three seconds is also nothing against what is being reported:
//! the liveness halt it exists to announce fires after a 600s outage, and the killswitch it exists
//! to announce went unnoticed for fourteen hours.
//!
//! Repeated *errors* need a second mechanism, because they do not stop when the window does. A
//! node answering `-32003 txpool is full` answers it for every send for minutes, which at one
//! message per window is still 20 identical messages a minute. [`ErrorLedger`] reports a class of
//! failure once, then goes quiet for [`ERROR_QUIET`] and reports the accumulated count when it
//! next speaks. Its key space is capped, so an endpoint inventing a new error string per request
//! cannot grow it.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot};
use tracing::{info, warn};

use crate::units::{format_fixed, FEED_SCALE};

/// How long a batch collects before it is sent.
///
/// See the module docs: one message per window bounds the per-chat rate at Telegram's documented
/// 20-per-minute ceiling for a group, and three seconds of latency is immaterial against the
/// events this reports.
const BATCH_WINDOW: Duration = Duration::from_secs(3);

/// Messages the quote loop may get ahead by before they start being dropped.
///
/// Bounded on purpose and sized generously rather than tightly: the whole point is that the
/// producer never waits, so the only question the capacity answers is how large a burst survives a
/// slow round trip. Two hundred and fifty-six is several windows' worth of the busiest flow the
/// simulator generates, and it is a few kilobytes.
const QUEUE_CAPACITY: usize = 256;

/// How long a class of send failure stays quiet after it has been reported once.
const ERROR_QUIET: Duration = Duration::from_secs(60);

/// Distinct failure classes the ledger will track. Past this, occurrences are counted in one
/// bucket rather than allocating a key per error string.
const ERROR_CLASSES_MAX: usize = 32;

/// Fills written out in full in one message. The rest are counted.
///
/// Twelve rather than a round twenty, and the number is arithmetic rather than taste: a fill line
/// carrying a 66-character transaction hash, both legs, both prices and the inventory runs to
/// about 250 characters, so twenty of them is 5,000 — past [`TEXT_MAX`], which means the tail of
/// the message would be cut off and the tail is where the "and N more" and the dropped-message
/// notice live. A cap that silently loses the count of what it capped is worse than a lower cap.
const FILLS_SHOWN_MAX: usize = 12;

/// Alerts written out in full in one message. The rest are counted.
///
/// There are a handful of correlation groups and at most one live alert per group, so this is a
/// ceiling on pathology rather than a working limit.
const ALERTS_SHOWN_MAX: usize = 8;

/// Where the text is cut. Telegram's own limit is 4096 UTF-8 characters and answers 400 past it;
/// the slack absorbs the truncation notice.
const TEXT_MAX: usize = 3_800;

/// Longest a 429 can silence this, whatever `retry_after` says.
const RATE_LIMIT_BACKOFF_MAX: Duration = Duration::from_secs(60);

/// Request timeout for one `sendMessage`.
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// One fill, in the terms a human needs to judge it.
///
/// Everything here is a plain number or an owned string, deliberately: the event crosses a channel
/// into a task that must not reach back into the [`crate::chain::ChainView`] or the config, and a
/// formatting decision that depends on live state is one that cannot be tested.
///
/// **There is no markout field, and that is not an omission.** A markout is the fill's value at
/// +1s, +10s and +60s against a later reference; at the moment the fill is observed none of those
/// references exists yet. `markout::Markout::settle` produces them a minute later, and wiring that
/// in would be a second class of message rather than a field on this one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fill {
    /// Which market.
    pub pair_id: u16,
    /// The feed symbol the pair follows, e.g. `ETHUSDT` — what a human recognises the pair by.
    pub symbol: String,
    /// True when the pool bought base and paid quote, matching [`crate::markout::Fill::is_bid`].
    pub is_bid: bool,
    /// The base leg, in the base token's own units.
    pub base_amount: u128,
    /// Base token decimals, so the amount can be rendered.
    pub base_decimals: u8,
    /// The quote leg, in the quote token's own units.
    pub quote_amount: u128,
    /// Quote token decimals.
    pub quote_decimals: u8,
    /// The price the fill actually executed at, at [`FEED_SCALE`].
    pub price_e8: u128,
    /// Our own reference at the fill's timestamp, at [`FEED_SCALE`].
    ///
    /// `None` when `markout::Markout::reference_at` had nothing inside its tolerance. The caller
    /// falls back to the executed price for scoring purposes, and passing that back here as a
    /// reference would render a perfect zero of edge on every such fill — a confident number about
    /// a comparison that was never made.
    pub reference_e8: Option<u128>,
    /// The pool's base balance as of the latest chain read.
    pub inventory_base: Option<u128>,
    /// The pool's quote balance as of the latest chain read.
    pub inventory_quote: Option<u128>,
    /// The transaction the fill was in.
    pub tx: String,
}

/// Something worth pushing to a phone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A fill against the pool.
    Fill(Fill),
    /// A killswitch latched. Covers chain liveness too, which trips as `Halt::Liveness`.
    Halt {
        /// The correlation group, or `"all"` when the cause is not per-group.
        group: String,
        /// `Halt::label`: `bleed`, `loss_budget` or `liveness`.
        switch: &'static str,
        /// The halt's own `Display`, which is what gets persisted to the state file.
        reason: String,
    },
    /// A liveness latch was cleared at startup because both RPC pools are answering again.
    LatchCleared {
        /// The group that was resumed.
        group: String,
        /// What the latch said when it was set.
        reason: String,
    },
    /// The book came up with a group already latched from a previous run.
    StayDown {
        /// The group that is still down.
        group: String,
        /// What latched it.
        reason: String,
        /// True when *every* group is down, so the process is about to withdraw and exit. This is
        /// the shape of the 59-minute outage, and it is the one alert that must survive the exit —
        /// see [`Notifier::flush`].
        exiting: bool,
    },
    /// A transaction could not be sent. Coalesced by class; see [`ErrorLedger`].
    SendFailed {
        /// The pair, when the failure belongs to one.
        pair_id: Option<u16>,
        /// What was being sent, e.g. `updateQuote`.
        kind: &'static str,
        /// The error's `Display`.
        error: String,
    },
}

// ---------------------------------------------------------------------------
// Credentials
// ---------------------------------------------------------------------------

/// The bot token and the chat to post into.
///
/// [`std::fmt::Debug`] is written by hand and prints the chat and nothing else, the same defence
/// `tx::Signer` makes for the signing key: the derived one would put the token in any log line
/// that ever formats this struct, and the point of a rule like "never log the token" is that it
/// holds for code nobody has written yet.
#[derive(Clone)]
struct Credentials {
    token: String,
    chat_id: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("chat_id", &self.chat_id)
            .finish_non_exhaustive()
    }
}

impl Credentials {
    /// Both values, or nothing.
    ///
    /// Blank is treated as absent. A `TELEGRAM_BOT_TOKEN=` line left in a `.env` is how a
    /// credential most often goes missing, and it must land in the same place a missing variable
    /// does rather than starting a notifier that 401s on every window forever.
    fn from_parts(token: Option<String>, chat_id: Option<String>) -> Option<Self> {
        let token = token
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty());
        let chat_id = chat_id
            .map(|c| c.trim().to_string())
            .filter(|c| !c.is_empty());
        match (token, chat_id) {
            (Some(token), Some(chat_id)) => Some(Self { token, chat_id }),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// The handle
// ---------------------------------------------------------------------------

/// What the channel carries. Not [`Event`], because a flush is an instruction rather than
/// something to report, and keeping it out of `Event` keeps that type plain data a test can build.
enum Msg {
    /// Something to report.
    Report(Event),
    /// Send whatever has accumulated now, and answer when it has gone out.
    Flush(oneshot::Sender<()>),
}

/// The producer side, held by the quote loop.
///
/// A disabled notifier — no credentials — holds `None` and every method is a no-op. That is the
/// whole of the "everything else behaves exactly as it does today" requirement: there is no task,
/// no HTTP client, no channel, and `send` compiles down to a null check.
pub struct Notifier {
    tx: Option<mpsc::Sender<Msg>>,
    /// Messages that never reached the task. Shared with it so the count is reported rather than
    /// merely incremented — a drop is the alerting lying by omission, which is worth a line.
    dropped: Arc<AtomicU64>,
}

impl Notifier {
    /// Read `TELEGRAM_BOT_TOKEN` and `TELEGRAM_CHAT_ID`, and start the task if both are there.
    ///
    /// Never fails and never panics. The absent case is logged at `info` and is the ordinary state
    /// of a dry run or a developer's machine.
    ///
    /// # Panics
    /// Never. It is called from inside the tokio runtime and only spawns.
    #[must_use]
    pub fn from_env() -> Self {
        let creds = Credentials::from_parts(
            std::env::var("TELEGRAM_BOT_TOKEN").ok(),
            std::env::var("TELEGRAM_CHAT_ID").ok(),
        );
        let Some(creds) = creds else {
            // Which of the two is missing is useful and leaks nothing; the presence of a token is
            // not the token. There is no spelling of the value itself in this module.
            info!(
                target: "notify", event = "disabled",
                token_set = std::env::var_os("TELEGRAM_BOT_TOKEN").is_some(),
                chat_id_set = std::env::var_os("TELEGRAM_CHAT_ID").is_some(),
                "no Telegram credentials; alerts are off and nothing else changes"
            );
            return Self::disabled();
        };

        let (tx, rx) = mpsc::channel(QUEUE_CAPACITY);
        let dropped = Arc::new(AtomicU64::new(0));
        info!(
            target: "notify", event = "enabled", chat_id = %creds.chat_id,
            batch_window_ms = BATCH_WINDOW.as_millis() as u64,
            queue_capacity = QUEUE_CAPACITY,
            "Telegram alerts on: fills, killswitch trips, chain liveness and repeated send failures"
        );
        tokio::spawn(run(creds, rx, Arc::clone(&dropped)));
        Self {
            tx: Some(tx),
            dropped,
        }
    }

    /// A notifier that does nothing, for a process with no credentials.
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            tx: None,
            dropped: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Whether anything will actually be sent.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.tx.is_some()
    }

    /// Hand an event to the task. **Never blocks, never awaits, never fails.**
    ///
    /// Deliberately not `async`, so no call site can accidentally put the network on the quote
    /// loop's critical path — the type system is doing the enforcing rather than a comment.
    /// `try_send` returning `Full` means the task is behind, which is the case this exists to be
    /// safe in: the message is dropped, the counter goes up, and the next batch says so.
    pub fn send(&self, event: Event) {
        let Some(tx) = &self.tx else { return };
        if tx.try_send(Msg::Report(event)).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Send whatever is batched, now, and wait up to `within` for it to go out.
    ///
    /// The one method here that awaits, and it exists for exactly one situation: the process is
    /// about to exit. `stay_down` and a killswitch halt both end in `EXIT_HALTED` within
    /// milliseconds of being logged, and a batch still inside its three-second window when the
    /// process exits is a batch nobody ever sees — which is precisely the 59-minute outage this
    /// module was written for. It is never called from the cycle.
    ///
    /// Bounded and best-effort in every direction: a full channel, a dead task, or a slow round
    /// trip all resolve as "carry on and exit".
    pub async fn flush(&self, within: Duration) {
        let Some(tx) = &self.tx else { return };
        let (ack, done) = oneshot::channel();
        if tx.try_send(Msg::Flush(ack)).is_err() {
            return;
        }
        let _ = tokio::time::timeout(within, done).await;
    }
}

// ---------------------------------------------------------------------------
// Coalescing repeated failures
// ---------------------------------------------------------------------------

/// The class a send failure is coalesced under.
///
/// Matched on the error's text rather than on a structured code, because the two failures that
/// actually repeat arrive by different routes: `nonce too low` is a `RpcError::Node` message from
/// the sequencer, and `-32003 txpool is full` has been seen both as a node error and inside an
/// HTTP body. A classifier that only understood `RpcError::Node { code }` would miss the second
/// spelling of the same outage, and the point of the class is that it groups what an operator
/// would group.
///
/// Anything unrecognised keeps a truncated form of its own message, so two genuinely different
/// failures do not collapse into one line that says nothing.
#[must_use]
pub fn classify(error: &str) -> String {
    let lower = error.to_ascii_lowercase();
    if lower.contains("txpool is full") || lower.contains("-32003") {
        return "txpool is full".to_string();
    }
    if lower.contains("nonce too low") {
        return "nonce too low".to_string();
    }
    if lower.contains("already known") || lower.contains("already in the pool") {
        return "already known".to_string();
    }
    if lower.contains("rate limited") || lower.contains("429") {
        return "rate limited".to_string();
    }
    if lower.contains("replacement transaction underpriced") || lower.contains("underpriced") {
        return "underpriced".to_string();
    }
    // Truncated on a character boundary, because an error carrying a hash or a nonce would
    // otherwise be a new class on every occurrence and defeat the coalescing entirely.
    let cut = error.char_indices().nth(60).map_or(error.len(), |(i, _)| i);
    error.get(..cut).unwrap_or(error).trim().to_string()
}

/// One class of failure, ready to be written into a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorReport {
    /// The class, from [`classify`].
    pub class: String,
    /// Occurrences since this class was last reported.
    pub since_last: u64,
    /// Occurrences since the process started.
    pub total: u64,
    /// An example, kept so the class is actionable rather than merely countable.
    pub sample: String,
}

/// Per-class state.
#[derive(Debug)]
struct Entry {
    quiet_until: Instant,
    pending: u64,
    total: u64,
    sample: String,
}

/// Collapses repeated send failures into one line per class per [`ERROR_QUIET`].
///
/// The alternative — one message per occurrence — is what makes an alerting channel unreadable,
/// and an unread alerting channel is the state this module exists to leave. A node with a full
/// txpool refuses every send for as long as the condition lasts, so the honest report is "this
/// started, and it is still going, and here is how many".
#[derive(Debug, Default)]
pub struct ErrorLedger {
    classes: BTreeMap<String, Entry>,
    /// Occurrences of classes past [`ERROR_CLASSES_MAX`], counted rather than keyed. A bound is
    /// required here and not merely tidy: the key comes from an error string an endpoint controls.
    overflow: u64,
}

impl ErrorLedger {
    /// Record one failure. `Some` when it should be reported in this batch.
    pub fn record(&mut self, class: String, sample: String, now: Instant) -> Option<ErrorReport> {
        if let Some(entry) = self.classes.get_mut(&class) {
            entry.total += 1;
            entry.pending += 1;
            if now < entry.quiet_until {
                return None;
            }
            let since_last = entry.pending;
            entry.pending = 0;
            entry.quiet_until = now + ERROR_QUIET;
            assert!(since_last >= 1, "a report always covers its own occurrence");
            return Some(ErrorReport {
                class,
                since_last,
                total: entry.total,
                sample: entry.sample.clone(),
            });
        }

        if self.classes.len() >= ERROR_CLASSES_MAX {
            self.overflow += 1;
            return None;
        }
        self.classes.insert(
            class.clone(),
            Entry {
                quiet_until: now + ERROR_QUIET,
                pending: 0,
                total: 1,
                sample: sample.clone(),
            },
        );
        Some(ErrorReport {
            class,
            since_last: 1,
            total: 1,
            sample,
        })
    }

    /// Classes whose quiet window has expired with occurrences waiting behind it.
    ///
    /// Called when a batch closes rather than on a timer of its own: a failure that stopped
    /// happening needs no follow-up, and one that is still happening will open a window anyway.
    pub fn due(&mut self, now: Instant) -> Vec<ErrorReport> {
        let mut out = Vec::new();
        for (class, entry) in &mut self.classes {
            if entry.pending == 0 || now < entry.quiet_until {
                continue;
            }
            out.push(ErrorReport {
                class: class.clone(),
                since_last: entry.pending,
                total: entry.total,
                sample: entry.sample.clone(),
            });
            entry.pending = 0;
            entry.quiet_until = now + ERROR_QUIET;
        }
        out
    }
}

// ---------------------------------------------------------------------------
// The batch
// ---------------------------------------------------------------------------

/// What one message is made of.
///
/// Every bucket is capped and the overflow is counted rather than kept, so a batch that cannot be
/// delivered — a 429 holds sends off for up to [`RATE_LIMIT_BACKOFF_MAX`] — occupies a bounded
/// amount of memory however long the burst behind it lasts.
#[derive(Debug, Default)]
pub struct Batch {
    alerts: Vec<Event>,
    alerts_omitted: u64,
    fills: Vec<Fill>,
    fills_omitted: u64,
    errors: Vec<ErrorReport>,
    /// Events `try_send` could not fit into the channel. Set from the shared counter at send time.
    dropped: u64,
}

impl Batch {
    /// Fold one event in, coalescing failures through `ledger`.
    pub fn absorb(&mut self, event: Event, ledger: &mut ErrorLedger, now: Instant) {
        match event {
            Event::Fill(fill) => {
                if self.fills.len() < FILLS_SHOWN_MAX {
                    self.fills.push(fill);
                } else {
                    self.fills_omitted += 1;
                }
            }
            Event::SendFailed {
                pair_id,
                kind,
                error,
            } => {
                let sample = match pair_id {
                    Some(id) => format!("pair {id} {kind}: {error}"),
                    None => format!("{kind}: {error}"),
                };
                if let Some(report) = ledger.record(classify(&error), sample, now) {
                    self.errors.push(report);
                }
            }
            alert => {
                if self.alerts.len() < ALERTS_SHOWN_MAX {
                    self.alerts.push(alert);
                } else {
                    self.alerts_omitted += 1;
                }
            }
        }
    }

    /// Nothing to say.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.alerts.is_empty()
            && self.fills.is_empty()
            && self.errors.is_empty()
            && self.dropped == 0
            && self.fills_omitted == 0
            && self.alerts_omitted == 0
    }

    /// The message text.
    ///
    /// Alerts first, then failures, then fills. The ordering is the point: something is wrong
    /// matters more than something happened, and a phone notification shows the first line.
    ///
    /// Plain text, with no `parse_mode`. Markdown would have to escape `_`, `*`, `[` and more out
    /// of every symbol, address and error message that passes through here, and Telegram answers a
    /// mis-escaped entity with a 400 — so the formatting would fail exactly on the malformed input
    /// an operator most needs to see.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        // First, above even the alerts, because it is the alerting admitting it is incomplete —
        // and because anything at the bottom of a long message is what truncation takes.
        if self.dropped > 0 {
            out.push_str(&format!(
                "({} alerts dropped: the notifier queue was full)\n",
                self.dropped
            ));
        }
        for alert in &self.alerts {
            out.push_str(&render_alert(alert));
            out.push('\n');
        }
        if self.alerts_omitted > 0 {
            out.push_str(&format!("... and {} more alerts\n", self.alerts_omitted));
        }
        for error in &self.errors {
            out.push_str(&format!(
                "TX FAILED {} x{} ({} since start) — {}\n",
                error.class, error.since_last, error.total, error.sample
            ));
        }
        for fill in &self.fills {
            out.push_str(&render_fill(fill));
            out.push('\n');
        }
        if self.fills_omitted > 0 {
            out.push_str(&format!("... and {} more fills\n", self.fills_omitted));
        }
        truncate(&out, TEXT_MAX)
    }
}

/// Cut to `max` characters on a character boundary, saying so.
///
/// Telegram counts UTF-8 characters and refuses a longer message outright, so a message that grew
/// past the limit would be the one that is lost — and the way a message grows is by carrying an
/// unusual number of events, which is the way an interesting one grows.
fn truncate(text: &str, max: usize) -> String {
    assert!(
        max > 64,
        "a truncation limit must leave room for the notice"
    );
    if text.chars().count() <= max {
        return text.to_string();
    }
    let cut = text.char_indices().nth(max).map_or(text.len(), |(i, _)| i);
    let mut out = text.get(..cut).unwrap_or(text).to_string();
    out.push_str("\n... truncated");
    out
}

/// One alert line.
fn render_alert(event: &Event) -> String {
    match event {
        Event::Halt {
            group,
            switch,
            reason,
        } => format!("HALT [{group}] killswitch {switch} tripped: {reason}"),
        Event::LatchCleared { group, reason } => format!(
            "RESUMED [{group}] the liveness latch was cleared at startup; both RPC pools are \
             answering again. It was set for: {reason}"
        ),
        Event::StayDown {
            group,
            reason,
            exiting: false,
        } => format!(
            "STILL DOWN [{group}] came up latched from a previous run; its pairs will not quote. \
             Reason: {reason}"
        ),
        Event::StayDown {
            group,
            reason,
            exiting: true,
        } => format!(
            "BOT EXITING [{group}] every group is latched from a previous run; withdrawing quotes \
             and exiting. THE POOL IS NOT QUOTING UNTIL SOMEONE CLEARS THIS. Reason: {reason}"
        ),
        // Neither reaches here: `absorb` routes fills and failures into their own buckets. Handled
        // rather than `unreachable!` because a panic in the alerting task is the one way this
        // module could take the process down, and it must not have one.
        Event::Fill(fill) => render_fill(fill),
        Event::SendFailed { kind, error, .. } => format!("TX FAILED {kind}: {error}"),
    }
}

/// One fill line.
fn render_fill(fill: &Fill) -> String {
    let side = if fill.is_bid { "we bought" } else { "we sold" };
    let mut line = format!(
        "fill [{}/{}] {side} {} base for {} quote @ {}",
        fill.pair_id,
        fill.symbol,
        format_fixed(fill.base_amount, fill.base_decimals),
        format_fixed(fill.quote_amount, fill.quote_decimals),
        format_fixed(fill.price_e8, FEED_SCALE),
    );
    match fill.reference_e8 {
        Some(reference) => {
            line.push_str(&format!(" vs ref {}", format_fixed(reference, FEED_SCALE)));
            match edge_e2(fill.price_e8, reference, fill.is_bid) {
                Some(edge) => line.push_str(&format!(" ({} to us)", signed_bps(edge))),
                None => line.push_str(" (edge unavailable)"),
            }
        }
        // Said out loud rather than left blank. A fill whose reference was outside `reference_at`'s
        // tolerance is scored against its own execution price, so any "edge" printed for it would
        // be a structural zero — a number that looks like a measurement and is not one.
        None => line.push_str(" (no reference within tolerance at the fill's timestamp)"),
    }
    if let (Some(base), Some(quote)) = (fill.inventory_base, fill.inventory_quote) {
        line.push_str(&format!(
            "; inventory now {} base / {} quote",
            format_fixed(base, fill.base_decimals),
            format_fixed(quote, fill.quote_decimals)
        ));
    }
    line.push_str(&format!("; {}", fill.tx));
    line
}

/// Hundredths of a basis point of edge, positive when the price we gave was on our side of the
/// reference.
///
/// The pool wants to buy below the reference and sell above it, so the sign has to flip with the
/// side — printing a raw `price - reference` would report every profitable bid as a loss. `_e2`
/// matches [`crate::markout`]'s unit, which is what the same fill will be scored in an hour later.
///
/// A ratio, so it is indifferent to the scale both prices arrive at; `None` only for a zero or
/// unrepresentable reference, which is not a comparison worth inventing an answer for.
#[must_use]
pub fn edge_e2(price_e8: u128, reference_e8: u128, is_bid: bool) -> Option<i64> {
    let price = i128::try_from(price_e8).ok()?;
    let reference = i128::try_from(reference_e8).ok()?;
    if reference <= 0 {
        return None;
    }
    let diff = if is_bid {
        reference.checked_sub(price)?
    } else {
        price.checked_sub(reference)?
    };
    // 10^6 rather than 10^4: one basis point is a hundred of these, matching `markout_e2`.
    let e2 = diff.checked_mul(1_000_000)?.checked_div(reference)?;
    i64::try_from(e2).ok()
}

/// `+1.65 bp` / `-12.00 bp`, from hundredths of a basis point.
fn signed_bps(e2: i64) -> String {
    let sign = if e2 < 0 { "-" } else { "+" };
    let magnitude = e2.unsigned_abs();
    format!("{sign}{}.{:02} bp", magnitude / 100, magnitude % 100)
}

// ---------------------------------------------------------------------------
// The task
// ---------------------------------------------------------------------------

/// Owns the HTTP client and does every byte of the network work.
///
/// Spawned the way `feed::ws::run` and `chain::view::run` are, and for the same reason those are:
/// the work is I/O on somebody else's schedule, and the loop that has to keep time must not be the
/// loop that waits for it. Never returns an error and never panics — the process must be able to
/// withdraw quotes on the way out, and it cannot do that if the alerting task takes it down first.
async fn run(creds: Credentials, mut rx: mpsc::Receiver<Msg>, dropped: Arc<AtomicU64>) {
    let http = match reqwest::Client::builder().timeout(HTTP_TIMEOUT).build() {
        Ok(c) => c,
        Err(e) => {
            // No URL has been constructed yet, so there is nothing here that could carry the
            // token; this is a TLS or resolver failure at construction.
            warn!(target: "notify", event = "client_failed", error = %e,
                  "could not build the HTTP client; alerts are off for this run");
            return;
        }
    };

    let mut ledger = ErrorLedger::default();
    let mut batch = Batch::default();
    let mut window: Option<Instant> = None;
    let mut quiet_until: Option<Instant> = None;

    loop {
        let msg = match window {
            Some(deadline) => match tokio::time::timeout_at(deadline.into(), rx.recv()).await {
                Ok(msg) => msg,
                Err(_) => {
                    close(
                        &http,
                        &creds,
                        &mut batch,
                        &mut ledger,
                        &dropped,
                        &mut quiet_until,
                    )
                    .await;
                    window = None;
                    continue;
                }
            },
            None => rx.recv().await,
        };

        let Some(msg) = msg else {
            // Every `Notifier` is gone, which means `run` in `main.rs` has returned. Anything still
            // batched is said on the way out rather than lost with the process.
            close(
                &http,
                &creds,
                &mut batch,
                &mut ledger,
                &dropped,
                &mut quiet_until,
            )
            .await;
            return;
        };

        match msg {
            Msg::Report(event) => {
                batch.absorb(event, &mut ledger, Instant::now());
                if window.is_none() {
                    window = Some(Instant::now() + BATCH_WINDOW);
                }
            }
            Msg::Flush(ack) => {
                close(
                    &http,
                    &creds,
                    &mut batch,
                    &mut ledger,
                    &dropped,
                    &mut quiet_until,
                )
                .await;
                window = None;
                // After the send, so a caller that is about to exit knows the message is out.
                let _ = ack.send(());
            }
        }
    }
}

/// Close the window: collect anything the ledger now owes, then deliver.
async fn close(
    http: &reqwest::Client,
    creds: &Credentials,
    batch: &mut Batch,
    ledger: &mut ErrorLedger,
    dropped: &AtomicU64,
    quiet_until: &mut Option<Instant>,
) {
    let now = Instant::now();
    for report in ledger.due(now) {
        batch.errors.push(report);
    }
    deliver(http, creds, batch, dropped, quiet_until).await;
}

/// Send one batch, or decide not to.
///
/// **There is no retry, and that is the design rather than a gap.** A retry loop here is a loop
/// whose length is decided by Telegram: an outage on their side becomes a growing backlog of
/// attempts inside a process whose actual job is to keep a market. A dropped alert costs one
/// message; an unbounded retry costs the thing the alerting is supposed to protect. The batch is
/// cleared before the request so a failure cannot even accidentally accumulate one.
///
/// A 429 is the exception, and only in the direction of doing *less*: `retry_after` silences
/// sending for that long, capped at [`RATE_LIMIT_BACKOFF_MAX`], while events keep batching into
/// the bounded buckets [`Batch`] already enforces.
async fn deliver(
    http: &reqwest::Client,
    creds: &Credentials,
    batch: &mut Batch,
    dropped: &AtomicU64,
    quiet_until: &mut Option<Instant>,
) {
    if batch.is_empty() && dropped.load(Ordering::Relaxed) == 0 {
        return;
    }
    if let Some(until) = *quiet_until {
        if Instant::now() < until {
            return;
        }
        *quiet_until = None;
    }

    // Taken only once the message is definitely going out, so a batch held back by a 429 does not
    // zero the counter and lose the fact that anything was dropped at all.
    batch.dropped = dropped.swap(0, Ordering::Relaxed);
    let text = batch.render();
    *batch = Batch::default();
    assert!(!text.is_empty(), "a non-empty batch renders to something");

    match post(http, creds, &text).await {
        Outcome::Sent => {}
        Outcome::RateLimited(after) => {
            let after = after.min(RATE_LIMIT_BACKOFF_MAX);
            *quiet_until = Some(Instant::now() + after);
            warn!(target: "notify", event = "rate_limited", retry_after_secs = after.as_secs(),
                  "Telegram is rate limiting this chat; holding sends until the window passes");
        }
        Outcome::Failed => {}
    }
}

/// What one `sendMessage` did.
enum Outcome {
    /// Telegram accepted it.
    Sent,
    /// HTTP 429, with the wait it asked for.
    RateLimited(Duration),
    /// Anything else. Already logged.
    Failed,
}

/// POST one message.
///
/// The URL carries the bot token as a path segment, which is why nothing in this function logs the
/// URL and why the `reqwest` error is passed through `without_url` before it is formatted:
/// `reqwest`'s own `Display` reads `error sending request for url (https://api.telegram.org/bot
/// <TOKEN>/sendMessage)`, so logging the error as it arrives would print the token in full on
/// every network failure — the exact leak a hand-written redaction elsewhere in this crate exists
/// to prevent, arriving through a third party's `Display` impl.
async fn post(http: &reqwest::Client, creds: &Credentials, text: &str) -> Outcome {
    let url = format!("https://api.telegram.org/bot{}/sendMessage", creds.token);
    let body = serde_json::json!({
        "chat_id": creds.chat_id,
        "text": text,
        "disable_web_page_preview": true,
    });

    let response = match http.post(&url).json(&body).send().await {
        Ok(r) => r,
        Err(e) => {
            warn!(target: "notify", event = "send_failed", error = %e.without_url(),
                  "could not reach Telegram; this batch is dropped and trading is unaffected");
            return Outcome::Failed;
        }
    };

    let status = response.status();
    if status.is_success() {
        return Outcome::Sent;
    }

    // Telegram's error body is `{"ok":false,"error_code":..,"description":".."}` and carries no
    // credential, but it is truncated anyway rather than trusted to stay that way.
    let body = response.text().await.unwrap_or_default();
    let retry_after = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("parameters")?.get("retry_after")?.as_u64());
    let detail = truncate(&body, 200);
    warn!(target: "notify", event = "rejected", status = status.as_u16(), detail = %detail,
          "Telegram refused the message; it is dropped and trading is unaffected");
    match (status.as_u16(), retry_after) {
        (429, after) => Outcome::RateLimited(Duration::from_secs(after.unwrap_or(30))),
        _ => Outcome::Failed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fill() -> Fill {
        Fill {
            pair_id: 1,
            symbol: "ETHUSDT".into(),
            is_bid: false,
            base_amount: 250_000_000_000_000_000,
            base_decimals: 18,
            quote_amount: 486_030_000,
            quote_decimals: 6,
            price_e8: 194_412_000_000,
            reference_e8: Some(194_380_000_000),
            inventory_base: Some(12_500_000_000_000_000_000),
            inventory_quote: Some(24_310_200_000),
            tx: "0xabc".into(),
        }
    }

    fn failure(error: &str) -> Event {
        Event::SendFailed {
            pair_id: Some(1),
            kind: "updateQuote",
            error: error.into(),
        }
    }

    // --- the missing-credential path ------------------------------------------------------

    /// The requirement that outranks every other one in this module: no credentials means no
    /// notifier, and a no-op that cannot touch a socket, a channel or a task.
    #[test]
    fn without_both_variables_there_is_no_notifier() {
        assert!(Credentials::from_parts(None, None).is_none());
        assert!(Credentials::from_parts(Some("t".into()), None).is_none());
        assert!(Credentials::from_parts(None, Some("-100".into())).is_none());
        assert!(Credentials::from_parts(Some("t".into()), Some("-100".into())).is_some());
    }

    /// A `TELEGRAM_BOT_TOKEN=` line with nothing after it is the likeliest way a credential goes
    /// missing, and it must land where an absent variable lands rather than starting a notifier
    /// that 401s on every window for the life of the process.
    #[test]
    fn a_blank_variable_counts_as_absent() {
        assert!(Credentials::from_parts(Some(String::new()), Some("-100".into())).is_none());
        assert!(Credentials::from_parts(Some("   ".into()), Some("-100".into())).is_none());
        assert!(Credentials::from_parts(Some("t".into()), Some("  ".into())).is_none());
        // And surrounding whitespace is trimmed rather than sent, since a trailing newline out of
        // a `.env` would otherwise become part of the URL path.
        let creds = Credentials::from_parts(Some(" t ".into()), Some(" -100 ".into()))
            .expect("both present");
        assert_eq!(creds.token, "t");
        assert_eq!(creds.chat_id, "-100");
    }

    /// A disabled notifier must be safe to call from anywhere, including outside a tokio runtime —
    /// which is the strongest available statement that it spawns nothing and touches no channel.
    #[test]
    fn a_disabled_notifier_swallows_everything_without_a_runtime() {
        let n = Notifier::disabled();
        assert!(!n.is_enabled());
        n.send(Event::Fill(fill()));
        n.send(failure("nonce too low"));
        n.send(Event::Halt {
            group: "equity".into(),
            switch: "bleed",
            reason: "NAV drawdown".into(),
        });
        assert_eq!(n.dropped.load(Ordering::Relaxed), 0, "nothing to drop");
    }

    /// The debug impl exists to keep the token out of a log line nobody has written yet.
    #[test]
    fn the_credentials_never_debug_print_the_token() {
        let creds = Credentials::from_parts(Some("123:SECRET".into()), Some("-100".into()))
            .expect("both present");
        let shown = format!("{creds:?}");
        assert!(!shown.contains("SECRET"), "the token leaked: {shown}");
        assert!(shown.contains("-100"), "the chat address is fine to print");
    }

    // --- the edge arithmetic ---------------------------------------------------------------

    /// The sign has to flip with the side, or every profitable bid reads as a loss.
    #[test]
    fn edge_is_positive_when_the_price_favoured_the_pool() {
        // Sold 1% above the reference: the pool won 100 bp.
        assert_eq!(edge_e2(101_000_000, 100_000_000, false), Some(10_000));
        // Bought 1% above the reference: the pool lost 100 bp.
        assert_eq!(edge_e2(101_000_000, 100_000_000, true), Some(-10_000));
        // And the mirror, so neither sign is an accident of one example.
        assert_eq!(edge_e2(99_000_000, 100_000_000, true), Some(10_000));
        assert_eq!(edge_e2(99_000_000, 100_000_000, false), Some(-10_000));
        assert_eq!(edge_e2(100_000_000, 100_000_000, true), Some(0));
    }

    /// A zero reference is not a comparison, and must not become one.
    #[test]
    fn a_zero_reference_has_no_edge_rather_than_an_infinite_one() {
        assert_eq!(edge_e2(100_000_000, 0, true), None);
        assert_eq!(edge_e2(u128::MAX, 1, false), None, "unrepresentable");
    }

    #[test]
    fn bps_are_printed_with_their_sign_and_two_places() {
        assert_eq!(signed_bps(165), "+1.65 bp");
        assert_eq!(signed_bps(-1_200), "-12.00 bp");
        assert_eq!(signed_bps(0), "+0.00 bp");
        assert_eq!(signed_bps(-7), "-0.07 bp");
    }

    // --- coalescing ------------------------------------------------------------------------

    /// The two failures the brief names arrive with different wrappers around them, so the
    /// classifier is matched on what an operator would read rather than on a wire code.
    #[test]
    fn the_failures_that_actually_repeat_share_a_class() {
        assert_eq!(
            classify("flashblocks: node error -32003: txpool is full"),
            "txpool is full"
        );
        assert_eq!(classify("txpool is full"), "txpool is full");
        assert_eq!(
            classify("rpc: node error -32000: nonce too low"),
            "nonce too low"
        );
        assert_ne!(classify("nonce too low"), classify("txpool is full"));
    }

    /// An unrecognised error keeps its own identity, or two unrelated faults would be reported as
    /// one thing happening twice.
    #[test]
    fn unknown_failures_stay_distinguishable_but_bounded() {
        assert_ne!(
            classify("execution reverted: NotUpdater"),
            classify("insufficient funds for gas")
        );
        let long = "x".repeat(500);
        assert!(classify(&long).chars().count() <= 60);
        // Two errors differing only past the cut collapse together, which is the point: an error
        // carrying a nonce or a transaction hash would otherwise be a fresh class on every
        // occurrence and defeat the ledger entirely. The prefix here is deliberately longer than
        // the cut — a shorter one would leave the varying part inside it and prove nothing.
        let a = "flashblocks: node error -32999: the sequencer refused this envelope, ref 41";
        let b = "flashblocks: node error -32999: the sequencer refused this envelope, ref 42";
        assert!(a.len() > 60, "the fixture has to reach past the cut");
        assert_eq!(classify(a), classify(b));
    }

    /// The whole reason the ledger exists: a node with a full txpool refuses every send for
    /// minutes, and that is one alert, not three hundred.
    #[test]
    fn a_repeated_failure_is_reported_once_and_then_counted() {
        let mut ledger = ErrorLedger::default();
        let t0 = Instant::now();

        let first = ledger
            .record("txpool is full".into(), "pair 1".into(), t0)
            .expect("the first occurrence is always reported");
        assert_eq!(first.since_last, 1);
        assert_eq!(first.total, 1);

        for i in 1..200 {
            assert!(
                ledger
                    .record(
                        "txpool is full".into(),
                        "pair 1".into(),
                        t0 + Duration::from_millis(i * 10)
                    )
                    .is_none(),
                "occurrence {i} inside the quiet window must not be its own message"
            );
        }
        assert!(
            ledger.due(t0 + Duration::from_secs(30)).is_empty(),
            "still inside the quiet window"
        );

        let due = ledger.due(t0 + ERROR_QUIET + Duration::from_secs(1));
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].since_last, 199, "everything suppressed, counted");
        assert_eq!(due[0].total, 200);
        assert!(
            ledger
                .due(t0 + ERROR_QUIET + Duration::from_secs(2))
                .is_empty(),
            "a class with nothing pending says nothing"
        );
    }

    /// Distinct classes are distinct alerts — collapsing a txpool outage into a nonce fault would
    /// report the wrong problem.
    #[test]
    fn different_classes_do_not_suppress_each_other() {
        let mut ledger = ErrorLedger::default();
        let t0 = Instant::now();
        assert!(ledger
            .record("txpool is full".into(), "a".into(), t0)
            .is_some());
        assert!(ledger
            .record("nonce too low".into(), "b".into(), t0)
            .is_some());
        assert!(ledger
            .record("txpool is full".into(), "a".into(), t0)
            .is_none());
    }

    /// The key comes from a string an endpoint controls, so the map has to have a ceiling.
    #[test]
    fn the_ledger_cannot_be_grown_without_bound_by_the_node() {
        let mut ledger = ErrorLedger::default();
        let t0 = Instant::now();
        for i in 0..500 {
            ledger.record(format!("class {i}"), "x".into(), t0);
        }
        assert_eq!(ledger.classes.len(), ERROR_CLASSES_MAX);
        assert_eq!(ledger.overflow, 500 - ERROR_CLASSES_MAX as u64);
    }

    // --- batching --------------------------------------------------------------------------

    /// A burst is one message, not one message per fill. This is what keeps the channel under
    /// Telegram's per-chat limit once the flow simulator starts trading.
    #[test]
    fn a_burst_of_fills_becomes_one_message() {
        let mut batch = Batch::default();
        let mut ledger = ErrorLedger::default();
        // A real transaction hash, because the cap is sized against the length of a rendered fill
        // line and the hash is a quarter of it. The three-character stub the other tests use would
        // let a too-high cap pass here and truncate in production.
        let long_hash = format!("0x{}", "ab".repeat(32));
        for _ in 0..50 {
            let mut f = fill();
            f.tx.clone_from(&long_hash);
            batch.absorb(Event::Fill(f), &mut ledger, Instant::now());
        }
        let text = batch.render();
        assert_eq!(
            text.matches("fill [1/ETHUSDT]").count(),
            FILLS_SHOWN_MAX,
            "the rest are counted rather than written out"
        );
        // The count of what was left out has to survive the length cap, or a burst reports fewer
        // fills than happened and says nothing about the difference.
        assert!(
            text.contains(&format!("and {} more fills", 50 - FILLS_SHOWN_MAX)),
            "{text}"
        );
        assert!(!text.contains("truncated"), "a full burst must still fit");
    }

    /// Something being wrong outranks something having happened: a phone notification shows the
    /// first line, so a halt cannot sit underneath twenty fills.
    #[test]
    fn alerts_come_before_failures_and_failures_before_fills() {
        let mut batch = Batch::default();
        let mut ledger = ErrorLedger::default();
        batch.absorb(Event::Fill(fill()), &mut ledger, Instant::now());
        batch.absorb(failure("nonce too low"), &mut ledger, Instant::now());
        batch.absorb(
            Event::Halt {
                group: "equity".into(),
                switch: "bleed",
                reason: "NAV drawdown 312 exceeds 300".into(),
            },
            &mut ledger,
            Instant::now(),
        );
        let text = batch.render();
        let halt = text.find("HALT").expect("the halt is in there");
        let failed = text.find("TX FAILED").expect("the failure is in there");
        let fill_at = text.find("fill [").expect("the fill is in there");
        assert!(halt < failed, "{text}");
        assert!(failed < fill_at, "{text}");
    }

    /// An empty batch must never become a message; the window closes on a schedule and most of
    /// those closes have nothing behind them.
    #[test]
    fn an_empty_batch_says_nothing() {
        let batch = Batch::default();
        assert!(batch.is_empty());
        assert!(batch.render().is_empty());
    }

    /// Dropping is the alerting lying by omission, so it is said out loud.
    #[test]
    fn a_full_queue_is_confessed_in_the_next_message() {
        let mut batch = Batch::default();
        let mut ledger = ErrorLedger::default();
        batch.absorb(Event::Fill(fill()), &mut ledger, Instant::now());
        batch.dropped = 7;
        assert!(batch.render().contains("7 alerts dropped"));
    }

    /// Telegram refuses anything past 4096 characters outright, and the message that grows past it
    /// is by construction the one carrying the most events.
    #[test]
    fn an_oversized_message_is_cut_rather_than_refused() {
        let long = "0123456789".repeat(1_000);
        let out = truncate(&long, TEXT_MAX);
        assert!(out.chars().count() <= TEXT_MAX + 32);
        assert!(out.ends_with("... truncated"));
        // Multi-byte input must not be cut mid-character, or the JSON body is invalid.
        let korean = "가".repeat(5_000);
        let out = truncate(&korean, TEXT_MAX);
        assert!(out.starts_with('가'));
        assert_eq!(
            truncate("short", TEXT_MAX),
            "short",
            "under the limit, untouched"
        );
    }

    // --- what a fill actually reads like ---------------------------------------------------

    /// The line has to answer, on a phone, whether this fill was one we wanted.
    #[test]
    fn a_fill_line_carries_the_price_the_reference_and_the_edge() {
        let line = render_fill(&fill());
        assert!(line.contains("[1/ETHUSDT]"), "{line}");
        assert!(line.contains("we sold"), "{line}");
        assert!(line.contains("0.250000000000000000 base"), "{line}");
        assert!(line.contains("486.030000 quote"), "{line}");
        assert!(line.contains("@ 1944.12000000"), "{line}");
        assert!(line.contains("vs ref 1943.80000000"), "{line}");
        // Sold 32 ticks above a 1943.80 reference: +1.646 bp, floored to the printed hundredth.
        assert!(line.contains("(+1.64 bp to us)"), "{line}");
        assert!(
            line.contains("inventory now 12.500000000000000000 base"),
            "{line}"
        );
        assert!(line.contains("24310.200000 quote"), "{line}");
        assert!(line.contains("0xabc"), "{line}");
    }

    /// A bid at the same numbers is the other sign, and the message must say so.
    #[test]
    fn the_same_numbers_on_the_other_side_report_the_opposite_edge() {
        let mut bid = fill();
        bid.is_bid = true;
        let line = render_fill(&bid);
        assert!(line.contains("we bought"), "{line}");
        assert!(line.contains("(-1.64 bp to us)"), "{line}");
    }

    /// When `reference_at` had nothing inside its tolerance the caller scores the fill against its
    /// own execution price, so any edge printed would be a structural zero. Saying "no reference"
    /// is the only honest line.
    #[test]
    fn a_fill_with_no_reference_says_so_instead_of_printing_a_zero() {
        let mut orphan = fill();
        orphan.reference_e8 = None;
        let line = render_fill(&orphan);
        assert!(line.contains("no reference"), "{line}");
        assert!(!line.contains("bp to us"), "{line}");
    }

    /// Inventory is optional because the chain view can be empty on the first cycles; a fill is
    /// still worth reporting without it.
    #[test]
    fn a_fill_without_inventory_still_renders() {
        let mut bare = fill();
        bare.inventory_base = None;
        bare.inventory_quote = None;
        let line = render_fill(&bare);
        assert!(!line.contains("inventory"), "{line}");
        assert!(line.contains("@ 1944.12000000"), "{line}");
    }

    /// The alerts carry the two things an operator needs before opening a terminal: which group,
    /// and why.
    #[test]
    fn every_alert_names_its_group_and_its_reason() {
        let halt = render_alert(&Event::Halt {
            group: "equity".into(),
            switch: "liveness",
            reason: "liveness: chain liveness lost for 615s (limit 600s)".into(),
        });
        assert!(halt.contains("[equity]"), "{halt}");
        assert!(halt.contains("liveness"), "{halt}");
        assert!(halt.contains("615s"), "{halt}");

        let cleared = render_alert(&Event::LatchCleared {
            group: "equity".into(),
            reason: "liveness: chain liveness lost for 615s".into(),
        });
        assert!(cleared.contains("[equity]"), "{cleared}");
        assert!(cleared.contains("615s"), "{cleared}");

        let down = render_alert(&Event::StayDown {
            group: "crypto".into(),
            reason: "bleed: NAV drawdown".into(),
            exiting: false,
        });
        assert!(down.contains("[crypto]"), "{down}");
        assert!(down.contains("will not quote"), "{down}");
    }

    /// The 59-minute outage in one line. It has to be unmistakable, because the failure mode it
    /// reports is that nobody looks.
    #[test]
    fn the_exiting_case_says_the_pool_has_stopped() {
        let exiting = render_alert(&Event::StayDown {
            group: "crypto".into(),
            reason: "liveness: chain liveness lost for 615s".into(),
            exiting: true,
        });
        assert!(exiting.contains("EXITING"), "{exiting}");
        assert!(exiting.contains("NOT QUOTING"), "{exiting}");
    }
}
