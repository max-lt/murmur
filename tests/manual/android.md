# Android App + CLI: End-to-End Test

Tests the Android app (running on an emulator) alongside a CLI daemon node. The daemon
handles the network; the Android app joins via mnemonic, syncs files, and manages
devices through its Compose UI. The Android app embeds the full engine + networking
via `murmur-ffi` (UniFFI), running iroh natively in the Rust core — unlike the desktop
app, it **can sync over the network**.

This test requires **3 terminal windows**: Terminal 1 runs the daemon, Terminal 2
runs CLI commands, Terminal 3 manages the emulator and ADB.

## Prerequisites

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

---

## Phase 1 — Create Network via CLI Daemon

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

## Phase 2 — Launch Android App and Join Network

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

## Phase 3 — Approve Android Device from CLI

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

## Phase 4 — File Sync: CLI to Android

Add a file from the CLI daemon:

```bash
echo "Hello from CLI — $(date)" > /tmp/test-cli-file.txt
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a add /tmp/test-cli-file.txt
```

Wait 5-10 seconds for gossip + blob sync.

In the Android app, navigate to the **Files tab**.

**Expected**: `test-cli-file.txt` appears in the file list with path, size, and
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

## Phase 5 — File Sync: Android to CLI

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

## Phase 6 — DAG Sync on Android App Restart

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

## Phase 7 — Status and DAG Consistency

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

## Phase 8 — Foreground Service Persistence

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

## Phase 9 — Create Network via Android (Fresh Install)

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

## Cleanup

```bash
# Stop daemon
# (Ctrl+C in Terminal 1)

# Uninstall app
adb uninstall net.murmur.app

# Clean up
rm -rf /tmp/murmur-a /tmp/test-cli-file.txt /tmp/test-android-file.txt /tmp/test-offline-android.txt
```

## Quick Reference

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
