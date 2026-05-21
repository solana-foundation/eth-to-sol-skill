# Explanation log: `02-naive-port.rs` → `03-optimized.rs`

One entry per change in `04-diff.md`, grouped by theme. Each entry follows the schema: **What / Why / Benefit / Tradeoff**.

This example translates a textbook ERC-20 atomic swap — two parties, one trade, no trust — into the canonical `program-examples/tokens/escrow` shape, and teaches three Solana ideas a Solidity developer has to internalize:

1. **Per-entity PDAs replace `mapping(id => entity)`.** In Solidity the offer book is `mapping(uint256 => Offer)`; in Solana there is no mapping, so the same logical book becomes one *account* per offer, addressed deterministically from `(maker, id)`. PDAs (Program-Derived Addresses) are how you "key" data on Solana.
2. **Per-instance authority PDAs scope blast radius.** The PDA that signs token transfers out of an offer's vault is itself per-offer. A bug in one offer's release path cannot drain another offer's funds, because the signing identity is cryptographically bound to one offer.
3. **`init` + `close` is the state lifecycle.** Solidity tracks "open / cancelled / taken" with a status field inside a mapping entry. Solana lets accounts come into and go out of existence; the existence of the `Offer` account *is* the offer's "open" status. No flag needed.

Vocabulary that comes up below, with EVM analogs:

- **PDA** (Program-Derived Address) — a deterministic on-chain account address derived from a list of byte "seeds" the program controls. Closest Solidity analog: a storage slot's location, derived from `keccak256(key, slot)`. The crucial difference is that a PDA refers to a whole *account* on-chain (with its own ownership, rent, and lifecycle), not a slot inside the program's storage.
- **rent** — A refundable SOL deposit every account pays to live on-chain. When the account is closed, the deposit goes back to whoever you specify in the close constraint.
- **CPI** (cross-program invocation) — Solana's version of one contract calling another. Every account the callee will touch must already be listed in the caller's transaction.
- **SPL Token / Token Account / vault** — The shared on-chain token program. A "Token Account" is one user's balance for one specific token. The "vault" in an escrow is just a Token Account that the offer's PDA controls instead of a user.
- **Anchor** — The framework around the lower-level Solana program API; provides macros, account validation, and the IDL (Hardhat-to-EVM analog).
- **`init` / `close = X`** — Anchor account-creation and account-destruction constraints. `init` creates the account at the start of the instruction and reverts if it already exists; `close = X` deletes the account at the end of the instruction and refunds its rent to `X`. No Solidity equivalent — Solidity storage slots can't be allocated or freed.
- **`has_one = X`** — An Anchor constraint that says "the account's stored `X` field must equal the `X` account passed into this instruction". Declarative form of `require(state.X == x)`.

After first use, each term is fair game.

---

## State model

### Global `EscrowState` with `Vec<Offer>` → per-offer PDA (diff §S1)

- **What:** Removed the `EscrowState` account entirely (`02-naive-port.rs:313`). Each offer is now a standalone account — a PDA derived from `[b"offer", maker.as_ref(), &id.to_le_bytes()]` (`03-optimized.rs:322`). One account per offer, addressable deterministically from the maker plus an id the maker chooses.
- **Why:** In Solidity, `mapping(uint256 => Offer)` puts every offer at a deterministic storage slot inside one contract; you look up `offers[42]` and get the slot back. The Solana equivalent is one *account* per offer — a PDA (Program-Derived Address; same "deterministic address from a key" idea, but each entry is its own on-chain account at the derived address). The naive port tried to imitate the Solidity layout by stuffing a `Vec<Offer>` into one shared state account. That works mechanically but breaks Solana's parallel-execution model: the runtime locks every writable account a transaction touches, so two takers fulfilling two unrelated offers would serialize on the shared state account. Per-offer PDAs put each offer on its own account, so writes to different offers don't block each other.
- **Benefit:** Unbounded open offers — the naive's `MAX_OFFERS = 50` cap is gone. Two unrelated offers' take/cancel transactions run in parallel on Solana's scheduler (no shared writable surface). Every take/cancel touches only that offer's own account, not the whole book. Adding `maker` to the seeds means each maker has their own id-space — they can't accidentally collide with another maker's id.
- **Tradeoff:** Two accounts created per offer (the Offer PDA + a vault Token Account that holds the escrowed tokens) instead of one global state account. Both refund their rent on close, so the steady-state footprint is identical — only the maker's temporary deposit while the offer is open changes. This is standard Solana UX: callers pay rent for the state they create, and recover it when the state goes away.

### `next_offer_id` counter deleted (diff §S2)

