# `script/` — deploying DuBu to GIWA Sepolia and running the comparison on chain

Two scripts and one shared base.

| file | what it does |
|---|---|
| `Deploy.s.sol` | deploys the whole stack and registers both markets. Resumable. |
| `Demo.s.sol` | seeds both venues, runs the size sweep, routes through the `Router`, prints the table. |
| `DubuScript.sol` | shared base — market definitions, `priceScaleExp` derivation, env plumbing, formatting. Not a script. |

Neither script ever redeploys Permit2, Multicall3, WETH9 or Pyth. Those are GIWA genesis
pre-installs and `Router.PERMIT2` is already a compile-time constant pointing at the canonical
Permit2 address.

---

## The commands, in order

```bash
# 0. sanity — chain is up, deployer is funded
make gas
make balance

# 1. dry run. Sends nothing, needs no key, runs against real chain state.
make deploy-dry

# 2. deploy for real
make deploy                       # keystore   (--account dubu-deployer)
PRIVATE_KEY=0x… make deploy       # raw key    (testnet only)

# 3. paste the `export …` block the script prints into .env

# 4. run the demo
make demo-dry                     # dry run first, same output, no transactions
make demo
```

`make deploy` and `make demo` pick their signing path from the Makefile: `--account dubu-deployer`
by default, `--private-key $PRIVATE_KEY` when `PRIVATE_KEY` is set in the environment or in `.env`.
Nothing in either script reads `PRIVATE_KEY` itself — they call bare `vm.startBroadcast()` and take
whatever wallet forge was given, so both paths work identically.

Set up the keystore once with:

```bash
cast wallet import dubu-deployer --interactive
```

---

## Environment variables

All optional. Every one of them may be present but blank — that is what `.env.example` ships, and
the scripts treat blank as unset rather than reverting on an unparseable address.

### Addresses (resumability)

Set these and the corresponding deployment step is skipped and logged as `reuse`.

| var | contract |
|---|---|
| `MUSDC` | mock USDC, 6 decimals |
| `MWETH` | mock WETH, 18 decimals |
| `MWBTC` | mock WBTC, 8 decimals |
| `UNIV2_FACTORY` | `UniswapV2Factory` |
| `UNIV2_ROUTER02` | `UniswapV2Router02` |
| `PROP_POOL` | `PropPool` |
| `ROUTER` | DuBu aggregation `Router` |
| `PROP_ADAPTER` | `PropPoolAdapter` |
| `UNIV2_ADAPTER` | `UniV2Adapter` |

The V2 pair addresses are **not** env vars — they are looked up from the factory, which is the only
source that cannot be wrong.

### Roles

| var | default | notes |
|---|---|---|
| `OWNER` | deployer | adds pairs, withdraws inventory, rotates roles. Destined for a timelock. |
| `MANAGER` | deployer | per-pair config, inventory top-ups. |
| `UPDATER` | deployer | `updateQuote` / `refreshCapacity` only. Hot key; assume it leaks. |
| `GUARDIAN` | deployer | pause only. Separate hardware from the updater. |

`Deploy` prints which is which and warns when they collapse onto one address. On testnet they all
default to the deployer, which defeats the entire point of the split — the updater role exists so
that a key signing many times a minute can move zero funds, and sharing it with the owner key means
a leaked hot key drains the pool.

**`Demo` requires the sender to hold `owner`, `manager` and `updater`.** It deposits and withdraws
inventory and pushes quotes; there is no way around that. It checks up front and reverts with the
actual role holders printed, rather than failing with `NotUpdater` two hundred transactions in.

### Behaviour

| var | default | notes |
|---|---|---|
| `MAX_STALE_SECS` | `3600` | freshness window for pairs created by `Deploy`. |
| `DEMO_MARKETS` | `3` | bitmask: `1` = mWETH/mUSDC, `2` = mWBTC/mUSDC, `3` = both. |
| `GAS_PRICE_WEI` | `2000000` | what the affordability preflight assumes. Measured price is 1,000,251. |

`MAX_STALE_SECS` is 3600, not the 60 that `test/Integration.t.sol` uses. The test pushes a ladder
and swaps against it in the same block; a broadcast run puts minutes and a couple hundred
transactions between the two. A live quoter runs well inside 60 — that is a statement about the
quoter, not about this default. `Demo` re-pushes the ladder before every measurement anyway, so the
window is a backstop rather than something the demo leans on.

---

## What gets deployed, and the `priceScaleExp` per market

```
mUSDC   6 dec    faucet drip 10,000        ($10,000)
mWETH  18 dec    faucet drip 5             ($10,000 at the reference price)
mWBTC   8 dec    faucet drip 0.1           ($10,000 at the reference price)
```

Both sides of every pair are our own `MintableToken`. GIWA Sepolia has no canonical stablecoin —
Circle publishes no CCTP domain for chain 91342, USDT0 does not list it, and every "USDC" on the
explorer is somebody's mock — and the only asset that arrives honestly is ETH at 0.015/day from the
faucet. A demo on bridged assets tops out at a few dollars of TVL, which makes every slippage curve
meaningless. Mintable tokens make pool depth a deployment parameter instead.

