//! When to push, and mostly when not to (archi_v2 §5.3).
//!
//! Drift is measured against the **executable top**, never `maxBid`, which is only the price at
//! zero usage: a taker arriving mid-epoch gets `executableTopBid(minBid, maxBid, capacity, used)`.
//! Comparing `maxBid` to `maxBid` errs both ways — it misses a move when an unchanged `maxBid`
//! hides a collapsed width, and invents one when a lower `maxBid` and a narrower width cancel out.
//!
//! Trigger order, first match wins:
//!
//! ```text
//! 1.  NoUsableQuote                the pool has no usable ladder at all
//! 1b. LadderStaleWithoutCapacity   a side is at zero depth and the stored row has gone stale
//! 2.  AdverseDrift                 the market moved AGAINST the posted quote
//! 3.  Heartbeat / Cadence          the quote is approaching expiry
//! 4.  FavourableDrift              the posted quote has merely become conservative
//! ```
//!
//! The usual maker asymmetry — chase a competitor eagerly, retreat slowly — is inverted here,
//! deliberately. On GIWA the prop pool is the only maker of consequence, so there is nobody to
//! lose the flow to: a basis point too conservative costs a little volume, while a basis point too
//! generous after the market has moved is a free option written to whoever notices it first. So
//! `adverse_drift_bps` is the TIGHT threshold and `favourable_drift_bps` the LOOSE one, and
//! [`crate::config::PairConfig::validate`] refuses a config with the two the other way round.
//!
//! Gates run before any trigger and abort rather than delay, because a gate firing means the state
//! is wrong rather than the timing:
//!
//! ```text
//! Halted -> ChainDown -> FeedNotLive -> ChainViewStale -> PoolPaused -> PushInFlight -> NoRow
//! ```
//!
//! `JumpWithdrawn` gates [`evaluate_capacity`] only. `PushInFlight` blocks rather than supersedes:
//! two `updateQuote`s for one pair in the mempool land in the order the sequencer picks, and the
//! older one may win.

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
    /// The pool has no capacity on at least one side and the stored ladder is not the one this
    /// cycle would post. Separate from [`drift`], which structurally cannot see it:
    /// `executable_top_ask` returns an infinity sentinel at zero capacity, so a withdrawn pool
    /// measures zero drift however far the market moved. Without it a cool-off ends with a stale
    /// row stored and the pool spends a block quoting the pre-jump price behind a restored epoch.
    LadderStaleWithoutCapacity,
    /// Nothing has been pushed for `push_interval_ms_max`, measured on our own clock. Separate
    /// from [`Self::Heartbeat`] because the chain cannot express it: `quote_age_secs` comes from
    /// `updatedAt`, a uint32 of **seconds**, so sub-second staleness is unobservable there.
    Cadence {
        /// Milliseconds since the last send for this pair.
        since_ms: u64,
    },
    /// The market moved against the posted quote.
    AdverseDrift {
        /// How far, in deci-bps (tenths of a bps) of the posted executable top.
        bps: u128,
        /// Which side moved furthest against us.
        side: Side,
        /// Threshold that was crossed, in deci-bps.
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
        /// How far, in deci-bps.
        bps: u128,
        /// Which side.
        side: Side,
        /// Threshold that was crossed, in deci-bps.
        threshold_bps: u32,
    },
}

impl Trigger {
    /// Short stable string for structured logs.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::NoUsableQuote => "no_usable_quote",
            Self::LadderStaleWithoutCapacity => "ladder_stale_without_capacity",
            Self::AdverseDrift { .. } => "adverse_drift",
            Self::Heartbeat { .. } => "heartbeat",
            Self::Cadence { .. } => "cadence",
            Self::FavourableDrift { .. } => "favourable_drift",
        }
    }

    /// Whether this trigger justifies re-posting a row identical to the one already stored. Only
    /// the heartbeat does: refreshing `updatedAt` is what it wants, and every other trigger
    /// re-sending identical bytes is a no-op transaction.
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
    /// The feed is not live; quoting from a stale price is what this prevents.
    FeedNotLive(FeedStatus),
    /// The last successful poll is too old to act on.
    ChainViewStale {
        /// Age of the view.
        age_secs: u64,
        /// Configured limit.
        limit_secs: u64,
    },
    /// The pool or the pair is paused, so pushing a row would succeed and change nothing.
    PoolPaused,
    /// A transaction for this pair is unconfirmed.
    PushInFlight,
    /// [`crate::jump`] is holding this pair's quotes down. **Blocks capacity only** — see
    /// [`evaluate_capacity`].
    JumpWithdrawn {
        /// Milliseconds left on the minimum cool-off. Zero means the timer arm is satisfied and
        /// the reference has not settled yet.
        cooloff_remaining_ms: u64,
    },
    /// No valid row could be built this cycle. The reason is logged where it was produced.
    NoRow,
    /// A trigger fired but the computed row is byte-identical to the stored one.
    Unchanged,
    /// Everything is healthy and nothing needs doing.
    NoTrigger {
        /// Largest adverse drift seen, in deci-bps, for the log.
        adverse_bps: u128,
        /// Largest favourable drift seen, in deci-bps.
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
            Self::JumpWithdrawn { .. } => "jump_withdrawn",
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

// --- Drift ---

/// How far the planned row's executable top has moved from the posted one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Drift {
    /// Largest move against us, in deci-bps (tenths of a bps): `10` is one bps.
    pub adverse_bps: u128,
    /// Which side that was.
    pub adverse_side: Option<Side>,
    /// Largest move in our favour, in deci-bps.
    pub favourable_bps: u128,
    /// Which side that was.
    pub favourable_side: Option<Side>,
}

