//! Markout: measuring, per counterparty, whether a fill was good.
//!
//! # Why this exists before anything that uses it
//!
//! The pool has been quoting without ever looking at what its fills did afterwards. Every
//! parameter in `updater.toml` — the half-spread, the volatility coefficient, the jump threshold —
//! is currently tuned against a simulator, and the simulator is only as good as its assumptions
//! about who is trading. Markout is the measurement that replaces those assumptions with
//! observations, and nothing downstream of it can be honestly tuned until it exists.
//!
//! It is also the *un-confounded* version of the flow signal, which is the reason it comes first
//! rather than order-flow imbalance.
//!
//! # The confounding this avoids, and why it matters here more than in the literature
//!
//! Order flow imbalance is the usual leading signal: Cont, Kukanov and Stoikov find price changes
//! are close to linear in it. The standard objection is that OFI is not exogenous — price moves
//! cause flow as much as flow causes price, and an informed trader's order and the price move are
//! both effects of the same news, so a regression on OFI measures a common cause rather than an
//! impact.
//!
//! For a maker reading *its own* fills there is a third and sharper problem. Our fill flow is the
//! intersection of the market's demand with **our own quote**. Quote tight and we are hit more;
//! quote wide and we are hit less. "We are being hit a lot" can mean "the price is moving" or it
//! can mean "we are pricing badly", and raw fill counts cannot tell those apart. Acting on the
//! second as though it were the first is a feedback loop: we widen, we are hit less, we read that
//! as calm, we tighten.
//!
//! Markout does not have that problem. It does not infer whether a fill was informed from how
//! often we were hit; it observes what the fill was worth afterwards. The cost is a lag — the
//! answer arrives one horizon later — and that lag is the honest price of an unconfounded signal.
//!
//! Two partial defences against the confounding remain useful for the *fast* signal built on top
//! of this, and are noted here because this module supplies their inputs:
//!
//! * Use the **imbalance** between sides rather than the level. Our spread applies to both sides,
//!   so it largely differences out.
//! * Use the **external book as a control**. If our fills lean one way and the venues we quote
//!   from lean the same way, the market is moving; if only ours do, our price is wrong.
//!
//! # Whose score is it
//!
//! Scored on the **receiver**, not the sender. For anything routed, `sender` is the adapter
//! contract, so scoring it would collapse every routed trade in the system into one counterparty
//! and score the router rather than the trader. A searcher calling the pool directly has
//! `sender == receiver`, so nothing is lost in the case that matters most.
//!
//! `partnerId` is recorded alongside but never trusted as identity: it is a number the caller
//! supplies about itself. It is a routing-source tag for attribution, not a credential.
//!
//! # Sign convention
//!
//! Positive markout means the pool won. A fill whose markout at +60s is −80 bp lost the pool 80
//! basis points of its notional against where the market went, which is the signature of having
//! been picked off rather than of having earned a spread.

use std::collections::{HashMap, VecDeque};

use serde::{Deserialize, Serialize};

use alloy_primitives::Address;

/// Horizons, in seconds, at which every fill is re-marked.
///
/// One second is roughly the block time and answers "was this taken by someone who knew something
/// in the same instant". Sixty is long enough for a real move to have resolved. Ten sits between
/// them because the interesting failure — a jump the pool quoted through — resolves in that range,
/// and having only the two ends would leave it invisible.
pub const HORIZONS_SECS: [u64; 3] = [1, 10, 60];

/// How long a reference observation is kept so a fill can be marked against it.
///
/// The longest horizon plus slack for a late-arriving log. Beyond this a fill is dropped
/// unmarked and counted, rather than marked against whatever happens to be nearest — a markout
/// computed against the wrong reference is worse than a missing one, because it looks like data.
const REF_RETENTION_SECS: u64 = HORIZONS_SECS[2] + 120;

/// One observed fill, waiting for its horizons to mature.
#[derive(Debug, Clone)]
pub struct Fill {
    /// Which market this fill was against.
    pub pair_id: u16,
    /// The counterparty this fill is scored against. See the module docs: receiver, not sender.
    pub receiver: Address,
    /// The routing-source tag the caller supplied about itself. Attribution, never identity.
    pub partner_id: u128,
    /// True when the pool bought base and paid quote.
    pub is_bid: bool,
    /// What the taker paid, in the input token's own units.
    pub amount_in: u128,
    /// What the pool paid out, in the output token's own units.
    pub amount_out: u128,
    /// Block timestamp of the fill.
    pub at_secs: u64,
    /// Our own reference at the moment of the fill, in the pair's price units.
    pub ref_at_fill: u128,
    /// The pair's decimal alignment, so the two legs can be compared.
    pub price_scale_exp: u8,
}

