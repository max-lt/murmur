# CLI: Advanced Tests — Security, Persistence, Edge Cases & Stress

Device revocation, access enforcement, restart persistence, network isolation,
edge cases, filesystem state verification, and multi-file stress. Run the
**basic** and **intermediate** tests first.

This test requires **4 terminal windows**: Terminals 1 and 2 run the daemons;
Terminals 3 and 4 run CLI commands against each daemon.

## Prerequisites

```bash
cargo build -p murmurd -p murmur-cli
rm -rf /tmp/murmur-a /tmp/murmur-b /tmp/murmur-c
```

Start both daemons, join and approve Node B (as in basic tests).

---

## A1 — Device Revocation

Revoke Node B from Node A:

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a revoke <NODE_B_DEVICE_ID>
```

**Expected**: "Device revoked" confirmation.

---

## A2 — Revoked Device Removed from Device List

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a devices
```

**Expected**: only "node-a" in the approved device list. "node-b" no longer listed.

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a devices --json
```

**Expected**: JSON `devices` array with one entry.

---

## A3 — Revoked Device Cannot Sync New Entries

Add a file from the revoked Node B:

```bash
echo "From revoked node — $(date)" > /tmp/test-revoked.txt
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-b add /tmp/test-revoked.txt
```

**Expected**: the command succeeds locally (file added to local DAG).

Check Node A's verbose logs for "unauthorized" or "revoked" rejection messages.

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a files
```

**Expected**: the revoked node's file does NOT appear on Node A.

---

## A4 — Revoke Invalid Device ID

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a revoke aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
```

**Expected**: error (device not found or already revoked). Non-zero exit code.

---

## A5 — Approve Invalid Device ID Format

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a approve "not-hex" --role backup
```

**Expected**: error about invalid device ID. Non-zero exit code.

---

## A6 — Approve with Invalid Role

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a approve aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa --role superadmin
```

**Expected**: error about invalid role. Non-zero exit code.

---

## A7 — Re-approve After Revocation

Re-join and re-approve Node B to restore the two-node setup:

Stop Node B (Ctrl+C in Terminal 2).

```bash
rm -rf /tmp/murmur-b
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-b join "<MNEMONIC>" --name "node-b-v2" --role full
```

Restart Node B:

```bash
# Terminal 2
cargo run --bin murmurd -- --data-dir /tmp/murmur-b --name "node-b-v2" --verbose
```

Wait for the join request to appear:

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a pending
```

**Expected**: "node-b-v2" appears as pending.

Approve:

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a approve <NEW_NODE_B_DEVICE_ID> --role full
```

**Expected**: "Device approved". Both nodes see both devices after a few seconds.

---

## A8 — Graceful Shutdown (Socket Cleanup)

Stop Node A (Ctrl+C in Terminal 1).

```bash
ls /tmp/murmur-a/murmurd.sock 2>&1
```

**Expected**: "No such file or directory" — socket is cleaned up on shutdown.

Check Terminal 1 output: no panics, no error traces, clean exit.

---

## A9 — Stale Socket Detection

If a stale socket file exists (simulating a crash), the daemon should handle it:

```bash
touch /tmp/murmur-a/murmurd.sock
```

Restart the daemon:

```bash
# Terminal 1
cargo run --bin murmurd -- --data-dir /tmp/murmur-a --name "node-a" --verbose
```

**Expected**: daemon detects and removes the stale socket, starts normally.
Check verbose logs for "stale socket" or similar message.

---

## A10 — Restart Persistence: Devices

After restarting Node A:

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a devices
```

**Expected**: all previously approved devices are present (loaded from Fjall on startup).

---

## A11 — Restart Persistence: Files

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a files
```

**Expected**: all previously synced files are intact.

---

## A12 — Restart Persistence: Folders

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a folder list
```

**Expected**: all previously created folders are present.

---

## A13 — Restart Persistence: Mnemonic

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a mnemonic
```

**Expected**: same mnemonic as originally generated.

---

## A14 — Restart Persistence: Status DAG Count

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a status --json | jq .dag_entries
```

**Expected**: DAG entry count matches the pre-restart count.

---

## A15 — Restart Persistence: Reconnection

