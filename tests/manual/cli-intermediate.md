# CLI: Intermediate Tests — Folders, Sync, Transfers & History

Folder management, file versioning, conflict handling, transfer status, large
file sync, and DAG delta sync. Run the **basic tests** first — this suite
assumes a two-node network is already established and both daemons are running.

This test requires **4 terminal windows**: Terminals 1 and 2 run the daemons;
Terminals 3 and 4 run CLI commands against each daemon.

## Prerequisites

Complete the basic test suite (cli-basic.md) or set up the equivalent state:

```bash
cargo build -p murmurd -p murmur-cli
rm -rf /tmp/murmur-a /tmp/murmur-b
```

Start both daemons, join and approve Node B (as in basic tests B1–B16).
Both nodes should be running with one file synced in the "default" folder.

---

## I1 — Folder List (Default Folder Exists)

The first `add` command auto-creates a "default" folder.

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a folder list
```

**Expected**: at least one folder — "default". Shows folder ID, name, file count,
subscription status.

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-b folder list
```

**Expected**: same "default" folder visible on Node B.

---

## I2 — Folder List JSON

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a folder list --json
```

**Expected**: valid JSON with `folders` array. Each entry has: `folder_id` (64 hex),
`name`, `created_by` (64 hex), `file_count`, `subscribed` (bool), `mode` (string or null).

**Save the default folder ID** from this output.

---

## I3 — Create a New Folder

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a folder create "photos"
```

**Expected**: confirmation message (e.g., "Folder created").

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a folder list
```

**Expected**: two folders — "default" and "photos".

Wait 5-10 seconds, then verify on Node B:

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-b folder list
```

**Expected**: same two folders on Node B.

**Save the photos folder ID**.

---

## I4 — Create a Second Folder

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a folder create "documents"
```

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a folder list
```

**Expected**: three folders — "default", "photos", "documents".

---

## I5 — Folder Status

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a folder status <DEFAULT_FOLDER_ID>
```

**Expected**:
- `Folder: default`
- `Folder ID:` matching the default folder ID
- `Files:` file count (>= 0)
- `Conflicts: 0`
- `Status:` a sync status string

---

## I6 — Folder Status JSON

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a folder status <DEFAULT_FOLDER_ID> --json
```

**Expected**: valid JSON with `folder_id`, `name`, `file_count`, `conflict_count`, `sync_status`.

---

## I7 — Folder Status for Non-existent Folder

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a folder status "aa".repeat(32)
```

(Use a 64-character hex string that doesn't match any folder.)

**Expected**: error response (e.g., "folder not found"). Non-zero exit code.

---

## I8 — Folder Status Invalid Hex

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a folder status "not-a-hex-id"
```

**Expected**: error about invalid folder ID format. Non-zero exit code.

---

## I9 — Subscribe to Folder

Subscribe Node B to the "photos" folder:

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-b folder subscribe <PHOTOS_FOLDER_ID> /tmp/murmur-b-photos
```

**Expected**: confirmation message (e.g., "Subscribed").

Verify:

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-b folder list --json
```

**Expected**: "photos" folder shows `subscribed: true`, `mode: "read-write"`.

---

## I10 — Subscribe Read-Only

Subscribe Node B to "documents" in read-only mode:

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-b folder subscribe <DOCUMENTS_FOLDER_ID> /tmp/murmur-b-docs --read-only
```

**Expected**: confirmation.

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-b folder list --json
```

**Expected**: "documents" folder shows `subscribed: true`, `mode: "read-only"`.

---

## I11 — Subscribe to Non-existent Folder

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-b folder subscribe aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa /tmp/murmur-b-fake
```

**Expected**: error — folder not found. Non-zero exit code.

---

## I12 — Change Folder Sync Mode

Change the "photos" subscription from read-write to read-only:

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-b folder mode <PHOTOS_FOLDER_ID> read-only
```

**Expected**: confirmation (e.g., "Mode changed").

Verify:

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-b folder list --json
```

**Expected**: "photos" folder now shows `mode: "read-only"`.

Change it back:

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-b folder mode <PHOTOS_FOLDER_ID> read-write
```

**Expected**: confirmation. Mode reverts to "read-write".

---

## I13 — Folder Files (Default Folder)

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a folder files <DEFAULT_FOLDER_ID>
```

**Expected**: lists files belonging to the "default" folder only. Files from other
folders are not shown.

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a folder files <DEFAULT_FOLDER_ID> --json
```

**Expected**: valid JSON with `files` array.

---

