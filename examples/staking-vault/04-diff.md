# Structured diff: 02-naive-port.rs → 03-optimized.rs

Each section names one meaningful change. Snippets are abridged; line references point at the canonical site of each change.

---

## State model

### S1. `Vec<UserStakeEntry>` → per-user `StakePosition` PDA

Naive (`02-naive-port.rs:336`–`349`, `:345`):

```rust
#[account]
pub struct VaultState {
    // ...accumulator + totals...
    pub entries: Vec<UserStakeEntry>, // SMELL: linear scan, write-hot
}
```

Optimized (`03-optimized.rs:478`–`488`):

```rust
#[account]
pub struct StakePosition {
    pub vault: Pubkey,
    pub user: Pubkey,
    pub balance: u64,
    pub reward_per_token_paid: u128,
    pub pending_rewards: u64,
    pub bump: u8,
}
// seeds = [b"position", vault.key().as_ref(), user.key().as_ref()]
```

One PDA per (vault, user). Solidity's three mappings (`balanceOf`, `userRewardPerTokenPaid`, `rewards`) consolidate into one struct because they're always read/written together — but the per-key storage moves out of the central `VaultState`.

---

### S2. Singleton vault → per-pair vault

Naive (`02-naive-port.rs:228`):

```rust
seeds = [b"vault"]   // SMELL: one vault per program deployment
```

Optimized (`03-optimized.rs:277`):

```rust
seeds = [b"vault", staking_mint.key().as_ref(), rewards_mint.key().as_ref()]
```

The same program can now run multiple staking pools for different (staking_mint, rewards_mint) pairs. Solidity's contract-per-pool model becomes program-with-many-pool-PDAs.

---

### S3. `owner: Pubkey` → `authority: Pubkey` with `has_one`

Naive (`02-naive-port.rs:339`): the field is `owner`, gated in-handler via `require_keys_eq!`.

Optimized (`03-optimized.rs:464`): the field is `authority`, gated declaratively via `has_one = authority` (`03-optimized.rs:451`):

```rust
#[account(mut, ..., has_one = authority)]
pub vault: Account<'info, VaultState>,
pub authority: Signer<'info>,
```

Same pattern as the ERC-20 example. See `examples/erc20-token/04-diff.md` §Sec3 for full context.

---

## Parallelism

### P1. Per-user writes are now disjoint

Naive — every state-changing call writes the singleton `vault` account (`02-naive-port.rs:271`):

```rust
#[account(mut, seeds = [b"vault"], bump)]  // SMELL: single write-lock for the program
pub vault: Account<'info, VaultState>,
```

The Vec lives inside this account, so even between *different* users, every `stake`/`withdraw`/`get_reward` serializes.

Optimized: each call writes `vault` *and* the user's own `StakePosition` (`03-optimized.rs:336`). The `position` account is a per-user PDA — disjoint between users.

But — see P2.

---

### P2. HONEST LIMIT — VaultState is still write-hot by design

Optimized (`03-optimized.rs:213`–`267`, the `update_reward` helper, called from every state-changing instruction):

```rust
vault.reward_per_token_stored = vault.reward_per_token_stored
    .checked_add(delta)
    .ok_or(StakeError::Overflow)?;
vault.last_update_time = now;
// ...
vault.total_supply = vault.total_supply.checked_add(amount)?;
```

Every `stake`, `withdraw`, `get_reward`, and `set_reward_rate` must update the global accumulator (`reward_per_token_stored`, `last_update_time`) and the global `total_supply`. These live on `VaultState`. So `VaultState` is writable on every state-changing instruction in the program.

**This is not an implementation defect — it's the Synthetix accumulator design.** The whole point of the pattern is that rewardPerToken is checkpointed lazily *but on every interaction*, so future computations can extrapolate from a single recent checkpoint. Splitting the accumulator across PDAs would require redesigning the reward distribution model (e.g. epoch-based payouts, per-cohort accumulators).

What the optimized version *does* fix: the additional contention from the balance map. Naive: vault write-lock + Vec scan + full re-serialize. Optimized: vault write-lock (small account, O(1) update) + per-user PDA write. The vault contention is fundamental; the Vec contention was avoidable.

---

### P3. O(n) Vec scans eliminated

Naive (`02-naive-port.rs:182`, `:207`):

```rust
vault.entries.iter_mut().find(|e| e.user == user)
```

