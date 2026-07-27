# dubu-updater

The quote updater for the DuBu prop AMM on GIWA.

`PropPool` holds inventory and stores a four-point price ladder per pair. It has no strategy of
its own — no reserves curve, no spread model, nothing that reacts to a price. Until something
pushes a new ladder, the one it holds is the one it quotes, until `maxStaleSecs` expires and it
quotes nothing at all. This is that something.

```
  Binance / OKX / Bybit (wss)   Nodit newHeads (wss) ──► wakes the loop, 1s
           │                    GIWA flashblocks (http, `pending`) ──► state, ~200ms
           ▼                                          ▼
  feed ──► fair_value ──► skew ──► ladder ──► policy ──► tx ──► PropPool.updateQuote
  n venues  MAD + quorum   A–S    dubu-core    send?           PropPool.refreshCapacity
                                                 ▲
                                                 │
                                          risk (killswitches)
```

## Layout

| module | what it owns |
|---|---|
| `config.rs` | TOML, unknown fields rejected, every range checked at load; redacted endpoint URLs |
| `feed/mod.rs` | per-venue liveness, staleness, sequence regression, and the transition log |
| `feed/ws.rs` | the one reconnect loop every venue shares |
| `feed/{binance,okx,bybit,coinbase}.rs` | one wire format each, and nothing else |
| `fair_value.rs` | micro-price per venue, MAD outlier rejection, quorum, one reference |
| `skew.rs` | EWMA volatility and the Avellaneda–Stoikov reservation price |
| `chain/mod.rs` | JSON-RPC, the runaway guard, one `eth_call` per cycle, chain health |
| `chain/heads.rs` | the `newHeads` subscription that drives the loop, and its watchdog |
| `ladder.rs` | fair value + knobs → `SolveInput` → four `uint56` prices → packed word |
| `policy.rs` | the decision to push, and mostly the decision not to |
| `risk.rs` | two latching killswitches and the NAV decomposition behind them |
| `tx.rs` | EIP-1559 build/sign/send, nonce, pending intents |
| `units.rs` | the only place a decimal string becomes a number |
| `main.rs` | the loop, its wake sources, and the shutdown that withdraws quotes first |

## Four things that are load-bearing

**Several venues, and they are not redundancy.** Fair value comes from three public market-data
websockets combined into one reference. One venue made the ladder's own price bounds vacuous in
exactly the way a single-source oracle is — the row and the limits meant to check it came from
the same number — so a feed that was *confidently* wrong produced a ladder nothing caught. The
on-chain deviation bound is still Pyth's job, later, and the cross-venue filter is not a
substitute for it: an error correlated across every venue is invisible to both. Pyth is live on
GIWA and `PropPool` is designed to grow a deviation check against it — but if this bot also
priced from Pyth, that check would be comparing a value against itself and would catch nothing.

**The pool's tokens have no market.** `mWETH`, `mWBTC` and `mUSDC` are mocks we deployed and
nothing anywhere trades them. Each pair is priced off the **real** asset its mock stands in for:
pairId 1 follows `ETHUSDT`, pairId 2 follows `BTCUSDT`. That is what makes the demo move and
what will make a markout study mean anything. Every venue spells that asset differently, so
`[pairs.venues]` names each one explicitly rather than guessing a transformation.

