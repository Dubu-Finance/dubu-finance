import { getAddress, isAddress, type Address } from 'viem';

import { findMarket, loadConfig, type Env } from './config.js';
import {
  makeClient,
  makerCanDeliver,
  quoteAmms,
  WEIGHT_DENOMINATOR,
  type AmmQuote,
  type Leg,
  type PropRefusal,
  type VenueId,
} from './quote.js';
import { requestQuote, type RfqQuote, type RfqResult } from './rfq.js';
import { buildRoute } from './route.js';

/**
 * DuBu's DEX aggregator: one `POST /quote` that prices every venue and hands back signable
 * calldata.
 *
 * The router is a pure executor — it never compares venues, re-splits, or finds a path
 * (`Router.sol`) — so the search happens off chain, and on a chain with a fee-ordered sequencer
 * and no public mempool what matters is how fast a taker gets from "I want to trade" to "here is a
 * transaction", which is why this runs in every region. It holds no key and takes no custody: a
 * compromised replica can only quote badly, and `minAmountOut` is inside the signed calldata.
 *
 * No multi-hop and no gas-adjusted ranking. Every market is a direct pair against mUSDC, so a hop
 * count the venue set cannot produce would be untested code, and at 0.001 gwei the gas difference
 * between one leg and two is below a rounding error on the quote. Both become real work the moment
 * a third venue or a non-mUSDC market lands.
 */

interface QuoteRequestBody {
  tokenIn?: string;
  tokenOut?: string;
  amountIn?: string;
  receiver?: string;
  slippageBps?: number;
  deadlineSecs?: number;
}

const SLIPPAGE_BPS_DEFAULT = 50;
const DEADLINE_SECS_DEFAULT = 120;
const SLIPPAGE_BPS_MAX = 1_000;

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);

    // A browser sending `POST` with `content-type: application/json` preflights first, and the
    // allow-origin header on the *response* does not answer that: an unanswered preflight is an
    // opaque "Failed to fetch" in the page while curl works perfectly.
    if (request.method === "OPTIONS") {
      return new Response(null, {
        status: 204,
        headers: {
          "access-control-allow-origin": "*",
          "access-control-allow-methods": "GET, POST, OPTIONS",
          "access-control-allow-headers": "content-type",
          "access-control-max-age": "86400",
        },
      });
    }

    if (request.method === 'GET' && url.pathname === '/health') {
      return json({ ok: true, chainId: loadConfig(env).chainId });
    }
    if (request.method === 'GET' && url.pathname === '/markets') {
      const cfg = loadConfig(env);
      return json({
        chainId: cfg.chainId,
        rfq: cfg.rfqMakerUrl !== null,
        markets: cfg.markets.map((m) => ({
          pairId: m.pairId,
          symbol: m.symbol,
          base: m.base,
          quote: m.quote,
          baseDecimals: m.baseDecimals,
          quoteDecimals: m.quoteDecimals,
        })),
      });
    }
    if (request.method === 'POST' && url.pathname === '/quote') {
      return handleQuote(request, env);
    }
    return json({ error: 'not found' }, 404);
  },
};

async function handleQuote(request: Request, env: Env): Promise<Response> {
  let body: QuoteRequestBody;
  try {
    body = (await request.json()) as QuoteRequestBody;
  } catch {
    return json({ error: 'body must be JSON' }, 400);
  }

  const cfg = loadConfig(env);

  const tokenIn = parseAddress(body.tokenIn);
  const tokenOut = parseAddress(body.tokenOut);
  if (!tokenIn || !tokenOut) return json({ error: 'tokenIn and tokenOut must be addresses' }, 400);
  if (tokenIn === tokenOut) return json({ error: 'tokenIn and tokenOut are the same token' }, 400);

  const receiver = parseAddress(body.receiver);
  if (!receiver) return json({ error: 'receiver must be an address' }, 400);

  let amountIn: bigint;
  try {
    amountIn = BigInt(body.amountIn ?? '0');
  } catch {
    return json({ error: 'amountIn must be an integer string in the token’s own units' }, 400);
  }
  if (amountIn <= 0n) return json({ error: 'amountIn must be positive' }, 400);

  const slippageBps = body.slippageBps ?? SLIPPAGE_BPS_DEFAULT;
  if (!Number.isInteger(slippageBps) || slippageBps < 0 || slippageBps > SLIPPAGE_BPS_MAX) {
    return json({ error: `slippageBps must be an integer in 0..${SLIPPAGE_BPS_MAX}` }, 400);
  }

  const found = findMarket(cfg.markets, tokenIn, tokenOut);
  if (!found) {
    return json(
      { error: 'no market for that token pair', markets: cfg.markets.map((m) => m.symbol) },
      400,
    );
  }
  const { market, sellingBase } = found;

  const client = makeClient(cfg);
  let amm;
  try {
    amm = await quoteAmms(client, cfg, tokenIn, tokenOut, amountIn);
  } catch (e) {
    return json({ error: 'could not read the chain', detail: String(e) }, 502);
  }

  const nowSecs = Math.floor(Date.now() / 1000);

  // What the maker could actually pay out, read before its quote is trusted. Fetched only when the
  // RFQ leg is on, so an AMM-only deployment pays nothing for it.
  const canDeliver =
    cfg.rfqMakerAddress && cfg.pmmSettle
      ? await makerCanDeliver(client, tokenOut, cfg.rfqMakerAddress, cfg.pmmSettle)
      : undefined;

  const rfq = await requestQuote(cfg, {
    tokenIn,
    tokenOut,
    amountIn,
    ammAmountOut: amm.amountOut,
    makerCanDeliver: canDeliver,
    nowSecs,
  });

  // Whole-size comparison only. An RFQ order is signed for one `takerAmount`, so splitting between
  // RFQ and the curve means a second round trip to the maker at the split size, for a gain the
  // on-chain grid has already mostly captured.
  const useRfq = rfq.quote !== null && rfq.quote.amountOut > amm.amountOut;
  const legs: Leg[] = useRfq ? [] : amm.legs;
  const chosenOut = useRfq ? (rfq.quote as RfqQuote).amountOut : amm.amountOut;

  if (chosenOut === 0n) return nothingRoutable(amm.propRefused, amm.solo);

  const minAmountOut =
    (chosenOut * BigInt(WEIGHT_DENOMINATOR - slippageBps)) / BigInt(WEIGHT_DENOMINATOR);
  const deadline = BigInt(nowSecs + (body.deadlineSecs ?? DEADLINE_SECS_DEFAULT));

  let route;
  try {
    route = buildRoute({
      cfg,
      market,
      sellingBase,
      tokenIn,
      tokenOut,
      receiver,
      amountIn,
      legs,
      rfq: useRfq ? { quote: rfq.quote as RfqQuote, weightBps: WEIGHT_DENOMINATOR } : null,
      quotedAmountOut: chosenOut,
      minAmountOut,
      deadline,
    });
  } catch (e) {
    return json({ error: 'could not build the route', detail: String(e) }, 500);
  }

  return json({
    market: market.symbol,
    tokenIn,
    tokenOut,
    amountIn: amountIn.toString(),
    amountOut: chosenOut.toString(),
    minAmountOut: minAmountOut.toString(),
    slippageBps,
    deadline: deadline.toString(),
    route: { to: route.to, data: route.data, value: '0x0', venues: route.venues },
    detail: handleQuoteDetail(amm, rfq, canDeliver, useRfq, legs),
    approve: {
      // The Router is the spender on every path, RFQ included: `PmmSettle` pulls the taker leg
      // from `msg.sender`, which is the `PmmAdapter` the Router funds. Approving the wrong
      // contract is a revert at fill time with nothing useful in the trace.
      token: tokenIn,
      spender: cfg.router,
      amountIn: amountIn.toString(),
    },
  });
}

