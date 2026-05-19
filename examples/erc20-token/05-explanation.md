# Explanation log: `02-naive-port.rs` → `03-optimized.rs`

One entry per change in `04-diff.md`, grouped by theme. Each entry follows the schema: **What / Why / Benefit / Tradeoff**.

---

## State model

### Replace `TokenState` monolith with a `Config` PDA + SPL Mint (diff §S1)

- **What:** Deleted `TokenState` (`02-naive-port.rs:268`–`286`). Added `Config` (`03-optimized.rs:224`–`240`) holding only governance fields: `authority`, `mint`, `max_supply`, two bumps. Token mechanics (supply, decimals, balances) move to the SPL Mint.
- **Why:** Solana state is per-account, not per-contract. A monolithic state account makes every operation that mutates *any* field a write-lock on *every* field. SPL Token has a separate Mint account already shaped this way, and we use it.
- **Benefit:** ~18 KB of account size → 74 bytes. Governance reads (who is `authority`) and supply reads (`mint.supply`) no longer contend for the same write lock.
- **Tradeoff:** A second account (`Mint`) to initialize, plus a PDA derivation step. Worth it.

### Move balances to Associated Token Accounts (diff §S2)

- **What:** Deleted `Vec<BalanceEntry>` from state (`02-naive-port.rs:275`). Holders use Associated Token Accounts; transfers go through SPL Token directly.
- **Why:** The Vec serialized every transfer in the system through one write lock — the cardinal Solana antipattern. Per-account balances let Sealevel parallelize transfers between disjoint pairs. SPL Token is an audited implementation of exactly this design.
- **Benefit:** Unbounded holder count (the `MAX_HOLDERS = 100` cap is gone). Linear parallel throughput. ~150 lines of custom balance/allowance logic deleted and replaced with audited code.
- **Tradeoff:** First-time recipients pay ~0.002 SOL to create their ATA. Off-chain readers query ATAs to display balances rather than reading one program account. Standard Solana UX.

### Move allowances to SPL Token's `delegate` mechanism (diff §S3)

- **What:** Deleted `Vec<AllowanceEntry>` (`02-naive-port.rs:276`) and the `approve` / `transfer_from` instructions (`02-naive-port.rs:66`, `:97`).
- **Why:** SPL Token has built-in delegation: `approve` sets `TokenAccount.delegate` and `delegated_amount`; the delegate then calls `transfer` directly. The semantics are nearly identical to ERC-20 `approve`/`transferFrom`.
- **Benefit:** Two instructions deleted. Audited delegation primitive instead of hand-written allowance state. Allowance writes happen on the owner's ATA, not on shared state — parallel-friendly.
- **Tradeoff:** SPL Token allows one delegate per `TokenAccount`, not a `(spender → amount)` map. The vast majority of real ERC-20 usage uses one active delegate at a time, so this is rarely felt. If multi-spender allowance is genuinely required, build a per-pair allowance PDA — but only on demand, not by default.

### Drop `total_supply`; read `mint.supply` instead (diff §S4)

- **What:** Removed `total_supply` field (`02-naive-port.rs:272`) and its `+=`/`-=` mutations (`02-naive-port.rs:141`, `:165`). Max-supply check reads `mint.supply` directly (`03-optimized.rs:52`).
- **Why:** SPL Token already tracks total supply atomically. Maintaining a parallel copy duplicates work and — worse — write-locks our `Config` on every mint/burn, defeating governance read parallelism.
- **Benefit:** One source of truth. No divergence bugs between our counter and SPL's. No write-hot governance account.
- **Tradeoff:** None. The naive design's `total_supply` was strict overhead.

### Drop on-chain `name`/`symbol` (diff §S5)

- **What:** Removed `name`/`symbol` string fields (`02-naive-port.rs:269`–`270`) and the storage they cost.
- **Why:** Solana's convention is Metaplex Token Metadata — an off-chain JSON descriptor referenced from an on-chain metadata account. Every wallet, explorer, and DEX reads metadata from there; nobody looks for inline strings on the token program.
- **Benefit:** Rent saved (52 bytes ≈ 0.0004 SOL). Wallet integrations work out of the box. Metadata is updatable independently of the token program.
- **Tradeoff:** Initialization is two transactions instead of one if metadata is needed at launch (mint + metadata). Standard Solana UX.

