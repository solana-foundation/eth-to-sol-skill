# Explanation log: `02-naive-port.rs` → `03-optimized.rs`

One entry per change in `04-diff.md`, grouped by theme. Each entry follows the schema: **What / Why / Benefit / Tradeoff**.

This example exists to teach two things at once:

1. The mechanical Solidity-to-Solana transforms that mirror the ERC-20 reference (per-user PDAs, checked arithmetic, declarative auth).
2. A **fundamental Solana constraint** the ERC-20 example didn't have to confront: Synthetix-style reward accumulators are write-hot *by design*. The protocol's contention floor is not the implementation's contention floor. Read §P2 carefully.

---

## State model

### Per-user `StakePosition` PDA instead of `Vec<UserStakeEntry>` inside the vault (diff §S1)

- **What:** Removed `entries: Vec<UserStakeEntry>` from `VaultState` (`02-naive-port.rs:345`). Each user now has a `StakePosition` PDA keyed by `[b"position", vault.key().as_ref(), user.key().as_ref()]` (`03-optimized.rs:478`).
- **Why:** Solana state is per-account. The Solidity contract had three mappings (`balanceOf`, `userRewardPerTokenPaid`, `rewards`) all keyed by user — naturally a per-user record. A `Vec` inside the central vault forces every stake/withdraw to scan the Vec *and* write-lock the vault for the whole program (cross-user). Per-user PDAs split the writes — only one user's PDA is touched per call, plus the vault for the accumulator update (which is fundamental — see §P2).
- **Benefit:** O(1) lookup instead of O(n). Cross-user parallelism for the per-user portion of state. No `MAX_STAKERS` cap; the staker count is unbounded. Each user pays their own ~0.002 SOL of rent rather than the protocol pre-paying for `MAX_STAKERS × entry_size`.
- **Tradeoff:** Two writable accounts per call (vault + position) instead of one. Each user's first `stake` triggers a one-time PDA init (~0.002 SOL rent + a few thousand CU). Anchor's `init_if_needed` makes this single-instruction; see §I2 for the re-init safety argument.

### Vault parameterized by `(staking_mint, rewards_mint)` (diff §S2)

- **What:** Vault seeds changed from `[b"vault"]` to `[b"vault", staking_mint.as_ref(), rewards_mint.as_ref()]` (`03-optimized.rs:277`). Same program can now run multiple staking pools.
- **Why:** Solidity's "one contract per pool" deploys are wasteful on Solana — each deploy costs program bytecode rent (~2 SOL per program). One program with parameterized vault PDAs is the canonical Solana pattern (think SPL Token: one program, N mints).
- **Benefit:** Cheaper to launch additional pools (~0.005 SOL per new vault instead of ~2 SOL per program deploy). Easier governance (one program upgrade authority covers all pools). DEXes and indexers that integrate the program automatically pick up new pools without re-integration work.
- **Tradeoff:** Vault-related seeds are longer (3 components instead of 1). A single bug in the program is now systemic across pools, not isolated to one deploy — important for the security threat model.

### `owner` field renamed to `authority` with declarative `has_one` gating (diff §S3)

- **What:** Field rename + constraint pattern, mirroring the ERC-20 reference example.
- **Why / Benefit / Tradeoff:** See `examples/erc20-token/05-explanation.md` § "Move `onlyOwner` checks to declarative `has_one` constraints" for the full discussion. Same rationale; same gains.

---

## Parallelism

### Per-user writes are now disjoint (diff §P1)

- **What:** Each user's `stake`/`withdraw`/`get_reward` writes their own `StakePosition` PDA. Cross-user calls touch disjoint position PDAs.
- **Why:** Sealevel parallelizes transactions with disjoint writable account sets. Per-user PDA splits the user-keyed state across accounts.
- **Benefit:** The user-state portion of the program parallelizes. For the parts of staking that are user-local (just balance + paid-marker updates), throughput scales with users.
- **Tradeoff:** Doesn't unlock cross-user parallelism on its own — the vault accumulator is the constraint. See §P2 next.

