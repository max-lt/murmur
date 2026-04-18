# Murmur — Implementation Plan

## How to Use This Document

This is the **implementation plan** for remaining Murmur milestones. Work milestone by milestone, in order.
For each milestone: implement, test, `cargo clippy -- -D warnings`, `cargo fmt`, stop.

For architecture and design details, see [architecture.md](architecture.md).
For a feature overview, see [features.md](features.md).

## Current Status (as of 2026-04-17)

| Milestone                                                        | Status     |
| ---------------------------------------------------------------- | ---------- |
| 0–17 — MVP : DAG, Network, Engine, Daemon, FFI, Android, Desktop | ✅ Done    |
| 18 — Desktop App: IPC Refactor & Core Screens                    | ✅ Done    |
| 19 — Zero-Config Onboarding & Default Folder                     | ✅ Done    |
| 20 — System Tray & Notifications (IPC: PauseSync/ResumeSync)     | ✅ Done    |
| 21 — Folder Discovery & Selective Sync                           | ✅ Done    |
| 22 — Rich Conflict Resolution                                    | ✅ Done    |
| 23 — Device Management Improvements                              | ✅ Done    |
| 24 — Sync Progress, Pause/Resume & Bandwidth                     | ✅ Done    |
| 25 — File Browser & Search                                       | ✅ Done    |
| 26 — Settings & Configuration UI                                 | ✅ Done    |
| 27 — Diagnostics & Network Health                                | ✅ Done    |
| 29 — Conflict Resolution Improvements                            | ✅ Done    |
| 30 — Onboarding                                                  | ✅ Done    |
| 31 — Sync Progress & Desktop UX Polish                           | ✅ Done    |
| 32 — Miscellaneous Quality-of-Life                               | 🔲 Planned |
| 33 — Cross-Platform Desktop Builds & Distribution                | 🔲 Planned |
| 34 — iOS App                                                     | 🔲 Planned |

---

## Milestone 31 — Sync Progress & Desktop UX Polish

Polish the desktop experience with transfer visibility and ergonomic improvements.
Cross-platform builds are split into M33 — they're a separate concern with very
different risks (signing, notarization, updaters) and shouldn't gate UX work.

### Features

- **Sync progress with ETA (smoothed)** — during large transfers, show speed (MB/s), percentage, and estimated time remaining. Use **EWMA over a 30s window** for the speed sample (`speed_t = α·instantaneous + (1−α)·speed_{t−1}`, α≈0.2). Naive `bytes/elapsed` is too jittery on real networks to produce useful ETAs.
- **Drag-and-drop into folder** — drag files/folders from the OS file manager onto the desktop folder-detail view to add them to the sync set. iced supports drop targets natively.
- **Per-folder color/icon** — small visual marker users can set per folder, surfaced in the sidebar and tray menu. Reduces "which folder am I looking at?" friction with many folders.
- **Activity feed** — a chronological view of recent engine events (synced, conflict detected/resolved, device joined, transfer started/finished). Replaces "did anything just happen?" guesswork.
- **Notification preferences** — per-event toggles in settings: conflict, transfer-completed, device-joined, error. Currently all-or-nothing.

### Implementation

1. Extend `TransferInfoIpc` with `started_at_unix: u64` and `last_progress_unix: u64`. Compute speed/ETA daemon-side (using EWMA); expose `bytes_per_sec_smoothed: u64` and `eta_seconds: Option<u64>` on the IPC response so all clients render identical numbers.
2. Add a progress bar widget (iced `ProgressBar`) to the transfers section showing percentage and "X MB/s — ~Y min remaining".
3. Wire iced drop-target events on `views/folders.rs` folder-detail to call `AddFile` IPC for each dropped path; show progress per file.
4. Add `color_hex: Option<String>` and `icon: Option<String>` fields to folder local config (`murmurd config.toml`, not the DAG — these are per-device cosmetic). Add `SetFolderColor` / `SetFolderIcon` IPC.
5. Replace today's tray notification logic with a `notification_settings: NotificationSettings` config struct; respect per-event toggles in the tray code.
6. Add `views/activity.rs` consuming the existing `SubscribeEvents` stream; ring-buffer last 200 events in the desktop process; render newest-first.

### Tests

- Unit test: EWMA ETA — given a sequence of progress samples, verify smoothed speed converges and ETA stabilizes within ±10% after 30s
- Unit test: notification settings — disabled events do not enqueue tray notifications
- UI smoke test: drag-and-drop calls `AddFile` for each path (mock IPC)
- Visual: confirm activity feed updates in near-real-time when files sync

---

## Milestone 32 — Miscellaneous Quality-of-Life

Small but impactful improvements across daemon, CLI, and filesystem handling.
Two anti-goals worth calling out:

