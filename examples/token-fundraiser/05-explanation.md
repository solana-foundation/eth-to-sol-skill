# Explanation log: `02-naive-port.rs` → `03-optimized.rs`

One entry per change in `04-diff.md`, grouped by theme. Each entry follows the schema: **What / Why / Benefit / Tradeoff**.

This example teaches the cleanest possible version of three Solana ideas using a recognizable real-world primitive (a one-shot ERC-20 crowdfund):

1. **Per-supporter PDAs replace `mapping(address => uint256)`.** The Solidity ledger is one storage slot per supporter inside a single contract. The Solana equivalent is one PDA per supporter — not a `Vec` inside a state account.
2. **`init_if_needed` is the "create-or-find" branch for free.** The handler doesn't need an `iter().find()` over a `Vec` — the account constraint does the work.
3. **`close = supporter` on refund replaces "mark refunded" sentinels.** Solidity zeroes the row; Solana tears down the account. The replay guard becomes structural, not arithmetic.

The reference Solana shape is `tokens/token-fundraiser` in solana-developers/program-examples — same instruction set (`initialize`, `contribute`, `claim`, `refund`), same per-supporter account model.

---

## State model

### Global ledger `Vec<Contribution>` → per-supporter `Contributor` PDA (diff §S1)

- **What:** Removed the `contributors: Vec<Contribution>` field from `Fundraiser` (`02-naive-port.rs:230`). Each supporter is now a standalone PDA derived from `[b"contributor", fundraiser.key().as_ref(), supporter.key().as_ref()]` (`03-optimized.rs:248`).
- **Why:** Solidity's `mapping(address => uint256)` is one slot per supporter inside one contract — addressable by key inside one storage tree. The Solana equivalent is one account per supporter, addressable by PDA derivation. A `Vec` inside a state account is the wrong primitive: it bounds the supporter count, forces every contribute/refund to mutate the singleton state account, and scans linearly on lookup.
- **Benefit:** Unbounded supporters (the naive `MAX_CONTRIBUTORS = 50` cap is gone). No serialization of cross-supporter activity. Every contribute/refund touches only that supporter's PDA plus the aggregate `total_raised` field.
- **Tradeoff:** One extra account created per supporter — rent ~0.00089 SOL per row. Refunded to the supporter on `refund()` via `close = supporter`. If the goal is met and the creator claims, the contributor PDAs stay live forever (they're not closed by `claim` because the supporter is the rent payer). That's an O(N) rent footprint paid by participants; consider a sweep instruction if needed.

### Linear `Vec` lookup → PDA derivation (diff §S2)

- **What:** Replaced the `f.contributors.iter_mut().find(|c| c.who == supporter_key)` branch (`02-naive-port.rs:62`) with `&mut ctx.accounts.contributor` (`03-optimized.rs:62`). The "is this supporter new?" branch moves into the `init_if_needed` constraint on the account.
- **Why:** The naive form has the program do work the runtime can do for free. Anchor validates account existence + seeds before the handler runs, so the only remaining work in the handler is "add to the amount".
- **Benefit:** Handler body is two lines. No `TooManyContributors` error path. Constant compute per call regardless of how many other supporters have contributed.
- **Tradeoff:** Requires the `init-if-needed` feature flag in `Cargo.toml` (already enabled in the workspace). The supporter pays rent on their first contribution.

### Refund replay protection: zero-amount sentinel → account close (diff §S3)

- **What:** Replaced the naive's `f.contributors[idx].amount = 0` (`02-naive-port.rs:131`) with `close = supporter` on the contributor PDA's `#[account(...)]` constraint (`03-optimized.rs:231`).
- **Why:** "Mark this row as refunded by zeroing the amount" is a Solidity idiom — it works because mappings are infinite and you can't delete entries cheaply. On Solana you can: close the account. A second `refund()` for the same supporter then fails at account validation (PDA doesn't exist), which is a structural guard, not a runtime check that depends on the row not being mutated.
- **Benefit:** Replay protection becomes structural. The supporter recovers their rent. Storage footprint of refunded supporters drops to zero.
- **Tradeoff:** None for the supporter (they refund, they get their rent back). For the creator's mental model: a successfully-funded campaign keeps its contributor PDAs around forever unless a separate cleanup instruction is added.

---

## Security

