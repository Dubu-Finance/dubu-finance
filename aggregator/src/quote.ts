import {
  createPublicClient,
  encodeFunctionData,
  decodeFunctionResult,
  fallback,
  http,
  type Address,
  type PublicClient,
} from 'viem';

import { ERC20_ABI, MULTICALL3_ABI, UNIV2_ROUTER_ABI } from './abi.js';
import { MULTICALL3, type Config } from './config.js';

/**
 * What each venue will pay for a given input, and the best way to divide the input between them.
 *
 * The split is searched, not derived: a grid of splits is evaluated against the venues' own
 * answers rather than against curves reimplemented here, because a second implementation of a
 * curve can disagree with the first and the disagreements get found by users, not by tests.
 *
 * UniV2 answers `getAmountsOut` inside a Multicall3 batch; the prop pool answers one POST to
 * [`Config.propQuoteUrl`] carrying all eleven grid amounts. That is still the venue's own
 * arithmetic — the engine prices from `dubu-core`'s `curve.rs`, an exact integer port of
 * `PropCurve.sol` asserted against the Solidity in `contracts/test/PropCurve.t.sol`. Pricing the
 * pool on chain needed `blockTag: 'pending'` to see a quote published inside the current block,
 * and GIWA serves pending state and pending timestamp inconsistently: a `block.timestamp` ahead of
 * the state whose `updatedAt` it is compared against makes `PropPool` return `STATUS_STALE` and
 * pay zero on every pair. There is deliberately no on-chain fallback — an unreachable engine shows
 * as prop being unavailable, not as a second and differently-wrong price.
 *
 * The search needs no special case for epoch capacity, the staleness ramp or a paused pair: all of
 * those show up as the venue quoting less, or quoting zero. The grid is coarse because near the
 * optimum the curve is nearly flat, and refining gains less than the price moves during the extra
 * round trip.
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

/**
 * A refusal the prop venue named, as opposed to one inferred from it paying nothing. A union
 * rather than a boolean: the engine has a vocabulary of these (`no-market`, `not-ready`,
 * `size-out-of-range`) and the next one worth telling apart should cost a member, not a flag.
 */
export type PropRefusal = 'no-capacity';

export interface PropGrid {
  /** One amount per grid point, in the order asked. Zeros whenever the venue could not be used. */
  amountsOut: bigint[];
  /** Set only when the engine named the reason itself; never inferred from zeros. */
  refused: PropRefusal | null;
}

export interface AmmQuote {
  /** Best combination found. Legs with zero weight are dropped. */
  legs: Leg[];
  amountOut: bigint;
  /** Each venue alone at the full size, for the response to show its work. */
  solo: Record<VenueId, bigint>;
  split: boolean;
  /** [`PropGrid.refused`], carried through so the HTTP layer can tell a venue that is between
   *  prices from one that is absent. The split search never sets it: it sees amounts only. */
  propRefused: PropRefusal | null;
}

export function makeClient(cfg: Config): PublicClient {
  // `fallback` rather than one endpoint: when GIWA's public RPC rate-limits, the multicall fails,
  // `allowFailure: true` turns that into `success: false`, and the venue decodes as 0n — a silent
  // zero indistinguishable from a pool that is not quoting. `rank` is off because reordering
  // during a burst spreads the burst across every endpoint rather than draining one.
  const urls = [cfg.rpcUrl, ...cfg.fallbackRpcUrls].filter((u, i, a) => u && a.indexOf(u) === i);
  return createPublicClient({
    transport: fallback(
      urls.map((u) => http(u, { retryCount: 1, timeout: 4_000 })),
      { rank: false },
    ),
  });
}

/**
 * Every grid point, priced at both venues, in two round trips that leave together.
 *
 * `allowFailure` is set on every call in the batch and the prop leg degrades by hand to match: a
 * venue that reverts, times out or answers nonsense must cost its own leg and nothing else, so
 * [`quotePropGrid`] has no throwing path.
 */
export async function quoteAmms(
  client: PublicClient,
  cfg: Config,
  tokenIn: Address,
  tokenOut: Address,
  amountIn: bigint,
  fetchImpl: typeof fetch = fetch,
): Promise<AmmQuote> {
  const toProp = SPLIT_GRID.map((bps) => (amountIn * BigInt(bps)) / BigInt(WEIGHT_DENOMINATOR));
  const toUniv2 = toProp.map((a) => amountIn - a);

  const calls = toUniv2.map((amount) => ({
    target: cfg.univ2Router,
    allowFailure: true,
    callData: encodeFunctionData({
      abi: UNIV2_ROUTER_ABI,
      functionName: 'getAmountsOut',
      args: [amount, [tokenIn, tokenOut]],
    }),
  }));

  const [raw, prop] = await Promise.all([
    client.readContract({
      address: MULTICALL3,
      abi: MULTICALL3_ABI,
      functionName: 'aggregate3',
      args: [calls],
      // Sealed `latest`, viem's default, so stated by omission. For a constant-product pool
      // `pending` buys nothing — reserves move when a trade seals, not when the maker re-quotes —
      // and on GIWA it costs correctness (see the header).
    }),
    quotePropGrid(cfg, tokenIn, tokenOut, toProp, fetchImpl),
  ]);
  const results = raw as readonly { success: boolean; returnData: `0x${string}` }[];

  // The same search the tests run against hand-written arrays: no second copy to keep in step.
  const best = chooseSplit(
    amountIn,
    prop.amountsOut,
    results.map((r) => decodeUniv2(r)),
  );
  return { ...best, propRefused: prop.refused };
}

