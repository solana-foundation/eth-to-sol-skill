//! Naive 1:1 port of `01-original.sol`. Mirrors the Solidity layout —
//! one state account holds the goal + creator + a `Vec<Contribution>`
//! that stands in for the `mapping(address => uint256)`. Compiles, works
//! end-to-end, but reproduces the EVM model's write-hot serialization
//! point and a hard-coded cap on contributors. See `03-optimized.rs` for
//! the idiomatic version and `04-diff.md` for the change-by-change diff.

use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, Token, TokenAccount, Transfer};

declare_id!("FundrSyR1sNa1ve111111111111111111111111111");

const FUNDRAISER_SEED: &[u8] = b"fundraiser";
const VAULT_SEED: &[u8] = b"vault";
const MAX_CONTRIBUTORS: usize = 50; // hard cap so the Vec fits in one account

#[program]
pub mod fundraiser_naive {
    use super::*;

    pub fn initialize(
        ctx: Context<Initialize>,
        goal: u64,
        duration: i64,
    ) -> Result<()> {
        let f = &mut ctx.accounts.fundraiser;
        f.token = ctx.accounts.token_mint.key();
        f.creator = ctx.accounts.creator.key();
        f.goal = goal;
        f.deadline = Clock::get()?.unix_timestamp + duration;
        f.total_raised = 0;
        f.claimed = false;
        f.contributors = Vec::new();
        f.bump = ctx.bumps.fundraiser;
        Ok(())
    }

    pub fn contribute(ctx: Context<Contribute>, amount: u64) -> Result<()> {
        require!(amount > 0, FundraiserError::ZeroAmount);
        let now = Clock::get()?.unix_timestamp;
        require!(now < ctx.accounts.fundraiser.deadline, FundraiserError::Ended);

        // Pull tokens from the supporter into the vault.
        let cpi = CpiContext::new(
            ctx.accounts.token_program.to_account_info(),
            Transfer {
                from: ctx.accounts.supporter_ata.to_account_info(),
                to: ctx.accounts.vault.to_account_info(),
                authority: ctx.accounts.supporter.to_account_info(),
            },
        );
        token::transfer(cpi, amount)?;

        let f = &mut ctx.accounts.fundraiser;
        f.total_raised = f.total_raised
            .checked_add(amount)
            .ok_or(FundraiserError::Overflow)?;

        let supporter_key = ctx.accounts.supporter.key();
        if let Some(c) =
            f.contributors.iter_mut().find(|c| c.who == supporter_key)
        {
            c.amount = c.amount
                .checked_add(amount)
                .ok_or(FundraiserError::Overflow)?;
        } else {
            require!(
                f.contributors.len() < MAX_CONTRIBUTORS,
                FundraiserError::TooManyContributors
            );
            f.contributors.push(Contribution {
                who: supporter_key,
                amount,
            });
        }
        Ok(())
    }

    pub fn claim(ctx: Context<Claim>) -> Result<()> {
        let f = &mut ctx.accounts.fundraiser;
        require!(
            f.creator == ctx.accounts.creator.key(),
            FundraiserError::NotCreator
        );
        require!(!f.claimed, FundraiserError::AlreadyClaimed);
        require!(f.total_raised >= f.goal, FundraiserError::GoalNotMet);

        f.claimed = true;
        let amount = f.total_raised;

        let creator_key = f.creator;
        let bump = f.bump;
        let signer_seeds: &[&[&[u8]]] =
            &[&[FUNDRAISER_SEED, creator_key.as_ref(), &[bump]]];

        let cpi = CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            Transfer {
                from: ctx.accounts.vault.to_account_info(),
                to: ctx.accounts.creator_ata.to_account_info(),
                authority: ctx.accounts.fundraiser.to_account_info(),
            },
            signer_seeds,
        );
        token::transfer(cpi, amount)?;
        Ok(())
    }

    pub fn refund(ctx: Context<Refund>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        require!(
            now >= ctx.accounts.fundraiser.deadline,
            FundraiserError::NotEnded
        );
        require!(
            ctx.accounts.fundraiser.total_raised < ctx.accounts.fundraiser.goal,
            FundraiserError::GoalMet
        );

        let f = &mut ctx.accounts.fundraiser;
        let supporter_key = ctx.accounts.supporter.key();
        let idx = f
            .contributors
            .iter()
            .position(|c| c.who == supporter_key)
            .ok_or(FundraiserError::NothingToRefund)?;
        let amount = f.contributors[idx].amount;
        require!(amount > 0, FundraiserError::NothingToRefund);
        f.contributors[idx].amount = 0;
        f.total_raised = f.total_raised
            .checked_sub(amount)
            .ok_or(FundraiserError::Overflow)?;

        let creator_key = f.creator;
        let bump = f.bump;
        let signer_seeds: &[&[&[u8]]] =
            &[&[FUNDRAISER_SEED, creator_key.as_ref(), &[bump]]];

        let cpi = CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            Transfer {
                from: ctx.accounts.vault.to_account_info(),
                to: ctx.accounts.supporter_ata.to_account_info(),
                authority: ctx.accounts.fundraiser.to_account_info(),
            },
            signer_seeds,
        );
        token::transfer(cpi, amount)?;
        Ok(())
    }
}

