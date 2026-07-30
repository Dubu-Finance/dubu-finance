//! The quote loop.
//!
//! ```text
//! startup   config -> chain facts -> killswitch latch -> key -> feed -> newHeads subscription
//! wake      a newHeads notification, or the fallback timer if heads have gone quiet
//! cycle     read chain (1 request) -> per pair: fair value -> row -> decision -> maybe send
//!           -> mark NAV -> killswitches
//! shutdown  withdraw quotes, then exit
//! ```
//!
//! # What wakes this loop
//!
//! A `newHeads` notification off the Nodit websocket, which arrives at the chain's 1s cadence.
//! Underneath it sits a fallback timer at `chain.fallback_poll_interval_ms`, and the two are in
//! the same `select!` — so the loop cannot stall whatever the subscription does, and no mode
//! switch is needed to move between them. At a healthy head cadence the timer essentially never
//! wins the race; when heads stop it becomes the driver, and the wake reason is in every cycle's
//! log line as `woke_on`.
//!
//! The head watchdog is the interesting half. A subscription that *errors* is easy — the socket
//! closes and the task reconnects with backoff. A subscription that reconnects and then silently
//! delivers nothing is the dangerous one, because the bot would sit believing the chain had
//! stopped. So when no head has arrived for `chain.head_stale_blocks` block times, the loop says
//! so once, loudly, and keeps reading on the timer. It deliberately does **not** declare the
//! chain down on that alone: the *read* answers that question, because
//! [`ChainHealth`] escalates on the block number as well as on request success. A silent socket
//! over a live chain keeps quoting; a silent socket over a frozen chain walks
//! `Degraded -> Down` and withdraws.
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
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use tokio::sync::watch;
use tracing::{error, info, warn};

use alloy_primitives::Address;
use dubu_updater::chain::heads::{self, HeadShared};
use dubu_updater::chain::swaps::SwapWatch;
use dubu_updater::chain::view::{self, SharedView as ViewSlot};
use dubu_updater::chain::{
    ChainFacts, ChainHealth, ChainReader, ChainStatus, ChainView, Rpc, Selection,
};
use dubu_updater::config::{Config, KeySource};
use dubu_updater::fair_value::{self, Reference, VenueQuote};
use dubu_updater::feed::ws::MarketFeed;
use dubu_updater::feed::{
    binance, bybit, coinbase, hyperliquid, okx, FeedStatus, VenueFeeds, VenueId, VenueWatch,
};
use dubu_updater::hedge;
use dubu_updater::jump;
use dubu_updater::ladder::{self, RowInputs};
use dubu_updater::maker;
use dubu_updater::markout::{self, Markout};
use dubu_updater::notify::{self, Notifier};
use dubu_updater::now_unix;
use dubu_updater::policy::{self, CapacityDecision, Context, Decision};
use dubu_updater::quoting;
use dubu_updater::risk::{Halt, KillSwitch, Position};
use dubu_updater::serve::{self, Shared as RfqShared};
use dubu_updater::skew::{self, Inventory, Volatility};
use dubu_updater::spread;
use dubu_updater::tx::{Episode, Fees, Intent, Sender, Sent, Settled, Signer, TxError};
use dubu_updater::units::{self, FEED_SCALE};

/// Emit a trace at `info` on a sampled cycle and at `debug` on the rest.
///
/// The cycle runs at 5-6Hz since it got its own clock, and these lines are per-pair, so leaving
/// them all at `info` turned a readable 13 lines a second into 63 -- five million a day, in which
/// the lines reporting an actual send are outnumbered by the ones reporting that nothing happened.
/// Sampling on `block_work` keeps exactly the resolution the log had before the clock changed, one
/// full trace per sealed block, and `RUST_LOG=debug` still shows every tick.
macro_rules! trace_at {
    ($loud:expr, $($arg:tt)*) => {
        if $loud { tracing::info!($($arg)*) } else { tracing::debug!($($arg)*) }
    };
}

/// How long the exit path waits for the alerting task to get its last message out.
///
/// The events worth waking somebody for — a killswitch trip, every group latched — are all
/// followed within milliseconds by `EXIT_HALTED`, so without a bounded wait here the batch dies
/// inside its window with the process and the operator learns nothing. That is the 2026-07-29
/// shape exactly: the log said everything, the process was gone, and it was 59 minutes before a
/// human noticed. Two seconds, because it is spent only while shutting down and it is shorter
/// than a systemd restart delay.
const NOTIFY_FLUSH: Duration = Duration::from_secs(2);

/// Exit code when the bot stops because a killswitch latched or the chain went away.
const EXIT_HALTED: i32 = 2;
/// Exit code for a startup failure.
const EXIT_STARTUP: i32 = 1;

#[derive(Debug)]
struct Args {
    config: String,
    once: bool,
    force_dry_run: bool,
    cycles: Option<u64>,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        config: "updater.toml".into(),
        once: false,
        force_dry_run: false,
        cycles: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--config" | "-c" => a.config = it.next().ok_or("--config needs a path")?,
            "--once" => a.once = true,
            "--cycles" => {
                let v = it.next().ok_or("--cycles needs a count")?;
                a.cycles = Some(
                    v.parse::<u64>()
                        .map_err(|_| format!("--cycles: `{v}` is not a number"))?,
                );
            }
            // A command-line override that can only ever make the bot safer. There is
            // deliberately no `--transmit` counterpart: broadcasting is a config decision, made
            // in a file that gets reviewed, not a flag someone can add to a command.
            "--dry-run" => a.force_dry_run = true,
            "--help" | "-h" => {
                println!(
                    "dubu-updater [--config <path>] [--once] [--cycles <n>] [--dry-run]\n\n\
                     --config   path to the TOML config (default: updater.toml)\n\
                     --once     run a single evaluation cycle and exit\n\
                     --cycles   run n cycles and exit; wakes on newHeads like a normal run\n\
                     --dry-run  force dry run regardless of config (there is no --transmit)\n"
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    Ok(a)
}

/// Not `#[tokio::main]`, and the reason is the `.env` load.
///
/// `std::env::set_var` mutates process-global state that other threads may be reading, so it is
/// only sound while the process is still single-threaded. `#[tokio::main]` builds a multi-thread
/// runtime *before* the body runs, which would put the dotenv load after the worker threads
/// exist. Building the runtime by hand keeps the ordering explicit: parse, load `.env`, start
/// logging, and only then spin up anything concurrent.
fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("dubu-updater: {e}");
            std::process::exit(EXIT_STARTUP);
        }
    };

    // `.env` before the config, because the config's endpoint URLs are `${VAR}` templates that
    // are expanded during parsing. Real environment variables always win over the file; see
    // `config::load_dotenv`. Values are never logged — only how many were set.
    let mut dotenv_vars = dubu_updater::config::load_dotenv(std::path::Path::new(".env"));
    if let Some(dir) = std::path::Path::new(&args.config).parent() {
        dotenv_vars += dubu_updater::config::load_dotenv(&dir.join(".env"));
    }

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

    if dotenv_vars > 0 {
        info!(target: "startup", event = "dotenv", variables = dotenv_vars,
              "loaded variables from .env (values are never logged)");
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("dubu-updater: cannot start the async runtime: {e}");
            std::process::exit(EXIT_STARTUP);
        }
    };

    match runtime.block_on(run(&args)) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            error!(target: "startup", event = "fatal", error = %e, "cannot start");
            std::process::exit(EXIT_STARTUP);
        }
    }
}

/// Everything resolved at startup, so the cycle never has to ask again.
struct Runtime {
    /// Whether the clock has been compared against the chain yet. See `check_clock_skew`.
    clock_checked: bool,
    /// What the hedge leg's task last published, when there is a leg. See [`HedgeShared`].
    ///
    /// The leg itself is not here any more: it lives in its own task, because its pass is six
    /// external HTTPS round trips and the cycle only ever needed one number out of it.
    hedge: Option<Arc<HedgeShared>>,
    /// The sealed block timestamp the per-block work last ran at.
    ///
    /// The cycle runs at the quote cadence now, several times per block, but some of what it does
    /// is keyed to a block rather than to a quote: reading Swap logs, and stamping a reference for
    /// markout to compare fills against. Both would repeat themselves for every cycle inside the
    /// same block -- one as wasted `eth_getLogs` calls, the other as duplicate reference samples
    /// carrying an identical timestamp.
    last_block_work: u64,
    /// When each pair was last sent, on our own clock.
    ///
    /// The chain cannot answer this below a second -- `updatedAt` is a uint32 of seconds -- so the
    /// cadence trigger measures from here instead. See `policy::Trigger::Cadence`.
    last_push: BTreeMap<u16, Instant>,
    cfg: Config,
    facts: ChainFacts,
    /// Ordinary RPC: transactions, nonce, receipts, startup metadata. Canonical.
    rpc: Rpc,
    /// The read pool, held only to report on it.
    ///
    /// The reader task owns it and the cycle never issues a call through it — but the cycle is
    /// where logging happens, and a pool whose rate limiting cannot be pinned to an endpoint is a
    /// pool nobody can act on. Note this is a *different* pool from [`Self::rpc`]: reads pin to the
    /// keyless public flashblocks endpoint and fall through to Nodit, writes go the other way.
    /// Measuring one and reading it as the other is a mistake this field exists to prevent.
    read_rpc: Arc<Rpc>,
    /// The chain state, published by the reader task rather than fetched here.
    ///
    /// `flash` and `reader` used to live on this struct and the cycle called them itself, which
    /// is what pinned the cycle to the read's cadence. They now belong to
    /// [`dubu_updater::chain::view::run`], and everything on this side reads whatever it last
    /// published. See that module for why up to one interval of staleness costs nothing.
    view: Arc<ViewSlot>,
    /// Every configured venue's feed state. One socket, one reconnect loop and one liveness
    /// state per venue, so one venue failing cannot take another down.
    feeds: Arc<VenueFeeds>,
    /// Edge-triggers per-venue liveness, so that losing a venue is an event rather than one
    /// fewer entry in a cross-section that still looks confident.
    watch: VenueWatch,
    /// State of the `newHeads` subscription that drives the loop.
    heads: Arc<HeadShared>,
    /// Return-variance estimator per pair. Keyed by pair id rather than positionally: a `Vec`
    /// indexed in lock-step with `cfg.pairs` is an invariant nothing enforces, and getting it
    /// wrong sizes one pair's skew off another pair's volatility.
    vol: BTreeMap<u16, Volatility>,
    /// Jump detection and the cool-off state machine, one detector per pair plus the scope rule.
    /// Fed by [`jump_scan`], which runs both inside the cycle and between cycles.
    jump: jump::Book,
    /// Fees for a jump withdrawal, and only for one. See [`dubu_updater::tx::Sender::send_with_fees`].
    withdraw_fees: Fees,
    sender: Sender,
    /// One per correlation group. A group's loss budget is its own; see where these are loaded.
    kills: BTreeMap<String, KillSwitch>,
    /// Shared with the reader task, which is where read successes and failures now come from.
    health: Arc<Mutex<ChainHealth>>,
    /// Follows our own `Swap` logs. Read off the canonical RPC, not the flashblocks one: markout
    /// anchors to block timestamps, so a preconfirmed head buys it nothing, and a fill read out of
    /// a preconfirmation that later reorganises would be a phantom counterparty score.
    swaps: SwapWatch,
    /// Who has been trading against us and how it went. Fed by [`scan_fills`].
    markout: Markout,
    /// The state the RFQ endpoint quotes from, or `None` when the RFQ leg is off. Written once
    /// per pair per cycle; read by whoever is asking for a quote at the time.
    rfq: Option<Arc<RfqShared>>,
    /// True when the RFQ maker and the pool are the same account, so the curve's epoch and an RFQ
    /// order draw on one balance. Normally false — `PmmSettle` pulls from the maker, and the two
    /// are separate balance sheets whose commitments must not be netted against each other.
    rfq_shares_pool_inventory: bool,
    /// Pushes fills and anything that went wrong to Telegram. Disabled and inert when the
    /// credentials are absent; see [`dubu_updater::notify`] for why nothing here may ever wait on
    /// it, and why it is a `Notifier` rather than an `Option<Notifier>` — the disabled state is a
    /// no-op, so no call site has to remember the difference.
    notify: Notifier,
}

/// Why this cycle is running. Logged on every cycle, because "the fallback timer has been the
/// One killswitch per correlation group, each latching to its own file.
///
/// Separate files so an operator clears one group without resuming another.
fn load_kills(cfg: &Config) -> Result<BTreeMap<String, KillSwitch>, Box<dyn std::error::Error>> {
    assert!(
        !cfg.pairs.is_empty(),
        "config validation admits no empty book"
    );

    let mut groups: Vec<String> = cfg.pairs.iter().map(|p| p.jump_group.clone()).collect();
    groups.sort_unstable();
    groups.dedup();
    assert!(!groups.is_empty());
    assert!(groups.len() <= cfg.pairs.len());

    let mut kills = BTreeMap::new();
    for group in &groups {
        let path = kill_state_path(&cfg.risk.state_path, group);
        kills.insert(
            group.clone(),
            KillSwitch::load(
                &path,
                cfg.risk.bleed_window_secs,
                cfg.risk.bleed_limit_units()?,
                cfg.risk.loss_budget_units()?,
                cfg.risk.shadow,
            )?,
        );
    }
    assert_eq!(
        kills.len(),
        groups.len(),
        "one switch per group, no collisions"
    );
    // A disabled killswitch has to be impossible to run without knowing. The mode is deliberately
    // temporary -- it exists to collect the drawdown data the limits cannot be sized without -- and
    // the way it goes wrong is not that it fails, it is that it is left on and forgotten, at which
    // point the book has no drawdown halt and every log line looks normal.
    if cfg.risk.shadow {
        error!(
            target: "risk", event = "shadow_mode", groups = kills.len(),
            bleed_limit = %cfg.risk.bleed_limit, bleed_window_secs = cfg.risk.bleed_window_secs,
            loss_budget = %cfg.risk.loss_budget,
            "SHADOW MODE: the killswitches measure but DO NOT latch. The drawdown halt is off for \
             every group. Trips appear as event=halt_shadow. Set risk.shadow = false to enforce."
        );
    }
    Ok(kills)
}

/// `risk.json` for the unnamed group, `risk-crypto.json` for a named one.
fn kill_state_path(base: &std::path::Path, group: &str) -> std::path::PathBuf {
    if group.is_empty() {
        return base.to_path_buf();
    }
    let stem = base.file_stem().unwrap_or_default();
    let extension = base
        .extension()
        .map_or_else(|| "json".into(), |e| e.to_string_lossy());
    let path = base.with_file_name(format!("{}-{group}.{extension}", stem.to_string_lossy()));
    assert_ne!(path, base, "a named group must not share the default file");
    path
}

/// What each pool reported the head to be, once both agreed the chain is moving.
#[derive(Debug, Clone, Copy)]
struct ProbedHeads {
    /// The head the write pool reported, `cfg.chain.rpc_url`.
    write_block: u64,
    /// The head the read pool reported, `cfg.chain.flashblocks_rpc_url` — the pool that failed on
    /// 2026-07-29, and so the one that actually has to answer before anything is cleared.
    read_block: u64,
}

/// Whether the chain is answering right now on **both** pools. `None` means do not act on it.
///
/// Both, because the pool that broke on 2026-07-29 was the read one. `flash` is pinned to GIWA's
/// free flashblocks endpoint, which answered `-32011 no backend is currently healthy to serve
/// traffic`, while the canonical RPC behind `rpc` stayed healthy throughout. So a probe that only
/// asks `rpc` interrogates the one path that never failed: it would clear the latch while
/// flashblocks was still refusing, the reader task would go on failing, [`ChainHealth`] would walk
/// back to `Down` and re-latch about 600s later, and every pass costs nine nonces re-asserting the
/// withdrawals. Requiring both puts the path that actually broke inside the check.
///
/// Concurrently, so the cost is one gap rather than two and both pools are sampled over the same
/// window — probing them in sequence would compare heads read four seconds apart and read a
/// perfectly normal seal as a disagreement between the pools.
///
/// Worth knowing and deliberately not designed around: on the box today `rpc_url` and
/// `flashblocks_rpc_url` are both `http://127.0.0.1:8545`, so both probes hit the same local node
/// and the stricter check is a no-op there. It stops being one the moment either pool is pointed
/// back at a remote endpoint, which is the configuration this has to be correct for.
async fn chain_is_answering(rpc: &Rpc, flash: &Rpc) -> Option<ProbedHeads> {
    let (write, read) = tokio::join!(probe_head(rpc), probe_head(flash));
    // Both, never either. This pattern is the entire requirement, so it is written once and here.
    match (write, read) {
        (Some(write_block), Some(read_block)) => Some(ProbedHeads {
            write_block,
            read_block,
        }),
        _ => None,
    }
}

/// Two head reads from one pool, a couple of seconds apart. `None` if either read failed or the
/// head went backwards.
///
/// Two rather than one because a single answered read only proves an endpoint is reachable, and the
/// mirror failure — an endpoint cheerfully serving a frozen number over a chain that has stopped —
/// looks identical from one sample. Requiring both also stops a pool that is still mid-outage from
/// passing by getting lucky once.
///
/// At-or-ahead rather than strictly ahead: GIWA seals at roughly 1s but nothing guarantees a seal
/// inside this particular gap, and a chain that is briefly quiet is not a chain that is down. The
/// cost of being wrong in that direction is bounded and self-correcting — [`ChainHealth`] re-decides
/// liveness from the reads the loop is about to make anyway, and latches again within
/// `halt_after_secs`. The cost of being too strict is the outage this exists to end.
///
/// Every failure names its pool, because "flashblocks is still down" and "the canonical RPC is
/// down" call for different things from whoever is reading. [`Rpc::name`] is the name the pool was
/// built with, so the log agrees with what that pool's `RpcError`s already say, and [`Rpc::url`] is
/// the redacted form — there is no accessor for the real one.
///
/// `eth_blockNumber` through the existing [`Rpc::quantity`], so this shares the pool's rate limiter
/// and endpoint failover with every other read rather than reaching around them.
async fn probe_head(rpc: &Rpc) -> Option<u64> {
    let first = match rpc.quantity("eth_blockNumber", serde_json::json!([])).await {
        Ok(n) => n,
        Err(e) => {
            warn!(target: "risk", event = "liveness_probe_failed", pool = rpc.name(),
                  url = %rpc.url(), attempt = 1, error = %e,
                  "this pool did not answer a head read; leaving the latch alone");
            return None;
        }
    };
    tokio::time::sleep(Duration::from_secs(2)).await;
    let second = match rpc.quantity("eth_blockNumber", serde_json::json!([])).await {
        Ok(n) => n,
        Err(e) => {
            warn!(target: "risk", event = "liveness_probe_failed", pool = rpc.name(),
                  url = %rpc.url(), attempt = 2, error = %e, first_block = first,
                  "this pool answered one head read and then did not; leaving the latch alone");
            return None;
        }
    };
    if second < first {
        warn!(target: "risk", event = "liveness_probe_reorg", pool = rpc.name(),
              url = %rpc.url(), first_block = first, second_block = second,
              "this pool's head went backwards between two reads; not a chain to resume onto");
        return None;
    }
    Some(second)
}

