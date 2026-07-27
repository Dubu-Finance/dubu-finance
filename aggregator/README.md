# DuBu aggregator

A Cloudflare Worker that prices every venue on GIWA for one trade and hands back signable calldata.

```
POST /quote     compare venues, return the best route as calldata
GET  /markets   the pairs this deployment will quote
GET  /health
```

`Router` is a pure executor — it never compares venues, never re-splits, never finds a path. This
is the other half.

## What it holds: nothing

No key, no custody, no database. It returns `to` and `data`; the caller decides whether to sign
them, and `minAmountOut` is inside the calldata they sign. That is a deliberate constraint rather
than an omission — the whole point of running in every region is defeated if compromising one
region is worth anything. The worst a hostile replica can do is quote badly, and the caller's
slippage bound is what catches that.

## The split is searched, not derived

The obvious implementation reimplements `PropCurve` here and solves for the optimum split. This one
evaluates a grid of splits through the venues' own view functions and takes the best, at a cost of
one multicall.

That is not laziness. An off-chain reimplementation of the curve is a second implementation that
can disagree with the first, and this codebase has already paid for that lesson once. It also means
the search needs no special case for the epoch capacity, the staleness ramp or a paused pair — all
of those surface as `getAmountOut` returning less, or zero, which the grid handles without knowing
why.

Measured against the live deployment, buying mWETH with mUSDC:

| size | prop alone | univ2 alone | routed | route |
|---|---|---|---|---|
| $10k | 4.9975 | 4.9800 | **4.9975** | prop 100% |
| $1M | 499.44 | 453.31 | **499.44** | prop 100% |
| $3M | **0** | 1151.18 | **1432.82** | prop 60% + univ2 40% |
| $5M | **0** | 1663.33 | **2149.44** | prop 40% + univ2 60% |

The $3M row is the whole argument. The prop pool's epoch capacity is $2M a side, so at $3M it
quotes zero and a single-venue router would fall back to UniV2 for 1151. Filling the pool to just
under its cap and sending the rest to UniV2 pays **24% more**, and nothing in this service knows
what an epoch is.

Route `0xd14d442399d0774b4581c051f78fde8dfe8217ea5ebe1329432d0309ac7fe2db` is that $3M split
executed on GIWA Sepolia: quoted 1432.819880 mWETH, received 1432.819880 mWETH, 237,934 gas.

## The RFQ leg, and why it is off by default

`RFQ_MAKER_URL` and `RFQ_MAKER_ADDRESS` are unset in `wrangler.toml`. Unset means AMM-only routing,
which is a worse quote and a working service.

The maker is the one input this service does not compute, and `src/rfq.ts` treats it accordingly.
Every field of a returned order is checked against what was *asked for* rather than against what
the response says about itself, and the EIP-712 signature is verified here rather than taken on
faith for having arrived over TLS:

- the tokens must be the exact pair requested, in the requested direction
- `takerAmount` must equal the requested input — a different size is a different trade
- the signature must recover to `RFQ_MAKER_ADDRESS`, configured at deploy time; **not** to a
  `maker` field in the response, which would be the response vouching for itself
- the expiry must have 20s of headroom, so a quote cannot expire between being returned and mined
- a quote beating the AMMs by more than 5% is refused. A maker quoting 10x is not generous, it is
  broken or hostile, and routing into it costs the user gas to discover that

Setting the URL without the address would mean accepting orders from whoever answers that URL, so
`config.ts` treats the two as a pair and disables the leg unless both are present.

```
wrangler secret put RFQ_MAKER_URL
wrangler secret put RFQ_MAKER_ADDRESS
```

## What it does not do

**No multi-hop.** Every market is a direct pair against mUSDC, so a hop count the venue set cannot
produce would be untested code.

**No gas-adjusted ranking.** At 0.001 gwei the difference between a one-leg and a two-leg route is
worth less than a rounding error on the quote. Pricing it would be theatre. Both of these become
real work the moment a third venue or a non-mUSDC market lands.

**No RFQ/AMM split.** An order is signed for one `takerAmount`, so a partial fill is a different
order — splitting would mean a second round trip to the maker at the split size, for a gain the
on-chain grid has already mostly captured. RFQ competes for the whole size or not at all.

**Markets are compiled in.** An aggregator that discovers its own markets at runtime is one
injected response away from routing into a contract an attacker named. Adding a market is a deploy.

## Running it

```
npm install
npm run dev          # local worker on :8787
npm test             # 39 tests, no chain needed
npm run typecheck
npm run deploy       # wrangler deploy
```

```
curl -X POST localhost:8787/quote -H 'content-type: application/json' -d '{
  "tokenIn":  "0xd28596C6750D87C53EA146134AfAB53de86C5155",
  "tokenOut": "0x81e46C6379498beBEB5DCcD47ab2DdFaf967d445",
  "amountIn": "10000000000",
  "receiver": "0x…",
  "slippageBps": 50
}'
```

The response carries the chosen route, every venue's standalone quote, and — when RFQ was refused —
which check refused it. A caller should be able to see the work rather than be told an answer.
