// Pass 1: Faithful Anchor port of StakingRewards.
//
// BASELINE — semantically identical to the Solidity, restructured as little
// as possible. `// SMELL:` markers flag the patterns the optimized version
// (03-optimized.rs) replaces.
//
// What's faithful:
// - One `VaultState` account holds reward_rate, last_update_time,
//   reward_per_token_stored, total_supply, and the per-user state — directly
//   mirroring the Solidity contract's storage layout.
// - The three Solidity `mapping(address => ...)` are consolidated into
//   `Vec<UserStakeEntry>` inside the vault. A real one-to-one translation
//   would use three parallel Vecs; consolidating is the small concession to
//   Rust ergonomics. The antipattern (linear scan, full re-serialize, single
//   write-lock) is unchanged.
// - Arithmetic is bare `+=`/`-=`/`*` matching the Solidity source. Solidity
//   0.8+ checks this at runtime; Rust release builds wrap silently. This is
//   a regression-in-safety that the optimized version fixes.
//
// What is NOT faithful, by necessity:
// - Token movements use SPL Token CPI (`spl_token::transfer`). There is no
//   non-CPI way to move SPL tokens on Solana, so the naive port is forced
//   to use the right primitive here. The pool is a program-owned ATA whose
//   authority is a `vault_authority` PDA.

use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, Token, TokenAccount, Transfer};

declare_id!("StakeNaive1111111111111111111111111111111111");

// Hard cap because Vec lives in a fixed-size account.
// SMELL: a real staking contract should not have this cap; it's a translation
//        artifact of using Vec-as-map.
const MAX_STAKERS: usize = 100;

// 1e18 scaling factor — matches Solidity's rewardPerToken precision.
const PRECISION: u128 = 1_000_000_000_000_000_000u128;

#[program]
pub mod staking_naive {
    use super::*;

    /// Solidity `constructor(stakingToken, rewardsToken)`.
    pub fn initialize(ctx: Context<Initialize>) -> Result<()> {
        let vault = &mut ctx.accounts.vault;
        vault.staking_mint = ctx.accounts.staking_mint.key();
        vault.rewards_mint = ctx.accounts.rewards_mint.key();
        vault.owner = ctx.accounts.owner.key();
        vault.reward_rate = 0;
        vault.last_update_time = Clock::get()?.unix_timestamp;
        vault.reward_per_token_stored = 0;
        vault.total_supply = 0;
        vault.entries = Vec::new();
        // SMELL: vault_authority bump not cached.
        Ok(())
    }

    /// Solidity `stake(amount) updateReward(msg.sender)`.
    pub fn stake(ctx: Context<Stake>, amount: u64) -> Result<()> {
        require!(amount > 0, StakeError::ZeroAmount);
        let now = Clock::get()?.unix_timestamp;
        let user = ctx.accounts.user.key();

        update_reward(&mut ctx.accounts.vault, Some(user), now)?;

        // Pull tokens in from the user's ATA. This part is unavoidably a CPI
        // — there is no "raw" SPL token transfer.
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
        vault.total_supply += amount; // SMELL: unchecked
        let entry = upsert_entry(vault, user)?;
        entry.balance += amount; // SMELL: unchecked

        emit!(Staked { user, amount });
        Ok(())
    }

    /// Solidity `withdraw(amount) updateReward(msg.sender)`.
    pub fn withdraw(ctx: Context<Withdraw>, amount: u64) -> Result<()> {
        require!(amount > 0, StakeError::ZeroAmount);
        let now = Clock::get()?.unix_timestamp;
        let user = ctx.accounts.user.key();

        update_reward(&mut ctx.accounts.vault, Some(user), now)?;

        let vault = &mut ctx.accounts.vault;
        let entry = vault
            .entries
            .iter_mut()
            .find(|e| e.user == user)
            .ok_or(error!(StakeError::InsufficientBalance))?;
        require!(entry.balance >= amount, StakeError::InsufficientBalance);
        entry.balance -= amount; // SMELL: unchecked
        vault.total_supply -= amount; // SMELL: unchecked

        // Drain from vault ATA → user ATA. PDA signs.
        let bump = ctx.bumps.vault_authority;
        let signer_seeds: &[&[u8]] = &[b"vault_authority", &[bump]];
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

        emit!(Withdrawn { user, amount });
        Ok(())
    }

