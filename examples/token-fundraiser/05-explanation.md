# Explanation log: `02-naive-port.rs` → `03-optimized.rs`

One entry per change in `04-diff.md`, grouped by theme. Each entry follows the schema: **What / Why / Benefit / Tradeoff**.

This example takes a familiar Solidity primitive — a one-shot ERC-20 crowdfund — and teaches three Solana ideas that a Solidity developer has to internalize before writing their first real Solana program:

1. **Per-supporter PDAs replace `mapping(address => uint256)`.** In Solidity the supporter ledger is `mapping(address => uint256) contributions`; in Solana there is no `mapping`, so the same logical ledger becomes one *account* per supporter at a deterministic address. Welcome to PDAs.
2. **Anchor account constraints replace handler-body checks.** The "is this supporter new?", "is the signer the creator?", and "does this account belong to this fundraiser?" checks all move out of the function body and into declarations on the account struct. The handler shrinks to two lines.
3. **Closing accounts replaces zeroing rows.** A Solidity contract marks a row "spent" by setting it to zero; on Solana, you delete the account entirely and the deposit it held (the rent) goes back to the supporter. The replay guard becomes structural rather than arithmetic.

The reference Solana shape is `tokens/token-fundraiser` in solana-developers/program-examples — same four-instruction lifecycle (`initialize`, `contribute`, `claim`, `refund`), same per-supporter account model.

A vocabulary note before we start. The terms that come up below, with their EVM analogs:

- **PDA** (Program-Derived Address) — Solana's deterministic account address. You derive it from a list of byte "seeds" the program controls — think of it as `keccak256(seeds)` with extra steps to keep the result off the secp256k1 curve. The crucial difference vs Solidity: each PDA is its own *account* on-chain, not a storage slot inside the program. There is no `state.somemap[key]` lookup; there is `Pubkey::find_program_address(seeds, programId)`.
- **rent** — A refundable SOL deposit every account pays to live on-chain (roughly 0.001 SOL per KB). When the account is closed, the rent goes back to whoever you say.
- **SPL Token** — The single shared on-chain token program. Every fungible token on Solana is just configuration on this one program, not its own contract.
- **CPI** (cross-program invocation) — One program calling another, the way one Solidity contract `call`s another. The catch: every account the callee will touch must already be in the *caller's* transaction.
- **Anchor** — The framework that wraps the lower-level Solana program API. Provides macros, account validation, and the IDL. Roughly the Hardhat-to-EVM relationship.
- **Signer<'info>** — The Anchor type that declares an account as a transaction signer; the explicit form of `msg.sender`. Solana requires every signer to be named up front in the instruction's account list.

After first use, each term is fair game; subsequent entries just use them.

---

## State model

### Global ledger `Vec<Contribution>` → per-supporter `Contributor` PDA (diff §S1)

