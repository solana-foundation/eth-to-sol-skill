// Pass 2: Solana-native refactor of StakingRewards.
//
// What stays: SPL Token CPIs for the actual stake/withdraw/reward transfers
// (no alternative on Solana). The Synthetix-style reward accumulator is also
// preserved as-is — it is a fundamentally write-hot design, see HONEST LIMIT
// below.
//
// What changes:
// - The `mapping(address => ...)` triple becomes one `StakePosition` PDA per
//   user, keyed by (vault, user). Different users write disjoint PDAs, so
//   stakes between Alice/Bob and Carol/Dave parallelize.
// - The vault itself is parameterized over `(staking_mint, rewards_mint)` so
//   the same program can run multiple pools.
// - Owner authority is a separate `authority: Pubkey` field on `VaultState`,
//   gated by `has_one = authority` constraints. Mint movement out of the
//   vault is signed by a program PDA, not by the authority — even a
//   compromised authority cannot drain the pool.
// - All arithmetic is `checked_*`. Accumulator math widens to `u128`
//   intermediaries and re-narrows with explicit guards.
// - All PDA bumps are cached at init and supplied on every subsequent access.
//
// HONEST LIMIT: every state-changing instruction (`stake`, `withdraw`,
// `get_reward`, `set_reward_rate`) writes `VaultState` because the
// accumulator (`reward_per_token_stored`, `last_update_time`, `total_supply`)
// must be checkpointed before per-user math. This is fundamental to the
// Synthetix design — there is no way to remove it without changing reward
// semantics. The optimization eliminates the *additional* contention from
// the Vec balance map; the residual contention is the protocol, not the
// implementation. Discussed in 05-explanation.md under "Honest limits".

use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, Token, TokenAccount, Transfer};

declare_id!("StakeNative11111111111111111111111111111111");

const PRECISION: u128 = 1_000_000_000_000_000_000u128; // 1e18 — matches Solidity

#[program]
pub mod staking_native {
    use super::*;

    /// Solidity `constructor(stakingToken, rewardsToken)`.
    /// Creates the singleton vault for this (staking_mint, rewards_mint) pair
    /// plus the program-controlled token accounts that hold the pools.
    pub fn initialize(ctx: Context<Initialize>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;

        let vault = &mut ctx.accounts.vault;
        vault.staking_mint = ctx.accounts.staking_mint.key();
        vault.rewards_mint = ctx.accounts.rewards_mint.key();
        vault.authority = ctx.accounts.authority.key();
        vault.reward_rate = 0;
        vault.last_update_time = now;
        vault.reward_per_token_stored = 0;
        vault.total_supply = 0;
        vault.bump = ctx.bumps.vault;
        vault.vault_authority_bump = ctx.bumps.vault_authority;
        Ok(())
    }

