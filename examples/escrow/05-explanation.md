# Explanation log: `02-naive-port.rs` → `03-optimized.rs`

One entry per change in `04-diff.md`, grouped by theme. Each entry follows the schema: **What / Why / Benefit / Tradeoff**.

This example teaches the cleanest possible version of three Solana ideas:

1. **Per-entity PDAs.** A Solidity `mapping(id => entity)` doesn't translate to a `Vec` on a state account — it translates to one PDA per id. This example has no other state.
2. **Per-instance authority PDAs.** When a program holds funds for distinct purposes, each purpose gets its own signing PDA. A bug in one purpose's release path cannot drain another's.
3. **`init` + `close` as state lifecycle.** Solidity tracks live/cancelled status in a mapping entry. Solana lets accounts come into and go out of existence; the existence of an `Offer` account IS the status of the offer.

The source contract is a textbook atomic ERC-20 swap. The Solana version mirrors the canonical `program-examples/tokens/escrow` shape.

---

## State model

### Global `EscrowState` with `Vec<Offer>` → per-offer PDA (diff §S1)

- **What:** Removed the `EscrowState` account entirely (`02-naive-port.rs:313`). Each offer is now a standalone PDA with seeds `[b"offer", maker.as_ref(), &id.to_le_bytes()]` (`03-optimized.rs:322`).
- **Why:** Solidity's `mapping(uint256 => Offer)` is one storage slot per id, addressable by id inside one contract. The Solana equivalent is one account per id, addressable by PDA derivation from `(maker, id)`. A `Vec` inside a state account is the wrong primitive: it bounds the offer count, forces every operation to write the state account, and offers no security benefit over the per-PDA pattern.
- **Benefit:** Unbounded open offers (the naive `MAX_OFFERS = 50` cap is gone). No serialization of cross-offer activity. Every take/cancel touches only the offer's own account. `Offer.maker` participation in the seeds means makers can run any number of concurrent offers without colliding on ids unless they reuse one themselves.
- **Tradeoff:** Two accounts created per offer (the Offer PDA + a vault TokenAccount) instead of one global. Both refund their rent on close, so the steady-state footprint is identical — only the maker's temporary deposit changes. Standard Solana UX.

### `next_offer_id` counter deleted (diff §S2)

- **What:** Removed the `next_offer_id: u64` field and its increment (`02-naive-port.rs:64`). The maker now supplies an `id: u64` as an instruction argument (`03-optimized.rs:37`).
- **Why:** A global counter is a write-hot field by definition — every `make_offer` increments it, forcing every concurrent maker to serialize on the state account. Solidity has no choice (its storage is contract-global); Solana does. Letting the maker pick the id (typically a client-side timestamp or random nonce) removes the last shared writable surface.
- **Benefit:** No global serialization point. Re-init protection comes from Anchor's `init` constraint against the seeds — if the maker reuses an id they've already used, the instruction fails at account validation before the handler runs.
- **Tradeoff:** Clients must pick ids. In practice they generate them trivially (`Date.now()` or `crypto.randomBytes(8)`); document the convention.

### Shared vault TokenAccount → per-offer vault (diff §S3)

- **What:** Replaced the singleton vault TokenAccount with one per offer, derived as `[b"vault", offer.key().as_ref()]` (`03-optimized.rs:206`–`214`).
- **Why:** In the naive design, if two open offers both escrow the same mint (e.g. both escrow USDC), their escrowed balances commingle in one TokenAccount. The program tracks per-offer amounts in state, but the on-chain TokenAccount can't tell which lamport belongs to which offer. A single bug — wrong amount calculated, wrong offer's amount read — can drain unrelated offers' escrowed funds.
- **Benefit:** Tokens are physically segregated per offer. A bug or compromise that mis-signs a vault transfer can only move that offer's tokens, not anything else.
- **Tradeoff:** Extra rent per offer (~0.002 SOL for the vault TokenAccount). Refunded on take/cancel via `token::close_account`. Net cost: only during the offer's lifetime.

---

## Parallelism

### Cross-offer activity fully parallel (diff §P1)

