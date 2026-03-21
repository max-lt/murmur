# Murmur — Implementation Plan

## How to Use This Document

This is the **implementation plan** for remaining Murmur milestones. Work milestone by milestone, in order.
For each milestone: implement, test, `cargo clippy -- -D warnings`, `cargo fmt`, stop.

For architecture and design details, see [architecture.md](architecture.md).
For a feature overview, see [features.md](features.md).

## Current Status (as of 2026-03-20)

| Milestone               | Status                                                                                                |
| ----------------------- | ----------------------------------------------------------------------------------------------------- |
| 0–10                    | ✅ Complete (Types, Seed, DAG, Network, Engine, Server, Integration Tests, Desktop App, FFI, Android) |
| 11 — Daemon + CLI Split | ✅ Complete (murmur-ipc, murmur-cli, murmurd refactor, gossip networking)                             |
| 12 — Hardening          | 🔲 Planned                                                                                            |

---

## Milestone 12 — Hardening

**Crates**: across all crates, primarily `murmur-engine`, `murmur-net`, `murmurd`, `murmur-ffi`

**Goal**: Production-quality security, performance, and reliability. Fix fundamental
networking gaps (blob sync, efficient delta sync), harden cryptographic operations,
and add observability. Implementation order matters — items within each section are
listed in dependency order.

**Current state context** (verified 2026-03-21):
- Blob transfer: `BlobPush`/`BlobRequest`/`BlobResponse` message types exist in
  `murmur-net` but are **never used** in `murmurd/networking.rs`. Only `DagEntryBroadcast`
  is handled. Peers learn about files via DAG entries but cannot fetch blob data.
- Peer sync: on `NeighborUp`, the daemon broadcasts **all** entries O(n). The
  `DagSyncRequest`/`DagSyncResponse` messages exist but are unused.
- Push queue: **does not exist** (not even in-memory). No retry logic of any kind.
- Mnemonic and device key stored as **plaintext** files (`~/.murmur/mnemonic`, `device.key`).
- Snapshot action type exists in `Action` enum but nothing creates or consumes snapshots.
  `MaterializedState` has no serialization support.

### 12a — Networking Foundations (prerequisite for everything else)

These items fill gaps in the current networking that other hardening items depend on.

- [ ] **Efficient delta sync on peer connect**: replace the O(n) `NeighborUp` full-broadcast
      with a tip-exchange protocol. On `NeighborUp`: (1) send `DagSyncRequest { tips }`,
      (2) peer responds with `DagSyncResponse { entries }` containing only the delta,
      (3) both sides call `receive_sync_entries()`. The message types already exist in
      `murmur-net/message.rs` — implement the handler in `murmurd/networking.rs`.
- [ ] **Blob sync between peers**: when a device receives a `FileAdded` DAG entry via
      gossip, it should request the blob from the sender. Implement a blob request/response
      flow in `murmurd/networking.rs` using the existing `BlobRequest`/`BlobResponse`
      messages. Track which blobs are needed (files in DAG but not on local disk) and
      request them from connected peers. Store via `on_blob_received()` callback.
- [ ] **Push queue with persistence**: in `murmurd`, maintain a queue of blobs pending
      transfer to peers (populated when `FileAdded` is appended locally). Persist the queue
      in a Fjall keyspace (`pending_blobs`) so it survives daemon restarts. On peer connect,
      push pending blobs. On ack (`BlobPushAck { ok: true }`), remove from queue.
- [ ] **Retry with exponential backoff**: push queue retries use exponential backoff with
      jitter. Base interval 5s, max interval 30 minutes. Persist retry count per blob in
      Fjall. Reset retry count on successful delivery.

### 12b — Security

- [ ] **DAG entry authorization**: currently any device with a valid signing key can
      broadcast any action — even revoked devices. Add authorization checks in
      `Dag::receive_entry()`: only approved devices may append `FileAdded`, `DeviceApproved`,
      `AccessGranted`, etc. `DeviceJoinRequest` is allowed from unapproved devices.
      Revoked device entries are rejected with a new `DagError::Unauthorized` variant.
- [ ] **Gossip sender verification**: add `sender_device_id: DeviceId` field to
      `GossipMessage`. In `handle_gossip_message()`, verify that the sender is a known
      (approved or pending) device before deserializing the DAG entry. Reject messages
      from unknown senders early to reduce attack surface.
- [ ] **Encrypted blob storage at rest**: AES-256-GCM with a key derived via
      `hkdf(seed, info="murmur/blob-encryption-key")`. Add the HKDF path in `murmur-seed`.
      In `murmurd/storage.rs`, `FjallPlatform` encrypts before `store_blob()` and decrypts
      after `load_blob()`. Core crates are unchanged (bytes in = bytes out). Use the
      `aes-gcm` crate (pure Rust, RustCrypto). Random 96-bit nonce per blob, prepended to
      ciphertext.
