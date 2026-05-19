# PDAs — seed design and bump discipline

Program-Derived Addresses are the Solana primitive that has no clean EVM analog. Get this right and the rest of the design falls into place.

## What a PDA is

A PDA is a `Pubkey` that:

- Is **deterministically derived** from a set of seeds and a program ID.
- Lies **off the ed25519 curve**, so it has no private key — nobody can sign for it directly.
- Can be "signed for" by its owning program via `invoke_signed`, presenting the seeds + bump as proof.

This is how programs hold authority over accounts. The program *is* the signer for any PDA derived from its own ID.

## Deriving a PDA

```rust
let (pda, bump) = Pubkey::find_program_address(
    &[b"vault", user.key().as_ref()],
    program_id,
);
```

`find_program_address` increments a bump value (starting at 255, descending) until the resulting address is off-curve. The bump that worked is the **canonical bump**. Always use it.

In Anchor, this is automatic:

```rust
#[account(
    init,
    payer = user,
    space = 8 + Vault::SIZE,
    seeds = [b"vault", user.key().as_ref()],
    bump,
)]
pub vault: Account<'info, Vault>,
```

`bump` (with no value) tells Anchor to find the canonical bump. `bump = stored_bump` (with a value) tells Anchor to verify the supplied bump is the same one used at init.

## Seed design

Seeds are byte arrays. Each seed is ≤32 bytes; total ≤16 seeds. Compose them so that:

1. **Uniqueness:** the seed set uniquely identifies the account's purpose. `[b"vault", user.as_ref()]` is unique per user.
2. **Discoverability:** anyone who knows the relevant identities can derive the PDA address without on-chain lookup.
3. **Stability:** seeds must not depend on data that changes over the PDA's lifetime. Once derived, the address is fixed.

Canonical seed patterns:

| Purpose | Seeds |
|---|---|
| Per-user state | `[b"<entity>", user.as_ref()]` |
| Per-pair state (e.g. allowance) | `[b"<entity>", a.as_ref(), b.as_ref()]` (order-sensitive — pick a convention) |
| Per-token state | `[b"<entity>", mint.as_ref()]` |
| Program-owned authority | `[b"<entity>_authority", mint.as_ref()]` |
| Counter / sequence | `[b"<entity>", index.to_le_bytes().as_ref()]` |
| Singleton | `[b"<entity>"]` |

Prefix every seed set with a `b"<entity>"` literal to namespace it within the program. Without prefixes, two unrelated entities could collide if their key sets happen to overlap (rare but real).

## Parameterized PDAs for multi-instance protocols

Solidity often deploys one contract per market / pool / vault. On Solana that's ~2 SOL per deploy in bytecode rent. The canonical Solana pattern is one program with parameterized PDAs:

```rust
// staking-vault: one pool per (staking_mint, rewards_mint) pair
seeds = [b"vault", staking_mint.as_ref(), rewards_mint.as_ref()]

// 4626 vault: one vault per underlying asset
seeds = [b"vault", asset_mint.as_ref()]

// AMM: one pool per (base_mint, quote_mint, fee_tier) triple
seeds = [b"pool", base_mint.as_ref(), quote_mint.as_ref(), &fee_tier.to_le_bytes()]

// Lending: one market per oracle / collateral pair
seeds = [b"market", oracle.as_ref()]
```

Same program code path serves N instances. Pair with per-instance **authority** PDAs:

```rust
seeds = [b"vault_authority", vault.key().as_ref()]
```

so each instance's funds are signed by a distinct PDA — isolates blast radius across instances. A bug in one pool's withdrawal logic cannot move funds in another pool because the PDA-signed CPI scopes the authority to a specific instance.

Cost comparison:

- Solidity (one contract per pool): ~2 SOL bytecode rent per deploy + governance per pool.
- Solana (parameterized PDA): ~0.005 SOL per pool (vault PDA + supporting accounts), governance scope is "the program" not "each pool."

When the program is upgraded, all pools migrate atomically — a property to think about carefully (single point of failure across pools). The right answer depends on protocol economics; document the decision.

## Bump caching — the rule

Calling `find_program_address` on-chain costs ~1500-2500 CU. It iterates until it finds the canonical bump. On every subsequent access to the same PDA, you should *not* recompute — you should pass the cached bump.

**Pattern:**

1. **At init**, derive the bump with `bump` (no value) and store `ctx.bumps.<name>` on the account.
2. **On every subsequent call**, supply the stored bump with `bump = <field>.bump`.

```rust
#[account]
pub struct Vault {
    pub owner: Pubkey,
    pub amount: u64,
    pub bump: u8,  // canonical bump, cached at init
}

// init:
#[account(init, payer = owner, space = 8 + 32 + 8 + 1, seeds = [b"vault", owner.key().as_ref()], bump)]
pub vault: Account<'info, Vault>,
// then: vault.bump = ctx.bumps.vault;

// later use:
#[account(seeds = [b"vault", owner.key().as_ref()], bump = vault.bump)]
pub vault: Account<'info, Vault>,
```

Two reasons to cache:

1. **CU savings.** Re-derivation is hundreds of CU per use, compounding across instructions.
2. **Canonicalization (the bigger reason).** Without the stored bump, an attacker could supply a *non-canonical* bump that produces a different off-curve address with the same seeds. If your access-control logic relies on PDA identity, this is a vulnerability. Cached canonical bumps eliminate the class. See `security/pda-canonicalization.md`.

## Signing with a PDA (invoke_signed / CpiContext::new_with_signer)

When your program controls a PDA and wants to use it as a CPI signer:

```rust
let mint_key = ctx.accounts.mint.key();
let seeds: &[&[u8]] = &[
    b"mint_authority",
    mint_key.as_ref(),
    &[config.mint_authority_bump],
];
let signer_seeds = &[seeds];

token::mint_to(
    CpiContext::new_with_signer(
        ctx.accounts.token_program.key(),
        MintTo {
            mint: ctx.accounts.mint.to_account_info(),
            to: ctx.accounts.recipient.to_account_info(),
            authority: ctx.accounts.mint_authority.to_account_info(),
        },
        signer_seeds,
    ),
    amount,
)?;
```

The double-slice (`&[seeds]`) is correct — `signer_seeds` is a slice of slice-of-byte-slices, supporting multiple PDA signers per CPI.

## Common PDA mistakes (porting from EVM)

- **Using `init_if_needed` to "look up or create"**: this opens a re-init bug if the program does not check the existing state. Prefer separate `init` and `update` instructions, gated by who can call each.
- **Storing the bump but not using it on subsequent access**: defeats the canonical-bump protection. Always re-supply via `bump = stored.bump`.
- **Deriving on each call without caching**: works, but burns CU and forfeits the canonicalization guarantee.
- **Using user-supplied data as part of a seed that affects authority**: an attacker can choose seeds. Make sure your seeds are constrained to identities you've validated.

## When *not* to use PDAs

- For accounts that exist only for the lifetime of one transaction (rare — use ephemeral keypairs or just don't).
- When the address needs to be assignable to an arbitrary public key (e.g. a fee recipient chosen at runtime). Use a stored `Pubkey` field, not a derived PDA.
- For ATAs of SPL tokens — those use the **Associated Token Program**'s deterministic derivation, not your own.