## I14 — Folder Files (Empty Folder)

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a folder files <PHOTOS_FOLDER_ID>
```

**Expected**: "No synced files." (or empty list — photos folder has no files yet).

---

## I15 — Add File and Verify in Folder

Add a file:

```bash
echo "Hello from Node A — $(date)" > /tmp/test-file-a.txt
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a add /tmp/test-file-a.txt
```

**Expected**: "File added" with blob hash.

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a folder files <DEFAULT_FOLDER_ID>
```

**Expected**: `test-file-a.txt` in the default folder file list.

---

## I16 — MIME Type Detection

Add files with different extensions:

```bash
echo '{"key": "value"}' > /tmp/test.json
echo '<html><body>hi</body></html>' > /tmp/test.html
dd if=/dev/urandom of=/tmp/test.bin bs=64 count=1 2>/dev/null
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a add /tmp/test.json
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a add /tmp/test.html
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a add /tmp/test.bin
```

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a files
```

**Expected**: MIME types reflect file content — e.g., `application/json`, `text/html`,
`application/octet-stream` (or similar). Verify these are plausible types.

---

## I17 — File Deduplication

Add the same file again:

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a add /tmp/test-file-a.txt
```

**Expected**: either dedup detected (same blob hash, no new entry) or a second entry
with the same blob hash. The file should not appear twice with different hashes.

---

## I18 — Large File Transfer (Chunked)

Create and add a 5 MB file to exercise chunked transfer:

```bash
dd if=/dev/urandom of=/tmp/test-large.bin bs=1M count=5 2>/dev/null
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a add /tmp/test-large.bin
```

**Expected**: "File added" with blob hash.

---

## I19 — Transfer Status

Immediately after adding the large file (while sync may still be in progress):

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a transfers
```

**Expected**: either shows an in-flight transfer with bytes_transferred/total_bytes
and percentage, or "No active transfers." if already complete.

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a transfers --json
```

**Expected**: valid JSON with `transfers` array. Each entry has `blob_hash`,
`bytes_transferred`, `total_bytes`.

---

## I20 — Large File Arrives on Node B

Wait 10-15 seconds:

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-b files
```

**Expected**: `test-large.bin` appears with size ~5242880 bytes.

Verify integrity:

```bash
b3sum /tmp/test-large.bin
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-b files --json | jq '.files[] | select(.path | contains("test-large.bin")) | .blob_hash'
```

**Expected**: hashes match.

---

## I21 — DAG Sync on Reconnect (Delta Sync)

**Terminal 2** — stop Node B (Ctrl+C).

Add a file while Node B is offline:

```bash
echo "Added while B was offline — $(date)" > /tmp/test-offline.txt
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a add /tmp/test-offline.txt
```

Restart Node B:

```bash
# Terminal 2
cargo run --bin murmurd -- --data-dir /tmp/murmur-b --name "node-b" --verbose
```

Watch verbose logs for `DagSyncRequest`/`DagSyncResponse` messages (delta sync).

Wait a few seconds:

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-b files
```

**Expected**: `test-offline.txt` appears on Node B.

---

## I22 — Delta Sync: Multiple Offline Entries

**Terminal 2** — stop Node B again.

Add multiple files while offline:

```bash
echo "Offline file 1" > /tmp/test-off1.txt
echo "Offline file 2" > /tmp/test-off2.txt
echo "Offline file 3" > /tmp/test-off3.txt
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a add /tmp/test-off1.txt
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a add /tmp/test-off2.txt
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a add /tmp/test-off3.txt
```

Restart Node B. Wait a few seconds:

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-b files
```

**Expected**: all three offline files appear on Node B. DAG entry counts match
across both nodes.

---

## I23 — Conflict Detection (Empty)

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a conflicts
```

**Expected**: "No active conflicts."

---

## I24 — Conflicts JSON

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a conflicts --json
```

**Expected**: valid JSON with `conflicts` array (empty).

---

## I25 — Conflicts Filtered by Folder

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a conflicts --folder <DEFAULT_FOLDER_ID>
```

**Expected**: "No active conflicts."

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a conflicts --folder <PHOTOS_FOLDER_ID>
```

**Expected**: "No active conflicts."

---

## I26 — Conflict Resolution (Non-existent)

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a resolve <DEFAULT_FOLDER_ID> no-such-file.txt aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
```

**Expected**: error (e.g., "conflict not found" or "no conflict for this file").
Non-zero exit code.

---

## I27 — File History

