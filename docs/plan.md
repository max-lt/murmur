# Murmur — Private Device Sync Network

## How to Use This Document

This is the **architecture and implementation plan** for Murmur. Work milestone by milestone, in order.
For each milestone: implement, test, `cargo clippy -- -D warnings`, `cargo fmt`, stop.

---

## What is Murmur?

Murmur is a **private, peer-to-peer device synchronization network** written in Rust. Think of it as a personal cloud that syncs files (photos, documents) between your devices — phone, tablet, laptop, NAS — without any third-party server.

A user generates a BIP39 mnemonic (12 or 24 words) that creates a private network. Devices join by entering the mnemonic and being approved by an existing device. Files flow automatically from sources (phone) to backup nodes (NAS), and any device can request temporary access to another device's files.

The name comes from a **murmuration** — the mesmerizing synchronized flight of starlings, where thousands of individuals move as one without central coordination.

## Core Design Principles

1. **Pure Rust core** — Zero C/C++ bindings in the core library. Cross-compilation to mobile targets must be trivial.
2. **Seed-based identity** — A BIP39 mnemonic is the root of trust. Everything derives from it: network ID, ALPN, gossip topic.
3. **Device approval** — Knowing the seed lets you knock on the door; an existing device must open it. Every device maintains the full peer list.
4. **DAG-based state** — All mutations (device joins, file additions, access grants) are recorded in a signed, hash-chained DAG — same LogTree pattern as Shoal.
5. **Bidirectional sync** — Push (automatic background upload to backup nodes) + Pull (on-demand access request with approval).
6. **Offline-first** — Devices sync when they can. The DAG merges naturally when they reconnect.

## Architecture Overview

The Rust core is **protocol and logic only** — it handles networking, the DAG,
cryptography, and sync state machines. It produces and consumes serialized bytes.
Storage is **not** part of the core. Each platform (desktop, Android, iOS) decides
how and where to persist DAG entries, blobs, and config. The core doesn't know and
doesn't care.

```
┌─────────────────────────────────────────────────────────┐
│  Platform (owns storage, UI, OS integration)            │
│                                                         │
│  ┌───────────┐  ┌───────────┐   ┌─────────────────────┐ │
│  │ Android   │  │   iOS     │   │  Desktop (murmurd)  │ │
│  │ Kotlin    │  │  Swift    │   │  Rust CLI + Web UI  │ │
│  │ Room/SQL  │  │ CoreData  │   │  Fjall + filesystem │ │
│  │ SAF       │  │ FileProv  │   │  axum               │ │
│  └─────┬─────┘  └─────┬─────┘   └──────────┬──────────┘ │
│        │              │                    │            │
│        └──────────────┼────────────────────┘            │
│                       │                                 │
│         ┌─────────────┴──────────────┐                  │
│         │  Callbacks / FFI boundary  │                  │
│         │                            │                  │
│         │  Platform → Core:          │                  │
│         │   "here's a DagEntry blob" │                  │
│         │   "send this file"         │                  │
│         │   "approve this device"    │                  │
│         │                            │                  │
│         │  Core → Platform:          │                  │
│         │   "store this DagEntry"    │                  │
│         │   "new device wants to     │                  │
│         │    join, ask the user"     │                  │
│         │   "file received, here     │                  │
│         │    are the bytes"          │                  │
│         └─────────────┬──────────────┘                  │
├───────────────────────┼─────────────────────────────────┤
│  Rust Core            │                                 │
│  (protocol + logic, no storage)                         │
│  ┌────────────────────┴────────────────────────┐        │
│  │         Engine (orchestrator)               │ murmur-engine
│  │         Sync state machine, approval flow   │        │
│  │         Produces/consumes serialized bytes  │        │
│  ├─────────────────────────────────────────────┤        │
│  │         DAG (mutation log)                  │ murmur-dag
│  │         Entries, signatures, merge, delta   │        │
│  │         In-memory only — platform persists  │        │
│  ├─────────────────────────────────────────────┤        │
│  │         Network (iroh + gossip)             │ murmur-net
│  │         QUIC transport, gossip broadcast    │        │
│  ├─────────────────────────────────────────────┤        │
│  │         Seed (BIP39 + key derivation)       │ murmur-seed
│  ├─────────────────────────────────────────────┤        │
│  │         Types (DeviceId, BlobHash, …)       │ murmur-types
│  └─────────────────────────────────────────────┘        │
└─────────────────────────────────────────────────────────┘
```