| market | decimals | reference price | `priceScaleExp` | encoded mid | headroom |
|---|---|---|---|---|---|
| mWETH/mUSDC | 18 / 6 | $2,000 | **24** | `2000000000000000` | 35x |
| mWBTC/mUSDC | 8 / 6 | $100,000 | **12** | `1000000000000000` | 71x |

Two markets, not one, because `priceScaleExp` is the identity when both tokens share a decimals
value. A deployment built from a single pair leaves half of that code path dead on chain; 18/6 and
8/6 exercise it in both directions of magnitude.

The exponent is derived, not chosen by hand: `price = midWhole * 10**quoteDecimals * 10**exp /
10**baseDecimals`, searched downward from `PropCurve.MAX_PRICE_SCALE_EXP` (38) for the largest `exp`
at which the **top of the ladder** (`maxAsk`, after both bps multiplications) still fits `uint56`
with 8x of room for the reference price to move. `PropPool.addPair` argues for the largest exponent
that fits, because the two bisected directions converge in about `log2(size / price)` steps and a
finer price is measurably cheaper gas. Taken literally that leaves a WETH/USDC pair unable to quote
a 4x move without a new pair id, and the exponent is immutable. 8x costs exactly one decimal
exponent on both markets here — `addPair`'s own measured table prices that at ~4% of a swap.

Everything checkable is asserted **before** `vm.startBroadcast()`: that every ladder price fits
`uint56`, that `minBid >= minPrice`, that the capacity fits `uint96`, and that
`capacity * type(uint56).max <= PropCurve.MAX_AMOUNT_OUT * 10**exp` — the bound `refreshCapacity`
enforces, checked here so it cannot surface three transactions after the immutable exponent was set.

---

## Cost

Measured against live GIWA state (gas price 1,000,251 wei = 0.001 gwei, deployer balance 0.005 ETH):

| run | transactions | gas actually used | cost |
|---|---|---|---|
| `Deploy`, cold | 13 | 22,756,498 | 0.0000228 ETH |
| `Demo`, cold (deploys its own stack) | 216 | 36,409,498 | 0.0000364 ETH |
| `Demo`, against a deployed stack | 203 | 13,655,800 | 0.0000137 ETH |
| `Demo`, repeat run (already seeded) | 184 | ~12,100,000 | 0.0000121 ETH |

0.005 ETH covers roughly 130 full deploy-plus-demo cycles. Both scripts print an affordability
preflight and **revert before signing anything** if the balance cannot cover their gas budget, so a
thin balance fails at transaction zero rather than at transaction 140.

The preflight is sized against gas *limits*, not gas used, because a node admits a transaction only
if the sender can pay `gasLimit * gasPrice`. See the note on `--gas-estimate-multiplier` below.

A full `make demo` broadcast is a couple hundred sequential transactions. At 1s blocks expect a few
minutes. Use `DEMO_MARKETS=1` to halve it.

---

## When a step fails halfway

The whole design assumes this happens.

**`Deploy` died mid-run.** Scroll back to the `deploy …` lines it already printed, put those
addresses in `.env`, and run it again. Every step reads its address from env first and only deploys
when it is unset; `createPair` is skipped when the factory already has the pair; `addPair` is
skipped when `pairIdFor` already resolves. Nothing is deployed twice and nothing is registered
twice. If forge's own broadcast log survived, the addresses are also in
`broadcast/Deploy.s.sol/91342/run-latest.json`.

**`Deploy` printed `SKIP PropPool.addPair(...)`.** The sender is not the pool's owner. This is the
expected outcome when `OWNER` points at a timelock or a separate key. Register the pairs from the
owner key and re-run:

```bash
cast send $PROP_POOL "addPair(address,address,uint8,uint32,uint56)" \
  $MWETH $MUSDC 24 3600 1000000000000000 --rpc-url $RPC --account <owner-key>
```

