# CLI: Basic Tests — Network, Devices & File Sync

Core functionality: network creation, device join/approval, simple file sync,
and basic status reporting. This is the minimum "smoke test" that should pass
before moving to intermediate or advanced tests.

This test requires **4 terminal windows**: Terminals 1 and 2 run the daemons;
Terminals 3 and 4 run CLI commands against each daemon.

## Prerequisites

```bash
cargo build -p murmurd -p murmur-cli
rm -rf /tmp/murmur-a /tmp/murmur-b
```

---

## B1 — Network Creation

**Terminal 1** — start the first daemon:

```bash
cargo run --bin murmurd -- --data-dir /tmp/murmur-a --name "node-a" --role full --verbose
```

**Expected**:
- Banner printed: `NEW MURMUR NETWORK CREATED`
- 24-word BIP39 mnemonic displayed
- `Listening on socket: /tmp/murmur-a/murmurd.sock`
- No errors or panics in output

**Save the mnemonic** — copy the 24 words for later phases.

---

## B2 — Status Command (Single Node)

**Terminal 3**:

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a status
```

**Expected**:
- `Device: node-a`
- `Device ID:` followed by a 64-character hex string
- `Network ID:` followed by a 64-character hex string
- `Peers: 0`
- `DAG entries: 2` (DeviceApproved + DeviceNameChanged)
- `Uptime:` a non-negative number

---

## B3 — Status JSON Output

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a status --json
```

**Expected**: valid JSON containing fields `device_id`, `device_name`, `network_id`,
`peer_count`, `dag_entries`, `uptime_secs`. Pipe through `jq .` to validate.

---

## B4 — Devices Command (Creator Only)

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a devices
```

**Expected**: one device — "node-a" with role "full".

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a devices --json
```

**Expected**: valid JSON with a `devices` array of length 1. Each entry has
`device_id`, `name`, `role`, `approved` fields.

---

## B5 — Pending (Empty)

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a pending
```

**Expected**: "No pending requests."

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a pending --json
```

**Expected**: `{"devices": []}` (valid JSON, empty array).

---

## B6 — Files (Empty)

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a files
```

**Expected**: "No synced files."

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a files --json
```

**Expected**: `{"files": []}` (valid JSON, empty array).

---

## B7 — Mnemonic Retrieval

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a mnemonic
```

**Expected**: displays the same 24-word mnemonic from B1.

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a mnemonic --json
```

**Expected**: valid JSON with `mnemonic` field matching the mnemonic from B1.

---

## B8 — Device Join (Offline Command)

**Terminal 4** — join Node B (no daemon needed for this step):

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-b join "<PASTE 24 WORDS HERE>" --name "node-b" --role backup
```

**Expected**:
- "Joined Murmur network (pending approval)."
- "Device ID:" followed by a 64-character hex string
- "Start the daemon with:" instruction

Verify files were created:

```bash
cat /tmp/murmur-b/config.toml
cat /tmp/murmur-b/mnemonic
ls -la /tmp/murmur-b/device.key
```

**Expected**:
- `config.toml` contains `name = "node-b"` and `role = "backup"`
- `mnemonic` file contains the 24 words
- `device.key` exists (32 bytes)

---

## B9 — Join Rejects Invalid Mnemonic

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-bad join "not a valid mnemonic at all" --name "bad" --role backup
```

**Expected**: error message containing "invalid mnemonic". Exit code non-zero.

---

## B10 — Join Rejects Invalid Role

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-bad join "<PASTE 24 WORDS HERE>" --name "bad" --role superadmin
```

**Expected**: error message about unknown role. Exit code non-zero.

---

## B11 — Join Prevents Double-Init

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-b join "<PASTE 24 WORDS HERE>" --name "node-b2" --role backup
```

**Expected**: error — "already initialized". The existing config is not overwritten.

---

## B12 — Start Node B Daemon

**Terminal 2**:

```bash
cargo run --bin murmurd -- --data-dir /tmp/murmur-b --name "node-b" --verbose
```

**Expected**:
- Daemon starts, loads from existing config
- Broadcasts `DeviceJoinRequest`
- Gossip connection logs appear
- No "NEW MURMUR NETWORK CREATED" banner (joining, not creating)

---

## B13 — Pending Shows Join Request

**Terminal 3**:

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a pending
```

**Expected**: one pending device — "node-b".

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a pending --json
```

**Expected**: JSON with one entry in `devices` array containing `device_id` and `name`.

