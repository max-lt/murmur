# Murmur — Private Device Sync Network

## How to Use This Document

This is the **architecture and implementation plan** for Murmur. Work milestone by milestone, in order.
For each milestone: implement, test, `cargo clippy -- -D warnings`, `cargo fmt`, stop.

## Current Status (as of 2026-03-19)

| Milestone | Status |
| --------- | ------ |
| 0 — Workspace Setup | ✅ Complete |
| 1 — Types | ✅ Complete |
| 2 — Seed | ✅ Complete |
| 3 — DAG | ✅ Complete |
| 4 — Network | ✅ Complete |
| 5 — Engine | ✅ Complete |
| 6 — Server Daemon | ✅ Complete |
| 7 — Integration Tests | ✅ Complete |
| 8 — COSMIC Desktop App | ✅ Complete |
| 9 — FFI Core Library | 🔲 Next |
| 10 — Android App | 🔲 Planned |
| 11 — iOS App | 🔲 Planned |
| 12 — Hardening | 🔲 Planned |

The entire Rust core, server daemon, and COSMIC desktop app are implemented and tested.
Next step is exposing the core via FFI so mobile platforms can consume it.

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

### Milestone 0 — Workspace Setup ✅

**Goal**: Cargo workspace compiles, CI-ready structure.

- [x] Create workspace `Cargo.toml` with all members
- [x] Create all crate directories with stub `lib.rs` / `main.rs`
- [x] Ensure `cargo build`, `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check` all pass
- [x] Create `CLAUDE.md` and `docs/plan.md`
- [x] Create `.gitignore`

**Tests**: `cargo build` and `cargo test` succeed with zero warnings.

---

### Milestone 1 — Types ✅

**Crate**: `murmur-types`

**Goal**: All shared types, identifiers, and the HLC.

- [x] `DeviceId` — 32-byte newtype, `Debug`/`Display` with hex, `Serialize`/`Deserialize`
- [x] `BlobHash` — 32-byte newtype, same traits
- [x] `NetworkId` — 32-byte newtype
- [x] `DeviceRole` enum (`Source`, `Backup`, `Full`)
- [x] `DeviceInfo` struct
- [x] `FileMetadata` struct
- [x] `AccessGrant` and `AccessScope`
- [x] `Action` enum (all variants listed in Key Data Types above)
- [x] `HybridClock` — same implementation as Shoal (`tick()`, `witness()`, monotonic)
- [x] `GossipPayload` enum for gossip messages (DAG entries, membership events)

**Tests** (≥15):

- Roundtrip serialization (postcard) for every type
- DeviceId from Ed25519 key
- HLC monotonicity, witness advances clock
- Display/Debug formatting

---

### Milestone 2 — Seed ✅

**Crate**: `murmur-seed`

**Goal**: BIP39 mnemonic generation, seed derivation, key management.

