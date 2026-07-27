# dubu-updater

The quote updater for the DuBu prop AMM on GIWA.

`PropPool` holds inventory and stores a four-point price ladder per pair. It has no strategy of
its own — no reserves curve, no spread model, nothing that reacts to a price. Until something
pushes a new ladder, the one it holds is the one it quotes, until `maxStaleSecs` expires and it
quotes nothing at all. This is that something.

```
  Binance bookTicker (ws)                    GIWA (http, polled)
           │                                          │
           ▼                                          ▼
  feed ──► fair_value ──► ladder ──► policy ──► tx ──► PropPool.updateQuote
           micro-price    dubu-core   send?           PropPool.refreshCapacity
                                        ▲
                                        │
                                 risk (killswitches)
```

## Layout

| module | what it owns |
|---|---|
| `config.rs` | TOML, unknown fields rejected, every range checked at load |
| `feed/` | Binance `bookTicker` websocket: reconnect, sequence regression, staleness |
| `fair_value.rs` | size-weighted micro-price, outlier filter |
| `chain.rs` | JSON-RPC, the request budget, one `eth_call` per poll cycle |
| `ladder.rs` | fair value + knobs → `SolveInput` → four `uint56` prices → packed word |
| `policy.rs` | the decision to push, and mostly the decision not to |
| `risk.rs` | two latching killswitches and the NAV decomposition behind them |
| `tx.rs` | EIP-1559 build/sign/send, nonce, pending intents |
| `units.rs` | the only place a decimal string becomes a number |
| `main.rs` | the loop, and the shutdown that withdraws quotes first |

## Four things that are load-bearing

**Two independent price sources.** Fair value comes from public exchange market data. The
on-chain deviation bound is Pyth's job, later. Pyth is live on GIWA and `PropPool` is designed
to grow a deviation check against it — but if this bot also priced from Pyth, that check would
be comparing a value against itself and would catch nothing.

**The pool's tokens have no market.** `mWETH`, `mWBTC` and `mUSDC` are mocks we deployed and
nothing anywhere trades them. Each pair is priced off the **real** asset its mock stands in for:
pairId 1 follows `ETHUSDT`, pairId 2 follows `BTCUSDT`. That is what makes the demo move and
what will make a markout study mean anything.

**Market data only.** No API key, no account endpoint, no order entry, and no way to add one
without a signing step that does not exist here. The book is unhedged because a Korean corporate
real-name exchange account is not available, so there is no account to hedge into.

**One curve, in `dubu-core`.** The skew, the spread projection, the inverse solve, the
validator, the round-trip check, the executable top, the inventory mark and the calldata packing
are all `dubu-core` calls. There is no second implementation of the curve in this crate, and
`ladder.rs` and `policy.rs` both say so at the top. The way that gets reintroduced is one
innocent `price * bps / 10_000` written inline because reaching for the library felt heavy.

## Running it dry

Dry run is the default. `transmit_allowed` must be **explicitly `true`** in the config for
anything to be broadcast; `--dry-run` forces it off, and there is deliberately no flag that
turns it on.

```
cd engine
cargo run -p dubu-updater -- --config crates/dubu-updater/updater.toml --once
```

No key is needed. A configured-but-missing key is a warning in dry run and fatal only when
transmitting.

```
--config <path>   default: updater.toml
--once            one evaluation cycle, then shut down
--dry-run         force dry run regardless of config
RUST_LOG=debug    per-tick feed detail
```

Everything is JSON on stdout: every fair value, every ladder with the packed word, every
decision with its reason, every transaction. That is the input to markout analysis later.

### Transmitting

```
export DUBU_UPDATER_KEY=0x...            # or tx.private_key_file
# set transmit_allowed = true in the config
cargo run --release -p dubu-updater -- --config crates/dubu-updater/updater.toml
```

