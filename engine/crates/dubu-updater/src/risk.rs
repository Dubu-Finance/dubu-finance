//! Two latching killswitches, and the NAV decomposition they are measured on.
//!
//! # The decomposition, which is the whole design
//!
//! NAV moves for two unrelated reasons, and conflating them makes both killswitches useless:
//! **revaluation**, the market moving under the inventory -- for an *unhedged* book a genuine gain
//! or loss, unbounded and symmetric, and unrelated to whether the quoting is any good -- and
//! **trade PnL**, what the fills themselves added or destroyed, which is the number that says
//! whether takers are picking us off.
//!
//! Between two observations they separate exactly, with no event decoding, because inventory only
//! changes when a trade happens:
//!
//! ```text
//! NAV          = quoteBalance + SUM_i value(baseBalance_i, fair_i)
//! revaluation  = SUM_i [ value(basePrev_i, fairNow_i) - value(basePrev_i, fairPrev_i) ]
//! tradePnl     = (NAV_now - NAV_prev) - revaluation
//! ```
//!
//! The property that makes this trustworthy: **with no trades, `tradePnl` is exactly zero.** Not
//! approximately — the balances are identical, so the two sums cancel term for term over the same
//! integer valuation. That is what lets a cumulative gross budget run for days without drifting
//! into a trip, and why there is no noise floor knob: there is no noise to floor.
//!
//! `value` is [`amount_out_bid`] against a flat ladder at the fair value — the chain's own integer
//! valuation, floored, rather than a division written here.
//!
//! # The two switches
//!
//! | switch | measured on | catches |
//! |---|---|---|
//! | **bleed** | peak-to-current **total NAV** drawdown inside a short window | a fast adverse move on inventory, or a burst of bad fills — the "stop now, work out why later" case |
//! | **loss budget** | cumulative gross **trade PnL** loss, all-time | systematic adverse selection: being picked off a little, repeatedly, while the market goes nowhere |
//!
//! Gross, per archi_v2 §5.4: every negative step is added and a later recovery does **not**
//! hand the budget back. A book that loses 1000 and makes 1000 has been picked off twice, not
//! zero times.
//!
//! # Latching and restart
//!
//! Both latch, written atomically — temp file, then rename — after every change and read at
//! startup, so a halted book **stays down** across a restart. Restarting is the most natural thing
//! an operator does when something looks wrong, and a killswitch that silently resumed quoting
//! would be decoration.
//!
//! What deliberately does **not** survive is the previous-observation state: the last balances and
//! fair values. Trades may have happened while the process was down, so attributing the balance
//! change across that gap would be an invention — the first observation after a restart re-seeds
//! and attributes nothing. The *cumulative* total and the latch persist, which is what matters.
//!
//! # Known gap
//!
//! A manager deposit or an owner withdrawal moves balances with no trade, so it lands in
//! `tradePnl` — a withdrawal looks like a loss. Attributing it properly needs the pool's `Swap`
//! and `ReserveSynced` events. Not built; see the README.

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
    /// The state file exists and is not parseable.
    ///
    /// Deliberately fatal rather than "start fresh": an unreadable latch is indistinguishable
    /// from a latch that was set, and starting fresh would resume a halted book.
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

/// Value a base balance in quote units, at a flat ladder on the fair value.
///
/// Uses the chain's own quote path so the mark and a real fill agree on what the inventory is
/// worth, rather than differing by whatever a hand-written division rounds differently.
///
/// Public because [`crate::skew`] needs the *same* valuation to measure inventory imbalance against
/// target: two that disagree by a rounding step would have the killswitch and the skew arguing
/// about how much the pool holds, which is invisible until it matters.
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

/// What every liveness halt's persisted reason string begins with.
///
/// One place, written by [`Halt`]'s `Display` and read back by [`KillSwitch::is_liveness_only`].
/// Those are the only two spellings of it, which is what makes matching on a prefix survivable —
/// see `is_liveness_only` for why the reason string is what gets matched at all.
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

/// Serialise a `u128` as a decimal string.
///
/// JSON numbers are `f64` to most readers, and this file is read with `jq` during an incident. A
/// total past 2^53 silently becoming approximate is not acceptable in the file that records why
/// the book is down.
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

