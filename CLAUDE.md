# Murmur — Private Device Sync Network

100% pure Rust core. Zero C bindings. See @docs/plan.md for full architecture and implementation plan.

## Build & Test

```bash
cargo build                          # build everything
cargo test                           # all tests
cargo test -p murmur-types           # single crate
cargo test --test two_device_sync    # single integration test
cargo clippy -- -D warnings          # lint (must pass, zero warnings)
cargo fmt --check                    # format check
```

IMPORTANT: Run `cargo clippy -- -D warnings` and `cargo fmt --check` before considering any milestone complete.

## Workspace Layout

Rust monorepo. All crates live in `crates/`. Integration tests in `tests/`.

```
crates/
  murmur-types/      Shared types (DeviceId, BlobHash, NetworkId, HLC, roles)
  murmur-seed/       BIP39 mnemonic + HKDF key derivation
  murmur-dag/        Signed append-only DAG (adapted from Shoal's LogTree, in-memory)
  murmur-net/        Network (iroh QUIC + gossip + shared wire utilities)
  murmur-engine/     Orchestrator (sync, approval, blob transfer, storage-agnostic)
  murmur-ipc/        IPC protocol types for daemon ↔ CLI communication
  murmur-cli/        CLI tool for managing murmurd (folders, conflicts, devices, status, etc.)
  murmurd/           Headless backup daemon (no subcommands, managed via murmur-cli)
tests/
  integration/       Multi-device simulation tests
```

## Code Style

- `thiserror` for error types in library crates, `anyhow` only in `murmurd`
- `tracing` for all logging, with structured fields. No `println!`
- Serialization: `postcard` + `serde` for wire format and persistence
- All IDs are `[u8; 32]` newtypes. Use `blake3` for hashing
- Async: `tokio` runtime. Traits use `async_trait` where needed
- Keep functions small. Prefer composition over deep nesting
- Every public type and function gets a doc comment

## Dependencies

Core (pure Rust, no storage, no IO):

| Purpose        | Crate               |
| -------------- | ------------------- |
| Hashing        | `blake3`            |
| Networking     | `iroh` 0.96         |
| Gossip         | `iroh-gossip` 0.96  |
| BIP39          | `bip39` v2          |
| Key derivation | `hkdf` + `sha2`     |
| Signing        | `ed25519-dalek` v2  |
| Serialization  | `postcard`, `serde` |
| Async          | `tokio`             |

Desktop only (in `murmurd`, the server daemon):

| Purpose     | Crate                        |
| ----------- | ---------------------------- |
| Metadata DB | `fjall` v3 (DAG persistence) |
| CLI         | `clap`                       |
| FS watching | `notify` 8                   |
| Ignore      | `ignore` 0.4                 |

IMPORTANT: Never add a dependency that requires C/C++ compilation or `cc` build script in the core crates. If unsure, check the dep's build.rs before adding.

## iroh on Mobile

iroh cross-compiles and runs natively on Android and iOS. The `n0-computer/iroh-ffi`
repo has UniFFI bindings (Swift, Kotlin) but releases are paused — n0 recommends writing
your own wrapper. Our approach: the FFI boundary is at `murmur-engine`, not at iroh.
Mobile apps call Murmur functions, never iroh types directly. iroh is an internal
implementation detail of the Rust core.

## Key Architecture Principle: Storage is NOT in the Core

The Rust core (murmur-types, murmur-seed, murmur-dag, murmur-net, murmur-engine) handles
protocol, logic, networking, and cryptography. It **never writes to disk**. It produces
serialized bytes (DagEntry, blobs) and hands them to the platform via callbacks. Each
platform (desktop, Android, iOS) decides how to persist.

- `murmurd` (desktop): uses Fjall for DAG, filesystem for blobs, axum for Web UI
- Android (future): Room/SQLite for DAG, MediaStore for blobs
- iOS (future): Core Data for DAG, app container for blobs

The core's DAG is in-memory (`HashMap`). On startup, the platform loads persisted
entries into the core. The core doesn't know or care how they were stored.

## Relationship to Shoal

Murmur adapts several Shoal components. When implementing, reference the Shoal source:

| Murmur crate   | Shoal source                             | What changes                                                                   |
| -------------- | ---------------------------------------- | ------------------------------------------------------------------------------ |
| `murmur-dag`   | `shoal-logtree/`                         | Different `Action` enum, in-memory only (no DagStore/Fjall), platform persists |
| `murmur-net`   | `shoal-net/` + `shoal-cluster/gossip.rs` | Blob push/pull instead of shard transfer                                       |
| `murmur-types` | `shoal-types/`                           | DeviceId/BlobHash instead of ShardId/ObjectId                                  |

The DAG entry structure is identical: `(hlc, device_id, action, parents) → blake3 hash → ed25519 signature`.

## Workflow

This project is built milestone by milestone. See @docs/plan.md for the full plan.

1. Implement only the milestone you are asked to work on
2. After completing a milestone, run ALL tests for the affected crates
3. Run `cargo clippy -- -D warnings` — must be clean
4. Run `cargo fmt` to format
5. Do NOT move to the next milestone unless explicitly asked

## Testing Conventions

- Unit tests: in the same file, under `#[cfg(test)] mod tests`
- Integration tests: in `tests/integration/`
- Use `tempfile` crate for tests that need filesystem
- Use `tokio::test` for async tests
- Name tests descriptively: `test_approve_device`, `test_dag_merge_concurrent`
- Every milestone has explicit test requirements in plan.md — implement ALL of them

## Common Patterns

### ID types

```rust
#[derive(Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
pub struct DeviceId([u8; 32]);

impl DeviceId {
    pub fn from_verifying_key(key: &VerifyingKey) -> Self {
        Self(key.to_bytes())
    }
}
```

### Error types per crate

```rust
#[derive(Debug, thiserror::Error)]
pub enum DagError {
    #[error("invalid hash")]
    InvalidHash,
    #[error("invalid signature")]
    InvalidSignature,
    #[error("missing parents: {0:?}")]
    MissingParents(Vec<[u8; 32]>),
    #[error("storage: {0}")]
    Storage(String),
}
```

### DAG entry creation (from Shoal's LogTree)

```rust
impl DagEntry {
    pub fn new_signed(
        hlc: u64,
        device_id: DeviceId,
        action: Action,
        parents: Vec<[u8; 32]>,
        signing_key: &SigningKey,
    ) -> Self {
        let hash = Self::compute_hash(hlc, device_id, &action, &parents);
        let signature: Signature = signing_key.sign(&hash);
        // ... same pattern as shoal-logtree/src/entry.rs
    }
}
```

## Bug Fix Workflow

When fixing a reported bug:

1. **Write a failing test first** that reproduces the bug
2. Run the test to confirm it fails
3. Apply the fix
4. Run the test to confirm it passes
5. Run the full test suite to ensure no regressions

## Folder Sync Architecture

Murmur is a Syncthing-style folder sync app. Milestones 13–17 (folder model, conflicts, streaming blobs, filesystem watching, IPC & CLI expansion) are complete. Key design decisions:

- **Shared folders** map to real directories on each device. murmurd watches them with `notify`.
- **Per-device subscribe model**: each device chooses which folders to sync and whether read-write or read-only.
- **File versioning**: `FileModified` action in DAG tracks explicit version chains per `(folder, path)`.
- **Conflict resolution**: fork-based — concurrent edits produce conflict files on disk, user resolves.
- **Three frontends**: Desktop app (iced), Web UI (htmx/axum), CLI — all thin clients to murmurd via IPC.
- **Streaming blobs**: no hard size limit, incremental blake3, chunked transfer, bounded memory.
- **Ignore patterns**: `.murmurignore` per folder (gitignore syntax) via `ignore` crate.
- **IPC protocol**: Unix socket, length-prefixed postcard serialization. Supports event streaming via `SubscribeEvents` for real-time UI updates.
- **CLI commands**: `murmur-cli folder {create,list,subscribe,unsubscribe,files,status,remove,mode}`, `conflicts`, `resolve`, `history`. All support `--json`.
- **Protocol spec**: `docs/protocol.md` — formal specification for third-party interoperability.

See @docs/plan.md for full milestone details and design rationale.

## Things to Avoid

- No `unwrap()` in library code. Use proper error propagation
- No `println!`. Use `tracing::{info, debug, warn, error}`
- No `unsafe` unless absolutely necessary and documented why
- No C dependencies in core crates. Ever
- No premature optimization. Correctness first, benchmark later
- Do not implement milestones out of order
