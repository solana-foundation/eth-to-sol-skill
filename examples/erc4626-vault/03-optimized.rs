// Pass 2: Solana-native refactor of ExampleVault (ERC-4626).
//
// Structural moves:
// - Shares are an SPL Token Mint owned by the program via a `vault_authority`
//   PDA. `mint_to` / `burn` flow through SPL Token; share transfer/approve
//   happen on SPL Token directly (clients don't go through this program for
//   share ERC-20 mechanics).
// - `totalAssets` and `totalSupply` are NOT stored on the vault. They read
//   directly from `asset_reserve.amount` and `share_mint.supply` — the
//   sources of truth maintained atomically by SPL Token.
// - `Vault` PDA holds only governance: asset_mint, share_mint, authority,
//   fee_bps, fee_recipient, and cached bumps. Deposits/withdrawals do NOT
//   write the vault account, which is a major parallelism win — only the
//   share Mint and the asset reserve are write-locked per call, and those
//   are inherent to having a single pool.
// - `mul_div(a, b, c, rounding)` is the single arithmetic primitive for the
//   4626 conversion math. All call sites pass the explicit `Rounding`
//   direction required by the spec (deposit/redeem round Down; mint/withdraw
//   round Up — both favor the vault).
// - Withdraw/redeem accept either the share owner *or* an SPL Token delegate
//   as the signer. SPL Token's `burn` performs the authority check; our
//   program does not re-implement allowance state.
//
// Security stance documented in code comments and in `05-explanation.md`:
//   inflation-attack defense (virtual offset), rounding direction at every
//   conversion site, and Token-2022 transfer-hook handling (rejected at the
//   type level — see `DECISIONS.md`).

use anchor_lang::prelude::*;
use anchor_spl::token::{self, Burn, Mint, MintTo, Token, TokenAccount, Transfer};

declare_id!("Vault4626Native1111111111111111111111111111");

// Virtual-offset inflation defense (OpenZeppelin pattern).
// virtual_shares = 10**DECIMALS_OFFSET, virtual_assets = 1.
const DECIMALS_OFFSET: u8 = 6;
const VIRTUAL_SHARES_OFFSET: u128 = 1_000_000; // 10^6
const VIRTUAL_ASSETS_OFFSET: u128 = 1;
const BPS_DENOMINATOR: u128 = 10_000;

/// Rounding direction for 4626 conversions. Always passed explicitly at the
/// call site — no defaults — so a code reviewer can verify direction matches
/// the spec without guessing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Rounding {
    Down,
    Up,
}

#[program]
pub mod vault4626_native {
    use super::*;

    pub fn initialize(
        ctx: Context<Initialize>,
        fee_bps: u16,
        fee_recipient: Pubkey,
        share_decimals: u8,
    ) -> Result<()> {
        require!(fee_bps <= 10_000, VaultError::InvalidFee);
        require_keys_neq!(fee_recipient, Pubkey::default(), VaultError::ZeroAddress);

        // Validate share decimals match the spec: share_decimals = asset_decimals + offset.
        let expected = ctx
            .accounts
            .asset_mint
            .decimals
            .checked_add(DECIMALS_OFFSET)
            .ok_or(VaultError::InvalidShareDecimals)?;
        require_eq!(
            share_decimals,
            expected,
            VaultError::InvalidShareDecimals
        );

        let vault = &mut ctx.accounts.vault;
        vault.asset_mint = ctx.accounts.asset_mint.key();
        vault.share_mint = ctx.accounts.share_mint.key();
        vault.authority = ctx.accounts.authority.key();
        vault.fee_bps = fee_bps;
        vault.fee_recipient = fee_recipient;
        vault.bump = ctx.bumps.vault;
        vault.vault_authority_bump = ctx.bumps.vault_authority;
        vault.asset_reserve_bump = ctx.bumps.asset_reserve;
        Ok(())
    }