The key comes from an environment variable or a file path — never a literal in source, never in
a committed file. Startup checks that it is the address `PropPool.updater()` returns and refuses
to start otherwise, because signing as anyone else means every transaction reverts `NotUpdater`.

## Configuration

`updater.toml` is annotated in full. The parts worth knowing before touching it:

- **Unknown keys are a hard error.** `half_spred_bps = 50` alongside `half_spread_bps = 5` would
  otherwise be a five-basis-point quote whose author believes it is fifty.
- **Amounts are decimal strings in human units.** TOML integers are `i64`; a thousand mWETH is
  `10^21`. `capacity = "1000"` means 1000 mWETH.
- **Ranges are checked at load, and cross-checks too.** Polling faster than the request budget,
  a loss budget below the bleed limit, `adverse_drift_bps` above `favourable_drift_bps`, a
  capture above capacity, a zero half-spread — all refused at startup with the field named.
- **Chain checks run before the loop.** That a pair exists, that `base_decimals` matches the
  deployed ERC-20, that `heartbeat_secs` fits inside the pool's own `maxStaleSecs`, that all
  pairs share a quote token. Each of those failing silently would be expensive.

## Polling and the rate limit

Two measured constraints shape this.

**GIWA has no `eth_subscribe` and no websocket** — the endpoint answers 405. Chain state is
polled on a configurable interval and every consumer treats the view as something with an age;
`view_stale_secs` blocks a push on a view that is too old.

**The public RPC rate-limits.** We saw HTTP 429 "over rate limit" during a 203-transaction
broadcast. Three mechanisms answer it:

1. **One request per poll cycle**, whatever the pair count. Multicall3 is preinstalled in GIWA's
   genesis, so the block number, block timestamp, both `snapshot`s and all three token balances
   are a single `aggregate3` `eth_call`. The naive shape is six requests per cycle.
2. **A local token bucket that refuses rather than queues.** A queue in front of a rate-limited
   endpoint converts a burst into latency, and latency in a quoting loop converts into adverse
   selection. Failing locally lets the caller skip the cycle instead.
3. **Backoff as a state, not a retry.** A 429 opens no further sockets until the penalty window
   expires, doubling per consecutive 429. Sustained failure becomes `Degraded` (every
   half-spread widens by `degraded_extra_half_spread_bps`) and then `Down` (halt, withdraw,
   exit non-zero) rather than disappearing into a retry loop.

Reads use the flashblocks endpoint under the **`pending`** tag only, where the ~200ms
preconfirmed state lives — its `latest` lags the ordinary RPC by about two blocks. Transactions
go to the ordinary endpoint, which is canonical; a nonce read from a preconfirmed state that
later reorganises is a stuck transaction.

## The trigger rules

Measured against the **executable top at the current usage**, never against `maxBid`. Once an
epoch is partly consumed the pool has already walked down the ladder, so `maxBid` is not the
price anyone would get. Comparing `maxBid` to `maxBid` fails in both directions and both are
pinned by tests: it **misses** a move when the new row's width differs, and it **invents** one
when the width changes to compensate.

```
gates    Halted → ChainDown → FeedNotLive → ChainViewStale → PoolPaused → PushInFlight → NoRow
triggers 1. NoUsableQuote    never quoted, or the stored row no longer passes validateLadder
         2. AdverseDrift     the market moved AGAINST the quote          (tight threshold)
         3. Heartbeat        min(heartbeat_secs, 0.8 × maxStaleSecs)
         4. FavourableDrift  the quote merely became conservative        (loose threshold)
last     Unchanged — a trigger fired but the row is identical. Only the heartbeat overrides it,
         because refreshing `updatedAt` is the whole point of the heartbeat.
```

The asymmetry **inverts archi_v2 §5.3's chase/retreat pair on purpose.** A maker competing for flow chases eagerly
and retreats slowly because it is one maker among many and losing the flow is the dominant cost.
On GIWA the prop pool is the only maker of consequence: there is nobody to lose the flow to, so
being a basis point too conservative costs a little volume, while being a basis point too
generous after the market has moved is a free option written to whoever notices first. The
config validator refuses a config with the two the other way round.

