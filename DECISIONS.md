# Design decisions

Choices made while constructing this skill. New translations should be consistent with these unless the input forces otherwise.

## Framework

- **Anchor 0.30+** is the default. We use current idioms: `Account<'info, T>`, `Signer<'info>`, `Program<'info, …>`, `init`, `seeds`, `bump`, `has_one`, `#[error_code]`, `ctx.bumps.<name>` (struct-field form, not the older `HashMap` form).
- Raw Solana / `solana-program` is mentioned only where Anchor cannot express the pattern or where CU pressure justifies the friction. The reference ERC-20 example does not need raw.

## Fungible tokens

- **SPL Token classic** is the default for *vanilla* ERC-20 translations — contracts that do not override `_update` / `_beforeTokenTransfer` / `_afterTokenTransfer`. Classic is the audited path, ATAs are universally supported, and DEXes/wallets understand it out of the box.
- **Token-2022** (`spl-token-2022`) is the *correct target*, not a footnote, when the source contract has any of:
  - **Fee-on-transfer** (deducts a fee on every transfer) → Token-2022 `TransferFeeConfig` extension.
  - **Transfer restrictions** (blacklist, KYC gating, paused transfers, "only whitelisted recipients") → Token-2022 `TransferHook` extension, with the hook program enforcing the rule. `DefaultAccountState` (frozen-by-default) for opt-in models.
  - **Rebasing / yield-bearing supply** → Token-2022 `InterestBearingConfig` extension. Note this changes how `balanceOf` is computed off-chain — UI integrators must use the extension's helpers, not raw `TokenAccount.amount`.
  - **Soulbound / non-transferable** → Token-2022 `NonTransferable` mint extension.
  - **Inline metadata on the mint** → Token-2022 `MetadataPointer` + `TokenMetadata` extensions. Avoids the separate Metaplex account.
  - **Confidential transfers** → Token-2022 `ConfidentialTransfer` extension. Rare in EVM ports but possible.

If the source has *any* of the above, target Token-2022 from the start. Trying to express transfer-hook semantics in a custom program that wraps classic SPL Token leads to either the wrapper being trivially bypassable (callers call SPL directly and skip the hook) or to a non-standard token that no DEX integrates with. Neither is acceptable.

The decision tree in SKILL.md has an explicit trigger row for this. When in doubt, grep the Solidity for `_update`, `_beforeTokenTransfer`, `_afterTokenTransfer`, `transfer(` overrides, or any modifier on `transfer` / `transferFrom`.

## NFTs

Not yet exercised by an example. The intent is **Metaplex Token Metadata** + SPL Token (or Token-2022 with NFT extension) for ERC-721; Metaplex Core for new builds. Documented in `translation/stdlib-mapping.md` but not deeply.

## Mint authority

In the reference example, the **program is the mint authority via a PDA**. A separate `Config` PDA stores an `authority: Pubkey` field that gates *administrative* actions (changing max supply, transferring control). This separates "who can mint" (the program, gated by `authority`) from "who can change governance" (`authority` directly).

This is a common Solana idiom that has no Ethereum analog. We do not give the user EOA mint authority by default because then the program is bypassable.

## Naive port — `Vec` vs separate PDAs for mappings

A Solidity `mapping(address => uint256)` has two equally faithful Anchor translations:

1. `Vec<(Pubkey, u64)>` in a single state account.
2. One PDA per key, seeded by the address.

Option (1) is the more pedagogically useful naive port — it captures the antipattern that all balances live in one account, which serializes all transfers and bounds the holder count. Option (2) already partially Solana-ifies the design and would dilute the diff.

The skill's naive ports use **option (1) with a hardcoded cap** and a `// SMELL:` marker. The optimized version then either uses SPL Token accounts (for fungible tokens) or per-key PDAs (for arbitrary state).

## Solidity version assumption

Inputs are assumed to be Solidity ^0.8.0, where arithmetic is checked by default. Earlier versions need an explicit `SafeMath` audit pass on the input before translation. The skill does not attempt automatic remediation of pre-0.8 unchecked math — it flags and asks.

## Reentrancy

Solana's account locking blocks classic same-program reentrancy automatically. The skill **still** discusses reentrancy because:

- Cross-program reentrancy (program A → program B → program A) is possible if A is written carelessly.
- Read-then-write hazards across CPIs ("stale state after CPI") are common and have no EVM analog.
- **Token-2022 transfer hooks** are a CPI-reentrancy vector specific to Solana: an underlying mint with a `TransferHook` extension can invoke arbitrary code during a `transfer` CPI. Vaults, AMMs, and lending markets that accept a user-supplied token Mint as configuration are in scope for this risk.
- Developers porting from EVM expect a `ReentrancyGuard` pattern and we should tell them why they don't need one *and* what they should worry about instead.

See `security/reentrancy.md` and `security/account-validation.md` ("Token-2022 rejection via typed Mint").

## Vault-shaped protocols (4626 / AMM / lending market) accepting arbitrary token Mints

The default stance is **classic SPL Token only** as the underlying. Enforced at the type level via `anchor_spl::token::{Mint, TokenAccount}`. This rejects Token-2022 mints at deserialization, eliminating transfer-hook reentrancy as an attack surface — no defensive code needed in the protocol body.

To accept Token-2022 underlyings, the protocol must:

1. Switch to `anchor_spl::token_interface::{Mint, TokenAccount}` (accepts both programs).
2. Explicitly allowlist which Token-2022 extensions are acceptable. `TransferFeeConfig` is usually fine (it just adjusts accounting). `TransferHook` is dangerous — the hook program is arbitrary code that runs inside your CPI. Allow only after audit.
3. Document the matrix of supported extensions in the protocol's `DECISIONS.md`.

The `examples/erc4626-vault` example chose the strict default; the choice is documented in code and in the example's explanation log.

## Compute budget guidance

We give CU intuitions (instruction defaults to 200k; max 1.4M; common costs) but do not micro-benchmark in the reference example. CU optimization is real but rarely the first bottleneck for ported EVM code; the bigger wins are usually structural.

## Style for the explanation log

Strict four-field schema (`What / Why / Benefit / Tradeoff`). No long-form prose. Group by theme. The aim is that a developer can scan a single entry in 15 seconds and learn one Solana-shaped lesson.
