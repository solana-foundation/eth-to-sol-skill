# Skill gaps surfaced by the erc4626-vault example

Each gap names what was missing, where it should live, and the rough shape of the addition. Triage classifications (ACCEPT / DEFER / REJECT) live in `AUDIT.md §J`. This file documents the gaps themselves with concrete fix proposals.

The skill carried this example correctly — none of the gaps were blockers. But the 4626 vault has structural properties (read-only governance during user actions, share/asset conversion math with mandatory rounding semantics, inflation-attack defense) that recur in any vault-shaped protocol (4626 strategies, AMM LP shares, money-market deposit tokens). The skill should grow to cover them.

---

## Gaps marked ACCEPT in AUDIT.md §J — to land in the skill revision this session

### 12. "Read aggregates from SPL Token; don't self-track"

**Where:** new section in `optimization/account-model.md`, near "Hot-write fields are an antipattern."

**Missing today:** the ERC-20 example deletes `total_supply` from the `Config` PDA but frames it as "we don't need it" rather than "SPL Token already maintains the canonical value, so re-tracking introduces a divergence-bug class AND forces our state account to be writable on every interaction." The 4626 vault generalizes: `total_assets = asset_reserve.amount`, `total_supply = share_mint.supply`. This insight is the architectural pivot that lets the optimized vault be read-only during user operations.

**Proposed addition (~25 lines):**

> ### Read aggregates from SPL Token instead of self-tracking
>
> Solidity contracts often track aggregate state — `totalSupply`, `totalAssets`, `totalDeposits` — alongside per-user balances. On Solana, when the underlying tokens move via SPL Token, the aggregates already exist:
>
> | Solidity aggregate | Solana equivalent | Maintained by |
> |---|---|---|
> | `IERC20(token).totalSupply()` of the contract's own shares | `Account<Mint>.supply` of the share Mint | SPL Token |
> | `asset.balanceOf(address(this))` of the contract's reserve | `Account<TokenAccount>.amount` of the reserve | SPL Token |
> | A counter of issued positions | rarely a Mint, but sometimes a `Vec` is the right answer | depends |
>
> Read these from the deserialized Anchor account. Do not store parallel copies. Two reasons:
>
> 1. **Divergence-bug class eliminated.** A missed `+=`/`-=` somewhere in your code makes your tracked aggregate disagree with reality. SPL Token's atomic mint/burn/transfer cannot disagree with itself.
> 2. **Parallelism.** If aggregates live on a singleton config/vault PDA, every state-changing instruction must write that PDA — every user contends. If aggregates live on SPL Token accounts, only the SPL accounts contend (which they would anyway, as inherent to fungible tokens), and your config/vault PDA can be read-only on user operations.
>
> Cross-reference: `optimization/parallelism.md` "Vault-as-read-only" pattern is the parallelism follow-on.

---

### 13. Rounding direction discipline — Rounding enum + checked mul_div pattern

**Where:** `security/arithmetic.md`, new section after "Mixed-width arithmetic."

**Missing today:** the existing file covers `checked_*`, widening, and a brief note on rounding direction in fee math. It does NOT cover the discipline pattern: an explicit `Rounding` enum passed at every conversion site, with a single `mul_div_*` helper. This is the standard pattern in Uniswap V3, OpenZeppelin Math, Solady — and required by ERC-4626 spec correctness.

**Proposed addition (~40 lines):**

