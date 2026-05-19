// Pass 1: Faithful Anchor port of TokenSwapEscrow.
//
// BASELINE — semantically identical to the Solidity, translated construct-by-construct.
// `// SMELL:` markers flag what the optimized version (03-optimized.rs) replaces.
//
// What's faithful:
// - One global `EscrowState` account holds `next_offer_id` and a `Vec<Offer>`
//   playing the role of Solidity's `mapping(uint256 => Offer)`. Every make /
//   take / cancel writes this single account.
// - Per-offer escrowed tokens live in token accounts owned by a singleton
//   `vault_authority` PDA. The mapping `offer.id → vault TokenAccount` is
//   maintained inline in each `Offer` entry.
// - Arithmetic uses bare `+=` / `-=` matching the Solidity source. Solidity
//   0.8 checks at runtime; Rust release builds wrap silently — a regression-
//   in-safety the optimized version fixes.
//
// What is NOT faithful, by necessity:
// - Token movements use SPL Token CPI. There is no non-CPI fungible-token
//   primitive on Solana, so both the naive and the optimized version are
//   forced to do this right. The smell is in how OFFERS are tracked, not in
//   how tokens move.

use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, Token, TokenAccount, Transfer};

declare_id!("EscrowNaive111111111111111111111111111111111");

// Hard cap because the Vec lives in a fixed-size account.
// SMELL: a real escrow program should never have an offer-count cap; pure
//        translation artifact.
const MAX_OFFERS: usize = 50;

#[program]
pub mod escrow_naive {
    use super::*;

    /// Solidity `constructor()` — implicit, no params. We still need an
    /// explicit initialize on Solana to create the singleton state account.
    pub fn initialize(ctx: Context<Initialize>) -> Result<()> {
        let state = &mut ctx.accounts.state;
        state.next_offer_id = 0;
        state.offers = Vec::new();
        // SMELL: bump for state and vault_authority not cached.
        Ok(())
    }

