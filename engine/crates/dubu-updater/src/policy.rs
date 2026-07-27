//! When to push, and — mostly — when not to.
//!
//! archi_v2 §5.3 puts it plainly: the sophistication is not in landing harder, it is in
//! deciding not to send. Every push costs gas, burns a nonce, and replaces a quote that was
//! probably fine. The interesting output of this module is the *reason* attached to a hold.
//!
//! # Everything is measured against the executable top
//!
//! Not `maxBid`. `maxBid` is the price at zero usage, and once an epoch is partly consumed the
//! pool has already walked down the ladder: a taker arriving now gets
//! `executableTopBid(minBid, maxBid, capacity, used)`, which is what `PropCurve` computes and
//! therefore what "our quote" actually means. Comparing `maxBid` against a new `maxBid`
//! misjudges the drift in both directions, and both are expensive:
//!
//! * it **misses** a move, when the new row has a different width. A row whose `maxBid` is
//!   unchanged but whose width collapsed is quoting a materially higher executable price at the
//!   current usage — and the naive comparison sees no change at all and holds.
//! * it **invents** a move, when the width changes the other way. `maxBid` drops 50 bp, the
//!   width narrows to match, the executable top at the current usage is identical, and the
//!   naive comparison pushes a row that changes nothing.
//!
//! Both are pinned by tests below. The executable top is also exactly the zero-size limit of
//! `avg_bid_price`, so this is the same number `dubu-core` reports everywhere else.
//!
//! # The trigger order, and why the asymmetry runs the way it does
//!
//! ```text
//! 1. NeverQuoted          the pool has no usable ladder at all
//! 2. AdverseDrift         the market moved AGAINST the posted quote
//! 3. Heartbeat            the quote is approaching expiry
//! 4. FavourableDrift      the posted quote has merely become conservative
//! ```
//!
//! archi_v2 §5.3 lifts a chase/retreat pair from a competing maker where the *chase* threshold is the
//! tighter one — you follow a competitor's improvement eagerly and back away slowly. **This
//! module inverts that, deliberately.** Such a maker is one among many, so failing
//! to chase means losing the flow to someone else, and that is the dominant cost. On GIWA the
//! prop pool is the only maker of consequence: there is nobody to lose the flow to, so being a
//! basis point too conservative costs a little volume, while being a basis point too generous
//! after the market has moved is a free option written to whoever notices first. So:
//!
//! * `adverse_drift_bps` is the tight one — our bid is now above fair, or our ask below it;
//! * `favourable_drift_bps` is the loose one — we would quote better, and holding costs volume.
//!
//! [`crate::config::PairConfig::validate`] refuses a config with the two the other way round.
//!
//! # Pre-send gates
//!
//! Checked before any trigger, in this order, and every one of them is an abort rather than a
//! delay. A gate that fires means the *state* is wrong, not that the timing is.
//!
//! ```text
//! Halted -> ChainDown -> FeedNotLive -> ChainViewStale -> PoolPaused -> PushInFlight -> NoRow
//! ```
//!
//! `PushInFlight` deserves its own note: a pair with an unconfirmed transaction is **not**
//! superseded. Sending a second `updateQuote` for the same pair while the first is in the
//! mempool means two rows land in an order the sequencer picks, and the one that wins may be
//! the older one. The pair stays blocked until the transaction confirms or
//! [`crate::config::TxConfig::pending_timeout_secs`] elapses.

use dubu_core::curve::{executable_top_ask, executable_top_bid, Ladder, NO_ASK};
use dubu_core::math::mul_div_floor;
use dubu_core::CurveError;

use crate::chain::{ChainStatus, Snap};
use crate::feed::FeedStatus;
use crate::ladder::CapacityPlan;

/// Which side of the book a measurement refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// The pool buying base.
    Bid,
    /// The pool selling base.
    Ask,
}

impl Side {
    /// Short stable string for structured logs.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Bid => "bid",
            Self::Ask => "ask",
        }
    }
}

/// Why a push is warranted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// No ladder has ever been posted, or the stored one no longer passes the pool's own
    /// validator (which can happen after the manager raises `minPrice`).
    NoUsableQuote,
    /// The market moved against the posted quote.
    AdverseDrift {
        /// How far, in bps of the posted executable top.
        bps: u128,
        /// Which side moved furthest against us.
        side: Side,
        /// Threshold that was crossed.
        threshold_bps: u32,
    },
    /// The quote is approaching expiry.
    Heartbeat {
        /// Age of the posted quote, in seconds, by block time.
        age_secs: u64,
        /// The limit that was reached.
        limit_secs: u64,
    },
    /// The posted quote has become conservative.
    FavourableDrift {
        /// How far, in bps.
        bps: u128,
        /// Which side.
        side: Side,
        /// Threshold that was crossed.
        threshold_bps: u32,
    },
}

impl Trigger {
    /// Short stable string for structured logs.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::NoUsableQuote => "no_usable_quote",
            Self::AdverseDrift { .. } => "adverse_drift",
            Self::Heartbeat { .. } => "heartbeat",
            Self::FavourableDrift { .. } => "favourable_drift",
        }
    }

    /// Whether this trigger justifies re-posting a row identical to the one already stored.
    ///
    /// Only the heartbeat does: its entire purpose is to refresh `updatedAt`, and the four
    /// prices being the same is not merely acceptable but expected. Every other trigger sending
    /// an identical row would be a no-op transaction.
    #[must_use]
    pub const fn justifies_identical_row(self) -> bool {
        matches!(self, Self::Heartbeat { .. })
    }
}

