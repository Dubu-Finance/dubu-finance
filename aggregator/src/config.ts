import { getAddress, type Address } from 'viem';

/**
 * Everything the worker reads from its environment, and the market table it serves.
 *
 * The token set is compiled in rather than fetched. An aggregator that discovers its own markets
 * at runtime is one prompt-injection away from routing into a contract an attacker named, and
 * "the list of things I will quote" is exactly the decision that should not be delegated to a
 * network response. Adding a market is a deploy.
 */

/** Cloudflare binds these; `wrangler.toml` documents which are secrets. */
export interface Env {
  RPC_URL?: string;
  FALLBACK_RPC_URLS?: string;
  ROUTER?: string;
  PROP_POOL?: string;
  PROP_ADAPTER?: string;
  UNIV2_ADAPTER?: string;
  UNIV2_ROUTER02?: string;
  PMM_SETTLE?: string;
  PMM_ADAPTER?: string;
  /** The full URL of the engine's prop-quote endpoint, path included. Unset means UniV2-only. */
  PROP_QUOTE_URL?: string;
  /** Where to ask for a signed RFQ quote. Unset means AMM-only routing. */
  RFQ_MAKER_URL?: string;
  /** The address every RFQ order must recover to. Unset means RFQ is refused outright. */
  RFQ_MAKER_ADDRESS?: string;
  PARTNER_ID?: string;
}

export interface Market {
  pairId: number;
  symbol: string;
  base: Address;
  quote: Address;
  baseDecimals: number;
  quoteDecimals: number;
}

export interface Config {
  chainId: number;
  rpcUrl: string;
  /** Tried in order when `rpcUrl` fails. A single endpoint is a single point of `prop: —`. */
  fallbackRpcUrls: string[];
  router: Address;
  propPool: Address;
  propAdapter: Address;
  univ2Adapter: Address;
  univ2Router: Address;
  pmmSettle: Address | null;
  pmmAdapter: Address | null;
  /**
   * Where the prop pool is priced, now that it is priced over HTTP rather than by `eth_call`.
   *
   * A **full endpoint URL**, POSTed to verbatim — `https://<host>/prop/amounts`, not `https://<host>`.
   * Nothing appends a path, so a base URL answers 404 and the leg reads as the engine having no
   * market for the pair. `RFQ_MAKER_URL` cost a debugging session to exactly that mistake, which is
   * why `quote.ts` logs a bare 404 and a declining engine as different things.
   *
   * `null` means the prop venue is not configured and every quote is UniV2-only. There is no
   * on-chain fallback by design: an unreachable engine shows as prop being unavailable, not as a
   * second and differently-wrong price.
   */
  propQuoteUrl: string | null;
  rfqMakerUrl: string | null;
  rfqMakerAddress: Address | null;
  partnerId: bigint;
  markets: Market[];
}

export const CHAIN_ID = 91342;
export const MULTICALL3: Address = '0xcA11bde05977b3631167028862bE2a173976CA11';
// The flashblocks-aware endpoint, and the `pending` tag that goes with it.
//
// Reading `latest` on a plain node meant a DuBu quote did not exist for a taker until its block
// sealed -- up to a second after the maker had already replaced it. The pool re-quotes every ~415ms,
// so most of what it publishes was invisible here, and the price this aggregator returned was
// systematically the previous one.
//
// Preconfirmed state is not final, which is the honest cost: a quote read here can be reordered
// before sealing. That is what `minAmountOut` is for, and it is a far smaller error than quoting a
// price the maker has already moved off.
//
// The quote path no longer reads `pending`, and that is the history rather than a contradiction:
// serving preconfirmed state and a preconfirmed timestamp inconsistently is what made `PropPool`
// call every quote stale, and it is why the pool is now priced over HTTP instead (see quote.ts).
// The UniV2 multicall reads sealed `latest` by choice. What still wants this endpoint is
// `makerCanDeliver`, which reads a balance and an allowance the maker may have moved this second.
export const DEFAULT_RPC = 'https://sepolia-rpc-flashblocks.giwa.io';

