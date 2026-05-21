# The EVM → SVM mental model

The single most useful sentence for translating a Solidity contract:

> **On Ethereum, the contract knows where its state lives. On Solana, the caller brings it.**

If a translation step ever feels wrong, come back here.

## You bring the state

A Solidity contract holds storage inside itself. `balanceOf(addr)` reads from a slot that the contract resolves internally; the caller hands the contract a function selector and arguments and the contract does the rest.

A Solana program is stateless. It contains executable code and nothing else. Every piece of data the program will read or write lives in a *separate account*, and the caller must list every one of those accounts in the transaction up front. State discovery is the caller's job — not the program's.

This is not a stylistic preference. It's the foundation of every translation decision:

- Why `#[derive(Accounts)]` exists — the program must declare its account list at compile time so callers can prepare it at call time.
- Why a `mapping(K => V)` becomes one PDA per key — there's no "implicit slot lookup", so each entry is its own account with a deterministic address.
- Why CPIs propagate account lists — a callee program can only touch accounts the caller already put in the transaction.
- Why Solana transactions look big — they carry the precomputed account graph inline.
- Why parallel execution (Sealevel) works — every transaction's read/write set is declared up front, so the runtime can schedule non-overlapping ones in parallel.

Internalize "you bring the state" and every other rule below is its consequence.

## Caller-brings-state, in code

The Solidity version of a counter (`uint256 public count; function increment() { count += 1; }`) becomes:

```rust
#[program]
pub mod counter {
    pub fn increment(ctx: Context<Increment>) -> Result<()> {
        ctx.accounts.counter.value += 1;
        Ok(())
    }
}

#[account]
pub struct Counter { pub value: u64 }

#[derive(Accounts)]
pub struct Increment<'info> {
    #[account(mut, seeds = [b"counter"], bump)]
    pub counter: Account<'info, Counter>,
}
```

The contract has split into three things:

1. The **program** (the code, addressed by program ID).
2. The **account** holding the counter's value (a PDA, owned by the program).
3. The **account list declaration** (`Increment`) telling Anchor how to validate the account before the handler runs.

A client calling `increment()` derives the counter PDA, includes it in the transaction's account list, signs, and sends. The program never "looks up" the counter — the transaction hands it over.

This pattern repeats for every translation. The first question for any Solidity contract is "where does its state live, and how does that decompose into accounts?"

## The account-graph propagation rule

Composability on EVM is late-bound: a contract holds another contract's address and `call`s it; the callee reads whatever state it needs. Composability on Solana is early-bound: when program A calls program B via CPI, *every account program B will touch must already be in A's instruction's account list*. There is no "callee fetches what it needs" pathway.

In practice:

- A swap aggregator routing through 5 AMMs includes the accounts for all 5 in a single transaction. Jupiter transactions look enormous for this reason — they carry the full account graph for every hop.
- A program cannot CPI into "whatever program is at this address" without knowing which accounts that program expects. Dynamic dispatch in the Solidity sense doesn't translate.
- Cross-program state mutation is explicit. When program A CPIs into program B and B mutates an account, A's instruction must have marked that account `mut`. No silent side effects in third-party programs.

For programs that don't know the downstream account list at compile time, Anchor exposes `remaining_accounts: &[AccountInfo]` — accounts that arrived in the transaction but weren't deserialized or validated. CPIs can forward them. Use sparingly; it's the escape hatch, not the default.

## Reentrancy is structural, not a guard

Solana's runtime takes a write-lock per writable account in a transaction. Two transactions touching overlapping writable accounts can't execute in parallel; within a single transaction, the same lock prevents a program from being re-entered while it's already executing. There's no `nonReentrant` modifier to port because the runtime enforces the invariant for free.

That doesn't mean the security model is identical — the failure modes are different. See `security/reentrancy.md` for what *can* still go wrong (read-after-CPI staleness, untrusted-CPI callback surfaces) and `security/account-validation.md` for the discriminator/ownership/substitution checks that replace the EVM reentrancy concern.

## Translation reference (one row at a time)

