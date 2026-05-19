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

For the reference example, the inputs and outputs are pre-built under `examples/erc20-token/`.

## Directory layout

```
eth-to-sol/
├── README.md                   # this file
├── SKILL.md                    # router — loaded on every invocation; lean
├── DECISIONS.md                # design decisions made during construction
├── translation/                # mechanical Solidity → Anchor mapping
│   ├── type-mapping.md
│   ├── pattern-mapping.md
│   └── stdlib-mapping.md
├── optimization/               # Solana-native restructuring guidance
│   ├── account-model.md
│   ├── pdas.md
│   ├── parallelism.md
│   ├── compute-budget.md
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
    └── erc20-token/
        ├── 01-original.sol     # standard ERC-20 with mint/burn/approve/Ownable
        ├── 02-naive-port.rs    # Pass 1: faithful Anchor port
        ├── 03-optimized.rs     # Pass 2: Solana-native version (SPL Token CPI)
        ├── 04-diff.md          # structured diff
        └── 05-explanation.md   # explanation log
```

## Adding new examples

The skill is designed so adding examples is mechanical:

1. Drop the Solidity source in `examples/<name>/01-original.sol`.
2. Run the skill against it — it produces `02` through `05`.
3. Do not edit `SKILL.md` or sub-files unless the input exposes a gap in the existing guidance.

Two follow-up examples are planned (DeFi staking, ERC-4626 vault). They should not require sub-file changes.

## Assumptions

- **Anchor** 0.30+ is the target framework. Raw Solana is noted only when CU-critical or when Anchor cannot express the pattern (rare).
- **SPL Token classic** is the default for fungible tokens. Token-2022 is mentioned where its extensions change the answer.
- **Solidity ^0.8.x** assumed for input contracts (built-in arithmetic checks). Earlier versions need extra care — see `security/arithmetic.md`.

See `DECISIONS.md` for the full list.

## Style

Sub-files are dense but scannable: headers, code blocks, tables. Code in examples is real Anchor, not pseudocode. Prose teaches without lecturing. Assume the reader knows Solidity well and Solana barely.
