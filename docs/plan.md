# Murmur — Implementation Plan

## How to Use This Document

This is the **implementation plan** for remaining Murmur milestones. Work milestone by milestone, in order.
For each milestone: implement, test, `cargo clippy -- -D warnings`, `cargo fmt`, stop.

For architecture and design details, see [architecture.md](architecture.md).
For a feature overview, see [features.md](features.md).

## Current Status (as of 2026-03-27)

| Milestone                              | Status                                                                                                          |
| -------------------------------------- | --------------------------------------------------------------------------------------------------------------- |
| 0–17                                   | ✅ Complete (Types, Seed, DAG, Network, Engine, Server, Integration Tests, Desktop App, FFI, Android, Daemon + CLI Split, Hardening, Folder Model & File Versioning, Conflict Detection & Resolution, Streaming Blob Storage & Transfer, Filesystem Watching & Ignore, IPC & CLI Expansion) |
| 18 — Desktop App (iced)                | 🔲 Planned |
| 19 — Web Dashboard (htmx)              | 🔲 Planned |
| 20 — Protocol Specification v0.1       | 🔲 Planned |

---

## Design Decisions (Milestones 13–20)

These decisions were agreed upon before implementation and guide all milestones below:

| Decision            | Choice                                                                              |
| ------------------- | ----------------------------------------------------------------------------------- |
| Folder model        | Syncthing-style shared folders mapped to real directories on each device            |
| Selective sync      | Per-device subscribe model; choose folders on join, add/remove later                |
| Folder permissions  | Self-selected: each device chooses read-write or read-only per folder               |
| File modifications  | Explicit version chain (`FileModified` action in DAG, full history)                 |
| Conflict strategy   | Fork-based: keep both versions on disk, surface to user for resolution              |
| Filesystem watching | `notify` crate in murmurd, auto-detect changes in shared folder directories         |
| Ignore patterns     | `.murmurignore` per folder (gitignore syntax), plus sensible defaults               |
| Interfaces          | Desktop app (iced) + Web UI (htmx) + CLI — all thin clients calling murmurd via IPC |
| Folder deletion     | Configurable: unsubscribing device chooses to keep or delete local files            |
| Large files         | No hard size limit; streaming blake3, chunked storage/transfer, bounded memory      |
| Web UI tech         | Server-rendered HTML with htmx, served by murmurd's axum server                     |
| Protocol spec       | Phased: v0.1 alongside implementation, iterate as features stabilize                |

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
