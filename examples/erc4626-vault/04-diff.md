# Structured diff: 02-naive-port.rs → 03-optimized.rs

Each section names one meaningful change. Snippets are abridged; line references point at the canonical site of each change.

---

## State model

### S1. `Vec<BalanceEntry>` for shares → SPL Token Mint

Naive (`02-naive-port.rs:474`–`483`, `:482`):

```rust
#[account]
pub struct VaultState {
    pub asset_mint: Pubkey,
    pub owner: Pubkey,
    pub fee_bps: u16,
    pub fee_recipient: Pubkey,
    pub total_assets: u64,
    pub total_supply: u64,
    pub balances: Vec<BalanceEntry>, // SMELL: write-hot, capped, O(n) scan
}
```

Optimized (`03-optimized.rs:673`–`683`):

```rust
#[account]
pub struct Vault {
    pub asset_mint: Pubkey,
    pub share_mint: Pubkey,    // <-- SPL Token Mint, not stored balances
    pub authority: Pubkey,
    pub fee_bps: u16,
    pub fee_recipient: Pubkey,
    pub bump: u8,
    pub vault_authority_bump: u8,
}
```

Shares live in user-owned SPL Token Accounts; the vault no longer tracks per-user share balances. Same lesson as `examples/erc20-token` §S2, applied to a different token.

---

### S2. `total_assets` and `total_supply` fields deleted

Naive (`02-naive-port.rs:480`–`481`): manually maintained on the vault, mutated at `:78`–`:79`, `:114`–`:115`, `:155`–`:156`, `:198`–`:199`, `:248`, `:251`.

Optimized: not stored. Every conversion reads them from SPL Token directly:

```rust
let total_supply = ctx.accounts.share_mint.supply;          // 03-optimized.rs:91, :132, :174, :219
let total_assets = ctx.accounts.asset_reserve.amount;       // 03-optimized.rs:92, :133, :175, :220
```

SPL Token maintains both atomically as a side effect of `mint_to`/`burn`/`transfer`. Self-tracking is redundant *and* (per §P1 below) is what forces vault to be writable on every deposit. Deleting these fields is the load-bearing move.

---

### S3. Share-token ERC-20 surface unified under SPL Token

Naive: no `share_transfer`/`share_approve` instructions (omitted for brevity). To use them in production would require a balance/allowance map.

Optimized: share transfer/approve happen via SPL Token directly. Clients call `spl_token::transfer` / `spl_token::approve` against their share ATA. Withdraw/redeem use SPL Token's built-in delegate (see §C2).

Same lesson as `examples/erc20-token` §C3, generalized to vault shares.

---

## Parallelism

### P1. Vault is READ-ONLY during deposit / mint / withdraw / redeem

Naive — vault is writable on every user action (`02-naive-port.rs:408`):

```rust
#[account(mut, seeds = [b"vault"], bump)]  // SMELL
pub vault: Account<'info, VaultState>,
```

All four user-facing 4626 operations write the vault (to mutate `total_assets`, `total_supply`, and the share `balances` Vec). The vault is the cross-user serialization bottleneck.

Optimized — vault is **not** writable in `Deposit` (`03-optimized.rs:531`–`537`) or `Withdraw` (`03-optimized.rs:574`–`580`):

```rust
#[account(
    seeds = [b"vault", asset_mint.key().as_ref()],
    bump = vault.bump,
    has_one = asset_mint,
    has_one = share_mint,
)]
pub vault: Account<'info, Vault>,   // no `mut`
```

Vault only mutates in admin paths (`set_fee_bps`, `set_fee_recipient`, `set_authority`). The user-facing instructions read vault for bumps + has_one cross-checks, but write only the share Mint, the asset reserve, and the depositor's ATAs.

The result: deposits from disjoint users / redeems from disjoint users only conflict on the inherent globals — `share_mint.supply` and `asset_reserve.amount` — which are maintained by SPL Token and have no app-level contention beyond what a single fungible token always has.

This is a more dramatic parallelism win than the staking-vault example, which had an unavoidably write-hot vault (Synthetix accumulator). 4626's conversion math is *pure* — no per-call checkpoint to mutate — so the vault stays read-only.

---

### P2. O(n) Vec scans eliminated

Naive (`02-naive-port.rs:346`–`368`): linear-scan `find(|e| e.holder == x)` on every share mint/burn.

Optimized: no scans. Per-user share ATAs are addressed by deterministic derivation; SPL Token does the rest.

---

## Security

### Sec1. Unchecked arithmetic → `checked_*` with `mul_div` helper

Naive — SMELL markers at `02-naive-port.rs:78`, `:79`, `:114`, `:115`, `:155`, `:156`, `:198`, `:199`, `:238` (fee math), `:248`, `:251`, `:317`, `:324`, `:331`–`:332`, `:339`, `:348`, `:366`. Example (`:238`):

