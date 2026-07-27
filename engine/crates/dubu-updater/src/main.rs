//! The quote loop.
//!
//! ```text
//! startup   config -> chain facts -> killswitch latch -> key -> feed
//! cycle     poll chain (1 request) -> per pair: fair value -> row -> decision -> maybe send
//!           -> mark NAV -> killswitches
//! shutdown  withdraw quotes, then exit
//! ```
//!
//! # Withdrawing quotes
//!
//! Both the shutdown path and a killswitch trip "withdraw quotes", and the mechanism deserves
//! stating because it is not the obvious one. The updater role **cannot call `pause`** — that is
//! the guardian's, held on separate hardware precisely so a compromised updater cannot take the
//! pool down. What the updater can do is `refreshCapacity(pairId, 0, 0)`, and a pair with zero
//! capacity returns zero from every quote path in `PropPool._outFor`. That is a complete
//! withdrawal, inside the authority this key actually has.
//!
//! The backstop behind it is the pool's own `maxStaleSecs`: even if this process dies without
//! withdrawing anything, every quote stops being fillable an hour later. That is why the
//! heartbeat must sit inside that window, and why `chain::verify_against_chain` refuses to start
//! if it does not.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::watch;
use tracing::{error, info, warn};

use dubu_updater::chain::{ChainFacts, ChainHealth, ChainPoller, ChainStatus, ChainView, Rpc};
use dubu_updater::config::{Config, KeySource, PairConfig};
use dubu_updater::fair_value::FairValueTracker;
use dubu_updater::feed::{binance, FeedShared, FeedStatus};
use dubu_updater::ladder::{self, RowInputs};
use dubu_updater::policy::{self, CapacityDecision, Context, Decision};
use dubu_updater::risk::{Halt, KillSwitch, Position};
use dubu_updater::tx::{Intent, Sender, Sent, Settled, Signer};
use dubu_updater::units::{self, FEED_SCALE};

/// Exit code when the bot stops because a killswitch latched or the chain went away.
const EXIT_HALTED: i32 = 2;
/// Exit code for a startup failure.
const EXIT_STARTUP: i32 = 1;

#[derive(Debug)]
struct Args {
    config: String,
    once: bool,
    force_dry_run: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args { config: "updater.toml".into(), once: false, force_dry_run: false };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--config" | "-c" => a.config = it.next().ok_or("--config needs a path")?,
            "--once" => a.once = true,
            // A command-line override that can only ever make the bot safer. There is
            // deliberately no `--transmit` counterpart: broadcasting is a config decision, made
            // in a file that gets reviewed, not a flag someone can add to a command.
            "--dry-run" => a.force_dry_run = true,
            "--help" | "-h" => {
                println!(
                    "dubu-updater [--config <path>] [--once] [--dry-run]\n\n\
                     --config   path to the TOML config (default: updater.toml)\n\
                     --once     run a single evaluation cycle and exit\n\
                     --dry-run  force dry run regardless of config (there is no --transmit)\n"
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    Ok(a)
}

#[tokio::main]
async fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("dubu-updater: {e}");
            std::process::exit(EXIT_STARTUP);
        }
    };

    tracing_subscriber::fmt()
        .json()
        .flatten_event(true)
        .with_current_span(false)
        .with_span_list(false)
        .with_target(true)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    match run(&args).await {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            error!(target: "startup", event = "fatal", error = %e, "cannot start");
            std::process::exit(EXIT_STARTUP);
        }
    }
}

/// Everything resolved at startup, so the cycle never has to ask again.
struct Runtime {
    cfg: Config,
    facts: ChainFacts,
    rpc: Rpc,
    flash: Rpc,
    poller: ChainPoller,
    feed: Arc<FeedShared>,
    /// Keyed by pair id rather than positionally: a `Vec` indexed in lock-step with
    /// `cfg.pairs` is an invariant nothing enforces, and getting it wrong prices one pair
    /// off another's outlier history.
    trackers: BTreeMap<u16, FairValueTracker>,
    sender: Sender,
    kill: KillSwitch,
    health: ChainHealth,
}