- **What:** No instruction writes any shared account. Each `make_offer` / `take_offer` / `cancel_offer` writes only `{offer, vault}` plus the user's own ATAs.
- **Why:** Solana parallelizes transactions whose writable account sets are disjoint. With no singleton state account, no global counter, no shared vault — different offers have completely disjoint footprints.
- **Benefit:** Throughput scales with usage. Two unrelated makers creating offers run in parallel. Two unrelated takers fulfilling offers run in parallel. This is the cleanest parallelism profile across the four reference examples — no residual contention floor.
- **Tradeoff:** None. The naive design's singleton state was a pure cost, not a feature.

### O(n) Vec scans eliminated (diff §P2)

- **What:** Removed the `iter().position(|o| o.id == id)` lookups (`02-naive-port.rs:99`, `:166`).
- **Why:** Linear scan inside an account costs CU linearly in the number of open offers AND requires the full Vec to be deserialized on entry. PDA derivation is O(1) and the offer's own ~130-byte account is loaded instead of the 6 KB state.
- **Benefit:** Constant-time lookup. No `MAX_OFFERS` cap.
- **Tradeoff:** None.

---

## Security

### PDA bumps cached + canonicalization enforced (diff §Sec1)

- **What:** Both `Offer.bump` and `Offer.vault_authority_bump` are stored on the account at init time and used via `bump = offer.bump` in account validation rather than being re-derived per call.
- **Why:** `Pubkey::find_program_address` is hot — re-deriving the bump on every instruction costs ~1500 CU. More importantly, accepting any valid bump (vs. the canonical one) opens a class of bugs where an attacker passes a non-canonical bump and the program signs for a different address than the one it thinks. Storing the canonical bump pins the PDA identity at init.
- **Benefit:** Saves CU per instruction; eliminates the non-canonical-bump bug class. See `security/pda-canonicalization.md` for the full pattern.
- **Tradeoff:** One `u8` of account space per stored bump. Negligible.

### Per-offer vault authority (diff §Sec2)

- **What:** `vault_authority` seeds changed from `[b"vault_authority"]` (singleton) to `[b"vault_authority", offer.key().as_ref()]` (per-offer) (`03-optimized.rs:189`–`193`).
- **Why:** The vault authority is the PDA that signs SPL Token transfers OUT of the vault. If a bug in `take_offer` causes the program to transfer the wrong amount, or to send to the wrong recipient, the damage should be bounded to the offer in question — not "any offer that happens to be open at the same time." Per-offer seeds enforce that bound at the cryptographic level: signing with `offer.key()` only authorizes vault transfers for that one offer.
- **Benefit:** Blast radius isolation. A bug that exfiltrates funds from one offer cannot reach another.
- **Tradeoff:** One more byte stored per Offer (`vault_authority_bump`). Negligible.

### `has_one` for maker (diff §Sec3)

- **What:** Replaced runtime `require_keys_eq!(offer.maker, signer.key())` with `has_one = maker` on the Offer account constraint (`03-optimized.rs:236`, `:285`).
- **Why:** Declarative constraints run before the handler body and surface in the IDL. They're harder to forget on new instructions and visible to clients without reading function bodies. Standard Anchor idiom from the other examples.
- **Benefit:** Fewer foot-guns. Auditable from the struct definition alone.
- **Tradeoff:** None.

### Mint cross-validation as constraints (diff §Sec4)

- **What:** Three runtime `require_keys_eq!` checks on token mints replaced with two `constraint = offer.token_X == token_X_mint.key()` declarations (`03-optimized.rs:238`–`239`).
- **Why:** Same reasoning as Sec3. Declarative beats runtime for "this account's field must match that account's field."
- **Benefit:** Audit surface narrows from "read the handler body" to "read the struct definition."
- **Tradeoff:** None.

### Unchecked counter increment removed (diff §Sec5)

- **What:** `state.next_offer_id += 1` deleted (no counter exists in the optimized version).
- **Why:** Bare arithmetic on a state field is a code-smell, period. Even when the value is unreachable in practice (`u64::MAX = 1.8e19` offers), the pattern fails code review and would have been a real bug had the field been narrower.
- **Benefit:** One fewer arithmetic site to audit. Class of bug deleted, not just patched.
- **Tradeoff:** None.