#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(mut)]
    pub creator: Signer<'info>,
    pub token_mint: Account<'info, Mint>,
    #[account(
        init,
        payer = creator,
        space = 8 + Fundraiser::INIT_SPACE,
        seeds = [FUNDRAISER_SEED, creator.key().as_ref()],
        bump,
    )]
    pub fundraiser: Account<'info, Fundraiser>,
    #[account(
        init,
        payer = creator,
        token::mint = token_mint,
        token::authority = fundraiser,
        seeds = [VAULT_SEED, fundraiser.key().as_ref()],
        bump,
    )]
    pub vault: Account<'info, TokenAccount>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct Contribute<'info> {
    #[account(mut)]
    pub supporter: Signer<'info>,
    #[account(mut, seeds = [FUNDRAISER_SEED, fundraiser.creator.as_ref()], bump = fundraiser.bump)]
    pub fundraiser: Account<'info, Fundraiser>,
    #[account(mut, seeds = [VAULT_SEED, fundraiser.key().as_ref()], bump)]
    pub vault: Account<'info, TokenAccount>,
    #[account(mut)]
    pub supporter_ata: Account<'info, TokenAccount>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct Claim<'info> {
    #[account(mut)]
    pub creator: Signer<'info>,
    #[account(mut, seeds = [FUNDRAISER_SEED, creator.key().as_ref()], bump = fundraiser.bump)]
    pub fundraiser: Account<'info, Fundraiser>,
    #[account(mut, seeds = [VAULT_SEED, fundraiser.key().as_ref()], bump)]
    pub vault: Account<'info, TokenAccount>,
    #[account(mut)]
    pub creator_ata: Account<'info, TokenAccount>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct Refund<'info> {
    #[account(mut)]
    pub supporter: Signer<'info>,
    #[account(mut, seeds = [FUNDRAISER_SEED, fundraiser.creator.as_ref()], bump = fundraiser.bump)]
    pub fundraiser: Account<'info, Fundraiser>,
    #[account(mut, seeds = [VAULT_SEED, fundraiser.key().as_ref()], bump)]
    pub vault: Account<'info, TokenAccount>,
    #[account(mut)]
    pub supporter_ata: Account<'info, TokenAccount>,
    pub token_program: Program<'info, Token>,
}

#[account]
#[derive(InitSpace)]
pub struct Fundraiser {
    pub token: Pubkey,
    pub creator: Pubkey,
    pub goal: u64,
    pub deadline: i64,
    pub total_raised: u64,
    pub claimed: bool,
    pub bump: u8,
    // SMELL: Vec inside a single state account = write-hot, capped, O(n) lookup.
    #[max_len(MAX_CONTRIBUTORS)]
    pub contributors: Vec<Contribution>,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, InitSpace)]
pub struct Contribution {
    pub who: Pubkey,
    pub amount: u64,
}

#[error_code]
pub enum FundraiserError {
    #[msg("contribution must be > 0")]
    ZeroAmount,
    #[msg("fundraiser has ended")]
    Ended,
    #[msg("fundraiser has not yet ended")]
    NotEnded,
    #[msg("signer is not the creator")]
    NotCreator,
    #[msg("goal already met — no refunds")]
    GoalMet,
    #[msg("goal not yet met")]
    GoalNotMet,
    #[msg("creator already claimed the pot")]
    AlreadyClaimed,
    #[msg("nothing to refund for this signer")]
    NothingToRefund,
    #[msg("contributor list is full")]
    TooManyContributors,
    #[msg("arithmetic overflow")]
    Overflow,
}
