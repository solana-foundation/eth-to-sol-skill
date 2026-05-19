# Arithmetic

Solidity 0.8+ checks arithmetic by default; pre-0.8 needed `SafeMath`. Rust on Solana is the *opposite* of 0.8+ — it **wraps silently in release builds** unless you use checked operations. Always use them.

## The rule

For every operation on a value that is, or can be derived from, user input:

```rust
let total = a.checked_add(b).ok_or(MyError::Overflow)?;
let diff  = a.checked_sub(b).ok_or(MyError::Underflow)?;
let prod  = a.checked_mul(b).ok_or(MyError::Overflow)?;
let quot  = a.checked_div(b).ok_or(MyError::DivByZero)?;  // also handles div by zero
let rem   = a.checked_rem(b).ok_or(MyError::DivByZero)?;
```

`checked_*` returns `Option<T>`: `None` on overflow/underflow/div-zero, `Some(value)` otherwise. The `.ok_or(...)?` pattern converts to a typed error and short-circuits the instruction.

## When `saturating_*` or `wrapping_*` is acceptable

Rare. Document inline if used:

- **`saturating_add` / `saturating_sub`**: clamps to type bounds. OK for counters that have a natural ceiling (e.g. `attempt_count.saturating_add(1)` where you never actually expect to hit u64::MAX). Comment why.
- **`wrapping_add` / `wrapping_sub`**: explicit modular arithmetic. OK for hash mixing or cryptographic primitives. Never OK for amounts.

A bare `+` or `-` on `u64` is a code smell. Pre-merge: grep the codebase for `[^\w\.](\+|\-|\*|\/)\s*[a-z]` in `.rs` files and audit each hit.

## Common porting bugs

### Translated Solidity `+=` to Rust `+=`

```solidity
balanceOf[to] += amount;  // checked in Solidity 0.8+
```

→ naively:

```rust
to_balance.amount += amount;  // SMELL: not checked
```

→ correctly:

```rust
to_balance.amount = to_balance.amount.checked_add(amount).ok_or(MyError::Overflow)?;
```

### Mixed-width arithmetic

When porting `uint256` math to `u64`, watch for intermediate overflow:

```solidity
uint256 fee = (amount * feeBps) / 10000;
```

If `amount` is `u64` and `fee_bps` is `u16`, the product overflows in `u64` for `amount ≥ ~2^48 / fee_bps`. Widen:

```rust
let fee = (amount as u128)
    .checked_mul(fee_bps as u128)
    .ok_or(MyError::Overflow)?
    .checked_div(10_000)
    .ok_or(MyError::DivByZero)?
    as u64;
```

The `as u64` at the end is a silent truncation — guard it:

```rust
let fee_u128 = (amount as u128).checked_mul(fee_bps as u128).ok_or(...)? / 10_000;
require!(fee_u128 <= u64::MAX as u128, MyError::Overflow);
let fee = fee_u128 as u64;
```

### Signed/unsigned mismatches

`i64` for timestamps is canonical Solana. If you subtract two timestamps, the result is a duration — could be negative if you got the order wrong. Use `checked_sub` and require positivity:

```rust
let elapsed = now.checked_sub(last_update).ok_or(MyError::ClockSkew)?;
require!(elapsed >= 0, MyError::ClockSkew);
```

### Negative time-delta → unsigned-cast pitfall (Solana-specific)

A common DeFi pattern is `dt = now - last_update`, cast to `u128` for use in a multiplication. The bug:

```rust
let dt = (now - vault.last_update_time) as u128; // SMELL
```

If `now < last_update_time` (rare but observed on cluster reconfigs and historical sysvar quirks), the bare `i64` subtraction is negative. `as u128` keeps the bit pattern: a negative `i64` becomes a value near `u128::MAX`. The downstream `dt * rate` overflows, or the "duration" is used directly as a multi-millennium time-span and credits astronomical rewards. There's no Ethereum analog — EVM's block.timestamp is monotonic at the protocol level.

Two-layer defense:

```rust
let dt: i64 = now.checked_sub(vault.last_update_time)
    .ok_or(MyError::ClockSkew)?;
require!(dt >= 0, MyError::ClockSkew);
let dt_u128 = dt as u128; // safe — known non-negative
```

Cheap; eliminates an entire class of silent reward-minting bugs.

### Width-narrowing casts (`u128 → u64`) must be bounds-checked

