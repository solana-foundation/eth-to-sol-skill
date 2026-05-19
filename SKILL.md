---
name: eth-to-sol
description: Translate Ethereum/Solidity contracts to production-grade Solana programs in two passes (faithful port, then Solana-native refactor) and teach the developer what changed and why.
---

# eth-to-sol

Translate Ethereum/Solidity contracts to production-grade Solana programs. The goal is not a 1:1 port — it is Solana-native code plus a teaching artifact that makes every decision legible to a developer who knows Solidity well and Solana barely.

## Two-pass protocol (hard rule)

Every translation produces two outputs in sequence. Do not collapse them.

1. **Pass 1 — Faithful port.** A semantically identical Anchor program. No restructuring, no SPL CPI substitutions, no parallelism rework. It exists so the refactor's value is legible. Mark obviously un-Solana patterns with `// SMELL:` comments rather than fixing them.
2. **Pass 2 — Solana-native refactor.** Restructured for Solana primitives: SPL programs via CPI, per-entity PDAs, parallelism-friendly account layout, explicit rent/sizing, compute-budget awareness, program splitting where warranted. Production-ready.

If a contract is trivially served by an existing Solana program (e.g. a vanilla ERC-20), the optimized version will be drastically smaller than the naive port. That is the lesson.

## Output contract

For an input named `foo`, produce exactly these artifacts:

| File | Contents |
|---|---|
| `01-original.<ext>` | The input (Solidity, Vyper, etc.). Already present; do not rewrite. |
| `02-naive-port.rs` | Pass 1. Compiles. Inline `// SMELL:` markers on antipatterns. |
| `03-optimized.rs` | Pass 2. Production-ready, fully commented at non-obvious sites. |
| `04-diff.md` | Structured diff. **Group sections by theme** (State model / Parallelism / Security / CPI & program reuse / Compute & rent / Idioms) — mirror the explanation log. Each section: short header, before/after snippets, `file:line` references to the two `.rs` files. |
| `05-explanation.md` | The explanation log. One entry per change in `04-diff.md`, grouped by theme. Schema below. |

When the optimized version meaningfully changes client-side integration (typically: SPL Token replaces a custom token surface, or balance/aggregate lookups move off the program), append a `## Frontend integration` section at the bottom of `05-explanation.md` containing: before/after TypeScript using `@solana/web3.js` + `@solana/spl-token`; the list of changed call sites a porting team will touch; migration scoping. If the integration shift is minor, fold it into a relevant entry's Tradeoff instead — don't bloat the file.

`05-explanation.md` is the teaching surface. Treat it as a first-class deliverable, not a comment block.

## Decision tree — which sub-files to load

Default-load: `translation/type-mapping.md`, `translation/pattern-mapping.md`, `security/arithmetic.md`, `security/account-validation.md`, `security/pda-canonicalization.md`.

The default loads are non-negotiable. Arithmetic, account validation, and PDA canonicalization are the three security classes that bite *every* ported contract; they apply even to trivial inputs.

| Source contains | Also load |
|---|---|
| ERC-20 / fungible token | `translation/stdlib-mapping.md`, `optimization/account-model.md`, `security/cpi-safety.md` |
| ERC-20 with `_update` / `_beforeTokenTransfer` override (fee-on-transfer, blacklist, paused-transfer, rebasing) | `translation/stdlib-mapping.md` (Token-2022 section); target Token-2022 with the matching extension (transfer fee, transfer hook, default account state, interest-bearing). Do **not** target classic SPL — the semantics cannot be expressed. |
| ERC-721 / ERC-1155 / NFT | `translation/stdlib-mapping.md`, `optimization/account-model.md`, `optimization/pdas.md` |
| `mapping(...)` storage | `optimization/account-model.md`, `optimization/pdas.md`, `optimization/parallelism.md` |
| Ownable / AccessControl / roles | `translation/stdlib-mapping.md`, `security/signer-checks.md` |
| Custom modifiers | `translation/pattern-mapping.md`, `security/signer-checks.md` |
| External calls / interfaces | `security/cpi-safety.md`, `security/reentrancy.md`, `optimization/program-splitting.md` |
| Heavy arithmetic / fixed-point | `security/arithmetic.md` (also default-loaded) |
| Hot-write global state (counters, `totalSupply`) | `optimization/parallelism.md`, `optimization/account-model.md` |
| Dynamic-sized state (arrays, mappings of unknown size) | `optimization/rent-and-size.md`, `optimization/account-model.md` |
| Multi-contract system | `optimization/program-splitting.md`, `security/cpi-safety.md` |
| Anything writing state after an external call | `security/reentrancy.md`, `security/cpi-safety.md` |
| Compute-pressured paths (multi-CPI swaps, loops in hot path, >300 expected CU per call) | `optimization/compute-budget.md` |
| Multiple account types owned by the program (type-confusion risk surface) | `security/account-validation.md` (also default-loaded) |
| Any PDA the program will sign for | `security/pda-canonicalization.md` (also default-loaded), `optimization/pdas.md` |
| Protocol takes a user-supplied token Mint as configuration (vault, AMM, lending market) | `security/reentrancy.md`, `security/account-validation.md`, `security/cpi-safety.md` |
| Vault/AMM/4626-shaped protocol (share/asset conversion math, deposit + withdraw + redeem semantics) | `security/arithmetic.md` (rounding-direction discipline), `optimization/account-model.md` (read aggregates from SPL Token), `optimization/parallelism.md` (read-only vault pattern) |
| Time-delta math (`now - last_update`, accumulator periods) | `security/arithmetic.md` (clock-skew + negative-delta-cast pitfall) |

