# Manual Testing Guide

End-to-end manual test for verifying Murmur's core functionality using two daemon
instances on the same machine. This test covers: network creation, device join/approval,
file sharing, blob sync, DAG sync, status reporting, and device revocation.

## Prerequisites

```bash
cargo build -p murmurd -p murmur-cli -p murmur-desktop
```

Binaries will be at `target/debug/murmurd`, `target/debug/murmur-cli`, and
`target/debug/murmur-desktop`. The commands below use `cargo run` for convenience,
but you can substitute direct binary paths.

Clean up any previous test state:

```bash
rm -rf /tmp/murmur-a /tmp/murmur-b /tmp/murmur-desktop
```

---

## CLI: Full End-to-End Test

This test requires **4 terminal windows** (or tabs). Terminals 1 and 2 run the daemons;
terminals 3 and 4 run CLI commands against each daemon.

### Phase 1 — Network Creation (Node A)

**Terminal 1** — start the first daemon (creates a new network automatically):

```bash
cargo run --bin murmurd -- --data-dir /tmp/murmur-a --name "node-a" --role full --verbose
```

**Expected output** — the daemon prints a 24-word BIP39 mnemonic, then starts listening:

```
========================================
  NEW MURMUR NETWORK CREATED
========================================
Your recovery mnemonic (write this down!):

  word1 word2 word3 ... word24

This mnemonic is the ONLY way to recover your network.
Store it safely offline. Anyone with this phrase can access your data.
========================================
...
Listening on socket: /tmp/murmur-a/murmurd.sock
```

**Save the mnemonic** — copy the 24 words. You will need them for Node B.

**Terminal 3** — verify Node A is running:

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a status
```

**Expected**: shows device name "node-a", role "full", 0 peers, 1 DAG entry (the
initial `DeviceApproved` for the creator).

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a devices
```

**Expected**: one device listed — "node-a" with role "full".

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a files
```

**Expected**: no files.

**Checkpoint 1**: Node A is running, network created, status is healthy.

---

### Phase 2 — Device Join (Node B)

**Terminal 4** — join Node B to the network using the mnemonic from Phase 1:

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-b join "<PASTE 24 WORDS HERE>" --name "node-b" --role backup
```

**Expected**: prints the device ID for node-b and confirms it joined.

**Terminal 2** — start the second daemon:

```bash
cargo run --bin murmurd -- --data-dir /tmp/murmur-b --name "node-b" --verbose
```

**Expected**: daemon starts, broadcasts a `DeviceJoinRequest`, and tries to connect
to the creator's iroh endpoint. You should see gossip connection logs.

**Terminal 3** — check Node A sees the pending device:

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a pending
```

**Expected**: one pending device — "node-b".

**Checkpoint 2**: Node B is running and has sent a join request visible on Node A.

---

### Phase 3 — Device Approval

**Terminal 3** — approve node-b from node-a. Use the device ID from the `pending` output:

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a approve <NODE_B_DEVICE_ID> --role backup
```

**Expected**: "Device approved" confirmation.

Wait a few seconds for gossip propagation, then verify from both sides:

```bash
# On Node A
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a devices
```

**Expected**: two devices — "node-a" (full) and "node-b" (backup).

```bash
# On Node B
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-b devices
```

**Expected**: same two devices listed.

```bash
# On Node A — confirm peer is connected
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a status
```

**Expected**: `peers: 1` (or more, depending on timing).

**Checkpoint 3**: Both devices are approved and see each other in the device list.

---

### Phase 4 — File Sharing (A to B)

Create a test file and add it from Node A:

```bash
echo "Hello from Node A — $(date)" > /tmp/test-file-a.txt
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a add /tmp/test-file-a.txt
```

**Expected**: "File added" with the blob hash.

Verify on Node A:

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a files
```

**Expected**: one file — `test-file-a.txt` with size and MIME type `text/plain`.

Wait 5-10 seconds for gossip + blob sync, then verify on Node B:

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-b files
```

**Expected**: same file `test-file-a.txt` appears on Node B.

Verify the blob data was actually transferred (not just the DAG entry):

```bash
ls /tmp/murmur-b/blobs/
```

**Expected**: blob directory structure exists with the file data. You can also verify
the content matches by comparing blake3 hashes:

```bash
b3sum /tmp/test-file-a.txt
# The hash (first 64 hex chars) should match the blob hash shown by `files`
```

**Checkpoint 4**: File added on Node A is visible and synced to Node B.

---

### Phase 5 — File Sharing (B to A)

Now test the reverse direction — add a file from Node B:

```bash
echo "Hello from Node B — $(date)" > /tmp/test-file-b.txt
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-b add /tmp/test-file-b.txt
```

Wait 5-10 seconds, then verify on Node A:

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a files
```

**Expected**: two files — `test-file-a.txt` and `test-file-b.txt`.

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-b files
```

**Expected**: same two files on Node B.

**Checkpoint 5**: Bidirectional file sync works.

---

### Phase 6 — Larger File Transfer

Test with a larger file to exercise chunked transfer (if the file is > 4 MB):

```bash
# Create a 5 MB test file
dd if=/dev/urandom of=/tmp/test-large.bin bs=1M count=5 2>/dev/null
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a add /tmp/test-large.bin
```

Check transfer status on Node A:

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a transfers
```

**Expected**: may show in-flight transfer or empty if already complete.

Wait 10-15 seconds, then verify on Node B:

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-b files
```

**Expected**: three files including `test-large.bin` (~5 MB).

Verify integrity:

```bash
b3sum /tmp/test-large.bin
# Compare with the blob hash from `files` output
```

**Checkpoint 6**: Large file transfer completes with correct hash.

---

### Phase 7 — DAG Sync on Reconnect

Test that a restarted node catches up via delta sync instead of getting the full
history broadcast.

**Terminal 2** — stop Node B (Ctrl+C).

While Node B is offline, add another file on Node A:

```bash
echo "Added while B was offline" > /tmp/test-offline.txt
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a add /tmp/test-offline.txt
```

**Terminal 2** — restart Node B:

```bash
cargo run --bin murmurd -- --data-dir /tmp/murmur-b --name "node-b" --verbose
```

Watch the verbose logs — you should see `DagSyncRequest` / `DagSyncResponse` messages
exchanged (delta sync), not a full history broadcast.

Wait a few seconds, then verify Node B caught up:

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-b files
```

**Expected**: all four files, including `test-offline.txt`.

**Checkpoint 7**: Delta sync works after reconnection.

---

### Phase 8 — Mnemonic Display

Verify the mnemonic can be retrieved:

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a mnemonic
```

**Expected**: displays the same 24-word mnemonic from Phase 1.

**Checkpoint 8**: Mnemonic is persisted and retrievable.

---

### Phase 9 — Status and DAG Consistency

Check that both nodes agree on the DAG state:

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a status --json
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-b status --json
```

**Expected**: both show the same `dag_entries` count. Peer count should be `1` on both.

**Checkpoint 9**: DAG is consistent across nodes.

---

### Phase 10 — Device Revocation

Revoke Node B from Node A:

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a revoke <NODE_B_DEVICE_ID>
```

**Expected**: "Device revoked" confirmation.

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a devices
```

**Expected**: only "node-a" remains in the approved list.

Try adding a file from the revoked Node B:

```bash
echo "From revoked node" > /tmp/test-revoked.txt
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-b add /tmp/test-revoked.txt
```

**Expected**: the file is added locally, but the DAG entry should be rejected by
Node A (check Node A verbose logs for "unauthorized" or "revoked" messages).

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a files
```

**Expected**: the revoked node's file does NOT appear on Node A.

**Checkpoint 10**: Revocation works — revoked device cannot sync new entries.

---

### Phase 11 — Graceful Shutdown

Stop both daemons with Ctrl+C (SIGINT).

**Expected**: both daemons shut down cleanly without errors. Check that:
- Socket files are removed: `ls /tmp/murmur-a/murmurd.sock` should not exist
- No crash logs or panics in the terminal output

**Checkpoint 11**: Clean shutdown.

---

### Phase 12 — Restart and State Persistence

Restart both daemons:

```bash
# Terminal 1
cargo run --bin murmurd -- --data-dir /tmp/murmur-a --name "node-a" --verbose

# Terminal 2
cargo run --bin murmurd -- --data-dir /tmp/murmur-b --name "node-b" --verbose
```

