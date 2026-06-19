## Structured diff: 02-naive-port.rs → 03-optimized.rs

Each section names one meaningful change. Snippets are abridged; line references point at the canonical site of each change.

---

## State model

### S1. `Vec<Contribution>` inside `Fundraiser` → per-supporter `Contributor` PDA

Naive (`02-naive-port.rs:222`–`231`):

```rust
#[account]
#[derive(InitSpace)]
pub struct Fundraiser {
    // ...
    // SMELL: Vec inside a single state account = write-hot, capped, O(n) lookup.
    #[max_len(MAX_CONTRIBUTORS)]
    pub contributors: Vec<Contribution>,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, InitSpace)]
pub struct Contribution {
    pub who: Pubkey,
    pub amount: u64,
}
```

Optimized (`03-optimized.rs:245`–`254`):

```rust
#[account]
#[derive(InitSpace)]
pub struct Contributor {
    pub amount: u64,
}

// Seeds: [b"contributor", fundraiser.key().as_ref(), supporter.key().as_ref()]
```

The `Contribution` row is now its own PDA, derived from `(fundraiser, supporter)`. No `MAX_CONTRIBUTORS` cap. No O(n) scan in `refund()`.

### S2. `Vec` lookup → PDA derivation

Naive `contribute` (`02-naive-port.rs:60`–`78`):

```rust
if let Some(c) = f.contributors.iter_mut().find(|c| c.who == supporter_key) {
    c.amount = c.amount.checked_add(amount).ok_or(FundraiserError::Overflow)?;
} else {
    require!(
        f.contributors.len() < MAX_CONTRIBUTORS,
        FundraiserError::TooManyContributors
    );
    f.contributors.push(Contribution { who: supporter_key, amount });
}
```

Optimized `contribute` (`03-optimized.rs:62`–`68`):

```rust
let c = &mut ctx.accounts.contributor;
c.amount = c.amount
    .checked_add(amount)
    .ok_or(FundraiserError::Overflow)?;
```

The `init_if_needed` constraint on the `contributor` account does the "create-or-find" branch at account validation time. The handler body just adds.

### S3. Refund replay protection via account close

Naive `refund` (`02-naive-port.rs:120`–`135`):

```rust
let idx = f.contributors.iter().position(|c| c.who == supporter_key)
    .ok_or(FundraiserError::NothingToRefund)?;
let amount = f.contributors[idx].amount;
require!(amount > 0, FundraiserError::NothingToRefund);
f.contributors[idx].amount = 0; // mark refunded in-place
```

Optimized `refund` (`03-optimized.rs:228`–`236`):

```rust
#[account(
    mut,
    close = supporter,
    seeds = [CONTRIBUTOR_SEED, fundraiser.key().as_ref(), supporter.key().as_ref()],
    bump,
)]
pub contributor: Account<'info, Contributor>,
```

