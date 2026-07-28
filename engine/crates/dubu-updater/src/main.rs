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
use std::sync::{Arc, Mutex};
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
    binance, bybit, coinbase, okx, FeedStatus, VenueFeeds, VenueId, VenueWatch,
};
use dubu_updater::jump;
use dubu_updater::ladder::{self, RowInputs};
use dubu_updater::maker;
use dubu_updater::markout::{self, Markout};
use dubu_updater::now_unix;
use dubu_updater::policy::{self, CapacityDecision, Context, Decision};
use dubu_updater::quoting;
use dubu_updater::risk::{Halt, KillSwitch, Position};
use dubu_updater::serve::{self, Shared as RfqShared};
use dubu_updater::skew::{self, Inventory, Volatility};
use dubu_updater::spread;
use dubu_updater::tx::{Fees, Intent, Sender, Sent, Settled, Signer};
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
    /// The sealed block timestamp the per-block work last ran at.
    ///
    /// The cycle runs at the quote cadence now, several times per block, but some of what it does
    /// is keyed to a block rather than to a quote: reading Swap logs, and stamping a reference for
    /// markout to compare fills against. Both would repeat themselves for every cycle inside the
    /// same block -- one as wasted `eth_getLogs` calls, the other as duplicate reference samples
    /// carrying an identical timestamp.
    last_block_work: u64,
    cfg: Config,
    facts: ChainFacts,
    /// Ordinary RPC: transactions, nonce, receipts, startup metadata. Canonical.
    rpc: Rpc,
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
    kill: KillSwitch,
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
}

