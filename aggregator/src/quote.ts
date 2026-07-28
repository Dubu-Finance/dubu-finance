import { createPublicClient, encodeFunctionData, decodeFunctionResult, fallback, http, type Address, type PublicClient } from 'viem';

import { ERC20_ABI, MULTICALL3_ABI, PROP_POOL_ABI, UNIV2_ROUTER_ABI } from './abi.js';
import { MULTICALL3, type Config } from './config.js';

/**
 * What each venue will pay for a given input, and the best way to divide the input between them.
 *
 * # The split is searched, not derived
 *
 * The obvious implementation reimplements each venue's curve here and solves for the optimum. This
 * one asks the venues instead: it evaluates a grid of splits through their own view functions and
 * takes the best. That is slower by exactly one multicall and better for a reason that has already
 * bitten this codebase once — an off-chain reimplementation of `PropCurve` is a second
 * implementation that can disagree with the first, and the disagreements are found by users rather
 * than by tests. `IPropPool.getAmountOut` is the authority on what the pool will pay, so it is what
 * gets asked.
 *
 * It also means the search needs no special case for the epoch capacity, the staleness ramp, a
 * paused pair, or anything else the pool might do next. Those all show up as `getAmountOut`
 * returning less, or returning zero, which the grid handles without knowing why.
 *
 * The grid is coarse on purpose. Between two adjacent points the curve is smooth and nearly flat
 * near the optimum, and the gain from refining is smaller than the price move during the round
 * trip that refining costs.
 */

/** Splits evaluated, as the share of input sent to the prop pool. */
export const SPLIT_GRID = [0, 1000, 2000, 3000, 4000, 5000, 6000, 7000, 8000, 9000, 10000] as const;

export const WEIGHT_DENOMINATOR = 10_000;

export type VenueId = 'prop' | 'univ2';

export interface Leg {
  venue: VenueId;
  /** Share of the total input, in bps of [`WEIGHT_DENOMINATOR`]. */
  weightBps: number;
  amountIn: bigint;
  amountOut: bigint;
}

export interface AmmQuote {
  /** Best combination found. Legs with zero weight are dropped. */
  legs: Leg[];
  amountOut: bigint;
  /** Each venue alone at the full size, for the response to show its work. */
  solo: Record<VenueId, bigint>;
  /** True when the winner uses both venues. */
  split: boolean;
}

export function makeClient(cfg: Config): PublicClient {
  // `fallback`, not a single endpoint, and the reason is a live incident rather than a precaution.
  //
  // The worker read one URL. When GIWA's public RPC rate-limited us -- `-32016 over rate limit`,
  // reproduced by hand on both public endpoints -- the multicall failed, `allowFailure: true`
  // turned that into `success: false`, `decodeProp` returned 0n, and the pool showed as `prop: —`.
  // Not "the pool is down": the pool was quoting, one second old, at a price 0.2% off the
  // reference. It simply could not be read, and a silent zero is indistinguishable from no quote.
  //
  // viem's fallback transport moves to the next URL on error and ranks by observed latency, so a
  // throttled endpoint stops being asked. `rank` is off: reordering during a burst would spread the
  // burst across every endpoint rather than draining one and moving on.
  const urls = [cfg.rpcUrl, ...cfg.fallbackRpcUrls].filter((u, i, a) => u && a.indexOf(u) === i);
  return createPublicClient({
    transport: fallback(
      urls.map((u) => http(u, { retryCount: 1, timeout: 4_000 })),
      { rank: false },
    ),
  });
}

/**
 * Every grid point, priced at both venues, in one round trip.
 *
 * `allowFailure` is set on every call. A venue that reverts — a paused pair, a UniV2 pool with no
 * reserves — must cost its own leg and nothing else. Voiding the batch would turn one venue's
 * outage into no quote at all, which is the opposite of what an aggregator is for.
 */