Every state-changing call does this *twice* (once for the user's own update, once for `upsert_entry`). Costs CU linearly in stakers; even at `MAX_STAKERS = 100` it's measurable.

Optimized: position PDA is looked up by deterministic seeds (`03-optimized.rs:336`). O(1) by design.

---

## Security

### Sec1. Unchecked arithmetic → `checked_*` everywhere

Naive — SMELL markers at `02-naive-port.rs:81`, `:83`, `:104`, `:105`, `:184`, `:191`, `:201`. Example (`:184`–`:191`):

```rust
let dt = (now - vault.last_update_time) as u128;  // bare cast + sub
let numerator = dt * (vault.reward_rate as u128) * PRECISION;  // bare mul, overflows for large rate/dt
let delta = numerator / (vault.total_supply as u128);          // bare div, panics on 0
vault.reward_per_token_stored += delta;                         // bare add
```

Optimized (`03-optimized.rs:220`–`235`):

```rust
let dt: i64 = now.checked_sub(vault.last_update_time).ok_or(StakeError::ClockSkew)?;
require!(dt >= 0, StakeError::ClockSkew);
let dt_u128 = dt as u128;
// ...
let numerator = dt_u128
    .checked_mul(vault.reward_rate as u128).ok_or(StakeError::Overflow)?
    .checked_mul(PRECISION).ok_or(StakeError::Overflow)?;
let delta = numerator.checked_div(vault.total_supply as u128).ok_or(StakeError::DivByZero)?;
vault.reward_per_token_stored = vault.reward_per_token_stored
    .checked_add(delta).ok_or(StakeError::Overflow)?;
```

Plus explicit re-narrow guards when `u128 → u64` (see Sec2).

---

### Sec2. Silent `as u64` truncation → explicit width guard

Naive (`02-naive-port.rs:201`):

```rust
entry.rewards += earned_inc as u64; // SMELL: cast can truncate
```

`earned_inc` is `u128`; the `as u64` silently keeps only the low 64 bits. For sufficiently large reward emissions this rounds the user's reward to a wrong (smaller) number with no error.

Optimized (`03-optimized.rs:255`–`260`):

```rust
require!(earned_inc_u128 <= u64::MAX as u128, StakeError::Overflow);
let earned_inc = earned_inc_u128 as u64;
```

The downcast is preceded by a check that turns the truncation case into an error.

---

### Sec3. PDA bumps cached + canonicalization enforced

Naive — bare `bump` on every PDA constraint (`02-naive-port.rs:271`, `:292`, `:311`, `:227` etc.):

```rust
#[account(mut, seeds = [b"vault"], bump)]   // re-derives every call
```

Optimized — bump cached at init, supplied on every subsequent access (`03-optimized.rs:467`, `468` store; `:329`, `:367`, `:408`, `:449` enforce):

```rust
pub bump: u8,
pub vault_authority_bump: u8,
// later:
#[account(mut, seeds = [b"vault", ...], bump = vault.bump, has_one = staking_mint)]
```

Same reasoning as `examples/erc20-token` §Sec2: closes the alternate-bump-attack class and saves ~1.5–2.5k CU per PDA access.

---

### Sec4. Clock-skew defense

Naive (`02-naive-port.rs:185`):

```rust
let dt = (now - vault.last_update_time) as u128;
```

If a validator's clock somehow advances backwards (shouldn't happen, but cluster reconfigs and sysvar quirks have produced this in practice), `now - last_update_time` is a negative `i64` that becomes a huge `u128` on cast — minting astronomical phantom rewards.

Optimized (`03-optimized.rs:220`–`223`):

```rust
let dt: i64 = now.checked_sub(vault.last_update_time).ok_or(StakeError::ClockSkew)?;
require!(dt >= 0, StakeError::ClockSkew);
let dt_u128 = dt as u128;
```

Two-layer defense: `checked_sub` catches the i64 underflow path, `require!(dt >= 0)` catches the normal-subtraction-of-larger-from-smaller path. The cast to `u128` then runs on a known-non-negative `i64`.

---

### Sec5. Token-account authority separated from governance authority

Naive (`02-naive-port.rs:230`, `:295`, `:314`): all signing for vault token transfers comes from a `vault_authority` PDA seeded by `[b"vault_authority"]` — singleton, not tied to a specific vault. The governance `owner` is a separate field, but never used for token-account authority.

Optimized (`03-optimized.rs:286`, `:333`, `:374`, `:415`): `vault_authority` seeded by `[b"vault_authority", vault.key().as_ref()]` — one authority per vault. Plus the bump is cached on the `VaultState`.

This is mostly a multi-pool concern (S2 forced it), but it also separates the two authority types more clearly: `authority` is governance (rotateable, gates rate changes), `vault_authority` is the program's PDA for moving pool funds (not rotateable; only the program can act through it).

---

### Sec6. StakePosition tied to (vault, user) — type-confusion-proof

Naive: per-user state is a flat entry inside `entries`. No way to confuse one user's entry for another's at the *type* level — the discriminator is on `VaultState`, not the entries.

Optimized: each `StakePosition` is its own typed account with its own discriminator (Anchor 8-byte prefix). Plus `has_one = user` (`03-optimized.rs:332`, `:373`, `:414`) verifies the position's `user` field matches the signer.

