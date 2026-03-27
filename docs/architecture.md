# Murmur — Architecture

## Overview

Murmur is a **private, peer-to-peer device synchronization network** written in Rust. It syncs files (photos, documents) between a user's devices — phone, tablet, laptop, NAS — without any third-party server.

A user generates a BIP39 mnemonic (12 or 24 words) that creates a private network. Devices join by entering the mnemonic and being approved by an existing device. Files flow automatically from sources (phone) to backup nodes (NAS), and any device can request temporary access to another device's files.

The name comes from a **murmuration** — the synchronized flight of starlings, where thousands of individuals move as one without central coordination.

---

## Design Principles

1. **Pure Rust core** — Zero C/C++ bindings in the core library. Cross-compilation to mobile targets is trivial.
2. **Seed-based identity** — A BIP39 mnemonic is the root of trust. Everything derives from it: network ID, ALPN, gossip topic.
3. **Device approval** — Knowing the seed lets you knock on the door; an existing device must open it.
4. **DAG-based state** — All mutations are recorded in a signed, hash-chained DAG (adapted from Shoal's LogTree).
5. **Bidirectional sync** — Push (automatic background upload) + Pull (on-demand access with approval).
6. **Offline-first** — Devices sync when they can. The DAG merges naturally when they reconnect.

---

## System Architecture

The Rust core is **protocol and logic only** — it handles networking, the DAG, cryptography, and sync state machines. It produces and consumes serialized bytes. Storage is **not** part of the core. Each platform decides how and where to persist DAG entries, blobs, and config.

```
┌─────────────────────────────────────────────────────────┐
│  Platform (owns storage, UI, OS integration)            │
│                                                         │
│  ┌───────────┐  ┌───────────┐   ┌─────────────────────┐ │
│  │ Android   │  │   iOS     │   │  Desktop (murmurd)  │ │
│  │ Kotlin    │  │  Swift    │   │  Rust CLI + iced UI │ │
│  │ Room/SQL  │  │ CoreData  │   │  Fjall + filesystem │ │
│  │ SAF       │  │ FileProv  │   │                     │ │
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
│                                                         │
│  FFI (UniFFI bindings for mobile)              murmur-ffi
└─────────────────────────────────────────────────────────┘
```

---

## Crate Map

```
crates/
  murmur-types/        Shared types, identifiers, HLC
  murmur-seed/         BIP39 mnemonic + HKDF key derivation
  murmur-dag/          Signed append-only DAG (in-memory)
  murmur-net/          iroh QUIC transport + gossip
  murmur-engine/       Orchestrator (sync, approval, blob transfer)
  murmur-ffi/          UniFFI bindings for mobile platforms
  murmur-ipc/          IPC protocol types (CliRequest/CliResponse) for daemon ↔ CLI
  murmur-cli/          CLI tool — manages murmurd via Unix socket IPC
  murmurd/             Headless daemon (no subcommands, managed via murmur-cli)
  murmur-desktop/      iced GUI desktop app
tests/
  integration/         Multi-device simulation tests
platforms/
  android/             Kotlin Android app (Jetpack Compose)
```

### murmur-types

All shared types, identifiers, and the hybrid logical clock.

| Type                  | Description                                                                                                              |
| --------------------- | ------------------------------------------------------------------------------------------------------------------------ |
| `DeviceId([u8; 32])`  | Ed25519 public key. Hex display. Doubles as iroh `NodeId`.                                                               |
| `BlobHash([u8; 32])`  | blake3 hash of file content. `from_data()` for in-memory, `from_reader()` for streaming (64 KiB buffers).                |
| `NetworkId([u8; 32])` | blake3(hkdf(seed, "murmur/network-id")). Identifies the private network.                                                 |
| `FolderId([u8; 32])`  | blake3(created_at ‖ created_by ‖ name). Unique folder identifier.                                                        |
| `SharedFolder`        | Folder metadata: folder_id, name, created_by, created_at.                                                                |
| `SyncMode`            | `ReadWrite` or `ReadOnly`. Per-device, per-folder subscription mode.                                                     |
| `FolderSubscription`  | A device's subscription to a folder: folder_id, device_id, mode.                                                         |
| `DeviceRole`          | `Source` (produces files), `Backup` (stores files), `Full` (both).                                                       |
| `DeviceInfo`          | Device metadata: ID, name, role, approval status, join timestamp.                                                        |
| `FileMetadata`        | File metadata: blob hash, folder_id, path (relative to folder root), size, MIME type, created_at, modified_at, origin.   |
| `ConflictInfo`        | Detected conflict: folder_id, path, competing versions, detection timestamp.                                             |
| `ConflictVersion`     | One side of a conflict: blob_hash, device_id, hlc, dag_entry_hash.                                                      |
| `AccessGrant`         | Scoped, time-limited access from one device to another. Signed by grantor.                                               |
| `AccessScope`         | `AllFiles`, `FilesByPrefix(String)`, `SingleFile(BlobHash)`.                                                             |
| `Action`              | 16-variant enum covering device management, folders, file sync, conflicts, access control, DAG maintenance.              |
| `HybridClock`         | Monotonic timestamp combining wall clock and logical counter. Thread-safe (AtomicU64).                                   |
| `GossipPayload`       | Envelope for gossip messages (DAG entries, blob transfer, membership events). Includes `BlobChunk` and `BlobChunkAck`.   |

### murmur-seed

BIP39 mnemonic generation and HKDF key derivation.

- `generate_mnemonic(WordCount)` — 12 or 24-word BIP39 mnemonic
- `parse_mnemonic(phrase)` — validate an existing mnemonic
- `NetworkIdentity` — derives all cryptographic material from a mnemonic:
  - `network_id` via HKDF(seed, "murmur/network-id") → blake3
  - `alpn` as `"murmur/0/" + hex(network_id)[..16]`
  - `first_device_key` via HKDF(seed, "murmur/first-device-key") → Ed25519 keypair
- `DeviceKeyPair` — random Ed25519 keypair for non-first devices. Serializable for disk persistence.

### murmur-dag

Signed append-only DAG, adapted from Shoal's LogTree. **In-memory only** — the platform persists entries.

**DagEntry** — the fundamental unit:

- Fields: `hash` (blake3), `hlc`, `device_id`, `action`, `parents` (DAG edges), `signature_r/s` (Ed25519)
- `new_signed()` — create, compute blake3 hash, sign with Ed25519
- `verify_hash()` / `verify_signature()` — cryptographic verification
- `to_bytes()` / `from_bytes()` — postcard serialization for persistence

**Dag** — the in-memory store:

- `load_entry()` — platform feeds a persisted entry on startup
- `append(action)` — create entry with current tips as parents, update tips, apply to materialized state
- `receive_entry()` — verify and apply an entry received from a peer
- `apply_sync_entries()` — batch receive with topological ordering
- `compute_delta(remote_tips)` — Kahn's algorithm to find entries the remote is missing
- `maybe_merge()` — auto-merge when multiple tips exist
- `is_ancestor(a, b)` — BFS through parent chain for conflict detection

**MaterializedState** — derived cache rebuilt by replaying DAG entries in topological order:

- `devices: BTreeMap<DeviceId, DeviceInfo>` — peer list
- `files: BTreeMap<(FolderId, String), FileMetadata>` — file index keyed by (folder, path)
- `folders: BTreeMap<FolderId, SharedFolder>` — shared folders
- `subscriptions: BTreeMap<(FolderId, DeviceId), FolderSubscription>` — per-device folder subscriptions
- `file_versions: BTreeMap<(FolderId, String), Vec<(BlobHash, u64)>>` — version chain per (folder, path)
- `conflicts: Vec<ConflictInfo>` — active file conflicts (concurrent edits detected by ancestry check)
- `grants: Vec<AccessGrant>` — active access grants

### murmur-net

iroh QUIC transport, gossip broadcast, and shared wire utilities.

**MurmurMessage** — 12 variants covering:

- DAG sync: `DagEntryBroadcast`, `DagSyncRequest/Response`
- Blob transfer: `BlobPush/PushAck`, `BlobRequest/Response`
- Access control: `AccessRequest/Response`
- Keepalive: `Ping/Pong`

**MurmurTransport** — iroh `Endpoint` wrapper:

- Length-prefixed postcard-over-QUIC messaging
- Connection pooling
- `push_blob()` / `pull_blob()` — blob transfer with blake3 verification
- `pull_dag_entries()` — DAG synchronization

**GossipHandle** — iroh-gossip wrapper:

- TopicId derived from NetworkId
- `subscribe_and_join()` / `broadcast_payload()`
- ALPN isolation: each network has a unique ALPN preventing cross-network connections

**Wire utilities** (`wire` module) — shared by `murmurd` and `murmur-ffi`:

- `compress_wire()` / `decompress_wire()` — deflate compression with 1-byte flag prefix (0=raw, 1=compressed). Only compresses payloads ≥256 bytes and only if compression saves space.
- `ChunkBuffer` — reassembly buffer for chunked blob transfers (blobs >4 MiB split into 1 MiB chunks)
- Constants: `CHUNK_THRESHOLD` (4 MiB), `CHUNK_SIZE` (1 MiB), `COMPRESS_THRESHOLD` (256 bytes), `MAX_BLOB_SIZE` (1 TiB advisory — no hard limit, streaming transfers handle arbitrarily large files)

### murmur-engine

The orchestrator tying DAG, network, and platform together. Storage-agnostic.

**MurmurEngine**:

- `create_network()` / `join_network()` — network lifecycle
- `approve_device()` / `revoke_device()` / `list_devices()` / `pending_requests()` — device management
- `add_file()` — file sync with dedup (loads full file in memory)
- `add_file_streaming(folder_id, path, file_path)` — streaming file sync from disk (two-pass: hash then stream, bounded memory). DAG entry is created only after blob is fully stored.
- `modify_file()` / `delete_file()` — file mutation with version tracking
- `create_folder()` / `remove_folder()` / `subscribe_folder()` / `unsubscribe_folder()` — folder management
- `list_folders()` / `folder_files()` / `folder_subscriptions()` / `file_history()` — folder queries
- `list_conflicts()` / `list_conflicts_in_folder()` / `resolve_conflict()` — conflict management
- `request_access()` / `handle_access_request()` — access control
- `compute_delta()` / `receive_sync_entries()` — DAG synchronization

**PlatformCallbacks** trait (implemented by each platform):

- `on_dag_entry(entry_bytes)` — persist a new DAG entry
- `on_blob_received(blob_hash, data)` — store a received blob (small blobs)
- `on_blob_needed(blob_hash) → Option<Vec<u8>>` — retrieve a stored blob
- `on_event(event)` — notify the UI/platform of engine events
- `on_blob_stream_start(blob_hash, total_size)` — prepare for streaming receive (e.g., create temp file)
- `on_blob_chunk(blob_hash, offset, data)` — receive a chunk of streaming blob
- `on_blob_stream_complete(blob_hash) → Result<(), String>` — verify and finalize streaming blob
- `on_blob_stream_abort(blob_hash)` — clean up aborted streaming transfer

**EngineEvent** — 15 variants: `DeviceJoinRequested`, `DeviceApproved`, `DeviceRevoked`, `FolderCreated`, `FolderSubscribed`, `FileSynced`, `FileModified`, `BlobReceived`, `AccessRequested`, `AccessGranted`, `DagSynced`, `NetworkCreated`, `NetworkJoined`, `BlobTransferProgress`, `ConflictDetected`.

### murmur-ffi

UniFFI 0.31 (proc-macro based) bindings exposing `murmur-engine` to mobile platforms.

- FFI wrapper types: `DeviceInfoFfi`, `FileMetadataFfi`, `AccessScopeFfi`, `MurmurEventFfi`
- `FfiPlatformCallbacks` — callback interface matching `PlatformCallbacks`
- `MurmurHandle` — wraps `Arc<Mutex<MurmurEngine>>` + owned `tokio::runtime::Runtime`. Thread-safe, synchronous FFI surface. The `Arc` enables sharing the engine with async networking tasks.
- Constructor functions: `new_mnemonic()`, `create_network()`, `join_network()`
- Generated bindings: Kotlin (for Android), Swift (for iOS)
- **Gossip networking**: `start()` creates an iroh endpoint, subscribes to gossip, and spawns background tasks for DAG sync and blob transfer — the same wire format as `murmurd`. `stop()` tears down networking. `connected_peers()` reports active gossip peers.

**Design**: The FFI boundary is at `murmur-engine`, not at iroh. Mobile code never sees iroh types. iroh is an internal implementation detail.

### murmur-ipc

Shared IPC types for daemon ↔ CLI communication over Unix socket.

- `CliRequest` — 8 variants: `Status`, `ListDevices`, `ListPending`, `ApproveDevice`, `RevokeDevice`, `ShowMnemonic`, `ListFiles`, `AddFile`
- `CliResponse` — 7 variants: `Status`, `Devices`, `Pending`, `Mnemonic`, `Files`, `Ok`, `Error`
- `socket_path()` — resolves to `~/.murmur/murmurd.sock`
- Wire format: length-prefixed postcard over Unix socket

### murmur-cli

CLI tool for managing a running `murmurd` instance.

- **Offline commands** (no daemon required): `join <mnemonic> [--name NAME]`
- **Online commands** (connects to daemon via Unix socket): `status`, `devices`, `pending`, `approve`, `revoke`, `mnemonic`, `files`, `add`
- Output: plain text by default, `--json` flag for machine-readable output

### murmurd

Headless daemon for NAS, Raspberry Pi, or VPS. Pure daemon — no subcommands, managed via `murmur-cli`.

- On first run (no config): auto-initializes — generates mnemonic, writes `config.toml`, prints mnemonic
- Config: `~/.murmur/config.toml` (device name, role, storage paths)
- Storage: Fjall for DAG persistence, content-addressed filesystem for blobs
- Startup: load all DAG entries from Fjall → feed into engine → clean up stale streaming temp files → start gossip networking → listen on Unix socket
- **Gossip networking**: creates an iroh endpoint, subscribes to a gossip topic derived from the network ID. Creator uses a deterministic iroh key (HKDF from mnemonic); joining devices use a random key and bootstrap with the creator's endpoint ID. DAG entries broadcast via gossip with deflate compression (shared wire format from `murmur-net`). On `NeighborUp`, peers exchange `DagSyncRequest` with their tips and receive only the delta. When a `FileAdded` entry arrives, the daemon requests missing blobs via `BlobRequest`/`BlobResponse`; large blobs (>4 MiB) use chunked transfer via `BlobChunk` messages with 5ms pacing between chunks.
- **Streaming blob storage**: incoming `BlobChunk` messages are written directly to a temp file (`~/.murmur/blobs/.tmp/`) instead of being reassembled in memory. On completion, streaming blake3 verification runs without loading the full file. For unencrypted blobs, atomic rename to final path. For encrypted blobs, chunked AEAD encryption (1 MiB chunks, per-chunk derived nonces, MCv1 format) keeps memory bounded.
- **Blob encryption at rest**: AES-256-GCM. Legacy blobs use single-nonce format (12-byte nonce + ciphertext). Streaming blobs use chunked MCv1 format (magic header + base nonce + per-chunk nonces derived via XOR). Both formats are transparently handled on read.
- **IPC socket**: accepts `CliRequest` from `murmur-cli`, produces `CliResponse`. Handles concurrent connections. Socket cleaned up on graceful shutdown; stale sockets detected and removed on startup.
- Signal handling: graceful shutdown on SIGTERM/SIGINT

### murmur-desktop

iced 0.14 GUI desktop app for linux desktops.

- **Setup screen**: device name, create/join network, mnemonic generation/entry
- **Devices tab**: approved devices with revoke, pending requests with approve
- **Files tab**: file list with metadata, add file by path
- **Status tab**: device ID, DAG entry count, event log
- Storage: same Fjall + filesystem pattern as murmurd

---

## Platform Implementations

### Platform Contract

Every platform must provide:

1. **Load** — feed persisted DAG entries into the engine on startup
2. **Callbacks** — persist new DAG entries and blobs when the engine produces them (including streaming callbacks for large files: `on_blob_stream_start`, `on_blob_chunk`, `on_blob_stream_complete`, `on_blob_stream_abort`)
3. **User decisions** — approve/reject device join requests, access requests

The engine provides:

1. **Serialized data** — DagEntry bytes, blob bytes (or streaming chunks) to persist
2. **Events** — device joined, file synced, conflict detected, access requested, etc.
3. **Functions** — add file (in-memory or streaming), manage folders, resolve conflicts, approve devices, etc.

### Desktop (murmurd + murmur-desktop)

| Concern          | Implementation                                                                              |
| ---------------- | ------------------------------------------------------------------------------------------- |
| DAG persistence  | Fjall v3 (embedded key-value store)                                                         |
| Blob storage     | Content-addressed filesystem (`~/.murmur/blobs/ab/cd/...`)                                  |
| Blob encryption  | AES-256-GCM at rest (single-nonce for small blobs, chunked MCv1 AEAD for streaming blobs)   |
| Streaming blobs  | Temp dir (`~/.murmur/blobs/.tmp/`), streaming blake3 verify, atomic rename on complete       |
| Config           | TOML file (`~/.murmur/config.toml`)                                                         |
| UI               | murmur-cli (via IPC) or iced GUI (murmur-desktop)                                           |

### Android

| Concern          | Implementation                                         |
| ---------------- | ------------------------------------------------------ |
| DAG persistence  | Room database (`DagEntryEntity` with hash + raw bytes) |
| Blob storage     | App-private filesystem (`filesDir/blobs/<aa>/<rest>`)  |
| Background sync  | Foreground Service (`MurmurService`, sticky)           |
| Auto-upload      | `ContentObserver` on `MediaStore.Images`               |
| File exposure    | `DocumentsProvider` (Android Files app integration)    |
| UI               | Jetpack Compose with ViewModels + StateFlow            |
| Boot persistence | `BootReceiver` (BOOT_COMPLETED, MY_PACKAGE_REPLACED)   |
| Rust integration | cargo-ndk → jniLibs (arm64-v8a, armeabi-v7a, x86_64)   |

---

## Key Data Types

```rust
// Identifiers
DeviceId([u8; 32])      // Ed25519 public key = iroh NodeId
BlobHash([u8; 32])      // blake3(file_content)
NetworkId([u8; 32])     // blake3(hkdf(seed, "murmur/network-id"))

// DAG entry (fundamental unit of state)
DagEntry {
    hash: [u8; 32],           // blake3(hlc, device_id, action, parents)
    hlc: u64,                 // hybrid logical clock
    device_id: DeviceId,      // author
    action: Action,           // what happened
    parents: Vec<[u8; 32]>,   // DAG edges
    signature_r: [u8; 32],    // ed25519 signature
    signature_s: [u8; 32],
}

// Actions (16 variants)
Action::DeviceJoinRequest { device_id, name }
Action::DeviceApproved { device_id, role }
Action::DeviceRevoked { device_id }
Action::DeviceNameChanged { device_id, name }
Action::FolderCreated { folder }
Action::FolderRemoved { folder_id }
Action::FolderSubscribed { folder_id, device_id, mode }
Action::FolderUnsubscribed { folder_id, device_id }
Action::FileAdded { metadata }
Action::FileModified { folder_id, path, old_hash, new_hash, metadata }
Action::FileDeleted { folder_id, path }
Action::ConflictResolved { folder_id, path, chosen_hash, discarded_hashes }
Action::AccessGranted { grant }
Action::AccessRevoked { to }
Action::Merge
Action::Snapshot { state_hash }
```

---

## Protocols

### Seed & Network Bootstrap

1. User generates or enters a BIP39 mnemonic (12 or 24 words)
2. Mnemonic → 64-byte seed via BIP39 (optional passphrase)
3. HKDF-SHA256 with domain separation derives:
   - `network_id` — gossip topic ID
   - `alpn` — QUIC ALPN for network isolation
   - `first_device_key` — Ed25519 keypair (first device only)
4. First device is auto-approved

### Device Join Flow

1. New device enters mnemonic → derives `network_id` + `alpn`
2. Generates fresh Ed25519 keypair → its `DeviceId`
3. Joins gossip topic, broadcasts `DeviceJoinRequest`
4. Any approved device can approve → creates `DeviceApproved` DAG entry
5. DAG entry propagates via gossip → all devices update peer list
6. New device syncs the full DAG from an existing peer

### Push Sync (automatic)

1. Source detects new file → compute `BlobHash = blake3(content)`
2. Dedup check against DAG
3. Create `FileAdded` DAG entry → broadcast via gossip
4. Transfer blob to backup nodes via iroh QUIC (direct stream)
5. Receiver verifies blake3, stores blob

### Pull Access (on-demand)

1. Device A sends `AccessRequest` to Device B (point-to-point QUIC)
2. Device B prompts user → creates `AccessGranted` DAG entry with scope and expiration
3. Device A fetches blobs directly from B via QUIC
4. Access expires automatically or can be revoked early

### DAG Sync & Merge

- Each device appends to its own branch of the DAG
- Entries reference current tips as parents → forms a DAG
- Gossip propagates new entries to all peers
- Multiple tips = concurrent branches → auto-merge entry collapses them
- Conflict resolution: LWW (Last-Writer-Wins) by HLC, tiebreak by DeviceId
- Materialized state reconstructed by replaying entries in topological order

---

## Technology Stack

### Core (pure Rust, no C dependencies)

| Purpose        | Crate                | Version |
| -------------- | -------------------- | ------- |
| Hashing        | `blake3`             | 1.x     |
| Networking     | `iroh`               | 0.96    |
| Gossip         | `iroh-gossip`        | 0.96    |
| BIP39          | `bip39`              | 2.x     |
| Key derivation | `hkdf` + `sha2`      | —       |
| Signing        | `ed25519-dalek`      | 2.x     |
| Serialization  | `postcard` + `serde` | 1.x     |
| Async          | `tokio`              | 1.x     |
| Logging        | `tracing`            | 0.1     |
| FFI            | `uniffi`             | 0.31    |

### Desktop only

| Purpose     | Crate       |
| ----------- | ----------- |
| Metadata DB | `fjall` v3  |
| CLI         | `clap` v4   |
| Desktop UI  | `iced` 0.14 |

### Android

| Purpose      | Library                             |
| ------------ | ----------------------------------- |
| UI           | Jetpack Compose (Material Design 3) |
| Database     | Room                                |
| Architecture | ViewModel + StateFlow               |
| Rust bridge  | cargo-ndk + UniFFI-generated Kotlin |

---

## Key Design Decisions

| Decision                 | Rationale                                                                               |
| ------------------------ | --------------------------------------------------------------------------------------- |
| BIP39 over random secret | Users can write down 12/24 words on paper. Same UX as Bitcoin wallets.                  |
| HKDF over BIP32          | No hierarchical derivation needed. HKDF with domain separation is simpler.              |
| iroh for networking      | QUIC with built-in hole punching and relay fallback. `NodeId` is already Ed25519.       |
| No storage in core       | Each platform has optimal storage (Fjall, Room, Core Data). Core deals with bytes only. |
| FFI at engine, not iroh  | Mobile code never touches iroh types. Stable, simple FFI surface.                       |
| UniFFI proc-macro        | No UDL file needed. Generates Kotlin + Swift from Rust annotations.                     |
| In-memory DAG            | Platform persists and feeds entries on startup. Core doesn't know how storage works.    |
| Postcard wire format     | Compact binary serialization. Deterministic for hashing.                                |
