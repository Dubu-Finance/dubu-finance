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
//! A `newHeads` notification and a fallback timer race in one `select!`, so there is no mode flag
//! to get stuck in whatever the subscription does. Heads going quiet is reported but never taken as
//! the chain being down: [`ChainHealth`] escalates on the block number as well as on request
//! success, so a silent socket over a live chain keeps quoting.
//!
//! The updater role cannot call `pause` — that is the guardian's, on separate hardware. Withdrawal
//! is `refreshCapacity(pairId, 0, 0)`, which makes every quote path in `PropPool._outFor` return
//! zero. The backstop behind it is the pool's own `maxStaleSecs`, which is why the heartbeat must
//! sit inside that window and why `chain::verify_against_chain` refuses to start if it does not.

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
    binance, bybit, coinbase, hyperliquid, okx, pyth, FeedStatus, VenueFeeds, VenueId, VenueWatch,
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
/// Per-pair lines at 5-6Hz drown the ones reporting a send. Sampling on `block_work` gives one full
/// trace per sealed block; `RUST_LOG=debug` still shows every tick.
macro_rules! trace_at {
    ($loud:expr, $($arg:tt)*) => {
        if $loud { tracing::info!($($arg)*) } else { tracing::debug!($($arg)*) }
    };
}

/// How long the exit path waits for the alerting task to get its last message out.
///
/// The events worth waking somebody for are followed within milliseconds by `EXIT_HALTED`, so
/// without a bounded wait the batch dies inside its window with the process. Two seconds is spent
/// only while shutting down and is shorter than a systemd restart delay.
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
            // No `--transmit` counterpart: this override can only make the bot safer, and
            // broadcasting stays a decision made in a reviewed config file.
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

/// Not `#[tokio::main]`, because of the `.env` load.
///
/// `std::env::set_var` is only sound while the process is single-threaded, and `#[tokio::main]`
/// builds the multi-thread runtime before the body runs. Building it by hand pins the ordering:
/// parse, load `.env`, start logging, and only then spin up anything concurrent.
fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("dubu-updater: {e}");
            std::process::exit(EXIT_STARTUP);
        }
    };

    // `.env` before the config, because the config's endpoint URLs are `${VAR}` templates expanded
    // during parsing. Real environment variables win over the file; see `config::load_dotenv`.
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
    hedge: Option<Arc<HedgeShared>>,
    /// The sealed block timestamp the per-block work last ran at. The cycle runs several times per
    /// block, but stamping a markout reference and reading Swap logs are keyed to the block, so
    /// repeating them within one costs a wasted `eth_getLogs` and a duplicate reference sample.
    last_block_work: u64,
    /// When each pair was last sent, on our own clock. The chain cannot answer this below a second
    /// -- `updatedAt` is a uint32 of seconds -- so `policy::Trigger::Cadence` measures from here.
    last_push: BTreeMap<u16, Instant>,
    cfg: Config,
    facts: ChainFacts,
    /// Ordinary RPC: transactions, nonce, receipts, startup metadata. Canonical.
    rpc: Rpc,
    /// The read pool, held only so the cycle can log its counters. A *different* pool from
    /// [`Self::rpc`]: reads pin to the keyless flashblocks endpoint and fall through to Nodit,
    /// writes go the other way, and reporting one as the other is what this field prevents.
    read_rpc: Arc<Rpc>,
    /// The chain state, published by [`dubu_updater::chain::view::run`] rather than fetched here,
    /// so the cycle is not paced by the read.
    view: Arc<ViewSlot>,
    /// Every configured venue's feed state. One socket, one reconnect loop and one liveness
    /// state per venue, so one venue failing cannot take another down.
    feeds: Arc<VenueFeeds>,
    /// Edge-triggers per-venue liveness, so losing a venue is an event rather than one fewer entry
    /// in a cross-section that still looks confident.
    watch: VenueWatch,
    /// State of the `newHeads` subscription that drives the loop.
    heads: Arc<HeadShared>,
    /// Return-variance estimator per pair. Keyed by pair id rather than positionally: a `Vec` in
    /// lock-step with `cfg.pairs` is an invariant nothing enforces, and getting it wrong sizes one
    /// pair's skew off another pair's volatility.
    vol: BTreeMap<u16, Volatility>,
    /// Jump detection and the cool-off state machine, one detector per pair plus the scope rule.
    /// Fed by [`jump_scan`], which runs both inside the cycle and between cycles.
    jump: jump::Book,
    /// Fees for a jump withdrawal, and only for one.
    /// See [`dubu_updater::tx::Sender::send_with_fees`].
    withdraw_fees: Fees,
    sender: Sender,
    /// One per correlation group. A group's loss budget is its own; see where these are loaded.
    kills: BTreeMap<String, KillSwitch>,
    /// Shared with the reader task, which is where read successes and failures come from.
    health: Arc<Mutex<ChainHealth>>,
    /// Follows our own `Swap` logs, off the canonical RPC rather than the flashblocks one: markout
    /// anchors to block timestamps, so a fill read out of a preconfirmation that later reorganises
    /// is a phantom counterparty score.
    swaps: SwapWatch,
    /// Who has been trading against us and how it went. Fed by [`scan_fills`].
    markout: Markout,
    /// The state the RFQ endpoint quotes from, or `None` when the RFQ leg is off.
    rfq: Option<Arc<RfqShared>>,
    /// True when the RFQ maker and the pool are the same account, so the curve's epoch and an RFQ
    /// order draw on one balance. Normally false — `PmmSettle` pulls from the maker, and the two
    /// are separate balance sheets whose commitments must not be netted against each other.
    rfq_shares_pool_inventory: bool,
    /// Pushes fills and anything that went wrong to Telegram. A `Notifier` rather than an
    /// `Option<Notifier>` so the disabled state is a no-op no call site has to remember; nothing
    /// here may ever wait on it.
    notify: Notifier,
    /// Realised profit and loss since the last digest, and when that digest was.
    pnl: notify::PnlWindow,
    pnl_at: Instant,
}

/// One killswitch per correlation group, each latching to its own file so an operator clears one
/// group without resuming another.
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
    // Shadow mode is temporary by design and fails by being left on: the book then has no drawdown
    // halt while every other log line looks normal.
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
    /// The head the read pool reported, `cfg.chain.flashblocks_rpc_url`.
    read_block: u64,
}

