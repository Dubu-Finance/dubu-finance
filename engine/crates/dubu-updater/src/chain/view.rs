//! The chain read, moved off the quote loop.
//!
//! The read used to happen inside the cycle, so the cycle ran at whatever cadence the read was
//! triggered at — `newHeads`, which is one second. That made one second the ceiling on how often
//! the pool could re-price, and re-pricing is the thing that decides how tight it can quote.
//!
//! # Why the read cannot simply move into the fast lane
//!
//! Measured, `eth_call` against the flashblocks endpoint round-trips in **96ms median, 107ms
//! worst**. Dropping that into a 200ms loop would spend half of every tick waiting on a socket,
//! make the fast lane's timing depend on the network, and give it a failure mode it does not
//! currently have. So it gets its own task instead: it polls on its own clock and publishes into
//! a slot, and everything else reads whatever is in the slot.
//!
//! # What the consumers give up, and why it costs nothing
//!
//! A reader of the slot sees state up to one poll interval old. What is actually in there:
//!
//! | field | changes when | stale by 330ms matters? |
//! |---|---|---|
//! | the stored ladder, `updatedAt` | **our own** transaction lands | no — we know what we sent |
//! | `bidCapacity`, `capGen` | **we** refresh | no — same |
//! | `bidUsed` / `askUsed` | somebody swaps | **safely** — a swap only *reduces* what is exposed |
//! | `balanceOf` | somebody swaps | no — inventory drives skew, which is a slow control |
//!
//! Every externally-driven change is a swap, and learning about a swap late means believing we
//! have more capacity left than we do — which the pool's own `effectiveCapacity` bounds on chain
//! regardless. The reference price, which is what actually decides whether to re-quote, does not
//! come from here at all: it arrives on the venue websockets at 115-172 updates a second.
//!
//! # Polling faster after a send
//!
//! The interval adapts on exactly one signal, and it is one we generate ourselves: a transaction
//! we just broadcast is about to change this state, ~440ms from now. [`SharedView::nudge`] tells
//! the reader to take an extra look then, which settles the send sooner and unblocks the in-flight
//! gate sooner.
//!
//! Deliberately **not** adaptive on fill frequency yet, which would be the better long-run signal:
//! third-party volume is currently zero, so such a rule would sit in its slow state forever and
//! the fast branch would first execute in production, on the busiest day. `quiet_polls` is logged
//! so the rule can be back-solved from data rather than guessed at when there is flow to see.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use tokio::sync::Notify;
use tracing::{info, warn};

use super::{ChainHealth, ChainReader, ChainView, Rpc};

/// The most recent successful read, plus the counters that say how it is going.
#[derive(Debug, Default)]
pub struct SharedView {
    slot: RwLock<Option<ChainView>>,
    nudge: Notify,
    polls: AtomicU64,
    failures: AtomicU64,
    /// Consecutive polls that found `used` unchanged on every pair — the signal a fill-frequency
    /// rule would eventually key on. Logged, not acted on. See the module docs.
    quiet_polls: AtomicU64,
}

impl SharedView {
    /// An empty slot. Every reader gets `None` until the first poll lands.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The latest view, cloned. `None` before the first successful read.
    ///
    /// Cloned rather than handed out behind the guard so a slow consumer cannot hold the lock
    /// across an await and stall the reader.
    #[must_use]
    pub fn latest(&self) -> Option<ChainView> {
        self.slot.read().ok().and_then(|g| g.clone())
    }

    /// Tell the reader that a transaction was just broadcast, so it should look again once that
    /// transaction has had time to land.
    pub fn nudge(&self) {
        self.nudge.notify_one();
    }

    /// Successful polls since start.
    #[must_use]
    pub fn polls(&self) -> u64 {
        self.polls.load(Ordering::Relaxed)
    }

    /// Failed polls since start.
    #[must_use]
    pub fn failures(&self) -> u64 {
        self.failures.load(Ordering::Relaxed)
    }

    /// Consecutive polls that saw no swap on any pair.
    #[must_use]
    pub fn quiet_polls(&self) -> u64 {
        self.quiet_polls.load(Ordering::Relaxed)
    }