export async function quoteAmms(
  client: PublicClient,
  cfg: Config,
  tokenIn: Address,
  tokenOut: Address,
  amountIn: bigint,
): Promise<AmmQuote> {
  const points = SPLIT_GRID.map((bps) => ({
    bps,
    toProp: (amountIn * BigInt(bps)) / BigInt(WEIGHT_DENOMINATOR),
  })).map((p) => ({ ...p, toUniv2: amountIn - p.toProp }));

  const calls = points.flatMap((p) => [
    {
      target: cfg.propPool,
      allowFailure: true,
      callData: encodeFunctionData({
        abi: PROP_POOL_ABI,
        functionName: 'getAmountOut',
        args: [tokenIn, tokenOut, p.toProp],
      }),
    },
    {
      target: cfg.univ2Router,
      allowFailure: true,
      callData: encodeFunctionData({
        abi: UNIV2_ROUTER_ABI,
        functionName: 'getAmountsOut',
        args: [p.toUniv2, [tokenIn, tokenOut]],
      }),
    },
  ]);

  const results = (await client.readContract({
    address: MULTICALL3,
    abi: MULTICALL3_ABI,
    functionName: 'aggregate3',
    args: [calls],
    // Preconfirmed state. viem defaults to `latest`, which is a SEALED block -- and the pool
    // re-quotes every ~415ms, so `latest` served the previous quote for most of every second. See
    // DEFAULT_RPC.
    blockTag: 'pending',
  })) as readonly { success: boolean; returnData: `0x${string}` }[];

  let best: AmmQuote | null = null;
  const solo: Record<VenueId, bigint> = { prop: 0n, univ2: 0n };

  points.forEach((p, i) => {
    const propOut = p.toProp === 0n ? 0n : decodeProp(results[i * 2]);
    const univOut = p.toUniv2 === 0n ? 0n : decodeUniv2(results[i * 2 + 1]);

    if (p.bps === WEIGHT_DENOMINATOR) solo.prop = propOut;
    if (p.bps === 0) solo.univ2 = univOut;

    // A zero from a venue that was asked for a non-zero amount is a refusal, not a free leg. It
    // has to disqualify the whole point: routing input into a venue that pays nothing is strictly
    // worse than not routing it at all, and the grid contains the not-routing-it point already.
    if ((p.toProp > 0n && propOut === 0n) || (p.toUniv2 > 0n && univOut === 0n)) return;

    const total = propOut + univOut;
    if (best && total <= best.amountOut) return;

    const legs: Leg[] = [];
    if (p.toProp > 0n) legs.push({ venue: 'prop', weightBps: p.bps, amountIn: p.toProp, amountOut: propOut });
    if (p.toUniv2 > 0n) {
      legs.push({
        venue: 'univ2',
        weightBps: WEIGHT_DENOMINATOR - p.bps,
        amountIn: p.toUniv2,
        amountOut: univOut,
      });
    }
    best = { legs, amountOut: total, solo, split: legs.length > 1 };
  });

  return best ?? { legs: [], amountOut: 0n, solo, split: false };
}

function decodeProp(r: { success: boolean; returnData: `0x${string}` } | undefined): bigint {
  if (!r?.success) return 0n;
  try {
    return decodeFunctionResult({ abi: PROP_POOL_ABI, functionName: 'getAmountOut', data: r.returnData }) as bigint;
  } catch {
    return 0n;
  }
}

function decodeUniv2(r: { success: boolean; returnData: `0x${string}` } | undefined): bigint {
  if (!r?.success) return 0n;
  try {
    const amounts = decodeFunctionResult({
      abi: UNIV2_ROUTER_ABI,
      functionName: 'getAmountsOut',
      data: r.returnData,
    }) as readonly bigint[];
    return amounts[amounts.length - 1] ?? 0n;
  } catch {
    return 0n;
  }
}