Every `as <smaller-int>` on user-derived data must either be preceded by a range check or use `try_into()` with explicit error mapping. The cast itself truncates silently, so a wrong number returns "success" instead of failing.

```rust
// Wrong — silent truncation:
let shares = mul_div_u128(assets as u128, ..., ...) as u64;

// Right — explicit bounds check:
let shares_u128 = mul_div_u128(assets as u128, ..., ...)?;
require!(shares_u128 <= u64::MAX as u128, MyError::Overflow);
let shares = shares_u128 as u64;

// Also right — try_into:
let shares: u64 = shares_u128.try_into().map_err(|_| MyError::Overflow)?;
```

Grep `\b\w+ as u(8|16|32|64)\b` over your code; every hit on a non-const expression should be reviewable as bounds-safe. This is a top-N source of overflow bugs in ported DeFi code.

### Division-by-zero in fee math

```rust
let share = numerator.checked_div(total_supply).ok_or(MyError::DivByZero)?;
```

`checked_div` already maps `b == 0` to `None` — `ok_or` converts that to the error. Good. Never write `numerator / total_supply` bare.

### Rounding direction is part of the API surface

When converting between two units via `mul_div` (shares↔assets, USDC↔collateral, base↔quote in AMMs), rounding direction is **part of the spec**, not an implementation detail. Wrong direction is exploitable — a delegate that calls `redeem(1, ...)` repeatedly to drain dust if direction rounds in the user's favor.

Integer division truncates toward zero. For fee math, this typically rounds *in the user's favor*. For collateral math (debt calculations), this rounds *against the protocol's safety*. Audit each division for the rounding direction you want.

**Discipline pattern — explicit Rounding enum + single helper:**

```rust
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Rounding { Down, Up }

fn mul_div_u128_to_u64(a: u128, b: u128, c: u128, rounding: Rounding) -> Result<u64> {
    require!(c > 0, MyError::DivByZero);
    let product = a.checked_mul(b).ok_or(MyError::Overflow)?;
    let result_u128 = match rounding {
        Rounding::Down => product.checked_div(c).ok_or(MyError::DivByZero)?,
        Rounding::Up => {
            // ceil(product / c) = (product + c - 1) / c, with all adds checked
            let c_minus_one = c.checked_sub(1).ok_or(MyError::Overflow)?;
            let raised = product.checked_add(c_minus_one).ok_or(MyError::Overflow)?;
            raised.checked_div(c).ok_or(MyError::DivByZero)?
        }
    };
    require!(result_u128 <= u64::MAX as u128, MyError::Overflow);
    Ok(result_u128 as u64)
}
```

At every call site, name the direction:

```rust
let shares = mul_div_u128_to_u64(assets as u128, num, den, Rounding::Down)?;  // deposit
let assets = mul_div_u128_to_u64(shares as u128, num, den, Rounding::Up)?;     // mint
```

Audit becomes a one-line check at each site. Direction-flip via refactor becomes a code review failure rather than a silent bug.

**For ERC-4626 specifically:**

| Operation | Direction | Why |
|---|---|---|
| `deposit` (assets→shares) | DOWN | Depositor gets at most their fair share — favors vault |
| `mint` (shares→assets) | UP | Depositor pays at least their fair share — favors vault |
| `withdraw` (assets→shares) | UP | Withdrawer burns at least their fair share — favors vault |
| `redeem` (shares→assets) | DOWN | Withdrawer gets at most their fair share — favors vault |

All four favor the vault. This is in the spec — wrong direction is a CVE.

### Fixed-point intuitions break

Solidity DeFi often uses 18-decimal "wad" math (`1e18 = 1.0`). Translated to Solana with 9-decimal SOL tokens or 6-decimal stablecoins, the scaling factors change — and so do overflow margins. Re-derive the precision math; don't translate the constants.

Crates worth knowing:
- **`spl-math`** — checked fixed-point and rational arithmetic.
- **`solana-program::native_token`** — lamport ↔ SOL conversions.
- **`anchor-lang` / `anchor-spl`** — checked helpers for token amounts.

## Don't trust the type system alone

`u64` does not stop a bug; it just stops one kind. A correctly-typed program with `+` instead of `checked_add` is still vulnerable. The lint-level rule is: *every arithmetic op on user-derived data, every time, uses checked variants.*
