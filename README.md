# Solana program

- Program id: `67qq9FLj9FVX1d4fT5Ms4HzCcUVpU6VGDk6v2zhVkWnj` (Solana mainnet)

## Verifiable build

```bash
solana-verify build --library-name unmarked_fees
solana-verify get-executable-hash target/deploy/unmarked_fees.so
solana-verify get-program-hash -um 67qq9FLj9FVX1d4fT5Ms4HzCcUVpU6VGDk6v2zhVkWnj
```

The two hashes must be equal. The same build runs in this repository's "Verifiable build" workflow.