#[allow(clippy::too_many_lines)]
async fn run(args: &Args) -> Result<i32, Box<dyn std::error::Error>> {
    let mut cfg = Config::load(std::path::Path::new(&args.config))?;
    if args.force_dry_run {
        cfg.tx.transmit_allowed = false;
    }

    let rpc = Rpc::new("rpc", cfg.chain.rpc_url.clone(), &cfg.chain)?;
    let flash = Rpc::new("flashblocks", cfg.chain.flashblocks_rpc_url.clone(), &cfg.chain)?;

    // Every check that needs the chain, before the loop is allowed to compute anything.
    let facts = dubu_updater::chain::verify_against_chain(&rpc, cfg.chain.pool, cfg.chain.multicall3, &cfg).await?;

    // A missing key is fatal only when something is actually going to be broadcast. A dry run
    // has to work with no key material present at all — that is most of what makes it safe to
    // hand to someone, and requiring the production key in order to rehearse would defeat it.
    let signer = match cfg.tx.key_source()? {
        Some(src) => {
            let where_from = match &src {
                KeySource::Env(n) => format!("env:{n}"),
                KeySource::File(p) => format!("file:{}", p.display()),
            };
            match (Signer::load(&src), cfg.tx.transmit_allowed) {
                (Ok(s), _) => {
                    info!(target: "startup", event = "key_loaded", address = %s.address(),
                          source = %where_from, "signing key loaded");
                    Some(s)
                }
                (Err(e), true) => return Err(e.into()),
                (Err(e), false) => {
                    warn!(target: "startup", event = "no_key", source = %where_from, error = %e,
                          "no signing key available; continuing because this is a dry run");
                    None
                }
            }
        }
        None => None,
    };

    // Signing as anything other than the pool's updater means every transaction reverts
    // `NotUpdater`. Cheaper to discover here than as a stream of failed pushes.
    if cfg.tx.transmit_allowed {
        match signer.as_ref().map(Signer::address) {
            Some(a) if a == facts.updater => {}
            Some(a) => {
                return Err(format!(
                    "transmit_allowed is set but the loaded key is {a}, and the pool's updater is {}",
                    facts.updater
                )
                .into())
            }
            None => return Err("transmit_allowed is set but no key is loaded".into()),
        }
    }

    let kill = KillSwitch::load(
        &cfg.risk.state_path,
        cfg.risk.bleed_window_secs,
        cfg.risk.bleed_limit_units()?,
        cfg.risk.loss_budget_units()?,
    )?;

    let mut sender = Sender::new(
        signer,
        cfg.chain.chain_id,
        cfg.chain.pool,
        &cfg.tx,
        cfg.tx.max_fee_wei()?,
        cfg.tx.max_priority_fee_wei()?,
    );

    info!(
        target: "startup",
        event = "configured",
        transmit_allowed = cfg.tx.transmit_allowed,
        pool = %cfg.chain.pool,
        chain_id = cfg.chain.chain_id,
        updater = %facts.updater,
        signing_as = ?sender.address(),
        pairs = cfg.pairs.len(),
        poll_interval_ms = cfg.chain.poll_interval_ms,
        requests_per_sec = cfg.chain.requests_per_sec,
        nav_token = %facts.nav_token,
        "dubu-updater starting"
    );
    if !cfg.tx.transmit_allowed {
        info!(target: "startup", event = "dry_run",
              "DRY RUN: rows will be computed and decisions logged, nothing will be broadcast");
    }

    // Stay-down. A restart is the first thing an operator does, and it must not resume a book
    // that a killswitch took down.
    if kill.is_halted() {
        error!(
            target: "risk", event = "stay_down",
            reason = kill.halt_reason().unwrap_or("(unrecorded)"),
            halted_at = ?kill.state().halted_at,
            state_path = %cfg.risk.state_path.display(),
            "killswitch is latched from a previous run; re-asserting the withdrawal and exiting. \
             Clear the state file deliberately to resume."
        );
        withdraw_quotes(&cfg, &rpc, &mut sender).await;
        return Ok(EXIT_HALTED);
    }

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let feed = Arc::new(FeedShared::new(Duration::from_millis(cfg.feed.stale_after_ms)));
    let streams: Vec<String> = cfg.pairs.iter().map(PairConfig::stream_name).collect();
    let feed_task = tokio::spawn(binance::run(cfg.feed.clone(), streams, Arc::clone(&feed), shutdown_rx.clone()));

    let poller = ChainPoller::new(
        cfg.chain.pool,
        cfg.chain.multicall3,
        cfg.pairs.iter().map(|p| p.pair_id).collect(),
        facts.tokens.clone(),
    );
    let trackers: BTreeMap<u16, FairValueTracker> = cfg
        .pairs
        .iter()
        .map(|p| (p.pair_id, FairValueTracker::new(cfg.feed.max_jump_bps, cfg.feed.outlier_tolerance)))
        .collect();

    let health = ChainHealth::new(Instant::now(), cfg.chain.degraded_after_secs, cfg.chain.halt_after_secs);
    let mut rt = Runtime { cfg, facts, rpc, flash, poller, feed, trackers, sender, kill, health };

    wait_for_feed(&rt).await;

    let code = quote_loop(&mut rt, args.once, shutdown_rx).await;

    let _ = shutdown_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(3), feed_task).await;
    Ok(code)
}

