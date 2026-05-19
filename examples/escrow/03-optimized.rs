// Pass 2: Solana-native refactor of TokenSwapEscrow.
//
// Structural moves:
// - There is no global state account. Each offer is a self-contained `Offer`
//   PDA seeded by `[b"offer", maker.as_ref(), &id.to_le_bytes()]`. Different
//   offers write disjoint accounts; cross-maker activity is fully parallel.
// - Each offer also owns its own vault TokenAccount, derived as
//   `[b"vault", offer.key().as_ref()]`. No sharing of vaults between offers
//   even when they involve the same mint — eliminates the "whose tokens are
//   these?" ambiguity the naive port lives with.
// - Vault token-account authority is a per-offer PDA. A bug or compromise
//   in one offer's logic cannot move funds from another offer.
// - The offer ID is supplied by the maker as an instruction argument
//   (`u64`) — typically `Date.now() / 1000` or a random nonce client-side —
//   so the program never needs a global counter. Drops the last shared
//   write-hot field from the naive design.
// - `init` + `close` lifecycle: the Offer and its vault are created at
//   `make_offer` and closed at `take_offer` / `cancel_offer`. Rent flows
//   back to the maker on close.
// - All arithmetic uses checked operations. Canonical PDA bumps are cached
//   on the Offer account and supplied on every subsequent access.

use anchor_lang::prelude::*;
use anchor_spl::token::{self, CloseAccount, Mint, Token, TokenAccount, Transfer};

declare_id!("EscrowNative11111111111111111111111111111111");

#[program]
pub mod escrow_native {
    use super::*;

    /// Maker locks `amount_offered` of `token_offered_mint`, asking for
    /// `amount_wanted` of `token_wanted_mint` in return. The `id` is chosen
    /// by the maker (any unique u64) so the program has no global counter.
    pub fn make_offer(
        ctx: Context<MakeOffer>,
        id: u64,
        amount_offered: u64,
        amount_wanted: u64,
    ) -> Result<()> {
        require!(amount_offered > 0 && amount_wanted > 0, EscrowError::ZeroAmount);
        require_keys_neq!(
            ctx.accounts.token_offered_mint.key(),
            ctx.accounts.token_wanted_mint.key(),
            EscrowError::SameToken
        );

        let offer = &mut ctx.accounts.offer;
        offer.id = id;
        offer.maker = ctx.accounts.maker.key();
        offer.token_offered = ctx.accounts.token_offered_mint.key();
        offer.amount_offered = amount_offered;
        offer.token_wanted = ctx.accounts.token_wanted_mint.key();
        offer.amount_wanted = amount_wanted;
        offer.bump = ctx.bumps.offer;
        offer.vault_authority_bump = ctx.bumps.vault_authority;

        token::transfer(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.maker_token_account_offered.to_account_info(),
                    to: ctx.accounts.vault.to_account_info(),
                    authority: ctx.accounts.maker.to_account_info(),
                },
            ),
            amount_offered,
        )?;

        emit!(OfferMade {
            id,
            maker: ctx.accounts.maker.key(),
            amount_offered,
            amount_wanted,
        });
        Ok(())
    }

    /// Taker fulfils an offer. The wanted token is pulled from the taker
    /// (sent directly to maker); the offered token is released from the vault
    /// to the taker. Vault + Offer accounts are closed atomically; rent refunds
    /// to the maker.
    pub fn take_offer(ctx: Context<TakeOffer>) -> Result<()> {
        let offer = &ctx.accounts.offer;
        let offer_key = offer.key();
        let vault_authority_bump = offer.vault_authority_bump;
        let amount_offered = offer.amount_offered;
        let amount_wanted = offer.amount_wanted;
        let id = offer.id;

        // Wanted token: taker → maker, atomic with the offered-release below.
        token::transfer(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.taker_token_account_wanted.to_account_info(),
                    to: ctx.accounts.maker_token_account_wanted.to_account_info(),
                    authority: ctx.accounts.taker.to_account_info(),
                },
            ),
            amount_wanted,
        )?;

        // Offered token: vault → taker, signed by per-offer vault_authority PDA.
        let signer_seeds: &[&[u8]] = &[
            b"vault_authority",
            offer_key.as_ref(),
            &[vault_authority_bump],
        ];
        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.vault.to_account_info(),
                    to: ctx.accounts.taker_token_account_offered.to_account_info(),
                    authority: ctx.accounts.vault_authority.to_account_info(),
                },
                &[signer_seeds],
            ),
            amount_offered,
        )?;

        // Close the vault token account; lamports go to the maker.
        token::close_account(CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            CloseAccount {
                account: ctx.accounts.vault.to_account_info(),
                destination: ctx.accounts.maker.to_account_info(),
                authority: ctx.accounts.vault_authority.to_account_info(),
            },
            &[signer_seeds],
        ))?;

        // The Offer account itself is closed via the `close = maker` constraint
        // on the Accounts struct — Anchor handles it after the handler returns.

        emit!(OfferTaken { id, taker: ctx.accounts.taker.key() });
        Ok(())
    }

    /// Maker withdraws their offered tokens. Closes the offer and vault.
    pub fn cancel_offer(ctx: Context<CancelOffer>) -> Result<()> {
        let offer = &ctx.accounts.offer;
        let offer_key = offer.key();
        let vault_authority_bump = offer.vault_authority_bump;
        let amount_offered = offer.amount_offered;
        let id = offer.id;

        let signer_seeds: &[&[u8]] = &[
            b"vault_authority",
            offer_key.as_ref(),
            &[vault_authority_bump],
        ];

        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.vault.to_account_info(),
                    to: ctx.accounts.maker_token_account_offered.to_account_info(),
                    authority: ctx.accounts.vault_authority.to_account_info(),
                },
                &[signer_seeds],
            ),
            amount_offered,
        )?;

        token::close_account(CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            CloseAccount {
                account: ctx.accounts.vault.to_account_info(),
                destination: ctx.accounts.maker.to_account_info(),
                authority: ctx.accounts.vault_authority.to_account_info(),
            },
            &[signer_seeds],
        ))?;

        emit!(OfferCancelled { id });
        Ok(())
    }
}