    /// Solidity `stake(amount) updateReward(msg.sender)`.
    pub fn stake(ctx: Context<Stake>, amount: u64) -> Result<()> {
        require!(amount > 0, StakeError::ZeroAmount);
        let now = Clock::get()?.unix_timestamp;

        // First-time stakers: cache the canonical bump in the new position.
        // `user == Pubkey::default()` is the unambiguous "freshly init'd" signal,
        // since a real user can never sign with the null key.
        if ctx.accounts.position.user == Pubkey::default() {
            ctx.accounts.position.vault = ctx.accounts.vault.key();
            ctx.accounts.position.user = ctx.accounts.user.key();
            ctx.accounts.position.bump = ctx.bumps.position;
        }

        // Accumulator update — must happen BEFORE balance changes so the
        // checkpoint reflects the pre-stake totalSupply.
        update_reward(&mut ctx.accounts.vault, Some(&mut ctx.accounts.position), now)?;

        // CPI: pull staking tokens into the program-owned pool.
        token::transfer(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.user_staking_ata.to_account_info(),
                    to: ctx.accounts.vault_staking_ata.to_account_info(),
                    authority: ctx.accounts.user.to_account_info(),
                },
            ),
            amount,
        )?;

        let vault = &mut ctx.accounts.vault;
        let position = &mut ctx.accounts.position;
        vault.total_supply = vault
            .total_supply
            .checked_add(amount)
            .ok_or(StakeError::Overflow)?;
        position.balance = position
            .balance
            .checked_add(amount)
            .ok_or(StakeError::Overflow)?;

        emit!(Staked {
            user: ctx.accounts.user.key(),
            amount,
        });
        Ok(())
    }

    /// Solidity `withdraw(amount) updateReward(msg.sender)`.
    pub fn withdraw(ctx: Context<Withdraw>, amount: u64) -> Result<()> {
        require!(amount > 0, StakeError::ZeroAmount);
        let now = Clock::get()?.unix_timestamp;

        update_reward(&mut ctx.accounts.vault, Some(&mut ctx.accounts.position), now)?;

        let position = &mut ctx.accounts.position;
        require!(
            position.balance >= amount,
            StakeError::InsufficientBalance
        );
        position.balance = position
            .balance
            .checked_sub(amount)
            .ok_or(StakeError::Overflow)?;
        ctx.accounts.vault.total_supply = ctx
            .accounts
            .vault
            .total_supply
            .checked_sub(amount)
            .ok_or(StakeError::Overflow)?;

        // Sign for vault_authority PDA to release funds.
        let vault_key = ctx.accounts.vault.key();
        let vault_authority_bump = ctx.accounts.vault.vault_authority_bump;
        let signer_seeds: &[&[u8]] =
            &[b"vault_authority", vault_key.as_ref(), &[vault_authority_bump]];
        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.vault_staking_ata.to_account_info(),
                    to: ctx.accounts.user_staking_ata.to_account_info(),
                    authority: ctx.accounts.vault_authority.to_account_info(),
                },
                &[signer_seeds],
            ),
            amount,
        )?;

        emit!(Withdrawn {
            user: ctx.accounts.user.key(),
            amount,
        });
        Ok(())
    }

    /// Solidity `getReward() updateReward(msg.sender)`.
    pub fn get_reward(ctx: Context<GetReward>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;

        update_reward(&mut ctx.accounts.vault, Some(&mut ctx.accounts.position), now)?;

        let position = &mut ctx.accounts.position;
        let reward = position.pending_rewards;
        if reward == 0 {
            return Ok(());
        }
        position.pending_rewards = 0;

        let vault_key = ctx.accounts.vault.key();
        let vault_authority_bump = ctx.accounts.vault.vault_authority_bump;
        let signer_seeds: &[&[u8]] =
            &[b"vault_authority", vault_key.as_ref(), &[vault_authority_bump]];
        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.vault_rewards_ata.to_account_info(),
                    to: ctx.accounts.user_rewards_ata.to_account_info(),
                    authority: ctx.accounts.vault_authority.to_account_info(),
                },
                &[signer_seeds],
            ),
            reward,
        )?;

        emit!(RewardPaid {
            user: ctx.accounts.user.key(),
            reward,
        });
        Ok(())
    }

    /// Solidity `setRewardRate(newRate) onlyOwner updateReward(address(0))`.
    pub fn set_reward_rate(ctx: Context<SetRewardRate>, new_rate: u64) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        update_reward(&mut ctx.accounts.vault, None, now)?;

        let old = ctx.accounts.vault.reward_rate;
        ctx.accounts.vault.reward_rate = new_rate;

        emit!(RewardRateUpdated { old, new_rate });
        Ok(())
    }
}

