# Structured diff: 02-naive-port.rs → 03-optimized.rs

Each section names one meaningful change. Snippets are abridged; line references point at the canonical site of each change.

---

## State model

### S1. Global `EscrowState` with `Vec<Offer>` → per-offer PDA

Naive (`02-naive-port.rs:313`–`322`):

```rust
#[account]
pub struct EscrowState {
    pub next_offer_id: u64,
    pub offers: Vec<Offer>, // SMELL: write-hot, capped, O(n) scan
}
// seeds = [b"state"]   — one singleton account, shared by all offers
```

Optimized (`03-optimized.rs:322`–`332`):

```rust
#[account]
pub struct Offer {
    pub id: u64,
    pub maker: Pubkey,
    pub token_offered: Pubkey,
    pub amount_offered: u64,
    pub token_wanted: Pubkey,
    pub amount_wanted: u64,
    pub bump: u8,
    pub vault_authority_bump: u8,
}
// seeds = [b"offer", maker.as_ref(), &id.to_le_bytes()]   — one PDA per (maker, id)
```

Solidity's `mapping(uint256 => Offer)` becomes one PDA per id, scoped under the maker. No global state account at all.

---

### S2. `next_offer_id` counter deleted

Naive (`02-naive-port.rs:64`):

```rust
let id = state.next_offer_id;
state.next_offer_id += 1; // SMELL: unchecked
```

Optimized: the maker supplies an `id: u64` as an instruction argument (`03-optimized.rs:37`). Any unique value works — typically a client-side timestamp or random nonce. Re-init protection comes from Anchor's `init` constraint against the seeds `[b"offer", maker.as_ref(), &id.to_le_bytes()]`; trying to re-use an id for the same maker fails at the account level.

This is the load-bearing move that lets us drop `EscrowState` entirely.

---

### S3. Shared vault token account → per-offer vault

Naive (`02-naive-port.rs:251`–`254`):

```rust
// One vault per (mint, vault_authority). Reused across offers of the same mint.
#[account(mut, token::mint = token_offered_mint, token::authority = vault_authority)]
pub vault_token_account_offered: Account<'info, TokenAccount>,
```

The naive design has an inherent ambiguity: if two open offers both escrow USDC, their escrowed amounts are commingled in one TokenAccount. The program tracks per-offer amounts in `state.offers[i].amount_offered`, but the vault's actual balance is the SUM.

Optimized (`03-optimized.rs:206`–`214`):

```rust
#[account(
    init,
    payer = maker,
    seeds = [b"vault", offer.key().as_ref()],
    bump,
    token::mint = token_offered_mint,
    token::authority = vault_authority,
)]
pub vault: Account<'info, TokenAccount>,
```

One vault per offer. Held tokens are physically segregated. Vault authority is also per-offer (`03-optimized.rs:189`–`194`), so a bug in one offer's release path cannot move funds from another offer.

---

## Parallelism

### P1. Cross-offer activity now fully parallel

Naive — every `make_offer` / `take_offer` / `cancel_offer` writes the singleton `state` account (`02-naive-port.rs:228`). Two unrelated makers creating offers serialize on `state`. The program throughput ceiling is ~1 offer-changing op per slot.

Optimized — each instruction's writable account set is `{offer, vault}` (plus the user's own ATAs). Two unrelated offers write disjoint accounts. Throughput scales linearly with concurrent offers.

This is the cleanest parallelism story across the four examples — no global state survives the refactor.

---

### P2. O(n) Vec scans eliminated

Naive (`02-naive-port.rs:99`, `:166`):

```rust
let idx = state.offers.iter().position(|o| o.id == id).ok_or(...)?;
```

Every take or cancel scans the Vec linearly. CU cost grows with open-offer count.

Optimized: the offer is reached by deterministic PDA derivation from `(maker, id)`. O(1).

---

## Security

### Sec1. PDA bumps cached and enforced

Naive — bare `bump,` on every constraint (`02-naive-port.rs:213`, `:217`, `:232`, etc). Bump not stored on state.

Optimized — `Offer.bump` and `Offer.vault_authority_bump` cached at init (`03-optimized.rs:67`–`68`), supplied on every subsequent access (`03-optimized.rs:237`, `:247`, `:289`, `:298`). Same pattern as the other reference examples; the canonical-bump enforcement closes the alternate-bump attack class.

---

### Sec2. Per-offer vault authority — blast radius isolation

Naive (`02-naive-port.rs:213`):

```rust
#[account(seeds = [b"vault_authority"], bump)]   // singleton authority
```

A bug in any take/cancel path that mis-signs a CPI can move funds from any vault.

Optimized (`03-optimized.rs:189`–`193`):

```rust
#[account(
    seeds = [b"vault_authority", offer.key().as_ref()],
    bump,
)]
```

The vault authority is scoped to a specific offer. Compromise of one offer's release path cannot drain another offer's vault — the signing seeds simply don't match.

---

### Sec3. `has_one` cross-validation of maker

Naive (`02-naive-port.rs:177`):