/// Whether the chain is answering right now on **both** pools. `None` means do not act on it.
///
/// Both, because the pools fail independently: clearing the latch on the write pool alone leaves
/// the reader failing, so [`ChainHealth`] walks back to `Down` and re-latches `halt_after_secs`
/// later. Probed concurrently so both are sampled over the same window; in sequence they would
/// compare heads read four seconds apart and read a normal seal as a disagreement.
async fn chain_is_answering(rpc: &Rpc, flash: &Rpc) -> Option<ProbedHeads> {
    let (write, read) = tokio::join!(probe_head(rpc), probe_head(flash));
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
/// Two rather than one, because a single answered read only proves the endpoint is reachable: one
/// serving a frozen number over a stopped chain looks identical. At-or-ahead rather than strictly
/// ahead, because nothing guarantees a seal inside this gap, and being wrong that way is
/// self-correcting since [`ChainHealth`] re-decides liveness from the loop's own reads.
///
/// Every failure names its pool: "flashblocks is down" and "the canonical RPC is down" call for
/// different responses. [`Rpc::url`] is the redacted form; there is no accessor for the real one.
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
        // The ceiling is the absorption limit and the floor the noise gate. Equal collapses the
        // clamp to a constant, at which point normal volatility trips every pair at once.
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
        // `None` is the polled venue: the websocket driver cannot carry it, and everything else —
        // the shared state, the staleness window, the backoff — is the same.
        let client: Option<Box<dyn MarketFeed>> = match venue {
            VenueId::Binance => Some(Box::new(binance::Client::new(&symbols))),
            VenueId::Okx => Some(Box::new(okx::Client::new(&symbols))),
            VenueId::Bybit => Some(Box::new(bybit::Client::new(&symbols))),
            VenueId::Coinbase => Some(Box::new(coinbase::Client::new(&symbols))),
            VenueId::Hyperliquid => Some(Box::new(hyperliquid::Client::new(&symbols))),
            VenueId::Pyth => None,
        };
        let Some(shared) = feeds.venue(*venue) else {
            continue;
        };
        let mapping = symbols
            .iter()
            .map(|(v, c)| format!("{v}->{c}"))
            .collect::<Vec<_>>()
            .join(",");
        info!(
            target: "startup", event = "venue", venue = %venue,
            url = %cfg.feed.urls.get(*venue), symbols = %mapping,
            "market data venue configured"
        );
        let url = cfg.feed.urls.get(*venue).to_string();
        tasks.push(match client {
            Some(c) => tokio::spawn(dubu_updater::feed::ws::run(
                cfg.feed.clone(),
                url,
                c,
                shared,
                shutdown_rx.clone(),
            )),
            None => tokio::spawn(dubu_updater::feed::pyth::run(
                cfg.feed.clone(),
                url,
                pyth::Client::new(&symbols),
                shared,
                shutdown_rx.clone(),
            )),
        });
    }
    assert_eq!(tasks.len(), venues.len(), "every venue got a task");
    (feeds, tasks)
}

/// Why this cycle is running. Logged on every cycle, because "the fallback timer has been the only
/// thing waking this loop for an hour" is invisible otherwise.
#[derive(Debug, Clone, Copy)]
enum Wake {
    /// First cycle, before any head could have arrived.
    Startup,
    /// A `newHeads` notification.
    Head(u64),
    /// The fallback timer. Normal only while heads are absent.
    Fallback,
    /// The quote clock, and the usual case: the posted spread has to cover the reference's drift
    /// over the re-quote interval, so pacing on heads would let the block time set the spread.
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

    // The write path is a pool but `Pin`, not `Rotate`: nonce, submit and receipt must all reach
    // one node's view of the pending set, and a nonce read from a node that has not seen the
    // previous send leaves a gap nothing fills. It is a pool at all because a quota belongs to a
    // key rather than to a node, so one exhausted key would otherwise stop sends outright.
    let mut write_urls = vec![cfg.chain.rpc_url.clone()];
    write_urls.extend(cfg.chain.write_rpc_urls.iter().cloned());
    let rpc = Rpc::pooled("rpc", &write_urls, Selection::Pin, &cfg.chain)?;

    // Reads are pinned for the opposite reason: the first endpoint is GIWA's public flashblocks
    // RPC, keyless and unmetered at ~3 req/s. Rotating past it spends keyed Nodit quota on requests
    // a free endpoint would have answered, and exhausting those keys takes the websocket and the
    // sealed clock down with them. So the pool moves to a key only when the free endpoint fails.
    let mut read_urls = vec![cfg.chain.flashblocks_rpc_url.clone()];
    read_urls.extend(cfg.chain.read_rpc_urls.iter().cloned());
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

    // A missing key is fatal only when something is going to be broadcast: a dry run has to work
    // with no key material present at all.
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

    // One killswitch per correlation group, not one for the book: a group is the claim about what
    // moves together, so it is what a loss budget is shared across. A single instance would make an
    // equity drawdown ETH's problem.
    let mut kills = load_kills(&cfg)?;

    let mut sender = Sender::new(
        signer,
        cfg.chain.chain_id,
        cfg.chain.pool,
        &cfg.tx,
        cfg.tx.max_fee_wei()?,
        cfg.tx.max_priority_fee_wei()?,
    );

    // Sends bypass `rpc_url` when a submit endpoint is configured. A local node that forwards to
    // the sequencer can accept a transaction, return its hash and never deliver it, and no failover
    // catches that because nothing reported a failure.
    //
    // `rpc_url` sits behind it rather than the sequencer standing alone, because the sequencer is
    // remote and the local node is not: without this a network blip there stops every send, which
    // is a worse failure than the forwarding it replaced. The pool's own rule makes this safe on a
    // transmit path -- only an endpoint fault steps to the next endpoint, while a node-level
    // refusal returns as-is, so a rejected transaction is never broadcast twice.
    if let Some(url) = cfg.chain.submit_rpc_url.as_ref() {
        let urls = [url.clone(), cfg.chain.rpc_url.clone()];
        sender.set_submit_rpc(Rpc::pooled("submit", &urls, Selection::Pin, &cfg.chain)?);
        info!(
            target: "startup", event = "submit_endpoint", url = %url,
            fallback = %cfg.chain.rpc_url,
            "eth_sendRawTransaction goes here; nonce and receipt stay on rpc_url"
        );
    }

