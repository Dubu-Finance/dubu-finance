import { describe, expect, it } from 'vitest';
import { decodeFunctionData, getAddress } from 'viem';

import { ROUTER_ABI } from '../src/abi.js';
import { loadConfig, MARKETS, type Env, type Market } from '../src/config.js';
import { chooseSplit, SPLIT_GRID, WEIGHT_DENOMINATOR } from '../src/quote.js';
import { buildRoute, packStep, univ2Pair } from '../src/route.js';

const env: Env = {
  ROUTER: '0x2B10D0b50ca3A7c0C7CCaBc969615b4Db3fb9471',
  PROP_POOL: '0xBbE55E29BbC6d71EcAb1ac011c9Ac5206aB2Fe74',
  PROP_ADAPTER: '0x16C5A0df5Ad0c8b0A450eDaa67c56593B02D19e2',
  UNIV2_ADAPTER: '0xA7383784E39d2d3C717C61735A363654360DeF46',
  UNIV2_ROUTER02: '0x98E2aa881cEFe66E394C8261d1D1BdE25D4BffA6',
};

const cfg = loadConfig(env);
const market = MARKETS[0]!;
const RECEIVER = getAddress('0x5AD176eBb13CAbE62Ee7c07F52a67b4A48CbEf83');

function build(legs: Parameters<typeof buildRoute>[0]['legs'], sellingBase = false) {
  return buildRoute({
    cfg,
    market,
    sellingBase,
    tokenIn: sellingBase ? market.base : market.quote,
    tokenOut: sellingBase ? market.quote : market.base,
    receiver: RECEIVER,
    amountIn: 1_000_000_000n,
    legs,
    rfq: null,
    quotedAmountOut: 500_000_000_000_000_000n,
    minAmountOut: 495_000_000_000_000_000n,
    deadline: 1_785_000_120n,
  });
}

/** Decodes the built calldata back into the plan, so assertions are about what the router receives. */
function decode(data: `0x${string}`) {
  const { args } = decodeFunctionData({ abi: ROUTER_ABI, data });
  return { p: args[0], minAmountOut: args[1] };
}

describe('the step word', () => {
  it('packs the pool into the low 160 bits and the weight above it', () => {
    const w = packStep(cfg.propPool, 6000, false, false);
    expect(BigInt.asUintN(160, w)).toBe(BigInt(cfg.propPool));
    expect((w >> 160n) & 0xffffn).toBe(6000n);
  });

  it('sets bit 255 for reverse and bit 254 for fundAdapter, independently', () => {
    expect(packStep(cfg.propPool, 0, true, false) >> 255n).toBe(1n);
    expect((packStep(cfg.propPool, 0, false, true) >> 254n) & 1n).toBe(1n);
    expect(packStep(cfg.propPool, 0, false, false) >> 254n).toBe(0n);
  });

  /** `RouteDecoder` reverts `ReservedBitsSet` on anything in 253..176. */
  it('leaves the reserved bits clear', () => {
    const w = packStep(cfg.propPool, WEIGHT_DENOMINATOR, true, true);
    const reserved = (w >> 176n) & ((1n << 78n) - 1n);
    expect(reserved).toBe(0n);
  });

  it('refuses a weight above the denominator rather than truncating it', () => {
    expect(() => packStep(cfg.propPool, WEIGHT_DENOMINATOR + 1, false, false)).toThrow();
  });
});

