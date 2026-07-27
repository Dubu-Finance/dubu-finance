//! Exchange market-data feed: state, staleness, and the type that makes a stale price unusable.
//!
//! # The one invariant
//!
//! > A stale or disconnected feed must be an explicit state the rest of the system can see,
//! > never a silently retained last price.
//!
//! That is enforced by the type rather than by discipline. [`FeedSnapshot::live`] returns
//! `Some` only while the feed is connected *and* the last accepted tick is inside
//! `stale_after_ms`; every other case is `None`, whatever is still sitting in memory. The last
//! tick is reachable through [`FeedSnapshot::last_seen`], whose name and doc comment say it is
//! for logging, and which no pricing path calls.
//!
//! This matters more than it sounds. The failure it prevents is the quiet one: the socket dies
//! during a fast move, the bot keeps quoting a two-minute-old price against a market that has
//! moved 80 bp, and every taker who notices gets a free option until `maxStaleSecs` expires.
//!
//! # What can and cannot be detected on this stream
//!
//! Binance's `bookTicker` carries `u`, the order-book update id. It is **monotonically
//! increasing but not contiguous** — ids jump by however many book events occurred — so a
//! dropped message is not detectable and this module does not claim to detect one. What *is*
//! detectable, and is treated as a gap, is a **regression**: `u` at or below the last accepted
//! value for that symbol. That means either a duplicate, a reordering, or a server-side book
//! reset, and in all three cases the tick is dropped rather than applied over newer state.
//!
//! Contiguity would need the `depth` diff stream, which carries `U`/`u` ranges. That is a real
//! upgrade and it is not built; see the README.

pub mod binance;

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// One best-bid/best-ask observation, prices and sizes at [`crate::units::FEED_SCALE`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BookTick {
    /// Exchange order-book update id. Monotone, not contiguous.
    pub update_id: u64,
    /// Best bid price.
    pub bid: u128,
    /// Size resting at the best bid.
    pub bid_qty: u128,
    /// Best ask price.
    pub ask: u128,
    /// Size resting at the best ask.
    pub ask_qty: u128,
}

/// Whether the socket is up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Link {
    /// Connected and reading.
    Up,
    /// Not connected — never connected, dropped, or backing off between attempts.
    Down,
}

/// What the rest of the system is allowed to conclude about one symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedStatus {
    /// Connected, and the last accepted tick is inside the staleness window.
    Live,
    /// Connected, but nothing has been accepted for this symbol recently.
    Stale {
        /// Age of the newest accepted tick.
        age_ms: u64,
    },
    /// The socket is down, whatever is in memory.
    Disconnected,
    /// Connected, but this symbol has never produced an accepted tick.
    NoData,
}

impl FeedStatus {
    /// Short stable string for structured logs.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Stale { .. } => "stale",
            Self::Disconnected => "disconnected",
            Self::NoData => "no_data",
        }
    }
}

/// A point-in-time read of one symbol.
///
/// Constructed by [`FeedShared::snapshot`]. The tick is private on purpose; see the module docs.
#[derive(Debug, Clone, Copy)]
pub struct FeedSnapshot {
    /// What the feed is doing.
    pub status: FeedStatus,
    /// Age of the newest accepted tick, if there has ever been one.
    pub age_ms: Option<u64>,
    /// Reconnects since process start. A climbing count with a `Live` status is still a warning.
    pub reconnects: u64,
    /// Sequence regressions dropped since process start.
    pub gaps: u64,
    tick: Option<BookTick>,
}

impl FeedSnapshot {
    /// The tick, **only** if it may be priced from.
    ///
    /// `None` whenever the feed is not [`FeedStatus::Live`]. This is the only accessor any
    /// pricing path may call.
    #[must_use]
    pub fn live(&self) -> Option<&BookTick> {
        match self.status {
            FeedStatus::Live => self.tick.as_ref(),
            _ => None,
        }
    }

    /// The last tick regardless of status — **for logging and diagnostics only**.
    ///
    /// Pricing from this is the bug the module docs describe. It exists so that a "feed went
    /// stale" log line can say what it went stale holding.
    #[must_use]
    pub fn last_seen(&self) -> Option<&BookTick> {
        self.tick.as_ref()
    }
}