### `claim()` authorization via `has_one` + seed-derived signer (diff §A1)

- **What:** Removed the runtime `require!(f.creator == ctx.accounts.creator.key(), ...)` check (`02-naive-port.rs:84`). Authorization is now enforced by Anchor account validation via `seeds = [FUNDRAISER_SEED, creator.key().as_ref()]` + `has_one = creator` on the `Fundraiser` account (`03-optimized.rs:196`).
- **Why:** The fundraiser PDA's seeds include `creator.key()`, so only the actual creator can re-derive its address. `has_one = creator` re-checks that the loaded account's `creator` field matches the signer. Both checks happen before the handler body, so a wrong signer can't reach any state-mutating code.
- **Benefit:** Authorization is checked at the runtime boundary, not inside the handler. Reviewers see the access-control rule at the top of the struct, not buried in instruction logic. One fewer way to forget the check on a new instruction.
- **Tradeoff:** Anchor-specific idiom. Reviewers coming from raw Solana need to know what `has_one` does.

### Invariant-first state update in `refund` (diff §A2)

- **What:** Decrement `total_raised` before the token-transfer CPI (`03-optimized.rs:106`), not after (`02-naive-port.rs:133`).
- **Why:** Anchor programs don't have Solidity-style re-entrancy (the runtime serializes cross-program calls), so the bug class isn't reachable today. But "update invariants before external calls" is a free habit that keeps the on-chain state consistent even when a CPI fails mid-transaction — and means the program reads the same way as Solidity code reviewed under the *checks-effects-interactions* rule.
- **Benefit:** Aligns with the security idiom every auditor knows. Cheap insurance.
- **Tradeoff:** Slight readability cost — the `total_raised` write isn't visually adjacent to the `contributor` close in the handler body.

---

## CPI & program reuse

### CPI authority signer seeds consolidated (diff §C1)

- **What:** Moved the seed byte-strings (`b"fundraiser"`, `b"vault"`, `b"contributor"`) into module-level constants (`03-optimized.rs:14`–`16`) so every PDA derivation references the same source.
- **Why:** Most PDA bugs are typos in seed strings — a wrong byte in one of two places where the same seed is referenced. Consolidating to one constant removes the divergence risk.
- **Benefit:** Hard to silently divergence. Reviewers scan one place to audit the program's PDA namespace.
- **Tradeoff:** None.

---

## Compute & rent

### `MAX_CONTRIBUTORS` cap deleted (diff §R1)

- **What:** Removed the `const MAX_CONTRIBUTORS: usize = 50` cap (`02-naive-port.rs:17`).
- **Why:** Required only because the `Vec<Contribution>` had to fit inside one account. With per-supporter PDAs, the cap has no purpose.
- **Benefit:** Unbounded scale. No `TooManyContributors` error to handle in the client.
- **Tradeoff:** None.

### `Fundraiser` account shrinks 23× (diff §R2)

- **What:** `Fundraiser::INIT_SPACE` drops from ~2094 bytes (with 50-entry Vec) to 90 bytes (`03-optimized.rs:236`–`245`).
- **Why:** Per-supporter state moved out. The fixed-size account is the cheap, hot-path-friendly singleton; per-supporter rent is paid by supporters on first contribution.
- **Benefit:** Smaller hot account means smaller compute on every contribute/claim/refund. Creator's initial rent burden drops from ~0.014 SOL to ~0.0009 SOL.
- **Tradeoff:** Aggregate rent across many supporters is higher, but it's distributed (each pays their own) and recovered on refund.

---

## Idioms

### `init_if_needed` for the contributor PDA (diff §I1)

- **What:** Use `init_if_needed` instead of two instructions (`register_contributor` + `contribute`) or a manual existence check (`03-optimized.rs:182`).
- **Why:** Solidity's `mapping(address => uint256) += amount` has no "create" step — the slot is implicitly present. The Solana analogue is `init_if_needed`, which creates the account on first call and is a no-op on subsequent calls.
- **Benefit:** One instruction does the work of two. The client never has to know whether it's the supporter's first contribution.
- **Tradeoff:** `init_if_needed` requires explicit opt-in via the `init-if-needed` Anchor feature; teams sometimes ban it because it's easier to forget account-collision checks. Here it's safe because the PDA seeds bind the account to a specific `(fundraiser, supporter)` pair — no second caller can hijack the slot.
