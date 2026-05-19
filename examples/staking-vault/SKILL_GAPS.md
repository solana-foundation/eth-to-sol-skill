# Skill gaps surfaced by the staking-vault example

Self-review notes. Each entry: what was missing, where it should live, and the rough shape of the addition. Ordered by priority.

The skill carried this example correctly — none of the gaps were blockers. But several patterns came up that the next example in this lane (ERC-4626 vault, on-chain governance, etc.) will hit again, and the skill should grow to cover them before that happens.

---

## P0 — Real gaps the skill didn't have

### 1. Synthetix-style accumulator patterns are fundamentally write-hot

**Where it should live:** new section in `optimization/parallelism.md`, near the existing "Hot-write fields are an antipattern" section.

**What's missing:** the existing guidance is "drop the global counter, or shard it." That works for cosmetic counters (`totalSupply` on an ERC-20). It does *not* work for accumulators like `rewardPerTokenStored + lastUpdateTime` where the *protocol's correctness* depends on a single coherent checkpoint updated on every interaction. The optimization is to remove *additional* contention from the user-state Vec, while being honest that the accumulator itself is the floor.

**Proposed addition (~30 lines):**

> ### Accumulator patterns — when the contention is the protocol
>
> Some DeFi designs have global state that must be checkpointed on every interaction by design: Synthetix-style reward accumulators (`rewardPerTokenStored`, `lastUpdateTime`), Curve/Convex emission models, ve-token decay curves. These share a structural property: the formula extrapolates from a recent checkpoint, and every state-changing call must refresh that checkpoint *before* its own mutation.
>
> The optimization is not to remove the contention but to bound it:
>
> 1. **Keep accumulator fields on a single small `Vault` PDA** — small means the per-call deserialization cost is low. Don't bury the accumulator inside a large account that also holds user state.
> 2. **Per-user state in separate PDAs** — eliminates the cross-user Vec contention, leaves only the accumulator-write contention.
> 3. **Be honest in docs** — the per-pool throughput ceiling is ~1 op per slot (~2.5/sec). Tell the deploying team. If they need more, the solution is more pools, an epoch-based reward model, or off-chain accumulators with Merkle proofs.
>
> The skill's job is to express the design truthfully, not to paper over the constraint with PDAs that don't actually help.

Reference the staking-vault example's `04-diff.md §P2` and `05-explanation.md §P2`.

---

### 2. Clock-skew defense on timestamp deltas

**Where it should live:** `security/arithmetic.md`, near the existing "Signed/unsigned mismatches" section.

**What's missing:** the existing example uses `now.checked_sub(last_update)` but doesn't call out the cast-to-unsigned bug that comes up *in practice* on Solana. The staking-vault example hit this; future examples that involve any time-delta math will hit it again.

**Proposed addition (~15 lines):**

> ### Negative time-delta → unsigned-cast pitfall
>
> A common pattern in DeFi math is `dt = now - last_update_time`, cast to `u128` for use in a multiplication. The bug:
>
> ```rust
> let dt = (now - vault.last_update_time) as u128; // SMELL
> ```
>
> If `now < last_update_time` (rare but happens on cluster reconfigs and historical sysvar quirks), the bare `i64` subtraction is negative; `as u128` keeps the bit pattern, producing a value near `u128::MAX`. The downstream multiplication overflows, *or* the value is used directly as a duration and you've just credited 10^20 seconds of rewards.
>
> Defense — two-layer:
>
> ```rust
> let dt: i64 = now.checked_sub(vault.last_update_time)
>     .ok_or(MyError::ClockSkew)?;
> require!(dt >= 0, MyError::ClockSkew);
> let dt_u128 = dt as u128; // safe — known non-negative
> ```
>
> Cheap; eliminates an entire class of silent reward-minting bugs.

---

### 3. Parameterized PDAs for multi-instance protocols

**Where it should live:** new short section in `optimization/pdas.md`, after "Seed design."