/// The jump detector, one threshold per pair and contagion inside a group.
fn build_jump_book(cfg: &Config) -> jump::Book {
    let bounds: Vec<(u16, jump::Bounds)> = cfg
        .pairs
        .iter()
        .map(|p| {
            let floor = p.half_spread_bps.ceil() as u16;
            (
                p.pair_id,
                jump::Bounds::new(p.jump_floor_bps.unwrap_or(floor), floor, p.width_bps),
            )
        })
        .collect();
    assert_eq!(bounds.len(), cfg.pairs.len(), "every pair gets a detector");

    let groups: BTreeMap<u16, String> = cfg
        .pairs
        .iter()
        .filter(|p| !p.jump_group.is_empty())
        .map(|p| (p.pair_id, p.jump_group.clone()))
        .collect();
    assert!(groups.len() <= cfg.pairs.len());

    for (id, b) in &bounds {
        // The ceiling is the absorption limit and the floor is the noise gate. Equal means the
        // clamp has collapsed to a constant, which is how narrowing the ladder once took the whole
        // book dark -- normal volatility then reads as a jump on every pair at once.
        assert!(
            b.ceiling_bps_e2 >= b.floor_bps_e2,
            "pair {id}: ceiling below floor"
        );
        info!(
            target: "jump", event = "bounds", pair_id = id,
            enabled = cfg.jump.enabled, scope = cfg.jump.scope.label(),
            sigma_k = cfg.jump.sigma_k,
            floor_bps_e2 = b.floor_bps_e2, ceiling_bps_e2 = b.ceiling_bps_e2,
            cooloff_secs = cfg.jump.cooloff_secs, scan_interval_ms = cfg.jump.scan_interval_ms,
            "jump trip threshold is clamp(sigma_k * sigma, half_spread, half_spread + width/2)"
        );
    }

    jump::Book::grouped(
        &bounds,
        cfg.jump.params(&cfg.skew),
        cfg.jump.scope,
        cfg.jump.enabled,
        &groups,
    )
}

/// One websocket task per venue a pair actually names.
///
/// A venue is enabled by being used rather than by a switch, so there is no way to configure one
/// that connects and quotes nothing.
fn spawn_feeds(
    cfg: &Config,
    shutdown_rx: &watch::Receiver<bool>,
) -> (Arc<VenueFeeds>, Vec<tokio::task::JoinHandle<()>>) {
    let venues = cfg.venues();
    assert!(
        !venues.is_empty(),
        "config validation admits no venueless book"
    );

    // Paired with the symbols each venue actually carries, so a venue never asked about a symbol is
    // absent from that cross-section rather than reported as having lost it.
    let coverage: Vec<(VenueId, Vec<String>)> = venues
        .iter()
        .map(|&v| {
            let symbols = cfg
                .venue_symbols(v)
                .into_iter()
                .map(|(_, canonical)| canonical)
                .collect();
            (v, symbols)
        })
        .collect();
    assert_eq!(coverage.len(), venues.len());

    let feeds = Arc::new(VenueFeeds::new(
        &coverage,
        Duration::from_millis(cfg.feed.stale_after_ms),
    ));
    let mut tasks = Vec::new();
    for venue in &venues {
        let symbols = cfg.venue_symbols(*venue);
        assert!(
            !symbols.is_empty(),
            "a venue in `venues()` is named by some pair"
        );
        let client: Box<dyn MarketFeed> = match venue {
            VenueId::Binance => Box::new(binance::Client::new(&symbols)),
            VenueId::Okx => Box::new(okx::Client::new(&symbols)),
            VenueId::Bybit => Box::new(bybit::Client::new(&symbols)),
            VenueId::Coinbase => Box::new(coinbase::Client::new(&symbols)),
            VenueId::Hyperliquid => Box::new(hyperliquid::Client::new(&symbols)),
        };
        let Some(shared) = feeds.venue(*venue) else {
            continue;
        };
        info!(
            target: "startup", event = "venue", venue = %venue,
            url = %cfg.feed.urls.get(*venue),
            symbols = %symbols.iter().map(|(v, c)| format!("{v}->{c}")).collect::<Vec<_>>().join(","),
            "market data venue configured"
        );
        tasks.push(tokio::spawn(dubu_updater::feed::ws::run(
            cfg.feed.clone(),
            cfg.feed.urls.get(*venue).to_string(),
            client,
            shared,
            shutdown_rx.clone(),
        )));
    }
    assert_eq!(tasks.len(), venues.len(), "every venue got a task");
    (feeds, tasks)
}

/// only thing waking this loop for an hour" is invisible otherwise.
#[derive(Debug, Clone, Copy)]
enum Wake {
    /// First cycle, before any head could have arrived.
    Startup,
    /// A `newHeads` notification. The normal case.
    Head(u64),
    /// The fallback timer. Normal only while heads are absent.
    Fallback,
    /// The quote clock. The normal case now: the cycle paces itself rather than waiting for a
    /// head, because the posted spread has to cover the reference's drift over the re-quote
    /// interval and a one-second interval was setting the spread.
    Tick,
}

impl Wake {
    const fn label(self) -> &'static str {
        match self {
            Self::Startup => "startup",
            Self::Head(_) => "head",
            Self::Fallback => "fallback_timer",
            Self::Tick => "quote_tick",
        }
    }

    const fn head_number(self) -> Option<u64> {
        match self {
            Self::Head(n) => Some(n),
            _ => None,
        }
    }
}

#[allow(clippy::too_many_lines)]
async fn run(args: &Args) -> Result<i32, Box<dyn std::error::Error>> {
    let mut cfg = Config::load(std::path::Path::new(&args.config))?;
    if args.force_dry_run {
        cfg.tx.transmit_allowed = false;
    }

    // The transmit client is PINNED to one endpoint, and stays pinned however many read
    // endpoints are configured. Nonce, submit and receipt must reach one node's view of the
    // pending set; reading a nonce from a node that has not seen the previous transaction leaves
    // a gap that nothing fills.
    // The write path gets a pool too. A quota belongs to a key, not to a node, and this was the
    // one path still running on a single key -- when it was exhausted the bot could not send at
    // all while the reader, rotating over six, never noticed. `Pin` rather than `Rotate` because
    // consecutive calls here must see the same node's view of our nonce.
    let mut write_urls = vec![cfg.chain.rpc_url.clone()];
    write_urls.extend(cfg.chain.write_rpc_urls.iter().cloned());
    let rpc = Rpc::pooled("rpc", &write_urls, Selection::Pin, &cfg.chain)?;

    // Reads rotate. A read is a question about one block and any node can answer it, so the pool's
    // budget is the sum of its keys rather than the smallest of them.
    let mut read_urls = vec![cfg.chain.flashblocks_rpc_url.clone()];
    read_urls.extend(cfg.chain.read_rpc_urls.iter().cloned());
    // `Pin`, not `Rotate`. Rotating spread the load so "the pool's budget is the sum of its keys
    // rather than the smallest of them" -- which was right when every endpoint was a keyed one with
    // a daily quota. It is not any more: the first endpoint is GIWA's public flashblocks RPC, which
    // has no key and no quota, and measured, it serves 3 req/s at 29/30. Rotating past it spends
    // Nodit quota on requests that a free endpoint would have answered, and today that exhausted
    // six of seven keys -- which took the websocket down with them, and with it the sealed clock.
    //
    // So it stays on the free one and moves to a key only when the free one genuinely fails.
    let flash = Rpc::pooled("flashblocks", &read_urls, Selection::Pin, &cfg.chain)?;
    info!(
        target: "chain", event = "read_pool", endpoints = flash.endpoint_count(),
        transmit = %rpc.url(),
        "read endpoints rotate; the transmit endpoint is pinned"
    );

    // Every check that needs the chain, before the loop is allowed to compute anything.
    let facts =
        dubu_updater::chain::verify_against_chain(&rpc, cfg.chain.pool, cfg.chain.multicall3, &cfg)
            .await?;

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

    // One killswitch per correlation group, not one for the book.
    //
    // The switch trips on a NAV drawdown and latches to disk, and a single instance made SK Hynix's
    // losses ETH's problem -- the same defect the jump detector's contagion had. Groups are the
    // claim about what moves together, so they are what a loss budget should be shared across.
    //
    // Each gets its own state file, so a latched group stays latched across restarts on its own and
    // an operator clears them one at a time.
    let mut kills = load_kills(&cfg)?;

    let mut sender = Sender::new(
        signer,
        cfg.chain.chain_id,
        cfg.chain.pool,
        &cfg.tx,
        cfg.tx.max_fee_wei()?,
        cfg.tx.max_priority_fee_wei()?,
    );

    // Every URL here is an `EndpointUrl`, whose `Display` is redacted — the Nodit API key is a
    // path segment, so this line prints `https://giwa-sepolia.nodit.io/***` and there is no
    // spelling of it that would print the key.
    info!(
        target: "startup",
        event = "configured",
        transmit_allowed = cfg.tx.transmit_allowed,
        pool = %cfg.chain.pool,
        chain_id = cfg.chain.chain_id,
        updater = %facts.updater,
        signing_as = ?sender.address(),
        pairs = cfg.pairs.len(),
        ws_url = %cfg.chain.ws_url,
        rpc_url = %cfg.chain.rpc_url,
        flashblocks_rpc_url = %cfg.chain.flashblocks_rpc_url,
        driver = "newHeads",
        block_time_ms = cfg.chain.block_time_ms,
        head_watchdog_ms = cfg.chain.head_stale_after().as_millis(),
        fallback_poll_interval_ms = cfg.chain.fallback_poll_interval_ms,
        requests_per_sec = cfg.chain.requests_per_sec,
        nav_token = %facts.nav_token,
        venues = %cfg.venues().iter().map(ToString::to_string).collect::<Vec<_>>().join(","),
        venues_min = cfg.feed.venues_min,
        mad_k = cfg.feed.mad_k,
        mad_floor_bps = cfg.feed.mad_floor_bps,
        dispersion_bps_max = cfg.feed.dispersion_bps_max,
        gamma = cfg.skew.gamma,
        vol_horizon_secs = cfg.skew.vol_horizon_secs,
        vol_tau_ms = cfg.skew.vol_tau_ms,
        skew_cap_bps = cfg.skew.positive_bps_max,
        skew_floor_bps = -i32::from(cfg.skew.negative_bps_max),
        spread_vol_coefficient = cfg.spread.vol_coefficient,
        spread_max_half_spread_bps = cfg.spread.half_spread_bps_max,
        jump_enabled = cfg.jump.enabled,
        jump_sigma_k = cfg.jump.sigma_k,
        jump_cooloff_secs = cfg.jump.cooloff_secs,
        jump_scope = cfg.jump.scope.label(),
        jump_scan_interval_ms = cfg.jump.scan_interval_ms,
        "dubu-updater starting"
    );
    if !cfg.tx.transmit_allowed {
        info!(target: "startup", event = "dry_run",
              "DRY RUN: rows will be computed and decisions logged, nothing will be broadcast");
    }

    // Started here rather than beside the other tasks below, because the first thing worth
    // reporting — a group that came up already latched — happens on the next few lines, before
    // there is a feed, a subscription or a cycle. Absent credentials leave it inert; see
    // `notify::Notifier::from_env`, which is the whole of "a missing alerting credential must
    // never be able to stop a live trading system".
    let notify = Notifier::from_env();

    // Stay-down. A restart is the first thing an operator does, and it must not resume a book
    // that a killswitch took down.
    let latched: Vec<&String> = kills
        .iter()
        .filter(|(_, k)| k.is_halted())
        .map(|(g, _)| g)
        .collect();
    if !latched.is_empty() && latched.len() < kills.len() {
        for g in &latched {
            error!(target: "risk", event = "stay_down_group", group = %g,
                   reason = kills[*g].halt_reason().unwrap_or("(unrecorded)"),
                   "this group is latched from a previous run; its pairs will not quote");
            // Pushed as well as logged. This one is a *partial* book — the other groups quote on,
            // so nothing about the process looks wrong from outside, and a group that is silently
            // absent from a running bot is precisely the fourteen-hour failure.
            notify.send(notify::Event::StayDown {
                group: (*g).clone(),
                reason: kills[*g].halt_reason().unwrap_or("(unrecorded)").into(),
                exiting: false,
            });
        }
    }

    // ... except when the only thing holding the book down is a chain outage that has since ended.
    //
    // On 2026-07-29 the public flashblocks RPC began answering `-32011 / HTTP 503`, chain liveness
    // was lost for 615s against the 600s limit, and the loop latched `Halt::Liveness` on every
    // group. The stay-down check below then did precisely what it is for — re-asserted the
    // withdrawals, exited `EXIT_HALTED` — and systemd's `Restart=on-failure` fed the process
    // straight back into it, six times about 19s apart, spending ~9 nonces a pass, until
    // `StartLimitBurst` was exhausted. The bot was then dead for 59 minutes, until a human noticed.
    // A transient ten-minute outage became a permanent one, and nothing about the book was wrong.
    //
    // Latching stays: `KillSwitch::halt` is unchanged and a liveness halt is still recorded and
    // still survives a reload, because at the moment it fires the chain genuinely is gone and the
    // quotes genuinely must come down. What is added is the other half — recovery — and it belongs
    // at startup rather than in the loop, because the process that latched has already exited by
    // the time the chain comes back.
    //
    // Two conditions, both required. Every latched group must be liveness and nothing else: a
    // `Bleed` or a `LossBudget` anywhere in the set is the book losing money, which wants a human
    // before it quotes again, and clearing the liveness groups around it would resume half a book on
    // a question nobody has answered. And both RPC pools must be demonstrably moving now, not merely
    // reachable — see `chain_is_answering` for why asking only the write pool would clear the latch
    // on the strength of the one path that did not fail.
    let liveness_only: Vec<String> = kills
        .iter()
        .filter(|(_, k)| k.is_liveness_only())
        .map(|(g, _)| g.clone())
        .collect();
    if !liveness_only.is_empty() && liveness_only.len() == latched.len() {
        match chain_is_answering(&rpc, &flash).await {
            Some(heads) => {
                for group in &liveness_only {
                    let kill = kills.get_mut(group).expect("named by the scan just above");
                    let reason = kill.halt_reason().unwrap_or("(unrecorded)").to_string();
                    let halted_at = kill.state().halted_at;
                    kill.resume()?;
                    // At `error!` and with its own event, deliberately. Clearing a killswitch
                    // without a human in the loop is exactly the kind of thing that must never be
                    // quiet: whoever reads this log after the next incident has to be able to find
                    // the moment the latch went away and what it said when it was set.
                    error!(
                        target: "risk", event = "liveness_latch_cleared", group = %group,
                        reason = %reason, halted_at = ?halted_at,
                        write_block = heads.write_block, read_block = heads.read_block,
                        state_path = %cfg.risk.state_path.display(),
                        "this group was latched for chain liveness only and both RPC pools are \
                         answering again; clearing the latch and resuming. The cumulative loss \
                         budget is carried over untouched"
                    );
                    assert!(
                        !kill.is_halted(),
                        "a group logged as resumed that is still latched would exit below anyway"
                    );
                    // For the same reason the log line above is at `error!`: a killswitch clearing
                    // itself with no human in the loop is a thing the operator has to be told
                    // happened, not a thing they should have to go and find.
                    notify.send(notify::Event::LatchCleared {
                        group: group.clone(),
                        reason: reason.clone(),
                    });
                }
            }
            // Which pool fell short is in the `liveness_probe_failed` line immediately above this
            // one; this is the decision, and it stays findable on its own.
            None => error!(
                target: "risk", event = "liveness_latch_kept",
                groups = liveness_only.len(),
                "every latched group is chain liveness only, but the chain is not answering on \
                 both pools yet; keeping the latch and exiting rather than resuming into the \
                 same outage"
            ),
        }
    }

    if kills.values().all(KillSwitch::is_halted) {
        let kill = kills.values().next().expect("at least one group");
        error!(
            target: "risk", event = "stay_down",
            reason = kill.halt_reason().unwrap_or("(unrecorded)"),
            halted_at = ?kill.state().halted_at,
            state_path = %cfg.risk.state_path.display(),
            "killswitch is latched from a previous run; re-asserting the withdrawal and exiting. \
             Clear the state file deliberately to resume."
        );
        // The 2026-07-29 outage, said out loud. This branch is where six restarts in a row went,
        // and each of them logged exactly this and then exited into a process nobody was watching.
        notify.send(notify::Event::StayDown {
            group: "all".into(),
            reason: kill.halt_reason().unwrap_or("(unrecorded)").into(),
            exiting: true,
        });
        withdraw_quotes(&cfg, &rpc, &mut sender).await;
        // Before the return, and it is the reason `flush` exists: the process is about to be gone,
        // and a batch still inside its window would go with it.
        notify.flush(NOTIFY_FLUSH).await;
        return Ok(EXIT_HALTED);
    }

    // Seed the nonce once, here, and never on the hot path again.
    //
    // `broadcast_next` used to start at 0, which left the first send after every restart with no
    // floor under it -- the single largest contributor to a historical run of 3,391 failed sends.
    // Reading it once here fixes that, and it is also the only read there is: `eth_getTransactionCount`
    // on the local node is derived from a pool that holds at most 16 of our transactions and silently
    // drops the rest, so at the ~37 in flight this cycle time allows, a per-send read would answer
    // twenty-odd values behind the truth every time. Not fatal if it fails -- an unseeded sequence
    // just means the first reservation reads it instead.
    match sender.seed_nonce(&rpc).await {
        Ok(n) => info!(target: "startup", event = "nonce_seeded", nonce = n,
                       "next nonce read from the node once; the send path tracks it from here"),
        Err(e) => warn!(target: "startup", event = "nonce_seed_failed", error = %e,
                        "could not seed the nonce; the first reservation will read it"),
    }

    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let (feeds, feed_tasks) = spawn_feeds(&cfg, &shutdown_rx);

    // The `newHeads` subscription: what drives the loop. A `watch` channel rather than an `mpsc`
    // because it coalesces — two heads landing during one cycle should produce one more cycle
    // against the newer state, not two, the second of which would compute a ladder for state
    // that is already superseded.
    let heads = Arc::new(HeadShared::new(cfg.chain.head_stale_after()));
    let (head_tx, head_rx) = watch::channel(0_u64);
    let heads_task = tokio::spawn(heads::run(
        cfg.chain.clone(),
        Arc::clone(&heads),
        head_tx,
        shutdown_rx.clone(),
    ));

    // Started before the reader, because the reader's call list depends on whether there is a
    // maker to read a deliverable for.
    let RfqLeg {
        shared: rfq_shared,
        maker: rfq_maker,
    } = start_rfq(&cfg)?;

    // With the RFQ leg on, the batch also carries what the maker can deliver — its balance and its
    // allowance to `PmmSettle`, per token. Folded in here rather than fetched separately so both
    // are answered at the same block as the inventory they are compared against.
    let pair_ids: Vec<u16> = cfg.pairs.iter().map(|p| p.pair_id).collect();
    let reader = match (&rfq_shared, &cfg.rfq) {
        (Some(_), Some(r)) => ChainReader::with_maker(
            cfg.chain.pool,
            cfg.chain.multicall3,
            pair_ids,
            facts.tokens.clone(),
            rfq_maker,
            r.pmm_settle.parse()?,
        ),
        _ => ChainReader::new(
            cfg.chain.pool,
            cfg.chain.multicall3,
            pair_ids,
            facts.tokens.clone(),
        ),
    };
    // The reader publishes our confirmed nonce alongside the state, which is what tells the
    // in-flight gate a send has landed. See `ChainView::sender_nonce`. Without a signing key there
    // is nothing to send and nothing to gate, so the extra request is not made.
    let reader = match sender.address() {
        Some(a) => reader.with_sender(a),
        None => reader,
    };
    let vol: BTreeMap<u16, Volatility> = cfg
        .pairs
        .iter()
        .map(|p| (p.pair_id, Volatility::new(cfg.skew.vol_config())))
        .collect();

    // Both trip bounds come from the pair's OWN configuration — the floor is its half-spread and
    // the ceiling is `half_spread + width/2`, its absorption limit — which is what lets one global
    // `sigma_k` be correct across two instruments whose measured sigmas differ by 3x.
    let jump_book = build_jump_book(&cfg);
    let withdraw_fees = Fees {
        max_fee: cfg.jump.withdraw_max_fee_wei()?,
        max_priority_fee: cfg.jump.withdraw_priority_fee_wei()?,
    };

    let health = Arc::new(Mutex::new(ChainHealth::new(
        Instant::now(),
        cfg.chain.degraded_after_secs,
        cfg.chain.halt_after_secs,
    )));
    let view = Arc::new(ViewSlot::new());

    // The chain read gets its own clock. Until now it happened inside the cycle, which made the
    // cycle run at whatever cadence the read was triggered at -- `newHeads`, one second -- and so
    // made one second the ceiling on how often the pool could re-price. A 96ms round trip does not
    // belong in a 200ms decision loop, so it moves out rather than in.
    // A handle kept back purely to report on it. The reader owns the pool and the cycle never
    // calls it, but the cycle is the only place that logs — and a read pool whose failures cannot
    // be attributed to an endpoint is one nobody can act on.
    let read_rpc = Arc::new(flash);
    tokio::spawn(view::run(
        Arc::new(reader),
        Arc::clone(&read_rpc),
        Arc::clone(&view),
        Arc::clone(&health),
        view::Pacing::default(),
    ));
    let cfg_pool = cfg.chain.pool;
    let mut rt = Runtime {
        clock_checked: false,
        hedge: None,
        last_block_work: 0,
        last_push: BTreeMap::new(),
        cfg,
        facts,
        rpc,
        read_rpc,
        view: Arc::clone(&view),
        feeds,
        watch: VenueWatch::new(),
        heads,
        vol,
        jump: jump_book,
        withdraw_fees,
        sender,
        kills,
        health,
        swaps: SwapWatch::new(cfg_pool),
        markout: Markout::new(),
        rfq_shares_pool_inventory: rfq_maker == cfg_pool,
        rfq: rfq_shared,
        notify,
    };

    wait_for_feed(&rt).await;
    wait_for_first_head(&rt).await;

    // Built after the feeds have had a moment, because the crossing interval is derived from
    // measured sigma and a zero sigma would derive a zero interval -- a hedge that crosses on
    // every fill, which is the behaviour the band exists to prevent. Falling back to the config's
    // horizon figure keeps it conservative rather than reckless when the estimator is still cold.
    let sigma_root_sec = rt
        .vol
        .get(&rt.cfg.pairs.first().map_or(0, |p| p.pair_id))
        .map(|v| v.sigma_millibps())
        .filter(|s| *s > 0)
        .map_or(594, |s| {
            let horizon = f64::from(u32::try_from(rt.cfg.skew.vol_horizon_secs).unwrap_or(300));
            (s as f64 / horizon.sqrt()) as u64
        });
    let mut hedge_leg = start_hedge(&rt.cfg, sigma_root_sec);
    if let Some(leg) = hedge_leg.as_mut() {
        // Probed only if a pair actually routes there, and a failure costs only those pairs.
        //
        // This used to drop the whole leg on any Binance error, which quietly took the paper
        // venue down with it -- so an exchange the equities never touch could leave them unhedged.
        // The venues are independent and the failure has to be too.
        let signed: Vec<u16> = leg
            .routes
            .iter()
            .filter(|(_, (v, _))| *v == dubu_updater::config::HedgeVenue::Binance)
            .map(|(id, _)| *id)
            .collect();
        if signed.is_empty() {
            info!(target: "hedge", event = "venue_skipped",
                  "no pair routes to the signed venue; not probing it");
        } else {
            let probe = match leg.venue.sync_clock().await {
                Ok(offset) => leg.venue.available_usdt().await.map(|u| (offset, u)),
                Err(e) => Err(e),
            };
            match probe {
                Ok((offset, usdt)) => {
                    info!(target: "hedge", event = "venue_ready", offset_ms = offset,
                          available_usdt = usdt, pairs = signed.len(),
                          "venue reachable and the signature works");
                }
                Err(e) => {
                    error!(target: "hedge", event = "venue_unreachable", error = %e,
                           pairs = signed.len(),
                           "those pairs quote unhedged; the other venues are unaffected");
                    for id in &signed {
                        leg.bands.remove(id);
                        leg.routes.remove(id);
                    }
                }
            }
        }
        if leg.bands.is_empty() {
            warn!(target: "hedge", event = "disabled", "no pair has a reachable venue");
            hedge_leg = None;
        }
    }

    // The hedge gets its own task, and the measurement is the entire reason.
    //
    // `run_hedge` was called from `run_cycle` and cost a median 1.373s of a 2.695s cycle -- and after
    // the send phase was batched it was 1.377s of a 1.657s cycle, which is to say all of what was
    // left. Every millisecond of it is six external HTTPS round trips polling a *paper* book, and
    // nothing in the quote path reads a mid from that book synchronously. So more than half the
    // price-setting loop was spent on work that sets no price.
    //
    // It publishes into `HedgeShared` and the cycle reads the slot, which is what `feed::ws::run` and
    // `chain::view::run` already do. Nothing else on this struct is shared with the task: the leg owns
    // its own bands, positions and venue clients outright, and the only thing crossing back is the
    // signed position the skew needs. Pool balances come from the reader's slot, so the task does not
    // need the cycle either -- which is what makes the extraction clean rather than a race.
    let hedge_task = hedge_leg.map(|leg| {
        let shared = Arc::new(HedgeShared::default());
        rt.hedge = Some(Arc::clone(&shared));
        // Only the base token per pair, because that is the only thing the pass reads out of
        // `ChainFacts`. Handing over the whole struct would share far more state than the task needs.
        let bases: BTreeMap<u16, Address> =
            rt.facts.pairs.iter().map(|(id, m)| (*id, m.base)).collect();
        tokio::spawn(run_hedge(
            leg,
            Arc::clone(&view),
            bases,
            shared,
            hedge_interval(&rt.cfg),
            shutdown_rx.clone(),
        ))
    });

    let limit = if args.once { Some(1) } else { args.cycles };
    let code = quote_loop(&mut rt, limit, head_rx, shutdown_rx).await;

    let _ = shutdown_tx.send(true);
    for task in feed_tasks {
        let _ = tokio::time::timeout(Duration::from_secs(3), task).await;
    }
    let _ = tokio::time::timeout(Duration::from_secs(3), heads_task).await;
    // Bounded like the feeds, and awaited for a stronger reason than they are: this is the only task
    // that can have a real order in flight. Dropping it mid-`venue.market()` leaves a crossing whose
    // fate nobody knows, where three seconds is usually enough for the venue to come back and let it
    // log what filled.
    if let Some(task) = hedge_task {
        let _ = tokio::time::timeout(Duration::from_secs(3), task).await;
    }
    // Every path out of the loop lands here — a killswitch, a signal, `--cycles` — and each of
    // them has just logged the reason it is leaving. Bounded, and best-effort in every direction:
    // this must not be able to delay a shutdown that is already under way.
    rt.notify.flush(NOTIFY_FLUSH).await;
    Ok(code)
}

