//! Two latching killswitches, and the NAV decomposition they are measured on.
//!
//! ```text
//! NAV          = quoteBalance + SUM_i value(baseBalance_i, fair_i)
//! revaluation  = SUM_i [ value(basePrev_i, fairNow_i) - value(basePrev_i, fairPrev_i) ]
//! tradePnl     = (NAV_now - NAV_prev) - revaluation
//! ```
//!
//! Inventory only moves on a trade, so the two separate exactly with no event decoding: with no
//! trades `tradePnl` is exactly zero rather than approximately, because the balances are identical
//! and the sums cancel term for term over the same integer valuation. That is what lets a
//! cumulative budget run for days without drifting into a trip, and why there is no noise-floor
//! knob. `value` is [`amount_out_bid`] against a flat ladder at the fair.
//!
//! The loss budget is gross per archi_v2 §5.4: a later recovery does not hand it back, because a
//! book that loses 1000 and makes 1000 has been picked off twice. Both switches latch, written
//! atomically — temp file, then rename — after every change and read at startup, so a halted book
//! stays down across the restart an operator reaches for first.
//!
//! What deliberately does **not** survive a restart is the previous-observation state, `prev` and
//! `window`: trades may have happened while the process was down, so attributing the balance change
//! across that gap would be an invention. The first observation after a restart re-seeds and
//! attributes nothing; the cumulative totals and the latch do persist.
//!
//! Known gap: a manager deposit or an owner withdrawal moves balances with no trade, so it lands in
//! `tradePnl` and reads as a loss. Attributing it needs the pool's `Swap` and `ReserveSynced`
//! events; not built, see the README.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use dubu_core::curve::{amount_out_bid, AMOUNT_MAX};
use serde::{Deserialize, Serialize};

/// Why the state file could not be used.
#[derive(Debug, thiserror::Error)]
pub enum RiskError {
    /// The state file could not be read or written.
    #[error("killswitch state at `{path}`: {source}")]
    Io {
        /// Path involved.
        path: PathBuf,
        /// Underlying failure.
        source: std::io::Error,
    },
    /// The state file exists and is not parseable. Fatal rather than "start fresh": an unreadable
    /// latch is indistinguishable from a set one, so starting fresh resumes a halted book.
    #[error(
        "killswitch state at `{path}` is unreadable ({source}); refusing to start, because \
             a corrupt latch cannot be distinguished from a set one"
    )]
    Corrupt {
        /// Path involved.
        path: PathBuf,
        /// Parse failure.
        source: serde_json::Error,
    },
    /// A balance could not be valued.
    #[error("cannot mark inventory: {0}")]
    Mark(dubu_core::CurveError),
}

/// One pair's inventory position at a point in time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    /// Pair id, used only to line positions up between observations.
    pub pair_id: u16,
    /// Base tokens the pool holds that back this pair.
    pub base_balance: u128,
    /// Fair value in the pair's pool price scale.
    pub fair: u128,
    /// The pair's decimal alignment.
    pub price_scale_exp: u8,
}

/// Value a base balance in quote units, at a flat ladder on the fair value. Public because
/// [`crate::skew`] must measure imbalance with the *same* valuation: two that disagree by a
/// rounding step have the killswitch and the skew arguing about how much the pool holds.
///
/// # Errors
/// [`dubu_core::CurveError`] only from the shared domain.
pub fn value(
    base_balance: u128,
    fair: u128,
    price_scale_exp: u8,
) -> Result<u128, dubu_core::CurveError> {
    if base_balance == 0 || fair == 0 {
        return Ok(0);
    }
    // Both zero cases returned above, so what follows is a real balance at a real price.
    assert!(base_balance > 0);
    assert!(fair > 0);
    amount_out_bid(
        base_balance.min(AMOUNT_MAX),
        fair,
        fair,
        AMOUNT_MAX,
        0,
        price_scale_exp,
    )
}

/// What every liveness halt's persisted reason string begins with. Written by [`Halt`]'s `Display`
/// and read back by [`KillSwitch::is_liveness_only`]; one literal is what makes matching on a
/// prefix survivable.
const LIVENESS_PREFIX: &str = "liveness: ";

/// A killswitch trip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Halt {
    /// Short-window NAV drawdown.
    Bleed {
        /// Peak-to-current drawdown inside the window, quote units.
        drawdown: u128,
        /// The limit it crossed.
        limit: u128,
        /// Window length.
        window_secs: u64,
    },
    /// Cumulative gross trade loss.
    LossBudget {
        /// Running total, quote units.
        cumulative: u128,
        /// The budget it crossed.
        budget: u128,
    },
    /// Something outside the two measured switches — a chain outage, an operator signal.
    Liveness {
        /// What happened.
        reason: String,
    },
}

impl Halt {
    /// Short stable string for structured logs.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        let label = match self {
            Self::Bleed { .. } => "bleed",
            Self::LossBudget { .. } => "loss_budget",
            Self::Liveness { .. } => "liveness",
        };
        // Structured logs key on this; an empty label loses which switch fired.
        assert!(!label.is_empty());
        label
    }
}

impl std::fmt::Display for Halt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bleed {
                drawdown,
                limit,
                window_secs,
            } => {
                write!(
                    f,
                    "bleed: NAV fell {drawdown} against a limit of {limit} within {window_secs}s"
                )
            }
            Self::LossBudget { cumulative, budget } => {
                write!(
                    f,
                    "loss budget: cumulative gross trade loss {cumulative} exceeds {budget}"
                )
            }
            Self::Liveness { reason } => write!(f, "{LIVENESS_PREFIX}{reason}"),
        }
    }
}

