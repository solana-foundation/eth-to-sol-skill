// Pass 2: Solana-native refactor of ExampleToken.
//
// The actual token mechanics — transfer, approve, transferFrom, balances, allowances,
// totalSupply — are delegated to SPL Token. This program owns only the *governance*
// layer: the configuration that gates minting, the max-supply cap, and ownership of
// the mint authority.
//
// User-visible consequences:
// - `transfer`, `approve`, `transferFrom` are not instructions in this program.
//   Holders call SPL Token directly. SPL Token already emits Transfer/Approval events.
// - Balances live in Associated Token Accounts (ATAs), one per (holder, mint).
//   Transfers between disjoint pairs parallelize via Sealevel write locks.
// - The mint authority is a PDA owned by this program, so only `mint_to` (gated by
//   `Config.authority`) can issue new supply.

use anchor_lang::prelude::*;
use anchor_spl::token::{self, Burn, Mint, MintTo, Token, TokenAccount};

declare_id!("Eth2So1OptimizedTokenProgramAddrXXXXXXXXXX");

#[program]
pub mod erc20_native {
    use super::*;

    /// Create the Mint, the program-owned mint authority PDA, and the Config PDA.
    /// Maps to Solidity `constructor(name, symbol, decimals, maxSupply)`.
    /// Note: `name`/`symbol` are not stored on-chain — they belong in Metaplex
    /// Token Metadata, not in this program's account.
    pub fn initialize(
        ctx: Context<Initialize>,
        _decimals: u8,
        max_supply: u64,
    ) -> Result<()> {
        let config = &mut ctx.accounts.config;
        config.authority = ctx.accounts.authority.key();
        config.mint = ctx.accounts.mint.key();
        config.max_supply = max_supply;
        config.bump = ctx.bumps.config;
        config.mint_authority_bump = ctx.bumps.mint_authority;
        Ok(())
    }

    /// Owner-gated mint. Maps to Solidity `mint(to, amount) onlyOwner`.
    /// The program signs for the mint authority PDA via `invoke_signed` (handled by
    /// Anchor's `CpiContext::new_with_signer`).
    pub fn mint_to(ctx: Context<MintToInstruction>, amount: u64) -> Result<()> {
        let config = &ctx.accounts.config;

        // Max-supply enforcement using SPL Mint's `supply`, not a self-tracked field.
        // Checked arithmetic — Rust release builds wrap silently otherwise.
        let new_supply = ctx
            .accounts
            .mint
            .supply
            .checked_add(amount)
            .ok_or(TokenError::Overflow)?;
        require!(
            new_supply <= config.max_supply,
            TokenError::MaxSupplyExceeded
        );

        let mint_key = ctx.accounts.mint.key();
        let seeds: &[&[u8]] = &[
            b"mint_authority",
            mint_key.as_ref(),
            &[config.mint_authority_bump],
        ];

        token::mint_to(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.key(),
                MintTo {
                    mint: ctx.accounts.mint.to_account_info(),
                    to: ctx.accounts.recipient.to_account_info(),
                    authority: ctx.accounts.mint_authority.to_account_info(),
                },
                &[seeds],
            ),
            amount,
        )?;
        Ok(())
    }

    /// User-initiated burn. Maps to Solidity `burn(amount)`.
    /// The holder is the signer; SPL Token verifies they own the ATA.
    pub fn burn(ctx: Context<BurnInstruction>, amount: u64) -> Result<()> {
        token::burn(
            CpiContext::new(
                ctx.accounts.token_program.key(),
                Burn {
                    mint: ctx.accounts.mint.to_account_info(),
                    from: ctx.accounts.holder_ata.to_account_info(),
                    authority: ctx.accounts.holder.to_account_info(),
                },
            ),
            amount,
        )?;
        Ok(())
    }

    /// Rotate governance authority. Maps to Solidity `transferOwnership(newOwner)`.
    /// Note this changes who can call `mint_to`; it does *not* change the on-chain
    /// SPL mint authority (which stays the program-owned PDA, as it must to keep the
    /// program in control of supply).
    pub fn set_authority(
        ctx: Context<SetAuthority>,
        new_authority: Pubkey,
    ) -> Result<()> {
        require_keys_neq!(new_authority, Pubkey::default(), TokenError::ZeroAddress);
        ctx.accounts.config.authority = new_authority;
        Ok(())
    }
}

// ---- Accounts ----

