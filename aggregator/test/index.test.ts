import { describe, expect, it } from 'vitest';

import { nothingRoutable } from '../src/index.js';
import type { VenueId } from '../src/quote.js';

/**
 * "The pair is re-pricing and will be back" and "there is no market here" were the same 404, so a
 * frontend showed "quote not available" for both and a taker had no reason to try again. These pin
 * the two apart, and pin the compatibility that lets the engine ship its half whenever it likes:
 * only a refusal the engine *named* becomes a 503.
 */

const solo: Record<VenueId, bigint> = { prop: 0n, univ2: 0n };

const read = async (res: Response) => ({ status: res.status, body: (await res.json()) as Record<string, unknown> });

describe('when nothing could be routed', () => {
  it('asks for a retry when the prop side is withdrawn to re-price', async () => {
    const { status, body } = await read(nothingRoutable('no-capacity', solo));
    expect(status).toBe(503);
    expect(body.retryable).toBe(true);
    expect(String(body.error)).toContain('re-pricing');
  });

  // The old behaviour, unchanged: a `null` is every other way of arriving here, including the
  // 200-with-zeros an engine that has not shipped the 503 still sends.
  it('still 404s when no venue named a reason', async () => {
    const { status, body } = await read(nothingRoutable(null, solo));
    expect(status).toBe(404);
    expect(body.error).toBe('no venue would fill that size');
    expect(body.retryable).toBeUndefined();
  });

  // The copy is load-bearing: a latched risk killswitch has held pairs down for over a day, so the
  // 404 must not promise that what caused it goes away by itself.
  it('promises recovery only where recovery is promised', async () => {
    const { body: withdrawn } = await read(nothingRoutable('no-capacity', solo));
    const { body: dead } = await read(nothingRoutable(null, solo));
    expect(String(withdrawn.detail)).toContain('retrying');
    expect(String(dead.detail)).not.toMatch(/resolve on their own|retry/i);
    expect(String(dead.detail)).toContain('operator');
  });

  it('shows what each venue offered either way', async () => {
    for (const res of [nothingRoutable('no-capacity', { prop: 0n, univ2: 7n }), nothingRoutable(null, { prop: 0n, univ2: 7n })]) {
      expect((await read(res)).body.solo).toEqual({ prop: '0', univ2: '7' });
    }
  });
});
