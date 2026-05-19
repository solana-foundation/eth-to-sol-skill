# Solidity ↔ Rust/Anchor type mapping

The mechanical layer. Translate types first, then translate behavior.

## Integers

| Solidity | Anchor/Rust | Notes |
|---|---|---|
| `uint8` / `int8` | `u8` / `i8` | |
| `uint16` / `int16` | `u16` / `i16` | |
| `uint32` / `int32` | `u32` / `i32` | |
| `uint64` / `int64` | `u64` / `i64` | Default for amounts, timestamps. |
| `uint128` / `int128` | `u128` / `i128` | Available but slower; review whether u64 suffices. |
| `uint256` / `int256` | `u128` (often) or pair of `u64` | Solana has no native u256. ERC-20 amounts with ≤9 decimals fit u64 (≈1.8e19 base units). Check `decimals × totalSupply` before downcasting. |

Solidity defaults to `uint256` for *everything*. Solana code should pick the narrowest correct type. Wider types cost CU on every op and rent on every account.

**Rule:** for an ERC-20 with `decimals ≤ 9`, use `u64` and document the assumption. For DeFi math that genuinely needs u256 (e.g. Uniswap-style price products), use `u128` with carefully placed intermediate widening, or a crate like `spl-math` for fixed-point.

## Addresses & identity

| Solidity | Anchor/Rust |
|---|---|
| `address` | `Pubkey` — 32 bytes (vs 20 for Solidity). Not assignment-compatible with anything 20-byte. |
| `msg.sender` | `ctx.accounts.<the_signer>.key()`. There is no implicit caller — callers are explicit accounts in the instruction's account list. |
| `address(this)` | Either `ctx.program_id` (the program itself) or a PDA derived from the program (the program's "self-account"). The distinction matters: programs do not own lamports the way Ethereum contracts do. |
| `tx.origin` | **No equivalent.** Solana has no concept of an EOA-distinct-from-contract chain. Any Solidity logic relying on `tx.origin` is a security smell to drop, not translate. |
| `address payable` | Not a thing. Any account can receive lamports — there is no payable/nonpayable distinction. |

## Booleans, strings, bytes

| Solidity | Anchor/Rust |
|---|---|
| `bool` | `bool` (1 byte on-chain). |
| `string` | `String` — pay rent for the bytes; size with `4 + max_len` in account `space`. |
| `bytes` | `Vec<u8>` — same sizing rule. |
| `bytes32` | `[u8; 32]` |
| `bytesN` | `[u8; N]` |

On-chain strings cost rent forever. Token names, symbols, URIs belong in **Metaplex Token Metadata** (off-chain JSON pointed to by an on-chain account), not in your program's state.

## Mappings

`mapping(K => V)` has no direct equivalent. Three translations, ranked:

1. **PDA per key** (preferred): `seeds = [b"prefix", key.as_ref()]`. Each entry is its own account.
2. **Vec in a single account** (small bounded sets): `Vec<(K, V)>`, hard-capped, rent paid up front for max size.
3. **Off-chain index** (read-heavy, write-rare): emit events, index off-chain (Helius / Solana indexers).

Default to (1) unless the set is fundamentally small and bounded. For ERC-20 specifically, do not translate balance/allowance mappings at all — see `translation/stdlib-mapping.md`.

```solidity
mapping(address => uint256) balances;
```

→ Pass 1 (faithful, Vec form):

```rust
#[account]
pub struct TokenState {
    pub balances: Vec<(Pubkey, u64)>, // SMELL: serializes all writes, hard cap
}
```

→ Pass 2 (PDA per key):

```rust
#[account]
pub struct Balance {
    pub owner: Pubkey,
    pub amount: u64,
    pub bump: u8,
}
// seeds = [b"balance", owner.as_ref()]
```

Nested mappings (`mapping(address => mapping(address => uint256)) allowance;`) → PDAs with two seeds: `[b"allowance", owner.as_ref(), spender.as_ref()]`.

## Structs

Solidity `struct` → Rust struct with `#[account]`. The `#[account]` macro adds an 8-byte discriminator at the front; account `space` must include it.

```solidity
struct Position { uint256 collateral; uint256 debt; uint64 lastUpdate; }
```
→
```rust
#[account]
pub struct Position {
    pub collateral: u64,
    pub debt: u64,
    pub last_update: i64,  // Solana timestamps are i64 unix seconds
    pub bump: u8,
}
// space = 8 (discriminator) + 8 + 8 + 8 + 1 = 33
```

Non-account structs (used only as fields inside accounts) derive `AnchorSerialize, AnchorDeserialize, Clone` and are not `#[account]`-tagged.

## Arrays

| Solidity | Anchor/Rust |
|---|---|
| `T[N]` fixed | `[T; N]` |
| `T[]` dynamic | `Vec<T>` with a hard cap, or split into per-element PDAs. |

Solidity's unbounded `T[]` that grows forever is an antipattern on both chains; on Solana it is also rent-incurring. Always cap.

## Enums

```solidity
enum Status { Pending, Active, Closed }
```
→
```rust
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq)]
pub enum Status { Pending, Active, Closed }
```

`#[repr(u8)]` is implied by Anchor's serialization. Anchor enums with variant data are allowed but increase deserialization cost — prefer flat enums for hot paths.

## Events

```solidity
event Transfer(address indexed from, address indexed to, uint256 amount);
```
→
```rust
#[event]
pub struct Transfer {
    pub from: Pubkey,
    pub to: Pubkey,
    pub amount: u64,
}
// fire with: emit!(Transfer { from, to, amount });
```

Anchor events are serialized to transaction logs and parsed off-chain via the IDL. There is no `indexed` distinction — all fields are equally available to indexers. Log emission costs CU; do not emit redundant events that SPL programs already emit (e.g. Transfer when SPL Token already fired one).

## Errors

```solidity
error InsufficientBalance();
require(x >= y, "msg");           // string-revert form
```
→
```rust
#[error_code]
pub enum TokenError {
    #[msg("insufficient balance")]
    InsufficientBalance,
}

require!(x >= y, TokenError::InsufficientBalance);
```

Never use `ProgramError::Custom(n)` with literal numbers — Anchor renumbers `#[error_code]` enums automatically and surfaces messages in the IDL. String literals in `msg!` are not user-facing errors.

## Time & block info

| Solidity | Anchor/Rust |
|---|---|
| `block.timestamp` | `Clock::get()?.unix_timestamp` (`i64`). |
| `block.number` | `Clock::get()?.slot` (`u64`). **Slots are not blocks** — they run ~400ms, ≈2× the EVM block rate. Time-locked logic that uses `block.number` must convert to slots carefully. |
| `block.difficulty` / `block.basefee` | No equivalent. |
| `blockhash(n)` | `recent_blockhashes` sysvar (deprecated) / `slot_hashes` sysvar. Rarely used in app code. |

Reading `Clock` via `Clock::get()` requires no account in the instruction's account list (it's a syscall). Cheap.

## Money

| Solidity | Anchor/Rust |
|---|---|
| `msg.value` | No analog. SOL is transferred by explicit `system_program::transfer` CPI or by lamport math on writable `AccountInfo`s. |
| `payable` | Not a thing. Any instruction can receive lamports via an accompanying transfer. |
| `wei` | `lamports`. 1 SOL = 1e9 lamports (vs 1 ETH = 1e18 wei). Different precision — beware of `decimals` confusion when porting fee math. |
| ERC-20 amounts | `u64` base units (assuming `decimals ≤ 9`). |