    /// Solidity `getReward() updateReward(msg.sender)`.
    pub fn get_reward(ctx: Context<GetReward>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let user = ctx.accounts.user.key();

        update_reward(&mut ctx.accounts.vault, Some(user), now)?;

        let vault = &mut ctx.accounts.vault;
        let entry = upsert_entry(vault, user)?;
        let reward = entry.rewards;
        if reward == 0 {
            return Ok(());
        }
        entry.rewards = 0;

        let bump = ctx.bumps.vault_authority;
        let signer_seeds: &[&[u8]] = &[b"vault_authority", &[bump]];
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

        emit!(RewardPaid { user, reward });
        Ok(())
    }

    /// Solidity `setRewardRate(newRate) onlyOwner updateReward(address(0))`.
    pub fn set_reward_rate(ctx: Context<SetRewardRate>, new_rate: u64) -> Result<()> {
        let vault = &mut ctx.accounts.vault;
        require_keys_eq!(vault.owner, ctx.accounts.owner.key(), StakeError::NotOwner);

        let now = Clock::get()?.unix_timestamp;
        update_reward(vault, None, now)?;

        let old = vault.reward_rate;
        vault.reward_rate = new_rate;

        emit!(RewardRateUpdated { old, new_rate });
        Ok(())
    }
}

// ---- The updateReward helper ----
//
// SMELL: This mutates `vault.entries` via linear scan. Every stake/withdraw/
//        get_reward writes the same VaultState account, so the entire program
//        runs serial. See optimization/parallelism.md.
fn update_reward(vault: &mut VaultState, account: Option<Pubkey>, now: i64) -> Result<()> {
    // Recompute the global reward-per-token accumulator.
    // SMELL: bare arithmetic on user-controllable values. `dt * rate * PRECISION`
    //        overflows u128 for sufficiently large rate × duration. Solidity 0.8+
    //        would have reverted; Rust release builds wrap silently.
    let dt = (now - vault.last_update_time) as u128;
    if vault.total_supply > 0 {
        let numerator = dt * (vault.reward_rate as u128) * PRECISION;
        let delta = numerator / (vault.total_supply as u128);
        vault.reward_per_token_stored += delta; // SMELL: unchecked
    }
    vault.last_update_time = now;

    if let Some(user) = account {
        let rpt_stored = vault.reward_per_token_stored;
        let entry = upsert_entry(vault, user)?;
        // earned = balance * (rpt_stored - userPaid) / PRECISION + entry.rewards
        let earned_inc =
            (entry.balance as u128) * (rpt_stored - entry.reward_per_token_paid) / PRECISION;
        entry.rewards += earned_inc as u64; // SMELL: cast can truncate, addition unchecked
        entry.reward_per_token_paid = rpt_stored;
    }
    Ok(())
}