- **What:** Removed the `next_offer_id: u64` field and its `+= 1` increment (`02-naive-port.rs:64`). The maker now supplies an `id: u64` as an instruction argument (`03-optimized.rs:37`).
- **Why:** A global counter is a write-hot field by definition — every `make_offer` increments it, which forces every concurrent maker to serialize on the state account holding the counter. Solidity has no choice (its storage is contract-global, and there's no parallel execution to lose anyway); Solana does. Letting the maker pick the id removes the last shared writable surface in the program.
- **Benefit:** No global serialization point. Re-init protection is still enforced — Anchor's `init` constraint refuses to create a PDA at an address that already exists, so if the maker reuses an id they've used before, the instruction reverts at account validation before the handler body runs. The seeds `[b"offer", maker, id]` mean each maker has their own id-space; they can't conflict with another maker.
- **Tradeoff:** Clients must pick ids. In practice they generate them trivially (`Date.now()` or `crypto.randomBytes(8)` works); document the convention. The EVM equivalent of "I trust the client to pick a unique nonce" is mildly unusual to Solidity developers used to contract-side auto-increment, but the cost of getting it wrong is just a clear instruction revert.

### Shared vault Token Account → per-offer vault (diff §S3)

- **What:** Replaced the singleton vault Token Account (one account holding *all* escrowed tokens) with one Token Account per offer, derived from `[b"vault", offer.key().as_ref()]` (`03-optimized.rs:206`–`214`). The vault is itself a PDA.
- **Why:** A Token Account on Solana is one balance for one specific token, owned by one specific authority. In the naive design, if two open offers escrow the same token (say both escrow USDC), the escrowed balances sit in the same Token Account. The program tracks per-offer amounts in its state, but the on-chain balance has no notion of which lamport belongs to which offer. A single arithmetic bug — wrong amount calculated, wrong offer's amount read — could let a take instruction drain tokens from an unrelated offer that happens to share the same vault. Splitting into per-offer vaults makes the safety property cryptographic instead of program-arithmetic: the vault for offer X *cannot hold* tokens that belong to offer Y.
- **Benefit:** Tokens are physically segregated per offer. A bug or compromise in the take logic for one offer can only move that offer's tokens — there's no commingled pool to drain.
- **Tradeoff:** Extra rent per offer (~0.002 SOL for the vault Token Account). Refunded on take or cancel via `token::close_account` (the SPL Token program's "destroy this account, return its rent" instruction). Net cost: rent paid only during the offer's lifetime, recovered when it ends.

---

## Parallelism

### Cross-offer activity fully parallel (diff §P1)

- **What:** No instruction in the optimized version writes any shared account. Each `make_offer` / `take_offer` / `cancel_offer` writes only the offer's own `{offer, vault}` PDAs plus the user's own token accounts.
- **Why:** Solana's runtime executes transactions in parallel when their writable account sets are disjoint — a key difference from EVM's strictly serialized execution. With no singleton state account, no global counter, and no shared vault, two unrelated offers have completely disjoint footprints. The scheduler picks them up on different cores.
- **Benefit:** Throughput scales with offer count instead of being floored by a single hot account. Two unrelated makers creating offers run concurrently. Two unrelated takers fulfilling offers run concurrently. This is the cleanest parallelism profile across the curated examples — no residual contention floor of any kind.
- **Tradeoff:** None. The naive design's singleton state was a pure cost, not a feature; this is the move you make every time the source design is `mapping(id => entity)`.

### O(n) Vec scans eliminated (diff §P2)

- **What:** Removed the `iter().position(|o| o.id == id)` lookups (`02-naive-port.rs:99`, `:166`) — the naive scanned the offer list linearly to find the matching id. The optimized version derives the PDA address directly from `(maker, id)` and Anchor loads exactly that account.
- **Why:** A linear scan inside an account costs compute units (Solana's gas equivalent) linearly in the number of open offers, *and* requires the full Vec to be deserialized at instruction entry (Anchor loads the entire account into memory). PDA derivation is O(1) and the offer's own ~130-byte account is loaded instead of the naive's ~6 KB state account.
- **Benefit:** Constant-time lookup regardless of how many offers are open. No `MAX_OFFERS` cap.
- **Tradeoff:** None.

---

## Security

### PDA bumps cached + canonicalization enforced (diff §Sec1)

- **What:** Both `Offer.bump` and `Offer.vault_authority_bump` are stored on the Offer account at init time and used via `bump = offer.bump` in account validation rather than being re-derived per call. The "bump" is a nonce that makes a PDA valid (the EVM analog would be "the extra byte you'd add to `keccak256` if the result happened to be on the secp256k1 curve").
- **Why:** Two reasons:
  1. **Compute.** `Pubkey::find_program_address` (the bump derivation) is expensive — about 1500 compute units per call. Storing the canonical bump once and reading it on every subsequent instruction saves that on every call.
  2. **Security.** A PDA's address has a *canonical* bump (the highest valid one), but technically several bumps can produce valid PDAs from the same seeds. An instruction that accepts any valid bump opens a class of bug where an attacker passes a non-canonical bump and the program signs for a different address than the one it thinks. Storing the canonical bump pins the PDA identity at init time and refuses anything else.
- **Benefit:** Saves ~1500 CU per instruction; structurally eliminates the non-canonical-bump bug class. The full pattern is documented in `security/pda-canonicalization.md`.
- **Tradeoff:** One `u8` of account space per stored bump. Negligible.

### Per-offer vault authority (diff §Sec2)

- **What:** The `vault_authority` PDA's seeds changed from `[b"vault_authority"]` (singleton across all offers) to `[b"vault_authority", offer.key().as_ref()]` (one per offer) (`03-optimized.rs:189`–`193`). The vault authority is the PDA that signs `token::transfer` CPIs to move tokens out of an offer's vault.
- **Why:** If a bug in `take_offer` causes the program to transfer the wrong amount or to send to the wrong recipient, the damage should be bounded to the offer in question — not "any offer that shares the same vault authority signer". Per-offer seeds enforce that bound at the cryptographic level: when the program signs a CPI with `signer_seeds = [b"vault_authority", offer_X.key()]`, that signature is only valid for transfers out of offer X's vault. It cannot accidentally move tokens belonging to offer Y, because Y's vault is owned by a different PDA derived from `offer_Y.key()`.
- **Benefit:** Blast radius is isolated by construction. A bug that exfiltrates funds from one offer cannot reach any other. This is the Solana equivalent of "least-privilege signing" applied to on-chain accounts.
- **Tradeoff:** One more byte stored per Offer (`vault_authority_bump`). Negligible.

### `has_one` for maker (diff §Sec3)

- **What:** Replaced runtime `require_keys_eq!(offer.maker, signer.key())` checks (`02-naive-port.rs:170`) with a `has_one = maker` constraint on the Offer account (`03-optimized.rs:236`, `:285`). `has_one = maker` tells Anchor "the account's `maker` field must equal the `maker` account passed into this instruction" — the declarative form of the runtime check.
- **Why:** Declarative constraints run before the handler body and surface in the program's IDL (so off-chain tooling can read access-control rules without parsing function bodies). They're harder to forget when adding new instructions and easier to audit — a reviewer reads the struct definition to see who can call what, the same way they'd scan for `onlyOwner` modifiers in Solidity.
- **Benefit:** Fewer foot-guns. The access-control rule is visible at the top of the struct, not buried in instruction logic.
- **Tradeoff:** None. The constraint compiles to the same check; it just lives in a better place.

### Mint cross-validation as constraints (diff §Sec4)

- **What:** Three runtime `require_keys_eq!` checks comparing the offered/wanted token mints against the offer's stored mints were replaced with two `constraint = offer.token_X == token_X_mint.key()` declarations on the account struct (`03-optimized.rs:238`–`239`).
- **Why:** Same reasoning as §Sec3 — declarative beats runtime when the rule is "this account's field must match that account's field". Runs before the handler executes; visible in the IDL.
- **Benefit:** Audit surface narrows from "read every function body" to "read the struct definitions".
- **Tradeoff:** None.

### Unchecked counter increment removed (diff §Sec5)

- **What:** `state.next_offer_id += 1` is deleted (no counter exists in the optimized version, per §S2).
- **Why:** Bare arithmetic on a state field — `x += 1`, `total -= amount`, etc. — is a code smell on Solana regardless of how unreachable the overflow case is. The standard is `checked_add` / `checked_sub` returning `Option<T>`, unwrapped with `.ok_or(MyError::Overflow)?` (see `security/arithmetic.md`). Even when the value is unreachable in practice (`u64::MAX` is 1.8e19 offers, more than the universe has time for), the pattern fails code review and would be a real bug had the field been narrower.
- **Benefit:** One fewer arithmetic site to audit. The class of bug is deleted, not just patched.
- **Tradeoff:** None.

---

## CPI & program reuse

### Atomic `take_offer` with vault closure (diff §C1)

- **What:** `take_offer` now includes a `token::close_account` CPI (`03-optimized.rs:124`–`133`) that closes the vault Token Account after draining it, and Anchor's `close = maker` constraint (`:242`) closes the Offer PDA itself. Both accounts' rent deposits refund to the maker.
- **Why:** In the naive design, vault Token Accounts persist forever after the offer is fulfilled — their rent deposits are stranded forever. Closing them as part of the take matches the intent of the instruction ("the offer is over, clean up"). On Solana you can structure cleanup as part of the same atomic transaction; on Solidity, the analog would be `selfdestruct` (deprecated and never really applicable to mappings anyway).
- **Benefit:** Maker recovers ~0.0035 SOL of rent on every fulfilled offer (~0.0015 SOL for the Offer PDA + ~0.002 SOL for the vault Token Account). The protocol's on-chain footprint goes back to zero when offers complete.
- **Tradeoff:** The take instruction has more account writes (close operations on two accounts). Marginal compute-unit cost. Worth it.

### Atomic `cancel_offer` (diff §C2)

- **What:** Same close pattern as take — vault closed via `token::close_account` CPI, Offer closed via `close = maker` constraint (`03-optimized.rs:174`–`183`).
- **Why / Benefit / Tradeoff:** Same as §C1.

---

## Compute & rent

### No global state → no protocol-level rent (diff §R1)

- **What:** Removed `EscrowState` entirely (about 6 KB at full Vec capacity, costing ~0.043 SOL of rent that the protocol deployer pays forever).
- **Why:** With no global counter and per-offer PDAs, nothing belongs on a singleton account anymore. The protocol has no persistent shared state.
- **Benefit:** Protocol carries no permanent rent burden. All rent is per-offer and refunded on close, so the protocol pays nothing at steady state.
- **Tradeoff:** None.

### Per-call data load ~25× smaller (diff §R2)

- **What:** Each instruction loads ~300 bytes of program state instead of the naive's ~6 KB.
- **Why:** Anchor deserializes account data on instruction entry (it has to, in order to type-check it). Smaller accounts = faster instruction entry = fewer compute units burned before any user logic runs.
- **Benefit:** Several thousand compute units saved per call (a meaningful fraction of the 200K-CU per-instruction budget on Solana).
- **Tradeoff:** None.

---

## Idioms

### `init` + `close` as state lifecycle (diff §I1)

- **What:** The Offer account exists exactly while the offer is open. `init` creates it on `make_offer`, `close = maker` deletes it on `take_offer` or `cancel_offer`. The vault Token Account mirrors this lifecycle via `init` and `token::close_account` CPIs.
- **Why:** In Solidity, entries in a `mapping` are always present — there's no way to delete them; "cleared" entries just hold zero values, and you need an `if status == Cleared` flag elsewhere to track what's actually live. Solana lets accounts come into and go out of existence, so "is this offer still open?" becomes "does this account exist?". The runtime tells you for free at account validation, before any handler code runs.
- **Benefit:** No bookkeeping bugs around "did we forget to clear this flag?". The runtime gives you the lifecycle property structurally.
- **Tradeoff:** Reading historical offers (after they've been closed) requires either program logs or an off-chain indexer, since the on-chain account is gone. Standard Solana UX; protocols that need historical state typically emit events (Anchor's `emit!` macro writes them to transaction logs) for indexers to pick up.

### `id` supplied by the maker (diff §I2)

- **What:** `make_offer` takes `id: u64` as its first argument; clients pick the value (timestamp, random nonce, etc.).
- **Why:** A global counter would force every `make_offer` to serialize on the state account holding the counter (see §S2). Per-`(maker, id)` PDA addressing means each maker has their own id-space; no coordination is needed across makers.
- **Benefit:** Drops the last shared mutable field in the program. ID generation is one line of TypeScript on the client.
- **Tradeoff:** Clients must pick ids. Conventionally a timestamp or random bytes. If the maker reuses an id they've already used and the previous offer is still open, the instruction reverts cleanly at account validation (`init` fails because the PDA exists).

### Maker is not a signer on `take_offer` (diff §I3)

- **What:** The `maker` field on the take struct is an `UncheckedAccount` rather than a `Signer` (`03-optimized.rs:266`–`267`); only the taker signs. The maker is just identified, not authenticated, on take.
- **Why:** Atomic-swap correctness requires that takers can fulfil an offer without the maker being online. The maker's identity is still asserted: `has_one = maker` on the Offer (§Sec3) checks that the maker account passed in matches the Offer's stored maker, and `token::authority = maker` on the maker's wanted-token-receiving account confirms the maker controls where the taker's payment lands. So we authenticate where it matters (token destination ownership) without forcing the maker to co-sign.
- **Benefit:** This is the whole point of an escrow protocol — the maker drops an offer and walks away; the taker fulfils later without coordination. Mirrors the Solidity contract's same property.
- **Tradeoff:** None.

### Error variants tightened (diff §I4)

- **What:** `OfferDoesNotExist` and `TooManyOffers` deleted; `MakerMismatch` clarified.
- **Why:** Anchor's `init` constraint makes "offer doesn't exist on take" a structural impossibility — the PDA either exists (Anchor loads it) or doesn't (the instruction reverts at account validation, before the handler body runs). The error doesn't need to live in the program's error enum because it can't be reached in the handler. `TooManyOffers` was an artifact of the Vec cap that no longer exists.
- **Benefit:** Failure modes that can't happen don't need error codes. Smaller surface for clients to handle.
- **Tradeoff:** None.