- [ ] **Encrypted seed/keypair on disk**: in `murmurd`, wrap the stored mnemonic and device
      key with a password-derived key (Argon2id → AES-256-GCM). `murmur-cli start` prompts
      for password. Use `argon2` crate (pure Rust). Store Argon2 params (salt, memory, iterations)
      alongside the ciphertext. Optional: keyring integration (Secret Service on Linux,
      Keychain on macOS/iOS, Keystore on Android) — behind a feature flag.
- [ ] **Secure memory zeroization**: use the `zeroize` crate to zero sensitive data on drop.
      Apply to: mnemonic strings in `murmurd`, raw seed bytes in `murmur-seed`'s
      `NetworkIdentity`, device key bytes loaded from disk, and any decrypted plaintext
      buffers. `ed25519-dalek::SigningKey` already implements `Zeroize`. Pure Rust.

### 12c — Performance

- [ ] **Chunked blob transfer**: split blobs > 4 MB into 1 MB chunks. Add
      `BlobPushChunk { blob_hash, chunk_index, total_chunks, data }` and
      `BlobPullChunk { blob_hash, chunk_index, data }` variants to `MurmurMessage`.
      Sender streams chunks over the QUIC connection; receiver reassembles and verifies
      the full blake3 hash before accepting. Add `EngineEvent::BlobTransferProgress`
      for UI progress bars. Depends on blob sync (12a) being implemented first.
- [ ] **Compression for DAG gossip**: compress DAG entry bytes before gossip broadcast
      using `ruzstd` (pure Rust zstd decompressor) + `zstd` (C-backed, `murmurd`-only for
      compression). Add a `Compressed { data }` variant to `GossipPayload`. Decompression
      in core crates uses `ruzstd` (pure Rust); compression in `murmurd` uses `zstd` for
      speed. Skip compression for entries < 256 bytes. For blob data, compress only
      text/JSON/document MIME types; skip already-compressed types (JPEG, PNG, MP4, ZIP).
      **Alternative**: use `flate2` with the `miniz_oxide` backend (pure Rust) if the
      `zstd`/`ruzstd` split is too complex — simpler at the cost of ~15% worse ratio.
- [ ] **Bandwidth throttling**: configurable upload/download rate limit in `murmurd`
      config. Token-bucket algorithm with configurable `max_upload_bytes_per_sec` and
      `max_download_bytes_per_sec`. Primarily for NAS deployments on metered links.
      Add `[network.throttle]` section to `config.toml`.
- [ ] **Parallel blob transfers**: when multiple blobs need syncing, transfer up to N
      concurrently (configurable, default 4). Use `tokio::sync::Semaphore` to bound
      concurrency. Significantly improves throughput for many small files.

### 12d — Observability

- [ ] **Progress events**: add `EngineEvent::BlobTransferProgress { blob_hash, bytes_sent,
      total_bytes }` to `murmur-engine`. Expose via `PlatformCallbacks::on_event()` and
      through `murmur-ffi` as `MurmurEventFfi::BlobTransferProgress`. Add
      `CliRequest::TransferStatus` / `CliResponse::TransferStatus` to `murmur-ipc` so the
      CLI can show real-time progress.
- [ ] **Metrics**: optional `prometheus` feature flag in `murmurd` exposing:
  - `murmur_dag_entries_total` (counter)
  - `murmur_blobs_stored_bytes` (gauge)
  - `murmur_blobs_pending_sync` (gauge — push queue size)
  - `murmur_sync_duration_seconds` (histogram)
  - `murmur_connected_peers` (gauge)
  - `murmur_blob_transfer_bytes_total` (counter, label: direction=upload|download)
- [ ] **Health endpoint**: optional HTTP endpoint (`--http-port`) in `murmurd` serving
      `/health` (JSON: status, peer count, last sync time, pending blob count) and
      `/metrics` (Prometheus format, if feature enabled). Use `axum` (already fits the
      desktop-only dep pattern).

### 12e — Reliability

- [ ] **DAG compaction / snapshots**: implement `MaterializedState` serialization (postcard).
      After every N entries (configurable, default 500), emit a `Snapshot` entry with
      `state_hash = blake3(serialized_state)`. Store the serialized state alongside the
      snapshot entry in Fjall. Peers that are far behind can fast-forward: load snapshot
      state directly instead of replaying all preceding entries. Old pre-snapshot entries
      can be archived (moved to a separate keyspace) but not deleted (they're still valid
      for audit). Add `Dag::load_snapshot(state, snapshot_entry)` method.