```rust
require_keys_eq!(offer.maker, ctx.accounts.maker.key(), EscrowError::NotMaker);
```

Run-time check inside the handler. If a future instruction forgets the check, the bug is silent.

Optimized (`03-optimized.rs:236`, `:285`):

```rust
#[account(
    mut,
    seeds = [b"offer", offer.maker.as_ref(), &offer.id.to_le_bytes()],
    bump = offer.bump,
    has_one = maker @ EscrowError::MakerMismatch,
    close = maker,
)]
pub offer: Account<'info, Offer>,
```

Declarative. The constraint runs before the handler body. The same struct is the reuse surface for any future maker-gated instruction.

---

### Sec4. Mint cross-validation moved to constraints

Naive (`02-naive-port.rs:104`–`:115`): three runtime `require_keys_eq!` checks ensure the supplied mints and the maker's wanted-token account match the offer.

Optimized (`03-optimized.rs:238`–`239`, `:286`):

```rust
constraint = offer.token_offered == token_offered_mint.key() @ EscrowError::MintMismatch,
constraint = offer.token_wanted  == token_wanted_mint.key()  @ EscrowError::MintMismatch,
```

Declarative. Visible in the IDL.

---

### Sec5. Unchecked counter increment removed

Naive (`02-naive-port.rs:64`): `state.next_offer_id += 1;` would silently wrap on u64::MAX (genuinely unreachable in practice, but bare arithmetic is still a code-review failure).

Optimized: no counter exists. Maker supplies the id. (See §S2.)

---

## CPI & program reuse

### C1. Atomic `take_offer` — Vault closure folded into the instruction

Naive (`02-naive-port.rs:96`–`:161`): transfer wanted from taker → maker, transfer offered from vault → taker. The vault TokenAccount persists with zero balance forever; the rent is stranded.

Optimized (`03-optimized.rs:83`–`140`): same two transfers, plus `token::close_account` of the vault (signing with the per-offer vault_authority), plus the Offer account is closed via Anchor's `close = maker` constraint. Rent on both accounts refunds to the maker as part of the take.

Net effect: a take consumes the offer entirely and returns ~all rent to the maker. The protocol's on-chain footprint goes to zero when an offer is fulfilled.

---

### C2. Atomic `cancel_offer` — same pattern

Naive (`02-naive-port.rs:163`–`:204`): transfer offered back to maker. Offer entry removed from Vec. Vault TokenAccount persists (rent stranded).

Optimized (`03-optimized.rs:142`–`182`): transfer + `token::close_account` + Anchor `close = maker` on the Offer. Maker recovers everything they paid in (token amount + rent).

---

## Compute & rent

### R1. No global state → no protocol-level rent

Naive (`02-naive-port.rs:319`–`322`): `EscrowState::SIZE = 8 + 4 + 50 * 120 = 6012 bytes`. Plus 8-byte discriminator. ~0.043 SOL of rent locked in the singleton state forever, regardless of how many offers exist.

Optimized: no singleton state account. Each maker pays ~0.0015 SOL for their offer's PDA + ~0.002 SOL for the vault TokenAccount — both refunded on take/cancel.

### R2. Per-call data load shrinks dramatically

Naive: every instruction loads the full ~6 KB `EscrowState` and deserializes the Vec.

Optimized: every instruction loads only the relevant `Offer` (~130 bytes) + `vault` TokenAccount (~165 bytes). ~25× less data deserialized per call.

---

## Idioms

### I1. `init` + `close` lifecycle replaces manual bookkeeping

The optimized version's lifecycle is entirely Anchor-driven:

- `make_offer`: `init` on Offer, `init` on vault TokenAccount.
- `take_offer` / `cancel_offer`: `close = maker` on Offer (Anchor handles), `token::close_account` on the vault (we CPI explicitly because the SPL Token Program owns it, not Anchor).

No `swap_remove`. No Vec bookkeeping. Account existence IS the offer's lifecycle state.

### I2. `id` as an instruction argument

Naive: `next_offer_id` counter on state, returned from `make_offer`.

Optimized: maker supplies `id: u64`. Clients typically use a timestamp or random nonce. The Anchor `init` constraint enforces uniqueness per (maker, id) at the account level — re-use fails before the handler runs.

### I3. Maker is no longer a `Signer` on `take_offer`

Naive (`02-naive-port.rs:280`–`281`): the wanted-token account is just `mut`; ownership is checked at runtime via `require_keys_eq!`.

Optimized (`03-optimized.rs:266`–`267`): `maker` is an `UncheckedAccount` validated via `has_one = maker` on the offer. The wanted-token account has `token::authority = maker`. Maker doesn't sign the take — that's correct; the taker fulfils the offer.

### I4. Error variants tightened

Naive (`02-naive-port.rs:361`–`:380`): 7 variants including `OfferDoesNotExist`, `TooManyOffers`, `MakerMismatch`, `MintMismatch`.

Optimized (`03-optimized.rs:361`–`378`): 5 variants. `OfferDoesNotExist` is unreachable (Anchor `init` / lookup-by-PDA does that check structurally); `TooManyOffers` is gone (no Vec).
