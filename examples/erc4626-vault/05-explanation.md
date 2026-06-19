# Explanation log: `02-naive-port.rs` → `03-optimized.rs`

One entry per change in `04-diff.md`, grouped by theme. Each entry follows the schema: **What / Why / Benefit / Tradeoff**.

This is the ERC-4626 tokenized vault — depositors hand the vault an underlying asset and receive share tokens that grow in value as the vault earns. The example translates a textbook Solidity 4626 into a Solana-native shape and teaches three Solana lessons beyond what the ERC-20 and escrow examples cover:

1. **Read aggregates from SPL Token, not from your own state.** Solidity's `totalSupply` and `totalAssets` are fields the contract maintains itself. On Solana the SPL Token program already keeps both (a Mint's `supply` is `totalSupply`; the reserve Token Account's `amount` is `totalAssets`). Storing them yourself duplicates audited code *and* forces the vault account to be writable on every deposit/withdraw — which kills the parallelism win you'd otherwise get.
2. **Rounding direction is a security property, not a style choice.** ERC-4626 mandates floor on deposit/redeem and ceil on mint/withdraw — all favoring the vault, so a delegate can't drain dust by repeated redemptions. The optimized version makes this explicit with a `Rounding` enum at every call site.
3. **Inflation-attack defense transfers verbatim from OpenZeppelin.** The virtual-offset pattern still works; what changes is that the surrounding arithmetic has to be checked end-to-end. Silent truncation in a naive port would *degrade* the defense without warning.

Vocabulary that comes up below, with EVM analogs:

- **SPL Token** — The single shared on-chain token program on Solana. Every fungible token is just configuration on this one program; nobody deploys their own ERC-20 equivalent. Here, the vault's *share* token uses SPL Token rather than a custom contract.
- **Mint account** — The on-chain config for one token: `supply`, `decimals`, `mint_authority`, `freeze_authority`. Owned by the SPL Token program. The closest Solidity analog is "the constant fields of an ERC-20 plus `totalSupply`, but all stored on the token program, not the issuer".
- **Token Account** — One user's balance for one specific token. Owned by the SPL Token program, owned-by (the `owner` field) the wallet that controls it. The Solidity analog is "the `balances[user]` entry, except it's its own on-chain account".
- **ATA / Associated Token Account** — The canonical per-wallet Token Account for a given mint. Its address is deterministically derivable from `(wallet, mint)`. The Solidity analog is "what `balances[user]` *would be* if Solidity gave it a deterministic address".
- **PDA** (Program-Derived Address) — A deterministic on-chain account address derived from byte seeds the program controls. The vault, the share mint, the asset reserve, and the vault-authority signer are all PDAs.
- **CPI** (cross-program invocation) — One program calling another, the way one Solidity contract calls another. The vault CPIs into SPL Token to mint shares, burn shares, and transfer underlying.
- **rent** — A refundable SOL deposit every account pays to live on-chain. Refunded in full when the account is closed.
- **Anchor** — The framework around the raw Solana program API; provides macros, account validation, the IDL. Hardhat-to-EVM analog.
- **`has_one = X`** — An Anchor account constraint: "the account's stored `X` field must equal the `X` account passed in this instruction".
- **`Account<'info, T>`** — Anchor's typed account wrapper. Performs owner-program check, discriminator check, and deserialization automatically. Skipping it invites type-confusion bugs (treating a Mint as a Vault, etc.).

After first use, each term is fair game.

---

## State model

### Replace `Vec<BalanceEntry>` shares with an SPL Token Mint (diff §S1)