    // Every URL here is an `EndpointUrl`, whose `Display` is redacted: the API key is a path
    // segment, and there is no spelling of this line that would print it.
    info!(
        target: "startup",
        event = "configured",
        submits_directly = sender.submits_directly(),
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

    // Started before the other tasks: the first thing worth reporting — a group that came up
    // already latched — happens on the next few lines. Absent credentials leave it inert rather
    // than failing, because alerting must never be able to stop a live trading system.
    let notify = Notifier::from_env();

    // Stay-down. A restart is the first thing an operator does, and it must not resume a book that
    // a killswitch took down.
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
            // Pushed as well as logged, because this is a *partial* book: the other groups quote
            // on, so nothing about the process looks wrong from outside.
            notify.send(notify::Event::StayDown {
                group: (*g).clone(),
                reason: kills[*g].halt_reason().unwrap_or("(unrecorded)").into(),
                exiting: false,
            });
        }
    }

    // ... except when the only thing holding the book down is a chain outage that has since ended.
    // A liveness latch is cleared at startup rather than in the loop, because the process that
    // latched has already exited by the time the chain returns; without this, systemd feeds the
    // process back into the same outage until `StartLimitBurst` is spent, a nonce per pair a pass.
    //
    // Two conditions, both required: every latched group must be liveness and nothing else — a
    // `Bleed` or `LossBudget` anywhere in the set is the book losing money and wants a human — and
    // both RPC pools must be demonstrably moving, per `chain_is_answering`.
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
                    // Its own event, so the moment the latch went away stays findable.
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
                    // A killswitch clearing itself is told to the operator, not left to be found.
                    notify.send(notify::Event::LatchCleared {
                        group: group.clone(),
                        reason: reason.clone(),
                    });
                }
            }
            // Which pool fell short is in the `liveness_probe_failed` line above; this is the
            // decision, on its own event.
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
        // Pushed, not only logged: this branch exits, so anything left in the log goes unread.
        notify.send(notify::Event::StayDown {
            group: "all".into(),
            reason: kill.halt_reason().unwrap_or("(unrecorded)").into(),
            exiting: true,
        });
        withdraw_quotes(&cfg, &rpc, &mut sender).await;
        // Before the return: a batch still inside its window would go with the process.
        notify.flush(NOTIFY_FLUSH).await;
        return Ok(EXIT_HALTED);
    }

    // Seed the nonce once, here, and never on the hot path again. This is the only nonce read there
    // is: `eth_getTransactionCount` is derived from a local pool that holds at most 16 of our
    // transactions and silently drops the rest, so a per-send read answers well behind the truth.
    // See `tx`. A failure is not fatal — the first reservation reads it instead.
    match sender.seed_nonce(&rpc).await {
        Ok(n) => info!(target: "startup", event = "nonce_seeded", nonce = n,
                       "next nonce read from the node once; the send path tracks it from here"),
        Err(e) => warn!(target: "startup", event = "nonce_seed_failed", error = %e,
                        "could not seed the nonce; the first reservation will read it"),
    }

    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let (feeds, feed_tasks) = spawn_feeds(&cfg, &shutdown_rx);

    // A `watch` channel rather than an `mpsc` because it coalesces: two heads landing during one
    // cycle should produce one more cycle against the newer state, not two.
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

    // With the RFQ leg on, the batch also carries what the maker can deliver — balance and
    // allowance to `PmmSettle`, per token — so it is answered at the same block as the inventory it
    // is compared against.
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
    // in-flight gate a send has landed. With no signing key there is nothing to gate, so the extra
    // request is not made.
    let reader = match sender.address() {
        Some(a) => reader.with_sender(a),
        None => reader,
    };
    let vol: BTreeMap<u16, Volatility> = cfg
        .pairs
        .iter()
        .map(|p| (p.pair_id, Volatility::new(cfg.skew.vol_config())))
        .collect();

    // Both trip bounds come from the pair's own configuration — floor its half-spread, ceiling its
    // absorption limit `half_spread + width/2` — which is what lets one global `sigma_k` be correct
    // across instruments whose measured sigmas differ by 3x.
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

    // The chain read gets its own clock, so its cadence no longer ceilings how often the pool can
    // re-price. The reader owns the pool; the handle is kept back only for the counters.
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
        pnl: notify::PnlWindow::default(),
        pnl_at: Instant::now(),
    };

    // The gas in a digest is a fall in the sender's balance across the window, so the first window
    // needs a reading to fall from. Without it the first digest reports no gas at all, and after a
    // restart the first digest is often the only one.
    if let Some(balance) = sender_balance(&rt).await {
        rt.pnl.open(balance);
    }

    wait_for_feed(&rt).await;
    wait_for_first_head(&rt).await;

    // Built after the feeds have had a moment: the band width is derived from measured sigma, and
    // zero sigma derives a zero-width band that crosses on every fill. The fallback covers a cold
    // estimator.
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
        // Probed only if a pair routes there, and a failure costs only those pairs: dropping the
        // whole leg on one venue's error would leave pairs that never touch it unhedged.
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

    // The hedge gets its own task: its pass is six external HTTPS round trips and nothing in the
    // quote path reads it synchronously. Nothing else is shared — the leg owns its bands, positions
    // and venue clients outright, the only thing crossing back is the signed position the skew
    // needs, and pool balances come from the reader's slot.
    let hedge_task = hedge_leg.map(|leg| {
        let shared = Arc::new(HedgeShared::default());
        rt.hedge = Some(Arc::clone(&shared));
        // Only the base token per pair, because that is all the pass reads out of `ChainFacts`.
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
    // Bounded like the feeds, but awaited for a stronger reason: this is the only task that can
    // have a real order in flight, and dropping it mid-`venue.market()` leaves a crossing whose
    // fate nobody knows.
    if let Some(task) = hedge_task {
        let _ = tokio::time::timeout(Duration::from_secs(3), task).await;
    }
    // Every path out of the loop lands here. Bounded and best-effort: this must not be able to
    // delay a shutdown already under way.
    rt.notify.flush(NOTIFY_FLUSH).await;
    Ok(code)
}

/// Give the sockets a chance to reach quorum before the first cycle.
///
/// Waits for quorum, not for every venue: blocking on the slowest venue would make startup as slow
/// as the worst endpoint, and a missing venue is a degradation rather than an outage.
async fn wait_for_feed(rt: &Runtime) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let now = Instant::now();
        // Each pair against its own quorum: a pair whose `PairConfig::venues_min` is one would
        // never satisfy `feed.venues_min`, so every boot would burn the full deadline.
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
                  "starting the loop without quorum on every symbol; pushes will be gated \
                   until it arrives");
            return;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Compare this machine's clock to the chain's, and say so loudly if they disagree.
///
/// The RFQ expiry is a contract between three clocks nothing synchronises: the maker stamps
/// `expiry` from here, the aggregator refuses anything with under a second left on its own clock,
/// and the settler enforces it against `block.timestamp`. Their shared budget is `ttl_secs` minus
/// the aggregator's headroom minus a block, which is zero at a 2s TTL, so a second of host skew
/// refuses every quote as expired.
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

/// Give the subscription a moment to establish, so the opening cycle is driven by a real head.
///
/// Bounded, and not fatal on timeout: a bot that cannot subscribe must still start and quote on the
/// fallback timer, since refusing to start would turn a degraded mode into an outage.
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
    // Edge-triggered, so a sustained outage logs once rather than once per cycle.
    let mut watchdog_open = false;
    let mut cycles: u64 = 0;

    // Mark whatever head arrived during startup as seen. Without this the first cycle runs as
    // `Startup` and is immediately followed by a duplicate for the head it already covered.
    head_rx.borrow_and_update();

    // Registered ONCE and polled by reference, never rebuilt inside the wait loop: a fresh `Signal`
    // only receives what arrives after it exists, so rebuilding would open that many windows in
    // which a SIGTERM lands on nobody.
    let signal = wait_for_signal();
    tokio::pin!(signal);

    'outer: loop {
        let cycle_start = Instant::now();
        cycles += 1;

        // The head watchdog reports that heads stopped; it does NOT conclude the chain stopped.
        // `ChainHealth` answers that from the block number too, so a silent socket over a live
        // chain keeps quoting.
        let head = rt.heads.snapshot(cycle_start);
        if head.status.is_live() {
            if watchdog_open {
                info!(target: "heads", event = "watchdog_clear",
                      head = ?head.last.map(|h| h.number),
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

        // Whatever the reader task last published, up to one poll interval old; see `chain::view`
        // for why that costs nothing.
        if let Some(v) = rt.view.latest() {
            // Before anything reads the in-flight gate: everything below this nonce is on chain and
            // must stop occupying a slot now rather than whenever a receipt call gets a turn.
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
            // One message for the whole book, since every group would carry an identical reason.
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

        // The wake, with the jump fast lane underneath it. A head and the timer race in one
        // `select!` with no mode flag between them, so the loop cannot get stuck in the wrong mode.
        // The third arm is the fast lane: waiting for the next head would put mean jump-detection
        // latency at half a block, so it scans in-memory feed snapshots on its own interval and
        // does NOT count as a wake, never consuming a chain read or a cycle.
        //
        // As configured `Wake::Fallback` is unreachable, because `quote_interval_ms` is below the
        // validated floor on `fallback_poll_interval_ms` and `deadline` is therefore always
        // `tick_at`. Raising `quote_interval_ms` would re-engage it; that is a config decision.
        let tick_at = cycle_start + quote_every;
        let deadline = tick_at.min(cycle_start + fallback);
        wake = 'wait: loop {
            // Shutdown and SIGTERM are polled BEFORE the deadline test, unconditionally, and that
            // ordering is load-bearing: a cycle longer than `quote_interval_ms` finds the deadline
            // already past and breaks on the first test, so a signal reached only through the
            // `select!` below is never observed and `withdraw_quotes` never runs. The always-ready
            // third arm keeps this non-blocking; `biased` orders the other two first.
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
                    // The subscription task is gone for good; the timer becomes the only driver.
                    // Sleeping to `next_scan` keeps the fast lane running, which must not depend on
                    // the chain telling us anything.
                    Err(_) => {
                        let until = next_scan.saturating_duration_since(Instant::now());
                        tokio::time::sleep(until).await;
                    }
                },
                () = tokio::time::sleep_until(next_scan.into()) => {}
            }
            jump_scan(rt).await;
        };
    }

    info!(target: "loop", event = "shutdown", "shutdown signal received; withdrawing quotes");
    // Before the quotes go: the window that has been accumulating since the last digest is only in
    // memory, and a deploy is a shutdown. At an hourly cadence a book that restarts more often than
    // that would never send a digest at all and would lose every fill it had recorded.
    flush_pnl(rt).await;
    withdraw_quotes(&rt.cfg, &rt.rpc, &mut rt.sender).await;
    0
}