impl Fill {
    /// Markout in hundredths of a basis point, positive when the pool won.
    ///
    /// The pool's position after a bid is `+base, -quote`; after an ask it is `-base, +quote`. Its
    /// value at a later reference is that position marked at that reference, and the markout is
    /// what that is worth against the notional it traded.
    ///
    /// Computed in signed 128-bit rather than the crate's usual unsigned path because a markout is
    /// meaningfully negative — that is the entire point of measuring it — and saturating a loss to
    /// zero would hide exactly the observation this module exists to make.
    pub fn markout_e2(&self, ref_later: u128) -> Option<i128> {
        let scale = 10i128.checked_pow(u32::from(self.price_scale_exp))?;
        let base = i128::try_from(if self.is_bid { self.amount_in } else { self.amount_out }).ok()?;
        let quote = i128::try_from(if self.is_bid { self.amount_out } else { self.amount_in }).ok()?;
        let later = i128::try_from(ref_later).ok()?;

        // What the base leg is worth at the later reference, in quote units.
        let base_value = base.checked_mul(later)?.checked_div(scale)?;
        // Bid: the pool paid `quote` and holds base now worth `base_value`.
        // Ask: the pool gave up base now worth `base_value` and holds `quote`.
        let pnl = if self.is_bid { base_value.checked_sub(quote)? } else { quote.checked_sub(base_value)? };

        // Notional is the base leg at the fill-time reference, so a markout is a fraction of what
        // was actually traded rather than of whichever leg happened to be larger.
        let notional = base.checked_mul(i128::try_from(self.ref_at_fill).ok()?)?.checked_div(scale)?;
        if notional == 0 {
            return None;
        }
        pnl.checked_mul(1_000_000)?.checked_div(notional)
    }
}

/// A counterparty's running record.
///
/// Deliberately sums rather than averages. An average markout weights a \$10 fill and a \$2M fill
/// equally, and the whole finding of the flow simulator was that one large informed fill outweighs
/// a day of small uninformed ones. Notional-weighted totals are what a spread decision should be
/// made against.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Score {
    /// How many settled fills this counterparty has.
    pub fills: u64,
    /// Sum of base legs valued at the fill-time reference, in quote units.
    pub notional: u128,
    /// Notional-weighted markout, in quote units, one per horizon.
    pub pnl: [i128; HORIZONS_SECS.len()],
    /// Block timestamp of the first settled fill, for judging whether a score is seasoned.
    pub first_seen_secs: u64,
    /// Block timestamp of the most recent settled fill.
    pub last_seen_secs: u64,
}

impl Score {
    /// Notional-weighted markout at a horizon, in hundredths of a bp. `None` until this
    /// counterparty has traded something.
    pub fn markout_e2(&self, horizon_index: usize) -> Option<i128> {
        if self.notional == 0 {
            return None;
        }
        let notional = i128::try_from(self.notional).ok()?;
        self.pnl.get(horizon_index)?.checked_mul(1_000_000)?.checked_div(notional)
    }

    /// True when there is enough here to act on.
    ///
    /// A single fill is not evidence: markout on one trade is mostly the market's next move, not
    /// the counterparty's information. Requiring both a fill count and a notional floor stops a
    /// counterparty earning a punitive spread by being unlucky once, and stops one earning a
    /// tight spread with a handful of dust trades.
    pub fn is_actionable(&self, min_fills: u64, min_notional: u128) -> bool {
        self.fills >= min_fills && self.notional >= min_notional
    }
}

/// Everything the pool has learned about who trades against it.
#[derive(Debug, Default)]
pub struct Markout {
    /// Fills whose horizons have not all matured.
    pending: VecDeque<Fill>,
    /// Reference observations, oldest first, for marking those fills.
    refs: VecDeque<(u64, u16, u128)>,
    scores: HashMap<Address, Score>,
    per_pair: HashMap<u16, Score>,
    /// Fills dropped because no reference survived to mark them against. A non-zero value here
    /// means the loop stalled or the retention window is too short, and it is reported rather
    /// than silently absorbed.
    /// See the field's own note below: dropped rather than guessed, and surfaced.
    pub unmarked: u64,
}

impl Markout {
    /// An empty record. Scores are built from observation only; there is no prior.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the reference used for a pair on this cycle. Called every cycle, fill or no fill,
    /// because a fill is marked against references that arrive *after* it.
    pub fn observe_reference(&mut self, pair_id: u16, at_secs: u64, reference: u128) {
        self.refs.push_back((at_secs, pair_id, reference));
        while let Some(&(t, _, _)) = self.refs.front() {
            if at_secs.saturating_sub(t) > REF_RETENTION_SECS {
                self.refs.pop_front();
            } else {
                break;
            }
        }
    }