/// Give the sockets a chance to reach quorum before the first cycle, so the opening log line is a
/// quote rather than "no quorum".
///
/// Waits for **quorum**, not for every venue. Blocking on the slowest venue would make the bot's
/// startup as slow as its worst endpoint, and the whole design is that a missing venue is a
/// degradation rather than an outage.
async fn wait_for_feed(rt: &Runtime) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let now = Instant::now();
        // Each pair against ITS OWN quorum, not against the global one. The equity pairs run at a
        // quorum of one because a second honest source does not exist (see `PairConfig::venues_min`),
        // and measuring them against `feed.venues_min` would leave this probe permanently unsatisfied
        // -- every boot would burn the full deadline and log "not ready" while the pool was fine.
        let short = rt
            .cfg
            .pairs
            .iter()
            .filter_map(|p| {
                let live = rt
                    .feeds
                    .snapshots(&p.symbol, now)
                    .iter()
                    .filter(|(_, s)| s.status.is_live())
                    .count();
                let need = usize::from(p.venues_min.unwrap_or(rt.cfg.feed.venues_min));
                (live < need).then_some((p.symbol.as_str(), live, need))
            })
            .next();
        let Some((symbol, live, need)) = short else {
            info!(target: "feed", event = "ready", pairs = rt.cfg.pairs.len(),
                  "every configured symbol has quorum");
            return;
        };
        if Instant::now() >= deadline {
            warn!(target: "feed", event = "not_ready", symbol = %symbol,
                  venues_live = live, venues_min = need,
                  "starting the loop without quorum on every symbol; pushes will be gated until it arrives");
            return;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Compare this machine's clock to the chain's, and say so loudly if they disagree.
///
/// The RFQ expiry is a contract between three clocks that nothing synchronises: the maker stamps
/// `expiry` from *here*, the aggregator refuses anything with less than a second left using
/// *its* clock, and the settler enforces it against `block.timestamp`. The budget between them is
/// `ttl_secs` minus the aggregator's headroom minus a block -- with a 2s TTL that is zero, and any
/// skew at all is spent before the order exists.
///
/// It happened: this machine ran two seconds slow, so every signed order arrived already expired
/// and the aggregator reported `rejected: expired` -- which is true, and says nothing about a
/// clock. The maker looked healthy, the tunnel answered, the price was right, and the RFQ leg
/// simply never filled. Two seconds of NTP drift, invisible from every log we had.
fn check_clock_skew(rt: &Runtime, snap: &heads::HeadSnapshot) {
    let Some(head) = snap.last else {
        return;
    };
    let local = now_unix();
    let skew =
        i64::try_from(local).unwrap_or(i64::MAX) - i64::try_from(head.timestamp).unwrap_or(0);

    // A sealed head is a second or so old by construction, so the honest expectation is `local`
    // slightly AHEAD of it. Behind, or far ahead, is the machine's clock being wrong.
    if (-1..=3).contains(&skew) {
        info!(target: "startup", event = "clock", local, chain = head.timestamp, skew_secs = skew,
              "clock agrees with the chain");
        return;
    }
    warn!(
        target: "startup", event = "clock_skew", local, chain = head.timestamp, skew_secs = skew,
        rfq_ttl_secs = rt.cfg.rfq.as_ref().map(|r| r.ttl_secs),
        "THIS MACHINE'S CLOCK DISAGREES WITH THE CHAIN. A signed RFQ order carries an expiry \
         stamped from here and validated elsewhere, so a skew of a second or more is spent before \
         the order exists and every quote is refused as expired. Enable automatic time \
         synchronisation on this host"
    );
}

/// Give the subscription a moment to establish before the first cycle, so the opening cycle is
/// driven by a real head rather than by the fallback timer.
///
/// Bounded, and **not** fatal on timeout. A bot that cannot subscribe must still start and still
/// quote — the fallback timer is exactly the mechanism for that, and refusing to start would
/// turn a degraded mode into an outage.
async fn wait_for_first_head(rt: &Runtime) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let snap = rt.heads.snapshot(Instant::now());
        if snap.status.is_live() {
            info!(target: "heads", event = "ready", head = ?snap.last.map(|h| h.number),
                  "newHeads is delivering; the loop will wake on heads");
            return;
        }
        if Instant::now() >= deadline {
            warn!(target: "heads", event = "not_ready", status = snap.status.label(),
                  fallback_poll_interval_ms = rt.cfg.chain.fallback_poll_interval_ms,
                  "no head yet; starting on the fallback timer and continuing to reconnect");
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The quote loop, woken by `newHeads` with the fallback timer underneath it.
///
/// `limit` caps the number of cycles (`--once` is `Some(1)`); `None` runs until a signal.
async fn quote_loop(
    rt: &mut Runtime,
    limit: Option<u64>,
    mut head_rx: watch::Receiver<u64>,
    mut shutdown: watch::Receiver<bool>,
) -> i32 {
    let mut last_view: Option<ChainView> = None;
    let fallback = Duration::from_millis(rt.cfg.chain.fallback_poll_interval_ms);
    // The cycle's own clock. Heads still wake it -- they just no longer pace it.
    let quote_every = Duration::from_millis(rt.cfg.chain.quote_interval_ms);
    let jump_scan_every = rt.cfg.jump.scan_interval();
    let mut halted = false;
    let mut wake = Wake::Startup;
    // Edge-triggered, so a sustained outage logs once rather than once per cycle. A watchdog
    // that repeats itself every two seconds is a watchdog nobody reads.
    let mut watchdog_open = false;
    let mut cycles: u64 = 0;

    // Mark whatever head arrived during startup as seen. Without this the first cycle runs as
    // `Startup` and is immediately followed by a duplicate for the head it already covered —
    // harmless, but it burns a request and puts two cycles at the same block in the log.
    head_rx.borrow_and_update();

    // Registered ONCE and polled by reference, rather than rebuilt on every pass of the wait
    // loop. That loop now runs at the jump scan interval rather than the block time, and a
    // freshly-created `Signal` only receives what arrives after it exists — so rebuilding it five
    // times a second would be five times as many windows in which a SIGTERM lands on nobody.
    let signal = wait_for_signal();
    tokio::pin!(signal);

    'outer: loop {
        let cycle_start = Instant::now();
        cycles += 1;

        // --- the head watchdog -------------------------------------------------------------
        //
        // This reports that heads stopped. It does NOT conclude that the chain stopped — the
        // read below answers that, because `ChainHealth` escalates on the block number as well
        // as on request success. A silent socket over a live chain must keep quoting; a silent
        // socket over a frozen chain must walk Degraded -> Down and withdraw. Deciding here
        // would get one of those two wrong.
        let head = rt.heads.snapshot(cycle_start);
        if head.status.is_live() {
            if watchdog_open {
                info!(target: "heads", event = "watchdog_clear", head = ?head.last.map(|h| h.number),
                      "heads are arriving again; the subscription is driving the loop once more");
                watchdog_open = false;
            }
        } else if !watchdog_open {
            watchdog_open = true;
            warn!(
                target: "heads", event = "watchdog", status = head.status.label(),
                head_age_ms = ?head.age_ms, last_head = ?head.last.map(|h| h.number),
                watchdog_ms = rt.heads.stale_after().as_millis(),
                reconnects = head.reconnects, subscriptions = head.subscriptions,
                fallback_poll_interval_ms = rt.cfg.chain.fallback_poll_interval_ms,
                "NO HEADS: the subscription has stopped delivering. Falling back to polling. \
                 Whether the CHAIN is down is decided by the block number the next read returns, \
                 not by this"
            );
        }

        // --- the read, which already happened ---------------------------------------------
        //
        // Whatever the reader task last published. Up to one poll interval old, and see
        // `chain::view` for why that costs nothing: every externally-driven change in here is a
        // swap, and a swap only reduces what is exposed.
        if let Some(v) = rt.view.latest() {
            // Before anything reads the gate. Everything below this nonce is on chain, so it must
            // stop occupying a slot now rather than whenever a receipt call gets a turn.
            if let Some(n) = v.sender_nonce {
                rt.sender.observe_landed(n);
            }
            last_view = Some(v);
        }

        let now = Instant::now();
        let (status, stall, best_block, last_error) = {
            let h = rt
                .health
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (
                h.status(now),
                h.stall(now),
                h.best_block(),
                h.last_error().unwrap_or("(none)").to_string(),
            )
        };
        if let ChainStatus::Down { stale_secs } = status {
            let halt = Halt::Liveness {
                reason: format!(
                    "chain liveness lost for {stale_secs}s (limit {}s); stalled signal: {}; \
                     best block {}; heads: {}; last error: {}",
                    rt.cfg.chain.halt_after_secs,
                    stall.label(),
                    best_block,
                    head.status.label(),
                    last_error
                ),
            };
            error!(target: "risk", event = "halt", switch = halt.label(), reason = %halt,
                   stall = stall.label(), best_block,
                   heads = head.status.label(),
                   "chain liveness is gone; halting and withdrawing quotes");
            // One message for the whole book rather than one per group, because chain liveness is
            // not a per-group fact and the groups would all carry the identical reason string.
            rt.notify.send(notify::Event::Halt {
                group: "all".into(),
                switch: halt.label(),
                reason: halt.to_string(),
            });
            // Chain liveness is not a per-group fact, so every group latches.
            for k in rt.kills.values_mut() {
                let _ = k.halt(&halt, now_unix());
            }
            halted = true;
        }

        if !halted {
            if let Some(view) = &last_view {
                halted = run_cycle(rt, view, status, wake, &head, cycles).await;
            }
        }

        if halted {
            withdraw_quotes(&rt.cfg, &rt.rpc, &mut rt.sender).await;
            return EXIT_HALTED;
        }

        if limit.is_some_and(|n| cycles >= n) {
            info!(target: "loop", event = "cycles_complete", cycles,
                  "requested cycle count reached; shutting down");
            withdraw_quotes(&rt.cfg, &rt.rpc, &mut rt.sender).await;
            return 0;
        }

        // --- the wake, with the jump fast lane underneath it --------------------------------
        //
        // A head and the fallback timer race in one `select!`. There is no mode flag and no
        // switch between them: when the subscription is healthy the head always wins, and when
        // it is not the timer does. That is the whole fallback mechanism, and it cannot get
        // stuck in the wrong mode because there is no mode.
        //
        // The third arm is the FAST LANE, and it is the one place in this bot where latency
        // genuinely matters. Waiting for the next head would put mean jump-detection latency at
        // half a block; this fires every `jump.scan_interval_ms` in the same task, reads only the
        // in-memory feed snapshots, and costs zero RPC unless something actually trips. It does
        // NOT count as a wake: after scanning it goes straight back to waiting, so a scan never
        // consumes a chain read or a cycle.
        // Whichever comes first: the quote clock, or the fallback that bounds how long a cycle can
        // go without one when heads are down.
        //
        // As configured the fallback cannot win -- not "never wins", cannot. `quote_interval_ms` is
        // 200 while config validation floors `fallback_poll_interval_ms` at 250, so `deadline` is
        // always `tick_at` and `Wake::Fallback` is unreachable. Worth stating rather than leaving as
        // a coincidence: the fallback stopped being a bound the moment the quote clock went below it,
        // and `Wake::Fallback` appearing in a log would today be impossible rather than merely rare.
        //
        // The quote clock is not a floor yet either. It binds only when a cycle finishes inside it,
        // and the measured cycle is 1.657s falling to roughly 0.30s once the hedge poll moves off --
        // still above 200ms, because one `eth_sendRawTransaction` to the Korean sequencer is 264ms on
        // its own and the phase cannot beat one round trip. Raising `quote_interval_ms` to about 330,
        // which is the measured flashblock time and already what `push_interval_ms_max` is set to,
        // would make this deadline genuinely engage. That is a config decision, not a code one.
        let tick_at = cycle_start + quote_every;
        let deadline = tick_at.min(cycle_start + fallback);
        wake = 'wait: loop {
            // Shutdown and SIGTERM are polled BEFORE the deadline test, and that ordering is the fix
            // rather than a tidy-up. They used to be reachable only through the `select!` further
            // down, which a cycle longer than `quote_interval_ms` never reaches: the deadline is
            // already past on entry, so the loop breaks on its first test. Against a 200ms interval
            // that has been every pass of every cycle this bot has ever run -- so `withdraw_quotes`
            // on shutdown has never once executed in production, a SIGTERM was simply not observed
            // by this task, and the between-cycle `jump_scan` below has never fired either. The 0.30s
            // cycle does not fix it on its own, because 0.30s is still past a 200ms deadline.
            //
            // Above the test, the check is unconditional: the body cannot be skipped without making
            // it. The always-ready third arm keeps it non-blocking and `biased` is what guarantees
            // the other two are polled first.
            tokio::select! {
                biased;
                _ = shutdown.changed() => break 'outer,
                () = &mut signal => break 'outer,
                () = std::future::ready(()) => {}
            }

            let now = Instant::now();
            if now >= deadline {
                break if deadline == tick_at {
                    Wake::Tick
                } else {
                    Wake::Fallback
                };
            }
            let next_scan = (now + jump_scan_every).min(deadline);
            tokio::select! {
                biased;
                _ = shutdown.changed() => break 'outer,
                () = &mut signal => break 'outer,
                r = head_rx.changed() => match r {
                    Ok(()) => break 'wait Wake::Head(*head_rx.borrow_and_update()),
                    // The subscription task is gone for good. Not fatal: the timer is now the only
                    // driver, which is the case this loop is built to survive. Fall through to the
                    // scan/deadline loop rather than sleeping the whole interval, so the fast lane
                    // survives the loss of the subscription too — it is the defence that must not
                    // depend on the chain telling us anything.
                    Err(_) => tokio::time::sleep(next_scan.saturating_duration_since(Instant::now())).await,
                },
                () = tokio::time::sleep_until(next_scan.into()) => {}
            }
            jump_scan(rt).await;
        };
    }

    info!(target: "loop", event = "shutdown", "shutdown signal received; withdrawing quotes");
    withdraw_quotes(&rt.cfg, &rt.rpc, &mut rt.sender).await;
    0
}

