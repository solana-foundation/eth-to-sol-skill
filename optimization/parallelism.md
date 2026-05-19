# Parallelism — Sealevel write locks

Solana's runtime (Sealevel) executes non-conflicting transactions in parallel. Two transactions conflict if they both write the same account, or one writes what the other reads. Design state so disjoint operations touch disjoint writable accounts.

This is the principal *performance* reason to use PDAs per entity. The naive single-account layout works — it is just stuck on one execution lane.

## How locking works

For every instruction in a transaction, every account is declared either readonly or writable. The scheduler builds a dependency graph from these declarations and parallelizes transactions whose writable sets do not intersect.

- **Two transactions writing the same account** → serialize.
- **One writing, one reading the same account** → serialize.
- **Two reading the same account** → parallelize.
- **Disjoint writable sets** → parallelize.

The lock is *per account*, not per program. A program with thousands of users can scale linearly *if* user state is per-account. A program with one big shared state account cannot scale at all.

## Anti-pattern: shared write-hot account

```rust
#[account]
pub struct Token {
    pub total_supply: u64,
    pub balances: Vec<(Pubkey, u64)>,   // SMELL: every transfer writes this
    pub allowances: Vec<(Pubkey, Pubkey, u64)>,
}
```

Every `transfer`, `mint`, `burn`, `approve` writes this one account. Throughput ceiling: ~1 op per slot per program. For a token with any meaningful TPS, this is broken.

## The fix: per-entity PDAs

```rust
#[account]
pub struct Config {
    pub authority: Pubkey,
    pub bump: u8,
}
// seeds = [b"config"]   — read-mostly

#[account]
pub struct Balance {
    pub owner: Pubkey,
    pub amount: u64,
    pub bump: u8,
}
// seeds = [b"balance", owner.as_ref()]
```

A transfer from Alice to Bob writes:
- Alice's `Balance` PDA
- Bob's `Balance` PDA

A transfer from Carol to Dave writes a fully disjoint set. The two run in parallel.

For ERC-20 → SPL Token specifically, this is what ATAs give you for free. SPL Token's per-account structure is *the same fix*, already implemented and audited.

## Subtle: read-locks matter too

If many instructions take the same `Config` PDA as a writable account "just to read it", you serialize them needlessly. Audit account declarations:

- `#[account(mut, ...)]` declares writable.
- `#[account(...)]` (no `mut`) declares readonly.

Read-only `Config` accesses across thousands of users do not conflict with each other. Writable accesses do, even if you only read the field.

## Subtle: total_supply / global counter pattern

Tracking a global counter (`totalSupply`, `totalShares`, `totalDeposits`) on a single PDA *forces* every mutating operation to write-lock that PDA, killing parallelism. Options:

1. **Drop it** if you can derive from another source (SPL Token `Mint.supply` for fungible tokens).
2. **Shard it.** Maintain N counter PDAs, hash the writer into a shard:
   ```rust
   let shard_idx = (writer.key().to_bytes()[0] as usize) % NUM_SHARDS;
   ```
   Sum off-chain. Costs O(N) reads to query the total, but unlocks parallelism.
3. **Defer it.** Update the global asynchronously via a separate reconciliation instruction. Acceptable only when staleness is OK.

The right answer is usually (1). Sharding (2) is only worth it for genuinely hot counters with no underlying source-of-truth.

## Accumulator patterns — when the contention is the protocol

Some DeFi designs have global state that *must* be checkpointed on every interaction. Synthetix-style staking (`rewardPerTokenStored` + `lastUpdateTime`), Curve/Convex emission accumulators, ve-token decay curves. They share a structural property: the formula extrapolates from a recent checkpoint, and every state-changing call refreshes that checkpoint *before* its own mutation.

The optimization is not to remove the contention but to bound it:

1. **Keep accumulator fields on a small `Vault` / `Pool` PDA.** Small means per-call deserialization is cheap. Don't bury the accumulator inside a large account that also holds user state — the Vec contention would compound the accumulator contention.
2. **Per-user state in separate PDAs.** This eliminates the cross-user Vec contention, leaving only the accumulator-write contention.
3. **Be honest in docs.** The per-pool throughput ceiling for this pattern is ~1 state-changing op per slot (~2.5/sec). If your protocol needs more, the answer is structural: more pools (parameterize the accumulator), epoch-based payouts, or off-chain accumulators with on-chain Merkle roots. Don't pretend a PDA layout can fix it.

The skill's job is to express the constraint truthfully, not paper over it with per-user PDAs that don't actually unlock anything because every call still has to touch the accumulator account.

Cross-reference: `examples/staking-vault/03-optimized.rs` is the worked instance; `04-diff.md §P2` and `05-explanation.md §"HONEST LIMIT"` discuss the ceiling.

## When the governance PDA can be read-only

The opposite extreme of the accumulator case: a protocol where no per-call writes touch the central config/vault PDA. ERC-4626 vanilla vaults are the canonical example.

Achievable when:

- Aggregates (`total_supply`, `total_assets`, etc.) read from SPL Token, not self-tracked. See `optimization/account-model.md` "Read aggregates from SPL Token."
- Per-user state lives in per-user PDAs (not a Vec on the vault).
- No global accumulator that must be checkpointed on every interaction. (Synthetix-style protocols cannot achieve this.)

When achieved, cross-user operations contend only on the inherent globals — `share_mint.supply`, `asset_reserve.amount` — which are write-hot in any fungible-token system and are maintained by SPL Token. The protocol's contention floor equals SPL Token's contention floor. This is the **minimum** possible contention for a pooled-asset protocol.

```rust
// In the Accounts struct for a user-facing deposit:
#[account(
    seeds = [b"vault", asset_mint.key().as_ref()],
    bump = vault.bump,
    has_one = asset_mint,
    has_one = share_mint,
)]
pub vault: Account<'info, Vault>,   // NOT mut — read-only on deposits
```

Cross-reference: `examples/erc4626-vault/03-optimized.rs` is the worked instance; `04-diff.md §P1` walks through the reads/writes.

## Solana's write-lock vs Ethereum's serial execution

Ethereum executes transactions serially within a block. Every transaction sees committed prior state; there is no parallelism to worry about, but there is also no parallelism to exploit. EVM developers tend to write code that incidentally serializes everything because there is no penalty for doing so.

Solana developers must design for parallelism *up front*. State layout is not just a storage decision — it is a throughput decision.

## How to verify

For a translated program, do this exercise per instruction:

1. List the writable accounts.
2. Identify which of them are functions of `(user, operation)` versus *global*.
3. Any *global* writable account is a serialization point. Defend it (does it need to be writable here?) or move the field out.
4. If the answer is "we write `Config.last_updated` on every call as a freshness indicator" — drop the field. It is parallelism poison and not load-bearing.

## Transaction-level account limits

A transaction can reference at most 64 accounts (with address lookup tables; 32 without). Per-instruction limits are tighter (~256KB total account data loaded). Per-entity PDAs are usually small (<200 bytes), so this is a real but generous budget.