    /// Queue a fill to be marked once its horizons mature.
    pub fn observe_fill(&mut self, fill: Fill) {
        self.pending.push_back(fill);
    }

    /// Mark every fill whose longest horizon has matured, and fold it into the scores.
    ///
    /// Returns the fills it settled, so the caller can log them individually — the per-fill record
    /// is what a later analysis joins on, and an aggregate alone cannot be re-cut.
    pub fn settle(&mut self, now_secs: u64) -> Vec<(Fill, [Option<i128>; HORIZONS_SECS.len()])> {
        let mut settled = Vec::new();
        let longest = HORIZONS_SECS[HORIZONS_SECS.len() - 1];

        while let Some(front) = self.pending.front() {
            if now_secs.saturating_sub(front.at_secs) < longest {
                break;
            }
            let fill = self.pending.pop_front().expect("front checked above");

            let mut marks = [None; HORIZONS_SECS.len()];
            let mut any = false;
            for (i, h) in HORIZONS_SECS.iter().enumerate() {
                let Some(r) = self.reference_at(fill.pair_id, fill.at_secs + h) else {
                    continue;
                };
                marks[i] = fill.markout_e2(r);
                any |= marks[i].is_some();
            }

            if !any {
                self.unmarked += 1;
                continue;
            }
            self.fold(&fill, &marks);
            settled.push((fill, marks));
        }
        settled
    }

    /// The reference nearest to `at`, if one is retained within a few seconds of it.
    ///
    /// Nearest rather than interpolated: the reference is a jump process and interpolating across
    /// a jump invents a price that never held. A tolerance rather than an exact match because the
    /// cycle is event-driven and does not land on whole seconds.
    fn reference_at(&self, pair_id: u16, at: u64) -> Option<u128> {
        const TOLERANCE_SECS: u64 = 3;
        let mut best: Option<(u64, u128)> = None;
        for &(t, p, r) in &self.refs {
            if p != pair_id {
                continue;
            }
            let d = t.abs_diff(at);
            if d > TOLERANCE_SECS {
                continue;
            }
            // `is_none_or` reads better but is stable only from 1.82, and this crate's MSRV is
            // 1.75.
            if best.map_or(true, |(bd, _)| d < bd) {
                best = Some((d, r));
            }
        }
        best.map(|(_, r)| r)
    }

    fn fold(&mut self, fill: &Fill, marks: &[Option<i128>; HORIZONS_SECS.len()]) {
        let scale = 10i128.saturating_pow(u32::from(fill.price_scale_exp));
        let base = i128::try_from(if fill.is_bid { fill.amount_in } else { fill.amount_out }).unwrap_or(0);
        let notional = base
            .saturating_mul(i128::try_from(fill.ref_at_fill).unwrap_or(0))
            .checked_div(scale)
            .unwrap_or(0);
        let notional_u = u128::try_from(notional).unwrap_or(0);

        for score in [
            self.scores.entry(fill.receiver).or_default(),
            self.per_pair.entry(fill.pair_id).or_default(),
        ] {
            score.fills += 1;
            score.notional = score.notional.saturating_add(notional_u);
            if score.first_seen_secs == 0 {
                score.first_seen_secs = fill.at_secs;
            }
            score.last_seen_secs = fill.at_secs;
            for (i, m) in marks.iter().enumerate() {
                if let Some(bps_e2) = m {
                    // Weight by notional so the sum is in quote units and a large fill counts
                    // for what it actually was.
                    let contribution = bps_e2.saturating_mul(notional) / 1_000_000;
                    score.pnl[i] = score.pnl[i].saturating_add(contribution);
                }
            }
        }
    }

    /// This counterparty's record, if it has ever settled a fill.
    #[must_use]
    pub fn score_of(&self, who: &Address) -> Option<&Score> {
        self.scores.get(who)
    }

    /// The whole market's record, which is the denominator a per-counterparty score is read against.
    #[must_use]
    pub fn pair_score(&self, pair_id: u16) -> Option<&Score> {
        self.per_pair.get(&pair_id)
    }