    /// ERC-4626 `deposit(assets, receiver)`. Shares are rounded DOWN.
    pub fn deposit(ctx: Context<Deposit>, assets: u64) -> Result<()> {
        require!(assets > 0, VaultError::ZeroAssets);

        let total_supply = ctx.accounts.share_mint.supply;
        let total_assets = ctx.accounts.asset_reserve.amount;
        let shares = convert_to_shares(assets, total_supply, total_assets, Rounding::Down)?;
        require!(shares > 0, VaultError::ZeroShares);

        // Pull asset tokens from depositor.
        token::transfer(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.user_asset_ata.to_account_info(),
                    to: ctx.accounts.asset_reserve.to_account_info(),
                    authority: ctx.accounts.user.to_account_info(),
                },
            ),
            assets,
        )?;

        // Mint shares to receiver — vault_authority PDA signs.
        mint_shares(
            &ctx.accounts.share_mint,
            &ctx.accounts.receiver_share_ata,
            &ctx.accounts.vault_authority,
            &ctx.accounts.token_program,
            &ctx.accounts.vault,
            shares,
        )?;

        emit!(DepositEvent {
            sender: ctx.accounts.user.key(),
            receiver: ctx.accounts.receiver_share_ata.owner,
            assets,
            shares,
        });
        Ok(())
    }

    /// ERC-4626 `mint(shares, receiver)`. Assets are rounded UP.
    pub fn mint(ctx: Context<Deposit>, shares: u64) -> Result<()> {
        require!(shares > 0, VaultError::ZeroShares);

        let total_supply = ctx.accounts.share_mint.supply;
        let total_assets = ctx.accounts.asset_reserve.amount;
        let assets = convert_to_assets(shares, total_supply, total_assets, Rounding::Up)?;
        require!(assets > 0, VaultError::ZeroAssets);

        token::transfer(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.user_asset_ata.to_account_info(),
                    to: ctx.accounts.asset_reserve.to_account_info(),
                    authority: ctx.accounts.user.to_account_info(),
                },
            ),
            assets,
        )?;

        mint_shares(
            &ctx.accounts.share_mint,
            &ctx.accounts.receiver_share_ata,
            &ctx.accounts.vault_authority,
            &ctx.accounts.token_program,
            &ctx.accounts.vault,
            shares,
        )?;

        emit!(DepositEvent {
            sender: ctx.accounts.user.key(),
            receiver: ctx.accounts.receiver_share_ata.owner,
            assets,
            shares,
        });
        Ok(())
    }

    /// ERC-4626 `withdraw(assets, receiver, owner_)`. Shares are rounded UP.
    /// `signer` may be the share owner OR an SPL Token delegate of the share
    /// ATA. SPL Token's `burn` enforces the authority check.
    pub fn withdraw(ctx: Context<Withdraw>, assets: u64) -> Result<()> {
        require!(assets > 0, VaultError::ZeroAssets);
        require!(
            ctx.accounts.asset_reserve.amount >= assets,
            VaultError::InsufficientLiquidity
        );

        let total_supply = ctx.accounts.share_mint.supply;
        let total_assets = ctx.accounts.asset_reserve.amount;
        let shares = convert_to_shares(assets, total_supply, total_assets, Rounding::Up)?;
        require!(shares > 0, VaultError::ZeroShares);

        // Burn shares from owner's ATA — SPL Token verifies signer is owner or delegate.
        token::burn(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                Burn {
                    mint: ctx.accounts.share_mint.to_account_info(),
                    from: ctx.accounts.owner_share_ata.to_account_info(),
                    authority: ctx.accounts.signer.to_account_info(),
                },
            ),
            shares,
        )?;

        // Transfer asset out — vault_authority PDA signs.
        transfer_asset_out(
            &ctx.accounts.asset_reserve,
            &ctx.accounts.receiver_asset_ata,
            &ctx.accounts.vault_authority,
            &ctx.accounts.token_program,
            &ctx.accounts.vault,
            assets,
        )?;

        emit!(WithdrawEvent {
            sender: ctx.accounts.signer.key(),
            receiver: ctx.accounts.receiver_asset_ata.owner,
            owner: ctx.accounts.owner_share_ata.owner,
            assets,
            shares,
        });
        Ok(())
    }

    /// ERC-4626 `redeem(shares, receiver, owner_)`. Assets are rounded DOWN.
    pub fn redeem(ctx: Context<Withdraw>, shares: u64) -> Result<()> {
        require!(shares > 0, VaultError::ZeroShares);

        let total_supply = ctx.accounts.share_mint.supply;
        let total_assets = ctx.accounts.asset_reserve.amount;
        let assets = convert_to_assets(shares, total_supply, total_assets, Rounding::Down)?;
        require!(assets > 0, VaultError::ZeroAssets);
        require!(
            ctx.accounts.asset_reserve.amount >= assets,
            VaultError::InsufficientLiquidity
        );

        token::burn(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                Burn {
                    mint: ctx.accounts.share_mint.to_account_info(),
                    from: ctx.accounts.owner_share_ata.to_account_info(),
                    authority: ctx.accounts.signer.to_account_info(),
                },
            ),
            shares,
        )?;

        transfer_asset_out(
            &ctx.accounts.asset_reserve,
            &ctx.accounts.receiver_asset_ata,
            &ctx.accounts.vault_authority,
            &ctx.accounts.token_program,
            &ctx.accounts.vault,
            assets,
        )?;

        emit!(WithdrawEvent {
            sender: ctx.accounts.signer.key(),
            receiver: ctx.accounts.receiver_asset_ata.owner,
            owner: ctx.accounts.owner_share_ata.owner,
            assets,
            shares,
        });
        Ok(())
    }

    /// Push realized yield into the vault. Mints fee shares to fee_recipient
    /// at the pre-yield price.
    pub fn earn(ctx: Context<Earn>, yield_amount: u64) -> Result<()> {
        if yield_amount == 0 {
            return Ok(());
        }

        // Snapshot pre-yield totals BEFORE the asset transfer — the fee is
        // computed at the pre-yield price so existing holders capture the
        // net-of-fee appreciation.
        let total_supply_before = ctx.accounts.share_mint.supply;
        let total_assets_before = ctx.accounts.asset_reserve.amount;

        let mut fee_shares: u64 = 0;
        if ctx.accounts.vault.fee_bps > 0 && total_supply_before > 0 {
            // fee_assets = yield_amount * fee_bps / 10000, rounded DOWN (favor vault holders).
            let fee_assets_u128 = (yield_amount as u128)
                .checked_mul(ctx.accounts.vault.fee_bps as u128)
                .ok_or(VaultError::Overflow)?
                .checked_div(BPS_DENOMINATOR)
                .ok_or(VaultError::DivByZero)?;
            // fee_shares at pre-yield price, rounded DOWN.
            fee_shares = mul_div_u128_to_u64(
                fee_assets_u128,
                (total_supply_before as u128)
                    .checked_add(VIRTUAL_SHARES_OFFSET)
                    .ok_or(VaultError::Overflow)?,
                (total_assets_before as u128)
                    .checked_add(VIRTUAL_ASSETS_OFFSET)
                    .ok_or(VaultError::Overflow)?,
                Rounding::Down,
            )?;

            if fee_shares > 0 {
                mint_shares(
                    &ctx.accounts.share_mint,
                    &ctx.accounts.fee_recipient_share_ata,
                    &ctx.accounts.vault_authority,
                    &ctx.accounts.token_program,
                    &ctx.accounts.vault,
                    fee_shares,
                )?;
            }
        }

        // Transfer yield from the caller (authority) into the reserve.
        token::transfer(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.authority_asset_ata.to_account_info(),
                    to: ctx.accounts.asset_reserve.to_account_info(),
                    authority: ctx.accounts.authority.to_account_info(),
                },
            ),
            yield_amount,
        )?;

        emit!(EarnEvent {
            gross_yield: yield_amount,
            fee_shares,
        });
        Ok(())
    }

    pub fn set_fee_bps(ctx: Context<AdminAction>, new_bps: u16) -> Result<()> {
        require!(new_bps <= 10_000, VaultError::InvalidFee);
        let vault = &mut ctx.accounts.vault;
        let old = vault.fee_bps;
        vault.fee_bps = new_bps;
        emit!(FeeBpsUpdated { old, new_bps });
        Ok(())
    }

    pub fn set_fee_recipient(
        ctx: Context<AdminAction>,
        new_recipient: Pubkey,
    ) -> Result<()> {
        require_keys_neq!(new_recipient, Pubkey::default(), VaultError::ZeroAddress);
        let vault = &mut ctx.accounts.vault;
        let old = vault.fee_recipient;
        vault.fee_recipient = new_recipient;
        emit!(FeeRecipientUpdated {
            old,
            new_recipient,
        });
        Ok(())
    }

    pub fn set_authority(
        ctx: Context<AdminAction>,
        new_authority: Pubkey,
    ) -> Result<()> {
        require_keys_neq!(new_authority, Pubkey::default(), VaultError::ZeroAddress);
        ctx.accounts.vault.authority = new_authority;
        Ok(())
    }
}