Verify state was persisted:

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a files
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a devices
```

**Expected**: all previously added files and device list are intact (loaded from
Fjall on startup).

**Checkpoint 12**: State survives daemon restart.

---

### CLI Test Cleanup

```bash
rm -rf /tmp/murmur-a /tmp/murmur-b /tmp/test-file-a.txt /tmp/test-file-b.txt /tmp/test-large.bin /tmp/test-offline.txt /tmp/test-revoked.txt
```

### CLI Test — Quick Reference

| # | What | Pass criteria |
|---|------|---------------|
| 1 | Network creation | Node A running, mnemonic printed, status shows 1 entry |
| 2 | Device join | Node B pending on Node A |
| 3 | Device approval | Both nodes see both devices |
| 4 | File sync A→B | File added on A appears on B with blob data |
| 5 | File sync B→A | File added on B appears on A |
| 6 | Large file | 5 MB file syncs with correct hash |
| 7 | Delta sync | Offline file synced after reconnect |
| 8 | Mnemonic | Retrievable and matches original |
| 9 | DAG consistency | Same entry count on both nodes |
| 10 | Revocation | Revoked device entries rejected |
| 11 | Graceful shutdown | No panics, socket cleaned up |
| 12 | Persistence | State intact after restart |

---

## Desktop + CLI: End-to-End Test

Tests the desktop GUI app alongside a CLI daemon node. The daemon handles
networking (gossip, blob sync); the desktop app manages a second device via its
GUI. Both run on the same machine with separate data directories.

**Architecture note**: the desktop app embeds the engine directly (no IPC daemon).
It has its own Fjall storage and blob directory.

This test requires **3 terminal windows**: Terminal 1 runs the daemon, Terminal 2
runs CLI commands, Terminal 3 runs the desktop app.

### Prerequisites

```bash
cargo build -p murmurd -p murmur-cli -p murmur-desktop
rm -rf /tmp/murmur-a /tmp/murmur-desktop
```

### Phase 1 — Create Network via Daemon

**Terminal 1** — start the daemon (creates a new network):

```bash
cargo run --bin murmurd -- --data-dir /tmp/murmur-a --name "daemon-node" --role full --verbose
```

**Save the mnemonic** printed to stdout.

**Terminal 2** — verify:

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a status
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a devices
```

**Expected**: daemon running, 1 device "daemon-node" (full), 2 DAG entries.

**Checkpoint D1**: Daemon network created and healthy.

---

### Phase 2 — Launch Desktop App (Join Mode)

**Terminal 3** — launch the desktop app with a custom data directory:

```bash
MURMUR_DATA_DIR=/tmp/murmur-desktop cargo run --bin murmur-desktop
```

The desktop app opens to the **Setup screen**.

1. Enter a device name (e.g. "desktop-node") in the name field
2. Click **"Join existing network"** to switch to join mode
3. Paste the 24-word mnemonic from Phase 1 into the mnemonic field
4. Click **"Join Network"**

**Expected**: the app transitions to the **Devices tab**. It shows the desktop
device as a single entry (pending approval — only visible to itself).

**Checkpoint D2**: Desktop app initialized in join mode, shows UI with device info.

---

### Phase 3 — Approve Desktop Device from CLI

**Terminal 2** — check the CLI daemon sees the pending device:

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a pending
```

**Expected**: one pending device — "desktop-node".

Approve the desktop device. Use the device ID from the `pending` output:

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a approve <DESKTOP_DEVICE_ID> --role full
```

**Expected**: "Device approved" confirmation.

Wait a few seconds for gossip propagation, then verify from both sides:

```bash
# On CLI daemon
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a devices
```

**Expected**: two devices — "daemon-node" (full) and "desktop-node" (full).

In the desktop app, navigate to the **Devices tab**.

**Expected**: both devices listed — "daemon-node" and "desktop-node".

**Checkpoint D3**: Desktop device approved, both sides see both devices.

---

### Phase 4 — File Sharing (CLI to Desktop)

Add a file from the CLI daemon:

```bash
echo "Hello from CLI daemon — $(date)" > /tmp/test-daemon-file.txt
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a add /tmp/test-daemon-file.txt
```

**Expected**: "File added" with the blob hash.

Wait 5-10 seconds for gossip + blob sync.

In the desktop app, navigate to the **Files tab**.

**Expected**: `test-daemon-file.txt` appears in the file list with filename, size,
and MIME type `text/plain`.