- **Don't block sync silently.** Several proposed "warnings" can become user-facing
  stalls if implemented as hard rejects. Default to _quarantine + warn_, never _drop_.
- **Don't bury signals in `tracing`.** `tracing::warn!` is invisible to GUI users —
  every detection here must also emit an `EngineEvent` so the activity feed surfaces it.

### Features

- **Duplicate detection** — warn (never block) when `add` or `modify` sees a file whose blake3 hash already exists under a different path in the same folder. Surface in desktop activity feed and CLI status.
- **Case-conflict detection** — on case-insensitive filesystems (macOS, Windows), detect when two files differ only in case. **Quarantine** the second variant under `<folder>/.murmur-quarantine/<original-path>.case-N` rather than blocking the add. Emit a warning event so the user can rename one. Blocking would cause confusing "files not appearing" support reports.
- **Configurable filesystem watch debounce** — allow users to set the debounce delay (default 500ms) in `config.toml` and via desktop settings; useful for IDEs that write in rapid bursts (saving on every keystroke).
- **`murmur-cli doctor`** — comprehensive self-diagnostic with a `--deep` mode for expensive checks. Default mode is fast (sub-second); `--deep` verifies cryptographic integrity.
- **Selective scrub** — `murmur-cli scrub <folder>` re-verifies all blob hashes for a folder against the DAG. Used after suspected disk corruption or filesystem repair.
- **Dry-run flags** — `--dry-run` on destructive operations (`folder remove`, `leave-network`, `reclaim-orphans`) shows what would happen without doing it.
- **Daemon backup/restore** — `murmur-cli backup <out.tar.zst>` exports config + DAG + key material (encrypted with the mnemonic); `restore <in.tar.zst>` rehydrates a daemon. For migrations and disaster recovery. (Blobs are _not_ in the backup — they re-sync from peers.)

### Implementation

1. **Duplicate detection**: in `handle_forward_sync_event`, after computing blake3, query the engine for existing files with same hash in the folder. Emit `tracing::warn!` AND `EngineEvent::DuplicateDetected { folder, new_path, existing_paths, hash }`. Desktop activity feed renders these.
2. **Case-conflict detection**: on file add, normalize path to lowercase and check the folder's file map. On collision, write to `<folder>/.murmur-quarantine/<path>.case-<N>` and emit `EngineEvent::CaseConflictQuarantined { folder, original_path, quarantine_path }`. The quarantine directory is in default `.murmurignore`.
3. **Configurable debounce**: add `watch_debounce_ms: u64` to `config.toml` (default 500, min 50, max 10000); pass to `FolderWatcher::new()`. Add `SetWatchDebounce` IPC and surface in desktop settings.
4. **`murmur-cli doctor`**: add `Doctor { deep: bool }` IPC request. Fast checks: daemon reachable (socket connect), config parseable (`GetConfig`), storage accessible (`StorageStats`), all subscribed folder paths exist + writable, disk space ≥ 1 GB free, relay connectivity (`RunConnectivityCheck`), peer reachability (`ListPeers` shows ≥ 1 alive peer if any are configured), HLC clock reasonable (within ±5 min of system time). Deep checks: DAG signature verification for every entry, blob hash verification for every blob, blob completeness (all referenced hashes exist on disk). Print as checklist with pass/fail/warn.
5. **Scrub**: add `ScrubFolder { folder_id_hex: String }` IPC. Streams progress events; reports any blob whose disk content doesn't hash to its expected value. Quarantines corrupt blobs (move to `<blob_store>/.corrupt/`) so re-sync from peers can restore them.
6. **Dry-run**: add `dry_run: bool` to `RemoveFolder`, `LeaveNetwork`, `ReclaimOrphanedBlobs` IPC requests; CLI exposes `--dry-run`. Daemon returns the would-be effect (file count, byte count, etc.) without applying.
7. **Backup/restore**: serialize config + DAG entries + signing key material into a tar.zst archive, AES-256-GCM encrypted with a key derived from the mnemonic (HKDF, distinct salt from network ID). `murmur-cli backup` and `restore` subcommands.

### Tests

- Unit test: duplicate detection emits both `tracing::warn!` and `EngineEvent::DuplicateDetected`
- Unit test: case-conflict moves to quarantine directory and emits event (does not block add)
- Unit test: debounce config is respected; out-of-range values clamped
- Integration test: `murmur-cli doctor` returns all-pass on a healthy daemon
- Integration test: `doctor --deep` detects a tampered DAG entry (manually corrupt one byte)
- Integration test: `scrub` detects a corrupt blob (truncate one) and quarantines it
- Integration test: `folder remove --dry-run` reports byte count without removing
- Integration test: backup → wipe daemon → restore → DAG state identical, peers re-sync blobs

