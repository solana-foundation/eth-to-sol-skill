# OpenZeppelin → Solana ecosystem mapping

Most OZ contracts have a Solana counterpart that is *not* a Rust port. Use the counterpart.

## ERC-20 → SPL Token

| OZ ERC-20 | SPL Token equivalent |
|---|---|
| Mint authority on contract | `spl_token::Mint.mint_authority` (a `Pubkey`, can be a PDA your program controls) |
| `balanceOf` mapping | One `TokenAccount` per holder (typically an Associated Token Account at a deterministic address) |
| `allowance` nested mapping | `TokenAccount.delegate` + `delegated_amount`. Single delegate per account, not arbitrary spender pairs. |
| `transfer(to, amount)` | `spl_token::transfer` — caller is `TokenAccount.owner`, no CPI to your program needed |
| `approve(spender, amount)` | `spl_token::approve` — sets the single delegate on the caller's `TokenAccount` |
| `transferFrom(from, to, amount)` | `spl_token::transfer` called by the delegate; the delegate must be the signer |
| `mint(to, amount)` | `spl_token::mint_to` CPI, signed by `mint_authority` (which is your program's PDA) |
| `burn(amount)` | `spl_token::burn` — caller is the `TokenAccount.owner` |
| `name` / `symbol` / metadata | Metaplex Token Metadata account, *not* SPL Token. SPL Token stores only `decimals` + `mint_authority` + `supply` + `freeze_authority`. |
| `totalSupply` | `spl_token::Mint.supply`. Updates atomically with `mint_to`/`burn`. |
| `pause` | `freeze_authority` on the Mint. Calling `spl_token::freeze_account` disables transfers for a specific `TokenAccount`. No global pause without Token-2022. |
| `decimals` | `spl_token::Mint.decimals` |

**Implication:** translating an ERC-20 to "an Anchor program that does its own token mechanics" is the wrong answer. The right answer is "an Anchor program that owns the Mint via a PDA and exposes the *governance* layer (who can mint, with what cap, etc.), while transfers happen on SPL Token directly."

For a worked example of "Anchor program holds the mint via PDA, governance instructions only", see `examples/erc4626-vault/03-optimized.rs` (vault shares as an SPL Mint owned by a PDA).

### Allowance gap

SPL Token has a single delegate per account, not a map of `(spender → amount)`. If the source contract genuinely needs multi-spender allowances:

- **Common case:** real users rarely need >1 active delegate. Document the limitation and proceed.
- **Required case:** build a custom allowance PDA per `(owner, spender)` pair and have a custom `transfer_from` instruction that decrements it before calling `spl_token::transfer` with your program as delegate. This is a real escape hatch but adds an account and an instruction round-trip. Justify before doing it.

### Token-2022

`spl-token-2022` extends classic SPL Token with: transfer fee, interest-bearing accounts, non-transferable, confidential transfers, hooks (program callbacks on transfer), permanent delegate, mint close authority. Use Token-2022 when the source contract has equivalent semantics that classic SPL cannot express. Otherwise classic is the answer (wider wallet/DEX support).

## ERC-721 → Metaplex Token Metadata + SPL Token

Each NFT is its own `Mint` with supply 1 and decimals 0, plus a Metadata account from Metaplex Token Metadata (`mpl-token-metadata`). Collections use Master Edition / Collection NFTs.

For new builds, **Metaplex Core** (`mpl-core`) is the current recommended path — it compresses NFT state into a single account and is significantly cheaper than the legacy Token Metadata model.

Mappings of ERC-721 (`tokenURI`, `ownerOf`) are not translated — they are inherent to the `TokenAccount` + Metadata model.

## ERC-1155

No first-class equivalent. Translation: a separate SPL Token Mint per `id`, or Token-2022 with grouping. Most "multi-token" use cases on Solana are expressed as N independent SPL mints rather than a single multi-token program.

## Ownable

```solidity
contract Foo is Ownable { ... }
```

→ Config PDA pattern:

```rust
#[account]
pub struct Config {
    pub authority: Pubkey,
    pub bump: u8,
}

#[derive(Accounts)]
pub struct OnlyOwnerAction<'info> {
    #[account(has_one = authority, seeds = [b"config"], bump = config.bump)]
    pub config: Account<'info, Config>,
    pub authority: Signer<'info>,
}

pub fn transfer_ownership(ctx: Context<TransferOwnership>, new_authority: Pubkey) -> Result<()> {
    ctx.accounts.config.authority = new_authority;
    Ok(())
}
```

`Ownable2Step` (OZ pattern with a pending-then-accept dance) → add `pending_authority: Pubkey` to the config PDA, set it on `transfer_ownership`, and require the pending authority to sign `accept_ownership` to commit. Worth doing for high-stakes contracts.

## AccessControl (role-based)

Roles → one PDA per role, seeded by role name + holder pubkey:

```rust
// seeds = [b"role", role_id.as_ref(), holder.as_ref()]
#[account]
pub struct RoleMembership {
    pub role: [u8; 32],   // role identifier (e.g. hash of "MINTER_ROLE")
    pub holder: Pubkey,
    pub bump: u8,
}
```

A guarded instruction takes the `RoleMembership` account as a constraint. The role admin PDA controls grant/revoke. Costs one extra account per call but parallelizes (different role holders write to different PDAs).

For small fixed role sets, a single config PDA with explicit fields (`pub minter: Pubkey;` `pub pauser: Pubkey;`) is simpler. Use the membership-PDA approach only when roles are dynamic or membership counts are non-trivial.

## ReentrancyGuard

Solana's account locking blocks classic same-program reentrancy automatically — see `security/reentrancy.md`. Do not port `ReentrancyGuard`. Do worry about cross-program reentrancy (program A calls B which calls A) if you expose callable instructions that the called-out program could be tricked into re-entering. Document the call boundary clearly.

## SafeMath

Solidity 0.8+ has built-in checked arithmetic. Translation: every `+`, `-`, `*`, `/` becomes `checked_add`, `checked_sub`, `checked_mul`, `checked_div`, each returning `Option<T>`, unwrapped with `.ok_or(MyError::Overflow)?`. See `security/arithmetic.md`.

For pre-0.8 contracts that used `SafeMath` explicitly — same translation; the input is already arithmetic-aware.

## Pausable

```solidity
contract Foo is Pausable { function pause() ... }
```

For SPL Token: use `freeze_authority`. Per-account freezing only; no global pause for classic SPL Token. Token-2022 has no global pause either, but the permanent delegate + transfer hook extensions can approximate one.

For non-token state: add `paused: bool` to your config PDA, gate writes with `require!(!config.paused, Err::Paused)`. Trivial.

## ERC-2612 (permit)

Solana doesn't need permit — every instruction already carries explicit signatures. The "approve-and-call in one tx" pattern is the *default* — pack `approve` + `transfer_from` into a single transaction. Translation: drop the permit entirely.

## EIP-712 typed signatures

Solana transactions are themselves signed; no off-chain message signing needed for in-protocol auth. Off-chain signed messages used for *gasless* flows (relayer-submitted intents) translate to **Squads multisig**, **Lighthouse**, or **Jito bundles** depending on the use case. Out of scope for direct translation.