/// Why a tick was not accepted.
///
/// One variant, because one thing is detectable on this stream. See the module docs on what
/// `bookTicker`'s update id can and cannot tell you.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejected {
    /// `u` at or below the last accepted id for this symbol: a duplicate, a reordering, or a
    /// server-side book reset. The stored tick is left alone.
    SequenceRegression {
        /// The id that arrived.
        got: u64,
        /// The newest id already applied.
        have: u64,
    },
}

struct SymbolState {
    tick: BookTick,
    at: Instant,
}

struct Inner {
    link: Link,
    symbols: HashMap<String, SymbolState>,
    reconnects: u64,
    gaps: u64,
}

/// Feed state shared between the socket task and the quote loop.
pub struct FeedShared {
    inner: Mutex<Inner>,
    stale_after: Duration,
}

impl FeedShared {
    /// Create the shared state. Symbols appear as their first tick arrives.
    #[must_use]
    pub fn new(stale_after: Duration) -> Self {
        Self { inner: Mutex::new(Inner { link: Link::Down, symbols: HashMap::new(), reconnects: 0, gaps: 0 }), stale_after }
    }

    /// Apply a tick. Returns the rejection reason if it was dropped.
    ///
    /// A regression bumps the gap counter and leaves the stored tick alone: newer data must
    /// never be overwritten by older data, which is the entire point of checking.
    pub fn record(&self, symbol: &str, tick: BookTick, now: Instant) -> Result<(), Rejected> {
        let mut g = self.lock();
        let have = g.symbols.get(symbol).map(|s| s.tick.update_id);
        if let Some(have) = have {
            if tick.update_id <= have {
                g.gaps += 1;
                return Err(Rejected::SequenceRegression { got: tick.update_id, have });
            }
        }
        g.symbols.insert(symbol.to_string(), SymbolState { tick, at: now });
        Ok(())
    }

    /// The socket came up.
    ///
    /// Per-symbol sequence ids are **cleared**: a reconnect may land on a different Binance
    /// edge whose book-update counter is behind the one we were reading, and refusing every
    /// tick from the new socket as a regression would look exactly like a dead feed. The stored
    /// prices are cleared with them, so the first post-reconnect status is [`FeedStatus::NoData`]
    /// rather than a resurrected pre-outage price.
    pub fn on_connected(&self, first: bool) {
        let mut g = self.lock();
        g.link = Link::Up;
        g.symbols.clear();
        if !first {
            g.reconnects += 1;
        }
    }

    /// The socket went down.
    pub fn on_disconnected(&self) {
        self.lock().link = Link::Down;
    }

    /// Read one symbol.
    #[must_use]
    pub fn snapshot(&self, symbol: &str, now: Instant) -> FeedSnapshot {
        let g = self.lock();
        let entry = g.symbols.get(symbol);
        let age = entry.map(|s| now.saturating_duration_since(s.at));
        let age_ms = age.map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
        let status = match (g.link, entry, age) {
            (Link::Down, _, _) => FeedStatus::Disconnected,
            (Link::Up, None, _) => FeedStatus::NoData,
            (Link::Up, Some(_), Some(d)) if d <= self.stale_after => FeedStatus::Live,
            (Link::Up, _, _) => FeedStatus::Stale { age_ms: age_ms.unwrap_or(u64::MAX) },
        };
        FeedSnapshot { status, age_ms, reconnects: g.reconnects, gaps: g.gaps, tick: entry.map(|s| s.tick) }
    }

    /// Whether the socket is up, independent of any symbol.
    #[must_use]
    pub fn link(&self) -> Link {
        self.lock().link
    }

    /// A poisoned feed mutex means a panic inside a critical section that only ever moves a
    /// `HashMap` entry. Recovering the guard is correct here: the data is a cache of the last
    /// tick, every consumer re-derives staleness from the timestamp it carries, and taking the
    /// process down would turn a cosmetic panic into a quoting outage.
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tick(id: u64, bid: u128, ask: u128) -> BookTick {
        BookTick { update_id: id, bid, bid_qty: 100_000_000, ask, ask_qty: 100_000_000 }
    }