Wait 5-10 seconds after both daemons are running:

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a status
```

**Expected**: `Peers: 1` — both nodes reconnected after restart.

---

## A16 — Filesystem State Verification

Verify the on-disk state of Node A:

```bash
cat /tmp/murmur-a/config.toml
```

**Expected**: `name = "node-a"`, `role = "full"`, blob_dir and data_dir paths set.

```bash
cat /tmp/murmur-a/mnemonic
```

**Expected**: 24-word mnemonic.

```bash
ls /tmp/murmur-a/device.key 2>/dev/null || echo "no device.key (expected for creator)"
```

**Expected**: creator may not have a separate device.key (uses HKDF-derived key).

```bash
ls /tmp/murmur-a/db/
```

**Expected**: Fjall database files/directories present.

```bash
ls /tmp/murmur-a/blobs/
```

**Expected**: content-addressed blob directory structure.

---

## A17 — Filesystem State: Node B

Verify Node B's on-disk state:

```bash
cat /tmp/murmur-b/config.toml
```

**Expected**: `name = "node-b-v2"`, `role = "full"`.

```bash
ls -la /tmp/murmur-b/device.key
```

**Expected**: 32-byte file (joining device generates a random key).

---

## A18 — Network Isolation

Create a completely separate network to verify no crosstalk:

```bash
rm -rf /tmp/murmur-c
cargo run --bin murmurd -- --data-dir /tmp/murmur-c --name "node-c" --role full --verbose &
DAEMON_C_PID=$!
sleep 5
```

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-c status
```

**Expected**: `Peers: 0` — Node C is on a different network and cannot see Nodes A or B.

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-c devices
```

**Expected**: only "node-c" listed.

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a status
```

**Expected**: peer count unchanged — Node C did not join Node A's network.

Stop Node C:

```bash
kill $DAEMON_C_PID 2>/dev/null
```

---

## A19 — Multi-File Batch Sync

Add many files rapidly on Node A:

```bash
for i in $(seq 1 20); do
  echo "Batch file $i — $(date +%s%N)" > /tmp/test-batch-$i.txt
  cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a add /tmp/test-batch-$i.txt
done
```

Wait 15-20 seconds for all entries to propagate.

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-b files --json | jq '.files | length'
```

**Expected**: file count includes all 20 batch files (plus any previously synced files).

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a status --json | jq .dag_entries
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-b status --json | jq .dag_entries
```

**Expected**: same DAG entry count on both nodes.

---

## A20 — Empty File

```bash
touch /tmp/test-empty.txt
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a add /tmp/test-empty.txt
```

**Expected**: "File added" (or dedup if empty blob exists). Size should be 0.

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a files --json | jq '.files[] | select(.path | contains("test-empty")) | .size'
```

**Expected**: `0`.

---

## A21 — Add Non-existent File

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a add /tmp/does-not-exist.txt
```

**Expected**: error about file not found. Non-zero exit code.

---

## A22 — Status Fields Validation

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a status --json
```

Validate each field:
- `device_id`: 64-character hex string
- `device_name`: non-empty string
- `network_id`: 64-character hex string
- `peer_count`: non-negative integer
- `dag_entries`: positive integer
- `uptime_secs`: non-negative integer

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a status --json | jq '{
  id_len: (.device_id | length),
  nid_len: (.network_id | length),
  name_present: (.device_name | length > 0),
  peers_gte_0: (.peer_count >= 0),
  entries_gt_0: (.dag_entries > 0),
  uptime_gte_0: (.uptime_secs >= 0)
}'
```

**Expected**: all values are `true` or match expected lengths (64 for hex IDs).

---

## A23 — File Origin Tracking

After syncing files from both nodes:

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a files --json | jq '.files[] | {path, device_origin}'
```

**Expected**: files added by Node A show Node A's device ID as `device_origin`.
Files added by Node B show Node B's device ID.

---

## A24 — Concurrent CLI Requests

Run multiple CLI commands simultaneously against the same daemon:

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a status &
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a devices &
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a files &
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a folder list &
wait
```

**Expected**: all four commands complete without errors. No deadlocks, no garbled output.

---

## A25 — Bidirectional Delta Sync

Stop Node B. Add a file on Node A. Simultaneously add a file on Node B (offline):

```bash
# Stop Node B (Ctrl+C in Terminal 2)
echo "From A while B offline" > /tmp/test-bidir-a.txt
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a add /tmp/test-bidir-a.txt
```

On Node B's CLI (daemon stopped, so this adds to local state only if daemon was running.
If not, restart daemon first, then stop and add):

