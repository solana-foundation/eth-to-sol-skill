# Explanation log: `02-naive-port.rs` → `03-optimized.rs`

One entry per change in `04-diff.md`, grouped by theme. Each entry follows the schema: **What / Why / Benefit / Tradeoff**.

This 4626 example teaches three Solana-specific lessons beyond the ERC-20 and staking-vault examples:

1. **Read aggregates from SPL Token, not from your own state.** `totalSupply` is `share_mint.supply`; `totalAssets` is `asset_reserve.amount`. The Solidity contract stored them; on Solana storing them is redundant *and* parallelism-poisoning.
2. **Rounding direction is a security property.** ERC-4626 mandates floor on deposit/redeem and ceil on mint/withdraw, both favoring the vault. The optimized version makes this explicit with a `Rounding` enum at every call site.
3. **Inflation-attack defense is preserved verbatim from OZ.** The virtual-offset pattern transfers cleanly; what changes is the surrounding arithmetic, which must be checked and bounds-guarded (silent truncation in a naive port would *degrade* the defense without warning).

---

## State model

### Replace `Vec<BalanceEntry>` shares with an SPL Token Mint (diff §S1)

- **What:** Removed `balances: Vec<BalanceEntry>` from `VaultState` (`02-naive-port.rs:482`). Shares are now an SPL Token Mint (`03-optimized.rs:675`), with each holder owning an SPL Token Account.
- **Why:** Same lesson as the ERC-20 example: per-user fungible-token balances belong in SPL Token accounts, not in a Vec inside the program's state. The 4626 vault is itself a token issuer, so the same architectural move applies to the share token.
- **Benefit:** Holder count unbounded. Per-holder writes parallelize (different holders' ATAs are disjoint accounts). The vault account itself shrinks dramatically (§R1).
- **Tradeoff:** First-time share recipients pay ATA rent (~0.002 SOL). Off-chain code reads share balances via SPL Token, not via a vault state field. Standard Solana UX.

### Delete `total_assets` / `total_supply`; read them from SPL Token (diff §S2)

- **What:** Removed both fields and their `+=`/`-=` mutations. Every conversion site reads `share_mint.supply` and `asset_reserve.amount` directly (`03-optimized.rs:91`–`92`, `:132`–`:133`, etc.).
- **Why:** SPL Token maintains both values atomically as side effects of `mint_to`/`burn`/`transfer`. Self-tracking duplicates audited code AND forces the vault to be writable on every deposit/withdraw — exactly the parallelism penalty §P1 is designed to remove.
- **Benefit:** Two sources of truth collapse into one. The vault can be read-only during all user-facing 4626 operations. No risk of `total_assets` and `asset_reserve.amount` diverging due to a missed update path.
- **Tradeoff:** One extra account passed at each call (the asset_reserve TokenAccount) so the program can read `.amount`. Anchor already needs it as `mut` for the CPI transfers, so the cost is zero — the account was going to be in the list anyway.

### Move share ERC-20 surface to SPL Token (diff §S3)

- **What:** Share transfer/approve are not vault instructions. Clients call SPL Token directly.
- **Why / Benefit / Tradeoff:** Identical to the ERC-20 example's §C3 reasoning, applied to vault shares. See **Frontend integration** below for the concrete client-side delta.

---

## Parallelism

### Vault is READ-ONLY on every user-facing 4626 operation (diff §P1)

- **What:** `Deposit` and `Withdraw` account structs omit `mut` on the vault (`03-optimized.rs:528`–`537`, `:571`–`:580`). Vault only mutates in admin instructions (`set_fee_bps`, etc.).
- **Why:** With `total_assets`/`total_supply` deleted (§S2) and balances moved to SPL Token (§S1), there is nothing left on the vault for deposit/withdraw to mutate. The vault stores only governance fields (fee_bps, authority, bumps), which are admin-rate, not transaction-rate.
- **Benefit:** Cross-user deposit/withdraw don't write-conflict on the vault account at all. Conflict is only at the inherent globals — `share_mint.supply` (every mint/burn) and `asset_reserve.amount` (every transfer in/out). Both are maintained by SPL Token; both are write-hot for any fungible-token system regardless of design. This is the *minimum* possible contention for a vault.
- **Tradeoff:** None. This is the most significant parallelism win across the three examples. The staking-vault example couldn't achieve this (Synthetix accumulator forces vault writes); the 4626 vault's pure conversion math has no such constraint.

### O(n) Vec scans eliminated (diff §P2)

- **What:** Removed `iter_mut().find()` loops for share balance lookup.
- **Why / Benefit / Tradeoff:** Identical to ERC-20 §P2 and staking-vault §P3.

---

## Security

### Every arithmetic op uses `checked_*`; one `mul_div_u128_to_u64` helper centralizes the 4626 math (diff §Sec1)

- **What:** Replaced bare `*`/`/`/`+`/`-` and `as u64` casts with a single helper (`03-optimized.rs:399`–`418`) that does checked multiplication, checked division, checked ceiling-addition, and a bounds-guarded narrowing.
- **Why:** ERC-4626 conversion math is the single most security-sensitive code in a vault. Solidity 0.8+ checks; Rust release builds wrap silently. A silent overflow in `mul_div` doesn't fail — it returns a wrong number that the depositor accepts. The naive port had silent truncation on every preview (`02-naive-port.rs:317`, `:324`, `:332`, `:339`), silent overflow on the ceiling addition (`:331`–`:332`, `:339`), and silent wrap on the balance updates (`:114`, etc.). Each could individually be exploited.
- **Benefit:** Every arithmetic failure is now a typed error (`Overflow`, `DivByZero`). Centralized helper means one site to audit. Inflation-defense math (which adds `VIRTUAL_SHARES_OFFSET` to numerator) is checked end-to-end.
- **Tradeoff:** Verbose helper body. Acceptable — it's the highest-stakes 30 lines in the program.

### Explicit `Rounding` enum at every conversion site (diff §Sec2)

- **What:** Added `Rounding { Down, Up }` (`03-optimized.rs:46`–`50`). Every preview call passes direction explicitly. Helper signatures take `rounding: Rounding` and behave accordingly.
- **Why:** ERC-4626 rounding direction is part of the spec, not an implementation detail. Deposit rounds shares DOWN (depositor gets at most their fair share); mint rounds assets UP (depositor pays at least their fair share); withdraw rounds shares UP (withdrawer burns at least their fair share); redeem rounds assets DOWN (withdrawer gets at most their fair share). Every direction favors the vault — preventing dust-extraction attacks where a delegate-withdrawer repeatedly redeems 1 wei to drain rounding errors. The naive port computed direction implicitly per-function; a refactor that consolidates the helpers risks flipping a direction silently.
- **Benefit:** Direction is a one-line check at each call site. Audit takes 4 lines: `Rounding::Down` on deposit/redeem; `Rounding::Up` on mint/withdraw. Wrong direction is a code review failure, not a silent bug.
- **Tradeoff:** Helper signature is longer. Worth it.

### PDA bumps cached + canonicalization enforced (diff §Sec3)

- **What / Why / Benefit / Tradeoff:** See `examples/erc20-token` §Sec2. Same pattern applied.

### Vault-authority signing scoped to the vault key (diff §Sec4)

- **What:** `seeds = [b"vault_authority", vault.key().as_ref()]` (per-vault) instead of singleton.
- **Why:** Future-proofs the program for multiple vaults (one per asset_mint). A singleton authority across vaults means a bug in one vault's withdrawal flow could move funds in any other.
- **Benefit:** Per-vault authority isolation. Each vault's funds are signed for by a distinct PDA.
- **Tradeoff:** Seeds are longer. Negligible.

### Classic SPL Token only — Token-2022 transfer hooks rejected at the type level (diff §Sec5)

- **What:** Both ports use `anchor_spl::token::{Mint, TokenAccount}`. Anchor's typed-account check verifies the account's owning program is the classic SPL Token program ID. Token-2022 mints fail the typed-account check and the instruction reverts.
- **Why:** Token-2022's `TransferHook` extension lets the mint specify an arbitrary program that runs on every transfer. If the vault's underlying is Token-2022 with such a hook, every `spl_token::transfer` CPI from the vault can call back into the vault — classic cross-program reentrancy. Allowing this requires either an explicit reentrancy state machine in the vault, or an allowlist of "safe" Token-2022 extensions. Both add real complexity. For an example, the right answer is to reject Token-2022 entirely and document the choice.
- **Benefit:** One entire class of attacks (transfer-hook reentrancy) is structurally impossible. No defensive code required in the vault body.
- **Tradeoff:** Vaults cannot accept Token-2022 underlyings without a redesign. Documented in `DECISIONS.md` and noted in the `Initialize` doc comment. A production deployment may need both flavors — at which point the vault should be split into two programs (one for classic SPL, one for Token-2022) so the security stance is per-program, not per-vault.

### ERC-4626 inflation-attack defense preserved (diff §Sec6)

- **What:** Both ports include the virtual-offset terms in every conversion. The optimized version's checked math means the defense cannot be silently degraded by an arithmetic bug.
- **Why:** The canonical 4626 attack: attacker stakes 1 wei before any honest user, gets 1 share. Donates a large amount of underlying directly to the vault. New `totalAssets` is huge, `totalSupply` is 1. Honest user's deposit of N wei converts to `N * 1 / huge ≈ 0` shares — they got 0 shares for their deposit. Attacker now owns ~100% of shares and can drain everything.
- **OZ's mitigation:** The virtual offset (`virtual_shares = 10^DECIMALS_OFFSET = 10^6` in this example, `virtual_assets = 1`) adds dilution. The attacker's 1 wei stake becomes `1 * (0 + 10^6) / (0 + 1) = 10^6` virtual shares. A subsequent donation of `D` assets is met by the conversion `N * (10^6 + 10^6) / (D + 1)`, which is non-zero unless `D` is enormous relative to `N`.

  **Numeric demonstration of bounded loss with virtual offset:**

  | Step | totalSupply | totalAssets | Note |
  |---|---|---|---|
  | Attacker deposits 1 asset | 10^6 shares | 1 | First deposit; attacker gets 10^6 shares due to offset |
  | Attacker donates 10^9 directly to reserve | 10^6 shares | 10^9 + 1 | Donation skews price |
  | Honest deposits 10^6 assets | 10^6 + 2000 shares | 10^9 + 10^6 + 1 | Honest user gets ~2000 shares, *not* zero |
  | Honest immediately redeems 2000 shares | ... | ... | Receives ~10^6 assets back (rounding loss minimal) |

  The defense doesn't make the attack profitable for the honest user, but it bounds the loss to dust. Without the offset, the honest user would receive 0 shares for 10^6 assets — full loss.

- **Benefit:** Standard 4626 attack class is mitigated. The defense is preserved exactly because it's already correct in Solidity — translation is mechanical.
- **Tradeoff:** Share decimals = asset decimals + 6, which means the share token has 6 more decimals than the underlying. UX should display whole shares (`amount / 10^share_decimals`) rather than raw `amount`. Standard Solana token UX.

  **Defense regression test that should catch a future bug:** at `total_supply = 0, total_assets = 0`, depositing 1 asset must yield ≥ `10^6` shares (the virtual_shares value). If a future code change makes the first depositor get 1 share or fewer, the defense is broken. A unit test asserting `convert_to_shares(1, 0, 0, Rounding::Down) >= 1_000_000` would catch this — see `mul_div_u128_to_u64` callsite in `convert_to_shares`.

### `has_one` cross-validation (diff §Sec7)

- **What:** Added `has_one = asset_mint`, `has_one = share_mint`, `has_one = authority` constraints to relevant account structs.
- **Why:** Declarative constraints prevent the "future instruction trusts asset_mint without going through vault" failure mode. Anchor enforces all linked mints/authorities before the handler runs.
- **Benefit:** Audits become local — read the struct, see the constraints, done. No need to walk through handler bodies to verify cross-references.
- **Tradeoff:** None.

---

## CPI & program reuse

### Share mint/burn via SPL Token CPI (diff §C1)

- **What:** `token::mint_to` for share issuance (vault_authority PDA signs); `token::burn` for share destruction (user or delegate signs).
- **Why:** Same architectural move as ERC-20 example §C1 — use audited SPL Token for token mechanics. The vault retains ONLY the conversion math + governance gating; the token movements are SPL Token's responsibility.
- **Benefit:** No custom mint/burn arithmetic. Standard SPL Token wallet integration.
- **Tradeoff:** ~5k CU per CPI. Acceptable; the conversion math is the costly part, not the CPIs.

### Withdraw-by-delegate uses SPL Token's native delegate (diff §C2)

- **What:** `Withdraw` accepts `signer: Signer<'info>` (no `token::authority` constraint on the share ATA). SPL Token's `burn` accepts the signer if it's either the ATA's owner or its registered delegate (with sufficient delegated_amount).
- **Why:** Solidity's `withdraw(assets, receiver, owner_)` lets a delegate spend the owner's allowance. SPL Token has this primitive built-in (`spl_token::approve` sets a single delegate per ATA). Reimplementing an allowance map in the vault would duplicate SPL Token and introduce a custom code path. The right answer is to lean on SPL Token's delegate.
- **Benefit:** No allowance map. Standard SPL Token approve/transferFrom-style flows work without per-vault integration.
- **Tradeoff:** SPL Token allows one delegate per ATA, not a `(owner, spender)` map. Same gap as the ERC-20 example — uncommon to need >1 active delegate, but document if your protocol does.

### Aggregates read from SPL Token, not stored (diff §C3)

- **What:** Per-call reads of `share_mint.supply` and `asset_reserve.amount`.
- **Why:** Single source of truth. Eliminates the divergence-bug class where a missed `+=`/`-=` makes self-tracked totals drift from reality.
- **Benefit:** Code shrinks. Vault is read-only on user actions (§P1). Self-tracking would have undone both wins.
- **Tradeoff:** None.

---

## Compute & rent

### Vault size: ~4 KB → 132 bytes (diff §R1)

- **What:** `VaultState::SIZE = 4118` → `Vault::SIZE = 132`.
- **Why:** Removing the 100-entry Vec saves 4000 bytes; storing only governance + cached bumps takes ~130.
- **Benefit:** ~12× rent savings on the protocol-paid account. Vault deserialization is fast (small struct).
- **Tradeoff:** Each share holder pays ~0.002 SOL for their ATA. Standard.

### Per-call data load: ~30× smaller (diff §R2)

- **What:** ~4 KB Vec deserialization replaced by ~380 bytes of SPL Token + vault loads.
- **Why:** Anchor deserializes account data on instruction entry; Vec deserialization is O(n).
- **Benefit:** Multi-kCU savings per call.
- **Tradeoff:** None.

---

## Idioms

### Pure conversion helpers + Rounding enum (diff §I1)

- **What / Why / Benefit / Tradeoff:** See diff §Sec2. The structural and security wins are linked.

### Consolidated CPI helpers (diff §I2)

- **What:** `mint_shares` and `transfer_asset_out` factor the vault_authority-signed CPIs. Signer seeds constructed once.
- **Why:** Bug surface is at the signer-seed construction. Inline construction at every call site means N places to drift; one helper means one place.
- **Benefit:** Auditable seed construction. Easier to add new vault_authority-signed operations.
- **Tradeoff:** A few extra helper signatures. Trivial.

### Use Anchor's typed `Mint` / `TokenAccount` (diff §I3)

- **What:** Every SPL Token account in the program is `Account<'info, Mint>` or `Account<'info, TokenAccount>`. No raw `AccountInfo` for SPL accounts.
- **Why:** Typed wrappers perform owner-program check, discriminator-equivalent check (for SPL the layout itself is the discriminator), and field deserialization automatically. Skipping them invites type-confusion bugs.
- **Benefit:** Audit cost drops — the type system carries the validation.
- **Tradeoff:** None.

### Typed errors (diff §I4)

- **What:** Nine `VaultError` variants.
- **Why:** Every failure mode has a name. Off-chain consumers can match on error codes via the IDL.
- **Benefit:** Diagnostics, not printfs.
- **Tradeoff:** None.

---

## Frontend integration

This is the section a porting team should read first.

The optimized 4626 vault changes the client-side integration shape compared to a Solidity 4626. The change is **larger** than the ERC-20 example's because 4626 has more entry points and the share token also moves to SPL.

### Before (Solidity ERC-4626 + ethers/viem)

```ts
// Approve underlying
await asset.approve(vault.address, amount);

// Deposit
const sharesOut = await vault.deposit(amount, recipient);

// Read your share balance
const myShares = await vault.balanceOf(myAddress);

// Read total share supply, total assets
const ts = await vault.totalSupply();
const ta = await vault.totalAssets();

// Withdraw on behalf of someone
await vault.approve(delegate, sharesAmount);  // delegate gets share allowance
await vault.connect(delegate).withdraw(assets, receiver, owner);
```

### After (this program + @solana/spl-token + @solana/web3.js)

```ts
import {
    getAssociatedTokenAddressSync,
    createAssociatedTokenAccountIdempotentInstruction,
    createApproveInstruction,
} from "@solana/spl-token";
import { PublicKey, Transaction } from "@solana/web3.js";

// Derive the vault PDAs (deterministic from program ID + asset mint)
const [vaultPda] = PublicKey.findProgramAddressSync(
    [Buffer.from("vault"), assetMint.toBuffer()],
    program.programId
);
const [vaultAuthority] = PublicKey.findProgramAddressSync(
    [Buffer.from("vault_authority"), vaultPda.toBuffer()],
    program.programId
);
const [shareMint] = PublicKey.findProgramAddressSync(
    [Buffer.from("share_mint"), assetMint.toBuffer()],
    program.programId
);
const [assetReserve] = PublicKey.findProgramAddressSync(
    [Buffer.from("asset_reserve"), vaultPda.toBuffer()],
    program.programId
);

// User's ATAs
const myAssetAta = getAssociatedTokenAddressSync(assetMint, me.publicKey);
const myShareAta = getAssociatedTokenAddressSync(shareMint, me.publicKey);

// Deposit
const tx = new Transaction()
    // Ensure your share ATA exists (idempotent — no-op if present)
    .add(createAssociatedTokenAccountIdempotentInstruction(
        me.publicKey, myShareAta, me.publicKey, shareMint))
    // Call the vault's deposit instruction
    .add(await program.methods.deposit(amount).accounts({
        vault: vaultPda,
        assetMint,
        shareMint,
        vaultAuthority,
        assetReserve,
        userAssetAta: myAssetAta,
        receiverShareAta: myShareAta,
        user: me.publicKey,
        tokenProgram: TOKEN_PROGRAM_ID,
    }).instruction());

// You never explicitly "approve" the asset — the depositor signs the transaction
// that includes the token::transfer CPI authority. SPL Token transfers are signed
// per-instruction, not approved-then-pulled.

// Read share balance
const myShares = await connection.getTokenAccountBalance(myShareAta);

// Read total share supply / total assets (no vault method needed)
const mintInfo = await connection.getParsedAccountInfo(shareMint);
const reserveInfo = await connection.getParsedAccountInfo(assetReserve);
// .data.parsed.info.supply  /  .data.parsed.info.tokenAmount.amount

// Withdraw on behalf (delegate flow)
// 1. Owner pre-authorizes a delegate on their share ATA via SPL Token:
await sendAndConfirmTransaction(connection, new Transaction().add(
    createApproveInstruction(myShareAta, delegate.publicKey, me.publicKey, sharesAmount)
), [me]);

// 2. Delegate calls withdraw — they sign as `signer`; SPL Token verifies delegate auth.
await program.methods.withdraw(assetsAmount).accounts({
    vault: vaultPda,
    assetMint,
    shareMint,
    vaultAuthority,
    assetReserve,
    ownerShareAta: myShareAta,        // <- still the owner's ATA
    receiverAssetAta: ...,
    signer: delegate.publicKey,        // <- delegate signs
    tokenProgram: TOKEN_PROGRAM_ID,
}).signers([delegate]).rpc();
```

What changes for your team:

- **No upfront approve of the underlying.** Solana transactions sign for SPL Token transfers per-instruction; there is no persistent allowance to manage.
- **Reads happen against SPL Token accounts.** Balances → `getTokenAccountBalance(ata)`. Total supply → `mint.supply`. Total assets → `reserve.amount`. No `vault.balanceOf()` / `vault.totalSupply()`.
- **ATA derivation is mandatory** before any operation that returns/burns shares. Wallets and the `@solana/spl-token` helpers handle this; budget one extra instruction per "fresh" recipient.
- **Delegate (transferFrom-equivalent) is one approve on the share ATA**, not a per-(spender) allowance map. If your dApp expected multi-spender support, see the §C2 tradeoff.
- **Event indexing changes.** SPL Token emits its own logs for share movement (mint/burn). The vault emits `DepositEvent` / `WithdrawEvent` / `EarnEvent` via Anchor `#[event]` — parse via the Anchor IDL.

If your existing 4626 dApp's frontend assumes one contract call per operation, budget a sprint for the migration; the architecture is straightforward but every call site touches.