### What the core does

- **Networking**: iroh QUIC connections, gossip broadcast, message encoding/decoding
- **DAG logic**: create entries, sign them, verify them, compute deltas, merge tips, topological sort
- **Sync protocol**: state machine for push/pull, device approval handshake, access grant flow
- **Cryptography**: BIP39 seed, HKDF derivation, Ed25519 signing/verification, blake3 hashing
- **Events**: emit events to the platform (new device join request, file received, sync complete, etc.)

### What the core does NOT do

- **Storage**: the core never writes to disk. It hands serialized bytes to the platform via callbacks.
  The platform decides how to persist (Fjall, SQLite, Room, Core Data, filesystem, whatever).
- **UI**: the core emits events. The platform renders them however it wants.
- **OS integration**: background services, file providers, notifications — all platform-specific.
- **Blob storage**: the core hashes and transfers blobs. Where they land on disk is the platform's problem.

### Platform contract

The platform must provide the core with:

- A way to **load** DAG entries and blobs on startup (feed them into the core)
- **Callbacks** to persist new DAG entries and blobs when the core produces them
- **User decisions** when the core asks (approve device? grant access?)

The core provides the platform with:

- **Serialized data** (DagEntry bytes, blob bytes) to persist
- **Events** (device joined, file synced, access requested, etc.)
- **Functions** to trigger actions (add file, approve device, request access, etc.)

## Technology Stack

Core dependencies (pure Rust, no storage):

| Layer            | Crate                | Purpose                                        |
| ---------------- | -------------------- | ---------------------------------------------- |
| Hashing          | `blake3`             | Content addressing, integrity, network ID      |
| Networking       | `iroh` 0.96          | QUIC connections, hole punching, relay         |
| Gossip/broadcast | `iroh-gossip` 0.96   | Epidemic broadcast for DAG entries             |
| BIP39            | `bip39` v2           | Mnemonic generation and seed derivation        |
| Key derivation   | `hkdf` + `sha2`      | Derive network ID, ALPN, device keys from seed |
| Signing          | `ed25519-dalek` v2   | DAG entry signatures, device identity          |
| Serialization    | `postcard` + `serde` | Wire format                                    |
| Async            | `tokio`              | Runtime                                        |
| Observability    | `tracing`            | Structured logging                             |

Desktop/server only (in `murmurd`, not in core):

| Layer       | Crate      | Purpose                                         |
| ----------- | ---------- | ----------------------------------------------- |
| Metadata DB | `fjall` v3 | DAG + state persistence (server implementation) |
| CLI         | `clap`     | Command-line interface                          |

### Key Choices

- **BIP39 over random secret**: Users can write down 12/24 words on paper. Recovery is possible without any digital backup. Same UX as Bitcoin wallets.
- **HKDF over BIP32**: We don't need hierarchical derivation. HKDF with domain-separated labels is simpler and sufficient (derive network ID, ALPN, etc. from the same seed).
- **iroh for networking**: QUIC with built-in hole punching and relay fallback. Devices behind NAT can still connect. The iroh `NodeId` is already an Ed25519 public key — we can use it directly as `DeviceId`.
- **No storage in core**: Each platform has its own optimal storage. Desktop uses Fjall, Android uses Room/SQLite, iOS uses Core Data. The core only deals with serialized bytes.
- **No erasure coding**: Unlike Shoal, we don't need Reed-Solomon. Files are stored whole on backup nodes. Simplicity wins here.

## Key Data Types