**Checkpoint D4**: File added on CLI daemon synced to desktop app.

---

### Phase 5 — File Sharing (Desktop to CLI)

Create a test file and add it from the desktop app:

```bash
echo "File added from desktop app" > /tmp/test-desktop.txt
```

Navigate to the **Files tab** in the desktop app.

1. Type `/tmp/test-desktop.txt` in the file path input field
2. Click **"Add File"**

**Expected**: the file appears in the files list with filename, size, and MIME type.

Verify the blob was stored:

```bash
ls /tmp/murmur-desktop/blobs/
```

**Expected**: blob directory structure with the file data.

Wait 5-10 seconds, then verify on CLI:

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a files
```

**Expected**: two files — `test-daemon-file.txt` and `test-desktop.txt`.

**Checkpoint D5**: Bidirectional file sync between desktop and CLI works.

---

### Phase 6 — Verify Desktop State via Filesystem

Check that the desktop app created proper state:

```bash
# Config exists
cat /tmp/murmur-desktop/config.toml

# Mnemonic matches the daemon's
cat /tmp/murmur-desktop/mnemonic

# Device key exists (joining device has a separate key)
ls -la /tmp/murmur-desktop/device.key

# DAG entries persisted
ls /tmp/murmur-desktop/db/
```

**Expected**:
- `config.toml` has device name "desktop-node" and role "full"
- Mnemonic matches the one from Phase 1
- `device.key` is a 32-byte file
- `db/` directory contains Fjall keyspaces

**Checkpoint D6**: Desktop state persisted correctly to disk.

---

### Phase 7 — Desktop Device Management

Navigate to the **Devices tab** in the desktop app.

**Expected**: shows the desktop device in the devices list.

Navigate to the **Status tab**.

**Expected**: shows:
- Device ID (64-character hex)
- DAG entry count (should be >= 1 for the join request)
- File count (2 — files from Phases 4 and 5)
- Data directory path (`/tmp/murmur-desktop`)
- Event log with recent events (NetworkJoined, FileAdded, etc.)

**Checkpoint D7**: Desktop UI displays correct status and event history.

---

### Phase 8 — Desktop App Restart Persistence

Close the desktop app (close the window or Ctrl+C in Terminal 3).

Relaunch it:

```bash
MURMUR_DATA_DIR=/tmp/murmur-desktop cargo run --bin murmur-desktop
```

**Expected**: the app skips the Setup screen and loads directly into the Devices
tab. All previously added files, devices, and events are intact.

Navigate to the **Files tab** — files from Phases 4 and 5 should still be listed.

**Checkpoint D8**: Desktop app state persists across restarts.

---

### Phase 9 — Create Network via Desktop (Fresh)

Test that the desktop app can also create a new network (not just join).

```bash
rm -rf /tmp/murmur-desktop-new
MURMUR_DATA_DIR=/tmp/murmur-desktop-new cargo run --bin murmur-desktop
```

1. Enter device name "desktop-creator"
2. Ensure **"Create new network"** mode is selected (default)
3. Click **"Generate Mnemonic"** — a 24-word phrase appears
4. Click **"Create Network"**

**Expected**: app transitions to Devices tab showing "desktop-creator" as an
approved device with role "full".

Navigate to the **Status tab** — should show 2 DAG entries (DeviceApproved +
DeviceNameChanged).

Verify on disk:

```bash
cat /tmp/murmur-desktop-new/mnemonic
cat /tmp/murmur-desktop-new/config.toml
```

**Expected**: mnemonic saved, config with device name "desktop-creator".

**Checkpoint D9**: Desktop app can create new networks independently.

---

### Desktop Test Cleanup

```bash
rm -rf /tmp/murmur-a /tmp/murmur-desktop /tmp/murmur-desktop-new /tmp/test-daemon-file.txt /tmp/test-desktop.txt
```

### Desktop + CLI Test — Quick Reference

| # | What | Pass criteria |
|---|------|---------------|
| D1 | Daemon network | Daemon running, 1 device, healthy status |
| D2 | Desktop join | Setup screen, join mode, transitions to Devices tab |
| D3 | Device approval | Desktop pending on CLI, approved, both sides see both devices |
| D4 | File sync CLI→Desktop | File added on CLI appears on desktop |
| D5 | File sync Desktop→CLI | File added on desktop appears on CLI, blob on disk |
| D6 | Filesystem state | config.toml, mnemonic, device.key, db/ all present |
| D7 | Status UI | Device ID, entry count, file count, event log correct |
| D8 | Restart persistence | State intact after relaunch |
| D9 | Desktop create network | Fresh network created, mnemonic saved |

---

## Android App + CLI: End-to-End Test

Tests the Android app (running on an emulator) alongside a CLI daemon node. The daemon
handles the network; the Android app joins via mnemonic, syncs files, and manages
devices through its Compose UI. The Android app embeds the full engine + networking
via `murmur-ffi` (UniFFI), running iroh natively in the Rust core — unlike the desktop
app, it **can sync over the network**.

This test requires **3 terminal windows**: Terminal 1 runs the daemon, Terminal 2
runs CLI commands, Terminal 3 manages the emulator and ADB.

### Prerequisites

Build the CLI tools and Android APK:

```bash
# CLI tools
cargo build -p murmurd -p murmur-cli

