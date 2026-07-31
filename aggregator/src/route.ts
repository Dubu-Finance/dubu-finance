import { encodeAbiParameters, encodeFunctionData, getAddress, type Address, type Hex } from 'viem';

import { PMM_PAYLOAD_ABI, PROP_PAYLOAD_ABI, ROUTER_ABI } from './abi.js';
import type { Config, Market } from './config.js';
import { WEIGHT_DENOMINATOR, type Leg } from './quote.js';
import type { RfqQuote } from './rfq.js';

/**
 * Turning a chosen split into calldata the caller can sign. The service stops here: it returns `to`
 * and `data`, and the caller decides whether to broadcast them. The step word:
 *
 * ```text
 *   bit  255      reverse       call sellQuote instead of sellBase
 *   bit  254      fundAdapter   push this leg's tokens to the adapter, not to the pool
 *   bits 253..176 reserved      MUST be zero — RouteDecoder reverts otherwise
 *   bits 175..160 weight        share of this hop's input, out of 10_000
 *   bits 159..0   pool          the venue contract
 * ```
 *
 * Two bits are easy to get wrong:
 *
 * 1. `fundAdapter` is for the RFQ leg only. `PmmSettle` pulls the taker leg from `msg.sender`,
 *    which is the adapter, so the tokens must be there; the AMM legs settle against their own
 *    balance delta and want them at the pool. Set on a prop or UniV2 leg it strands the input at
 *    the adapter; missing from the RFQ leg the adapter reads zero and reverts `NothingToFill`.
 *
 * 2. `reverse` means the ladder side, not the token order: `PropPoolAdapter` carries the pair as
 *    `(base, quote)` and uses the bit to pick which way to walk it, so a route selling quote for
 *    base sets it and a route selling base for quote does not.
 */

const BIT_REVERSE = 1n << 255n;
const BIT_FUND_ADAPTER = 1n << 254n;

export function packStep(
  pool: Address,
  weightBps: number,
  reverse: boolean,
  fundAdapter: boolean,
): bigint {
  if (weightBps < 0) throw new Error(`weightBps out of range: ${weightBps}`);
  if (weightBps > WEIGHT_DENOMINATOR) throw new Error(`weightBps out of range: ${weightBps}`);
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
 * One batch with parallel forks rather than several weighted batches: a fork's weight is a share of
 * *this hop's* input, and with a single hop the two encodings are equivalent, so the flatter wins.
 */
export function buildRoute(args: BuildArgs): BuiltRoute {
  const { cfg, market, sellingBase, legs, rfq } = args;
  const steps: SwapStep[] = [];
  const venues: string[] = [];

  const propPayload = encodeAbiParameters(PROP_PAYLOAD_ABI, [
    market.base,
    market.quote,
    // `limitAmount`, the per-leg guard, left at zero: the router already enforces `minAmountOut`
    // over the whole route, and a per-leg minimum from the same quote would reject split routes
    // for moving in a direction the route as a whole still satisfies.
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
    if (!cfg.pmmAdapter || !cfg.pmmSettle) {
      throw new Error('RFQ leg requested with the RFQ leg disabled');
    }
    steps.push({
      adapter: cfg.pmmAdapter,
      // The step's pool is PmmSettle, never the maker — the maker is inside the signed order and
      // is not the router's to choose. `fundAdapter` is set: see the note above.
      rawData: packStep(cfg.pmmSettle, rfq.weightBps, false, true),
      payload: encodeAbiParameters(PMM_PAYLOAD_ABI, [
        rfq.quote.order as unknown as never,
        rfq.quote.signature,
        // Decay tolerance of zero: fill at the order's stated terms or revert, rather than opting
        // the caller into a worse price than they were quoted.
        0,
      ]),
    });
    venues.push('rfq');
  }

  if (steps.length === 0) throw new Error('no legs to route');

  return { to: cfg.router, data: buildRouteCalldata(args, steps), venues };
}

function buildRouteCalldata(args: BuildArgs, steps: SwapStep[]): Hex {
  return encodeFunctionData({
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
}

/**
 * A table rather than a `getPair` round trip or the `CREATE2` derivation, which needs the factory's
 * init-code hash — one more constant to keep in step with a deployment. Both addresses are from
 * `contracts/DEPLOYMENTS.md`, so redeploying the UniV2 stack means editing this table, and
 * `test/route.test.ts` fails if it and the market list disagree about which pairs exist.
 */
const UNIV2_PAIRS: Record<number, Address> = {
  1: getAddress('0x94f0033BABBa0bEC1C17B808E0980ECFd3B35b4C'),
  2: getAddress('0xd6d0f9F9536590b2C0FDC76Fab5FE415F59C0bED'),
  3: getAddress('0x008997B212771a705d6520008d01c3EFedEd3ce4'),
  4: getAddress('0x946a3035cF34F36f5702B4a65F13eE2DD5dDae30'),
  5: getAddress('0x36Dde52071C5c2E2e29baa7f5666435D62143441'),
  6: getAddress('0x87b3ee110f32233Be1dE7504efb00D014087241D'),
  7: getAddress('0x4682bf45F062F2F68539b98e50FF882459803ba9'),
  8: getAddress('0xC7aD621c721b48dE595aC145EA0B46f2C840884B'),
  9: getAddress('0x027b1618c0C927684B4a4026368f0c6E1f77E314'),
};

export function univ2Pair(_cfg: Config, market: Market): Address {
  const pair = UNIV2_PAIRS[market.pairId];
  if (!pair) throw new Error(`no UniV2 pair recorded for pairId ${market.pairId}`);
  return pair;
}