```rust
let fee_assets = ((yield_amount as u128) * (vault.fee_bps as u128)) / 10_000u128;
let num = fee_assets * ((vault.total_supply as u128) + VIRTUAL_SHARES_OFFSET);
let den = (vault.total_assets as u128) + VIRTUAL_ASSETS_OFFSET;
fee_shares = (num / den) as u64;  // SMELL: silent truncation
```

Optimized (`03-optimized.rs:399`–`418`): single `mul_div_u128_to_u64` helper used at every conversion site.

```rust
fn mul_div_u128_to_u64(a: u128, b: u128, c: u128, rounding: Rounding) -> Result<u64> {
    require!(c > 0, VaultError::DivByZero);
    let product = a.checked_mul(b).ok_or(VaultError::Overflow)?;
    let result_u128 = match rounding {
        Rounding::Down => product.checked_div(c).ok_or(VaultError::DivByZero)?,
        Rounding::Up => {
            let c_minus_one = c.checked_sub(1).ok_or(VaultError::Overflow)?;
            let raised = product.checked_add(c_minus_one).ok_or(VaultError::Overflow)?;
            raised.checked_div(c).ok_or(VaultError::DivByZero)?
        }
    };
    require!(result_u128 <= u64::MAX as u128, VaultError::Overflow);
    Ok(result_u128 as u64)
}
```

Every multiply checked. Every divide checked (catches `c = 0`). Every add in the ceiling formula checked (catches `product + (c - 1)` overflow). Final cast guarded by explicit bounds check.

---

### Sec2. Explicit `Rounding` direction at every conversion site

Naive — direction is implicit in two separate code paths: `preview_deposit`/`preview_redeem` round down via `/` (`02-naive-port.rs:317`, `:324`); `preview_mint`/`preview_withdraw` round up via ad-hoc `(num + den - 1) / den` (`02-naive-port.rs:332`, `:339`). A reviewer must read each helper to know its direction.

Optimized (`03-optimized.rs:46`–`50`, used at `:93`, `:134`, `:177`, `:222`, `:284`):

```rust
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Rounding { Down, Up }
```

Every call site states direction explicitly:

```rust
let shares = convert_to_shares(assets, total_supply, total_assets, Rounding::Down)?;  // deposit
let assets = convert_to_assets(shares, total_supply, total_assets, Rounding::Up)?;    // mint
let shares = convert_to_shares(assets, total_supply, total_assets, Rounding::Up)?;    // withdraw
let assets = convert_to_assets(shares, total_supply, total_assets, Rounding::Down)?;  // redeem
```

ERC-4626 rounding direction is part of the spec — wrong direction is exploitable. The optimized version makes the spec audit a four-line check.

---

### Sec3. PDA bumps cached + canonicalization enforced

Naive — bare `bump` on every PDA constraint (`02-naive-port.rs:377`, `:381`, `:391`, `:399`, `:408`, `:412`, `:418`, `:428`, `:432`, `:438`, `:448`, `:452`, `:458`, `:466`). Bump not stored on the vault.

Optimized — `Vault.bump` and `Vault.vault_authority_bump` stored at init (`03-optimized.rs:75`–`76`), supplied on every subsequent access (`03-optimized.rs:530`, `:537`, `:546`, `:573`, `:580`, `:589`, `:617`, `:625`, `:634`, `:662`).

Same reasoning as `examples/erc20-token` §Sec2 and `examples/staking-vault` §Sec3.

---

### Sec4. Vault token-account authority scoped to vault key

Naive — `seeds = [b"vault_authority"]` (singleton across the whole program).

Optimized — `seeds = [b"vault_authority", vault.key().as_ref()]` (per-vault). Supports the multi-vault future (one vault per asset_mint) without all vaults sharing a single signing authority.

---

### Sec5. Token-2022 / transfer-hook rejection at the type level

Both versions use `anchor_spl::token::{Mint, TokenAccount}` (classic SPL Token). Anchor verifies the account's owner program is the classic SPL Token program — Token-2022 mints fail deserialization. Documented in `DECISIONS.md`.

This is *intentional* — it eliminates the cross-program-reentrancy attack vector where a Token-2022 asset_mint with a transfer hook could call back into the vault during `deposit`/`withdraw`'s SPL Token CPI. Supporting Token-2022 underlyings requires either (a) explicit reentrancy-state machine in the vault, or (b) constraining underlying to a known-safe extension subset. Neither is in scope for this example.

---

### Sec6. ERC-4626 inflation-attack defense preserved + tested

Both ports include the OZ virtual-offset defense: `virtual_shares = 10^6`, `virtual_assets = 1`. Numerator/denominator of every share-asset conversion includes these terms (`02-naive-port.rs:39`–`40`, `03-optimized.rs:37`–`38`).

The optimized version's `mul_div_u128_to_u64` makes the defense calculation overflow-safe (Sec1) and direction-explicit (Sec2), so the defense cannot be silently degraded by a future arithmetic bug. The explanation log §"Inflation defense" walks through a numeric example demonstrating the bounded loss.