    /// Solidity `makeOffer(tokenOffered, amountOffered, tokenWanted, amountWanted)`.
    pub fn make_offer(
        ctx: Context<MakeOffer>,
        amount_offered: u64,
        amount_wanted: u64,
    ) -> Result<()> {
        require!(amount_offered > 0 && amount_wanted > 0, EscrowError::ZeroAmount);
        require_keys_neq!(
            ctx.accounts.token_offered_mint.key(),
            ctx.accounts.token_wanted_mint.key(),
            EscrowError::SameToken
        );

        let state = &mut ctx.accounts.state;
        require!(state.offers.len() < MAX_OFFERS, EscrowError::TooManyOffers);

        let id = state.next_offer_id;
        state.next_offer_id += 1; // SMELL: unchecked

        let offer = Offer {
            id,
            maker: ctx.accounts.maker.key(),
            token_offered: ctx.accounts.token_offered_mint.key(),
            amount_offered,
            token_wanted: ctx.accounts.token_wanted_mint.key(),
            amount_wanted,
        };
        state.offers.push(offer);

        // Pull the maker's tokens into the vault. The vault is a program-
        // owned TokenAccount the test harness has pre-created; in production
        // a more careful design would create one per offer (see optimized).
        token::transfer(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.maker_token_account_offered.to_account_info(),
                    to: ctx.accounts.vault_token_account_offered.to_account_info(),
                    authority: ctx.accounts.maker.to_account_info(),
                },
            ),
            amount_offered,
        )?;

        emit!(OfferMade { id, maker: ctx.accounts.maker.key(), amount_offered, amount_wanted });
        Ok(())
    }

    /// Solidity `takeOffer(id)`.
    pub fn take_offer(ctx: Context<TakeOffer>, id: u64) -> Result<()> {
        let state = &mut ctx.accounts.state;

        // SMELL: O(n) Vec scan to find the offer.
        let idx = state
            .offers
            .iter()
            .position(|o| o.id == id)
            .ok_or(EscrowError::OfferDoesNotExist)?;
        let offer = state.offers[idx].clone();

        // Validate the supplied token accounts match the offer.
        require_keys_eq!(
            ctx.accounts.token_offered_mint.key(),
            offer.token_offered,
            EscrowError::MintMismatch
        );
        require_keys_eq!(
            ctx.accounts.token_wanted_mint.key(),
            offer.token_wanted,
            EscrowError::MintMismatch
        );
        require_keys_eq!(
            ctx.accounts.maker_token_account_wanted.owner,
            offer.maker,
            EscrowError::MakerMismatch
        );

        // Remove the offer entry first.
        // SMELL: swap_remove reorders the Vec — fine semantically but obscures
        //        chronology; another reason the optimized version doesn't use a Vec.
        state.offers.swap_remove(idx);

        // Pull the wanted token from taker → maker.
        token::transfer(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.taker_token_account_wanted.to_account_info(),
                    to: ctx.accounts.maker_token_account_wanted.to_account_info(),
                    authority: ctx.accounts.taker.to_account_info(),
                },
            ),
            offer.amount_wanted,
        )?;

        // Release the offered token from vault → taker. PDA signs.
        let bump = ctx.bumps.vault_authority;
        let signer_seeds: &[&[u8]] = &[b"vault_authority", &[bump]];
        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.vault_token_account_offered.to_account_info(),
                    to: ctx.accounts.taker_token_account_offered.to_account_info(),
                    authority: ctx.accounts.vault_authority.to_account_info(),
                },
                &[signer_seeds],
            ),
            offer.amount_offered,
        )?;

        emit!(OfferTaken { id, taker: ctx.accounts.taker.key() });
        Ok(())
    }

    /// Solidity `cancelOffer(id)` — maker only.
    pub fn cancel_offer(ctx: Context<CancelOffer>, id: u64) -> Result<()> {
        let state = &mut ctx.accounts.state;

        // SMELL: O(n) Vec scan.
        let idx = state
            .offers
            .iter()
            .position(|o| o.id == id)
            .ok_or(EscrowError::OfferDoesNotExist)?;
        let offer = state.offers[idx].clone();

        require_keys_eq!(offer.maker, ctx.accounts.maker.key(), EscrowError::NotMaker);
        require_keys_eq!(
            ctx.accounts.token_offered_mint.key(),
            offer.token_offered,
            EscrowError::MintMismatch
        );

        state.offers.swap_remove(idx);

        // Refund the maker.
        let bump = ctx.bumps.vault_authority;
        let signer_seeds: &[&[u8]] = &[b"vault_authority", &[bump]];
        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.vault_token_account_offered.to_account_info(),
                    to: ctx.accounts.maker_token_account_offered.to_account_info(),
                    authority: ctx.accounts.vault_authority.to_account_info(),
                },
                &[signer_seeds],
            ),
            offer.amount_offered,
        )?;

        emit!(OfferCancelled { id });
        Ok(())
    }
}

// ---- Accounts ----