> ### Rounding direction is part of the API surface
>
> When converting between two units via `mul_div` (shares↔assets, USDC↔collateral, base↔quote), the rounding direction is part of the spec, not an implementation detail. Wrong direction is exploitable: a delegate that calls `redeem(1, ...)` repeatedly to drain dust if direction rounds in the user's favor.
>
> Discipline:
>
> 1. Define an enum locally:
>    ```rust
>    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
>    pub enum Rounding { Down, Up }
>    ```
> 2. Write one `mul_div` helper. Every conversion site passes the direction explicitly:
>    ```rust
>    fn mul_div_u128_to_u64(a: u128, b: u128, c: u128, rounding: Rounding) -> Result<u64> {
>        require!(c > 0, MyError::DivByZero);
>        let product = a.checked_mul(b).ok_or(MyError::Overflow)?;
>        let result_u128 = match rounding {
>            Rounding::Down => product.checked_div(c).ok_or(MyError::DivByZero)?,
>            Rounding::Up => {
>                let c_minus_one = c.checked_sub(1).ok_or(MyError::Overflow)?;
>                let raised = product.checked_add(c_minus_one).ok_or(MyError::Overflow)?;
>                raised.checked_div(c).ok_or(MyError::DivByZero)?
>            }
>        };
>        require!(result_u128 <= u64::MAX as u128, MyError::Overflow);
>        Ok(result_u128 as u64)
>    }
>    ```
> 3. At every call site, name the direction:
>    ```rust
>    let shares = mul_div_u128_to_u64(assets as u128, ..., Rounding::Down)?;  // deposit
>    let assets = mul_div_u128_to_u64(shares as u128, ..., Rounding::Up)?;     // mint
>    ```
> 4. A code reviewer can audit direction in N lines: one per call site.
>
> For ERC-4626 specifically: deposit/redeem round DOWN; mint/withdraw round UP. All four favor the vault. This is in the spec.

---

### 14. "Vault as read-only state" parallelism pattern

**Where:** `optimization/parallelism.md`, follow-on to the "Per-entity PDAs" section.

**Missing today:** the file discusses per-user PDAs and the Synthetix-style accumulator constraint, but doesn't articulate the highest-throughput case: when the governance PDA itself is read-only during user operations. This is the 4626 vault story.

**Proposed addition (~20 lines):**

> ### When the governance PDA can be read-only during user actions
>
> The highest-parallelism shape: deposits/withdrawals/swaps don't write your governance/config PDA at all. They write only:
>
> - The user's own token accounts (always per-user, never shared).
> - The protocol's pooled SPL Token accounts (writable, but inherent — same contention as any fungible-token system).
> - The user's per-position PDA, if your protocol has one (per-user, never shared).
>
> The governance PDA stays read-only — instructions reference it (for cached bumps, has_one cross-checks, configuration values like fee_bps) but don't mutate it. Cross-user operations don't write-conflict on it.
>
> This shape is achievable when:
>
> - Aggregates (`total_supply`, `total_assets`) are read from SPL Token, not self-tracked. (See `optimization/account-model.md`.)
> - Per-user state lives in per-user PDAs.
> - The protocol has no global accumulator that must be checkpointed on every interaction (the Synthetix case forces vault writes).
>
> Vanilla ERC-4626 and AMM-style protocols can usually achieve this. Synthetix-style emission protocols cannot (see the "accumulator" section above).
>
> Cross-reference: `examples/erc4626-vault/03-optimized.rs` is the worked instance.

---

### 15. Token-2022 rejection via Anchor's typed Mint

**Where:** `security/account-validation.md`, in the table of "what Anchor checks by default."

**Missing today:** the file lists `Account<'info, Mint>` as "Anchor verifies owner is SPL Token" but doesn't draw the consequence: this *rejects Token-2022 mints* by default, because Token-2022 mints are owned by a different program ID. For 4626 vaults and any protocol with arbitrary-token underlyings, this is the cheapest defense against transfer-hook reentrancy.