/// Measure both sides at the current usage. Both ladders are evaluated at the **same** capacity
/// and usage — the ones on chain, since `updateQuote` changes neither; anything else compares two
/// points on two different curves and calls the difference a price move. A planned bid top below
/// the posted one means the market fell and we would overpay; a planned ask top above it means we
/// would undersell. A side with no capacity is skipped rather than counted as an infinite move,
/// which on the ask side also avoids reading [`NO_ASK`] as a price.
///
/// # Errors
/// [`CurveError`] if a stored ladder is inverted, which the chain's own validator excludes.
pub fn drift(snap: &Snap, planned: &Ladder) -> Result<Drift, CurveError> {
    let mut d = Drift::default();
    // A zero move records no side: the trigger arms pattern-match on the side being present, so
    // `Some(Bid)` alongside 0 bps would read as a move that did not happen.
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
            // 100_000, not 10_000: the unit is deci-bps, so a 0.5 bps threshold has something
            // finer than a whole bp to compare against.
            let bps = mul_div_floor(posted.abs_diff(want), 100_000, posted).unwrap_or(u128::MAX);
            note(bps, Side::Bid, want < posted, &mut d);
        }
    }

    if snap.ask_capacity > 0 {
        let used = snap.ask_used();
        let posted = executable_top_ask(snap.min_ask, snap.max_ask, snap.ask_capacity, used)?;
        let want = executable_top_ask(planned.min_ask, planned.max_ask, snap.ask_capacity, used)?;
        // `ask_capacity > 0` already excludes the `NO_ASK` infinity sentinel; the guard stays
        // because reading it as a number is a silent, enormous error rather than a loud one.
        if posted > 0 && posted != NO_ASK && want != NO_ASK {
            let bps = mul_div_floor(posted.abs_diff(want), 100_000, posted).unwrap_or(u128::MAX);
            note(bps, Side::Ask, want > posted, &mut d);
        }
    }

    Ok(d)
}

// --- Evaluation ---

/// Everything one evaluation needs. Plain data, so both tests and the loop build it the same way
/// and there is no mock to diverge from the real thing.
#[derive(Debug, Clone, Copy)]
pub struct Context<'a> {
    /// Block timestamp from the chain view. Quote age is measured against it rather than the
    /// local clock, because it is what `PropPool` compares to.
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
    /// [`crate::jump`] is holding this pair's quotes down.
    pub jump_withdrawn: bool,
    /// Milliseconds left on the minimum cool-off, for the log line.
    pub jump_cooloff_remaining_ms: u64,
    /// Configured heartbeat.
    pub heartbeat_secs: u64,
    /// Push at least this often, in milliseconds, measured from our own last send. `None` disables
    /// the cadence trigger and leaves the chain-derived heartbeat as the only time-based one.
    pub push_interval_ms_max: Option<u64>,
    /// Milliseconds since this pair's last send. `None` when nothing has been sent yet, which the
    /// other triggers already cover.
    pub since_last_push_ms: Option<u64>,
    /// The TIGHT threshold, for a move against us, in deci-bps (tenths of a bps). Must stay below
    /// `favourable_drift_bps`; [`crate::config::PairConfig::validate`] enforces it.
    pub adverse_drift_bps: u32,
    /// The LOOSE threshold, for a move in our favour, in deci-bps.
    pub favourable_drift_bps: u32,
    /// Capacity divergence threshold, in percent.
    pub capacity_divergence_pct: u32,
}

