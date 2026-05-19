# Reentrancy — Solana edition

Solana's account-locking model blocks the *classic* EVM reentrancy attack automatically. It does not block every reentrancy hazard. EVM developers tend to assume one of two extremes (either "Solana has no reentrancy, free pass" or "Solana works just like EVM, install a ReentrancyGuard equivalent") — both are wrong.

## What the EVM bug looks like

```solidity
function withdraw() external {
    uint256 bal = balances[msg.sender];
    msg.sender.call{value: bal}("");        // attacker recurses here
    balances[msg.sender] = 0;               // too late: state mutated after external call
}
```

The attacker's fallback calls `withdraw` again before the first call clears the balance. Drains the contract.

## Why Solana doesn't have *this* bug

Two reasons:

1. **Explicit account locking.** Every account that an instruction will write is declared up front. The runtime serializes any transaction that touches the same writable account. The attacker's "re-entered" instruction would need to claim the same account writable a second time — which would conflict with the outer instruction's lock and fail.
2. **No implicit value transfer.** EVM's `msg.sender.call{value: bal}("")` hands control to an arbitrary fallback. Solana has no analog — every value movement is an explicit CPI to a specific program. There is no "untrusted contract receives ETH" hook.

For a program that mutates only its own state and CPIs only into trusted programs (SPL Token, System Program), classic reentrancy is structurally impossible. Don't port `ReentrancyGuard`.

## What can still go wrong

### Cross-program reentrancy

If your program CPIs into program B, and B is malicious (or buggy), B can CPI back into your program before your outer instruction completes. The accounts that B re-passes to you have already had their write locks taken by your outer instruction — but reads happen at instruction entry. So:

```rust
// outer instruction starts. ctx.accounts.config snapshot: { authority: 0xABC }
let config = &mut ctx.accounts.config;
config.authority = new_authority;  // in-memory mutation, not yet flushed

token::transfer(cpi_ctx, amount)?;   // CPI into SPL Token

// SPL Token is trusted; this is fine. But if we CPI'd into an untrusted program here,
// it could re-call our program. Our re-entry instruction would deserialize `config` from
// the on-chain state — which still shows the OLD authority (the mutation above hasn't been
// flushed back to account storage yet, because that happens at the end of the instruction).
// The re-entered call sees stale state and might make decisions on it.
```

Anchor flushes account mutations at the end of the instruction, not after every assignment. A re-entered call sees the on-chain state, which is the pre-instruction snapshot.

**Defense:** don't CPI into untrusted programs while your in-memory account state diverges from on-chain state. Either commit (close the instruction and re-enter via a follow-up tx) or only CPI into known-safe programs.

### Read-after-CPI staleness

The complementary bug:

```rust
let supply_before = ctx.accounts.mint.supply;
token::mint_to(cpi_ctx, amount)?;
// ctx.accounts.mint.supply is STILL supply_before — not yet reloaded
require!(
    ctx.accounts.mint.supply == supply_before + amount,   // FALSE — bypass
    MyError::Invariant
);
```

Anchor accounts are deserialized once at instruction entry. CPIs that modify them on-chain do not update the in-memory copy. Use `.reload()` to refresh:

```rust
token::mint_to(cpi_ctx, amount)?;
ctx.accounts.mint.reload()?;
require_eq!(ctx.accounts.mint.supply, supply_before.checked_add(amount).ok_or(...)?);
```

This is not strictly reentrancy, but it lives in the same mental space — "what does my Anchor account reflect after a CPI?"

### Composability hazards

If your program exposes a `process_callback` instruction that other programs can call, you've created a re-entry surface. Any program that you CPI into can call back into `process_callback` and run your logic with whatever account set they assemble. Defense:

- Restrict the caller: check `ctx.accounts.invoker.key() == ctx.accounts.config.expected_invoker.key()`.
- Or don't expose callback instructions at all. Most ported EVM contracts don't need them.

## The discipline

For each instruction that CPIs out:

1. List the CPI targets. Are all of them trusted? (SPL Token, System Program, Metaplex, your own programs are typically yes. Arbitrary user-supplied programs are no — see `security/cpi-safety.md` on allowlisting.)
2. After every CPI, identify which Anchor accounts may have changed. `.reload()` them before using their state.
3. Order operations as: **validate, mutate own state, then CPI**. Mutations come *before* untrusted external calls. This is the same "checks-effects-interactions" pattern from EVM, applied for slightly different reasons. The reason it still works: by the time the CPI runs, your account is already mutated in memory and the values you guard on are committed to the local view.

## Should you port `ReentrancyGuard`?

No. It would do nothing — Solana's lock model prevents the same-account reentrancy it was designed for. Adding it is cargo-cult security and confuses readers. If you find yourself worrying about reentrancy, you usually want one of:

- A CPI allowlist (`security/cpi-safety.md`).
- Account `.reload()` discipline after CPIs.
- "Mutate-before-CPI" ordering of operations.

None of those are guard patterns; they're state-flow patterns.