/// Why a push was not made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hold {
    /// A killswitch has latched.
    Halted,
    /// The chain connection has been failing long enough to stop.
    ChainDown,
    /// The feed is not live. Quoting from a stale price is the failure this prevents.
    FeedNotLive(FeedStatus),
    /// The last successful poll is too old to act on.
    ChainViewStale {
        /// Age of the view.
        age_secs: u64,
        /// Configured limit.
        limit_secs: u64,
    },
    /// The pool or the pair is paused. Pushing a row would succeed and change nothing.
    PoolPaused,
    /// A transaction for this pair is unconfirmed.
    PushInFlight,
    /// No valid row could be built this cycle. The reason is logged where it was produced.
    NoRow,
    /// A trigger fired but the computed row is byte-identical to the stored one.
    Unchanged,
    /// Everything is healthy and nothing needs doing. The common case.
    NoTrigger {
        /// Largest adverse drift seen, for the log.
        adverse_bps: u128,
        /// Largest favourable drift seen.
        favourable_bps: u128,
        /// Quote age.
        age_secs: u64,
    },
}

impl Hold {
    /// Short stable string for structured logs.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Halted => "halted",
            Self::ChainDown => "chain_down",
            Self::FeedNotLive(_) => "feed_not_live",
            Self::ChainViewStale { .. } => "chain_view_stale",
            Self::PoolPaused => "pool_paused",
            Self::PushInFlight => "push_in_flight",
            Self::NoRow => "no_row",
            Self::Unchanged => "unchanged",
            Self::NoTrigger { .. } => "no_trigger",
        }
    }
}

/// The verdict for one pair, one action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Push, because of this.
    Send(Trigger),
    /// Do not push, because of this.
    Hold(Hold),
}

impl Decision {
    /// Whether this decision sends.
    #[must_use]
    pub const fn sends(self) -> bool {
        matches!(self, Self::Send(_))
    }

    /// Short stable string for structured logs.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Send(t) => t.label(),
            Self::Hold(h) => h.label(),
        }
    }
}

/// Why a capacity epoch needs refreshing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapacityTrigger {
    /// A side has no capacity at all, so the pool quotes zero however good the ladder is.
    NoEpoch {
        /// Which side.
        side: Side,
    },
    /// Remaining capacity has diverged from the planned epoch, in either direction.
    Diverged {
        /// Which side.
        side: Side,
        /// Remaining capacity as a percentage of the planned epoch.
        remaining_pct: u32,
        /// Threshold it crossed.
        threshold_pct: u32,
        /// `true` when remaining is *above* the plan — the pool is offering size it can no
        /// longer settle. See [`evaluate_capacity`].
        over_offered: bool,
    },
}

impl CapacityTrigger {
    /// Short stable string for structured logs.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::NoEpoch { .. } => "no_epoch",
            Self::Diverged { .. } => "capacity_diverged",
        }
    }
}

/// The verdict for a capacity refresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapacityDecision {
    /// Refresh, because of this.
    Send(CapacityTrigger),
    /// Do not refresh, because of this.
    Hold(Hold),
}

impl CapacityDecision {
    /// Short stable string for structured logs.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Send(t) => t.label(),
            Self::Hold(h) => h.label(),
        }
    }
}

// ---------------------------------------------------------------------------
// Drift
// ---------------------------------------------------------------------------

/// How far the planned row's executable top has moved from the posted one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Drift {
    /// Largest move against us, in bps.
    pub adverse_bps: u128,
    /// Which side that was.
    pub adverse_side: Option<Side>,
    /// Largest move in our favour, in bps.
    pub favourable_bps: u128,
    /// Which side that was.
    pub favourable_side: Option<Side>,
}

/// Measure both sides at the current usage.
///
/// The two ladders are evaluated at the **same** capacity and usage — the ones on chain — for
/// the simple reason that `updateQuote` changes neither. Anything else compares two different
/// points on two different curves and calls the difference a price move.
///
/// Direction, which is the part worth being careful about:
///
/// * a planned bid top **below** the posted one means the market fell and our posted bid is now
///   too high — we would overpay for base. Adverse.
/// * a planned ask top **above** the posted one means the market rose and our posted ask is now
///   too low — we would undersell base. Adverse.
///
/// A side with no capacity is skipped rather than counted as an infinite move. On the ask side
/// that also avoids treating [`NO_ASK`] as a price, which it is not.
///
/// # Errors
/// [`CurveError`] if a stored ladder is inverted, which the chain's own validator excludes.
pub fn drift(snap: &Snap, planned: &Ladder) -> Result<Drift, CurveError> {
    let mut d = Drift::default();
    // A zero move records no side. The distinction matters: `adverse_side: None` alongside
    // `adverse_bps: 0` is "nothing moved", where `Some(Bid)` alongside 0 would read as "the bid
    // moved, by nothing", and the trigger arms below pattern-match on the side being present.
    let note = |bps: u128, side: Side, adverse: bool, d: &mut Drift| {
        if bps == 0 {
            return;
        }
        if adverse {
            if bps > d.adverse_bps {
                d.adverse_bps = bps;
                d.adverse_side = Some(side);
            }
        } else if bps > d.favourable_bps {
            d.favourable_bps = bps;
            d.favourable_side = Some(side);
        }
    };

    if snap.bid_capacity > 0 {
        let used = snap.bid_used();
        let posted = executable_top_bid(snap.min_bid, snap.max_bid, snap.bid_capacity, used)?;
        let want = executable_top_bid(planned.min_bid, planned.max_bid, snap.bid_capacity, used)?;
        if posted > 0 {
            let bps = mul_div_floor(posted.abs_diff(want), 10_000, posted).unwrap_or(u128::MAX);
            note(bps, Side::Bid, want < posted, &mut d);
        }
    }

    if snap.ask_capacity > 0 {
        let used = snap.ask_used();
        let posted = executable_top_ask(snap.min_ask, snap.max_ask, snap.ask_capacity, used)?;
        let want = executable_top_ask(planned.min_ask, planned.max_ask, snap.ask_capacity, used)?;
        // `NO_ASK` is an infinity sentinel, never a price; `ask_capacity > 0` excludes it, but
        // the guard stays because reading it as a number is a silent, enormous error.
        if posted > 0 && posted != NO_ASK && want != NO_ASK {
            let bps = mul_div_floor(posted.abs_diff(want), 10_000, posted).unwrap_or(u128::MAX);
            note(bps, Side::Ask, want > posted, &mut d);
        }
    }

    Ok(d)
}

