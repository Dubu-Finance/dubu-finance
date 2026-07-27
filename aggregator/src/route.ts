import { encodeAbiParameters, encodeFunctionData, getAddress, type Address, type Hex } from 'viem';

import { PMM_PAYLOAD_ABI, PROP_PAYLOAD_ABI, ROUTER_ABI } from './abi.js';
import type { Config, Market } from './config.js';
import { WEIGHT_DENOMINATOR, type Leg } from './quote.js';
import type { RfqQuote } from './rfq.js';

/**
 * Turning a chosen split into calldata the caller can sign.
 *
 * The service stops here. It never holds a key, never sends a transaction, and never takes
 * custody: it returns `to` and `data`, and the caller decides whether to broadcast them. That is
 * not modesty about the deployment — it is what keeps a geo-distributed edge worker from being a
 * thing worth compromising. An aggregator that signed would have a key in every region it ran in.
 *
 * # The step word
 *
 * ```text
 *   bit  255      reverse       call sellQuote instead of sellBase
 *   bit  254      fundAdapter   push this leg's tokens to the adapter, not to the pool
 *   bits 253..176 reserved      MUST be zero — RouteDecoder reverts otherwise
 *   bits 175..160 weight        share of this hop's input, out of 10_000
 *   bits 159..0   pool          the venue contract
 * ```
 *
 * # Two things that are easy to get wrong here
 *
 * 1. **`fundAdapter` for the RFQ leg only.** `PmmSettle` pulls the taker leg from `msg.sender`,
 *    which is the adapter, so the tokens must be at the adapter. The AMM legs settle against their
 *    own balance delta and want the tokens at the pool. Setting the bit wrongly on a prop or UniV2
 *    leg strands the input at the adapter; leaving it off the RFQ leg makes that adapter read a
 *    zero balance and revert `NothingToFill`.
 *
 * 2. **`reverse` means the ladder side, not the token order.** `PropPoolAdapter` carries the pair
 *    as `(base, quote)` in its payload and uses the bit to pick which way to walk it. A route
 *    selling quote for base sets it; a route selling base for quote does not.
 */

/** `1 << 255`. */
const BIT_REVERSE = 1n << 255n;
/** `1 << 254`. */
const BIT_FUND_ADAPTER = 1n << 254n;

export function packStep(pool: Address, weightBps: number, reverse: boolean, fundAdapter: boolean): bigint {
  if (weightBps < 0 || weightBps > WEIGHT_DENOMINATOR) {
    throw new Error(`weightBps out of range: ${weightBps}`);
  }
  let word = BigInt(getAddress(pool));
  word |= BigInt(weightBps) << 160n;
  if (reverse) word |= BIT_REVERSE;
  if (fundAdapter) word |= BIT_FUND_ADAPTER;
  return word;
}

export interface SwapStep {
  adapter: Address;
  rawData: bigint;
  payload: Hex;
}

export interface BuiltRoute {
  to: Address;
  data: Hex;
  /** Legs in the order they appear in the plan, for the response to describe itself. */
  venues: string[];
}

export interface BuildArgs {
  cfg: Config;
  market: Market;
  /** True when the trade sells the market's base token. */
  sellingBase: boolean;
  tokenIn: Address;
  tokenOut: Address;
  receiver: Address;
  amountIn: bigint;
  legs: Leg[];
  rfq: { quote: RfqQuote; weightBps: number } | null;
  quotedAmountOut: bigint;
  minAmountOut: bigint;
  deadline: bigint;
}

/**
 * Builds one batch whose steps are the chosen legs, weighted.
 *
 * One batch with parallel forks rather than several weighted batches: a fork's weight is a share of
 * *this hop's* input, and with a single hop the two encodings are equivalent, so the flatter one
 * wins. Multi-hop routing is not implemented — every market here is a direct pair against mUSDC,
 * and a hop count the venue set cannot produce would be untested code.
 */