The full type-level mapping lives in `translation/type-mapping.md`; the pattern-level mapping is in `translation/pattern-mapping.md`; library swaps are in `translation/stdlib-mapping.md`. This table is the one-line index — use it to find the right detail file.

| EVM concept | SVM equivalent | Detail in |
|---|---|---|
| Contract bytecode | Program (BPF binary, executable account owned by BPF Loader) | — |
| Contract storage | Accounts owned by the program | `type-mapping.md` |
| Storage slot | A program-owned account | `type-mapping.md` |
| `mapping(K => V)` | One PDA per key | `type-mapping.md` § Mappings |
| `msg.sender` | `Signer<'info>` declared in the instruction's account list | `pattern-mapping.md` |
| `tx.origin` | Fee payer (no transitive concept) | `type-mapping.md` |
| `address(this)` | `crate::ID` or a self-PDA | `type-mapping.md` |
| `calldata` | Instruction data (`&[u8]`) | — |
| Function selector | First 8 bytes (Anchor discriminator from instruction name) | — |
| Constructor | `initialize` instruction (no special constructor concept) | `pattern-mapping.md` |
| Immutable code | Upgradable by default; freeze by setting upgrade authority to `None` | `pattern-mapping.md` |
| UUPS / Transparent proxy | BPF Loader Upgradeable in place; no proxy pattern | `pattern-mapping.md` |
| ABI (JSON) | IDL (JSON), generated by Anchor | — |
| Events / logs | `#[event]` + `emit!`, parsed off-chain | `type-mapping.md` § Events |
| `block.timestamp` | `Clock::get()?.unix_timestamp` (`i64`) | `type-mapping.md` |
| `block.number` | `Clock::get()?.slot` (`u64`, ~400 ms per slot) | `type-mapping.md` |
| `block.difficulty` / `basefee` | — | `type-mapping.md` |
| `msg.value` | No analog; explicit `system_program::transfer` CPI | `type-mapping.md` § Money |
| `payable` | Not a thing | `type-mapping.md` |
| `wei` | `lamports` (1 SOL = 1e9 lamports, vs 1 ETH = 1e18 wei) | `type-mapping.md` |
| ERC-20 amounts | `u64` base units (assuming `decimals ≤ 9`) | `type-mapping.md` |
| Gas (gwei) | Compute Units (1.4M CU max per transaction) | — |
| Global gas auction | Local fee markets per writable account | — |
| Nonce | Recent blockhash; tx expires ~150 slots (~60s) after | — |
| Reentrancy guard | Runtime invariant (write-lock) — don't port | `security/reentrancy.md` |
| `require` / `revert` | `require!` / `Err(...)` | `pattern-mapping.md` |
| ERC-20 | Mint + Token Account (+ ATA), all owned by the Token Program | `stdlib-mapping.md` § ERC-20 |
| `transferFrom` | `transfer` with delegate (single delegate per account, not per-pair allowance) | `stdlib-mapping.md` |
| `approve` | `token::approve` — sets delegate + `delegated_amount` | `stdlib-mapping.md` |
| ERC-721 / ERC-1155 | Metaplex Token Metadata / Core; per-NFT Mint with supply 1 | `stdlib-mapping.md` |
| `delegatecall` | No analog — use CPI with explicit account propagation | This file § The account-graph rule |
| `selfdestruct` | `#[account(mut, close = recipient)]` on the account | `pattern-mapping.md` |
| Ownable | Config PDA with `authority: Pubkey` + `has_one = authority` | `stdlib-mapping.md` |
| AccessControl | One PDA per (role, holder) pair, or fixed role fields on config | `stdlib-mapping.md` |
| ReentrancyGuard | Don't translate | `security/reentrancy.md` |
| SafeMath | `checked_*` arithmetic on every op | `security/arithmetic.md` |
| Pausable (token) | `freeze_authority` on the Mint (per-account freeze only) | `stdlib-mapping.md` |
| ERC-2612 permit | Drop — Solana txs already carry explicit signatures | `stdlib-mapping.md` |