```rust
// === Identifiers ===
pub struct DeviceId([u8; 32]);     // Ed25519 public key (= iroh NodeId)
pub struct NetworkId([u8; 32]);    // blake3(hkdf(seed, "murmur/network-id"))
pub struct BlobHash([u8; 32]);     // blake3(file_content)

// === Device info ===
pub enum DeviceRole {
    Source,                         // produces files (phone, tablet)
    Backup,                         // stores files (NAS, server)
    Full,                           // both
}

pub struct DeviceInfo {
    pub device_id: DeviceId,
    pub name: String,               // human-readable ("Max's iPhone")
    pub role: DeviceRole,
    pub iroh_addr: Option<NodeAddr>, // last known address
    pub approved: bool,
    pub approved_by: Option<DeviceId>,
    pub joined_at: u64,             // HLC timestamp
}

// === File metadata ===
pub struct FileMetadata {
    pub blob_hash: BlobHash,
    pub filename: String,
    pub size: u64,
    pub mime_type: Option<String>,
    pub created_at: u64,            // filesystem timestamp
    pub device_origin: DeviceId,    // which device created this file
}

// === Access control ===
pub struct AccessGrant {
    pub to: DeviceId,
    pub from: DeviceId,
    pub scope: AccessScope,
    pub expires_at: u64,            // unix timestamp
    pub grant_signature: [u8; 64],  // signed by `from`
}

pub enum AccessScope {
    AllFiles,
    FilesByPrefix(String),          // e.g. "photos/2025/"
    SingleFile(BlobHash),
}

// === DAG entry (adapted from Shoal's LogTree) ===
pub struct DagEntry {
    pub hash: [u8; 32],            // blake3(hlc, device_id, action, parents)
    pub hlc: u64,                   // hybrid logical clock
    pub device_id: DeviceId,        // author
    pub action: Action,
    pub parents: Vec<[u8; 32]>,     // DAG edges
    pub signature_r: [u8; 32],      // ed25519 signature (two halves)
    pub signature_s: [u8; 32],
}

pub enum Action {
    // Device management
    DeviceJoinRequest { device_id: DeviceId, name: String },
    DeviceApproved { device_id: DeviceId, role: DeviceRole },
    DeviceRevoked { device_id: DeviceId },
    DeviceNameChanged { device_id: DeviceId, name: String },

    // File sync
    FileAdded { metadata: FileMetadata },
    FileDeleted { blob_hash: BlobHash },

    // Access control
    AccessGranted { grant: AccessGrant },
    AccessRevoked { to: DeviceId },

    // DAG maintenance
    Merge,
    Snapshot { state_hash: [u8; 32] },
}
```

## Detailed Behavior

### Seed & Network Bootstrap

1. User generates or enters a BIP39 mnemonic (12 or 24 words)
2. Mnemonic → 64-byte seed via BIP39 (with empty passphrase, or user-provided)
3. From the seed, derive via HKDF-SHA256 with domain separation:
   - `network_id = blake3(hkdf(seed, info="murmur/network-id"))` — gossip topic ID
   - `alpn = "murmur/0/" + hex(network_id)[..16]` — QUIC ALPN for network isolation
   - On the **first device only**, the seed also determines the first device's signing key:
     `first_device_key = hkdf(seed, info="murmur/first-device-key")` → Ed25519 keypair
   - Subsequent devices generate their own random Ed25519 keypair locally
4. The first device is auto-approved (it created the network)

### Device Join Flow