// ---------------------------------------------------------------------------
// Evaluation
// ---------------------------------------------------------------------------

/// Everything one evaluation needs. Plain data, so both tests and the loop build it the same
/// way and there is no mock to diverge from the real thing.
#[derive(Debug, Clone, Copy)]
pub struct Context<'a> {
    /// Block timestamp from the chain view. Quote age is measured against this, not the local
    /// clock, because it is what `PropPool` compares to.
    pub block_timestamp: u64,
    /// The pair's on-chain state.
    pub snap: &'a Snap,
    /// The row this cycle computed, or `None` if it could not be built.
    pub planned: Option<Ladder>,
    /// The capacity epoch this cycle would post.
    pub capacity: CapacityPlan,
    /// The pool's own `minPrice`, for re-validating what is already stored.
    pub min_price: u128,
    /// A killswitch has latched.
    pub halted: bool,
    /// Feed status for this pair's symbol.
    pub feed: FeedStatus,
    /// Chain connection status.
    pub chain: ChainStatus,
    /// Age of the chain view.
    pub view_age_secs: u64,
    /// Configured limit on that age.
    pub view_stale_secs: u64,
    /// A transaction for this pair is unconfirmed.
    pub in_flight: bool,
    /// Configured heartbeat.
    pub heartbeat_secs: u64,
    /// Tight threshold, for a move against us.
    pub adverse_drift_bps: u32,
    /// Loose threshold, for a move in our favour.
    pub favourable_drift_bps: u32,
    /// Capacity divergence threshold, in percent.
    pub capacity_divergence_pct: u32,
}

impl Context<'_> {
    /// The gates, in order. `None` means everything is in a state where acting is meaningful.
    ///
    /// Shared by both decisions: a halted bot must not refresh capacity either, and a stale
    /// feed must not be allowed to widen an epoch (archi_v2 §5.3 flags exactly that case — a
    /// stale price must not be able to induce capacity churn).
    fn gates(&self) -> Option<Hold> {
        if self.halted {
            return Some(Hold::Halted);
        }
        if matches!(self.chain, ChainStatus::Down { .. }) {
            return Some(Hold::ChainDown);
        }
        if self.feed != FeedStatus::Live {
            return Some(Hold::FeedNotLive(self.feed));
        }
        if self.view_age_secs > self.view_stale_secs {
            return Some(Hold::ChainViewStale {
                age_secs: self.view_age_secs,
                limit_secs: self.view_stale_secs,
            });
        }
        if self.snap.paused() {
            return Some(Hold::PoolPaused);
        }
        if self.in_flight {
            return Some(Hold::PushInFlight);
        }
        None
    }

    /// The effective heartbeat: the configured one, but never so late that the pool's own
    /// freshness window expires first.
    ///
    /// `maxStaleSecs * 4 / 5` is archi_v2 §5.3's 0.8 factor. The margin is not decoration — a
    /// heartbeat at exactly `maxStaleSecs` re-posts a quote that has already stopped being
    /// fillable, so the pool spends the round-trip latency of every heartbeat quoting nothing.
    #[must_use]
    pub const fn heartbeat_limit(&self) -> u64 {
        let chain_bound = (self.snap.max_stale_secs as u64) * 4 / 5;
        if self.heartbeat_secs < chain_bound {
            self.heartbeat_secs
        } else {
            chain_bound
        }
    }
}

