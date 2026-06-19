# The EVM -> SVM mental model

The single most useful sentence for translating a Solidity contract:

> **On Ethereum, the contract knows where its state lives. On Solana, the caller brings it.**

If a translation step ever feels wrong, come back here.

## You bring the state

A Solidity contract holds storage inside itself. `balanceOf(addr)` reads from a slot that the contract resolves internally; the caller hands the contract a function selector and arguments and the contract does the rest.

A Solana program is stateless. It contains executable code and nothing else. Every piece of data the program will read or write lives in a *separate account*, and the caller must list every one of those accounts in the transaction up front. State discovery is the caller's job, not the program's.

This is not a stylistic preference. It is the foundation of every translation decision:

- Why `#[derive(Accounts)]` exists: the program must declare its account list at compile time so callers can prepare it at call time.
- Why a `mapping(K => V)` becomes one PDA per key: there is no implicit slot lookup, so each entry is its own account with a deterministic address.
- Why CPIs propagate account lists: a callee program can only touch accounts the caller already put in the transaction.
- Why Solana transactions look big: they carry the precomputed account graph inline, and large graphs need versioned transactions plus Address Lookup Tables.
- Why parallel execution works: every transaction's read/write set is declared up front, so Sealevel can schedule non-overlapping ones in parallel.

Internalize "you bring the state" and every other rule below is its consequence.

## Caller-brings-state, in code

The Solidity version of a counter (`uint256 public count; function increment() { count += 1; }`) becomes:

```rust
#[program]
pub mod counter {
    pub fn increment(ctx: Context<Increment>) -> Result<()> {
        ctx.accounts.counter.value = ctx
            .accounts
            .counter
            .value
            .checked_add(1)
            .ok_or(CounterError::Overflow)?;
        Ok(())
    }
}

#[account]
pub struct Counter {
    pub value: u64,
    pub bump: u8,
}

#[derive(Accounts)]
pub struct Increment<'info> {
    #[account(mut, seeds = [b"counter"], bump = counter.bump)]
    pub counter: Account<'info, Counter>,
}
```

The contract has split into three things:

1. The **program**: code addressed by a program ID.
2. The **account** holding the counter value: a PDA owned by the program.
3. The **account list declaration**: Anchor validation before the handler runs.

A client calling `increment()` derives the counter PDA, includes it in the transaction account list, signs, and sends. The program never "looks up" the counter; the transaction hands it over.

This pattern repeats for every translation. The first question for any Solidity contract is: where does its state live, and how does that decompose into accounts?

## The account-graph propagation rule

EVM composability is late-bound: a contract holds another contract's address and calls it; the callee reads whatever storage it needs.

Solana composability is early-bound: when program A calls program B via CPI, every account program B will touch must already be in A's instruction account list. There is no "callee fetches what it needs" path.

In practice:

- A swap aggregator routing through five AMMs includes the accounts for all five hops in one transaction.
- A program cannot CPI into "whatever program is at this address" without knowing and validating the called program and its account contract.
- Cross-program mutation is explicit. If program B mutates an account during a CPI, the original transaction must already have marked that account writable.

For dynamic downstream account sets, Anchor exposes `remaining_accounts: &[AccountInfo]`. Treat it as an escape hatch: validate program IDs, owners, signer status, and account ordering before forwarding.

## Reentrancy is structural, not a guard

Do not port `ReentrancyGuard` or a `nonReentrant` mutex. Solana's runtime prevents a program from appearing twice in a single call stack, and account write locks prevent conflicting writable account access from executing concurrently.

That does not mean CPIs are risk-free. The replacement concerns are account validation, stale reads after CPI, untrusted CPI targets, PDA signer authority, and Token-2022 transfer-hook surfaces. See `security/reentrancy.md`, `security/cpi-safety.md`, and `security/account-validation.md`.

## Translation reference

The full type-level mapping lives in `translation/type-mapping.md`; the pattern-level mapping is in `translation/pattern-mapping.md`; library swaps are in `translation/stdlib-mapping.md`. This table is the one-line index.

| EVM concept | SVM equivalent | Detail |
|---|---|---|
| Contract bytecode | Program: BPF binary, executable account owned by BPF Loader | Program is code only; state lives elsewhere. |
| Contract storage | Accounts owned by the program | See `optimization/account-model.md`. |
| Storage slot | Program-owned account | Account sizing and rent are explicit. |
| `mapping(K => V)` | One PDA per key | See `translation/type-mapping.md` and `optimization/pdas.md`. |
| `msg.sender` | `Signer<'info>` | Explicit account; no implicit caller. |
| `tx.origin` | Fee payer | No transitive origin concept. |
| `address(this)` | Program ID (`crate::ID`) | Or a PDA controlled by the program when signing is needed. |
| `calldata` | Instruction data | Anchor deserializes typed arguments. |
| Function selector | First 8 bytes, Anchor discriminator | Derived from the instruction name. |
| Constructor | `initialize` instruction | State bootstraps through a normal instruction. |
| Immutable code | Upgradable by default | Freeze by setting upgrade authority to `None`. |
| UUPS / Transparent proxy | BPF Loader Upgradeable | In-place upgrade; governance is the upgrade authority. |
| ABI | IDL | Generated by Anchor. |
| Events / indexed topics | Logs/events plus off-chain indexing | No indexed event topics. |
| `block.timestamp` | `Clock::get()?.unix_timestamp` | Clock sysvar. |
| `block.number` | Slot | Roughly 400 ms slots. |
| Gas | Compute Units | See `optimization/compute-budget.md`. |
| Global gas auction | Priority fees plus local fee markets | See `optimization/transactions-and-commitment.md`. |
| Nonce | Recent blockhash | Transactions expire after roughly 150 slots; see `optimization/transactions-and-commitment.md`. |
| Block confirmations | `processed`, `confirmed`, `finalized` | Choose by reversibility of the downstream action. |
| Mempool | None in the EVM sense | Gulf Stream forwards transactions to upcoming leaders. |
| Reentrancy guard | Runtime invariant | Do not port; audit new Solana failure modes instead. |
| `require` / `revert` | `require!` / `Err(...)` | See `translation/pattern-mapping.md`. |
| ERC-20 | Mint + Token Account + ATA | See `translation/stdlib-mapping.md`. |
| `transferFrom` | Transfer by delegate | SPL Token delegate is per token account, not per `(owner, spender)`. |
| `approve` | SPL Token approve | Sets `delegate` and `delegated_amount`. |
| ERC-721 / ERC-1155 | Token-2022, Metaplex, or compressed NFTs | Pick the ecosystem primitive instead of porting maps. |
| `delegatecall` | No direct analog | Use CPI with explicit account propagation. |
| `selfdestruct` | `#[account(mut, close = recipient)]` | See `translation/pattern-mapping.md`. |
| Ownable | Config PDA with `authority: Pubkey` + `has_one = authority` | See `translation/stdlib-mapping.md`. |
| AccessControl | One PDA per `(role, holder)` pair, or fixed role fields on config | See `translation/stdlib-mapping.md`. |
| SafeMath | `checked_*` arithmetic on every op | See `security/arithmetic.md`. |
| Pausable token | `freeze_authority` on the Mint, per-account freeze only | See `translation/stdlib-mapping.md`. |
| ERC-2612 permit | Drop | Solana transactions already carry explicit signatures. |
