import { getAddress, isAddress, type Address } from 'viem';

import { findMarket, loadConfig, type Env } from './config.js';
import { makeClient, makerCanDeliver, quoteAmms, WEIGHT_DENOMINATOR, type Leg } from './quote.js';
import { requestQuote, type RfqQuote } from './rfq.js';
import { buildRoute } from './route.js';

/**
 * DuBu's DEX aggregator: one `POST /quote` that prices every venue and hands back signable
 * calldata.
 *
 * # Why an edge worker
 *
 * The router is a pure executor — it never compares venues, never re-splits, never finds a path
 * (`Router.sol`). Something has to do that off chain, and on a chain with a fee-ordered sequencer
 * and no public mempool, the thing that matters is how quickly a taker gets from "I want to trade"
 * to "here is a transaction". Running in every region turns the round trip from a transcontinental
 * one into a local one, and the work itself is one multicall plus arithmetic.
 *
 * It also has to be true that a compromised region cannot steal anything, and that is a design
 * constraint rather than a hope: this service holds no key, takes no custody, and returns calldata
 * the caller decides whether to sign. The worst a hostile replica can do is quote badly, and
 * `minAmountOut` is in the calldata the caller signs.
 *
 * # What it does not do
 *
 * No multi-hop. Every market is a direct pair against mUSDC, so a hop count the venue set cannot
 * produce would be untested code. No gas-adjusted ranking either — at 0.001 gwei the difference
 * between a one-leg and a two-leg route is worth less than a rounding error on the quote, and
 * pretending to price it would be theatre. Both become real work the moment a third venue or a
 * non-mUSDC market lands.
 */

interface QuoteRequestBody {
  tokenIn?: string;
  tokenOut?: string;
  amountIn?: string;
  receiver?: string;
  slippageBps?: number;
  deadlineSecs?: number;
}

const DEFAULT_SLIPPAGE_BPS = 50;
const DEFAULT_DEADLINE_SECS = 120;
const MAX_SLIPPAGE_BPS = 1_000;

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);

    // The CORS preflight. A browser sending `POST` with `content-type: application/json` asks
    // first, and until this existed the ask got a 404 — so every request from a page failed with
    // an opaque "Failed to fetch" while curl worked perfectly. Returning the allow-origin header
    // on the *response* is not enough on its own; the preflight has to be answered too.
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

  const slippageBps = body.slippageBps ?? DEFAULT_SLIPPAGE_BPS;
  if (!Number.isInteger(slippageBps) || slippageBps < 0 || slippageBps > MAX_SLIPPAGE_BPS) {
    return json({ error: `slippageBps must be an integer in 0..${MAX_SLIPPAGE_BPS}` }, 400);
  }

  const found = findMarket(cfg.markets, tokenIn, tokenOut);
  if (!found) return json({ error: 'no market for that token pair', markets: cfg.markets.map((m) => m.symbol) }, 400);
  const { market, sellingBase } = found;

  const client = makeClient(cfg);
  let amm;
  try {
    amm = await quoteAmms(client, cfg, tokenIn, tokenOut, amountIn);
  } catch (e) {
    return json({ error: 'could not read the chain', detail: String(e) }, 502);
  }

  const nowSecs = Math.floor(Date.now() / 1000);

  // What the maker could actually pay out, read before its quote is trusted. Only fetched when
  // the RFQ leg is on, so an AMM-only deployment pays nothing for it.
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

  // Whole-size comparison only. An RFQ order is signed for one `takerAmount`, so a partial fill
  // would be a different order — splitting between RFQ and the curve means asking the maker for a
  // quote at the split size, which is a second round trip to the maker for a gain the on-chain
  // grid has already mostly captured. Recorded here as a limit rather than left to be inferred.
  const useRfq = rfq.quote !== null && rfq.quote.amountOut > amm.amountOut;
  const legs: Leg[] = useRfq ? [] : amm.legs;
  const chosenOut = useRfq ? (rfq.quote as RfqQuote).amountOut : amm.amountOut;

  if (chosenOut === 0n) {
    return json(
      {
        error: 'no venue would fill that size',
        detail:
          'Every venue returned zero. For the prop AMM that means the epoch capacity is spent, ' +
          'the quote is stale, or the pair is paused — all of which resolve on their own — or ' +
          'that the engine pricing it could not be reached, which does not.',
        solo: { prop: amm.solo.prop.toString(), univ2: amm.solo.univ2.toString() },
      },
      404,
    );
  }

  const minAmountOut = (chosenOut * BigInt(WEIGHT_DENOMINATOR - slippageBps)) / BigInt(WEIGHT_DENOMINATOR);
  const deadline = BigInt(nowSecs + (body.deadlineSecs ?? DEFAULT_DEADLINE_SECS));

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
    // The work, shown. A caller comparing us against going direct to one venue needs to see what
    // each venue offered, and a caller wondering why RFQ is absent needs to see why it was refused
    // rather than being told nothing.
    detail: {
      prop: amm.solo.prop.toString(),
      univ2: amm.solo.univ2.toString(),
      rfq: rfq.quote ? rfq.quote.amountOut.toString() : null,
      rfqRejected: rfq.rejected,
      rfqMakerReason: rfq.makerReason ?? null,
      // Stated rather than implied: `null` means the solvency read failed and the quote was taken
      // unverified on that axis, which is a different claim from "verified and fine".
      rfqMakerCanDeliver: canDeliver === undefined ? null : canDeliver.toString(),
      split: !useRfq && amm.split,
      legs: legs.map((l) => ({
        venue: l.venue,
        weightBps: l.weightBps,
        amountIn: l.amountIn.toString(),
        amountOut: l.amountOut.toString(),
      })),
    },
    approve: {
      // The taker must approve the Router for the AMM path, and PmmSettle for the RFQ path,
      // because PmmSettle pulls the taker leg from `msg.sender` and that is the PmmAdapter which
      // the Router funds. Stated per-route rather than as a blanket instruction: approving the
      // wrong one is a revert at fill time with nothing useful in the trace.
      token: tokenIn,
      spender: cfg.router,
      amountIn: amountIn.toString(),
    },
  });
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
      // A quote is worth about as long as a block. Caching it at the edge for longer would serve
      // a price the chain has already moved past.
      'cache-control': 'no-store',
    },
  });
}
