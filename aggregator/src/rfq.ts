import { getAddress, hexToBigInt, isHex, recoverTypedDataAddress, type Address, type Hex } from 'viem';

import { ORDER_COMPONENTS } from './abi.js';
import type { Config } from './config.js';

/**
 * Asking the maker for a signed quote, and refusing most of what comes back.
 *
 * # The maker is not trusted
 *
 * This is the one place the aggregator takes instructions from something it did not compute. The
 * maker endpoint is a network service; a compromised or merely buggy one can return an order for
 * the wrong tokens, an amount it will not honour, an expiry in the past, or a signature from
 * somebody else entirely. None of that is caught downstream — the router executes the plan it is
 * given, and a bad RFQ leg surfaces as a revert at best and a bad fill at worst.
 *
 * So every field is checked against what was *asked for*, not against what the response says about
 * itself, and the signature is verified here rather than being taken on faith because it arrived
 * over TLS. In particular:
 *
 * - `takerAsset` and `makerAsset` must be the exact pair requested, in the requested direction.
 * - `takerAmount` must equal the requested input. A maker that quotes a different size has quoted
 *   a different trade.
 * - The signature must recover to [`Config.rfqMakerAddress`], the address configured at deploy
 *   time. Not to a `maker` field in the response, which would be the response vouching for itself.
 * - `order.maker` must be that same address, since that is who `PmmSettle` will pull from.
 * - The expiry must have [`MIN_EXPIRY_HEADROOM_SECS`] left, so a quote cannot expire between being
 *   returned and being mined.
 *
 * # Being suspicious of a good price
 *
 * [`MAX_IMPROVEMENT_BPS`] rejects a quote that beats the on-chain venues by more than a plausible
 * margin. A maker quoting 40% better than the AMMs is not generous, it is broken or hostile, and
 * routing into it produces a plan that reverts and a user who paid gas to find out. An RFQ leg
 * should beat the curve by tens of basis points, not by a multiple.
 */

/** A quote must still be valid this long after we return it. */
export const MIN_EXPIRY_HEADROOM_SECS = 20;

/** How far past the best AMM quote an RFQ quote may claim to be, in bps, before it is refused. */
export const MAX_IMPROVEMENT_BPS = 500;

export interface Order {
  maker: Address;
  makerAsset: Address;
  takerAsset: Address;
  makerAmount: bigint;
  takerAmount: bigint;
  nonce: bigint;
  expiry: bigint;
  decayStart: bigint;
  decayPerSec: number;
  decayCap: number;
  minFillBps: number;
}

export interface RfqQuote {
  order: Order;
  signature: Hex;
  /** What the taker gets: `makerAmount`, restated for the caller's comparison. */
  amountOut: bigint;
}

/** Why a maker's response was refused. Returned rather than thrown — an unusable RFQ quote is a
 *  normal outcome that must degrade to AMM-only routing, not a failed request. */
export type RfqRejection =
  | 'disabled'
  | 'unreachable'
  | 'malformed'
  | 'wrong-pair'
  | 'wrong-size'
  | 'expired'
  | 'bad-signature'
  | 'implausible';

export interface RfqResult {
  quote: RfqQuote | null;
  rejected: RfqRejection | null;
}

export interface RfqRequest {
  tokenIn: Address;
  tokenOut: Address;
  amountIn: bigint;
  /** The best the on-chain venues offered, for the plausibility check. Zero disables it. */
  ammAmountOut: bigint;
  nowSecs: number;
}

const EIP712_TYPES = {
  Order: ORDER_COMPONENTS.map((c) => ({ name: c.name, type: c.type })),
} as const;

/** Requests a quote and validates it. Never throws; a failure is a rejection. */
export async function requestQuote(cfg: Config, req: RfqRequest, fetchImpl = fetch): Promise<RfqResult> {
  if (!cfg.rfqMakerUrl || !cfg.rfqMakerAddress || !cfg.pmmSettle) {
    return { quote: null, rejected: 'disabled' };
  }

  let body: unknown;
  try {
    const res = await fetchImpl(cfg.rfqMakerUrl, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({
        chainId: cfg.chainId,
        verifyingContract: cfg.pmmSettle,
        takerAsset: req.tokenIn,
        makerAsset: req.tokenOut,
        takerAmount: req.amountIn.toString(),
      }),
    });
    if (!res.ok) return { quote: null, rejected: 'unreachable' };
    body = await res.json();
  } catch {
    return { quote: null, rejected: 'unreachable' };
  }

  return validateQuote(cfg, req, body);
}