- **What:** Removed the `contributors: Vec<Contribution>` field from the `Fundraiser` state account (`02-naive-port.rs:230`). Every supporter is now a standalone account — a PDA derived from `[b"contributor", fundraiser.key().as_ref(), supporter.key().as_ref()]` (`03-optimized.rs:248`). One account per supporter, addressable deterministically from the fundraiser + supporter pair.
- **Annotation:** This contract has no central contributor ledger — each supporter owns their own `Contributor` account, derived from `[b"contributor", fundraiser, supporter]`. Solidity would put every supporter at a slot in `mapping(address => uint256) contributions` inside the contract; on Solana the runtime locks every writable account a transaction touches, so a single shared ledger would force all simultaneous contributions to serialize. Per-supporter PDAs make the writes disjoint and unblock parallel execution.
- **Why:** In Solidity, `mapping(address => uint256) contributions` puts every supporter at a deterministic storage slot *inside the contract* — `contributions[alice]` lands at `keccak256(alice, slot)`. The Solana equivalent is one *account* per supporter — a PDA (Program-Derived Address; same "deterministic address from key" idea, but each entry is its own on-chain account at the derived address, not a slot inside the program). The naive port tried to imitate the Solidity layout by stuffing a `Vec<Contribution>` into one shared state account. That works mechanically but loses the per-account property Solana needs: the runtime locks every writable account a transaction touches, so two supporters contributing at the same time would serialize on the shared state account. With one PDA per supporter, the two writes run in parallel.
- **Benefit:** Unbounded supporters — the naive `MAX_CONTRIBUTORS = 50` cap is gone. Two unrelated contributions are now genuinely concurrent (Solana's parallel-execution scheduler can run them on different cores). Every contribute/refund touches only that supporter's PDA plus the aggregate `total_raised` field, instead of mutating a singleton state account.
- **Tradeoff:** One extra account per supporter. Each PDA holds 16 bytes of data and costs ~0.00089 SOL of rent (a refundable SOL deposit the supporter pays at first contribution to keep the account alive). The supporter recovers it on `refund` via `close = supporter` (see §S3). If the goal is met and the creator claims, the contributor PDAs stay live forever — they're not closed by `claim` because the supporter is the rent payer and only they can recover it. That's an O(N) rent footprint paid by participants; a "sweep" cleanup instruction can be added if it matters in practice.

### Linear `Vec` lookup → PDA derivation (diff §S2)

- **What:** Replaced the naive's `f.contributors.iter_mut().find(|c| c.who == supporter_key)` lookup (`02-naive-port.rs:62`) — which scans the contributor list linearly looking for the supporter's row — with `&mut ctx.accounts.contributor` (`03-optimized.rs:62`), a direct reference to the supporter's account. The "is this supporter new or returning?" branch moves out of the handler body and into an `init_if_needed` constraint on the account itself.
- **Annotation:** There's no lookup loop here — the supporter's `Contributor` account is already loaded into `ctx.accounts.contributor` by the time the handler runs. Anchor validates that the right PDA was passed in and, via the `init_if_needed` constraint on the struct, creates it on the first contribution. A Solidity dev expects `mapping[key]` to be O(1) at the language level; here the framework does the lookup before any handler code runs, so a hand-rolled scan would be doing work the runtime already does for free.
- **Why:** The naive form has the program do work the Solana runtime can do for free. Anchor (the framework on top of raw Solana, similar to Hardhat-vs-raw-EVM) validates account existence and PDA seeds *before* the handler body runs — so by the time the handler executes, the right account is either already loaded (returning supporter) or freshly initialized (new supporter). Either way the handler just adds to it.
- **Benefit:** Handler body is two lines instead of fifteen. No `TooManyContributors` error path. Constant compute per contribution regardless of how many supporters have contributed — the EVM mental model says "scanning is fine because mappings are O(1)", and here the equivalent is "PDA derivation is O(1) and the runtime does it before our code starts".
- **Tradeoff:** Requires the `init-if-needed` feature flag in `Cargo.toml` (already enabled in the workspace). The supporter pays the PDA's rent on their first contribution — a one-time cost they recover when they refund or if a sweep instruction is added.

### Refund replay protection: zero-amount sentinel → account close (diff §S3)

- **What:** Replaced the naive's `f.contributors[idx].amount = 0` (`02-naive-port.rs:131`) — the "mark this row as refunded by zeroing the amount" pattern — with `close = supporter` on the contributor PDA's `#[account(...)]` constraint (`03-optimized.rs:231`). `close = supporter` is an Anchor constraint that, when the instruction succeeds, zeros the account's data, marks it deallocated, and transfers all of its rent back to `supporter`. There is no Solidity equivalent because Solidity storage slots can't be deleted.
- **Annotation:** When a supporter refunds, their `Contributor` account is destroyed by the `close = supporter` constraint on the struct, and the rent deposit returns to them. Solidity would mark the row "refunded" by setting its amount to zero and gating future calls on `amount > 0`, because mappings can't actually be deleted. On Solana you delete the account — a duplicate refund then fails at account validation (the PDA doesn't exist) rather than relying on a runtime sentinel check inside the handler.
- **Why:** "Zero the row" is a Solidity idiom — it works because mappings are infinite and you can't actually delete an entry; the best you can do is set it to its zero value and rely on a runtime check (`if amount > 0`) to reject re-spends. On Solana you *can* delete the account, so the cheaper move is to delete it: a second `refund()` for the same supporter then fails at *account validation* (the PDA doesn't exist anymore) instead of inside the handler at a runtime check. The replay guard becomes structural — built into the account graph rather than a value-check that depends on the row not being mutated by some other code path.
- **Benefit:** Replay protection becomes structural — there is literally no account for a duplicate refund to operate on, so the program can't even reach the handler body. The supporter recovers their rent (the ~0.00089 SOL they deposited on first contribution). Storage footprint of refunded supporters drops to zero rather than lingering as "row with `amount = 0`" forever.
- **Tradeoff:** None for the supporter (they refund, they get their rent back). For the creator's mental model: a successfully-funded campaign's contributor PDAs stay around forever unless a separate cleanup instruction sweeps them — same rent footprint as before §S1 noted, but the user-visible behavior is "my contribution PDA still exists in my wallet view after the campaign succeeded".

---

## Security