Always load every security/* file relevant to the constructs present. Security is non-negotiable.

## Pre-flight checklist (gate on the optimized version)

Every item must hold before emitting `03-optimized.rs`. If one fails, fix and re-check.

- [ ] Every arithmetic op is `checked_*` — or has an inline justification for `saturating_*` / `wrapping_*`. No bare `+ - * /` on user-controlled values.
- [ ] Every `Account<'info, T>` either uses Anchor's typed checks or includes explicit owner + discriminator validation. No `AccountInfo` smuggled through without checks.
- [ ] Every signer-required path uses `Signer<'info>` or a manual `is_signer` check. No "the front end won't call it without a signer" reasoning.
- [ ] Every PDA derivation either uses `seeds = [...], bump = stored_bump` (preferred — saves ~1.5k CU per call) or bare `seeds = [...], bump,` (acceptable when CU is not pressured; both forms enforce canonicalization via Anchor's `find_program_address` check). The cached form is strongly preferred — all reference examples use it. Do **not** use `bump = <user_input>` — that's the actual canonicalization vulnerability.
- [ ] CPIs use `CpiContext::new` or `CpiContext::new_with_signer`. The program arg is a `Pubkey` (use `ctx.accounts.<program>.key()`) — Anchor 1.0+ removed the `AccountInfo` form. No hand-rolled `invoke`/`invoke_signed` with manually assembled `AccountInfo` arrays unless raw Solana is justified.
- [ ] No path mutates state after a CPI to an untrusted program without re-reading and re-validating. (See `security/reentrancy.md` for why account locking is necessary but not sufficient.)
- [ ] Account sizing is explicit: `space = 8 + <sum>`; the 8 is the Anchor discriminator. Variable-size fields have hard caps.
- [ ] Errors use `#[error_code]`. No `ProgramError::Custom(n)` literals, no `msg!`-then-fail.
- [ ] No PDA shares a write lock with high-frequency unrelated state. Per-entity PDAs over global counters where the protocol allows.
- [ ] If the contract emits events that SPL programs already emit (Transfer for SPL Token), prefer not duplicating them.
- [ ] Re-init protection: PDAs that should only init once use `init` (not `init_if_needed`) and have unique seeds.

## Explanation log schema

Each entry is exactly four fields. Keep them tight — one to three sentences each.

```
### <short title>

- **What:** the concrete change. Reference the diff section or `file:line` in the .rs files.
- **Why:** the underlying Solana property that motivates it. Connect to the Ethereum equivalent the developer is leaving behind.
- **Benefit:** what is gained. Be specific: CU saved, parallelism unlocked, security class avoided, code deleted.
- **Tradeoff:** what is given up. If nothing meaningful, say so and justify briefly.
```

Group entries under thematic headers: `## State model`, `## Parallelism`, `## Security`, `## CPI & program reuse`, `## Compute & rent`, `## Idioms`.

Example entry:

```
### Balances moved from on-chain map to SPL Token accounts

- **What:** Removed `balances: Vec<BalanceEntry>` from `TokenState` (`02-naive-port.rs:153`). Each holder now has an Associated Token Account owned by SPL Token; transfers go through `token::transfer` directly, not through this program (`03-optimized.rs` has no transfer instruction).
- **Why:** Solana state is per-account, not per-contract. A `Vec` inside one account forces every transfer in the system to write-lock that account, serializing all transfers. SPL Token already implements correct, audited fungible-token mechanics with per-account storage.
- **Benefit:** Transfers between disjoint sender/recipient pairs parallelize. ~150 lines of custom balance/allowance logic deleted. No path for a buggy balance update.
- **Tradeoff:** Holders create an ATA before first receipt (one-time ~0.002 SOL rent). Off-chain code computes ATAs to read balances rather than reading one contract account.
```

## Reference example

Trace the protocol on `examples/erc20-token/` end-to-end before producing translations of new inputs. The example exists so you can verify the protocol produces the contract.

## Ambiguities

See `DECISIONS.md` at the skill root for choices made during construction (Anchor version, SPL Token classic vs Token-2022, etc.). When in doubt on a new translation, prefer the choice consistent with the reference example unless the input forces otherwise.