// ---- 4626 conversion math ----

fn convert_to_shares(
    assets: u64,
    total_supply: u64,
    total_assets: u64,
    rounding: Rounding,
) -> Result<u64> {
    mul_div_u128_to_u64(
        assets as u128,
        (total_supply as u128)
            .checked_add(VIRTUAL_SHARES_OFFSET)
            .ok_or(VaultError::Overflow)?,
        (total_assets as u128)
            .checked_add(VIRTUAL_ASSETS_OFFSET)
            .ok_or(VaultError::Overflow)?,
        rounding,
    )
}

fn convert_to_assets(
    shares: u64,
    total_supply: u64,
    total_assets: u64,
    rounding: Rounding,
) -> Result<u64> {
    mul_div_u128_to_u64(
        shares as u128,
        (total_assets as u128)
            .checked_add(VIRTUAL_ASSETS_OFFSET)
            .ok_or(VaultError::Overflow)?,
        (total_supply as u128)
            .checked_add(VIRTUAL_SHARES_OFFSET)
            .ok_or(VaultError::Overflow)?,
        rounding,
    )
}

/// Compute `a * b / c` in u128, rounding per direction, then narrow to u64
/// with an explicit bounds check. Every step is checked; no silent wrap or
/// truncation is possible.
fn mul_div_u128_to_u64(a: u128, b: u128, c: u128, rounding: Rounding) -> Result<u64> {
    require!(c > 0, VaultError::DivByZero);
    let product = a.checked_mul(b).ok_or(VaultError::Overflow)?;
    let result_u128 = match rounding {
        Rounding::Down => product.checked_div(c).ok_or(VaultError::DivByZero)?,
        Rounding::Up => {
            // ceil(product / c) = (product + c - 1) / c
            let c_minus_one = c.checked_sub(1).ok_or(VaultError::Overflow)?;
            let raised = product.checked_add(c_minus_one).ok_or(VaultError::Overflow)?;
            raised.checked_div(c).ok_or(VaultError::DivByZero)?
        }
    };
    require!(
        result_u128 <= u64::MAX as u128,
        VaultError::Overflow
    );
    Ok(result_u128 as u64)
}