---

## Parallelism

### Eliminate the write-hot `state` PDA (diff §P1)

- **What:** Removed the `Mutate` struct (`02-naive-port.rs:251`) and the singleton `state` PDA. Each operation now writes only the accounts it logically touches.
- **Why:** Sealevel parallelizes transactions whose writable account sets are disjoint. A program with one shared writable account caps at one operation per slot, no matter the hardware.
- **Benefit:** Transfers between Alice→Bob and Carol→Dave write disjoint ATAs. They run in parallel. Throughput scales with usage, not program design.
- **Tradeoff:** None — this is pure benefit. The naive design's single-PDA shape was a cost, not a feature.

### Remove O(n) Vec scans (diff §P2)

- **What:** Deleted `find(|e| e.holder == x)` loops (`02-naive-port.rs:201`, `:218`, `:163`).
- **Why:** Linear search inside an account means each call deserializes the whole Vec, scans, mutates, reserializes. Costs CU linearly in holder count; even at 100 holders it's measurable.
- **Benefit:** Direct addressing — every account is reached by a deterministic Pubkey. O(1) lookups, no iteration.
- **Tradeoff:** None.

---

## Security

### Switch every arithmetic op to `checked_*` (diff §Sec1)

- **What:** Replaced `+= / -= / +` on user-derived values with `checked_add` / `checked_sub` / `checked_mul`, plus `.ok_or(TokenError::Overflow)?`. Naive sites marked `// SMELL: unchecked` at `02-naive-port.rs:119`, `:135`, `:141`, `:164`, `:165`, `:210`, `:220`.
- **Why:** Rust release builds wrap arithmetic silently — opposite of Solidity 0.8+, which checks by default. Porting `+` to `+` is a regression in safety, not a translation. Even the max-supply guard itself overflowed on adversarial input in the naive version.
- **Benefit:** Overflow/underflow become explicit, typed errors. No silent wrap that turns into a balance gift.
- **Tradeoff:** Slightly more verbose code at each call site. Mechanical, and worth it on every byte of token math.

### Cache and enforce canonical PDA bumps (diff §Sec2)

- **What:** Stored canonical bumps on `Config` (`03-optimized.rs:233`–`235`); every subsequent PDA access uses `bump = config.bump` / `bump = config.mint_authority_bump`. Naive form used bare `bump` everywhere (`02-naive-port.rs:237`, `:252`, `:260`).
- **Why:** Bare `bump` accepts any value that produces a valid off-curve address — multiple such values can exist for a single seed set. If a seed component is user-controllable, an attacker can derive an alternate PDA, fund it, and trick the program into operating on the wrong account.
- **Benefit:** Class of bug eliminated. Bonus: 1.5–2.5k CU saved per PDA per call by not re-deriving.
- **Tradeoff:** 1 extra byte per PDA-controlling account. Negligible.

### Move `onlyOwner` checks to declarative `has_one` constraints (diff §Sec3)

- **What:** Replaced runtime `require_keys_eq!(state.owner, ...)` checks (`02-naive-port.rs:130`, `:181`) with `has_one = authority` on the account struct (`03-optimized.rs:160`).
- **Why:** Declarative constraints run before the function body and surface in the IDL. They are harder to forget on new instructions; they are visible to clients without reading the function bodies.
- **Benefit:** Fewer foot-guns. Auditable from the struct definition alone. Bypassing requires bypassing Anchor's validation layer, not a runtime branch in your function.
- **Tradeoff:** None.

### Mint authority is a program-owned PDA, not a stored Pubkey (diff §Sec4)