/// Push whatever the current profit-and-loss window holds, if it holds anything.
///
/// Reads the balance even when there is nothing to report so the next window does not attribute two
/// windows of gas to one; see the call in [`run_cycle`].
async fn flush_pnl(rt: &mut Runtime) {
    let since = rt.pnl_at.elapsed();
    rt.pnl_at = Instant::now();
    let empty = rt.pnl.is_empty();
    let balance = sender_balance(rt).await;
    let pnl = rt.pnl.take(since.as_secs(), balance);
    if !empty {
        rt.notify.send(notify::Event::Pnl(pnl));
    }
}

/// The fast lane: sample the reference on every pair, decide whether it jumped, and withdraw
/// immediately if it did.
///
/// No chain read, because waiting for the multicall adds a round trip to a race. No policy: the
/// gates in [`policy::evaluate_capacity`] exist to stop the bot acting on a state it does not
/// understand, and a withdrawal is correct in every such state — `PushInFlight` especially must not
/// delay it, since the two calls touch different storage words. Only on the edge, since
/// `refreshCapacity(pair, 0, 0)` against a pair already at zero only burns a nonce. At a raised
/// priority fee, because the sequencer orders by fee and the counterparty outbids quote traffic.
///
/// It still inherits the nonce queue, so an earlier transaction that never lands delays it. The
/// mitigation would be a second signing key, which does not exist.
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
        // No reference means no observation. The anchor then ages, and a gap past
        // `vol_max_sample_ms` trips as `feed_gap` on recovery — correct, because the pool spent the
        // outage armed behind a fixed ladder.
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
                // A jump on one pair is information about the other, and the asymmetry decides it:
                // contagion's false positive costs one cool-off of spread, its false negative the
                // full pick-off on the other pair.
                for id in rt.jump.contagion(pair_id, now) {
                    warn!(target: "jump", event = "contagion", pair_id = id, origin = pair_id,
                          scope = rt.jump.scope().label(),
                          "withdrawing this pair too: a jump on the other pair is information \
                           about it");
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
    // The RFQ leg goes dark first, and synchronously. `refreshCapacity(pair, 0, 0)` stops only the
    // curve; without this, signed orders stay fillable at pre-jump prices for their whole TTL.
    // Retiring costs no transaction, because an order that was never signed cannot be filled.
    //
    // It does not cancel orders already signed: `PmmSettle.cancelNonces` costs a transaction and a
    // block, which loses the same race the withdrawal does. A short TTL is the real control.
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
            // Coalesced, and it matters more here than on the quote path: the next cycle re-asserts
            // this, so a persistent failure reproduces at the cycle rate against an armed pool.
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
/// it to ask what that account can deliver. Asking the *pool* instead is the mistake this pairing
/// exists to prevent.
struct RfqLeg {
    shared: Option<Arc<RfqShared>>,
    maker: Address,
}

/// Starts the RFQ maker endpoint, if one is configured.
///
/// No `[rfq]` section means the leg is off and the aggregator routes AMM-only. Anything present but
/// wrong is fatal rather than degraded, because every failure mode of a misconfigured maker is
/// silent from this side: a wrong `pmm_settle` signs orders against a domain nothing verifies, and
/// the process looks healthy while answering requests no taker can fill. The key is loaded before
/// the listener binds, so a bad key fails at startup.
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

    // Both want checking against the chain: the maker address is what `PmmSettle` must have an
    // allowance from, the separator is what a taker recomputes, and a mismatch in either produces
    // unfillable quotes and no error on this side.
    let params = rfq.params(cfg.skew.vol_horizon_secs)?;
    info!(
        target: "rfq", event = "enabled", maker = %key.address(), pmm_settle = %pmm_settle,
        chain_id = cfg.chain.chain_id,
        domain_separator = %format!("{:#x}", key.domain_separator()),
        base_half_spread_e2 = rfq.base_half_spread_e2, ttl_secs = rfq.ttl_secs,
        sigma_horizon_secs = params.sigma_horizon_secs,
        // What the TTL costs at a representative volatility, so the trade-off is visible here
        // rather than inferred from quotes later.
        ttl_premium_e2_at_20bp = params.ttl_premium_e2(20_000),
        "RFQ maker enabled; verify the maker address holds the PmmSettle allowance and that \
         the domain separator matches PmmSettle.DOMAIN_SEPARATOR()"
    );

    let serve_shared = Arc::clone(&shared);
    let chain_id = cfg.chain.chain_id;
    tokio::spawn(async move {
        if let Err(e) =
            serve::run(&rfq.serve, serve_shared, key, params, chain_id, pmm_settle).await
        {
            // Not fatal to the quote cycle: taking the ladder down because an HTTP listener died
            // would turn one lost venue into no venue.
            error!(target: "rfq", event = "serve_failed", error = %e,
                   "THE RFQ ENDPOINT IS DOWN; the curve keeps quoting and routing falls back \
                    to AMM-only");
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
    /// Tracked here rather than polled because it has to include orders in flight — a sent hedge
    /// has already committed the exposure, and counting only settled fills double-counts.
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

/// The hedge leg's net position per pair, published for the cycle to read without awaiting.
///
/// The resulting staleness costs nothing: the number only moves when the hedge crosses,
/// `hedge::Bands` will not cross twice inside its cool-off, and the pass interval is half of that.
#[derive(Debug, Default)]
struct HedgeShared {
    /// Net position per pair, in the pool's base units and signed: negative is short.
    positions: RwLock<BTreeMap<u16, i128>>,
}

impl HedgeShared {
    /// This pair's net venue position, or `None` when the leg has not reported one.
    ///
    /// `None` rather than zero, because the two say different things to the skew: no position is an
    /// exposure of zero, no *report* is an unknown, and treating an unknown as flat skews against a
    /// hedge that may well exist. Copied out from under the guard rather than borrowed through it,
    /// so a cycle cannot hold the lock across its work and stall the pass that writes it.
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
/// Derived, not chosen: `hedge::Bands` refuses a second crossing inside `cooloff_ms`, so passing
/// faster cannot produce another order, only spend the venue's rate budget. Half the shortest
/// cool-off, so a band that becomes crossable is acted on within one cool-off rather than two.
/// Floored at 500ms because `/api/v3/depth?limit=100` is weight 5: five symbols at two passes a
/// second is 3,000 a minute against Binance's 6,000-per-minute IP budget.
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
/// Never fatal. A pool that cannot reach its hedge venue must still quote as though it had no
/// hedge, the configuration it was already safe under.
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

    // The band WIDTH is derived, not configured: `hedge::derive_band` sizes it from the pair's
    // carry budget, because below `(fee/sigma)^2` a crossing spends more on fees than the exposure
    // it clears was worth. What is configured per pair is the cool-off, which also paces the task.
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
                // The same taker fee the real venue charges, because a paper book that fills for
                // free reports a hedge nobody could execute. The fee dominates: 4 bp against a
                // measured 0.001 bp spread on BTCUSDT.
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

/// Poll the venues and cross out whatever drift has earned a crossing, on its own clock.
///
/// A failed pass is logged and the loop continues: a slot holding a slightly older position book
/// beats a hedge task that gives up. It needs nothing from the quote cycle — the hedge is never
/// told about fills, and reads the pool's balance from the reader's slot instead.
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

    // A ticker rather than `sleep(every)` at the bottom: sleeping then working makes the real
    // period `every + work`, and the work here is six round trips. `Delay` on a missed tick, so a
    // slow pass shifts the schedule rather than queueing catch-up passes past the rate budget.
    let mut ticker = tokio::time::interval(every);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            biased;
            _ = shutdown.changed() => break,
            _ = ticker.tick() => {}
        }
        // No view, no pass: a pair whose balance has not been read is skipped rather than observed
        // as zero, and with no view every pair is in that state.
        let Some(v) = view.latest() else { continue };
        hedge_pass(&mut leg, &v, &bases).await;
        // After the crossings settle: publishing first would hide exposure already taken on.
        shared.publish(&leg.positions);
    }

    info!(target: "hedge", event = "task_stopped", "shutting down; no further crossings");
}

/// One pass: observe every band, refresh the books, and send whatever crossings are due.
///
/// At most one order per pair per pass. The cool-off stops a second crossing before the first is
/// reflected; [`hedge_interval`] derives the pass rate from that cool-off for the same reason.
async fn hedge_pass(leg: &mut HedgeLeg, view: &ChainView, bases: &BTreeMap<u16, Address>) {
    let now = Instant::now();

    // Exposure is recomputed from two absolutes every pass -- the pool's balance and the venue
    // position this leg tracks -- never accumulated from deltas. A pair whose balance did not make
    // it into this view is SKIPPED, not observed as zero: a missing read is not a flat book, and
    // treating it as one would unwind a live hedge.
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

    // A drifting host clock breaks every signed call at once, with an error naming the timestamp
    // and not the cause. Re-measured on a timer rather than discovered.
    if now.saturating_duration_since(leg.last_resync) >= leg.resync {
        leg.last_resync = now;
        if let Err(e) = leg.venue.sync_clock().await {
            warn!(target: "hedge", event = "clock_resync_failed", error = %e,
                  "keeping the previous offset");
        }
    }

    // Every book in one concurrent pass: nothing orders the spot-depth calls against `allMids`, so
    // issuing them sequentially is six copies of one wait.
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

        // Truncated, never rounded: rounding up asks the venue for more than the pool holds, and
        // the rejection leaves the drift uncleared while looking transient.
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
        // refused, because the drift would otherwise settle against a cost that never existed.
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
                // Settled on what filled, not on what was asked for: a partial fill leaves the
                // difference un-hedged, and the position must say so or the skew believes an open
                // exposure was neutralised.
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
                let deviation_left = leg
                    .bands
                    .get(&order.pair_id)
                    .map(dubu_updater::hedge::Bands::deviation);
                info!(
                    target: "hedge", event = "crossed", pair_id = order.pair_id, symbol = %symbol,
                    side = order.side.as_str(), requested = qty, executed = fill.executed_qty,
                    avg_price = fill.avg_price, status = %fill.status, order_id = fill.order_id,
                    deviation_left,
                    "inventory neutralised on the venue"
                );
            }
            Err(e) => {
                // Nothing reached the venue, so the commitment comes back off: leaving it would
                // skew against a hedge that does not exist.
                *leg.positions.entry(order.pair_id).or_insert(0) -= signed;
                error!(
                    target: "hedge", event = "cross_failed", pair_id = order.pair_id,
                    symbol = %symbol, side = order.side.as_str(), qty, error = %e,
                    "the drift stays outstanding and will be retried"
                );
            }
        }
    }
}

