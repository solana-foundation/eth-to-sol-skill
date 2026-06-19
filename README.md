# eth-to-sol

A Claude skill that ports Ethereum/Solidity contracts to production-grade Solana programs and teaches the developer what changed and why.

## What it does

Given a Solidity contract, the skill produces:

1. **A faithful Anchor port** — semantically identical, deliberately un-Solana.
2. **A Solana-native refactor** — restructured around SPL programs, PDAs, the account model, and Sealevel parallelism. Production-ready.
3. **A structured diff** between the two ports.
4. **An explanation log** — a per-change record of *what / why / benefit / tradeoff*. This is the teaching artifact.

The two-pass structure is the point. The faithful port is the baseline that makes the refactor's value legible. A developer who reads only the optimized version learns what production Solana looks like; a developer who reads both learns *why*.

## Invoking the skill

Inside Claude Code with this directory available as a skill:

```
Run the eth-to-sol skill on this contract: <paste Solidity or path to .sol file>
```

The skill reads `SKILL.md`, traverses its decision tree to load only the sub-files relevant to the input, runs the pre-flight checklist, and emits the four artifacts.

For the reference example, the inputs and outputs are pre-built under `examples/token-fundraiser/` (Solidity ERC-20 crowdfund → canonical `tokens/token-fundraiser` shape from solana-developers/program-examples).

## Directory layout

```
eth-to-sol/
├── README.md                   # this file
├── SKILL.md                    # router — loaded on every invocation; lean
├── translation/                # mechanical Solidity → Anchor mapping
│   ├── mental-model.md
│   ├── type-mapping.md
│   ├── pattern-mapping.md
│   └── stdlib-mapping.md
├── optimization/               # Solana-native restructuring guidance
│   ├── account-model.md
│   ├── pdas.md
│   ├── parallelism.md
│   ├── compute-budget.md
│   ├── transactions-and-commitment.md
│   ├── rent-and-size.md
│   └── program-splitting.md
├── security/                   # non-negotiable hardening rules
│   ├── signer-checks.md
│   ├── account-validation.md
│   ├── arithmetic.md
│   ├── pda-canonicalization.md
│   ├── cpi-safety.md
│   └── reentrancy.md
└── examples/
    ├── token-fundraiser/        # ERC-20 crowdfund → goal-or-refund Anchor program
    │   ├── 01-original.sol     # ERC-20 crowdfund with goal + deadline
    │   ├── 02-naive-port.rs    # Pass 1: faithful Anchor port (Vec<Contribution>)
    │   ├── 03-optimized.rs     # Pass 2: per-supporter PDAs, close-on-refund
    │   ├── 04-diff.md          # structured diff
    │   └── 05-explanation.md   # explanation log
    ├── escrow/                  # two-party ERC-20 atomic swap
    └── erc4626-vault/           # tokenized vault (ERC-4626 share math)
```

## Adding new examples

The skill is designed so adding examples is mechanical:

1. Drop the Solidity source in `examples/<name>/01-original.sol`.
2. Run the skill against it — it produces `02` through `05`.
3. Do not edit `SKILL.md` or sub-files unless the input exposes a gap in the existing guidance.

Three reference examples ship with the skill (token-fundraiser, escrow, erc4626-vault) covering the most common ERC-20-adjacent translation surface. New examples should not require sub-file changes — if they do, the gap is in the guidance, not the example.

## Source alignment

The skill tracks Solana Enterprise Training Module 1B, "From EVM to SVM":
https://github.com/solana-foundation/solana-enterprise-training/tree/main/module-1b-from-evm-to-svm

The skill references the module's engineering topics: the caller-brings-state mental model, account/PDA translation, SPL Token, CPI account propagation, Solana security checks, transactions, local fees, commitment, and developer tooling. It does not ship course UI, slides, quizzes, or app-specific interface material.

## Assumptions

- **Anchor 1.0+** is the target framework. The reference examples compile against `anchor-lang = "1.0.2"` / `anchor-spl = "1.0.2"`. Note that 1.0 changed `CpiContext::new` / `new_with_signer` to take `Pubkey` (via `.key()`) instead of `AccountInfo` for the program argument. Raw Solana is noted only when CU-critical or when Anchor cannot express the pattern (rare).
- **SPL Token classic** is the default for fungible tokens. Token-2022 is mentioned where its extensions change the answer.
- **Solidity ^0.8.x** assumed for input contracts (built-in arithmetic checks). Earlier versions need extra care — see `security/arithmetic.md`.

## Style

Sub-files are dense but scannable: headers, code blocks, tables. Code in examples is real Anchor, not pseudocode. Prose teaches without lecturing. Assume the reader knows Solidity well and Solana barely.
