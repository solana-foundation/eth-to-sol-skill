# Compute budget

Solana meters execution in **compute units (CU)**. Every instruction has a budget; exceeding it aborts. This is the analog of EVM gas, but unlike gas it is not directly paid in SOL — the budget is a runtime constraint, with a separate **priority fee** mechanism for ordering.

For transaction-level mechanics — recent blockhash expiry, versioned transactions, Address Lookup Tables, local fee markets, retry strategy, and commitment levels — load `optimization/transactions-and-commitment.md`.

## The numbers

- **Default per-instruction budget:** 200,000 CU.
- **Maximum per-instruction budget:** 1,400,000 CU (requestable via `ComputeBudgetInstruction::set_compute_unit_limit`).
- **Per-transaction ceiling:** sum of all instructions, bounded by the same 1.4M.

Rough costs (Solana is highly optimized — exact numbers shift between releases):

| Operation | Approx CU |
|---|---|
| Account deserialization (Anchor, small) | 1,000–3,000 |
| Account serialization on save | 1,000–3,000 |
| `find_program_address` | 1,500–2,500 per call |
| SPL Token transfer CPI | 4,500–6,000 |
| SPL Token mint_to CPI | 4,500–6,500 |
| `Clock::get()` | ~100 |
| `emit!` (event log) | 100–500 + per-byte |
| `msg!` (string log) | 100 per call + per-byte |
| Account `init` (allocation + write) | 5,000–10,000 |

Treat these as orders of magnitude, not budgets. A typical Anchor instruction lands in the 20–80k range without micro-optimization.

## When CU matters

- **Compose-heavy paths**: instructions that CPI into 3–4 programs (e.g. swap routers, multi-hop AMMs) can blow through 200k easily.
- **Hot loops**: iterating over `Vec` fields, especially in the naive port. Each iteration deserializes/serializes the whole `Vec`.
- **Large account writes**: serializing a 10 KB account costs real CU.

When CU matters, the answer is usually structural (drop the loop, split the account, drop the event) before it is micro-optimization.

## Common wins porting from EVM

1. **Cache PDA bumps.** Re-deriving on every call is 1.5–2.5k CU per PDA per call. Cached = 0.
2. **Don't iterate `Vec` fields**; use PDA-per-key. Iteration is O(n) deserialization plus O(n) compares.
3. **Drop redundant events.** SPL Token already emits Transfer; don't duplicate.
4. **Don't `msg!` in the hot path.** Strings cost. Use `#[event]` (smaller serialization) where data, not text, is the goal.
5. **Use Anchor's account types where they exist.** `Account<'info, TokenAccount>` does the SPL deserialization once; manual deserialization in your code costs more.
6. **Avoid `init_if_needed`** in hot paths — it carries a runtime existence check. Use `init` once, separate `update` instructions thereafter.

## Increasing the budget

To bump an instruction past 200k:

```typescript
// client side, before the actual instruction:
import { ComputeBudgetProgram } from '@solana/web3.js';

ComputeBudgetProgram.setComputeUnitLimit({ units: 600_000 });
```

The runtime caps at 1.4M total per transaction. Set the limit only as high as you need — higher requests increase priority fee cost.

## Priority fees

Priority fees are paid in micro-lamports per CU and tip the validator to include your transaction sooner. They are not the gas cost; they are an ordering bid:

```text
priority_fee = requested_compute_unit_limit * compute_unit_price
```

Set them against the transaction's actual writable account set, not generic network busyness. Congestion is usually local to hot writable accounts, so a payment transaction touching cold accounts should not blindly inherit the fee implied by unrelated DEX activity. Keep the CU limit tight because the fee scales with the limit requested, not the compute consumed.

## When raw Solana beats Anchor on CU

Anchor's account validation costs ~1–3k CU per account, deserialization included. For instructions touching 10+ accounts in a hot path, the overhead becomes real. Options:

- **Manual account checks**: use `AccountInfo` and do owner/discriminator checks yourself. Risky — easy to forget a check.
- **Anchor lite** (`UncheckedAccount` + minimal manual checks): selectively skip Anchor's checks where they cost more than the safety they buy.

For the reference ERC-20 example: stick with Anchor. The bottleneck is not CU.

## Measuring

Use `solana-program-test` or `litesvm` for offline measurement. `solana logs <txid>` shows "consumed N of M compute units" per instruction on real txs. Profile before optimizing.