/// Serialise a `u128` as a decimal string: JSON numbers are `f64` to most readers, including `jq`,
/// so a total past 2^53 would read back approximate.
mod u128_string {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &u128, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&v.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u128, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct NavPoint {
    at: u64,
    #[serde(with = "u128_string")]
    nav: u128,
}

/// The part of the killswitch state that outlives the process.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RiskState {
    /// Schema version, so a future change is a migration and not a misread.
    pub version: u32,
    /// The latch. While set, the bot withdraws quotes and exits.
    pub halted: bool,
    /// Human-readable reason the latch was set.
    pub halt_reason: Option<String>,
    /// Unix seconds the latch was set.
    pub halted_at: Option<u64>,
    /// Cumulative gross trade loss, quote units.
    #[serde(with = "u128_string")]
    pub cumulative_trade_loss: u128,
    /// Cumulative gross trade gain, for context in the log. Not used by any switch.
    #[serde(with = "u128_string")]
    pub cumulative_trade_gain: u128,
    /// How many observations have been attributed.
    pub observations: u64,
}

impl RiskState {
    /// A clean state.
    #[must_use]
    pub const fn fresh() -> Self {
        let state = Self {
            version: 1,
            halted: false,
            halt_reason: None,
            halted_at: None,
            cumulative_trade_loss: 0,
            cumulative_trade_gain: 0,
            observations: 0,
        };
        // A missing state file starts here: halted would keep a healthy book down, and a non-zero
        // total would start it part-way through its budget.
        assert!(!state.halted);
        assert!(state.cumulative_trade_loss == 0);
        assert!(state.observations == 0);
        state
    }
}

/// The killswitch itself: persistent latch plus the in-memory window and previous mark.
#[derive(Debug)]
pub struct KillSwitch {
    path: PathBuf,
    state: RiskState,
    bleed_window_secs: u64,
    bleed_limit: u128,
    loss_budget: u128,
    /// Measure but do not latch; see [`Observation::would_halt`]. Never persisted: a mode that
    /// survived a restart in the state file would be a disabled killswitch that nobody remembers
    /// disabling.
    shadow: bool,
    /// In-memory only; see the module docs on what must not survive a restart.
    prev: Option<(u128, BTreeMap<u16, Position>)>,
    window: Vec<NavPoint>,
}

/// What one observation concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    /// Total NAV in quote units.
    pub nav: u128,
    /// Value change attributable to price moves on the inventory held at the previous
    /// observation. Signed.
    pub revaluation: i128,
    /// Value change attributable to fills. Signed. Exactly zero when no balance moved.
    pub trade_pnl: i128,
    /// Peak-to-current NAV drawdown inside the bleed window.
    pub drawdown: u128,
    /// Running gross trade loss.
    pub cumulative_trade_loss: u128,
    /// Set when this observation only seeded the baseline and attributed nothing.
    pub seeded: bool,
    /// The verdict this observation reached, **whether or not it was enforced**. In shadow mode
    /// [`KillSwitch::observe`] returns `None` and the verdict appears only here, so acting on this
    /// field rather than on the return value re-enables enforcement.
    pub would_halt: Option<Halt>,
}