/**
 * Picks the best grid point from pre-computed venue outputs. Split out so the search is testable
 * without a chain.
 *
 * `propOut[i]` and `univOut[i]` are what each venue pays for grid point `i`; a venue asked for a
 * non-zero amount that answers zero disqualifies that point.
 */
export function chooseSplit(amountIn: bigint, propOut: bigint[], univOut: bigint[]): AmmQuote {
  const solo: Record<VenueId, bigint> = {
    prop: propOut[SPLIT_GRID.length - 1] ?? 0n,
    univ2: univOut[0] ?? 0n,
  };
  let best: AmmQuote | null = null;

  SPLIT_GRID.forEach((bps, i) => {
    const toProp = (amountIn * BigInt(bps)) / BigInt(WEIGHT_DENOMINATOR);
    const toUniv2 = amountIn - toProp;
    const p = toProp === 0n ? 0n : (propOut[i] ?? 0n);
    const u = toUniv2 === 0n ? 0n : (univOut[i] ?? 0n);
    if ((toProp > 0n && p === 0n) || (toUniv2 > 0n && u === 0n)) return;

    const total = p + u;
    if (best && total <= best.amountOut) return;

    const legs: Leg[] = [];
    if (toProp > 0n) legs.push({ venue: 'prop', weightBps: bps, amountIn: toProp, amountOut: p });
    if (toUniv2 > 0n) {
      legs.push({ venue: 'univ2', weightBps: WEIGHT_DENOMINATOR - bps, amountIn: toUniv2, amountOut: u });
    }
    best = { legs, amountOut: total, solo, split: legs.length > 1 };
  });

  return best ?? { legs: [], amountOut: 0n, solo, split: false };
}

/**
 * The most the RFQ maker could pay out in `token`: the lesser of what it holds and what it has
 * allowed `PmmSettle` to pull.
 *
 * The lesser of the two, because `PmmSettle` custodies nothing — it issues a `transferFrom` against
 * the maker's own balance, so either being short is the same failure. Reading both is the direct
 * form of the question "will this order settle", which is what an earlier version tried to infer
 * from how good the price looked. See `rfq.ts`.
 *
 * `undefined` when either read fails, and the caller must treat that as *unverified* rather than
 * as passing. A missing observation that quietly reads as a clean bill of health is the failure
 * mode `markout`'s `unmarked` counter exists to avoid, in a different corner of the system.
 */
export async function makerCanDeliver(
  client: PublicClient,
  token: Address,
  maker: Address,
  settler: Address,
): Promise<bigint | undefined> {
  const calls = [
    {
      target: token,
      allowFailure: true,
      callData: encodeFunctionData({ abi: ERC20_ABI, functionName: 'balanceOf', args: [maker] }),
    },
    {
      target: token,
      allowFailure: true,
      callData: encodeFunctionData({ abi: ERC20_ABI, functionName: 'allowance', args: [maker, settler] }),
    },
  ];

  try {
    const results = (await client.readContract({
      address: MULTICALL3,
      abi: MULTICALL3_ABI,
      functionName: 'aggregate3',
      args: [calls],
      // Same reason as the quote path: a sealed read cannot see a transfer or an approval the maker
      // made in the last second, so a maker that CAN deliver gets refused as if it could not.
      blockTag: 'pending',
    })) as readonly { success: boolean; returnData: `0x${string}` }[];

    const balance = decodeUint(results[0], 'balanceOf');
    const allowance = decodeUint(results[1], 'allowance');
    if (balance === undefined || allowance === undefined) return undefined;
    return balance < allowance ? balance : allowance;
  } catch {
    return undefined;
  }
}

function decodeUint(
  r: { success: boolean; returnData: `0x${string}` } | undefined,
  functionName: 'balanceOf' | 'allowance',
): bigint | undefined {
  if (!r?.success) return undefined;
  try {
    return decodeFunctionResult({ abi: ERC20_ABI, functionName, data: r.returnData }) as bigint;
  } catch {
    return undefined;
  }
}