### `claim()` authorization via `has_one` + seed-derived signer (diff §A1)

- **What:** Removed the runtime `require!(f.creator == ctx.accounts.creator.key(), ...)` check from the handler body (`02-naive-port.rs:84`). Authorization is now enforced by Anchor account validation: the `Fundraiser` PDA's seeds include `creator.key()`, and a `has_one = creator` constraint on the account declares that its stored `creator` field must equal the `creator` signer passed in (`03-optimized.rs:196`).
- **Annotation:** Authorization for `claim()` is enforced by two declarative checks on the struct, not by a `require!` line inside the handler: the `Fundraiser` PDA's seeds include the creator's pubkey, and `has_one = creator` makes Anchor verify the stored `creator` field matches the signer passed in. A Solidity dev would write `require(msg.sender == owner)` at the top of `withdraw()`; here a wrong signer fails at account validation before any handler code runs and the rule shows up in the program's IDL so off-chain tooling sees the access check too.
- **Why:** Two layers of protection that both happen *before* the handler runs:
  1. **Seed binding.** The `Fundraiser` PDA's address is derived from `[b"fundraiser", creator.key().as_ref()]`, so a wrong signer would derive a *different* address and Anchor would fail to load the account at all.
  2. **`has_one = creator`.** Anchor compares the loaded `Fundraiser.creator` field against the `creator` account passed in this instruction; mismatch → load fails before the handler body executes.

  The Solidity equivalent of `require(msg.sender == owner)` is a runtime check inside the function. Anchor's account validation makes the same check declarative — it lives on the account struct, runs before any state mutation, and surfaces in the program's IDL (so off-chain tooling can warn about misuse).
- **Benefit:** Authorization is checked at the runtime boundary, not buried in instruction logic. Reviewers see the access-control rule at the top of the account struct, the same way a Solidity reviewer would look for `onlyOwner` modifiers — except here it's enforced before *any* state mutation is even reachable. One fewer way to forget the check on a new instruction.
- **Tradeoff:** Anchor-specific idiom. A reviewer used to raw Solana (no framework) has to know what `has_one` and seed-binding do. Documented in `security/signer-checks.md`.

### Invariant-first state update in `refund` (diff §A2)

- **What:** Decrement `total_raised` (the running sum on the `Fundraiser`) *before* the token-transfer CPI that pays the supporter back, not after (`03-optimized.rs:106` vs `02-naive-port.rs:133`).
- **Annotation:** `total_raised` is decremented before the `token::transfer` CPI that pays the supporter back — the checks-effects-interactions pattern from Solidity reentrancy auditing. Solana's runtime locks every writable account for the whole transaction so reentrancy isn't actually a bug class here, but updating invariants before external calls is still cheap insurance: if the CPI fails partway through, on-chain state stays consistent with what the program thinks happened.
- **Why:** This is the *checks-effects-interactions* pattern from Solidity reentrancy auditing, applied for slightly different reasons. Anchor programs can't be reentered the way a Solidity contract can — Solana's runtime locks every writable account for the duration of a transaction, so a CPI'd-into program can't call back into ours while we're still executing. Reentrancy isn't a bug class on Solana. But "update invariants before external calls" is still a free habit: if the CPI fails partway through (network error, sandbox crash, downstream program rejects), the on-chain state stays consistent with what the program *thinks* happened. And it makes the program read the same way as Solidity code under the same audit rule.
- **Benefit:** Aligns with the security idiom every Solidity auditor reads for. Cheap insurance against weird CPI failure modes.
- **Tradeoff:** Slight readability cost — the `total_raised` write isn't visually adjacent to the `contributor` account close in the handler body, which can make the flow a half-step harder to trace.

---

## CPI & program reuse

### PDA seed strings consolidated (diff §C1)

- **What:** Moved the seed byte-strings (`b"fundraiser"`, `b"vault"`, `b"contributor"`) into module-level constants (`03-optimized.rs:14`–`16`) so every PDA derivation references the same source — both the `#[derive(Accounts)]` constraints and the `signer_seeds` arrays used to sign CPIs on behalf of the PDA.
- **Annotation:** All three PDA seed byte-strings (`b"fundraiser"`, `b"vault"`, `b"contributor"`) live in module-level constants so every derivation references the same source. A PDA's address is determined entirely by its seeds, and a one-byte typo at any of the places a seed is referenced derives a different address — silently. Most PDA-related production bugs are this exact shape; closest Solidity analog is "use named constants for storage-slot indexes", same hygiene with higher stakes.
- **Why:** A PDA's address is determined entirely by its seeds. A one-byte typo in one of two places where the same seed is referenced derives a *different* address — and the bug is silent: the program just signs CPIs for a PDA it doesn't actually control, or loads an account that doesn't match what it intended. Most PDA-related production bugs are this exact shape. Consolidating to one constant removes the divergence risk.
- **Benefit:** Hard to silently diverge. Reviewers audit the program's PDA namespace in one place. The Solidity-equivalent practice is "use named constants for storage-slot indexes"; same hygiene, higher stakes here because Solana can't recover from a wrong-address sign.
- **Tradeoff:** None.

