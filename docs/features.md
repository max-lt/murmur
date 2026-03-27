# Murmur — Features

Murmur is a private, peer-to-peer file sync network. No servers, no accounts, no cloud. Your devices talk directly to each other over encrypted QUIC connections, and everything is coordinated through a cryptographic append-only log that merges automatically.

---

## 12 Words to a Private Network

Write down 12 words. That's it — no server setup, no account creation, no device ID exchange, no QR code scanning. A BIP39 mnemonic (the same standard Bitcoin wallets use) is the single root of trust for your entire network. Every device that knows the words can request to join. Every cryptographic key derives from them.

Lose your phone? Enter the words on a new one. Want to add a NAS? Enter the words. The mnemonic is the network.

## Two-Factor Trust

Knowing the mnemonic gets you to the door — it doesn't get you in. Every new device must be explicitly approved by an existing device before it can participate. This means a leaked mnemonic alone doesn't compromise your network. Revocation is instant and propagates to all devices.

## Works Offline, Merges Automatically

Devices don't need to be online at the same time. Each device appends to its own branch of a signed, hash-chained DAG. When two devices reconnect — whether after minutes or months — the DAG merges automatically. Only the delta transfers. There's no central coordinator deciding what's "current."

## No File Size Limit

There is no 256 MiB cap, no "large file" mode, no special configuration. A 50 GB video gets the same treatment as a 50 KB document. Files are hashed, transferred, and stored in streaming fashion — blake3 hashing in 64 KiB buffers, network transfer in 1 MiB chunks, disk writes directly from the network stream. Memory usage stays constant regardless of file size.

## Encryption at Rest

Blobs on disk can be encrypted with AES-256-GCM. Large files use chunked authenticated encryption so that a multi-gigabyte file never needs to be fully loaded into memory for encryption or decryption. Each chunk has its own derived nonce — there's no shortcuts or weakened crypto for large files.

## Full File History

Every file modification is recorded in the DAG with an explicit version chain. You can query the complete history of any file — who changed it, when, and what the content hash was at each point. This isn't a bolted-on feature; it's an inherent property of the append-only DAG.

## Conflicts Without Data Loss

When two devices edit the same file concurrently (a true DAG fork), Murmur detects the conflict automatically via ancestry analysis. It keeps every version — nothing is silently overwritten or discarded. The user sees all competing versions and chooses how to resolve. Three-way and n-way conflicts are handled the same as two-way.

## Selective, Per-Device Sync

Each device chooses which folders to sync and whether to participate as read-write or read-only. A phone might subscribe to Photos (read-write) and Documents (read-only). A NAS might subscribe to everything. A laptop might only want a few project folders. Subscriptions are self-managed — no central admin.

## Scoped, Time-Limited Access

Need to share a specific file with another device temporarily? Access grants are scoped (single file, folder prefix, or everything) and time-limited. They're signed by the grantor and expire automatically. No permanent sharing links, no "anyone with the link" footgun.

## Every Mutation is Signed

Every state change — device joins, file additions, folder subscriptions, conflict resolutions — is recorded as a DAG entry that's blake3-hashed and Ed25519-signed by its author. Tampered entries are rejected. You get a complete, cryptographically verifiable audit trail of everything that happened in your network.

## Pure Rust, Runs Everywhere

The entire core — networking, cryptography, DAG, sync engine — is pure Rust with zero C dependencies. It cross-compiles to Android and iOS without toolchain headaches. The same code runs on a Raspberry Pi, a Linux desktop, an Android phone, and (eventually) an iPhone. Platform differences (storage, UI) are handled by a thin callback layer; the protocol logic is identical everywhere.

## NAT Traversal Built In

Powered by iroh's QUIC transport with automatic hole punching and relay fallback. Devices behind NAT, on different Wi-Fi networks, or across continents can connect without port forwarding or VPN setup. Each network is isolated at the QUIC ALPN layer — different mnemonic, different network, no crosstalk.

---

## Platform Support

| Platform | Status | Interface |
| -------- | ------ | --------- |
| Linux (daemon) | Complete | CLI (`murmur-cli`) + headless daemon (`murmurd`) |
| Linux (desktop) | Complete | iced GUI app |
| Android | Complete | Jetpack Compose app with background sync |
| iOS | Planned | Swift app wrapping the Rust core |
| Web dashboard | Planned | htmx/axum served by murmurd |

## Security Summary

| Property | Mechanism |
| -------- | --------- |
| Identity | BIP39 mnemonic + HKDF key derivation |
| Authentication | Ed25519 signatures on every DAG entry |
| Integrity | blake3 hashes on every entry and blob |
| Transport encryption | QUIC TLS |
| Encryption at rest | AES-256-GCM (chunked AEAD for large files) |
| Network isolation | Unique ALPN per network |
| Authorization | Device approval gate + scoped access grants |
