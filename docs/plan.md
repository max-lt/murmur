# Murmur — Implementation Plan

## How to Use This Document

This is the **implementation plan** for remaining Murmur milestones. Work milestone by milestone, in order.
For each milestone: implement, test, `cargo clippy -- -D warnings`, `cargo fmt`, stop.

For architecture and design details, see [architecture.md](architecture.md).
For a feature overview, see [features.md](features.md).

## Current Status (as of 2026-03-27)

| Milestone                              | Status                                                                                                |
| -------------------------------------- | ----------------------------------------------------------------------------------------------------- |
| 0–10                                   | ✅ Complete (Types, Seed, DAG, Network, Engine, Server, Integration Tests, Desktop App, FFI, Android) |
| 11 — Daemon + CLI Split                | ✅ Complete (murmur-ipc, murmur-cli, murmurd refactor, gossip networking, FFI networking)             |
| 12 — Hardening                         | ✅ Complete (networking foundations, security, performance, observability, reliability)                |
| 13 — Folder Model & File Versioning    | ✅ Complete (types, DAG state, engine methods, auto-folder in murmurd, all tests pass)                |
| 14 — Conflict Detection & Resolution   | 🔲 Planned                                                                                           |
| 15 — Streaming Blob Storage & Transfer | 🔲 Planned                                                                                           |
| 16 — Filesystem Watching & Ignore      | 🔲 Planned                                                                                           |
| 17 — IPC & CLI Expansion               | 🔲 Planned                                                                                           |
| 18 — Desktop App (iced)                | 🔲 Planned                                                                                           |
| 19 — Web Dashboard (htmx)              | 🔲 Planned                                                                                           |
| 20 — Protocol Specification v0.1       | 🔲 Planned                                                                                           |

---

## Design Decisions (Milestones 13–20)

These decisions were agreed upon before implementation and guide all milestones below:

| Decision               | Choice                                                                                            |
| ---------------------- | ------------------------------------------------------------------------------------------------- |
| Folder model           | Syncthing-style shared folders mapped to real directories on each device                          |
| Selective sync         | Per-device subscribe model; choose folders on join, add/remove later                              |
| Folder permissions     | Self-selected: each device chooses read-write or read-only per folder                             |
| File modifications     | Explicit version chain (`FileModified` action in DAG, full history)                               |
| Conflict strategy      | Fork-based: keep both versions on disk, surface to user for resolution                            |
| Filesystem watching    | `notify` crate in murmurd, auto-detect changes in shared folder directories                       |
| Ignore patterns        | `.murmurignore` per folder (gitignore syntax), plus sensible defaults                             |
| Interfaces             | Desktop app (iced) + Web UI (htmx) + CLI — all thin clients calling murmurd via IPC               |
| Folder deletion        | Configurable: unsubscribing device chooses to keep or delete local files                          |
| Large files            | No hard size limit; streaming blake3, chunked storage/transfer, bounded memory                    |
| Web UI tech            | Server-rendered HTML with htmx, served by murmurd's axum server                                   |
| Protocol spec          | Phased: v0.1 alongside implementation, iterate as features stabilize                              |

---

## Milestone 13 — Folder Model, Paths & File Versioning

**Crates**: `murmur-types`, `murmur-dag`, `murmur-engine`

**Goal**: Introduce shared folders, file paths (relative to folder root), per-device folder subscriptions
with read-write/read-only mode, and file modification tracking with version history.
This is the foundational data model that all subsequent milestones build on.

**New types in `murmur-types`**:

```rust
/// Unique folder identifier — blake3(created_at ‖ created_by ‖ name)
#[derive(Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
pub struct FolderId([u8; 32]);

/// A shared folder in the network
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SharedFolder {
    pub folder_id: FolderId,
    pub name: String,
    pub created_by: DeviceId,
    pub created_at: u64, // HLC
}

/// How a device participates in a folder
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SyncMode {
    ReadWrite,
    ReadOnly,
}

/// A device's subscription to a folder
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FolderSubscription {
    pub folder_id: FolderId,
    pub device_id: DeviceId,
    pub mode: SyncMode,
}
```

**Updated `FileMetadata`**:

```rust
pub struct FileMetadata {
    pub blob_hash: BlobHash,
    pub folder_id: FolderId,       // NEW — which folder this file belongs to
    pub path: String,              // CHANGED — was `filename`, now relative path within folder
    pub size: u64,
    pub mime_type: Option<String>,
    pub created_at: u64,
    pub modified_at: u64,          // NEW — last modification timestamp
    pub device_origin: DeviceId,
}
```