**Market data only.** No API key, no account endpoint, no order entry on any venue, and no way
to add one without a signing step that does not exist here. The book is unhedged because a
Korean corporate real-name exchange account is not available, so there is no account to hedge
into — which is also why the inventory skew exists. With no hedge venue, Avellaneda–Stoikov is
the only inventory control there is.

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
cp crates/dubu-updater/.env.example crates/dubu-updater/.env   # then fill in NODIT_API_KEY
cargo run -p dubu-updater -- --config crates/dubu-updater/updater.toml --cycles 4 --dry-run
```

No signing key is needed. A configured-but-missing key is a warning in dry run and fatal only
when transmitting. `NODIT_API_KEY` *is* needed even for a dry run, because the endpoints require
it — `.env` is read from the working directory and from the config's own directory, and a real
environment variable beats both.

```
--config <path>   default: updater.toml
--once            one evaluation cycle, then shut down
--cycles <n>      n cycles, then shut down; wakes on newHeads like a normal run
--dry-run         force dry run regardless of config
RUST_LOG=debug    per-tick feed detail, and every head as it arrives
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
- **No secrets, only `${VAR}` templates.** See [Secrets](#secrets). A missing variable fails at
  startup naming the variable.
- **Ranges are checked at load, and cross-checks too.** A fallback interval below the block time,
  a head watchdog window that could never fire before the halt timer, an `http(s)` `ws_url`, a
  loss budget below the bleed limit, `adverse_drift_bps` above `favourable_drift_bps`, a capture
  above capacity, a zero half-spread — all refused at startup with the field named. So are the
  four that came with this change: `min_venues < 2` (which would be the single-source oracle
  again), a pair naming fewer venues than the quorum needs (it could never quote), a
  `max_dispersion_bps` at or below `mad_floor_bps` (the regime gate would fire before the filter
  ever ran), and `max_negative_bps > max_positive_bps` (the book-lifting cap must be the tighter
  one).
- **A venue is enabled by a pair naming a symbol for it**, under `[pairs.venues]`, and by nothing
  else. Two switches for one venue is two places for them to disagree, and the failure that
  produces — a venue connected and quoting nothing — counts toward nothing and reads as healthy.
  Unknown keys there are a hard error, so `bybbit = "ETHUSDT"` fails at startup instead of
  quietly leaving the pair one venue short of the quorum it is written to have. Endpoints are
  built-in defaults; `[feed.urls]` overrides one.
- **Chain checks run before the loop.** That a pair exists, that `base_decimals` matches the
  deployed ERC-20, that `heartbeat_secs` fits inside the pool's own `maxStaleSecs`, that all
  pairs share a quote token. Each of those failing silently would be expensive.

## The reference price: several venues, a MAD filter, and a quorum

Three venues, all quoting against **USDT**, because the pairs track `ETHUSDT` and `BTCUSDT`:

| venue | stream | shape |
|---|---|---|
| Binance | `bookTicker`, combined stream in the URL | `{"u","s","b","B","a","A"}`, snapshot per frame |
| OKX | `bbo-tbt`, subscribed over the socket | `{"arg":{"instId"},"data":[{"bids":[[px,sz,·,·]],"asks":[…],"seqId"}]}` |
| Bybit | `orderbook.1`, subscribed over the socket | `{"topic","type":"snapshot"\|"delta","data":{"s","b":[[px,sz]],"a":[…],"u"}}` |

Coinbase is **implemented and deliberately not configured**, which is a measurement rather than
a preference. Over 60s of live ETHUSDT/BTCUSDT on 2026-07-27:

- its deep `ETH-USD`/`BTC-USD` books sat a persistent **+8.4 / +9.3 bps** above the three USDT
  venues. That entire gap is the USDT/USD basis. A USD-quoted venue is not a second observation
  of the same price, it is a different price, and averaging it into a 5 bp half-spread is a units
  error wearing the costume of redundancy;
- its `ETH-USDT` book is the right *unit* but thin, and Coinbase's `ticker` channel fires on
  **trades** rather than on book changes, so it updated in 4 of 24 samples. It would count toward
  quorum and contribute nothing, which is worse than being absent.

The client is still there, parsed against a captured live frame in its tests, for a pair whose
base genuinely trades against USD.

### Combining them

```text
per venue   micro-price, plus the unconditional rejections (crossed, zero depth, zero price)
median      of the cross-section
dev_v       |micro_v − median|,  in deci-bps of the median
mad         median(dev_v)                    ← the market's current disagreement
threshold   max(mad_k × mad, mad_floor_bps)
survivors   dev_v ≤ threshold
reference   mean(survivors)                  ← equal weight
```

**Why MAD and not a fixed band.** A fixed band is wrong in both directions at once: too wide in a
calm market to catch anything, and firing constantly in a fast one. It also had to *concede*
after a few consecutive rejections or a genuine fast move became a permanent outage — so every
real move cost several ticks of staleness at exactly the moment staleness is most expensive.
`mad` is the disagreement itself, so the band widens by itself when the market is fast. A venue
printing garbage is far from a median the others agree on whatever the regime; a real move takes
every venue with it, the median moves, and nothing is rejected and nothing has to be conceded.

**Why the floor.** It is not a fixed band smuggled back in — it is what stops the filter eating
itself. When every venue agrees to within a tick, `mad` collapses to zero and everything but the
median is an outlier. Measured, `mad` runs 0.1–1.0 bps and the largest ordinary single-venue
deviation was 1.6 bps, so `mad_floor_bps = 2` is above ordinary disagreement and below anything
that matters. In calm markets the floor binds and in fast ones the MAD does; `bound` in every
`reference` log line says which.

**Why equal weight.** *Liquidity weight* reads well and is wrong: top-of-book size is mostly a
function of tick size, so a venue with a coarse tick would dominate for a reason unrelated to how
good its price is — and each venue's own liquidity information is already inside its micro-price.
*The median itself* is maximally robust and throws away every venue but one, and it moves
discontinuously as venues drop out. Equal weight over MAD-filtered survivors takes the robustness
from the filter and the efficiency from the average. Every survivor is within `threshold` of the
median by construction, so **losing a venue moves the reference by at most `threshold / n`** — the
price stays continuous while the redundancy does not, which is why the venue count is in every log
line.

### Degrading, which is never silent

```
quorum        min_venues survivors, ≥ 2 and refused below it at startup
below quorum  no reference, no push, feed = no_quorum
dispersed     mad > max_dispersion_bps → no reference, no push, feed = dispersed
venue lost    edge-triggered `venue_down` / `venue_up`, naming the venue and both statuses
outlier       `venue_rejected` per venue per cycle, with its deviation and the threshold
```

`Dispersed` is the one that matters and it is why the gate is on the **dispersion** rather than on
a count of rejections. One venue away from the pack barely moves `mad`. Half the venues away from
the other half moves it to roughly half the gap, and at that point their mean is a number no venue
is showing — so the bot stops rather than averaging through a regime change. A rejection count
could never distinguish the two: at least half the cross-section always survives a MAD filter, so
"a majority was rejected" is not a state that can occur.

Below quorum the pair simply does not quote. `policy`'s `FeedNotLive` gate is unchanged; what
changed is that the status it carries is now `NoQuorum` or `Dispersed` rather than a single
socket's health, because one venue's status is never on its own a reason to quote or not to.

## Inventory skew: Avellaneda–Stoikov, linear term only

```text
r = s − q · γ · σ²
```

The bot used to quote symmetrically around the reference regardless of what the pool held, so
sustained one-way flow accumulated inventory and nothing pushed it back.

The load-bearing detail, and the reason to reach for A–S rather than a hand-rolled `κ · q`: the
coefficient scales with **σ², not with a constant**. The same imbalance is far more dangerous in a
fast market than a calm one, and a fixed `κ` is wrong in both directions at once.

**The derivative and integral terms are deliberately absent.** A PID needs three coefficients
tuned against replay data that does not exist here — three numbers picked by feel, which
archi_v2 §5.4 is explicit about not doing. The linear term is one coefficient and is provably
optimal under the model's assumptions. A derivative term earns its place once fills show the
imbalance *oscillating* around target rather than converging; an integral term once they show a
steady-state offset the proportional term never closes. Both are measurable from the `skew` log
line plus fills, and neither should be added before they have been measured.

**Volatility.** EWMA of squared relative returns, sampled once per quote cycle (~1 Hz off
`newHeads`), kept as a *per-second* variance so cycle-cadence jitter does not bias the level, then
scaled by √t to a **300-second horizon**. That horizon is the same window as
`risk.bleed_window_secs`, and that is the argument: the bleed killswitch already defines the
horizon over which an adverse move against inventory counts as a real loss, and the skew exists to
push inventory back before that switch has anything to measure. One second is the cycle cadence,
not a holding horizon; one hour is the staleness backstop, and at that scale σ² barely moves and
the skew becomes the constant A–S was chosen over. `vol_tau_ms` is the EWMA time constant, so the
half-life is `τ·ln2 ≈ 41s`; the estimator re-anchors rather than folding a feed outage in as one
enormous one-second return, and it is seeded at zero so an unknown volatility produces no skew
rather than an invented one.

**Units, so γ is a number a human can reason about.** `q` is a share of the book in ppm, σ is in
bps, and `skew_bps = γ · q · σ² / 10⁴`. Measured live, ETHUSDT's 300s σ is about **10 bps** and
BTCUSDT's about **3 bps**, so at `γ = 1000` a pool 20% off target skews 2 bp against a 5 bp
half-spread. This was first written with `γ = 100` on a guess that σ was 30 bps; since the skew is
quadratic in σ the guess was wrong by a factor of nine and the applied skew rounded to zero on
every row. Every row logs `imbalance_ppm`, `sigma_millibps`, `raw_decibps`, `applied_bps` and
whether the clamp bound — which is the sample a back-solve needs, and without which γ is guesswork.

**Target inventory is configuration**, per pair, as `target_base_share_pct` — a *share* of the
book rather than an amount, so it stays meaningful as the pool grows. The book is the pair's base
valued at the reference plus its share of the shared quote token, split evenly across pairs. That
split is a simplification and is named as one: it makes exactly the assumption archi_v2 §5.4's
missing cross-asset clamp already makes, in the same place.

**The clamp, and why it is asymmetric.** `max_positive_bps = 30`, `max_negative_bps = 10`.

- A **positive** skew moves the book down: the pool is long, wants to sell, and both sides fall.
  The bid falling is defensive and the ask falling is the point. Neither is a gift to a taker.
- A **negative** skew moves the book up: the pool is short, and its **bid** rises toward and past
  the reference. A bid above fair value is a free option written to whoever notices first — the
  same direction the `adverse_drift_bps` asymmetry exists to defend against — and there is no
  structural floor to stop it. So it is capped tighter. A short book is worked off more slowly
  than a long one, which is the right trade for a book that cannot hedge: a slow recovery costs
  volume, a picked-off bid costs money. The config validator refuses these two the other way round.

Both caps also stop a wild σ print from inverting the strategy: σ² is quadratic, so a 10× error in
volatility is a 100× error in skew.

**The skew can never produce a row the chain rejects.** Two separate reasons:

- *Crossing is structurally impossible.* The skew moves the **mid**, and both targets hang off the
  skewed mid with the same half-spread, so the spread between them is preserved exactly and
  `minBid < minAsk` for every skew in range. A test checks the whole range against `dubu-core`'s
  own builder rather than by argument.
- *The floor is clamped, not discovered.* A large positive skew can push the bid target under the
  pair's `minPrice`, at which point `ladder::build` correctly refuses the row — but a refused row
  is a quoting outage. `skew::min_price_cap_bps` computes the largest skew that still clears the
  floor and folds it into the cap, logged as `clamp = "min_price_floor"`. A test proves the
  clamped skew still builds a row and one bp more does not.

The skew is applied as `RowInputs::skew_bps`, so it goes through `dubu-core`'s own `skewed_mid`,
the inverse solve, `validateLadder`, the round-trip check and the fill check **before** anything is
packed. There is no path by which a skew reaches the chain unvalidated. Resolution is whole bps,
because that is what `dubu-core` takes and sub-bp resolution here would mean a second
implementation of the skew in this crate.

## Endpoints, and which one drives what

Three endpoints, three jobs. This is not redundancy — it is three different freshness
guarantees, and using the wrong one for a job is a real bug.

| config field | endpoint | job | freshness | why that one |
|---|---|---|---|---|
| `ws_url` | Nodit WSS | `newHeads` — **drives the loop** | 1s, confirmed | the only one that answers `eth_subscribe` at all |
| `flashblocks_rpc_url` | GIWA flashblocks | every state read, `pending` tag | **~200ms, preconfirmed** | fresher than the head that triggered the read |
| `rpc_url` | Nodit HTTPS | transactions, nonce, receipts, startup metadata | canonical | a nonce must come from state that cannot reorganise |

Heads say *when* to look; they are not what is read. The flashblocks `pending` tag is ~200ms
preconfirmed state, which is **fresher than the 1s confirmed head that woke the cycle** — that
gap is the whole reason the split survives. A preconfirmed swap has already moved
`bidUsed`/`askUsed`, and quoting against the pre-swap usage computes the executable top at a
point on the ladder the pool has already walked past. Its `latest` lags the ordinary RPC by about
two blocks, so it is only ever read under `pending`. Transactions go to whichever endpoint
`rpc_url` names, now defaulting to Nodit.

Measured on the Nodit endpoint: `newHeads` delivers at 904 / 1050 / 966 / 997 ms against a 1s
block time; 20 rapid `eth_blockNumber` calls returned 200 every time; `eth_subscribe` over its
*HTTPS* endpoint correctly reports `notifications not supported`, which is why an `http(s)`
`ws_url` is refused at startup rather than silently degrading. `debug_traceTransaction` needs a
higher plan tier and `trace_block` does not exist; nothing here calls either.

### One thing worth knowing if you touch `heads.rs`

**Nodit sends JSON-RPC over WebSocket *binary* frames.** All four market-data venues send text.
Both are legal — RFC 6455 leaves the choice to the application — and a client that matches on
the opcode instead of the payload silently drops every frame from the other one. The failure
gives no signal pointing at the cause: the handshake returns 101, frames arrive on schedule,
and it presents as "the endpoint never replied". `heads::payload` handles both and a test pins
it; `feed::ws` decodes binary frames as UTF-8 for the same reason, against the day a venue
changes its mind.

## The loop is event-driven, and what is left of the polling machinery

The loop wakes on a `newHeads` notification and does its reads then. That is strictly better
than a timer twice over: fewer requests, and the reads happen when there is actually new state
rather than on an arbitrary cadence that either fires between blocks or drifts past one.

Underneath sits a **fallback timer** at `fallback_poll_interval_ms`, in the same `select!` as the
head. There is no mode flag and no switch: when the subscription is healthy the head always wins
the race, and when it is not the timer does. Every cycle logs `woke_on`.

Some of the polling-era machinery is still here. It stays on its own merits, and the code says so
at each site so a later reader does not delete it as debris:

- **Multicall3 batching — kept.** One `eth_call` per head instead of six, whatever the pair
  count. The rate limit *motivated* it; it is justified without one, because a batch is answered
  at a single block where six separate calls can straddle a boundary and build a view that never
  existed.
- **Backoff — kept.** A dedicated endpoint is not an infinite one, and the failure this really
  guards against is ours: with no penalty window any transient upstream error turns the bot into
  a retry flood, which is how a small outage becomes a large one.
- **The killswitches, the intent lifecycle, the pre-send gates — untouched.** This change did not
  reach them.
- **The token bucket — demoted.** It survives as a *fuse*, not a budget: `requests_per_sec` is
  now loose enough that normal operation never approaches it, and it exists so a reconnect storm
  or a spinning loop hits a local ceiling before the provider's.
- **The poll timer as primary driver — gone.** Renamed to `fallback_poll_interval_ms`, and the
  config now refuses a value *below* the block time, because that would quietly make it the
  driver again.
- **The budget cross-check — deleted.** `poll_interval_ms` used to be refused if its steady-state
  rate exceeded `requests_per_sec`. That check only made sense against a hostile budget.

## The head watchdog, and why it does not decide the chain is down

A subscription that **errors** is the easy case: the socket closes, the task reconnects with
exponential backoff, the fallback timer covers the gap.

A subscription that reconnects and then **silently stops delivering** is the dangerous one. Every
visible signal says healthy — the connection is open, nothing errored, the last head looks like a
real block — while the bot sits believing the chain has stopped and quotes a frozen view into a
moving market. So head liveness is a first-class state, exactly as feed liveness is:
`Live / Stale / Down / NoData`, with `Stale` meaning *connected and silent*.

When no head has arrived for `head_stale_blocks × block_time_ms` (10s by default), the loop says
so once — edge-triggered, because a watchdog that repeats every two seconds is one nobody reads —
and keeps reading on the timer.

What it deliberately does **not** do is conclude that the chain is down. That question is answered
by the *next read*, because chain health now escalates on **two** signals over the same
`Healthy → Degraded → Down` thresholds:

| | reads landing | block number advancing | verdict |
|---|---|---|---|
| socket died, chain fine | yes | yes | keep quoting on the timer, log the watchdog |
| endpoint unreachable | **no** | no | `Degraded` → `Down` → withdraw |
| chain frozen, RPC answering | yes | **no** | `Degraded` → `Down` → withdraw |

The third row is new and it closes a real hole. "The RPC replied" used to be the only thing
measured, so an endpoint answering cheerfully about a chain that had stopped read as perfectly
healthy forever — the same class of bug as the silent websocket, one layer down. Progress is
taken from the block number the read itself returns, and only a strictly greater number counts.

Wiring a silent socket straight to `Down` would withdraw quotes over a quiet websocket on a
healthy chain. Ignoring it would be worse. Splitting the two signals is how both are avoided.

`view_stale_secs` still blocks a push on a view that is too old, independently of all of this.

## Secrets

**The endpoint URL *is* the credential.** Nodit puts the API key in the path
(`https://giwa-sepolia.nodit.io/<KEY>`), so it is not a string that happens to contain a secret.

- `updater.toml` holds `${NODIT_API_KEY}` templates and never a literal. An unset or empty
  variable is a startup error naming the **variable**, never the value.
- The value comes from the real environment, or from a **gitignored `.env`** next to the config
  (`.env.example` is the committed template). A variable already set in the real environment
  always wins over the file.
- `config::EndpointUrl` makes redaction structural rather than a rule to remember: `Display` and
  `Debug` both emit `scheme://host/***`, so `url = %cfg.chain.ws_url` in a `tracing` macro cannot
  print the key and neither can a `{:?}` dump of the whole config. The real string is reachable
  only through `expose()` — grep for it to audit every use; there are two, the HTTP client and
  the websocket connect. Query strings and `user:pass@` userinfo are redacted too.

The signing key is unchanged: an environment variable or a file path, never a literal, never
logged.

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

- **The on-chain reference-oracle deviation bound.** `PropPool.updateQuote` still has no Pyth
  read. The cross-venue filter narrowed this gap and did not close it: it catches *one* venue
  being confidently wrong, and it is silent about an error correlated across all of them.
  `ladder.rs`'s bid ceiling and ask floor remain *self-consistency* bounds against the number
  that produced the row. This is still the largest gap here.
- **Deposit and withdrawal attribution.** A manager `deposit` or an owner `withdraw` moves
  balances with no trade, so it lands in `tradePnl` — a withdrawal looks like a loss to the
  budget switch. Fixing it needs the pool's `Swap` and `ReserveSynced` events.
- **Cross-asset quote clamp.** archi_v2 §5.4's `Σ(bid liabilities) ≤ 0.95 × quote holdings`.
  Capacity is clamped to inventory **per pair**; both pairs draw bids from the same mUSDC and
  nothing caps the sum. Harmless at current sizing (≈$4.3M of configured bid against $11.1M
  held) and wrong in principle.
- **Cost-basis ask floor.** `askFloor = costBasis × (1 + minExitEdgeBps)`, and the cold-start
  rule that forces `askCapacity = 0` when the cost basis is unknown. Needs fill history.
- **A tuned γ.** The skew is live and `γ = 1000` was set from one measurement of σ on one quiet
  afternoon. archi_v2 §5.4 says thresholds come from a replay back-solve, and that applies here
  too. The `skew` log line exists to make that possible; nothing consumes it yet.
- **The derivative and integral terms** of the inventory controller. Deliberate, and the
  conditions that would justify each are written down above.
- **Directional flow budget.** The leaky bucket where a bid fill debits the bid budget and
  charges the ask.
- **True sequence-gap detection.** Every venue's update id is monotone but not contiguous, so a
  dropped message is undetectable on these streams and this crate does not claim to detect one —
  only a *regression*, which is dropped and counted. Contiguity needs each venue's depth-diff
  stream and its own sequencing rules.
- **A failover venue list.** Three venues is enough to reject an outlier and, with
  `min_venues = 2`, to survive losing one. Losing two stops the pair, and there is no fourth
  configured to fall back on — Coinbase is implemented but disqualified for these pairs, for the
  measured reasons above.
- **Markout.** Every quote and decision is logged as structured JSON for exactly this, and
  nothing consumes it yet. archi_v2 §5.5 wants ClickHouse or Parquet.
- **Metrics and alerting.** Logs only. No Prometheus, no pager.
- **Batching.** `updateQuote` takes an array and `refreshCapacityBatch` exists; this sends one
  pair per transaction. Two pairs make that a rounding error, and a batch means one pair's
  invalid row reverts the other's.
- **A failover RPC provider.** Three endpoints are configured but they are three *roles*, not
  three replicas: if Nodit goes away, the head subscription and the transaction path both go with
  it. What exists is graceful degradation within one provider — heads to fallback polling, and a
  liveness ladder that distinguishes a dead endpoint from a frozen chain. What does not exist is
  a second provider to fail over *to*.

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
cargo test --all                              # 200 in this crate
cargo clippy --all-targets -- -D warnings
```

No test touches the network. The feeds and chain are mocked by *constructing the data types
directly* rather than by a mock object, so there is no mock that can drift from the real thing —
and every venue parser is pinned against a frame **captured verbatim off that venue's live
socket**, which is the closest a network-free test gets to the wire.
The load-bearing ones:

- **feed** — one venue dying not touching another; losing a venue producing exactly one event
  and a steady outage producing none; a `Stale { age_ms }` that changes every cycle not
  re-firing; a venue that never connects announcing itself once; Bybit's `delta` leaving the
  untouched side alone, its `["px","0"]` deletion not being read as a price, and a reconnect
  clearing the merge state; a protocol-level `snapshot` overriding the regression check.
- **fair_value** — one venue printing garbage rejected while the rest still quote; a
  market-wide move taking every venue with it and rejecting nothing; the band widening by itself
  so that the same 3 bp deviation is an outlier in a calm cross-section and ordinary in a fast
  one; a split cross-section refusing to quote rather than averaging through it; two venues
  declining to attribute an outlier; a venue dropping out moving the reference by less than the
  threshold.
- **skew** — a flat market skewing zero *while the imbalance is non-zero*; the estimator
  converging on the volatility it is fed and scaling as √t; an outage gap re-anchoring instead of
  becoming one enormous return; 2× σ producing 4× skew and not 2×; the book-lifting cap being the
  tighter one; the `minPrice` clamp checked against `dubu-core` by building a row at the clamp and
  failing one bp past it; and **no skew in range crossing the book**, checked across the whole
  range rather than argued.
- **heads** — a real `newHeads` frame parsed out of **both** a text and a binary frame; a
  notification from *another* subscription not counting as our liveness; a replayed header not
  resetting the watchdog; a connected-but-silent subscription reading `Stale` rather than `Live`
  or `Down`; a reconnect clearing the stored head rather than resurrecting it.
- **chain health** — a frozen chain escalating to `Down` **while every read succeeds**, and the
  stall reason naming which of the two signals went quiet.
- **config** — the API key surviving no formatter: `Display`, `Debug`, and a `{:?}` dump of the
  whole config; `${VAR}` expansion naming the variable and never the value; `.env` never
  overriding the real environment.
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