Restart both daemons. Wait for sync.

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a status --json | jq .dag_entries
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-b status --json | jq .dag_entries
```

**Expected**: DAG entry counts match after bidirectional delta sync.

---

## A26 — Folder Operations After Restart

Restart Node A. Verify folder operations work on persisted state:

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a folder list
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a folder create "post-restart"
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a folder list
```

**Expected**: pre-existing folders are present. New folder "post-restart" is created
successfully.

---

## A27 — File History Survives Restart

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a history <DEFAULT_FOLDER_ID> test-file-a.txt
```

**Expected**: version history is preserved across daemon restart. Same entries as before.

---

## A28 — Approve With Different Roles

Set up a scenario to test all three roles:

```bash
# Assuming node-b-v2 is already approved as "full"
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a devices --json | jq '.devices[] | {name, role}'
```

**Expected**: "node-a" is "full", "node-b-v2" is "full" (or whatever was set in A7).

---

## A29 — Multiple Folders with Different Subscription States

Create several folders and verify mixed subscription states on Node B:

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a folder create "music"
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a folder create "videos"
```

Wait for sync. On Node B, subscribe to only one:

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-b folder subscribe <MUSIC_FOLDER_ID> /tmp/murmur-b-music
```

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-b folder list --json
```

**Expected**: "music" is `subscribed: true`, "videos" is `subscribed: false`.

---

## A30 — DAG Consistency Final Check

After all operations across all test suites:

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a status --json
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-b status --json
```

**Expected**:
- Same `dag_entries` count on both nodes
- Same `network_id` on both nodes
- `peer_count >= 1` on both nodes

---

## A31 — Clean Shutdown (Both Nodes)

Stop both daemons with Ctrl+C.

**Expected**:
- No panics or error traces in either terminal
- Socket files cleaned up

```bash
ls /tmp/murmur-a/murmurd.sock 2>&1
ls /tmp/murmur-b/murmurd.sock 2>&1
```

**Expected**: both return "No such file or directory".

---

## Cleanup

```bash
rm -rf /tmp/murmur-a /tmp/murmur-b /tmp/murmur-c /tmp/murmur-b-photos /tmp/murmur-b-music
rm -f /tmp/test-revoked.txt /tmp/test-empty.txt /tmp/test-bidir-a.txt
rm -f /tmp/test-batch-*.txt
```

## Quick Reference

| # | What | Pass criteria |
|---|------|---------------|
| A1 | Revoke device | "Device revoked" |
| A2 | Revoked removed | Only creator in device list |
| A3 | Revoked can't sync | Revoked node's entries rejected by Node A |
| A4 | Revoke invalid ID | Error response |
| A5 | Approve bad hex | Error about invalid ID |
| A6 | Approve bad role | Error about invalid role |
| A7 | Re-join after revoke | New device joins and is approved |
| A8 | Shutdown socket cleanup | Socket file removed |
| A9 | Stale socket | Daemon removes stale socket, starts normally |
| A10 | Persist: devices | Devices survive restart |
| A11 | Persist: files | Files survive restart |
| A12 | Persist: folders | Folders survive restart |
| A13 | Persist: mnemonic | Mnemonic survives restart |
| A14 | Persist: DAG count | Entry count matches pre-restart |
| A15 | Persist: reconnection | Peers reconnect after restart |
| A16 | Filesystem: Node A | config.toml, mnemonic, db/, blobs/ correct |
| A17 | Filesystem: Node B | config.toml, device.key correct |
| A18 | Network isolation | Separate network has 0 peers, no crosstalk |
| A19 | Batch file sync | 20 files sync, DAG consistent |
| A20 | Empty file | 0-byte file added successfully |
| A21 | Non-existent file | Error response |
| A22 | Status fields | All JSON fields present and valid |
| A23 | Origin tracking | device_origin matches adding node |
| A24 | Concurrent requests | Four parallel commands succeed |
| A25 | Bidirectional delta | Both sides sync offline entries |
| A26 | Folders after restart | Folder create works on persisted state |
| A27 | History after restart | Version history survives restart |
| A28 | Role assignment | Different roles visible in device list |
| A29 | Mixed subscriptions | Subscribed/unsubscribed states per folder |
| A30 | Final DAG consistency | Same entry count, same network ID |
| A31 | Clean shutdown | No panics, sockets cleaned |