impl KillSwitch {
    /// Load the latch, or start a clean one if the file does not exist.
    ///
    /// # Errors
    /// [`RiskError::Io`] if the file exists and cannot be read, [`RiskError::Corrupt`] if it does
    /// not parse. Neither is recoverable by starting fresh.
    pub fn load(
        path: &Path,
        bleed_window_secs: u64,
        bleed_limit: u128,
        loss_budget: u128,
        shadow: bool,
    ) -> Result<Self, RiskError> {
        // Paired with the config validator: a zero window has no peak to draw down from, and a zero
        // limit or budget trips on the first observation.
        assert!(bleed_window_secs > 0);
        assert!(bleed_limit > 0);
        assert!(loss_budget > 0);
        let state = match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).map_err(|source| RiskError::Corrupt {
                path: path.to_path_buf(),
                source,
            })?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => RiskState::fresh(),
            Err(source) => {
                return Err(RiskError::Io {
                    path: path.to_path_buf(),
                    source,
                })
            }
        };
        let switch = Self {
            path: path.to_path_buf(),
            state,
            bleed_window_secs,
            bleed_limit,
            loss_budget,
            shadow,
            prev: None,
            window: Vec::new(),
        };
        // What must NOT survive a restart, asserted where it is constructed; see the module docs.
        assert!(switch.prev.is_none());
        assert!(switch.window.is_empty());
        Ok(switch)
    }

    /// Whether the latch is set.
    #[must_use]
    pub const fn is_halted(&self) -> bool {
        self.state.halted
    }

    /// Why, if it is.
    #[must_use]
    pub fn halt_reason(&self) -> Option<&str> {
        self.state.halt_reason.as_deref()
    }

    /// Whether the latch is set and was set by a liveness halt, with nothing else behind it. Chain
    /// liveness is the one cause that heals on its own; `Bleed` and `LossBudget` want a human's
    /// eyes first. The persisted reason string is the discriminator rather than a `halt_kind` field
    /// because [`RiskState`] is versioned and `deny_unknown_fields`: a new field makes a rollback
    /// fail to parse the latch and then, per [`RiskError::Corrupt`], refuse to start at all.
    #[must_use]
    pub fn is_liveness_only(&self) -> bool {
        if self.state.halted {
            return self
                .state
                .halt_reason
                .as_deref()
                .is_some_and(|r| r.starts_with(LIVENESS_PREFIX));
        }
        false
    }

    /// Clear the latch, keeping everything the switches have measured. Not a general "clear the
    /// killswitch": the caller must have established that the halt is liveness-only and that the
    /// condition behind it is gone, and there is no path here for a bleed or an exhausted budget.
    /// The totals survive because the loss budget is all-time and gross — resuming with fresh ones
    /// makes waiting for an outage a way to launder one.
    ///
    /// # Errors
    /// [`RiskError::Io`] if the cleared state cannot be written. Loud, because a resume that did
    /// not reach disk is one the next restart silently undoes.
    pub fn resume(&mut self) -> Result<(), RiskError> {
        let spent = (
            self.state.cumulative_trade_loss,
            self.state.cumulative_trade_gain,
            self.state.observations,
        );
        self.state.halted = false;
        self.state.halt_reason = None;
        self.state.halted_at = None;
        // All three or none: a cleared latch still carrying a reason reads as a halt in force.
        assert!(!self.state.halted);
        assert!(self.state.halt_reason.is_none());
        assert!(self.state.halted_at.is_none());
        // Zeroing the totals along with the latch is the one thing a resume must never do.
        assert!(spent.0 == self.state.cumulative_trade_loss);
        assert!(spent.1 == self.state.cumulative_trade_gain);
        assert!(spent.2 == self.state.observations);
        self.persist()
    }

    /// The persistent state, for logging.
    #[must_use]
    pub const fn state(&self) -> &RiskState {
        &self.state
    }

    /// Set the latch for a reason the measured switches do not cover — a chain outage, an operator
    /// instruction.
    ///
    /// # Errors
    /// [`RiskError::Io`] if the latch cannot be written. Loud, because an unwritten latch will not
    /// survive the restart.
    pub fn halt(&mut self, halt: &Halt, now: u64) -> Result<(), RiskError> {
        if self.state.halted {
            return Ok(());
        }
        self.state.halted = true;
        self.state.halt_reason = Some(halt.to_string());
        self.state.halted_at = Some(now);
        // All three or none: a latch whose reason did not survive the restart is one an operator
        // clears blind.
        assert!(self.state.halted);
        assert!(self.state.halt_reason.is_some());
        assert!(self.state.halted_at == Some(now));
        self.persist()
    }

    /// Feed one mark through both switches, returning the trip if this observation caused one. A
    /// no-op once halted: a latched switch stops deciding and its numbers must stop moving.
    ///
    /// # Errors
    /// [`RiskError::Mark`] if a balance cannot be valued, [`RiskError::Io`] if a trip cannot be
    /// persisted.
    pub fn observe(
        &mut self,
        quote_balance: u128,
        positions: &[Position],
        now: u64,
    ) -> Result<(Observation, Option<Halt>), RiskError> {
        // The caller skips empty groups: a drawdown against an empty book is not a drawdown.
        assert!(!positions.is_empty());
        let by_pair: BTreeMap<u16, Position> = positions.iter().map(|p| (p.pair_id, *p)).collect();
        // A repeated pair id would silently drop a position from every sum below.
        assert_eq!(by_pair.len(), positions.len());

        let nav = nav_of(quote_balance, &by_pair)?;
        let drawdown = self.record_drawdown(now, nav);

        // Revaluation is computed on the PREVIOUS balances, so the residual is what the fills did.
        let (revaluation, trade_pnl, seeded) = match &self.prev {
            None => (0i128, 0i128, true),
            Some((prev_quote, prev_positions)) => {
                let reval = revaluation_of(prev_positions, &by_pair)?;
                let prev_nav = nav_of(*prev_quote, prev_positions)?;
                let delta = i128::try_from(nav).unwrap_or(i128::MAX)
                    - i128::try_from(prev_nav).unwrap_or(i128::MAX);
                (reval, delta - reval, false)
            }
        };
        // A seed attributes nothing; inventing an attribution across the gap poisons the budget.
        if seeded {
            assert!(revaluation == 0);
            assert!(trade_pnl == 0);
        }

        if !seeded && !self.state.halted {
            self.accumulate(trade_pnl);
        }

        self.prev = Some((quote_balance, by_pair));
        assert!(
            self.prev.is_some(),
            "the next observation has a baseline to attribute against"
        );

        let mut obs = Observation {
            nav,
            revaluation,
            trade_pnl,
            drawdown,
            cumulative_trade_loss: self.state.cumulative_trade_loss,
            seeded,
            would_halt: None,
        };

        if self.state.halted {
            return Ok((obs, None));
        }

        let verdict = self.verdict(drawdown);
        obs.would_halt = verdict.clone();

        if self.shadow {
            // Everything but the latch still happens: persisting the totals is what leaves a shadow
            // run with data to size a limit from.
            self.persist()?;
            assert!(
                !self.state.halted,
                "shadow mode latched the book; the mode has no other job than not doing this"
            );
            return Ok((obs, None));
        }

        if let Some(h) = &verdict {
            self.halt(h, now)?;
            assert!(self.state.halted, "a trip must leave the latch set");
        } else {
            // Without this a restart hands back whatever budget was consumed since the last trip.
            self.persist()?;
        }
        Ok((obs, verdict))
    }

    fn record_drawdown(&mut self, now: u64, nav: u128) -> u128 {
        self.window.push(NavPoint { at: now, nav });
        let cutoff = now.saturating_sub(self.bleed_window_secs);
        self.window.retain(|p| p.at >= cutoff);
        // The point just pushed is stamped `now` and the cutoff is at or before it, so it survives
        // its own retain — which is what makes the peak at least this NAV.
        assert!(!self.window.is_empty());
        let peak = self.window.iter().map(|p| p.nav).max().unwrap_or(nav);
        assert!(peak >= nav);
        peak.saturating_sub(nav)
    }

    /// Fold one attributed step into the running totals, gross: only a loss consumes the budget and
    /// a later gain does not hand it back.
    fn accumulate(&mut self, trade_pnl: i128) {
        let before = self.state.cumulative_trade_loss;
        if trade_pnl < 0 {
            self.state.cumulative_trade_loss = self
                .state
                .cumulative_trade_loss
                .saturating_add(trade_pnl.unsigned_abs());
        } else {
            self.state.cumulative_trade_gain = self
                .state
                .cumulative_trade_gain
                .saturating_add(trade_pnl.unsigned_abs());
        }
        self.state.observations += 1;
        assert!(self.state.cumulative_trade_loss >= before);
        if trade_pnl >= 0 {
            assert!(self.state.cumulative_trade_loss == before);
        }
    }

    /// Which switch, if either, this observation tripped. Bleed first: faster-moving, more urgent.
    fn verdict(&self, drawdown: u128) -> Option<Halt> {
        if drawdown >= self.bleed_limit {
            return Some(Halt::Bleed {
                drawdown,
                limit: self.bleed_limit,
                window_secs: self.bleed_window_secs,
            });
        }
        if self.state.cumulative_trade_loss >= self.loss_budget {
            return Some(Halt::LossBudget {
                cumulative: self.state.cumulative_trade_loss,
                budget: self.loss_budget,
            });
        }
        None
    }

    /// Write the state atomically: temp file in the same directory, then rename. Same directory
    /// because `rename` is only atomic within one filesystem, and a half-written latch cannot be
    /// distinguished from a corrupt one, which refuses to start.
    fn persist(&self) -> Result<(), RiskError> {
        let io = |source| RiskError::Io {
            path: self.path.clone(),
            source,
        };
        if let Some(dir) = self.path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir).map_err(io)?;
            }
        }
        let text = serde_json::to_string_pretty(&self.state).map_err(|e| RiskError::Io {
            path: self.path.clone(),
            source: std::io::Error::other(e),
        })?;
        assert!(!text.is_empty());
        let tmp = self.path.with_extension("json.tmp");
        // A scratch path equal to the latch would write over it and rename it to itself.
        assert!(tmp != self.path);
        std::fs::write(&tmp, text).map_err(io)?;
        std::fs::rename(&tmp, &self.path).map_err(io)?;
        Ok(())
    }
}

