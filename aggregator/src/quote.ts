import { createPublicClient, encodeFunctionData, decodeFunctionResult, fallback, http, type Address, type PublicClient } from 'viem';

import { ERC20_ABI, MULTICALL3_ABI, UNIV2_ROUTER_ABI } from './abi.js';
import { MULTICALL3, type Config } from './config.js';

/**
 * What each venue will pay for a given input, and the best way to divide the input between them.
 *
 * # The split is searched, not derived
 *
 * The obvious implementation reimplements each venue's curve here and solves for the optimum. This
 * one asks the venues instead: it evaluates a grid of splits and takes the best. That is slower by
 * one round trip and better for a reason that has already bitten this codebase once — a curve
 * reimplemented for the aggregator is a second implementation that can disagree with the first, and
 * the disagreements are found by users rather than by tests.
 *
 * # The two venues are asked over different transports
 *
 * UniV2 is asked through `getAmountsOut` in a Multicall3 batch. The prop pool is asked over HTTP,
 * of the engine that quotes it: one POST to [`Config.propQuoteUrl`] carrying all eleven grid
 * amounts.
 *
 * That is not a weakening of the rule above. The engine prices from `dubu-core`'s `curve.rs`, an
 * exact integer port of `PropCurve.sol` whose vectors are asserted against the Solidity in
 * `contracts/test/PropCurve.t.sol`. The answer still comes from the venue's own arithmetic, and
 * there is still no curve authored here — what changed is the transport the question travels over,
 * not who answers it.
 *
 * The move off `eth_call` was forced. Pricing the pool needed `blockTag: 'pending'` to see a quote
 * the maker had published inside the current block, and GIWA's node serves pending STATE and
 * pending TIMESTAMP inconsistently: a `block.timestamp` running 6s ahead of a state whose
 * `updatedAt` had advanced 2s made `PropPool` compute a 5s quote age against a 5s `maxStaleSecs`,
 * return `STATUS_STALE`, and pay 0 on every pair. The engine knows how old its own quote is without
 * asking a node what time it is. Quoting a maker over HTTP is also what 0x, 1inch and Hashflow all
 * do. There is deliberately no on-chain fallback: an unreachable engine shows as prop being
 * unavailable, not as a second and differently-wrong price.
 *
 * Either way the search needs no special case for the epoch capacity, the staleness ramp, a paused
 * pair, or anything else the pool might do next. Those all show up as the venue quoting less, or
 * quoting zero, which the grid handles without knowing why.
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
 * Every grid point, priced at both venues. Two round trips that leave together, not one after the
 * other — the multicall and the prop request are the same wall-clock second.
 *
 * `allowFailure` is set on every call in the batch, and the prop leg degrades by hand to match. A
 * venue that reverts, times out, or answers nonsense must cost its own leg and nothing else:
 * voiding the whole quote would turn one venue's outage into no quote at all, which is the opposite
 * of what an aggregator is for. [`quotePropGrid`] therefore has no throwing path.
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

  const [raw, propOut] = await Promise.all([
    client.readContract({
      address: MULTICALL3,
      abi: MULTICALL3_ABI,
      functionName: 'aggregate3',
      args: [calls],
      // Sealed `latest`, which is viem's default and so is stated by omission. `pending` used to be
      // set here, for the prop pool alone: preconfirmed state was the only way to see a quote the
      // maker had published within the current block. The prop pool is no longer read this way (see
      // the note on transports above), and for a constant-product pool `pending` bought nothing --
      // UniV2 reserves move when a trade seals, not when the maker re-quotes, and a sealed read is
      // the consistent one. Reading pending state also cost correctness on GIWA rather than just
      // buying freshness, which is the whole reason this multicall is now UniV2-only.
    }),
    quotePropGrid(cfg, tokenIn, tokenOut, toProp, fetchImpl),
  ]);
  const results = raw as readonly { success: boolean; returnData: `0x${string}` }[];

  // The same search `chooseSplit` runs against hand-written arrays in the tests, run against these
  // ones. Both venues now hand back a plain array per grid point, so there is no second copy of the
  // search to keep in step with the tested one.
  return chooseSplit(
    amountIn,
    propOut,
    results.map((r) => decodeUniv2(r)),
  );
}

/**
 * How long the prop venue gets to answer.
 *
 * Two seconds: several times the engine's own ~415ms quote cycle, and short enough that a hung
 * maker costs a slower quote rather than a failed one. It runs concurrently with the multicall, so
 * it bounds the quote's latency rather than adding to it.
 */
const PROP_QUOTE_TIMEOUT_MS = 2_000;

/**
 * An `observedAgeMs` past which the engine's view of the chain is worth a log line.
 *
 * Five seconds, matching `PropPool`'s own `maxStaleSecs` so the number means the same thing in both
 * places. A threshold for noticing, not for refusing — see [`quotePropGrid`].
 */
const PROP_OBSERVED_AGE_SUSPECT_MS = 5_000;