    /// Fills observed but not yet mature. A number that only grows means settlement has stalled.
    #[must_use]
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Counterparties ordered worst-first at the given horizon, for reporting.
    #[must_use]
    pub fn worst(&self, horizon_index: usize, limit: usize) -> Vec<(Address, &Score)> {
        let mut v: Vec<_> = self.scores.iter().map(|(a, s)| (*a, s)).collect();
        v.sort_by_key(|(_, s)| s.markout_e2(horizon_index).unwrap_or(0));
        v.truncate(limit);
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXP: u8 = 24;
    const MID: u128 = 2_000_000_000_000_000; // $2,000 at exp 24 for an 18/6 pair

    fn who(b: u8) -> Address {
        Address::repeat_byte(b)
    }

    /// A bid of 1 base at the reference, i.e. the pool paid exactly fair value.
    fn flat_bid(at: u64) -> Fill {
        Fill {
            pair_id: 1,
            receiver: who(1),
            partner_id: 0,
            is_bid: true,
            amount_in: 1_000_000_000_000_000_000, // 1 base, 18dp
            amount_out: 2_000_000_000,            // 2,000 quote, 6dp
            at_secs: at,
            ref_at_fill: MID,
            price_scale_exp: EXP,
        }
    }

    #[test]
    fn a_fill_at_the_reference_that_does_not_move_marks_out_flat() {
        let f = flat_bid(100);
        assert_eq!(f.markout_e2(MID), Some(0));
    }

    #[test]
    fn buying_before_a_rise_marks_out_positive_for_the_pool() {
        // The pool bought base at 2,000 and the market went to 2,020: +100 bp.
        let f = flat_bid(100);
        let m = f.markout_e2(MID * 101 / 100).expect("marked");
        assert_eq!(m, 10_000, "a 1% rise on a bid is +100 bp = 10,000 e2");
    }

    #[test]
    fn buying_before_a_fall_marks_out_negative() {
        // This is the picked-off case, and the number must be allowed to be negative.
        let f = flat_bid(100);
        let m = f.markout_e2(MID * 99 / 100).expect("marked");
        assert_eq!(m, -10_000);
    }

    #[test]
    fn the_ask_side_has_the_opposite_sign() {
        let mut f = flat_bid(100);
        f.is_bid = false;
        f.amount_in = 2_000_000_000; // quote in
        f.amount_out = 1_000_000_000_000_000_000; // base out
        // The pool sold base and the market rose: it sold too cheaply.
        assert_eq!(f.markout_e2(MID * 101 / 100), Some(-10_000));
    }

    #[test]
    fn a_fill_is_settled_only_once_its_longest_horizon_has_matured() {
        let mut m = Markout::new();
        m.observe_reference(1, 100, MID);
        m.observe_fill(flat_bid(100));

        assert!(m.settle(159).is_empty(), "59s is inside the 60s horizon");
        assert_eq!(m.pending_len(), 1);

        m.observe_reference(1, 101, MID);
        m.observe_reference(1, 110, MID);
        m.observe_reference(1, 160, MID);
        assert_eq!(m.settle(160).len(), 1);
        assert_eq!(m.pending_len(), 0);
    }

    #[test]
    fn a_fill_with_no_surviving_reference_is_counted_rather_than_guessed() {
        // The alternative — marking against the nearest thing available at any distance — produces
        // a number that looks like data and is not.
        let mut m = Markout::new();
        m.observe_reference(1, 100, MID);
        m.observe_fill(flat_bid(100));
        // Only a reference 30s away from every horizon survives.
        m.observe_reference(1, 300, MID);
        assert!(m.settle(400).is_empty());
        assert_eq!(m.unmarked, 1);
    }

    #[test]
    fn scores_are_notional_weighted_not_averaged() {
        let mut m = Markout::new();
        for t in [100u64, 200] {
            m.observe_reference(1, t, MID);
            for h in HORIZONS_SECS {
                m.observe_reference(1, t + h, if t == 100 { MID * 101 / 100 } else { MID * 99 / 100 });
            }
        }
        // A small winner and a hundred-times-larger loser.
        m.observe_fill(flat_bid(100));
        let mut big = flat_bid(200);
        big.amount_in *= 100;
        big.amount_out *= 100;
        m.observe_fill(big);
        m.settle(400);

        let s = m.score_of(&who(1)).expect("scored");
        assert_eq!(s.fills, 2);
        let mo = s.markout_e2(2).expect("weighted");
        assert!(mo < -9_000, "the large loser must dominate, got {mo}");
    }

    #[test]
    fn one_fill_is_not_enough_to_act_on() {
        let mut m = Markout::new();
        m.observe_reference(1, 100, MID);
        for h in HORIZONS_SECS {
            m.observe_reference(1, 100 + h, MID * 99 / 100);
        }
        m.observe_fill(flat_bid(100));
        m.settle(200);

        let s = m.score_of(&who(1)).expect("scored");
        assert!(!s.is_actionable(5, 0), "a single fill must not earn a punitive spread");
        assert!(s.is_actionable(1, 0));
    }
}
