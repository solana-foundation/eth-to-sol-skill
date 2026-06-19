# Audit: examples/erc4626-vault/

I am the auditor. One section per check. Findings tagged **PASS / WEAK / FAIL**.

---

## A. Compilation

**Tag: PASS**

Built both programs with Anchor 1.0.2 against the existing workspace (`/tmp/eth-to-sol-test`).

Build commands and tails (build logs saved to `/tmp/vault-{naive,native}-build.log`):

```
$ anchor build -p vault-naive
    Finished `release` profile [optimized] target(s) in 0.23s
    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.34s

$ anchor build -p vault-native
    Finished `release` profile [optimized] target(s) in 0.08s
    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.08s
```

Grep for `warning` / `error` over both log files returns 0 matches. No warnings, no errors. Artifact sizes: `vault4626_naive.so = 317 KB`, `vault4626_native.so = 360 KB`.

One transient issue caught and fixed mid-build (logged here for honesty): the naive port's `earn` instruction originally had `mint_to(vault, vault.fee_recipient, ...)` which violated Rust's borrow checker (mutable + immutable borrow on `vault` at the same call site). Fixed by extracting `let recipient = vault.fee_recipient;` first (`02-naive-port.rs:246`). Build re-ran clean.

---

## B. Skill-protocol conformance

**Tag: PASS** (with one nit)

Walking SKILL.md step by step.

### Step: Two-pass protocol

> 1. **Pass 1 — Faithful port.** A semantically identical Anchor program. No restructuring, no SPL CPI substitutions, no parallelism rework. It exists so the refactor's value is legible. Mark obviously un-Solana patterns with `// SMELL:` comments rather than fixing them.

→ `02-naive-port.rs` exists, 555 lines. Monolithic `VaultState` with `Vec<BalanceEntry>`, manual `total_assets`/`total_supply` tracking, bare-arithmetic `mul_div`, no delegate-withdraw path. 24 `// SMELL:` markers. (Verified in check C.)

> 2. **Pass 2 — Solana-native refactor.** Restructured for Solana primitives... Production-ready.

→ `03-optimized.rs` exists, 748 lines. SPL Token Mint for shares, vault read-only on user operations, checked `mul_div_u128_to_u64` helper, explicit `Rounding` enum, SPL Token delegate for withdraw-on-behalf, Token-2022 rejection via type-level constraint.

### Step: Output contract

| Required | Produced | Match |
|---|---|---|
| `02-naive-port.rs` | ✓ | yes |
| `03-optimized.rs` | ✓ | yes |
| `04-diff.md` | ✓ (15 sections, themed) | yes |
| `05-explanation.md` | ✓ (20 entries, four-field schema, themed groups) | yes |

### Step: Decision tree

Loaded sub-files per the decision tree:

- Default: `translation/type-mapping.md`, `translation/pattern-mapping.md`, `security/arithmetic.md`, `security/account-validation.md`, `security/pda-canonicalization.md` ✓
- ERC-20 token / fungible token (shares are fungible) → `translation/stdlib-mapping.md`, `optimization/account-model.md`, `security/cpi-safety.md` ✓
- `mapping(...)` storage → `optimization/account-model.md`, `optimization/pdas.md`, `optimization/parallelism.md` ✓
- Ownable → `translation/stdlib-mapping.md`, `security/signer-checks.md` ✓
- Heavy arithmetic / fixed-point → `security/arithmetic.md` ✓
- Hot-write global state → `optimization/parallelism.md` ✓ (and ultimately eliminated — see diff §P1)

### Step: Pre-flight checklist

Run line-by-line in section D below.

### Step: Explanation log schema

> Each entry is exactly four fields. Keep them tight — one to three sentences each.

→ Verified in check F.

### Nit