// ---- CPI helpers — vault_authority signs ----

fn mint_shares<'info>(
    share_mint: &Account<'info, Mint>,
    receiver_share_ata: &Account<'info, TokenAccount>,
    vault_authority: &UncheckedAccount<'info>,
    token_program: &Program<'info, Token>,
    vault: &Account<'info, Vault>,
    amount: u64,
) -> Result<()> {
    let vault_key = vault.key();
    let bump = vault.vault_authority_bump;
    let signer_seeds: &[&[u8]] = &[b"vault_authority", vault_key.as_ref(), &[bump]];
    token::mint_to(
        CpiContext::new_with_signer(
            token_program.to_account_info(),
            MintTo {
                mint: share_mint.to_account_info(),
                to: receiver_share_ata.to_account_info(),
                authority: vault_authority.to_account_info(),
            },
            &[signer_seeds],
        ),
        amount,
    )
}

fn transfer_asset_out<'info>(
    asset_reserve: &Account<'info, TokenAccount>,
    receiver_asset_ata: &Account<'info, TokenAccount>,
    vault_authority: &UncheckedAccount<'info>,
    token_program: &Program<'info, Token>,
    vault: &Account<'info, Vault>,
    amount: u64,
) -> Result<()> {
    let vault_key = vault.key();
    let bump = vault.vault_authority_bump;
    let signer_seeds: &[&[u8]] = &[b"vault_authority", vault_key.as_ref(), &[bump]];
    token::transfer(
        CpiContext::new_with_signer(
            token_program.to_account_info(),
            Transfer {
                from: asset_reserve.to_account_info(),
                to: receiver_asset_ata.to_account_info(),
                authority: vault_authority.to_account_info(),
            },
            &[signer_seeds],
        ),
        amount,
    )
}

// ---- Accounts ----

