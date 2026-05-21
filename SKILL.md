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

## Read first

Before producing any translation, internalize the EVM → SVM mental shift in `translation/mental-model.md`. The one-line summary: *on Ethereum the contract knows where its state lives; on Solana the caller brings it.* Every translation rule below is a consequence — if a step ever feels wrong, return to that file.

## Decision tree — which sub-files to load

Default-load: `translation/mental-model.md`, `translation/type-mapping.md`, `translation/pattern-mapping.md`, `security/arithmetic.md`, `security/account-validation.md`, `security/pda-canonicalization.md`.

The default loads are non-negotiable. The mental-model file frames every other decision; arithmetic, account validation, and PDA canonicalization are the three security classes that bite *every* ported contract.

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

Each entry is exactly four fields. Keep them tight — one to four sentences each.

```
### <short title>

- **What:** the concrete change. Reference the diff section or `file:line` in the .rs files.
- **Why:** the underlying Solana property that motivates it. Connect to the Ethereum equivalent the developer is leaving behind.
- **Benefit:** what is gained. Be specific: CU saved, parallelism unlocked, security class avoided, code deleted.
- **Tradeoff:** what is given up. If nothing meaningful, say so and justify briefly.
```

Group entries under thematic headers: `## State model`, `## Parallelism`, `## Security`, `## CPI & program reuse`, `## Compute & rent`, `## Idioms`.

## Explanation style — write for a Solidity-fluent reader who has never seen Solana

The reader knows Solidity well. They know financial systems. They have **not** internalized PDAs, the account model, SPL Token, rent, CPI, or Anchor's constraint vocabulary. The explanation log is where they bridge — every entry must land for that reader.

### Rules

1. **First-use translation, always inline.** The first time any Solana-specific term appears in a given explanation log, give a short EVM analog in parentheses or em-dashes. Don't assume a glossary; weave it into the prose. After first use, the term is fair game.

   Required glossing on first use (non-exhaustive):
   - **PDA** — "PDA (Program-Derived Address — a deterministic account address derived from seeds the program controls; analog of a Solidity storage slot keyed by `(address, mapping)` — but each PDA is its own account, not a slot inside the program)"
   - **SPL Token** — "SPL Token (the shared on-chain token program every fungible token reuses on Solana — instead of each ERC-20 deploying its own contract, every token is just configuration on this one program)"
   - **CPI** — "CPI (cross-program invocation — Solana's version of one contract `call`-ing another, but every account the callee will touch must already be in the caller's transaction)"
   - **rent** — "rent (a refundable SOL deposit every account pays to live on-chain; ~0.001 SOL per KB of account data, returned in full when the account is closed)"
   - **lamports** — "lamports (1 SOL = 1e9 lamports — Solana's gwei equivalent, but at 9 decimals instead of 18)"
   - **ATA / Associated Token Account** — "ATA (Associated Token Account — the canonical per-wallet token account for a given mint, with a deterministically derivable address; the analog of \"the wallet's balance for this token\")"
   - **Mint account** — "Mint account (the on-chain configuration for a token: total supply, decimals, who can mint — owned by the SPL Token program, not by the issuer)"
   - **Signer<'info>** — "Signer (an explicit `msg.sender` — Solana requires every signing account to be declared up front in the instruction's account list, vs. Solidity's implicit `msg.sender`)"
   - **Anchor** — "Anchor (the framework on top of raw Solana programs, similar to how Hardhat relates to raw EVM — provides macros, account validation, and the IDL)"
   - **init_if_needed** — "init_if_needed (an Anchor constraint that creates the account on first call and is a no-op on subsequent calls — Solana's closest analog to Solidity's implicit `mapping[key] = value`)"
   - **close = X** — "close = X (an Anchor constraint that tears the account down and refunds its rent to X when the instruction succeeds — there's no Solidity equivalent because Solidity storage slots can't be deleted)"
   - **discriminator** — "discriminator (an 8-byte type tag Anchor prepends to every account it manages, so deserializing a `Vault` account as a `Mint` fails loudly — no EVM analog because EVM has no typed account model)"
   - **has_one** — "has_one = authority (an Anchor constraint that verifies the account's stored `authority` field equals the `authority` account passed in the same instruction — the declarative form of `require(state.authority == signer)`)"
   - **seeds + bump** — "seeds (the byte inputs the program uses to derive a PDA; the `bump` is a nonce that makes the address valid). Conceptually: `keccak256(abi.encodePacked(...))` with extra steps to keep the result off the secp256k1 curve."
   - **write-lock / parallelism** — "Solana's runtime locks every writable account a transaction touches, so two transactions that mutate different accounts run in parallel — the EVM-style global single-threaded execution is replaced by per-account locks (Sealevel)."

