# Structured diff: 02-naive-port.rs → 03-optimized.rs

Each section names one meaningful change. Snippets are abridged; line references point at the canonical site of each change.

---

## State model

### S1. `TokenState` monolith → `Config` PDA + SPL Mint

Naive (`02-naive-port.rs:268`–`286`):

```rust
#[account]
pub struct TokenState {
    pub name: String,
    pub symbol: String,
    pub decimals: u8,
    pub total_supply: u64,
    pub max_supply: u64,
    pub owner: Pubkey,
    pub balances: Vec<BalanceEntry>,     // ~4040 bytes capacity
    pub allowances: Vec<AllowanceEntry>, // ~14400 bytes capacity
}
```

Optimized (`03-optimized.rs:224`–`240`):

```rust
#[account]
pub struct Config {
    pub authority: Pubkey,
    pub mint: Pubkey,
    pub max_supply: u64,
    pub bump: u8,
    pub mint_authority_bump: u8,
}
// SIZE = 32 + 32 + 8 + 1 + 1 = 74 bytes
```

The `Mint` (SPL Token) replaces `total_supply` (`mint.supply`) and `decimals` (`mint.decimals`). Name/symbol move off-chain (Metaplex Token Metadata, not modelled here).

---

### S2. `Vec<BalanceEntry>` deleted — balances are ATAs

Naive (`02-naive-port.rs:275`); all uses at `02-naive-port.rs:201`, `:218`, `:163`.

```rust
pub balances: Vec<BalanceEntry>, // O(n) scan on every operation
```

Optimized: no balance storage in this program. The recipient is a `TokenAccount` (`03-optimized.rs:181`–`185`):

```rust
#[account(mut, token::mint = mint)]
pub recipient: Account<'info, TokenAccount>,
```

Holders create Associated Token Accounts (one per `(holder, mint)`) and call SPL Token directly for transfers.

---

### S3. `Vec<AllowanceEntry>` deleted — allowances are SPL Token delegates

Naive (`02-naive-port.rs:276`); used at `02-naive-port.rs:66`–`:90` (`approve`) and `:97`–`:124` (`transfer_from`).

```rust
pub allowances: Vec<AllowanceEntry>,
// approve() and transfer_from() implement the full allowance lifecycle.
```

Optimized: no allowance storage. SPL Token's `approve` instruction sets `TokenAccount.delegate` and `delegated_amount`. The `transfer_from` translation is "delegate calls SPL Token's `transfer`." Not implemented in this program.

---

### S4. `total_supply` field deleted

Naive (`02-naive-port.rs:272`); mutated at `02-naive-port.rs:141` and `:165`.

```rust
state.total_supply += amount; // mint
state.total_supply -= amount; // burn
```

Optimized: not stored. Max-supply enforcement reads `ctx.accounts.mint.supply` directly (`03-optimized.rs:52`–`58`):

```rust
let new_supply = ctx.accounts.mint.supply
    .checked_add(amount)
    .ok_or(TokenError::Overflow)?;
require!(new_supply <= config.max_supply, TokenError::MaxSupplyExceeded);
```

---

### S5. `name` / `symbol` strings deleted

Naive (`02-naive-port.rs:269`–`270`); set in `initialize` at `02-naive-port.rs:39`–`:40`.

Optimized: not stored on-chain. The convention is **Metaplex Token Metadata** — an off-chain JSON descriptor pointed to by an on-chain metadata account. The `initialize` instruction (`03-optimized.rs:29`) does not take a name/symbol; integrators add metadata via `mpl-token-metadata` in a follow-up instruction.

---

## Parallelism

### P1. Write-hot `state` PDA eliminated

Naive — every mutating instruction declares `state` writable (`02-naive-port.rs:251`–`:256`):

```rust
#[derive(Accounts)]
pub struct Mutate<'info> {
    #[account(mut, seeds = [b"state"], bump)]
    pub state: Account<'info, TokenState>,
    pub caller: Signer<'info>,
}
```

Every transfer, approve, burn, and transfer_from in the system writes this one account. Sealevel serializes all of them.