#[derive(Accounts)]
#[instruction(fee_bps: u16, fee_recipient: Pubkey, share_decimals: u8)]
pub struct Initialize<'info> {
    #[account(
        init,
        payer = authority,
        space = 8 + Vault::SIZE,
        seeds = [b"vault", asset_mint.key().as_ref()],
        bump,
    )]
    pub vault: Account<'info, Vault>,

    /// Asset underlying. Classic SPL Token only — Token-2022 mints fail the
    /// `Mint` deserialization (different owning program). This is the
    /// intentional Token-2022 / transfer-hook rejection; see DECISIONS.md.
    pub asset_mint: Account<'info, Mint>,

    /// CHECK: PDA, signs both share-mint and asset-reserve operations.
    /// Validated by seeds + bump; never deserialized.
    #[account(
        seeds = [b"vault_authority", vault.key().as_ref()],
        bump,
    )]
    pub vault_authority: UncheckedAccount<'info>,

    #[account(
        init,
        payer = authority,
        mint::decimals = share_decimals,
        mint::authority = vault_authority,
        seeds = [b"share_mint", asset_mint.key().as_ref()],
        bump,
    )]
    pub share_mint: Account<'info, Mint>,

    #[account(
        init,
        payer = authority,
        token::mint = asset_mint,
        token::authority = vault_authority,
        seeds = [b"asset_reserve", vault.key().as_ref()],
        bump,
    )]
    pub asset_reserve: Account<'info, TokenAccount>,

    #[account(mut)]
    pub authority: Signer<'info>,

    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,
    pub rent: Sysvar<'info, Rent>,
}

/// Shared by `deposit` and `mint`. Vault is read-only — no field on the vault
/// changes during a deposit (`fee_bps`, `fee_recipient`, etc. are admin-only),
/// so cross-user deposits don't write-conflict on the vault account.
#[derive(Accounts)]
pub struct Deposit<'info> {
    #[account(
        seeds = [b"vault", asset_mint.key().as_ref()],
        bump = vault.bump,
        has_one = asset_mint,
        has_one = share_mint,
    )]
    pub vault: Account<'info, Vault>,

    pub asset_mint: Account<'info, Mint>,

    #[account(mut)]
    pub share_mint: Account<'info, Mint>,

    /// CHECK: PDA, validated by seeds + cached bump. Signer for mint_to CPI.
    #[account(
        seeds = [b"vault_authority", vault.key().as_ref()],
        bump = vault.vault_authority_bump,
    )]
    pub vault_authority: UncheckedAccount<'info>,

    #[account(
        mut,
        seeds = [b"asset_reserve", vault.key().as_ref()],
        bump = vault.asset_reserve_bump,
        token::mint = asset_mint,
        token::authority = vault_authority,
    )]
    pub asset_reserve: Account<'info, TokenAccount>,

    #[account(mut, token::mint = asset_mint, token::authority = user)]
    pub user_asset_ata: Account<'info, TokenAccount>,

    #[account(mut, token::mint = share_mint)]
    pub receiver_share_ata: Account<'info, TokenAccount>,

    pub user: Signer<'info>,
    pub token_program: Program<'info, Token>,
}

