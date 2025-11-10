# Block Reward Distribution to Stakers

Rakurai validators can share **block rewards** with their stakers using a **Merkle proof-based** distribution flow powered by the [Reward Distribution program](../rakurai_docs/rakurai_programs/programs/reward_distribution/README.md). This guide covers the full process — from enabling distribution on-chain to how rewards are accumulated, calculated, and claimed each epoch.

---

## Background

### No native protocol support

Solana's staking protocol distributes **inflation / voting rewards** to stakers automatically, but it provides **no on-chain mechanism to distribute block rewards** (the lamports earned during leader slots) to stakers. By default, block rewards remain in the **validator identity account**.

### Rakurai's solution

Rakurai introduces an opt-in feature that lets any validator running Rakurai share block rewards with stakers. The flow is:

1. Configure how much of each block reward the validator retains (via the [**Rakurai Activation Account**](../rakurai_docs/rakurai_programs/programs/rakurai_activation/README.md)).
2. Accumulate the staker share on-chain in a per-epoch [**Reward Collection Account (RCA)**](../rakurai_docs/rakurai_programs/programs/reward_distribution/README.md#how-it-works) throughout the epoch.
3. At epoch end, compute stake-weighted allocations off-chain (optionally adjusted via [distribution config](#customize-distribution-optional)), upload a Merkle root, and claim stakers share.


### Free for Rakurai validators

When you delegate Merkle Root Authority to Rakurai (recommended),
<p style="font-size:14px;">
    <span style="color:#66ff66;"><i><b>0% distribution fees charged by Rakurai</b></i></span>
    — only standard Solana transaction fees apply.
</p>


---

## Configuration & flexibility

Distribution is controlled at two levels:

- **On-chain (RAA)** — Set a global `block_reward_commission_bps` to define what share of block rewards the validator keeps vs. what goes to stakers. This is the only setting required to get started.
- **Off-chain (optional)** — Before each epoch ends, submit a [`reward_distribution_config.json`](#customize-distribution-optional) to fine-tune allocations when the Merkle tree is built. You can exclude stake accounts, set per-staker commission rates, redirect a staker's share to a referral claimant, or split the validator's retained rewards — without changing on-chain program logic.

With no off-chain config, rewards are distributed **proportionally by stake** at your on-chain commission rate. With a config file, you get full flexibility over who receives what before claims go live.
 
---

## Default behavior (distribution disabled)

When a validator creates their [Rakurai Activation Account (RAA)](../rakurai_docs/rakurai_programs/programs/rakurai_activation/README.md#rakuraiactivationaccount-account-creation), `block_reward_commission_bps` defaults to **10,000 bps (100%)**.

| Commission (bps) | Validator share | Staker share |
|-----------------:|----------------:|-------------:|
| 10000            | 100%            | 0%           |
| 5000             | 50%             | 50%          |
| 0                | 0%              | 100%         |

At **100%**, the validator keeps all block rewards. Nothing is transferred to an RCA and no staker distribution occurs for that epoch.

> Block reward commission is **independent** of Solana's voting commission. It applies only to block rewards.

---

## Enable block reward distribution

To start sharing block rewards with stakers, set `block_reward_commission_bps` below **10,000** in your RAA.

### 1. Update commission via CLI

Use the Rakurai Activation CLI [`update-commission`](../rakurai_docs/rakurai_programs/cli/README.md#3-update-commission) command:

```sh
rakurai-activation -p <PROGRAM_ID> update-commission \
  --block_reward_commission_bps <VALUE> \
  --identity_pubkey <IDENTITY_PUBKEY> \
  --keypair <IDENTITY_KEYPAIR> \
  --url <RPC_URL>
```

- **Mainnet program ID:** `rAKACC6Qw8HYa87ntGPRbfYEMnK2D9JVLsmZaKPpMmi`
- **Testnet program ID:** `pmQHMpnpA534JmxEdwY3ADfwDBFmy5my3CeutHM2QTt`

You can also set commission at RAA creation time with `rakurai-activation init --block_reward_commission_bps <VALUE>`. See the [CLI README](../rakurai_docs/rakurai_programs/cli/README.md#1-init) for details.

Verify your settings anytime with [`rakurai-activation show`](../rakurai_docs/rakurai_programs/cli/README.md#4-show).


### 2. When the new commission takes effect

The updated commission applies:

- From the **current epoch**, if no [RCA](../rakurai_docs/rakurai_programs/programs/reward_distribution/README.md#1-rewardcollectionaccount-account-initialization) has been created for the current epoch yet.
- From the **next epoch**, if an RCA for the current epoch already exists.

### 3. Configure the Rakurai client (node operator)

The Rakurai Solana client must be configured with the correct [CLI arguments](../rakurai_docs/validators/setup_and_build.md#4-add-additional-cli-args) so it can:

- Initialize the per-epoch RCA on your first leader turn.
- Transfer staker rewards into the RCA on every subsequent leader turn.
- Set `reward_merkle_root_authority` — the account allowed to upload the Merkle root after the epoch ends.

Set this authority to [**Rakurai**](../rakurai_docs/rakurai_programs/programs/reward_distribution/README.md) for fully automated distribution, or keep it yourself if you prefer to run distribution manually.

---

## Epoch lifecycle

Once commission is below 100%, the following happens every epoch. Full details are in the [Epoch lifecycle](../rakurai_docs/rakurai_programs/programs/reward_distribution/README.md#-epoch-flow).

1. **RCA initialization** — On the first leader turn, the Rakurai client creates a per-epoch [Reward Collection Account (RCA)](../rakurai_docs/rakurai_programs/programs/reward_distribution/README.md#1-rewardcollectionaccount-account-initialization).
2. **Per-turn accumulation** — On every leader turn, the staker share of the previous turn's block reward is transferred into the RCA; validator and block-builder commissions are retained separately. See [Per-Turn Transfers](../rakurai_docs/rakurai_programs/programs/reward_distribution/README.md#2-per-turn-transfers).
3. **Post-epoch distribution** — At the final slot, a stake snapshot is taken, a Merkle tree is built off-chain (allocations can be adjusted via [distribution config](#customize-distribution-optional)), the root is uploaded on-chain, and stakers claim their rewards. See [Post-Epoch Staker Distribution](../rakurai_docs/rakurai_programs/programs/reward_distribution/README.md#3-post-epoch-staker-distribution).

When `reward_merkle_root_authority` is set to Rakurai, steps 1–3 are fully automated (0% distribution fee). See [Reward Distribution — Free & Automated by Rakurai](../rakurai_docs/rakurai_programs/programs/reward_distribution/README.md#reward-distribution--free--automated-by-rakurai).

---

## Customize distribution (optional)

The off-chain config described [above](#configuration--flexibility) is submitted as `reward_distribution_config.json` to the Rakurai team (via Slack/Telegram) before the epoch ends. See the options and examples below.

#### Example configuration

```json
{
  "validators_config": {
    "comment": "Validator-specific configuration keyed by vote account.",
    "<vote_account>": {
      "stakers": {
        "comment": "These `excluded_stake_pubkeys` stake accounts are excluded and will NOT receive rewards. All other stakers must receive at least the minimum block_reward_commission_bps specified On-Chain. For example, if validator commission = 7000 BPS (70%), each staker must receive at least 30% of rewards.",
        "excluded_stake_pubkeys": [
          "<stake_account_1>",
          "<stake_account_2>"
        ],

        "custom_commissions": {
          "<stake_account_3>": {
            "commission_bps": 4000,
            "comment": "Validator gets 60%, staker gets 40%"
          },

          "<stake_account_4>": {
            "commission_bps": 5000,
            "referral_claimant_pubkey": "<referral_account>",
            "referral_claimant_commission_bps": 1000,
            "comment": "Validator gets 50%. From staker's 50%, 10% is redirected to referral claimant"
          }
        }
      },

      "validator_reward_split": {
        "claimant_pubkey": "<claimant>",
        "claimant_commission_bps": 5000,
        "comment": "validator rewards (if any): 50% to claimant, 50% to validator identity. If not specified, all remaining goes to validator identity."
      }
    }
  }
}
```

#### Configuration options

##### Exclude stake accounts

```json
"excluded_stake_pubkeys": [
  "<stake_account>"
]
```

Excluded stake accounts receive **no block rewards**. You can effectively implement a whitelist by excluding every stake account except the ones you want to reward.

##### Custom stake account commissions

```json
"custom_commissions": {
  "<stake_account>": {
    "commission_bps": 4000
  }
}
```

Overrides the validator's global commission for a single stake account. For example, if your global commission is **70%**, you can give one staker **40%** commission (60% to staker instead of 30%).

##### Referral reward sharing

```json
"custom_commissions": {
  "<stake_account>": {
    "commission_bps": 5000,
    "referral_claimant_pubkey": "<referral_account>",
    "referral_claimant_commission_bps": 1000
  }
}
```

A portion of the staker's rewards is redirected to a referral claimant. In this example:

- Validator receives **50%** (instead of 70%)
- Staker receives **40%** (instead of 30%)
- Referral claimant receives **10%**

##### Validator reward split

```json
"validator_reward_split": {
  "claimant_pubkey": "<claimant>",
  "claimant_commission_bps": 5000
}
```

Splits the validator's retained rewards with another account. `5000` means 50% to the claimant and 50% to the validator identity.

### What you can do with these options

- Share block rewards with all stakers proportionally.
- Effectively whitelist selected stake accounts.
- Exclude specific stake accounts.
- Offer preferential reward rates to selected stakers.
- Run referral reward programs.
- Split validator-retained rewards with another claimant.

---