/// Give the socket a chance to produce a first tick before the first cycle, so the opening log
/// line is a quote rather than "feed not live".
async fn wait_for_feed(rt: &Runtime) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let all_live = rt
            .cfg
            .pairs
            .iter()
            .all(|p| rt.feed.snapshot(&p.symbol, Instant::now()).status == FeedStatus::Live);
        if all_live {
            info!(target: "feed", event = "ready", "every configured symbol is live");
            return;
        }
        if Instant::now() >= deadline {
            warn!(target: "feed", event = "not_ready",
                  "starting the loop without a live feed on every symbol; pushes will be gated until it arrives");
            return;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn quote_loop(rt: &mut Runtime, once: bool, mut shutdown: watch::Receiver<bool>) -> i32 {
    let mut last_view: Option<ChainView> = None;
    let interval = Duration::from_millis(rt.cfg.chain.poll_interval_ms);
    let mut halted = false;

    loop {
        let cycle_start = Instant::now();

        match rt.poller.poll(&rt.flash).await {
            Ok(v) => {
                rt.health.on_success(Instant::now());
                last_view = Some(v);
            }
            Err(e) => {
                rt.health.on_failure(&e);
                let level_is_rate_limit = e.is_rate_limit();
                warn!(
                    target: "chain", event = "poll_failed", error = %e,
                    rate_limit = level_is_rate_limit,
                    consecutive_failures = rt.health.consecutive_failures(),
                    status = rt.health.status(Instant::now()).label(),
                    "chain poll failed"
                );
            }
        }

        let status = rt.health.status(Instant::now());
        if let ChainStatus::Down { stale_secs } = status {
            let halt = Halt::Liveness {
                reason: format!(
                    "no successful chain poll for {stale_secs}s (limit {}s); \
                     last error: {}",
                    rt.cfg.chain.halt_after_secs,
                    rt.health.last_error().unwrap_or("(none)")
                ),
            };
            error!(target: "risk", event = "halt", switch = halt.label(), reason = %halt,
                   "chain connection is down; halting and withdrawing quotes");
            let _ = rt.kill.halt(&halt, now_unix());
            halted = true;
        }

        if !halted {
            if let Some(view) = &last_view {
                halted = run_cycle(rt, view, status).await;
            }
        }

        if halted {
            withdraw_quotes(&rt.cfg, &rt.rpc, &mut rt.sender).await;
            return EXIT_HALTED;
        }

        if once {
            info!(target: "loop", event = "once_complete", "single cycle requested; shutting down");
            withdraw_quotes(&rt.cfg, &rt.rpc, &mut rt.sender).await;
            return 0;
        }

        let elapsed = cycle_start.elapsed();
        let sleep_for = interval.saturating_sub(elapsed);
        tokio::select! {
            biased;
            _ = shutdown.changed() => break,
            () = wait_for_signal() => break,
            () = tokio::time::sleep(sleep_for) => {}
        }
    }

    info!(target: "loop", event = "shutdown", "shutdown signal received; withdrawing quotes");
    withdraw_quotes(&rt.cfg, &rt.rpc, &mut rt.sender).await;
    0
}

/// One evaluation over every pair. Returns `true` if a killswitch latched.
async fn run_cycle(rt: &mut Runtime, view: &ChainView, status: ChainStatus) -> bool {
    // Settle anything outstanding first, so `in_flight` is current when the gates run.
    for (pair_id, pending, settled) in rt.sender.poll_pending(&rt.rpc).await {
        match settled {
            Settled::Confirmed { block } => info!(
                target: "tx", event = "confirmed", pair_id, kind = pending.kind,
                tx = %pending.hash, nonce = pending.nonce, block, "transaction confirmed"
            ),
            Settled::Reverted { block } => error!(
                target: "tx", event = "reverted", pair_id, kind = pending.kind,
                tx = %pending.hash, nonce = pending.nonce, block,
                "transaction REVERTED after passing every off-chain check; \
                 this is a dubu-core divergence and must be investigated before quoting resumes"
            ),
            Settled::TimedOut { waited_secs } => warn!(
                target: "tx", event = "timed_out", pair_id, kind = pending.kind,
                tx = %pending.hash, nonce = pending.nonce, waited_secs,
                max_fee_wei = rt.cfg.tx.max_fee_per_gas_gwei,
                "transaction never confirmed; abandoning the intent and resyncing the nonce. \
                 If this repeats, raise tx.max_fee_per_gas_gwei"
            ),
        }
    }

    let view_age = view.age(Instant::now()).as_secs();
    let degraded_extra = if matches!(status, ChainStatus::Degraded { .. }) {
        rt.cfg.chain.degraded_extra_half_spread_bps
    } else {
        0
    };
    if degraded_extra > 0 {
        warn!(target: "chain", event = "degraded", extra_half_spread_bps = degraded_extra,
              status = status.label(), "chain view is degraded; widening every half-spread");
    }

    let mut positions: Vec<Position> = Vec::new();
    let mut sends: Vec<Intent> = Vec::new();

    for i in 0..rt.cfg.pairs.len() {
        let pair = rt.cfg.pairs[i].clone();
        let Some(meta) = rt.facts.pairs.get(&pair.pair_id).copied() else { continue };
        let Some(snap) = view.snaps.get(&pair.pair_id).copied() else { continue };
        let Some(tracker) = rt.trackers.get_mut(&pair.pair_id) else { continue };

        let feed_snap = rt.feed.snapshot(&pair.symbol, Instant::now());
        let shift = units::price_shift(meta.price_scale_exp, meta.base_decimals, meta.quote_decimals);

        // Fair value. The tracker is reset whenever the feed is not live so that a recovered
        // feed is not immediately rejected as an outlier against a pre-outage price.
        let fair = match feed_snap.live() {
            None => {
                tracker.reset();
                None
            }
            Some(tick) => match tracker.observe(tick) {
                Ok(fv) => {
                    let pool_price = units::to_pool_price(fv.micro, shift);
                    info!(
                        target: "feed", event = "fair_value", pair_id = pair.pair_id, symbol = %pair.symbol,
                        micro = %units::format_fixed(fv.micro, FEED_SCALE),
                        bid = %units::format_fixed(fv.bid, FEED_SCALE),
                        ask = %units::format_fixed(fv.ask, FEED_SCALE),
                        book_spread_bps = fv.book_spread_bps,
                        pool_price = ?pool_price,
                        regime_shift = fv.after_concession,
                        feed_age_ms = ?feed_snap.age_ms,
                        "fair value"
                    );
                    pool_price
                }
                Err(e) => {
                    warn!(target: "feed", event = "tick_rejected", pair_id = pair.pair_id,
                          symbol = %pair.symbol, error = %e, "tick rejected by the outlier filter");
                    None
                }
            },
        };

        let base_balance = view.balances.get(&meta.base).copied().unwrap_or(0);
        let quote_balance = view.balances.get(&meta.quote).copied().unwrap_or(0);

        // Capacity the pool can actually honour.
        let capacity = fair
            .and_then(|f| {
                ladder::plan_capacity(&ladder::CapacityInputs {
                    configured_bid: pair.bid_capacity_units().unwrap_or(0),
                    configured_ask: pair.ask_capacity_units().unwrap_or(0),
                    base_balance,
                    quote_balance,
                    min_base_reserve: meta.min_base_reserve,
                    min_quote_reserve: meta.min_quote_reserve,
                    fair: f,
                    price_scale_exp: meta.price_scale_exp,
                })
                .ok()
            })
            .unwrap_or(ladder::CapacityPlan {
                bid: 0,
                ask: 0,
                bid_cut_by_inventory: false,
                ask_cut_by_inventory: false,
            });
        if capacity.bid_cut_by_inventory || capacity.ask_cut_by_inventory {
            warn!(target: "ladder", event = "capacity_cut", pair_id = pair.pair_id,
                  bid = %capacity.bid, ask = %capacity.ask,
                  bid_cut = capacity.bid_cut_by_inventory, ask_cut = capacity.ask_cut_by_inventory,
                  "configured capacity exceeds what the inventory can settle; cut to fit");
        }

        // The row is solved against the capacity that will be in force when it executes: the
        // one already on chain, unless this cycle is also going to post a new epoch.
        let bid_cap = if snap.bid_capacity == 0 { capacity.bid } else { snap.bid_capacity };
        let ask_cap = if snap.ask_capacity == 0 { capacity.ask } else { snap.ask_capacity };

        let row = fair.and_then(|f| {
            match ladder::build(&RowInputs {
                pair_id: pair.pair_id,
                fair: f,
                half_spread_bps: pair.half_spread_bps.saturating_add(degraded_extra),
                width_bps: pair.width_bps,
                skew_bps: pair.skew_bps,
                capture: pair.capture_units().unwrap_or(0),
                bid_capacity: bid_cap,
                ask_capacity: ask_cap,
                min_price: meta.min_price,
                price_scale_exp: meta.price_scale_exp,
            }) {
                Ok(r) => Some(r),
                Err(e) => {
                    warn!(target: "ladder", event = "row_dropped", pair_id = pair.pair_id, error = %e,
                          "computed row failed an off-chain check; dropped and NOT sent");
                    None
                }
            }
        });

        if let Some(r) = &row {
            let h = |p: u128| units::from_pool_price(p, shift).map(|v| units::format_fixed(v, FEED_SCALE));
            info!(
                target: "ladder", event = "row", pair_id = pair.pair_id, symbol = %pair.symbol,
                min_bid = %r.ladder.min_bid, max_bid = %r.ladder.max_bid,
                min_ask = %r.ladder.min_ask, max_ask = %r.ladder.max_ask,
                human_max_bid = ?h(r.ladder.max_bid), human_min_ask = ?h(r.ladder.min_ask),
                bid_target = %r.bid_target, ask_target = %r.ask_target,
                realised_bid = %r.realised_bid, realised_ask = %r.realised_ask,
                bid_width = %r.bid.width, ask_width = %r.ask.width,
                bid_binding = ?r.bid.binding, ask_binding = ?r.ask.binding,
                ask_repaired = r.ask_repaired,
                bid_capture_cost = %r.bid_capture_cost, ask_capture_cost = %r.ask_capture_cost,
                word = %r.word.to_hex().unwrap_or_default(),
                "ladder computed and validated"
            );
        }

        let ctx = Context {
            block_timestamp: view.block_timestamp,
            snap: &snap,
            planned: row.as_ref().map(|r| r.ladder),
            capacity,
            min_price: meta.min_price,
            halted: rt.kill.is_halted(),
            feed: feed_snap.status,
            chain: status,
            view_age_secs: view_age,
            view_stale_secs: rt.cfg.chain.view_stale_secs,
            in_flight: rt.sender.in_flight(pair.pair_id),
            heartbeat_secs: pair.heartbeat_secs,
            adverse_drift_bps: pair.adverse_drift_bps,
            favourable_drift_bps: pair.favourable_drift_bps,
            capacity_divergence_pct: pair.capacity_divergence_pct,
        };

        let quote_decision = policy::evaluate_quote(&ctx).unwrap_or_else(|e| {
            warn!(target: "policy", event = "evaluate_failed", pair_id = pair.pair_id, error = %e,
                  "could not evaluate the stored ladder; holding");
            Decision::Hold(policy::Hold::NoRow)
        });
        let capacity_decision = policy::evaluate_capacity(&ctx);

        info!(
            target: "policy", event = "decision", pair_id = pair.pair_id, symbol = %pair.symbol,
            quote = quote_decision.label(), quote_detail = ?quote_decision,
            capacity = capacity_decision.label(), capacity_detail = ?capacity_decision,
            quote_age_secs = snap.quote_age_secs(view.block_timestamp),
            heartbeat_limit_secs = ctx.heartbeat_limit(),
            bid_used = %snap.bid_used(), ask_used = %snap.ask_used(),
            bid_capacity = %snap.bid_capacity, ask_capacity = %snap.ask_capacity,
            block = view.block_number, block_timestamp = view.block_timestamp,
            view_age_secs = view_age, feed = feed_snap.status.label(), chain = status.label(),
            "decision"
        );

        // Capacity first: a ladder is worthless against a zero epoch, and posting the epoch
        // first means the row that follows is solved against the capacity it will execute on.
        if let CapacityDecision::Send(_) = capacity_decision {
            sends.push(Intent::RefreshCapacity {
                pair_id: pair.pair_id,
                bid: capacity.bid,
                ask: capacity.ask,
            });
        } else if let (Decision::Send(_), Some(r)) = (quote_decision, &row) {
            match r.packed() {
                Ok(word) => sends.push(Intent::UpdateQuote { pair_id: pair.pair_id, word }),
                Err(e) => error!(target: "ladder", event = "pack_failed", pair_id = pair.pair_id,
                                 error = %e, "a validated row would not pack; dropped"),
            }
        }

        if let Some(f) = fair {
            positions.push(Position {
                pair_id: pair.pair_id,
                base_balance,
                fair: f,
                price_scale_exp: meta.price_scale_exp,
            });
        }
    }

    for intent in sends {
        emit(rt, intent).await;
    }

    // The killswitches. Skipped entirely when any pair has no fair value: marking inventory
    // against a price we do not have would be an invention, and an invented NAV is worse than
    // no observation at all.
    if positions.len() == rt.cfg.pairs.len() {
        let quote_balance = view.balances.get(&rt.facts.nav_token).copied().unwrap_or(0);
        match rt.kill.observe(quote_balance, &positions, now_unix()) {
            Ok((obs, halt)) => {
                info!(
                    target: "risk", event = "mark",
                    nav = %obs.nav, revaluation = %obs.revaluation, trade_pnl = %obs.trade_pnl,
                    drawdown = %obs.drawdown, cumulative_trade_loss = %obs.cumulative_trade_loss,
                    seeded = obs.seeded, "NAV marked"
                );
                if let Some(h) = halt {
                    error!(target: "risk", event = "halt", switch = h.label(), reason = %h,
                           "KILLSWITCH TRIPPED; withdrawing quotes and exiting");
                    return true;
                }
            }
            Err(e) => warn!(target: "risk", event = "mark_failed", error = %e, "could not mark NAV"),
        }
    } else {
        info!(target: "risk", event = "mark_skipped", have = positions.len(), want = rt.cfg.pairs.len(),
              "not every pair has a fair value; skipping the NAV observation rather than inventing one");
    }

    false
}

async fn emit(rt: &mut Runtime, intent: Intent) {
    match rt.sender.send(&rt.rpc, intent).await {
        Ok(Sent::DryRun { calldata, would_be_hash, would_be_nonce }) => info!(
            target: "tx", event = "dry_run", pair_id = intent.pair_id(), kind = intent.label(),
            to = %rt.cfg.chain.pool, calldata = %dubu_updater::chain::hex0x(&calldata),
            calldata_bytes = calldata.len(), would_be_hash = ?would_be_hash, would_be_nonce = ?would_be_nonce,
            "DRY RUN: would send this transaction"
        ),
        Ok(Sent::Broadcast { hash, nonce }) => info!(
            target: "tx", event = "sent", pair_id = intent.pair_id(), kind = intent.label(),
            tx = %hash, nonce, "transaction broadcast"
        ),
        Err(e) => error!(
            target: "tx", event = "send_failed", pair_id = intent.pair_id(), kind = intent.label(),
            error = %e, "could not send"
        ),
    }
}

/// Post a zero capacity epoch on every pair, which is how the updater role withdraws quotes.
///
/// Best effort and never fatal: this runs on the way out, often *because* something is already
/// broken, and an error here must not stop the remaining pairs from being withdrawn.
async fn withdraw_quotes(cfg: &Config, rpc: &Rpc, sender: &mut Sender) {
    for p in &cfg.pairs {
        let intent = Intent::RefreshCapacity { pair_id: p.pair_id, bid: 0, ask: 0 };
        match sender.send(rpc, intent).await {
            Ok(Sent::DryRun { calldata, .. }) => info!(
                target: "tx", event = "withdraw_dry_run", pair_id = p.pair_id,
                calldata = %dubu_updater::chain::hex0x(&calldata),
                "DRY RUN: would withdraw quotes by posting a zero capacity epoch"
            ),
            Ok(Sent::Broadcast { hash, nonce }) => warn!(
                target: "tx", event = "withdrawn", pair_id = p.pair_id, tx = %hash, nonce,
                "quotes withdrawn: capacity epoch set to zero"
            ),
            Err(e) => error!(
                target: "tx", event = "withdraw_failed", pair_id = p.pair_id, error = %e,
                "COULD NOT WITHDRAW QUOTES; the pool's own maxStaleSecs is now the only backstop"
            ),
        }
    }
}

/// Resolve on SIGINT or SIGTERM.
async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