/// Read the fills that landed since the last cycle, mark the ones that have matured, and log both.
///
/// Runs after the quote decisions: markout is a measurement this cycle's quotes do not depend on,
/// and an `eth_getLogs` round trip in front of the ladder would add latency to a race.
///
/// Bounded to the confirmed head, NOT to `view.block_number`: the read view comes from the
/// flashblocks endpoint's `pending` tag and can be ahead of any block the canonical RPC has. With
/// no head the scan is skipped and the cursor does not advance, so the next poll picks the same
/// range up — that replayability is why this polls instead of subscribing.
async fn scan_fills(rt: &mut Runtime, head: &heads::HeadSnapshot, view: &ChainView) {
    // The subscription first, the reader's own sealed read second. Both, because returning when
    // only the socket is down loses fills entirely, and with them the hedge's work.
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
                "could not read Swap logs; the cursor is unchanged and the next cycle re-reads \
                 this range"
            );
            return;
        }
    };

    // A cursor behind the head reads exactly like a chain with nothing happening on it, so it is
    // reported rather than inferred. Draining is normal after a restart; not draining is the fault.
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

        // The notional denominator: our reference at the fill's own timestamp when there is one,
        // otherwise the price the trade executed at. Both are honest scales; reaching for the
        // nearest reference regardless of distance is not, which `reference_at` refuses.
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
        // Kept rather than consumed by the `match` below: whether a *real* reference existed is
        // itself information, and `fill_alert` must not report an edge computed against the fill's
        // own execution price.
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

        // The same event, pushed. This arrives in bursts, and `notify`'s batching is sized for it.
        if let Some(fill) = fill_alert(rt, view, log, &meta, base, quote, reference) {
            // Folded in before the alert is moved, so the digest is built from exactly the fills
            // that were reported and the two can never disagree.
            rt.pnl.record(&fill);
            rt.notify.send(notify::Event::Fill(fill));
        }

        // The hedge is deliberately NOT told about this fill: it reads the pool's balance and the
        // venue's position directly, so the fill shows up as exposure next pass anyway and feeding
        // it deltas here would count the trade twice. `markout` does need it, to score the price.
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
        let worst: Vec<(String, Option<i128>, u64)> = rt
            .markout
            .worst(markout::HORIZONS_SECS.len() - 1, 3)
            .iter()
            .map(|(a, s)| (a.to_string(), s.markout_e2(2), s.fills))
            .collect();
        info!(
            target: "markout", event = "scoreboard",
            new_fills = polled.fills.len(), pending = rt.markout.pending_len(),
            unmarked = rt.markout.unmarked, duplicates = polled.duplicates,
            removed = polled.removed,
            gaps = rt.swaps.gaps(), skipped_blocks = rt.swaps.skipped_blocks(),
            worst = ?worst,
            "markout scoreboard"
        );
    }
}