/** What each venue offered, so a caller comparing us against going direct can see the work and a
 *  caller wondering why RFQ is absent can see why it was refused. */
function handleQuoteDetail(
  amm: AmmQuote,
  rfq: RfqResult,
  canDeliver: bigint | undefined,
  useRfq: boolean,
  legs: Leg[],
) {
  return {
    prop: amm.solo.prop.toString(),
    univ2: amm.solo.univ2.toString(),
    rfq: rfq.quote ? rfq.quote.amountOut.toString() : null,
    rfqRejected: rfq.rejected,
    rfqMakerReason: rfq.makerReason ?? null,
    // `null` means the solvency read failed and the quote was taken unverified on that axis, which
    // is a different claim from "verified and fine".
    rfqMakerCanDeliver: canDeliver === undefined ? null : canDeliver.toString(),
    split: !useRfq && amm.split,
    legs: legs.map((l) => ({
      venue: l.venue,
      weightBps: l.weightBps,
      amountIn: l.amountIn.toString(),
      amountOut: l.amountOut.toString(),
    })),
  };
}

/**
 * Two facts rather than one. A prop side withdrawn for a price-jump cool-off comes back on its own
 * in tens of seconds, so it answers 503: the request is worth repeating. Everything else answers
 * 404, because it is not, at least not on a schedule this service can name — and collapsing the
 * two shows a frontend the same "quote not available" it shows for a pair that does not exist, so
 * the taker stops asking. Only fires when *nothing* could be routed: a withdrawn prop pool on a
 * pair UniV2 can serve never reaches here, because the all-UniV2 grid point wins. Exported so the
 * choice is testable without a chain.
 */
export function nothingRoutable(
  propRefused: PropRefusal | null,
  solo: Record<VenueId, bigint>,
): Response {
  const seen = { prop: solo.prop.toString(), univ2: solo.univ2.toString() };

  if (propRefused === 'no-capacity') {
    return json(
      {
        error: 'the pair is temporarily re-pricing',
        detail:
          'The prop pool has withdrawn its quotes for this side after a price move, and no other ' +
          'venue would fill that size. The withdrawal is a cool-off measured in tens of seconds, ' +
          'so the same request is worth retrying.',
        retryable: true,
        solo: seen,
      },
      503,
    );
  }

  return json(
    {
      error: 'no venue would fill that size',
      detail:
        'Every venue returned zero. On the prop side that is spent epoch capacity, which refills ' +
        'on the next epoch, or a stale quote, a paused pair, or an engine that could not be ' +
        'reached. A pause can be a latched killswitch, so none of those last three is known to ' +
        'clear without an operator. A side withdrawn to re-price answers 503 rather than this.',
      solo: seen,
    },
    404,
  );
}

function parseAddress(v: string | undefined): Address | null {
  if (typeof v !== 'string' || !isAddress(v)) return null;
  return getAddress(v);
}

function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body, null, 2), {
    status,
    headers: {
      'content-type': 'application/json; charset=utf-8',
      // Read-only, no credentials, no cookies: the dashboard and any other origin may call it.
      'access-control-allow-origin': '*',
      // A quote is worth about as long as a block; caching it at the edge for longer would serve a
      // price the chain has already moved past.
      'cache-control': 'no-store',
    },
  });
}