**New/updated `Action` variants**:

```rust
// New folder actions
FolderCreated { folder: SharedFolder },
FolderRemoved { folder_id: FolderId },
FolderSubscribed { folder_id: FolderId, device_id: DeviceId, mode: SyncMode },
FolderUnsubscribed { folder_id: FolderId, device_id: DeviceId },

// Updated file actions (now folder-scoped)
FileAdded { folder_id: FolderId, metadata: FileMetadata },
FileModified { folder_id: FolderId, path: String, old_hash: BlobHash, new_hash: BlobHash, metadata: FileMetadata },
FileDeleted { folder_id: FolderId, path: String },
```

**DAG changes (`murmur-dag`)**:

- `MaterializedState` adds:
  - `folders: BTreeMap<FolderId, SharedFolder>`
  - `subscriptions: BTreeMap<(FolderId, DeviceId), FolderSubscription>`
  - `file_versions: BTreeMap<(FolderId, String), Vec<(BlobHash, u64)>>` — version chain per `(folder, path)`, ordered by HLC
- Apply all new action variants during state materialization
- `files` map re-keyed: `BTreeMap<(FolderId, String), FileMetadata>` instead of `BTreeMap<BlobHash, FileMetadata>`

**Engine changes (`murmur-engine`)**:

- `create_folder(name: &str) → Result<(SharedFolder, DagEntry)>` — creates folder, auto-subscribes creator as ReadWrite
- `remove_folder(folder_id: FolderId) → Result<DagEntry>` — only creator can remove
- `subscribe_folder(folder_id: FolderId, mode: SyncMode) → Result<DagEntry>`
- `unsubscribe_folder(folder_id: FolderId) → Result<DagEntry>`
- `modify_file(folder_id: FolderId, path: &str, old_hash: BlobHash, new_hash: BlobHash, metadata: FileMetadata, data: Vec<u8>) → Result<DagEntry>`
- `list_folders() → Vec<SharedFolder>`
- `folder_subscriptions(folder_id: FolderId) → Vec<FolderSubscription>`
- `folder_files(folder_id: FolderId) → Vec<FileMetadata>`
- `file_history(folder_id: FolderId, path: &str) → Vec<(BlobHash, u64)>`
- Update `add_file()` to require `folder_id`, verify device is subscribed as ReadWrite
- Update `receive_entry()` to handle new action variants
- Enforce read-only: reject `FileAdded`/`FileModified`/`FileDeleted` from devices subscribed as ReadOnly

**Tasks**:

- [x] Add `FolderId`, `SharedFolder`, `SyncMode`, `FolderSubscription` types to `murmur-types`
- [x] Update `FileMetadata`: add `folder_id`, rename `filename` → `path`, add `modified_at`
- [x] Add new `Action` variants: `FolderCreated`, `FolderRemoved`, `FolderSubscribed`, `FolderUnsubscribed`, `FileModified`
- [x] Update existing `FileAdded` and `FileDeleted` to include `folder_id` / path
- [x] Update `MaterializedState` in `murmur-dag`: folders, subscriptions, file_versions, re-key files map
- [x] Implement state materialization for all new actions
- [x] Add engine methods: `create_folder`, `remove_folder`, `subscribe_folder`, `unsubscribe_folder`
- [x] Add engine methods: `modify_file`, `list_folders`, `folder_subscriptions`, `folder_files`, `file_history`
- [x] Update `add_file` to require folder_id and check subscription
- [x] Enforce read-only mode in `receive_entry` (reject writes from read-only subscribers)
- [x] Update all existing tests to use new `FileMetadata` / `Action` signatures
- [x] Update `CLAUDE.md`: document new types, updated Action enum, folder patterns

**Tests** (≥12):

- [x] Create folder → appears in `list_folders()`
- [x] Remove folder → removed from state
- [x] Subscribe to folder (ReadWrite) → subscription recorded
- [x] Subscribe to folder (ReadOnly) → mode correctly stored
- [x] Unsubscribe from folder → subscription removed
- [x] Add file with folder_id and path → appears in `folder_files()`
- [x] Modify file → version chain has both old and new hashes in order
- [x] `file_history()` returns versions ordered by HLC
- [x] `folder_files()` returns only files in that folder, not other folders
- [x] Read-only device attempting `add_file()` → rejected
- [x] Read-only device attempting `modify_file()` → rejected
- [x] Folder creator auto-subscribed as ReadWrite
- [x] File with same path in different folders → independent, no collision
- [x] Remove folder does not affect files in other folders