impl Context<'_> {
    /// The gates, in order; `None` means acting is meaningful. Shared by both decisions, because
    /// archi_v2 §5.3 requires that a stale price cannot induce capacity churn either.
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

    /// Whether the pool is currently offering no depth on at least one side, and therefore
    /// quoting nothing whatever ladder is stored.
    #[must_use]
    pub const fn withdrawn_on_chain(&self) -> bool {
        self.snap.bid_capacity == 0 || self.snap.ask_capacity == 0
    }

    /// The effective heartbeat: the configured one, but never so late that the pool's own
    /// freshness window expires first. `maxStaleSecs * 4 / 5` is archi_v2 §5.3's 0.8 factor; the
    /// margin exists because a heartbeat at exactly `maxStaleSecs` re-posts a quote that has
    /// already stopped being fillable, so the pool quotes nothing for one round trip each time.
    #[must_use]
    pub const fn heartbeat_limit(&self) -> u64 {
        let chain_bound = (self.snap.stale_secs_max as u64) * 4 / 5;
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

    // 1. Also covers a manager raising `minPrice` above a stored ladder, after which the pool
    //    rejects every fill until something is pushed.
    let unusable = ctx.snap.never_quoted() || ctx.snap.ladder().validate(ctx.min_price).is_err();
    if unusable {
        return Ok(finish(
            Decision::Send(Trigger::NoUsableQuote),
            ctx,
            &planned,
        ));
    }

    // 1b. Deliberately NOT gated on `jump_withdrawn`: the pool quotes nothing on that side
    //     whatever put it at zero, so keeping the row current costs a nearly-free transaction and
    //     lets capacity be restored in one step. `finish` still holds an unmoved row `Unchanged`.
    if ctx.withdrawn_on_chain() && planned != ctx.snap.ladder() {
        return Ok(finish(
            Decision::Send(Trigger::LadderStaleWithoutCapacity),
            ctx,
            &planned,
        ));
    }

    let d = drift(ctx.snap, &planned)?;

    // 2. The tight threshold, ahead of the heartbeat: a quote that is wrong now is more urgent
    //    than one that expires later.
    if d.adverse_bps >= u128::from(ctx.adverse_drift_bps) {
        if let Some(side) = d.adverse_side {
            let t = Trigger::AdverseDrift {
                bps: d.adverse_bps,
                side,
                threshold_bps: ctx.adverse_drift_bps,
            };
            return Ok(finish(Decision::Send(t), ctx, &planned));
        }
    }

    if age >= limit {
        let t = Trigger::Heartbeat {
            age_secs: age,
            limit_secs: limit,
        };
        return Ok(finish(Decision::Send(t), ctx, &planned));
    }

    // 3b. After the drift triggers, so a cycle where the reference did move is reported as a move.
    if let (Some(limit_ms), Some(since_ms)) = (ctx.push_interval_ms_max, ctx.since_last_push_ms) {
        if since_ms >= limit_ms {
            let t = Trigger::Cadence { since_ms };
            return Ok(finish(Decision::Send(t), ctx, &planned));
        }
    }

    // 4. The loose threshold.
    if d.favourable_bps >= u128::from(ctx.favourable_drift_bps) {
        if let Some(side) = d.favourable_side {
            let t = Trigger::FavourableDrift {
                bps: d.favourable_bps,
                side,
                threshold_bps: ctx.favourable_drift_bps,
            };
            return Ok(finish(Decision::Send(t), ctx, &planned));
        }
    }

    Ok(Decision::Hold(Hold::NoTrigger {
        adverse_bps: d.adverse_bps,
        favourable_bps: d.favourable_bps,
        age_secs: age,
    }))
}

/// Last gate: a trigger fired, but is the row actually different? Split out so the identical-row
/// rule is stated once; the heartbeat is exempt because refreshing `updatedAt` is its change.
fn finish(decision: Decision, ctx: &Context<'_>, planned: &Ladder) -> Decision {
    let Decision::Send(t) = decision else {
        return decision;
    };
    if *planned == ctx.snap.ladder() && !t.justifies_identical_row() {
        return Decision::Hold(Hold::Unchanged);
    }
    decision
}

/// Decide whether to post a fresh capacity epoch. Separate from the quote decision because they
/// are separate transactions against separate storage words: `updateQuote` is the price decision
/// and `refreshCapacity` the risk decision. They share the gates and nothing else.
#[must_use]
pub fn evaluate_capacity(ctx: &Context<'_>) -> CapacityDecision {
    if let Some(h) = ctx.gates() {
        return CapacityDecision::Hold(h);
    }
    // The jump gate is on THIS decision only. The withdrawal *is* a zero epoch, so without the
    // gate `NoEpoch` below would re-arm the pool on the next cycle. `evaluate_quote` stays ungated
    // so the ladder tracks the market through the cool-off and the resume is one `refreshCapacity`
    // rather than an epoch armed at a pre-jump price.
    if ctx.jump_withdrawn {
        return CapacityDecision::Hold(Hold::JumpWithdrawn {
            cooloff_remaining_ms: ctx.jump_cooloff_remaining_ms,
        });
    }
    // A refresh that would post zero is a withdrawal, and withdrawal is the killswitch's job
    // rather than a routine trigger's.
    if ctx.capacity.bid == 0 || ctx.capacity.ask == 0 {
        return CapacityDecision::Hold(Hold::NoRow);
    }

    let sides = [
        (
            Side::Bid,
            ctx.snap.bid_capacity,
            ctx.snap.bid_used(),
            ctx.capacity.bid,
        ),
        (
            Side::Ask,
            ctx.snap.ask_capacity,
            ctx.snap.ask_used(),
            ctx.capacity.ask,
        ),
    ];

    // A side with no epoch at all quotes zero however good the ladder is.
    for (side, capacity, _, _) in sides {
        if capacity == 0 {
            return CapacityDecision::Send(CapacityTrigger::NoEpoch { side });
        }
    }

    // Divergence against the PLANNED epoch, in BOTH directions. Below the plan the pool quotes
    // less depth than intended; above it, inventory has left and the posted epoch offers size the
    // pool cannot settle, so `getAmountOut` advertises a quote that reverts `ReserveFloorBreached`.
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

    CapacityDecision::Hold(Hold::NoTrigger {
        adverse_bps: 0,
        favourable_bps: 0,
        age_secs: 0,
    })
}

#[cfg(test)]
mod tests {
    /// The cadence must express an interval `updatedAt`'s second resolution cannot. The planned
    /// ladder has to differ from the stored one: unlike the heartbeat, the cadence is not exempt
    /// from the unchanged-row guard.
    #[test]
    fn cadence_fires_below_the_second_the_heartbeat_cannot_see() {
        let l = ladder(900, 1_000, 1_100, 1_200);
        let s = snap(l, 1_000, 0, 100);
        let mut c = ctx(&s, Some(ladder(901, 1_001, 1_101, 1_201)));
        c.heartbeat_secs = 5;
        c.block_timestamp = 100; // age 0s: the heartbeat has nothing to say
                                 // Both thresholds raised past the 9 bps this row implies.
        c.adverse_drift_bps = 1_000;
        c.favourable_drift_bps = 1_000;
        c.push_interval_ms_max = Some(330);

        c.since_last_push_ms = Some(200);
        assert!(
            matches!(
                evaluate_quote(&c),
                Ok(Decision::Hold(_)) | Ok(Decision::Send(Trigger::AdverseDrift { .. }))
            ),
            "inside the cadence, this trigger must not fire"
        );

        c.since_last_push_ms = Some(400);
        match evaluate_quote(&c) {
            Ok(Decision::Send(Trigger::Cadence { since_ms })) => assert_eq!(since_ms, 400),
            other => panic!("expected a cadence send past the interval, got {other:?}"),
        }
    }

