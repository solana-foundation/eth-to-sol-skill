# Rent and account sizing

Solana accounts are billed for the storage they occupy. The pricing model is **rent**, but the practical model is **rent exemption**: every account holds a minimum SOL balance proportional to its size, and as long as it does, it never pays ongoing rent. Drain it below the minimum, the runtime garbage-collects it.

In practice, every account is created rent-exempt. "Rent" really means "size-dependent SOL deposit refunded when the account is closed."

## Sizing an account

Anchor account `space`:

```rust
space = 8 + <sum of field sizes>
```

The 8 bytes are the discriminator Anchor prepends. Field sizes:

| Type | Bytes |
|---|---|
| `bool` | 1 |
| `u8` / `i8` | 1 |
| `u16` / `i16` | 2 |
| `u32` / `i32` | 4 |
| `u64` / `i64` | 8 |
| `u128` / `i128` | 16 |
| `Pubkey` | 32 |
| `Option<T>` | 1 + size(T) (1-byte tag + payload, payload always allocated) |
| `String` | 4 + max_byte_length (4-byte length prefix + max content) |
| `Vec<T>` | 4 + max_len × size(T) |
| `[T; N]` fixed | N × size(T) |
| Nested struct | sum of fields (no discriminator) |

**Strings and Vecs require a max bound.** There is no "grow as you go" — the account is allocated with a fixed size at init.

## Rent-exempt minimum

`Rent::default().minimum_balance(space)` returns the SOL (in lamports) required for an account of size `space` to be rent-exempt. Approximate values:

| Account size | Min balance |
|---|---|
| 8 bytes (empty discriminator) | ~890,000 lamports (~0.0009 SOL) |
| 100 bytes | ~1.6M lamports (~0.0016 SOL) |
| 1 KB | ~7M lamports (~0.007 SOL) |
| 10 KB | ~70M lamports (~0.07 SOL) |

Per-user PDAs (50–200 bytes) usually cost 0.001–0.002 SOL. Config PDAs and large state accounts vary widely; size carefully.

## Who pays

The `payer = ...` field on an `init` constraint names the signer who funds the new account's rent-exempt deposit. Common patterns:

- **User-funded per-user state**: `payer = user` — each user pays for their own balance/position account. Refund on close.
- **Protocol-funded shared state**: `payer = authority` — the protocol deployer funds the singleton config account.
- **Protocol-funded user state** (gas-sponsor style): `payer = treasury` where treasury is a program-owned PDA. Common for onboarding flows that hide the rent cost from users.

## Reallocation

Growing or shrinking an account post-init:

```rust
#[account(
    mut,
    realloc = 8 + NewSize::SIZE,
    realloc::payer = payer,
    realloc::zero = false,
)]
pub state: Account<'info, State>,
#[account(mut)]
pub payer: Signer<'info>,
pub system_program: Program<'info, System>,
```

- `realloc = N`: new size in bytes.
- `realloc::payer`: funds the rent delta (must be a `Signer` and `mut`).
- `realloc::zero = false`: keep existing data, append zeroes. `true` zeroes the whole buffer.

Hard limits:
- Max account size: 10 MiB.
- Max single-instruction realloc delta: 10 KiB (grow or shrink per call).
- Anchor's `realloc` constraint enforces these; for larger growth, multiple calls.

## Closing an account

```rust
#[account(mut, close = recipient)]
pub stale: Account<'info, State>,
```

Marks the account zeroed and rent-refunded to `recipient`. The runtime garbage-collects after the transaction. Useful for ephemeral state (escrows, one-shot orders).

A common bug: closing without zeroing the discriminator. Anchor's `close` constraint zeroes correctly. If you must close manually:

```rust
account.to_account_info().assign(&System::id());
account.to_account_info().realloc(0, false)?;
**recipient.try_borrow_mut_lamports()? += **account.try_borrow_lamports()?;
**account.try_borrow_mut_lamports()? = 0;
```

But almost always prefer Anchor's `close = ...`.

## Sizing pitfalls when porting from EVM

1. **`mapping(...)` translated to `Vec`** — sized at init, hits the cap forever. Fix: PDA per key.
2. **Translated `string` fields** with no length bound — Solidity allows unbounded strings; Anchor needs a cap. Pick `String` max-length explicitly. For names/symbols, 32 bytes is plenty; for URIs, 200 bytes is plenty. Anything longer belongs off-chain.
3. **Bumps not stored** — wasted 1 byte per PDA but more importantly, recomputation cost on every access. Always store the bump.
4. **Discriminator forgotten** — Anchor's runtime errors are clear; this still trips people on day one.
5. **`Option<Pubkey>`** is 33 bytes, not 32 — the 1-byte tag costs.

## Token-2022 and rent

`spl-token-2022` Mints with extensions cost more rent than classic SPL because each enabled extension adds bytes. For a Mint with transfer fee + metadata pointer + permanent delegate, you're looking at ~0.005 SOL rent for the Mint itself. Budget accordingly when scoping migrations.