/**
 * How long the prop venue gets to answer: several times the engine's own ~415ms quote cycle, and
 * short enough that a hung maker costs a slower quote rather than a failed one. It runs
 * concurrently with the multicall, so it bounds the quote's latency rather than adding to it.
 */
const PROP_QUOTE_TIMEOUT_MS = 2_000;

/**
 * An `observedAgeMs` past which the engine's view of the chain is worth a log line. Matches
 * `PropPool`'s own `maxStaleSecs` so the number means the same thing in both places. A threshold
 * for noticing, not for refusing — see [`quotePropGrid`].
 */
const PROP_OBSERVED_AGE_SUSPECT_MS = 5_000;

/**
 * The prop venue's price for every grid amount, in one request. Never throws and never rejects: an
 * unreachable engine, a non-2xx, a body of the wrong shape or length, an entry that is not a whole
 * non-negative integer — all of it degrades to zeros, which the grid reads as the venue refusing
 * every size and routes around, the same bargain `allowFailure` makes for the on-chain legs. A zero
 * *from* the engine is that same refusal at one size (capacity spent, out of domain, under the
 * minimum price), not a failure.
 *
 * Exactly one degradation is labelled: a 503 carrying `no-capacity` is the engine withdrawing the
 * side while it re-prices, over in tens of seconds, where everything else here is indefinite. The
 * label changes no routing, so an engine that does not send the code behaves as it did before.
 *
 * `fetchImpl` is injectable so hostile and malformed responses are testable without a network.
 */
export async function quotePropGrid(
  cfg: Config,
  tokenIn: Address,
  tokenOut: Address,
  amountsIn: readonly bigint[],
  fetchImpl: typeof fetch = fetch,
): Promise<PropGrid> {
  const zeros = amountsIn.map(() => 0n);
  if (cfg.propQuoteUrl === null) return { amountsOut: zeros, refused: null };

  let body: unknown;
  try {
    const res = await fetchImpl(cfg.propQuoteUrl, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({
        tokenIn,
        tokenOut,
        amountsIn: amountsIn.map((a) => a.toString()),
      }),
      signal: AbortSignal.timeout(PROP_QUOTE_TIMEOUT_MS),
    });

    if (!res.ok) return await quotePropGridRejected(res, zeros);
    body = await res.json();
  } catch (e) {
    // DNS, TLS, a dropped connection or the abort. Never a refusal: not being heard from is a
    // different fact from having no market.
    warn('engine unreachable; no prop leg', String(e));
    return { amountsOut: zeros, refused: null };
  }

  const amountsOut = parseAmountsOut(body, amountsIn.length);
  if (!amountsOut) {
    warn('malformed response; no prop leg', `expected ${amountsIn.length} integer amountsOut`);
    return { amountsOut: zeros, refused: null };
  }

  // `observedAgeMs` is an ELAPSED age off the engine's own monotonic clock and must never be
  // compared against `Date.now()`: an absolute timestamp crossing hosts carries the offset between
  // the two clocks, which once made every RFQ order read as expired. (`quoteAgeSecs` is the
  // chain-side age of the ladder, which the pool's own window is measured against; this is how
  // long ago the engine looked.) Logged, never enforced — the engine owns its staleness rules and
  // already answers 0 for the sizes they disqualify, and a second opinion computed here is the
  // arithmetic that returned zero on every pair.
  const observedAgeMs = readObservedAgeMs(body);
  if (observedAgeMs !== null && observedAgeMs > PROP_OBSERVED_AGE_SUSPECT_MS) {
    warn(
      `engine observation ${observedAgeMs}ms old`,
      'quoted anyway — the engine decides its own staleness',
    );
  }

  // A 200 makes no claim about capacity: zeros here are the venue refusing every size.
  return { amountsOut, refused: null };
}

/** The non-2xx arm of [`quotePropGrid`]: always zeros, labelled only when the engine named it. */
async function quotePropGridRejected(res: Response, zeros: bigint[]): Promise<PropGrid> {
  const declined = await res
    .json()
    .then((b) => (b as { error?: string } | null)?.error)
    .catch(() => undefined);
  // A non-2xx with no error body is almost always a wrong path: `PROP_QUOTE_URL` is a full endpoint
  // and nothing appends one. Named declines (`no-market`, `not-ready`) are normal, so the two are
  // logged apart — a 404 reading as a refusal sends the search to the pricing.
  warn(
    declined
      ? `engine declined (http ${res.status}): ${declined}; no prop leg`
      : `http ${res.status} with no error body; no prop leg`,
    declined ? undefined : 'PROP_QUOTE_URL must be the full endpoint, path included',
  );
  // The status is matched as well as the code: `no-capacity` on anything but a 503 is not the
  // agreed response, and promising a retry that never comes good is worse than saying nothing.
  const refused: PropRefusal | null =
    res.status === 503 && declined === 'no-capacity' ? 'no-capacity' : null;
  return { amountsOut: zeros, refused };
}