/// **The fast lane.** Sample the reference on every pair, decide whether it jumped, and withdraw
/// immediately if it did.
///
/// This is deliberately the shortest path in the crate, and everything that makes it short is a
/// decision rather than an accident:
///
/// * **No chain read.** It works entirely off the in-memory feed snapshots the venue sockets are
///   already writing. Waiting for the multicall would add a round trip to a race.
/// * **No policy.** [`policy::evaluate_capacity`]'s gates exist to stop the bot *acting* on a
///   state it does not understand; a withdrawal is the one action that is correct in every such
///   state, and `PushInFlight` in particular must not delay it. A pair with an unconfirmed
///   `updateQuote` is exactly a pair that needs withdrawing, and the two calls touch different
///   storage words — even if the quote lands after the withdrawal, the capacity is still zero and
///   the pool still quotes nothing.
/// * **Only on the edge.** A pair already withdrawn has its cool-off re-armed by the detector and
///   sends nothing: `refreshCapacity(pair, 0, 0)` against a pair already at zero buys nothing and
///   burns a nonce.
/// * **At a raised priority fee.** See [`dubu_updater::tx::Sender::send_with_fees`]; the sequencer
///   orders by fee and the counterparty outbids quote traffic by construction.
///
/// The one thing it cannot skip is the nonce. In steady state it is cached from the last quote
/// push and the withdrawal is a single `eth_sendRawTransaction`; after a resync it costs one extra
/// round trip. The withdrawal also inherits the nonce queue, so an earlier transaction that never
/// lands delays it — the mitigation for that would be a second signing key, which does not exist.
async fn jump_scan(rt: &mut Runtime) {
    if !rt.jump.enabled() {
        return;
    }
    let now = Instant::now();
    let mut owed: Vec<u16> = Vec::new();

    for i in 0..rt.cfg.pairs.len() {
        let (pair_id, symbol) = (rt.cfg.pairs[i].pair_id, rt.cfg.pairs[i].symbol.clone());
        let quorum = rt.cfg.pairs[i].venues_min;
        let snaps = rt.feeds.snapshots(&symbol, now);
        let mut quotes: Vec<VenueQuote> = Vec::new();
        for (venue, s) in &snaps {
            let Some(tick) = s.live() else { continue };
            if let Ok(q) = VenueQuote::new(*venue, tick, s.age_ms.unwrap_or(0)) {
                quotes.push(q);
            }
        }
        // No reference means no observation. The detector's own anchor then ages, and a gap past
        // `vol_max_sample_ms` trips it as `feed_gap` on recovery — which is the right answer,
        // because the pool spent the outage armed behind a fixed ladder.
        let Ok(reference) = fair_value::combine(&quotes, &rt.cfg.feed.mad_params_with(quorum))
        else {
            continue;
        };

        let Some(vol) = rt.vol.get(&pair_id) else {
            continue;
        };
        let Some(obs) = rt.jump.observe(pair_id, reference.micro, now, vol) else {
            continue;
        };

        if let Some(reason) = obs.tripped {
            let level = if obs.edge {
                "JUMP"
            } else {
                "jump (already withdrawn)"
            };
            warn!(
                target: "jump", event = "trip", pair_id, symbol = %symbol,
                reason = reason.label(), edge = obs.edge,
                move_bps_e2 = obs.move_bps_e2, threshold_bps_e2 = obs.threshold_bps_e2,
                bound = obs.bound.label(), sigma_bps_e2 = obs.sigma_bps_e2, dt_ms = obs.dt_ms,
                range_bps_e2 = obs.range_bps_e2,
                micro = %units::format_fixed(reference.micro, FEED_SCALE),
                trips = rt.jump.detector(pair_id).map_or(0, jump::Detector::trips),
                cooloff_secs = rt.cfg.jump.cooloff_secs,
                "{level}: the reference moved further than the ladder absorbs; withdrawing quotes"
            );
            if obs.edge {
                owed.push(pair_id);
                // A jump on one pair is information about the other. See `jump`'s module docs for
                // why the asymmetry decides this: contagion's false positive is one cool-off of
                // foregone spread, its false negative is the full pick-off on the other pair.
                for id in rt.jump.contagion(pair_id, now) {
                    warn!(target: "jump", event = "contagion", pair_id = id, origin = pair_id,
                          scope = rt.jump.scope().label(),
                          "withdrawing this pair too: a jump on the other pair is information about it");
                    owed.push(id);
                }
            }
        } else if obs.resumed {
            info!(
                target: "jump", event = "resume", pair_id, symbol = %symbol,
                range_bps_e2 = obs.range_bps_e2, threshold_bps_e2 = obs.threshold_bps_e2,
                trips = rt.jump.detector(pair_id).map_or(0, jump::Detector::trips),
                "cool-off over: the reference has settled, capacity will be restored"
            );
        }
    }

    for pair_id in owed {
        withdraw_pair(rt, pair_id, "jump").await;
    }
}

/// Post a zero capacity epoch on one pair, at the withdrawal fees.
async fn withdraw_pair(rt: &mut Runtime, pair_id: u16, why: &str) {
    // The RFQ leg goes dark first, and synchronously.
    //
    // `refreshCapacity(pair, 0, 0)` stops the curve, and until this existed it stopped nothing
    // else: signed orders stayed fillable at pre-jump prices for their whole TTL, so the jump
    // defence had a hole exactly the width of that TTL — against the single largest loss the flow
    // simulator found. Retiring here is immediate and needs no transaction, because an order that
    // was never signed cannot be filled.
    //
    // What it does not do is cancel orders already signed. Those are out in the world until they
    // expire; `PmmSettle.cancelNonces` could reach them but costs a transaction and a block, which
    // is the same race the withdrawal already loses. Keeping the TTL short is the real control,
    // and `quoting` prices it.
    if let Some(rfq) = &rt.rfq {
        rfq.retire(pair_id);
    }
    let intent = Intent::RefreshCapacity {
        pair_id,
        bid: 0,
        ask: 0,
    };
    let started = Instant::now();
    match rt
        .sender
        .send_with_fees(&rt.rpc, intent, Some(rt.withdraw_fees))
        .await
    {
        Ok(Sent::DryRun {
            calldata,
            would_be_nonce,
            ..
        }) => info!(
            target: "tx", event = "withdraw_dry_run", pair_id, why,
            calldata = %dubu_updater::chain::hex0x(&calldata),
            would_be_nonce = ?would_be_nonce,
            max_priority_fee_wei = %rt.withdraw_fees.max_priority_fee,
            elapsed_us = started.elapsed().as_micros(),
            "DRY RUN: would withdraw quotes by posting a zero capacity epoch"
        ),
        Ok(Sent::Broadcast { hash, nonce }) => warn!(
            target: "tx", event = "withdrawn", pair_id, why, tx = %hash, nonce,
            max_priority_fee_wei = %rt.withdraw_fees.max_priority_fee,
            elapsed_ms = started.elapsed().as_millis(),
            "quotes withdrawn: capacity epoch set to zero"
        ),
        Err(e) => {
            error!(
                target: "tx", event = "withdraw_failed", pair_id, why, error = %e,
                "COULD NOT WITHDRAW QUOTES; the next cycle will re-assert it"
            );
            // The same coalescing as the quote path, and it matters more here: this is the fast
            // lane's withdrawal, which the next cycle re-asserts, so a persistent send failure
            // reproduces itself every ~200ms against a pool that is still armed into a jump.
            rt.notify.send(notify::Event::SendFailed {
                pair_id: Some(pair_id),
                kind: "withdraw",
                error: e.to_string(),
            });
        }
    }
}

/// The RFQ leg, once started: the state the endpoint quotes from, and the address it signs as.
///
/// The address is returned even though `Shared` does not carry it, because the chain reader needs
/// it to ask what that account can deliver — and asking the *pool* instead is the bug this pair
/// exists to make impossible to write again.
struct RfqLeg {
    shared: Option<Arc<RfqShared>>,
    maker: Address,
}

/// Starts the RFQ maker endpoint, if one is configured.
///
/// `Ok(None)` is the ordinary case and means the leg is off: the aggregator routes AMM-only, which
/// is a worse quote and a working system. Anything present but wrong is fatal here rather than
/// degraded, because every failure mode of a misconfigured maker is *silent* from its own side —
/// a wrong `pmm_settle` signs orders against a domain nothing verifies, and the operator sees a
/// healthy process answering requests that no taker can ever fill.
///
/// The key is loaded before the listener binds so a bad key fails at startup rather than at the
/// first request.
fn start_rfq(cfg: &Config) -> Result<RfqLeg, Box<dyn std::error::Error>> {
    let Some(rfq) = cfg.rfq.clone() else {
        info!(target: "rfq", event = "disabled",
              "no [rfq] section; the RFQ leg is off and routing is AMM-only");
        return Ok(RfqLeg {
            shared: None,
            maker: Address::ZERO,
        });
    };

    let pmm_settle: Address = rfq.pmm_settle.parse()?;
    let key = Arc::new(maker::MakerKey::load(
        &rfq.key_source()?,
        cfg.chain.chain_id,
        pmm_settle,
    )?);
    let maker_address = key.address();
    let shared = Arc::new(RfqShared::new());

    // Both are logged, and both are worth checking against the chain before trusting a quote: the
    // maker address is what `PmmSettle` must have an allowance from, and the separator is what a
    // taker's own verification recomputes. A mismatch in either produces unfillable quotes and no
    // error on this side.
    let params = rfq.params(cfg.skew.vol_horizon_secs)?;
    info!(
        target: "rfq", event = "enabled", maker = %key.address(), pmm_settle = %pmm_settle,
        chain_id = cfg.chain.chain_id,
        domain_separator = %format!("{:#x}", key.domain_separator()),
        base_half_spread_e2 = rfq.base_half_spread_e2, ttl_secs = rfq.ttl_secs,
        sigma_horizon_secs = params.sigma_horizon_secs,
        // What the TTL costs at a representative volatility, so the trade-off is visible at
        // startup rather than inferred from quotes later.
        ttl_premium_e2_at_20bp = params.ttl_premium_e2(20_000),
        "RFQ maker enabled; verify the maker address holds the PmmSettle allowance and that the \
         domain separator matches PmmSettle.DOMAIN_SEPARATOR()"
    );

    let serve_shared = Arc::clone(&shared);
    let chain_id = cfg.chain.chain_id;
    tokio::spawn(async move {
        if let Err(e) =
            serve::run(&rfq.serve, serve_shared, key, params, chain_id, pmm_settle).await
        {
            // Not fatal to the quote cycle. The pool keeps quoting its curve either way, and
            // taking the ladder down because an HTTP listener died would turn a lost venue into
            // no venue.
            error!(target: "rfq", event = "serve_failed", error = %e,
                   "THE RFQ ENDPOINT IS DOWN; the curve keeps quoting and routing falls back to AMM-only");
        }
    });

    Ok(RfqLeg {
        shared: Some(shared),
        maker: maker_address,
    })
}

/// The venue client, the per-pair bands, and the mapping between pool units and venue units.
struct HedgeLeg {
    /// Net position on the venue per pair, in the pool's base units and signed: negative is short.
    ///
    /// Tracked here rather than polled, because it has to include orders that are in flight. A
    /// hedge that has been sent has already committed the exposure, and skew that only counts
    /// settled fills double-counts the inventory for as long as the venue takes to answer.
    positions: BTreeMap<u16, i128>,
    venue: hedge::binance::Venue,
    /// The read-only leg, for pairs Binance does not carry. `None` until a pair asks for it.
    paper: Option<hedge::hyperliquid::Paper>,
    /// Which venue each pair hedges on, and the HIP-3 book when it is the paper one.
    routes: BTreeMap<u16, (dubu_updater::config::HedgeVenue, String)>,
    /// Every distinct HIP-3 book in use, so mids are polled once per book rather than per pair.
    dexs: Vec<String>,
    /// Binance spot symbols whose paper fills price against the real book.
    spot: Vec<String>,
    bands: BTreeMap<u16, hedge::Bands>,
    symbols: BTreeMap<u16, String>,
    /// Base-unit divisor per pair, so a `u128` of pool units becomes the decimal the venue wants.
    scale: BTreeMap<u16, f64>,
    qty_decimals: BTreeMap<u16, u32>,
    resync: Duration,
    last_resync: Instant,
}