- [ ] **Blob integrity check on startup**: on daemon start, optionally verify all stored
      blobs against their blake3 hashes. `Storage::verify_all_blobs() -> Vec<BlobHash>`
      returns corrupted blob hashes. Log warnings and re-request corrupted blobs from peers.
      Controlled by `[storage] verify_on_startup = true` config flag (default false, since
      it's O(n) in blob count and size).
- [ ] **Peer discovery via mDNS**: use `mdns-sd` crate (pure Rust) for LAN peer discovery,
      supplementing iroh's relay-based discovery. Broadcast the gossip topic as a mDNS
      service; discover peers and add them to the gossip bootstrap set. Reduces latency
      for local network syncs significantly. Behind `[network] mdns = true` config flag.
- [ ] **Graceful shutdown**: on SIGTERM/SIGINT, drain in-flight blob transfers (wait up to
      10s), flush the push queue to Fjall, then shut down. Currently the daemon drops
      everything immediately on Ctrl-C.

### New dependencies (all verified pure Rust for core crates)

| Crate      | Purpose                 | Core-safe | Notes                                    |
| ---------- | ----------------------- | --------- | ---------------------------------------- |
| `aes-gcm`  | Blob/seed encryption    | ✅         | RustCrypto, pure Rust                    |
| `argon2`   | Password key derivation | ✅         | RustCrypto, pure Rust                    |
| `zeroize`  | Secure memory clearing  | ✅         | RustCrypto, pure Rust                    |
| `ruzstd`   | zstd decompression      | ✅         | Pure Rust decompressor                   |
| `zstd`     | zstd compression        | ❌         | C bindings — `murmurd` only, not in core |
| `mdns-sd`  | mDNS peer discovery     | ✅         | Pure Rust                                |
| `axum`     | HTTP health/metrics     | ❌         | `murmurd` only (feature-gated)           |
| `flate2`   | Alt. compression        | ✅         | With `miniz_oxide` backend, pure Rust    |

### Tests (≥20)

**Networking foundations:**
- [ ] Delta sync: two peers exchange tips, only missing entries transferred
- [ ] Delta sync: peer with empty DAG receives full history via sync, not brute broadcast
- [ ] Blob request: peer receives FileAdded entry, requests blob, receives and stores it
- [ ] Blob request: request for unknown blob returns `BlobResponse { data: None }`
- [ ] Push queue: add file, blob appears in pending queue, push to peer, removed from queue
- [ ] Push queue persists across daemon restart: pending blobs re-queued on startup
- [ ] Retry backoff: failed push retries with increasing delay, caps at 30 minutes

**Security:**
- [ ] DAG authorization: entry from revoked device is rejected with `Unauthorized`
- [ ] DAG authorization: `DeviceJoinRequest` from unapproved device is accepted
- [ ] Gossip sender verification: message from unknown device is dropped before DAG processing
- [ ] Encrypted blob: write encrypted, read back, verify plaintext matches
- [ ] Encrypted blob: tampered ciphertext → decryption error, blob rejected
- [ ] Encrypted seed: persist encrypted, reload with correct password → success
- [ ] Encrypted seed: reload with wrong password → error
- [ ] Zeroize: after `NetworkIdentity` is dropped, seed bytes are zeroed (debug assertion)

**Performance:**
- [ ] Chunked transfer: 8 MB file sent in 8 chunks, reassembled correctly on receiver
- [ ] Chunked transfer: corruption in chunk 3 → rejected, blake3 mismatch reported
- [ ] Compression roundtrip: compress DAG entry, decompress, verify identical
- [ ] Bandwidth throttle: upload speed stays within configured limit (±10%)

**Reliability:**
- [ ] Snapshot: create snapshot, new peer fast-forwards via snapshot, not full replay
- [ ] Blob integrity: corrupted blob on disk detected by `verify_all_blobs()`
- [ ] mDNS discovery: two daemons on LAN discover each other without relay
- [ ] Graceful shutdown: in-flight transfer completes before daemon exits

# DO NOT DO NOW, FOR LATER ONLY

## iOS App

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

**Tasks**:

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

**Build instructions** (xcframework from murmur-ffi):

```bash
cargo build --release --target aarch64-apple-ios -p murmur-ffi
cargo build --release --target aarch64-apple-ios-sim -p murmur-ffi
xcodebuild -create-xcframework \
  -library target/aarch64-apple-ios/release/libmurmur_ffi.a \
  -library target/aarch64-apple-ios-sim/release/libmurmur_ffi.a \
  -output MurmurCore.xcframework
```

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
