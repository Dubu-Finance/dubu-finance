import { describe, expect, it } from 'vitest';
import { privateKeyToAccount } from 'viem/accounts';
import { getAddress, type Address } from 'viem';

import { ORDER_COMPONENTS } from '../src/abi.js';
import { loadConfig, type Env } from '../src/config.js';
import { MIN_EXPIRY_HEADROOM_SECS, validateQuote, type Order } from '../src/rfq.js';

/**
 * The maker is the one input this service does not compute, so these are the tests that matter
 * most. Each one is a response a compromised or buggy maker could plausibly return, and each must
 * be refused rather than routed into.
 *
 * Signatures are real: a throwaway key signs the EIP-712 digest exactly as the maker would, so
 * "the signature verifies" is being tested rather than stubbed. A test that mocked the recovery
 * would pass with the check deleted.
 */

const MAKER_KEY = '0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d' as const;
const account = privateKeyToAccount(MAKER_KEY);

const PMM_SETTLE: Address = getAddress('0x68CFa6E265AffD5D0DB2C49E4bb9DaEC5A920A9E');
const MUSDC: Address = getAddress('0xd28596C6750D87C53EA146134AfAB53de86C5155');
const MWETH: Address = getAddress('0x81e46C6379498beBEB5DCcD47ab2DdFaf967d445');

const NOW = 1_785_000_000;

function env(overrides: Partial<Env> = {}): Env {
  return {
    PMM_SETTLE,
    PMM_ADAPTER: '0x92CC1139212d02c8CF198dE804161432feEa4eBD',
    RFQ_MAKER_URL: 'https://maker.example/quote',
    RFQ_MAKER_ADDRESS: account.address,
    ...overrides,
  };
}

function baseOrder(overrides: Partial<Order> = {}): Order {
  return {
    maker: getAddress(account.address),
    makerAsset: MWETH,
    takerAsset: MUSDC,
    makerAmount: 5_000_000_000_000_000_000n, // 5 mWETH out
    takerAmount: 10_000_000_000n, // 10,000 mUSDC in
    nonce: 1n,
    expiry: BigInt(NOW + 60),
    decayStart: 0n,
    decayPerSec: 0,
    decayCap: 0,
    minFillBps: 0,
    ...overrides,
  };
}

async function signed(order: Order, signer = account) {
  const signature = await signer.signTypedData({
    domain: { name: 'DuBu PmmSettle', version: '1', chainId: 91342, verifyingContract: PMM_SETTLE },
    types: { Order: ORDER_COMPONENTS.map((c) => ({ name: c.name, type: c.type })) },
    primaryType: 'Order',
    message: order as never,
  });
  return {
    order: {
      ...order,
      makerAmount: order.makerAmount.toString(),
      takerAmount: order.takerAmount.toString(),
      nonce: order.nonce.toString(),
      expiry: order.expiry.toString(),
      decayStart: order.decayStart.toString(),
    },
    signature,
  };
}

const req = {
  tokenIn: MUSDC,
  tokenOut: MWETH,
  amountIn: 10_000_000_000n,
  ammAmountOut: 4_900_000_000_000_000_000n,
  nowSecs: NOW,
};

describe('a well-formed quote', () => {
  it('is accepted and reports the maker amount as the output', async () => {
    const r = await validateQuote(loadConfig(env()), req, await signed(baseOrder()));
    expect(r.rejected).toBeNull();
    expect(r.quote?.amountOut).toBe(5_000_000_000_000_000_000n);
  });
});

describe('a maker that answers a different question', () => {
  it('is refused when the tokens are not the pair that was asked for', async () => {
    const r = await validateQuote(loadConfig(env()), req, await signed(baseOrder({ makerAsset: MUSDC })));
    expect(r.rejected).toBe('wrong-pair');
  });

  it('is refused when the direction is inverted', async () => {
    const flipped = baseOrder({ makerAsset: MUSDC, takerAsset: MWETH });
    const r = await validateQuote(loadConfig(env()), req, await signed(flipped));
    expect(r.rejected).toBe('wrong-pair');
  });

  // A maker quoting a smaller size and a correspondingly smaller output would otherwise be
  // compared against the AMMs' quote for the FULL size and lose on merit — but if it ever won, the
  // route would move less than the caller asked for.
  it('is refused when the size is not the size that was asked for', async () => {
    const r = await validateQuote(loadConfig(env()), req, await signed(baseOrder({ takerAmount: 5_000_000_000n })));
    expect(r.rejected).toBe('wrong-size');
  });
});

describe('signatures', () => {
  it('is refused when signed by somebody other than the configured maker', async () => {
    const impostor = privateKeyToAccount('0x8b3a350cf5c34c9194ca85829a2df0ec3153be0318b5e2d3348e872092edffba');
    // The order still *names* the real maker; only the signature is the impostor's. This is the
    // shape that passes a naive "does order.maker look right" check.
    const body = await signed(baseOrder(), impostor);
    const r = await validateQuote(loadConfig(env()), req, body);
    expect(r.rejected).toBe('bad-signature');
  });

  it('is refused when the order names a maker we did not configure', async () => {
    const other = privateKeyToAccount('0x8b3a350cf5c34c9194ca85829a2df0ec3153be0318b5e2d3348e872092edffba');
    const r = await validateQuote(loadConfig(env()), req, await signed(baseOrder({ maker: other.address }), other));
    expect(r.rejected).toBe('bad-signature');
  });

  it('is refused when the signature is not 65 bytes', async () => {
    const body = await signed(baseOrder());
    const r = await validateQuote(loadConfig(env()), req, { ...body, signature: '0xdeadbeef' });
    expect(r.rejected).toBe('malformed');
  });

  // The domain binds the verifying contract. An order signed for a different PmmSettle would
  // recover to somebody else entirely against ours, which is exactly what should happen.
  it('is refused when signed against a different settlement contract', async () => {
    const order = baseOrder();
    const signature = await account.signTypedData({
      domain: {
        name: 'DuBu PmmSettle',
        version: '1',
        chainId: 91342,
        verifyingContract: getAddress('0x00000000000000000000000000000000000000bd'),
      },
      types: { Order: ORDER_COMPONENTS.map((c) => ({ name: c.name, type: c.type })) },
      primaryType: 'Order',
      message: order as never,
    });
    const r = await validateQuote(loadConfig(env()), req, {
      order: { ...order, makerAmount: order.makerAmount.toString(), takerAmount: order.takerAmount.toString(),
               nonce: '1', expiry: order.expiry.toString(), decayStart: '0' },
      signature,
    });
    expect(r.rejected).toBe('bad-signature');
  });
});

