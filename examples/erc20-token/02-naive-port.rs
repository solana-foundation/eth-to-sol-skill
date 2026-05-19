// Pass 1: Faithful Anchor port of ExampleToken.
//
// This is the BASELINE. It is semantically identical to the Solidity contract,
// translated construct-for-construct. It is deliberately un-Solana — `// SMELL:`
// markers flag patterns the optimized version (03-optimized.rs) replaces.
//
// Do not deploy this. It compiles and is correct, but it is structurally wrong:
// - All balances live in one account (the `TokenState`), serializing every transfer.
// - Allowances are a Vec inside the same account.
// - We reimplement what SPL Token already provides.
// - Arithmetic is unchecked (matching Solidity 0.8's implicit checks loosely, but Rust
//   release builds wrap silently — this is actually a regression in safety).

use anchor_lang::prelude::*;

declare_id!("Eth2So1NaiveTokenExamp1eProgramAddressXXXX");

// Hard caps required because the Vec lives in a fixed-size account.
// SMELL: a real ERC-20 should not have these caps; this is a translation artifact.
const NAME_MAX: usize = 32;
const SYMBOL_MAX: usize = 16;
const MAX_HOLDERS: usize = 100;
const MAX_ALLOWANCES: usize = 200;

#[program]
pub mod erc20_naive {
    use super::*;

    /// Solidity `constructor(name, symbol, decimals, maxSupply)`.
    pub fn initialize(
        ctx: Context<Initialize>,
        name: String,
        symbol: String,
        decimals: u8,
        max_supply: u64,
    ) -> Result<()> {
        require!(name.len() <= NAME_MAX, TokenError::FieldTooLong);
        require!(symbol.len() <= SYMBOL_MAX, TokenError::FieldTooLong);

        let state = &mut ctx.accounts.state;
        state.name = name;
        state.symbol = symbol;
        state.decimals = decimals;
        state.total_supply = 0;
        state.max_supply = max_supply;
        state.owner = ctx.accounts.owner.key();
        state.balances = Vec::new();
        state.allowances = Vec::new();
        // SMELL: bump not stored — every subsequent call re-derives. See security/pda-canonicalization.md.

        emit!(OwnershipTransferred {
            previous_owner: Pubkey::default(),
            new_owner: ctx.accounts.owner.key(),
        });
        Ok(())
    }

    /// Solidity `transfer(to, amount)`.
    pub fn transfer(ctx: Context<Mutate>, to: Pubkey, amount: u64) -> Result<()> {
        require_keys_neq!(to, Pubkey::default(), TokenError::ZeroAddress);
        let from = ctx.accounts.caller.key();
        do_transfer(&mut ctx.accounts.state, from, to, amount)
    }

    /// Solidity `approve(spender, amount)`.
    pub fn approve(ctx: Context<Mutate>, spender: Pubkey, amount: u64) -> Result<()> {
        let owner = ctx.accounts.caller.key();
        let state = &mut ctx.accounts.state;

        if let Some(entry) = state
            .allowances
            .iter_mut()
            .find(|e| e.owner == owner && e.spender == spender)
        {
            entry.amount = amount;
        } else {
            require!(
                state.allowances.len() < MAX_ALLOWANCES,
                TokenError::TooManyAllowances
            );
            state.allowances.push(AllowanceEntry {
                owner,
                spender,
                amount,
            });
        }

        emit!(Approval {
            owner,
            spender,
            amount
        });
        Ok(())
    }

    /// Solidity `transferFrom(from, to, amount)`.
    pub fn transfer_from(
        ctx: Context<Mutate>,
        from: Pubkey,
        to: Pubkey,
        amount: u64,
    ) -> Result<()> {
        require_keys_neq!(to, Pubkey::default(), TokenError::ZeroAddress);
        let spender = ctx.accounts.caller.key();
        let state = &mut ctx.accounts.state;

        let allowance = state
            .allowances
            .iter_mut()
            .find(|e| e.owner == from && e.spender == spender)
            .ok_or(error!(TokenError::InsufficientAllowance))?;
        require!(
            allowance.amount >= amount,
            TokenError::InsufficientAllowance
        );

        // Infinite-allowance idiom from ERC-20.
        if allowance.amount != u64::MAX {
            allowance.amount -= amount; // SMELL: unchecked subtraction
        }

        do_transfer(state, from, to, amount)
    }

    /// Solidity `mint(to, amount)` — onlyOwner.
    pub fn mint(ctx: Context<OwnerAction>, to: Pubkey, amount: u64) -> Result<()> {
        require_keys_neq!(to, Pubkey::default(), TokenError::ZeroAddress);
        let state = &mut ctx.accounts.state;
        require_keys_eq!(
            state.owner,
            ctx.accounts.owner.key(),
            TokenError::NotOwner
        );

        // SMELL: unchecked addition; max-supply check itself can overflow on adversarial input.
        require!(
            state.total_supply + amount <= state.max_supply,
            TokenError::MaxSupplyExceeded
        );

        state.total_supply += amount; // SMELL: unchecked
        upsert_balance(state, to, amount)?;

        emit!(Transfer {
            from: Pubkey::default(),
            to,
            amount,
        });
        Ok(())
    }