---

## Milestone 33 — Cross-Platform Desktop Builds & Distribution

Ship the desktop app on macOS, Windows, and Linux with proper packaging, code signing,
and auto-update. This is split from M31 because the work is largely orthogonal to UX
(it's mostly toolchain / CI / signing key management) and has very different failure
modes (signing-cert expiry, notarization rejection, store review).

### Features

- **macOS build** — universal binary (x86_64 + aarch64), signed with a Developer ID Application certificate, notarized via `notarytool`, packaged as DMG with `create-dmg`
- **Windows build** — x86_64 binary, signed with an EV or OV code-signing certificate, packaged as MSI (WiX v4) and a portable zip
- **Linux packaging** — AppImage (portable, no install), `.deb` (Debian/Ubuntu), `.rpm` (Fedora/openSUSE); Flatpak as stretch goal
- **Auto-update** — `murmur-desktop` polls a release manifest URL on startup + every 24h; downloads new versions, verifies a minisign signature, prompts the user to restart. Manifest hosted on GitHub Releases.
- **CI matrix** — GitHub Actions: build + test on Linux/macOS/Windows; release pipeline tags → builds → signs → notarizes → uploads artifacts → updates manifest

### Implementation

1. macOS: `cargo-bundle` for `.app` (with `Info.plist`, including `murmur://` URL scheme registration); `lipo` to merge x86_64+aarch64 binaries; `codesign` + `notarytool`; `create-dmg` for the DMG installer
2. Windows: `cargo build --target x86_64-pc-windows-msvc`; WiX v4 toolset for MSI; `signtool` for signing
3. Linux: `cargo-appimage` or manual AppImage assembly; `cargo-deb` for `.deb`; `cargo-generate-rpm` for `.rpm`
4. Auto-update: add `update.rs` to `murmur-desktop`; manifest schema `{ version, url, minisign_sig, sha256, channel }`; verify with the project's pinned minisign public key (compiled in)
5. GitHub Actions: matrix `os: [ubuntu-latest, macos-latest, windows-latest]`; tag-triggered release workflow uses encrypted secrets for signing keys (Apple App Store Connect API key, Windows code-signing PFX, minisign secret key)
6. Document the signing-key rotation procedure in `docs/release.md` (new file)

### Tests

- CI: `cargo build -p murmur-desktop` succeeds on each matrix entry
- CI: produced macOS `.app` passes `spctl --assess` (Gatekeeper)
- CI: produced Windows MSI is signed (`signtool verify /pa`)
- Manual smoke: install on each OS, verify daemon starts, verify `murmur://` URL handler works
- Unit test: update manifest signature verification — valid sig accepted, tampered manifest rejected
- Unit test: version comparison — only update when newer (semver)

---

## Milestone 34 — iOS App

Port the existing Android architecture (Rust core via FFI + native UI) to iOS.
Listed as planned in `docs/features.md` but absent from the plan — adding it here.

### Features

- **iOS app** — Swift + SwiftUI app embedding the `murmur-ffi` Rust core
- **Background sync** — using `BGProcessingTask` for periodic sync when the app is suspended; full background mode is restricted on iOS so sync windows are best-effort
- **Photos auto-backup** — opt-in PhotoKit observer that adds new photos/videos to a designated folder, similar to the Android flow
- **Files app integration** — File Provider extension exposing synced folders inside the iOS Files app

### Implementation

1. Verify `murmur-ffi` cross-compiles to `aarch64-apple-ios` and `aarch64-apple-ios-sim` (it should — pure Rust, no C deps)
2. Generate Swift bindings: continue with UniFFI (already used for Kotlin) or hand-write a thin C-ABI wrapper if UniFFI's Swift output proves friction-prone
3. Build the SwiftUI app: `Onboarding`, `Folders`, `FolderDetail`, `Conflicts`, `Devices`, `Settings` screens — same information architecture as Android for consistency
4. Background sync: register `BGProcessingTask` identifiers; schedule from foreground on app-background transition; the task runs the engine for up to ~30s
5. Photos backup: PhotoKit `PHPhotoLibrary` observer; add new assets to the configured folder via the FFI engine
6. File Provider extension: implement `NSFileProviderReplicatedExtension`; back it with the same engine instance via shared App Group container
7. Distribution: TestFlight first; App Store review will likely require justifying the background networking entitlement

### Tests

- Build: iOS app builds and links against `murmur-ffi` for both device and simulator targets
- Integration: pair an iOS device into a network with an existing daemon; verify file sync both directions
- Manual: foreground sync, background sync window, conflict detection, photo auto-backup
- App Store: TestFlight build accepted by Apple review

---

## Phase 2: Desktop app remaining Features

System Tray & Notifications
