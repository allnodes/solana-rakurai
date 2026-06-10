# How to Use Post-pack Confirmation

This guide explains how the validator streams transaction updates to postpack-confirmation endpoints, and how consumers can respond with bundles.

---

## What is Post-pack Confirmation?

Rakurai scheduler provides some updates (post-pack confirmations). As soon as a transaction gets scheduled for execution, it forwards the update to configured **post-pack confirmation endpoints** over gRPC. These updates are generated from the **point of no-return**. Consumers of this service can only see the updates just before they imminently become part of the block therefore ensuring no front-running.

Post-pack confirmation uses the **Jito packet gRPC protocol** ([`packet.proto`](jito-protos/protos/packet.proto), [`block_engine.proto`](jito-protos/protos/block_engine.proto)) — the same `Packet` / `PacketBatch` / `StartExpiringPacketStream` shape used by the Jito relayer.

The consumer's job is to receive the postpack-confirmation `Packet`, then send back a **bundle** that includes:

1. The original postpack-confirmation packet(s) (unchanged)
2. Any additional transactions (e.g. backrun / arb)

Duplicate transactions are suppressed. One `Packet` is sent per transaction per endpoint.

---

## Endpoint Configuration

Post-pack confirmation endpoints can be configured in two ways:

* **On-chain Account:** `5SonkAVc6Pi7vfv8WayKsuCrzM6UgBgPTVneBQizs5Jg`
* **Admin RPC:** `setPostPackConfirmationConfig`

**Merge rule:** union of on-chain account + Admin RPC entries, keyed by URL. Admin RPC wins on the same URL.

Config is re-read periodically; Admin RPC changes take effect without restart. Each `setPostPackConfirmationConfig` call **replaces** the full admin entry list. On-chain account entries remain unless overridden by URL.

**Active endpoints** = merged admin + on-chain entries, minus any UUIDs on the blocklist (see `setPostPackConfirmationUuidBlocklist` below). Use `getPostPackConfirmationConfig` to inspect all layers.

### Entry shape

[`PostPackConfirmation`](core/src/banking_stage.rs):

| Field | Description |
|-------|-------------|
| `url` | gRPC base URL (e.g. `http://127.0.0.1:20000`) |
| `uuid` | Endpoint unique identifier |

---

## Configure via Admin RPC

Admin IPC is request/response: keep the socket open briefly so `socat` can read the reply before stdin closes. Pipe `sleep 1` after each request.

### `setPostPackConfirmationConfig`

Replaces the full admin entry list.

```bash
(echo '{"jsonrpc":"2.0","id":1,"method":"setPostPackConfirmationConfig","params":["{\"entries\":[{\"url\":\"http://127.0.0.1:20000\",\"uuid\":\"PostPackConfig2\"},{\"url\":\"http://127.0.0.1:10000\",\"uuid\":\"PostPackConfig1\"}]}"]}'; sleep 1) \
  | socat - UNIX-CONNECT:admin.rpc
```
##### Clear PostPack AdminRPC Entries
```bash
echo '{"jsonrpc":"2.0","id":1,"method":"setPostPackConfirmationConfig","params":[{\"entries\":[]}"]}' \
| socat - UNIX-CONNECT:admin.rpc
```

### `getPostPackConfirmationConfig`

Returns the live status maintained by the scheduler (admin + on-chain merge, blocklist, and what is actually connected).

**Response fields** ([`PostPackConfirmationConfigStatus`](core/src/banking_stage.rs)):

| Field | Description |
|-------|-------------|
| `admin_entries` | Entries set via `setPostPackConfirmationConfig` |
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

### `setPostPackConfirmationUuidBlocklist`

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

> **Note:** Use `params:[[]]` (one parameter: an empty UUID array). `params:[]` omits the parameter and will not clear the blocklist.

---

## gRPC protocol

### Packet shape ([`packet.proto`](jito-protos/protos/packet.proto))

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