describe('funding', () => {
  // The AMM legs settle against their own balance delta; the tokens belong at the pool.
  it('does not set fundAdapter on an AMM leg', () => {
    const { data } = build([{ venue: 'prop', weightBps: 10000, amountIn: 1n, amountOut: 1n }]);
    const step = decode(data).p.batches[0]!.hops[0]!.steps[0]!;
    expect((step.rawData >> 254n) & 1n).toBe(0n);
  });

  // PmmSettle pulls the taker leg from msg.sender, which is the adapter. Forget this and the
  // adapter reads a zero balance and reverts NothingToFill.
  it('sets fundAdapter on the RFQ leg', () => {
    const withRfq = loadConfig({
      ...env,
      PMM_SETTLE: '0x68CFa6E265AffD5D0DB2C49E4bb9DaEC5A920A9E',
      PMM_ADAPTER: '0x92CC1139212d02c8CF198dE804161432feEa4eBD',
      RFQ_MAKER_URL: 'https://maker.example/quote',
      RFQ_MAKER_ADDRESS: RECEIVER,
    });
    const { data } = buildRoute({
      cfg: withRfq,
      market,
      sellingBase: false,
      tokenIn: market.quote,
      tokenOut: market.base,
      receiver: RECEIVER,
      amountIn: 1_000_000_000n,
      legs: [],
      rfq: {
        weightBps: WEIGHT_DENOMINATOR,
        quote: {
          amountOut: 1n,
          signature: `0x${'11'.repeat(65)}`,
          order: {
            maker: RECEIVER,
            makerAsset: market.base,
            takerAsset: market.quote,
            makerAmount: 1n,
            takerAmount: 1n,
            nonce: 1n,
            expiry: 1_785_000_120n,
            decayStart: 0n,
            decayPerSec: 0,
            decayCap: 0,
            minFillBps: 0,
          },
        },
      },
      quotedAmountOut: 1n,
      minAmountOut: 1n,
      deadline: 1_785_000_120n,
    });
    const step = decode(data).p.batches[0]!.hops[0]!.steps[0]!;
    expect((step.rawData >> 254n) & 1n).toBe(1n);
    // The step's pool is PmmSettle, never the maker.
    expect(getAddress(`0x${BigInt.asUintN(160, step.rawData).toString(16).padStart(40, '0')}`)).toBe(
      withRfq.pmmSettle,
    );
  });
});

describe('direction', () => {
  it('sets reverse when the route buys base with quote, and not when it sells base', () => {
    const buying = build([{ venue: 'prop', weightBps: 10000, amountIn: 1n, amountOut: 1n }], false);
    const selling = build([{ venue: 'prop', weightBps: 10000, amountIn: 1n, amountOut: 1n }], true);
    expect(decode(buying.data).p.batches[0]!.hops[0]!.steps[0]!.rawData >> 255n).toBe(1n);
    expect(decode(selling.data).p.batches[0]!.hops[0]!.steps[0]!.rawData >> 255n).toBe(0n);
  });
});

describe('a split route', () => {
  it('becomes two forks in one hop whose weights sum to the denominator', () => {
    const { data, venues } = build([
      { venue: 'prop', weightBps: 7000, amountIn: 7n, amountOut: 7n },
      { venue: 'univ2', weightBps: 3000, amountIn: 3n, amountOut: 3n },
    ]);
    const p = decode(data).p;
    expect(p.batches).toHaveLength(1);
    expect(p.batches[0]!.hops).toHaveLength(1);
    const steps = p.batches[0]!.hops[0]!.steps;
    expect(steps).toHaveLength(2);
    const weights = steps.map((s) => Number((s.rawData >> 160n) & 0xffffn));
    expect(weights).toEqual([7000, 3000]);
    expect(weights[0]! + weights[1]!).toBe(WEIGHT_DENOMINATOR);
    expect(venues).toEqual(['prop', 'univ2']);
  });

  it('points each fork at its own venue and adapter', () => {
    const { data } = build([
      { venue: 'prop', weightBps: 5000, amountIn: 5n, amountOut: 5n },
      { venue: 'univ2', weightBps: 5000, amountIn: 5n, amountOut: 5n },
    ]);
    const steps = decode(data).p.batches[0]!.hops[0]!.steps;
    expect(getAddress(steps[0]!.adapter)).toBe(cfg.propAdapter);
    expect(getAddress(steps[1]!.adapter)).toBe(cfg.univ2Adapter);
    expect(BigInt.asUintN(160, steps[1]!.rawData)).toBe(BigInt(univ2Pair(cfg, market)));
  });

  it('drops a zero-weight leg rather than encoding a no-op fork', () => {
    const { data } = build([
      { venue: 'prop', weightBps: 10000, amountIn: 10n, amountOut: 10n },
      { venue: 'univ2', weightBps: 0, amountIn: 0n, amountOut: 0n },
    ]);
    expect(decode(data).p.batches[0]!.hops[0]!.steps).toHaveLength(1);
  });

  it('refuses to build a route with nothing in it', () => {
    expect(() => build([])).toThrow();
  });
});

describe('slippage', () => {
  it('carries minAmountOut through to the router as the caller signs it', () => {
    const { data } = build([{ venue: 'prop', weightBps: 10000, amountIn: 1n, amountOut: 1n }]);
    const { p, minAmountOut } = decode(data);
    expect(minAmountOut).toBe(495_000_000_000_000_000n);
    expect(p.quotedAmountOut).toBe(500_000_000_000_000_000n);
    expect(getAddress(p.receiver)).toBe(RECEIVER);
  });
});

