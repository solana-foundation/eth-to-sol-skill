# Explanation log: `02-naive-port.rs` → `03-optimized.rs`

One entry per change in `04-diff.md`, grouped by theme. Each entry follows the schema: **What / Why / Benefit / Tradeoff**.

This is a textbook two-asset atomic swap: a maker offers some amount of one ERC-20 in exchange for some amount of another; a taker who's willing to trade fulfills the offer atomically; either party can cancel before fulfillment. The Solidity version keeps an offer book at `mapping(uint256 => Offer)` inside the contract plus a `nextOfferId` counter, escrows tokens with `transferFrom`, and tracks each offer's lifecycle with a status field that auth checks gate on.

On Solana the same protocol becomes one `Offer` PDA per offer (derived from `[b"offer", maker, id]`) plus a per-offer vault Token Account that holds the escrowed tokens. There's no offer-book contract — the offers exist as standalone accounts that come into existence on `make_offer` and get deleted on `take_offer` / `cancel_offer`, so account existence *is* the lifecycle and no status field is needed. The reference Solana shape is `tokens/escrow` in solana-developers/program-examples.

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

### `mapping(uint256 => Offer)` offer book → per-offer PDA (diff §S1)

- **What:** Removed the `EscrowState` account entirely (`02-naive-port.rs:313`). Each offer is now a standalone account — a PDA derived from `[b"offer", maker.as_ref(), &id.to_le_bytes()]` (`03-optimized.rs:322`). One account per offer, addressable deterministically from the maker plus an id the maker chooses.
- **Annotation:** This program has no central offer book — each offer owns its own `Offer` account derived from `[b"offer", maker, id]`. Solidity would keep the offers at `mapping(uint256 => Offer)` slots inside a single contract; on Solana the runtime locks every writable account a transaction touches, so a shared offer book would make two takers fulfilling unrelated offers serialize on the same state account. Per-offer PDAs keep the writable sets disjoint and unlock parallel execution.
- **Why:** In Solidity, `mapping(uint256 => Offer)` puts every offer at a deterministic storage slot inside one contract; you look up `offers[42]` and get the slot back. The Solana equivalent is one *account* per offer — a PDA (Program-Derived Address; same "deterministic address from a key" idea, but each entry is its own on-chain account at the derived address). The naive port tried to imitate the Solidity layout by stuffing a `Vec<Offer>` into one shared state account. That works mechanically but breaks Solana's parallel-execution model: the runtime locks every writable account a transaction touches, so two takers fulfilling two unrelated offers would serialize on the shared state account. Per-offer PDAs put each offer on its own account, so writes to different offers don't block each other.
- **Benefit:** Unbounded open offers — the naive's `MAX_OFFERS = 50` cap is gone. Two unrelated offers' take/cancel transactions run in parallel on Solana's scheduler (no shared writable surface). Every take/cancel touches only that offer's own account, not the whole book. Adding `maker` to the seeds means each maker has their own id-space — they can't accidentally collide with another maker's id.
- **Tradeoff:** Two accounts created per offer (the Offer PDA + a vault Token Account that holds the escrowed tokens) instead of one global state account. Both refund their rent on close, so the steady-state footprint is identical — only the maker's temporary deposit while the offer is open changes. This is standard Solana UX: callers pay rent for the state they create, and recover it when the state goes away.

### Contract-side `++nextOfferId` → client-supplied id (diff §S2)