Optimized: there is no `Mutate` struct. `mint_to` (`03-optimized.rs:46`) writes the `mint` (genuinely shared) and `recipient`; `burn` (`03-optimized.rs:86`) writes the `mint` and the holder's ATA. Transfers between disjoint users write disjoint ATAs and parallelize.

The `mint` is still write-locked on supply changes — but supply changes are governance-rate, not transfer-rate.

---

### P2. O(n) Vec scans eliminated

Naive (`02-naive-port.rs:201`–`:216`, `:218`–`:229`):

```rust
let from_entry = state.balances.iter_mut().find(|e| e.holder == from).ok_or(...)?;
```

Every transfer linear-scans up to `MAX_HOLDERS = 100` entries. Costs CU and forces full Vec re-serialization on save.

Optimized: no scans. ATAs are addressed directly via deterministic ATA derivation; the program never iterates a collection.

---

## Security

### Sec1. Unchecked arithmetic → `checked_*`

Naive — marked at `02-naive-port.rs:119`, `:135` (the `+ amount <=` check itself can overflow), `:141`, `:164`, `:165`, `:210`, `:220`. Example (`02-naive-port.rs:135`–`:141`):

```rust
require!(
    state.total_supply + amount <= state.max_supply, // overflows silently in release builds
    TokenError::MaxSupplyExceeded
);
state.total_supply += amount;
```

Optimized (`03-optimized.rs:52`–`58`):

```rust
let new_supply = ctx.accounts.mint.supply
    .checked_add(amount)
    .ok_or(TokenError::Overflow)?;
require!(new_supply <= config.max_supply, TokenError::MaxSupplyExceeded);
```

All other arithmetic surfaces (balance decrements, allowance decrements) are deleted entirely — SPL Token does them, with audited checks.

---

### Sec2. PDA bumps cached at init, enforced on every access

Naive (`02-naive-port.rs:49` marker); all subsequent uses at `02-naive-port.rs:237`, `:252`, `:260`:

```rust
#[account(mut, seeds = [b"state"], bump)] // <-- bump not enforced against stored value
```

The bump is re-derived on every call. An attacker cannot exploit this for the singleton `state` PDA (no seed input is attacker-controlled), but the pattern leaves no defense against alternate-bump variants for PDAs that *do* take attacker-controllable seeds — and the naive port's discipline is missing.

Optimized (`03-optimized.rs:157`–`165`):

```rust
#[account(
    seeds = [b"config", mint.key().as_ref()],
    bump = config.bump,           // canonical bump enforcement
    has_one = mint,
    has_one = authority,
)]
pub config: Account<'info, Config>,
```

`Config` stores its own bump (`03-optimized.rs:233`) and the `mint_authority` bump (`03-optimized.rs:235`). Every subsequent access supplies them.

---

### Sec3. `onlyOwner` modifier → `has_one = authority` constraint

Naive (`02-naive-port.rs:130`–`:133` and `:180`–`:183`):

```rust
require_keys_eq!(state.owner, ctx.accounts.owner.key(), TokenError::NotOwner);
```

Run-time, in-function. If anyone forgets to add it to a future privileged instruction, the bug is silent until exploited.

Optimized (`03-optimized.rs:160`–`165`):

```rust
#[account(
    has_one = mint,
    has_one = authority,
)]
pub config: Account<'info, Config>,
pub authority: Signer<'info>,
```

Declarative. Surfaces in the IDL. Anchor enforces it before the function body runs, on every instruction that uses the struct.

---

### Sec4. Mint-authority controlled by program (PDA), not an EOA

Naive: there is no real Solana analog — the `owner` field is just a `Pubkey` stored on the state account. Whoever holds the key calls `mint` directly.

Optimized (`03-optimized.rs:130`–`135` and `:137`–`:144`):

```rust
#[account(
    seeds = [b"mint_authority", mint.key().as_ref()],
    bump,
)]
pub mint_authority: UncheckedAccount<'info>,

#[account(
    init,
    payer = authority,
    mint::decimals = decimals,
    mint::authority = mint_authority, // <-- PDA, not the human authority
)]
pub mint: Account<'info, Mint>,
```

The mint authority on the SPL Mint is a PDA owned by *this* program. The human `authority` cannot mint directly — they must call `mint_to` on this program, which CPIs into SPL Token signing as the PDA (`03-optimized.rs:59`–`75`). Bypass impossible.