// ---- Accounts ----

#[derive(Accounts)]
#[instruction(id: u64)]
pub struct MakeOffer<'info> {
    #[account(
        init,
        payer = maker,
        space = 8 + Offer::SIZE,
        seeds = [b"offer", maker.key().as_ref(), &id.to_le_bytes()],
        bump,
    )]
    pub offer: Account<'info, Offer>,

    /// CHECK: PDA — signs vault transfers for THIS offer only. Validated by
    /// seeds + bump; never deserialized.
    #[account(
        seeds = [b"vault_authority", offer.key().as_ref()],
        bump,
    )]
    pub vault_authority: UncheckedAccount<'info>,

    pub token_offered_mint: Account<'info, Mint>,
    pub token_wanted_mint: Account<'info, Mint>,

    #[account(
        init,
        payer = maker,
        seeds = [b"vault", offer.key().as_ref()],
        bump,
        token::mint = token_offered_mint,
        token::authority = vault_authority,
    )]
    pub vault: Account<'info, TokenAccount>,

    #[account(
        mut,
        token::mint = token_offered_mint,
        token::authority = maker,
    )]
    pub maker_token_account_offered: Account<'info, TokenAccount>,

    #[account(mut)]
    pub maker: Signer<'info>,
    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct TakeOffer<'info> {
    /// Offer is closed when this instruction returns; rent refunds to maker.
    #[account(
        mut,
        seeds = [b"offer", offer.maker.as_ref(), &offer.id.to_le_bytes()],
        bump = offer.bump,
        has_one = maker @ EscrowError::MakerMismatch,
        constraint = offer.token_offered == token_offered_mint.key() @ EscrowError::MintMismatch,
        constraint = offer.token_wanted == token_wanted_mint.key() @ EscrowError::MintMismatch,
        close = maker,
    )]
    pub offer: Account<'info, Offer>,

    /// CHECK: PDA, cached bump.
    #[account(
        seeds = [b"vault_authority", offer.key().as_ref()],
        bump = offer.vault_authority_bump,
    )]
    pub vault_authority: UncheckedAccount<'info>,

    #[account(
        mut,
        seeds = [b"vault", offer.key().as_ref()],
        bump,
        token::mint = token_offered_mint,
        token::authority = vault_authority,
    )]
    pub vault: Account<'info, TokenAccount>,

    pub token_offered_mint: Account<'info, Mint>,
    pub token_wanted_mint: Account<'info, Mint>,

    #[account(mut, token::mint = token_offered_mint, token::authority = taker)]
    pub taker_token_account_offered: Account<'info, TokenAccount>,

    #[account(mut, token::mint = token_wanted_mint, token::authority = taker)]
    pub taker_token_account_wanted: Account<'info, TokenAccount>,

    #[account(mut, token::mint = token_wanted_mint, token::authority = maker)]
    pub maker_token_account_wanted: Account<'info, TokenAccount>,

    /// CHECK: validated via offer.has_one. Receives the Offer's rent refund.
    #[account(mut)]
    pub maker: UncheckedAccount<'info>,

    pub taker: Signer<'info>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct CancelOffer<'info> {
    #[account(
        mut,
        seeds = [b"offer", maker.key().as_ref(), &offer.id.to_le_bytes()],
        bump = offer.bump,
        has_one = maker @ EscrowError::NotMaker,
        constraint = offer.token_offered == token_offered_mint.key() @ EscrowError::MintMismatch,
        close = maker,
    )]
    pub offer: Account<'info, Offer>,

    /// CHECK: PDA, cached bump.
    #[account(
        seeds = [b"vault_authority", offer.key().as_ref()],
        bump = offer.vault_authority_bump,
    )]
    pub vault_authority: UncheckedAccount<'info>,

    #[account(
        mut,
        seeds = [b"vault", offer.key().as_ref()],
        bump,
        token::mint = token_offered_mint,
        token::authority = vault_authority,
    )]
    pub vault: Account<'info, TokenAccount>,

    pub token_offered_mint: Account<'info, Mint>,

    #[account(mut, token::mint = token_offered_mint, token::authority = maker)]
    pub maker_token_account_offered: Account<'info, TokenAccount>,

    #[account(mut)]
    pub maker: Signer<'info>,
    pub token_program: Program<'info, Token>,
}

// ---- State ----

#[account]
pub struct Offer {
    pub id: u64,
    pub maker: Pubkey,
    pub token_offered: Pubkey,
    pub amount_offered: u64,
    pub token_wanted: Pubkey,
    pub amount_wanted: u64,
    pub bump: u8,
    pub vault_authority_bump: u8,
}

impl Offer {
    pub const SIZE: usize = 8 + 32 + 32 + 8 + 32 + 8 + 1 + 1; // 122 bytes
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
    #[msg("caller is not the maker of this offer")]
    NotMaker,
    #[msg("offer.maker does not match the supplied maker account")]
    MakerMismatch,
    #[msg("supplied token mint does not match the offer")]
    MintMismatch,
}