1. New device enters the mnemonic → derives `network_id` + `alpn`
2. Device generates a fresh Ed25519 keypair → its `DeviceId`
3. Device joins the gossip topic, broadcasts a `DeviceJoinRequest`
4. All existing approved devices receive the request
5. Any approved device can approve: creates a `DeviceApproved` DAG entry
6. The DAG entry propagates via gossip → all devices update their peer list
7. The new device syncs the full DAG from an existing peer (same as Shoal's `pull_log_entries`)

### Push Sync (automatic)

1. Source device detects new file (e.g. new photo)
2. Compute `BlobHash = blake3(file_content)`
3. Check DAG: is this blob already known? (dedup)
4. Create `FileAdded` DAG entry with metadata
5. Broadcast DAG entry via gossip
6. Transfer blob data to backup nodes via iroh QUIC (direct stream, not gossip)
7. Backup node verifies blake3 on receive, stores to disk

### Pull Access (on-demand)

1. Tablet wants to view Phone's photos
2. Tablet sends an `AccessRequest` message to Phone (point-to-point via iroh QUIC)
3. Phone displays notification: "Tablet wants to access your photos. Allow?"
4. If approved: Phone creates `AccessGranted` DAG entry with scope and expiration
5. Tablet can now fetch blobs directly from Phone via QUIC
6. Access expires automatically; Phone can also revoke early via `AccessRevoked`

### DAG Sync & Merge

Same as Shoal's LogTree:

- Each device appends to its own branch of the DAG
- Entries reference current tips as parents → forms a DAG
- Gossip propagates new entries to all peers
- When a device receives entries, it updates tips (remove parents, add new entry)
- Multiple tips = concurrent branches → auto-merge entry collapses them
- Conflict resolution: LWW (Last-Writer-Wins) by HLC, tiebreak by DeviceId
- Materialized state (peer list, file index) is reconstructed by replaying the DAG

### Materialized State

The DAG is the source of truth. The materialized state is a derived cache:

```rust
pub struct MaterializedState {
    /// All known devices and their status.
    pub devices: BTreeMap<DeviceId, DeviceInfo>,
    /// All known files indexed by blob hash.
    pub files: BTreeMap<BlobHash, FileMetadata>,
    /// Active access grants.
    pub grants: Vec<AccessGrant>,
}
```

Rebuilt by replaying DAG entries in topological order. The core maintains this in memory.
The platform can serialize and cache it for fast startup, but it can always be reconstructed from the DAG.

### Blob Storage

Not part of the core. The core handles:

- Computing `BlobHash = blake3(content)` for integrity
- Transferring blob bytes over iroh QUIC
- Verifying blake3 on receive
- Emitting "store this blob" / "give me this blob" events to the platform

Each platform stores blobs however it wants:

- **Desktop (`murmurd`)**: content-addressed files on disk (`~/.murmur/blobs/ab/cd/...`)
- **Android**: MediaStore, app-private storage, or SAF
- **iOS**: app container, File Provider shared storage

Files are identified globally by their `BlobHash`. Same file from two devices = same hash = stored once (dedup at the protocol level).

---

## Workspace Layout

```
crates/
  murmur-types/      Shared types (DeviceId, BlobHash, NetworkId, roles, etc.)
  murmur-seed/       BIP39 mnemonic handling + HKDF key derivation
  murmur-dag/        Signed DAG (adapted from Shoal's LogTree, in-memory only)
  murmur-net/        Network layer (iroh QUIC + gossip)
  murmur-engine/     Orchestrator (sync logic, approval flow, blob transfer, storage-agnostic)
  murmurd/           Headless backup server daemon (NAS, RPi, VPS)
tests/
  integration/       End-to-end tests (multi-device simulation)
docs/
  plan.md            This file

# Future (not in initial milestones):
platforms/
  android/           Kotlin app + Foreground Service + DocumentsProvider
  ios/               Swift app + File Provider Extension + background tasks
  desktop/           Desktop app (Tauri or native, TBD)
```

---

## Milestones

### Milestone 0 — Workspace Setup

**Goal**: Cargo workspace compiles, CI-ready structure.

- [ ] Create workspace `Cargo.toml` with all members
- [ ] Create all crate directories with stub `lib.rs` / `main.rs`
- [ ] Ensure `cargo build`, `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check` all pass
- [ ] Create `CLAUDE.md` and `docs/plan.md`
- [ ] Create `.gitignore`

**Tests**: `cargo build` and `cargo test` succeed with zero warnings.

---

### Milestone 1 — Types

**Crate**: `murmur-types`

**Goal**: All shared types, identifiers, and the HLC.

- [ ] `DeviceId` — 32-byte newtype, `Debug`/`Display` with hex, `Serialize`/`Deserialize`
- [ ] `BlobHash` — 32-byte newtype, same traits
- [ ] `NetworkId` — 32-byte newtype
- [ ] `DeviceRole` enum (`Source`, `Backup`, `Full`)
- [ ] `DeviceInfo` struct
- [ ] `FileMetadata` struct
- [ ] `AccessGrant` and `AccessScope`
- [ ] `Action` enum (all variants listed in Key Data Types above)
- [ ] `HybridClock` — same implementation as Shoal (`tick()`, `witness()`, monotonic)
- [ ] `GossipPayload` enum for gossip messages (DAG entries, membership events)

**Tests** (≥15):

- Roundtrip serialization (postcard) for every type
- DeviceId from Ed25519 key
- HLC monotonicity, witness advances clock
- Display/Debug formatting

---

### Milestone 2 — Seed

**Crate**: `murmur-seed`

**Goal**: BIP39 mnemonic generation, seed derivation, key management.

- [ ] Generate new mnemonic (12 or 24 words, user's choice)
- [ ] Validate existing mnemonic
- [ ] Mnemonic → 64-byte seed (BIP39 standard, optional passphrase)
- [ ] HKDF derivation with domain separation:
  - `network_id`: `hkdf(seed, info="murmur/network-id")` → blake3 → `NetworkId`
  - `alpn`: `"murmur/0/" + hex(network_id)[..16]`
  - `first_device_key`: `hkdf(seed, info="murmur/first-device-key")` → Ed25519 `SigningKey`
- [ ] `NetworkIdentity` struct that holds all derived values
- [ ] `DeviceKeyPair` — generate random Ed25519 keypair for non-first devices
- [ ] Persist device keypair to disk (encrypted at rest with seed-derived key, future; plaintext for v1)

**Tests** (≥12):

- Generate 12-word mnemonic, validate it
- Generate 24-word mnemonic, validate it
- Reject invalid mnemonic
- Same mnemonic → same network_id (deterministic)
- Same mnemonic → same ALPN
- Same mnemonic → same first_device_key
- Different mnemonic → different network_id
- HKDF domain separation: network_id ≠ first_device_key bytes
- DeviceKeyPair generation produces valid Ed25519 keys
- Sign and verify with derived keys

---

### Milestone 3 — DAG

**Crate**: `murmur-dag`

**Goal**: Signed append-only DAG, adapted from Shoal's `shoal-logtree`. This is the core CRDT.
**The DAG is in-memory only.** The platform is responsible for persisting and loading entries.

Adapt from Shoal:

- `LogEntry` → `DagEntry` (same structure: hlc, device_id, action, parents, hash, signature)
- `LogTree` → `Dag` (append, receive_entry, merge, sync)
- Drop: `LogTreeStore`'s Fjall backend. The DAG stores entries in a `HashMap` in memory.
  On startup, the platform feeds entries back into the DAG. The DAG emits new entries
  via callbacks for the platform to persist however it wants.

Key changes from Shoal:

- Replace `Action::Put/Delete` (S3 objects) with our `Action` enum (devices, files, access)
- Replace `Version` / bucket/key state with `MaterializedState` (devices map, files map, grants)
- No disk backend — in-memory `HashMap<[u8; 32], DagEntry>` + `HashSet<[u8; 32]>` for tips
- Keep: hash computation, signature verification, tip management, topological sort, delta sync

- [ ] `DagEntry` — same fields as Shoal's LogEntry, using our Action enum
- [ ] `DagEntry::new_signed()` — create, hash (blake3), sign (ed25519)
- [ ] `DagEntry::verify_hash()` and `verify_signature()`
- [ ] `DagEntry::to_bytes()` / `from_bytes()` — postcard serialization for platform persistence
- [ ] `Dag` struct — owns in-memory entry store, tips, clock, device_id, signing_key
- [ ] `Dag::load_entry()` — platform feeds a persisted entry back on startup
- [ ] `Dag::append()` — tick HLC, get tips as parents, sign, store in memory, update tips, apply to state, **return the new entry** for the platform to persist
- [ ] `Dag::receive_entry()` — verify hash+sig, check parents exist, store, update tips, apply. Returns the entry for platform persistence.
- [ ] `Dag::apply_sync_entries()` — batch receive with topological ordering
- [ ] `Dag::compute_delta()` — given remote tips, compute entries to send (Kahn's toposort)
- [ ] `Dag::maybe_merge()` — auto-merge when multiple tips exist
- [ ] Materialized state: `apply_to_state()` processes each Action variant
  - `DeviceApproved` → insert into devices map
  - `DeviceRevoked` → mark as revoked
  - `FileAdded` → insert into files map
  - `FileDeleted` → remove from files map
  - `AccessGranted` → add to grants list
  - `AccessRevoked` → remove from grants list

**Tests** (≥30):

- Append single entry, verify hash and signature
- Append multiple entries, verify DAG chain (parents correct)
- Receive remote entry, verify it's stored
- Reject entry with bad hash
- Reject entry with bad signature
- Reject entry with missing parents
- Two devices append concurrently → merge produces single tip
- Delta computation: device A has entries B doesn't → correct delta
- Topological sort ordering
- Materialized state: approve device → appears in devices map
- Materialized state: revoke device → marked revoked
- Materialized state: add file → appears in files map
- Materialized state: delete file → removed from files map
- Materialized state: grant access → appears in grants
- Materialized state: revoke access → removed from grants
- LWW conflict resolution (higher HLC wins)
- LWW tiebreak (higher DeviceId wins when HLC equal)
- Snapshot and state hash
- Load entries on startup: feed entries → correct materialized state
- Serialization roundtrip: `to_bytes()` / `from_bytes()` for all entry types
- Sync roundtrip: two DAGs, diverge, sync, converge

---

### Milestone 4 — Network

**Crate**: `murmur-net`

**Goal**: iroh QUIC transport + gossip, adapted from Shoal's `shoal-net` and `shoal-cluster`.

- [ ] `MurmurMessage` enum (wire protocol):
  - `DagEntryBroadcast { entry_bytes }` — gossip a new DAG entry
  - `DagSyncRequest { tips }` — request missing entries
  - `DagSyncResponse { entries }` — response with missing entries
  - `BlobPush { blob_hash, data }` — push file data to backup
  - `BlobPushAck { blob_hash, ok }` — acknowledge blob stored
  - `BlobRequest { blob_hash }` — request a blob
  - `BlobResponse { blob_hash, data }` — response with blob data
  - `AccessRequest { from, scope }` — request temporary access
  - `AccessResponse { grant }` — response with signed grant (or rejection)
  - `Ping { timestamp }` / `Pong { timestamp }`
- [ ] `MurmurTransport` — wraps iroh `Endpoint`:
  - Connection pooling (same as Shoal)
  - Length-prefixed postcard messages over QUIC streams
  - `send_to()`, `request_response()` (uni and bi streams)
  - `push_blob()`, `pull_blob()` — blob transfer with blake3 verification
  - `pull_dag_entries()` — DAG sync
- [ ] `GossipService` — adapted from Shoal:
  - `TopicId = blake3(network_id)` (derived from seed)
  - `subscribe_and_join()` with bootstrap peers
  - Receiver loop: dispatch DAG entries to engine via channel
  - `broadcast_payload()` for outgoing entries
- [ ] ALPN derivation: `network_alpn(network_id)` → `"murmur/0/<hex prefix>"`
  - Nodes with wrong seed can't even complete QUIC handshake

**Tests** (≥20):

- Message roundtrip serialization for every variant
- Two endpoints connect, exchange Ping/Pong
- Blob push + verify blake3 on receive
- Blob push with corruption → rejected
- DAG sync: two nodes, one has entries the other doesn't
- Gossip: broadcast entry, verify all subscribers receive it
- ALPN isolation: different network IDs can't connect
- Connection pooling: multiple messages reuse connection

---

### Milestone 5 — Engine

**Crate**: `murmur-engine`

**Goal**: The orchestrator that ties DAG and network together. The engine is
**storage-agnostic** — it does not read or write to disk. It communicates with
the platform via events and callbacks.

- [ ] `MurmurEngine` struct — the main entry point:
  - Owns: `Dag`, `MurmurTransport`, `GossipService`
  - Initialized with `NetworkIdentity` (from seed) + device keypair
  - The platform feeds persisted DAG entries on startup via `load_entry()`
- [ ] **Platform callbacks** (the engine calls these, the platform implements them):
  - `on_dag_entry(entry_bytes: Vec<u8>)` — persist a new DAG entry
  - `on_blob_received(blob_hash: BlobHash, data: Vec<u8>)` — store a received blob
  - `on_blob_needed(blob_hash: BlobHash) → Option<Vec<u8>>` — load a blob for transfer
  - `on_event(event: EngineEvent)` — notify UI of state changes
- [ ] **Device management**:
  - `create_network()` — first device, auto-approved
  - `join_network()` — new device, broadcasts join request
  - `approve_device(device_id)` — approve a pending device
  - `revoke_device(device_id)` — remove a device
  - `list_devices()` → `Vec<DeviceInfo>`
  - `pending_requests()` → `Vec<DeviceJoinRequest>`
- [ ] **Push sync**:
  - `add_file(blob_hash, metadata, data)` — the platform provides the hash and bytes,
    the engine creates a DAG entry, gossips it, pushes the blob to backup nodes
  - Push queue: retry on failure, skip already-synced
- [ ] **Pull access**:
  - `request_access(device_id, scope)` — send access request
  - `handle_access_request(from, scope)` → return grant or rejection
  - `fetch_blob(device_id, blob_hash)` — pull a blob using an active grant
- [ ] **DAG sync loop**:
  - On gossip receive: apply entry to local DAG, call `on_dag_entry` for persistence
  - Periodic full sync with known peers (pull missing entries)
  - On new peer join: full DAG transfer
- [ ] **Event system**:
  - `EngineEvent` enum: `DeviceJoinRequested`, `DeviceApproved`, `FileSynced`, `AccessRequested`, etc.

**Tests** (≥25):

- Create network → first device in peer list
- Join network → join request appears on existing device
- Approve device → new device in peer list on all devices
- Revoke device → device removed from peer list
- Add file → DAG entry created, `on_dag_entry` callback called
- Add duplicate file → deduplicated (no new DAG entry)
- Push file to backup → `on_blob_received` called on backup
- Request access → pending on target device
- Grant access → requester can fetch blob
- Deny access → requester gets rejection
- Access expiration → fetch after expiry fails
- Two devices add files concurrently → both appear after sync
- Full sync: new device joins, gets complete file list
- Callbacks: all platform callbacks are invoked correctly

---

### Milestone 6 — Server Daemon

**Crate**: `murmurd`

**Goal**: Headless backup server daemon. Runs on a NAS, Raspberry Pi, or VPS.
This is the first "platform implementation" — it implements the platform callbacks
that `murmur-engine` expects, using Fjall for DAG persistence and filesystem for blobs.

`murmurd` is typically the first device in the network: the user generates a mnemonic,
starts the daemon, and then connects phones/tablets to it.

- [ ] CLI (clap), minimal:
  - `murmurd init` — generate mnemonic, create network, store seed, start
  - `murmurd init --join <mnemonic>` — join existing network as backup node
  - `murmurd start` — start daemon (reads config from `~/.murmur/config.toml`)
  - `murmurd approve <device_id>` — approve pending device (one-shot command)
  - `murmurd status` — print network status, connected devices, then exit
- [ ] Config file (`~/.murmur/config.toml`):

  ```toml
  [device]
  name = "Home NAS"
  role = "backup"

  [storage]
  blob_dir = "/data/murmur/blobs"
  data_dir = "/data/murmur/db"       # Fjall
  ```

- [ ] Platform callbacks implementation:
  - `on_dag_entry` → persist to Fjall
  - `on_blob_received` → write to `blob_dir` (content-addressed: `ab/cd/<hash>`)
  - `on_blob_needed` → read from `blob_dir`
  - `on_event` → log via `tracing`
- [ ] Startup: load all DAG entries from Fjall → feed into engine via `load_entry()`
- [ ] Auto-approve mode (optional config flag): automatically approve new devices
      that present the correct network ID. Useful for home setups.
- [ ] Signal handling: graceful shutdown on SIGTERM/SIGINT
- [ ] Systemd service file example in `docs/`

**Tests** (≥10):

- Init creates config and data directories
- Init with `--join` and valid mnemonic succeeds
- Init with invalid mnemonic fails with clear error
- Config parsing roundtrip
- Blob storage: write and read back with blake3 verification
- DAG persistence: write entries, restart, verify they reload
- Daemon start/stop lifecycle
- Auto-approve mode: new device auto-approved when flag is set

---

### Milestone 7 — Integration Tests

**Directory**: `tests/`

**Goal**: End-to-end tests simulating real multi-device scenarios.

- [ ] **Two-device sync**: Device A creates network, device B joins, A approves, both sync files
- [ ] **Three-device topology**: Phone (source) → NAS (backup), Tablet requests access from Phone
- [ ] **Concurrent edits**: Two devices add files simultaneously, DAGs merge correctly
- [ ] **Offline reconnect**: Device goes offline, adds files, reconnects, syncs
- [ ] **Device revocation**: Revoked device can no longer sync or access
- [ ] **Access grant lifecycle**: Request → grant → use → expiry
- [ ] **Large file transfer**: Multi-MB file push and pull with integrity verification
- [ ] **Deduplication**: Same file added on two devices → stored once on backup
- [ ] **DAG convergence**: After network partition heals, all devices reach same state

**Tests** (≥20 integration tests):
All scenarios above, tested with in-memory transport (no real network needed).

---

## Future Milestones (not yet planned in detail)

### Milestone 8 — Mobile: Core Library (`murmur-core`)

- Build `murmur-core` as static library for Android (`cargo-ndk`) and iOS (xcframework)
- Define FFI boundary (UniFFI or manual C FFI — TBD)
- Expose: init, join, approve, list_devices, add_file, sync status, events callback
- **iroh on mobile**: iroh already cross-compiles and runs on Android and iOS natively.
  `n0-computer/iroh-ffi` provides UniFFI-based bindings (Swift, Kotlin, Python) but
  releases are currently **paused** — n0 recommends writing your own thin wrapper around
  iroh that exposes only what your app needs, rather than wrapping iroh's full API.
  This is exactly our approach: the FFI boundary is at the `murmur-engine` level, not
  at iroh. iroh is an internal implementation detail of the Rust core. The mobile apps
  call `murmur_create_network()`, `murmur_join()`, `murmur_approve()`, etc. — they
  never touch iroh types directly.

### Milestone 9 — Android App

- Kotlin wrapper around `murmur-core` native library
- Foreground Service for background sync
- DocumentsProvider for file manager integration (appear in Files app)
- UI: device management, photo gallery, sync status
- Camera intent / MediaStore integration for auto-upload
- Note: iroh's QUIC transport works on Android — the Rust core handles
  all networking. Kotlin only does OS integration (service lifecycle,
  file provider, notifications, UI).

### Milestone 10 — iOS App

- Swift wrapper around `murmur-core` native library
- File Provider Extension for Files.app integration
- Background App Refresh + BGProcessingTask for periodic sync
- NSURLSession background transfers for large files (when app is suspended)
- PhotoKit integration for camera roll auto-upload
- Note: same as Android — iroh runs natively, Swift only does OS glue.
  The iroh-ffi Swift package on SPM can be referenced for patterns, but
  we wrap murmur-engine, not iroh.

### Milestone 11 — Hardening

- Encrypted blob storage at rest (seed-derived key)
- Encrypted mnemonic/seed storage on disk
- Rate limiting, bandwidth throttling
- Compression (zstd) for blob transfer
- Chunking for large files (avoid loading entire file in memory)
- Progress reporting for large transfers

---

## Notes for Implementation

### Error Handling

- `thiserror` for library error types, `anyhow` only in `murmurd`
- Every error type should be descriptive with context

### Logging

- `tracing` throughout with structured fields
- Info: device joined/approved/revoked, file synced, access granted
- Debug: individual message sends/receives, DAG entry details
- Warn: sync failures, rejected entries, expired grants
- Error: network failures, storage corruption

### Relationship to Shoal

Murmur borrows heavily from Shoal's architecture:

- **DAG** (`murmur-dag`): Direct adaptation of `shoal-logtree`. Same `DagEntry` structure (hash, HLC, parents, signature), same tip management and merge logic. Different `Action` enum and materialized state. No `DagStore`/Fjall — the DAG is in-memory, the platform persists.
- **Gossip** (`murmur-net`): Same pattern as `shoal-cluster/gossip.rs`. TopicId from secret, subscribe_and_join, receiver loop with channel dispatch.
- **Transport** (`murmur-net`): Simplified from `shoal-net`. No shard transfer, replaced with blob push/pull. Same length-prefixed postcard-over-QUIC pattern.
- **ALPN isolation**: Same `cluster_alpn()` pattern — network-specific ALPN prevents cross-network connections.

### Things NOT to do yet

- No file versioning (just latest version, LWW)
- No conflict resolution UI (automatic LWW is fine for photos)
- No partial/chunked transfer (whole files for v1)
- No compression
- No encryption at rest
- No multi-network support (one mnemonic = one network per daemon)
- No relay/proxy nodes
- No desktop app (server daemon only for now, client apps are future milestones)
- No Web UI (daemon is headless, managed via CLI or client apps)