---

## Compute & rent

### `MAX_CONTRIBUTORS` cap deleted (diff §R1)

- **What:** Removed the `const MAX_CONTRIBUTORS: usize = 50` cap (`02-naive-port.rs:17`) and the `TooManyContributors` error it raised.
- **Annotation:** There's no `MAX_CONTRIBUTORS` cap and no `TooManyContributors` error path in this program. A naive port would need one because a `Vec<Contribution>` on a single state account has a fixed size set at init time — Anchor needs to know the byte budget to allocate the account and pay rent. With per-supporter PDAs each supporter brings their own account, so there's nothing to cap.
- **Why:** The cap existed only because the `Vec<Contribution>` had to fit inside one account's data — Anchor needs a known max size at init time to allocate space (and pay the right rent). With per-supporter PDAs (§S1), each supporter brings their own account, so there's nothing to cap.
- **Benefit:** Unbounded scale. No `TooManyContributors` error path for the client to handle.
- **Tradeoff:** None.

### `Fundraiser` account shrinks 23× (diff §R2)

- **What:** `Fundraiser::INIT_SPACE` (the account's allocated size, which determines its rent cost) drops from ~2094 bytes with a 50-entry `Vec` to 90 bytes (`03-optimized.rs:236`–`245`).
- **Annotation:** The `Fundraiser` account is ~90 bytes — just the campaign config and aggregates, nothing per-supporter. A naive port carrying a `Vec<Contribution>` sized for the worst-case supporter count would blow this up to ~2 KB and triple the creator's init-time rent burden. Anchor deserializes the whole account on every instruction entry, so smaller is also cheaper compute per call.
- **Why:** Per-supporter state moved out (§S1), so the singleton state account only has to hold the fundraiser config and aggregates. Per-supporter rent is paid by supporters on first contribution, not by the creator at init.
- **Benefit:** Smaller hot account → smaller compute on every contribute/claim/refund (Anchor deserializes the whole account on entry; less data = fewer compute units burned). Creator's initial rent burden drops from ~0.014 SOL to ~0.0009 SOL — meaningful for a Solidity dev used to "deploying a contract is the cost, then state writes are gas" and surprised that Solana charges per-byte-of-state.
- **Tradeoff:** Aggregate rent across many supporters is higher than the naive's "one account holds everything", but it's distributed (each supporter pays their own) and recovered on refund.

---

## Idioms

### `init_if_needed` for the contributor PDA (diff §I1)

- **What:** The `contributor` account in the `Contribute` instruction uses `init_if_needed` (`03-optimized.rs:182`), which tells Anchor "create this account on the first call, no-op on subsequent calls". The supporter pays rent on the first contribution; later contributions just load and mutate.
- **Annotation:** The `contributor` constraint uses `init_if_needed`, so `contribute` is one instruction whether it's the supporter's first contribution or their tenth — Anchor creates the PDA on the first call and no-ops the create on later calls. Solidity's `mapping[address] += amount` has no "create" step because the slot is conceptually always there; without `init_if_needed` you'd ship two instructions (`register_contributor` then `contribute`) for the same UX. Safe here because the PDA seeds bind the account to a specific `(fundraiser, supporter)` pair — no caller can hijack the slot without that signer.
- **Why:** Solidity's `mapping(address => uint256) contributions += amount` has no "create" step — the slot is conceptually always there (mappings are infinite by default). The closest Solana analog is `init_if_needed`: it creates the underlying account the first time, and after that it's just there. Without `init_if_needed` you'd need two instructions: a `register_contributor` to create the account, then `contribute` to top it up — bad UX for an EVM developer used to one-shot writes.
- **Benefit:** One instruction does the work of two. The client never has to know whether it's the supporter's first contribution.
- **Tradeoff:** `init_if_needed` requires explicit opt-in via the `init-if-needed` Anchor feature flag, and some teams ban it because it's easier to forget account-collision checks. Here it's safe because the PDA seeds bind the account to a specific `(fundraiser, supporter)` pair — no other caller can hijack the slot since only the matching supporter signer can derive the same address.