2. **Comparative framing in Why bullets.** Prefer "In Solidity, you would have written X because Y. On Solana, the equivalent shape is Z because W" over "the PDA stores X". The reader anchors on what they already know.

3. **Plain-English code references.** When citing a Rust line by `file:line`, briefly say what it does in EVM-flavored language. Not just `f.contributors.iter_mut().find(|c| c.who == k)` — say "scans the contributor list linearly to find the supporter's row (the on-chain analog of `contributors[k]` in Solidity, but more expensive)."

4. **Spell out the consequence chain.** The reader doesn't yet know why "one PDA per supporter" matters. Don't say "no serialization of cross-supporter activity" without first explaining that Solana serializes writes to the *same account*, so isolating writes to different accounts is what unlocks parallelism. Two sentences > one tight one if the second sentence is doing teaching work.

5. **Tradeoff is honest.** Rent, extra accounts, Anchor-specific idioms a non-Anchor reader has to learn — name them concretely. The reader is evaluating a real migration; gloss only hurts.

### Side-by-side: bad → better

The current `examples/token-fundraiser/05-explanation.md` entry for §S1 reads:

> **Why:** Solidity's `mapping(address => uint256)` is one slot per supporter inside one contract — addressable by key inside one storage tree. The Solana equivalent is one account per supporter, addressable by PDA derivation. A `Vec` inside a state account is the wrong primitive: it bounds the supporter count, forces every contribute/refund to mutate the singleton state account, and scans linearly on lookup.

A Solidity-fluent reader who's never seen Solana parses "PDA derivation" as noise. Better:

> **Why:** In Solidity, `mapping(address => uint256) contributions` puts every supporter at a deterministic storage slot inside the contract — `contributions[alice]` lands at `keccak256(alice, slot)`. The Solana equivalent is one *account* per supporter — a PDA (Program-Derived Address; same idea, but each entry is its own on-chain account at a deterministic address derived from `[b"contributor", fundraiser, alice]`, not a slot inside the program). A `Vec<Contribution>` on a single state account would imitate the Solidity layout but it loses the per-account property: Solana's runtime locks every writable account a transaction touches, so two supporters contributing at the same time would serialize on the shared state account. One PDA per supporter is what lets the two writes run in parallel.

Same content, ~2× the words, lands for the reader. **Err on the side of teaching.** Length is not a virtue; clarity for the assumed reader is. If a section already used the term, you don't need to re-explain it — the rule is "first use in this explanation log."

### Original example entry (for structure reference)

```
### Balances moved from on-chain map to SPL Token accounts

- **What:** Removed `balances: Vec<BalanceEntry>` from `TokenState` (`02-naive-port.rs:153`). Each holder now has an Associated Token Account (ATA — the canonical per-wallet token balance account, derived deterministically from `(wallet, mint)`); transfers go through `token::transfer` on the SPL Token program directly, not through this program (`03-optimized.rs` has no transfer instruction).
- **Why:** In Solidity, an ERC-20 contract holds the balance ledger itself — `mapping(address => uint256) balanceOf` inside the contract. On Solana, the SPL Token program is the shared ledger for every fungible token on the network; each holder gets their own on-chain account (an ATA) and transfers move balances between those accounts directly. Keeping a custom `Vec` would force every transfer in the system to write-lock the same state account, serializing all of them, while reimplementing mechanics SPL Token already audits.
- **Benefit:** Transfers between disjoint sender/recipient pairs run in parallel (Solana's runtime locks accounts, not the program — so writes to Alice→Bob and Carol→Dan don't block each other). ~150 lines of custom balance/allowance logic deleted. No path for a buggy custom balance update.
- **Tradeoff:** Holders create an ATA before first receipt — a one-time ~0.002 SOL rent deposit (refundable when the account is closed). Off-chain code computes ATA addresses to read balances instead of reading one contract account.
```

## Reference example

Trace the protocol on `examples/token-fundraiser/` end-to-end before producing translations of new inputs. The example exists so you can verify the protocol produces the contract.

## Ambiguities

See `DECISIONS.md` at the skill root for choices made during construction (Anchor version, SPL Token classic vs Token-2022, etc.). When in doubt on a new translation, prefer the choice consistent with the reference example unless the input forces otherwise.
