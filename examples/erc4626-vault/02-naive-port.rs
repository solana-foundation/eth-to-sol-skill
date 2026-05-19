// Pass 1: Faithful Anchor port of ExampleVault (ERC-4626).
//
// BASELINE — semantically identical to the Solidity, translated construct-by-construct.
// `// SMELL:` markers flag the patterns the optimized version (03-optimized.rs) replaces.
//
// What's faithful:
// - One `VaultState` account holds asset_mint, owner, fee_bps, fee_recipient,
//   _totalAssets, totalSupply, and per-user share balances — mirroring the Solidity
//   contract's storage layout directly.
// - Share balances live in `Vec<BalanceEntry>` (the consolidated "ERC-20 inside the
//   vault contract" pattern). The optimized version replaces this with an SPL Token
//   Mint for shares.
// - Arithmetic uses u128 widening (no other way to do 4626 math without it) but the
//   narrow-back-to-u64 cast is unchecked, mirroring an EVM dev's first attempt.
// - Rounding directions match the Solidity (Floor on deposit/redeem, Ceil on
//   mint/withdraw) — but the ceiling computation `(num + den - 1)` itself can
//   overflow without a check.
//
// What is NOT faithful, by necessity:
// - Asset reserve: a program-owned SPL TokenAccount (authority = vault_authority PDA).
//   There is no Solana primitive other than SPL Token for moving fungible tokens.
// - The share-token ERC-20 surface (transfer/approve) is omitted from the naive port —
//   already exercised by the ERC-20 reference example; reintroduced via SPL Token in
//   the optimized version.
// - `withdraw`/`redeem` require `owner == msg.sender` (no delegate path). The
//   optimized version uses SPL Token's built-in delegate model, which has no clean
//   naive analog without reimplementing allowance maps.

use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, Token, TokenAccount, Transfer};

declare_id!("Vault4626Naive111111111111111111111111111111");

// Hard cap because Vec lives in a fixed-size account.
// SMELL: a real 4626 vault should not have a depositor cap; translation artifact.
const MAX_HOLDERS: usize = 100;

// Virtual-offset constants — match Solidity DECIMALS_OFFSET = 6.
const VIRTUAL_SHARES_OFFSET: u128 = 1_000_000; // 10^6
const VIRTUAL_ASSETS_OFFSET: u128 = 1;

#[program]
pub mod vault4626_naive {
    use super::*;

    /// Solidity `constructor(asset, feeBps, feeRecipient)`.
    pub fn initialize(
        ctx: Context<Initialize>,
        fee_bps: u16,
        fee_recipient: Pubkey,
    ) -> Result<()> {
        require!(fee_bps <= 10_000, VaultError::InvalidFee);
        require_keys_neq!(fee_recipient, Pubkey::default(), VaultError::ZeroAddress);

        let vault = &mut ctx.accounts.vault;
        vault.asset_mint = ctx.accounts.asset_mint.key();
        vault.owner = ctx.accounts.owner.key();
        vault.fee_bps = fee_bps;
        vault.fee_recipient = fee_recipient;
        vault.total_assets = 0;
        vault.total_supply = 0;
        vault.balances = Vec::new();
        // SMELL: vault_authority bump not cached.
        Ok(())
    }

    /// Solidity `deposit(assets, receiver)` — rounds shares DOWN (favors vault).
    pub fn deposit(ctx: Context<MoveIn>, assets: u64, receiver: Pubkey) -> Result<()> {
        require!(assets > 0, VaultError::ZeroAssets);

        let vault = &mut ctx.accounts.vault;
        let shares = preview_deposit(assets, vault.total_supply, vault.total_assets)?;
        require!(shares > 0, VaultError::ZeroShares);

        // SMELL: bare cast — silent truncation if computation ever exceeds u64::MAX.
        // (preview_deposit returns u64 already cast from u128 without bounds check.)

        // Update totals — SMELL: unchecked.
        vault.total_assets += assets;
        vault.total_supply += shares;

        // Mint shares to receiver — SMELL: O(n) Vec scan + write-lock.
        mint_to(vault, receiver, shares)?;

        // Pull assets in.
        token::transfer(
            CpiContext::new(
                ctx.accounts.token_program.key(),
                Transfer {
                    from: ctx.accounts.user_asset_ata.to_account_info(),
                    to: ctx.accounts.asset_reserve.to_account_info(),
                    authority: ctx.accounts.user.to_account_info(),
                },
            ),
            assets,
        )?;

        emit!(Deposit {
            sender: ctx.accounts.user.key(),
            receiver,
            assets,
            shares,
        });
        Ok(())
    }