/**
 * Every market now has a UniV2 pool, and did not always.
 *
 * The pool quotes nine pairs; UniV2 had liquidity for two, so a taker asking for mSOL was told the
 * pair did not exist while the maker was quoting it. The seven missing pools were created on
 * 2026-07-31 and the table in `route.ts` lists all nine.
 *
 * The behaviour that mattered then still has to hold, because a market can always be listed before
 * its pool exists: it must still route on the prop leg alone, and the split search must never hand
 * UniV2 a leg it cannot pack. Those are driven from a synthetic market below rather than from
 * whichever real pair happens to be missing a pool, so the guard survives a complete deployment.
 */
describe('markets without a UniV2 pool', () => {
  it('is no longer any real market', () => {
    const withoutPool = MARKETS.filter((m) => throws(() => univ2Pair(cfg, m)));
    expect(withoutPool).toEqual([]);
    expect(MARKETS.length).toBeGreaterThan(0);
  });

  // A pairId the table cannot know about, which is the state a newly listed market is in.
  const template = MARKETS[0];
  if (!template) throw new Error('MARKETS is empty');
  const unlisted: Market = { ...template, pairId: 9999, symbol: 'mNEW/mUSDC' };

  it('still builds an all-prop route', () => {
    const route = buildRoute({
      cfg,
      market: unlisted,
      sellingBase: false,
      tokenIn: unlisted.quote,
      tokenOut: unlisted.base,
      receiver: RECEIVER,
      amountIn: 1_000_000n,
      legs: [{ venue: 'prop', weightBps: WEIGHT_DENOMINATOR, amountIn: 1_000_000n, amountOut: 1n }],
      rfq: null,
      quotedAmountOut: 1n,
      minAmountOut: 1n,
      deadline: 1_785_000_120n,
    });
    expect(route.venues).toEqual(['prop']);
  });

  // The throw is the guard: it fires only if the split search ever hands UniV2 a leg on a market
  // with no pool, which would otherwise pack the zero address as the pool and settle into nothing.
  it('refuses to pack a UniV2 leg', () => {
    expect(() => univ2Pair(cfg, unlisted)).toThrow();
  });
});

function throws(f: () => unknown): boolean {
  try {
    f();
    return false;
  } catch {
    return true;
  }
}

describe('the split search', () => {
  const grid = SPLIT_GRID.length;

  it('takes the single best venue when splitting does not help', () => {
    // Prop pays more at every size; the optimum is all-prop.
    const prop = Array.from({ length: grid }, (_, i) => BigInt(SPLIT_GRID[i]!) * 11n);
    const univ = Array.from({ length: grid }, (_, i) => BigInt(WEIGHT_DENOMINATOR - SPLIT_GRID[i]!) * 10n);
    const r = chooseSplit(10_000n, prop, univ);
    expect(r.split).toBe(false);
    expect(r.legs[0]!.venue).toBe('prop');
    expect(r.legs[0]!.weightBps).toBe(WEIGHT_DENOMINATOR);
  });

  it('splits when the combination beats either venue alone', () => {
    // A hump in the middle: neither extreme is best.
    const prop = SPLIT_GRID.map((bps) => (bps === 5000 ? 900n : bps === 10000 ? 1000n : 400n));
    const univ = SPLIT_GRID.map((bps) => (bps === 5000 ? 900n : bps === 0 ? 1000n : 400n));
    const r = chooseSplit(10_000n, prop, univ);
    expect(r.split).toBe(true);
    expect(r.amountOut).toBe(1800n);
    expect(r.legs.map((l) => l.weightBps)).toEqual([5000, 5000]);
  });

  // A venue answering zero to a non-zero request is refusing, and routing into it would burn that
  // share of the input. The all-other-venue point is already on the grid.
  it('disqualifies a grid point where a venue refuses the size', () => {
    const prop = SPLIT_GRID.map(() => 0n); // prop refuses everything
    const univ = SPLIT_GRID.map((bps) => BigInt(WEIGHT_DENOMINATOR - bps));
    const r = chooseSplit(10_000n, prop, univ);
    expect(r.legs).toHaveLength(1);
    expect(r.legs[0]!.venue).toBe('univ2');
    expect(r.legs[0]!.weightBps).toBe(WEIGHT_DENOMINATOR);
  });

  it('returns nothing routable when every venue refuses', () => {
    const zero = SPLIT_GRID.map(() => 0n);
    const r = chooseSplit(10_000n, zero, zero);
    expect(r.legs).toHaveLength(0);
    expect(r.amountOut).toBe(0n);
  });
});
