<p align="center">
    <br /><br />
    <picture>
      <source media="(prefers-color-scheme: dark)" srcset="allnodes/images/rakurai-dark-mode.png">
      <img alt="Rakurai-Solana Node Allnodes Edition" src="allnodes/images/rakurai-light-mode.png" style="width: 16em">
    </picture>
</p>

# Rakurai-Solana Node with modifications from Allnodes

## Modifications made by Allnodes

This repository features the following enhancements to the Jito-Solana codebase:

### 1. Fast snapshot distribution

✅ Only on [Allnodes Bare-Metal Servers](https://www.allnodes.com/hosting/solana)

Our infrastructure includes modifications that improve default snapshot downloading, which combined with
ultra-high-speed channels deliver ultra-fast snapshot downloads. This dramatically reduces the initial sync time for
new validators and enables faster deployment and recovery scenarios. The use of snapshot-finder or any other 3rd party
download tools is no longer needed.

### 2. Enhanced voting logic modifications

✅ Only on [Allnodes Bare-Metal Servers](https://www.allnodes.com/hosting/solana)

Our validator implementation includes voting modifications developed by **Zantetsu | Shinobi Systems** that enhance the
original voting logic.

These modifications work by:

- Taking the next votable slot that the original codebase identifies as potentially ready for voting
- Applying additional criteria before casting the vote
- Providing more sophisticated voting decision-making

This enhancement improves validator consensus participation through more intelligent vote timing and slot evaluation.

### 3. Automatic Performance Optimization for Proof-of-History

✅ Only on [Allnodes Bare-Metal Servers](https://www.allnodes.com/hosting/solana)

Your Solana node will automatically select the fastest CPU core for Proof-of-History processing, maximizing performance
out of the box.

### 4. Hardware-optimized SHA256 patch

Our validator implementation includes a third-party performance patch developed by **kagren**. It optimizes SHA256
hashing operations using SHA-NI instructions available on modern AMD processors (Zen3, Zen4, and Zen5
architectures). This enhancement significantly improves hashing performance for block verification and other
cryptographic operations.

# Rakurai-Solana Docs

Welcome to the Rakurai documentation. These guides are intended for validator operators, searchers, and traders using the Rakurai ecosystem.

---

## 1. Background

### 1.1. What is Rakurai-Solana?

Rakurai-Solana is a high-performance Solana validator node designed to achieve **superior block rewards** and **higher Transactions Per Second (TPS)**. It incorporates heuristics-based **transaction scheduling** and other optimization techniques to efficiently process high-value transactions, boosting both performance and profitability for node operators.

### 1.2. High-Level Flow Architecture

The Rakurai node is composed of five main components:

1. **Rakurai Scheduler Library** — A scheduler optimized for selecting high-value transactions.
2. **Rakurai Agave Client** — A fork of the jito-solana client modified to run the Rakurai scheduler library.
3. **Rakurai Activation Program** — A smart contract that controls node participation and enables validators to run a Rakurai node.
4. **Reward Distribution Program** — Distributes block rewards to stakers via per-epoch **Reward Collection Accounts (RCA)** and post-epoch Merkle claims; tracks on-chain tip and MevShare revenue in per-validator, per-service **Tips Collection Accounts (TCA)** and **MevShare Collection Accounts (MCA)**.
5. **Rakurai Tip Manager Program** — Manages tips sent to Rakurai validators across eight tip PDAs; drains and splits tips into the validator's TCA.

### 1.3. Validator Incentives and Rewards Flow

With Rakurai's advanced transaction scheduler, validators can capture higher block rewards while improving both TPS and CU utilization. At present, Rakurai does not charge any fees for running its client. Validators may keep these rewards entirely or choose to share a portion with their stakers. Distribution is executed via a **configurable, trustless, merkle-root-based system**. In the future, Rakurai plans to charge a small commission on the block rewards earned by the validator.

### 1.4. How Rakurai Interacts with the Solana Ecosystem

Rakurai nodes function like standard Solana validators but include performance enhancements focused on transaction throughput and block reward optimization. They remain fully compatible with the Solana protocol while offering measurable improvements in validator economics. Rakurai actively maintains and updates the scheduler library to ensure compatibility with the latest Solana releases.

---

## 2. Documentation

| Section                                                               | Description                                                                                                  |
| --------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------ |
| [Validators](./rakurai_docs/validators/README.md)                     | Setup, operation, upgrades, binary attestation, and Geyser integration for Rakurai validators.               |
| [Transaction Inclusion Network (TIN)](./rakurai_docs/transaction_inclusion/README.md) | Integrate with TIN: bundle support, virtual priority, and post-pack confirmations. |
| [Programs](./rakurai_docs/rakurai_programs/README.md)                 | Documentation for Rakurai on-chain programs and protocol components.                                         |

## 3. Contacts

| Channel | Link |
| ------- | ---- |
| Website | [rakurai.io](https://rakurai.io) |
| Telegram | [t.me/rakurai_official](https://t.me/rakurai_official) |
| Discord | [discord.gg/XS7GmnmCJg](https://discord.gg/XS7GmnmCJg) |
| X | [@Rakurai_io](https://x.com/Rakurai_io) |
| LinkedIn | [Rakurai](https://www.linkedin.com/company/rakurai/) |
| GitHub | [rakurai-io/rakurai-validator](https://github.com/rakurai-io/rakurai-validator) |

---

# Voting mod configuration

Voting mod (also known as "mostly confirmed threshold" voting patch) is enabled by default and comes with a predefined
configuration which should work for most users. If you wish to use a custom configuration:

1. create a configuration file (default filename is `mostly_confirmed_threshold` located in the current directory from
   where you run the validator). Values in this example are defaults, their meanings will be explained in the next
   section:

```bash
echo '0.45 4 0 24' > ./mostly_confirmed_threshold
```

2. optionally, you can provide a different filename and/or path for the config file using the
   `--mostly-confirmed-threshold-config <path/to/config/file>` argument.

> In order to disable the voting mod, you need to add the `--disable-mostly-confirmed-threshold` flag to the validator
command.

## Mostly confirmed threshold configuration file format:

The `mostly_confirmed_threshold` file contains a simple whitespace-separated list of four values:

```
a b c d
```

### Parameters

#### *a* (float) - vote weight threshold
The minimum vote weight threshold required before voting on a slot. Slots that haven't achieved this vote weight will
not be voted on, except for:

- Slots within the "vote ahead of threshold" region
- When the escape hatch distance has been reached

#### *b* (integer) - vote ahead of threshold
The number of slots ahead of the threshold slot to vote on, regardless of vote weight. This parameter reduces vote
latency by allowing voting on recent slots even if they haven't met the threshold.

#### *c* (integer) - skip recovery mode
Controls the stake-weighted vote percentage required on a slot after skips have occurred. Must be one of:

- `0` - No restriction
- `1` - Slot after a skip must have `mostly_confirmed_threshold` before voting
- `2` - Slot after a skip must be confirmed before voting

#### *d* (integer) - escape hatch distance
The maximum number of slots to wait without voting while waiting for the threshold to be met. After this many slots of non-voting, the validator will vote anyway.

**Purpose**: This escape hatch prevents network deadlock by ensuring progress even when the threshold isn't being achieved. Without this mechanism, if multiple forks occur simultaneously and all have less than the threshold vote weight, validators could become stuck waiting indefinitely.

### Default values

When the configuration file is absent, the following default values are used:

```
0.45 4 0 24
```

- Threshold: 45% vote weight
- Vote ahead: 4 slots
- Skip recovery: No restriction
- Escape hatch: 24 slots