describe('expiry', () => {
  it('is refused when already expired', async () => {
    const r = await validateQuote(loadConfig(env()), req, await signed(baseOrder({ expiry: BigInt(NOW - 1) })));
    expect(r.rejected).toBe('expired');
  });

  // The headroom is the point: a quote that is valid *now* but expires before the transaction can
  // be signed and mined is a route that reverts and a user who paid gas to find out.
  it('is refused when it expires inside the headroom, even though it is still valid', async () => {
    const barely = BigInt(NOW + MIN_EXPIRY_HEADROOM_SECS - 1);
    expect(MIN_EXPIRY_HEADROOM_SECS).toBeGreaterThan(0);
    const r = await validateQuote(loadConfig(env()), req, await signed(baseOrder({ expiry: barely })));
    expect(r.rejected).toBe('expired');
  });
});

describe('can the maker actually deliver', () => {
  it('is refused when the order exceeds what the maker can pay out', async () => {
    const r = await validateQuote(
      loadConfig(env()),
      { ...req, makerCanDeliver: 4_000_000_000_000_000_000n },
      await signed(baseOrder()), // asks for 5
    );
    expect(r.rejected).toBe('maker-cannot-deliver');
  });

  it('is accepted when the maker can cover it exactly', async () => {
    const r = await validateQuote(
      loadConfig(env()),
      { ...req, makerCanDeliver: 5_000_000_000_000_000_000n },
      await signed(baseOrder()),
    );
    expect(r.rejected).toBeNull();
  });

  // An unread balance must not read as a passing one. The caller reports it as unverified instead
  // of this pretending it checked.
  it('does not invent a verdict when the solvency read failed', async () => {
    const r = await validateQuote(loadConfig(env()), { ...req, makerCanDeliver: undefined }, await signed(baseOrder()));
    expect(r.rejected).toBeNull();
  });
});

describe('the nonsense filter', () => {
  // The regression this replaced a 5% bound to fix: on GIWA the UniV2 pool was 30% off the market
  // and the maker was right. A correct quote must survive being much better than a stale AMM.
  it('accepts a quote that beats a stale AMM by 30%', async () => {
    const better = baseOrder({ makerAmount: 6_400_000_000_000_000_000n }); // ~30% over the AMMs
    const r = await validateQuote(loadConfig(env()), req, await signed(better));
    expect(r.rejected).toBeNull();
  });

  it('still refuses a response that cannot be a price at all', async () => {
    const absurd = baseOrder({ makerAmount: 5_000_000_000_000_000_000_000_000n }); // a million times
    const r = await validateQuote(loadConfig(env()), req, await signed(absurd));
    expect(r.rejected).toBe('implausible');
  });

  // On a market the AMMs cannot serve there is no baseline, and refusing on those grounds would
  // refuse RFQ precisely where RFQ is the only venue.
  it('is skipped when the AMMs quoted nothing', async () => {
    const huge = baseOrder({ makerAmount: 5_000_000_000_000_000_000_000_000n });
    const r = await validateQuote(loadConfig(env()), { ...req, ammAmountOut: 0n }, await signed(huge));
    expect(r.rejected).toBeNull();
  });
});

describe('configuration', () => {
  // A URL without an address would mean trusting whoever answers that URL.
  it('disables the leg when the maker address is not configured', async () => {
    const cfg = loadConfig(env({ RFQ_MAKER_ADDRESS: undefined }));
    expect(cfg.rfqMakerUrl).toBeNull();
    const r = await validateQuote(cfg, req, await signed(baseOrder()));
    expect(r.rejected).toBe('disabled');
  });

  it('disables the leg when the settlement contract is not configured', async () => {
    expect(loadConfig(env({ PMM_SETTLE: undefined })).rfqMakerUrl).toBeNull();
  });
});

describe('malformed responses', () => {
  it.each([
    ['null', null],
    ['a string', 'nope'],
    ['an empty object', {}],
    ['a missing signature', { order: baseOrder() }],
  ])('refuses %s', async (_label, body) => {
    const r = await validateQuote(loadConfig(env()), req, body);
    expect(r.rejected).toBe('malformed');
  });

  it('refuses a field that would silently truncate in the ABI encoder', async () => {
    const body = await signed(baseOrder());
    const r = await validateQuote(loadConfig(env()), req, {
      ...body,
      order: { ...body.order, minFillBps: 70_000 },
    });
    expect(r.rejected).toBe('malformed');
  });

  it('refuses a zero output dressed up as a quote', async () => {
    const r = await validateQuote(loadConfig(env()), req, await signed(baseOrder({ makerAmount: 0n })));
    expect(r.rejected).toBe('malformed');
  });
});
