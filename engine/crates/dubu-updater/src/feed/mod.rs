//! Exchange market-data feeds: several venues, their liveness, and the type that makes a stale
//! price unusable.
//!
//! # The one invariant
//!
//! > A stale or disconnected feed must be an explicit state the rest of the system can see,
//! > never a silently retained last price.
//!
//! That is enforced by the type rather than by discipline. [`FeedSnapshot::live`] returns
//! `Some` only while that venue is connected *and* the last accepted tick is inside
//! `stale_after_ms`; every other case is `None`, whatever is still sitting in memory. The last
//! tick is reachable through [`FeedSnapshot::last_seen`], whose name and doc comment say it is
//! for logging, and which no pricing path calls.
//!
//! This matters more than it sounds. The failure it prevents is the quiet one: the socket dies
//! during a fast move, the bot keeps quoting a two-minute-old price against a market that has
//! moved 80 bp, and every taker who notices gets a free option until `maxStaleSecs` expires.
//!
//! # Why more than one venue
//!
//! One exchange makes the on-chain price bound vacuous in exactly the way a single-source oracle
//! is: the ladder and its own sanity limits are derived from the same number, so a feed that is
//! *confidently wrong* — a bad parse, a socket that stays open and serves stale data, an exchange
//! printing garbage mid-outage — produces a ladder nothing downstream can catch. Several
//! independent venues give the combiner in [`crate::fair_value`] a cross-section to reject
//! against, which is the only defence available until the on-chain Pyth deviation bound exists.
//!
//! Each venue is its own socket, its own reconnect loop and its own [`FeedShared`]. Nothing is
//! shared between them but the clock, so one venue failing cannot take another down.
//!
//! # Two statuses, deliberately
//!
//! [`VenueStatus`] is what one venue is doing with one symbol. [`FeedStatus`] is what the *system*
//! is allowed to conclude about that symbol once every venue has been read and combined — and the
//! interesting states there are not about sockets at all, they are `NoQuorum` and `Dispersed`.
//! [`crate::policy`] gates on the second, never the first: a single venue's status is never on its
//! own a reason to quote or not to quote.
//!
//! # What can and cannot be detected on these streams
//!
//! Every venue here carries some monotone update id, and none of them is contiguous, so a
//! *dropped* message is not detectable and this module does not claim to detect one. What **is**
//! detectable, and is treated as a gap, is a **regression**: an id at or below the last accepted
//! value for that symbol on that venue. That means a duplicate, a reordering, or a server-side
//! book reset, and in all three cases the tick is dropped rather than applied over newer state.
//! The one exception is a frame whose protocol explicitly says "reset your local book" — Bybit's
//! `snapshot` — which is applied through [`FeedShared::record_reset`] with the check bypassed,
//! because refusing it would be refusing the venue's own resynchronisation.

pub mod binance;
pub mod bybit;
pub mod coinbase;
pub mod okx;
pub mod ws;

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// One market-data venue.
///
/// Public market data only. No venue here needs a key, an account, or a signature, and there is
/// no order-entry code path anywhere in this crate to extend one into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum VenueId {
    /// Binance spot, `bookTicker` on the combined-stream endpoint.
    Binance,
    /// OKX v5 public, `bbo-tbt`.
    Okx,
    /// Bybit v5 spot public, `orderbook.1`.
    Bybit,
    /// Coinbase Exchange, `ticker`.
    Coinbase,
}

impl VenueId {
    /// Every venue this crate can speak to, in a stable order.
    pub const ALL: [Self; 4] = [Self::Binance, Self::Okx, Self::Bybit, Self::Coinbase];

    /// Short stable string for structured logs and config keys.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Binance => "binance",
            Self::Okx => "okx",
            Self::Bybit => "bybit",
            Self::Coinbase => "coinbase",
        }
    }

    /// The public endpoint, used when the config does not override it.
    #[must_use]
    pub const fn default_ws_url(self) -> &'static str {
        match self {
            Self::Binance => "wss://stream.binance.com:9443/stream",
            Self::Okx => "wss://ws.okx.com:8443/ws/v5/public",
            Self::Bybit => "wss://stream.bybit.com/v5/public/spot",
            Self::Coinbase => "wss://ws-feed.exchange.coinbase.com",
        }
    }
}