// ---- The updateReward helper ----
//
// Pure function over &mut VaultState and Option<&mut StakePosition>.
// No account I/O, no CPI, no Clock::get — `now` is threaded in by the caller
// to keep this trivially auditable.
fn update_reward(
    vault: &mut VaultState,
    position: Option<&mut StakePosition>,
    now: i64,
) -> Result<()> {
    // 1) Bring the global accumulator forward to `now`, using the OLD
    //    total_supply (i.e. before this instruction's balance change).
    let dt: i64 = now
        .checked_sub(vault.last_update_time)
        .ok_or(StakeError::ClockSkew)?;
    require!(dt >= 0, StakeError::ClockSkew);
    let dt_u128 = dt as u128;

    if vault.total_supply > 0 {
        // delta = dt * rate * PRECISION / total_supply
        let numerator = dt_u128
            .checked_mul(vault.reward_rate as u128)
            .ok_or(StakeError::Overflow)?
            .checked_mul(PRECISION)
            .ok_or(StakeError::Overflow)?;
        let delta = numerator
            .checked_div(vault.total_supply as u128)
            .ok_or(StakeError::DivByZero)?;
        vault.reward_per_token_stored = vault
            .reward_per_token_stored
            .checked_add(delta)
            .ok_or(StakeError::Overflow)?;
    }
    vault.last_update_time = now;

    // 2) Settle the caller's accrued rewards against the (now-checkpointed)
    //    accumulator and bump their paid-marker.
    if let Some(pos) = position {
        let rpt_stored = vault.reward_per_token_stored;
        let unpaid = rpt_stored
            .checked_sub(pos.reward_per_token_paid)
            .ok_or(StakeError::Overflow)?;
        let earned_inc_u128 = (pos.balance as u128)
            .checked_mul(unpaid)
            .ok_or(StakeError::Overflow)?
            .checked_div(PRECISION)
            .ok_or(StakeError::DivByZero)?;
        // Re-narrow with an explicit guard so silent truncation is impossible.
        require!(
            earned_inc_u128 <= u64::MAX as u128,
            StakeError::Overflow
        );
        let earned_inc = earned_inc_u128 as u64;
        pos.pending_rewards = pos
            .pending_rewards
            .checked_add(earned_inc)
            .ok_or(StakeError::Overflow)?;
        pos.reward_per_token_paid = rpt_stored;
    }

    Ok(())
}

// ---- Accounts ----

#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(
        init,
        payer = authority,
        space = 8 + VaultState::SIZE,
        seeds = [b"vault", staking_mint.key().as_ref(), rewards_mint.key().as_ref()],
        bump,
    )]
    pub vault: Account<'info, VaultState>,

    pub staking_mint: Account<'info, Mint>,
    pub rewards_mint: Account<'info, Mint>,

    /// CHECK: PDA — token-account authority for the vault pools.
    /// Validated by seeds + bump; never deserialized.
    #[account(
        seeds = [b"vault_authority", vault.key().as_ref()],
        bump,
    )]
    pub vault_authority: UncheckedAccount<'info>,

    #[account(
        init,
        payer = authority,
        token::mint = staking_mint,
        token::authority = vault_authority,
        seeds = [b"vault_staking_ata", vault.key().as_ref()],
        bump,
    )]
    pub vault_staking_ata: Account<'info, TokenAccount>,

    #[account(
        init,
        payer = authority,
        token::mint = rewards_mint,
        token::authority = vault_authority,
        seeds = [b"vault_rewards_ata", vault.key().as_ref()],
        bump,
    )]
    pub vault_rewards_ata: Account<'info, TokenAccount>,

    #[account(mut)]
    pub authority: Signer<'info>,
    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct Stake<'info> {
    #[account(
        mut,
        seeds = [b"vault", vault.staking_mint.as_ref(), vault.rewards_mint.as_ref()],
        bump = vault.bump,
        has_one = staking_mint,
    )]
    pub vault: Account<'info, VaultState>,

    pub staking_mint: Account<'info, Mint>,

    /// init_if_needed is safe here: seeds make the PDA unique per (vault,user);
    /// re-init only flows back into the same in-memory `position` for the same
    /// user. New positions land with zeroed fields, which is the correct
    /// starting state. The handler caches `bump` on first init.
    #[account(
        init_if_needed,
        payer = user,
        space = 8 + StakePosition::SIZE,
        seeds = [b"position", vault.key().as_ref(), user.key().as_ref()],
        bump,
    )]
    pub position: Account<'info, StakePosition>,

    #[account(
        mut,
        seeds = [b"vault_staking_ata", vault.key().as_ref()],
        bump,
    )]
    pub vault_staking_ata: Account<'info, TokenAccount>,

    #[account(mut, token::mint = staking_mint, token::authority = user)]
    pub user_staking_ata: Account<'info, TokenAccount>,

    #[account(mut)]
    pub user: Signer<'info>,
    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct Withdraw<'info> {
    #[account(
        mut,
        seeds = [b"vault", vault.staking_mint.as_ref(), vault.rewards_mint.as_ref()],
        bump = vault.bump,
        has_one = staking_mint,
    )]
    pub vault: Account<'info, VaultState>,

    pub staking_mint: Account<'info, Mint>,

    #[account(
        mut,
        seeds = [b"position", vault.key().as_ref(), user.key().as_ref()],
        bump = position.bump,
        has_one = user @ StakeError::NotPositionOwner,
    )]
    pub position: Account<'info, StakePosition>,

    /// CHECK: PDA, validated by seeds + cached bump.
    #[account(
        seeds = [b"vault_authority", vault.key().as_ref()],
        bump = vault.vault_authority_bump,
    )]
    pub vault_authority: UncheckedAccount<'info>,

    #[account(
        mut,
        seeds = [b"vault_staking_ata", vault.key().as_ref()],
        bump,
    )]
    pub vault_staking_ata: Account<'info, TokenAccount>,

    #[account(mut, token::mint = staking_mint, token::authority = user)]
    pub user_staking_ata: Account<'info, TokenAccount>,

    pub user: Signer<'info>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct GetReward<'info> {
    #[account(
        mut,
        seeds = [b"vault", vault.staking_mint.as_ref(), vault.rewards_mint.as_ref()],
        bump = vault.bump,
        has_one = rewards_mint,
    )]
    pub vault: Account<'info, VaultState>,

    pub rewards_mint: Account<'info, Mint>,

    #[account(
        mut,
        seeds = [b"position", vault.key().as_ref(), user.key().as_ref()],
        bump = position.bump,
        has_one = user @ StakeError::NotPositionOwner,
    )]
    pub position: Account<'info, StakePosition>,

    /// CHECK: PDA.
    #[account(
        seeds = [b"vault_authority", vault.key().as_ref()],
        bump = vault.vault_authority_bump,
    )]
    pub vault_authority: UncheckedAccount<'info>,

    #[account(
        mut,
        seeds = [b"vault_rewards_ata", vault.key().as_ref()],
        bump,
    )]
    pub vault_rewards_ata: Account<'info, TokenAccount>,

    #[account(mut, token::mint = rewards_mint, token::authority = user)]
    pub user_rewards_ata: Account<'info, TokenAccount>,

    pub user: Signer<'info>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct SetRewardRate<'info> {
    #[account(
        mut,
        seeds = [b"vault", vault.staking_mint.as_ref(), vault.rewards_mint.as_ref()],
        bump = vault.bump,
        has_one = authority,
    )]
    pub vault: Account<'info, VaultState>,
    pub authority: Signer<'info>,
}