`PushInFlight` matters more than it looks: a pair with an unconfirmed transaction is **not**
superseded. Two `updateQuote`s in flight are ordered by a sequencer that sorts on fee, and the
one that wins may be the older one — a quote that silently goes backwards. The escape hatch is
`pending_timeout_secs`, which abandons the intent (it does not replace it) and resyncs the
nonce.

Capacity is a separate decision against a separate storage word, sharing only the gates.
`refreshCapacity` is the risk decision; `updateQuote` is the price decision.

## The two killswitches

Both **latch**, and the latch is written atomically and read at startup: a halted book stays
down across a restart. Restarting is the first thing an operator does when something looks
wrong, and a switch that silently resumed would be decoration. Clear `risk.state_path`
deliberately to resume.

They are measured on a decomposition that separates two things a naive NAV number conflates:

```
NAV         = quoteBalance + Σ value(baseBalance_i, fair_i)
revaluation = Σ [ value(basePrev_i, fairNow_i) − value(basePrev_i, fairPrev_i) ]
tradePnl    = (NAV_now − NAV_prev) − revaluation
```

With no trades, `tradePnl` is **exactly** zero — the balances are identical so the sums cancel
term for term over the same integer valuation function. That is what lets a cumulative budget
run for days without drifting into a trip, and it is why there is no noise-floor knob.

**`bleed_limit`** — peak-to-current drawdown of **total NAV** inside `bleed_window_secs`.
Catches a fast adverse move on inventory or a burst of bad fills: stop now, work out why later.
This book is unhedged, so a market move against the inventory is a real loss, not an accounting
artefact.

**`loss_budget`** — cumulative **gross trade-attributable** loss, all-time. Catches systematic
adverse selection: being picked off a little, repeatedly, while the market goes nowhere, which a
short-window drawdown limit never sees. Gross per archi_v2 §5.4 — a later gain does not refund
the budget, because a book that loses 1000 and makes 1000 has been picked off twice, not zero
times.

On a trip, and on a `Down` chain: **withdraw quotes, log loudly, exit non-zero (2)**. Withdrawal
is `refreshCapacity(pairId, 0, 0)` — the updater role *cannot* call `pause`, which belongs to
the guardian on separate hardware, and a pair with zero capacity returns zero from every quote
path in `PropPool._outFor`. That is a complete withdrawal inside the authority this key actually
has. The backstop behind it is the pool's own `maxStaleSecs`: even if the process dies without
withdrawing anything, every quote stops being fillable an hour later.

The thresholds in `updater.toml` are demo numbers on a mock book. Real ones have to be
back-solved from a replay drawdown, per archi_v2 §5.4. Do not pick them by feel.

## What is not built

Listed because each one is a real gap, not because the list is decorative.

- **The reference-oracle deviation bound.** The only genuinely independent check on a wrong fair
  value, and it is absent on both sides: `PropPool.updateQuote` has no Pyth read, and this bot
  has nothing to compare against. `ladder.rs`'s bid ceiling and ask floor are *self-consistency*
  bounds against the same number that produced the row — they cannot catch a feed that is
  confidently wrong. This is the largest gap here.
- **Deposit and withdrawal attribution.** A manager `deposit` or an owner `withdraw` moves
  balances with no trade, so it lands in `tradePnl` — a withdrawal looks like a loss to the
  budget switch. Fixing it needs the pool's `Swap` and `ReserveSynced` events.
- **Cross-asset quote clamp.** archi_v2 §5.4's `Σ(bid liabilities) ≤ 0.95 × quote holdings`.
  Capacity is clamped to inventory **per pair**; both pairs draw bids from the same mUSDC and
  nothing caps the sum. Harmless at current sizing (≈$4.3M of configured bid against $11.1M
  held) and wrong in principle.
