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
  ROUTER?: string;
  PROP_POOL?: string;
  PROP_ADAPTER?: string;
  UNIV2_ADAPTER?: string;
  UNIV2_ROUTER02?: string;
  PMM_SETTLE?: string;
  PMM_ADAPTER?: string;
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
  router: Address;
  propPool: Address;
  propAdapter: Address;
  univ2Adapter: Address;
  univ2Router: Address;
  pmmSettle: Address | null;
  pmmAdapter: Address | null;
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
export const DEFAULT_RPC = 'https://sepolia-rpc-flashblocks.giwa.io';

const MUSDC: Address = getAddress('0xd28596C6750D87C53EA146134AfAB53de86C5155');
const MWETH: Address = getAddress('0x81e46C6379498beBEB5DCcD47ab2DdFaf967d445');
const MWBTC: Address = getAddress('0x3548991B5EF2D7805EFa95bEa6CeDeAee3869875');

/** GIWA Sepolia, matching `contracts/DEPLOYMENTS.md`. */
export const MARKETS: Market[] = [
  { pairId: 1, symbol: 'mWETH/mUSDC', base: MWETH, quote: MUSDC, baseDecimals: 18, quoteDecimals: 6 },
  { pairId: 2, symbol: 'mWBTC/mUSDC', base: MWBTC, quote: MUSDC, baseDecimals: 8, quoteDecimals: 6 },
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
    router: required(env, 'ROUTER', '0x2B10D0b50ca3A7c0C7CCaBc969615b4Db3fb9471'),
    propPool: required(env, 'PROP_POOL', '0xBbE55E29BbC6d71EcAb1ac011c9Ac5206aB2Fe74'),
    propAdapter: required(env, 'PROP_ADAPTER', '0x16C5A0df5Ad0c8b0A450eDaa67c56593B02D19e2'),
    univ2Adapter: required(env, 'UNIV2_ADAPTER', '0xA7383784E39d2d3C717C61735A363654360DeF46'),
    univ2Router: required(env, 'UNIV2_ROUTER02', '0x98E2aa881cEFe66E394C8261d1D1BdE25D4BffA6'),
    pmmSettle: rfqReady ? pmmSettle : null,
    pmmAdapter: rfqReady ? pmmAdapter : null,
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