/// One NAV sample inside the bleed window.
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
        // A missing state file starts here, so a fresh state that read as halted would keep a
        // healthy book down and one carrying loss would start it part-way through its budget.
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
}

impl KillSwitch {
    /// Load the latch, or start a clean one if the file does not exist.
    ///
    /// # Errors
    /// [`RiskError::Io`] if the file exists and cannot be read, [`RiskError::Corrupt`] if it
    /// exists and does not parse. Neither is recoverable by starting fresh — see the variant.
    pub fn load(
        path: &Path,
        bleed_window_secs: u64,
        bleed_limit: u128,
        loss_budget: u128,
    ) -> Result<Self, RiskError> {
        // The config validator refuses all three; restated here because a zero window has no peak
        // to draw down from and a zero limit or budget trips on the first observation.
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
            prev: None,
            window: Vec::new(),
        };
        // What must NOT survive a restart, asserted where it is constructed: trades may have
        // happened while the process was down, so the first observation has to re-seed.
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

    /// Whether the latch is set and was set by a liveness halt, with nothing else behind it.
    ///
    /// Chain liveness is the one halt cause that heals on its own — the chain comes back, and
    /// nothing about the book has changed. `Bleed` and `LossBudget` are the book losing money, and
    /// want a human's eyes before it quotes again. Only the caller in `main.rs` acts on this
    /// distinction, and only at startup; see there.
    ///
    /// The persisted reason string is the discriminator, which looks fragile and is the deliberate
    /// choice. [`RiskState`] is versioned and `deny_unknown_fields`, so a `halt_kind` field would
    /// be a schema migration in both directions: an older binary reading a file written by a newer
    /// one would fail to parse the latch and then, per [`RiskError::Corrupt`], refuse to start at
    /// all. A rollback that cannot read the killswitch is a worse outcome than a prefix match, and
    /// [`LIVENESS_PREFIX`] keeps the literal in one place rather than two.
    #[must_use]
    pub fn is_liveness_only(&self) -> bool {
        self.state.halted
            && self
                .state
                .halt_reason
                .as_deref()
                .is_some_and(|r| r.starts_with(LIVENESS_PREFIX))
    }

    /// Clear the latch, keeping everything the switches have measured.
    ///
    /// The inverse of [`Self::halt`] for exactly one cause, and it is not a general "clear the
    /// killswitch": the caller must have established both that the halt is liveness-only and that
    /// the condition behind it is gone. There is no path here for a bleed or an exhausted budget.
    ///
    /// `cumulative_trade_loss`, `cumulative_trade_gain` and `observations` deliberately survive.
    /// The loss budget is all-time and gross, so resuming with fresh totals would hand back budget
    /// the book has already spent — and worse, it would make waiting for a chain outage a way to
    /// launder an exhausted one.
    ///
    /// # Errors
    /// [`RiskError::Io`] if the cleared state cannot be written. Loud for the mirror of the reason
    /// `halt` is: a resume that did not reach disk is one the next restart silently undoes, and
    /// the caller would have gone on to quote against a file that still says the book is down.
    pub fn resume(&mut self) -> Result<(), RiskError> {
        let spent = (
            self.state.cumulative_trade_loss,
            self.state.cumulative_trade_gain,
            self.state.observations,
        );
        self.state.halted = false;
        self.state.halt_reason = None;
        self.state.halted_at = None;
        // All three or none, mirroring `halt`: a cleared latch still carrying a reason and a
        // timestamp has an operator reading a halt that is not in force.
        assert!(!self.state.halted);
        assert!(self.state.halt_reason.is_none());
        assert!(self.state.halted_at.is_none());
        // Asserted rather than merely intended, because the tempting edit here is to zero the
        // totals along with the latch and that is the one thing a resume must never do.
        assert!(
            spent
                == (
                    self.state.cumulative_trade_loss,
                    self.state.cumulative_trade_gain,
                    self.state.observations,
                )
        );
        self.persist()
    }

    /// The persistent state, for logging.
    #[must_use]
    pub const fn state(&self) -> &RiskState {
        &self.state
    }