(For mWBTC/mUSDC: `24` → `12` and the minPrice → `500000000000000`. Both figures are printed in the
preflight's Markets table; `minPrice` is half the encoded mid.)

**`Demo` died mid-sweep.** Just run it again. It restores both venues to the reference state at the
start of every market and asserts it got there, so a half-finished sweep leaves nothing to clean up
by hand. Repeat runs have been verified to produce a byte-identical table.

**A transaction reverted with `OutOfGas`.** See below — raise `--gas-estimate-multiplier`.

**`Demo` reverted with `sender does not hold owner/manager/updater`.** It printed the three role
holders. Either run it from that key or move the roles.

**`Demo` reverted with `V2 pair is not seeded at the reference mid`.** Somebody traded the pair
between the seeding transaction and the assertion, or the reset did not land. The script prints
both the observed spot and the expected mid. Re-running restores the pair from scratch.

---

## Verifying on Blockscout

`make deploy` already passes `--verify --verifier blockscout --verifier-url
https://sepolia-explorer.giwa.io/api`, so contracts are submitted as they are deployed. To verify
one after the fact:

```bash
forge verify-contract <address> src/PropPool.sol:PropPool \
  --verifier blockscout \
  --verifier-url https://sepolia-explorer.giwa.io/api \
  --rpc-url https://sepolia-rpc.giwa.io \
  --constructor-args $(cast abi-encode "constructor(address,address,address,address)" \
      $OWNER $MANAGER $UPDATER $GUARDIAN)
```

Constructor arguments per contract:

| contract | constructor |
|---|---|
| `MintableToken` | `(string name, string symbol, uint8 decimals, uint256 claimAmount)` |
| `UniswapV2Factory` | `(address feeToSetter)` — the deployer |
| `UniswapV2Router02` | `(address factory, address WETH)` — WETH is `0x4200…0006` |
| `PropPool` | `(address owner, address manager, address updater, address guardian)` |
| `Router`, `PropPoolAdapter`, `UniV2Adapter` | none |

`UniswapV2Pair` is created by the factory via CREATE2 and takes no constructor arguments; verify it
as `src/reference/univ2/UniswapV2Pair.sol:UniswapV2Pair` with no `--constructor-args`.

Two things about this build that matter for a match: `foundry.toml` sets `bytecode_hash = "none"`
and `cbor_metadata = false`, so the deployed bytecode carries no metadata trailer. Blockscout
matches on the standard-json input forge submits, so this is fine — but a manual "flattened source"
paste into the explorer UI will *not* match unless the same settings, `optimizer_runs = 1_000_000`
and `evm_version = "prague"`, are reproduced exactly.

---

## Things that will bite you (all measured, not guessed)

**1. `--gas-estimate-multiplier`.** forge sizes each broadcast transaction from a simulation in
which the *entire script* runs in one EVM, so every account and storage slot is warm after its first
touch. Each transaction then lands cold. On a routed swap — `Router` → adapter → pair → two tokens —
the cold-access surcharge exceeds forge's default 130% head-room: reproduced as an `OutOfGas` revert
inside `UniswapV2Pair.swap` at a 187,903 gas limit. Both Makefile targets therefore pass
`--gas-estimate-multiplier 200`. Gas here is 0.001 gwei and a limit is not a payment, so the margin
is free. If you drive the scripts by hand rather than through `make`, pass it yourself.

**2. `[etherscan]` in `foundry.toml` must not carry `chain = 91342`.** Foundry resolves that field
through alloy-chains' *named* chain table, which has no entry for GIWA, and its presence makes every
`forge script --rpc-url` invocation abort with `Chain 91342 not supported` before a single line of
the script runs — with or without verification flags. The entry keeps the `giwa_sepolia` alias and
the explorer URL; the chain id comes from the RPC. This was removed as part of this work; if
somebody adds it back, both scripts stop running.

**3. The flashblocks RPC is not a drop-in.** `https://sepolia-rpc-flashblocks.giwa.io` exposes
preconfirmed state under the `pending` tag only, and its `latest` lags the ordinary RPC. Do not
point `make deploy` at it.

**4. Every token here has an unauthenticated public `mint`.** `Deploy` warns when `block.chainid`
is not 91342. Anything that holds these tokens as collateral or prices against them can be drained
for the cost of one transaction. They exist to make a testnet demo unconstrained by faucets and
have no other use.

---

## What the demo measures, and what it does not

`Demo` reproduces `test/Integration.t.sol` on chain: same $20M-per-venue TVL, same reference mid,
same 5 bp half-spread / 25 bp width ladder, same $2M-per-epoch capacity, same $1k → $1M sweep. The
mWETH/mUSDC table it produces on chain is **identical, to the hundredth of a basis point**, to the
one the test prints off chain — despite the on-chain pair running at `priceScaleExp = 24` and the
test at 18, which is a useful check that the exponent is a precision and gas parameter and not a
pricing one.

Fairness is checked rather than trusted, before any trade is sent:

* the V2 pair's spot price must **equal** the reference mid (seeded off-mid, "V2 slippage" would
  silently include a mispricing an arbitrageur removes in one block);
* both venues must hold **identical token amounts**, not merely equal notional;
* every measurement is restored to the same starting state afterwards — `refreshCapacity` opens a
  fresh epoch on the prop AMM, and the V2 pair is burned to dust and re-minted to exactly the
  reference reserves — and both restores are asserted;
* every prop fill is cross-checked against `getAmountOut`, the aggregator-facing view, so the number
  an integrator would have been quoted is provably the number that settled.

The script prints its own counterweights next to the table, and they belong on the slide with it:
at small sizes this is a **fee** comparison (30 bp flat versus a 5 bp half-spread) and not a curve
comparison; **above the epoch capacity the prop AMM refuses while V2 fills badly**, which is a
different trade-off and not a strictly better one; and **adverse selection is not modelled at all**,
because the reference mid here is right by fiat. These are execution-quality numbers for an honest
taker. They are not a claim about the market maker's PnL.
