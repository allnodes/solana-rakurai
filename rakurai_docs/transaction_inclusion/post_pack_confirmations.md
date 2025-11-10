# Rakurai Post-Pack Confirmations — Guide

How the validator streams transaction updates to post-pack confirmation endpoints and how consumers respond with bundles.

**Audience:** Searchers and Traders consuming post-pack confirmations and validator operators configuring endpoints.

---

## 1. Overview

Rakurai scheduler provides updates (post-pack confirmations). As soon as a transaction is scheduled for execution, it forwards the update to configured **post-pack confirmation endpoints** over gRPC. These updates are generated from the **point of no-return**. Consumers of this service only see updates just before they imminently become part of the block, ensuring no front-running.

Post-pack confirmation uses the **Jito packet gRPC protocol** ([`packet.proto`](../../jito-protos/protos/packet.proto), [`block_engine.proto`](../../jito-protos/protos/block_engine.proto)) — the same `Packet` / `PacketBatch` / `StartExpiringPacketStream` shape used by the Jito relayer.

The consumer's job is to receive the post-pack confirmation `Packet`, then send back a **bundle** that includes:

1. The original post-pack confirmation packet(s) (unchanged)
2. Any additional transactions (e.g., backrun / arb)

Duplicate transactions are suppressed. One `Packet` is sent per transaction per endpoint.

**What consumers receive:** transactions as Jito Packet format [`Packet`](../../jito-protos/protos/packet.proto) messages (raw Solana wire bytes), streamed over `StartExpiringPacketStream`.

**What you send back:** a bundle that includes the original post-pack confirmation packet **unchanged**, plus any additional transactions (e.g., arb). The protocol mirrors the Jito relayer packet/bundle flow.

---

## 2. Admin RPC

Admin IPC is request/response: keep the socket open briefly so `socat` can read the reply before stdin closes.

### 2.1. getPostPackConfirmationConfig

Returns the live status maintained by the scheduler (admin + on-chain merge, blocklist, and what is actually connected).

| Field | Description |
|-------|-------------|
| `onchain_entries` | Entries loaded from the on-chain PDA |
| `blocklisted_uuids` | Endpoint UUIDs blocked via `setPostPackConfirmationUuidBlocklist` |
| `blocklisted_entries` | Full merged entries whose `uuid` is blocklisted (url + uuid) |
| `active_entries` | Merged admin + on-chain (admin wins on same URL), excluding blocklisted UUIDs — these are the endpoints receiving scheduler updates |

```bash
(echo '{"jsonrpc":"2.0","id":1,"method":"getPostPackConfirmationConfig","params":[]}'; sleep 1) \
  | socat - UNIX-CONNECT:admin.rpc | jq
```

**Example response:**

```json
{
  "admin_entries": [
    {"url":"http://127.0.0.1:20000","uuid":"PostPackConfig2"},
    {"url":"http://127.0.0.1:10000","uuid":"PostPackConfig1"}
  ],
  "onchain_entries": [],
  "blocklisted_uuids": ["PostPackConfig1"],
  "blocklisted_entries": [
    {"url":"http://127.0.0.1:10000","uuid":"PostPackConfig1"}
  ],
  "active_entries": [
    {"url":"http://127.0.0.1:20000","uuid":"PostPackConfig2"}
  ]
}
```

### 2.2. setPostPackConfirmationUuidBlocklist

Blocklists post-pack confirmation endpoints by **UUID**. Each call **replaces** the full blocklist. Pass an empty array to clear.

Blocklisted UUIDs are removed from `active_entries` on the next scheduler config sync. If a blocklisted endpoint already has an open gRPC connection, it is torn down immediately on sync; other endpoints stay connected.

**Example — block one endpoint by UUID:**

```bash
(echo '{"jsonrpc":"2.0","id":1,"method":"setPostPackConfirmationUuidBlocklist","params":[["PostPackConfig1"]]}'; sleep 1) \
  | socat - UNIX-CONNECT:admin.rpc
```

**Example — clear blocklist (reconnect blocklisted endpoints on next sync):**

```bash
(echo '{"jsonrpc":"2.0","id":1,"method":"setPostPackConfirmationUuidBlocklist","params":[[]]}'; sleep 1) \
  | socat - UNIX-CONNECT:admin.rpc
```

**Note:** Use `params:[[]]` (one parameter: an empty UUID array). `params:[]` omits the parameter and will not clear the blocklist.

---

## 3. gRPC protocol

### 3.1. Packet shape

[`packet.proto`](../../jito-protos/protos/packet.proto)

For each transaction the validator sends one `PacketBatchUpdate` with `msg = batches`:

```
PacketBatchUpdate
  └── batches: ExpiringPacketBatch
        ├── header.ts
        ├── batch: PacketBatch
        │     └── packets[]: Packet
        │           ├── data    ← raw Solana wire transaction bytes
        │           └── meta    ← Packet meta (size, addr, port, flags, sender_stake)
        └── expiry_ms = 0
```

**Decode in Rust:**

```rust
use solana_transaction::versioned::VersionedTransaction;

let txn: VersionedTransaction = bincode::deserialize(&packet.data)?;
```

## 4. Rakurai Post-Pack Confirmations — FAQ

### 4.1 How do I register an Post-pack endpoint?

Endpoints are configured **on-chain** and loaded by the scheduler from the on-chain PDA.

Operators inspect and control endpoints with:

- `getPostPackConfirmationConfig` — returns `onchain_entries`, `blocklisted_uuids`, `blocklisted_entries`, and `active_entries`.
- `setPostPackConfirmationUuidBlocklist` — blocklists endpoints by UUID (each call replaces the full blocklist; pass an empty array to clear).

> **Note:** the validator can blocklist any searcher from receiving post-pack confirmations through [Admin RPC](./post_pack_confirmations.md#2-admin-rpc).


---

### 4.2 How do searchers share MEV revenue?

Post-pack / MEV-share revenue is deposited directly into the searcher's own account, which Rakurai does **not** control. Instead of automatically collecting these funds, the agreed revenue share is tracked on-chain using a **[MEV Share Collection Account (MCA)](../rakurai_programs/programs/reward_distribution/README.md#tip--mevshare-collection-accounts)**.

The process consists of three steps:

1. **Register** — A MEV Share Collection Account (MCA) is created for each validator, and the searcher agree with Rakurai on a comission percentage that will be shared among validator & Rakurai.

2. **Report** — After the epoch ends, the searcher reports the revenue share owed for the previous epoch by recording the amount in the relevant MCA. This updates the on-chain accounting only and does **not** transfer any lamports.

3. **Settle** — The searcher then transfers the recorded amount into the relevant MCA. From there, the reward distribution program distributes the funds between the validator and Rakurai according to the configured revenue-sharing agreement.

---

### 4.3 How is revenue distributed?

Distribution works exactly like tips — see [How Distribution Happens](../rakurai_programs/programs/reward_distribution/README.md#how-tip--mevshare-are-distributed). Once the recorded amount is settled into the MCA, it is split in two parts:

- **Client** (i.e. Rakurai): the client commission is credited to its account (percentage recorded in the MCA).
- **Validator**: the remaining share is credited to its identity account.
