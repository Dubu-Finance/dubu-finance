# testdata

`eip1559_vector.hex` — one signed EIP-1559 transaction, produced by Foundry rather than by this
crate, so that `tx::tests::the_signed_envelope_matches_cast_byte_for_byte` is a genuine
cross-implementation check on the RLP field order and not a round trip against ourselves.

Regenerate with:

```
cast mktx \
  --private-key 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 \
  --chain 91342 --nonce 7 \
  --priority-gas-price 5000000 --gas-price 50000000 --gas-limit 400000 \
  0xA629071E606F425dB93310c3ecc35E00Fbe16358 0xdeadbeef
```

The key is Anvil's first development account, printed in every Anvil startup banner and in
Foundry's documentation. It is a published test fixture, not a secret, and it is used here only
so a reader can reproduce the vector.