    /// Solidity `burn(amount)`.
    pub fn burn(ctx: Context<Mutate>, amount: u64) -> Result<()> {
        let caller = ctx.accounts.caller.key();
        let state = &mut ctx.accounts.state;

        let entry = state
            .balances
            .iter_mut()
            .find(|e| e.holder == caller)
            .ok_or(error!(TokenError::InsufficientBalance))?;
        require!(entry.amount >= amount, TokenError::InsufficientBalance);

        entry.amount -= amount; // SMELL: unchecked
        state.total_supply -= amount; // SMELL: unchecked

        emit!(Transfer {
            from: caller,
            to: Pubkey::default(),
            amount,
        });
        Ok(())
    }

    /// Solidity `transferOwnership(newOwner)` — onlyOwner.
    pub fn transfer_ownership(
        ctx: Context<OwnerAction>,
        new_owner: Pubkey,
    ) -> Result<()> {
        require_keys_neq!(new_owner, Pubkey::default(), TokenError::ZeroAddress);
        let state = &mut ctx.accounts.state;
        require_keys_eq!(
            state.owner,
            ctx.accounts.owner.key(),
            TokenError::NotOwner
        );

        let prev = state.owner;
        state.owner = new_owner;

        emit!(OwnershipTransferred {
            previous_owner: prev,
            new_owner,
        });
        Ok(())
    }
}

// ---- Helpers ----

fn do_transfer(state: &mut TokenState, from: Pubkey, to: Pubkey, amount: u64) -> Result<()> {
    // SMELL: O(n) scan of balances. Every transfer in the system writes this single account,
    //        serializing all transfers. See optimization/parallelism.md.
    let from_entry = state
        .balances
        .iter_mut()
        .find(|e| e.holder == from)
        .ok_or(error!(TokenError::InsufficientBalance))?;
    require!(from_entry.amount >= amount, TokenError::InsufficientBalance);
    from_entry.amount -= amount; // SMELL: unchecked

    upsert_balance(state, to, amount)?;

    emit!(Transfer { from, to, amount });
    Ok(())
}

fn upsert_balance(state: &mut TokenState, holder: Pubkey, delta: u64) -> Result<()> {
    if let Some(entry) = state.balances.iter_mut().find(|e| e.holder == holder) {
        entry.amount += delta; // SMELL: unchecked
    } else {
        require!(
            state.balances.len() < MAX_HOLDERS,
            TokenError::TooManyHolders
        );
        state.balances.push(BalanceEntry {
            holder,
            amount: delta,
        });
    }
    Ok(())
}

// ---- Accounts ----

#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(
        init,
        payer = owner,
        space = 8 + TokenState::SIZE,
        seeds = [b"state"],
        bump,
    )]
    pub state: Account<'info, TokenState>,
    #[account(mut)]
    pub owner: Signer<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Mutate<'info> {
    // SMELL: writable in every transfer / approve / burn / transfer_from — write-lock bottleneck.
    #[account(mut, seeds = [b"state"], bump)]
    pub state: Account<'info, TokenState>,
    pub caller: Signer<'info>,
}

#[derive(Accounts)]
pub struct OwnerAction<'info> {
    #[account(mut, seeds = [b"state"], bump)]
    pub state: Account<'info, TokenState>,
    pub owner: Signer<'info>,
}

// ---- State ----

#[account]
pub struct TokenState {
    pub name: String,        // 4 + NAME_MAX
    pub symbol: String,      // 4 + SYMBOL_MAX
    pub decimals: u8,        // 1
    pub total_supply: u64,   // 8
    pub max_supply: u64,     // 8
    pub owner: Pubkey,       // 32
    pub balances: Vec<BalanceEntry>,     // 4 + MAX_HOLDERS * (32+8)
    pub allowances: Vec<AllowanceEntry>, // 4 + MAX_ALLOWANCES * (32+32+8)
}

impl TokenState {
    pub const SIZE: usize = (4 + NAME_MAX)
        + (4 + SYMBOL_MAX)
        + 1
        + 8
        + 8
        + 32
        + (4 + MAX_HOLDERS * (32 + 8))
        + (4 + MAX_ALLOWANCES * (32 + 32 + 8));
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct BalanceEntry {
    pub holder: Pubkey,
    pub amount: u64,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct AllowanceEntry {
    pub owner: Pubkey,
    pub spender: Pubkey,
    pub amount: u64,
}

// ---- Events ----

#[event]
pub struct Transfer {
    pub from: Pubkey,
    pub to: Pubkey,
    pub amount: u64,
}

#[event]
pub struct Approval {
    pub owner: Pubkey,
    pub spender: Pubkey,
    pub amount: u64,
}

#[event]
pub struct OwnershipTransferred {
    pub previous_owner: Pubkey,
    pub new_owner: Pubkey,
}

// ---- Errors ----

#[error_code]
pub enum TokenError {
    #[msg("caller is not the owner")]
    NotOwner,
    #[msg("zero address")]
    ZeroAddress,
    #[msg("max supply exceeded")]
    MaxSupplyExceeded,
    #[msg("insufficient balance")]
    InsufficientBalance,
    #[msg("insufficient allowance")]
    InsufficientAllowance,
    #[msg("too many holders for this account's capacity")]
    TooManyHolders,
    #[msg("too many allowances for this account's capacity")]
    TooManyAllowances,
    #[msg("field exceeds maximum length")]
    FieldTooLong,
}