    /// Set the latch for a reason the measured switches do not cover — a chain outage, an
    /// operator instruction.
    ///
    /// # Errors
    /// [`RiskError::Io`] if the latch cannot be written. That failure is deliberately loud: an
    /// unwritten latch is one that will not survive the restart.
    pub fn halt(&mut self, halt: &Halt, now: u64) -> Result<(), RiskError> {
        if self.state.halted {
            return Ok(());
        }
        self.state.halted = true;
        self.state.halt_reason = Some(halt.to_string());
        self.state.halted_at = Some(now);
        // All three or none: a latch an operator cannot read the reason for after a restart is one
        // they will clear blind.
        assert!(self.state.halted);
        assert!(self.state.halt_reason.is_some());
        assert!(self.state.halted_at == Some(now));
        self.persist()
    }

    /// Feed one mark through both switches.
    ///
    /// Returns the trip, if this observation caused one. A no-op once halted: the latch does not
    /// need re-deciding and its numbers should not keep moving after the fact.
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
        // A NAV marked on nothing is not a NAV, and a drawdown measured against it is not a
        // drawdown. The caller skips groups with no positions rather than marking an empty book.
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
        // A seed attributes nothing: trades may have happened before it, and inventing an
        // attribution across that gap is what would poison the cumulative budget.
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

        let obs = Observation {
            nav,
            revaluation,
            trade_pnl,
            drawdown,
            cumulative_trade_loss: self.state.cumulative_trade_loss,
            seeded,
        };

        if self.state.halted {
            return Ok((obs, None));
        }

        let halt = self.verdict(drawdown);
        if let Some(h) = &halt {
            self.halt(h, now)?;
            assert!(self.state.halted, "a trip that did not latch is decoration");
        } else {
            // Persist the running totals even without a trip, or a restart hands back whatever
            // budget was consumed since the last one.
            self.persist()?;
        }
        Ok((obs, halt))
    }

    /// Push this NAV into the bleed window, drop what has aged out, and return the peak-to-current
    /// drawdown inside it.
    fn record_drawdown(&mut self, now: u64, nav: u128) -> u128 {
        self.window.push(NavPoint { at: now, nav });
        let cutoff = now.saturating_sub(self.bleed_window_secs);
        self.window.retain(|p| p.at >= cutoff);
        // The point just pushed is stamped `now` and the cutoff is at or before it, so it always
        // survives its own retain -- which is what makes the peak below at least this NAV.
        assert!(!self.window.is_empty());
        let peak = self.window.iter().map(|p| p.nav).max().unwrap_or(nav);
        assert!(peak >= nav);
        peak.saturating_sub(nav)
    }

    /// Fold one attributed step into the running totals.
    ///
    /// Gross, per archi_v2 §5.4: every negative step is added and a later recovery does **not** hand
    /// the budget back. A book that loses 1000 and makes 1000 has been picked off twice, not zero
    /// times.
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
        // Gross means gross: the only thing that may consume the budget is a loss.
        if trade_pnl >= 0 {
            assert!(self.state.cumulative_trade_loss == before);
        }
    }

    /// Which switch, if either, this observation tripped.
    ///
    /// Bleed first: it is the faster-moving switch and the more urgent verdict.
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

    /// Write the state atomically: temp file in the same directory, then rename.
    ///
    /// Same directory because `rename` is only atomic within a filesystem. A half-written latch
    /// is the one file in this system that must not exist.
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
        // Writing the scratch file over the latch and then renaming it to itself would destroy the
        // one file that must never be half-written.
        assert!(tmp != self.path);
        std::fs::write(&tmp, text).map_err(io)?;
        std::fs::rename(&tmp, &self.path).map_err(io)?;
        Ok(())
    }
}

