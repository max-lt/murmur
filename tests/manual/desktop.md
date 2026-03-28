# Desktop + CLI: End-to-End Test

Tests the desktop GUI app alongside a CLI daemon node. The daemon handles
networking (gossip, blob sync); the desktop app manages a second device via its
GUI. Both run on the same machine with separate data directories.

**Architecture note**: the desktop app embeds the engine directly (no IPC daemon).
It has its own Fjall storage and blob directory.

This test requires **3 terminal windows**: Terminal 1 runs the daemon, Terminal 2
runs CLI commands, Terminal 3 runs the desktop app.

## Prerequisites

```bash
cargo build -p murmurd -p murmur-cli -p murmur-desktop
rm -rf /tmp/murmur-a /tmp/murmur-desktop
```

---

## Phase 1 — Create Network via Daemon

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

## Phase 2 — Launch Desktop App (Join Mode)

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

## Phase 3 — Approve Desktop Device from CLI

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

## Phase 4 — File Sharing (CLI to Desktop)

Add a file from the CLI daemon:

```bash
echo "Hello from CLI daemon — $(date)" > /tmp/test-daemon-file.txt
cargo run --bin murmur-cli -- --data-dir /tmp/murmur-a add /tmp/test-daemon-file.txt
```

**Expected**: "File added" with the blob hash.

Wait 5-10 seconds for gossip + blob sync.

In the desktop app, navigate to the **Files tab**.

**Expected**: `test-daemon-file.txt` appears in the file list with path, size,
and MIME type `text/plain`.

**Checkpoint D4**: File added on CLI daemon synced to desktop app.

---

## Phase 5 — File Sharing (Desktop to CLI)

Create a test file and add it from the desktop app:

```bash
echo "File added from desktop app" > /tmp/test-desktop.txt
```

Navigate to the **Files tab** in the desktop app.

1. Type `/tmp/test-desktop.txt` in the file path input field
2. Click **"Add File"**

**Expected**: the file appears in the files list with path, size, and MIME type.

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

## Phase 6 — Verify Desktop State via Filesystem

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

## Phase 7 — Desktop Device Management

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

## Phase 8 — Desktop App Restart Persistence

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

## Phase 9 — Create Network via Desktop (Fresh)

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

## Cleanup

```bash
rm -rf /tmp/murmur-a /tmp/murmur-desktop /tmp/murmur-desktop-new /tmp/test-daemon-file.txt /tmp/test-desktop.txt
```

## Quick Reference

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