    /// Solidity `mint(shares, receiver)` — rounds assets UP (favors vault).
    pub fn mint_shares(ctx: Context<MoveIn>, shares: u64, receiver: Pubkey) -> Result<()> {
        require!(shares > 0, VaultError::ZeroShares);

        let vault = &mut ctx.accounts.vault;
        let assets = preview_mint(shares, vault.total_supply, vault.total_assets)?;

        vault.total_assets += assets; // SMELL: unchecked
        vault.total_supply += shares; // SMELL: unchecked

        mint_to(vault, receiver, shares)?;

        token::transfer(
            CpiContext::new(
                ctx.accounts.token_program.key(),
                Transfer {
                    from: ctx.accounts.user_asset_ata.to_account_info(),
                    to: ctx.accounts.asset_reserve.to_account_info(),
                    authority: ctx.accounts.user.to_account_info(),
                },
            ),
            assets,
        )?;

        emit!(Deposit {
            sender: ctx.accounts.user.key(),
            receiver,
            assets,
            shares,
        });
        Ok(())
    }

    /// Solidity `withdraw(assets, receiver, owner_)` — rounds shares UP.
    /// SMELL: no delegate path; naive port requires owner == msg.sender.
    pub fn withdraw(
        ctx: Context<MoveOut>,
        assets: u64,
        receiver: Pubkey,
    ) -> Result<()> {
        require!(assets > 0, VaultError::ZeroAssets);

        let vault = &mut ctx.accounts.vault;
        require!(vault.total_assets >= assets, VaultError::InsufficientLiquidity);
        let shares = preview_withdraw(assets, vault.total_supply, vault.total_assets)?;

        let owner = ctx.accounts.user.key();
        burn_from(vault, owner, shares)?;
        vault.total_assets -= assets; // SMELL: unchecked
        vault.total_supply -= shares; // SMELL: unchecked

        // Send assets out — vault_authority PDA signs.
        let bump = ctx.bumps.vault_authority;
        let signer_seeds: &[&[u8]] = &[b"vault_authority", &[bump]];
        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.key(),
                Transfer {
                    from: ctx.accounts.asset_reserve.to_account_info(),
                    to: ctx.accounts.receiver_asset_ata.to_account_info(),
                    authority: ctx.accounts.vault_authority.to_account_info(),
                },
                &[signer_seeds],
            ),
            assets,
        )?;

        emit!(Withdraw {
            sender: owner,
            receiver,
            owner,
            assets,
            shares,
        });
        Ok(())
    }

    /// Solidity `redeem(shares, receiver, owner_)` — rounds assets DOWN.
    pub fn redeem(
        ctx: Context<MoveOut>,
        shares: u64,
        receiver: Pubkey,
    ) -> Result<()> {
        require!(shares > 0, VaultError::ZeroShares);

        let vault = &mut ctx.accounts.vault;
        let assets = preview_redeem(shares, vault.total_supply, vault.total_assets)?;
        require!(vault.total_assets >= assets, VaultError::InsufficientLiquidity);

        let owner = ctx.accounts.user.key();
        burn_from(vault, owner, shares)?;
        vault.total_assets -= assets; // SMELL: unchecked
        vault.total_supply -= shares; // SMELL: unchecked

        let bump = ctx.bumps.vault_authority;
        let signer_seeds: &[&[u8]] = &[b"vault_authority", &[bump]];
        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.key(),
                Transfer {
                    from: ctx.accounts.asset_reserve.to_account_info(),
                    to: ctx.accounts.receiver_asset_ata.to_account_info(),
                    authority: ctx.accounts.vault_authority.to_account_info(),
                },
                &[signer_seeds],
            ),
            assets,
        )?;

        emit!(Withdraw {
            sender: owner,
            receiver,
            owner,
            assets,
            shares,
        });
        Ok(())
    }

    /// Solidity `_earn(yield)` — owner pushes realized yield; fee minted to fee_recipient
    /// at the pre-yield price.
    pub fn earn(ctx: Context<Earn>, yield_amount: u64) -> Result<()> {
        if yield_amount == 0 {
            return Ok(());
        }

        let vault = &mut ctx.accounts.vault;
        require_keys_eq!(vault.owner, ctx.accounts.owner.key(), VaultError::NotOwner);

        let mut fee_shares: u64 = 0;
        if vault.fee_bps > 0 && vault.total_supply > 0 {
            // Fee in asset units. SMELL: bare math.
            let fee_assets =
                ((yield_amount as u128) * (vault.fee_bps as u128)) / 10_000u128;
            // Fee in share units, at pre-yield price.
            let num = fee_assets
                * ((vault.total_supply as u128) + VIRTUAL_SHARES_OFFSET);
            let den = (vault.total_assets as u128) + VIRTUAL_ASSETS_OFFSET;
            fee_shares = (num / den) as u64; // SMELL: silent truncation
            let recipient = vault.fee_recipient;
            mint_to(vault, recipient, fee_shares)?;
            vault.total_supply += fee_shares; // SMELL: unchecked
        }

        vault.total_assets += yield_amount; // SMELL: unchecked

        token::transfer(
            CpiContext::new(
                ctx.accounts.token_program.key(),
                Transfer {
                    from: ctx.accounts.owner_asset_ata.to_account_info(),
                    to: ctx.accounts.asset_reserve.to_account_info(),
                    authority: ctx.accounts.owner.to_account_info(),
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
        let vault = &mut ctx.accounts.vault;
        require_keys_eq!(vault.owner, ctx.accounts.owner.key(), VaultError::NotOwner);
        require!(new_bps <= 10_000, VaultError::InvalidFee);
        let old = vault.fee_bps;
        vault.fee_bps = new_bps;
        emit!(FeeBpsUpdated { old, new_bps });
        Ok(())
    }

    pub fn set_fee_recipient(ctx: Context<AdminAction>, new_recipient: Pubkey) -> Result<()> {
        let vault = &mut ctx.accounts.vault;
        require_keys_eq!(vault.owner, ctx.accounts.owner.key(), VaultError::NotOwner);
        require_keys_neq!(new_recipient, Pubkey::default(), VaultError::ZeroAddress);
        let old = vault.fee_recipient;
        vault.fee_recipient = new_recipient;
        emit!(FeeRecipientUpdated { old, new_recipient });
        Ok(())
    }

    pub fn transfer_ownership(ctx: Context<AdminAction>, new_owner: Pubkey) -> Result<()> {
        let vault = &mut ctx.accounts.vault;
        require_keys_eq!(vault.owner, ctx.accounts.owner.key(), VaultError::NotOwner);
        require_keys_neq!(new_owner, Pubkey::default(), VaultError::ZeroAddress);
        let prev = vault.owner;
        vault.owner = new_owner;
        emit!(OwnershipTransferred {
            previous_owner: prev,
            new_owner,
        });
        Ok(())
    }
}

// ---- Preview helpers (4626 conversion math) ----
//
// SMELL: each returns `result as u64` without bounds-check. For pathological
//        inputs (very large totals × very large args), the cast silently
//        truncates and the depositor either gets too few shares or the vault
//        accepts too few assets.

fn preview_deposit(assets: u64, total_supply: u64, total_assets: u64) -> Result<u64> {
    // Rounds DOWN — favors vault.
    let num = (assets as u128) * ((total_supply as u128) + VIRTUAL_SHARES_OFFSET);
    let den = (total_assets as u128) + VIRTUAL_ASSETS_OFFSET;
    Ok((num / den) as u64) // SMELL: silent truncation
}

fn preview_redeem(shares: u64, total_supply: u64, total_assets: u64) -> Result<u64> {
    // Rounds DOWN — favors vault.
    let num = (shares as u128) * ((total_assets as u128) + VIRTUAL_ASSETS_OFFSET);
    let den = (total_supply as u128) + VIRTUAL_SHARES_OFFSET;
    Ok((num / den) as u64) // SMELL: silent truncation
}

fn preview_mint(shares: u64, total_supply: u64, total_assets: u64) -> Result<u64> {
    // Rounds UP — user pays a bit more, favors vault.
    let num = (shares as u128) * ((total_assets as u128) + VIRTUAL_ASSETS_OFFSET);
    let den = (total_supply as u128) + VIRTUAL_SHARES_OFFSET;
    // SMELL: bare `num + den - 1` — can overflow u128 at extreme magnitudes.
    Ok(((num + den - 1) / den) as u64) // SMELL: silent truncation + add overflow
}

fn preview_withdraw(assets: u64, total_supply: u64, total_assets: u64) -> Result<u64> {
    // Rounds UP — user burns a bit more, favors vault.
    let num = (assets as u128) * ((total_supply as u128) + VIRTUAL_SHARES_OFFSET);
    let den = (total_assets as u128) + VIRTUAL_ASSETS_OFFSET;
    Ok(((num + den - 1) / den) as u64) // SMELL: silent truncation + add overflow
}

// ---- Internal share balance helpers (Vec-as-map antipattern) ----
//
// SMELL: linear scan + full-Vec serialize/deserialize on every share movement.

fn mint_to(vault: &mut VaultState, to: Pubkey, shares: u64) -> Result<()> {
    if let Some(entry) = vault.balances.iter_mut().find(|e| e.holder == to) {
        entry.amount += shares; // SMELL: unchecked
    } else {
        require!(vault.balances.len() < MAX_HOLDERS, VaultError::TooManyHolders);
        vault.balances.push(BalanceEntry {
            holder: to,
            amount: shares,
        });
    }
    Ok(())
}

fn burn_from(vault: &mut VaultState, from: Pubkey, shares: u64) -> Result<()> {
    let entry = vault
        .balances
        .iter_mut()
        .find(|e| e.holder == from)
        .ok_or(error!(VaultError::InsufficientShares))?;
    require!(entry.amount >= shares, VaultError::InsufficientShares);
    entry.amount -= shares; // SMELL: unchecked
    Ok(())
}

// ---- Accounts ----

#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(
        init,
        payer = owner,
        space = 8 + VaultState::SIZE,
        seeds = [b"vault"],
        bump,
    )]
    pub vault: Account<'info, VaultState>,

    pub asset_mint: Account<'info, Mint>,

    /// CHECK: PDA, not deserialized.
    #[account(seeds = [b"vault_authority"], bump)]
    pub vault_authority: UncheckedAccount<'info>,

    #[account(
        init,
        payer = owner,
        token::mint = asset_mint,
        token::authority = vault_authority,
        seeds = [b"asset_reserve"],
        bump,
    )]
    pub asset_reserve: Account<'info, TokenAccount>,

    #[account(mut)]
    pub owner: Signer<'info>,
    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct MoveIn<'info> {
    // SMELL: writable on every deposit / mint — single write-lock for the program.
    #[account(mut, seeds = [b"vault"], bump)]
    pub vault: Account<'info, VaultState>,

    /// CHECK: PDA.
    #[account(seeds = [b"vault_authority"], bump)]
    pub vault_authority: UncheckedAccount<'info>,

    #[account(mut, seeds = [b"asset_reserve"], bump)]
    pub asset_reserve: Account<'info, TokenAccount>,

    #[account(mut, token::mint = vault.asset_mint, token::authority = user)]
    pub user_asset_ata: Account<'info, TokenAccount>,

    pub user: Signer<'info>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct MoveOut<'info> {
    #[account(mut, seeds = [b"vault"], bump)]
    pub vault: Account<'info, VaultState>,

    /// CHECK: PDA.
    #[account(seeds = [b"vault_authority"], bump)]
    pub vault_authority: UncheckedAccount<'info>,

    #[account(mut, seeds = [b"asset_reserve"], bump)]
    pub asset_reserve: Account<'info, TokenAccount>,

    #[account(mut, token::mint = vault.asset_mint)]
    pub receiver_asset_ata: Account<'info, TokenAccount>,

    /// SMELL: no delegate support — owner == msg.sender enforced implicitly.
    pub user: Signer<'info>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct Earn<'info> {
    #[account(mut, seeds = [b"vault"], bump)]
    pub vault: Account<'info, VaultState>,

    /// CHECK: PDA.
    #[account(seeds = [b"vault_authority"], bump)]
    pub vault_authority: UncheckedAccount<'info>,

    #[account(mut, seeds = [b"asset_reserve"], bump)]
    pub asset_reserve: Account<'info, TokenAccount>,

    #[account(mut, token::mint = vault.asset_mint, token::authority = owner)]
    pub owner_asset_ata: Account<'info, TokenAccount>,

    pub owner: Signer<'info>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct AdminAction<'info> {
    #[account(mut, seeds = [b"vault"], bump)]
    pub vault: Account<'info, VaultState>,
    pub owner: Signer<'info>,
}

// ---- State ----

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

impl VaultState {
    pub const SIZE: usize = 32 + 32 + 2 + 32 + 8 + 8 + 4 + MAX_HOLDERS * BalanceEntry::SIZE;
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct BalanceEntry {
    pub holder: Pubkey,
    pub amount: u64,
}

impl BalanceEntry {
    pub const SIZE: usize = 32 + 8; // 40
}

// ---- Events ----

#[event]
pub struct Deposit {
    pub sender: Pubkey,
    pub receiver: Pubkey,
    pub assets: u64,
    pub shares: u64,
}

#[event]
pub struct Withdraw {
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

#[event]
pub struct OwnershipTransferred {
    pub previous_owner: Pubkey,
    pub new_owner: Pubkey,
}

// ---- Errors ----

#[error_code]
pub enum VaultError {
    #[msg("caller is not the owner")]
    NotOwner,
    #[msg("zero address")]
    ZeroAddress,
    #[msg("zero assets")]
    ZeroAssets,
    #[msg("zero shares")]
    ZeroShares,
    #[msg("invalid fee — must be ≤ 10000 bps")]
    InvalidFee,
    #[msg("insufficient asset liquidity in the vault")]
    InsufficientLiquidity,
    #[msg("insufficient share balance")]
    InsufficientShares,
    #[msg("too many holders for this account's capacity")]
    TooManyHolders,
}