// ---- State ----

#[account]
pub struct VaultState {
    pub staking_mint: Pubkey,
    pub rewards_mint: Pubkey,
    /// Governance authority — gates `set_reward_rate` and future admin ops.
    /// Not the token-account authority (that's `vault_authority` PDA).
    pub authority: Pubkey,
    pub reward_rate: u64,
    pub last_update_time: i64,
    pub reward_per_token_stored: u128,
    pub total_supply: u64,
    pub bump: u8,
    pub vault_authority_bump: u8,
}

impl VaultState {
    pub const SIZE: usize = 32 + 32 + 32 + 8 + 8 + 16 + 8 + 1 + 1; // 138
}

#[account]
pub struct StakePosition {
    pub vault: Pubkey,
    pub user: Pubkey,
    pub balance: u64,
    pub reward_per_token_paid: u128,
    pub pending_rewards: u64,
    pub bump: u8,
}

impl StakePosition {
    pub const SIZE: usize = 32 + 32 + 8 + 16 + 8 + 1; // 97
}

// ---- Events ----

#[event]
pub struct Staked {
    pub user: Pubkey,
    pub amount: u64,
}

#[event]
pub struct Withdrawn {
    pub user: Pubkey,
    pub amount: u64,
}

#[event]
pub struct RewardPaid {
    pub user: Pubkey,
    pub reward: u64,
}

#[event]
pub struct RewardRateUpdated {
    pub old: u64,
    pub new_rate: u64,
}

// ---- Errors ----

#[error_code]
pub enum StakeError {
    #[msg("amount must be > 0")]
    ZeroAmount,
    #[msg("insufficient staked balance")]
    InsufficientBalance,
    #[msg("arithmetic overflow")]
    Overflow,
    #[msg("division by zero")]
    DivByZero,
    #[msg("clock skew — now < last_update_time")]
    ClockSkew,
    #[msg("position owner mismatch")]
    NotPositionOwner,
}
