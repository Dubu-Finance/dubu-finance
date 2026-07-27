/**
 * The ABI fragments this service touches, and nothing else.
 *
 * Written out here rather than imported from `contracts/out/` on purpose. A generated artefact
 * would drift silently: the aggregator would keep compiling against a shape the deployed contract
 * no longer has, and the failure would surface as a decoded-garbage quote rather than a build
 * error. These are small enough to read, and `test/abi.test.ts` pins the selectors against the
 * values `cast sig` produces from the Solidity sources.
 */

/** `IPropPool`. Only the two views a router path needs — this service never writes. */
export const PROP_POOL_ABI = [
  {
    type: 'function',
    name: 'getAmountOut',
    stateMutability: 'view',
    inputs: [
      { name: 'tokenIn', type: 'address' },
      { name: 'tokenOut', type: 'address' },
      { name: 'amountIn', type: 'uint256' },
    ],
    outputs: [{ name: 'amountOut', type: 'uint256' }],
  },
  {
    type: 'function',
    name: 'effectiveCapacity',
    stateMutability: 'view',
    inputs: [{ name: 'pairId', type: 'uint16' }],
    outputs: [
      { name: 'bidCapacity', type: 'uint96' },
      { name: 'askCapacity', type: 'uint96' },
      { name: 'decaySecs', type: 'uint16' },
    ],
  },
] as const;

/** `UniswapV2Router02`. */
export const UNIV2_ROUTER_ABI = [
  {
    type: 'function',
    name: 'getAmountsOut',
    stateMutability: 'view',
    inputs: [
      { name: 'amountIn', type: 'uint256' },
      { name: 'path', type: 'address[]' },
    ],
    outputs: [{ name: 'amounts', type: 'uint256[]' }],
  },
] as const;

/** `Multicall3`, genesis pre-install. `aggregate3` so one failing call cannot void the batch. */
export const MULTICALL3_ABI = [
  {
    type: 'function',
    name: 'aggregate3',
    stateMutability: 'payable',
    inputs: [
      {
        name: 'calls',
        type: 'tuple[]',
        components: [
          { name: 'target', type: 'address' },
          { name: 'allowFailure', type: 'bool' },
          { name: 'callData', type: 'bytes' },
        ],
      },
    ],
    outputs: [
      {
        name: 'returnData',
        type: 'tuple[]',
        components: [
          { name: 'success', type: 'bool' },
          { name: 'returnData', type: 'bytes' },
        ],
      },
    ],
  },
] as const;

/**
 * `Router.swapExactIn`, and the nested route shape it takes.
 *
 * ```text
 *   RouteParams
 *     └─ Batch[]      parallel, weighted split of the total input
 *          └─ Hop[]   sequential; the output of hop k funds hop k+1
 *               └─ SwapStep[]  parallel forks inside one hop, weighted
 * ```
 */
export const ROUTER_ABI = [
  {
    type: 'function',
    name: 'swapExactIn',
    stateMutability: 'nonpayable',
    inputs: [
      {
        name: 'p',
        type: 'tuple',
        components: [
          { name: 'tokenIn', type: 'address' },
          { name: 'tokenOut', type: 'address' },
          { name: 'receiver', type: 'address' },
          { name: 'amountIn', type: 'uint256' },
          { name: 'quotedAmountOut', type: 'uint256' },
          { name: 'deadline', type: 'uint256' },
          {
            name: 'batches',
            type: 'tuple[]',
            components: [
              { name: 'weightBps', type: 'uint16' },
              {
                name: 'hops',
                type: 'tuple[]',
                components: [
                  { name: 'tokenIn', type: 'address' },
                  {
                    name: 'steps',
                    type: 'tuple[]',
                    components: [
                      { name: 'adapter', type: 'address' },
                      { name: 'rawData', type: 'uint256' },
                      { name: 'payload', type: 'bytes' },
                    ],
                  },
                ],
              },
            ],
          },
        ],
      },
      { name: 'minAmountOut', type: 'uint256' },
    ],
    outputs: [{ name: 'amountOut', type: 'uint256' }],
  },
] as const;

/** `PmmSettle.Order`, in EIP-712 field order. Any reordering changes the digest. */
export const ORDER_COMPONENTS = [
  { name: 'maker', type: 'address' },
  { name: 'makerAsset', type: 'address' },
  { name: 'takerAsset', type: 'address' },
  { name: 'makerAmount', type: 'uint256' },
  { name: 'takerAmount', type: 'uint256' },
  { name: 'nonce', type: 'uint64' },
  { name: 'expiry', type: 'uint64' },
  { name: 'decayStart', type: 'uint64' },
  { name: 'decayPerSec', type: 'uint32' },
  { name: 'decayCap', type: 'uint32' },
  { name: 'minFillBps', type: 'uint16' },
] as const;

/**
 * `PmmAdapter`'s step payload: `abi.encode(Order, signature, maxDecayPpm)`.
 *
 * No amount field, and that is the `IAdapter` contract rather than an omission — the adapter reads
 * its own balance to learn the size, which is why the funding bit must be set. See `route.ts`.
 */
export const PMM_PAYLOAD_ABI = [
  { name: 'order', type: 'tuple', components: ORDER_COMPONENTS },
  { name: 'signature', type: 'bytes' },
  { name: 'maxDecayPpm', type: 'uint32' },
] as const;

/** `PropPoolAdapter`'s step payload. */
export const PROP_PAYLOAD_ABI = [
  { name: 'base', type: 'address' },
  { name: 'quote', type: 'address' },
  { name: 'limitAmount', type: 'uint256' },
  { name: 'partnerId', type: 'uint256' },
  { name: 'deadline', type: 'uint256' },
] as const;