**What's missing:** the skill covers "singleton config" and "per-entity PDA" but not the *between* case: a parameterized vault / pool / market PDA where the seed includes a configuration choice (a mint pair, an oracle, a tier).

**Proposed addition (~15 lines):**

> ### Parameterized PDAs (multi-instance protocols)
>
> Solidity contracts are often deployed once per market / pool / vault. On Solana the canonical pattern is one program with parameterized PDAs:
>
> ```rust
> seeds = [b"vault", staking_mint.as_ref(), rewards_mint.as_ref()]
> seeds = [b"pool", base_mint.as_ref(), quote_mint.as_ref(), fee_tier.to_le_bytes().as_ref()]
> seeds = [b"market", oracle.as_ref()]
> ```
>
> Same code path serves N instances. Each new instance costs only one PDA + its supporting accounts (~0.005 SOL), not a fresh program deploy (~2 SOL). Governance scope is "the program," not "each pool individually."
>
> Pair with per-instance `vault_authority` PDAs (`seeds = [b"<auth>", <instance-key>.as_ref()]`) so each instance's funds are signed by a distinct PDA — isolates blast radius across instances.

---

### 4. Two-authority separation for protocols holding pool funds

**Where it should live:** new section in `security/signer-checks.md`, between "Manual signer checks" and "PDA signers."

**What's missing:** the skill covers "use Signer<'info> + has_one" for governance gating and "use a PDA for the SPL Mint authority" for token issuance. It doesn't explicitly cover the case where a protocol holds *pool* funds (staking pools, AMM reserves, lending markets) and needs both:

- a **governance authority** (human/multisig key, rotatable, gates admin actions like rate changes)
- a **vault authority PDA** (program-controlled, signs token transfers out of the pool)

**Proposed addition (~20 lines):**

> ### Two-authority separation for fund-holding protocols
>
> A staking pool, AMM, or lending market holds user-deposited tokens. The program needs two distinct authorities:
>
> | Authority | Type | Rotatable | Purpose |
> |---|---|---|---|
> | `authority` (governance) | `Pubkey` field on `Config` PDA | yes — by current authority | gates admin actions: change rates, pause, fund rewards |
> | `vault_authority` | program PDA (no off-chain key) | no | signs SPL Token CPIs that move user funds out of the pool |
>
> Keep them separate. Compromising `authority` should let an attacker change rates but not drain the pool — funds can only leave via the program's typed instructions, which are signed by the PDA (not the authority).
>
> Stored on `Config`:
> ```rust
> pub authority: Pubkey,
> pub vault_authority_bump: u8,
> ```
> The vault_authority PDA itself has no stored field — it's the program-derived identity, validated via `seeds = [b"vault_authority", <scope-key>.as_ref()], bump = config.vault_authority_bump`.

---

## P1 — Refinements

### 5. `init_if_needed` safety pattern + documentation convention

**Where it should live:** `translation/pattern-mapping.md`, near "Re-init protection."

**What's missing:** the skill says "do not use `init_if_needed` on an init-once account" but doesn't address the case where `init_if_needed` is *correct*: per-user state where seeds are deterministic, fields zero-init to the correct starting state, and first-init writes immutable fields like the bump.

**Proposed addition (~15 lines):**

> ### When `init_if_needed` is the right answer
>
> `init_if_needed` is correct, with a written safety argument, when:
>
> 1. PDA seeds are deterministic and uniquely identify the entity (e.g. `[b"position", vault.key().as_ref(), user.key().as_ref()]`).
> 2. The struct's zero-initialized state is the correct starting state (e.g. `balance: 0`, `pending_rewards: 0`).
> 3. The handler writes immutable fields (bump, owner, parent reference) on first init, and idempotent updates thereafter.
>
> Document the safety argument inline as a doc comment on the constraint. Auditors will look for it; without it, the use looks like a re-init footgun.

---

### 6. Width-narrowing casts as a checklist item

