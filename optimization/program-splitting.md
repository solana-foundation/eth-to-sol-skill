# When to split into multiple programs

Solidity codebases often combine multiple concerns in one contract (token + staking + governance + fees). On Solana, splitting these into independent programs that CPI each other is often the better shape — but not always.

## Reasons to split

1. **Different upgrade cadences.** If staking logic changes often but the token contract should be locked, separating them lets you upgrade staking without touching token authority.
2. **Different trust assumptions.** A core token program with a frozen upgrade authority + an experimental DeFi program with a multisig authority is a stronger structure than one program serving both roles.
3. **Reuse from other protocols.** If your staking design is generic, splitting lets other token issuers point their tokens at it.
4. **Independent failure domains.** A bug in governance shouldn't be able to mint tokens. Cross-program calls go through explicit CPI and explicit account passing — this is much easier to audit than internal calls.
5. **Account-size pressure.** Hitting realloc / size limits on a single state account often means you should have split.

## Reasons NOT to split

1. **CPI overhead is real.** Each CPI burns ~1–3k CU minimum, plus the inner program's CU. A 3-hop CPI chain can run out of budget. (1.4M cap, ~1000-CU stack overhead per hop, max 4 hops including the entry).
2. **Account-passing complexity.** Each cross-program call needs the inner program's accounts in the outer transaction. Long signature chains are hard to maintain.
3. **Signer authority leaks.** If program A CPIs into B with B trusting A's signature, audit the trust carefully — see `security/cpi-safety.md`.

## The pattern that works: thin program + audited dependencies

For an ERC-20 port:

- **Your program** (~100 lines): governance, config, mint-authority gating, max supply enforcement.
- **SPL Token** (audited, deployed): the actual mint, transfer, burn, approve mechanics.

You CPI into SPL Token. You do not reimplement it. This is the same pattern as "inheriting from OpenZeppelin's ERC20" except the dependency is a runtime program, not source-level inheritance, and the dependency is audited at the binary level rather than via source review.

This pattern generalizes:

- **DEX**: thin pool program + SPL Token (asset transfers) + SPL Token (LP token mint).
- **NFT mint**: thin mint program + Metaplex Token Metadata + Metaplex Bubblegum (compression) if applicable.
- **Lending**: thin position program + SPL Token (collateral & debt accounting) + oracle program (Pyth/Switchboard).

## When one program is the right answer

- The contract is genuinely standalone and has no Solana-ecosystem dependency.
- The state mutations form a single atomic unit that can't be split.
- The contract is small (< ~500 lines) and the split adds more complexity than it removes.

If unsure, **start with one program**. Split only when a specific pain (upgrade cadence, audit pressure, CU exhaustion) makes the split obviously profitable.

## Cross-program account ownership

A program can only mutate accounts it *owns*. If program A wants program B to update an account, A passes the account to B (writable) and B mutates it — but B must be the account's owner, not A. Cross-program state updates always work through this ownership chain:

- If both A and B might mutate, the account is owned by whichever does the actual mutation; the other reads.
- If two programs both need to "own" the same data, you have a design problem — split the data, or merge the programs.

## Account ownership transfer

Programs can transfer ownership of an account they own:

```rust
account.to_account_info().assign(&new_program_id);
```

Rare; almost always a sign of an over-clever design. Most "transfer ownership" patterns from EVM translate to changing an `authority: Pubkey` field on a program-owned account, not changing the account's owner program.

## Anti-pattern: monolithic program with feature flags

```solidity
contract Mega {
    function token_transfer(...) external { ... }
    function stake_deposit(...) external { ... }
    function gov_vote(...) external { ... }
    function fee_claim(...) external { ... }
}
```

Translating this verbatim to one Anchor program with many instructions is *fine* and standard practice. Solidity-style contract splitting (one contract per concern) is not the EVM convention; one mega-contract is. On Solana the same one-program-with-many-instructions is standard, *and* the SPL/Metaplex split-out of token mechanics is also standard. These coexist.

Split when there's an external reason (audit, upgrade, reuse), not because Solidity would have used multiple contracts.