### HONEST LIMIT — VaultState is still write-hot, and this is fundamental (diff §P2)

- **What:** Every state-changing instruction (`stake`, `withdraw`, `get_reward`, `set_reward_rate`) writes the `VaultState` account because the accumulator (`reward_per_token_stored`, `last_update_time`) and `total_supply` must be checkpointed on every interaction. (`03-optimized.rs:213`–`267`.)
- **Why:** This is the Synthetix accumulator pattern itself, not a Solana implementation accident. The pattern works by checkpointing `rewardPerTokenStored` lazily, but to enable lazy evaluation later, every state-changing interaction must update `lastUpdateTime` and `rewardPerTokenStored` *before* anything else changes the totalSupply. Splitting these fields across PDAs would either break the math (the formula depends on the relationship between `rewardPerTokenStored`, `lastUpdateTime`, and `totalSupply` being a coherent snapshot) or require a different reward model entirely (epoch-based payouts, per-cohort accumulators, off-chain attestations).
- **Benefit:** Honesty. A developer porting Synthetix-style staking should know up front: per-pool throughput tops out around ~one stake/withdraw/getReward per slot (~2.5 per second). If that's a problem, the answer is a different reward design, not a different state layout. The skill's job is to make this constraint explicit, not paper over it with PDAs that don't actually help.
- **Tradeoff (the real one):** If your protocol needs higher throughput per pool, options in increasing order of work:
  1. **Run multiple pools** (the program already supports this — diff §S2). Throughput scales by pool count.
  2. **Switch to epoch-based rewards**: users claim rewards from a per-epoch snapshot account. Removes intra-epoch contention but pays out less smoothly.
  3. **Off-chain accumulator + Merkle root**: maintain `rewardPerTokenStored` off-chain, post Merkle roots on-chain periodically. Users claim with a proof. Most flexible, most engineering.

  All three are out of scope for a faithful port of Synthetix-style staking. The port should be honest about the ceiling.

### O(n) Vec scans eliminated (diff §P3)

- **What:** Deleted `iter_mut().find(|e| e.user == user)` and the `upsert_entry` helper. The user's position is reached by deterministic PDA derivation (`02-naive-port.rs:182`, `:207` deleted; `03-optimized.rs:336` instead).
- **Why:** Linear search inside an account costs CU linearly in entries *and* requires deserializing the full Vec on entry / re-serializing on exit. PDA derivation is O(1).
- **Benefit:** Constant-time lookup. No `MAX_STAKERS` cap. ~5–10k CU saved per call at modest staker counts; more at high counts.
- **Tradeoff:** None.

---

## Security

### Every arithmetic op uses `checked_*` (diff §Sec1)

- **What:** All `+`, `-`, `*`, `/`, `as` casts replaced with `checked_*` (or in the case of `as`, guarded by explicit range checks). Affected sites in the naive port: `02-naive-port.rs:81`, `:83`, `:104`, `:105`, `:184` (the accumulator math), `:191`, `:201`.
- **Why:** Rust release builds wrap arithmetic silently — opposite of Solidity 0.8+. The naive port's `dt * rate * PRECISION` is the most dangerous: for `rate = 1e15`, `dt = 1e6 seconds`, `PRECISION = 1e18`, the product is ~1e39, which silently wraps to a small number in u128. Wrapped accumulator updates mean wildly wrong rewards for everyone — a class of bug that would be silent in production until users started getting paid amounts that don't match the off-chain spreadsheet.
- **Benefit:** Overflow becomes an explicit, typed error. Silent wrap eliminated. The accumulator math is now auditable line-by-line — each intermediate value either fits or errors.
- **Tradeoff:** A few extra characters per op. The pattern is mechanical; once internalized, it adds no real friction.

### Width-narrowing cast (`u128 → u64`) now guarded (diff §Sec2)