---

### Sec7. has_one constraints cross-validate vault references

Naive — no `has_one`. Vault is the only place asset_mint/share_mint live, so cross-checks aren't needed within the vault account itself, but if a future instruction trusts asset_mint without going through vault, no constraint catches a swap.

Optimized — `has_one = asset_mint, has_one = share_mint` on `Deposit`/`Withdraw`/`Earn` (`03-optimized.rs:534`–`535`, `:578`–`:579`, `:619`–`:620`); `has_one = authority` on `Earn` and `AdminAction` (`03-optimized.rs:621`, `:662`). Anchor enforces all four cross-links before the handler runs.

---

## CPI & program reuse

### C1. Share mint/burn via SPL Token CPI (PDA-signed mint)

Naive: balance updates are direct Vec mutations.

Optimized (`03-optimized.rs:420`–`443` mint helper; `:188`–`:198` burn at withdraw site): `token::mint_to` for share issuance (signed by `vault_authority` PDA); `token::burn` for share destruction (signed by the user or their delegate).

Mirror of the ERC-20 example's mint-via-CPI pattern (`examples/erc20-token` §C1).

---

### C2. Withdraw-from-owner-by-delegate uses SPL Token's native delegate

Solidity `withdraw(assets, receiver, owner_)` allows `msg.sender != owner_` if msg.sender has an allowance. The Solidity implementation maintains an allowance map and `_spendAllowance`'s into it.

Naive port omits this entirely (`02-naive-port.rs:441` notes the smell: requires owner == signer).

Optimized port uses SPL Token's per-ATA `delegate` field (`03-optimized.rs:603`–`605`):

```rust
/// No `token::authority` constraint here — both paths (owner-signs and delegate-signs)
/// must be allowed. SPL Token's `burn` does the auth check.
#[account(mut, token::mint = share_mint)]
pub owner_share_ata: Account<'info, TokenAccount>,
```

The `signer` may be the ATA's owner OR its SPL delegate; SPL Token's `burn` enforces. No custom allowance state on the vault.

---

### C3. Aggregates read from SPL Token, not stored locally

Diff §S2 is the state-model framing; here is the resulting CPI-side fact: every conversion reads `share_mint.supply` and `asset_reserve.amount` directly from SPL Token's deserialized state. SPL Token maintains them; we read them.

This is the single most important architectural insight in the 4626 port: a Solana 4626 vault doesn't need to track totals — SPL Token does.

---

## Compute & rent

### R1. Vault size shrinks from ~4 KB to 132 bytes

Naive (`02-naive-port.rs:486`): `VaultState::SIZE = 32 + 32 + 2 + 32 + 8 + 8 + 4 + 100*40 = 4118 bytes`. Rent: ~0.029 SOL, paid by owner at init.

Optimized (`03-optimized.rs:686`): `Vault::SIZE = 132 bytes`. Rent: ~0.0025 SOL. Each holder's share ATA (165 bytes, ~0.0019 SOL) is paid by the holder, not the vault.

### R2. Per-call data load shrinks ~30×

Naive: every user instruction loads the full ~4 KB VaultState (including Vec deserialization).

Optimized: every user instruction loads `Vault` (132 bytes) + `Mint` (82 bytes) + `TokenAccount` (165 bytes) ≈ 380 bytes of program state, plus user ATAs (~330 bytes). The Vec deserialization cost is gone entirely.

---

## Idioms

### I1. Pure conversion helpers + Rounding enum

Naive: four separate preview functions, each with its own ad-hoc arithmetic.

Optimized (`03-optimized.rs:360`–`397`): two pure helpers (`convert_to_shares`, `convert_to_assets`) parameterized by `Rounding`. Each preview operation maps to one call. Single arithmetic site (`mul_div_u128_to_u64`); every overflow/division/narrowing concern in one place.

### I2. Consolidated CPI helpers for vault_authority-signed operations

Naive: signer-seed construction inline at every call site (`02-naive-port.rs:158`–`:159`, `:201`–`:202`). Easy to drift between sites.

Optimized (`03-optimized.rs:420`–`460`): `mint_shares` and `transfer_asset_out` helpers. Signer seeds constructed once, in one place, using the cached bump.

### I3. SPL Token's `Mint`/`TokenAccount` deserialized at instruction entry

Optimized leans hard on Anchor's typed account wrappers — every `Account<'info, Mint>` and `Account<'info, TokenAccount>` Anchor verifies owner (classic SPL Token program), parses fields, and exposes `mint.supply`/`account.amount` as plain `u64`. No `try_borrow_data` / manual deserialization.

### I4. Errors: 9 typed variants covering every failure class

Naive (`02-naive-port.rs:544`–`562`): 7 variants, no Overflow / DivByZero.

Optimized (`03-optimized.rs:728`–`748`): 9 variants including `Overflow`, `DivByZero`, `InvalidShareDecimals`, `FeeRecipientMismatch`. Every conversion failure path maps to a specific error.