impl std::fmt::Display for VenueId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// One best-bid/best-ask observation, prices and sizes at [`crate::units::FEED_SCALE`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BookTick {
    /// Venue update id. Monotone, not contiguous.
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

/// Whether one venue's socket is up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Link {
    /// Connected and reading.
    Up,
    /// Not connected — never connected, dropped, or backing off between attempts.
    Down,
}

/// What one venue is doing with one symbol.
///
/// Note what this is *not*: a reason to quote. That is [`FeedStatus`], which is derived from every
/// venue at once. A single venue going `Disconnected` is an event to log, not a reason to stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VenueStatus {
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

impl VenueStatus {
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

    /// Whether this venue may be priced from.
    #[must_use]
    pub const fn is_live(self) -> bool {
        matches!(self, Self::Live)
    }
}

/// What the **system** is allowed to conclude about one symbol, after every venue is combined.
///
/// This is what [`crate::policy`] gates on. The two non-`Live` states are the whole reason the
/// multi-venue design exists, and they mean genuinely different things:
///
/// * `NoQuorum` — not enough venues are producing a usable price. We do not know the price.
/// * `Dispersed` — enough venues are live and they **disagree**. There is no single price to
///   know. Averaging through that is how a regime change gets quoted as if it were noise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedStatus {
    /// Quorum met, the survivors agree, and a reference price was produced.
    Live,
    /// Fewer venues produced a usable price than the quorum requires.
    NoQuorum {
        /// How many venues produced one.
        have: u8,
        /// How many are required.
        need: u8,
    },
    /// Quorum met, but the venues do not agree closely enough for one number to represent them.
    Dispersed {
        /// Median absolute deviation across venues, in bps of the median.
        dispersion_bps: u32,
        /// The limit it crossed.
        limit_bps: u32,
        /// How many venues were in the cross-section.
        venues: u8,
    },
}

impl FeedStatus {
    /// Short stable string for structured logs.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::NoQuorum { .. } => "no_quorum",
            Self::Dispersed { .. } => "dispersed",
        }
    }
}

/// A point-in-time read of one symbol on one venue.
///
/// Constructed by [`FeedShared::snapshot`]. The tick is private on purpose; see the module docs.
#[derive(Debug, Clone, Copy)]
pub struct FeedSnapshot {
    /// What this venue is doing with this symbol.
    pub status: VenueStatus,
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
    /// `None` whenever this venue is not [`VenueStatus::Live`]. This is the only accessor any
    /// pricing path may call.
    #[must_use]
    pub fn live(&self) -> Option<&BookTick> {
        match self.status {
            VenueStatus::Live => self.tick.as_ref(),
            _ => None,
        }
    }

    /// The last tick regardless of status — **for logging and diagnostics only**.
    ///
    /// Pricing from this is the bug the module docs describe. It exists so that a "venue went
    /// stale" log line can say what it went stale holding.
    #[must_use]
    pub fn last_seen(&self) -> Option<&BookTick> {
        self.tick.as_ref()
    }
}

/// Why a tick was not accepted.
///
/// One variant, because one thing is detectable on these streams. See the module docs on what a
/// venue's update id can and cannot tell you.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejected {
    /// An id at or below the last accepted one for this symbol: a duplicate, a reordering, or a
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

/// One venue's feed state, shared between its socket task and the quote loop.
pub struct FeedShared {
    venue: VenueId,
    inner: Mutex<Inner>,
    stale_after: Duration,
}

impl FeedShared {
    /// Create the shared state for one venue. Symbols appear as their first tick arrives.
    #[must_use]
    pub fn new(venue: VenueId, stale_after: Duration) -> Self {
        Self {
            venue,
            inner: Mutex::new(Inner { link: Link::Down, symbols: HashMap::new(), reconnects: 0, gaps: 0 }),
            stale_after,
        }
    }

    /// Which venue this is.
    #[must_use]
    pub const fn venue(&self) -> VenueId {
        self.venue
    }

    /// Apply a tick. Returns the rejection reason if it was dropped.
    ///
    /// A regression bumps the gap counter and leaves the stored tick alone: newer data must
    /// never be overwritten by older data, which is the entire point of checking.
    ///
    /// # Errors
    /// [`Rejected::SequenceRegression`].
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

