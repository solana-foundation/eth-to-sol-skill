# Changelog

## Session: 4626 vault example + skill revision pass

### Files added

- `examples/erc4626-vault/01-original.sol` — ERC-4626 vault with virtual-offset inflation defense, owner-gated fee on yield, abstract `_earn()` hook.
- `examples/erc4626-vault/02-naive-port.rs` — Faithful Anchor port. Monolithic `VaultState` with `Vec<BalanceEntry>`, manual `total_assets`/`total_supply` tracking, bare-arithmetic conversions, no delegate-withdraw path. 24 `// SMELL:` markers.
- `examples/erc4626-vault/03-optimized.rs` — Solana-native refactor. Shares as SPL Token Mint; aggregates read from SPL; vault read-only on user operations; `mul_div_u128_to_u64` helper with explicit `Rounding` enum; SPL Token delegate for withdraw-on-behalf; classic SPL Token only (Token-2022 rejection at type level).
- `examples/erc4626-vault/04-diff.md` — 15 themed sections with line-anchored before/after snippets.
- `examples/erc4626-vault/05-explanation.md` — 20 entries grouped by theme + dedicated `Frontend integration` section with TypeScript before/after.
- `examples/erc4626-vault/AUDIT.md` — Full self-audit (checks A–J). 1 WEAK finding (D.3 — `asset_reserve` PDA bump uncached) caught and fixed; rebuilt clean. Triage of 18 SKILL_GAPS items.
- `examples/erc4626-vault/SKILL_GAPS.md` — 7 new gaps proposed with concrete sub-file additions. Triage references AUDIT §J.

### Files modified

- `SKILL.md` —
  - Output contract: added `01-original.<ext>` row (input artifact).
  - Output contract: `04-diff.md` schema now explicitly says "group sections by theme."
  - Output contract: `05-explanation.md` schema gains explicit `Frontend integration` section convention with trigger criteria.
  - Decision tree: added 3 new rows — "Protocol takes a user-supplied token Mint" → reentrancy/cpi-safety/account-validation; "Vault/AMM/4626-shaped protocol" → arithmetic/account-model/parallelism; "Time-delta math" → arithmetic.
  - Pre-flight checklist: clarified the PDA-bump item. Both cached `bump = stored` and bare `bump,` enforce canonicalization; the difference is purely CU. The cached form is the preferred default. `bump = <user_input>` is the actual security smell.

- `optimization/account-model.md` — Added "Read aggregates from SPL Token; don't self-track" section after the hot-write antipattern discussion. Tables the equivalences (`totalSupply` → `Mint.supply`, `balanceOf(self)` → `TokenAccount.amount`); explains why self-tracking forces vault writes and how reading from SPL unlocks the read-only-vault pattern.

- `optimization/parallelism.md` — Two new sections.
  - "Accumulator patterns — when the contention is the protocol" — Synthetix-style staking, Curve/Convex emission, ve-decay. Explains the per-pool-throughput ceiling and three honest mitigations (more pools, epoch-based, off-chain Merkle).
  - "When the governance PDA can be read-only" — the 4626 vault pattern. Conditions, contention floor, code example, cross-references.

- `optimization/pdas.md` — Added "Parameterized PDAs for multi-instance protocols" after seed-design. Covers the per-asset / per-pair / per-fee-tier patterns, cost comparison (parameterized PDA vs one contract per pool), per-instance authority isolation, and the program-wide upgrade-risk consideration.

- `security/arithmetic.md` —
  - Added "Negative time-delta → unsigned-cast pitfall" — Solana-specific bug (i64 sub → cast u128 inflates near MAX). Two-layer defense.
  - Added "Width-narrowing casts (`u128 → u64`) must be bounds-checked" as a discrete checklist-grade rule with examples.
  - Replaced the brief "Rounding direction" subsection with a full pattern: explicit `Rounding` enum, single `mul_div` helper, per-call-site direction. ERC-4626 spec table included.

- `security/signer-checks.md` — Added "Two-authority separation for fund-holding protocols." Tabulates governance authority (rotatable Pubkey field) vs vault_authority PDA (intrinsic, never rotated). Explains compromise-isolation property. Examples referenced.

- `security/account-validation.md` — Added "Token-2022 rejection via typed `Mint` / `TokenAccount`" inside the "What Anchor checks by default" section. Documents the cheapest defense against transfer-hook reentrancy and the escape hatch (`token_interface` + explicit extension allowlist).

- `DECISIONS.md` — Expanded the "Reentrancy" section with Token-2022 transfer hooks. New "Vault-shaped protocols accepting arbitrary token Mints" section documenting the classic-SPL-Token-only default and the migration path for accepting Token-2022.

### Open gaps deferred to future sessions

From `examples/staking-vault/SKILL_GAPS.md` and `examples/erc4626-vault/SKILL_GAPS.md`:

- **#5: `init_if_needed` safety pattern + documentation convention.** Only the staking-vault example uses it. Defer until a second example does, at which point the pattern can be extracted with confidence.
- **#7: Pure helpers over `ctx`-passing helpers.** Stylistic; useful but not load-bearing for security or correctness. Worth a paragraph eventually.
- **#10: `05-explanation.md` themes should be defined, not just listed.** The current six themes are working consistently across all three examples (audit check I.4 passed). Definitions would be process-creep without observed pain.
- **#11: Two-step ownership threshold ("high-stakes" definition).** Real ambiguity, but examples currently use one-step. No active inconsistency to resolve.

These are listed in `AUDIT.md §J` as DEFER with justifications.

### Honest self-assessment

> **If I ran the ERC-20 / staking / 4626 examples again from scratch with the revised skill, here's what would differ:**

The substantive outputs would be largely the same — the existing examples already use the patterns the skill now documents (read-aggregates-from-SPL-Token, Rounding-enum, two-authority separation, parameterized PDAs, Token-2022 rejection, accumulator contention ceiling). What would differ:

1. **A re-run on the 4626 vault would converge faster** — the agent wouldn't have to derive the `mul_div + Rounding` pattern or the "vault read-only" insight from first principles. It's now stock skill content.
2. **Staking-vault would likely gain a `Frontend integration` section** — the convention is now codified, and the staking flows are different enough from a Solidity dApp's `stake()`/`getReward()` calls to deserve the section.
3. **The audit caught one concrete bug in the 4626 example** (`asset_reserve_bump` not cached) which is now fixed. A re-run with the revised skill, taking the same pre-flight rule literally, would not have produced that bug.
4. **The dogfood test in `examples/staking-vault/SKILL_GAPS.md` reported that a fresh agent would converge to the same architecture as the reference**; that's already empirically true. The revised skill makes the convergence less work, not different.

A re-run would not produce structurally different programs. It would produce the same programs with less mid-task invention.