#[derive(Accounts)]
#[instruction(decimals: u8)]
pub struct Initialize<'info> {
    /// The deploying governance authority. Becomes `Config.authority`.
    #[account(mut)]
    pub authority: Signer<'info>,

    /// Singleton governance account, one per mint.
    #[account(
        init,
        payer = authority,
        space = 8 + Config::SIZE,
        seeds = [b"config", mint.key().as_ref()],
        bump,
    )]
    pub config: Account<'info, Config>,

    /// PDA that owns the Mint's `mint_authority`. The program signs for this PDA
    /// when minting; no off-chain key exists.
    /// CHECK: PDA, validated by seeds + bump. Never deserialized.
    #[account(
        seeds = [b"mint_authority", mint.key().as_ref()],
        bump,
    )]
    pub mint_authority: UncheckedAccount<'info>,

    /// The SPL Mint itself. `mint_authority` is the program's PDA above.
    #[account(
        init,
        payer = authority,
        mint::decimals = decimals,
        mint::authority = mint_authority,
    )]
    pub mint: Account<'info, Mint>,

    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct MintToInstruction<'info> {
    /// has_one = mint + has_one = authority enforces both links automatically.
    #[account(
        seeds = [b"config", mint.key().as_ref()],
        bump = config.bump,
        has_one = mint,
        has_one = authority,
    )]
    pub config: Account<'info, Config>,

    /// Must sign. Anchor enforces is_signer; has_one above enforces identity.
    pub authority: Signer<'info>,

    #[account(mut)]
    pub mint: Account<'info, Mint>,

    /// CHECK: PDA, validated by seeds + cached bump.
    #[account(
        seeds = [b"mint_authority", mint.key().as_ref()],
        bump = config.mint_authority_bump,
    )]
    pub mint_authority: UncheckedAccount<'info>,

    /// Token account to receive freshly minted tokens. Must be for this mint.
    #[account(
        mut,
        token::mint = mint,
    )]
    pub recipient: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct BurnInstruction<'info> {
    #[account(mut)]
    pub mint: Account<'info, Mint>,

    pub holder: Signer<'info>,

    /// The holder's ATA. token::authority constraint enforces ownership.
    #[account(
        mut,
        token::mint = mint,
        token::authority = holder,
    )]
    pub holder_ata: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct SetAuthority<'info> {
    #[account(
        mut,
        seeds = [b"config", config.mint.as_ref()],
        bump = config.bump,
        has_one = authority,
    )]
    pub config: Account<'info, Config>,

    pub authority: Signer<'info>,
}

// ---- State ----

#[account]
pub struct Config {
    /// Governance authority — gates `mint_to` and `set_authority`.
    pub authority: Pubkey,
    /// The SPL Mint this config governs.
    pub mint: Pubkey,
    /// Hard cap on `mint.supply`. Checked at every `mint_to`.
    pub max_supply: u64,
    /// Canonical bump for the config PDA.
    pub bump: u8,
    /// Canonical bump for the mint_authority PDA.
    pub mint_authority_bump: u8,
}

impl Config {
    pub const SIZE: usize = 32 + 32 + 8 + 1 + 1;
}

// ---- Errors ----

#[error_code]
pub enum TokenError {
    #[msg("arithmetic overflow")]
    Overflow,
    #[msg("max supply exceeded")]
    MaxSupplyExceeded,
    #[msg("zero address")]
    ZeroAddress,
}

// Notes on what is intentionally NOT in this file:
//
// - `transfer`, `approve`, `transfer_from` instructions: clients call SPL Token directly.
//   Single delegate per ATA (SPL's `approve`) covers the common case; the rare case where
//   multi-spender allowances are required would justify a custom allowance PDA — out of
//   scope for the standard ERC-20 shape.
//
// - On-chain `name` / `symbol` / `decimals` storage: belongs in Metaplex Token Metadata.
//   SPL Mint stores `decimals` natively (set above via `mint::decimals = decimals`).
//
// - Custom Transfer/Approval events: SPL Token already emits them via program logs.
//   Duplicating costs CU and adds nothing.
//
// - A `total_supply` field on Config: redundant with `mint.supply`. Self-tracking would
//   also write-lock Config on every mint, defeating parallelism for governance reads.
//
// - A `paused` flag: SPL Token's `freeze_authority` already supports per-account freeze.
//   For a true global pause, switch to Token-2022 with the appropriate extension.