/** A missing or unreadable age costs a log line, not the quote: this is telemetry, not contract. */
function readObservedAgeMs(body: unknown): number | null {
  const v = (body as { observedAgeMs?: unknown }).observedAgeMs;
  return typeof v === 'number' && Number.isFinite(v) && v >= 0 ? v : null;
}

/** Each outcome names itself — declined, not reached, and answered something else point at three
 *  different things to fix. The URL is never logged: it is a secret in every deployment. */
function warn(reason: string, detail?: string): void {
  console.warn(`prop quote: ${reason}${detail ? ` — ${detail}` : ''}`);
}

/**
 * `amountsOut`, or `null` if the body is not the shape that was agreed. All-or-nothing: one entry
 * this code cannot read leaves the rest unverified rather than merely partial, and salvaging the
 * readable ones would route real input on a body already known to be wrong.
 */
function parseAmountsOut(body: unknown, expected: number): bigint[] | null {
  if (typeof body !== 'object' || body === null) return null;
  const raw = (body as { amountsOut?: unknown }).amountsOut;
  // Same length and same order as `amountsIn`: the response carries no amounts of its own, so a
  // short or long array cannot be realigned, only rejected.
  if (!Array.isArray(raw) || raw.length !== expected) return null;

  const out: bigint[] = [];
  for (const v of raw) {
    if (typeof v !== 'string' && typeof v !== 'number') return null;
    try {
      const n = BigInt(v);
      // A negative is not a worse quote, it is a body this code does not understand.
      if (n < 0n) return null;
      out.push(n);
    } catch {
      return null;
    }
  }
  return out;
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
 * Picks the best grid point from pre-computed venue outputs, split out so the search is testable
 * without a chain. `propOut[i]` and `univOut[i]` are what each venue pays for grid point `i`; a
 * venue asked for a non-zero amount that answers zero disqualifies that point. `propRefused` is the
 * caller's to attach — the search is told what the venues paid and nothing about why.
 */
export function chooseSplit(
  amountIn: bigint,
  propOut: bigint[],
  univOut: bigint[],
): Omit<AmmQuote, 'propRefused'> {
  const solo: Record<VenueId, bigint> = {
    prop: propOut[SPLIT_GRID.length - 1] ?? 0n,
    univ2: univOut[0] ?? 0n,
  };
  let best: Omit<AmmQuote, 'propRefused'> | null = null;

  SPLIT_GRID.forEach((bps, i) => {
    const toProp = (amountIn * BigInt(bps)) / BigInt(WEIGHT_DENOMINATOR);
    const toUniv2 = amountIn - toProp;
    const p = toProp === 0n ? 0n : (propOut[i] ?? 0n);
    const u = toUniv2 === 0n ? 0n : (univOut[i] ?? 0n);
    if (toProp > 0n && p === 0n) return;
    if (toUniv2 > 0n && u === 0n) return;

    const total = p + u;
    if (best && total <= best.amountOut) return;

    const legs: Leg[] = [];
    if (toProp > 0n) legs.push({ venue: 'prop', weightBps: bps, amountIn: toProp, amountOut: p });
    if (toUniv2 > 0n) {
      legs.push({
        venue: 'univ2',
        weightBps: WEIGHT_DENOMINATOR - bps,
        amountIn: toUniv2,
        amountOut: u,
      });
    }
    best = { legs, amountOut: total, solo, split: legs.length > 1 };
  });

  return best ?? { legs: [], amountOut: 0n, solo, split: false };
}

/**
 * The most the RFQ maker could pay out in `token`: the lesser of what it holds and what it has
 * allowed `PmmSettle` to pull. The lesser, because `PmmSettle` custodies nothing — it issues a
 * `transferFrom` against the maker's own balance, so either being short is the same failure.
 *
 * `undefined` when either read fails, and the caller must treat that as *unverified*, not as
 * passing.
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
      callData: encodeFunctionData({
        abi: ERC20_ABI,
        functionName: 'allowance',
        args: [maker, settler],
      }),
    },
  ];

  try {
    const results = (await client.readContract({
      address: MULTICALL3,
      abi: MULTICALL3_ABI,
      functionName: 'aggregate3',
      args: [calls],
      // A sealed read cannot see a transfer or an approval the maker made in the last second, so a
      // maker that CAN deliver gets refused as if it could not.
      //
      // TODO: this is the same inconsistent pending read that forced the prop quote off chain. A
      // balance does not decay against a clock, so the failure here would be a stale number rather
      // than a systematic zero, but it is the same node behaviour and should be revisited.
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