---

## Milestone 14 — Conflict Detection & Resolution

**Crates**: `murmur-types`, `murmur-dag`, `murmur-engine`

**Goal**: Detect when two devices concurrently modify the same file (DAG fork on the same path),
track active conflicts in materialized state, and provide resolution mechanics.

**Conflict detection algorithm**: During state materialization, when processing `FileModified` or
`FileAdded` entries for the same `(folder_id, path)`, check if they are concurrent (neither entry's
DAG hash is an ancestor of the other). If concurrent → conflict.

**New types in `murmur-types`**:

```rust
/// A detected conflict on a file
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConflictInfo {
    pub folder_id: FolderId,
    pub path: String,
    pub versions: Vec<ConflictVersion>,
    pub detected_at: u64, // HLC when first detected
}

/// One side of a conflict
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConflictVersion {
    pub blob_hash: BlobHash,
    pub device_id: DeviceId,
    pub hlc: u64,
    pub dag_entry_hash: [u8; 32],
}
```

**New `Action` variant**:

```rust
ConflictResolved {
    folder_id: FolderId,
    path: String,
    chosen_hash: BlobHash,
    discarded_hashes: Vec<BlobHash>,
}
```

**DAG/Engine changes**:

- `MaterializedState` adds: `conflicts: Vec<ConflictInfo>`
- Conflict detection runs during `apply_action()` when processing file mutations
- Ancestry check: use `is_ancestor(hash_a, hash_b)` on the DAG to determine concurrency
- `list_conflicts() → Vec<ConflictInfo>`
- `list_conflicts_in_folder(folder_id: FolderId) → Vec<ConflictInfo>`
- `resolve_conflict(folder_id: FolderId, path: &str, chosen_hash: BlobHash) → Result<DagEntry>`
- Resolving creates a `ConflictResolved` entry and removes the conflict from active state
- New `EngineEvent` variant: `ConflictDetected { folder_id, path, versions }`

**Conflict file naming on disk** (used later by M16, defined here for spec):
When a conflict is detected, the non-chosen versions are renamed on disk:
`{stem}.conflict-{short_device_id}-{timestamp}.{ext}` (e.g., `report.conflict-a1b2c3-1711234567.docx`)

**Tasks**:

- [ ] Add `ConflictInfo`, `ConflictVersion` types to `murmur-types`
- [ ] Add `ConflictResolved` action variant
- [ ] Implement `is_ancestor(a, b) → bool` in `murmur-dag` (BFS/DFS through parent chain)
- [ ] Implement conflict detection during state materialization
- [ ] Add `conflicts` field to `MaterializedState`
- [ ] Add engine methods: `list_conflicts`, `list_conflicts_in_folder`, `resolve_conflict`
- [ ] Add `ConflictDetected` variant to `EngineEvent`
- [ ] Emit `ConflictDetected` event via callbacks when conflict is first detected

**Tests** (≥10):

- [ ] Two devices add file with same `(folder_id, path)` concurrently → conflict detected
- [ ] Two devices modify same file concurrently → conflict detected
- [ ] Sequential modifications (causal chain) → no conflict
- [ ] Resolve conflict → removed from `list_conflicts()`
- [ ] Resolve conflict → creates correct `ConflictResolved` DAG entry
- [ ] Resolve conflict → `folder_files()` shows the chosen version
- [ ] Multiple conflicts on different files tracked independently
- [ ] Conflict with >2 versions (3 devices edit concurrently) → all versions captured
- [ ] `ConflictDetected` event emitted via callbacks
- [ ] Delete vs modify on same file concurrently → conflict detected
- [ ] Resolving already-resolved conflict → error

---

## Milestone 15 — Streaming Blob Storage & Transfer

**Crates**: `murmur-types`, `murmur-net`, `murmur-engine`, `murmurd`

**Goal**: Remove the 256 MiB hard blob size limit. Files of any size can be added, stored, and
transferred without loading the entire file into memory. blake3 hashing is done incrementally.
Memory usage stays bounded regardless of file size.

**Architecture**:

```
Producer:  file on disk → streaming blake3 hash → 1 MiB chunks → network
Consumer:  network → chunks → temp file → verify hash → rename to final path
```

**Changes**:

- Remove `MAX_BLOB_SIZE` constant (or set to a very high advisory limit like 1 TiB)
- `blake3::Hasher` streaming API: read file in 64 KiB buffers, update hasher incrementally
- New engine method: `add_file_streaming(folder_id, path, file_path: &Path) → Result<DagEntry>` — reads from disk, never holds full file in memory
- Update `PlatformCallbacks`:
  - `on_blob_stream_start(blob_hash: BlobHash, total_size: u64)` — prepare to receive chunks
  - `on_blob_chunk(blob_hash: BlobHash, offset: u64, data: Vec<u8>)` — receive a chunk (write to temp file)
  - `on_blob_stream_complete(blob_hash: BlobHash) → Result<()>` — verify hash, rename temp → final
  - Keep existing `on_blob_received()` for small blobs (below chunk threshold)
- Update blob transfer in `murmur-net`:
  - Sender: read file in chunks, send `BlobChunk` messages
  - Receiver: buffer chunks to temp file, verify on completion
  - Backpressure: max N chunks in flight (flow control via ack)
- Update `murmurd` storage:
  - `FjallPlatform` implements streaming callbacks
  - Temp directory for in-progress transfers: `~/.murmur/blobs/.tmp/`
  - Atomic rename on completion
- `CHUNK_SIZE` remains 1 MiB, `CHUNK_THRESHOLD` remains 4 MiB (below threshold → send as single `BlobPush`)

**Tasks**:

- [ ] Remove `MAX_BLOB_SIZE` hard limit
- [ ] Implement streaming blake3 hash computation (read file in 64 KiB buffers)
- [ ] Add `add_file_streaming()` to engine — accepts `&Path`, streams from disk
- [ ] Add streaming callbacks to `PlatformCallbacks`: `on_blob_stream_start`, `on_blob_chunk`, `on_blob_stream_complete`
- [ ] Update `murmur-net` blob transfer: chunked send with backpressure
- [ ] Update `murmur-net` blob receive: reassemble chunks to temp file
- [ ] Update `FjallPlatform` in `murmurd`: implement streaming callbacks, temp dir, atomic rename
- [ ] Add flow control: max 4 chunks in flight per transfer, ack-based pacing
- [ ] Update `EngineEvent::BlobTransferProgress` to report streaming progress

**Tests** (≥8):

- [ ] Streaming blake3 hash matches single-shot hash for same content
- [ ] `add_file_streaming()` succeeds for file >256 MiB (use tempfile)
- [ ] Chunked transfer between two engines: file arrives intact
- [ ] blake3 verification catches corrupted chunk
- [ ] Temp file cleaned up on successful completion
- [ ] Temp file cleaned up on failed/interrupted transfer
- [ ] Memory usage stays bounded during large transfer (no full file in RAM)
- [ ] Multiple concurrent streaming transfers complete correctly
- [ ] Small file (<4 MiB) still uses single `BlobPush` (no chunking overhead)

---

## Milestone 16 — Filesystem Watching & Ignore Patterns

**Crates**: `murmurd` (primary), `murmur-types` (ignore pattern types if shared)

**Dependencies**: `notify 8` (filesystem events, pure Rust), `ignore` (gitignore-style pattern matching, pure Rust)

**Goal**: murmurd watches the local directories of subscribed folders for changes and automatically
creates DAG entries. Conversely, when DAG entries arrive from the network, files are written to the
local folder directory. This makes sync automatic and bidirectional.

**Architecture**:

```
Local disk change → notify event → debounce → compute hash → engine.add_file() / modify_file() / delete_file()
Remote DAG entry → engine callback → write file to local folder directory
```

**Folder directory mapping**: Each folder subscription has a `local_path` — the directory on this device
where the folder's files live. Stored in murmurd config:

```toml
[[folders]]
folder_id = "a1b2c3..."
local_path = "/home/user/Sync/Photos"
mode = "read-write"
```

**Ignore patterns**:

- Per-folder `.murmurignore` file in folder root (gitignore syntax via `ignore` crate)
- Default ignore list (always applied):
  ```
  .DS_Store
  Thumbs.db
  ._*
  *.tmp
  *~
  .murmurignore
  *.conflict-*          # conflict files are managed, not synced
  ```

**Debouncing**: Coalesce filesystem events within a 500ms window to avoid syncing partially-written files.
After 500ms of no events on a path, process the final state.

**Initial scan**: When a device subscribes to a folder (or on daemon restart), scan the local directory
and reconcile with the DAG state:
- Local file not in DAG → `add_file()`
- DAG file not on disk → write to disk (pull from network if blob missing)
- Both exist, hashes differ → treat as modification or conflict depending on DAG state

