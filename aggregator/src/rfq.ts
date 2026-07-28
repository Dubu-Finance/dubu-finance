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
 * # Checking that the maker can deliver, rather than guessing from the price
 *
 * An earlier version refused any quote beating the AMMs by more than 5%, on the reasoning that a
 * maker quoting far better than the curve is broken or hostile. That check was aimed at the wrong
 * variable, and the live deployment proved it: the maker quoted mWETH off a real Binance reference
 * while the UniV2 pool — seeded once and never arbitraged — was 30% away from the market. The
 * maker was right and the AMM was wrong, and the check rejected the correct price. The premise
 * "the AMMs are near fair" is exactly what an aggregator cannot assume; it is what it exists to
 * find out.
 *
 * What the check was reaching for is real, but it is not a price property. The failure that costs
 * a taker gas is an order the maker *cannot honour* — insufficient balance, or an allowance that
 * no longer covers it — and `PmmSettle` pulls both legs with `transferFrom`, so that fails at fill
 * time with nothing useful in the trace. That is directly observable: [`checkSolvency`] reads the
 * maker's balance and its allowance to the settler, in the same multicall the venue quotes already
 * use. A quote the maker cannot cover is refused for the reason it will actually fail.
 *
 * [`MAX_IMPROVEMENT_BPS`] survives at a much wider setting, as a last-resort guard against a
 * response that is not merely optimistic but nonsensical — a maker offering a thousand times the
 * market is malfunctioning whatever its balance says.
 */

/**
 * A quote must still be valid this long after we return it.
 *
 * One second, sized against the whole path a quote has to survive, measured rather than guessed:
 *
 * ```text
 *   quote round trip to the taker   ~78ms   (edge, measured)
 *   sign and submit                 ~50ms
 *   inclusion in a preconfirmation  440ms median, 508ms max over six transactions
 *   ----------------------------------------
 *                                  ~640ms worst observed
 * ```
 *
 * The target is *inclusion*, not confirmation: a fill executes against preconfirmed state, so an
 * order only has to be unexpired at the block that includes it. A confirmed receipt takes about a
 * second and nothing here waits for one — an earlier 3s was sized against that receipt, which
 * measured bookkeeping instead of the chain.
 *
 * Headroom is not free. The maker's TTL is an option it writes, priced by
 * `quoting::ttl_premium_e2` and growing with sqrt(TTL), so every second demanded here forces a
 * second of TTL and therefore a wider quote. This pairs with a 2s TTL, leaving the taker a full
 * second of slack over a 640ms worst case.
 */
export const MIN_EXPIRY_HEADROOM_SECS = 1;

/**
 * How far past the best AMM quote an RFQ quote may claim to be, in bps, before it is refused.
 *
 * 100x, not the 5% an earlier version used. This is a nonsense filter, not a price opinion: on a
 * market where the AMMs are stale a correct RFQ quote can be tens of percent better, and refusing
 * it would be refusing the right price. What it still catches is a response that cannot be a price
 * at all.
 */
export const MAX_IMPROVEMENT_BPS = 1_000_000;

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
  | 'implausible'
  | 'maker-cannot-deliver'
  | 'refused';

export interface RfqResult {
  quote: RfqQuote | null;
  rejected: RfqRejection | null;
  /**
   * The maker's own words, when it refused for a reason of its own.
   *
   * Kept because 'the maker declined, and here is why' and 'the maker could not be reached' are
   * different operational facts and collapsing them loses the one that is actionable. A size over
   * the maker's per-order cap read as `unreachable` until this existed, which pointed at the
   * network instead of at the cap.
   */
  makerReason?: string;
}

export interface RfqRequest {
  tokenIn: Address;
  tokenOut: Address;
  amountIn: bigint;
  /** The best the on-chain venues offered, for the nonsense filter. Zero disables it. */
  ammAmountOut: bigint;
  /**
   * What the maker can actually pay out: the lesser of its balance in the maker asset and its
   * allowance to `PmmSettle`. `undefined` skips the check, which is what happens when the read
   * failed — a missing observation must not silently become a passing one, so the caller says so
   * in the response rather than this pretending it verified something.
   */
  makerCanDeliver?: bigint;
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
    if (!res.ok) {
      // A 5xx is the maker failing; a 4xx is the maker declining, and it says why in the body.
      const reason = await res
        .json()
        .then((b) => (b as { error?: string })?.error)
        .catch(() => undefined);
      return res.status >= 500
        ? { quote: null, rejected: 'unreachable', makerReason: reason }
        : { quote: null, rejected: 'refused', makerReason: reason ?? `http ${res.status}` };
    }
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

  // The check that matters: can the maker actually pay this out? An order it cannot cover fails
  // at fill time inside a `transferFrom`, which costs the taker gas and yields nothing useful in
  // the trace.
  if (req.makerCanDeliver !== undefined && order.makerAmount > req.makerCanDeliver) {
    return { quote: null, rejected: 'maker-cannot-deliver' };
  }

  // The nonsense filter. Wide on purpose — see MAX_IMPROVEMENT_BPS. Skipped when there is no AMM
  // quote to compare against, because then there is no baseline and refusing would mean refusing
  // every RFQ quote on a market the AMMs cannot serve, which is the market RFQ exists for.
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
