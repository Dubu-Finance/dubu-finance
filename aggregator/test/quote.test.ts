import { beforeEach, describe, expect, it, vi } from 'vitest';
import { getAddress, type Address } from 'viem';

import { loadConfig, type Env } from '../src/config.js';
import { quotePropGrid } from '../src/quote.js';

/**
 * The prop venue is now a network service, so it is the second input this worker does not compute
 * and cannot afford to trust. Every case here is a response the engine could plausibly return —
 * declining, restarting, misconfigured, or simply wrong — and the requirement is identical for all
 * of them: zeros, no throw, and a quote that UniV2 still serves.
 *
 * A test that asserted "it throws on a bad body" would be asserting the outage this degradation
 * exists to prevent.
 */

const MUSDC: Address = getAddress('0xd28596C6750D87C53EA146134AfAB53de86C5155');
const MWETH: Address = getAddress('0x81e46C6379498beBEB5DCcD47ab2DdFaf967d445');
const URL_ = 'https://engine.example/prop/amounts';

const AMOUNTS = [0n, 500n, 1_000n] as const;

const cfg = (overrides: Partial<Env> = {}) => loadConfig({ PROP_QUOTE_URL: URL_, ...overrides });

/** A `fetch` that answers with one canned response, and records what it was asked. */
function stub(body: unknown, init: ResponseInit = {}) {
  const calls: { url: string; init: RequestInit }[] = [];
  const impl = (async (url: unknown, requestInit: unknown) => {
    calls.push({ url: String(url), init: (requestInit ?? {}) as RequestInit });
    const payload = typeof body === 'string' ? body : JSON.stringify(body);
    return new Response(payload, { headers: { 'content-type': 'application/json' }, ...init });
  }) as unknown as typeof fetch;
  return { impl, calls };
}

/** A `fetch` that fails the way a dropped connection or the 2s abort does. */
function throwing(error: Error) {
  return (async () => {
    throw error;
  }) as unknown as typeof fetch;
}

/** The whole result: the amounts, and what the engine said about answering that way. */
const askFully = (fetchImpl: typeof fetch, config = cfg()) =>
  quotePropGrid(config, MWETH, MUSDC, AMOUNTS, fetchImpl);

/** Just the amounts, which is what all but one of these cases is about. */
const ask = async (fetchImpl: typeof fetch, config = cfg()) => (await askFully(fetchImpl, config)).amountsOut;

beforeEach(() => {
  // The degradation logs one line per failure; the tests assert behaviour, not the log.
  vi.spyOn(console, 'warn').mockImplementation(() => {});
});

describe('a well-formed engine response', () => {
  it('returns the amounts in the order they were asked for', async () => {
    const { impl } = stub({
      pairId: 1,
      side: 'bid',
      amountsOut: ['0', '998', '1996'],
      observedAgeMs: 412,
      quoteAgeSecs: 1,
    });
    expect(await ask(impl)).toEqual([0n, 998n, 1_996n]);
  });

  it('asks once, by POST, with every grid amount as a decimal string', async () => {
    const { impl, calls } = stub({ amountsOut: ['0', '1', '2'] });
    await ask(impl);

    expect(calls).toHaveLength(1);
    // Verbatim: nothing appends a path. See the note on PROP_QUOTE_URL in config.ts.
    expect(calls[0]!.url).toBe(URL_);
    expect(calls[0]!.init.method).toBe('POST');
    expect(JSON.parse(String(calls[0]!.init.body))).toEqual({
      tokenIn: MWETH,
      tokenOut: MUSDC,
      amountsIn: ['0', '500', '1000'],
    });
  });

  it('carries a zero through as the refusal it is, rather than as a failure', async () => {
    const { impl } = stub({ amountsOut: ['0', '998', '0'] });
    expect(await ask(impl)).toEqual([0n, 998n, 0n]);
  });
});

/**
 * `observedAgeMs` is telemetry, not a veto. Recomputing staleness on this side is the exact
 * arithmetic that returned zero on every pair and forced the venue off chain, so the tests pin the
 * amounts surviving a stale observation rather than the aggregator second-guessing the engine.
 */
describe('the engine’s observation age', () => {
  it('does not withhold amounts from an old observation, but does say so', async () => {
    const { impl } = stub({ amountsOut: ['0', '998', '1996'], observedAgeMs: 9_000 });
    expect(await ask(impl)).toEqual([0n, 998n, 1_996n]);
    expect(vi.mocked(console.warn).mock.calls.flat().join(' ')).toContain('9000ms old');
  });

  it('stays quiet for a fresh observation', async () => {
    const { impl } = stub({ amountsOut: ['0', '998', '1996'], observedAgeMs: 412 });
    expect(await ask(impl)).toEqual([0n, 998n, 1_996n]);
    expect(console.warn).not.toHaveBeenCalled();
  });

  it('quotes normally when the age is missing or unreadable', async () => {
    for (const observedAgeMs of [undefined, null, 'soon', -1, Number.NaN]) {
      const { impl } = stub({ amountsOut: ['0', '998', '1996'], observedAgeMs });
      expect(await ask(impl)).toEqual([0n, 998n, 1_996n]);
    }
  });
});