/// NAV: the quote balance plus every base balance marked at its own fair value.
///
/// # Errors
/// [`RiskError::Mark`] if a balance cannot be valued.
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
///
/// Marking the old balances at both fair values is what leaves the residual as the fills' doing.
///
/// # Errors
/// [`RiskError::Mark`] if a balance cannot be valued.
fn revaluation_of(
    prev_positions: &BTreeMap<u16, Position>,
    now_positions: &BTreeMap<u16, Position>,
) -> Result<i128, RiskError> {
    let mut reval = 0i128;
    for (id, prev) in prev_positions {
        // A pair that vanished from the config between observations cannot be revalued; treat its
        // old fair as still current, which contributes zero.
        let fair_now = now_positions.get(id).map_or(prev.fair, |p| p.fair);
        let then =
            value(prev.base_balance, prev.fair, prev.price_scale_exp).map_err(RiskError::Mark)?;
        let now_v =
            value(prev.base_balance, fair_now, prev.price_scale_exp).map_err(RiskError::Mark)?;
        // The property the whole decomposition rests on: the same balance at the same fair over
        // the same integer valuation must cancel term for term, so a quiet market contributes
        // nothing and `trade_pnl` is exactly zero rather than approximately.
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
        KillSwitch::load(&scratch(name), 300, BLEED, BUDGET).unwrap()
    }

    /// A switch with the bleed limit disabled, for the tests that are about attribution or the
    /// cumulative budget. The two switches genuinely overlap — any single-step loss large
    /// enough to exhaust the budget also breaches a comparable bleed limit — so isolating one
    /// means turning the other off rather than hoping it stays quiet.
    fn ks_budget_only(name: &str) -> KillSwitch {
        KillSwitch::load(&scratch(name), 300, u128::MAX, BUDGET).unwrap()
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
        // The property the whole design rests on. The market moves 20%, no fill happens, and
        // the cumulative budget must not move by a single unit.
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

        // Sell it back at 2000: a 56.18 gain. The budget must NOT be handed back — a book that
        // loses 56 and makes 56 has been picked off once, not zero times.
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
        // This is the case that justifies having two switches. Ten losses of 1000 mUSDC, each
        // well inside the 2000 bleed limit, and each spaced far enough apart that the previous
        // peak has aged out of the 300s window — so the bleed switch sees a drawdown of zero
        // every single time. Being picked off for a little, repeatedly, is invisible to a
        // short-window drawdown limit and is exactly what the cumulative budget is for.
        let mut k = KillSwitch::load(&scratch("budget"), 300, BLEED, BUDGET).unwrap();
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
        // The deliberate difference from the loss budget: this book is unhedged, so a fast
        // adverse move on inventory IS a loss, whether or not anyone traded with us.
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
        // Otherwise the switch is a permanent all-time-high drawdown limit and trips on any
        // slow drift, which is not what a short-window bleed limit means.
        let mut k = KillSwitch::load(&scratch("window"), 300, BLEED, BUDGET).unwrap();
        k.observe(0, &[pos(10 * ETH, FAIR)], 1_000).unwrap();

        // 400 seconds later the old peak is outside the window, so the same price is not a
        // drawdown at all.
        let low = FAIR - 200_000_000_000_000;
        let (obs, halt) = k.observe(0, &[pos(10 * ETH, low)], 1_400).unwrap();
        assert_eq!(obs.drawdown, 0);
        assert_eq!(halt, None);
        assert!(!k.is_halted());
    }

    #[test]
    fn the_latch_survives_a_restart_and_the_book_stays_down() {
        // The requirement in one test: a restart must not silently resume a halted book.
        let path = scratch("restart");
        let mut k = KillSwitch::load(&path, 300, BLEED, BUDGET).unwrap();
        k.observe(0, &[pos(10 * ETH, FAIR)], 1_000).unwrap();
        let (_, halt) = k
            .observe(0, &[pos(10 * ETH, FAIR - 200_000_000_000_000)], 1_100)
            .unwrap();
        assert!(halt.is_some());
        drop(k);

        let restarted = KillSwitch::load(&path, 300, BLEED, BUDGET).unwrap();
        assert!(restarted.is_halted(), "a restart resumed a halted book");
        assert!(restarted.halt_reason().unwrap().contains("bleed"));
        assert!(restarted.state().halted_at.is_some());
    }

    #[test]
    fn the_cumulative_budget_survives_a_restart_too() {
        // A restart that reset the running total would make the budget a per-process limit,
        // which an operator restarting after every trip would never reach.
        let path = scratch("cumulative");
        let mut k = KillSwitch::load(&path, 300, u128::MAX, BUDGET).unwrap();
        k.observe(1_000_000_000_000, &[pos(0, FAIR)], 1_000)
            .unwrap();
        k.observe(1_000_000_000_000 - 3_000_000_000, &[pos(0, FAIR)], 1_001)
            .unwrap();
        assert_eq!(k.state().cumulative_trade_loss, 3_000_000_000);
        drop(k);

        let mut k = KillSwitch::load(&path, 300, u128::MAX, BUDGET).unwrap();
        assert_eq!(k.state().cumulative_trade_loss, 3_000_000_000);
        assert!(!k.is_halted());

        // And the first post-restart observation only re-seeds: trades may have happened while
        // the process was down, so attributing the balance gap to trade PnL would be invented.
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
        let err = KillSwitch::load(&path, 300, BLEED, BUDGET).unwrap_err();
        assert!(matches!(err, RiskError::Corrupt { .. }), "got {err}");
    }

    #[test]
    fn a_missing_latch_file_is_a_clean_start() {
        let k = KillSwitch::load(&scratch("missing"), 300, BLEED, BUDGET).unwrap();
        assert!(!k.is_halted());
        assert_eq!(k.state().cumulative_trade_loss, 0);
    }

    #[test]
    fn a_liveness_halt_latches_like_the_measured_ones() {
        let path = scratch("liveness");
        let mut k = KillSwitch::load(&path, 300, BLEED, BUDGET).unwrap();
        k.halt(
            &Halt::Liveness {
                reason: "chain down for 600s".into(),
            },
            5_000,
        )
        .unwrap();
        assert!(k.is_halted());
        drop(k);
        let k = KillSwitch::load(&path, 300, BLEED, BUDGET).unwrap();
        assert!(k.is_halted());
        assert!(k.halt_reason().unwrap().contains("chain down"));
    }

    #[test]
    fn a_liveness_latch_is_still_recognisable_as_one_after_a_reload() {
        // The startup recovery in `main.rs` runs against a file some previous process wrote, so
        // there is no in-memory `Halt` left to ask by then — recognising the cause has to survive
        // the round trip through JSON, which is the whole reason the reason string carries it.
        let path = scratch("liveness-only");
        let mut k = KillSwitch::load(&path, 300, BLEED, BUDGET).unwrap();
        k.halt(
            &Halt::Liveness {
                reason: "chain liveness lost for 615s (limit 600s)".into(),
            },
            5_000,
        )
        .unwrap();
        assert!(k.is_liveness_only());
        drop(k);

        let k = KillSwitch::load(&path, 300, BLEED, BUDGET).unwrap();
        assert!(k.is_halted());
        assert!(k.is_liveness_only());
    }

    #[test]
    fn neither_measured_switch_latches_as_liveness_only() {
        // The distinction has to hold in this direction or the recovery becomes a killswitch that
        // any restart clears. A bleed and an exhausted budget are the book losing money.
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

        // And an unlatched switch is not liveness-only either, or the recovery path would fire on
        // a book that was never down.
        let k = ks("never-latched");
        assert!(!k.is_halted());
        assert!(!k.is_liveness_only());
    }

    #[test]
    fn a_resume_clears_the_latch_and_keeps_the_spent_budget() {
        let path = scratch("resume");
        let mut k = KillSwitch::load(&path, 300, u128::MAX, BUDGET).unwrap();
        let quote = 1_000_000_000_000u128;
        k.observe(quote, &[pos(0, FAIR)], 1_000).unwrap();
        // A 3000 loss, then a 2000 gain, so both cumulative totals are non-zero and a resume that
        // reset either of them cannot pass by accident.
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

        // Across the reload, because a resume that only cleared memory would be undone by the very
        // next restart — and it is the restarts that this whole path exists to survive.
        let k = KillSwitch::load(&path, 300, u128::MAX, BUDGET).unwrap();
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

    #[test]
    fn large_totals_survive_the_json_round_trip_exactly() {
        // Past 2^53, which is where a JSON number would start lying.
        let path = scratch("bignum");
        let mut k = KillSwitch::load(&path, 300, u128::MAX, u128::MAX).unwrap();
        k.state.cumulative_trade_loss = 123_456_789_012_345_678_901_234_567_890;
        k.persist().unwrap();
        let back = KillSwitch::load(&path, 300, u128::MAX, u128::MAX).unwrap();
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