# Android native libs (skip if jniLibs already built)
cargo ndk -t x86_64 -o platforms/android/app/src/main/jniLibs build -p murmur-ffi

# Android APK
cd platforms/android && ./gradlew assembleDebug && cd ../..
```

Start the Android emulator (x86_64 image required for the native libs):

```bash
# Terminal 3
emulator -avd <AVD_NAME> -no-snapshot-load &
# Wait for boot:
adb wait-for-device
adb shell getprop sys.boot_completed  # should print "1"
```

Clean up previous test state:

```bash
rm -rf /tmp/murmur-a
# Clear Android app data (if previously installed):
adb shell pm clear net.murmur.app 2>/dev/null || true
```

Install the APK:

```bash
adb install -r platforms/android/app/build/outputs/apk/debug/app-debug.apk
```

### Phase 1 — Create Network via CLI Daemon

**Terminal 1** — start the daemon:

```bash
cargo run --bin murmurd -- --data-dir /tmp/murmur-a --name "cli-node" --role full --verbose
```

**Save the 24-word mnemonic** printed to stdout.

**Terminal 2** — verify:

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a status
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a devices
```

**Expected**: daemon running, 1 device "cli-node" (full), 2 DAG entries.

**Checkpoint A1**: CLI daemon network created and healthy.

---

### Phase 2 — Launch Android App and Join Network

**Terminal 3** — launch the app:

```bash
adb shell am start -n net.murmur.app/.ui.MainActivity
```

The app opens to the **Setup screen**.

1. Tap **"Join network"** button to switch to join mode
2. Enter a device name (e.g. "android-phone")
3. Paste the 24-word mnemonic from Phase 1 into the mnemonic field
4. Tap **"Join"**

**Expected**: the app shows a loading spinner briefly, then transitions to the
**Devices tab**. The Android device appears in its own device list.

Check the daemon logs (Terminal 1) — you should see gossip peer connection messages
and a `DeviceJoinRequest` entry received.

**Terminal 2** — verify the join request arrived:

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a pending
```

**Expected**: one pending device — "android-phone".

**Checkpoint A2**: Android app joined, join request visible on CLI daemon.

---

### Phase 3 — Approve Android Device from CLI

**Terminal 2** — approve the Android device:

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a approve <ANDROID_DEVICE_ID> --role backup
```

**Expected**: "Device approved" confirmation.

Wait 5-10 seconds for gossip propagation, then verify from CLI:

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a devices
```

**Expected**: two devices — "cli-node" (full) and "android-phone" (backup).

In the Android app, navigate to the **Devices tab**.

**Expected**: both devices listed — "cli-node" and "android-phone".

**Checkpoint A3**: Android device approved, both sides see both devices.

---

### Phase 4 — File Sync: CLI to Android

Add a file from the CLI daemon:

```bash
echo "Hello from CLI — $(date)" > /tmp/test-cli-file.txt
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a add /tmp/test-cli-file.txt
```

Wait 5-10 seconds for gossip + blob sync.

In the Android app, navigate to the **Files tab**.

**Expected**: `test-cli-file.txt` appears in the file list with filename, size, and
MIME type `text/plain`.

**Terminal 2** — verify from CLI:

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a files
```

**Expected**: one file — `test-cli-file.txt`.

Verify the blob landed on the Android device:

