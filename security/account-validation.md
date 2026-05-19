# Account validation — owner, discriminator, type confusion

Solana programs receive raw account data. Without validation, *any* account can be supplied for *any* parameter, and the program will happily deserialize it as whatever type it expected. This is the source of the largest class of Solana exploits.

Anchor's typed account wrappers do most of the work automatically. The job is to make sure you actually use them and don't bypass them.

## The three checks every account needs

For every account in an instruction:

1. **Owner check**: the account's `owner` field (the program that owns it) must be the program you expect. For `Account<'info, MyState>`, Anchor verifies the owner is *the current program*. For SPL accounts (`Account<'info, TokenAccount>`), Anchor verifies the owner is SPL Token.
2. **Discriminator check**: the first 8 bytes must match the expected type's discriminator. Prevents passing a `Position` account where a `Vault` was expected, even though both are owned by the same program.
3. **Type-specific constraints**: for ATAs, `token::mint = expected`; for PDAs, `seeds = [...], bump = ...`; for arbitrary fields, `has_one` or `constraint = `.

When you use `Account<'info, T>` from Anchor, (1) and (2) happen automatically. When you use `AccountInfo` or `UncheckedAccount`, you must do them manually — and you should justify why you're not using a typed account.

## Type confusion attack

```rust
// VULNERABLE
#[derive(Accounts)]
pub struct Bad<'info> {
    /// CHECK: it's a position, we'll trust the layout
    pub position: AccountInfo<'info>,
}

pub fn bad(ctx: Context<Bad>) -> Result<()> {
    let data = ctx.accounts.position.try_borrow_data()?;
    let pos = Position::try_deserialize_unchecked(&mut &data[..])?;
    // ...
}
```

An attacker passes a `Vault` account instead of a `Position`. The bytes happen to overlap; the deserialization succeeds with garbage values. Now `pos.collateral` is reinterpreted bytes from `Vault.deposit_count` — totally controlled by the attacker.

The fix:

```rust
#[derive(Accounts)]
pub struct Good<'info> {
    pub position: Account<'info, Position>,  // ✓ owner & discriminator both checked
}
```

`Account<'info, Position>` rejects any account whose first 8 bytes don't match Anchor's hash of "Position", and any account whose owner isn't this program.

## SPL Token account validation

Token accounts have additional fields that need constraint:

```rust
#[derive(Accounts)]
pub struct TransferOut<'info> {
    pub mint: Account<'info, Mint>,

    #[account(
        mut,
        token::mint = mint,           // the ATA must be for this Mint
        token::authority = user,      // the ATA must be owned (in SPL terms) by user
    )]
    pub user_ata: Account<'info, TokenAccount>,

    pub user: Signer<'info>,
}
```

Without `token::mint = mint`, the attacker could pass an ATA for a different mint and confuse the program into operating on the wrong asset. Without `token::authority`, the attacker could pass someone else's ATA.

## `has_one` for cross-reference

When account A stores a reference to account B, `has_one` enforces the link:

```rust
#[account]
pub struct Position {
    pub owner: Pubkey,
    pub vault: Pubkey,
}

#[derive(Accounts)]
pub struct ClosePosition<'info> {
    #[account(has_one = owner, has_one = vault)]
    pub position: Account<'info, Position>,
    pub owner: Signer<'info>,
    pub vault: Account<'info, Vault>,
}
```

This is the canonical way to assert "the supplied `vault` account is the one this `position` references." Without it, an attacker could pass an arbitrary `Vault`.

## The "CHECK" discipline

Anchor requires every `UncheckedAccount` / `AccountInfo` to be commented with `/// CHECK: ...` explaining why no automatic check is needed. Treat this as a contract:

- The comment is part of the security review surface.
- If the comment says "this is the mint authority PDA, verified by seeds" — the seeds constraint must actually be there.
- If the comment says "we'll check in the body" — the check must actually be in the body.

A `/// CHECK: ` with no real justification is a flag.

## What Anchor checks by default

| Account type | Owner checked? | Discriminator checked? | Type fields checked? |
|---|---|---|---|
| `Account<'info, T>` | ✓ (current program) | ✓ (against `T`'s discriminator) | only with constraints |
| `Account<'info, Mint>` | ✓ (SPL Token) | n/a (SPL has no discriminator) | mint constraints |
| `Account<'info, TokenAccount>` | ✓ (SPL Token) | n/a | token constraints |
| `Signer<'info>` | n/a | n/a | `is_signer = true` |
| `SystemAccount<'info>` | ✓ (System Program) | n/a | none |
| `Program<'info, T>` | ✓ (program ID matches `T::id()`) | n/a | executable flag |
| `Sysvar<'info, T>` | ✓ (Sysvar) | n/a | matches sysvar key |
| `UncheckedAccount<'info>` / `AccountInfo<'info>` | ✗ | ✗ | ✗ — manual only |

Default to typed accounts. Drop down to `UncheckedAccount` only with a documented reason.

### Token-2022 rejection via typed `Mint` / `TokenAccount`

`anchor_spl::token::{Mint, TokenAccount}` enforce the owning program is the **classic** SPL Token program (program ID `Tokenkeg...`). Token-2022 mints (owned by `TokenzQd...`) fail this check at deserialization — the instruction reverts before your handler runs.

This is the cheapest defense against a specific attack: a Token-2022 mint with a `TransferHook` extension can call back into your program during a `transfer` CPI (the hook program is invoked between SPL Token's accounting and the post-transfer state, and that hook program can be arbitrary code). Requiring classic SPL Token at the type level **eliminates this attack class structurally** — no defensive code needed in your program body.

```rust
// Type-level Token-2022 rejection:
use anchor_spl::token::{Mint, TokenAccount};  // classic only

#[derive(Accounts)]
pub struct Deposit<'info> {
    pub asset_mint: Account<'info, Mint>,        // Token-2022 mints fail here
    pub user_ata: Account<'info, TokenAccount>,  // Token-2022 ATAs fail here
    // ...
}
```

If your protocol must accept Token-2022, switch to `anchor_spl::token_interface::{Mint, TokenAccount}` (accepts both programs) and **explicitly allowlist the Token-2022 extensions** you've audited as safe. Treat any program supplied as a transfer hook target with the same suspicion as any user-supplied CPI target. See `security/cpi-safety.md`.

Used by `examples/erc4626-vault/03-optimized.rs` for the underlying asset — documented decision in that example's `DECISIONS.md`.

## Closed-account reuse attack

When closing an account (`close = recipient`), Anchor zeroes the data and reassigns ownership to the System Program. But the *lamports* persist — the recipient gets them. Make sure the closed account isn't aliased somewhere else in the same instruction:

- Closing `position` and then immediately reading `position` again in the same instruction — bug.
- Closing an account whose key is also held in another account's reference — make sure the reference is cleared too, or future calls will reject the dangling reference (which is the safe failure mode, but still a bug).