/// Shared by `withdraw` and `redeem`. Vault is read-only. `signer` may be
/// the share-ATA owner or an SPL Token delegate — SPL Token's `burn` enforces.
#[derive(Accounts)]
pub struct Withdraw<'info> {
    #[account(
        seeds = [b"vault", asset_mint.key().as_ref()],
        bump = vault.bump,
        has_one = asset_mint,
        has_one = share_mint,
    )]
    pub vault: Account<'info, Vault>,

    pub asset_mint: Account<'info, Mint>,

    #[account(mut)]
    pub share_mint: Account<'info, Mint>,

    /// CHECK: PDA. Signer for transfer-out CPI.
    #[account(
        seeds = [b"vault_authority", vault.key().as_ref()],
        bump = vault.vault_authority_bump,
    )]
    pub vault_authority: UncheckedAccount<'info>,

    #[account(
        mut,
        seeds = [b"asset_reserve", vault.key().as_ref()],
        bump = vault.asset_reserve_bump,
        token::mint = asset_mint,
        token::authority = vault_authority,
    )]
    pub asset_reserve: Account<'info, TokenAccount>,

    /// Source of the burn. SPL Token verifies `signer` is `owner_share_ata.owner`
    /// or `owner_share_ata.delegate` (with sufficient delegated_amount).
    /// No `token::authority` constraint here — both paths must be allowed.
    #[account(mut, token::mint = share_mint)]
    pub owner_share_ata: Account<'info, TokenAccount>,

    #[account(mut, token::mint = asset_mint)]
    pub receiver_asset_ata: Account<'info, TokenAccount>,

    pub signer: Signer<'info>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct Earn<'info> {
    #[account(
        seeds = [b"vault", asset_mint.key().as_ref()],
        bump = vault.bump,
        has_one = asset_mint,
        has_one = share_mint,
        has_one = authority,
        constraint = fee_recipient_share_ata.owner == vault.fee_recipient
            @ VaultError::FeeRecipientMismatch,
    )]
    pub vault: Account<'info, Vault>,

    pub asset_mint: Account<'info, Mint>,

    #[account(mut)]
    pub share_mint: Account<'info, Mint>,

    /// CHECK: PDA, signs share mint_to.
    #[account(
        seeds = [b"vault_authority", vault.key().as_ref()],
        bump = vault.vault_authority_bump,
    )]
    pub vault_authority: UncheckedAccount<'info>,

    #[account(
        mut,
        seeds = [b"asset_reserve", vault.key().as_ref()],
        bump = vault.asset_reserve_bump,
        token::mint = asset_mint,
        token::authority = vault_authority,
    )]
    pub asset_reserve: Account<'info, TokenAccount>,

    #[account(mut, token::mint = asset_mint, token::authority = authority)]
    pub authority_asset_ata: Account<'info, TokenAccount>,

    #[account(mut, token::mint = share_mint)]
    pub fee_recipient_share_ata: Account<'info, TokenAccount>,

    pub authority: Signer<'info>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct AdminAction<'info> {
    #[account(
        mut,
        seeds = [b"vault", vault.asset_mint.as_ref()],
        bump = vault.bump,
        has_one = authority,
    )]
    pub vault: Account<'info, Vault>,
    pub authority: Signer<'info>,
}

// ---- State ----

#[account]
pub struct Vault {
    pub asset_mint: Pubkey,
    pub share_mint: Pubkey,
    /// Governance authority — gates fee setters and `earn` (the yield source).
    pub authority: Pubkey,
    pub fee_bps: u16,
    pub fee_recipient: Pubkey, // raw owner pubkey; ATA passed at call sites
    pub bump: u8,
    pub vault_authority_bump: u8,
    pub asset_reserve_bump: u8,
}

impl Vault {
    pub const SIZE: usize = 32 + 32 + 32 + 2 + 32 + 1 + 1 + 1; // 133 bytes
}

// ---- Events ----

#[event]
pub struct DepositEvent {
    pub sender: Pubkey,
    pub receiver: Pubkey,
    pub assets: u64,
    pub shares: u64,
}

#[event]
pub struct WithdrawEvent {
    pub sender: Pubkey,
    pub receiver: Pubkey,
    pub owner: Pubkey,
    pub assets: u64,
    pub shares: u64,
}

#[event]
pub struct EarnEvent {
    pub gross_yield: u64,
    pub fee_shares: u64,
}

#[event]
pub struct FeeBpsUpdated {
    pub old: u16,
    pub new_bps: u16,
}

#[event]
pub struct FeeRecipientUpdated {
    pub old: Pubkey,
    pub new_recipient: Pubkey,
}

// ---- Errors ----

#[error_code]
pub enum VaultError {
    #[msg("zero address")]
    ZeroAddress,
    #[msg("zero assets")]
    ZeroAssets,
    #[msg("zero shares")]
    ZeroShares,
    #[msg("invalid fee — must be ≤ 10000 bps")]
    InvalidFee,
    #[msg("share decimals must equal asset decimals + DECIMALS_OFFSET")]
    InvalidShareDecimals,
    #[msg("insufficient asset liquidity in the vault")]
    InsufficientLiquidity,
    #[msg("arithmetic overflow")]
    Overflow,
    #[msg("division by zero")]
    DivByZero,
    #[msg("fee recipient ATA owner does not match vault.fee_recipient")]
    FeeRecipientMismatch,
}