```bash
adb shell ls /data/data/net.murmur.app/files/blobs/
```

**Expected**: blob directory structure with content-addressed file data.

**Checkpoint A4**: File added on CLI synced to Android app with blob data.

---

### Phase 5 — File Sync: Android to CLI

In the Android app, navigate to the **Files tab**.

To add a file from the Android side, first push a test file to the emulator:

```bash
echo "Hello from Android — $(date)" > /tmp/test-android-file.txt
adb push /tmp/test-android-file.txt /sdcard/Download/test-android-file.txt
```

In the Files tab, use the file picker (if available) or the app's add-file interface
to add `/sdcard/Download/test-android-file.txt`.

Wait 5-10 seconds for sync.

**Terminal 2** — verify on CLI:

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a files
```

**Expected**: two files — `test-cli-file.txt` and `test-android-file.txt`.

**Checkpoint A5**: Bidirectional file sync between Android and CLI works.

---

### Phase 6 — DAG Sync on Android App Restart

Force-stop the Android app:

```bash
adb shell am force-stop net.murmur.app
```

While the app is stopped, add a file from CLI:

```bash
echo "Added while Android offline — $(date)" > /tmp/test-offline-android.txt
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a add /tmp/test-offline-android.txt
```

Relaunch the Android app:

```bash
adb shell am start -n net.murmur.app/.ui.MainActivity
```

Wait 10-15 seconds for the foreground service to restart and sync.

Navigate to the **Files tab** in the app.

**Expected**: all files appear, including `test-offline-android.txt`.

Check the daemon logs for `DagSyncRequest`/`DagSyncResponse` (delta sync on reconnect).

**Checkpoint A6**: Android app catches up after restart via delta sync.

---

### Phase 7 — Status and DAG Consistency

In the Android app, navigate to the **Status tab**.

**Expected**:
- Device ID displayed (64-character hex)
- Event log showing recent events (DeviceApproved, FileAdded, etc.)

**Terminal 2** — check CLI status:

```bash
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a status
```

**Expected**: `dag_entries` count matches what the Android app reports. Peer count
should be `1`.

**Checkpoint A7**: DAG consistent between Android and CLI.

---

### Phase 8 — Foreground Service Persistence

Verify the foreground service keeps running:

```bash
adb shell dumpsys activity services net.murmur.app/.MurmurService | head -20
```

**Expected**: service is running with `isForeground=true`.

Press the home button on the emulator (don't force-stop). Wait 30 seconds.

```bash
adb shell dumpsys activity services net.murmur.app/.MurmurService | head -20
```

**Expected**: service still running in foreground — background sync active.

**Checkpoint A8**: Foreground service persists in background.

---

### Phase 9 — Create Network via Android (Fresh Install)

Clear the app data to simulate a fresh install:

```bash
adb shell pm clear net.murmur.app
```

Launch the app:

```bash
adb shell am start -n net.murmur.app/.ui.MainActivity
```

1. Enter device name "android-creator"
2. Ensure **"Create network"** mode is selected (default)
3. Tap **"Generate & Create"**

**Expected**: a 24-word mnemonic is displayed. The app transitions to the Devices
tab showing "android-creator" as an approved device.

Navigate to the **Status tab** — should show DAG entries for the newly created network.

**Checkpoint A9**: Android app can create new networks independently.

---

### Android Test Cleanup

```bash
# Stop daemon
# (Ctrl+C in Terminal 1)

# Uninstall app
adb uninstall net.murmur.app

# Clean up
rm -rf /tmp/murmur-a /tmp/test-cli-file.txt /tmp/test-android-file.txt /tmp/test-offline-android.txt
```

### Android + CLI Test — Quick Reference

| # | What | Pass criteria |
|---|------|---------------|
| A1 | CLI daemon network | Daemon running, mnemonic printed, healthy status |
| A2 | Android join | App joins, join request visible on CLI |
| A3 | Device approval | Both sides see both devices |
| A4 | File sync CLI→Android | File added on CLI appears on Android with blob |
| A5 | File sync Android→CLI | File added on Android appears on CLI |
| A6 | Delta sync on restart | Offline file synced after app restart |
| A7 | DAG consistency | Same entry count on both sides |
| A8 | Foreground service | Service persists in background |
| A9 | Android create network | Fresh network created from Android app |