/// The hedge leg's net position per pair, published for the cycle to read.
///
/// The cycle needs exactly one number out of the hedge — the signed venue position, which the
/// inventory skew values at the same reference as the pool's own balance — and it needs it without
/// awaiting anything. Everything else the leg does is external I/O: six HTTPS round trips that were
/// measured at 1.377s of a 1.657s cycle, polling a *paper* book nothing in the quote path reads
/// synchronously. So the leg moved to its own task and left this slot behind, which is the same split
/// `chain::view::SharedView` already makes for the chain read.
///
/// What the cycle gives up is a position book up to one hedge interval old, and it costs nothing here.
/// This number only moves when the hedge crosses, and `hedge::Bands` will not cross twice inside its
/// cool-off — 2s by default, against a pass interval derived to be half of it. So the worst case is a
/// skew computed one crossing behind, on a control that `chain::view` already documents as slow.
#[derive(Debug, Default)]
struct HedgeShared {
    /// Net position per pair, in the pool's base units and signed: negative is short.
    positions: RwLock<BTreeMap<u16, i128>>,
}

impl HedgeShared {
    /// This pair's net venue position, or `None` when the leg has not reported one.
    ///
    /// `None` rather than zero, because the two say different things to the skew: no position is an
    /// exposure of zero, no *report* is an unknown, and treating an unknown as flat would skew against
    /// a hedge that may well exist. `run_cycle` already distinguishes them, and this preserves that.
    ///
    /// Copied out from under the guard rather than borrowed through it, so a cycle cannot hold the
    /// lock across the rest of its work and stall the pass that writes it.
    fn position(&self, pair_id: u16) -> Option<i128> {
        self.positions.read().ok()?.get(&pair_id).copied()
    }

    /// Replace the published book. Called once per pass, after the pass has settled its crossings.
    fn publish(&self, positions: &BTreeMap<u16, i128>) {
        if let Ok(mut g) = self.positions.write() {
            *g = positions.clone();
        }
    }
}

/// How often the hedge task takes a pass.
///
/// Derived from the shortest configured cool-off rather than chosen. A pass observes the pool balance
/// and may cross; `hedge::Bands` refuses a second crossing inside `cooloff_ms`, so observing faster
/// than that cannot produce another order — it can only spend the venue's rate budget. Half the
/// shortest cool-off, so a band that becomes crossable is acted on within one cool-off rather than
/// after two.
///
/// Floored at 500ms, because the budget does not care why we are asking: `/api/v3/depth?limit=100` is
/// weight 5, so five symbols at two passes a second is 3,000 a minute against Binance's documented
/// 6,000-per-minute IP budget. Half the budget, for the one leg here that sends no orders at all.
fn hedge_interval(cfg: &Config) -> Duration {
    /// Never faster than this, whatever the cool-offs say.
    const FLOOR: Duration = Duration::from_millis(500);
    let shortest = cfg
        .hedge
        .as_ref()
        .and_then(|hc| hc.pairs.iter().map(|p| p.cooloff_ms).min())
        .unwrap_or(2_000);
    Duration::from_millis(shortest / 2).max(FLOOR)
}

/// Build the hedge leg, or explain why there isn't one.
///
/// Never fatal. A pool that cannot reach its hedge venue must still quote -- it just has to quote
/// as though it had no hedge, which is the configuration it was already safe under. Refusing to
/// start would turn a degraded mode into an outage, the same call `wait_for_first_head` makes.
fn start_hedge(cfg: &Config, sigma_millibps_per_sqrt_sec: u64) -> Option<HedgeLeg> {
    let hc = cfg.hedge.as_ref()?;
    let creds = match hedge::binance::Credentials::from_env(&hc.key_env, &hc.secret_env) {
        Ok(c) => c,
        Err(e) => {
            warn!(target: "hedge", event = "disabled", error = %e,
                  key_env = %hc.key_env, secret_env = %hc.secret_env,
                  "hedge configured but no credentials; quoting unhedged");
            return None;
        }
    };
    let venue = match hedge::binance::Venue::new(
        hc.base_url.clone(),
        creds,
        Duration::from_millis(hc.timeout_ms),
    ) {
        Ok(v) => v,
        Err(e) => {
            warn!(target: "hedge", event = "disabled", error = %e, "cannot build venue client");
            return None;
        }
    };

    // The band WIDTH is derived, not configured: `hedge::derive_band` sizes it from the pair's carry
    // budget, because below `(fee/sigma)^2` a crossing spends more on fees than the exposure it clears
    // was worth. What is configured per pair is the cool-off, and that is also what paces the hedge
    // task -- see `hedge_interval`.
    //
    // This comment used to cite `hedge::derive_hedge_interval`, which has never existed in this crate.
    let mut bands = BTreeMap::new();
    let mut symbols = BTreeMap::new();
    let mut routes = BTreeMap::new();
    let mut dexs: Vec<String> = Vec::new();
    let mut spot: Vec<String> = Vec::new();
    let mut scale = BTreeMap::new();
    let mut qty_decimals = BTreeMap::new();
    for hp in &hc.pairs {
        let Some(pair) = cfg.pairs.iter().find(|p| p.pair_id == hp.pair_id) else {
            warn!(target: "hedge", event = "unknown_pair", pair_id = hp.pair_id,
                  "hedge configured for a pair the bot does not quote; ignored");
            continue;
        };
        let parse = |s: &str| s.parse::<u128>().unwrap_or(0);
        bands.insert(
            hp.pair_id,
            hedge::Bands::new(
                hp.pair_id,
                hedge::Band {
                    width: hedge::derive_band(parse(&hp.carry_base)),
                    qty_min: parse(&hp.qty_base_min),
                    cooloff: Duration::from_millis(hp.cooloff_ms),
                    order_max: parse(&hp.order_base_max),
                },
            ),
        );
        symbols.insert(hp.pair_id, hp.symbol.clone());
        routes.insert(hp.pair_id, (hp.venue, hp.dex.clone()));
        if hp.venue == dubu_updater::config::HedgeVenue::BinancePaper {
            spot.push(hp.symbol.clone());
        }
        if hp.venue == dubu_updater::config::HedgeVenue::HyperliquidPaper && !dexs.contains(&hp.dex)
        {
            dexs.push(hp.dex.clone());
        }
        qty_decimals.insert(hp.pair_id, hp.qty_decimals);
        scale.insert(hp.pair_id, 10f64.powi(i32::from(pair.base_decimals)));
    }
    if bands.is_empty() {
        warn!(target: "hedge", event = "disabled", "no hedgeable pairs; quoting unhedged");
        return None;
    }

    info!(
        target: "hedge", event = "enabled", base_url = %hc.base_url,
        pairs = bands.len(), taker_fee_bps_e2 = hc.taker_fee_bps_e2,
        sigma_millibps_per_sqrt_sec,
        bands_bps = bands.values().map(|b| b.width().to_string()).collect::<Vec<_>>().join(","),
        "hedge leg configured; each pair carries exposure inside its band and reflects at the edge"
    );
    // Built only if some pair asks for it, so a crypto-only config makes no Hyperliquid requests.
    let paper = if dexs.is_empty() {
        None
    } else {
        match hedge::hyperliquid::Paper::new(
            hc.hyperliquid_url.clone(),
            Duration::from_millis(hc.timeout_ms),
        ) {
            Ok(mut p) => {
                // The same taker fee the real venue charges. A paper book that fills for free
                // reports a hedge nobody could execute, and the fee is the larger half of the cost
                // at these sizes -- 4 bp against a measured 0.001 bp spread on BTCUSDT.
                p.charge(hc.taker_fee_bps_e2);
                info!(target: "hedge", event = "paper_enabled", url = %hc.hyperliquid_url,
                      books = dexs.len(), spot_symbols = spot.len(),
                      taker_fee_bps_e2 = hc.taker_fee_bps_e2,
                      "paper pairs take the decision and book the fill; no order is sent");
                Some(p)
            }
            Err(e) => {
                warn!(target: "hedge", event = "paper_disabled", error = %e,
                      "those pairs will quote unhedged");
                None
            }
        }
    };

    Some(HedgeLeg {
        positions: BTreeMap::new(),
        venue,
        paper,
        routes,
        dexs,
        spot,
        bands,
        symbols,
        scale,
        qty_decimals,
        resync: Duration::from_secs(hc.clock_resync_secs),
        last_resync: Instant::now(),
    })
}

/// Poll the venues and cross out whatever drift has earned a crossing, forever, on its own clock.
///
/// Never returns until shutdown. A failed pass is logged and the loop continues: a slot holding a
/// slightly older position book is a far better outcome than a hedge task that gives up, which is the
/// same call `chain::view::run` makes about the chain read.
///
/// # Why this is not on the cycle any more
///
/// It used to be called from `run_cycle` and it was the cycle's largest single cost — a median 1.373s
/// of 2.695s, and 1.377s of 1.657s once the send phase was batched, which by then was everything that
/// was left. All of it is external I/O against a paper book that nothing in the quote path reads
/// synchronously, so the price-setting loop was spending more than half its time on work that set no
/// price.
///
/// The docstring this replaces claimed it had to run "after `scan_fills`, on the same cycle, because
/// the fills it reacts to were just recorded". That was never true, and `scan_fills` says so itself:
/// the hedge is not told about fills, it reads the pool's balance and its own venue position directly.
/// The only thing it needed from the cycle was the balance, and the balance comes from the reader's
/// slot, which this task can read for itself.
async fn run_hedge(
    mut leg: HedgeLeg,
    view: Arc<ViewSlot>,
    bases: BTreeMap<u16, Address>,
    shared: Arc<HedgeShared>,
    every: Duration,
    mut shutdown: watch::Receiver<bool>,
) {
    info!(
        target: "hedge", event = "task_started", interval_ms = every.as_millis() as u64,
        pairs = leg.bands.len(),
        "the hedge polls on its own clock now; the quote loop no longer waits for it"
    );

    // A ticker rather than `sleep(every)` at the bottom, for the reason `chain::view::run` gives:
    // sleeping then working makes the real period `every + work`, and the work here is the six round
    // trips this whole change exists to get off the critical path. `Delay` on a missed tick so a slow
    // pass shifts the schedule rather than queueing catch-up passes behind it — which at these
    // latencies would be a way to exceed the venue's rate budget without ever asking for it.
    let mut ticker = tokio::time::interval(every);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            biased;
            _ = shutdown.changed() => break,
            _ = ticker.tick() => {}
        }
        // No view, no pass. A pair whose balance has not been read is skipped rather than observed as
        // zero — see `hedge_pass` — and with no view at all every pair is in that state, so there is
        // nothing to observe and a crossing would be against invented inventory.
        let Some(v) = view.latest() else { continue };
        hedge_pass(&mut leg, &v, &bases).await;
        // After the crossings settle, so the cycle reads positions that include what this pass just
        // committed. Publishing first would hand the skew a book one pass out of date in the one
        // direction that matters: it would not know about exposure this task has already taken on.
        shared.publish(&leg.positions);
    }

    info!(target: "hedge", event = "task_stopped", "shutting down; no further crossings");
}

/// One pass: observe every band, refresh the books, and send whatever crossings are due.
///
/// Sends at most one order per pair per pass. The cool-off exists to stop a second crossing before the
/// first is reflected, and passing faster than the venue answers would defeat it — see
/// [`hedge_interval`] for why the pass rate is derived from that cool-off rather than picked.
async fn hedge_pass(leg: &mut HedgeLeg, view: &ChainView, bases: &BTreeMap<u16, Address>) {
    let now = Instant::now();

    // Where every band gets its exposure from, and the only place it comes from.
    //
    // Two absolutes: the pool's balance, already read this cycle for the skew, and the venue
    // position this leg has been tracking. No `eth_getLogs`, no cursor, no ledger of what was sent.
    // A pair whose balance did not make it into this view is SKIPPED rather than observed as zero --
    // a missing read is not a flat book, and treating it as one would unwind a live hedge.
    for (pair_id, band) in &mut leg.bands {
        let Some(base) = bases.get(pair_id) else {
            continue;
        };
        let Some(&pool_base) = view.balances.get(base) else {
            continue;
        };
        let venue_base = leg.positions.get(pair_id).copied().unwrap_or(0);
        band.observe(i128::try_from(pool_base).unwrap_or(i128::MAX), venue_base);
    }

    // A drifting host clock breaks every signed call at once, with an error that names the
    // timestamp and not the cause. Re-measure on a timer rather than waiting to find out.
    if now.saturating_duration_since(leg.last_resync) >= leg.resync {
        leg.last_resync = now;
        if let Err(e) = leg.venue.sync_clock().await {
            warn!(target: "hedge", event = "clock_resync_failed", error = %e,
                  "keeping the previous offset");
        }
    }

    // Every book in one concurrent pass. These were six sequential round trips — five spot depth
    // calls and one `allMids` — and nothing orders them, so sequential was six copies of one wait.
    // See `hedge::hyperliquid::Paper::poll_all`, which is also where the per-symbol/per-dex asymmetry
    // is argued.
    let dexs = leg.dexs.clone();
    let spot = leg.spot.clone();
    if let Some(paper) = leg.paper.as_mut() {
        let report = paper.poll_all(&spot, &dexs).await;
        for (dex, e) in &report.dex_failures {
            warn!(target: "hedge", event = "paper_poll_failed", dex = %dex, error = %e,
                  "the book keeps its previous mids");
        }
        paper.warn_if_stale(now, Duration::from_secs(10));
    }

    let due: Vec<hedge::Order> = leg
        .bands
        .values_mut()
        .filter_map(|b| b.evaluate(now))
        .collect();

    for order in due {
        let Some(symbol) = leg.symbols.get(&order.pair_id).cloned() else {
            continue;
        };
        let scale = leg.scale.get(&order.pair_id).copied().unwrap_or(1.0);
        let decimals = leg.qty_decimals.get(&order.pair_id).copied().unwrap_or(3);

        // Truncated, never rounded. Rounding up asks the venue for more than the pool actually
        // holds, and the rejection that follows leaves the drift uncleared while looking like a
        // transient error.
        let raw = order.qty as f64 / scale;
        let step = 10f64.powi(i32::try_from(decimals).unwrap_or(3));
        let qty = (raw * step).floor() / step;
        if qty <= 0.0 {
            continue;
        }

        // Committed before the round trip, not after. The exposure is taken the moment the order
        // leaves; waiting for the reply is what let one fill be hedged twice.
        let committed = i128::try_from(order.qty).unwrap_or(i128::MAX);
        let signed = match order.side {
            hedge::Side::Sell => -committed,
            hedge::Side::Buy => committed,
        };
        *leg.positions.entry(order.pair_id).or_insert(0) += signed;

        let route = leg
            .routes
            .get(&order.pair_id)
            .map_or(dubu_updater::config::HedgeVenue::Binance, |(v, _)| *v);

        // Paper: the same decision, the same book, no order. A write that cannot be priced is
        // refused rather than invented -- an imaginary mid reports a cost that never existed, and
        // the drift would be settled against it exactly as if it had.
        if matches!(
            route,
            dubu_updater::config::HedgeVenue::HyperliquidPaper
                | dubu_updater::config::HedgeVenue::BinancePaper
        ) {
            let written = leg
                .paper
                .as_mut()
                .ok_or_else(|| "no paper book".to_string())
                .and_then(|p| p.write(&symbol, order.side, qty).map_err(|e| e.to_string()));
            match written {
                Ok(fill) => {
                    if let Some(b) = leg.bands.get_mut(&order.pair_id) {
                        b.settle(Instant::now(), order.side, order.qty);
                    }
                    info!(
                        target: "hedge", event = "crossed_paper", pair_id = order.pair_id,
                        symbol = %symbol, side = order.side.as_str(), qty,
                        mid = fill.mid, price = fill.price,
                        cost_bps_e2 = fill.cost_bps_e2, position = fill.position,
                        "hedge decided and booked; no order was sent"
                    );
                }
                Err(e) => {
                    // Back out the commitment, exactly as a rejected live order does.
                    *leg.positions.entry(order.pair_id).or_insert(0) -= signed;
                    warn!(
                        target: "hedge", event = "paper_failed", pair_id = order.pair_id,
                        symbol = %symbol, error = %e,
                        "the drift stays outstanding"
                    );
                }
            }
            continue;
        }

        match leg.venue.market(&symbol, order.side, qty).await {
            Ok(fill) => {
                // Settled on what filled, not on what was asked for. See `hedge::Bands::settle`.
                // Correct the commitment to what actually filled. A partial fill leaves the
                // difference un-hedged, and the position must say so or the skew believes an
                // exposure was neutralised that is still open.
                let filled_base = (fill.executed_qty * scale) as u128;
                let actual = i128::try_from(filled_base).unwrap_or(i128::MAX);
                let corrected = match order.side {
                    hedge::Side::Sell => -actual,
                    hedge::Side::Buy => actual,
                };
                *leg.positions.entry(order.pair_id).or_insert(0) += corrected - signed;
                if let Some(b) = leg.bands.get_mut(&order.pair_id) {
                    b.settle(Instant::now(), order.side, filled_base);
                }
                info!(
                    target: "hedge", event = "crossed", pair_id = order.pair_id, symbol = %symbol,
                    side = order.side.as_str(), requested = qty, executed = fill.executed_qty,
                    avg_price = fill.avg_price, status = %fill.status, order_id = fill.order_id,
                    deviation_left = leg.bands.get(&order.pair_id).map(dubu_updater::hedge::Bands::deviation),
                    "inventory neutralised on the venue"
                );
            }
            Err(e) => {
                // Nothing reached the venue, so the commitment has to come back off. Leaving it
                // would report an exposure that was never taken and skew against a hedge that does
                // not exist.
                *leg.positions.entry(order.pair_id).or_insert(0) -= signed;
                error!(
                target: "hedge", event = "cross_failed", pair_id = order.pair_id, symbol = %symbol,
                side = order.side.as_str(), qty, error = %e,
                "the drift stays outstanding and will be retried"
                );
            }
        }
    }
}

