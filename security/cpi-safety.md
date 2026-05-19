# CPI safety

Cross-Program Invocations let your program call other programs. Unlike EVM `call`, which can pass arbitrary calldata to any address, Solana CPIs require explicit account passing and explicit program identification — but they also introduce a new class of bugs that EVM `call` doesn't have.

## Anatomy of a safe CPI

```rust
use anchor_spl::token::{self, MintTo, Token};

let cpi_accounts = MintTo {
    mint: ctx.accounts.mint.to_account_info(),
    to: ctx.accounts.recipient.to_account_info(),
    authority: ctx.accounts.mint_authority.to_account_info(),
};
let cpi_program = ctx.accounts.token_program.key();
let cpi_ctx = CpiContext::new_with_signer(cpi_program, cpi_accounts, signer_seeds);

token::mint_to(cpi_ctx, amount)?;
```

What's checked:

- `ctx.accounts.token_program` is typed `Program<'info, Token>`, which Anchor verifies has the correct program ID and `executable = true`.
- The accounts passed match the inner instruction's expected positions (because `MintTo` is a typed struct).
- The signer seeds correctly identify the authority PDA.

What you must check yourself:

- The target program ID is what you think it is. `Program<'info, Token>` covers SPL Token specifically. If the program is supplied dynamically (e.g. user picks the AMM to swap through), you need an explicit allowlist:

```rust
const ALLOWED_AMMS: [Pubkey; 2] = [orca::id(), raydium::id()];
require!(
    ALLOWED_AMMS.contains(&ctx.accounts.amm_program.key()),
    MyError::UnsupportedAmm
);
```

Never CPI into a program supplied as `AccountInfo` without validating its ID. The inner program could be malicious — it could re-enter your program, drain accounts, or simply lie about success.

## Never trust CPI return data

Solana programs return an exit code (success/failure) but no rich return value. Anchor's higher-level helpers (`token::mint_to`, etc.) return `Result<()>`. If you need data from the CPI's effects, **re-read the account** after the call:

```rust
let supply_before = ctx.accounts.mint.supply;
token::mint_to(cpi_ctx, amount)?;
ctx.accounts.mint.reload()?;
require_eq!(ctx.accounts.mint.supply, supply_before + amount);
```

The `reload()` call re-deserializes the account from its post-CPI state. Without it, `ctx.accounts.mint.supply` still shows the pre-CPI value — Anchor accounts are deserialized at instruction entry, not after every CPI.

This is a frequent porting bug: a developer assumes the in-memory Anchor account reflects state changes from a CPI, like EVM does. It doesn't. Reload or read the account again.

## `invoke` vs `invoke_signed`

- `invoke(instruction, account_infos)` — call without signing for any PDA. Inner program receives accounts; signer flags pass through from the outer transaction.
- `invoke_signed(instruction, account_infos, signer_seeds)` — same, plus the runtime grants `is_signer = true` to PDAs whose seeds (with program ID) match the supplied seeds.

In Anchor:
- `CpiContext::new(program, accounts)` → `invoke`
- `CpiContext::new_with_signer(program, accounts, signer_seeds)` → `invoke_signed`

Use `new_with_signer` only when the inner program requires a PDA-controlled authority to sign. For user-initiated CPIs (e.g. user burns their own tokens), `new` is correct.

## Account ordering matters

Solana instructions take a *positional* list of accounts. Anchor's typed structs (`MintTo`, `Transfer`, etc.) handle ordering for you. If you build CPIs by hand:

```rust
let instruction = solana_program::instruction::Instruction {
    program_id: token_program_id,
    accounts: vec![
        AccountMeta::new(mint_pubkey, false),         // position 0
        AccountMeta::new(to_pubkey, false),           // position 1
        AccountMeta::new_readonly(authority, true),   // position 2 — signer
    ],
    data: ...,
};
```

The wrong order means the inner program operates on the wrong accounts. Always prefer Anchor's typed CPI helpers.

## Privilege escalation paths

A subtle CPI bug: your program signs for a PDA in a CPI, but the inner program does *something other than what you intended* with that authority.

```rust
// BUG: signs over an arbitrary inner instruction
let ix: Instruction = bincode::deserialize(&ctx.accounts.user_supplied_data.data.borrow())?;
invoke_signed(&ix, &accounts, signer_seeds)?;
```

If the program supplied `bincode` data was tampered with, you've just signed an arbitrary instruction with PDA authority. The PDA might be your token's mint authority — the attacker just minted infinite tokens.

**Rule:** Never sign arbitrary user-supplied instructions with PDA authority. Sign only specific, typed CPIs your program constructs.

## Reentrancy via CPI

See `security/reentrancy.md` for the full treatment. The short version: a program you CPI into can CPI back into you. Solana's account-locking prevents the *same* account from being modified twice in a transaction, but it doesn't prevent the inner program from making side-effects that violate your invariants. Re-validate state on the return path if the called program is untrusted.

## Allow-listing CPI targets

If your program calls into a fixed set of external programs (e.g. an allowlist of DEX aggregators), constrain the program account at the type level:

```rust
#[derive(Accounts)]
pub struct SwapVia<'info> {
    #[account(constraint = orca_program.key() == orca::id())]
    pub orca_program: UncheckedAccount<'info>,
    // ...
}
```

For dynamic allowlisting (a runtime-mutable list), keep the allowlist in a `Config` PDA and check membership at instruction entry.

## CPI depth limit

Solana caps CPI depth at 4 levels (the outer transaction is depth 1; the deepest reachable CPI is depth 4). Exceeding it aborts. Deep call chains are an antipattern anyway — they make CU accounting and account passing intractable.