**Proposed addition (~10 lines, to the existing table's notes):**

> **Note:** `anchor_spl::token::Mint` / `TokenAccount` enforce the owning program is the **classic** SPL Token program (program ID `Tokenkeg...`). Token-2022 mints (owned by `TokenzQd...`) fail this check.
>
> This is the cheapest defense against transfer-hook reentrancy: a Token-2022 mint with a `TransferHook` extension can call back into your program during a `transfer` CPI. Requiring classic SPL Token at the type level eliminates this attack class structurally.
>
> If your protocol must accept Token-2022, use `anchor_spl::token_interface::{Mint, TokenAccount}` (accepts both programs) and add an explicit allowlist of Token-2022 extensions — see `security/cpi-safety.md`.

---

### 16. Decision-tree row: `security/reentrancy.md` for inputs with arbitrary token underlying

**Where:** `SKILL.md` decision tree.

**Missing today:** `security/reentrancy.md` only loads on "External calls / interfaces" and "Anything writing state after an external call." A protocol whose user-supplied input is an arbitrary token Mint — vaults, AMMs, lending markets — should always load reentrancy.md because the CPI into SPL Token is the same shape as a CPI into any user-controlled program.

**Proposed row:**

> | Protocol accepts a user-supplied token Mint as configuration (vault, AMM, lending market) | `security/reentrancy.md`, `security/account-validation.md` |

---

### 17. Clarify the pre-flight checklist item on bump caching

**Where:** `SKILL.md` pre-flight checklist.

**Current wording:**

> [ ] Every PDA derivation uses `seeds = [...], bump = stored_bump` after init. Bumps are cached on the account; `find_program_address` only runs once per PDA lifetime.

**Issue:** reads as a security mandate. Actually it's a CU optimization — bare `bump,` on non-init access also enforces canonicalization (Anchor calls `find_program_address` and verifies the supplied account matches the canonical address). The difference is purely CU: cached saves ~1.5–2.5k CU per access; bare re-derives.

**Proposed rewording:**

> [ ] Every PDA derivation either uses `seeds = [...], bump = stored_bump` (preferred — saves ~1.5k CU per call) or bare `seeds = [...], bump,` (acceptable when CU is not pressured; still canonicalization-enforcing). The cached form is strongly preferred and used by all reference examples. Do NOT use `bump = <user_input>` — that's the actual canonicalization vulnerability.

---

### 18. Decision-tree row: any CPI into SPL Token where the mint is user-supplied → load reentrancy.md

Already covered by #16 above (the row for "user-supplied token Mint" implicitly covers all CPIs into those mints).

---

### I.3 — Frontend-section placement convention (cross-example consistency)

**Where:** `SKILL.md` output contract.

**Missing today:** the output contract describes `05-explanation.md` as having one entry per change, themed. It doesn't say where (or whether) to include a frontend-integration section.

**Proposed addition to the output contract:**

> When the optimized version changes the client-side integration in a way a porting team needs to know about on day one — typically when SPL Token replaces a custom token surface, or when share/balance lookups move off the program — append a `## Frontend integration` section to `05-explanation.md` with:
>
> - Before/after code (TypeScript using `@solana/web3.js` + `@solana/spl-token`).
> - List of changed call sites a porting team will touch.
> - Migration scoping ("budget a sprint vs. an afternoon").
>
> If the integration shift is minor (e.g. additional account in the args), keep the discussion inside a relevant entry's Tradeoff rather than a standalone section.

---

## Gaps marked DEFER in AUDIT.md §J — not landing this session

For completeness (not edited in skill this session):

- #5: `init_if_needed` safety pattern + documentation convention
- #7: Pure helpers over `ctx`-passing helpers
- #10: `05-explanation.md` themes should be defined
- #11: Two-step ownership threshold

Reasons in `AUDIT.md §J`.

---

## Items I confirmed the skill *does* cover well

- Default-load `security/arithmetic.md` was the right call — every conversion site needed it.
- The PDA canonicalization story (`security/pda-canonicalization.md`) covered the security correctness; the CU-vs-security nuance is the only refinement.
- `translation/stdlib-mapping.md` covered SPL Token delegate semantics; my use of SPL delegate for withdraw-on-behalf was directly supported.
- `optimization/account-model.md` "PDA per key" was the right lens for per-user share balances → ATAs.
- The output contract (four artifacts) plus the explanation log schema (What/Why/Benefit/Tradeoff) plus the section themes (State model / Parallelism / Security / CPI / Compute & rent / Idioms) carried the example end-to-end at the structural level.

No edits proposed for those files (beyond the additions noted above).
