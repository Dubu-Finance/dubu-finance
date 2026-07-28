# Deployments

## GIWA Sepolia (chain id 91342)

Deployed 2026-07-27 from `0x5AD176eBb13CAbE62Ee7c07F52a67b4A48CbEf83`, nonce 0.

The original 11 contracts are verified on Blockscout. **`PropPool`, `PmmSettle` and `PmmAdapter`
below are not** — they were deployed in a later pass with verification deliberately skipped, and
the source is in this repo at the commit that deployed them.

| contract | address |
|---|---|
| mUSDC (6 dec) | [`0xd28596C6750D87C53EA146134AfAB53de86C5155`](https://sepolia-explorer.giwa.io/address/0xd28596C6750D87C53EA146134AfAB53de86C5155) |
| mWETH (18 dec) | [`0x81e46C6379498beBEB5DCcD47ab2DdFaf967d445`](https://sepolia-explorer.giwa.io/address/0x81e46C6379498beBEB5DCcD47ab2DdFaf967d445) |
| mWBTC (8 dec) | [`0x3548991B5EF2D7805EFa95bEa6CeDeAee3869875`](https://sepolia-explorer.giwa.io/address/0x3548991B5EF2D7805EFa95bEa6CeDeAee3869875) |
| mBNB (18 dec) | [`0x54fbDB9F5bf1c345F0230773C66607DF3f7b99AC`](https://sepolia-explorer.giwa.io/address/0x54fbDB9F5bf1c345F0230773C66607DF3f7b99AC) |
| mXRP (6 dec) | [`0x4Cbc341D56232805B258ed5a33C7b80dbF1A9d01`](https://sepolia-explorer.giwa.io/address/0x4Cbc341D56232805B258ed5a33C7b80dbF1A9d01) |
| mSOL (9 dec) | [`0x1F96E44136D765802005c5083a51830841dca9b3`](https://sepolia-explorer.giwa.io/address/0x1F96E44136D765802005c5083a51830841dca9b3) |
| UniswapV2Factory | [`0x751cd28542301ac7158a0417C9B8475aae22eD59`](https://sepolia-explorer.giwa.io/address/0x751cd28542301ac7158a0417C9B8475aae22eD59) |
| UniswapV2Router02 | [`0x98E2aa881cEFe66E394C8261d1D1BdE25D4BffA6`](https://sepolia-explorer.giwa.io/address/0x98E2aa881cEFe66E394C8261d1D1BdE25D4BffA6) |
| **PropPool** | [`0xBbE55E29BbC6d71EcAb1ac011c9Ac5206aB2Fe74`](https://sepolia-explorer.giwa.io/address/0xBbE55E29BbC6d71EcAb1ac011c9Ac5206aB2Fe74) |
| **Router** | [`0x2B10D0b50ca3A7c0C7CCaBc969615b4Db3fb9471`](https://sepolia-explorer.giwa.io/address/0x2B10D0b50ca3A7c0C7CCaBc969615b4Db3fb9471) |
| PropPoolAdapter | [`0x16C5A0df5Ad0c8b0A450eDaa67c56593B02D19e2`](https://sepolia-explorer.giwa.io/address/0x16C5A0df5Ad0c8b0A450eDaa67c56593B02D19e2) |
| UniV2Adapter | [`0xA7383784E39d2d3C717C61735A363654360DeF46`](https://sepolia-explorer.giwa.io/address/0xA7383784E39d2d3C717C61735A363654360DeF46) |
| **PmmSettle** (RFQ) | [`0x68CFa6E265AffD5D0DB2C49E4bb9DaEC5A920A9E`](https://sepolia-explorer.giwa.io/address/0x68CFa6E265AffD5D0DB2C49E4bb9DaEC5A920A9E) |
| PmmAdapter | [`0x92CC1139212d02c8CF198dE804161432feEa4eBD`](https://sepolia-explorer.giwa.io/address/0x92CC1139212d02c8CF198dE804161432feEa4eBD) |
| pair mWETH/mUSDC | [`0x94f0033BABBa0bEC1C17B808E0980ECFd3B35b4C`](https://sepolia-explorer.giwa.io/address/0x94f0033BABBa0bEC1C17B808E0980ECFd3B35b4C) |
| pair mWBTC/mUSDC | [`0xd6d0f9F9536590b2C0FDC76Fab5FE415F59C0bED`](https://sepolia-explorer.giwa.io/address/0xd6d0f9F9536590b2C0FDC76Fab5FE415F59C0bED) |

Markets: pairId 1 = mWETH/mUSDC (18/6, `priceScaleExp` 24), pairId 2 = mWBTC/mUSDC (8/6, exp 12).

### The pool was redeployed

`PropPool` moved from `0xA629071E606F425dB93310c3ecc35E00Fbe16358` to the address above. The first
deployment predated the Pyth deviation bound and the capacity decay ramp, both of which added
storage, so the live contract and this repo's build had drifted apart — the old address answers
`snapshot(uint16)` but has no `setPairDecay` selector (`0xcddec480`), which is how the drift was
confirmed rather than assumed.

`Router`, `PropPoolAdapter` and `UniV2Adapter` were **not** redeployed and did not need to be. None
of them holds any state: the pool is a call argument carried in the route's step word, so a new pool
address costs nothing but a config change. That is the routing design paying for itself.

The old pool still holds its demo inventory and still works; it is simply not what anything points
at any more. `chain::swaps` keeps a decoder fixture captured from a swap on it, which stays valid —
a historical log does not stop being a real log.

### The RFQ leg

`PmmSettle`'s EIP-712 domain separator is
`0x785df92cfa961225995c562e9a42c1b5645097a5bd5b868c303785afb34c5ee7`, and the deploy script derived
that value independently before broadcasting and then asserted it against the deployed instance. A
maker whose signer disagrees with the chain here produces quotes nobody can fill, with no error on
the maker's side, so it is checked from two directions on purpose.

The maker has approved `PmmSettle` for $1M of notional per asset — 1,000,000 mUSDC, 500 mWETH,
10 mWBTC. Finite rather than `type(uint256).max`: `PmmSettle` custodies nothing, so that allowance
is the maker's entire exposure to a bug in it.

All four roles are the deployer. That is fine on testnet and wrong on mainnet — the split exists
because the updater key is hot and will eventually leak, and it only means something when the
guardian and owner live somewhere else.

Cost: deploy 0.0000254 ETH over 13 transactions, demo 0.0000156 ETH over 203. Gas was 0.001 gwei.

## Measured on chain

`make demo`, 203 transactions, all successful. Both venues seeded to $20M of identical token
inventory, UniV2 seeded exactly at the reference mid, every size measured from a restored state.
Prop ladder: 5 bp half-spread, 25 bp width, $2M per-epoch capacity per side.

Realised cost against the reference mid, in basis points:

```
 notional | BUY base                     | SELL base
          | prop      univ2      ratio   | prop      univ2      ratio
 $1k      | 5.00      31.09      6.21x   | 5.00      30.99      6.19x
 $10k     | 5.06      40.09      7.92x   | 5.06      39.93      7.89x
 $100k    | 5.62      130.09     23.14x  | 5.62      128.41     22.84x
 $1M      | 11.24     1030.09    91.64x  | 11.24     933.89     83.08x
```

Identical to `test/Integration.t.sol` to the hundredth of a bp, despite running at
`priceScaleExp` 24 against the test's 18 — useful confirmation that the exponent is a precision
and gas parameter, not a pricing one.

Router picked the prop AMM at every size, delivering +26.07 bp more base at $1k rising to
+1017.69 bp at $1M.

### What these numbers do not say

Four things, all of which belong next to the table and not in a footnote:

1. **At $1k almost none of the gap is the curve.** UniV2 charges a flat 30 bp fee and the prop
   AMM a 5 bp half-spread; that accounts for essentially the whole 6.2x. The curve only starts
   mattering at size.
2. **91x at $1M is not typical.** It is the largest size inside the epoch, where UniV2's impact
   has compounded and the prop AMM's has not. Re-run at a 30/150 bp ladder and the advantage is
   still 15x, but quoting the 91x as the headline would be dishonest.
3. **Above the epoch capacity the prop AMM refuses and UniV2 fills.** One unit past the ask
   ceiling (~$2M) the pool quotes 0 and the swap reverts `InsufficientCapacity`, while UniV2
   fills at 2033 bp. A venue that declines is not strictly better than one that fills badly — it
   is a different trade-off, and the capacity bound is what limits how much a stale ladder can be
   picked off for.
4. **Adverse selection is not modelled at all.** The reference mid is right by fiat here. On a
   live chain a stale ladder gets picked off and nothing above measures that. These are
   execution-quality numbers for an honest taker, not a claim about the market maker's PnL.

And both tokens are ours and both venues were seeded by us. The reason is token overlap, not an
absence of venues: an earlier version of this file said GIWA had no third-party liquidity to
compare against, and that was wrong. A scan of UniV2/UniV3 `Swap` topics over 100,000 blocks
(~27.8 h, ending block 31,781,085) finds four factories that are not ours and 227 third-party swaps
across 20 pools. Those pools hold real WBTC, USDC, WETH9 and GIWAP; ours hold mocks we minted,
because GIWA has no canonical stablecoin and no Circle CCTP domain — and the third-party WBTC and
USDC expose no `mint` or `faucet` in their bytecode, so that inventory would have to be bought
rather than minted. There is no path between the two token sets, so nothing routes across them.
This measures curves, not markets.

The honest external check is the third-party UniV3 pool
`0x98e5d56f4844cb510ce62cf8e2479b8cbf18acfc` (WBTC/USDC, 0.30%, 2.2901 WBTC + 176,598 USDC,
about $325K). It is not ours and not routable from here, but it prices the same asset: measured at
64,923 USDC/WBTC against a 65,470 Binance mid, **−83.5 bp**, where the UniV2 above was seeded at
the reference mid. `web/index.html` shows all three side by side. This is a small ecosystem — 227
third-party swaps in 27.8 hours, six in that pool — and a pool sitting 83.5 bp off market is not
one being arbitraged tightly.

## Reproducing

```
make deploy     # ~13 tx, resumable — addresses already in .env are reused
make demo       # ~203 tx, idempotent, restores both venues between sizes
make demo-dry   # same table, sends nothing, needs no key
```

### Markets added after the original deployment

`script/AddMarkets.s.sol` added BNB, XRP and SOL against the live pool on 2026-07-28, taking
`pairCount` from 2 to 5. It is a separate script from `Deploy.s.sol` on purpose: `Deploy` re-derives
every market from its own run, so raising `MARKET_COUNT` and re-running it against a live pool would
call `addPair` on the existing two and revert with `PairExists` -- after having already deployed the
new tokens, leaving orphans.

| pair | token | decimals | priceScaleExp | hedge |
|---|---|---|---|---|
| 3 | mBNB | 18 | 24 | BNBUSDT |
| 4 | mXRP | 6 | 15 | XRPUSDT |
| 5 | mSOL | 9 | 16 | SOLUSDT |

The three exponents differ because the tokens carry each chain's own decimals rather than a uniform
18. That is deliberate: identical decimals never exercise `PropCurve`'s alignment path at all.

⚠️ The quote leg was not topped up. All five markets draw on the same mUSDC reserve, so the pool now
splits one balance five ways -- fund it before raising capacity.