/// Decide whether to push a new ladder.
///
/// # Errors
/// [`CurveError`] only if a stored ladder is inverted, which the chain excludes; the caller
/// treats it as "hold and log".
pub fn evaluate_quote(ctx: &Context<'_>) -> Result<Decision, CurveError> {
    if let Some(h) = ctx.gates() {
        return Ok(Decision::Hold(h));
    }
    let Some(planned) = ctx.planned else {
        return Ok(Decision::Hold(Hold::NoRow));
    };

    let age = ctx.snap.quote_age_secs(ctx.block_timestamp);
    let limit = ctx.heartbeat_limit();

    // 1. No usable quote. Also covers the case where the manager raised `minPrice` above a
    //    stored ladder: the pool will reject every fill against it until something is pushed.
    let unusable = ctx.snap.never_quoted() || ctx.snap.ladder().validate(ctx.min_price).is_err();
    if unusable {
        return Ok(finish(Decision::Send(Trigger::NoUsableQuote), ctx, &planned));
    }

    let d = drift(ctx.snap, &planned)?;

    // 2. Adverse drift — the tight threshold. See the module docs for why this outranks the
    //    heartbeat: a quote that is wrong now is more urgent than a quote that expires later.
    if d.adverse_bps >= u128::from(ctx.adverse_drift_bps) {
        if let Some(side) = d.adverse_side {
            let t = Trigger::AdverseDrift { bps: d.adverse_bps, side, threshold_bps: ctx.adverse_drift_bps };
            return Ok(finish(Decision::Send(t), ctx, &planned));
        }
    }

    // 3. Heartbeat.
    if age >= limit {
        let t = Trigger::Heartbeat { age_secs: age, limit_secs: limit };
        return Ok(finish(Decision::Send(t), ctx, &planned));
    }

    // 4. Favourable drift — the loose threshold.
    if d.favourable_bps >= u128::from(ctx.favourable_drift_bps) {
        if let Some(side) = d.favourable_side {
            let t = Trigger::FavourableDrift { bps: d.favourable_bps, side, threshold_bps: ctx.favourable_drift_bps };
            return Ok(finish(Decision::Send(t), ctx, &planned));
        }
    }

    Ok(Decision::Hold(Hold::NoTrigger {
        adverse_bps: d.adverse_bps,
        favourable_bps: d.favourable_bps,
        age_secs: age,
    }))
}

/// Last gate: a trigger fired, but is the row actually different?
///
/// Split out so that the "identical row" rule is stated once. The heartbeat is exempt because
/// refreshing `updatedAt` *is* the change it wants; every other trigger sending an identical
/// row would spend gas to store the bytes that are already there.
fn finish(decision: Decision, ctx: &Context<'_>, planned: &Ladder) -> Decision {
    let Decision::Send(t) = decision else { return decision };
    if *planned == ctx.snap.ladder() && !t.justifies_identical_row() {
        return Decision::Hold(Hold::Unchanged);
    }
    decision
}