const MUSDC: Address = getAddress('0xd28596C6750D87C53EA146134AfAB53de86C5155');
const MWETH: Address = getAddress('0x81e46C6379498beBEB5DCcD47ab2DdFaf967d445');
const MWBTC: Address = getAddress('0x3548991B5EF2D7805EFa95bEa6CeDeAee3869875');
const MBNB: Address = getAddress('0x54fbDB9F5bf1c345F0230773C66607DF3f7b99AC');
const MXRP: Address = getAddress('0x4Cbc341D56232805B258ed5a33C7b80dbF1A9d01');
const MSOL: Address = getAddress('0x1F96E44136D765802005c5083a51830841dca9b3');
const MAAPL: Address = getAddress('0xab3F1C8A9358Feb5872F81330FC811C3c53Ae9ff');
const MTSLA: Address = getAddress('0xf5456CF225efaf7807cBC14079733b211eAc84d7');
const MSKHY: Address = getAddress('0x37D1e1307eba9B489844B9A1198b5F77577630FD');
const MSPCX: Address = getAddress('0x38EfEf195b347B9EcEf07185C716C9A93E232B9a');

/**
 * GIWA Sepolia, matching `contracts/DEPLOYMENTS.md` and the pool's own `pairConfig`.
 *
 * All nine pairs the pool quotes, not the two that also have a UniV2 pool. Those are different
 * sets, and listing the intersection is what made a taker asking for mSOL see "this pair is not
 * available" while the maker was quoting it forty-eight times a minute.
 *
 * A pair with no UniV2 pool costs the grid nothing. `getAmountsOut` reverts with no reserves,
 * `allowFailure` turns that into a zero, every split routing anything to UniV2 is disqualified for
 * paying nothing, and the all-prop point wins on its own — which is the same path a UniV2 outage
 * takes on a pair that does have a pool.
 *
 * `baseDecimals` was read from each token's `decimals()` rather than assumed. They are not uniform
 * — mSOL is 9, mXRP is 6, the equities are 8 — and a wrong one misprices by orders of magnitude
 * without failing anything.
 *
 * # Adding a market has an on-chain prerequisite, and nothing here checks it
 *
 * The prop leg works the moment a row is added: the pool already quotes the pair. The RFQ leg does
 * not, and fails quietly. `PmmSettle` custodies nothing and settles by `transferFrom` against the
 * maker's own balance, so the maker must have approved it for the base token — and the seven pairs
 * added after the first two had an allowance of zero while holding plenty of inventory. The engine
 * answered `insufficient-inventory`, the aggregator surfaced `refused`, and `refused` reads as the
 * maker declining a price rather than as a missing approval.
 *
 * So when adding a market: check `allowance(maker, PmmSettle)` for the base token, not just the
 * maker's balance. A zero there is the whole difference between the RFQ leg quoting and silently
 * never quoting.
 */
export const MARKETS: Market[] = [
  { pairId: 1, symbol: 'mWETH/mUSDC', base: MWETH, quote: MUSDC, baseDecimals: 18, quoteDecimals: 6 },
  { pairId: 2, symbol: 'mWBTC/mUSDC', base: MWBTC, quote: MUSDC, baseDecimals: 8, quoteDecimals: 6 },
  { pairId: 3, symbol: 'mBNB/mUSDC', base: MBNB, quote: MUSDC, baseDecimals: 18, quoteDecimals: 6 },
  { pairId: 4, symbol: 'mXRP/mUSDC', base: MXRP, quote: MUSDC, baseDecimals: 6, quoteDecimals: 6 },
  { pairId: 5, symbol: 'mSOL/mUSDC', base: MSOL, quote: MUSDC, baseDecimals: 9, quoteDecimals: 6 },
  { pairId: 6, symbol: 'mAAPL/mUSDC', base: MAAPL, quote: MUSDC, baseDecimals: 8, quoteDecimals: 6 },
  { pairId: 7, symbol: 'mTSLA/mUSDC', base: MTSLA, quote: MUSDC, baseDecimals: 8, quoteDecimals: 6 },
  { pairId: 8, symbol: 'mSKHY/mUSDC', base: MSKHY, quote: MUSDC, baseDecimals: 8, quoteDecimals: 6 },
  { pairId: 9, symbol: 'mSPCX/mUSDC', base: MSPCX, quote: MUSDC, baseDecimals: 8, quoteDecimals: 6 },
];