/// Turn one observed fill into the alert a human reads on a phone.
///
/// `None` when the fill cannot be described honestly — an unknown pair, a price that will not
/// convert — rather than a message with a placeholder in it. Carries no markout field, because that
/// needs references at +1s, +10s and +60s that do not exist yet and arrives later as `marked`. The
/// inventory is the pool's balance as of the reader's last poll, so it is the inventory *now*
/// rather than the one this fill produced.
fn fill_alert(
    rt: &Runtime,
    view: &ChainView,
    log: &dubu_updater::chain::swaps::SwapLog,
    meta: &dubu_updater::chain::PairMeta,
    base: u128,
    quote: u128,
    reference: Option<u128>,
) -> Option<notify::Fill> {
    // The pair's feed symbol: `pair_id` alone means nothing without the config file open.
    let symbol = rt
        .cfg
        .pairs
        .iter()
        .find(|p| p.pair_id == log.pair_id)?
        .symbol
        .clone();

    // Both prices convert through the pair's own shift rather than being assembled from raw amounts
    // here: `units` is the only place a decimal scale is decided, and a second derivation is how
    // the two quietly stop agreeing.
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
        // `reference`, not `ref_at_fill`: the latter falls back to the fill's own execution price
        // when nothing was in tolerance, which would render every such fill at exactly zero edge.
        reference_e8: reference.and_then(|r| units::from_pool_price(r, shift)),
        inventory_base: view.balances.get(&meta.base).copied(),
        inventory_quote: view.balances.get(&meta.quote).copied(),
        tx: log.tx_hash.to_string(),
    })
}

