# Transactions and commitment

Use this file when the translation affects transaction construction, large account lists, payment batches, swap routes, priority fees, landing/retry behavior, settlement finality, or client/tooling migration. Keep this guidance API- and operations-focused; do not produce UI components or app screens.

## Recent Blockhash And Expiry

Solana does not use Ethereum-style nonces. Every transaction includes a recent blockhash, and that blockhash is valid for roughly 150 slots, about 60 seconds at normal slot time.

Production consequences:

- A transaction that does not land before expiry must be rebuilt with a fresh blockhash and re-signed.
- Poll `getSignatureStatuses` or the SDK equivalent until the signature reaches the desired commitment or the blockhash expires.
- A signature can land only once because the signed message includes the blockhash.

If a translated workflow depends on delayed signing, offline approvals, or long-lived prepared transactions, call out that the Solana version needs a fresh blockhash near send time.

## Versioned Transactions And Address Lookup Tables

Solana transactions have a hard size cap of roughly 1,232 bytes. Every inline account key costs 32 bytes. Token transfers, swap routes, and batch payments can hit this limit before they hit compute.

Use versioned v0 transactions with Address Lookup Tables (ALTs) when a transaction references many accounts. An ALT stores pubkeys on-chain and lets a v0 transaction reference them by compact index instead of carrying each 32-byte key inline.

Rules of thumb:

- Legacy transactions are fine for prototypes and small instruction sets.
- Payment batches, swap aggregators, and any route with a large precomputed account graph should use v0 + ALTs.
- Without ALTs, token-transfer batches often top out around 4-6 transfers. With ALTs, 20-30 transfers is a more realistic practical ceiling, subject to compute and account limits.

When a port replaces an EVM "loop over recipients" with Solana transactions, do not build a program that loops internally just to imitate EVM batching. Prefer multiple SPL Token transfer instructions in a v0 transaction, with ALTs if account keys dominate size.

## Compute Budget And Priority Fees

Compute Units (CU) are the runtime meter. The default per-instruction limit is 200,000 CU, and the transaction ceiling is 1,400,000 CU. Raise the limit with Compute Budget Program instructions only when measurement shows the default is too low.

Priority fee is an ordering bid:

```text
priority_fee = requested_compute_unit_limit * compute_unit_price
```

Keep the requested CU limit tight because the fee scales with what you request, not what the transaction actually consumes.

Priority fees are local in effect because contention is driven by writable accounts. If a DEX pool is hot, transactions writing that pool must bid more; an unrelated payment touching cold accounts should not blindly bid off global network activity. Estimate against the transaction's actual writable account set using `getRecentPrioritizationFees` or a provider estimator.

Load `optimization/compute-budget.md` for program-level CU design and measurement guidance.

## Commitment Levels

Choose commitment based on the consequence of being wrong.

| Commitment | Typical latency | Reorg risk | Use |
|---|---:|---|---|
| `processed` | < 400 ms | Possible | Optimistic status, local feedback, low-stakes monitoring. |
| `confirmed` | ~1 sec | Negligible in practice | Default for most production user flows, swaps, custody-side recording. |
| `finalized` | ~12-13 sec | None | Irreversible off-chain actions: fiat payout, goods release, official settlement. |

If an on-chain event triggers an off-chain action that cannot be undone, require `finalized`. If the system can correct itself later, `confirmed` is usually the right production default.

## Client And Tooling Map

Mention these only when the user asks for integration notes, tests, or operational workflow. They are not part of the on-chain program output unless the migration materially changes call sites.

| EVM tool / habit | Solana equivalent |
|---|---|
| ethers.js / viem | `@solana/kit` for new code; `@solana/web3.js` when existing ecosystem libraries require it |
| Hardhat / Foundry | Anchor CLI plus `cargo build-sbf`; LiteSVM, Bankrun, Mollusk, or `anchor test` depending on test depth |
| ABI | Anchor IDL |
| Etherscan / Tenderly | Solana Explorer, Solscan, SolanaFM, Helius/Triton tracing |
| The Graph | Custom indexer, Helius DAS/API, Carbon, Vixen, or Geyser/Yellowstone streams |

For generated notes, focus on account derivation, account lists, ATA creation, transaction format, commitment, and retry behavior. Do not generate UI implementation details.
