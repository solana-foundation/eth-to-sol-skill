# Signer checks

Every privileged action must verify that the account claiming authority actually signed the transaction. Anchor makes this easy; missing it is one of the most common Solana program bugs.

## The primitive

`Signer<'info>` is an account type that requires the runtime flag `is_signer = true` for the supplied account. Use it for every "msg.sender == X" check.

```rust
#[derive(Accounts)]
pub struct UpdateAuthority<'info> {
    #[account(mut, has_one = authority)]
    pub config: Account<'info, Config>,
    pub authority: Signer<'info>,
}
```

`has_one = authority` says: `config.authority` must equal the `authority` account's key. `Signer<'info>` says: the `authority` account must have signed the transaction. Together: "the account stored as `config.authority` must sign."

## Manual signer checks (when constraints don't fit)

If the authority is computed dynamically (e.g. one of several valid signers), use a manual check:

```rust
pub fn action(ctx: Context<Action>) -> Result<()> {
    let signer = &ctx.accounts.signer;
    require!(signer.is_signer, MyError::Unauthorized);
    require!(
        signer.key() == ctx.accounts.config.authority
            || signer.key() == ctx.accounts.config.guardian,
        MyError::Unauthorized
    );
    // ...
}
```

`Signer<'info>` would only allow one of those. For OR-logic, use `AccountInfo` + manual `is_signer` check.

## The bug: `AccountInfo` without `is_signer` check

```rust
// VULNERABLE
#[derive(Accounts)]
pub struct Bad<'info> {
    pub config: Account<'info, Config>,
    /// CHECK: it's the authority, trust me
    pub authority: AccountInfo<'info>,
}

pub fn bad(ctx: Context<Bad>) -> Result<()> {
    require_keys_eq!(ctx.accounts.config.authority, ctx.accounts.authority.key());
    // missing: require!(ctx.accounts.authority.is_signer)
    // anyone can supply a matching key without signing
}
```

This check is only "is the account the right key", not "did the account sign". An attacker can pass the authority's pubkey without having its private key. Always include the `is_signer` check, or use `Signer<'info>` which does it for you.

## PDA "signers"

PDAs cannot sign in the usual sense — they have no private key. A program *can* sign for its own PDAs via `invoke_signed`. From the inner program's view, the PDA appears in the account list with `is_signer = true`. This is how mint-authority PDAs work: SPL Token sees a signed authority; our program does the signing on the PDA's behalf.

In Anchor:

```rust
let signer_seeds: &[&[u8]] = &[b"mint_authority", mint_key.as_ref(), &[config.bump]];
token::mint_to(
    CpiContext::new_with_signer(token_program, accounts, &[signer_seeds]),
    amount,
)?;
```

`new_with_signer` arranges for the runtime to set `is_signer = true` on the PDA when the inner program (SPL Token) inspects its accounts.

## Two-authority separation for fund-holding protocols

A staking pool, AMM, lending market, or 4626 vault holds user-deposited tokens. The program needs **two distinct authorities**, conflating them is an exploit waiting to happen:

| Authority | Type | Rotatable | Purpose |
|---|---|---|---|
| `authority` (governance) | `Pubkey` field on `Vault`/`Config` PDA | yes — by current authority via `set_authority` | gates admin actions: change rates, change fee, pause, fund rewards |
| `vault_authority` | program-derived PDA (no off-chain key) | no | signs SPL Token CPIs that move user funds out of the pool — `transfer`, `mint_to`, `burn` |

Keep them separate. Compromising `authority` should let an attacker change rates but **not drain the pool** — funds can only leave via the program's typed instructions, which are signed by the PDA (not the authority).

```rust
#[account]
pub struct Vault {
    pub authority: Pubkey,            // governance — admin-rotatable
    pub vault_authority_bump: u8,     // PDA bump — never rotated
    // ...
}

// Governance gate
#[derive(Accounts)]
pub struct SetFeeBps<'info> {
    #[account(mut, has_one = authority)]
    pub vault: Account<'info, Vault>,
    pub authority: Signer<'info>,
}

// Fund movement — PDA-signed
#[derive(Accounts)]
pub struct Withdraw<'info> {
    pub vault: Account<'info, Vault>,
    /// CHECK: PDA. Signs the SPL transfer-out CPI. Not rotatable; identity is intrinsic.
    #[account(seeds = [b"vault_authority", vault.key().as_ref()], bump = vault.vault_authority_bump)]
    pub vault_authority: UncheckedAccount<'info>,
    // ... reserve, receiver, user, token_program
}
```

The vault_authority PDA itself never stores a field — it's the program-derived identity, signed for via `invoke_signed` whenever the program needs to move pool funds.

Used by `examples/token-fundraiser` (the `Fundraiser` PDA is the vault TokenAccount's authority — signing for it on `claim` and `refund`), `examples/escrow` (per-offer vault_authority PDA), and `examples/erc4626-vault` (vault_authority PDA dual-rules as both Mint authority and reserve authority).

## Common mistakes porting from EVM

- **Translating `msg.sender == owner` as a `require_keys_eq` without a `Signer`**: EVM has implicit signer; Anchor needs explicit. Always pair the key check with a signer requirement.
- **Forgetting `mut` on the signer when they pay rent**: `init` constraints require `payer = signer` where `signer` is `Signer<'info>` *and* `mut` (because lamports leave their account).
- **Trusting an `AccountInfo` because "the client will set it right"**: never. All inputs are adversarial.
- **Using `Signer<'info>` on a PDA**: PDAs are not signers from the runtime's view in the outer transaction. Pass them as `AccountInfo` or `UncheckedAccount` with `/// CHECK:` notes, and sign them via CPI when the program needs to act through them.

## Multisig and threshold signers

Native Solana has no transaction-level multisig; it is implemented at the application level (e.g. **Squads Protocol**). When porting a Solidity contract that uses a multisig (`Gnosis Safe`-owner), the translation is "set the program's `authority` to a Squads multisig PDA"; the multisig already handles the threshold logic.

For role-based access (`AccessControl`), see `translation/stdlib-mapping.md`.