/** The validation half, separated so it can be tested against hand-written hostile responses. */
export async function validateQuote(cfg: Config, req: RfqRequest, body: unknown): Promise<RfqResult> {
  if (!cfg.rfqMakerAddress || !cfg.pmmSettle) return { quote: null, rejected: 'disabled' };

  const parsed = parseOrder(body);
  if (!parsed) return { quote: null, rejected: 'malformed' };
  const { order, signature } = parsed;

  if (order.takerAsset !== getAddress(req.tokenIn) || order.makerAsset !== getAddress(req.tokenOut)) {
    return { quote: null, rejected: 'wrong-pair' };
  }
  if (order.takerAmount !== req.amountIn) {
    return { quote: null, rejected: 'wrong-size' };
  }
  if (order.expiry <= BigInt(req.nowSecs + MIN_EXPIRY_HEADROOM_SECS)) {
    return { quote: null, rejected: 'expired' };
  }
  if (order.makerAmount === 0n) {
    return { quote: null, rejected: 'malformed' };
  }

  // A quote that is too good is a defect, not a gift. Skipped when there is no AMM quote to
  // compare against, because then there is no baseline and refusing would just mean refusing every
  // RFQ quote on a market the AMMs cannot serve — which is the market RFQ exists for.
  if (req.ammAmountOut > 0n) {
    const ceiling = (req.ammAmountOut * BigInt(10_000 + MAX_IMPROVEMENT_BPS)) / 10_000n;
    if (order.makerAmount > ceiling) return { quote: null, rejected: 'implausible' };
  }

  if (order.maker !== cfg.rfqMakerAddress) {
    return { quote: null, rejected: 'bad-signature' };
  }

  let signer: Address;
  try {
    signer = await recoverTypedDataAddress({
      domain: {
        name: 'DuBu PmmSettle',
        version: '1',
        chainId: cfg.chainId,
        verifyingContract: cfg.pmmSettle,
      },
      types: EIP712_TYPES,
      primaryType: 'Order',
      // `parseOrder` has already range-checked every field against its ABI type, so the structural
      // cast is asserting something that was verified rather than assumed.
      message: order as never,
      signature,
    });
  } catch {
    return { quote: null, rejected: 'bad-signature' };
  }

  if (getAddress(signer) !== cfg.rfqMakerAddress) {
    return { quote: null, rejected: 'bad-signature' };
  }

  return { quote: { order, signature, amountOut: order.makerAmount }, rejected: null };
}

function parseOrder(body: unknown): { order: Order; signature: Hex } | null {
  if (typeof body !== 'object' || body === null) return null;
  const b = body as Record<string, unknown>;
  const raw = (b.order ?? b) as Record<string, unknown>;
  const signature = b.signature;
  if (typeof signature !== 'string' || !isHex(signature) || signature.length !== 132) return null;

  try {
    const order: Order = {
      maker: getAddress(str(raw.maker)),
      makerAsset: getAddress(str(raw.makerAsset)),
      takerAsset: getAddress(str(raw.takerAsset)),
      makerAmount: num(raw.makerAmount),
      takerAmount: num(raw.takerAmount),
      nonce: num(raw.nonce),
      expiry: num(raw.expiry),
      decayStart: num(raw.decayStart ?? 0),
      decayPerSec: Number(num(raw.decayPerSec ?? 0)),
      decayCap: Number(num(raw.decayCap ?? 0)),
      minFillBps: Number(num(raw.minFillBps ?? 0)),
    };
    // Range checks the ABI encoder would otherwise silently satisfy by truncating.
    if (order.decayPerSec > 0xffff_ffff || order.decayCap > 0xffff_ffff || order.minFillBps > 0xffff) {
      return null;
    }
    if (order.nonce > 0xffff_ffff_ffff_ffffn || order.expiry > 0xffff_ffff_ffff_ffffn) return null;
    return { order, signature };
  } catch {
    return null;
  }
}

function str(v: unknown): string {
  if (typeof v !== 'string') throw new Error('expected string');
  return v;
}

function num(v: unknown): bigint {
  if (typeof v === 'bigint') return v;
  if (typeof v === 'number') {
    if (!Number.isSafeInteger(v) || v < 0) throw new Error('unsafe number');
    return BigInt(v);
  }
  if (typeof v === 'string') {
    const n = isHex(v) ? hexToBigInt(v) : BigInt(v);
    if (n < 0n) throw new Error('negative');
    return n;
  }
  throw new Error('expected numeric');
}