**Save the device ID** from this output for the next phase.

---

## B14 — Device Approval

**Terminal 3** — approve node-b:

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a approve <NODE_B_DEVICE_ID> --role backup
```

**Expected**: "Device approved" confirmation.

Wait 5-10 seconds for gossip propagation.

---

## B15 — Both Nodes See Both Devices

**Terminal 3**:

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a devices
```

**Expected**: two devices — "node-a" (full) and "node-b" (backup).

**Terminal 4**:

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-b devices
```

**Expected**: same two devices listed.

---

## B16 — Peer Count After Approval

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a status
```

**Expected**: `Peers: 1` (or more).

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-b status
```

**Expected**: `Peers: 1` (or more).

---

## B17 — File Sync A→B

Create and add a test file:

```bash
echo "Hello from Node A — $(date)" > /tmp/test-file-a.txt
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a add /tmp/test-file-a.txt
```

**Expected**: "File added" with a 64-character blob hash. A "default" folder is
auto-created on the first `add`.

Verify on Node A:

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a files
```

**Expected**: one file — `test-file-a.txt` with size, MIME type `text/plain`.

Wait 5-10 seconds, then verify on Node B:

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-b files
```

**Expected**: same file appears on Node B.

---

## B18 — Blob Integrity Verification

Verify the blob was actually transferred (not just the DAG entry):

```bash
ls /tmp/murmur-b/blobs/
```

**Expected**: content-addressed directory structure with blob data.

Compare hashes:

```bash
b3sum /tmp/test-file-a.txt
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-b files
```

**Expected**: blake3 hash from `b3sum` matches the blob hash from `files` output.

---

## B19 — File Sync B→A

Add a file from Node B:

```bash
echo "Hello from Node B — $(date)" > /tmp/test-file-b.txt
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-b add /tmp/test-file-b.txt
```

Wait 5-10 seconds.

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a files
```

**Expected**: two files — `test-file-a.txt` and `test-file-b.txt`.

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-b files
```

**Expected**: same two files on Node B.

---

## B20 — Files JSON Output

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a files --json
```

**Expected**: valid JSON with `files` array. Each entry has: `blob_hash` (64 hex chars),
`folder_id` (64 hex chars), `path`, `size` (positive integer), `mime_type`, `device_origin`.

---

## B21 — DAG Entry Counts Match

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a status --json | jq .dag_entries
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-b status --json | jq .dag_entries
```

**Expected**: both return the same number.

---

## B22 — CLI With No Daemon Running

Stop Node B (Ctrl+C in Terminal 2). Then try a command:

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-b status
```

**Expected**: error — "murmurd is not running (socket not found at ...)". Non-zero exit code.

Restart Node B for subsequent tests:

```bash
# Terminal 2
cargo run --bin murmurd -- --data-dir /tmp/murmur-b --name "node-b" --verbose
```

---

## Cleanup

```bash
rm -rf /tmp/murmur-a /tmp/murmur-b /tmp/murmur-bad /tmp/test-file-a.txt /tmp/test-file-b.txt
```

## Quick Reference

| # | What | Pass criteria |
|---|------|---------------|
| B1 | Network creation | Daemon running, mnemonic printed, socket listening |
| B2 | Status (plain) | All fields present, correct DAG entries |
| B3 | Status (JSON) | Valid JSON, all fields present |
| B4 | Devices | One device "node-a" (full) |
| B5 | Pending (empty) | "No pending requests." |
| B6 | Files (empty) | "No synced files." |
| B7 | Mnemonic | Matches original, works in plain + JSON |
| B8 | Join (offline) | Config/mnemonic/key files created |
| B9 | Bad mnemonic | Error, non-zero exit |
| B10 | Bad role | Error, non-zero exit |
| B11 | Double init | Error, no overwrite |
| B12 | Start Node B | Daemon starts, join request broadcast |
| B13 | Pending (populated) | Node B visible as pending |
| B14 | Approve | "Device approved" |
| B15 | Devices (both) | Both nodes see both devices |
| B16 | Peer count | Both show >= 1 peer |
| B17 | File sync A→B | File added on A appears on B |
| B18 | Blob integrity | blake3 hash matches |
| B19 | File sync B→A | File added on B appears on A |
| B20 | Files JSON | Valid JSON, all fields present |
| B21 | DAG consistency | Same entry count on both nodes |
| B22 | No daemon error | Clear error message, non-zero exit |