fn upsert_entry<'a>(vault: &'a mut VaultState, user: Pubkey) -> Result<&'a mut UserStakeEntry> {
    if let Some(idx) = vault.entries.iter().position(|e| e.user == user) {
        return Ok(&mut vault.entries[idx]);
    }
    require!(vault.entries.len() < MAX_STAKERS, StakeError::TooManyStakers);
    vault.entries.push(UserStakeEntry {
        user,
        balance: 0,
        reward_per_token_paid: 0,
        rewards: 0,
    });
    let last = vault.entries.len() - 1;
    Ok(&mut vault.entries[last])
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

    pub staking_mint: Account<'info, Mint>,
    pub rewards_mint: Account<'info, Mint>,

    /// CHECK: PDA used as authority for vault token accounts. Not deserialized.
    #[account(seeds = [b"vault_authority"], bump)]
    pub vault_authority: UncheckedAccount<'info>,

    #[account(
        init,
        payer = owner,
        token::mint = staking_mint,
        token::authority = vault_authority,
        seeds = [b"vault_staking_ata"],
        bump,
    )]
    pub vault_staking_ata: Account<'info, TokenAccount>,

    #[account(
        init,
        payer = owner,
        token::mint = rewards_mint,
        token::authority = vault_authority,
        seeds = [b"vault_rewards_ata"],
        bump,
    )]
    pub vault_rewards_ata: Account<'info, TokenAccount>,

    #[account(mut)]
    pub owner: Signer<'info>,
    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct Stake<'info> {
    // SMELL: writable on every stake — single write-lock for the entire program.
    #[account(mut, seeds = [b"vault"], bump)]
    pub vault: Account<'info, VaultState>,

    /// CHECK: PDA, not deserialized.
    #[account(seeds = [b"vault_authority"], bump)]
    pub vault_authority: UncheckedAccount<'info>,

    #[account(mut, seeds = [b"vault_staking_ata"], bump)]
    pub vault_staking_ata: Account<'info, TokenAccount>,

    #[account(mut, token::mint = vault.staking_mint, token::authority = user)]
    pub user_staking_ata: Account<'info, TokenAccount>,

    pub user: Signer<'info>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct Withdraw<'info> {
    #[account(mut, seeds = [b"vault"], bump)]
    pub vault: Account<'info, VaultState>,

    /// CHECK: PDA.
    #[account(seeds = [b"vault_authority"], bump)]
    pub vault_authority: UncheckedAccount<'info>,

    #[account(mut, seeds = [b"vault_staking_ata"], bump)]
    pub vault_staking_ata: Account<'info, TokenAccount>,

    #[account(mut, token::mint = vault.staking_mint, token::authority = user)]
    pub user_staking_ata: Account<'info, TokenAccount>,

    pub user: Signer<'info>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct GetReward<'info> {
    #[account(mut, seeds = [b"vault"], bump)]
    pub vault: Account<'info, VaultState>,

    /// CHECK: PDA.
    #[account(seeds = [b"vault_authority"], bump)]
    pub vault_authority: UncheckedAccount<'info>,

    #[account(mut, seeds = [b"vault_rewards_ata"], bump)]
    pub vault_rewards_ata: Account<'info, TokenAccount>,

    #[account(mut, token::mint = vault.rewards_mint, token::authority = user)]
    pub user_rewards_ata: Account<'info, TokenAccount>,

    pub user: Signer<'info>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct SetRewardRate<'info> {
    #[account(mut, seeds = [b"vault"], bump)]
    pub vault: Account<'info, VaultState>,
    pub owner: Signer<'info>,
}

// ---- State ----

#[account]
pub struct VaultState {
    pub staking_mint: Pubkey,
    pub rewards_mint: Pubkey,
    pub owner: Pubkey,
    pub reward_rate: u64,
    pub last_update_time: i64,
    pub reward_per_token_stored: u128,
    pub total_supply: u64,
    pub entries: Vec<UserStakeEntry>, // SMELL: see file header
}

impl VaultState {
    pub const SIZE: usize = 32 + 32 + 32 // mints + owner
        + 8                                // reward_rate
        + 8                                // last_update_time
        + 16                               // reward_per_token_stored
        + 8                                // total_supply
        + 4 + MAX_STAKERS * UserStakeEntry::SIZE;
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct UserStakeEntry {
    pub user: Pubkey,
    pub balance: u64,
    pub reward_per_token_paid: u128,
    pub rewards: u64,
}

impl UserStakeEntry {
    pub const SIZE: usize = 32 + 8 + 16 + 8; // 64
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
    #[msg("caller is not the owner")]
    NotOwner,
    #[msg("too many stakers for this account's capacity")]
    TooManyStakers,
}