    /// Apply a tick **unconditionally**, for a frame whose protocol says the book was reset.
    ///
    /// Only Bybit needs this: its `orderbook.1` stream stamps a frame `snapshot` when the
    /// service restarts and the update id goes back to 1. Refusing that as a regression would be
    /// refusing the venue's own resynchronisation and would present as a permanently dead symbol.
    /// Every other caller uses [`FeedShared::record`], because bypassing the check is exactly the
    /// hole the check exists to close.
    pub fn record_reset(&self, symbol: &str, tick: BookTick, now: Instant) {
        let mut g = self.lock();
        g.symbols.insert(symbol.to_string(), SymbolState { tick, at: now });
    }

    /// The socket came up.
    ///
    /// Per-symbol sequence ids are **cleared**: a reconnect may land on a different edge whose
    /// book-update counter is behind the one we were reading, and refusing every tick from the
    /// new socket as a regression would look exactly like a dead feed. The stored prices are
    /// cleared with them, so the first post-reconnect status is [`VenueStatus::NoData`] rather
    /// than a resurrected pre-outage price.
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
            (Link::Down, _, _) => VenueStatus::Disconnected,
            (Link::Up, None, _) => VenueStatus::NoData,
            (Link::Up, Some(_), Some(d)) if d <= self.stale_after => VenueStatus::Live,
            (Link::Up, _, _) => VenueStatus::Stale { age_ms: age_ms.unwrap_or(u64::MAX) },
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

// ---------------------------------------------------------------------------
// Every venue at once
// ---------------------------------------------------------------------------

/// Every configured venue's feed state, read together.
pub struct VenueFeeds {
    feeds: BTreeMap<VenueId, std::sync::Arc<FeedShared>>,
}

impl VenueFeeds {
    /// Build from the venues that at least one pair names.
    #[must_use]
    pub fn new(venues: &[VenueId], stale_after: Duration) -> Self {
        Self {
            feeds: venues
                .iter()
                .map(|&v| (v, std::sync::Arc::new(FeedShared::new(v, stale_after))))
                .collect(),
        }
    }

    /// The shared state for one venue, for handing to its socket task.
    #[must_use]
    pub fn venue(&self, v: VenueId) -> Option<std::sync::Arc<FeedShared>> {
        self.feeds.get(&v).cloned()
    }

    /// How many venues are configured.
    #[must_use]
    pub fn len(&self) -> usize {
        self.feeds.len()
    }

    /// Whether no venue is configured. Refused at config load, so this is only for completeness.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.feeds.is_empty()
    }

    /// Every configured venue, in a stable order.
    pub fn ids(&self) -> impl Iterator<Item = VenueId> + '_ {
        self.feeds.keys().copied()
    }

    /// Read one symbol on every venue that quotes it, in a stable order.
    ///
    /// A venue that does not carry this symbol at all still appears, as
    /// [`VenueStatus::NoData`] — the caller wants to see that a venue is configured and silent,
    /// which is precisely the state an absence would hide.
    #[must_use]
    pub fn snapshots(&self, symbol: &str, now: Instant) -> Vec<(VenueId, FeedSnapshot)> {
        self.feeds.iter().map(|(&v, f)| (v, f.snapshot(symbol, now))).collect()
    }
}

// ---------------------------------------------------------------------------
// Liveness transitions
// ---------------------------------------------------------------------------

/// One venue's liveness changing for one symbol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
    /// Which venue.
    pub venue: VenueId,
    /// Which symbol.
    pub symbol: String,
    /// What it was.
    pub from: VenueStatus,
    /// What it is now.
    pub to: VenueStatus,
}

impl Transition {
    /// Whether this transition is a venue becoming usable.
    #[must_use]
    pub const fn recovered(&self) -> bool {
        self.to.is_live()
    }
}

/// Edge-triggers on per-venue liveness so that losing a venue is an **event**, not an absence.
///
/// Without this, a venue that quietly stops delivering shows up only as one fewer entry in the
/// cross-section — and a cross-section that silently shrinks from four venues to two still
/// produces a confident-looking reference price. The transition is the thing worth alerting on;
/// the steady state is not.
#[derive(Debug, Default)]
pub struct VenueWatch {
    last: BTreeMap<(VenueId, String), VenueStatus>,
}

