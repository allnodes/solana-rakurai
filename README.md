# Rakurai-Solana Docs

Welcome to the Rakurai documentation. These guides are intended for validator operators, searchers, and traders using the Rakurai ecosystem.

---

## 1. Background

### What is Rakurai-Solana?

Rakurai-Solana is a high-performance Solana validator node designed to achieve **superior block rewards** and **higher Transactions Per Second (TPS)**. It incorporates heuristics-based **transaction scheduling** and other optimization techniques to efficiently process high-value transactions, boosting both performance and profitability for node operators.

### High-Level Flow Architecture

The Rakurai node is composed of four main components:

1. **Rakurai Scheduler Library** — A scheduler optimized for selecting high-value transactions.
2. **Rakurai Agave Client** — A fork of the jito-solana client modified to run the Rakurai scheduler library.
3. **Rakurai Activation Program** — A smart contract that controls node participation and enables validators to run a Rakurai node.
4. **Reward Distribution Program** — A mechanism for distributing validator rewards to stakers using a permissionless, Merkle-root-based verification model.

### Validator Incentives and Rewards Flow

With Rakurai's advanced transaction scheduler, validators can capture higher block rewards while improving both TPS and CU utilization. At present, Rakurai does not charge any fees for running its client. Validators may keep these rewards entirely or choose to share a portion with their stakers. Distribution is executed via a **configurable, trustless, merkle-root-based system**. In the future, Rakurai plans to charge a small commission on the block rewards earned by the validator.

### How Rakurai Interacts with the Solana Ecosystem

Rakurai nodes function like standard Solana validators but include performance enhancements focused on transaction throughput and block reward optimization. They remain fully compatible with the Solana protocol while offering measurable improvements in validator economics. Rakurai actively maintains and updates the scheduler library to ensure compatibility with the latest Solana releases.

---

## Documentation

| Section                                                               | Description                                                                                                  |
| --------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------ |
| [Validators](./rakurai_docs/validators/README.md)                     | Setup, operation, upgrades, binary attestation, and Geyser integration for Rakurai validators.               |
| [Transaction Inclusion](./rakurai_docs/transaction_inclusion/README.md) | Integrate with Rakurai transaction inclusion, bundle support, virtual priority, and post-pack confirmations. |
| [Programs](./rakurai_docs/rakurai_programs/README.md)                 | Documentation for Rakurai on-chain programs and protocol components.                                         |