/**
 * The prop venue's price for every grid amount, in one request.
 *
 * Never throws and never rejects. An unreachable engine, a non-2xx, a body of the wrong shape, an
 * `amountsOut` of the wrong length, an entry that is not a whole non-negative integer — all of it
 * degrades to zeros, which the grid reads as the venue refusing every size and routes around. The
 * alternative is one HTTP failure taking down a quote UniV2 could have served, which is the same
 * bargain `allowFailure` makes for the on-chain legs.
 *
 * A zero *from* the engine is not a failure but that same refusal at one size: capacity spent, out
 * of domain, under the minimum price. It is the venue's answer, and it is the answer the old
 * `getAmountOut` gave for those cases too.
 *
 * Split out from [`quoteAmms`] and given an injectable `fetchImpl` for the same reason
 * `validateQuote` is split out of `requestQuote`: the interesting cases are hostile and malformed
 * responses, and they should be testable without a chain or a network.
 */
export async function quotePropGrid(
  cfg: Config,
  tokenIn: Address,
  tokenOut: Address,
  amountsIn: readonly bigint[],
  fetchImpl: typeof fetch = fetch,
): Promise<bigint[]> {
  const zeros = amountsIn.map(() => 0n);
  if (cfg.propQuoteUrl === null) return zeros;

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
      // A maker that stops answering must not become a quote that stops answering.
      signal: AbortSignal.timeout(PROP_QUOTE_TIMEOUT_MS),
    });

    if (!res.ok) {
      const declined = await res
        .json()
        .then((b) => (b as { error?: string } | null)?.error)
        .catch(() => undefined);
      // `no-market` and `not-ready` are the engine speaking, and they are normal. A non-2xx with no
      // error body is almost always this side pointing at the wrong path, because PROP_QUOTE_URL is
      // a full endpoint and nothing appends one. RFQ_MAKER_URL taught that lesson already: a 404
      // from a bare host read as the maker declining, and the search went to the pricing instead of
      // to the URL. The two are logged apart so that cannot happen twice.
      warn(
        declined
          ? `engine declined (http ${res.status}): ${declined}; no prop leg`
          : `http ${res.status} with no error body; no prop leg`,
        declined ? undefined : 'PROP_QUOTE_URL must be the full endpoint, path included',
      );
      return zeros;
    }
    body = await res.json();
  } catch (e) {
    // DNS, TLS, a dropped connection, or the 2s abort. Never a refusal: the engine was not heard
    // from at all, which is a different fact from it having no market.
    warn('engine unreachable; no prop leg', String(e));
    return zeros;
  }

  const amountsOut = parseAmountsOut(body, amountsIn.length);
  if (!amountsOut) {
    warn('malformed response; no prop leg', `expected ${amountsIn.length} integer amountsOut`);
    return zeros;
  }

  // `observedAgeMs` is an ELAPSED age off the engine's own monotonic clock, and it is never
  // compared against `Date.now()`. An absolute timestamp crossing hosts is what silently killed the
  // RFQ leg once: the maker stamped expiry from a clock running 2s slow, this side refused anything
  // with under a second left, and every order read as expired while nothing looked broken. An
  // elapsed age cannot carry that offset because it never leaves the clock that measured it.
  //
  // Logged, never enforced. The engine owns its staleness rules and already answers 0 for the sizes
  // they disqualify; a second staleness opinion computed here is precisely the arithmetic that
  // returned zero on every pair and forced this rewrite. (`quoteAgeSecs` is the chain-side age of
  // the ladder, which is what the pool's own window is measured against; this one is how long ago
  // the engine looked.)
  const observedAgeMs = readObservedAgeMs(body);
  if (observedAgeMs !== null && observedAgeMs > PROP_OBSERVED_AGE_SUSPECT_MS) {
    warn(`engine observation ${observedAgeMs}ms old`, 'quoted anyway — the engine decides its own staleness');
  }

  return amountsOut;
}

/** The engine's elapsed observation age, when it sent a usable one. Never fatal: a missing or
 *  unreadable age costs a log line, not the quote — the amounts are the contract, this is telemetry. */
function readObservedAgeMs(body: unknown): number | null {
  const v = (body as { observedAgeMs?: unknown }).observedAgeMs;
  return typeof v === 'number' && Number.isFinite(v) && v >= 0 ? v : null;
}

/** One line per prop-quote event worth an operator's attention, each naming its own outcome so the
 *  facts do not collapse together — "the engine declined", "the engine was not reached" and "the
 *  engine answered something else" point at three different things to go and fix, and an earlier
 *  version of this problem on the RFQ leg reported all of them as the maker declining.
 *
 *  The URL is never logged: it is a secret in every deployment that has one. */
function warn(reason: string, detail?: string): void {
  console.warn(`prop quote: ${reason}${detail ? ` — ${detail}` : ''}`);
}

/**
 * `amountsOut`, or `null` if the body is not the shape that was agreed.
 *
 * All-or-nothing on purpose. One entry this code cannot read means the response is not the contract
 * it claims to be, and the rest of it is unverified rather than merely partial — salvaging the
 * readable entries would route real input on the strength of a body already known to be wrong.
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
      // A sealed read cannot see a transfer or an approval the maker made in the last second, so a
      // maker that CAN deliver gets refused as if it could not.
      //
      // TODO: this is the same inconsistent pending read that forced the prop quote off chain. A
      // balance does not decay against a clock, so the failure here would be a stale or mismatched
      // number rather than a systematic zero — but it is the same node behaviour and should be
      // revisited rather than left because it has not bitten yet.
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