- **Inventory-driven skew.** `skew_bps` is static. archi_v2 §5.4's
  `bidSkew = clamp(slope × (current/target − 1))` needs a target inventory per pair.
- **Cost-basis ask floor.** `askFloor = costBasis × (1 + minExitEdgeBps)`, and the cold-start
  rule that forces `askCapacity = 0` when the cost basis is unknown. Needs fill history.
- **Directional flow budget.** The leaky bucket where a bid fill debits the bid budget and
  charges the ask.
- **True sequence-gap detection.** `bookTicker`'s update id is monotone but not contiguous, so a
  dropped message is undetectable on this stream and this crate does not claim to detect one —
  only a *regression*, which is dropped and counted. Contiguity needs the `depth` diff stream's
  `U`/`u` ranges.
- **Markout.** Every quote and decision is logged as structured JSON for exactly this, and
  nothing consumes it yet. archi_v2 §5.5 wants ClickHouse or Parquet.
- **Metrics and alerting.** Logs only. No Prometheus, no pager.
- **Batching.** `updateQuote` takes an array and `refreshCapacityBatch` exists; this sends one
  pair per transaction. Two pairs make that a rounding error, and a batch means one pair's
  invalid row reverts the other's.
- **A second RPC endpoint.** `halt_after_secs` on a single provider is the whole liveness story.

## Dependency pins

`Cargo.lock` holds `ruint` at 1.17.0 and the workspace pins `alloy-primitives`/`alloy-sol-types`
to the 1.x line resolvable under rustc 1.88. `alloy-consensus` and `alloy-signer-local` are
**absent on purpose**: every version at or below this toolchain's MSRV ceiling calls
`serde::__private`, which serde deleted in 1.0.229, and pinning serde back is blocked by
`serde_with` — which `alloy-consensus` itself pulls in. The two windows do not overlap at any
version, so `tx.rs` encodes the EIP-1559 envelope from its RLP field list and signs the prehash
with `k256`. That is a fully specified wire format, pinned byte-for-byte in
`testdata/eip1559_vector.hex` against `cast mktx` output. Raising the toolchain to ≥ 1.91
retires all of it.

## Tests

```
cargo test --all                              # 113 in this crate
cargo clippy --all-targets -- -D warnings
```

No test touches the network. The feed and chain are mocked by *constructing the data types
directly* rather than by a mock object, so there is no mock that can drift from the real thing.
The load-bearing ones:

- **policy** — every gate aborts and aborts in order; each trigger fires at its threshold and
  **not one basis point below it**; the two executable-top cases a `maxBid` comparison gets
  wrong; the heartbeat re-posting an identical row and nothing else doing so; a superseded usage
  generation not looking like a consumed epoch.
- **ladder** — a live-shaped row passing every check, `width_bps` bounding rather than setting
  the ladder, a fair value under the pool's floor refused rather than clamped, capacity cut to
  what the inventory can settle.
- **risk** — the latch surviving a restart, the cumulative budget surviving a restart while the
  previous mark deliberately does not, a quiet book attributing exactly zero, gross accounting
  not refunding on a recovery, and the budget catching what the bleed window is too short to
  see.
- **tx** — the signed envelope byte-for-byte against `cast mktx`.

### Checking a computed row against the real chain

A dry run prints the packed word for every row. `eth_call` runs `updateQuote` — including
`validateLadder` — against live state without broadcasting anything, which is the strongest
check available short of sending:

```
cast call --from <updater> <pool> "updateQuote(uint256[])" "[<word from the log>]" \
  --rpc-url https://sepolia-rpc.giwa.io
```

Empty output means the row was accepted. A deliberately crossed row reverts `0x001fbb8d`
(`BidBelowMinPrice`) or `0x5eac0444` (`CrossedBook`), which is a useful negative control that
the check is doing anything at all.