/// Read the fills that landed since the last cycle, mark the ones that have matured, and log both.
///
/// Runs after the quote decisions rather than before them, deliberately. Markout is a measurement
/// and this cycle's quotes do not depend on it — putting an `eth_getLogs` round trip in front of
/// the ladder would add latency to the one thing on this loop that is actually racing. Acting on
/// what it learns is a decision for a later cycle, which is the correct shape anyway: a score is
/// only meaningful once it has settled.
///
/// Bounded to the confirmed head from `newHeads`, not to `view.block_number`. The read view comes
/// from the flashblocks endpoint's `pending` tag and can be *ahead* of any block the canonical RPC
/// has, so using it would ask for logs from a block that does not exist there yet. With no head,
/// the scan is skipped rather than guessed at — the cursor does not advance, so the next poll
/// picks the same range up. That replayability is why this polls instead of subscribing.
async fn scan_fills(rt: &mut Runtime, head: &heads::HeadSnapshot, view: &ChainView) {
    // The subscription first, the reader's own sealed read second. Returning here when the socket
    // is down is what stopped fills being seen at all today -- and with them the hedge, which had
    // nothing to neutralise because nothing was ever observed. The preconfirmed sub-fetch inside
    // `SwapWatch::poll` is bounded by this too, so losing the head lost the fast path as well.
    let Some(confirmed) = head
        .last
        .map(|h| h.number)
        .or_else(|| view.sealed.map(|(n, _)| n))
    else {
        return;
    };

    let polled = match rt.swaps.poll(&rt.rpc, confirmed).await {
        Ok(p) => p,
        Err(e) => {
            warn!(
                target: "markout", event = "poll_failed", error = %e,
                cursor = ?rt.swaps.cursor(),
                "could not read Swap logs; the cursor is unchanged and the next cycle re-reads this range"
            );
            return;
        }
    };

    // A cursor quietly behind the head reads exactly like a chain with nothing happening on it,
    // which is how this scan sat 792 blocks back for half an hour without anything louder than a
    // per-poll warning. Draining is normal after a restart; not draining is the fault.
    if polled.behind_blocks > 0 || polled.unread_chunks > 0 {
        warn!(
            target: "markout", event = "scan_behind",
            behind_blocks = polled.behind_blocks, unread_chunks = polled.unread_chunks,
            cursor = ?rt.swaps.cursor(), head = confirmed,
            "the Swap scan has not caught up; fills in the gap are not in any markout total yet"
        );
    }

    if polled.undecodable > 0 || polled.unresolved > 0 {
        error!(
            target: "markout", event = "logs_lost",
            undecodable = polled.undecodable, unresolved = polled.unresolved,
            "SWAP LOGS COULD NOT BE ACCOUNTED FOR: these fills are missing from every markout \
             total below. A non-zero count here means the chain and the engine disagree about the \
             event, and is never a rounding matter"
        );
    }

    for log in &polled.fills {
        let Some(meta) = rt.facts.pairs.get(&log.pair_id).copied() else {
            warn!(
                target: "markout", event = "unknown_pair", pair_id = log.pair_id,
                tx = %log.tx_hash, "a fill on a pair this updater does not quote; not scored"
            );
            continue;
        };

        // The notional denominator. Our reference at the fill's own timestamp when we have one;
        // otherwise the price the trade itself executed at, which is always available and is a
        // true statement about the fill. Both are honest scales for "how big was this trade" —
        // what would not be is reaching for the nearest reference regardless of how far away it
        // is, which is the one thing `reference_at`'s tolerance exists to prevent.
        let base = if log.is_bid {
            log.amount_in
        } else {
            log.amount_out
        };
        let quote = if log.is_bid {
            log.amount_out
        } else {
            log.amount_in
        };
        // Kept, rather than consumed by the `match` below, because whether a *real* reference
        // existed is itself information — see `fill_alert`, which must not report an edge computed
        // against the fill's own execution price.
        let reference = rt.markout.reference_at(log.pair_id, log.at_secs);
        let ref_at_fill = match reference {
            Some(r) => r,
            None => {
                let scale = 10u128.pow(u32::from(meta.price_scale_exp));
                match quote
                    .checked_mul(scale)
                    .and_then(|n| n.checked_div(base.max(1)))
                {
                    Some(p) if p > 0 => p,
                    _ => continue,
                }
            }
        };

        info!(
            target: "markout", event = "fill", pair_id = log.pair_id,
            receiver = %log.receiver, sender = %log.sender, routed = log.sender != log.receiver,
            partner_id = log.partner_id, is_bid = log.is_bid,
            amount_in = log.amount_in, amount_out = log.amount_out,
            block = log.block_number, at_secs = log.at_secs, tx = %log.tx_hash,
            "fill observed"
        );

        // The same event, pushed. Third-party volume is zero today and the flow simulator is about
        // to make it not be, so this is the message that will arrive in bursts; the batching in
        // `notify` is sized for that rather than for the current quiet.
        if let Some(fill) = fill_alert(rt, view, log, &meta, base, quote, reference) {
            rt.notify.send(notify::Event::Fill(fill));
        }

        // The hedge is NOT told about this fill, and that is deliberate. It reads the pool's balance
        // and the venue's position directly -- see `hedge::Bands` -- so a fill it never hears about
        // still shows up as exposure on the next cycle. Feeding it deltas here as well would count
        // the same trade twice. What the fill is still needed for is `markout`, below, which scores
        // the price it happened at and cannot get that from a balance.

        rt.markout.observe_fill(markout::Fill {
            pair_id: log.pair_id,
            receiver: log.receiver,
            partner_id: log.partner_id,
            is_bid: log.is_bid,
            amount_in: log.amount_in,
            amount_out: log.amount_out,
            at_secs: log.at_secs,
            ref_at_fill,
            price_scale_exp: meta.price_scale_exp,
        });
    }

    // Settled against the chain's clock, since that is what the fills are stamped with.
    let now_secs = head.last.map(|h| h.timestamp).unwrap_or(0);
    for (fill, marks) in rt.markout.settle(now_secs) {
        info!(
            target: "markout", event = "marked", pair_id = fill.pair_id,
            receiver = %fill.receiver, is_bid = fill.is_bid, at_secs = fill.at_secs,
            m1_e2 = ?marks[0], m10_e2 = ?marks[1], m60_e2 = ?marks[2],
            "fill marked out; negative is the counterparty winning"
        );
    }

    if !polled.fills.is_empty() || rt.markout.pending_len() > 0 {
        let worst = rt.markout.worst(markout::HORIZONS_SECS.len() - 1, 3);
        info!(
            target: "markout", event = "scoreboard",
            new_fills = polled.fills.len(), pending = rt.markout.pending_len(),
            unmarked = rt.markout.unmarked, duplicates = polled.duplicates, removed = polled.removed,
            gaps = rt.swaps.gaps(), skipped_blocks = rt.swaps.skipped_blocks(),
            worst = ?worst.iter().map(|(a, s)| (a.to_string(), s.markout_e2(2), s.fills)).collect::<Vec<_>>(),
            "markout scoreboard"
        );
    }
}

/// Turn one observed fill into the alert a human reads on a phone.
///
/// `None` when the fill cannot be described honestly — an unknown pair, a price that will not
/// convert — rather than a message with a placeholder in it. An alert nobody can act on is worse
/// than no alert, because it trains the reader to skim.
///
/// # What is in scope here and what is not, because the difference is not obvious
///
/// **Markout is not.** A markout is the fill's value at +1s, +10s and +60s measured against a
/// later reference, and at the instant a fill is observed none of those references exists. It
/// arrives about a minute later out of `Markout::settle`, as the `marked` event a few lines below
/// the call site. Reporting a zero, or the fill-time reference twice, would be a number that looks
/// like a measurement and is not one, so this carries no markout field at all.
///
/// **The skew is not.** It is computed per pair inside `run_cycle` from that cycle's inventory and
/// volatility and is not stored anywhere this can reach; the closest honest substitute is the
/// inventory the skew is derived from, which is what goes out instead.
///
/// **The inventory is, with a caveat that is in the wording.** `view.balances` is the pool's
/// balance as of the reader task's last poll — up to ~330ms old, and read at a block that may be
/// later than the fill's, since this scan runs behind the confirmed head. So it is the inventory
/// *now*, not the inventory this fill produced, and the message says "inventory now" for that
/// reason. Reconstructing a per-fill position would mean replaying every fill in the block against
/// a balance nobody sampled at that block, which is a measurement this does not have.
fn fill_alert(
    rt: &Runtime,
    view: &ChainView,
    log: &dubu_updater::chain::swaps::SwapLog,
    meta: &dubu_updater::chain::PairMeta,
    base: u128,
    quote: u128,
    reference: Option<u128>,
) -> Option<notify::Fill> {
    // The pair's feed symbol, which is what an operator calls it. `pair_id` alone is a number
    // whose meaning lives in a config file they would have to go and open.
    let symbol = rt
        .cfg
        .pairs
        .iter()
        .find(|p| p.pair_id == log.pair_id)?
        .symbol
        .clone();

    // Both prices are converted through the pair's own shift rather than assembled from the raw
    // amounts here. `units` is the only place in this crate where a decimal scale is decided, and
    // a second derivation of the same conversion is how the two quietly stop agreeing.
    let shift = units::price_shift(
        meta.price_scale_exp,
        meta.base_decimals,
        meta.quote_decimals,
    );
    let scale = dubu_core::math::pow10(meta.price_scale_exp)?;
    let executed = quote.checked_mul(scale)?.checked_div(base.max(1))?;
    let price_e8 = units::from_pool_price(executed, shift)?;

    Some(notify::Fill {
        pair_id: log.pair_id,
        symbol,
        is_bid: log.is_bid,
        base_amount: base,
        base_decimals: meta.base_decimals,
        quote_amount: quote,
        quote_decimals: meta.quote_decimals,
        price_e8,
        // `reference`, not `ref_at_fill`. The latter falls back to the fill's own execution price
        // when nothing was in tolerance, and handing that over as a reference would render every
        // such fill at exactly zero edge — see `notify::Fill::reference_e8`.
        reference_e8: reference.and_then(|r| units::from_pool_price(r, shift)),
        inventory_base: view.balances.get(&meta.base).copied(),
        inventory_quote: view.balances.get(&meta.quote).copied(),
        tx: log.tx_hash.to_string(),
    })
}

