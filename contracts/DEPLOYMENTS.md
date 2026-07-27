# Deployments

## GIWA Sepolia (chain id 91342)

Deployed 2026-07-27 from `0x5AD176eBb13CAbE62Ee7c07F52a67b4A48CbEf83`, nonce 0.
All 11 contracts verified on Blockscout.

| contract | address |
|---|---|
| mUSDC (6 dec) | [`0xd28596C6750D87C53EA146134AfAB53de86C5155`](https://sepolia-explorer.giwa.io/address/0xd28596C6750D87C53EA146134AfAB53de86C5155) |
| mWETH (18 dec) | [`0x81e46C6379498beBEB5DCcD47ab2DdFaf967d445`](https://sepolia-explorer.giwa.io/address/0x81e46C6379498beBEB5DCcD47ab2DdFaf967d445) |
| mWBTC (8 dec) | [`0x3548991B5EF2D7805EFa95bEa6CeDeAee3869875`](https://sepolia-explorer.giwa.io/address/0x3548991B5EF2D7805EFa95bEa6CeDeAee3869875) |
| UniswapV2Factory | [`0x751cd28542301ac7158a0417C9B8475aae22eD59`](https://sepolia-explorer.giwa.io/address/0x751cd28542301ac7158a0417C9B8475aae22eD59) |
| UniswapV2Router02 | [`0x98E2aa881cEFe66E394C8261d1D1BdE25D4BffA6`](https://sepolia-explorer.giwa.io/address/0x98E2aa881cEFe66E394C8261d1D1BdE25D4BffA6) |
| **PropPool** | [`0xA629071E606F425dB93310c3ecc35E00Fbe16358`](https://sepolia-explorer.giwa.io/address/0xA629071E606F425dB93310c3ecc35E00Fbe16358) |
| **Router** | [`0x2B10D0b50ca3A7c0C7CCaBc969615b4Db3fb9471`](https://sepolia-explorer.giwa.io/address/0x2B10D0b50ca3A7c0C7CCaBc969615b4Db3fb9471) |
| PropPoolAdapter | [`0x16C5A0df5Ad0c8b0A450eDaa67c56593B02D19e2`](https://sepolia-explorer.giwa.io/address/0x16C5A0df5Ad0c8b0A450eDaa67c56593B02D19e2) |
| UniV2Adapter | [`0xA7383784E39d2d3C717C61735A363654360DeF46`](https://sepolia-explorer.giwa.io/address/0xA7383784E39d2d3C717C61735A363654360DeF46) |
| pair mWETH/mUSDC | [`0x94f0033BABBa0bEC1C17B808E0980ECFd3B35b4C`](https://sepolia-explorer.giwa.io/address/0x94f0033BABBa0bEC1C17B808E0980ECFd3B35b4C) |
| pair mWBTC/mUSDC | [`0xd6d0f9F9536590b2C0FDC76Fab5FE415F59C0bED`](https://sepolia-explorer.giwa.io/address/0xd6d0f9F9536590b2C0FDC76Fab5FE415F59C0bED) |

Markets: pairId 1 = mWETH/mUSDC (18/6, `priceScaleExp` 24), pairId 2 = mWBTC/mUSDC (8/6, exp 12).

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

And both tokens are ours and both venues were seeded by us, because GIWA has no canonical
stablecoin to compare against. This measures curves, not markets.

## Reproducing

```
make deploy     # ~13 tx, resumable — addresses already in .env are reused
make demo       # ~203 tx, idempotent, restores both venues between sizes
make demo-dry   # same table, sends nothing, needs no key
```