- [x] Generate new mnemonic (12 or 24 words, user's choice)
- [x] Validate existing mnemonic
- [x] Mnemonic → 64-byte seed (BIP39 standard, optional passphrase)
- [x] HKDF derivation with domain separation:
  - `network_id`: `hkdf(seed, info="murmur/network-id")` → blake3 → `NetworkId`
  - `alpn`: `"murmur/0/" + hex(network_id)[..16]`
  - `first_device_key`: `hkdf(seed, info="murmur/first-device-key")` → Ed25519 `SigningKey`
- [x] `NetworkIdentity` struct that holds all derived values
- [x] `DeviceKeyPair` — generate random Ed25519 keypair for non-first devices
- [x] Persist device keypair to disk (plaintext for v1; encrypted at rest deferred to M11)

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

### Milestone 3 — DAG ✅

**Crate**: `murmur-dag`

**Goal**: Signed append-only DAG, adapted from Shoal's `shoal-logtree`. This is the core CRDT.
**The DAG is in-memory only.** The platform is responsible for persisting and loading entries.

- [x] `DagEntry` — same fields as Shoal's LogEntry, using our Action enum
- [x] `DagEntry::new_signed()` — create, hash (blake3), sign (ed25519)
- [x] `DagEntry::verify_hash()` and `verify_signature()`
- [x] `DagEntry::to_bytes()` / `from_bytes()` — postcard serialization for platform persistence
- [x] `Dag` struct — owns in-memory entry store, tips, clock, device_id, signing_key
- [x] `Dag::load_entry()` — platform feeds a persisted entry back on startup
- [x] `Dag::append()` — tick HLC, get tips as parents, sign, store in memory, update tips, apply to state, return new entry
- [x] `Dag::receive_entry()` — verify hash+sig, check parents exist, store, update tips, apply
- [x] `Dag::apply_sync_entries()` — batch receive with topological ordering
- [x] `Dag::compute_delta()` — given remote tips, compute entries to send (Kahn's toposort)
- [x] `Dag::maybe_merge()` — auto-merge when multiple tips exist
- [x] Materialized state: `apply_to_state()` for all Action variants

**Tests**: ≥30 passing.

---

### Milestone 4 — Network ✅

**Crate**: `murmur-net`

**Goal**: iroh QUIC transport + gossip, adapted from Shoal's `shoal-net` and `shoal-cluster`.

- [x] `MurmurMessage` enum (all variants: DagEntryBroadcast, DagSyncRequest/Response, BlobPush/PushAck/Request/Response, AccessRequest/Response, Ping/Pong)
- [x] `MurmurTransport` — iroh `Endpoint` wrapper with connection pooling, length-prefixed postcard over QUIC
- [x] `push_blob()`, `pull_blob()` — blob transfer with blake3 verification
- [x] `pull_dag_entries()` — DAG sync
- [x] `GossipService` — TopicId from network_id, subscribe_and_join, broadcast_payload
- [x] ALPN isolation: `network_alpn(network_id)` → `"murmur/0/<hex prefix>"`

**Tests**: ≥20 passing.

---

### Milestone 5 — Engine ✅

**Crate**: `murmur-engine`

**Goal**: The orchestrator that ties DAG and network together. Storage-agnostic; communicates with the platform via events and callbacks.

- [x] `MurmurEngine` struct — owns Dag, MurmurTransport, GossipService
- [x] Platform callbacks: `on_dag_entry`, `on_blob_received`, `on_blob_needed`, `on_event`
- [x] Device management: `create_network`, `join_network`, `approve_device`, `revoke_device`, `list_devices`, `pending_requests`
- [x] Push sync: `add_file` with deduplication and push queue
- [x] Pull access: `request_access`, `handle_access_request`, `fetch_blob`
- [x] DAG sync loop: gossip receive → apply → persist callback; periodic full sync
- [x] `EngineEvent` enum: DeviceJoinRequested, DeviceApproved, FileSynced, AccessRequested, etc.

**Tests**: ≥25 passing.

---

### Milestone 6 — Server Daemon ✅

**Crate**: `murmurd`

**Goal**: Headless backup server daemon. Runs on a NAS, Raspberry Pi, or VPS.

- [x] CLI: `murmurd init`, `murmurd init --join <mnemonic>`, `murmurd start`, `murmurd approve <device_id>`, `murmurd status`
- [x] Config file (`~/.murmur/config.toml`) with device name/role and storage paths
- [x] Platform callbacks: Fjall for DAG persistence, content-addressed filesystem for blobs
- [x] Startup: load all DAG entries from Fjall → feed into engine
- [x] Auto-approve mode (config flag)
- [x] Signal handling: graceful shutdown on SIGTERM/SIGINT
- [x] Systemd service file example in `docs/`

**Tests**: ≥10 passing.

---

### Milestone 7 — Integration Tests ✅

**Directory**: `tests/`

**Goal**: End-to-end tests simulating real multi-device scenarios.

- [x] Two-device sync: A creates network, B joins, A approves, both sync files
- [x] Three-device topology: Phone → NAS (backup), Tablet requests access from Phone
- [x] Concurrent edits: two devices add files simultaneously, DAGs merge correctly
- [x] Offline reconnect: device goes offline, adds files, reconnects, syncs
- [x] Device revocation: revoked device can no longer sync or access
- [x] Access grant lifecycle: request → grant → use → expiry
- [x] Large file transfer: multi-MB file push and pull with integrity verification
- [x] Deduplication: same file added on two devices → stored once on backup
- [x] DAG convergence: after network partition heals, all devices reach same state

**Tests**: ≥20 integration tests passing, using in-memory transport.

---

---

### Milestone 8 — COSMIC Desktop App

**Crate**: `murmur-desktop`

**Goal**: A graphical desktop application for Pop!\_OS (COSMIC) and other Linux desktops.
Built with [`iced`](https://iced.rs) — the pure-Rust UI toolkit that COSMIC is built on —
so it looks native on Pop!\_OS. This is the second "platform implementation" after `murmurd`,
providing a full GUI instead of a headless CLI.

Unlike `murmurd` (headless backup daemon), `murmur-desktop` is a user-facing app with device
management, file browsing, and sync status. It reuses the same storage pattern (Fjall + filesystem)
and implements the same `PlatformCallbacks` trait, with the addition of an event channel that
drives UI updates.

**Architecture**:

```
crates/murmur-desktop/
  Cargo.toml
  src/
    main.rs         # iced Application: state, Message, update, view
    storage.rs      # Fjall + filesystem + DesktopPlatform (PlatformCallbacks)
```

- [x] `iced` application with dark theme (matches COSMIC default)
- [x] **Setup screen**: device name input, create/join network toggle, mnemonic
  generation or entry, error display
- [x] **Main screen** with sidebar navigation:
  - **Devices tab**: approved device list with revoke, pending requests with approve
  - **Files tab**: file list with metadata, add file by path
  - **Status tab**: device ID, DAG entry count, event log
- [x] `DesktopPlatform` — implements `PlatformCallbacks`:
  - `on_dag_entry` → persist to Fjall
  - `on_blob_received` → write to content-addressed filesystem
  - `on_blob_needed` → read from filesystem (with blake3 verification)
  - `on_event` → push to `Arc<Mutex<Vec<EngineEvent>>>` → UI drains on update
- [x] Storage: same Fjall + filesystem pattern as `murmurd`
- [x] Persistent config at `~/.murmur-desktop/config.toml`; auto-loads on startup
  if already initialized
- [x] File add: read from filesystem path, compute blake3, create `FileAdded` DAG entry

**Tests** (≥10):

- [x] Storage open creates directories
- [x] Blob store and load roundtrip
- [x] Blob load with blake3 verification
- [x] Blob load missing returns None
- [x] DAG entry persist and reload
- [x] Multiple DAG entries persist across storage reopen
- [x] Platform callbacks: on\_dag\_entry persists
- [x] Platform callbacks: on\_blob\_received + on\_blob\_needed roundtrip
- [x] Platform callbacks: on\_event pushes to event queue
- [x] Config file roundtrip (TOML serialize/deserialize)

---

### Milestone 9 — FFI Core Library (`murmur-ffi`) (was M8)

**Crate**: `murmur-ffi` (new crate, added to workspace)

**Goal**: Expose `murmur-engine` to mobile platforms via a thin, stable C-compatible FFI
using [UniFFI](https://mozilla.github.io/uniffi-rs/). This is the **only** boundary mobile
apps cross. iroh is an internal detail — mobile code never sees iroh types.

**FFI surface** (defined in `murmur.udl`):

```udl
namespace murmur {
    MurmurHandle create_network(string device_name, string mnemonic, PlatformCallbacks callbacks);
    MurmurHandle join_network(string device_name, string mnemonic, PlatformCallbacks callbacks);
};

interface MurmurHandle {
    void load_dag_entry(bytes entry_bytes);
    void start();
    void stop();
    void approve_device(string device_id_hex);
    void revoke_device(string device_id_hex);
    sequence<DeviceInfoFfi> list_devices();
    sequence<DeviceInfoFfi> pending_requests();
    void add_file(bytes blob_hash, FileMetadataFfi metadata, bytes data);
    void request_access(string device_id_hex, AccessScopeFfi scope);
    bytes? fetch_blob(string device_id_hex, bytes blob_hash);
};

callback interface PlatformCallbacks {
    void on_dag_entry(bytes entry_bytes);
    void on_blob_received(bytes blob_hash, bytes data);
    bytes? on_blob_needed(bytes blob_hash);
    void on_event(MurmurEventFfi event);
};
```

- [ ] `crates/murmur-ffi/` — new crate with `uniffi` as build dependency
- [ ] `murmur.udl` — UniFFI definition file
- [ ] Thin wrapper types: `DeviceInfoFfi`, `FileMetadataFfi`, `AccessScopeFfi`, `MurmurEventFfi`
  - All byte arrays as `Vec<u8>`, all IDs as hex strings or raw bytes
  - No iroh types, no `ed25519-dalek` types crossing the boundary
- [ ] `MurmurHandle` — wraps `Arc<Mutex<MurmurEngine>>`, thread-safe
- [ ] Async bridging: `tokio::runtime::Runtime` owned by `MurmurHandle`, all async engine
  calls driven from it. FFI surface is fully synchronous (mobile calls from any thread).
- [ ] Build targets:
  - Android: `cargo-ndk` → `jniLibs/` (arm64-v8a, armeabi-v7a, x86_64)
  - iOS: `cargo-lipo` or `xcodebuild` xcframework (arm64 device + arm64-sim + x86_64-sim)
- [ ] `build.rs` to invoke `uniffi_bindgen` → generate Kotlin + Swift bindings

**Build instructions** (in `crates/murmur-ffi/README.md`):

```bash
# Android
cargo ndk -t arm64-v8a -t armeabi-v7a -t x86_64 -o ./jniLibs build --release -p murmur-ffi

# iOS (xcframework)
cargo build --release --target aarch64-apple-ios -p murmur-ffi
cargo build --release --target aarch64-apple-ios-sim -p murmur-ffi
xcodebuild -create-xcframework \
  -library target/aarch64-apple-ios/release/libmurmur_ffi.a \
  -library target/aarch64-apple-ios-sim/release/libmurmur_ffi.a \
  -output MurmurCore.xcframework
```

**Tests** (≥10):

- [ ] All FFI wrapper types roundtrip through their native representations
- [ ] `create_network()` returns a valid handle
- [ ] `join_network()` with invalid mnemonic returns error (not panic)
- [ ] `approve_device()` on unknown device_id returns error (not panic)
- [ ] `add_file()` invokes `on_dag_entry` callback
- [ ] `on_blob_needed` callback returning `None` causes `fetch_blob` to return `None`
- [ ] `list_devices()` returns correct FFI structs after approve
- [ ] Thread safety: concurrent calls from multiple threads don't deadlock
- [ ] `start()` + `stop()` lifecycle — no crash, tokio runtime shuts down cleanly
- [ ] FFI string/bytes encoding is UTF-8 and length-prefixed (UniFFI guarantees)

---

### Milestone 10 — Android App (was M9)

**Directory**: `platforms/android/`

**Goal**: Kotlin Android application that wraps `murmur-ffi`. The app manages photos and
documents, syncing them automatically to the NAS via murmurd. iroh handles all networking
inside the Rust core — Kotlin only does Android OS integration.

**Architecture**:

```
platforms/android/
  app/
    src/main/
      java/net/murmur/app/
        MurmurService.kt       # Foreground Service — runs the engine
        MurmurEngine.kt        # Kotlin wrapper around MurmurHandle (FFI)
        DeviceViewModel.kt     # StateFlow-based ViewModel for device list
        FileViewModel.kt       # StateFlow-based ViewModel for file list
        ui/
          MainActivity.kt
          DeviceScreen.kt      # Approve/revoke devices
          FileScreen.kt        # Photo/file grid
          SyncStatusBar.kt
      jniLibs/                 # .so files from cargo-ndk
      assets/
        murmur.udl             # copied for reference
  build.gradle.kts
  settings.gradle.kts
```

- [ ] Gradle project setup: `cargo-ndk` build task in `build.gradle.kts`
- [ ] `MurmurService` — Android Foreground Service:
  - Starts on boot (BOOT_COMPLETED receiver)
  - Holds a `MurmurHandle` — calls `start()` on service start, `stop()` on destroy
  - Implements `PlatformCallbacks`:
    - `on_dag_entry` → Room database insert
    - `on_blob_received` → write to app-private storage (`filesDir/blobs/`)
    - `on_blob_needed` → read from `filesDir/blobs/`
    - `on_event` → broadcast LocalBroadcastManager intent → update UI via ViewModel
  - Loads all DAG entries from Room on startup → calls `load_dag_entry()` for each
- [ ] Room database:
  - `DagEntryEntity(hash TEXT PRIMARY KEY, data BLOB)` — stores raw postcard bytes
  - `DagEntryDao` with insert + loadAll
- [ ] `DocumentsProvider` subclass — exposes synced files in Android Files app:
  - `queryRoots()` → one root per network
  - `queryChildDocuments()` → list files from engine's `list_files()`
  - `openDocument()` → stream blob from `filesDir/blobs/`
- [ ] MediaStore / PhotoPicker integration for auto-upload:
  - `ContentObserver` on `MediaStore.Images` → detects new photos
  - Hashes new photo, calls `engine.add_file()`
- [ ] UI (Jetpack Compose):
  - Device list screen: pending requests with approve/deny buttons
  - File grid screen: thumbnails of synced files
  - Sync status notification with progress
- [ ] Notification channel for sync status and join requests

**Tests** (≥10):

- [ ] `MurmurService` starts and binds without crash
- [ ] `on_dag_entry` callback persists to Room
- [ ] Startup loads Room entries and calls `load_dag_entry()`
- [ ] `on_blob_received` writes file to expected path
- [ ] `on_blob_needed` reads file back correctly
- [ ] `DocumentsProvider.queryRoots()` returns correct root
- [ ] `DocumentsProvider.queryChildDocuments()` lists synced files
- [ ] New photo in MediaStore triggers `add_file()`
- [ ] Approve device flow: join request appears in UI → tap approve → device approved
- [ ] Service survives process death and restarts (WorkManager or sticky service)

---

### Milestone 11 — iOS App (was M10)

**Directory**: `platforms/ios/`

**Goal**: Swift iOS application wrapping `murmur-ffi` via the generated Swift bindings.
iroh runs natively in the Rust core; Swift only handles iOS OS integration.

**Architecture**:

```
platforms/ios/
  MurmurApp/
    MurmurApp.swift           # @main, App lifecycle
    MurmurEngine.swift        # Swift wrapper around MurmurHandle
    MurmurService.swift       # Background task management
    PlatformCallbacksImpl.swift # implements PlatformCallbacks protocol
    Persistence/
      CoreDataStack.swift     # DAG entry persistence
      DagEntry+CoreData.swift
      BlobStore.swift         # Content-addressed blob storage in app container
    UI/
      DeviceListView.swift
      FileGridView.swift
      SyncStatusView.swift
    FileProvider/
      FileProviderExtension.swift
      FileProviderItem.swift
  MurmurCore.xcframework/     # built from murmur-ffi
  Package.swift               # SwiftPM (if using SPM for the framework)
```

- [ ] Xcode project + SwiftPM integration for `MurmurCore.xcframework`
- [ ] `MurmurEngine.swift` — wraps `MurmurHandle`, manages object lifecycle
- [ ] `PlatformCallbacksImpl` — implements the generated `PlatformCallbacks` protocol:
  - `onDagEntry(entryBytes:)` → Core Data insert
  - `onBlobReceived(blobHash:data:)` → write to `FileManager.default.containerURL()/blobs/`
  - `onBlobNeeded(blobHash:)` → read from same location
  - `onEvent(event:)` → post `NotificationCenter` notification → update SwiftUI state
- [ ] Core Data model:
  - `DagEntryMO`: `hash: String` (PK), `data: Data`
  - On startup: fetch all → call `loadDagEntry()` for each
- [ ] File Provider Extension (`MurmurFileProvider`):
  - `NSFileProviderManager` + `NSFileProviderItem` per synced file
  - `startProvidingItem(at:completionHandler:)` → `fetchBlob()` on demand
  - `importDocument(at:toParentItemIdentifier:completionHandler:)` → `addFile()`
- [ ] Background sync:
  - `BGProcessingTask` for periodic full DAG sync when app is in background
  - `BGAppRefreshTask` for lightweight heartbeat (check for pending approvals)
  - `NSURLSession` background transfer for large blobs (delegates to Rust core)
- [ ] PhotoKit integration:
  - `PHPhotoLibraryChangeObserver` → detect new photos
  - Hash + `addFile()` on new asset
- [ ] SwiftUI views: device list with approve sheet, file grid, sync status bar

**Tests** (≥10):

- [ ] `MurmurEngine` initializes without crash
- [ ] `onDagEntry` persists to Core Data
- [ ] Startup loads Core Data entries correctly
- [ ] `onBlobReceived` writes to expected path in container
- [ ] `onBlobNeeded` reads blob back
- [ ] File Provider item count matches `listFiles()` count
- [ ] `startProvidingItem` triggers `fetchBlob()`
- [ ] PhotoKit observer fires on new photo → `addFile()` called
- [ ] `BGProcessingTask` handler runs without crashing
- [ ] Approve device flow: join request notification → approve → device in list

---

### Milestone 12 — Hardening (was M11)

**Crates**: across all crates, primarily `murmur-engine`, `murmurd`, `murmur-ffi`

**Goal**: Production-quality security, performance, and reliability. No new public API —
all changes are internal improvements and new configuration options.

**Security**:

- [ ] **Encrypted blob storage at rest**: AES-256-GCM with a key derived via
  `hkdf(seed, info="murmur/blob-encryption-key")`. `murmurd` and mobile platforms
  encrypt before write, decrypt after read. Core is unchanged (bytes in = bytes out).
- [ ] **Encrypted seed/keypair on disk**: In `murmurd`, wrap the stored seed and device
  keypair with a password-derived key (Argon2id). CLI prompts for password on `start`.
  Optional: keyring integration (Secret Service on Linux, Keychain on macOS/iOS, Keystore on Android).
- [ ] **Gossip message authentication**: already handled by DAG signatures, but add
  explicit sender verification in `GossipService` to reject messages from unknown devices
  before they reach the DAG layer.

**Performance**:

- [ ] **Chunked blob transfer**: split blobs > 4 MB into 1 MB chunks. Stream chunks over
  the QUIC connection instead of buffering the entire file in memory. Sender and receiver
  both process chunk-by-chunk. Add `BlobPushChunk` / `BlobPullChunk` message variants.
- [ ] **zstd compression**: compress DAG entry bytes before gossip broadcast and before
  writing to Fjall. Add `Compressed { algorithm, data }` wrapper in `MurmurMessage`.
  For blobs, compress text/JSON/document MIME types; skip already-compressed types
  (JPEG, PNG, MP4).
- [ ] **Bandwidth throttling**: configurable upload/download rate limit in `murmurd`
  config (tokens-per-second bucket). Primarily for NAS deployments on metered links.
- [ ] **Push queue persistence**: in `murmurd`, store the pending push queue in Fjall
  so it survives daemon restarts. Currently in-memory only.

**Observability**:

- [ ] **Progress events**: add `EngineEvent::BlobTransferProgress { blob_hash, bytes_sent, total_bytes }`
  so UIs can show a real progress bar for large transfers.
- [ ] **Metrics**: optional `prometheus` feature flag in `murmurd` exposing:
  - `murmur_dag_entries_total` (counter)
  - `murmur_blobs_stored_bytes` (gauge)
  - `murmur_sync_duration_seconds` (histogram)
  - `murmur_connected_peers` (gauge)
- [ ] **Health endpoint**: optional HTTP endpoint (`--http-port`) in `murmurd` serving
  `/health` (JSON: status, peer count, last sync time) for monitoring systems.

**Reliability**:

- [ ] **DAG compaction**: after N entries, emit a `Snapshot` entry capturing the full
  `MaterializedState`. Peers that are far behind can fast-forward to the snapshot instead
  of replaying the full history. Old entries before the snapshot can be archived.
- [ ] **Retry with backoff**: push queue retries should use exponential backoff with jitter
  (currently linear). Max retry interval: 30 minutes. Persist retry count in Fjall.
- [ ] **Peer discovery via mDNS**: use `mdns-sd` crate for LAN peer discovery, supplementing
  iroh's relay-based discovery. Reduces latency for local network syncs significantly.

**Tests** (≥15):

- [ ] Encrypted blob: write encrypted, read back, verify plaintext matches
- [ ] Encrypted blob: tampered ciphertext → decryption error, blob rejected
- [ ] Encrypted seed: persist encrypted, reload with correct password → success
- [ ] Encrypted seed: reload with wrong password → error
- [ ] Chunked transfer: 8 MB file sent in 8 chunks, reassembled correctly on receiver
- [ ] Chunked transfer: corruption in chunk 3 → rejected, blake3 mismatch reported
- [ ] zstd roundtrip: compress DAG entry, decompress, verify identical
- [ ] Bandwidth throttle: upload speed stays within configured limit (±10%)
- [ ] Push queue persists across daemon restart: pending blobs re-queued on startup
- [ ] Progress events: `BlobTransferProgress` events fired for large transfers
- [ ] Snapshot entry: create snapshot, new peer joins, syncs via snapshot, not full history
- [ ] mDNS discovery: two daemons on LAN discover each other without relay

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