/// One evaluation over every pair. Returns `true` if a killswitch latched.
async fn run_cycle(
    rt: &mut Runtime,
    view: &ChainView,
    status: ChainStatus,
    wake: Wake,
    head: &heads::HeadSnapshot,
    cycles: u64,
) -> bool {
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

    // The clock. NOT `view.block_timestamp` — see `ChainView::block_timestamp`: the `pending` tag
    // projects an unsealed block's header and lands about twelve seconds in the future, which made
    // every quote read as 12s old. The `newHeads` head is a sealed block, so its timestamp is real;
    // it lags by roughly a second, which over-states age slightly and is the safe direction.
    //
    // Falling back to the local clock when heads are down is a worse approximation than a sealed
    // header and a much better one than a projection: the machine's skew against the sequencer is
    // seconds at worst, where the projection is twelve out by construction.
    // Sealed, and from whichever source has one. The subscription is the fast path when it is up;
    // the reader's own `latest` read is the one that survives a key running out, which is what took
    // the socket down today and silently moved this to the host clock. Verified sealed rather than
    // assumed: measured, `latest` advances exactly once per block on both endpoints, where
    // `pending` advances 2.1 times.
    let chain_now = head
        .last
        .map(|h| h.timestamp)
        .or_else(|| view.sealed.map(|(_, ts)| ts))
        .unwrap_or_else(now_unix);

    // Hand the RFQ maker the chain's clock. It stamps every signed expiry from this rather than
    // from the host's wall clock -- see `serve::Inner::chain_clock` for why that distinction is the
    // difference between the leg working and every order arriving expired.
    if let (Some(rfq), Some(h)) = (&rt.rfq, head.last) {
        // Back-dated by the head's own age, so `chain_now` counts from when the head actually
        // arrived rather than from this cycle. Both ends are monotonic, so the host's wall clock
        // never enters the arithmetic.
        let received = Instant::now()
            .checked_sub(Duration::from_millis(head.age_ms.unwrap_or(0)))
            .unwrap_or_else(Instant::now);
        rfq.publish_clock(h.timestamp, received);
    }

    // Once, on the first cycle that has a real sealed head to compare against. Deliberately not at
    // startup: `wait_for_first_head` is not fatal on timeout, so the check ran there with no head at
    // all and returned silently -- on the very run where the subscription was down and the clock
    // mattered most.
    if !rt.clock_checked && head.last.is_some() {
        rt.clock_checked = true;
        check_clock_skew(rt, head);
    }

    // True once per sealed block. See `Runtime::last_block_work`.
    let block_work = chain_now > rt.last_block_work;
    if block_work {
        rt.last_block_work = chain_now;
    }

    // One line per cycle saying what woke it and how far the read got ahead of the head. The
    // delta is the flashblocks endpoint earning its place: `pending` there is typically at or
    // ahead of the confirmed head that triggered the read, which is the freshness the split
    // exists for. A persistently negative delta would mean the read source has fallen behind
    // the head source and the endpoints want revisiting.
    // Once a second rather than once a cycle. The cycle runs at 5Hz now and this line carries no
    // per-cycle information -- it reports the state of the read, the head and the health, none of
    // which move faster than a block. Emitting it every tick would bury the lines that do mean
    // something in five times their own volume.
    if block_work {
        info!(
            // Cycles since start. The line is emitted once per sealed block, so the difference
        // between two of them IS the cycle rate -- which is the number the quote clock exists to
        // raise and therefore the one that has to be visible rather than assumed.
        target: "loop", event = "cycle", cycles, woke_on = wake.label(), head = ?wake.head_number(),
            heads_status = head.status.label(), head_age_ms = ?head.age_ms,
            head_reconnects = head.reconnects,
            read_block = view.block_number, block_timestamp = view.block_timestamp,
            read_ahead_of_head = head.last.map(|h| i64::try_from(view.block_number).unwrap_or(i64::MAX)
                - i64::try_from(h.number).unwrap_or(i64::MAX)),
            chain = status.label(), view_age_secs = view_age,
            // The reader task's own counters. Worth having on the cycle line because the two now run
            // on different clocks: a cycle count that no longer matches a read count is the whole
            // point of the split, and `quiet_polls` is the signal a fill-frequency rule would key on
            // once there is third-party flow to see.
            reads = rt.view.polls(), read_failures = rt.view.failures(),
            quiet_polls = rt.view.quiet_polls(),
            // Per endpoint, in the order they were configured, because the failure messages cannot
            // say which one: they all carry the pool's name. A rising count against one position is
            // a key to rotate or a limit to raise; a rising count against every position is a rate
            // the whole pool cannot serve, and the two want opposite responses.
            //
            // Both pools, because `read_failures` above is the READ pool's and reading the write
            // pool's counters against it explains the wrong thing entirely.
            read_rate_limited_by_endpoint = %rt.read_rpc.rate_limit_events_by_endpoint()
                .iter().map(u64::to_string).collect::<Vec<_>>().join(","),
            write_rate_limited_by_endpoint = %rt.rpc.rate_limit_events_by_endpoint()
                .iter().map(u64::to_string).collect::<Vec<_>>().join(","),
            "cycle"
        );
    }

    let degraded_extra = if matches!(status, ChainStatus::Degraded { .. }) {
        rt.cfg.chain.degraded_extra_half_spread_bps
    } else {
        0
    };
    if degraded_extra > 0 {
        warn!(target: "chain", event = "degraded", extra_half_spread_bps = degraded_extra,
              status = status.label(), "chain view is degraded; widening every half-spread");
    }

    // The jump check runs BEFORE anything else in the cycle, and before `vol.observe` folds this
    // tick's return into the variance. Testing a jump against a sigma that already contains it is
    // a test that can never fire.
    jump_scan(rt).await;

    let mut positions: Vec<Position> = Vec::new();
    let mut sends: Vec<Intent> = Vec::new();
    let mut reassert: Vec<u16> = Vec::new();

    for i in 0..rt.cfg.pairs.len() {
        let pair = rt.cfg.pairs[i].clone();
        // A latched group quotes nothing. Withdrawal is the halt path's job; this stops the row
        // from being rebuilt underneath it on the next cycle.
        if rt
            .kills
            .get(&pair.jump_group)
            .is_some_and(KillSwitch::is_halted)
        {
            // Only when the chain still shows capacity, so a group that is already down does not
            // re-send `refreshCapacity(pair, 0, 0)` every cycle and burn a nonce for nothing.
            //
            // Capacity alone is not enough, and this is the same defect the jump path had: the
            // snapshot goes on showing depth until the withdrawal is included, which takes about
            // two seconds against a cycle of under three, so a capacity-only guard fires again on
            // the very next cycle and sends a second withdrawal for a pair already withdrawing.
            // The equity group has been latched since 2026-07-29 and pays that on four pairs, on
            // every cycle inside the inclusion window, at the 100x tip a withdrawal carries.
            if view
                .snaps
                .get(&pair.pair_id)
                .is_some_and(|s| s.bid_capacity != 0 || s.ask_capacity != 0)
                && !rt.sender.withdrawal_in_flight(pair.pair_id)
            {
                reassert.push(pair.pair_id);
            }
            continue;
        }
        let Some(meta) = rt.facts.pairs.get(&pair.pair_id).copied() else {
            continue;
        };
        let Some(snap) = view.snaps.get(&pair.pair_id).copied() else {
            continue;
        };

        let now = Instant::now();
        let shift = units::price_shift(
            meta.price_scale_exp,
            meta.base_decimals,
            meta.quote_decimals,
        );

        // Re-assert a withdrawal the chain does not show. The fast lane sends once, on the edge;
        // if that transaction was rejected, dropped, or sent while the RPC was unavailable, the
        // pool is still armed and the detector believes otherwise. This is the only thing that
        // notices, and it runs at the chain's own cadence rather than the fast lane's because it
        // needs a chain read to answer the question at all.
        //
        // `withdrawal_in_flight` and not merely `at_capacity`, because `at_capacity` answers a
        // different question. `in_flight_max` is 2, so the fast lane's own withdrawal leaves a
        // slot open, and this runs ~1.4s behind it against a ~2s inclusion latency — the chain
        // genuinely still shows capacity at that moment, so the test above is true for a reason
        // that is already being fixed. Every one of those re-sends was a second transaction at the
        // 100x withdrawal tip buying a state the first was about to reach: measured over 16.2
        // hours, 150-154 withdrawal transactions per pair against 73-75 real episodes, so roughly
        // 740 of them bought nothing at all. What is genuinely rejected or dropped still gets
        // re-asserted, because that is exactly the case where nothing is left in flight.
        let jump_withdrawn = rt.jump.withdrawn(pair.pair_id);
        if jump_withdrawn
            && (snap.bid_capacity != 0 || snap.ask_capacity != 0)
            && !rt.sender.at_capacity(pair.pair_id)
            && !rt.sender.withdrawal_in_flight(pair.pair_id)
        {
            warn!(
                target: "jump", event = "reassert", pair_id = pair.pair_id,
                bid_capacity = %snap.bid_capacity, ask_capacity = %snap.ask_capacity,
                "withdrawn, but the chain still shows a capacity epoch; re-sending the withdrawal"
            );
            reassert.push(pair.pair_id);
        }

        // --- the cross-section -------------------------------------------------------------
        //
        // Every venue that quotes this symbol, then the MAD filter and the quorum rule on top.
        // A venue that is not `Live` contributes nothing and is reported as a transition, never
        // as an absence: a cross-section that silently shrinks from three venues to two still
        // produces a confident-looking price.
        let snaps = rt.feeds.snapshots(&pair.symbol, now);
        for t in rt.watch.diff(&pair.symbol, &snaps) {
            if t.recovered() {
                info!(target: "feed", event = "venue_up", venue = %t.venue, symbol = %t.symbol,
                      from = t.from.label(), to = t.to.label(), "venue is delivering again");
            } else {
                warn!(target: "feed", event = "venue_down", venue = %t.venue, symbol = %t.symbol,
                      from = t.from.label(), to = t.to.label(),
                      "VENUE LOST: it no longer contributes to the reference price");
            }
        }

        let mut quotes: Vec<VenueQuote> = Vec::new();
        for (venue, s) in &snaps {
            let Some(tick) = s.live() else { continue };
            match VenueQuote::new(*venue, tick, s.age_ms.unwrap_or(0)) {
                Ok(q) => quotes.push(q),
                Err(e) => warn!(target: "feed", event = "venue_book_rejected", venue = %venue,
                                symbol = %pair.symbol, error = %e,
                                "structurally broken book; this venue is out of the cross-section"),
            }
        }

        let reference = fair_value::combine(&quotes, &rt.cfg.feed.mad_params_with(pair.venues_min));
        let feed_status = match &reference {
            Ok(_) => FeedStatus::Live,
            Err(e) => e.status(),
        };

        let Some(vol) = rt.vol.get_mut(&pair.pair_id) else {
            continue;
        };
        let fair = match &reference {
            Ok(r) => {
                vol.observe(r.micro, now);
                log_reference(
                    &pair.symbol,
                    pair.pair_id,
                    r,
                    &snaps,
                    shift,
                    vol,
                    block_work,
                );
                units::to_pool_price(r.micro, shift)
            }
            // No branch on the error side: markout's reference history must contain only prices
            // the venues actually agreed on. Carrying the last good one forward through an outage
            // would mark every fill in that window against a price nobody was showing.
            Err(e) => {
                // No reference means no return either. Folding a gap into the estimator would
                // enter the whole outage as one enormous one-second return.
                vol.reset();
                warn!(
                    target: "feed", event = "no_reference", pair_id = pair.pair_id,
                    symbol = %pair.symbol, reason = e.label(), error = %e,
                    venues_live = quotes.len(), venues_configured = snaps.len(),
                    venues_min = rt.cfg.feed.venues_min,
                    "NO REFERENCE PRICE: not quoting this pair until the venues agree"
                );
                None
            }
        };
        let sigma_sq = vol.sigma_sq_bps_e6();
        let sigma_millibps = vol.sigma_millibps();
        let vol_samples = vol.samples();

        // Stamped with the block timestamp, not the wall clock, because the fills these marks are
        // compared against carry block timestamps too. Mixing the two clocks would put a systematic
        // offset between a fill and its own reference.
        if block_work {
            if let Some(f) = fair {
                rt.markout.observe_reference(pair.pair_id, chain_now, f);
            }
        }

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
        let bid_cap = if snap.bid_capacity == 0 {
            capacity.bid
        } else {
            snap.bid_capacity
        };
        let ask_cap = if snap.ask_capacity == 0 {
            capacity.ask
        } else {
            snap.ask_capacity
        };

        // Publish this market to the RFQ endpoint, or withdraw it.
        //
        // The epoch handed over is the one that will be in force, so RFQ subtracts the commitment
        // the curve is about to honour rather than a stale one. With no fair value the market is
        // retired outright: an RFQ order is a firm price for its whole TTL, so signing one against
        // a reference the venues no longer agree on is worse than leaving a stale ladder up, which
        // at least stops itself at `maxStaleSecs`.
        //
        // The `jump_withdrawn` half of that gate is what makes `withdraw_pair`'s `rfq.retire`
        // mean anything. Without it the retire lasted exactly one cycle: this arm ran on nothing
        // but `fair.is_some()`, so ~3s after the fast lane took the market down it went straight
        // back up, and it did so for the rest of the cool-off. Measured on the live system: a trip
        // at 05:45:31.730 and the maker signing that pair again at 05:45:38.569 — 6.8s into a
        // 30.2s cool-off, at roughly a 1bp half-spread, into exactly the jump the curve had just
        // fled. The zero epoch was holding the whole time; the leg that quotes without touching
        // the chain was walking straight through it, which is the hole `withdraw_pair` documents
        // as the single largest loss the flow simulator found.
        //
        // This is strictly more conservative, and it is worth naming what it costs because the
        // cost is user-visible. The RFQ market was accidentally masking the prop pool's zero, so
        // pairs 3/4/5 go from answering nothing ~0.4% of the day to ~5%, and what was a 3-second
        // flicker becomes the full 30-second outage it always should have been. That is the right
        // trade by a margin that is not close — `jump.rs` prices one 30s outage at $0.48 against
        // $16,580 for one avoided pick-off, or ~34,000:1 — and the outage is not new, only honest.
        // `serve::prop_amounts` is the other half: it now answers `no-capacity` with a 503 rather
        // than a 200 of zeros, so the caller is told to come back rather than told there is no
        // market here.
        if let Some(rfq) = &rt.rfq {
            match fair {
                // NOT `base_balance` / `quote_balance`. Those are the POOL's, and `PmmSettle`
                // pulls from the MAKER — different address, and the binding constraint is usually
                // the allowance rather than the balance. Sizing an RFQ quote against the pool's
                // inventory means signing orders that revert inside a `transferFrom`, which costs
                // the taker gas and leaves nothing useful in the trace.
                Some(f) if !jump_withdrawn => rfq.publish(quoting::MarketState {
                    pair_id: pair.pair_id,
                    base: meta.base,
                    quote: meta.quote,
                    fair: f,
                    price_scale_exp: meta.price_scale_exp,
                    sigma_millibps,
                    base_balance: view.maker_deliverable.get(&meta.base).copied().unwrap_or(0),
                    quote_balance: view
                        .maker_deliverable
                        .get(&meta.quote)
                        .copied()
                        .unwrap_or(0),
                    // Subtract the curve's epoch only if it comes out of the same account. The
                    // maker and the pool are normally different addresses holding different
                    // tokens, and taking the pool's commitment off the maker's balance is
                    // comparing two balance sheets that never meet — it made a maker holding 500
                    // mWETH of allowance refuse a one-mWETH order because the *pool* had promised
                    // 1000.
                    epoch_ask_base: if rt.rfq_shares_pool_inventory {
                        ask_cap
                    } else {
                        0
                    },
                    epoch_bid_base: if rt.rfq_shares_pool_inventory {
                        bid_cap
                    } else {
                        0
                    },
                }),
                // No fair value, or a cool-off in force. Both are the same instruction to the
                // maker — sign nothing for this pair — and neither costs a transaction, because an
                // order that was never signed cannot be filled.
                _ => rfq.retire(pair.pair_id),
            }

            // The prop pool's own quote, mirrored for the aggregator.
            //
            // Unconditional, unlike the RFQ market above: this is the chain's state rather than
            // this process's opinion of it, and every reason the pool would stop quoting is already
            // *in* it. A jump withdrawal zeroes the epoch, so the curve pays nothing. A halted
            // updater stops pushing, so the ladder ages past `maxStaleSecs` and the gate refuses.
            // Withdrawing here as well would take the venue down while the chain was still quoting
            // it, which is the disagreement this whole endpoint exists to remove.
            rfq.publish_prop(serve::PropState {
                pair_id: pair.pair_id,
                base: meta.base,
                quote: meta.quote,
                ladder: snap.ladder(),
                bid_capacity: snap.bid_capacity,
                ask_capacity: snap.ask_capacity,
                bid_used: snap.bid_used(),
                ask_used: snap.ask_used(),
                updated_at: snap.updated_at,
                stale_secs_max: snap.stale_secs_max,
                decay_secs: meta.decay_secs,
                price_scale_exp: snap.price_scale_exp,
                paused: snap.paused(),
                observed_at: now,
            });
        }

        // --- the half-spread ---------------------------------------------------------------
        //
        // `half_spread = min(s0 + s1 * sigma, cap) + degraded_extra`, from the SAME sigma the
        // skew below uses. Computed before the skew, because `price_cap_bps_min` needs the spread
        // that will actually be posted: a wider spread pushes the bid target lower and therefore
        // leaves less room to skew down, and clamping against the unwidened value would let the
        // skew push a row under the pair's `minPrice` and have it refused outright.
        // Rescaled from the estimator's inventory window to the quote's own exposure window. The
        // skew keeps the 300 s number because inventory really is held that long; a posted quote is
        // not. See `spread::rescale_sigma`.
        let spread_sigma = spread::rescale_sigma(
            sigma_millibps,
            rt.cfg.skew.vol_horizon_secs,
            rt.cfg.skew.spread_horizon_secs,
        );
        let spread = spread::compute(
            pair.half_spread_bps_e2(),
            spread_sigma,
            u32::from(degraded_extra) * 100,
            &rt.cfg.spread.params(),
        );
        let half_spread = spread.half_spread_e2;
        // The floor is the operator's and the volatility term only ever adds to it. A half-spread
        // below `s0` would mean the cap narrowed the configured spread, which `spread::compute` and
        // the config validator both refuse -- asserted again here, on the value reaching the chain.
        assert!(half_spread >= pair.half_spread_bps_e2());
        assert!(u128::from(half_spread) < dubu_core::ladder::BPS_E2_MAX);
        // Every row, every cycle. Without these five fields, back-solving `s1` from history later
        // is guesswork — `vol_decibps` next to `capped` is what says whether the model or the
        // ceiling has been doing the deciding.
        trace_at!(
        block_work,

                    target: "spread", event = "half_spread", pair_id = pair.pair_id, symbol = %pair.symbol,
                    s0_bps_e2 = spread.s0_bps_e2,
                    sigma_millibps = spread.sigma_millibps,
                    sigma_horizon_secs = rt.cfg.skew.spread_horizon_secs,
                    sigma_millibps_inventory = sigma_millibps,
                    sigma_horizon_secs_inventory = rt.cfg.skew.vol_horizon_secs,
                    vol_samples,
                    s1 = rt.cfg.spread.vol_coefficient,
                    vol_decibps = spread.vol_decibps,
                    vol_e2 = spread.vol_e2(),
                    vol_scaled_e2 = spread.vol_scaled_e2,
                    capped = spread.capped,
                    cap_bps = rt.cfg.spread.half_spread_bps_max,
                    degraded_extra_e2 = spread.degraded_extra_e2,
                    half_spread_e2 = spread.half_spread_e2,
                    absorption_e2 = spread.half_spread_e2 + u32::from(pair.width_bps) * 50,
                    "volatility-scaled half-spread"
                );

        // --- the inventory skew ------------------------------------------------------------
        //
        // Avellaneda-Stoikov: r = s - q * gamma * sigma^2. Computed here and applied as
        // `RowInputs::skew_bps`, which means it goes through `dubu-core`'s own `skewed_mid`
        // and therefore through `validateLadder` and every other check `ladder::build` runs,
        // BEFORE anything is packed. There is no path by which a skew reaches the chain
        // unvalidated.
        let skew = fair.map(|f| {
            let inventory = Inventory {
                base_value: dubu_updater::risk::value(base_balance, f, meta.price_scale_exp)
                    .unwrap_or(0),
                // The shared quote token split evenly across pairs. Named as a simplification in
                // `skew::Inventory`; it is the same assumption archi_v2 5.4's missing cross-asset
                // clamp already makes.
                quote_share: quote_balance / rt.cfg.pairs.len().max(1) as u128,
                // The hedge, valued at the same reference and signed. Zero when there is no hedge
                // leg, which is exactly the behaviour this had before one existed.
                hedge_value: rt
                    .hedge
                    .as_ref()
                    .and_then(|h| h.position(pair.pair_id))
                    .map(|base| {
                        let magnitude = dubu_updater::risk::value(
                            base.unsigned_abs(),
                            f,
                            meta.price_scale_exp,
                        )
                        .unwrap_or(0);
                        let v = i128::try_from(magnitude).unwrap_or(i128::MAX);
                        if base < 0 { -v } else { v }
                    })
                    .unwrap_or(0),
            };
            // The skew still works in whole bps. Rounded UP, which is the conservative
            // direction: a larger half-spread leaves LESS room to skew down before the row hits
            // the pair's `minPrice`, so ceiling here can only tighten the cap, never loosen it.
            let floor_cap = skew::price_cap_bps_min(
                f,
                meta.min_price,
                u16::try_from(half_spread.div_ceil(100)).unwrap_or(u16::MAX),
            );
            let s = skew::compute(
                &inventory,
                sigma_sq,
                sigma_millibps,
                &rt.cfg.skew.params(),
                floor_cap,
            );
            // Net exposure is a share of the book, so it cannot exceed it. This caught nothing
            // today, but the field's meaning changed today -- it used to be a deviation from a
            // funding target and is now the exposure itself -- and that is exactly when a domain
            // assertion earns its place.
            assert!(
                s.imbalance_ppm.abs() <= 1_000_000,
                "imbalance out of domain: {}",
                s.imbalance_ppm
            );
            assert!(
                i32::from(s.applied_bps) >= -i32::from(rt.cfg.skew.negative_bps_max),
                "skew below its own floor: {} < -{}",
                s.applied_bps,
                rt.cfg.skew.negative_bps_max
            );

            // Every row, every cycle. This is the input to tuning gamma later, and without it
            // gamma is guesswork: `raw_decibps` next to `applied_bps` is what says whether the
            // model or the clamp has been doing the deciding.
            trace_at!(
block_work,

                target: "skew", event = "skew", pair_id = pair.pair_id, symbol = %pair.symbol,
                imbalance_ppm = s.imbalance_ppm,
                // The skew now targets zero net exposure. `target_base_share_pct` is a funding
                // number and no longer reaches this path; logged here so a reader can see they
                // have separated rather than wonder where it went.
                funding_target_ppm = pair.target_base_share_ppm(),
                hedge_value = %inventory.hedge_value,
                base_value = %inventory.base_value, quote_share = %inventory.quote_share,
                sigma_millibps = s.sigma_millibps,
                sigma_horizon_secs = rt.cfg.skew.vol_horizon_secs,
                vol_samples,
                gamma = rt.cfg.skew.gamma,
                raw_decibps = s.raw_decibps,
                applied_bps = s.applied_bps,
                clamp = s.clamp.label(), clamped = s.clamp.bound(),
                floor_cap_bps = s.floor_cap_bps,
                "inventory skew"
            );
            s
        });

        let row = fair.and_then(|f| {
            match ladder::build(&RowInputs {
                pair_id: pair.pair_id,
                fair: f,
                half_spread_bps_e2: half_spread,
                width_bps_e2: u32::from(pair.width_bps) * 100,
                skew_bps: skew.map_or(0, |s| s.applied_bps),
                capture: pair.capture_units().unwrap_or(0),
                bid_capacity: bid_cap,
                ask_capacity: ask_cap,
                min_price: meta.min_price,
                price_scale_exp: meta.price_scale_exp,
            }) {
                Ok(r) => Some(r),
                Err(e) => {
                    warn!(target: "ladder", event = "row_dropped", pair_id = pair.pair_id, error = %e,
                          skew_bps = skew.map_or(0, |s| s.applied_bps),
                          "computed row failed an off-chain check; dropped and NOT sent");
                    None
                }
            }
        });

        if let Some(r) = &row {
            let h = |p: u128| {
                units::from_pool_price(p, shift).map(|v| units::format_fixed(v, FEED_SCALE))
            };
            trace_at!(
            block_work,

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
                            skew_bps = skew.map_or(0, |s| s.applied_bps),
                            fair = %r.fair, mid = %r.mid,
                            word = %r.word.to_hex().unwrap_or_default(),
                            "ladder computed and validated"
                        );
        }

        let ctx = Context {
            block_timestamp: chain_now,
            snap: &snap,
            planned: row.as_ref().map(|r| r.ladder),
            capacity,
            min_price: meta.min_price,
            halted: rt.kills.values().all(KillSwitch::is_halted),
            feed: feed_status,
            chain: status,
            view_age_secs: view_age,
            view_stale_secs: rt.cfg.chain.view_stale_secs,
            in_flight: rt.sender.at_capacity(pair.pair_id),
            jump_withdrawn,
            jump_cooloff_remaining_ms: rt
                .jump
                .detector(pair.pair_id)
                .map_or(0, |d| d.cooloff_remaining_ms(now)),
            heartbeat_secs: pair.heartbeat_secs,
            push_interval_ms_max: pair.push_interval_ms_max,
            since_last_push_ms: rt.last_push.get(&pair.pair_id).map(|t| {
                now.saturating_duration_since(*t)
                    .as_millis()
                    .min(u128::from(u64::MAX)) as u64
            }),
            adverse_drift_bps: pair.adverse_drift_decibps(),
            favourable_drift_bps: pair.favourable_drift_decibps(),
            capacity_divergence_pct: pair.capacity_divergence_pct,
        };

        let quote_decision = policy::evaluate_quote(&ctx).unwrap_or_else(|e| {
            warn!(target: "policy", event = "evaluate_failed", pair_id = pair.pair_id, error = %e,
                  "could not evaluate the stored ladder; holding");
            Decision::Hold(policy::Hold::NoRow)
        });
        let capacity_decision = policy::evaluate_capacity(&ctx);

        trace_at!(
        block_work,

                    target: "policy", event = "decision", pair_id = pair.pair_id, symbol = %pair.symbol,
                    quote = quote_decision.label(), quote_detail = ?quote_decision,
                    capacity = capacity_decision.label(), capacity_detail = ?capacity_decision,
                    quote_age_secs = snap.quote_age_secs(chain_now),
                    heartbeat_limit_secs = ctx.heartbeat_limit(),
                    bid_used = %snap.bid_used(), ask_used = %snap.ask_used(),
                    bid_capacity = %snap.bid_capacity, ask_capacity = %snap.ask_capacity,
                    block = view.block_number, block_timestamp = view.block_timestamp,
                    view_age_secs = view_age, feed = feed_status.label(), chain = status.label(),
                    venues_live = quotes.len(), venues_configured = snaps.len(),
                    half_spread_bps = half_spread,
                    jump_state = rt.jump.detector(pair.pair_id).map_or("disabled", |d| d.state().label()),
                    jump_trips = rt.jump.detector(pair.pair_id).map_or(0, jump::Detector::trips),
                    "decision"
                );

        // What goes out when both fired, and in what order.
        //
        // Ordinarily one intent per pair per cycle, capacity first: a ladder is worthless against a
        // zero epoch, and posting the epoch first means the row that follows is solved against the
        // capacity it will execute on.
        //
        // When the pool is offering no depth — the state a jump withdrawal leaves it in, and the
        // state a freshly deployed pool starts in — **both** go out, row first.
        //
        // The ordering is the point. Capacity-first there would restore a full epoch behind
        // whatever ladder is stored, which after a jump is the pre-jump one, handing a taker
        // exactly the fill the withdrawal was for. Sending both with sequential nonces makes them
        // execute in that order, and between the two landings the pool holds a fresh row against a
        // zero epoch, which quotes nothing. The window the reversal exists to close stays closed.
        //
        // Sending *only* the row there — which is what this did until a redeployed pool sat at zero
        // capacity for 141 cycles without ever posting an epoch — is a livelock. The row is stale
        // every cycle precisely because the reference keeps moving, so it wins the arbitration
        // every cycle, and the capacity refresh it is meant to precede never gets a turn. Nothing
        // is stuck: transactions land, gas is spent, the ladder is current, and the pool quotes
        // zero forever. That is the failure mode worth naming, because unlike a deadlock it looks
        // exactly like a healthy process.
        //
        // `ladder::build` already solves against the *planned* capacity when the on-chain one is
        // zero, so the row is coherent against the epoch that is about to follow it.
        let quote_intent = || match row.as_ref().map(ladder::QuoteRow::packed) {
            Some(Ok(word)) => Some(Intent::UpdateQuote {
                pair_id: pair.pair_id,
                word,
            }),
            Some(Err(e)) => {
                error!(target: "ladder", event = "pack_failed", pair_id = pair.pair_id,
                       error = %e, "a validated row would not pack; dropped");
                None
            }
            None => None,
        };
        let capacity_intent = || Intent::RefreshCapacity {
            pair_id: pair.pair_id,
            bid: capacity.bid,
            ask: capacity.ask,
        };
        let wants_quote = quote_decision.sends();
        let wants_capacity = matches!(capacity_decision, CapacityDecision::Send(_));
        let withdrawn = ctx.withdrawn_on_chain();

        // A validated row that will not pack falls through rather than taking the capacity refresh
        // down with it.
        let pushed_quote = if wants_quote && (withdrawn || !wants_capacity) {
            match quote_intent() {
                Some(i) => {
                    sends.push(i);
                    true
                }
                None => false,
            }
        } else {
            false
        };
        // Both when the pool is dark, so it can come back; otherwise whichever one the
        // arbitration above did not already take.
        if wants_capacity && (withdrawn || !pushed_quote) {
            sends.push(capacity_intent());
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

    // Withdrawals first, and at the withdrawal fees. Everything else this cycle is a quote
    // decision; these are a risk decision that the chain has not caught up with yet.
    for pair_id in reassert {
        withdraw_pair(rt, pair_id, "jump_reassert").await;
    }

    // One batch, not one send at a time.
    //
    // `send_batch` reserves the nonces synchronously in this order -- which is what keeps a
    // `RefreshCapacity` ahead of the `UpdateQuote` for the same pair, since that argument rests
    // entirely on nonce order being absolute -- and only then broadcasts them through a
    // `FuturesUnordered`. Five sends cost one 264ms round trip instead of five, which is 1.323s of a
    // 2.695s cycle recovered.
    match rt.sender.send_batch(&rt.rpc, &sends, None).await {
        Ok(batch) => {
            for (intent, out) in batch.sent {
                record(rt, intent, out);
            }
            // One line per phase for the ceiling; one line per EPISODE for backpressure. The
            // difference is not stylistic. The ceiling is a fact about this batch and cannot repeat
            // faster than the cycle, but a full upstream persists ACROSS batches, so a per-batch line
            // repeats at the cycle rate for as long as the condition lasts -- on the order of 1,600
            // lines a minute at 19 tx/s. That is exactly how the last episode buried the single line
            // saying the account was about to wedge, and it cost 59 minutes of downtime.
            if batch.held_at_capacity > 0 {
                info!(
                    target: "tx", event = "held_at_capacity", held = batch.held_at_capacity,
                    in_flight = rt.sender.in_flight_total(),
                    ceiling = dubu_updater::tx::IN_FLIGHT_TOTAL_MAX,
                    "account is at its in-flight ceiling; these intents were not offered"
                );
            }
            // `batch.held_backpressure` is deliberately not logged on its own. `Sender::send_batch`
            // counts it into the episode and hands back a transition only when one opens or closes,
            // which is the only cadence at which this is readable.
            match batch.episode {
                Some(Episode::Opened { refusals }) => {
                    error!(
                        target: "tx", event = "backpressure_opened", refusals,
                        withheld = batch.held_backpressure,
                        "upstream answered -32003; the send phase is standing down rather than \
                         re-offering to a pool that just refused. At this cadence the account wedges \
                         about 13s after inclusion stops, against 138s at the old rate"
                    );
                    // Coalesced by class on the other side rather than sent one-for-one: this is the
                    // quote path, so the condition touches every send for as long as it lasts. See
                    // `notify::ErrorLedger`.
                    rt.notify.send(notify::Event::SendFailed {
                        pair_id: None,
                        kind: "txpool_full",
                        error: format!("upstream refused {refusals} sends with -32003"),
                    });
                }
                Some(Episode::Closed {
                    refusals,
                    held,
                    secs,
                }) => info!(
                    target: "tx", event = "backpressure_closed", refusals, held, secs,
                    "upstream took a transaction again; this is what the episode cost"
                ),
                None => {}
            }
        }
        Err(e) => {
            // Nothing was offered at all -- no key, or a nonce that could not be established. That is
            // strictly worse than one intent failing, which is why it alerts rather than only logging:
            // every pair is unquoted until it clears, and the pool's own `maxStaleSecs` is then the
            // only thing withdrawing them.
            error!(
                target: "tx", event = "batch_failed", intents = sends.len(), error = %e,
                "could not reserve the send batch; NO transaction was offered this cycle"
            );
            rt.notify.send(notify::Event::SendFailed {
                pair_id: None,
                kind: "send_batch",
                error: e.to_string(),
            });
        }
    }

    // Every cycle, not once per sealed block.
    //
    // It was gated with the other per-block work because `eth_getLogs` over sealed ranges finds
    // nothing four cycles out of five. But a fill is not per-block information -- it is the only
    // signal that an informed counterparty just traded against us, and at a 415ms re-quote interval
    // waiting a full second for it meant two or three more quotes went out at the same price before
    // we knew. `SwapWatch::poll` now also reads the `pending` tag, so the extra frequency has
    // preconfirmed fills to find rather than re-reading blocks it has already seen.
    scan_fills(rt, head, view).await;

    // The killswitches. A group is skipped when any pair *in that group* has no fair value:
    // marking inventory against a price we do not have would be an invention, and an invented NAV
    // is worse than no observation at all.
    //
    // The completeness test is per group, and it used to be `positions.len() == cfg.pairs.len()`
    // over all nine pairs. That is why nothing was observed for fourteen hours: the equity group
    // halted on 2026-07-29, a halted group `continue`s before its reference is computed, so
    // `positions` held only the five crypto pairs, `5 == 9` was never true, and **both** groups
    // stopped being marked -- including the crypto one, whose drawdown halt therefore could not
    // fire either. One group's latch silently disabled the other group's killswitch. Scoping the
    // test to the group makes each group's observation depend only on its own pairs.
    let quote_balance = view.balances.get(&rt.facts.nav_token).copied().unwrap_or(0);
    // Per group, each against its own share of the shared quote token.
    //
    // The even split is the same simplification `skew::Inventory::quote_share` already makes and
    // is named as one there: both groups draw bids from the same mUSDC and nothing yet caps the
    // sum of their liabilities against it.
    let groups: Vec<String> = rt.kills.keys().cloned().collect();
    let pairs_total = rt.cfg.pairs.len().max(1);
    for g in groups {
        let ids: Vec<u16> = rt
            .cfg
            .pairs
            .iter()
            .filter(|p| p.jump_group == g)
            .map(|p| p.pair_id)
            .collect();
        let mine: Vec<dubu_updater::risk::Position> = positions
            .iter()
            .filter(|p| ids.contains(&p.pair_id))
            .copied()
            .collect();
        // Every pair in the group, not merely one of them. A partial group would mark a NAV that
        // is missing whole positions and read the absence as a drawdown -- which is the failure
        // this switch exists to catch, arriving as a false positive.
        if mine.len() != ids.len() {
            trace_at!(
                block_work,
                target: "risk", event = "mark_skipped", group = %g,
                have = mine.len(), want = ids.len(),
                "not every pair in this group has a fair value; skipping rather than inventing one"
            );
            continue;
        }
        let share = quote_balance / pairs_total as u128 * ids.len() as u128;
        let Some(kill) = rt.kills.get_mut(&g) else {
            continue;
        };
        match kill.observe(share, &mine, now_unix()) {
            Ok((obs, halt)) => {
                trace_at!(
                    block_work,
                    target: "risk", event = "mark", group = %g,
                    nav = %obs.nav, revaluation = %obs.revaluation,
                    trade_pnl = %obs.trade_pnl, drawdown = %obs.drawdown,
                    cumulative_trade_loss = %obs.cumulative_trade_loss,
                    seeded = obs.seeded, "NAV marked"
                );
                // Shadow mode: the verdict is reported and the book keeps quoting. Read from the
                // observation and never from `halt`, which is `None` here by construction -- that
                // separation is the only thing keeping a measurement run from becoming an
                // enforcing one. Logged at error level on purpose: a would-be trip is the single
                // most important line a shadow run produces, and the whole point of the run is
                // that somebody reads it.
                if halt.is_none() {
                    if let Some(h) = &obs.would_halt {
                        error!(target: "risk", event = "halt_shadow", group = %g,
                               switch = h.label(), reason = %h, pairs = ids.len(),
                               drawdown = %obs.drawdown,
                               cumulative_trade_loss = %obs.cumulative_trade_loss,
                               "SHADOW: this WOULD have halted the group; not latching, \
                                the book is still quoting");
                    }
                }
                if let Some(h) = halt {
                    error!(target: "risk", event = "halt", group = %g, switch = h.label(),
                           reason = %h, pairs = ids.len(),
                           "KILLSWITCH TRIPPED for this group; its pairs stop quoting");
                    // The fourteen-hour silence. This trip only ever produced one line in a
                    // stream that emits several a second, and the group went on not quoting
                    // until somebody happened to look at the state file.
                    rt.notify.send(notify::Event::Halt {
                        group: g.clone(),
                        switch: h.label(),
                        reason: h.to_string(),
                    });
                }
            }
            Err(e) => {
                warn!(target: "risk", event = "mark_failed", group = %g, error = %e,
                      "could not mark NAV")
            }
        }
    }
    // Only when every group is down is there nothing left to quote.
    if rt.kills.values().all(KillSwitch::is_halted) {
        error!(target: "risk", event = "halt_all",
               "every group is halted; withdrawing quotes and exiting");
        return true;
    }

    false
}

/// One line per pair per cycle saying how the reference price was reached.
///
/// The whole cross-section is in it, rejections included, because "which venue was dropped and
/// how far out was it" is the question asked after the fact and there is nowhere else to get it.
/// `bound` says whether the MAD or the floor set the rejection threshold, which is the same as
/// saying whether the filter is in its fast-market regime or its calm one.
fn log_reference(
    symbol: &str,
    pair_id: u16,
    r: &Reference,
    snaps: &[(VenueId, dubu_updater::feed::FeedSnapshot)],
    shift: i32,
    vol: &Volatility,
    loud: bool,
) {
    trace_at!(
        loud,
        target: "feed", event = "reference", pair_id, symbol = %symbol,
        micro = %units::format_fixed(r.micro, FEED_SCALE),
        median = %units::format_fixed(r.median, FEED_SCALE),
        pool_price = ?units::to_pool_price(r.micro, shift),
        venues_used = r.venues_used(),
        venues_configured = snaps.len(),
        venues_rejected = r.rejected.len(),
        detail = %r.venue_summary(),
        dispersion_decibps = r.dispersion_decibps,
        threshold_decibps = r.threshold_decibps,
        bound = r.bound.label(),
        sigma_millibps = vol.sigma_millibps(),
        vol_samples = vol.samples(),
        "reference price"
    );
    for d in &r.rejected {
        warn!(
            target: "feed", event = "venue_rejected", pair_id, symbol = %symbol,
            venue = %d.venue, micro = %units::format_fixed(d.micro, FEED_SCALE),
            deviation_decibps = d.decibps, threshold_decibps = r.threshold_decibps,
            mad_decibps = r.dispersion_decibps, bound = r.bound.label(),
            "OUTLIER: venue dropped from the cross-section for this cycle"
        );
    }
}

/// Record one send's outcome. Split out of the send itself so a batch can reuse it.
///
/// This used to do both -- await one `eth_sendRawTransaction` and then log it -- and doing both was
/// what forced the sends to be serial. The wait is 264ms of France-to-Korea round trip and none of
/// it is ours, so five of them cost 1.323s of a 2.695s cycle on five copies of the same wait. The
/// broadcast now happens concurrently in `Sender::send_batch` and this handles each result after the
/// fact; there is no await left here, which is the point.
fn record(rt: &mut Runtime, intent: Intent, out: Result<Sent, TxError>) {
    match out {
        Ok(Sent::DryRun {
            calldata,
            would_be_hash,
            would_be_nonce,
        }) => info!(
            target: "tx", event = "dry_run", pair_id = intent.pair_id(), kind = intent.label(),
            to = %rt.cfg.chain.pool, calldata = %dubu_updater::chain::hex0x(&calldata),
            calldata_bytes = calldata.len(), would_be_hash = ?would_be_hash, would_be_nonce = ?would_be_nonce,
            "DRY RUN: would send this transaction"
        ),
        Ok(Sent::Broadcast { hash, nonce }) => {
            // Stamped on broadcast, not on confirmation. The cadence this drives is about how
            // often a fresh price leaves here; waiting for the receipt would make the interval
            // include the inclusion latency twice and turn a 330ms cadence into ~770ms.
            if matches!(intent, Intent::UpdateQuote { .. }) {
                rt.last_push.insert(intent.pair_id(), Instant::now());
            }
            // The one adaptive signal the reader takes, and it is one we generate: this
            // transaction is about to change the state the reader publishes, ~440ms from now.
            // Looking again then settles it sooner and unblocks the in-flight gate sooner.
            rt.view.nudge();
            info!(
                target: "tx", event = "sent", pair_id = intent.pair_id(), kind = intent.label(),
                tx = %hash, nonce, "transaction broadcast"
            );
        }
        Err(e) => {
            error!(
                target: "tx", event = "send_failed", pair_id = intent.pair_id(), kind = intent.label(),
                error = %e, "could not send"
            );
            // Coalesced by class on the other side, not sent one-for-one. This is the quote path,
            // so a node answering `-32003 txpool is full` refuses every send for as long as the
            // condition lasts — at 5-6 cycles a second across every pair, one message per
            // occurrence would be thousands. See `notify::ErrorLedger`.
            rt.notify.send(notify::Event::SendFailed {
                pair_id: Some(intent.pair_id()),
                kind: intent.label(),
                error: e.to_string(),
            });
        }
    }
}

/// Post a zero capacity epoch on every pair, which is how the updater role withdraws quotes.
///
/// Best effort and never fatal: this runs on the way out, often *because* something is already
/// broken, and an error here must not stop the remaining pairs from being withdrawn.
async fn withdraw_quotes(cfg: &Config, rpc: &Rpc, sender: &mut Sender) {
    for p in &cfg.pairs {
        let intent = Intent::RefreshCapacity {
            pair_id: p.pair_id,
            bid: 0,
            ask: 0,
        };
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
