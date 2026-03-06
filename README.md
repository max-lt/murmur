# Murmur

Private, peer-to-peer device synchronization network. Sync photos, documents and files between your devices — phone, tablet, laptop, NAS — without any third-party server.

Built in 100% pure Rust. Zero C bindings.

## How it works

1. Generate a **BIP39 mnemonic** (12 or 24 words) — this is your network's root of trust
2. Start a backup daemon on your NAS or server
3. Connect phones and tablets by entering the mnemonic — existing devices approve new ones
4. Files flow automatically from sources (phone) to backup nodes (NAS)
5. Any device can request temporary access to another device's files

All mutations (device joins, file additions, access grants) are recorded in a **signed, hash-chained DAG** that syncs via gossip. Devices merge naturally when they reconnect — fully offline-first.

## Architecture

```
┌─────────────────────────────────────────────┐
│  Platform (storage, UI, OS integration)     │
│  murmurd / Android (future) / iOS (future)  │
├─────────────────────────────────────────────┤
│  Rust Core (protocol + logic, no storage)   │
│  ┌─────────────────────────────────────┐    │
│  │  Engine    — sync, approval, blobs  │    │
│  │  DAG       — signed append-only log │    │
│  │  Network   — iroh QUIC + gossip     │    │
│  │  Seed      — BIP39 + HKDF keys     │    │
│  │  Types     — DeviceId, BlobHash, …  │    │
│  └─────────────────────────────────────┘    │
└─────────────────────────────────────────────┘
```

The core never writes to disk. It produces serialized bytes and hands them to the platform via callbacks. Each platform decides how to persist (Fjall, SQLite, Core Data, etc.).

## Workspace layout

```
crates/
  murmur-types/    Shared types (DeviceId, BlobHash, NetworkId, HLC, roles)
  murmur-seed/     BIP39 mnemonic + HKDF key derivation
  murmur-dag/      Signed append-only DAG (in-memory, platform persists)
  murmur-net/      iroh QUIC + gossip broadcast
  murmur-engine/   Orchestrator (sync, approval, blob transfer)
  murmurd/         Headless backup server daemon
tests/
  integration/     Multi-device simulation tests
```

## Build & test

```bash
cargo build                          # build everything
cargo test                           # all tests (~165)
cargo test -p murmur-types           # single crate
cargo clippy -- -D warnings          # lint (must pass, zero warnings)
cargo fmt --check                    # format check
```

## Dependencies

| Purpose        | Crate              |
|----------------|--------------------|
| Hashing        | blake3             |
| Networking     | iroh 0.96          |
| Gossip         | iroh-gossip 0.96   |
| BIP39          | bip39 v2           |
| Key derivation | hkdf + sha2        |
| Signing        | ed25519-dalek v2   |
| Serialization  | postcard + serde   |
| Async          | tokio              |

Desktop only (in `murmurd`): fjall v3 (metadata DB), clap (CLI).

No dependency requires C/C++ compilation in the core crates.

## License

See [LICENSE](LICENSE) for details.