    fn shared() -> FeedShared {
        FeedShared::new(Duration::from_millis(5_000))
    }

    #[test]
    fn a_disconnected_feed_yields_no_price_however_fresh_the_tick() {
        let f = shared();
        let now = Instant::now();
        f.on_connected(true);
        f.record("ETHUSDT", tick(1, 100, 101), now).unwrap();
        assert!(f.snapshot("ETHUSDT", now).live().is_some());

        f.on_disconnected();
        let s = f.snapshot("ETHUSDT", now);
        assert_eq!(s.status, FeedStatus::Disconnected);
        assert!(s.live().is_none(), "a disconnected feed must not hand out a price");
        // ... but the diagnostics path can still see what it died holding.
        assert_eq!(s.last_seen().map(|t| t.bid), Some(100));
    }

    #[test]
    fn a_stale_tick_yields_no_price() {
        let f = shared();
        let t0 = Instant::now();
        f.on_connected(true);
        f.record("ETHUSDT", tick(1, 100, 101), t0).unwrap();

        let just_inside = t0 + Duration::from_millis(5_000);
        assert_eq!(f.snapshot("ETHUSDT", just_inside).status, FeedStatus::Live);
        assert!(f.snapshot("ETHUSDT", just_inside).live().is_some());

        let just_outside = t0 + Duration::from_millis(5_001);
        let s = f.snapshot("ETHUSDT", just_outside);
        assert!(matches!(s.status, FeedStatus::Stale { .. }));
        assert!(s.live().is_none(), "a stale feed must not hand out a price");
        assert_eq!(s.age_ms, Some(5_001));
    }

    #[test]
    fn an_unseen_symbol_is_no_data_not_a_zero_price() {
        let f = shared();
        f.on_connected(true);
        let s = f.snapshot("BTCUSDT", Instant::now());
        assert_eq!(s.status, FeedStatus::NoData);
        assert!(s.live().is_none());
        assert!(s.last_seen().is_none());
    }

    #[test]
    fn a_sequence_regression_is_dropped_and_counted() {
        let f = shared();
        let now = Instant::now();
        f.on_connected(true);
        f.record("ETHUSDT", tick(10, 100, 101), now).unwrap();

        // Older id: dropped, and the newer price survives.
        assert_eq!(
            f.record("ETHUSDT", tick(9, 50, 51), now),
            Err(Rejected::SequenceRegression { got: 9, have: 10 })
        );
        // Equal id: also dropped — a duplicate carries no new information.
        assert!(f.record("ETHUSDT", tick(10, 50, 51), now).is_err());

        let s = f.snapshot("ETHUSDT", now);
        assert_eq!(s.live().unwrap().bid, 100, "stale data overwrote newer data");
        assert_eq!(s.gaps, 2);

        // A non-contiguous forward jump is normal on this stream and must be accepted.
        f.record("ETHUSDT", tick(9_999, 200, 201), now).unwrap();
        assert_eq!(f.snapshot("ETHUSDT", now).live().unwrap().bid, 200);
        assert_eq!(f.snapshot("ETHUSDT", now).gaps, 2);
    }

    #[test]
    fn a_reconnect_clears_prices_and_sequence_state() {
        let f = shared();
        let now = Instant::now();
        f.on_connected(true);
        f.record("ETHUSDT", tick(1_000_000, 100, 101), now).unwrap();

        f.on_disconnected();
        f.on_connected(false);

        // The pre-outage price is gone rather than resurrected.
        assert_eq!(f.snapshot("ETHUSDT", now).status, FeedStatus::NoData);
        assert_eq!(f.snapshot("ETHUSDT", now).reconnects, 1);
        // ... and an edge whose counter is behind the old one is not mistaken for a regression.
        f.record("ETHUSDT", tick(7, 200, 201), now).unwrap();
        assert_eq!(f.snapshot("ETHUSDT", now).live().unwrap().bid, 200);
        assert_eq!(f.snapshot("ETHUSDT", now).gaps, 0);
    }
}