**Reverse sync** (DAG → disk): When a `FileAdded`, `FileModified`, or `FileDeleted` entry is received
from the network and the folder is subscribed:
- `FileAdded` / `FileModified` → write blob to `local_path/relative_path` (create subdirs as needed)
- `FileDeleted` → delete file from disk (move to trash? or just delete?)
- Conflict detected → write conflict files with naming convention from M14

**Read-only folders**: For folders subscribed as ReadOnly, filesystem watching is still active but
only for reverse sync (DAG → disk). Local changes are NOT synced to the network. Optionally warn
the user if they modify files in a read-only folder.

**Tasks**:

- [ ] Add `notify` and `ignore` dependencies to `murmurd`
- [ ] Implement `FolderWatcher` struct: manages one `notify::Watcher` per subscribed folder
- [ ] Implement folder directory mapping in murmurd config (TOML `[[folders]]` array)
- [ ] Implement `.murmurignore` loading and default ignore patterns
- [ ] Implement debouncer: coalesce events within 500ms window per path
- [ ] Handle filesystem events: create → `add_file()`, modify → `modify_file()`, delete → `delete_file()`, rename → delete + add
- [ ] Implement initial scan on subscribe / daemon restart
- [ ] Implement reverse sync: DAG entry → write file to local directory
- [ ] Implement conflict file creation on disk (naming convention from M14)
- [ ] Read-only mode: suppress outgoing sync for read-only folders
- [ ] Handle subdirectory creation/deletion
- [ ] Suppress echo events (don't re-sync a file that was just written by reverse sync)

**Tests** (≥12):

- [ ] New file in watched directory → `add_file()` called with correct folder_id and path
- [ ] Modified file → `modify_file()` called with correct old/new hashes
- [ ] Deleted file → DAG `FileDeleted` entry created
- [ ] `.murmurignore` pattern excludes matching files from sync
- [ ] Default ignore patterns applied (`.DS_Store`, `*.tmp`, etc.)
- [ ] Debouncing: rapid successive writes produce single sync event
- [ ] Initial scan: existing files synced to DAG on startup
- [ ] Initial scan: DAG files not on disk written to directory
- [ ] Reverse sync: remote `FileAdded` → file appears in local directory
- [ ] Reverse sync: remote `FileDeleted` → file removed from local directory
- [ ] Ignored file not synced even during initial scan
- [ ] Renamed file detected as delete + add
- [ ] Subdirectory with files handled correctly (relative paths preserved)
- [ ] Read-only folder: local file changes NOT synced to network
- [ ] Echo suppression: file written by reverse sync doesn't trigger outgoing sync

---

## Milestone 17 — IPC & CLI Expansion

**Crates**: `murmur-ipc`, `murmur-cli`, `murmurd`

**Goal**: Expose all folder, conflict, subscription, and file history features via IPC. Extend the CLI
with folder management commands. Add **event streaming** to IPC so that the desktop app and web UI
can receive real-time updates without polling.

**Event streaming**: New IPC flow — client sends `Subscribe` request, server keeps the connection open
and pushes `Event` messages as they occur. This enables real-time UI updates in the desktop app (M18)
and SSE in the web UI (M19).

**New IPC types**:

```rust
// New requests
CreateFolder { name: String },
RemoveFolder { folder_id_hex: String },
ListFolders,
SubscribeFolder { folder_id_hex: String, local_path: String, mode: String },
UnsubscribeFolder { folder_id_hex: String, keep_local: bool },
FolderFiles { folder_id_hex: String },
FolderStatus { folder_id_hex: String },
ListConflicts,
ResolveConflict { folder_id_hex: String, path: String, chosen_hash_hex: String },
FileHistory { folder_id_hex: String, path: String },
SetFolderMode { folder_id_hex: String, mode: String },
SubscribeEvents,  // long-lived: server pushes EngineEvents

// New responses
Folders { folders: Vec<FolderInfoIpc> },
FolderStatus { folder_id: String, name: String, file_count: u64, conflict_count: u64, sync_status: String },
Conflicts { conflicts: Vec<ConflictInfoIpc> },
FileVersions { versions: Vec<FileVersionIpc> },
Event { event: EngineEventIpc },  // pushed to event subscribers

// New IPC data types
FolderInfoIpc { folder_id: String, name: String, created_by: String, file_count: u64, subscribed: bool, mode: Option<String> },
ConflictInfoIpc { folder_id: String, folder_name: String, path: String, versions: Vec<ConflictVersionIpc> },
ConflictVersionIpc { blob_hash: String, device_id: String, device_name: String, hlc: u64 },
FileVersionIpc { blob_hash: String, device_id: String, device_name: String, modified_at: u64, size: u64 },
EngineEventIpc { event_type: String, data: String },  // JSON-serialized event details
```

**New CLI commands**:

```
murmur-cli folder create <name>
murmur-cli folder list
murmur-cli folder subscribe <folder_id> <local_path> [--read-only]
murmur-cli folder unsubscribe <folder_id> [--keep-local]
murmur-cli folder files <folder_id>
murmur-cli folder status <folder_id>
murmur-cli folder remove <folder_id>
murmur-cli folder mode <folder_id> <read-write|read-only>
murmur-cli conflicts [--folder <folder_id>]
murmur-cli resolve <folder_id> <path> <chosen_hash>
murmur-cli history <folder_id> <path>
```

All commands support `--json` for machine-readable output.

**Tasks**:

- [ ] Add all new request/response variants to `murmur-ipc`
- [ ] Add `FolderInfoIpc`, `ConflictInfoIpc`, `ConflictVersionIpc`, `FileVersionIpc` types
- [ ] Implement event streaming: `SubscribeEvents` request → long-lived connection pushing `Event` responses
- [ ] Wire up all new IPC requests in murmurd's IPC handler
- [ ] Add `folder` subcommand group to `murmur-cli` (create, list, subscribe, unsubscribe, files, status, remove, mode)
- [ ] Add `conflicts` command to `murmur-cli`
- [ ] Add `resolve` command to `murmur-cli`
- [ ] Add `history` command to `murmur-cli`
- [ ] `--json` support for all new commands
- [ ] Update `CLAUDE.md` with new IPC protocol and CLI commands

**Tests** (≥10):

- [ ] IPC round-trip: `CreateFolder` → `Folders` response with new folder
- [ ] IPC round-trip: `SubscribeFolder` → `Ok` response
- [ ] IPC round-trip: `ListFolders` returns correct folder list
- [ ] IPC round-trip: `ListConflicts` returns correct conflicts
- [ ] IPC round-trip: `ResolveConflict` → `Ok` + conflict removed
- [ ] IPC round-trip: `FileHistory` returns version chain
- [ ] IPC round-trip: `FolderStatus` returns correct counts
- [ ] Event streaming: client receives `Event` when DAG entry is created
- [ ] CLI `folder create` + `folder list` — end-to-end
- [ ] CLI `--json` output parses as valid JSON
- [ ] Error handling: subscribe to non-existent folder → `Error` response
- [ ] Error handling: resolve non-existent conflict → `Error` response

---

## Milestone 18 — Desktop App (iced) — Folders, Conflicts & Sync

**Crate**: `murmur-desktop`

**Goal**: Refactor the desktop app from a standalone engine-embedding app into a **thin IPC client**
to murmurd. Then add full UI for folder management, conflict resolution, file history, and sync status.
The desktop app, web UI, and CLI all use the same murmurd backend via IPC.

**Architecture change**: Currently `murmur-desktop` runs its own `MurmurEngine` with embedded storage
and networking. After this milestone, it connects to murmurd via Unix socket IPC (same protocol as
`murmur-cli`). Uses the `SubscribeEvents` IPC stream for real-time UI updates.

- On launch: check if murmurd is running (try connecting to socket). If not, offer to start it.
- All state comes from IPC responses. No local engine, no local storage, no local networking.

**Screens**:

1. **Setup** (existing, updated): Create network or join existing. Now also shows available folders to subscribe to after joining.
2. **Folders** (new): List all network folders. Create new folder. Subscribe/unsubscribe. Per-folder sync status indicator (synced, syncing, conflicts).
3. **Folder Detail** (new): File browser with directory tree for a specific folder. Shows file sizes, modification dates, device origin. Sync progress bar.
4. **Conflicts** (new): List of active conflicts across all folders. Each conflict shows the file path, competing versions with device names and timestamps. Buttons: "Keep this version", "Keep other", "Keep both". Preview panel if file is text/image.
5. **File History** (new): Version list for a selected file. Shows each version's hash, device, timestamp, size. "Restore" button to revert to a previous version.
6. **Devices** (existing, updated): Device list with per-folder subscription info. Approve/revoke. Shows online/offline status.
7. **Status** (existing, updated): Network overview — folder count, total files, total conflicts, connected peers, DAG entries, event log.

**Tasks**:

- [ ] Refactor `murmur-desktop` to remove embedded engine, storage, and networking
- [ ] Implement IPC client: connect to murmurd socket, send requests, receive responses
- [ ] Implement event stream listener: subscribe to murmurd events for real-time updates
- [ ] Auto-detect murmurd: check socket on launch, offer to start daemon
- [ ] **Folders screen**: list folders, create folder dialog, subscribe/unsubscribe buttons, sync status badges
- [ ] **Folder Detail screen**: directory tree view, file list with metadata, sync progress
- [ ] **Conflicts screen**: conflict list, version comparison, resolution buttons
- [ ] **File History screen**: version timeline, restore button
- [ ] **Updated Devices screen**: per-folder subscription info per device
- [ ] **Updated Status screen**: folder count, conflict count, event log with real-time updates
- [ ] **Updated Setup screen**: folder selection after joining network
- [ ] Navigation: sidebar with folder list + Devices + Status sections

**Tests** (≥8):

- [ ] App connects to murmurd via IPC socket
- [ ] Folder list view populates from IPC `ListFolders` response
- [ ] Create folder → IPC `CreateFolder` sent → folder appears in list
- [ ] Subscribe folder → IPC `SubscribeFolder` sent with correct mode
- [ ] Conflict list populates from IPC `ListConflicts` response
- [ ] Resolve conflict → IPC `ResolveConflict` sent → conflict removed from list
- [ ] File history view shows version chain from IPC `FileHistory` response
- [ ] Real-time event: file synced on another device → UI updates without manual refresh
- [ ] Graceful handling when murmurd is not running (error message, not crash)

---

## Milestone 19 — Web Dashboard (htmx)

**Crate**: `murmurd` (HTTP module expansion)

**Dependencies**: `askama` (Jinja-like templates, pure Rust, compile-time checked), htmx (JS library, ~14 KB, embedded as static asset)

**Goal**: Full-featured web management dashboard served by murmurd. Server-rendered HTML with htmx for
dynamic updates. Accessible at `http://localhost:<port>` when murmurd is started with `--http-port`.
Provides the same management capabilities as the desktop app, for use on headless servers or when
a browser is more convenient.

**Architecture**:

```
Browser ──HTTP──▶ murmurd (axum) ──▶ Engine (same in-process engine used by IPC)
                      │
                      ├── GET /            → dashboard (askama template)
                      ├── GET /folders     → folder list
                      ├── POST /folders    → create folder
                      ├── GET /folders/:id → folder detail + file browser
                      ├── GET /devices     → device list
                      ├── POST /devices/:id/approve → approve
                      ├── GET /conflicts   → conflict list
                      ├── POST /conflicts/resolve → resolve
                      ├── GET /events (SSE) → Server-Sent Events stream
                      └── GET /static/*    → htmx, CSS
```

**Pages**:

1. **Dashboard** (`GET /`): Overview cards — folder count, device count, conflict count, connected peers, DAG entries. Live-updating via htmx polling or SSE.
2. **Folders** (`GET /folders`): Table of folders with name, file count, sync status, subscribe/unsubscribe actions. "Create folder" form.
3. **Folder Detail** (`GET /folders/:id`): File list with directory grouping, sizes, dates. Sync progress. Subscribe/unsubscribe/change mode controls.
4. **Devices** (`GET /devices`): Approved + pending devices. Approve/revoke buttons. Per-device folder subscriptions.
5. **Conflicts** (`GET /conflicts`): Conflict list grouped by folder. Each shows competing versions. Resolve buttons inline.
6. **File History** (`GET /folders/:id/history/*path`): Version timeline for a file.

**htmx patterns**:
- `hx-post` for actions (approve device, resolve conflict, create folder)
- `hx-get` with `hx-trigger="every 2s"` for live sync status on dashboard
- Server-Sent Events (`GET /events`) with `hx-ext="sse"` for real-time conflict/sync notifications
- `hx-swap="outerHTML"` for partial page updates after actions
- `hx-confirm` for destructive actions (remove folder, revoke device)

**Styling**: Minimal, clean CSS embedded in templates. No CSS framework dependency. Dark/light mode via `prefers-color-scheme`.

**Tasks**:

- [ ] Add `askama` dependency to `murmurd`
- [ ] Embed htmx JS as static asset (include in binary via `include_str!` or `rust-embed`)
- [ ] Create base template with navigation (sidebar: Dashboard, Folders, Devices, Conflicts)
- [ ] Implement dashboard page with overview cards
- [ ] Implement folders list page with create form
- [ ] Implement folder detail page with file browser
- [ ] Implement devices page with approve/revoke actions
- [ ] Implement conflicts page with inline resolution
- [ ] Implement file history page
- [ ] Implement SSE endpoint (`GET /events`) streaming `EngineEvent`s
- [ ] CSS: clean, minimal, responsive, dark/light mode
- [ ] Wire all routes to engine operations (same engine instance as IPC)
- [ ] Enable web UI by default when `--http-port` is set (no separate feature flag)

**Tests** (≥8):

- [ ] Dashboard page returns 200 with correct counts
- [ ] Folder list page renders all folders
- [ ] `POST /folders` creates folder, redirects to list
- [ ] Device list shows approved and pending devices
- [ ] `POST /devices/:id/approve` approves device
- [ ] Conflicts page renders active conflicts
- [ ] `POST /conflicts/resolve` resolves conflict, removes from list
- [ ] SSE endpoint streams events as they occur
- [ ] Folder detail page shows file list with correct paths
- [ ] Static assets (htmx, CSS) served correctly

---

## Milestone 20 — Protocol Specification v0.1

**Location**: `docs/protocol.md`

**Goal**: Write a complete, implementer-facing specification of the Murmur protocol. Any developer
should be able to read this document and build a compatible client in any language. The spec covers
wire formats, DAG semantics, folder model, conflict resolution, and security — everything needed for
interoperability.

**Sections**:

1. **Overview** — What Murmur is, design goals (privacy, no servers, BIP39 trust root), target use cases
2. **Terminology** — Definitions: network, device, folder, DAG, entry, tip, HLC, blob
3. **Cryptographic Primitives** — Ed25519 (signing), blake3 (hashing), HKDF-SHA256 (key derivation), BIP39 (mnemonic), AES-256-GCM (blob encryption at rest)
4. **Identity & Key Derivation** — NetworkId derivation from mnemonic, DeviceId from Ed25519 public key, first-device deterministic key, joiner random key, ALPN string format
5. **DAG Structure** — Entry format (hlc, device_id, action, parents, hash, signature), hash computation algorithm (exact byte layout), signature scheme, HLC rules
6. **Actions** — Complete list of all action variants with:
   - Serialization format (postcard field order)
   - Authorization rules (who can create this action)
   - State transition (how it modifies materialized state)
7. **Folder Model** — SharedFolder, FolderSubscription, SyncMode, folder lifecycle (create → subscribe → use → unsubscribe → remove)
8. **File Versioning** — FileMetadata fields, version chains, path semantics (relative, forward-slash separated, UTF-8)
9. **Conflict Detection & Resolution** — Algorithm (ancestry check on concurrent file mutations), ConflictInfo structure, resolution protocol, conflict file naming convention
10. **Wire Protocol** — Message framing (4-byte BE length prefix + postcard payload), all `MurmurMessage` variants with field layouts, compression (deflate with 1-byte flag), chunking (1 MiB chunks, 4 MiB threshold)
11. **Gossip Protocol** — Topic derivation from NetworkId, `GossipPayload` variants, PlumTree semantics (eager push + lazy pull), DAG sync algorithm (tip exchange → delta computation → entry transfer)
12. **Blob Transfer** — Push vs pull, chunked streaming, backpressure (ack-based), integrity verification (blake3), resume semantics
13. **Discovery & Connection** — iroh relay servers, direct connections, ALPN negotiation (`murmur/0/<hex_prefix>`), hole punching
14. **Security Model** — Threat model (what Murmur protects against, what it doesn't), approval gate (mnemonic alone is insufficient), blob encryption at rest, transport encryption (QUIC TLS), replay protection (HLC + signature)
15. **IPC Protocol** — Unix socket, framing, request/response types, event streaming — for local tool integration
16. **Compatibility & Versioning** — Protocol version negotiation, backwards compatibility policy, breaking change rules

**Deliverables**:

- [ ] Write `docs/protocol.md` — complete v0.1 specification
- [ ] Include worked examples: hash computation for a sample DAG entry, key derivation from a sample mnemonic
- [ ] Include message diagrams: device join flow, file sync flow, conflict resolution flow
- [ ] Update `docs/architecture.md` with folder model and streaming changes
- [ ] Update `docs/features.md` with all new features (folders, conflicts, versioning, watching, web UI)
- [ ] Update `CLAUDE.md` with protocol spec reference and final dependency list

**Verification** (not automated tests, but manual checks):

- [ ] Every `Action` variant in code has a corresponding section in the spec
- [ ] Every `MurmurMessage` variant in code has a corresponding wire format description
- [ ] Hash computation example in spec matches `DagEntry::compute_hash()` output
- [ ] Key derivation example in spec matches `NetworkIdentity` output for a known mnemonic
- [ ] Conflict detection algorithm in spec matches implementation in `murmur-dag`

---

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