- **What:** Replaced `earned_inc as u64` (`02-naive-port.rs:201`) with `require!(earned_inc_u128 <= u64::MAX as u128)` + cast (`03-optimized.rs:255`–`260`).
- **Why:** `as u64` truncates the high bits silently. If `earned_inc_u128` ever exceeds `u64::MAX`, the user's accrued rewards get rounded down to (value modulo 2^64). For long-staked users in high-rate pools this is not even hypothetical — it's the kind of bug that gets discovered in production after a few months.
- **Benefit:** Truncation becomes an explicit error. Off-chain reconciliation matches on-chain results.
- **Tradeoff:** A path that would silently succeed (with a wrong number) now fails (correctly). If users hit this in production, they need an admin-recovery path — but that's the right behavior.

### PDA bumps cached + canonicalization enforced (diff §Sec3)

- **What:** `VaultState.bump`, `VaultState.vault_authority_bump`, and `StakePosition.bump` are set at init via `ctx.bumps.<name>` and supplied on every subsequent access via `bump = <field>`.
- **Why / Benefit / Tradeoff:** See `examples/erc20-token/05-explanation.md` § "Cache and enforce canonical PDA bumps" and the skill's `security/pda-canonicalization.md`. Same reasoning, same gains: closes the alternate-bump-attack class, saves 1.5–2.5k CU per access.

### Clock-skew guard on the timestamp subtraction (diff §Sec4)

- **What:** `(now - vault.last_update_time) as u128` becomes a two-step check: `checked_sub` followed by `require!(dt >= 0)`. (`03-optimized.rs:220`–`223`.)
- **Why:** `Clock::get()?.unix_timestamp` is supposed to be monotonic, but cluster reconfigs and historical sysvar quirks have produced backwards clocks. If `now < last_update_time`, the bare `i64` subtraction is negative, and casting a negative `i64` to `u128` produces a value near `u128::MAX`. The accumulator then jumps by an astronomical delta — silently minting rewards out of thin air. No equivalent attack exists in Solidity because Ethereum's clock is monotonic at the protocol level.
- **Benefit:** Bad-clock states fail loud instead of corrupting state silently. The check is cheap (one branch). For the cost of two instructions you avoid an entire class of stuck-vault / phantom-mint incidents.
- **Tradeoff:** A pathologically slow validator with a clock issue would now error its callers — but that's correct, and the user retries on a healthy validator.

### Token-account authority scoped to each vault (diff §Sec5)

- **What:** `vault_authority` PDA seed now includes `vault.key()` (`03-optimized.rs:286`). Naive used a singleton `[b"vault_authority"]`.
- **Why:** Required by the multi-pool change (§S2). A singleton authority across all pools means a bug or compromise in one pool's logic could move funds in any other.
- **Benefit:** Each pool's funds are signed for by a distinct PDA. Authority isolation across pools.
- **Tradeoff:** Seed is longer (one more 32-byte component). Negligible.

### `StakePosition` is its own typed account (diff §Sec6)

- **What:** Each user's state is a separate Anchor `#[account]` with its own 8-byte discriminator. `has_one = user` enforces that `position.user` matches the signer.
- **Why:** A typed account with a discriminator cannot be type-confused with anything else. The naive Vec entry shared the vault's discriminator and could only be reached by index — which was correctness-checked by the linear scan, but a future refactor that exposes raw indices would lose that guard.
- **Benefit:** Type confusion impossible. Belt-and-suspenders for ownership: seeds derive a unique PDA per user *and* `has_one` verifies the stored user. Either check alone would suffice; both is cheap defense-in-depth.
- **Tradeoff:** Each `StakePosition` carries an 8-byte discriminator (16 bytes of rent at ~0.0007 SOL per 100 bytes — negligible).

---

## CPI & program reuse

### SPL Token CPI shape unchanged; only authority seeds differ (diff §C1)

- **What:** Both versions CPI into SPL Token for stake/withdraw/reward transfers — there is no alternative. The optimized version's authority signer seeds include the vault key.
- **Why:** The token movement primitive is SPL Token's `transfer`, regardless of how clever your state layout is. Reusing audited code; signed by a program PDA so funds can only leave through your program's logic.
- **Benefit:** No custom token-move logic to audit. Standard SPL Token semantics.
- **Tradeoff:** None — both ports use this pattern.