/// Value a signed venue position at `fair`, in the quote token's units.
///
/// Valued through [`dubu_updater::risk::value`] rather than multiplied here, so the skew and the
/// killswitch agree to the rounding step about what the pool holds. The sign survives: a short is
/// negative exposure, and `skew::Inventory` nets it against the pool's own holding.
fn hedge_value_signed(base: i128, fair: u128, price_scale_exp: u8) -> i128 {
    let magnitude =
        dubu_updater::risk::value(base.unsigned_abs(), fair, price_scale_exp).unwrap_or(0);
    let v = i128::try_from(magnitude).unwrap_or(i128::MAX);
    if base < 0 {
        -v
    } else {
        v
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

    // A SEALED timestamp, from whichever source has one. NOT `view.block_timestamp`: the `pending`
    // tag projects an unsealed header about twelve seconds into the future, which reads every quote
    // as 12s old. A sealed head lags by roughly a second, over-stating age, which is the safe
    // direction; the host-clock fallback is worse than that and better than a projection.
    let chain_now = head
        .last
        .map(|h| h.timestamp)
        .or_else(|| view.sealed.map(|(_, ts)| ts))
        .unwrap_or_else(now_unix);

    // Hand the RFQ maker the chain's clock: it stamps every signed expiry from this rather than the
    // host's wall clock, which is the difference between the leg working and every order expired.
    if let (Some(rfq), Some(h)) = (&rt.rfq, head.last) {
        // Back-dated by the head's own age, so it counts from when the head arrived rather than
        // from this cycle. Both ends are monotonic, so the wall clock never enters the arithmetic.
        let received = Instant::now()
            .checked_sub(Duration::from_millis(head.age_ms.unwrap_or(0)))
            .unwrap_or_else(Instant::now);
        rfq.publish_clock(h.timestamp, received);
    }

    // Once, on the first cycle with a real sealed head. Not at startup, because
    // `wait_for_first_head` is not fatal on timeout and the check would return silently with no
    // head at all — on exactly the run where the subscription is down.
    if !rt.clock_checked && head.last.is_some() {
        rt.clock_checked = true;
        check_clock_skew(rt, head);
    }

    // True once per sealed block. See `Runtime::last_block_work`.
    let block_work = chain_now > rt.last_block_work;
    if block_work {
        rt.last_block_work = chain_now;
    }

    // Once per sealed block: everything on this line moves at the block rate, so emitting it every
    // tick would bury the per-cycle lines. `cycles` is a running count, so the gap between two of
    // these is the cycle rate. Persistently negative `read_ahead_of_head` means the read source has
    // fallen behind the head source.
    if block_work {
        let read_ahead_of_head = head.last.map(|h| {
            i64::try_from(view.block_number).unwrap_or(i64::MAX)
                - i64::try_from(h.number).unwrap_or(i64::MAX)
        });
        // Per endpoint in configured order, for both pools, because the failure messages carry only
        // the pool's name. A rising count against one position is a key to rotate; against every
        // position it is a rate the pool cannot serve, and the two want opposite responses.
        let limited_by_endpoint = |rpc: &Rpc| {
            rpc.rate_limit_events_by_endpoint()
                .iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(",")
        };
        info!(
            target: "loop", event = "cycle", cycles, woke_on = wake.label(),
            head = ?wake.head_number(),
            heads_status = head.status.label(), head_age_ms = ?head.age_ms,
            head_reconnects = head.reconnects,
            read_block = view.block_number, block_timestamp = view.block_timestamp,
            read_ahead_of_head,
            chain = status.label(), view_age_secs = view_age,
            // The reader task's counters: a cycle count no longer matching a read count is the
            // clock split working.
            reads = rt.view.polls(), read_failures = rt.view.failures(),
            quiet_polls = rt.view.quiet_polls(),
            read_rate_limited_by_endpoint = %limited_by_endpoint(&rt.read_rpc),
            write_rate_limited_by_endpoint = %limited_by_endpoint(&rt.rpc),
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

    // BEFORE anything else in the cycle, and before `vol.observe` folds this tick's return into the
    // variance: a jump tested against a sigma that already contains it can never fire.
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
            // Both conditions, not capacity alone. The snapshot shows depth until the withdrawal is
            // included -- seconds, against a cycle of milliseconds -- so a capacity-only guard
            // re-sends `refreshCapacity(pair, 0, 0)` every cycle inside that window, each at the
            // 100x tip a withdrawal carries.
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

        // Re-assert a withdrawal the chain does not show. The fast lane sends once, on the edge; if
        // that transaction was rejected, dropped, or sent while the RPC was unavailable, the pool
        // is still armed while the detector believes otherwise, and only this notices.
        //
        // `withdrawal_in_flight` as well as `at_capacity`: `in_flight_max` is 2, so the fast lane's
        // own withdrawal leaves a slot open while the chain still shows capacity. Without the
        // in-flight test roughly half of all withdrawals were second sends at the 100x tip, buying
        // a state the first was about to reach. Something genuinely rejected or dropped is still
        // re-asserted, because nothing is left in flight then.
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

        // The cross-section: every venue quoting this symbol, then the MAD filter and the quorum
        // rule. A venue that is not `Live` is reported as a transition rather than an absence,
        // because a cross-section silently shrinking to two still produces a confident price.
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
                    &quotes,
                    shift,
                    vol,
                    block_work,
                );
                units::to_pool_price(r.micro, shift)
            }
            // Nothing carried forward on the error side: markout's reference history must hold only
            // prices the venues agreed on, or an outage's fills are marked against a phantom.
            Err(e) => {
                // No reference means no return either: folding a gap into the estimator would enter
                // the whole outage as one enormous one-second return.
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

        // Stamped with the block timestamp, not the wall clock: the fills these are compared
        // against carry block timestamps, and mixing the clocks offsets a fill from its reference.
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

        // Publish this market to the RFQ endpoint, or withdraw it. The epoch handed over is the one
        // that will be in force, so RFQ subtracts the commitment the curve is about to honour. With
        // no fair value the market is retired outright: an RFQ order is a firm price for its whole
        // TTL, so signing against a reference the venues no longer agree on is worse than a stale
        // ladder, which at least stops at `maxStaleSecs`. `jump_withdrawn` is what makes
        // `withdraw_pair`'s `rfq.retire` mean anything — gating on `fair.is_some()` alone
        // re-publishes next cycle, and the maker walks straight through its own cool-off.
        if let Some(rfq) = &rt.rfq {
            match fair {
                // NOT `base_balance` / `quote_balance`, which are the POOL's. `PmmSettle` pulls
                // from the MAKER, a different address bound by its allowance, so sizing an RFQ
                // quote against the pool's inventory signs orders that revert in `transferFrom`.
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
                    // Subtract the curve's epoch only when it comes out of the same account:
                    // netting it against a separate maker's balance compares two balance sheets
                    // that never meet, and the maker would refuse orders it can honour.
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
                // No fair value, or a cool-off in force. Both mean the same thing to the maker —
                // sign nothing for this pair — and neither costs a transaction.
                _ => rfq.retire(pair.pair_id),
            }

            // The prop pool's own quote, mirrored for the aggregator. Unconditional, unlike the RFQ
            // market above: this is the chain's state, not this process's opinion of it, and every
            // reason the pool would stop quoting is already in it. Withdrawing here too would take
            // the venue down while the chain still quoted it.
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

        // `half_spread = min(s0 + s1 * sigma, cap) + degraded_extra`. Computed BEFORE the skew,
        // because `price_cap_bps_min` needs the spread that will actually be posted: a wider spread
        // leaves less room to skew down, so clamping against the unwidened value lets the skew push
        // a row under `minPrice` and be refused. Sigma is rescaled from the estimator's inventory
        // window to the quote's exposure window, since a posted quote is not held that long.
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
        // The volatility term only ever adds to the operator's floor: a half-spread below `s0`
        // would mean the cap narrowed the configured spread, which `spread::compute` and the config
        // validator both refuse. Paired here, on the value that reaches the chain.
        assert!(half_spread >= pair.half_spread_bps_e2());
        assert!(u128::from(half_spread) < dubu_core::ladder::BPS_E2_MAX);
        // Every row, every cycle, because back-solving `s1` from history needs all of it:
        // `vol_decibps` beside `capped` says whether the model or the ceiling decided.
        trace_at!(
            block_work,
            target: "spread", event = "half_spread", pair_id = pair.pair_id,
            symbol = %pair.symbol,
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

        // The inventory skew, Avellaneda-Stoikov: r = s - q * gamma * sigma^2. Applied as
        // `RowInputs::skew_bps`, so it goes through `dubu-core`'s `skewed_mid` and every check
        // `ladder::build` runs; no skew reaches the chain unvalidated.
        let skew = fair.map(|f| {
            let inventory = Inventory {
                base_value: dubu_updater::risk::value(base_balance, f, meta.price_scale_exp)
                    .unwrap_or(0),
                // The shared quote token split evenly across pairs. A simplification: nothing yet
                // caps the sum of the pairs' liabilities against it.
                quote_share: quote_balance / rt.cfg.pairs.len().max(1) as u128,
                // The hedge, valued at the same reference and signed. Zero when there is no leg.
                hedge_value: rt
                    .hedge
                    .as_ref()
                    .and_then(|h| h.position(pair.pair_id))
                    .map(|base| hedge_value_signed(base, f, meta.price_scale_exp))
                    .unwrap_or(0),
            };
            // The skew works in whole bps, so the half-spread rounds UP: a larger half-spread
            // leaves LESS room to skew down before the row hits `minPrice`, so this can only
            // tighten the cap.
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
            // Over 100% of the book is legitimate, not impossible: `skew::Inventory::imbalance_ppm`
            // leaves a hedge larger than the holding signed and unclamped on purpose, so the pool
            // can price the unwind. This used to assert the bound and would have aborted the
            // process on the first over-hedged pair, leaving a ladder on chain that nothing was
            // updating until `maxStaleSecs`. `skew::compute` clamps what is actually applied
            // between `negative_bps_max` and the cap, so a wide imbalance is loud but not unsafe.
            if s.imbalance_ppm.abs() > 1_000_000 {
                warn!(
                    target: "skew", event = "imbalance_over_book", pair_id = pair.pair_id,
                    symbol = %pair.symbol, imbalance_ppm = s.imbalance_ppm,
                    hedge_value = %inventory.hedge_value, applied_bps = s.applied_bps,
                    "net exposure exceeds the book; the hedge is larger than the holding"
                );
            }
            assert!(
                i32::from(s.applied_bps) >= -i32::from(rt.cfg.skew.negative_bps_max),
                "skew below its own floor: {} < -{}",
                s.applied_bps,
                rt.cfg.skew.negative_bps_max
            );

            // Every row, every cycle, because tuning gamma has no other input: `raw_decibps` beside
            // `applied_bps` says whether the model or the clamp decided.
            trace_at!(
                block_work,
                target: "skew", event = "skew", pair_id = pair.pair_id, symbol = %pair.symbol,
                imbalance_ppm = s.imbalance_ppm,
                // The skew targets zero net exposure. `target_base_share_ppm` is a funding number
                // that no longer reaches this path; logged so the separation is visible.
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
                    warn!(target: "ladder", event = "row_dropped", pair_id = pair.pair_id,
                          error = %e, skew_bps = skew.map_or(0, |s| s.applied_bps),
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
                bid_capture_cost = %r.bid_capture_cost,
                ask_capture_cost = %r.ask_capture_cost,
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

        let jump_state = rt
            .jump
            .detector(pair.pair_id)
            .map_or("disabled", |d| d.state().label());
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
            jump_state,
            jump_trips = rt.jump.detector(pair.pair_id).map_or(0, jump::Detector::trips),
            "decision"
        );

        // What goes out when both fired, and in what order. Nonce order is absolute, so push order
        // IS execution order -- `tx::Sender::reserve_batch` walks this slice in order for exactly
        // that reason, and reordering these pushes reorders execution on chain.
        //
        // Ordinarily one intent per pair per cycle, capacity first: a ladder is worthless against a
        // zero epoch, and posting the epoch first means the row that follows is solved against the
        // capacity it will execute on.
        //
        // When the pool offers no depth -- after a jump withdrawal, or on a fresh pool -- BOTH go
        // out, ROW FIRST. Capacity-first restores a full epoch behind whatever ladder is stored,
        // which after a jump is the pre-jump one, handing a taker exactly the fill the withdrawal
        // was for. Sending only the row instead livelocks: the reference keeps moving, so the row
        // is stale and wins the arbitration every cycle while the capacity refresh never gets a
        // turn, and the pool quotes zero while looking healthy. `ladder::build` solves against the
        // *planned* capacity when the on-chain one is zero, so the row stays coherent.
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

    // One batch, not one send at a time. `send_batch` reserves the nonces synchronously in this
    // order -- which is the whole basis for the per-pair ordering argued above -- and only then
    // broadcasts through a `FuturesUnordered`, so the batch costs one round trip.
    match rt.sender.send_batch(&rt.rpc, &sends, None).await {
        Ok(batch) => {
            for (intent, out) in batch.sent {
                record(rt, intent, out);
            }
            // The ceiling is a fact about this batch, so it logs per batch. A full upstream
            // persists ACROSS batches, so it logs per EPISODE: per-batch there would repeat at the
            // cycle rate and bury the line that matters.
            if batch.held_at_capacity > 0 {
                info!(
                    target: "tx", event = "held_at_capacity", held = batch.held_at_capacity,
                    in_flight = rt.sender.in_flight_total(),
                    ceiling = dubu_updater::tx::IN_FLIGHT_TOTAL_MAX,
                    "account is at its in-flight ceiling; these intents were not offered"
                );
            }
            // `batch.held_backpressure` is deliberately not logged on its own: `Sender::send_batch`
            // counts it into the episode and returns a transition only when one opens or closes.
            match batch.episode {
                Some(Episode::Opened { refusals }) => {
                    error!(
                        target: "tx", event = "backpressure_opened", refusals,
                        withheld = batch.held_backpressure,
                        "upstream answered -32003; the send phase is standing down rather than \
                         re-offering to a pool that just refused. At this cadence the account \
                         wedges about 13s after inclusion stops, against 138s at the old rate"
                    );
                    // Coalesced by class, not sent one-for-one: on the quote path this condition
                    // touches every send for as long as it lasts. See `notify::ErrorLedger`.
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
            // Nothing was offered at all -- no key, or a nonce that could not be established. Worse
            // than one intent failing: every pair is unquoted until it clears, with the pool's own
            // `maxStaleSecs` the only thing withdrawing them.
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

    // Every cycle, not once per sealed block: a fill is the only signal that an informed
    // counterparty just traded against us, and a second of delay is two or three more quotes at the
    // same price. `SwapWatch::poll` reads the `pending` tag, so the extra frequency finds
    // preconfirmed fills rather than blocks it has already seen.
    scan_fills(rt, head, view).await;

    // The profit-and-loss digest. Hourly, and [`flush_pnl`] sends only when something filled: an
    // idle window pushed on a timer trains the reader to ignore the channel.
    const PNL_EVERY: Duration = Duration::from_secs(3_600);
    if rt.pnl_at.elapsed() >= PNL_EVERY {
        flush_pnl(rt).await;
    }

    // The killswitches. A group is skipped when any pair IN THAT GROUP has no fair value, because
    // marking inventory against a price we do not have invents a NAV. The completeness test is
    // scoped to the group, NOT to `cfg.pairs`: a halted group `continue`s before its reference is
    // computed, so a book-wide test can never be satisfied once any group latches, and that would
    // silently disable every other group's killswitch.
    let quote_balance = view.balances.get(&rt.facts.nav_token).copied().unwrap_or(0);
    // Each group against its own share of the shared quote token, split evenly — the same
    // simplification `skew::Inventory::quote_share` makes.
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
        // Every pair in the group, not merely one: a partial group marks a NAV missing whole
        // positions and reads the absence as a drawdown.
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
                // observation and never from `halt`, which is `None` here by construction; that
                // separation keeps a measurement run from becoming an enforcing one.
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
                    // Pushed as well as logged: a trip is one line in a stream that emits several a
                    // second, and the group stays dark until somebody notices.
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

/// The sender's ETH balance, for the gas leg of the digest.
///
/// Read rather than accumulated from receipts: a balance difference counts every transaction that
/// actually settled, including any this process did not send and any whose receipt was never seen.
/// `None` on a failed read, which the digest reports as unmeasured rather than as zero spend.
async fn sender_balance(rt: &Runtime) -> Option<u128> {
    let address = rt.sender.address()?;
    rt.rpc
        .quantity(
            "eth_getBalance",
            serde_json::json!([address.to_string(), "latest"]),
        )
        .await
        .ok()
        .map(u128::from)
}

/// One line per pair per cycle saying how the reference price was reached.
///
/// The whole cross-section including rejections, because "which venue was dropped and how far out
/// was it" is asked after the fact and there is nowhere else to get it. `bound` says whether the
/// MAD or the floor set the rejection threshold: fast-market regime or calm one. `spreads` is each
/// venue's own top of book, and is where Pyth's confidence interval surfaces.
#[allow(clippy::too_many_arguments)]
fn log_reference(
    symbol: &str,
    pair_id: u16,
    r: &Reference,
    snaps: &[(VenueId, dubu_updater::feed::FeedSnapshot)],
    quotes: &[VenueQuote],
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
        spreads = %fair_value::spread_summary(quotes),
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

/// Record one send's outcome.
///
/// Deliberately has no `await`: awaiting the broadcast here is what forced the sends to be serial,
/// one round trip each. `Sender::send_batch` broadcasts concurrently; this handles the results.
fn record(rt: &mut Runtime, intent: Intent, out: Result<Sent, TxError>) {
    match out {
        Ok(Sent::DryRun {
            calldata,
            would_be_hash,
            would_be_nonce,
        }) => info!(
            target: "tx", event = "dry_run", pair_id = intent.pair_id(), kind = intent.label(),
            to = %rt.cfg.chain.pool, calldata = %dubu_updater::chain::hex0x(&calldata),
            calldata_bytes = calldata.len(), would_be_hash = ?would_be_hash,
            would_be_nonce = ?would_be_nonce,
            "DRY RUN: would send this transaction"
        ),
        Ok(Sent::Broadcast { hash, nonce }) => {
            // Stamped on broadcast, not on confirmation: the cadence this drives is how often a
            // fresh price leaves here, and waiting for the receipt would count inclusion latency
            // into the interval twice.
            if matches!(intent, Intent::UpdateQuote { .. }) {
                rt.last_push.insert(intent.pair_id(), Instant::now());
            }
            // This transaction is about to change the state the reader publishes, so looking again
            // settles it and unblocks the in-flight gate sooner.
            rt.view.nudge();
            info!(
                target: "tx", event = "sent", pair_id = intent.pair_id(), kind = intent.label(),
                tx = %hash, nonce, "transaction broadcast"
            );
        }
        Err(e) => {
            error!(
                target: "tx", event = "send_failed", pair_id = intent.pair_id(),
                kind = intent.label(), error = %e, "could not send"
            );
            // Coalesced by class, not sent one-for-one: a node answering `-32003 txpool is full`
            // refuses every send while it lasts, which at 5-6 cycles a second across every pair is
            // thousands of messages. See `notify::ErrorLedger`.
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
/// Best effort and never fatal: this runs on the way out, usually because something is already
/// broken, and an error on one pair must not stop the rest from being withdrawn.
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