Check version history for a file:

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a history <DEFAULT_FOLDER_ID> test-file-a.txt
```

**Expected**: at least one version showing:
- Blob hash (64 hex chars)
- Size in bytes
- Device name ("node-a")
- Device ID (64 hex chars)
- Modification timestamp

---

## I28 — File History JSON

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a history <DEFAULT_FOLDER_ID> test-file-a.txt --json
```

**Expected**: valid JSON with `versions` array. Each entry has `blob_hash`, `device_id`,
`device_name`, `modified_at`, `size`.

---

## I29 — File History (Non-existent File)

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a history <DEFAULT_FOLDER_ID> no-such-file.txt
```

**Expected**: "No version history."

---

## I30 — File History on Node B (Cross-Node Consistency)

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-b history <DEFAULT_FOLDER_ID> test-file-a.txt
```

**Expected**: same version history as Node A for the same file.

---

## I31 — Unsubscribe from Folder

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-b folder unsubscribe <DOCUMENTS_FOLDER_ID>
```

**Expected**: confirmation.

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-b folder list --json
```

**Expected**: "documents" folder shows `subscribed: false`.

---

## I32 — Unsubscribe with Keep-Local

Subscribe first, then unsubscribe with `--keep-local`:

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-b folder subscribe <DOCUMENTS_FOLDER_ID> /tmp/murmur-b-docs2
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-b folder unsubscribe <DOCUMENTS_FOLDER_ID> --keep-local
```

**Expected**: unsubscription succeeds. If local files existed in `/tmp/murmur-b-docs2`,
they are preserved on disk.

---

## I33 — Folder Remove

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a folder remove <DOCUMENTS_FOLDER_ID>
```

**Expected**: confirmation.

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a folder list
```

**Expected**: "documents" folder no longer appears.

Wait 5-10 seconds. On Node B:

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-b folder list
```

**Expected**: "documents" folder removed on Node B as well.

---

## I34 — DAG Consistency After All Operations

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a status --json | jq .dag_entries
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-b status --json | jq .dag_entries
```

**Expected**: both nodes report the same DAG entry count.

---

## Cleanup

```bash
rm -rf /tmp/murmur-a /tmp/murmur-b /tmp/murmur-b-photos /tmp/murmur-b-docs /tmp/murmur-b-docs2
rm -f /tmp/test-file-a.txt /tmp/test-file-b.txt /tmp/test.json /tmp/test.html /tmp/test.bin
rm -f /tmp/test-large.bin /tmp/test-offline.txt /tmp/test-off1.txt /tmp/test-off2.txt /tmp/test-off3.txt
```

## Quick Reference

| # | What | Pass criteria |
|---|------|---------------|
| I1 | Folder list | Default folder visible on both nodes |
| I2 | Folder list JSON | Valid JSON, all fields present |
| I3 | Create folder | "photos" folder created, synced to Node B |
| I4 | Create second folder | "documents" created, three folders total |
| I5 | Folder status | Correct name, file count, conflict count |
| I6 | Folder status JSON | Valid JSON with all fields |
| I7 | Status non-existent folder | Error response |
| I8 | Status invalid hex | Error about invalid folder ID |
| I9 | Subscribe | Node B subscribed to "photos" |
| I10 | Subscribe read-only | "documents" subscribed in read-only mode |
| I11 | Subscribe non-existent | Error response |
| I12 | Change mode | Mode switches between read-write and read-only |
| I13 | Folder files | Lists only files in specified folder |
| I14 | Folder files (empty) | Empty result for folder with no files |
| I15 | Add file to folder | File appears in folder-specific listing |
| I16 | MIME detection | Different file types get appropriate MIME types |
| I17 | Dedup | Same file content produces same blob hash |
| I18 | Large file add | 5 MB file added successfully |
| I19 | Transfer status | Shows active/completed transfers, valid JSON |
| I20 | Large file sync | 5 MB file arrives on Node B with correct hash |
| I21 | Delta sync | Offline file synced after reconnect |
| I22 | Multi-entry delta | Multiple offline files all sync |
| I23 | Conflicts (empty) | "No active conflicts." |
| I24 | Conflicts JSON | Valid JSON with empty array |
| I25 | Conflicts by folder | Folder filter works |
| I26 | Resolve non-existent | Error response |
| I27 | File history | Version chain with device info |
| I28 | File history JSON | Valid JSON with versions array |
| I29 | History non-existent | "No version history." |
| I30 | History cross-node | Same history on both nodes |
| I31 | Unsubscribe | Subscription removed |
| I32 | Unsubscribe keep-local | Local files preserved |
| I33 | Folder remove | Folder removed from both nodes |
| I34 | DAG consistency | Same entry count after all operations |