export function buildRoute(args: BuildArgs): BuiltRoute {
  const { cfg, market, sellingBase, legs, rfq } = args;
  const steps: SwapStep[] = [];
  const venues: string[] = [];

  const propPayload = encodeAbiParameters(PROP_PAYLOAD_ABI, [
    market.base,
    market.quote,
    // `limitAmount` is the per-leg guard. Left at zero: the router already enforces
    // `minAmountOut` over the whole route, and a per-leg minimum computed from the same quote
    // would reject split routes for moving between the quote and the fill in a direction the
    // route as a whole still satisfies.
    0n,
    cfg.partnerId,
    args.deadline,
  ]);

  for (const leg of legs) {
    if (leg.weightBps === 0) continue;
    if (leg.venue === 'prop') {
      steps.push({
        adapter: cfg.propAdapter,
        rawData: packStep(cfg.propPool, leg.weightBps, !sellingBase, false),
        payload: propPayload,
      });
      venues.push('prop');
    } else {
      steps.push({
        adapter: cfg.univ2Adapter,
        // UniV2's pair is the pool and it settles against its own balance delta, so no funding
        // bit. `reverse` is unused by that adapter; the token order is carried by the hop.
        rawData: packStep(univ2Pair(cfg, market), leg.weightBps, !sellingBase, false),
        payload: '0x',
      });
      venues.push('univ2');
    }
  }

  if (rfq && rfq.weightBps > 0) {
    if (!cfg.pmmAdapter || !cfg.pmmSettle) throw new Error('RFQ leg requested with the RFQ leg disabled');
    steps.push({
      adapter: cfg.pmmAdapter,
      // The step's pool is PmmSettle, never the maker — the maker is inside the signed order and
      // is not the router's to choose. `fundAdapter` is set: see the note above.
      rawData: packStep(cfg.pmmSettle, rfq.weightBps, false, true),
      payload: encodeAbiParameters(PMM_PAYLOAD_ABI, [
        rfq.quote.order as unknown as never,
        rfq.quote.signature,
        // Decay tolerance. Zero means "fill at the order's stated terms or revert" — this service
        // does not opt a caller into a worse price than it quoted them.
        0,
      ]),
    });
    venues.push('rfq');
  }

  if (steps.length === 0) throw new Error('no legs to route');

  const data = encodeFunctionData({
    abi: ROUTER_ABI,
    functionName: 'swapExactIn',
    args: [
      {
        tokenIn: args.tokenIn,
        tokenOut: args.tokenOut,
        receiver: args.receiver,
        amountIn: args.amountIn,
        quotedAmountOut: args.quotedAmountOut,
        deadline: args.deadline,
        batches: [
          {
            weightBps: WEIGHT_DENOMINATOR,
            hops: [{ tokenIn: args.tokenIn, steps }],
          },
        ],
      },
      args.minAmountOut,
    ],
  });

  return { to: cfg.router, data, venues };
}

/**
 * The UniV2 pair address for a market.
 *
 * Table rather than a `getPair` round trip, and rather than the `CREATE2` derivation — the
 * derivation needs the factory's init-code hash, which is one more constant to keep in step with a
 * deployment and buys nothing over writing the two answers down. Both are from
 * `contracts/DEPLOYMENTS.md`. Redeploying the UniV2 stack means editing this table, and
 * `test/route.test.ts` fails loudly if the market list and this table ever disagree about which
 * pairs exist.
 */
const UNIV2_PAIRS: Record<number, Address> = {
  1: getAddress('0x94f0033BABBa0bEC1C17B808E0980ECFd3B35b4C'),
  2: getAddress('0xd6d0f9F9536590b2C0FDC76Fab5FE415F59C0bED'),
};

export function univ2Pair(_cfg: Config, market: Market): Address {
  const pair = UNIV2_PAIRS[market.pairId];
  if (!pair) throw new Error(`no UniV2 pair recorded for pairId ${market.pairId}`);
  return pair;
}