---

## CPI & program reuse

### Atomic `take_offer` with vault closure (diff §C1)

- **What:** `take_offer` now includes a `token::close_account` CPI (`03-optimized.rs:124`–`133`) that closes the vault TokenAccount after draining it, and Anchor's `close = maker` constraint (`:242`) closes the Offer account itself. Both rent deposits refund to the maker.
- **Why:** In the naive design, vault TokenAccounts persist forever after the offer is fulfilled — their lamports are stranded. Closing them as part of the take instruction matches the "the offer is over, clean up" intent.
- **Benefit:** Maker recovers ~0.0035 SOL of rent on every fulfilled offer (Offer + vault). Protocol's on-chain footprint goes to zero when offers complete.
- **Tradeoff:** The take instruction has more account writes (close operations). Marginal CU. Worth it.

### Atomic `cancel_offer` (diff §C2)

- **What:** Same close pattern as take — vault closed via `token::close_account`, Offer closed via `close = maker` (`03-optimized.rs:174`–`183`).
- **Why / Benefit / Tradeoff:** Same as C1.

---

## Compute & rent

### No global state → no protocol-level rent (diff §R1)

- **What:** Removed `EscrowState` (~6 KB, ~0.043 SOL rent) entirely.
- **Why:** With no global counter and per-offer PDAs, nothing belongs on a singleton account anymore.
- **Benefit:** Protocol carries no permanent rent burden. All rent is per-offer and refunded on close.
- **Tradeoff:** None.

### Per-call data load ~25× smaller (diff §R2)

- **What:** Each instruction loads ~300 bytes of program state instead of ~6 KB.
- **Why:** Anchor deserializes account data on instruction entry. Smaller accounts = faster instruction entry.
- **Benefit:** Multi-kCU saved per call.
- **Tradeoff:** None.

---

## Idioms

### `init` + `close` as state lifecycle (diff §I1)

- **What:** The Offer account exists exactly while the offer is open. `init` at make, `close = maker` at take/cancel. Vault TokenAccount mirrors this lifecycle via `init` and `token::close_account` CPIs.
- **Why:** Solidity has no equivalent — entries in a `mapping` are always there, just zero-valued for "cleared" entries. Solana lets accounts come and go. Conflating "account exists" with "offer is open" eliminates the need for any "is_active" flag.
- **Benefit:** No bookkeeping bugs around "did we forget to clear this?" The runtime gives you the answer for free: ask "does the account exist?".
- **Tradeoff:** Reloading historical offers (after they've been closed) requires reading from program logs or an off-chain indexer, since the on-chain account is gone. Standard Solana UX; protocols that need historical data emit events.

### `id` supplied by the maker (diff §I2)

- **What:** `make_offer` takes `id: u64` as its first argument. Clients pick the value.
- **Why:** A global counter would force serialization on the state account. Per-(maker, id) PDA addressing means each maker has their own id-space; no coordination is needed.
- **Benefit:** Drops the last shared mutable field. Trivial id generation client-side.
- **Tradeoff:** Clients must pick ids. Conventionally a timestamp or random nonce — one line of TypeScript.

### Maker is not a signer on `take_offer` (diff §I3)

- **What:** Maker is an `UncheckedAccount` in the take struct (`03-optimized.rs:266`–`267`); only the taker signs.
- **Why:** Atomic-swap correctness demands that takers can fulfil without maker participation. Maker's identity is asserted via `has_one = maker` on the offer; maker's ownership of the wanted-token-receiving account is asserted via `token::authority = maker` on that account.
- **Benefit:** The whole point of an escrow protocol — the maker doesn't have to be online for the take.
- **Tradeoff:** None.

### Error variants tightened (diff §I4)

- **What:** `OfferDoesNotExist` and `TooManyOffers` deleted; `MakerMismatch` clarified.
- **Why:** Anchor's `init` constraint makes "offer doesn't exist on take" structurally impossible — the PDA either exists (we read it) or doesn't (instruction reverts at account validation). `TooManyOffers` was an artifact of the Vec cap.
- **Benefit:** Failure modes that can't happen don't need error codes.
- **Tradeoff:** None.