---

## CPI & program reuse

### C1. `mint` instruction reimplements supply tracking → CPI to `spl_token::mint_to`

Naive (`02-naive-port.rs:126`–`:151`). Custom supply increment, custom balance upsert, custom event.

Optimized (`03-optimized.rs:46`–`77`):

```rust
token::mint_to(
    CpiContext::new_with_signer(
        ctx.accounts.token_program.to_account_info(),
        MintTo {
            mint: ctx.accounts.mint.to_account_info(),
            to: ctx.accounts.recipient.to_account_info(),
            authority: ctx.accounts.mint_authority.to_account_info(),
        },
        &[seeds],
    ),
    amount,
)?;
```

The program is the signer for the PDA via `new_with_signer`. SPL Token handles the actual supply and balance update.

---

### C2. `burn` instruction reimplements balance decrement → CPI to `spl_token::burn`

Naive (`02-naive-port.rs:153`–`:174`). Linear scan, decrement, manual event.

Optimized (`03-optimized.rs:86`–`99`):

```rust
token::burn(
    CpiContext::new(
        ctx.accounts.token_program.to_account_info(),
        Burn {
            mint: ctx.accounts.mint.to_account_info(),
            from: ctx.accounts.holder_ata.to_account_info(),
            authority: ctx.accounts.holder.to_account_info(),
        },
    ),
    amount,
)?;
```

No PDA signer — the holder signs for their own ATA. The same instruction in 9 lines instead of 21, with no custom arithmetic.

---

### C3. `transfer`, `approve`, `transfer_from` instructions removed

Naive (`02-naive-port.rs:59` `transfer`, `:66` `approve`, `:97` `transfer_from`). ~80 lines of custom code total.

Optimized: not present. Clients call SPL Token directly (`spl_token::transfer`, `spl_token::approve`). The program is not in the path.

---

## Idioms

### I1. Solidity `constructor` → explicit `initialize` instruction

Naive (`02-naive-port.rs:30`). Same Anchor pattern as optimized — both use `initialize`. No diff.

Optimized (`03-optimized.rs:29`). Different *signature*: takes `decimals` and `max_supply` only; no `name`/`symbol` because those move to Metaplex. Mints the SPL `Mint` and configures the PDA in one transaction.

---

### I2. `OwnershipTransferred` event removed; `Transfer`/`Approval` events removed

Naive (`02-naive-port.rs:306`, `:313`, `:320`). Three custom `#[event]` types emitted at five sites.

Optimized: no custom events. SPL Token emits its own logs on every mint/burn/transfer/approve, parsed by every Solana indexer. `set_authority` change is observable via account-state diffs; no event is added.

---

### I3. Hardcoded capacity constants removed

Naive (`02-naive-port.rs:20`–`:23`):

```rust
const NAME_MAX: usize = 32;
const SYMBOL_MAX: usize = 16;
const MAX_HOLDERS: usize = 100;
const MAX_ALLOWANCES: usize = 200;
```

These exist because Vec-in-account requires a hard cap.

Optimized: no caps. Token holder count is unbounded (one ATA per holder, deterministically derived).

---

### I4. Helper functions `do_transfer` and `upsert_balance` deleted

Naive (`02-naive-port.rs:201` and `:218`). ~30 lines of branching/arithmetic.

Optimized: deleted. SPL Token covers the underlying mutation; the program never touches per-holder state directly.

---

### I5. Account size: ~18 KB → 74 bytes

Naive (`02-naive-port.rs:280`–`:287`). `TokenState::SIZE` ≈ 32 + 16 + 1 + 8 + 8 + 32 + (4 + 100×40) + (4 + 200×72) = **18,489 bytes** (plus 8-byte discriminator). Rent cost ~0.13 SOL, paid by whoever called `initialize`.

Optimized (`03-optimized.rs:238`–`:240`). `Config::SIZE = 74` bytes (plus 8-byte discriminator). Rent cost ~0.0016 SOL. The Mint is a separate account at ~82 bytes (≈0.0014 SOL rent, paid by `authority`). Total: ~0.003 SOL. ~40× cheaper *and* unbounded holder capacity.