- **What:** Removed the `next_offer_id: u64` field and its `+= 1` increment (`02-naive-port.rs:64`). The maker now supplies an `id: u64` as an instruction argument (`03-optimized.rs:37`).
- **Annotation:** The maker passes their own `id` into `make_offer`; nothing in the program increments a shared counter. A global counter would force every concurrent maker to serialize on whichever account stored it — Solidity has no parallel execution to lose anyway, so contract-side auto-increment is free there, but on Solana a write-hot counter is the easiest way to kill throughput. Re-init protection is preserved by Anchor's `init` constraint: reusing an id reverts at account validation because the PDA already exists.
- **Why:** A global counter is a write-hot field by definition — every `make_offer` increments it, which forces every concurrent maker to serialize on the state account holding the counter. Solidity has no choice (its storage is contract-global, and there's no parallel execution to lose anyway); Solana does. Letting the maker pick the id removes the last shared writable surface in the program.
- **Benefit:** No global serialization point. Re-init protection is still enforced — Anchor's `init` constraint refuses to create a PDA at an address that already exists, so if the maker reuses an id they've used before, the instruction reverts at account validation before the handler body runs. The seeds `[b"offer", maker, id]` mean each maker has their own id-space; they can't conflict with another maker.
- **Tradeoff:** Clients must pick ids. In practice they generate them trivially (`Date.now()` or `crypto.randomBytes(8)` works); document the convention. The EVM equivalent of "I trust the client to pick a unique nonce" is mildly unusual to Solidity developers used to contract-side auto-increment, but the cost of getting it wrong is just a clear instruction revert.

### Shared `balanceOf[contract]` custody → per-offer vault (diff §S3)

- **What:** Replaced the singleton vault Token Account (one account holding *all* escrowed tokens) with one Token Account per offer, derived from `[b"vault", offer.key().as_ref()]` (`03-optimized.rs:206`–`214`). The vault is itself a PDA.
- **Annotation:** Every offer escrows its tokens into its own per-offer vault Token Account, derived from `[b"vault", offer]`. A Token Account on Solana is one balance for one authority — a shared vault would commingle every open offer's escrowed tokens in the same balance, and a bug in `take_offer` (wrong amount, wrong offer read) could let one taker drain another offer's funds. Per-offer vaults make the isolation cryptographic: the vault for offer X simply cannot hold tokens belonging to offer Y.
- **Why:** A Token Account on Solana is one balance for one specific token, owned by one specific authority. In the naive design, if two open offers escrow the same token (say both escrow USDC), the escrowed balances sit in the same Token Account. The program tracks per-offer amounts in its state, but the on-chain balance has no notion of which lamport belongs to which offer. A single arithmetic bug — wrong amount calculated, wrong offer's amount read — could let a take instruction drain tokens from an unrelated offer that happens to share the same vault. Splitting into per-offer vaults makes the safety property cryptographic instead of program-arithmetic: the vault for offer X *cannot hold* tokens that belong to offer Y.
- **Benefit:** Tokens are physically segregated per offer. A bug or compromise in the take logic for one offer can only move that offer's tokens — there's no commingled pool to drain.
- **Tradeoff:** Extra rent per offer (~0.002 SOL for the vault Token Account). Refunded on take or cancel via `token::close_account` (the SPL Token program's "destroy this account, return its rent" instruction). Net cost: rent paid only during the offer's lifetime, recovered when it ends.

---

## Parallelism

### Cross-offer activity fully parallel (diff §P1)

- **What:** No instruction in the optimized version writes any shared account. Each `make_offer` / `take_offer` / `cancel_offer` writes only the offer's own `{offer, vault}` PDAs plus the user's own token accounts.
- **Annotation:** No instruction here writes a shared account — every write touches that offer's own `Offer` / vault PDA pair plus the user's own token accounts. Solana's runtime schedules transactions with disjoint writable sets in parallel (this isn't a thing in the EVM, where transactions are strictly serialized). A naive port keeping a singleton offer book would force unrelated takes to queue on the shared state account; here the scheduler runs them on different cores.
- **Why:** Solana's runtime executes transactions in parallel when their writable account sets are disjoint — a key difference from EVM's strictly serialized execution. With no singleton state account, no global counter, and no shared vault, two unrelated offers have completely disjoint footprints. The scheduler picks them up on different cores.
- **Benefit:** Throughput scales with offer count instead of being floored by a single hot account. Two unrelated makers creating offers run concurrently. Two unrelated takers fulfilling offers run concurrently. This is the cleanest parallelism profile across the curated examples — no residual contention floor of any kind.
- **Tradeoff:** None. The naive design's singleton state was a pure cost, not a feature; this is the move you make every time the source design is `mapping(id => entity)`.

### `offers[id]` lookup → direct PDA derivation (diff §P2)

- **What:** Removed the `iter().position(|o| o.id == id)` lookups (`02-naive-port.rs:99`, `:166`) — the naive scanned the offer list linearly to find the matching id. The optimized version derives the PDA address directly from `(maker, id)` and Anchor loads exactly that account.
- **Annotation:** There's no offer-lookup loop here — Anchor loads the exact `Offer` PDA the instruction's seeds derive from `(maker, id)`, and the handler operates directly on `ctx.accounts.offer`. A naive port scanning a `Vec<Offer>` on a shared state account would do work linear in the number of open offers AND would have to deserialize the whole multi-KB account every call. PDA derivation is O(1) and only the offer's own ~130-byte account is touched.
- **Why:** A linear scan inside an account costs compute units (Solana's gas equivalent) linearly in the number of open offers, *and* requires the full Vec to be deserialized at instruction entry (Anchor loads the entire account into memory). PDA derivation is O(1) and the offer's own ~130-byte account is loaded instead of the naive's ~6 KB state account.
- **Benefit:** Constant-time lookup regardless of how many offers are open. No `MAX_OFFERS` cap.
- **Tradeoff:** None.

---

## Security

### PDA bumps cached + canonicalization enforced (diff §Sec1)

- **What:** Both `Offer.bump` and `Offer.vault_authority_bump` are stored on the Offer account at init time and used via `bump = offer.bump` in account validation rather than being re-derived per call. The "bump" is a nonce that makes a PDA valid (the EVM analog would be "the extra byte you'd add to `keccak256` if the result happened to be on the secp256k1 curve").
- **Annotation:** The Offer struct stores `bump` and `vault_authority_bump` and every PDA constraint reads them via `bump = offer.bump` rather than re-deriving each call. Re-deriving costs ~1500 compute units per call (the analog of "the extra byte you'd add to `keccak256` if the result happened to be on the secp256k1 curve" — Solana keeps trying bumps until it finds a valid one). Pinning the canonical bump at init also closes a class of bug where an attacker passes a non-canonical bump and the program signs for a different address than it thinks it's signing for.
- **Why:** Two reasons:
  1. **Compute.** `Pubkey::find_program_address` (the bump derivation) is expensive — about 1500 compute units per call. Storing the canonical bump once and reading it on every subsequent instruction saves that on every call.
  2. **Security.** A PDA's address has a *canonical* bump (the highest valid one), but technically several bumps can produce valid PDAs from the same seeds. An instruction that accepts any valid bump opens a class of bug where an attacker passes a non-canonical bump and the program signs for a different address than the one it thinks. Storing the canonical bump pins the PDA identity at init time and refuses anything else.
- **Benefit:** Saves ~1500 CU per instruction; structurally eliminates the non-canonical-bump bug class. The full pattern is documented in `security/pda-canonicalization.md`.
- **Tradeoff:** One `u8` of account space per stored bump. Negligible.

### Per-offer vault authority (diff §Sec2)

- **What:** The `vault_authority` PDA's seeds changed from `[b"vault_authority"]` (singleton across all offers) to `[b"vault_authority", offer.key().as_ref()]` (one per offer) (`03-optimized.rs:189`–`193`). The vault authority is the PDA that signs `token::transfer` CPIs to move tokens out of an offer's vault.
- **Annotation:** The PDA that signs token transfers out of a vault is itself derived per-offer (`[b"vault_authority", offer]`), so the signing identity is cryptographically bound to one specific offer. Solidity doesn't have this notion — auth is usually a stored `address` checked at runtime, and one compromised owner key can move everything. Here a bug in `take_offer` for offer X can only sign for offer X's vault: the signature would be invalid against any other offer's vault authority.
- **Why:** If a bug in `take_offer` causes the program to transfer the wrong amount or to send to the wrong recipient, the damage should be bounded to the offer in question — not "any offer that shares the same vault authority signer". Per-offer seeds enforce that bound at the cryptographic level: when the program signs a CPI with `signer_seeds = [b"vault_authority", offer_X.key()]`, that signature is only valid for transfers out of offer X's vault. It cannot accidentally move tokens belonging to offer Y, because Y's vault is owned by a different PDA derived from `offer_Y.key()`.
- **Benefit:** Blast radius is isolated by construction. A bug that exfiltrates funds from one offer cannot reach any other. This is the Solana equivalent of "least-privilege signing" applied to on-chain accounts.
- **Tradeoff:** One more byte stored per Offer (`vault_authority_bump`). Negligible.

### `require(msg.sender == offer.maker)` → `has_one = maker` (diff §Sec3)

- **What:** Replaced runtime `require_keys_eq!(offer.maker, signer.key())` checks (`02-naive-port.rs:170`) with a `has_one = maker` constraint on the Offer account (`03-optimized.rs:236`, `:285`). `has_one = maker` tells Anchor "the account's `maker` field must equal the `maker` account passed into this instruction" — the declarative form of the runtime check.
- **Annotation:** Auth for `cancel_offer` is the `has_one = maker` constraint on the struct, not a `require!` line at the top of the handler. Solidity would check `require(msg.sender == offer.maker)` inside the function body; here Anchor verifies the same equality before any handler code runs and the rule surfaces in the program's IDL so off-chain tooling sees the access check too. Easier to audit — a reviewer scans the struct definition to see who can call what.
- **Why:** Declarative constraints run before the handler body and surface in the program's IDL (so off-chain tooling can read access-control rules without parsing function bodies). They're harder to forget when adding new instructions and easier to audit — a reviewer reads the struct definition to see who can call what, the same way they'd scan for `onlyOwner` modifiers in Solidity.
- **Benefit:** Fewer foot-guns. The access-control rule is visible at the top of the struct, not buried in instruction logic.
- **Tradeoff:** None. The constraint compiles to the same check; it just lives in a better place.

### `require(token == offer.token)` → struct-level constraint (diff §Sec4)

- **What:** Three runtime `require_keys_eq!` checks comparing the offered/wanted token mints against the offer's stored mints were replaced with two `constraint = offer.token_X == token_X_mint.key()` declarations on the account struct (`03-optimized.rs:238`–`239`).
- **Annotation:** The "mints passed in must match the offer's stored mints" rule lives on the Offer's struct as `constraint = offer.token_X == token_X_mint.key()`, not as `require_keys_eq!` lines inside `take_offer`. Declarative when the rule is "this account's field must match that account's field" beats runtime — the check runs before the handler executes and is visible in the IDL. Audit surface narrows from "read every handler body" to "read the struct definitions".
- **Why:** Same reasoning as §Sec3 — declarative beats runtime when the rule is "this account's field must match that account's field". Runs before the handler executes; visible in the IDL.
- **Benefit:** Audit surface narrows from "read every function body" to "read the struct definitions".
- **Tradeoff:** None.

### Unchecked counter increment removed (diff §Sec5)

- **What:** `state.next_offer_id += 1` is deleted (no counter exists in the optimized version, per §S2).
- **Annotation:** No `x += 1` bare arithmetic on a state field anywhere in this program. The standard on Solana is `checked_add` / `checked_sub` returning `Option<T>`, unwrapped with `.ok_or(MyError::Overflow)?`, even when the overflow case is unreachable in practice. A naive port doing `state.next_offer_id += 1` would fail code review on principle and could be a real bug if the field width changed; the cleanest move is to delete the counter entirely (see §S2).
- **Why:** Bare arithmetic on a state field — `x += 1`, `total -= amount`, etc. — is a code smell on Solana regardless of how unreachable the overflow case is. The standard is `checked_add` / `checked_sub` returning `Option<T>`, unwrapped with `.ok_or(MyError::Overflow)?` (see `security/arithmetic.md`). Even when the value is unreachable in practice (`u64::MAX` is 1.8e19 offers, more than the universe has time for), the pattern fails code review and would be a real bug had the field been narrower.
- **Benefit:** One fewer arithmetic site to audit. The class of bug is deleted, not just patched.
- **Tradeoff:** None.

---

## CPI & program reuse

### `selfdestruct`-style cleanup on take, atomically (diff §C1)

- **What:** `take_offer` now includes a `token::close_account` CPI (`03-optimized.rs:124`–`133`) that closes the vault Token Account after draining it, and Anchor's `close = maker` constraint (`:242`) closes the Offer PDA itself. Both accounts' rent deposits refund to the maker.
- **Annotation:** When the offer is taken, both accounts get torn down atomically: a `token::close_account` CPI destroys the vault Token Account, and the `close = maker` constraint on the struct destroys the `Offer` PDA, with rent deposits flowing back to the maker. Solidity's nearest analog is `selfdestruct` (deprecated and never really applied to mapping entries anyway). Without this cleanup the accounts would linger forever holding their rent deposits — recoverable in principle, but only via a separate cleanup instruction.
- **Why:** In the naive design, vault Token Accounts persist forever after the offer is fulfilled — their rent deposits are stranded forever. Closing them as part of the take matches the intent of the instruction ("the offer is over, clean up"). On Solana you can structure cleanup as part of the same atomic transaction; on Solidity, the analog would be `selfdestruct` (deprecated and never really applicable to mappings anyway).
- **Benefit:** Maker recovers ~0.0035 SOL of rent on every fulfilled offer (~0.0015 SOL for the Offer PDA + ~0.002 SOL for the vault Token Account). The protocol's on-chain footprint goes back to zero when offers complete.
- **Tradeoff:** The take instruction has more account writes (close operations on two accounts). Marginal compute-unit cost. Worth it.

### Atomic `cancel_offer` (diff §C2)

- **What:** Same close pattern as take — vault closed via `token::close_account` CPI, Offer closed via `close = maker` constraint (`03-optimized.rs:174`–`183`).
- **Annotation:** Cancel mirrors take's teardown: maker pulls their offered tokens back, the vault Token Account is destroyed via `token::close_account`, and the `Offer` PDA is destroyed via the `close = maker` constraint. Both rent deposits return to the maker. Same atomic-cleanup pattern, same reasoning — accounts shouldn't outlive their purpose.
- **Why / Benefit / Tradeoff:** Same as §C1.

---

## Compute & rent

### No global state → no protocol-level rent (diff §R1)

- **What:** Removed `EscrowState` entirely (about 6 KB at full Vec capacity, costing ~0.043 SOL of rent that the protocol deployer pays forever).
- **Annotation:** This program has no protocol-level state account — no `EscrowState`, no shared config, no counter. A naive port carrying a `Vec<Offer>` and `next_offer_id` on a singleton state account would lock the protocol deployer into ~0.043 SOL of permanent rent. Here all rent is per-offer and refunded on take/cancel, so the protocol's steady-state on-chain footprint is zero.
- **Why:** With no global counter and per-offer PDAs, nothing belongs on a singleton account anymore. The protocol has no persistent shared state.
- **Benefit:** Protocol carries no permanent rent burden. All rent is per-offer and refunded on close, so the protocol pays nothing at steady state.
- **Tradeoff:** None.

### Per-call data load ~25× smaller (diff §R2)

- **What:** Each instruction loads ~300 bytes of program state instead of the naive's ~6 KB.
- **Annotation:** Each instruction here loads only the offer's own ~130-byte `Offer` PDA plus the vault — total program state read is ~300 bytes. Anchor deserializes the whole account on instruction entry (it has to, to type-check), so the size of the account directly drives the per-call compute cost. A naive port loading a multi-KB shared state account on every call would burn several thousand CU before any user logic runs.
- **Why:** Anchor deserializes account data on instruction entry (it has to, in order to type-check it). Smaller accounts = faster instruction entry = fewer compute units burned before any user logic runs.
- **Benefit:** Several thousand compute units saved per call (a meaningful fraction of the 200K-CU per-instruction budget on Solana).
- **Tradeoff:** None.

---

## Idioms

### Mapping `status` flag → account-existence lifecycle (diff §I1)

- **What:** The Offer account exists exactly while the offer is open. `init` creates it on `make_offer`, `close = maker` deletes it on `take_offer` or `cancel_offer`. The vault Token Account mirrors this lifecycle via `init` and `token::close_account` CPIs.
- **Annotation:** "Is this offer still open?" is "does this `Offer` account exist?" — the account is created by `init` in `make_offer` and destroyed by `close = maker` in `take_offer` / `cancel_offer`. There's no `status: Open | Cancelled | Taken` field anywhere because Solana lets accounts come and go and the runtime tells you about existence for free at account validation. In Solidity mapping entries are always present and you have to track liveness with a status flag plus a runtime check.
- **Why:** In Solidity, entries in a `mapping` are always present — there's no way to delete them; "cleared" entries just hold zero values, and you need an `if status == Cleared` flag elsewhere to track what's actually live. Solana lets accounts come into and go out of existence, so "is this offer still open?" becomes "does this account exist?". The runtime tells you for free at account validation, before any handler code runs.
- **Benefit:** No bookkeeping bugs around "did we forget to clear this flag?". The runtime gives you the lifecycle property structurally.
- **Tradeoff:** Reading historical offers (after they've been closed) requires either program logs or an off-chain indexer, since the on-chain account is gone. Standard Solana UX; protocols that need historical state typically emit events (Anchor's `emit!` macro writes them to transaction logs) for indexers to pick up.

### Contract-side id → caller-supplied id (diff §I2)

- **What:** `make_offer` takes `id: u64` as its first argument; clients pick the value (timestamp, random nonce, etc.).
- **Annotation:** `make_offer` takes the `id` as a client-supplied argument — typically a timestamp or random nonce. A naive port using a contract-side auto-increment counter would force every `make_offer` to write-lock the counter account and serialize all concurrent makers. Per-`(maker, id)` PDA addressing means each maker has their own id-space and id reuse fails cleanly at account validation when `init` finds the PDA already exists.
- **Why:** A global counter would force every `make_offer` to serialize on the state account holding the counter (see §S2). Per-`(maker, id)` PDA addressing means each maker has their own id-space; no coordination is needed across makers.
- **Benefit:** Drops the last shared mutable field in the program. ID generation is one line of TypeScript on the client.
- **Tradeoff:** Clients must pick ids. Conventionally a timestamp or random bytes. If the maker reuses an id they've already used and the previous offer is still open, the instruction reverts cleanly at account validation (`init` fails because the PDA exists).

### Maker is not a signer on `take_offer` (diff §I3)

- **What:** The `maker` field on the take struct is an `UncheckedAccount` rather than a `Signer` (`03-optimized.rs:266`–`267`); only the taker signs. The maker is just identified, not authenticated, on take.
- **Annotation:** Only the taker signs `take_offer` — the maker is on the struct as `UncheckedAccount`, identified but not authenticated. This is the whole point of an escrow: the maker drops the offer and walks away, and the taker fulfils it later without coordination. The maker's identity is still pinned via `has_one = maker` on the Offer and `token::authority = maker` on the destination token account, so the taker can't redirect the payment — but the maker doesn't need to be online to co-sign.
- **Why:** Atomic-swap correctness requires that takers can fulfil an offer without the maker being online. The maker's identity is still asserted: `has_one = maker` on the Offer (§Sec3) checks that the maker account passed in matches the Offer's stored maker, and `token::authority = maker` on the maker's wanted-token-receiving account confirms the maker controls where the taker's payment lands. So we authenticate where it matters (token destination ownership) without forcing the maker to co-sign.
- **Benefit:** This is the whole point of an escrow protocol — the maker drops an offer and walks away; the taker fulfils later without coordination. Mirrors the Solidity contract's same property.
- **Tradeoff:** None.

### Error variants tightened (diff §I4)

- **What:** `OfferDoesNotExist` and `TooManyOffers` deleted; `MakerMismatch` clarified.
- **Annotation:** The error enum here only carries failures the handler can actually reach. `OfferDoesNotExist` doesn't appear because Anchor's account validation fails before the handler runs if the PDA isn't there. `TooManyOffers` is gone because there's no `Vec` cap to overflow. Solidity error enums often include "shouldn't happen" cases as defensive programming; on Solana the equivalent failures get caught at account validation, so the runtime + framework own them and the program's error surface stays tight.
- **Why:** Anchor's `init` constraint makes "offer doesn't exist on take" a structural impossibility — the PDA either exists (Anchor loads it) or doesn't (the instruction reverts at account validation, before the handler body runs). The error doesn't need to live in the program's error enum because it can't be reached in the handler. `TooManyOffers` was an artifact of the Vec cap that no longer exists.
- **Benefit:** Failure modes that can't happen don't need error codes. Smaller surface for clients to handle.
- **Tradeoff:** None.