This is harder to exploit than the naive version (the Vec is bounded by `vault.entries.len()` and a Vec attack would need to corrupt the vault account, which is owner-checked) but eliminates an entire attack surface: an attacker can't pass an arbitrary account as a `StakePosition` because the discriminator check would fail.

---

## CPI & program reuse

### C1. SPL Token CPI for actual token movement — unchanged in shape

Both versions CPI into SPL Token's `transfer` for stake/withdraw/reward payouts. There is no Solana primitive to "move SPL tokens" other than CPI to SPL Token.

What changes between versions is the **authority seeds**:

Naive (`02-naive-port.rs:113`, `:148`):

```rust
let signer_seeds: &[&[u8]] = &[b"vault_authority", &[bump]];
```

Optimized (`03-optimized.rs:138`, `:184`):

```rust
let signer_seeds: &[&[u8]] = &[b"vault_authority", vault_key.as_ref(), &[vault_authority_bump]];
```

The `vault.key().as_ref()` in the seed scopes the authority to *this* vault — supporting multi-pool.

---

## Compute & rent

### R1. `VaultState` shrinks from ~6.5 KB to 138 bytes

Naive (`02-naive-port.rs:348`–`356`): `VaultState::SIZE = 32×3 + 8 + 8 + 16 + 8 + 4 + 100×64 = 6540 bytes` (plus 8-byte discriminator). Rent: ~0.046 SOL, paid by `owner` at init.

Optimized (`03-optimized.rs:474`): `VaultState::SIZE = 138 bytes`. Rent: ~0.0025 SOL. The Vec moves to per-user PDAs — each `StakePosition` is 97 bytes (~0.0019 SOL), paid by *that user* at first stake. The protocol's rent burden is constant; user-level rent scales linearly with users (which is also what they should pay for).

---

### R2. Per-call account-data load shrinks dramatically

Naive: every state-changing instruction loads the full ~6.5 KB `VaultState` (plus deserializing the Vec). The deserialization step itself is O(n) in the number of entries.

Optimized: every call loads 138 bytes (VaultState) + 97 bytes (StakePosition). Combined ~235 bytes. ~25× less data deserialized per call. The CU savings show up most on hot calls (`stake`/`withdraw`).

---

## Idioms

### I1. `update_reward` is a pure helper, not a Solidity-style modifier

Naive (`02-naive-port.rs:182`–`205`): `update_reward(&mut VaultState, Option<Pubkey>, i64)`. Performs a linear scan and a Vec upsert internally.

Optimized (`03-optimized.rs:213`–`267`): `update_reward(&mut VaultState, Option<&mut StakePosition>, i64)`. No internal account I/O; both account references are passed in.

The optimized signature is auditable in isolation — no hidden account lookups, no allocation, no Vec traversal. The caller is responsible for supplying the right `StakePosition`; Anchor's account validation ensures it's the right one.

---

### I2. `init_if_needed` for StakePosition, with a documented re-init safety argument

Optimized (`03-optimized.rs:340`–`348`):

```rust
/// init_if_needed is safe here: seeds make the PDA unique per (vault,user);
/// re-init only flows back into the same in-memory `position` for the same
/// user. New positions land with zeroed fields, which is the correct
/// starting state. The handler caches `bump` on first init.
#[account(init_if_needed, payer = user, ..., seeds = [b"position", ...], bump)]
pub position: Account<'info, StakePosition>,
```

`init_if_needed` is flagged by some auditors as a re-init footgun. The documented argument — seeds are deterministic per user, fields start zeroed which is the right initial state, first-init writes the bump — makes the usage defensible. The alternative (separate `register` + `stake` instructions) doubles the user-facing instruction count for no security gain in this specific pattern.

---

### I3. Two new typed errors: `Overflow`, `DivByZero`, `ClockSkew`, `NotPositionOwner`

Naive (`02-naive-port.rs:398`–`408`): `ZeroAmount`, `InsufficientBalance`, `NotOwner`, `TooManyStakers`.

Optimized (`03-optimized.rs:521`–`535`): `ZeroAmount`, `InsufficientBalance`, `Overflow`, `DivByZero`, `ClockSkew`, `NotPositionOwner`. New errors surface failure modes that the naive port either ignored (overflow → silent wrap) or didn't have (clock skew, position ownership). `TooManyStakers` is gone — no Vec, no cap.

---

### I4. Account `mut` on user payer for StakePosition rent

Naive: the user is `Signer<'info>` but not `mut`. Works because the user doesn't pay rent (their balance lives inside the singleton vault).

Optimized (`03-optimized.rs:357`): `#[account(mut)] pub user: Signer<'info>` — the user pays for their own StakePosition rent. Mirrors who *should* be paying.