    /// Unset means the pair keeps only the second-resolution heartbeat it always had.
    #[test]
    fn cadence_is_off_when_unconfigured() {
        let l = ladder(900, 1_000, 1_100, 1_200);
        let s = snap(l, 1_000, 0, 100);
        let mut c = ctx(&s, Some(l));
        c.block_timestamp = 100;
        c.push_interval_ms_max = None;
        c.since_last_push_ms = Some(999_999);
        assert!(
            !matches!(
                evaluate_quote(&c),
                Ok(Decision::Send(Trigger::Cadence { .. }))
            ),
            "an unconfigured cadence must never fire"
        );
    }

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
            stale_secs_max: 3_600,
        }
    }

    fn ladder(min_bid: u128, max_bid: u128, min_ask: u128, max_ask: u128) -> Ladder {
        Ladder {
            min_bid,
            max_bid,
            min_ask,
            max_ask,
        }
    }

    /// Healthy: nothing gated, quote fresh, row identical to what is stored.
    fn ctx<'a>(s: &'a Snap, planned: Option<Ladder>) -> Context<'a> {
        Context {
            push_interval_ms_max: None,
            since_last_push_ms: None,
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
            jump_withdrawn: false,
            jump_cooloff_remaining_ms: 0,
            heartbeat_secs: 2_400,
            adverse_drift_bps: 20,
            favourable_drift_bps: 80,
            capacity_divergence_pct: 30,
        }
    }

    // --- Drift is measured at the current usage, not at maxBid ---

    #[test]
    fn a_naive_max_bid_comparison_would_miss_this_move() {
        // Posted 800..1000 over 100 of capacity, half consumed: a taker gets 900.
        let s = snap(ladder(800, 1_000, 1_100, 1_300), 100, 50, 1_000);
        assert_eq!(executable_top_bid(800, 1_000, 100, 50), Ok(900));

        // Same maxBid, zero width: a taker gets 1000, 11% better, and `maxBid` sees nothing.
        let planned = ladder(1_000, 1_000, 1_100, 1_300);
        assert_eq!(
            planned.max_bid, s.max_bid,
            "the naive comparison is blind here by construction"
        );

        let d = drift(&s, &planned).unwrap();
        assert_eq!(d.favourable_bps, 11_111);
        assert_eq!(d.favourable_side, Some(Side::Bid));

        let c = Context {
            adverse_drift_bps: 20,
            favourable_drift_bps: 80,
            ..ctx(&s, Some(planned))
        };
        assert!(matches!(
            evaluate_quote(&c).unwrap(),
            Decision::Send(Trigger::FavourableDrift {
                side: Side::Bid,
                ..
            })
        ));
    }

    #[test]
    fn a_naive_max_bid_comparison_would_invent_this_move() {
        // A push that changes nothing: 800..1000 and 850..950 both execute at 900 at 50/100.
        let s = snap(ladder(800, 1_000, 1_100, 1_300), 100, 50, 1_000);
        let planned = ladder(850, 950, 1_150, 1_250);
        assert_eq!(executable_top_bid(850, 950, 100, 50), Ok(900));
        assert_eq!(
            executable_top_ask(1_150, 1_250, 100, 50),
            executable_top_ask(1_100, 1_300, 100, 50)
        );

        // `maxBid` moved 500 bps, so the naive comparison fires; the executable top did not.
        assert_eq!(drift(&s, &planned).unwrap(), Drift::default());
        let c = ctx(&s, Some(planned));
        assert!(matches!(
            evaluate_quote(&c).unwrap(),
            Decision::Hold(Hold::NoTrigger {
                adverse_bps: 0,
                favourable_bps: 0,
                ..
            })
        ));
    }

    #[test]
    fn drift_direction_follows_which_way_it_hurts() {
        let s = snap(ladder(900, 1_000, 1_100, 1_200), 100, 0, 1_000);

        // Market fell: our posted bid is now too high, and our posted ask is conservative.
        let down = ladder(800, 900, 1_000, 1_100);
        let d = drift(&s, &down).unwrap();
        assert_eq!(
            d.adverse_side,
            Some(Side::Bid),
            "a lower planned bid means we are overpaying"
        );
        assert_eq!(d.adverse_bps, 10_000);
        assert_eq!(d.favourable_side, Some(Side::Ask));

        // Market rose: our posted ask is now too low, our bid is conservative.
        let up = ladder(1_000, 1_100, 1_200, 1_300);
        let d = drift(&s, &up).unwrap();
        assert_eq!(
            d.adverse_side,
            Some(Side::Ask),
            "a higher planned ask means we are underselling"
        );
        assert_eq!(d.favourable_side, Some(Side::Bid));
    }

    #[test]
    fn a_side_with_no_capacity_is_skipped_rather_than_read_as_a_price() {
        // Reading the NO_ASK sentinel as a number is ~10^36 bps of drift, every cycle forever.
        let mut s = snap(ladder(900, 1_000, 1_100, 1_200), 100, 0, 1_000);
        s.ask_capacity = 0;
        assert_eq!(executable_top_ask(s.min_ask, s.max_ask, 0, 0), Ok(NO_ASK));

        let d = drift(&s, &ladder(900, 1_000, 5_000, 6_000)).unwrap();
        assert_eq!(d.adverse_bps, 0);
        assert_eq!(d.adverse_side, None);
        assert_eq!(d.favourable_bps, 0);
    }

    // --- Triggers firing ---

    #[test]
    fn a_never_quoted_pair_is_pushed_immediately() {
        let s = snap(ladder(0, 0, 0, 0), 100, 0, 0);
        let c = ctx(&s, Some(ladder(900, 1_000, 1_100, 1_200)));
        assert_eq!(
            evaluate_quote(&c).unwrap(),
            Decision::Send(Trigger::NoUsableQuote)
        );
    }

    #[test]
    fn a_stored_ladder_below_a_raised_min_price_is_pushed_immediately() {
        // After a raised `minPrice` the pool rejects every fill, and nothing else here notices.
        let s = snap(ladder(900, 1_000, 1_100, 1_200), 100, 0, 1_000);
        let c = Context {
            min_price: 950,
            ..ctx(&s, Some(ladder(960, 1_000, 1_100, 1_200)))
        };
        assert_eq!(
            evaluate_quote(&c).unwrap(),
            Decision::Send(Trigger::NoUsableQuote)
        );
    }

    #[test]
    fn adverse_drift_fires_at_the_threshold_and_not_below_it() {
        let s = snap(
            ladder(1_000_000, 1_000_000, 2_000_000, 2_000_000),
            100,
            0,
            1_000,
        );
        let c = |bid: u128| ctx(&s, Some(ladder(bid, bid, 2_000_000, 2_000_000)));

        // 1 bp under: below the 2 bp threshold, and the ask side did not move. Hold.
        let just_under = 1_000_000 - 100;
        assert_eq!(
            drift(&s, &ladder(just_under, just_under, 2_000_000, 2_000_000))
                .unwrap()
                .adverse_bps,
            10
        );
        assert!(!evaluate_quote(&c(just_under)).unwrap().sends());

        // Exactly 2 bp: fires.
        let at = 1_000_000 - 200;
        assert_eq!(
            drift(&s, &ladder(at, at, 2_000_000, 2_000_000))
                .unwrap()
                .adverse_bps,
            20
        );
        assert!(matches!(
            evaluate_quote(&c(at)).unwrap(),
            Decision::Send(Trigger::AdverseDrift {
                bps: 20,
                side: Side::Bid,
                threshold_bps: 20
            })
        ));
    }

    #[test]
    fn a_sub_one_bp_threshold_fires_where_a_whole_bp_threshold_would_not() {
        // Why the unit is deci-bps: 0.5 bp and 1 bp have to be distinguishable.
        let s = snap(
            ladder(1_000_000, 1_000_000, 2_000_000, 2_000_000),
            100,
            0,
            1_000,
        );
        let moved = 1_000_000 - 50; // 0.5 bp move.
        let planned = ladder(moved, moved, 2_000_000, 2_000_000);
        assert_eq!(drift(&s, &planned).unwrap().adverse_bps, 5);

        let loose = Context {
            adverse_drift_bps: 10,
            ..ctx(&s, Some(planned))
        };
        assert!(
            !evaluate_quote(&loose).unwrap().sends(),
            "5 deci-bps must not clear a 10 deci-bps threshold"
        );

        let tight = Context {
            adverse_drift_bps: 5,
            ..ctx(&s, Some(planned))
        };
        assert!(matches!(
            evaluate_quote(&tight).unwrap(),
            Decision::Send(Trigger::AdverseDrift {
                bps: 5,
                threshold_bps: 5,
                ..
            })
        ));
    }

    #[test]
    fn favourable_drift_needs_the_looser_threshold() {
        let s = snap(
            ladder(1_000_000, 1_000_000, 2_000_000, 2_000_000),
            100,
            0,
            1_000,
        );
        // Bid 5 bp better, ask unchanged: favourable, under the 8 bp threshold, so hold.
        let better = 1_000_000 + 500;
        let c = ctx(&s, Some(ladder(better, better, 2_000_000, 2_000_000)));
        let d = drift(&s, c.planned.as_ref().unwrap()).unwrap();
        assert_eq!((d.adverse_bps, d.favourable_bps), (0, 50));
        assert!(matches!(
            evaluate_quote(&c).unwrap(),
            Decision::Hold(Hold::NoTrigger { .. })
        ));

        // 8 bp: fires.
        let better = 1_000_000 + 800;
        let c = ctx(&s, Some(ladder(better, better, 2_000_000, 2_000_000)));
        assert!(matches!(
            evaluate_quote(&c).unwrap(),
            Decision::Send(Trigger::FavourableDrift {
                bps: 80,
                threshold_bps: 80,
                ..
            })
        ));
    }

    #[test]
    fn adverse_drift_outranks_the_heartbeat() {
        // Both are due. The reported reason must be the one that costs money.
        let s = snap(
            ladder(1_000_000, 1_000_000, 2_000_000, 2_000_000),
            100,
            0,
            1_000,
        );
        let moved = 1_000_000 - 1_000;
        let c = Context {
            block_timestamp: 1_000 + 3_000,
            ..ctx(&s, Some(ladder(moved, moved, 2_000_000, 2_000_000)))
        };
        assert!(matches!(
            evaluate_quote(&c).unwrap(),
            Decision::Send(Trigger::AdverseDrift { .. })
        ));
    }

    #[test]
    fn the_heartbeat_fires_before_the_pools_own_window_expires() {
        let l = ladder(1_000_000, 1_000_000, 2_000_000, 2_000_000);
        let s = snap(l, 100, 0, 1_000);
        // maxStaleSecs is 3600, so the bound is 2880 even though the config says 2400.
        let c = ctx(&s, Some(l));
        assert_eq!(c.heartbeat_limit(), 2_400);

        // A config looser than the chain window is clamped to it, not honoured.
        let c = Context {
            heartbeat_secs: 999_999,
            ..ctx(&s, Some(l))
        };
        assert_eq!(c.heartbeat_limit(), 2_880);
        assert!(
            c.heartbeat_limit() < u64::from(s.stale_secs_max),
            "the quote must never expire between pushes"
        );
    }

    #[test]
    fn the_heartbeat_re_posts_an_identical_row_and_nothing_else_does() {
        let l = ladder(1_000_000, 1_000_000, 2_000_000, 2_000_000);
        let s = snap(l, 100, 0, 1_000);

        // One second short of the heartbeat, identical row: hold.
        let c = Context {
            block_timestamp: 1_000 + 2_399,
            ..ctx(&s, Some(l))
        };
        assert!(matches!(
            evaluate_quote(&c).unwrap(),
            Decision::Hold(Hold::NoTrigger { .. })
        ));

        // At the heartbeat, identical row: send anyway, to refresh `updatedAt`.
        let c = Context {
            block_timestamp: 1_000 + 2_400,
            ..ctx(&s, Some(l))
        };
        assert_eq!(
            evaluate_quote(&c).unwrap(),
            Decision::Send(Trigger::Heartbeat {
                age_secs: 2_400,
                limit_secs: 2_400
            })
        );
    }

    #[test]
    fn a_trigger_on_an_identical_row_holds_unless_it_is_the_heartbeat() {
        // `NoUsableQuote` on an already-identical row: re-storing identical bytes buys nothing.
        let l = ladder(0, 0, 0, 0);
        let s = snap(l, 100, 0, 0);
        let c = ctx(&s, Some(l));
        assert_eq!(evaluate_quote(&c).unwrap(), Decision::Hold(Hold::Unchanged));
    }

    // --- Gates ---

    #[test]
    fn every_gate_aborts_and_they_abort_in_order() {
        let l = ladder(900, 1_000, 1_100, 1_200);
        // A never-quoted pair, so a trigger is definitely pending behind every gate.
        let s = snap(ladder(0, 0, 0, 0), 100, 0, 0);
        let base = ctx(&s, Some(l));

        let cases: Vec<(Context<'_>, Hold)> = vec![
            (
                Context {
                    halted: true,
                    ..base
                },
                Hold::Halted,
            ),
            (
                Context {
                    chain: ChainStatus::Down { stale_secs: 600 },
                    ..base
                },
                Hold::ChainDown,
            ),
            (
                Context {
                    feed: FeedStatus::NoQuorum { have: 1, need: 2 },
                    ..base
                },
                Hold::FeedNotLive(FeedStatus::NoQuorum { have: 1, need: 2 }),
            ),
            (
                Context {
                    feed: FeedStatus::Dispersed {
                        dispersion_bps: 600,
                        limit_bps: 250,
                        venues: 3,
                    },
                    ..base
                },
                Hold::FeedNotLive(FeedStatus::Dispersed {
                    dispersion_bps: 600,
                    limit_bps: 250,
                    venues: 3,
                }),
            ),
            (
                Context {
                    view_age_secs: 21,
                    ..base
                },
                Hold::ChainViewStale {
                    age_secs: 21,
                    limit_secs: 20,
                },
            ),
            (
                Context {
                    in_flight: true,
                    ..base
                },
                Hold::PushInFlight,
            ),
        ];
        for (c, expected) in cases {
            assert_eq!(
                evaluate_quote(&c).unwrap(),
                Decision::Hold(expected),
                "gate {expected:?} did not abort"
            );
        }

        // Paused needs its own snapshot.
        let mut paused = s;
        paused.flags = 1;
        let c = ctx(&paused, Some(l));
        assert_eq!(
            evaluate_quote(&c).unwrap(),
            Decision::Hold(Hold::PoolPaused)
        );

        // Precedence: halted outranks everything, including a down chain.
        let c = Context {
            halted: true,
            chain: ChainStatus::Down { stale_secs: 600 },
            ..base
        };
        assert_eq!(evaluate_quote(&c).unwrap(), Decision::Hold(Hold::Halted));
    }

    #[test]
    fn a_degraded_chain_still_quotes() {
        // Degraded widens the spread upstream; stopping here turns an RPC wobble into an outage.
        let s = snap(ladder(0, 0, 0, 0), 100, 0, 0);
        let c = Context {
            chain: ChainStatus::Degraded { stale_secs: 40 },
            ..ctx(&s, Some(ladder(900, 1_000, 1_100, 1_200)))
        };
        assert!(evaluate_quote(&c).unwrap().sends());
    }

    #[test]
    fn a_row_that_could_not_be_built_holds_rather_than_sending_the_old_one() {
        let s = snap(ladder(0, 0, 0, 0), 100, 0, 0);
        let c = ctx(&s, None);
        assert_eq!(evaluate_quote(&c).unwrap(), Decision::Hold(Hold::NoRow));
    }

    #[test]
    fn a_feed_without_quorum_blocks_even_a_pair_that_has_never_quoted() {
        // No exception for a never-quoted pair: an uncorroborated price is how the first fill
        // gets picked off.
        let s = snap(ladder(0, 0, 0, 0), 100, 0, 0);
        let no_quorum = FeedStatus::NoQuorum { have: 1, need: 2 };
        let c = Context {
            feed: no_quorum,
            ..ctx(&s, Some(ladder(900, 1_000, 1_100, 1_200)))
        };
        assert_eq!(
            evaluate_quote(&c).unwrap(),
            Decision::Hold(Hold::FeedNotLive(no_quorum))
        );
    }

    #[test]
    fn venues_that_disagree_block_a_push_exactly_as_a_dead_feed_does() {
        // A split cross-section is the absence of a price: the mean of two camps 60 bp apart is
        // a number no venue is showing.
        let l = ladder(900, 1_000, 1_100, 1_200);
        let s = snap(l, 100, 0, 1_000);
        let dispersed = FeedStatus::Dispersed {
            dispersion_bps: 600,
            limit_bps: 250,
            venues: 4,
        };
        let c = Context {
            feed: dispersed,
            ..ctx(&s, Some(ladder(800, 900, 1_000, 1_100)))
        };
        assert_eq!(
            evaluate_quote(&c).unwrap(),
            Decision::Hold(Hold::FeedNotLive(dispersed))
        );
        assert_eq!(
            evaluate_capacity(&c),
            CapacityDecision::Hold(Hold::FeedNotLive(dispersed))
        );
    }

    // --- Capacity ---

    #[test]
    fn a_full_epoch_does_not_get_refreshed() {
        let l = ladder(900, 1_000, 1_100, 1_200);
        let s = snap(l, 1_000, 0, 1_000);
        assert!(matches!(
            evaluate_capacity(&ctx(&s, Some(l))),
            CapacityDecision::Hold(Hold::NoTrigger { .. })
        ));
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
        assert!(matches!(
            evaluate_capacity(&ctx(&s, Some(l))),
            CapacityDecision::Hold(Hold::NoTrigger { .. })
        ));
    }

    #[test]
    fn an_epoch_the_inventory_can_no_longer_settle_is_cut_back() {
        // Inventory left, so the posted epoch offers size the swap reverts on. Checking only the
        // under-supplied direction would leave that standing until some unrelated trigger fired.
        let l = ladder(900, 1_000, 1_100, 1_200);
        let s = snap(l, 1_000, 0, 1_000);
        let mut c = ctx(&s, Some(l));
        c.capacity = CapacityPlan {
            bid: 1_000,
            ask: 500,
            bid_cut_by_inventory: false,
            ask_cut_by_inventory: true,
        };
        assert!(matches!(
            evaluate_capacity(&c),
            CapacityDecision::Send(CapacityTrigger::Diverged {
                side: Side::Ask,
                remaining_pct: 200,
                over_offered: true,
                ..
            })
        ));

        // Inside the threshold: hold, so a dust-sized inventory change does not churn the epoch.
        c.capacity = CapacityPlan {
            bid: 1_000,
            ask: 800,
            bid_cut_by_inventory: false,
            ask_cut_by_inventory: true,
        };
        assert!(matches!(
            evaluate_capacity(&c),
            CapacityDecision::Hold(Hold::NoTrigger { .. })
        ));
    }

    #[test]
    fn a_side_with_no_epoch_is_refreshed_before_anything_else() {
        // No ask epoch on chain: without this the pool quotes zero however good the ladder is.
        let l = ladder(900, 1_000, 1_100, 1_200);
        let mut s = snap(l, 1_000, 0, 1_000);
        s.ask_capacity = 0;
        let mut c = ctx(&s, Some(l));
        c.capacity = CapacityPlan {
            bid: 1_000,
            ask: 1_000,
            bid_cut_by_inventory: false,
            ask_cut_by_inventory: false,
        };
        assert_eq!(
            evaluate_capacity(&c),
            CapacityDecision::Send(CapacityTrigger::NoEpoch { side: Side::Ask })
        );
    }

    #[test]
    fn a_superseded_usage_generation_does_not_look_like_a_consumed_epoch() {
        // capGen 14 against usedGen 13: the pool reads that usage as zero, so the epoch is full.
        let l = ladder(900, 1_000, 1_100, 1_200);
        let mut s = snap(l, 1_000, 990, 1_000);
        s.cap_gen = 14;
        s.used_gen = 13;
        assert_eq!(s.ask_used(), 0);
        assert!(matches!(
            evaluate_capacity(&ctx(&s, Some(l))),
            CapacityDecision::Hold(Hold::NoTrigger { .. })
        ));

        // With the generations matching, the same counters do trigger.
        s.used_gen = 14;
        assert!(matches!(
            evaluate_capacity(&ctx(&s, Some(l))),
            CapacityDecision::Send(CapacityTrigger::Diverged { .. })
        ));
    }

    #[test]
    fn a_stale_feed_cannot_widen_an_epoch() {
        // archi_v2 5.3: capacity grows only while the reference price is coherent.
        let l = ladder(900, 1_000, 1_100, 1_200);
        let mut s = snap(l, 1_000, 0, 1_000);
        s.bid_capacity = 0;
        let no_quorum = FeedStatus::NoQuorum { have: 0, need: 2 };
        let mut c = Context {
            feed: no_quorum,
            ..ctx(&s, Some(l))
        };
        c.capacity = CapacityPlan {
            bid: 1_000,
            ask: 1_000,
            bid_cut_by_inventory: false,
            ask_cut_by_inventory: false,
        };
        assert_eq!(
            evaluate_capacity(&c),
            CapacityDecision::Hold(Hold::FeedNotLive(no_quorum))
        );
    }

    #[test]
    fn a_halted_bot_refreshes_nothing() {
        let l = ladder(900, 1_000, 1_100, 1_200);
        let mut s = snap(l, 1_000, 0, 1_000);
        s.bid_capacity = 0;
        let mut c = Context {
            halted: true,
            ..ctx(&s, Some(l))
        };
        c.capacity = CapacityPlan {
            bid: 1_000,
            ask: 1_000,
            bid_cut_by_inventory: false,
            ask_cut_by_inventory: false,
        };
        assert_eq!(evaluate_capacity(&c), CapacityDecision::Hold(Hold::Halted));
    }

    // --- The jump withdrawal ---

    #[test]
    fn a_jump_cooloff_holds_capacity_down_and_lets_the_ladder_keep_moving() {
        // The withdrawal IS the zero epoch, so capacity must be gated or `NoEpoch` re-arms the
        // pool; the quote must not be, or the resume re-arms at the pre-jump price.
        let l = ladder(1_000_000, 1_000_000, 2_000_000, 2_000_000);
        let mut s = snap(l, 1_000, 0, 1_000);
        s.bid_capacity = 0;
        s.ask_capacity = 0;

        let moved = ladder(990_000, 990_000, 1_990_000, 1_990_000);
        let c = Context {
            jump_withdrawn: true,
            jump_cooloff_remaining_ms: 21_000,
            // The epoch the pool would post: the withdrawal holds this rather than lacking one.
            capacity: CapacityPlan {
                bid: 1_000,
                ask: 1_000,
                bid_cut_by_inventory: false,
                ask_cut_by_inventory: false,
            },
            ..ctx(&s, Some(moved))
        };
        assert_eq!(
            evaluate_capacity(&c),
            CapacityDecision::Hold(Hold::JumpWithdrawn {
                cooloff_remaining_ms: 21_000
            }),
            "capacity must stay at zero for the whole cool-off"
        );
        assert_eq!(
            evaluate_quote(&c).unwrap(),
            Decision::Send(Trigger::LadderStaleWithoutCapacity),
            "and the ladder must keep tracking the market behind it"
        );

        // The row has to have moved, or a quiet cool-off is one transaction a second for 30 s.
        let quiet = Context {
            planned: Some(l),
            ..c
        };
        assert!(
            !evaluate_quote(&quiet).unwrap().sends(),
            "an unchanged row must not be re-posted"
        );

        // The epoch comes back in one transaction, against an already-current ladder.
        let resumed = Context {
            jump_withdrawn: false,
            ..c
        };
        assert_eq!(
            evaluate_capacity(&resumed),
            CapacityDecision::Send(CapacityTrigger::NoEpoch { side: Side::Bid })
        );
    }

    #[test]
    fn drift_is_blind_to_a_withdrawn_pool_which_is_why_the_new_trigger_exists() {
        // Why the trigger is not a case of adverse drift: at zero capacity `drift` skips both
        // sides, however far the market has moved.
        let l = ladder(1_000_000, 1_000_000, 2_000_000, 2_000_000);
        let mut s = snap(l, 1_000, 0, 1_000);
        s.bid_capacity = 0;
        s.ask_capacity = 0;
        let moved = ladder(900_000, 900_000, 1_900_000, 1_900_000); // 1000 bp away
        assert_eq!(
            drift(&s, &moved).unwrap(),
            Drift::default(),
            "drift genuinely cannot see it"
        );
        assert!(matches!(
            evaluate_quote(&ctx(&s, Some(moved))).unwrap(),
            Decision::Send(Trigger::LadderStaleWithoutCapacity)
        ));
    }

    #[test]
    fn a_halted_bot_outranks_the_jump_gate() {
        // The shared gates run first, so the operator reads the latch rather than the symptom.
        let l = ladder(900, 1_000, 1_100, 1_200);
        let mut s = snap(l, 1_000, 0, 1_000);
        s.bid_capacity = 0;
        let c = Context {
            halted: true,
            jump_withdrawn: true,
            ..ctx(&s, Some(l))
        };
        assert_eq!(evaluate_capacity(&c), CapacityDecision::Hold(Hold::Halted));
    }

    #[test]
    fn a_zero_planned_epoch_is_not_posted_as_a_routine_refresh() {
        // Posting (0, 0) is the killswitch's withdrawal; via divergence an inventory dip would
        // silently pull the pool's quotes.
        let l = ladder(900, 1_000, 1_100, 1_200);
        let s = snap(l, 1_000, 0, 1_000);
        let mut c = ctx(&s, Some(l));
        c.capacity = CapacityPlan {
            bid: 0,
            ask: 0,
            bid_cut_by_inventory: true,
            ask_cut_by_inventory: true,
        };
        assert_eq!(evaluate_capacity(&c), CapacityDecision::Hold(Hold::NoRow));
    }
}