/// Decide whether to post a fresh capacity epoch.
///
/// Separate from the quote decision because they are separate transactions against separate
/// storage words with separate meanings: `updateQuote` is the price decision and
/// `refreshCapacity` is the risk decision. They share the gates and nothing else.
#[must_use]
pub fn evaluate_capacity(ctx: &Context<'_>) -> CapacityDecision {
    if let Some(h) = ctx.gates() {
        return CapacityDecision::Hold(h);
    }
    // A refresh that would post zero is not a refresh, it is a withdrawal, and withdrawal is
    // the killswitch's job rather than a routine trigger's.
    if ctx.capacity.bid == 0 || ctx.capacity.ask == 0 {
        return CapacityDecision::Hold(Hold::NoRow);
    }

    let sides = [
        (Side::Bid, ctx.snap.bid_capacity, ctx.snap.bid_used(), ctx.capacity.bid),
        (Side::Ask, ctx.snap.ask_capacity, ctx.snap.ask_used(), ctx.capacity.ask),
    ];

    // 1. A side with no epoch at all quotes zero however good the ladder is.
    for (side, capacity, _, _) in sides {
        if capacity == 0 {
            return CapacityDecision::Send(CapacityTrigger::NoEpoch { side });
        }
    }

    // 2. Divergence, measured against the *planned* epoch rather than the posted one, and in
    //    **both** directions. The two mean different things and both need acting on:
    //
    //    * remaining far BELOW the plan — the epoch has been consumed, or the operator raised
    //      the configured capacity. The pool is quoting less depth than intended.
    //    * remaining far ABOVE the plan — inventory left the pool, so `plan_capacity` cut what
    //      it can settle, and the posted epoch now offers size the pool cannot deliver. Every
    //      aggregator polling `getAmountOut` sees a fillable quote that reverts
    //      `ReserveFloorBreached` when taken. Only checking the first direction leaves that
    //      standing until the next unrelated trigger happens to refresh it.
    for (side, capacity, used, planned) in sides {
        let remaining = capacity.saturating_sub(used);
        let pct = mul_div_floor(remaining, 100, planned.max(1)).unwrap_or(u128::from(u32::MAX));
        let div = u128::from(ctx.capacity_divergence_pct);
        let (under, over) = (pct < 100 - div, pct > 100 + div);
        if under || over {
            return CapacityDecision::Send(CapacityTrigger::Diverged {
                side,
                remaining_pct: u32::try_from(pct).unwrap_or(u32::MAX),
                threshold_pct: ctx.capacity_divergence_pct,
                over_offered: over,
            });
        }
    }

    CapacityDecision::Hold(Hold::NoTrigger { adverse_bps: 0, favourable_bps: 0, age_secs: 0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(l: Ladder, capacity: u128, used: u128, updated_at: u64) -> Snap {
        Snap {
            min_bid: l.min_bid,
            max_bid: l.max_bid,
            min_ask: l.min_ask,
            max_ask: l.max_ask,
            updated_at,
            bid_capacity: capacity,
            ask_capacity: capacity,
            bid_used_raw: used,
            ask_used_raw: used,
            cap_gen: 1,
            used_gen: 1,
            flags: 0,
            price_scale_exp: 24,
            max_stale_secs: 3_600,
        }
    }

    fn ladder(min_bid: u128, max_bid: u128, min_ask: u128, max_ask: u128) -> Ladder {
        Ladder { min_bid, max_bid, min_ask, max_ask }
    }

    /// A healthy context: nothing gated, quote fresh, row identical to what is stored.
    fn ctx<'a>(s: &'a Snap, planned: Option<Ladder>) -> Context<'a> {
        Context {
            block_timestamp: s.updated_at + 10,
            snap: s,
            planned,
            capacity: CapacityPlan {
                bid: s.bid_capacity,
                ask: s.ask_capacity,
                bid_cut_by_inventory: false,
                ask_cut_by_inventory: false,
            },
            min_price: 1,
            halted: false,
            feed: FeedStatus::Live,
            chain: ChainStatus::Healthy,
            view_age_secs: 1,
            view_stale_secs: 20,
            in_flight: false,
            heartbeat_secs: 2_400,
            adverse_drift_bps: 2,
            favourable_drift_bps: 8,
            capacity_divergence_pct: 30,
        }
    }

    // -----------------------------------------------------------------------
    // Drift is measured at the current usage, not at maxBid
    // -----------------------------------------------------------------------

    #[test]
    fn a_naive_max_bid_comparison_would_miss_this_move() {
        // Posted: 800..1000 over 100 of capacity, half consumed. The taker arriving now gets
        // 1000 - ceil(200 * 50 / 100) = 900.
        let s = snap(ladder(800, 1_000, 1_100, 1_300), 100, 50, 1_000);
        assert_eq!(executable_top_bid(800, 1_000, 100, 50), Ok(900));

        // Planned: same maxBid, zero width. The taker would get 1000 — an 11% better price than
        // the pool is currently offering. `maxBid` is identical, so comparing it sees nothing.
        let planned = ladder(1_000, 1_000, 1_100, 1_300);
        assert_eq!(planned.max_bid, s.max_bid, "the naive comparison is blind here by construction");

        let d = drift(&s, &planned).unwrap();
        assert_eq!(d.favourable_bps, 1_111);
        assert_eq!(d.favourable_side, Some(Side::Bid));

        let c = Context { adverse_drift_bps: 2, favourable_drift_bps: 8, ..ctx(&s, Some(planned)) };
        assert!(matches!(
            evaluate_quote(&c).unwrap(),
            Decision::Send(Trigger::FavourableDrift { side: Side::Bid, .. })
        ));
    }

    #[test]
    fn a_naive_max_bid_comparison_would_invent_this_move() {
        // The expensive direction: a push that changes nothing. Posted 800..1000 at 50/100 use
        // executes at 900. Planned 850..950 at the same usage also executes at 900.
        let s = snap(ladder(800, 1_000, 1_100, 1_300), 100, 50, 1_000);
        let planned = ladder(850, 950, 1_150, 1_250);
        assert_eq!(executable_top_bid(850, 950, 100, 50), Ok(900));
        assert_eq!(executable_top_ask(1_150, 1_250, 100, 50), executable_top_ask(1_100, 1_300, 100, 50));

        // `maxBid` moved 500 bps, so the naive comparison fires; the executable top did not
        // move at all, so this one holds.
        assert_eq!(drift(&s, &planned).unwrap(), Drift::default());
        let c = ctx(&s, Some(planned));
        assert!(matches!(
            evaluate_quote(&c).unwrap(),
            Decision::Hold(Hold::NoTrigger { adverse_bps: 0, favourable_bps: 0, .. })
        ));
    }

    #[test]
    fn drift_direction_follows_which_way_it_hurts() {
        let s = snap(ladder(900, 1_000, 1_100, 1_200), 100, 0, 1_000);

        // Market fell: our posted bid is now too high, and our posted ask is conservative.
        let down = ladder(800, 900, 1_000, 1_100);
        let d = drift(&s, &down).unwrap();
        assert_eq!(d.adverse_side, Some(Side::Bid), "a lower planned bid means we are overpaying");
        assert_eq!(d.adverse_bps, 1_000);
        assert_eq!(d.favourable_side, Some(Side::Ask));

        // Market rose: our posted ask is now too low, our bid is conservative.
        let up = ladder(1_000, 1_100, 1_200, 1_300);
        let d = drift(&s, &up).unwrap();
        assert_eq!(d.adverse_side, Some(Side::Ask), "a higher planned ask means we are underselling");
        assert_eq!(d.favourable_side, Some(Side::Bid));
    }

    #[test]
    fn a_side_with_no_capacity_is_skipped_rather_than_read_as_a_price() {
        // executable_top_ask returns the NO_ASK infinity sentinel at zero capacity. Reading it
        // as a number would produce a drift of ~10^36 bps and push every cycle forever.
        let mut s = snap(ladder(900, 1_000, 1_100, 1_200), 100, 0, 1_000);
        s.ask_capacity = 0;
        assert_eq!(executable_top_ask(s.min_ask, s.max_ask, 0, 0), Ok(NO_ASK));

        let d = drift(&s, &ladder(900, 1_000, 5_000, 6_000)).unwrap();
        assert_eq!(d.adverse_bps, 0);
        assert_eq!(d.adverse_side, None);
        assert_eq!(d.favourable_bps, 0);
    }

    // -----------------------------------------------------------------------
    // Triggers firing
    // -----------------------------------------------------------------------

    #[test]
    fn a_never_quoted_pair_is_pushed_immediately() {
        let s = snap(ladder(0, 0, 0, 0), 100, 0, 0);
        let c = ctx(&s, Some(ladder(900, 1_000, 1_100, 1_200)));
        assert_eq!(evaluate_quote(&c).unwrap(), Decision::Send(Trigger::NoUsableQuote));
    }

    #[test]
    fn a_stored_ladder_below_a_raised_min_price_is_pushed_immediately() {
        // The manager can raise `minPrice` after a ladder is stored. Until something is pushed
        // the pool rejects every fill, and nothing else in this module would notice.
        let s = snap(ladder(900, 1_000, 1_100, 1_200), 100, 0, 1_000);
        let c = Context { min_price: 950, ..ctx(&s, Some(ladder(960, 1_000, 1_100, 1_200))) };
        assert_eq!(evaluate_quote(&c).unwrap(), Decision::Send(Trigger::NoUsableQuote));
    }

    #[test]
    fn adverse_drift_fires_at_the_threshold_and_not_below_it() {
        let s = snap(ladder(1_000_000, 1_000_000, 2_000_000, 2_000_000), 100, 0, 1_000);
        let c = |bid: u128| ctx(&s, Some(ladder(bid, bid, 2_000_000, 2_000_000)));

        // 1 bp under: below the 2 bp threshold, and the ask side did not move. Hold.
        let just_under = 1_000_000 - 100;
        assert_eq!(drift(&s, &ladder(just_under, just_under, 2_000_000, 2_000_000)).unwrap().adverse_bps, 1);
        assert!(!evaluate_quote(&c(just_under)).unwrap().sends());

        // Exactly 2 bp: fires.
        let at = 1_000_000 - 200;
        assert_eq!(drift(&s, &ladder(at, at, 2_000_000, 2_000_000)).unwrap().adverse_bps, 2);
        assert!(matches!(
            evaluate_quote(&c(at)).unwrap(),
            Decision::Send(Trigger::AdverseDrift { bps: 2, side: Side::Bid, threshold_bps: 2 })
        ));
    }

    #[test]
    fn favourable_drift_needs_the_looser_threshold() {
        let s = snap(ladder(1_000_000, 1_000_000, 2_000_000, 2_000_000), 100, 0, 1_000);
        // Bid 5 bp better than posted: favourable, under the 8 bp threshold, and the ask is
        // unchanged so nothing is adverse. Hold — this is the case that saves the gas.
        let better = 1_000_000 + 500;
        let c = ctx(&s, Some(ladder(better, better, 2_000_000, 2_000_000)));
        let d = drift(&s, c.planned.as_ref().unwrap()).unwrap();
        assert_eq!((d.adverse_bps, d.favourable_bps), (0, 5));
        assert!(matches!(evaluate_quote(&c).unwrap(), Decision::Hold(Hold::NoTrigger { .. })));

        // 8 bp: fires.
        let better = 1_000_000 + 800;
        let c = ctx(&s, Some(ladder(better, better, 2_000_000, 2_000_000)));
        assert!(matches!(
            evaluate_quote(&c).unwrap(),
            Decision::Send(Trigger::FavourableDrift { bps: 8, threshold_bps: 8, .. })
        ));
    }

    #[test]
    fn adverse_drift_outranks_the_heartbeat() {
        // Both are due. The reported reason must be the one that costs money.
        let s = snap(ladder(1_000_000, 1_000_000, 2_000_000, 2_000_000), 100, 0, 1_000);
        let moved = 1_000_000 - 1_000;
        let c = Context {
            block_timestamp: 1_000 + 3_000,
            ..ctx(&s, Some(ladder(moved, moved, 2_000_000, 2_000_000)))
        };
        assert!(matches!(evaluate_quote(&c).unwrap(), Decision::Send(Trigger::AdverseDrift { .. })));
    }

    #[test]
    fn the_heartbeat_fires_before_the_pools_own_window_expires() {
        let l = ladder(1_000_000, 1_000_000, 2_000_000, 2_000_000);
        let s = snap(l, 100, 0, 1_000);
        // maxStaleSecs is 3600, so the bound is 2880 even though the config says 2400.
        let c = ctx(&s, Some(l));
        assert_eq!(c.heartbeat_limit(), 2_400);

        // A config looser than the chain window is clamped to it, not honoured.
        let c = Context { heartbeat_secs: 999_999, ..ctx(&s, Some(l)) };
        assert_eq!(c.heartbeat_limit(), 2_880);
        assert!(c.heartbeat_limit() < u64::from(s.max_stale_secs), "the quote must never expire between pushes");
    }

    #[test]
    fn the_heartbeat_re_posts_an_identical_row_and_nothing_else_does() {
        let l = ladder(1_000_000, 1_000_000, 2_000_000, 2_000_000);
        let s = snap(l, 100, 0, 1_000);

        // One second short of the heartbeat, identical row: hold.
        let c = Context { block_timestamp: 1_000 + 2_399, ..ctx(&s, Some(l)) };
        assert!(matches!(evaluate_quote(&c).unwrap(), Decision::Hold(Hold::NoTrigger { .. })));

        // At the heartbeat, identical row: send anyway. Refreshing `updatedAt` is the point.
        let c = Context { block_timestamp: 1_000 + 2_400, ..ctx(&s, Some(l)) };
        assert_eq!(
            evaluate_quote(&c).unwrap(),
            Decision::Send(Trigger::Heartbeat { age_secs: 2_400, limit_secs: 2_400 })
        );
    }

    #[test]
    fn a_trigger_on_an_identical_row_holds_unless_it_is_the_heartbeat() {
        // Contrived but exactly the guard's job: `NoUsableQuote` on a pair whose stored ladder
        // equals the planned one. Re-storing identical bytes buys nothing.
        let l = ladder(0, 0, 0, 0);
        let s = snap(l, 100, 0, 0);
        let c = ctx(&s, Some(l));
        assert_eq!(evaluate_quote(&c).unwrap(), Decision::Hold(Hold::Unchanged));
    }

    // -----------------------------------------------------------------------
    // Gates
    // -----------------------------------------------------------------------

    #[test]
    fn every_gate_aborts_and_they_abort_in_order() {
        let l = ladder(900, 1_000, 1_100, 1_200);
        // A never-quoted pair, so a trigger is definitely pending behind every gate.
        let s = snap(ladder(0, 0, 0, 0), 100, 0, 0);
        let base = ctx(&s, Some(l));

        let cases: Vec<(Context<'_>, Hold)> = vec![
            (Context { halted: true, ..base }, Hold::Halted),
            (Context { chain: ChainStatus::Down { stale_secs: 600 }, ..base }, Hold::ChainDown),
            (
                Context { feed: FeedStatus::Stale { age_ms: 9_000 }, ..base },
                Hold::FeedNotLive(FeedStatus::Stale { age_ms: 9_000 }),
            ),
            (Context { feed: FeedStatus::Disconnected, ..base }, Hold::FeedNotLive(FeedStatus::Disconnected)),
            (
                Context { view_age_secs: 21, ..base },
                Hold::ChainViewStale { age_secs: 21, limit_secs: 20 },
            ),
            (Context { in_flight: true, ..base }, Hold::PushInFlight),
        ];
        for (c, expected) in cases {
            assert_eq!(evaluate_quote(&c).unwrap(), Decision::Hold(expected), "gate {expected:?} did not abort");
        }

        // Paused needs its own snapshot.
        let mut paused = s;
        paused.flags = 1;
        let c = ctx(&paused, Some(l));
        assert_eq!(evaluate_quote(&c).unwrap(), Decision::Hold(Hold::PoolPaused));

        // Precedence: halted outranks everything, including a down chain.
        let c = Context { halted: true, chain: ChainStatus::Down { stale_secs: 600 }, ..base };
        assert_eq!(evaluate_quote(&c).unwrap(), Decision::Hold(Hold::Halted));
    }

    #[test]
    fn a_degraded_chain_still_quotes() {
        // Degraded widens the spread upstream; it must not stop the loop, or a transient RPC
        // wobble becomes a quoting outage which is the worse failure.
        let s = snap(ladder(0, 0, 0, 0), 100, 0, 0);
        let c = Context { chain: ChainStatus::Degraded { stale_secs: 40 }, ..ctx(&s, Some(ladder(900, 1_000, 1_100, 1_200))) };
        assert!(evaluate_quote(&c).unwrap().sends());
    }

    #[test]
    fn a_row_that_could_not_be_built_holds_rather_than_sending_the_old_one() {
        let s = snap(ladder(0, 0, 0, 0), 100, 0, 0);
        let c = ctx(&s, None);
        assert_eq!(evaluate_quote(&c).unwrap(), Decision::Hold(Hold::NoRow));
    }

    #[test]
    fn a_stale_feed_blocks_even_a_pair_that_has_never_quoted() {
        // The tempting exception — "it has no quote at all, surely anything is better" — is
        // wrong: a price from a dead feed is how the first fill gets picked off.
        let s = snap(ladder(0, 0, 0, 0), 100, 0, 0);
        let c = Context { feed: FeedStatus::NoData, ..ctx(&s, Some(ladder(900, 1_000, 1_100, 1_200))) };
        assert_eq!(evaluate_quote(&c).unwrap(), Decision::Hold(Hold::FeedNotLive(FeedStatus::NoData)));
    }

    // -----------------------------------------------------------------------
    // Capacity
    // -----------------------------------------------------------------------

    #[test]
    fn a_full_epoch_does_not_get_refreshed() {
        let l = ladder(900, 1_000, 1_100, 1_200);
        let s = snap(l, 1_000, 0, 1_000);
        assert!(matches!(evaluate_capacity(&ctx(&s, Some(l))), CapacityDecision::Hold(Hold::NoTrigger { .. })));
    }

    #[test]
    fn capacity_diverges_only_past_the_threshold() {
        let l = ladder(900, 1_000, 1_100, 1_200);
        // 31% consumed against a 30% threshold: refresh.
        let s = snap(l, 1_000, 310, 1_000);
        assert!(matches!(
            evaluate_capacity(&ctx(&s, Some(l))),
            CapacityDecision::Send(CapacityTrigger::Diverged {
                remaining_pct: 69,
                threshold_pct: 30,
                over_offered: false,
                ..
            })
        ));
        // 29% consumed: hold. Refreshing here would hand the epoch's risk budget back for free.
        let s = snap(l, 1_000, 290, 1_000);
        assert!(matches!(evaluate_capacity(&ctx(&s, Some(l))), CapacityDecision::Hold(Hold::NoTrigger { .. })));
    }

    #[test]
    fn an_epoch_the_inventory_can_no_longer_settle_is_cut_back() {
        // Inventory left the pool, so the plan is half the posted epoch. The posted one now
        // offers size the pool cannot deliver: an aggregator sees a fillable quote and the swap
        // reverts `ReserveFloorBreached`. Checking only the under-supplied direction would leave
        // that standing until some unrelated trigger happened to refresh it.
        let l = ladder(900, 1_000, 1_100, 1_200);
        let s = snap(l, 1_000, 0, 1_000);
        let mut c = ctx(&s, Some(l));
        c.capacity = CapacityPlan { bid: 1_000, ask: 500, bid_cut_by_inventory: false, ask_cut_by_inventory: true };
        assert!(matches!(
            evaluate_capacity(&c),
            CapacityDecision::Send(CapacityTrigger::Diverged {
                side: Side::Ask,
                remaining_pct: 200,
                over_offered: true,
                ..
            })
        ));

        // Inside the threshold in that direction: hold, so a dust-sized inventory change does
        // not churn the epoch.
        c.capacity = CapacityPlan { bid: 1_000, ask: 800, bid_cut_by_inventory: false, ask_cut_by_inventory: true };
        assert!(matches!(evaluate_capacity(&c), CapacityDecision::Hold(Hold::NoTrigger { .. })));
    }

    #[test]
    fn a_side_with_no_epoch_is_refreshed_before_anything_else() {
        // The chain has no ask epoch; the plan has one to post. Without this the pool quotes
        // zero on the ask however good the ladder is.
        let l = ladder(900, 1_000, 1_100, 1_200);
        let mut s = snap(l, 1_000, 0, 1_000);
        s.ask_capacity = 0;
        let mut c = ctx(&s, Some(l));
        c.capacity = CapacityPlan { bid: 1_000, ask: 1_000, bid_cut_by_inventory: false, ask_cut_by_inventory: false };
        assert_eq!(evaluate_capacity(&c), CapacityDecision::Send(CapacityTrigger::NoEpoch { side: Side::Ask }));
    }

    #[test]
    fn a_superseded_usage_generation_does_not_look_like_a_consumed_epoch() {
        // The live pool's exact shape: capGen 14, usedGen 13, a large raw ask counter. The pool
        // reads that usage as zero, so the epoch is full and must not be refreshed.
        let l = ladder(900, 1_000, 1_100, 1_200);
        let mut s = snap(l, 1_000, 990, 1_000);
        s.cap_gen = 14;
        s.used_gen = 13;
        assert_eq!(s.ask_used(), 0);
        assert!(matches!(evaluate_capacity(&ctx(&s, Some(l))), CapacityDecision::Hold(Hold::NoTrigger { .. })));

        // With the generations matching, the same counters do trigger.
        s.used_gen = 14;
        assert!(matches!(
            evaluate_capacity(&ctx(&s, Some(l))),
            CapacityDecision::Send(CapacityTrigger::Diverged { .. })
        ));
    }

    #[test]
    fn a_stale_feed_cannot_widen_an_epoch() {
        // archi_v2 5.3: capacity growth only while the reference price is coherent, so a stale
        // price cannot induce capacity churn.
        let l = ladder(900, 1_000, 1_100, 1_200);
        let mut s = snap(l, 1_000, 0, 1_000);
        s.bid_capacity = 0;
        let mut c = Context { feed: FeedStatus::Disconnected, ..ctx(&s, Some(l)) };
        c.capacity = CapacityPlan { bid: 1_000, ask: 1_000, bid_cut_by_inventory: false, ask_cut_by_inventory: false };
        assert_eq!(evaluate_capacity(&c), CapacityDecision::Hold(Hold::FeedNotLive(FeedStatus::Disconnected)));
    }

    #[test]
    fn a_halted_bot_refreshes_nothing() {
        let l = ladder(900, 1_000, 1_100, 1_200);
        let mut s = snap(l, 1_000, 0, 1_000);
        s.bid_capacity = 0;
        let mut c = Context { halted: true, ..ctx(&s, Some(l)) };
        c.capacity = CapacityPlan { bid: 1_000, ask: 1_000, bid_cut_by_inventory: false, ask_cut_by_inventory: false };
        assert_eq!(evaluate_capacity(&c), CapacityDecision::Hold(Hold::Halted));
    }

    #[test]
    fn a_zero_planned_epoch_is_not_posted_as_a_routine_refresh() {
        // Posting (0, 0) is the withdrawal the killswitch performs, and routing it through the
        // ordinary divergence path would let an inventory dip silently pull the pool's quotes.
        let l = ladder(900, 1_000, 1_100, 1_200);
        let s = snap(l, 1_000, 0, 1_000);
        let mut c = ctx(&s, Some(l));
        c.capacity = CapacityPlan { bid: 0, ask: 0, bid_cut_by_inventory: true, ask_cut_by_inventory: true };
        assert_eq!(evaluate_capacity(&c), CapacityDecision::Hold(Hold::NoRow));
    }
}