    fn publish(&self, view: ChainView) {
        // `used` moving is the only externally-driven change in this state, so it is the only
        // thing that says somebody traded. Counted here rather than derived later because the
        // comparison needs the previous view, which is about to be overwritten.
        let quiet = self
            .slot
            .read()
            .ok()
            .and_then(|g| g.as_ref().map(|old| swaps_look_identical(old, &view)))
            .unwrap_or(true);
        if quiet {
            self.quiet_polls.fetch_add(1, Ordering::Relaxed);
        } else {
            self.quiet_polls.store(0, Ordering::Relaxed);
        }
        if let Ok(mut g) = self.slot.write() {
            *g = Some(view);
        }
        self.polls.fetch_add(1, Ordering::Relaxed);
    }
}

/// True when no pair's used-amounts moved between two views.
fn swaps_look_identical(a: &ChainView, b: &ChainView) -> bool {
    a.snaps.len() == b.snaps.len()
        && a.snaps.iter().all(|(id, old)| {
            b.snaps.get(id).is_some_and(|new| {
                old.bid_used() == new.bid_used() && old.ask_used() == new.ask_used()
            })
        })
}

/// How the reader paces itself.
#[derive(Debug, Clone, Copy)]
pub struct Pacing {
    /// The ordinary interval between polls.
    pub interval: Duration,
    /// How long after a broadcast to take the extra look. Sized to the measured 440ms median from
    /// `eth_sendRawTransaction` to inclusion in a preconfirmation, with a little slack.
    pub after_send: Duration,
}

impl Default for Pacing {
    fn default() -> Self {
        Self {
            // One flashblock, measured at 327ms median on GIWA. Polling faster than the state
            // advances buys nothing but requests.
            interval: Duration::from_millis(330),
            after_send: Duration::from_millis(500),
        }
    }
}

/// Polls the chain forever, publishing into `shared`.
///
/// Never returns. Read failures are counted and logged and the loop continues — a slot holding a
/// slightly older view is a far better outcome than a reader that gives up, and the health ladder
/// in [`ChainHealth`] is what escalates a persistent failure into a halt.
pub async fn run(
    reader: Arc<ChainReader>,
    rpc: Arc<Rpc>,
    shared: Arc<SharedView>,
    health: Arc<std::sync::Mutex<ChainHealth>>,
    pacing: Pacing,
) {
    info!(
        target: "chain", event = "reader_started",
        interval_ms = pacing.interval.as_millis() as u64,
        after_send_ms = pacing.after_send.as_millis() as u64,
        "chain reads run on their own clock now; the quote loop no longer waits for one"
    );

    // A ticker rather than `sleep(interval)` at the top of the loop. Sleeping then working makes
    // the real period `interval + work`, and the work here is a 96ms round trip -- measured, that
    // turned a 330ms interval into 426ms and 3.0 reads a second into 2.1. `Delay` on a missed tick
    // so a slow read shifts the schedule rather than queueing catch-up reads behind it.
    let mut ticker = tokio::time::interval(pacing.interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            () = shared.nudge.notified() => {
                // A transaction is in flight and about to change this state. Wait for it to land
                // rather than reading immediately, which would only see the state before it.
                tokio::time::sleep(pacing.after_send).await;
            }
        }

        match reader.read(&rpc).await {
            Ok(v) => {
                if let Ok(mut h) = health.lock() {
                    h.on_read(Instant::now(), v.block_number);
                }
                shared.publish(v);
            }
            Err(e) => {
                shared.failures.fetch_add(1, Ordering::Relaxed);
                let rate_limited = e.is_rate_limit();
                let (consecutive, status) = match health.lock() {
                    Ok(mut h) => {
                        h.on_failure(&e);
                        (h.consecutive_failures(), h.status(Instant::now()).label())
                    }
                    Err(_) => (0, "unknown"),
                };
                warn!(
                    target: "chain", event = "read_failed", error = %e, rate_limit = rate_limited,
                    consecutive_failures = consecutive, status,
                    "chain read failed; the slot keeps the previous view"
                );
            }
        }
    }
}