describe('an engine that cannot be used', () => {
  it('does not ask at all when the venue is unconfigured', async () => {
    const { impl, calls } = stub({ amountsOut: ['1', '2', '3'] });
    expect(await ask(impl, loadConfig({}))).toEqual([0n, 0n, 0n]);
    expect(calls).toHaveLength(0);
  });

  it('degrades on a dropped connection', async () => {
    expect(await ask(throwing(new TypeError('fetch failed')))).toEqual([0n, 0n, 0n]);
  });

  it('degrades when the request times out', async () => {
    const timeout = new DOMException('The operation was aborted due to timeout', 'TimeoutError');
    expect(await ask(throwing(timeout))).toEqual([0n, 0n, 0n]);
  });

  it('degrades on a declined pair', async () => {
    const { impl } = stub({ error: 'no-market', detail: 'pair withdrawn' }, { status: 404 });
    expect(await ask(impl)).toEqual([0n, 0n, 0n]);
  });

  it('degrades before the first quote cycle has published', async () => {
    const { impl } = stub({ error: 'not-ready', detail: 'no cycle yet' }, { status: 503 });
    expect(await ask(impl)).toEqual([0n, 0n, 0n]);
  });

  // The RFQ_MAKER_URL failure, in its new home: a base URL answers 404 from a router that has
  // never heard of the engine, and that must not read as the engine declining.
  it('degrades on a 404 with no error body, and says so differently', async () => {
    const { impl } = stub('<!doctype html><title>404</title>', { status: 404 });
    expect(await ask(impl)).toEqual([0n, 0n, 0n]);
    expect(vi.mocked(console.warn).mock.calls.flat().join(' ')).toContain('full endpoint');
  });
});

/**
 * A withdrawal is not an outage. The engine pulls a side for a price-jump cool-off and is quoting
 * again in tens of seconds, and until it was told apart the aggregator reported it with the same
 * 404 it uses for a pair that has no market at all.
 *
 * What is asserted here is the label on top of behaviour that is deliberately unchanged: the
 * amounts are still zeros, because a route UniV2 can serve must not be blocked by the prop pool
 * being between prices.
 */
describe('a prop side withdrawn while it re-prices', () => {
  it('names the withdrawal, and still quotes zeros', async () => {
    const { impl } = stub({ error: 'no-capacity', detail: 'bid capacity withdrawn' }, { status: 503 });
    expect(await askFully(impl)).toEqual({ amountsOut: [0n, 0n, 0n], refused: 'no-capacity' });
  });

  it('reads no other decline as a withdrawal', async () => {
    const others = [
      [503, 'not-ready'],
      [404, 'no-market'],
      [400, 'bad-amount'],
      [500, 'poisoned'],
      // The code without the status it was agreed on is not the response this side was promised.
      [200, 'no-capacity'],
      [404, 'no-capacity'],
    ] as const;
    for (const [status, error] of others) {
      const { impl } = stub({ error, detail: 'x' }, { status });
      expect((await askFully(impl)).refused).toBeNull();
    }
  });

  it('does not invent a withdrawal from a 503 with no error body', async () => {
    const { impl } = stub('<!doctype html><title>503</title>', { status: 503 });
    expect(await askFully(impl)).toEqual({ amountsOut: [0n, 0n, 0n], refused: null });
  });

  // The engine may ship this after the aggregator, or be rolled back afterwards. Zeros on a 200
  // are what it sends today, and they must keep meaning exactly what they mean today.
  it('makes no claim about capacity when the engine answers 200 with zeros', async () => {
    const { impl } = stub({ amountsOut: ['0', '0', '0'], observedAgeMs: 412 });
    expect(await askFully(impl)).toEqual({ amountsOut: [0n, 0n, 0n], refused: null });
  });

  it('makes no claim about capacity when the engine is unreachable', async () => {
    expect(await askFully(throwing(new TypeError('fetch failed')))).toEqual({
      amountsOut: [0n, 0n, 0n],
      refused: null,
    });
  });
});

describe('an engine that answers the wrong thing', () => {
  it('rejects a body that is not an object', async () => {
    const { impl } = stub('null');
    expect(await ask(impl)).toEqual([0n, 0n, 0n]);
  });

  it('rejects a body that is not JSON at all', async () => {
    const { impl } = stub('not json');
    expect(await ask(impl)).toEqual([0n, 0n, 0n]);
  });

  it('rejects a missing amountsOut', async () => {
    const { impl } = stub({ pairId: 1, side: 'bid' });
    expect(await ask(impl)).toEqual([0n, 0n, 0n]);
  });

  // A short array cannot be realigned against the grid, only rejected: the response carries no
  // amounts of its own to match on, so entry k is only meaningful as "the answer to amountsIn[k]".
  it('rejects an amountsOut of the wrong length', async () => {
    const { impl } = stub({ amountsOut: ['0', '998'] });
    expect(await ask(impl)).toEqual([0n, 0n, 0n]);
  });

  it('rejects the whole response over one unreadable entry', async () => {
    const { impl } = stub({ amountsOut: ['0', 'nine hundred', '1996'] });
    expect(await ask(impl)).toEqual([0n, 0n, 0n]);
  });

  it('rejects a negative amount', async () => {
    const { impl } = stub({ amountsOut: ['0', '-998', '1996'] });
    expect(await ask(impl)).toEqual([0n, 0n, 0n]);
  });

  it('rejects a fractional amount', async () => {
    const { impl } = stub({ amountsOut: [0, 998.5, 1996] });
    expect(await ask(impl)).toEqual([0n, 0n, 0n]);
  });

  it('rejects a nested null where an amount belongs', async () => {
    const { impl } = stub({ amountsOut: ['0', null, '1996'] });
    expect(await ask(impl)).toEqual([0n, 0n, 0n]);
  });
});