- **What:** Removed `balances: Vec<BalanceEntry>` from `VaultState` (`02-naive-port.rs:482`). Shares are now an SPL Token Mint (`03-optimized.rs:675`); each holder owns an SPL Token Account (their ATA) for the share mint.
- **Why:** In Solidity, an ERC-4626 vault is also an ERC-20 — the contract holds a `mapping(address => uint256) balanceOf` for shares. The Solana equivalent is to make shares a real SPL Token: a Mint account for "the share token" plus per-holder Token Accounts (typically ATAs, the canonical per-wallet Token Account for a given mint). Keeping the `Vec` would force every deposit/withdraw to write-lock the shared vault state account, serializing all deposits behind each other. With shares as an SPL Token, different holders' deposits/withdraws touch different Token Accounts and run in parallel.
- **Benefit:** Holder count is unbounded. Per-holder share writes parallelize (Alice's ATA and Bob's ATA are disjoint accounts, so transactions touching them don't conflict). The vault account itself shrinks dramatically (§R1) because it no longer carries the per-holder balance table.
- **Tradeoff:** First-time share recipients pay ATA rent (~0.002 SOL — a refundable SOL deposit to keep the account alive). Off-chain code reads share balances by querying the holder's share ATA through SPL Token, not by reading a `balances` field on the vault. Standard Solana UX for any token, including the share token here.

### Delete `total_assets` / `total_supply`; read them from SPL Token (diff §S2)

- **What:** Removed both fields from the vault and their `+=` / `-=` mutations on every deposit/withdraw path. Every conversion site now reads `share_mint.supply` (the share token's total supply, maintained by SPL Token) and `asset_reserve.amount` (the vault's underlying balance, also an SPL Token field) directly (`03-optimized.rs:91`–`92`, `:132`–`:133`, etc.).
- **Why:** SPL Token maintains both values atomically as side effects of `mint_to` (mints `n` and bumps supply by `n` in one operation), `burn` (the inverse), and `transfer` (no supply change, but the source/destination amounts update atomically). Self-tracking duplicates audited code AND forces the vault to be writable on every deposit/withdraw — exactly the parallelism penalty §P1 below is designed to remove.
- **Benefit:** One source of truth instead of two — no risk of `total_assets` drifting from `asset_reserve.amount` due to a missed update path. The vault becomes read-only during all user-facing 4626 operations, which unlocks the parallelism win below.
- **Tradeoff:** One extra account passed to each call (the `asset_reserve` Token Account, so the program can read its `amount`). Anchor already needs it as `mut` for the underlying transfers, so the marginal cost is zero — the account was going to be in the list anyway.

### Move share ERC-20 surface to SPL Token (diff §S3)

- **What:** Share transfer/approve are not vault instructions — clients call SPL Token directly with the share mint. The vault exposes only the 4626 operations (deposit, mint, withdraw, redeem) plus governance.
- **Why:** Same lesson as the ERC-20 example, applied to the share token. Reimplementing transfer/approve inside the vault would duplicate the SPL Token program — auditable code, audited primitives, a bigger surface for bugs, and no integration benefit. See **Client/API integration notes** below for the concrete client-side delta.
- **Benefit:** Vault code shrinks. The share token plugs into every Solana wallet, explorer, and indexer for free.
- **Tradeoff:** Clients have to know that "the share token" is an SPL Token with a known mint address; they read balances through SPL Token, not through the vault. Documented in the Client/API integration notes section.

---

## Parallelism

### Vault is READ-ONLY on every user-facing 4626 operation (diff §P1)

- **What:** `Deposit` and `Withdraw` account structs omit `mut` on the vault account (`03-optimized.rs:528`–`537`, `:571`–`:580`). The vault is only marked `mut` in admin instructions (`set_fee_bps`, etc.). Solana's runtime locks writable accounts for the duration of a transaction; declaring the vault read-only here means deposit/withdraw don't claim a write-lock on it.
- **Why:** With `total_assets`/`total_supply` deleted (§S2) and balances moved to SPL Token (§S1), there is nothing left on the vault account for a deposit/withdraw to mutate. The vault stores only governance fields (`fee_bps`, `authority`, cached bumps) which change at admin-rate, not transaction-rate.
- **Benefit:** Cross-user deposit/withdraw transactions don't write-conflict on the vault at all. Conflict remains only at the inherent globals — `share_mint.supply` (every mint/burn touches it) and `asset_reserve.amount` (every transfer in/out touches it). Both are maintained by SPL Token; both are write-hot for any fungible-token system regardless of design. This is the *minimum* possible contention for a vault on Solana.
- **Tradeoff:** None. This is the largest parallelism win in this example: in Solidity every deposit and every withdraw mutates the vault contract (the EVM serializes them anyway), but on Solana, removing the unnecessary self-tracking lets the runtime actually run them in parallel.

### O(n) Vec scans eliminated (diff §P2)

- **What:** Removed the `iter_mut().find()` lookups for share balance entries (the naive port scanned the `balances` Vec linearly to find a holder's row).
- **Why:** Same reasoning as in the escrow and token-fundraiser examples. A linear scan inside an account costs compute units linearly in the holder count *and* requires the full Vec to be deserialized at instruction entry. With shares as SPL Token, the holder's ATA is loaded directly by Anchor — no scan, fixed-size deserialization.
- **Benefit:** Constant-time lookup regardless of holder count.
- **Tradeoff:** None.

---

## Security

### Every arithmetic op uses `checked_*`; one `mul_div_u128_to_u64` helper centralizes the 4626 math (diff §Sec1)

- **What:** Replaced bare `*` / `/` / `+` / `-` and `as u64` casts with a single helper (`03-optimized.rs:399`–`418`) that does checked multiplication, checked division, checked ceiling-addition for the up-rounding case, and a bounds-guarded narrowing back to `u64` after the intermediate `u128` math.
- **Why:** ERC-4626 conversion math is the single most security-sensitive code in a vault. Solidity 0.8+ checks arithmetic by default; Rust release builds wrap silently on overflow. A silent overflow in the share/asset conversion doesn't fail — it returns a wrong number that the depositor cheerfully accepts. The naive port had silent truncation on every preview path (`02-naive-port.rs:317`, `:324`, `:332`, `:339`), silent overflow on the ceiling-add for up-rounding, and silent wrap on the balance updates. Each could individually be exploited to either dilute honest holders or extract value from rounding errors.
- **Benefit:** Every arithmetic failure is now a typed error (`Overflow`, `DivByZero`). The helper is one place to audit; auditors don't have to re-verify the math at every call site. Inflation-defense math (which adds `VIRTUAL_SHARES_OFFSET` to the numerator — see §Sec6) is checked end-to-end.
- **Tradeoff:** Verbose helper body — about 30 lines. Acceptable; it's the highest-stakes 30 lines in the program.

### Explicit `Rounding` enum at every conversion site (diff §Sec2)

- **What:** Added a `Rounding { Down, Up }` enum (`03-optimized.rs:46`–`50`). Every preview/conversion call passes a direction explicitly. The helper signatures take `rounding: Rounding` and floor or ceiling accordingly.
- **Why:** ERC-4626's rounding directions are part of the spec, not an implementation detail. The rules:
  - `deposit` rounds shares **down** — the depositor gets at most their fair share.
  - `mint` rounds assets **up** — the depositor pays at least their fair share.
  - `withdraw` rounds shares **up** — the withdrawer burns at least their fair share.
  - `redeem` rounds assets **down** — the withdrawer gets at most their fair share.

  Every direction favors the vault, which prevents dust-extraction attacks (a delegate that repeatedly redeems 1 wei to drain rounding errors). The naive port computed direction implicitly inside each function; a future refactor that consolidates the helpers risks silently flipping a direction. An explicit `Rounding` arg at every call site makes the audit trivial: 4 lines, one per direction.
- **Benefit:** Wrong direction is a code review failure, not a silent runtime bug. The audit checklist is "scan for `Rounding::Down` on deposit/redeem and `Rounding::Up` on mint/withdraw" — minutes, not hours.
- **Tradeoff:** Helper signature is longer. Worth it.

### PDA bumps cached + canonicalization enforced (diff §Sec3)

- **What:** Store `Vault.bump` and `Vault.vault_authority_bump` at init, pass via `bump = vault.bump` in account validation on every subsequent call.
- **Why:** Re-deriving with `find_program_address` costs ~1500 CU per call. Accepting a non-canonical bump opens a bug class where an attacker passes a different valid bump and the program signs for a different address than it thinks. Pinning the canonical bump at init eliminates both.
- **Benefit:** Cheaper instructions; closes the non-canonical-bump attack class.
- **Tradeoff:** Two `u8`s of account space. See `security/pda-canonicalization.md` for the full pattern.

### Vault-authority signing scoped to the vault key (diff §Sec4)

- **What:** The signer PDA's seeds are `[b"vault_authority", vault.key().as_ref()]` (per-vault) instead of `[b"vault_authority"]` (singleton).
- **Why:** The `vault_authority` PDA is what signs CPIs to mint shares, burn shares, and move underlying out of the reserve. If the program ever grows to support multiple vaults (one per asset, say), a singleton authority across vaults would mean a bug in one vault's withdrawal flow could be used to move funds out of any other vault — because the same signing identity authorizes all of them. Including `vault.key()` in the seeds makes each vault's authority cryptographically distinct.
- **Benefit:** Per-vault authority isolation. The blast radius of any bug in CPI signing is one vault.
- **Tradeoff:** Seeds are slightly longer (one extra 32-byte pubkey). Negligible CU/storage cost.

### Classic SPL Token only — Token-2022 transfer hooks rejected at the type level (diff §Sec5)

- **What:** Both ports use `anchor_spl::token::{Mint, TokenAccount}` (the classic SPL Token bindings). Anchor's typed-account wrapper verifies that the account's owning program is the classic SPL Token program ID; a Token-2022 mint passed in would fail the typed-account check, and the instruction reverts before the handler runs.
- **Why:** Token-2022 is the newer SPL Token variant with extensions (transfer fees, interest, confidential transfers, transfer hooks). The Transfer Hook extension specifically lets the mint specify an arbitrary program that runs on every transfer — including the vault's `token::transfer` CPI. That program could call back into the vault, which is a classic cross-program reentrancy surface. Allowing Token-2022 underlyings here would require either an explicit reentrancy state machine in the vault, or an allowlist of "safe" extensions. Both add real complexity. For this example, the right answer is to reject Token-2022 entirely at the type level and document the choice.
- **Benefit:** An entire class of attacks (transfer-hook reentrancy) is structurally impossible. No defensive code required in the vault body.
- **Tradeoff:** Vaults cannot accept Token-2022 underlyings without a redesign. Noted in the `Initialize` doc comment. A production deployment that needs both token flavors should split the program into two — one for classic SPL, one for Token-2022 — so the security stance is per-program, not per-vault.

### ERC-4626 inflation-attack defense preserved (diff §Sec6)

- **What:** Both ports include the virtual-offset terms in every conversion. The optimized version's checked math means the defense cannot be silently degraded by an arithmetic bug.
- **Why:** The canonical 4626 attack: an attacker stakes 1 wei before any honest user and receives 1 share. They then donate a large amount of underlying directly to the vault's reserve. Now `totalAssets` is huge but `totalSupply` is 1. An honest user's deposit of N wei converts to `N * 1 / huge ≈ 0` shares — they got zero shares for their deposit, and the attacker still owns ~100% of the vault.

  **OpenZeppelin's mitigation** is a virtual offset: pretend the vault has `10^DECIMALS_OFFSET = 10^6` virtual shares and 1 virtual asset already. The attacker's 1-wei first stake now mints `1 * (0 + 10^6) / (0 + 1) = 10^6` shares. After a 10^9-wei donation, an honest deposit of 10^6 assets still mints ~2000 real shares (not zero), so the honest user can redeem and get most of their assets back. The attack still bounds rounding losses but is no longer total-loss.

  | Step | totalSupply (shares) | totalAssets | Note |
  |---|---|---|---|
  | Attacker deposits 1 asset | 10^6 | 1 | First deposit; virtual offset gives 10^6 shares |
  | Attacker donates 10^9 directly to reserve | 10^6 | 10^9 + 1 | Donation skews the share price |
  | Honest deposits 10^6 assets | ~10^6 + 2000 | 10^9 + 10^6 + 1 | Honest user gets ~2000 shares, *not* zero |
  | Honest redeems 2000 shares | … | … | Receives ~10^6 assets back; loss is dust |

  Without the offset, the honest user would receive zero shares for their 10^6 assets — full loss.
- **Benefit:** The standard 4626 attack class is mitigated. Translation is mechanical because the defense is preserved as-is from OpenZeppelin's Solidity; the optimized version's checked arithmetic ensures a future code change can't silently weaken it.
- **Tradeoff:** Share decimals = asset decimals + 6, which means the share token has 6 more decimal places than the underlying. Clients should convert raw share amounts to human units (`amount / 10^share_decimals`) rather than expose raw `amount`. Standard Solana token accounting. **Regression-test idea:** at `total_supply = 0, total_assets = 0`, depositing 1 asset must yield >= `10^6` shares. If a future code change makes the first depositor get 1 share or fewer, the defense is broken — assert `convert_to_shares(1, 0, 0, Rounding::Down) >= 1_000_000` in unit tests.

### `has_one` cross-validation (diff §Sec7)

- **What:** Added `has_one = asset_mint`, `has_one = share_mint`, and `has_one = authority` constraints to the relevant account structs (`03-optimized.rs:550`, `:592`, etc.). `has_one = X` is an Anchor constraint that tells the framework: "the account's stored `X` field must equal the `X` account passed into this instruction" — declarative form of `require(vault.asset_mint == asset_mint_account)`.
- **Why:** A future instruction author who adds an account to the program could forget to check that it links back to the vault correctly. `has_one` makes the linkage declarative — Anchor enforces it at account validation time before any handler code runs. The reviewer sees the access-control rules at the top of the struct, the same way a Solidity reviewer scans for `onlyOwner` modifiers.
- **Benefit:** Audits become local. Read the struct, see the constraints, done — no need to walk handler bodies to verify cross-references.
- **Tradeoff:** None.

---

## CPI & program reuse

### Share mint/burn via SPL Token CPI (diff §C1)

- **What:** `token::mint_to` issues shares (signed by the `vault_authority` PDA, since the share mint's authority is set to that PDA at init); `token::burn` destroys shares (signed by the holder or their delegate). Both happen via CPI into the SPL Token program.
- **Why:** Same architectural move as the other examples — use audited SPL Token for token mechanics. The vault retains only the conversion math and governance gating; the token movements are SPL Token's responsibility. The Solidity ERC-4626 has `_mint(receiver, shares)` and `_burn(owner, shares)` calling its inherited ERC-20; the Solana analog is the SPL Token CPI.
- **Benefit:** No custom mint/burn arithmetic in the vault. Standard SPL Token wallet integration — every Solana wallet, explorer, and indexer recognizes the share token automatically.
- **Tradeoff:** ~5,000 CU per CPI (the cost of crossing a program boundary). Acceptable — the conversion math is the costly part, not the CPIs.

### Withdraw-by-delegate uses SPL Token's native delegate (diff §C2)

- **What:** The `Withdraw` instruction accepts a generic `signer: Signer<'info>` with no `token::authority` constraint on the share ATA. SPL Token's `burn` instruction accepts the signer if it's *either* the ATA's owner *or* its registered delegate (with sufficient `delegated_amount`).
- **Why:** Solidity's `withdraw(assets, receiver, owner_)` lets a delegate spend the owner's allowance — this is a real workflow (yield aggregators, sweep accounts, etc.). SPL Token has the same primitive built in: `spl_token::approve` sets a single delegate per Token Account with a delegated amount. Reimplementing an allowance map in the vault would duplicate this and introduce a custom code path. The right answer is to lean on SPL Token's native delegate.
- **Benefit:** No allowance map in the vault. Standard SPL Token approve/transferFrom-style flows work without per-vault integration.
- **Tradeoff:** SPL Token allows one delegate per Token Account, not a `(owner, spender) → amount` map. Same gap as the ERC-20 example — uncommon to need >1 active delegate per holder, but if your protocol does, you'd need a custom allowance PDA per `(owner, spender)` pair.

### Aggregates read from SPL Token, not stored (diff §C3)

- **What:** Per-call reads of `share_mint.supply` and `asset_reserve.amount` during every conversion. No self-tracked totals on the vault.
- **Why:** SPL Token is the source of truth — anything the vault would store about totals is a duplicate that can drift. Eliminating the duplicate eliminates the divergence-bug class where a missed `+=` / `-=` makes self-tracked totals lie about reality.
- **Benefit:** Code shrinks. The vault is read-only on user actions (§P1) — self-tracking would have made the vault writable and undone the parallelism win.
- **Tradeoff:** None.

---

## Compute & rent

### Vault size: ~4 KB → 132 bytes (diff §R1)

- **What:** `VaultState::SIZE = 4118` → `Vault::SIZE = 132`.
- **Why:** Removing the 100-entry `Vec<BalanceEntry>` saves ~4000 bytes; storing only governance fields and cached bumps leaves ~130. Rent on Solana scales with account size (~0.001 SOL per KB), so the protocol-paid rent shrinks ~30×.
- **Benefit:** Significantly cheaper to deploy. Vault deserialization on every instruction entry is faster (smaller struct).
- **Tradeoff:** Each share holder pays ~0.002 SOL for their ATA when they first receive shares. Refunded if the ATA is later closed. Standard Solana UX.

### Per-call data load: ~30× smaller (diff §R2)

- **What:** ~4 KB Vec deserialization on the naive's vault entry replaced by ~380 bytes total of SPL Token + vault account loads in the optimized version.
- **Why:** Anchor deserializes account data on instruction entry — it has to type-check it before the handler runs. Smaller accounts = faster entry = fewer compute units burned before any user logic.
- **Benefit:** Multi-kCU savings per call — meaningful inside Solana's 200K-CU per-instruction budget.
- **Tradeoff:** None.

---

## Idioms

### Pure conversion helpers + Rounding enum (diff §I1)

- **What / Why / Benefit / Tradeoff:** See §Sec1 and §Sec2. The structural and security wins are linked — the helpers are pure functions, take rounding direction explicitly, and live in one auditable place.

### Consolidated CPI helpers (diff §I2)

- **What:** `mint_shares()` and `transfer_asset_out()` factor the `vault_authority`-signed CPIs. The signer seeds are constructed once inside the helper.
- **Why:** Signer-seed construction is a common bug surface — a typo in one of multiple inline constructions can cause the program to sign for a different PDA than intended. One helper means one place to audit.
- **Benefit:** Auditable seed construction. Easier to add new `vault_authority`-signed operations without drift.
- **Tradeoff:** A few extra helper signatures in the file. Trivial.

### Use Anchor's typed `Mint` / `TokenAccount` (diff §I3)

- **What:** Every SPL Token account in the program is `Account<'info, Mint>` or `Account<'info, TokenAccount>`. No raw `AccountInfo` for SPL accounts.
- **Why:** Typed wrappers perform owner-program check (does this account belong to SPL Token?), structural check (does the data layout match a Mint or a Token Account?), and field deserialization automatically. Skipping them and using raw `AccountInfo` invites type-confusion bugs — passing a Token Account where a Mint was expected, etc.
- **Benefit:** Audit cost drops — the type system carries the validation. Reviewers don't have to verify "is this account checked to be a real Mint?" at every use site.
- **Tradeoff:** None.

### Typed errors (diff §I4)

- **What:** Nine `VaultError` variants, named per failure mode.
- **Why:** Every failure mode has a name and a stable code in the program's IDL. Off-chain consumers (clients, indexers) can match on error codes and surface human-readable messages without parsing log text.
- **Benefit:** Real diagnostics, not `msg!()` strings buried in transaction logs.
- **Tradeoff:** None.

---

## Client/API integration notes

This is the section a porting team should read first.

The optimized 4626 vault changes the client-side integration shape compared to a Solidity 4626. The change is *larger* than the ERC-20 example's because 4626 has more entry points and the share token also moves to SPL Token.

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

// User's ATAs (the canonical per-wallet Token Accounts for asset + share)
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
// per-instruction, not approved-then-pulled like ERC-20's allowance pattern.

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

### What changes for your team

- **No upfront approve of the underlying.** Solana transactions sign for SPL Token transfers per-instruction; there is no persistent allowance to manage like ERC-20's `approve` → `transferFrom` two-step.
- **Reads happen against SPL Token accounts, not the vault.** Balances → `getTokenAccountBalance(ata)`. Total supply → `mint.supply`. Total assets → `reserve.amount`. No `vault.balanceOf()` / `vault.totalSupply()` to call — those concepts moved to SPL Token's accounts.
- **ATA derivation is mandatory** before any operation that returns or burns shares for a "fresh" recipient. Wallets and the `@solana/spl-token` helpers handle this; budget one extra instruction per first-time recipient.
- **Delegate (transferFrom-equivalent) is one `approve` on the share ATA**, not a per-`(owner, spender)` allowance map. If your dApp expected multi-spender support, see the §C2 tradeoff.
- **Event indexing changes.** SPL Token emits its own logs for share movement (mint/burn). The vault emits `DepositEvent` / `WithdrawEvent` / `EarnEvent` via Anchor's `#[event]` macro — parse via the Anchor IDL.

If your existing 4626 dApp's client assumes one contract call per operation, budget a sprint for the migration; the architecture is straightforward but every call site touches.
