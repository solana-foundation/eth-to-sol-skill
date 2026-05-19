# Account model — discrete PDAs over monolithic state

The single biggest restructuring win when porting from EVM. Read this before anything else in `optimization/`.

## The mental shift

Ethereum: a contract is a *namespace* over storage. Every variable lives "in the contract" and is addressed by storage slot. `balanceOf[alice]` and `balanceOf[bob]` live in the same conceptual place — the contract — and are looked up by key inside the contract's storage trie.

Solana: a program is *code*; state lives in separate accounts that the program *owns*. Each account has its own address (a `Pubkey`), its own data buffer, its own rent. The program decides which accounts can be read or written during an instruction — but accounts are first-class addressable things, not slots inside a contract.

Consequence: where Solidity uses one `mapping` over many keys, Solana uses one *PDA per key*. The mapping's keys become seeds.

## Why this is the right default

1. **Parallelism.** Sealevel parallelizes instructions that touch disjoint sets of writable accounts. A `mapping(address => uint256) balances` translated naively to `Vec<(Pubkey, u64)>` inside one account means *every transfer in the system write-locks that one account*. With one PDA per balance holder, transfers between disjoint pairs parallelize freely.
2. **Rent independence.** Account-per-entity means each entity pays its own rent. Single-account-with-Vec means the contract owner (or whoever called `initialize`) pays rent for the maximum capacity up front.
3. **Discoverability.** PDA seeds are deterministic — given a public key and a seed prefix, you can derive the account address client-side without an on-chain lookup.
4. **Composability.** Other programs can take your `Balance` PDA as an input account and reason about it directly.

## The translation rule

```
mapping(K => V) m;             →   PDA per K, seeds = [b"prefix", key.as_ref()]
mapping(K1 => mapping(K2 => V)) → PDA per (K1, K2), seeds = [b"prefix", k1.as_ref(), k2.as_ref()]
```

The Solidity contract becomes:

- **One `Config` PDA** holding the formerly-singleton fields (`owner`, `name`, `paused`, etc.).
- **N entity PDAs** — one per logical mapping entry.

Example:

```solidity
contract Vault {
    address public owner;
    uint256 public totalDeposits;
    mapping(address => uint256) public deposits;
}
```

→

```rust
#[account]
pub struct VaultConfig {
    pub authority: Pubkey,
    pub total_deposits: u64,
    pub bump: u8,
}
// seeds = [b"vault"]

#[account]
pub struct Deposit {
    pub depositor: Pubkey,
    pub amount: u64,
    pub bump: u8,
}
// seeds = [b"deposit", depositor.as_ref()]
```

## When NOT to do this

Per-key PDAs are not free. Each one is an account with its own rent (~0.002 SOL minimum). For maps that will hold thousands of keys, this is fine. For maps with three to five keys (e.g. a small whitelist), the PDA-per-key shape is overkill and a `Vec` inside the config PDA is fine.

Guideline:

- **> ~10 entries, or growth-unbounded** → PDA per key.
- **≤ ~10 entries, bounded** → `Vec` in a single account, capped, with rent paid for max capacity.

The cutoff is approximate. The deciding factor is usually whether different entries are written by different actors — if yes, PDA per key, *always*, regardless of count, because the parallelism story dominates.

## Hot-write fields are an antipattern

A common Solidity-shaped translation: keep `totalSupply` on the config PDA and increment it on every mint and decrement on every burn. This makes the config PDA a *write-hot* account: every mint and burn write-locks it, serializing all of them.

Fixes, in order of preference:

1. **Delete the field.** If you can derive the value from an SPL Token Mint (`mint.supply`), don't track it yourself.
2. **Move the field to a counter PDA.** Now it still serializes, but at least it doesn't block reads of other config fields.
3. **Shard the counter.** Split `total_supply` into `total_supply_shard[i]` PDAs, one per N writers. Aggregate off-chain. Only worth doing for genuinely hot paths.

For ERC-20 specifically, option (1) is the answer: SPL Token tracks `supply` for you.

## Read aggregates from SPL Token; don't self-track

Solidity contracts often store aggregates — `totalSupply`, `totalAssets`, `totalDeposits` — alongside per-user balances. On Solana, when those tokens move via SPL Token, the aggregates already exist on the SPL accounts:

| Solidity aggregate | Solana equivalent | Maintained by |
|---|---|---|
| `IERC20(shareToken).totalSupply()` | `Account<'info, Mint>.supply` of the share Mint | SPL Token |
| `IERC20(asset).balanceOf(address(this))` (your pool's reserve) | `Account<'info, TokenAccount>.amount` of the reserve | SPL Token |
| A counter of issued positions | rarely a Mint; sometimes a `Vec` (small + bounded) | depends |

Read them off the deserialized Anchor account. Do not store a parallel copy on your config/vault PDA. Two reasons:

1. **Divergence-bug class eliminated.** Any missed `+=`/`-=` in your code makes your tracked aggregate disagree with reality. SPL Token's atomic mint/burn/transfer cannot disagree with itself; reading from it is the canonical value.
2. **Parallelism win.** If `total_supply`/`total_assets` live on your config PDA, *every* state-changing instruction writes that PDA — cross-user contention is forced. If they live on SPL Token accounts, your config/vault PDA can be **read-only** during user operations (deposits, withdrawals, swaps). See `optimization/parallelism.md` "When the governance PDA can be read-only."

```rust
// Anti-pattern: self-tracked aggregates
#[account]
pub struct Vault {
    pub total_assets: u64,    // SMELL: mirrors asset_reserve.amount, forces write-lock
    pub total_supply: u64,    // SMELL: mirrors share_mint.supply
    // ...
}

// Right pattern: derive at call time
let total_assets = ctx.accounts.asset_reserve.amount;
let total_supply = ctx.accounts.share_mint.supply;
```

Cross-reference: `examples/erc4626-vault/03-optimized.rs` deletes both `total_assets` and `total_supply` from the vault account and reads them on demand.

## Discriminator and rent

Every Anchor `#[account]` adds an 8-byte discriminator at the front of the account data. Account `space` must always include this:

```rust
space = 8 + sizeof(fields)
```

Forgetting the 8 produces a runtime deserialization error. The Anchor docs and codegen account for it, but raw-Solana drop-downs do not.

Rent-exempt minimum balance scales linearly with `space`. Sub-100-byte accounts cost ~0.001 SOL; per-user PDAs typically land in the 0.001–0.005 SOL range.

## Reallocation

Account size is fixed at init. To grow:

```rust
#[account(mut, realloc = 8 + NewLayout::SIZE, realloc::payer = payer, realloc::zero = false)]
pub state: Account<'info, State>,
```

`realloc::zero = false` keeps existing bytes (you append zeroes); `true` zeroes the whole buffer. Reallocation costs additional rent up front; shrinking refunds rent to the payer.

Avoid relying on realloc — predict max size at init when you can. See `optimization/rent-and-size.md`.

## A practical checklist when laying out state

1. List every `mapping`, `array`, and storage variable from the Solidity contract.
2. For each, ask: *who writes it, how often, by which actor?*
3. Group items that are written by the **same actor in the same instruction** into one account. Split items written by different actors into separate accounts.
4. Mapping → PDA per key (unless small + bounded).
5. Singletons that everyone reads but few write → config PDA.
6. Anything derivable from an existing Solana program (SPL Token supply, Metaplex metadata) → delete from your state, read from theirs.

After this exercise, the optimized program usually has half as much custom state as the naive port.