> Always load every security/* file relevant to the constructs present.

`security/reentrancy.md` is relevant here (Token-2022 transfer-hook reentrancy is the canonical 4626 risk on Solana) but the decision tree doesn't trigger it for "ERC-20 / fungible token" inputs. I loaded it from broader judgment, not from the tree. **This is a skill issue, not an example issue** — flagged for revision in skill edits below.

---

## C. Naive port honesty

**Tag: PASS** (24 of 24 verified)

Every `// SMELL:` marker in `02-naive-port.rs` cited with the actual antipattern present.

| Line | Marker text | Antipattern verified |
|---|---|---|
| 4 | file-header reference | n/a |
| 35 | "should not have this cap" | `const MAX_HOLDERS: usize = 100;` is an artificial cap forced by Vec-in-account |
| 63 | "vault_authority bump not cached" | `vault.bump` is set but `vault_authority_bump` is not — every later access re-derives |
| 75 | "bare cast — silent truncation" | refers to `(num / den) as u64` in preview helpers (`:317`, `:324`, `:332`, `:339`) |
| 78 | "Update totals — unchecked" | `vault.total_assets += assets;` and `vault.total_supply += shares;` at `:78`–`:79` |
| 82 | "O(n) Vec scan + write-lock" | `mint_to` at `:346` does `iter_mut().find(...)` — confirmed |
| 114 | "unchecked" | `vault.total_assets += assets;` — confirmed bare |
| 115 | "unchecked" | `vault.total_supply += shares;` — confirmed bare |
| 141 | "no delegate path" | `pub user: Signer<'info>` is the only auth; no delegate accepted |
| 155 | "unchecked" | `vault.total_assets -= assets;` — confirmed bare |
| 156 | "unchecked" | `vault.total_supply -= shares;` — confirmed bare |
| 198 | "unchecked" | `vault.total_assets -= assets;` — confirmed bare |
| 199 | "unchecked" | `vault.total_supply -= shares;` — confirmed bare |
| 238 | "bare math" | `(yield_amount as u128) * (vault.fee_bps as u128) / 10_000u128` — confirmed bare |
| 245 | "silent truncation" | `(num / den) as u64` — confirmed |
| 248 | "unchecked" | `vault.total_supply += fee_shares;` — confirmed bare |
| 251 | "unchecked" | `vault.total_assets += yield_amount;` — confirmed bare |
| 308 | section header SMELL | n/a |
| 317 | "silent truncation" | `Ok((num / den) as u64)` — confirmed |
| 324 | "silent truncation" | `Ok((num / den) as u64)` — confirmed |
| 331 | "bare `num + den - 1` — can overflow u128" | `(num + den - 1)` is bare addition — confirmed |
| 332 | "silent truncation + add overflow" | `Ok(((num + den - 1) / den) as u64)` — confirmed both |
| 339 | "silent truncation + add overflow" | `Ok(((num + den - 1) / den) as u64)` — confirmed both |
| 344 | section header SMELL | n/a |
| 348 | "unchecked" | `entry.amount += shares;` — confirmed bare |
| 366 | "unchecked" | `entry.amount -= shares;` — confirmed bare |
| 408 | "writable on every deposit / mint" | `#[account(mut, seeds = [b"vault"], bump)]` at the Mutate-shape struct — confirmed |
| 441 | "no delegate support" | `pub user: Signer<'info>` in `MoveOut` — confirmed |
| 482 | "write-hot, capped, O(n) scan" | `pub balances: Vec<BalanceEntry>` — confirmed |

No SMELL marker labels code that doesn't exhibit the smell. No real smells lack a marker. Spot-checked the entire optimized vault arithmetic surface (Sec D below) — every place the optimized version uses `checked_*` corresponds to bare arithmetic in the naive port.

---

## D. Optimized version — security pre-flight

The skill's pre-flight checklist (from `SKILL.md` "Pre-flight checklist"), item by item.

### D.1 — `Every arithmetic op is checked_*` (or saturating/wrapping with justification)

**Tag: PASS**

Grepped every `+`, `-`, `*`, `/`, `%` in `03-optimized.rs`. Outside of comments, docstrings, and `#[msg(...)]` literals, every arithmetic op is one of:

- `checked_add`/`checked_sub`/`checked_mul`/`checked_div` — lines 68 (decimals), 276, 278 (fee_assets in earn), 284, 287 (offsets), 369, 372, 387, 390 (convert helpers), 401, 403, 406, 407, 408 (mul_div internals).
- `+`/`-` inside a `bump = vault.<bump_field>` constraint or array literal (semantic, not arithmetic).

Zero bare arithmetic on user-derived values. The `mul_div_u128_to_u64` helper (`:399`–`:418`) is the single arithmetic primitive for 4626 conversions; every call site routes through it.

The `as u64` narrowing at `:415`–`:416` is preceded by `require!(result_u128 <= u64::MAX as u128, VaultError::Overflow)` — bounds-guarded.

### D.2 — `Every account is validated (owner, discriminator, signer where applicable)`

**Tag: PASS**

All 5 Accounts structs, all accounts:

#### `Initialize` (lines 472–525)

| Account | Type | Owner | Discriminator | Signer | Constraint |
|---|---|---|---|---|---|
| `vault` | `Account<'info, Vault>` | ✓ (program) | ✓ (Anchor discriminator) | n/a | `init`, seeds, bump |
| `asset_mint` | `Account<'info, Mint>` | ✓ (SPL Token program — implicit Token-2022 rejection) | ✓ (SPL Mint layout) | n/a | typed |
| `vault_authority` | `UncheckedAccount<'info>` | ✗ (PDA, no data) | n/a | n/a | seeds + bump |
| `share_mint` | `Account<'info, Mint>` | ✓ (SPL Token) | ✓ | n/a | `init`, mint::decimals, mint::authority |
| `asset_reserve` | `Account<'info, TokenAccount>` | ✓ (SPL Token) | ✓ | n/a | `init`, token::mint, token::authority |
| `authority` | `Signer<'info>` | n/a | n/a | ✓ (`is_signer = true`) | mut |

#### `Deposit` (lines 528–567)

| Account | Type | Owner | Discriminator | Signer | Constraint |
|---|---|---|---|---|---|
| `vault` | `Account<'info, Vault>` | ✓ | ✓ | n/a | seeds, bump = vault.bump, has_one = asset_mint, has_one = share_mint |
| `asset_mint` | `Account<'info, Mint>` | ✓ | ✓ | n/a | typed |
| `share_mint` | `Account<'info, Mint>` | ✓ | ✓ | n/a | typed, mut |
| `vault_authority` | `UncheckedAccount<'info>` | ✗ | n/a | n/a | seeds, bump = vault.vault_authority_bump |
| `asset_reserve` | `Account<'info, TokenAccount>` | ✓ (SPL Token) | ✓ | n/a | seeds, bump (bare), token::mint, token::authority |
| `user_asset_ata` | `Account<'info, TokenAccount>` | ✓ | ✓ | n/a | token::mint, token::authority = user |
| `receiver_share_ata` | `Account<'info, TokenAccount>` | ✓ | ✓ | n/a | token::mint |
| `user` | `Signer<'info>` | n/a | n/a | ✓ | — |
| `token_program` | `Program<'info, Token>` | ✓ (program ID match + executable) | n/a | n/a | typed |

#### `Withdraw` (lines 571–611)

Same shape as Deposit; `signer: Signer<'info>` may be the share-ATA owner or its delegate. The lack of `token::authority` constraint on `owner_share_ata` is intentional (`/// CHECK` note at `:603`–`:605`) — SPL Token's `burn` enforces auth at CPI time.

#### `Earn` (lines 615–656)

Adds `has_one = authority` and a `constraint = fee_recipient_share_ata.owner == vault.fee_recipient @ VaultError::FeeRecipientMismatch` — the latter ensures fees go to the configured recipient, not an attacker-supplied ATA.

#### `AdminAction` (lines 659–670)

Vault `mut`, `has_one = authority`, `bump = vault.bump`. Authority signs.

### D.3 — `Every PDA uses canonical bumps and caches them`

**Tag: WEAK** (with proposed fix)

Cached and used: `vault.bump` (cached at `:82`, used at `:532`, `:575`, `:619`, `:664`), `vault.vault_authority_bump` (cached at `:83`, used at `:546`, `:589`, `:636` and in CPI helpers at `:429`, `:454`).

**Finding:** The `asset_reserve` PDA constraint uses bare `bump,` (re-derive canonical) at `:553` (`Deposit`), `:596` (`Withdraw`), `:643` (`Earn`). Bare `bump,` is **security-equivalent** to cached — both enforce canonicalization via Anchor's account validation — but is **CU-inefficient**: every call re-derives the canonical bump (~1.5–2.5k CU per access).

This is one literal reading of the checklist item ("caches them") in tension with another reading ("uses canonical bumps") — both forms use canonical bumps, only one caches. Strictly the example doesn't meet the cached-form rule.

**Proposed fix (applied below):** add `asset_reserve_bump: u8` to `Vault`, cache at init, use `bump = vault.asset_reserve_bump` at subsequent sites. Three diff sites. Will re-build to confirm.

Note: `share_mint` is identified by Pubkey via `has_one = share_mint`, not by seeds — no bump cache needed there.

### D.4 — `CPIs use CpiContext correctly; no privilege escalation`

**Tag: PASS**

Five CPI sites (lines 98, 138, 184, 229, 306) use `CpiContext::new` (no PDA signing — the user signs). Two CPI sites (lines 432, 457) use `CpiContext::new_with_signer` (vault_authority PDA signs).

Vault_authority signs only for: (a) minting shares to a depositor's ATA (correct — PDA is the mint authority); (b) transferring assets *out* of the asset reserve to a receiver (correct — PDA is the reserve's authority).

Signer seeds in both PDA-signed CPIs: `&[b"vault_authority", vault_key.as_ref(), &[bump]]` — vault_authority is scoped to a specific vault, not a global program PDA. No privilege escalation across vaults (when multiple exist).

No CPI signs an *arbitrary user-supplied instruction* — every CPI is to SPL Token's typed `MintTo`/`Burn`/`Transfer` helpers via `anchor_spl::token::{mint_to, burn, transfer}`.

---

## E. ERC-4626-specific risks

### E.1 — Inflation attack

**Tag: PASS**

Defense: virtual offset (`VIRTUAL_SHARES_OFFSET = 10^6`, `VIRTUAL_ASSETS_OFFSET = 1`) added to numerator/denominator of every conversion. Applied at:

- `convert_to_shares` (`:360`–`:376`) — adds offset to both legs
- `convert_to_assets` (`:378`–`:394`) — adds offset to both legs
- `earn` fee-share calculation (`:281`–`:291`) — uses same offset

The defense is preserved verbatim from the OZ Solidity. The optimized version's checked arithmetic prevents a future bug from silently degrading the defense via overflow/truncation (audit D.1 confirms).

**Regression test described in `05-explanation.md` §"ERC-4626 inflation-attack defense preserved"**: at `total_supply = 0, total_assets = 0`, `convert_to_shares(1, 0, 0, Rounding::Down)` must return `>= 1_000_000` (the virtual-shares value). If a future refactor breaks this (e.g. removes the offset, or breaks the mul_div), this assertion fails.

Numeric walk-through of attack-mitigated-but-not-prevented case included in the explanation log under the same section. Confirmed the table reflects the actual formula behavior.

### E.2 — Rounding direction

**Tag: PASS**

Every conversion site cited:

| Operation | Spec direction | Rounding enum at site | Site |
|---|---|---|---|
| `deposit` | assets→shares, round DOWN | `Rounding::Down` | `:93` |
| `mint` | shares→assets, round UP | `Rounding::Up` | `:134` |
| `withdraw` | assets→shares, round UP | `Rounding::Up` | `:177` |
| `redeem` | shares→assets, round DOWN | `Rounding::Down` | `:222` |
| `earn` fee_shares (assets→shares at pre-yield price, the fee favors holders so round DOWN) | round DOWN | `Rounding::Down` | `:290` |

All five rounding directions match the spec. The `Rounding` enum is required at each call site — no defaults, no implicit choices.

### E.3 — Reentrancy via Token-2022 transfer hook

**Tag: PASS**

The vault uses `anchor_spl::token::{Mint, TokenAccount}` (classic SPL Token program). Anchor's typed-account check verifies the account's owning program is the classic SPL Token program ID. A Token-2022 mint passed as `asset_mint` fails the typed-account check and the instruction reverts.

This eliminates the entire class of "underlying-mint transfer hook calls back into vault" attacks at the type level — no defensive code needed in the vault body.

Documented in:
- `03-optimized.rs:483`–`485` (doc comment on `asset_mint` field)
- `05-explanation.md` §"Classic SPL Token only — Token-2022 transfer hooks rejected at the type level"
- `DECISIONS.md` (Token-2022 row covers this)

This is the **documented decision** path from check E's requirements: "either ... or a documented decision in DECISIONS.md (probably: 'this vault only accepts classic SPL Token underlyings, enforced by a constraint on the mint')."

---

## F. Explanation log quality

**Tag: PASS** (3 of 3 entries reviewed)

Three entries sampled randomly (by line position: ~1/4, ~1/2, ~3/4 of the file):

### F.1 — "Replace `Vec<BalanceEntry>` shares with an SPL Token Mint" (§"State model", first entry)

- **What/Why readable cold?** Yes. Why states "Same lesson as the ERC-20 example: per-user fungible-token balances belong in SPL Token accounts, not in a Vec inside the program's state." A Solidity dev with no Solana background gets "shares are tokens; tokens go in token accounts."
- **Tradeoff real?** Yes: "First-time share recipients pay ATA rent (~0.002 SOL). Off-chain code reads share balances via SPL Token, not via a vault state field." Concrete and quantified.
- **Tautology check?** No. The Why connects to a Solidity property (mappings of fungible balances) rather than asserting Solana convention.

### F.2 — "Explicit `Rounding` direction at every conversion site" (§"Security")

- **What/Why readable cold?** Yes. Why explains the ERC-4626 spec property ("rounding direction is part of the spec, not an implementation detail"), states what each direction does and why ("preventing dust-extraction attacks where a delegate-withdrawer repeatedly redeems 1 wei to drain rounding errors"), and notes the maintenance-failure-mode it prevents.
- **Tradeoff real?** Yes: "Helper signature is longer. Worth it." Honest about the cost being minor.
- **Tautology check?** No. Connects to ERC-4626's mandatory rounding semantics — a Solidity-side property.

### F.3 — "Withdraw-by-delegate uses SPL Token's native delegate" (§"CPI & program reuse")

- **What/Why readable cold?** Yes. Why explicitly cites Solidity's `_spendAllowance` map, then says SPL Token has the equivalent built in. "Reimplementing an allowance map in the vault would duplicate SPL Token and introduce a custom code path." Clear delta from EVM to Solana.
- **Tradeoff real?** Yes: "SPL Token allows one delegate per ATA, not a `(owner, spender)` map." Notes when this matters and points back to the §C2 discussion.
- **Tautology check?** No. References specific Solidity construct (`_spendAllowance`) and specific Solana construct (SPL Token `approve` setting `TokenAccount.delegate`).

---

## G. Client/API integration notes honesty

**Tag: PASS**

`05-explanation.md` has a dedicated "Client/API integration notes" section (last major section, ~50 lines). Includes:

- **Before/After code snippets** in TypeScript using `@solana/spl-token` + `@solana/web3.js`. Concrete instruction shape, ATA derivation, idempotent ATA creation.
- **Five enumerated changes** for the porting team: no upfront approve, reads via SPL Token, ATA derivation, delegate model, event indexing.
- **Migration budgeting** ("budget a sprint, not an afternoon").
- **Delegate flow** spelled out in code: SPL Token `approve` instruction sets the delegate, the delegate then calls our `withdraw` with their key as `signer`.

The ERC-20 example covers transfer/approve at the share-token level. The 4626 example adds: PDA derivation by client, ATA management for both asset and share token, vault-instruction account list.

This is the kind of section a production team can paste into their migration spike doc.

---

## H. Dogfood — cold context simulation

**Tag: PASS** (with one note for SKILL_GAPS)

Simulated handing only `SKILL.md` + sub-files + `01-original.sol` to a fresh agent. Differences I'd expect a fresh run to produce vs. what's in `03-optimized.rs`:

| Decision point | What I'd expect from a cold run | What's in the example | Comment |
|---|---|---|---|
| Vault PDA seeds | `[b"vault", asset_mint.as_ref()]` | same | ✓ |
| Read totals from SPL Token | the cold agent might or might not catch this without seeing the ERC-20 example's `total_supply` deletion | done in example | a fresh agent might still self-track totals; SKILL.md doesn't explicitly say "read aggregates from SPL Token" |
| Rounding enum | unclear — could go enum or inline | enum + helper | a fresh agent might inline the 4 conversions in 4 functions like the naive port; **the Rounding enum pattern isn't in the skill** |
| Inflation defense | preserved from Solidity verbatim (mechanical) | same | ✓ |
| Token-2022 rejection | likely identified via `security/cpi-safety.md` + `DECISIONS.md` | same | ✓ (skill has decision-tree row for "ERC-20 with `_update` override") |
| Withdraw-by-delegate | depends on whether agent finds SPL Token delegate model | done | `translation/stdlib-mapping.md` covers this; should be reachable |
| Vault as read-only | this is the subtlest move; depends on the agent noticing the implication of S2 | done | **the read-only-vault parallelism win is not articulated in the skill** |

**Substantive divergence risk: medium.** The two patterns the cold agent might miss:
- The **"read aggregates from SPL Token"** pattern (read `mint.supply` instead of tracking `total_supply` yourself). The ERC-20 example deletes `total_supply` but in the spirit of "we don't need it" not "SPL Token already has it." Generalizing to a vault's `total_assets = reserve.amount` requires reading between the lines.
- The **Rounding enum + explicit direction** discipline for conversion math. Solidity 4626 ports often inline `(num + den - 1) / den` directly. The optimized version's enum pattern is hygiene that should be doctrine, not invention.

Both flagged in SKILL_GAPS.md for this example (see §J).

---

## I. Cross-example consistency

**Tag: WEAK** (3 inconsistencies found)

### I.1 — Governance PDA naming

| Example | Governance PDA name |
|---|---|
| token-fundraiser | `Fundraiser` |
| escrow | `Offer` (per-offer, not singleton) |
| erc4626-vault | `Vault` |

The role is identical across all three: a singleton-per-protocol-instance PDA holding governance fields (authority, fee/rate config, cached bumps). Names diverge. Defensible (entity-specific naming) but inconsistent.

**Severity: low.** Doesn't break a reader; just reads as slight stylistic drift.

### I.2 — Error code naming

| Example | Error enum name | Naming of "math overflow" |
|---|---|---|
| token-fundraiser | `FundraiserError` | `Overflow` |
| escrow | `EscrowError` | `Overflow` |
| erc4626-vault | `VaultError` | `Overflow` |

Per-entity enum names are fine (idiomatic for distinguishing errors in client code). Variant names `Overflow`, `ZeroAmount`, `InsufficientBalance`, etc. are consistent.

### I.3 — Client/API integration notes section placement

| Example | Where |
|---|---|
| token-fundraiser | Standalone top-level section after §Idioms |
| escrow | Standalone top-level section after §Idioms |
| erc4626-vault | Standalone top-level section after §Idioms |

**This is a real inconsistency.** Two of three examples cover client/API integration with concrete before/after; staking-vault doesn't (likely because the staking-vault's client integration is simpler — the user just calls `stake`/`withdraw` with their ATAs as args, no fundamental shift like share-token integration). But the placement convention varies.

**Resolved in skill revision:** use a dedicated `## Client/API integration notes` section at the bottom of `05-explanation.md` when the integration shift is material. Minor integration shifts stay inside the relevant Tradeoff field.

### I.4 — Explanation log section headers

Identical across all three: `State model`, `Parallelism`, `Security`, `CPI & program reuse`, `Compute & rent`, `Idioms`. Pass.

### I.5 — PDA seed conventions

All three use byte-literal prefix + scope-key pattern: `[b"<entity>", <scope>.as_ref()]`. Internally consistent.

---

## J. SKILL_GAPS.md triage

Reading `examples/staking-vault/SKILL_GAPS.md` (11 items, P0–P2) plus this example's new gaps.

### Staking-vault gaps

| # | Gap | Classification | Reason |
|---|---|---|---|
| 1 | Synthetix-style accumulator patterns are fundamentally write-hot | **ACCEPT** | Real pattern, will recur in DeFi ports (Curve, Aave-staking). 30-line addition to `optimization/parallelism.md`. |
| 2 | Clock-skew defense on timestamp deltas | **ACCEPT** | Solana-specific footgun; cheap to add to `security/arithmetic.md`. |
| 3 | Parameterized PDAs for multi-instance protocols | **ACCEPT** | Recurring pattern (this 4626 example uses it too). Add to `optimization/pdas.md`. |
| 4 | Two-authority separation for fund-holding protocols | **ACCEPT** | Used in staking-vault AND this 4626 example AND ERC-20 (mint_authority vs governance authority). Add to `security/signer-checks.md`. |
| 5 | `init_if_needed` safety pattern + documentation convention | **DEFER** | Useful, but staking-vault example is the only one that uses `init_if_needed`. Add when a third example exercises it. |
| 6 | Width-narrowing casts as a checklist item | **ACCEPT** | The 4626 example's `mul_div_u128_to_u64` is a poster child. Strengthen `security/arithmetic.md` checklist line. |
| 7 | Pure helpers over `ctx`-passing helpers | **DEFER** | Stylistic; useful but not load-bearing for security or correctness. |
| 8 | `01-original.<ext>` not in output-contract table | **ACCEPT** | One-line fix in SKILL.md. |
| 9 | `04-diff.md` schema should say "group by theme" | **ACCEPT** | One-line fix in SKILL.md. |
| 10 | `05-explanation.md` themes should be defined | **DEFER** | The themes are working (consistent across all three examples per check I.4); definitions would be process-creep without obvious payoff. |
| 11 | Two-step ownership threshold | **DEFER** | Real but examples currently use one-step; no immediate pressure to codify the cutoff. |

### This example's new gaps

| # | Gap | Classification | Reason |
|---|---|---|---|
| 12 | "Read aggregates from SPL Token instead of self-tracking" pattern | **ACCEPT** | The single biggest architectural insight in this example. Generalizes from `total_supply` deletion (ERC-20) to `total_assets = reserve.amount` (4626). Add to `optimization/account-model.md`. |
| 13 | Rounding-direction discipline + `Rounding` enum pattern | **ACCEPT** | 4626 has it as a spec requirement, but the pattern is broadly useful for fee math, swap math, etc. Add to `security/arithmetic.md`. |
| 14 | Vault-as-read-only-state parallelism win | **ACCEPT** | When user actions don't mutate the governance vault, cross-user parallelism is unbounded. Article this in `optimization/parallelism.md`. |
| 15 | Token-2022 rejection via Anchor's typed `Mint` | **ACCEPT** | DECISIONS.md mentions Token-2022 but doesn't explain that `anchor_spl::token::Mint` *implicitly* rejects Token-2022 via owner-program check. Add to `security/cpi-safety.md` or `security/account-validation.md`. |
| 16 | `security/reentrancy.md` should be loaded for Token-2022-capable inputs | **ACCEPT** | Decision tree should add a row for "underlying is a Token-2022 mint or fungible-token input where the underlying could carry a transfer hook." |
| 17 | The pre-flight item "every PDA caches its bump" is CU-grounded, not security-grounded | **ACCEPT** | Clarify the checklist wording. Both cached and bare-`bump,` enforce canonicalization; cached saves CU. The skill should explain the distinction and recommend cached as the default but acknowledge bare is sometimes acceptable. |
| 18 | The decision tree triggers `security/reentrancy.md` only for "external calls / interfaces", but reentrancy applies to CPIs into underlying tokens too | **ACCEPT** | Decision tree row: "any program that CPIs into an underlying mint/token program supplied as input" → load reentrancy.md. |

**Triage outcome:** 9 ACCEPT (1, 2, 3, 4, 6, 8, 9, plus 12–18), 4 DEFER (5, 7, 10, 11), 0 REJECT.

The ACCEPT items below get addressed in the skill revision section.

---

## Audit summary

| Check | Tag | Notes |
|---|---|---|
| A. Compilation | PASS | 0 warnings, 0 errors, both `.so` produced. One mid-build borrow-checker bug fixed and verified. |
| B. Skill protocol conformance | PASS | Output contract, decision tree, schemas all match. One nit on `reentrancy.md` not being decision-tree-triggered (→ skill edit). |
| C. Naive port honesty | PASS | 24/24 SMELL markers verified against actual code. |
| D. Security pre-flight | PASS (1 WEAK) | Arithmetic, account validation, CPI all clean. D.3 finds `asset_reserve` PDA uses bare `bump,` at 3 sites — security-equivalent but doesn't match strict checklist wording. Proposed fix below. |
| E. 4626 risks | PASS | Inflation defense preserved + numeric demonstration; rounding direction correct at all 5 sites; Token-2022 rejected at type level + documented. |
| F. Explanation log quality | PASS | 3 random entries pass What/Why/Tradeoff/tautology checks. |
| G. Client/API integration notes honesty | PASS | Dedicated section with before/after code and migration sizing. |
| H. Dogfood | PASS | Two cold-context divergence risks identified → flagged in SKILL_GAPS (#12, #14). |
| I. Cross-example consistency | WEAK | Minor stylistic naming drift remains; Client/API notes convention was moved into `SKILL.md`. |
| J. SKILL_GAPS triage | done | 14 ACCEPT, 4 DEFER, 0 REJECT. |

### Required example-level fix

**Fix D.3:** Cache `asset_reserve_bump` on `Vault`; use it in Deposit/Withdraw/Earn. Three-site change in `03-optimized.rs`, plus one field on `Vault`, plus init wiring.

**Status: applied and rebuilt clean.**

- Added `asset_reserve_bump: u8` to `Vault` struct.
- Updated `Vault::SIZE` from 132 → 133 bytes.
- Wired in `initialize`: `vault.asset_reserve_bump = ctx.bumps.asset_reserve;`.
- All three `asset_reserve` constraint sites switched from `bump,` → `bump = vault.asset_reserve_bump`.
- Rebuilt `vault-native`: `Finished release profile [optimized] target(s) in 1.66s` — clean, 0 warnings, 0 errors.

D.3 tag upgraded from **WEAK** to **PASS**.
