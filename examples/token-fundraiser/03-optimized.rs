//! Idiomatic Solana version. Per-supporter PDA replaces the `Vec`, so
//! contributors don't serialize on a shared write-hot account, the cap is
//! gone, and `refund()` looks up its own row in O(1) by PDA derivation.
//!
//! Lifecycle matches `tokens/token-fundraiser` from
//! solana-developers/program-examples — see `04-diff.md` for the
//! section-by-section differences from the naive port.

use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, Token, TokenAccount, Transfer};

declare_id!("FundrSyROpt1m1zedAnchOr1234567890ABCDEFGHJK");

const FUNDRAISER_SEED: &[u8] = b"fundraiser";
const VAULT_SEED: &[u8] = b"vault";
const CONTRIBUTOR_SEED: &[u8] = b"contributor";

#[program]
pub mod fundraiser {
    use super::*;

    pub fn initialize(
        ctx: Context<Initialize>,
        goal: u64,
        duration: i64,
    ) -> Result<()> {
        require!(goal > 0, FundraiserError::ZeroGoal);
        require!(duration > 0, FundraiserError::ZeroDuration);

        let f = &mut ctx.accounts.fundraiser;
        f.token = ctx.accounts.token_mint.key();
        f.creator = ctx.accounts.creator.key();
        f.goal = goal;
        f.deadline = Clock::get()?.unix_timestamp + duration;
        f.total_raised = 0;
        f.claimed = false;
        f.bump = ctx.bumps.fundraiser;
        Ok(())
    }

    pub fn contribute(ctx: Context<Contribute>, amount: u64) -> Result<()> {
        require!(amount > 0, FundraiserError::ZeroAmount);
        require!(
            Clock::get()?.unix_timestamp < ctx.accounts.fundraiser.deadline,
            FundraiserError::Ended
        );

        // Pull tokens from the supporter into the per-fundraiser vault.
        let cpi = CpiContext::new(
            ctx.accounts.token_program.to_account_info(),
            Transfer {
                from: ctx.accounts.supporter_ata.to_account_info(),
                to: ctx.accounts.vault.to_account_info(),
                authority: ctx.accounts.supporter.to_account_info(),
            },
        );
        token::transfer(cpi, amount)?;

        // Update aggregate + per-supporter ledger. `init_if_needed` makes the
        // first contribution create the PDA, subsequent ones top it up.
        ctx.accounts.fundraiser.total_raised = ctx.accounts.fundraiser
            .total_raised
            .checked_add(amount)
            .ok_or(FundraiserError::Overflow)?;

        let c = &mut ctx.accounts.contributor;
        c.amount = c.amount
            .checked_add(amount)
            .ok_or(FundraiserError::Overflow)?;
        Ok(())
    }

    pub fn claim(ctx: Context<Claim>) -> Result<()> {
        let f = &mut ctx.accounts.fundraiser;
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
                authority: f.to_account_info(),
            },
            signer_seeds,
        );
        token::transfer(cpi, amount)?;
        Ok(())
    }

    pub fn refund(ctx: Context<Refund>) -> Result<()> {
        require!(
            Clock::get()?.unix_timestamp >= ctx.accounts.fundraiser.deadline,
            FundraiserError::NotEnded
        );
        require!(
            ctx.accounts.fundraiser.total_raised < ctx.accounts.fundraiser.goal,
            FundraiserError::GoalMet
        );

        let amount = ctx.accounts.contributor.amount;
        require!(amount > 0, FundraiserError::NothingToRefund);

        // Aggregate decrements first so the on-chain invariant holds even
        // if the CPI fails mid-call.
        ctx.accounts.fundraiser.total_raised = ctx.accounts.fundraiser
            .total_raised
            .checked_sub(amount)
            .ok_or(FundraiserError::Overflow)?;

        let creator_key = ctx.accounts.fundraiser.creator;
        let bump = ctx.accounts.fundraiser.bump;
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

        // Closing the contributor PDA refunds rent and prevents replay —
        // a second refund() for the same supporter will fail at account
        // validation, not just at the `amount > 0` check.
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
    #[account(
        mut,
        seeds = [FUNDRAISER_SEED, fundraiser.creator.as_ref()],
        bump = fundraiser.bump,
    )]
    pub fundraiser: Account<'info, Fundraiser>,
    #[account(
        mut,
        seeds = [VAULT_SEED, fundraiser.key().as_ref()],
        bump,
    )]
    pub vault: Account<'info, TokenAccount>,
    #[account(mut)]
    pub supporter_ata: Account<'info, TokenAccount>,
    #[account(
        init_if_needed,
        payer = supporter,
        space = 8 + Contributor::INIT_SPACE,
        seeds = [
            CONTRIBUTOR_SEED,
            fundraiser.key().as_ref(),
            supporter.key().as_ref(),
        ],
        bump,
    )]
    pub contributor: Account<'info, Contributor>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Claim<'info> {
    /// Account validation enforces signer == creator via the PDA seeds —
    /// no runtime require!(creator == f.creator) needed.
    #[account(mut)]
    pub creator: Signer<'info>,
    #[account(
        mut,
        seeds = [FUNDRAISER_SEED, creator.key().as_ref()],
        bump = fundraiser.bump,
        has_one = creator,
    )]
    pub fundraiser: Account<'info, Fundraiser>,
    #[account(
        mut,
        seeds = [VAULT_SEED, fundraiser.key().as_ref()],
        bump,
    )]
    pub vault: Account<'info, TokenAccount>,
    #[account(mut)]
    pub creator_ata: Account<'info, TokenAccount>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct Refund<'info> {
    #[account(mut)]
    pub supporter: Signer<'info>,
    #[account(
        mut,
        seeds = [FUNDRAISER_SEED, fundraiser.creator.as_ref()],
        bump = fundraiser.bump,
    )]
    pub fundraiser: Account<'info, Fundraiser>,
    #[account(
        mut,
        seeds = [VAULT_SEED, fundraiser.key().as_ref()],
        bump,
    )]
    pub vault: Account<'info, TokenAccount>,
    #[account(mut)]
    pub supporter_ata: Account<'info, TokenAccount>,
    #[account(
        mut,
        close = supporter,
        seeds = [
            CONTRIBUTOR_SEED,
            fundraiser.key().as_ref(),
            supporter.key().as_ref(),
        ],
        bump,
    )]
    pub contributor: Account<'info, Contributor>,
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
    // No more Vec — per-supporter state moved to its own PDA.
}

#[account]
#[derive(InitSpace)]
pub struct Contributor {
    pub amount: u64,
}

#[error_code]
pub enum FundraiserError {
    #[msg("contribution must be > 0")]
    ZeroAmount,
    #[msg("goal must be > 0")]
    ZeroGoal,
    #[msg("duration must be > 0")]
    ZeroDuration,
    #[msg("fundraiser has ended")]
    Ended,
    #[msg("fundraiser has not yet ended")]
    NotEnded,
    #[msg("goal already met — no refunds")]
    GoalMet,
    #[msg("goal not yet met")]
    GoalNotMet,
    #[msg("creator already claimed the pot")]
    AlreadyClaimed,
    #[msg("nothing to refund for this signer")]
    NothingToRefund,
    #[msg("arithmetic overflow")]
    Overflow,
}