class ConfigError extends Error {}

function required(env: Env, key: keyof Env, fallback?: string): Address {
  const raw = env[key] ?? fallback;
  if (!raw) throw new ConfigError(`${key} is not configured`);
  return getAddress(raw);
}

function optional(env: Env, key: keyof Env): Address | null {
  const raw = env[key];
  return raw ? getAddress(raw) : null;
}

/**
 * Reads the environment into a fully-resolved config, or throws.
 *
 * The RFQ leg is all-or-nothing: a maker URL without a maker address would mean accepting orders
 * from whoever answers that URL, so the two are checked together and the leg is disabled unless
 * both are present. Disabled means AMM-only routing, which is a worse quote and a working service.
 *
 * The prop leg needs no such pairing, and that is worth stating rather than leaving to be inferred:
 * `PROP_QUOTE_URL` is its only knob. The pool and adapter addresses it routes through are
 * `required` with defaults, so there is no half-configured prop leg to null out wholesale.
 */
export function loadConfig(env: Env): Config {
  const pmmSettle = optional(env, 'PMM_SETTLE');
  const pmmAdapter = optional(env, 'PMM_ADAPTER');
  const rfqMakerAddress = optional(env, 'RFQ_MAKER_ADDRESS');
  const rfqMakerUrl = env.RFQ_MAKER_URL ?? null;

  const rfqReady = Boolean(pmmSettle && pmmAdapter && rfqMakerAddress && rfqMakerUrl);

  return {
    chainId: CHAIN_ID,
    rpcUrl: env.RPC_URL ?? DEFAULT_RPC,
    // The plain public RPC is second: it is not flashblocks-aware, so a quote served from it is a
    // sealed one and up to a second stale. Stale is worse than fresh and far better than absent --
    // reading nothing shows the pool as having no quote at all.
    fallbackRpcUrls: (env.FALLBACK_RPC_URLS ?? 'https://sepolia-rpc.giwa.io')
      .split(',')
      .map((s) => s.trim())
      .filter(Boolean),
    router: required(env, 'ROUTER', '0x2B10D0b50ca3A7c0C7CCaBc969615b4Db3fb9471'),
    propPool: required(env, 'PROP_POOL', '0xBbE55E29BbC6d71EcAb1ac011c9Ac5206aB2Fe74'),
    propAdapter: required(env, 'PROP_ADAPTER', '0x16C5A0df5Ad0c8b0A450eDaa67c56593B02D19e2'),
    univ2Adapter: required(env, 'UNIV2_ADAPTER', '0xA7383784E39d2d3C717C61735A363654360DeF46'),
    univ2Router: required(env, 'UNIV2_ROUTER02', '0x98E2aa881cEFe66E394C8261d1D1BdE25D4BffA6'),
    pmmSettle: rfqReady ? pmmSettle : null,
    pmmAdapter: rfqReady ? pmmAdapter : null,
    propQuoteUrl: env.PROP_QUOTE_URL ?? null,
    rfqMakerUrl: rfqReady ? rfqMakerUrl : null,
    rfqMakerAddress: rfqReady ? rfqMakerAddress : null,
    partnerId: BigInt(env.PARTNER_ID ?? '0'),
    markets: MARKETS,
  };
}

/** The market for an ordered token pair, and which way round the trade runs. */
export function findMarket(
  markets: Market[],
  tokenIn: Address,
  tokenOut: Address,
): { market: Market; sellingBase: boolean } | null {
  const a = getAddress(tokenIn);
  const b = getAddress(tokenOut);
  for (const market of markets) {
    if (market.base === a && market.quote === b) return { market, sellingBase: true };
    if (market.quote === a && market.base === b) return { market, sellingBase: false };
  }
  return null;
}