---

## Compute & rent

### `VaultState` shrinks from ~6.5 KB to 138 bytes (diff §R1)

- **What:** `VaultState::SIZE` drops from ~6540 bytes to 138 bytes. The 100-entry Vec moves to per-user PDAs.
- **Why:** Protocol-paid rent should be constant; user-state rent should scale with users and be paid by users. The naive layout had the protocol pre-paying for 100 users' worth of slots.
- **Benefit:** Vault init cost drops from ~0.046 SOL to ~0.0025 SOL (~18× cheaper). New pools are cheaper to launch.
- **Tradeoff:** First-time stakers pay ~0.002 SOL to create their `StakePosition`. Standard Solana UX.

### Per-call account-data load shrinks ~25× (diff §R2)

- **What:** Each instruction loads & deserializes `VaultState` (138 bytes) + `StakePosition` (97 bytes) = ~235 bytes, instead of the full `VaultState` (~6540 bytes) with its Vec.
- **Why:** Anchor deserializes account data on instruction entry. Vec deserialization is O(n) in entries.
- **Benefit:** Multiple-kCU savings per call, scaling with prior staker count. CU headroom for the accumulator math itself.
- **Tradeoff:** None.

---

## Idioms

### `update_reward` helper is pure — no account I/O, no Vec mutation (diff §I1)

- **What:** Signature changes from `update_reward(&mut VaultState, Option<Pubkey>, i64)` (with internal upsert + scan) to `update_reward(&mut VaultState, Option<&mut StakePosition>, i64)` (both account refs passed in).
- **Why:** Account I/O in helpers is opaque. The optimized signature makes every account interaction explicit at the call site, with Anchor's account validation having already proven the right accounts were passed.
- **Benefit:** Helper is auditable in isolation. Easier to unit-test (just construct two stack values).
- **Tradeoff:** Callers have to thread the position explicitly. Worth it.

### `init_if_needed` with a written safety argument (diff §I2)

- **What:** `StakePosition` uses `init_if_needed` in the `Stake` accounts struct, with a doc comment explaining why it's safe (`03-optimized.rs:340`–`348`).
- **Why:** `init_if_needed` is sometimes flagged in audits because it can mask re-init bugs. Here, the seeds are deterministic per (vault, user) — the PDA's address is fixed at first-init and the "init" path only runs when the account doesn't yet exist. New positions zero-initialize, which is the *correct* starting state for `balance`, `reward_per_token_paid`, `pending_rewards`. The first-init handler also writes the `bump` and `user` fields.
- **Benefit:** Single instruction for first stake (no separate `register` step). Better UX, no security cost given the safety argument.
- **Tradeoff:** Reviewer must verify the safety argument once. Subsequent reviewers can take it on the documented authority.

### Four typed errors added (`Overflow`, `DivByZero`, `ClockSkew`, `NotPositionOwner`) (diff §I3)

- **What:** New `#[error_code]` variants surface failure modes that the naive port either ignored or didn't have.
- **Why:** Each error code corresponds to a specific class of bug or misuse. Anchor's IDL surfaces them to clients; off-chain code can match on them.
- **Benefit:** Concrete diagnostics. A failure mode without an error variant is a failure mode that gets debugged via printf.
- **Tradeoff:** None.

### User pays own StakePosition rent (diff §I4)

- **What:** `#[account(mut)] pub user: Signer<'info>` in `Stake` (so SOL can leave the user's account to fund the rent of the StakePosition PDA created on first stake).
- **Why:** Users should pay for state that's about them. The protocol owner shouldn't be subsidizing per-user rent.
- **Benefit:** Cleaner economics. Protocol's rent burden stays constant at deployment time.
- **Tradeoff:** First-stake users see a ~0.002 SOL deduction. Standard Solana UX; wallets surface this in the tx preview.