- **What:** The SPL Mint's `mint_authority` field is set to a program-derived PDA at init (`03-optimized.rs:144`). The program signs for that PDA via `invoke_signed` when calling SPL Token's `mint_to` (`03-optimized.rs:59`–`75`). The governance `authority` keypair (which gates `mint_to` on *this* program) is a separate identity that cannot mint directly.
- **Why:** In Solidity, `mint` is gated by `onlyOwner`, and the contract *is* the mint authority — there's no way to issue tokens except through the contract's `mint` function. The naive Solana translation breaks this: it stores `owner` as a pubkey but the actual SPL Mint has no mint authority bound to the program at all, so whoever ends up holding any keypair the Mint trusts can call SPL Token directly and skip our max-supply check. Making the program-owned PDA the mint authority restores Solidity's "all mints flow through our function" invariant — SPL Token will only accept `mint_to` instructions signed for the PDA, which only this program can do.
- **Benefit:** No path to mint that bypasses `Config.max_supply`. Even if `Config.authority` is compromised, the attacker can mint up to `max_supply` and rotate governance — but cannot extract a back-door minting key. The program upgrade authority becomes the highest-stakes secret, not an off-chain mint key floating around.
- **Tradeoff:** The mint authority cannot be changed by `set_authority` (which only rotates the governance `Config.authority`). Genuinely transferring the mint to a different program requires either a program upgrade or an SPL `set_authority` call signed by the PDA — non-trivial migration. Acceptable for most ERC-20 ports; document it for the deploying team.

---

## CPI & program reuse

### `mint` → CPI to `spl_token::mint_to` with PDA signer (diff §C1)

- **What:** Replaced custom `state.total_supply += amount` and `upsert_balance` with one `token::mint_to` CPI signed by the program-owned `mint_authority` PDA (`03-optimized.rs:46`–`77`).
- **Why:** Reusing SPL Token means we don't reimplement supply tracking or balance updates. The PDA-signer pattern is Solana's idiom for "a program controls this authority"; `CpiContext::new_with_signer` wires it without manual `invoke_signed`.
- **Benefit:** ~25 lines of custom code → 1 CPI. Logic that updates supply and balances now runs in an audited program. Max-supply enforcement is the only logic we retain — the *governance* surface, which is the only thing genuinely application-specific.
- **Tradeoff:** A CPI costs ~5k CU. The naive version did the work in process at ~3k CU. Net: marginal CU regression, massive code-and-correctness improvement. Accept.

### `burn` → CPI to `spl_token::burn` (diff §C2)

- **What:** Replaced custom balance scan + decrement with `token::burn` CPI (`03-optimized.rs:86`–`99`). Holder signs for their own ATA; no program signing needed.
- **Why:** Same reasoning as mint — SPL Token is the implementation. The holder is `Signer<'info>`, the ATA constraints enforce `token::authority = holder`, and SPL Token does the rest.
- **Benefit:** 21 lines → 14, with stronger validation. The holder cannot burn anyone else's tokens because the ATA's authority must match the signer (enforced by `token::authority = holder` at `03-optimized.rs:203`).
- **Tradeoff:** None worth listing.

### Drop `transfer`, `approve`, `transfer_from` instructions entirely (diff §C3)