#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(
        init,
        payer = payer,
        space = 8 + EscrowState::SIZE,
        seeds = [b"state"],
        bump,
    )]
    pub state: Account<'info, EscrowState>,

    /// CHECK: PDA, signs vault transfers. Not deserialized.
    #[account(seeds = [b"vault_authority"], bump)]
    pub vault_authority: UncheckedAccount<'info>,

    #[account(mut)]
    pub payer: Signer<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct MakeOffer<'info> {
    // SMELL: writable on every offer — single write-lock for the program.
    #[account(mut, seeds = [b"state"], bump)]
    pub state: Account<'info, EscrowState>,

    /// CHECK: PDA.
    #[account(seeds = [b"vault_authority"], bump)]
    pub vault_authority: UncheckedAccount<'info>,

    pub token_offered_mint: Account<'info, Mint>,
    pub token_wanted_mint: Account<'info, Mint>,

    #[account(
        mut,
        token::mint = token_offered_mint,
        token::authority = maker,
    )]
    pub maker_token_account_offered: Account<'info, TokenAccount>,

    /// Vault for the offered token, owned by vault_authority PDA. In the naive
    /// design the same vault account is reused across all offers of the same
    /// mint — meaning we couldn't tell apart escrowed amounts from different
    /// offers if they used the same mint. SMELL: this is wrong, the optimized
    /// version creates one vault per offer.
    #[account(
        mut,
        token::mint = token_offered_mint,
        token::authority = vault_authority,
    )]
    pub vault_token_account_offered: Account<'info, TokenAccount>,

    pub maker: Signer<'info>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct TakeOffer<'info> {
    #[account(mut, seeds = [b"state"], bump)]
    pub state: Account<'info, EscrowState>,

    /// CHECK: PDA.
    #[account(seeds = [b"vault_authority"], bump)]
    pub vault_authority: UncheckedAccount<'info>,

    pub token_offered_mint: Account<'info, Mint>,
    pub token_wanted_mint: Account<'info, Mint>,

    #[account(mut, token::mint = token_offered_mint, token::authority = vault_authority)]
    pub vault_token_account_offered: Account<'info, TokenAccount>,

    #[account(mut, token::mint = token_offered_mint, token::authority = taker)]
    pub taker_token_account_offered: Account<'info, TokenAccount>,

    #[account(mut, token::mint = token_wanted_mint, token::authority = taker)]
    pub taker_token_account_wanted: Account<'info, TokenAccount>,

    #[account(mut, token::mint = token_wanted_mint)]
    pub maker_token_account_wanted: Account<'info, TokenAccount>,

    pub taker: Signer<'info>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct CancelOffer<'info> {
    #[account(mut, seeds = [b"state"], bump)]
    pub state: Account<'info, EscrowState>,

    /// CHECK: PDA.
    #[account(seeds = [b"vault_authority"], bump)]
    pub vault_authority: UncheckedAccount<'info>,

    pub token_offered_mint: Account<'info, Mint>,

    #[account(mut, token::mint = token_offered_mint, token::authority = vault_authority)]
    pub vault_token_account_offered: Account<'info, TokenAccount>,

    #[account(mut, token::mint = token_offered_mint, token::authority = maker)]
    pub maker_token_account_offered: Account<'info, TokenAccount>,

    pub maker: Signer<'info>,
    pub token_program: Program<'info, Token>,
}

// ---- State ----

#[account]
pub struct EscrowState {
    pub next_offer_id: u64,
    pub offers: Vec<Offer>, // SMELL: write-hot, capped, O(n) scan
}

impl EscrowState {
    pub const SIZE: usize = 8                     // next_offer_id
        + 4 + MAX_OFFERS * Offer::SIZE;          // Vec<Offer>
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct Offer {
    pub id: u64,
    pub maker: Pubkey,
    pub token_offered: Pubkey,
    pub amount_offered: u64,
    pub token_wanted: Pubkey,
    pub amount_wanted: u64,
}

impl Offer {
    pub const SIZE: usize = 8 + 32 + 32 + 8 + 32 + 8; // 120
}

// ---- Events ----

#[event]
pub struct OfferMade {
    pub id: u64,
    pub maker: Pubkey,
    pub amount_offered: u64,
    pub amount_wanted: u64,
}

#[event]
pub struct OfferTaken {
    pub id: u64,
    pub taker: Pubkey,
}

#[event]
pub struct OfferCancelled {
    pub id: u64,
}

// ---- Errors ----

#[error_code]
pub enum EscrowError {
    #[msg("amount must be > 0")]
    ZeroAmount,
    #[msg("offered and wanted tokens must differ")]
    SameToken,
    #[msg("offer does not exist")]
    OfferDoesNotExist,
    #[msg("caller is not the maker of this offer")]
    NotMaker,
    #[msg("supplied token mint does not match the offer")]
    MintMismatch,
    #[msg("supplied maker token account does not belong to the offer's maker")]
    MakerMismatch,
    #[msg("too many open offers")]
    TooManyOffers,
}
