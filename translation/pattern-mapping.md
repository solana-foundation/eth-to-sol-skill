# Solidity ↔ Anchor pattern mapping

How common Solidity *constructs* translate to Anchor *idioms*. This is where the EVM mental model needs to be unwound.

## `msg.sender`

Solidity treats the caller as ambient. Anchor makes it explicit: the caller appears in the instruction's account list, marked `Signer<'info>`.

```solidity
function foo() external {
    require(msg.sender == owner);
}
```

→

```rust
#[derive(Accounts)]
pub struct Foo<'info> {
    pub state: Account<'info, State>,
    #[account(constraint = state.owner == authority.key())]
    pub authority: Signer<'info>,
}
```

Or, using `has_one`:

```rust
#[derive(Accounts)]
pub struct Foo<'info> {
    #[account(has_one = owner)]
    pub state: Account<'info, State>,
    pub owner: Signer<'info>,
}
```

`has_one = owner` says: the account's `owner` field must equal the `owner` account in this struct. Combined with `Signer<'info>`, this is the canonical "only the owner can call this" pattern.

## Modifiers

Solidity:

```solidity
modifier onlyOwner() {
    if (msg.sender != owner) revert NotOwner();
    _;
}

function mint(...) external onlyOwner { ... }
```

Anchor has no modifier syntax. Translate to either:

1. **An `#[derive(Accounts)]` constraint** (preferred — moves the check to account validation, before the function body runs):

```rust
#[derive(Accounts)]
pub struct Mint<'info> {
    #[account(has_one = owner)]
    pub state: Account<'info, State>,
    pub owner: Signer<'info>,
}
```

2. **An explicit guard at function top** (when the check needs runtime data):

```rust
pub fn mint(ctx: Context<Mint>, amount: u64) -> Result<()> {
    require_keys_eq!(ctx.accounts.state.owner, ctx.accounts.signer.key(), Err::NotOwner);
    // ...
}
```

Prefer constraints. They are declarative, surface in the IDL, and run before any mutation can happen.

## `mapping`

See `translation/type-mapping.md` for the type-level translation. The behavioral pattern:

```solidity
mapping(address => uint256) public balanceOf;
function balanceOf(address a) view returns (uint256) { return balanceOf[a]; }
```

A Solidity `mapping` is *implicit lookup*: `balanceOf[a]` works for every `a` without ever having been written. On Solana, an unwritten PDA *does not exist*. Reading it returns "account does not exist", not "zero". Translations must either:

- Treat "PDA does not exist" as the zero case (most common — wrap the load in `init_if_needed` or check existence client-side).
- Eagerly initialize the PDA in a setup instruction.

For ERC-20 specifically, this is moot — SPL Token's account model already has a "not-yet-created" → zero balance equivalent via ATAs.

## `require` / `revert`

| Solidity | Anchor |
|---|---|
| `require(cond, "msg")` | `require!(cond, ErrorType::Variant)` |
| `revert MyError()` | `return Err(error!(MyError))` or `Err(MyError.into())` |
| `assert(cond)` | `require!(cond, ...)` — Anchor has no `assert!` distinction. |

Anchor exposes several `require_*!` macros for common comparisons:

```rust
require_keys_eq!(a, b, MyError::Mismatch);
require_eq!(x, y, MyError::Mismatch);
require_gte!(balance, amount, MyError::InsufficientBalance);
```

These produce better error messages than a plain `require!(a == b, ...)` and should be preferred where they apply.

## Constructor

Solidity constructors run *once at deploy time*. Anchor has no constructor — programs are deployed without state, and you write an explicit `initialize` instruction that runs once.

```solidity
constructor(string memory _name) {
    name = _name;
    owner = msg.sender;
}
```

→

```rust
pub fn initialize(ctx: Context<Initialize>, name: String) -> Result<()> {
    let state = &mut ctx.accounts.state;
    state.name = name;
    state.owner = ctx.accounts.payer.key();
    state.bump = ctx.bumps.state;
    Ok(())
}

#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(init, payer = payer, space = 8 + State::SIZE, seeds = [b"state"], bump)]
    pub state: Account<'info, State>,
    #[account(mut)]
    pub payer: Signer<'info>,
    pub system_program: Program<'info, System>,
}
```

**Re-init protection:** because anyone can call `initialize`, the PDA's seeds must make double-initialization impossible. Anchor's `init` constraint fails if the account already exists. Do not use `init_if_needed` on an init-once account.

## Receive / fallback

```solidity
receive() external payable { ... }
fallback() external payable { ... }
```

No equivalent. Solana programs cannot "receive lamports" passively — every value transfer is an explicit instruction. Logic that relied on `receive` to do work on incoming ETH must become an explicit instruction the sender calls.

## Inheritance

Solidity inheritance flattens at compile time. Anchor has none — Rust traits and module composition replace it. In practice:

- "Inherits from `ERC20`" → use SPL Token (CPI), don't reimplement.
- "Inherits from `Ownable`" → store an `authority: Pubkey` field on your config PDA, gate writes with `has_one = authority` + `Signer<'info>`.
- "Inherits from `AccessControl`" → one PDA per role, or a `roles: Vec<Role>` field on a config PDA (small role sets only).

## View / pure / external / public / internal / private

Visibility modifiers do not exist on Solana — every instruction is callable by anyone with the IDL. Access is enforced by *constraints*, not by syntax.

`view`/`pure` functions have no analog. Clients read state by deserializing accounts directly (no instruction call needed). If you find yourself writing an instruction that only reads, you are usually wrong — expose the data via account layout and let the client read it.

## `delegatecall` / proxy patterns

No equivalent. Solana programs are upgradeable via `solana program deploy` with an upgrade authority — there is no proxy pattern. EIP-1967 / UUPS / Transparent Proxy are not translated; they become "deploy with an upgrade authority and rotate it to multisig/governance".

## `selfdestruct`

```solidity
selfdestruct(payable(recipient));
```

Solana equivalent: drain lamports from a program-owned account back to a recipient. This makes the account rent-non-exempt and the runtime garbage-collects it. Not a program-level operation — it is an account operation. Account closure idiom in Anchor:

```rust
#[account(mut, close = recipient)]
pub stale_account: Account<'info, StaleData>,
```

`close = recipient` zeroes the account's data, marks it unallocated, and transfers all lamports to `recipient`.

## Events as state

A Solidity antipattern: writing data only to events and relying on indexers. On Solana, the same pattern is *more* viable because indexers (Helius, Triton) are more uniform — but logs are CU-priced, so high-volume events still warrant evaluation. See `optimization/compute-budget.md`.