- **What:** Three instructions (`02-naive-port.rs:59`, `:66`, `:97`) disappear from the optimized program. Clients call SPL Token directly.
- **Why:** Wrapping SPL Token's native instructions in our own adds nothing — no validation we have to do that SPL Token doesn't already. A wrapper is a tax (extra CU, extra instruction in the transaction) without a benefit.
- **Benefit:** ~80 lines deleted. One fewer program in the user's transaction stack. Every existing Solana wallet, DEX, and indexer integrates with SPL Token transfers natively — your token works in Phantom, Jupiter, Raydium, Helius, etc. on day one with no per-integration work.
- **Tradeoff (this is the big one — read carefully if you're porting an existing dApp):** Your frontend changes. Concretely:

  Before (web3.js + your old ERC-20 contract, mental model):
  ```ts
  // one call to your contract
  await tokenContract.transfer(recipient, amount);
  ```

  After (web3.js + this program + SPL Token):
  ```ts
  import {
      getAssociatedTokenAddressSync,
      createAssociatedTokenAccountIdempotentInstruction,
      createTransferInstruction,
      TOKEN_PROGRAM_ID,
  } from "@solana/spl-token";

  const senderAta    = getAssociatedTokenAddressSync(mint, sender.publicKey);
  const recipientAta = getAssociatedTokenAddressSync(mint, recipient);

  const tx = new Transaction()
      // Ensure the recipient has an ATA. Idempotent — no-op if it exists.
      // Sender pays the rent (~0.002 SOL) the first time.
      .add(createAssociatedTokenAccountIdempotentInstruction(
          sender.publicKey, recipientAta, recipient, mint))
      // Then the actual transfer — to SPL Token, NOT to your program.
      .add(createTransferInstruction(
          senderAta, recipientAta, sender.publicKey, amount));

  await sendAndConfirmTransaction(connection, tx, [sender]);
  ```

  What changes for your team, beyond syntax:
  - **Approve flows** (`approve` + `transferFrom`) use `createApproveInstruction` against the holder's ATA. The approver picks a single `delegate`; the delegate then signs a normal `createTransferInstruction`. Multi-spender allowance maps don't exist (see §S3).
  - **Balance display** is `connection.getTokenAccountBalance(ata)` against the holder's ATA (or `getParsedTokenAccountsByOwner` to list all holdings). It is *not* `tokenContract.balanceOf(holder)`.
  - **Event listening** subscribes to SPL Token's log shape, not your program's. Most teams use Helius / Triton / a generic Solana indexer for this; their parsers already understand `mint_to`, `burn`, `transfer`, `approve` events from SPL Token.

  If your dApp's frontend assumes a single contract call per token op, this is a real rewrite — not the architecture, but every call site. Budget a sprint for the migration, not an afternoon. If that's not acceptable, the alternative is to *add* thin wrapper instructions to this program (transfer, approve) that CPI into SPL Token — costs ~5k CU per call and ~10 lines of code each, gives you a single-call entry point for backward-compatible frontends. The skill recommends against it as the default but it's a legitimate hatch when migration cost matters more than CU.

---

## Compute & rent

### Account size reduced from ~18 KB to 74 bytes (diff §I5)

- **What:** Config replaces TokenState. `8 + 74 = 82` bytes (~0.0016 SOL rent) vs `8 + 18,489 = 18,497` bytes (~0.13 SOL rent).
- **Why:** Removing the in-account Vecs is the only state-size lever; the Mint account (separate, ~82 bytes) and per-holder ATAs (165 bytes each) are paid by *holders*, not by `authority`.
- **Benefit:** ~40× rent savings on the protocol-paid account. Lower deployment friction for new tokens.
- **Tradeoff:** Each holder pays their own ATA rent. Standard SPL Token UX — not a regression, just visible.

### Helper functions `do_transfer` / `upsert_balance` deleted (diff §I4)

- **What:** Internal helpers (`02-naive-port.rs:201` and `:218`) are gone.
- **Why:** The work they performed is gone — SPL Token does it.
- **Benefit:** Less code to audit. Less code to upgrade. Less code to read on day one for a developer new to the project.
- **Tradeoff:** None.

---

## Idioms

### `constructor` → `initialize` instruction (diff §I1)

- **What:** Both ports use an explicit `initialize` instruction (Anchor has no constructor). The optimized form takes `decimals` and `max_supply` only; name/symbol are absent.
- **Why:** No EVM-style deploy-time code. Initial state is always written by an explicit transaction signed by the deployer.
- **Benefit:** Re-init protection is automatic — Anchor's `init` constraint fails if the account exists.
- **Tradeoff:** One extra transaction at launch (vs EVM's atomic deploy-and-init). Solana convention.

### Drop custom `Transfer` / `Approval` / `OwnershipTransferred` events (diff §I2)

- **What:** Three `#[event]` types and their five emission sites (`02-naive-port.rs:306`, `:313`, `:320`) are gone.
- **Why:** SPL Token emits the underlying mint/burn/transfer/approve via its own program logs — already indexed by every Solana indexer. Duplicating costs CU per call without adding information. For `OwnershipTransferred`, account-state diffs already encode it; no indexer asks for the event form.
- **Benefit:** Less CU per call. Less code. No risk of emitted-event-vs-actual-state divergence.
- **Tradeoff:** Integrators expecting an EVM-shaped event stream need to learn the SPL log shape. Mechanical; every Solana indexer already speaks it.

### Drop hardcoded capacity constants (diff §I3)

- **What:** `NAME_MAX`, `SYMBOL_MAX`, `MAX_HOLDERS`, `MAX_ALLOWANCES` (`02-naive-port.rs:20`–`:23`) all deleted.
- **Why:** They existed only because Vec-in-account demands a fixed upper bound. With Vecs gone, the constants have no purpose.
- **Benefit:** No artificial holder cap. No "we set MAX_HOLDERS = 100 and now need to redeploy" problem. Removes a class of "out-of-capacity" runtime errors that didn't need to exist.
- **Tradeoff:** None.