`close = supporter` tears down the PDA and returns its rent to the supporter on success. A second `refund()` call for the same supporter fails at account validation (PDA doesn't exist) — not at a runtime `amount > 0` check that depends on the row not being mutated by something else.

---

## Security

### A1. `claim()` authorization via `has_one` + seed-derived signer

Naive (`02-naive-port.rs:82`–`87`):

```rust
require!(
    f.creator == ctx.accounts.creator.key(),
    FundraiserError::NotCreator
);
```

Optimized (`03-optimized.rs:194`–`200`):

```rust
#[account(
    mut,
    seeds = [FUNDRAISER_SEED, creator.key().as_ref()],
    bump = fundraiser.bump,
    has_one = creator,
)]
pub fundraiser: Account<'info, Fundraiser>,
```

Two layers of protection enforced by Anchor before the handler runs:

1. The fundraiser PDA's seeds include `creator.key()` — only the actual creator can re-derive the same address.
2. `has_one = creator` re-checks that `fundraiser.creator == creator.key()` on every call.

The runtime `require!` was redundant. Moving the check up the stack means a wrong signer fails account validation, not partway through the instruction.

### A2. Invariant-first state update in `refund`

Naive (`02-naive-port.rs:131`–`134`):

```rust
f.contributors[idx].amount = 0;
f.total_raised = f.total_raised
    .checked_sub(amount)
    .ok_or(FundraiserError::Overflow)?;
// CPI fires AFTER both state writes
```

Optimized (`03-optimized.rs:106`–`110`):

```rust
ctx.accounts.fundraiser.total_raised = ctx.accounts.fundraiser
    .total_raised
    .checked_sub(amount)
    .ok_or(FundraiserError::Overflow)?;
// state decrements first; then CPI; PDA close happens at instruction exit
```

The naive ordering is also safe (there are no re-entrancy paths in Anchor), but the optimized version sticks to "update invariants before external calls" so the on-chain state matches expectation even if the CPI fails mid-transaction.

---

## CPI & program reuse

### C1. Authority signer seeds derived once

Naive `refund` and `claim` re-build the seeds in each handler (`02-naive-port.rs:96`–`98`, `144`–`146`):

```rust
let creator_key = f.creator;
let bump = f.bump;
let signer_seeds: &[&[&[u8]]] =
    &[&[FUNDRAISER_SEED, creator_key.as_ref(), &[bump]]];
```

Optimized version uses the same pattern (no functional change), but consolidates the seed constants at the top of the module (`03-optimized.rs:14`–`16`):

```rust
const FUNDRAISER_SEED: &[u8] = b"fundraiser";
const VAULT_SEED: &[u8] = b"vault";
const CONTRIBUTOR_SEED: &[u8] = b"contributor";
```

A typo'd seed in one of the seed arrays is the most common silent-failure source on PDA-heavy programs. One constant per seed name eliminates the divergence risk.

---

## Compute & rent

### R1. `MAX_CONTRIBUTORS` cap removed

Naive (`02-naive-port.rs:17`):

```rust
const MAX_CONTRIBUTORS: usize = 50; // hard cap so the Vec fits in one account
```

Optimized: removed. The `Fundraiser` account is a fixed size (no Vec) and each contributor is its own account, so the program scales to an unbounded number of supporters. Rent for each `Contributor` PDA is paid by the supporter on first `contribute()` and refunded to them on `refund()` (via `close = supporter`).

### R2. `Fundraiser` account size, contributors-bearing → fixed

Naive `Fundraiser::INIT_SPACE` (derived) ≈ `32 + 32 + 8 + 8 + 8 + 1 + 1 + (4 + 50 * (32 + 8))` = `2094 bytes`.

Optimized `Fundraiser::INIT_SPACE` = `32 + 32 + 8 + 8 + 8 + 1 + 1` = `90 bytes`.

23× smaller, allocated once at `initialize()`. The savings move into per-supporter `Contributor` PDAs (`8 + 8 = 16` bytes each) paid for by the supporter, not the creator.

---

## Idioms

### I1. `init_if_needed` for the contributor PDA

Optimized `contribute` `Contributor` account (`03-optimized.rs:182`–`189`):

```rust
#[account(
    init_if_needed,
    payer = supporter,
    space = 8 + Contributor::INIT_SPACE,
    seeds = [
        CONTRIBUTOR_SEED,
        fundraiser.key().as_ref(),
        supporter.key().as_ref(),
    ],
    bump,
)]
pub contributor: Account<'info, Contributor>,
```

`init_if_needed` is the cleanest way to say "first call creates, subsequent calls top up" — the equivalent of Solidity's `mapping(address => uint256) += amount` idiom. Requires `features = ["init-if-needed"]` in `Cargo.toml`.

---

## Client/API integration notes

No client/API change is needed beyond the canonical `tokens/token-fundraiser` shape from solana-developers/program-examples, but consumers of the program should know:

- The `Contributor` PDA address is deterministic: `[b"contributor", fundraiserPda, supporter.publicKey]`. Refund flows derive it client-side and pass it in.
- `refund()` will close the supporter's contributor PDA. If a client needs the pre-refund contribution data, read it once, then refund — don't fetch it post-refund.
- The fundraiser's vault TokenAccount address is `[b"vault", fundraiserPda]` — same shape as the canonical `program-examples/tokens/token-fundraiser` so existing tooling works unchanged.