/// NAV: the quote balance plus every base balance marked at its own fair value.
///
/// # Errors
/// [`RiskError::Mark`] on a balance that cannot be valued.
fn nav_of(quote_balance: u128, positions: &BTreeMap<u16, Position>) -> Result<u128, RiskError> {
    let mut nav = quote_balance;
    for p in positions.values() {
        nav = nav.saturating_add(
            value(p.base_balance, p.fair, p.price_scale_exp).map_err(RiskError::Mark)?,
        );
    }
    // Inventory is only ever worth a non-negative amount, so marking it cannot shrink the NAV.
    assert!(nav >= quote_balance);
    Ok(nav)
}

/// Value change attributable to price moves on the balances held at the PREVIOUS observation.
/// Marking the old balances at both fair values is what leaves the residual as the fills' doing;
/// marking the *current* balances folds the fills into revaluation and leaves nothing to attribute.
///
/// # Errors
/// [`RiskError::Mark`] on a balance that cannot be valued.
fn revaluation_of(
    prev_positions: &BTreeMap<u16, Position>,
    now_positions: &BTreeMap<u16, Position>,
) -> Result<i128, RiskError> {
    let mut reval = 0i128;
    for (id, prev) in prev_positions {
        // A pair that vanished from the config between observations cannot be revalued: its old
        // fair is treated as still current, which contributes zero.
        let fair_now = now_positions.get(id).map_or(prev.fair, |p| p.fair);
        let then =
            value(prev.base_balance, prev.fair, prev.price_scale_exp).map_err(RiskError::Mark)?;
        let now_v =
            value(prev.base_balance, fair_now, prev.price_scale_exp).map_err(RiskError::Mark)?;
        // The same balance at the same fair must cancel term for term, or a quiet market feeds
        // `trade_pnl` and the cumulative budget drifts into a trip on its own.
        if fair_now == prev.fair {
            assert_eq!(now_v, then);
        }
        reval +=
            i128::try_from(now_v).unwrap_or(i128::MAX) - i128::try_from(then).unwrap_or(i128::MAX);
    }
    Ok(reval)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch path unique to each test, outside the repo.
    fn scratch(name: &str) -> PathBuf {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir()
            .join(format!("dubu-risk-{name}-{n}"))
            .join("killswitch.json");
        let _ = std::fs::remove_file(&p);
        p
    }

    /// pairId 1's shape: 18-decimal base, 6-decimal quote, priceScaleExp 24.
    fn pos(base: u128, fair: u128) -> Position {
        Position {
            pair_id: 1,
            base_balance: base,
            fair,
            price_scale_exp: 24,
        }
    }

    const ETH: u128 = 1_000_000_000_000_000_000;
    const FAIR: u128 = 1_943_820_000_000_000; // 1943.82
    /// Limits in mUSDC units (6 decimals): 2000 and 10000.
    const BLEED: u128 = 2_000_000_000;
    const BUDGET: u128 = 10_000_000_000;

    fn ks(name: &str) -> KillSwitch {
        KillSwitch::load(&scratch(name), 300, BLEED, BUDGET, false).unwrap()
    }

    /// A switch with the bleed limit disabled. The two overlap — a single-step loss large enough to
    /// exhaust the budget also breaches a comparable bleed limit — so isolating one means turning
    /// the other off.
    fn ks_budget_only(name: &str) -> KillSwitch {
        KillSwitch::load(&scratch(name), 300, u128::MAX, BUDGET, false).unwrap()
    }

    #[test]
    fn the_mark_agrees_with_the_chains_own_quote_path() {
        // 10 mWETH at 1943.82 is 19_438.20 mUSDC, at 6 decimals.
        assert_eq!(value(10 * ETH, FAIR, 24).unwrap(), 19_438_200_000);
        assert_eq!(value(0, FAIR, 24).unwrap(), 0);
        assert_eq!(value(10 * ETH, 0, 24).unwrap(), 0);
    }

    #[test]
    fn a_quiet_book_attributes_exactly_zero_trade_pnl() {
        // The market moves 20% either way with no fill.
        let mut k = ks_budget_only("quiet");
        let quote = 1_000_000_000_000u128;
        k.observe(quote, &[pos(4_445 * ETH, FAIR)], 1_000).unwrap();

        for (i, fair) in [FAIR * 12 / 10, FAIR * 8 / 10, FAIR, FAIR * 11 / 10]
            .into_iter()
            .enumerate()
        {
            let (obs, halt) = k
                .observe(quote, &[pos(4_445 * ETH, fair)], 1_001 + i as u64)
                .unwrap();
            assert_eq!(
                obs.trade_pnl, 0,
                "a market move with no fill must attribute no trade PnL"
            );
            assert_ne!(
                obs.revaluation, 0,
                "... but it must still show up as revaluation"
            );
            assert_eq!(halt, None);
        }
        assert_eq!(k.state().cumulative_trade_loss, 0);
        assert_eq!(k.state().cumulative_trade_gain, 0);
    }

    #[test]
    fn a_profitable_fill_is_attributed_to_trade_pnl_not_revaluation() {
        // The pool buys 1 mWETH for 1900 mUSDC while fair is 1943.82: a ~43.82 gain.
        let mut k = ks("fill");
        k.observe(10_000_000_000, &[pos(0, FAIR)], 1_000).unwrap();
        let (obs, halt) = k
            .observe(10_000_000_000 - 1_900_000_000, &[pos(ETH, FAIR)], 1_001)
            .unwrap();
        assert_eq!(obs.revaluation, 0, "the price did not move");
        assert_eq!(obs.trade_pnl, 43_820_000, "43.82 mUSDC of edge");
        assert_eq!(halt, None);
        assert_eq!(k.state().cumulative_trade_loss, 0);
        assert_eq!(k.state().cumulative_trade_gain, 43_820_000);
    }

    #[test]
    fn a_losing_fill_accumulates_gross_and_a_later_gain_does_not_refund_it() {
        let mut k = ks("gross");
        // Buy 1 mWETH for 2000 while fair is 1943.82: a 56.18 loss.
        k.observe(10_000_000_000, &[pos(0, FAIR)], 1_000).unwrap();
        let (obs, _) = k
            .observe(10_000_000_000 - 2_000_000_000, &[pos(ETH, FAIR)], 1_001)
            .unwrap();
        assert_eq!(obs.trade_pnl, -56_180_000);
        assert_eq!(k.state().cumulative_trade_loss, 56_180_000);

        // Sell it back at 2000: a 56.18 gain, and the budget must not be handed back.
        let (obs, _) = k.observe(10_000_000_000, &[pos(0, FAIR)], 1_002).unwrap();
        assert_eq!(obs.trade_pnl, 56_180_000);
        assert_eq!(
            k.state().cumulative_trade_loss,
            56_180_000,
            "gross means gross"
        );
        assert_eq!(k.state().cumulative_trade_gain, 56_180_000);
    }

    #[test]
    fn the_loss_budget_catches_what_the_bleed_switch_is_too_short_to_see() {
        // Ten losses of 1000 mUSDC, each inside the 2000 bleed limit and each spaced far enough
        // apart that the previous peak has aged out of the 300s window.
        let mut k = KillSwitch::load(&scratch("budget"), 300, BLEED, BUDGET, false).unwrap();
        let mut quote = 1_000_000_000_000u128;
        k.observe(quote, &[pos(0, FAIR)], 1_000).unwrap();

        let mut halted = None;
        for i in 0..12u64 {
            quote -= 1_000_000_000;
            let (obs, h) = k.observe(quote, &[pos(0, FAIR)], 1_400 + i * 400).unwrap();
            assert_eq!(
                obs.drawdown, 0,
                "each step alone must be invisible to the bleed switch"
            );
            if let Some(h) = h {
                halted = Some((i, h));
                break;
            }
        }
        let (i, h) = halted.expect("the budget must trip");
        assert_eq!(i, 9, "10 x 1000 reaches the 10000 budget");
        assert_eq!(
            h,
            Halt::LossBudget {
                cumulative: 10_000_000_000,
                budget: 10_000_000_000
            }
        );
        assert!(k.is_halted());
    }

    #[test]
    fn the_bleed_switch_trips_on_a_market_move_with_no_fills_at_all() {
        // An unhedged book takes a fast adverse move as a loss whether or not anyone traded.
        let mut k = ks("bleed");
        let quote = 0u128;
        // 10 mWETH: a 2000-unit drawdown needs the price to fall ~200 a coin.
        k.observe(quote, &[pos(10 * ETH, FAIR)], 1_000).unwrap();
        let (obs, halt) = k
            .observe(quote, &[pos(10 * ETH, FAIR - 200_000_000_000_000)], 1_100)
            .unwrap();
        assert_eq!(obs.trade_pnl, 0, "no fill happened");
        assert_eq!(obs.drawdown, 2_000_000_000);
        assert!(matches!(
            halt,
            Some(Halt::Bleed {
                drawdown: 2_000_000_000,
                ..
            })
        ));
        assert!(k.is_halted());
    }

    #[test]
    fn the_bleed_window_forgets_a_peak_that_has_aged_out() {
        // Otherwise the switch is an all-time-high drawdown limit and trips on any slow drift.
        let mut k = KillSwitch::load(&scratch("window"), 300, BLEED, BUDGET, false).unwrap();
        k.observe(0, &[pos(10 * ETH, FAIR)], 1_000).unwrap();

        // 400s later the old peak is outside the window, so the same price is not a drawdown.
        let low = FAIR - 200_000_000_000_000;
        let (obs, halt) = k.observe(0, &[pos(10 * ETH, low)], 1_400).unwrap();
        assert_eq!(obs.drawdown, 0);
        assert_eq!(halt, None);
        assert!(!k.is_halted());
    }

    #[test]
    fn the_latch_survives_a_restart_and_the_book_stays_down() {
        let path = scratch("restart");
        let mut k = KillSwitch::load(&path, 300, BLEED, BUDGET, false).unwrap();
        k.observe(0, &[pos(10 * ETH, FAIR)], 1_000).unwrap();
        let (_, halt) = k
            .observe(0, &[pos(10 * ETH, FAIR - 200_000_000_000_000)], 1_100)
            .unwrap();
        assert!(halt.is_some());
        drop(k);

        let restarted = KillSwitch::load(&path, 300, BLEED, BUDGET, false).unwrap();
        assert!(restarted.is_halted(), "a restart resumed a halted book");
        assert!(restarted.halt_reason().unwrap().contains("bleed"));
        assert!(restarted.state().halted_at.is_some());
    }

    #[test]
    fn the_cumulative_budget_survives_a_restart_too() {
        // A reset total makes the budget a per-process limit, which restarting after every trip
        // would never reach.
        let path = scratch("cumulative");
        let mut k = KillSwitch::load(&path, 300, u128::MAX, BUDGET, false).unwrap();
        k.observe(1_000_000_000_000, &[pos(0, FAIR)], 1_000)
            .unwrap();
        k.observe(1_000_000_000_000 - 3_000_000_000, &[pos(0, FAIR)], 1_001)
            .unwrap();
        assert_eq!(k.state().cumulative_trade_loss, 3_000_000_000);
        drop(k);

        let mut k = KillSwitch::load(&path, 300, u128::MAX, BUDGET, false).unwrap();
        assert_eq!(k.state().cumulative_trade_loss, 3_000_000_000);
        assert!(!k.is_halted());

        // And the first post-restart observation only re-seeds; see the module docs.
        let (obs, _) = k.observe(1, &[pos(0, FAIR)], 2_000).unwrap();
        assert!(obs.seeded);
        assert_eq!(obs.trade_pnl, 0);
        assert_eq!(
            k.state().cumulative_trade_loss,
            3_000_000_000,
            "the seed must attribute nothing"
        );
    }

    #[test]
    fn a_halted_switch_stops_moving_its_numbers() {
        let mut k = ks("frozen");
        k.observe(0, &[pos(10 * ETH, FAIR)], 1_000).unwrap();
        k.observe(0, &[pos(10 * ETH, FAIR - 200_000_000_000_000)], 1_100)
            .unwrap();
        assert!(k.is_halted());
        let before = k.state().cumulative_trade_loss;

        let (_, halt) = k.observe(0, &[pos(0, FAIR)], 1_200).unwrap();
        assert_eq!(halt, None, "a halted switch must not re-trip");
        assert_eq!(k.state().cumulative_trade_loss, before);
    }

    #[test]
    fn a_corrupt_latch_refuses_to_start_rather_than_starting_fresh() {
        let path = scratch("corrupt");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ this is not json").unwrap();
        let err = KillSwitch::load(&path, 300, BLEED, BUDGET, false).unwrap_err();
        assert!(matches!(err, RiskError::Corrupt { .. }), "got {err}");
    }

    #[test]
    fn a_missing_latch_file_is_a_clean_start() {
        let k = KillSwitch::load(&scratch("missing"), 300, BLEED, BUDGET, false).unwrap();
        assert!(!k.is_halted());
        assert_eq!(k.state().cumulative_trade_loss, 0);
    }

    #[test]
    fn a_liveness_halt_latches_like_the_measured_ones() {
        let path = scratch("liveness");
        let mut k = KillSwitch::load(&path, 300, BLEED, BUDGET, false).unwrap();
        k.halt(
            &Halt::Liveness {
                reason: "chain down for 600s".into(),
            },
            5_000,
        )
        .unwrap();
        assert!(k.is_halted());
        drop(k);
        let k = KillSwitch::load(&path, 300, BLEED, BUDGET, false).unwrap();
        assert!(k.is_halted());
        assert!(k.halt_reason().unwrap().contains("chain down"));
    }

    #[test]
    fn a_liveness_latch_is_still_recognisable_as_one_after_a_reload() {
        // The startup recovery runs against a file a previous process wrote, so there is no
        // in-memory `Halt` left to ask: the cause has to survive the JSON round trip.
        let path = scratch("liveness-only");
        let mut k = KillSwitch::load(&path, 300, BLEED, BUDGET, false).unwrap();
        k.halt(
            &Halt::Liveness {
                reason: "chain liveness lost for 615s (limit 600s)".into(),
            },
            5_000,
        )
        .unwrap();
        assert!(k.is_liveness_only());
        drop(k);

        let k = KillSwitch::load(&path, 300, BLEED, BUDGET, false).unwrap();
        assert!(k.is_halted());
        assert!(k.is_liveness_only());
    }

    #[test]
    fn neither_measured_switch_latches_as_liveness_only() {
        // In this direction too, or the recovery becomes a killswitch any restart clears.
        let mut k = ks("bleed-not-liveness");
        k.observe(0, &[pos(10 * ETH, FAIR)], 1_000).unwrap();
        k.observe(0, &[pos(10 * ETH, FAIR - 200_000_000_000_000)], 1_100)
            .unwrap();
        assert!(k.is_halted());
        assert!(!k.is_liveness_only());

        let mut k = ks_budget_only("budget-not-liveness");
        k.halt(
            &Halt::LossBudget {
                cumulative: BUDGET,
                budget: BUDGET,
            },
            5_000,
        )
        .unwrap();
        assert!(k.is_halted());
        assert!(!k.is_liveness_only());

        // Nor is an unlatched switch, or the recovery fires on a book that was never down.
        let k = ks("never-latched");
        assert!(!k.is_halted());
        assert!(!k.is_liveness_only());
    }

    #[test]
    fn a_resume_clears_the_latch_and_keeps_the_spent_budget() {
        let path = scratch("resume");
        let mut k = KillSwitch::load(&path, 300, u128::MAX, BUDGET, false).unwrap();
        let quote = 1_000_000_000_000u128;
        k.observe(quote, &[pos(0, FAIR)], 1_000).unwrap();
        // A 3000 loss then a 2000 gain, so a resume that reset either total cannot pass by luck.
        k.observe(quote - 3_000_000_000, &[pos(0, FAIR)], 1_001)
            .unwrap();
        k.observe(quote - 1_000_000_000, &[pos(0, FAIR)], 1_002)
            .unwrap();
        assert_eq!(k.state().cumulative_trade_loss, 3_000_000_000);
        assert_eq!(k.state().cumulative_trade_gain, 2_000_000_000);
        let observations = k.state().observations;

        k.halt(
            &Halt::Liveness {
                reason: "chain liveness lost for 615s (limit 600s)".into(),
            },
            5_000,
        )
        .unwrap();
        assert!(k.is_liveness_only());
        k.resume().unwrap();
        assert!(!k.is_halted());
        assert!(!k.is_liveness_only());
        assert!(k.halt_reason().is_none());
        assert!(k.state().halted_at.is_none());
        drop(k);

        // Across the reload: a resume that only cleared memory is undone by the next restart.
        let k = KillSwitch::load(&path, 300, u128::MAX, BUDGET, false).unwrap();
        assert!(!k.is_halted());
        assert!(k.halt_reason().is_none());
        assert_eq!(
            k.state().cumulative_trade_loss,
            3_000_000_000,
            "a resume that hands back spent budget makes an outage a way to launder one"
        );
        assert_eq!(k.state().cumulative_trade_gain, 2_000_000_000);
        assert_eq!(k.state().observations, observations);
    }

    /// The same switch as `ks`, measuring but not latching.
    fn ks_shadow(name: &str) -> KillSwitch {
        KillSwitch::load(&scratch(name), 300, BLEED, BUDGET, true).unwrap()
    }

    /// Drive a drawdown past `BLEED` and hand back what the last observation concluded. mUSDC's 6
    /// decimals throughout: a 1,000,000 peak and a mark 3,000 below it, inside the 300s window.
    /// Zero base on both marks, so NAV is the quote leg alone and the drawdown is the difference.
    fn breach(k: &mut KillSwitch) -> (Observation, Option<Halt>) {
        k.observe(1_000_000_000_000, &[pos(0, FAIR)], 1_000)
            .unwrap();
        k.observe(997_000_000_000, &[pos(0, FAIR)], 1_100).unwrap()
    }

    #[test]
    fn a_shadow_trip_is_reported_and_the_book_keeps_quoting() {
        let mut k = ks_shadow("shadow-trip");
        let (obs, halt) = breach(&mut k);
        assert_eq!(
            obs.would_halt,
            Some(Halt::Bleed {
                drawdown: 3_000_000_000,
                limit: BLEED,
                window_secs: 300
            })
        );
        // The returned option is what a caller acts on, and none of it is enforced.
        assert!(halt.is_none(), "shadow mode returned an enforceable halt");
        assert!(!k.is_halted(), "shadow mode latched the book");
        assert!(k.halt_reason().is_none());
        assert!(k.state().halted_at.is_none());
    }

    #[test]
    fn the_same_breach_latches_once_shadow_is_off() {
        // The control for the test above; without it a switch that never trips would pass.
        let mut k = ks("shadow-control");
        let (obs, halt) = breach(&mut k);
        assert_eq!(obs.would_halt, halt, "enforcing: the two agree exactly");
        assert!(halt.is_some());
        assert!(k.is_halted());
    }

    #[test]
    fn a_shadow_run_still_accumulates_and_still_persists() {
        // The mode exists for the data it leaves behind; measuring only in memory collects nothing.
        let path = scratch("shadow-persist");
        let mut k = KillSwitch::load(&path, 300, BLEED, BUDGET, true).unwrap();
        let mut quote = 1_000_000_000_000u128;
        k.observe(quote, &[pos(0, FAIR)], 1_000).unwrap();
        for i in 0..4u64 {
            quote -= 1_000_000_000;
            k.observe(quote, &[pos(0, FAIR)], 1_400 + i * 400).unwrap();
        }
        assert_eq!(k.state().cumulative_trade_loss, 4_000_000_000);
        assert_eq!(k.state().observations, 4);

        // On disk, not merely in memory.
        let reloaded = KillSwitch::load(&path, 300, BLEED, BUDGET, true).unwrap();
        assert_eq!(reloaded.state().cumulative_trade_loss, 4_000_000_000);
        assert_eq!(reloaded.state().observations, 4);
        assert!(!reloaded.is_halted());
    }

    #[test]
    fn shadow_is_a_property_of_the_run_and_never_of_the_state_file() {
        // A persisted mode is a killswitch somebody disabled once and nobody can see is disabled.
        let path = scratch("shadow-not-sticky");
        let mut shadowed = KillSwitch::load(&path, 300, BLEED, BUDGET, true).unwrap();
        breach(&mut shadowed);
        assert!(!shadowed.is_halted());
        assert!(!std::fs::read_to_string(&path).unwrap().contains("shadow"));

        let mut enforcing = KillSwitch::load(&path, 300, BLEED, BUDGET, false).unwrap();
        let (_, halt) = breach(&mut enforcing);
        assert!(
            halt.is_some(),
            "a restart without shadow must enforce again"
        );
        assert!(enforcing.is_halted());
    }

    #[test]
    fn an_already_latched_group_reports_no_verdict_even_in_shadow() {
        // Shadow mode is not a way to clear a latch; `resume` is the only one.
        let path = scratch("shadow-prelatched");
        let mut k = KillSwitch::load(&path, 300, BLEED, BUDGET, false).unwrap();
        breach(&mut k);
        assert!(k.is_halted());

        let mut k = KillSwitch::load(&path, 300, BLEED, BUDGET, true).unwrap();
        assert!(
            k.is_halted(),
            "shadow mode cleared a latch it found on disk"
        );
        let (obs, halt) = breach(&mut k);
        assert!(halt.is_none());
        assert!(
            obs.would_halt.is_none(),
            "a halted switch stops deciding; its numbers must not keep moving"
        );
    }

    #[test]
    fn large_totals_survive_the_json_round_trip_exactly() {
        // Past 2^53, which is where a JSON number would start lying.
        let path = scratch("bignum");
        let mut k = KillSwitch::load(&path, 300, u128::MAX, u128::MAX, false).unwrap();
        k.state.cumulative_trade_loss = 123_456_789_012_345_678_901_234_567_890;
        k.persist().unwrap();
        let back = KillSwitch::load(&path, 300, u128::MAX, u128::MAX, false).unwrap();
        assert_eq!(
            back.state().cumulative_trade_loss,
            123_456_789_012_345_678_901_234_567_890
        );
        // And it really is stored as a string.
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("\"123456789012345678901234567890\""));
    }

    #[test]
    fn two_pairs_are_marked_and_attributed_together() {
        let mut k = ks("twopair");
        let btc = |base: u128, fair: u128| Position {
            pair_id: 2,
            base_balance: base,
            fair,
            price_scale_exp: 12,
        };
        let btc_fair = 1_180_000_000_000_000u128; // 118_000 a coin, 8-decimal base

        k.observe(0, &[pos(ETH, FAIR), btc(100_000_000, btc_fair)], 1_000)
            .unwrap();
        let (obs, _) = k
            .observe(0, &[pos(ETH, FAIR), btc(100_000_000, btc_fair)], 1_001)
            .unwrap();
        assert_eq!(obs.nav, 1_943_820_000 + 118_000_000_000);
        assert_eq!(obs.trade_pnl, 0);

        // Only BTC moves: revaluation is non-zero, trade PnL still exactly zero.
        let (obs, _) = k
            .observe(
                0,
                &[pos(ETH, FAIR), btc(100_000_000, btc_fair * 9 / 10)],
                1_002,
            )
            .unwrap();
        assert_eq!(obs.trade_pnl, 0);
        assert_eq!(obs.revaluation, -11_800_000_000);
    }
}