**Where it should live:** `security/arithmetic.md`, expanding the "Mixed-width arithmetic" section.

**What's missing:** the existing example shows widening for intermediate calculation (`as u128`), then narrowing back (`as u64`) — and mentions guarding the narrowing, but the guard is one line of prose. Should be a hard checklist item: "every `as <smaller-int>` cast must be preceded by a range check or follow a `try_into()` with error mapping."

**Proposed addition:** strengthen the existing prose to a sentence in the per-file checklist at the top of `security/arithmetic.md`: *"every `as <smaller-int>` cast on user-derived data must be preceded by a range check or use `try_into()` with error mapping."*

---

### 7. Pure helpers as a convention

**Where it should live:** `translation/pattern-mapping.md`, new short section.

**What's missing:** Anchor encourages thinking in terms of accounts (validated, deserialized, mutated via `ctx`). It's easy to write helper functions that take `ctx` directly or do their own account I/O. The cleaner convention — for auditability and testing — is helpers that take primitive references (`&mut VaultState`, `Option<&mut StakePosition>`, `i64 now`) and do no I/O.

**Proposed addition (~10 lines):**

> ### Pure helpers over `ctx`-passing helpers
>
> Avoid:
>
> ```rust
> fn update_reward(ctx: &mut Context<Stake>, user: Pubkey) -> Result<()> {
>     let vault = &mut ctx.accounts.vault;
>     // ... opaque account fetches inside the helper
> }
> ```
>
> Prefer:
>
> ```rust
> fn update_reward(
>     vault: &mut VaultState,
>     position: Option<&mut StakePosition>,
>     now: i64,
> ) -> Result<()>
> ```
>
> The pure form is auditable in isolation, unit-testable on stack values, and forces the caller to make the account dependencies explicit. The cost is one explicit parameter at each call site.

---

## P2 — Doc-clarity nits (from dogfood + this session)

### 8. The output-contract table should mention `01-original.<ext>` is the input

`SKILL.md` lists `02-naive-port.rs` ... `05-explanation.md` but never explicitly says "01 is the source." Took the dogfood agent (and a careful reader) a moment. Two-line fix.

### 9. `04-diff.md` schema should say "group by theme"

The output contract says "structured diff" but doesn't pin down grouping. Both the ERC-20 example and this staking example group thematically (mirroring `05-explanation.md`). Make this an explicit instruction.

### 10. `05-explanation.md` themes should be defined, not just listed

The themes (State model / Parallelism / Security / CPI & program reuse / Compute & rent / Idioms) are listed in SKILL.md's explanation log schema but their boundaries aren't defined. Some entries straddle (e.g. cached bumps is both security and compute). The current convention — "pick the primary theme, mention the other in the entry's body" — works but should be stated.

### 11. Two-step ownership threshold

`translation/stdlib-mapping.md` says `Ownable2Step` is "worth doing for high-stakes contracts" without defining the threshold. The dogfood agent (independently) reached "high stakes ≥ controls a Mint authority or token pool" and chose two-step. Codifying that rule of thumb would prevent inconsistency between examples.

---

## Items I confirmed the skill *does* cover well

(For balance — these were tested by this example and didn't need extension.)

- Two-pass protocol (faithful → optimized) was directly usable.
- Decision tree triggered the right sub-files: `parallelism.md`, `account-model.md`, `pdas.md`, `arithmetic.md`, `signer-checks.md`, `cpi-safety.md` all loaded and were on-target.
- `optimization/account-model.md`'s "PDA per key" guidance was the load-bearing insight for `StakePosition`.
- `security/pda-canonicalization.md` was already correct on the bump-cache pattern; I followed it verbatim.
- The output contract (four artifacts) plus the explanation log schema (What/Why/Benefit/Tradeoff) carried the example end-to-end with no ambiguity at the structural level.
- `04-diff.md` and `05-explanation.md` mirroring each other's section structure worked well as a navigation pattern.

No edits proposed for those files.