impl VenueWatch {
    /// Build an empty watch.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record this cycle's statuses and return only what changed.
    ///
    /// The first observation of a venue/symbol pair reports a transition from
    /// [`VenueStatus::NoData`], so a venue that never connects at all announces itself once
    /// rather than never.
    pub fn diff(&mut self, symbol: &str, snaps: &[(VenueId, FeedSnapshot)]) -> Vec<Transition> {
        let mut out = Vec::new();
        for (venue, snap) in snaps {
            let key = (*venue, symbol.to_string());
            let previous = self.last.insert(key, snap.status);
            match previous {
                // Compare on the label rather than the value: `Stale { age_ms }` changes on every
                // cycle by construction, and a watchdog that fires every cycle is one nobody reads.
                Some(p) if p.label() == snap.status.label() => {}
                Some(p) => out.push(Transition {
                    venue: *venue,
                    symbol: symbol.to_string(),
                    from: p,
                    to: snap.status,
                }),
                None if snap.status.is_live() => {}
                None => out.push(Transition {
                    venue: *venue,
                    symbol: symbol.to_string(),
                    from: VenueStatus::NoData,
                    to: snap.status,
                }),
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tick(id: u64, bid: u128, ask: u128) -> BookTick {
        BookTick { update_id: id, bid, bid_qty: 100_000_000, ask, ask_qty: 100_000_000 }
    }

    fn shared() -> FeedShared {
        FeedShared::new(VenueId::Binance, Duration::from_millis(5_000))
    }

    #[test]
    fn a_disconnected_venue_yields_no_price_however_fresh_the_tick() {
        let f = shared();
        let now = Instant::now();
        f.on_connected(true);
        f.record("ETHUSDT", tick(1, 100, 101), now).unwrap();
        assert!(f.snapshot("ETHUSDT", now).live().is_some());

        f.on_disconnected();
        let s = f.snapshot("ETHUSDT", now);
        assert_eq!(s.status, VenueStatus::Disconnected);
        assert!(s.live().is_none(), "a disconnected venue must not hand out a price");
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
        assert_eq!(f.snapshot("ETHUSDT", just_inside).status, VenueStatus::Live);
        assert!(f.snapshot("ETHUSDT", just_inside).live().is_some());

        let just_outside = t0 + Duration::from_millis(5_001);
        let s = f.snapshot("ETHUSDT", just_outside);
        assert!(matches!(s.status, VenueStatus::Stale { .. }));
        assert!(s.live().is_none(), "a stale venue must not hand out a price");
        assert_eq!(s.age_ms, Some(5_001));
    }

    #[test]
    fn an_unseen_symbol_is_no_data_not_a_zero_price() {
        let f = shared();
        f.on_connected(true);
        let s = f.snapshot("BTCUSDT", Instant::now());
        assert_eq!(s.status, VenueStatus::NoData);
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

        // A non-contiguous forward jump is normal on these streams and must be accepted.
        f.record("ETHUSDT", tick(9_999, 200, 201), now).unwrap();
        assert_eq!(f.snapshot("ETHUSDT", now).live().unwrap().bid, 200);
        assert_eq!(f.snapshot("ETHUSDT", now).gaps, 2);
    }

    #[test]
    fn a_protocol_level_reset_overrides_the_regression_check() {
        // Bybit stamps a frame `snapshot` when its service restarts and `u` goes back to 1.
        // Treating that as a regression would present as a permanently dead symbol.
        let f = shared();
        let now = Instant::now();
        f.on_connected(true);
        f.record("ETHUSDT", tick(1_000_000, 100, 101), now).unwrap();
        assert!(f.record("ETHUSDT", tick(1, 200, 201), now).is_err());
        assert_eq!(f.snapshot("ETHUSDT", now).live().unwrap().bid, 100);

        f.record_reset("ETHUSDT", tick(1, 200, 201), now);
        assert_eq!(f.snapshot("ETHUSDT", now).live().unwrap().bid, 200);
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
        assert_eq!(f.snapshot("ETHUSDT", now).status, VenueStatus::NoData);
        assert_eq!(f.snapshot("ETHUSDT", now).reconnects, 1);
        // ... and an edge whose counter is behind the old one is not mistaken for a regression.
        f.record("ETHUSDT", tick(7, 200, 201), now).unwrap();
        assert_eq!(f.snapshot("ETHUSDT", now).live().unwrap().bid, 200);
        assert_eq!(f.snapshot("ETHUSDT", now).gaps, 0);
    }

    #[test]
    fn venues_are_independent() {
        // The property the whole design rests on: one venue dying must not touch another.
        let feeds = VenueFeeds::new(&[VenueId::Binance, VenueId::Okx], Duration::from_millis(5_000));
        let now = Instant::now();
        for v in [VenueId::Binance, VenueId::Okx] {
            let f = feeds.venue(v).unwrap();
            f.on_connected(true);
            f.record("ETHUSDT", tick(1, 100, 101), now).unwrap();
        }
        feeds.venue(VenueId::Binance).unwrap().on_disconnected();

        let snaps = feeds.snapshots("ETHUSDT", now);
        assert_eq!(snaps.len(), 2);
        let by = |v| snaps.iter().find(|(id, _)| *id == v).unwrap().1.status;
        assert_eq!(by(VenueId::Binance), VenueStatus::Disconnected);
        assert_eq!(by(VenueId::Okx), VenueStatus::Live, "one venue dying took another with it");
    }

    #[test]
    fn losing_a_venue_is_an_event_and_a_steady_state_is_not() {
        let mut w = VenueWatch::new();
        let feeds = VenueFeeds::new(&[VenueId::Binance, VenueId::Okx], Duration::from_millis(5_000));
        let now = Instant::now();
        for v in [VenueId::Binance, VenueId::Okx] {
            let f = feeds.venue(v).unwrap();
            f.on_connected(true);
            f.record("ETHUSDT", tick(1, 100, 101), now).unwrap();
        }

        // Everything healthy on the first look: nothing to say.
        assert!(w.diff("ETHUSDT", &feeds.snapshots("ETHUSDT", now)).is_empty());

        // One venue drops: exactly one event, naming it.
        feeds.venue(VenueId::Binance).unwrap().on_disconnected();
        let ev = w.diff("ETHUSDT", &feeds.snapshots("ETHUSDT", now));
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].venue, VenueId::Binance);
        assert_eq!(ev[0].to, VenueStatus::Disconnected);
        assert!(!ev[0].recovered());

        // Still down on the next cycle: silence, because a watchdog that repeats is unread.
        assert!(w.diff("ETHUSDT", &feeds.snapshots("ETHUSDT", now)).is_empty());

        // Back up: one recovery event.
        feeds.venue(VenueId::Binance).unwrap().on_connected(false);
        feeds.venue(VenueId::Binance).unwrap().record("ETHUSDT", tick(2, 100, 101), now).unwrap();
        let ev = w.diff("ETHUSDT", &feeds.snapshots("ETHUSDT", now));
        assert_eq!(ev.len(), 1);
        assert!(ev[0].recovered());
    }

    #[test]
    fn a_venue_that_never_connects_announces_itself_once() {
        // The absence this closes: a venue configured, never delivering, and never mentioned.
        let mut w = VenueWatch::new();
        let feeds = VenueFeeds::new(&[VenueId::Coinbase], Duration::from_millis(5_000));
        let now = Instant::now();
        let ev = w.diff("ETHUSDT", &feeds.snapshots("ETHUSDT", now));
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].to, VenueStatus::Disconnected);
        assert!(w.diff("ETHUSDT", &feeds.snapshots("ETHUSDT", now)).is_empty());
    }

    #[test]
    fn an_ageing_stale_venue_does_not_re_announce_every_cycle() {
        let mut w = VenueWatch::new();
        let feeds = VenueFeeds::new(&[VenueId::Bybit], Duration::from_millis(1_000));
        let t0 = Instant::now();
        let f = feeds.venue(VenueId::Bybit).unwrap();
        f.on_connected(true);
        f.record("ETHUSDT", tick(1, 100, 101), t0).unwrap();
        w.diff("ETHUSDT", &feeds.snapshots("ETHUSDT", t0));

        let t1 = t0 + Duration::from_millis(2_000);
        assert_eq!(w.diff("ETHUSDT", &feeds.snapshots("ETHUSDT", t1)).len(), 1, "going stale is an event");
        let t2 = t0 + Duration::from_millis(9_000);
        assert!(
            w.diff("ETHUSDT", &feeds.snapshots("ETHUSDT", t2)).is_empty(),
            "`Stale {{ age_ms }}` changes every cycle; it must not re-fire"
        );
    }
}