/// Why this cycle is running. Logged on every cycle, because "the fallback timer has been the
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
    let rpc = Rpc::new("rpc", &cfg.chain.rpc_url, &cfg.chain)?;

    // Reads rotate. A read is a question about one block and any node can answer it, so the pool's
    // budget is the sum of its keys rather than the smallest of them.
    let mut read_urls = vec![cfg.chain.flashblocks_rpc_url.clone()];
    read_urls.extend(cfg.chain.read_rpc_urls.iter().cloned());
    let flash = Rpc::pooled("flashblocks", &read_urls, Selection::Rotate, &cfg.chain)?;
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
        min_venues = cfg.feed.min_venues,
        mad_k = cfg.feed.mad_k,
        mad_floor_bps = cfg.feed.mad_floor_bps,
        max_dispersion_bps = cfg.feed.max_dispersion_bps,
        gamma = cfg.skew.gamma,
        vol_horizon_secs = cfg.skew.vol_horizon_secs,
        vol_tau_ms = cfg.skew.vol_tau_ms,
        skew_cap_bps = cfg.skew.max_positive_bps,
        skew_floor_bps = -i32::from(cfg.skew.max_negative_bps),
        spread_vol_coefficient = cfg.spread.vol_coefficient,
        spread_max_half_spread_bps = cfg.spread.max_half_spread_bps,
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

    // One task per venue a pair actually names. A venue is enabled by being used, not by a
    // separate switch, so there is no way to configure a venue that connects and quotes nothing.
    let venues = cfg.venues();
    let feeds = Arc::new(VenueFeeds::new(
        &venues,
        Duration::from_millis(cfg.feed.stale_after_ms),
    ));
    let mut feed_tasks = Vec::new();
    for venue in &venues {
        let symbols = cfg.venue_symbols(*venue);
        let client: Box<dyn MarketFeed> = match venue {
            VenueId::Binance => Box::new(binance::Client::new(&symbols)),
            VenueId::Okx => Box::new(okx::Client::new(&symbols)),
            VenueId::Bybit => Box::new(bybit::Client::new(&symbols)),
            VenueId::Coinbase => Box::new(coinbase::Client::new(&symbols)),
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
        feed_tasks.push(tokio::spawn(dubu_updater::feed::ws::run(
            cfg.feed.clone(),
            cfg.feed.urls.get(*venue).to_string(),
            client,
            shared,
            shutdown_rx.clone(),
        )));
    }

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
    let vol: BTreeMap<u16, Volatility> = cfg
        .pairs
        .iter()
        .map(|p| (p.pair_id, Volatility::new(cfg.skew.vol_config())))
        .collect();

    // Both trip bounds come from the pair's OWN configuration — the floor is its half-spread and
    // the ceiling is `half_spread + width/2`, its absorption limit — which is what lets one global
    // `sigma_k` be correct across two instruments whose measured sigmas differ by 3x.
    let jump_bounds: Vec<(u16, jump::Bounds)> = cfg
        .pairs
        .iter()
        .map(|p| {
            (
                p.pair_id,
                jump::Bounds::new(
                    p.jump_floor_bps.unwrap_or(p.half_spread_bps),
                    p.half_spread_bps,
                    p.width_bps,
                ),
            )
        })
        .collect();
    let jump_book = jump::Book::new(
        &jump_bounds,
        cfg.jump.params(&cfg.skew),
        cfg.jump.scope,
        cfg.jump.enabled,
    );
    for (id, b) in &jump_bounds {
        info!(
            target: "jump", event = "bounds", pair_id = id,
            enabled = cfg.jump.enabled, scope = cfg.jump.scope.label(),
            sigma_k = cfg.jump.sigma_k,
            floor_bps_e2 = b.floor_bps_e2, ceiling_bps_e2 = b.ceiling_bps_e2,
            cooloff_secs = cfg.jump.cooloff_secs, scan_interval_ms = cfg.jump.scan_interval_ms,
            "jump trip threshold is clamp(sigma_k * sigma, half_spread, half_spread + width/2)"
        );
    }
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
    tokio::spawn(view::run(
        Arc::new(reader),
        Arc::new(flash),
        Arc::clone(&view),
        Arc::clone(&health),
        view::Pacing::default(),
    ));
    let cfg_pool = cfg.chain.pool;
    let mut rt = Runtime {
        last_block_work: 0,
        cfg,
        facts,
        rpc,
        view: Arc::clone(&view),
        feeds,
        watch: VenueWatch::new(),
        heads,
        vol,
        jump: jump_book,
        withdraw_fees,
        sender,
        kill,
        health,
        swaps: SwapWatch::new(cfg_pool),
        markout: Markout::new(),
        rfq_shares_pool_inventory: rfq_maker == cfg_pool,
        rfq: rfq_shared,
    };

    wait_for_feed(&rt).await;
    wait_for_first_head(&rt).await;

    let limit = if args.once { Some(1) } else { args.cycles };
    let code = quote_loop(&mut rt, limit, head_rx, shutdown_rx).await;

    let _ = shutdown_tx.send(true);
    for task in feed_tasks {
        let _ = tokio::time::timeout(Duration::from_secs(3), task).await;
    }
    let _ = tokio::time::timeout(Duration::from_secs(3), heads_task).await;
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
        let worst = rt
            .cfg
            .pairs
            .iter()
            .map(|p| {
                rt.feeds
                    .snapshots(&p.symbol, now)
                    .iter()
                    .filter(|(_, s)| s.status.is_live())
                    .count()
            })
            .min()
            .unwrap_or(0);
        if worst >= usize::from(rt.cfg.feed.min_venues) {
            info!(target: "feed", event = "ready", venues_live = worst,
                  min_venues = rt.cfg.feed.min_venues, "every configured symbol has quorum");
            return;
        }
        if Instant::now() >= deadline {
            warn!(target: "feed", event = "not_ready", venues_live = worst,
                  min_venues = rt.cfg.feed.min_venues,
                  "starting the loop without quorum on every symbol; pushes will be gated until it arrives");
            return;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
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
            let _ = rt.kill.halt(&halt, now_unix());
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
        // Whichever comes first: the quote clock, or the fallback that bounds how long a cycle
        // can go without one when heads are down. With the quote clock at 200ms the fallback never
        // wins, and that is the point -- it stays as the bound it always was rather than as the
        // thing that decides the cadence.
        let tick_at = cycle_start + quote_every;
        let deadline = tick_at.min(cycle_start + fallback);
        wake = 'wait: loop {
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
        let Ok(reference) = fair_value::combine(&quotes, &rt.cfg.feed.mad_params()) else {
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
        Err(e) => error!(
            target: "tx", event = "withdraw_failed", pair_id, why, error = %e,
            "COULD NOT WITHDRAW QUOTES; the next cycle will re-assert it"
        ),
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
async fn scan_fills(rt: &mut Runtime, head: &heads::HeadSnapshot) {
    let Some(confirmed) = head.last.map(|h| h.number) else {
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
        let ref_at_fill = match rt.markout.reference_at(log.pair_id, log.at_secs) {
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
    let chain_now = head.last.map_or_else(now_unix, |h| h.timestamp);

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
        let jump_withdrawn = rt.jump.withdrawn(pair.pair_id);
        if jump_withdrawn
            && (snap.bid_capacity != 0 || snap.ask_capacity != 0)
            && !rt.sender.at_capacity(pair.pair_id)
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

        let reference = fair_value::combine(&quotes, &rt.cfg.feed.mad_params());
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
                    min_venues = rt.cfg.feed.min_venues,
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
        if let Some(rfq) = &rt.rfq {
            match fair {
                // NOT `base_balance` / `quote_balance`. Those are the POOL's, and `PmmSettle`
                // pulls from the MAKER — different address, and the binding constraint is usually
                // the allowance rather than the balance. Sizing an RFQ quote against the pool's
                // inventory means signing orders that revert inside a `transferFrom`, which costs
                // the taker gas and leaves nothing useful in the trace.
                Some(f) => rfq.publish(quoting::MarketState {
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
                None => rfq.retire(pair.pair_id),
            }
        }

        // --- the half-spread ---------------------------------------------------------------
        //
        // `half_spread = min(s0 + s1 * sigma, cap) + degraded_extra`, from the SAME sigma the
        // skew below uses. Computed before the skew, because `min_price_cap_bps` needs the spread
        // that will actually be posted: a wider spread pushes the bid target lower and therefore
        // leaves less room to skew down, and clamping against the unwidened value would let the
        // skew push a row under the pair's `minPrice` and have it refused outright.
        let spread = spread::compute(
            pair.half_spread_bps,
            sigma_millibps,
            degraded_extra,
            &rt.cfg.spread.params(),
        );
        let half_spread = spread.half_spread_bps;
        // Every row, every cycle. Without these five fields, back-solving `s1` from history later
        // is guesswork — `vol_decibps` next to `capped` is what says whether the model or the
        // ceiling has been doing the deciding.
        trace_at!(
        block_work,

                    target: "spread", event = "half_spread", pair_id = pair.pair_id, symbol = %pair.symbol,
                    s0_bps = spread.s0_bps,
                    sigma_millibps = spread.sigma_millibps,
                    sigma_horizon_secs = rt.cfg.skew.vol_horizon_secs,
                    vol_samples,
                    s1 = rt.cfg.spread.vol_coefficient,
                    vol_decibps = spread.vol_decibps,
                    vol_bps = spread.vol_bps(),
                    vol_scaled_bps = spread.vol_scaled_bps,
                    capped = spread.capped,
                    cap_bps = rt.cfg.spread.max_half_spread_bps,
                    degraded_extra_bps = spread.degraded_extra_bps,
                    half_spread_bps = spread.half_spread_bps,
                    absorption_bps = u32::from(spread.half_spread_bps) + u32::from(pair.width_bps) / 2,
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
                target_ppm: pair.target_base_share_ppm(),
            };
            let floor_cap = skew::min_price_cap_bps(f, meta.min_price, half_spread);
            let s = skew::compute(
                &inventory,
                sigma_sq,
                sigma_millibps,
                &rt.cfg.skew.params(),
                floor_cap,
            );

            // Every row, every cycle. This is the input to tuning gamma later, and without it
            // gamma is guesswork: `raw_decibps` next to `applied_bps` is what says whether the
            // model or the clamp has been doing the deciding.
            trace_at!(
block_work,

                target: "skew", event = "skew", pair_id = pair.pair_id, symbol = %pair.symbol,
                imbalance_ppm = s.imbalance_ppm,
                target_ppm = inventory.target_ppm,
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
                half_spread_bps: half_spread,
                width_bps: pair.width_bps,
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
            halted: rt.kill.is_halted(),
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

    for intent in sends {
        emit(rt, intent).await;
    }

    if block_work {
        scan_fills(rt, head).await;
    }

    // The killswitches. Skipped entirely when any pair has no fair value: marking inventory
    // against a price we do not have would be an invention, and an invented NAV is worse than
    // no observation at all.
    if positions.len() == rt.cfg.pairs.len() {
        let quote_balance = view.balances.get(&rt.facts.nav_token).copied().unwrap_or(0);
        match rt.kill.observe(quote_balance, &positions, now_unix()) {
            Ok((obs, halt)) => {
                trace_at!(
                block_work,

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
            Err(e) => {
                warn!(target: "risk", event = "mark_failed", error = %e, "could not mark NAV")
            }
        }
    } else {
        info!(target: "risk", event = "mark_skipped", have = positions.len(), want = rt.cfg.pairs.len(),
              "not every pair has a fair value; skipping the NAV observation rather than inventing one");
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

async fn emit(rt: &mut Runtime, intent: Intent) {
    match rt.sender.send(&rt.rpc, intent).await {
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
            // The one adaptive signal the reader takes, and it is one we generate: this
            // transaction is about to change the state the reader publishes, ~440ms from now.
            // Looking again then settles it sooner and unblocks the in-flight gate sooner.
            rt.view.nudge();
            info!(
                target: "tx", event = "sent", pair_id = intent.pair_id(), kind = intent.label(),
                tx = %hash, nonce, "transaction broadcast"
            );
        }
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
