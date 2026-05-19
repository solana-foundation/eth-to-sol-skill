# PDA canonicalization

For any seed set, multiple bumps can yield off-curve addresses. Only one — the **canonical bump** — is the one `find_program_address` returns (the first bump from 255 downward that produces an off-curve key). If your access control relies on PDA identity but accepts any valid bump, an attacker can derive an *alternate* PDA with the same seeds but a different bump, and bypass the check.

## The vulnerability

```rust
// VULNERABLE
#[derive(Accounts)]
pub struct Withdraw<'info> {
    #[account(
        mut,
        seeds = [b"vault", user.key().as_ref()],
        bump,  // <-- no stored bump; any bump that produces a valid PDA is accepted
    )]
    pub vault: Account<'info, Vault>,
    pub user: Signer<'info>,
}
```

An attacker creates a `Vault` PDA with seeds `[b"vault", attacker.as_ref()]` using bump 254 (assuming it's also off-curve), funds it, and then calls `withdraw` supplying `user = victim` and a vault PDA derived with bump 254 (which is *not* the canonical vault for victim). The program accepts because the seeds match and the bump produces a valid PDA.

The fix: **store the canonical bump at init and require it on every subsequent access.**

```rust
#[account]
pub struct Vault {
    pub owner: Pubkey,
    pub amount: u64,
    pub bump: u8,  // <-- canonical bump, set at init
}

// at init:
#[account(
    init,
    payer = user,
    space = 8 + Vault::SIZE,
    seeds = [b"vault", user.key().as_ref()],
    bump,  // <-- find canonical
)]
pub vault: Account<'info, Vault>,
// in handler:
ctx.accounts.vault.bump = ctx.bumps.vault;

// on subsequent access:
#[account(
    mut,
    seeds = [b"vault", user.key().as_ref()],
    bump = vault.bump,  // <-- require stored canonical
)]
pub vault: Account<'info, Vault>,
```

`bump = vault.bump` tells Anchor to verify the supplied seeds + the stored bump derive to the account's address. Since the stored bump was set to the canonical bump at init, only the canonical PDA can ever match.

## Why this works

`find_program_address` is deterministic. The canonical bump for a given seed set is a fixed value (depending on the seeds, usually 255, sometimes 254, rarely lower). By storing it at init, you commit to one address forever. Any alternate-bump PDA the attacker creates has a different address, fails the seeds-and-bump check on the next call, and is rejected.

## The rule

1. **At init**: use `bump` (no value). Anchor calls `find_program_address`; the canonical bump appears in `ctx.bumps.<name>`. Store it.
2. **On every subsequent access**: use `bump = <account>.bump`. Never re-derive.
3. **Never** use `bump = <user_input>` — the bump must come from the account itself, which is committed at init.

This is also the CU-optimal pattern (see `optimization/pdas.md`) — but the security argument is the load-bearing one. Caching for CU savings is a side benefit; canonicalization is the actual reason.

## When to use `Pubkey::create_program_address`

`Pubkey::create_program_address` takes seeds + bump and returns the resulting address without iterating. Use it on-chain when you already know the canonical bump (you stored it). It is the inner primitive that `find_program_address` iterates over.

Anchor's `bump = stored_bump` constraint uses `create_program_address` under the hood.

## ATA address derivation

Associated Token Accounts use a *different* derivation:

```
ATA = find_program_address(
    seeds = [owner.as_ref(), token_program.as_ref(), mint.as_ref()],
    program_id = ATA_PROGRAM_ID,
)
```

This is the Associated Token Program's PDA, not yours. Anchor's `associated_token::AssociatedToken` constraint enforces it:

```rust
#[account(
    associated_token::mint = mint,
    associated_token::authority = user,
)]
pub user_ata: Account<'info, TokenAccount>,
```

You do not pick the bump; the ATA program does. The address is fully deterministic from `(owner, mint)`.

## Cross-program PDA collision

Two different programs can derive a PDA with the same byte representation but different effective addresses (since the program ID is part of the derivation). Within one program, two different *seed sets* can never collide if you prefix consistently. The standard prefix `b"<entity>"` per entity type prevents in-program collisions.

If you ever find yourself constructing seeds without an entity prefix — add one.

## Checklist

- [ ] Every PDA struct has a `pub bump: u8` field.
- [ ] Every `init` constraint uses bare `bump` (no value).
- [ ] Every non-init constraint uses `bump = <account>.bump`.
- [ ] No constraint uses `bump = <user_input>` or omits `bump =` after init.
- [ ] Every seed set is prefixed by an entity-name byte string literal.
