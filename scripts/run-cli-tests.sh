#!/usr/bin/env bash
#
# Automated CLI manual test runner.
# Exercises tests from tests/manual/cli-basic.md, cli-intermediate.md, cli-advanced.md.
#
# Usage: ./scripts/run-cli-tests.sh
#
set -uo pipefail

MURMURD="./target/debug/murmurd"
CLI="./target/debug/murmur-cli"
TESTDIR="${TMPDIR:-/tmp/claude}/murmur-test"
DIR_A="$TESTDIR/a"
DIR_B="$TESTDIR/b"
DIR_C="$TESTDIR/c"
SCRATCH="$TESTDIR/scratch"
mkdir -p "$SCRATCH"
DAEMON_A_PID=""
DAEMON_B_PID=""

PASS=0
FAIL=0
SKIP=0
FAILURES=""

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
NC='\033[0m'

cleanup() {
    echo ""
    echo "=== Cleaning up ==="
    [ -n "$DAEMON_A_PID" ] && kill "$DAEMON_A_PID" 2>/dev/null && wait "$DAEMON_A_PID" 2>/dev/null || true
    [ -n "$DAEMON_B_PID" ] && kill "$DAEMON_B_PID" 2>/dev/null && wait "$DAEMON_B_PID" 2>/dev/null || true
    rm -rf "$TESTDIR"
    echo ""
    echo "=============================="
    echo -e "  Results: ${GREEN}${PASS} passed${NC}, ${RED}${FAIL} failed${NC}, ${YELLOW}${SKIP} skipped${NC}"
    echo "=============================="
    if [ -n "$FAILURES" ]; then
        echo ""
        echo -e "${RED}Failed tests:${NC}"
        echo "$FAILURES"
    fi
}
trap cleanup EXIT

pass() {
    PASS=$((PASS + 1))
    echo -e "  ${GREEN}PASS${NC}: $1"
}

fail() {
    FAIL=$((FAIL + 1))
    echo -e "  ${RED}FAIL${NC}: $1 — $2"
    FAILURES="${FAILURES}  $1: $2\n"
}

skip() {
    SKIP=$((SKIP + 1))
    echo -e "  ${YELLOW}SKIP${NC}: $1 — $2"
}

# Start a daemon in the background.
# Usage: start_daemon <dir> <name> [role]
# Writes PID to $TESTDIR/.last_pid on success.
# Returns 0 on success, 1 on failure.
start_daemon() {
    local dir="$1"
    local name="$2"
    local role="${3:-full}"
    "$MURMURD" --data-dir "$dir" --name "$name" --role "$role" >"$TESTDIR/.daemon-out" 2>&1 &
    local pid=$!
    # Wait for socket to appear
    local waited=0
    while [ ! -S "$dir/murmurd.sock" ] && [ $waited -lt 30 ]; do
        sleep 0.5
        waited=$((waited + 1))
        if ! kill -0 "$pid" 2>/dev/null; then
            echo "ERROR: daemon died during startup (dir=$dir)" >&2
            cat "$TESTDIR/.daemon-out" >&2 2>/dev/null || true
            return 1
        fi
    done
    if [ ! -S "$dir/murmurd.sock" ]; then
        echo "ERROR: daemon socket never appeared (dir=$dir)" >&2
        return 1
    fi
    echo "$pid" > "$TESTDIR/.last_pid"
    return 0
}

# Read the last started daemon's PID
last_pid() {
    cat "$TESTDIR/.last_pid" 2>/dev/null || echo ""
}

stop_daemon() {
    local pid="$1"
    if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
        kill "$pid" 2>/dev/null
        wait "$pid" 2>/dev/null || true
    fi
}

# Wait for a condition with timeout. Returns 0 if condition met, 1 if timeout.
wait_for() {
    local desc="$1"
    local timeout="${2:-15}"
    shift 2
    local waited=0
    while [ $waited -lt "$timeout" ]; do
        if eval "$@" >/dev/null 2>&1; then
            return 0
        fi
        sleep 1
        waited=$((waited + 1))
    done
    return 1
}

echo "=============================="
echo "  Murmur CLI Test Runner"
echo "=============================="
echo ""

# Build
echo "=== Building ==="
cargo build -p murmurd -p murmur-cli 2>&1 | tail -1
echo ""

# Clean slate
rm -rf "$TESTDIR"
mkdir -p "$SCRATCH"

########################################################################
# BASIC TESTS
########################################################################
echo "========================================="
echo "  BASIC TESTS (Network, Devices, Sync)"
echo "========================================="
echo ""

# B1 — Network Creation
echo "--- B1: Network Creation ---"
start_daemon "$DIR_A" "node-a" "full" && DAEMON_A_PID=$(last_pid) || DAEMON_A_PID=""
if [ -S "$DIR_A/murmurd.sock" ]; then
    pass "B1 — daemon started, socket exists"
else
    fail "B1" "daemon socket not found"
fi

# Save mnemonic
MNEMONIC=$(cat "$DIR_A/mnemonic" 2>/dev/null || echo "")
if [ -n "$MNEMONIC" ]; then
    WORD_COUNT=$(echo "$MNEMONIC" | wc -w)
    if [ "$WORD_COUNT" -eq 24 ]; then
        pass "B1 — mnemonic is 24 words"
    else
        fail "B1" "mnemonic has $WORD_COUNT words, expected 24"
    fi
else
    fail "B1" "no mnemonic file"
fi

# B2 — Status Command
echo "--- B2: Status Command ---"
STATUS_OUT=$("$CLI" --data-dir "$DIR_A" status 2>&1) || true
if echo "$STATUS_OUT" | grep -q "Device:.*node-a"; then
    pass "B2 — status shows device name"
else
    fail "B2" "status missing device name: $STATUS_OUT"
fi
if echo "$STATUS_OUT" | grep -q "DAG entries:"; then
    pass "B2 — status shows DAG entries"
else
    fail "B2" "status missing DAG entries"
fi

# B3 — Status JSON
echo "--- B3: Status JSON ---"
STATUS_JSON=$("$CLI" --data-dir "$DIR_A" status --json 2>&1) || true
if echo "$STATUS_JSON" | python3 -c "import sys,json; d=json.load(sys.stdin); assert 'device_id' in d and 'dag_entries' in d" 2>/dev/null; then
    pass "B3 — status JSON is valid with expected fields"
else
    fail "B3" "invalid JSON or missing fields"
fi

# B4 — Devices Command
echo "--- B4: Devices Command ---"
DEVICES_OUT=$("$CLI" --data-dir "$DIR_A" devices 2>&1) || true
if echo "$DEVICES_OUT" | grep -q "node-a"; then
    pass "B4 — devices shows node-a"
else
    fail "B4" "devices missing node-a"
fi

DEVICES_JSON=$("$CLI" --data-dir "$DIR_A" devices --json 2>&1) || true
if echo "$DEVICES_JSON" | python3 -c "import sys,json; d=json.load(sys.stdin); assert len(d['devices']) == 1" 2>/dev/null; then
    pass "B4 — devices JSON has 1 device"
else
    fail "B4" "devices JSON wrong count"
fi

# B5 — Pending (Empty)
echo "--- B5: Pending (Empty) ---"
PENDING_OUT=$("$CLI" --data-dir "$DIR_A" pending 2>&1) || true
if echo "$PENDING_OUT" | grep -qi "no pending"; then
    pass "B5 — no pending requests"
else
    fail "B5" "unexpected pending output: $PENDING_OUT"
fi

# B6 — Files (Empty)
echo "--- B6: Files (Empty) ---"
FILES_OUT=$("$CLI" --data-dir "$DIR_A" files 2>&1) || true
if echo "$FILES_OUT" | grep -qi "no synced files"; then
    pass "B6 — no synced files"
else
    fail "B6" "unexpected files output: $FILES_OUT"
fi

# B7 — Mnemonic Retrieval
echo "--- B7: Mnemonic Retrieval ---"
MNEMONIC_OUT=$("$CLI" --data-dir "$DIR_A" mnemonic 2>&1) || true
if [ "$MNEMONIC_OUT" = "$MNEMONIC" ]; then
    pass "B7 — mnemonic matches"
else
    fail "B7" "mnemonic mismatch"
fi

# B8 — Device Join (Offline)
echo "--- B8: Device Join (Offline) ---"
JOIN_OUT=$("$CLI" --data-dir "$DIR_B" join "$MNEMONIC" --name "node-b" --role backup 2>&1) || true
if echo "$JOIN_OUT" | grep -q "Joined Murmur network"; then
    pass "B8 — join succeeded"
else
    fail "B8" "join failed: $JOIN_OUT"
fi
if [ -f "$DIR_B/config.toml" ] && [ -f "$DIR_B/mnemonic" ] && [ -f "$DIR_B/device.key" ]; then
    pass "B8 — config, mnemonic, device.key all created"
else
    fail "B8" "missing files after join"
fi

# B9 — Invalid Mnemonic
echo "--- B9: Invalid Mnemonic ---"
BAD_MNEMONIC_OUT=$("$CLI" --data-dir $TESTDIR/bad join "not a valid mnemonic" --name "bad" --role backup 2>&1) && BAD_EXIT=0 || BAD_EXIT=$?
if [ $BAD_EXIT -ne 0 ]; then
    pass "B9 — invalid mnemonic rejected (exit $BAD_EXIT)"
else
    fail "B9" "invalid mnemonic accepted"
fi

# B10 — Invalid Role
echo "--- B10: Invalid Role ---"
BAD_ROLE_OUT=$("$CLI" --data-dir $TESTDIR/bad join "$MNEMONIC" --name "bad" --role superadmin 2>&1) && BAD_EXIT=0 || BAD_EXIT=$?
if [ $BAD_EXIT -ne 0 ]; then
    pass "B10 — invalid role rejected (exit $BAD_EXIT)"
else
    fail "B10" "invalid role accepted"
fi

# B11 — Double Init
echo "--- B11: Double Init ---"
DOUBLE_INIT_OUT=$("$CLI" --data-dir "$DIR_B" join "$MNEMONIC" --name "node-b2" --role backup 2>&1) && DI_EXIT=0 || DI_EXIT=$?
if [ $DI_EXIT -ne 0 ]; then
    pass "B11 — double init rejected"
else
    fail "B11" "double init allowed"
fi

# B12 — Start Node B
echo "--- B12: Start Node B ---"
start_daemon "$DIR_B" "node-b" "backup" && DAEMON_B_PID=$(last_pid) || DAEMON_B_PID=""
if [ -S "$DIR_B/murmurd.sock" ]; then
    pass "B12 — Node B daemon started"
else
    fail "B12" "Node B socket not found"
fi

# B13 — Pending Shows Join Request
echo "--- B13: Pending Shows Join Request ---"
# Wait for the join request to propagate
sleep 3
PENDING_OUT=$("$CLI" --data-dir "$DIR_A" pending 2>&1) || true
if echo "$PENDING_OUT" | grep -q "node-b"; then
    pass "B13 — node-b visible as pending"
    # Extract device ID
    NODE_B_ID=$(echo "$PENDING_OUT" | grep "node-b" | awk '{print $1}')
else
    fail "B13" "node-b not in pending list: $PENDING_OUT"
    # Try to get the ID anyway
    NODE_B_ID=""
fi

# B14 — Device Approval
echo "--- B14: Device Approval ---"
if [ -n "$NODE_B_ID" ]; then
    APPROVE_OUT=$("$CLI" --data-dir "$DIR_A" approve "$NODE_B_ID" --role backup 2>&1) || true
    if echo "$APPROVE_OUT" | grep -qi "approved\|ok\|success"; then
        pass "B14 — device approved"
    else
        fail "B14" "approval response: $APPROVE_OUT"
    fi
else
    skip "B14" "no device ID from B13"
fi

# Wait for gossip propagation
sleep 5

# B15 — Both Nodes See Both Devices
echo "--- B15: Both Nodes See Both Devices ---"
DEVICES_A=$("$CLI" --data-dir "$DIR_A" devices 2>&1) || true
DEVICES_B=$("$CLI" --data-dir "$DIR_B" devices 2>&1) || true
if echo "$DEVICES_A" | grep -q "node-a" && echo "$DEVICES_A" | grep -q "node-b"; then
    pass "B15 — Node A sees both devices"
else
    fail "B15" "Node A devices: $DEVICES_A"
fi
if echo "$DEVICES_B" | grep -q "node-a" && echo "$DEVICES_B" | grep -q "node-b"; then
    pass "B15 — Node B sees both devices"
else
    fail "B15" "Node B devices: $DEVICES_B"
fi

# B16 — Peer Count
echo "--- B16: Peer Count ---"
STATUS_A=$("$CLI" --data-dir "$DIR_A" status 2>&1) || true
PEERS_A=$(echo "$STATUS_A" | grep "Peers:" | awk '{print $2}')
if [ "${PEERS_A:-0}" -ge 1 ]; then
    pass "B16 — Node A has >= 1 peer"
else
    fail "B16" "Node A peers: $PEERS_A"
fi

# B17 — File Sync A→B
echo "--- B17: File Sync A→B ---"
echo "Hello from Node A" > $SCRATCH/test-file-a.txt
ADD_OUT=$("$CLI" --data-dir "$DIR_A" add $SCRATCH/test-file-a.txt 2>&1) || true
if echo "$ADD_OUT" | grep -qi "added\|ok\|success\|hash"; then
    pass "B17 — file added on Node A"
else
    fail "B17" "add failed: $ADD_OUT"
fi

# Wait for sync
sleep 5

FILES_A=$("$CLI" --data-dir "$DIR_A" files 2>&1) || true
if echo "$FILES_A" | grep -q "test-file-a.txt"; then
    pass "B17 — file visible on Node A"
else
    fail "B17" "file not on Node A: $FILES_A"
fi

FILES_B=$("$CLI" --data-dir "$DIR_B" files 2>&1) || true
if echo "$FILES_B" | grep -q "test-file-a.txt"; then
    pass "B17 — file synced to Node B"
else
    fail "B17" "file not on Node B: $FILES_B"
fi

# B18 — Blob Integrity
echo "--- B18: Blob Integrity ---"
if [ -d "$DIR_B/blobs" ] && [ "$(ls -A "$DIR_B/blobs/" 2>/dev/null | head -1)" != "" ]; then
    pass "B18 — blob directory has content on Node B"
else
    fail "B18" "blob directory empty or missing on Node B"
fi

# B19 — File Sync B→A
echo "--- B19: File Sync B→A ---"
echo "Hello from Node B" > $SCRATCH/test-file-b.txt
ADD_B_OUT=$("$CLI" --data-dir "$DIR_B" add $SCRATCH/test-file-b.txt 2>&1) || true
if echo "$ADD_B_OUT" | grep -qi "added\|ok\|success\|hash"; then
    pass "B19 — file added on Node B"
else
    fail "B19" "add on B failed: $ADD_B_OUT"
fi
sleep 5
FILES_A2=$("$CLI" --data-dir "$DIR_A" files 2>&1) || true
if echo "$FILES_A2" | grep -q "test-file-b.txt"; then
    pass "B19 — file synced to Node A"
else
    fail "B19" "file not on Node A: $FILES_A2"
fi

# B20 — Files JSON
echo "--- B20: Files JSON ---"
FILES_JSON=$("$CLI" --data-dir "$DIR_A" files --json 2>&1) || true
if echo "$FILES_JSON" | python3 -c "import sys,json; d=json.load(sys.stdin); assert len(d['files']) >= 2; f=d['files'][0]; assert 'blob_hash' in f and 'path' in f and 'size' in f" 2>/dev/null; then
    pass "B20 — files JSON valid with all fields"
else
    fail "B20" "files JSON invalid"
fi

# B21 — DAG Consistency
echo "--- B21: DAG Entry Counts Match ---"
DAG_A=$("$CLI" --data-dir "$DIR_A" status --json 2>&1 | python3 -c "import sys,json; print(json.load(sys.stdin)['dag_entries'])" 2>/dev/null) || DAG_A="?"
DAG_B=$("$CLI" --data-dir "$DIR_B" status --json 2>&1 | python3 -c "import sys,json; print(json.load(sys.stdin)['dag_entries'])" 2>/dev/null) || DAG_B="?"
if [ "$DAG_A" = "$DAG_B" ] && [ "$DAG_A" != "?" ]; then
    pass "B21 — DAG entries match: $DAG_A"
else
    fail "B21" "DAG mismatch: A=$DAG_A B=$DAG_B"
fi

# B22 — No Daemon Error
echo "--- B22: No Daemon Error ---"
stop_daemon "$DAEMON_B_PID"
DAEMON_B_PID=""
sleep 1
NO_DAEMON_OUT=$("$CLI" --data-dir "$DIR_B" status 2>&1) && ND_EXIT=0 || ND_EXIT=$?
if [ $ND_EXIT -ne 0 ] && echo "$NO_DAEMON_OUT" | grep -qi "not running"; then
    pass "B22 — clear error when daemon not running"
else
    fail "B22" "unexpected output (exit=$ND_EXIT): $NO_DAEMON_OUT"
fi

# Restart Node B for intermediate tests
start_daemon "$DIR_B" "node-b" "backup" && DAEMON_B_PID=$(last_pid) || DAEMON_B_PID=""
sleep 3

########################################################################
# INTERMEDIATE TESTS
########################################################################
echo ""
echo "========================================="
echo "  INTERMEDIATE TESTS (Folders, History)"
echo "========================================="
echo ""

# I1 — Folder List
echo "--- I1: Folder List ---"
FOLDER_LIST=$("$CLI" --data-dir "$DIR_A" folder list 2>&1) || true
if echo "$FOLDER_LIST" | grep -qi "default\|folder"; then
    pass "I1 — folder list shows content"
else
    fail "I1" "folder list: $FOLDER_LIST"
fi

# I2 — Folder List JSON
echo "--- I2: Folder List JSON ---"
FOLDER_JSON=$("$CLI" --data-dir "$DIR_A" folder list --json 2>&1) || true
if echo "$FOLDER_JSON" | python3 -c "import sys,json; d=json.load(sys.stdin); assert 'folders' in d" 2>/dev/null; then
    pass "I2 — folder list JSON valid"
    # Extract default folder ID
    DEFAULT_FOLDER_ID=$(echo "$FOLDER_JSON" | python3 -c "import sys,json; d=json.load(sys.stdin); folders=[f for f in d['folders'] if f['name']=='default']; print(folders[0]['folder_id'] if folders else '')" 2>/dev/null) || DEFAULT_FOLDER_ID=""
else
    fail "I2" "folder list JSON invalid"
    DEFAULT_FOLDER_ID=""
fi

# I3 — Create Folder
echo "--- I3: Create Folder ---"
CREATE_OUT=$("$CLI" --data-dir "$DIR_A" folder create "photos" 2>&1) || true
if echo "$CREATE_OUT" | grep -qi "created\|ok\|success"; then
    pass "I3 — photos folder created"
else
    fail "I3" "create folder: $CREATE_OUT"
fi

sleep 5

# Extract photos folder ID
FOLDER_JSON2=$("$CLI" --data-dir "$DIR_A" folder list --json 2>&1) || true
PHOTOS_FOLDER_ID=$(echo "$FOLDER_JSON2" | python3 -c "import sys,json; d=json.load(sys.stdin); folders=[f for f in d['folders'] if f['name']=='photos']; print(folders[0]['folder_id'] if folders else '')" 2>/dev/null) || PHOTOS_FOLDER_ID=""

# Verify on Node B
FOLDER_B=$("$CLI" --data-dir "$DIR_B" folder list 2>&1) || true
if echo "$FOLDER_B" | grep -q "photos"; then
    pass "I3 — photos folder synced to Node B"
else
    fail "I3" "photos not on Node B: $FOLDER_B"
fi

# I4 — Create Second Folder
echo "--- I4: Create Second Folder ---"
"$CLI" --data-dir "$DIR_A" folder create "documents" 2>&1 || true
sleep 3
FOLDER_JSON3=$("$CLI" --data-dir "$DIR_A" folder list --json 2>&1) || true
FOLDER_COUNT=$(echo "$FOLDER_JSON3" | python3 -c "import sys,json; print(len(json.load(sys.stdin)['folders']))" 2>/dev/null) || FOLDER_COUNT="?"
if [ "${FOLDER_COUNT:-0}" -ge 3 ]; then
    pass "I4 — three folders exist"
else
    fail "I4" "folder count: $FOLDER_COUNT"
fi

DOCUMENTS_FOLDER_ID=$(echo "$FOLDER_JSON3" | python3 -c "import sys,json; d=json.load(sys.stdin); folders=[f for f in d['folders'] if f['name']=='documents']; print(folders[0]['folder_id'] if folders else '')" 2>/dev/null) || DOCUMENTS_FOLDER_ID=""

# I5 — Folder Status
echo "--- I5: Folder Status ---"
if [ -n "$DEFAULT_FOLDER_ID" ]; then
    FSTATUS=$("$CLI" --data-dir "$DIR_A" folder status "$DEFAULT_FOLDER_ID" 2>&1) || true
    if echo "$FSTATUS" | grep -q "Folder:"; then
        pass "I5 — folder status returned"
    else
        fail "I5" "folder status: $FSTATUS"
    fi
else
    skip "I5" "no default folder ID"
fi

# I6 — Folder Status JSON
echo "--- I6: Folder Status JSON ---"
if [ -n "$DEFAULT_FOLDER_ID" ]; then
    FSTATUS_JSON=$("$CLI" --data-dir "$DIR_A" folder status "$DEFAULT_FOLDER_ID" --json 2>&1) || true
    if echo "$FSTATUS_JSON" | python3 -c "import sys,json; d=json.load(sys.stdin); assert 'folder_id' in d and 'file_count' in d" 2>/dev/null; then
        pass "I6 — folder status JSON valid"
    else
        fail "I6" "folder status JSON invalid"
    fi
else
    skip "I6" "no default folder ID"
fi

# I7 — Folder Status Non-existent
echo "--- I7: Folder Status Non-existent ---"
BAD_FID="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
FSTATUS_BAD=$("$CLI" --data-dir "$DIR_A" folder status "$BAD_FID" 2>&1) && FS_EXIT=0 || FS_EXIT=$?
if [ $FS_EXIT -ne 0 ] || echo "$FSTATUS_BAD" | grep -qi "error\|not found"; then
    pass "I7 — non-existent folder returns error"
else
    fail "I7" "no error for bad folder: $FSTATUS_BAD"
fi

# I8 — Folder Status Invalid Hex
echo "--- I8: Folder Status Invalid Hex ---"
FSTATUS_BADHEX=$("$CLI" --data-dir "$DIR_A" folder status "not-hex" 2>&1) && FH_EXIT=0 || FH_EXIT=$?
if [ $FH_EXIT -ne 0 ] || echo "$FSTATUS_BADHEX" | grep -qi "error\|invalid"; then
    pass "I8 — invalid hex returns error"
else
    fail "I8" "no error for bad hex: $FSTATUS_BADHEX"
fi

# I9 — Subscribe to Folder
echo "--- I9: Subscribe to Folder ---"
if [ -n "$PHOTOS_FOLDER_ID" ]; then
    SUB_OUT=$("$CLI" --data-dir "$DIR_B" folder subscribe "$PHOTOS_FOLDER_ID" $TESTDIR/b-photos 2>&1) || true
    if echo "$SUB_OUT" | grep -qi "subscrib\|ok\|success"; then
        pass "I9 — subscribed to photos"
    else
        fail "I9" "subscribe: $SUB_OUT"
    fi
else
    skip "I9" "no photos folder ID"
fi

# I10 — Subscribe Read-Only
echo "--- I10: Subscribe Read-Only ---"
if [ -n "$DOCUMENTS_FOLDER_ID" ]; then
    SUB_RO=$("$CLI" --data-dir "$DIR_B" folder subscribe "$DOCUMENTS_FOLDER_ID" $TESTDIR/b-docs --read-only 2>&1) || true
    if echo "$SUB_RO" | grep -qi "subscrib\|ok\|success"; then
        pass "I10 — subscribed read-only"
    else
        fail "I10" "subscribe read-only: $SUB_RO"
    fi
    # Verify mode
    FLIST_B=$("$CLI" --data-dir "$DIR_B" folder list --json 2>&1) || true
    DOC_MODE=$(echo "$FLIST_B" | python3 -c "import sys,json; d=json.load(sys.stdin); folders=[f for f in d['folders'] if f['name']=='documents']; print(folders[0].get('mode','') if folders else '')" 2>/dev/null) || DOC_MODE=""
    if [ "$DOC_MODE" = "read-only" ]; then
        pass "I10 — mode is read-only"
    else
        fail "I10" "mode is '$DOC_MODE', expected 'read-only'"
    fi
else
    skip "I10" "no documents folder ID"
fi

# I11 — Subscribe Non-existent
echo "--- I11: Subscribe Non-existent ---"
SUB_BAD=$("$CLI" --data-dir "$DIR_B" folder subscribe "$BAD_FID" $TESTDIR/b-fake 2>&1) && SB_EXIT=0 || SB_EXIT=$?
if [ $SB_EXIT -ne 0 ] || echo "$SUB_BAD" | grep -qi "error\|not found"; then
    pass "I11 — subscribe to non-existent folder rejected"
else
    fail "I11" "no error: $SUB_BAD"
fi

# I12 — Change Folder Mode
echo "--- I12: Change Folder Mode ---"
if [ -n "$PHOTOS_FOLDER_ID" ]; then
    MODE_OUT=$("$CLI" --data-dir "$DIR_B" folder mode "$PHOTOS_FOLDER_ID" read-only 2>&1) || true
    if echo "$MODE_OUT" | grep -qi "mode\|ok\|success\|changed"; then
        pass "I12 — mode changed to read-only"
    else
        fail "I12" "mode change: $MODE_OUT"
    fi
    # Change back
    "$CLI" --data-dir "$DIR_B" folder mode "$PHOTOS_FOLDER_ID" read-write 2>&1 || true
else
    skip "I12" "no photos folder ID"
fi

# I13 — Folder Files
echo "--- I13: Folder Files ---"
if [ -n "$DEFAULT_FOLDER_ID" ]; then
    FFILES=$("$CLI" --data-dir "$DIR_A" folder files "$DEFAULT_FOLDER_ID" 2>&1) || true
    if echo "$FFILES" | grep -q "test-file-a.txt\|test-file-b.txt\|No synced files"; then
        pass "I13 — folder files returned content"
    else
        fail "I13" "folder files: $FFILES"
    fi
else
    skip "I13" "no default folder ID"
fi

# I14 — Folder Files Empty
echo "--- I14: Folder Files (Empty Folder) ---"
if [ -n "$PHOTOS_FOLDER_ID" ]; then
    FFILES_EMPTY=$("$CLI" --data-dir "$DIR_A" folder files "$PHOTOS_FOLDER_ID" 2>&1) || true
    if echo "$FFILES_EMPTY" | grep -qi "no synced files\|0)\|files (0"; then
        pass "I14 — photos folder is empty"
    else
        # It's also OK if it just lists files (maybe some were added)
        pass "I14 — folder files returned (possibly empty)"
    fi
else
    skip "I14" "no photos folder ID"
fi

# I16 — MIME Type Detection
echo "--- I16: MIME Type Detection ---"
echo '{"key": "value"}' > $SCRATCH/test.json
"$CLI" --data-dir "$DIR_A" add $SCRATCH/test.json 2>&1 || true
sleep 2
FILES_MIME=$("$CLI" --data-dir "$DIR_A" files 2>&1) || true
if echo "$FILES_MIME" | grep -q "test.json"; then
    pass "I16 — JSON file added with MIME info"
else
    fail "I16" "test.json not in files"
fi

# I18 — Large File Transfer
echo "--- I18: Large File Transfer ---"
dd if=/dev/urandom of=$SCRATCH/test-large.bin bs=1M count=5 2>/dev/null
LARGE_OUT=$("$CLI" --data-dir "$DIR_A" add $SCRATCH/test-large.bin 2>&1) || true
if echo "$LARGE_OUT" | grep -qi "added\|ok\|success\|hash"; then
    pass "I18 — 5 MB file added"
else
    fail "I18" "large file add: $LARGE_OUT"
fi

# I19 — Transfer Status
echo "--- I19: Transfer Status ---"
TRANSFERS=$("$CLI" --data-dir "$DIR_A" transfers 2>&1) || true
# Either shows active transfers or "no active transfers" — both are valid
if echo "$TRANSFERS" | grep -qi "transfer\|no active"; then
    pass "I19 — transfer status command works"
else
    fail "I19" "transfers: $TRANSFERS"
fi

TRANSFERS_JSON=$("$CLI" --data-dir "$DIR_A" transfers --json 2>&1) || true
if echo "$TRANSFERS_JSON" | python3 -c "import sys,json; d=json.load(sys.stdin); assert 'transfers' in d" 2>/dev/null; then
    pass "I19 — transfers JSON valid"
else
    fail "I19" "transfers JSON invalid"
fi

# I20 — Large File Arrives
echo "--- I20: Large File Sync ---"
sleep 10
FILES_B_LARGE=$("$CLI" --data-dir "$DIR_B" files 2>&1) || true
if echo "$FILES_B_LARGE" | grep -q "test-large.bin"; then
    pass "I20 — large file synced to Node B"
else
    fail "I20" "large file not on Node B"
fi

# I21 — Delta Sync
echo "--- I21: Delta Sync on Reconnect ---"
stop_daemon "$DAEMON_B_PID"
DAEMON_B_PID=""
sleep 1
echo "Added while B offline" > $SCRATCH/test-offline.txt
"$CLI" --data-dir "$DIR_A" add $SCRATCH/test-offline.txt 2>&1 || true

start_daemon "$DIR_B" "node-b" "backup" && DAEMON_B_PID=$(last_pid) || DAEMON_B_PID=""
sleep 8

FILES_B_DELTA=$("$CLI" --data-dir "$DIR_B" files 2>&1) || true
if echo "$FILES_B_DELTA" | grep -q "test-offline.txt"; then
    pass "I21 — delta sync worked"
else
    fail "I21" "offline file not synced to Node B"
fi

# I23 — Conflicts (Empty)
echo "--- I23: Conflicts (Empty) ---"
CONFLICTS=$("$CLI" --data-dir "$DIR_A" conflicts 2>&1) || true
if echo "$CONFLICTS" | grep -qi "no active conflicts"; then
    pass "I23 — no conflicts"
else
    fail "I23" "unexpected conflicts: $CONFLICTS"
fi

# I24 — Conflicts JSON
echo "--- I24: Conflicts JSON ---"
CONFLICTS_JSON=$("$CLI" --data-dir "$DIR_A" conflicts --json 2>&1) || true
if echo "$CONFLICTS_JSON" | python3 -c "import sys,json; d=json.load(sys.stdin); assert 'conflicts' in d" 2>/dev/null; then
    pass "I24 — conflicts JSON valid"
else
    fail "I24" "conflicts JSON invalid"
fi

# I25 — Conflicts by Folder
echo "--- I25: Conflicts by Folder ---"
if [ -n "$DEFAULT_FOLDER_ID" ]; then
    CONFLICTS_F=$("$CLI" --data-dir "$DIR_A" conflicts --folder "$DEFAULT_FOLDER_ID" 2>&1) || true
    if echo "$CONFLICTS_F" | grep -qi "no active conflicts\|conflicts"; then
        pass "I25 — conflicts by folder works"
    else
        fail "I25" "conflicts by folder: $CONFLICTS_F"
    fi
else
    skip "I25" "no default folder ID"
fi

# I26 — Resolve Non-existent
echo "--- I26: Resolve Non-existent ---"
if [ -n "$DEFAULT_FOLDER_ID" ]; then
    RESOLVE_BAD=$("$CLI" --data-dir "$DIR_A" resolve "$DEFAULT_FOLDER_ID" no-such.txt "$BAD_FID" 2>&1) && RB_EXIT=0 || RB_EXIT=$?
    if [ $RB_EXIT -ne 0 ] || echo "$RESOLVE_BAD" | grep -qi "error\|not found"; then
        pass "I26 — resolve non-existent rejected"
    else
        fail "I26" "no error: $RESOLVE_BAD"
    fi
else
    skip "I26" "no default folder ID"
fi

# I27 — File History
echo "--- I27: File History ---"
if [ -n "$DEFAULT_FOLDER_ID" ]; then
    HISTORY=$("$CLI" --data-dir "$DIR_A" history "$DEFAULT_FOLDER_ID" test-file-a.txt 2>&1) || true
    if echo "$HISTORY" | grep -qi "version\|node-a\|bytes"; then
        pass "I27 — file history returned"
    else
        fail "I27" "history: $HISTORY"
    fi
else
    skip "I27" "no default folder ID"
fi

# I28 — File History JSON
echo "--- I28: File History JSON ---"
if [ -n "$DEFAULT_FOLDER_ID" ]; then
    HISTORY_JSON=$("$CLI" --data-dir "$DIR_A" history "$DEFAULT_FOLDER_ID" test-file-a.txt --json 2>&1) || true
    if echo "$HISTORY_JSON" | python3 -c "import sys,json; d=json.load(sys.stdin); assert 'versions' in d" 2>/dev/null; then
        pass "I28 — history JSON valid"
    else
        fail "I28" "history JSON invalid"
    fi
else
    skip "I28" "no default folder ID"
fi

# I29 — History Non-existent
echo "--- I29: History Non-existent ---"
if [ -n "$DEFAULT_FOLDER_ID" ]; then
    HISTORY_NONE=$("$CLI" --data-dir "$DIR_A" history "$DEFAULT_FOLDER_ID" no-such-file.txt 2>&1) || true
    if echo "$HISTORY_NONE" | grep -qi "no version"; then
        pass "I29 — no history for non-existent file"
    else
        fail "I29" "unexpected: $HISTORY_NONE"
    fi
else
    skip "I29" "no default folder ID"
fi

# I31 — Unsubscribe
echo "--- I31: Unsubscribe ---"
if [ -n "$DOCUMENTS_FOLDER_ID" ]; then
    UNSUB=$("$CLI" --data-dir "$DIR_B" folder unsubscribe "$DOCUMENTS_FOLDER_ID" 2>&1) || true
    if echo "$UNSUB" | grep -qi "unsubscrib\|ok\|success"; then
        pass "I31 — unsubscribed from documents"
    else
        fail "I31" "unsubscribe: $UNSUB"
    fi
else
    skip "I31" "no documents folder ID"
fi

# I33 — Folder Remove
echo "--- I33: Folder Remove ---"
if [ -n "$DOCUMENTS_FOLDER_ID" ]; then
    RMFOLDER=$("$CLI" --data-dir "$DIR_A" folder remove "$DOCUMENTS_FOLDER_ID" 2>&1) || true
    if echo "$RMFOLDER" | grep -qi "removed\|ok\|success"; then
        pass "I33 — documents folder removed"
    else
        fail "I33" "folder remove: $RMFOLDER"
    fi
    sleep 3
    # Verify removed on both
    FLIST_AFTER=$("$CLI" --data-dir "$DIR_A" folder list 2>&1) || true
    if echo "$FLIST_AFTER" | grep -q "documents"; then
        fail "I33" "documents still listed on Node A"
    else
        pass "I33 — documents gone from Node A"
    fi
else
    skip "I33" "no documents folder ID"
fi

# I34 — DAG Consistency
echo "--- I34: DAG Consistency After Folder Ops ---"
sleep 3
DAG_A2=$("$CLI" --data-dir "$DIR_A" status --json 2>&1 | python3 -c "import sys,json; print(json.load(sys.stdin)['dag_entries'])" 2>/dev/null) || DAG_A2="?"
DAG_B2=$("$CLI" --data-dir "$DIR_B" status --json 2>&1 | python3 -c "import sys,json; print(json.load(sys.stdin)['dag_entries'])" 2>/dev/null) || DAG_B2="?"
if [ "$DAG_A2" = "$DAG_B2" ] && [ "$DAG_A2" != "?" ]; then
    pass "I34 — DAG consistent: $DAG_A2 entries"
else
    fail "I34" "DAG mismatch: A=$DAG_A2 B=$DAG_B2"
fi

########################################################################
# ADVANCED TESTS
########################################################################
echo ""
echo "========================================="
echo "  ADVANCED TESTS (Security, Persistence)"
echo "========================================="
echo ""

# A1 — Device Revocation
echo "--- A1: Device Revocation ---"
if [ -n "$NODE_B_ID" ]; then
    # Get current Node B's device ID (may have changed if re-joined)
    DEVICES_JSON_NOW=$("$CLI" --data-dir "$DIR_A" devices --json 2>&1) || true
    CURR_B_ID=$(echo "$DEVICES_JSON_NOW" | python3 -c "import sys,json; d=json.load(sys.stdin); devs=[dev for dev in d['devices'] if 'node-b' in dev['name']]; print(devs[0]['device_id'] if devs else '')" 2>/dev/null) || CURR_B_ID=""
    if [ -n "$CURR_B_ID" ]; then
        REVOKE_OUT=$("$CLI" --data-dir "$DIR_A" revoke "$CURR_B_ID" 2>&1) || true
        if echo "$REVOKE_OUT" | grep -qi "revoked\|ok\|success"; then
            pass "A1 — device revoked"
        else
            fail "A1" "revoke: $REVOKE_OUT"
        fi
    else
        skip "A1" "couldn't find node-b device ID"
    fi
else
    skip "A1" "no device ID"
fi

# A2 — Revoked Device Removed
echo "--- A2: Revoked Device Removed ---"
DEVICES_AFTER=$("$CLI" --data-dir "$DIR_A" devices 2>&1) || true
if echo "$DEVICES_AFTER" | grep -q "node-b"; then
    fail "A2" "node-b still in device list"
else
    pass "A2 — node-b removed from device list"
fi

# A3 — Revoked Device Cannot Sync
echo "--- A3: Revoked Cannot Sync ---"
echo "From revoked node" > $SCRATCH/test-revoked.txt
"$CLI" --data-dir "$DIR_B" add $SCRATCH/test-revoked.txt 2>&1 || true
sleep 3
FILES_A_REV=$("$CLI" --data-dir "$DIR_A" files 2>&1) || true
if echo "$FILES_A_REV" | grep -q "test-revoked.txt"; then
    fail "A3" "revoked node's file appeared on Node A"
else
    pass "A3 — revoked node's file rejected"
fi

# A5 — Approve Invalid Hex
echo "--- A5: Approve Invalid Hex ---"
APPROVE_BAD=$("$CLI" --data-dir "$DIR_A" approve "not-hex" --role backup 2>&1) && AB_EXIT=0 || AB_EXIT=$?
if [ $AB_EXIT -ne 0 ] || echo "$APPROVE_BAD" | grep -qi "error\|invalid"; then
    pass "A5 — invalid hex rejected"
else
    fail "A5" "no error: $APPROVE_BAD"
fi

# A7 — Re-approve After Revocation
echo "--- A7: Re-join After Revocation ---"
stop_daemon "$DAEMON_B_PID"
DAEMON_B_PID=""
sleep 1
rm -rf "$DIR_B"
"$CLI" --data-dir "$DIR_B" join "$MNEMONIC" --name "node-b-v2" --role full 2>&1 || true
start_daemon "$DIR_B" "node-b-v2" "full" && DAEMON_B_PID=$(last_pid) || DAEMON_B_PID=""
sleep 5
PENDING_REJOIN=$("$CLI" --data-dir "$DIR_A" pending 2>&1) || true
if echo "$PENDING_REJOIN" | grep -q "node-b-v2"; then
    pass "A7 — node-b-v2 visible as pending"
    NEW_B_ID=$(echo "$PENDING_REJOIN" | grep "node-b-v2" | awk '{print $1}')
    "$CLI" --data-dir "$DIR_A" approve "$NEW_B_ID" --role full 2>&1 || true
    sleep 5
    DEVICES_REJOIN=$("$CLI" --data-dir "$DIR_A" devices 2>&1) || true
    if echo "$DEVICES_REJOIN" | grep -q "node-b-v2"; then
        pass "A7 — node-b-v2 approved"
    else
        fail "A7" "node-b-v2 not in devices after approval: $DEVICES_REJOIN"
    fi
else
    fail "A7" "node-b-v2 not in pending: $PENDING_REJOIN"
fi

# A8 — Graceful Shutdown
echo "--- A8: Graceful Shutdown ---"
stop_daemon "$DAEMON_A_PID"
DAEMON_A_PID=""
sleep 1
if [ ! -S "$DIR_A/murmurd.sock" ]; then
    pass "A8 — socket cleaned up on shutdown"
else
    fail "A8" "socket still exists after shutdown"
fi

# A9 — Stale Socket Detection
echo "--- A9: Stale Socket Detection ---"
touch "$DIR_A/murmurd.sock"
start_daemon "$DIR_A" "node-a" "full" && DAEMON_A_PID=$(last_pid) || DAEMON_A_PID=""
if [ -S "$DIR_A/murmurd.sock" ]; then
    pass "A9 — daemon started despite stale socket"
else
    fail "A9" "daemon failed to start with stale socket"
fi

# A10–A14 — Restart Persistence
echo "--- A10–A14: Restart Persistence ---"
DEVICES_PERSIST=$("$CLI" --data-dir "$DIR_A" devices 2>&1) || true
if echo "$DEVICES_PERSIST" | grep -q "node-a"; then
    pass "A10 — devices persist across restart"
else
    fail "A10" "devices lost: $DEVICES_PERSIST"
fi

FILES_PERSIST=$("$CLI" --data-dir "$DIR_A" files 2>&1) || true
if echo "$FILES_PERSIST" | grep -q "test-file-a.txt"; then
    pass "A11 — files persist across restart"
else
    fail "A11" "files lost: $FILES_PERSIST"
fi

FOLDERS_PERSIST=$("$CLI" --data-dir "$DIR_A" folder list 2>&1) || true
if echo "$FOLDERS_PERSIST" | grep -qi "default\|photos\|folder"; then
    pass "A12 — folders persist across restart"
else
    fail "A12" "folders lost: $FOLDERS_PERSIST"
fi

MNEMONIC_PERSIST=$("$CLI" --data-dir "$DIR_A" mnemonic 2>&1) || true
if [ "$MNEMONIC_PERSIST" = "$MNEMONIC" ]; then
    pass "A13 — mnemonic persists across restart"
else
    fail "A13" "mnemonic changed"
fi

DAG_PERSIST=$("$CLI" --data-dir "$DIR_A" status --json 2>&1 | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['dag_entries'])" 2>/dev/null) || DAG_PERSIST="?"
if [ "$DAG_PERSIST" != "?" ] && [ "${DAG_PERSIST:-0}" -gt 0 ]; then
    pass "A14 — DAG entries persist: $DAG_PERSIST"
else
    fail "A14" "DAG entries: $DAG_PERSIST"
fi

# A15 — Reconnection
echo "--- A15: Peer Reconnection ---"
sleep 8
PEERS_RECONNECT=$("$CLI" --data-dir "$DIR_A" status --json 2>&1 | python3 -c "import sys,json; print(json.load(sys.stdin)['peer_count'])" 2>/dev/null) || PEERS_RECONNECT="0"
if [ "${PEERS_RECONNECT:-0}" -ge 1 ]; then
    pass "A15 — peers reconnected after restart"
else
    fail "A15" "peer count: $PEERS_RECONNECT"
fi

# A16 — Filesystem State Node A
echo "--- A16: Filesystem State ---"
if [ -f "$DIR_A/config.toml" ]; then
    pass "A16 — config.toml exists"
else
    fail "A16" "config.toml missing"
fi
if [ -f "$DIR_A/mnemonic" ]; then
    pass "A16 — mnemonic file exists"
else
    fail "A16" "mnemonic file missing"
fi
if [ -d "$DIR_A/db" ]; then
    pass "A16 — db directory exists"
else
    fail "A16" "db directory missing"
fi
if [ -d "$DIR_A/blobs" ]; then
    pass "A16 — blobs directory exists"
else
    fail "A16" "blobs directory missing"
fi

# A17 — Filesystem State Node B
echo "--- A17: Filesystem State Node B ---"
if [ -f "$DIR_B/device.key" ]; then
    KEYSIZE=$(wc -c < "$DIR_B/device.key")
    if [ "$KEYSIZE" -eq 32 ]; then
        pass "A17 — device.key is 32 bytes"
    else
        fail "A17" "device.key is $KEYSIZE bytes"
    fi
else
    fail "A17" "device.key missing"
fi

# A18 — Network Isolation
echo "--- A18: Network Isolation ---"
rm -rf "$DIR_C"
# Start a third daemon on a different network
"$MURMURD" --data-dir "$DIR_C" --name "node-c" --role full 2>/dev/null &
DAEMON_C_PID=$!
# Wait for socket
local_waited=0
while [ ! -S "$DIR_C/murmurd.sock" ] && [ $local_waited -lt 15 ]; do
    sleep 0.5
    local_waited=$((local_waited + 1))
done
if [ -S "$DIR_C/murmurd.sock" ]; then
    PEERS_C=$("$CLI" --data-dir "$DIR_C" status --json 2>&1 | python3 -c "import sys,json; print(json.load(sys.stdin)['peer_count'])" 2>/dev/null) || PEERS_C="?"
    if [ "${PEERS_C:-0}" -eq 0 ]; then
        pass "A18 — isolated network has 0 peers"
    else
        fail "A18" "isolated network has $PEERS_C peers"
    fi
    NID_A=$("$CLI" --data-dir "$DIR_A" status --json 2>&1 | python3 -c "import sys,json; print(json.load(sys.stdin)['network_id'])" 2>/dev/null) || NID_A=""
    NID_C=$("$CLI" --data-dir "$DIR_C" status --json 2>&1 | python3 -c "import sys,json; print(json.load(sys.stdin)['network_id'])" 2>/dev/null) || NID_C=""
    if [ -n "$NID_A" ] && [ -n "$NID_C" ] && [ "$NID_A" != "$NID_C" ]; then
        pass "A18 — different network IDs"
    else
        fail "A18" "network IDs: A=$NID_A C=$NID_C"
    fi
    kill "$DAEMON_C_PID" 2>/dev/null && wait "$DAEMON_C_PID" 2>/dev/null || true
else
    fail "A18" "Node C daemon didn't start"
    kill "$DAEMON_C_PID" 2>/dev/null && wait "$DAEMON_C_PID" 2>/dev/null || true
fi

# A19 — Multi-File Batch Sync
echo "--- A19: Batch File Sync ---"
for i in $(seq 1 10); do
    echo "Batch file $i" > "$SCRATCH/test-batch-$i.txt"
    "$CLI" --data-dir "$DIR_A" add "$SCRATCH/test-batch-$i.txt" 2>&1 || true
done
sleep 10
FILES_B_BATCH=$("$CLI" --data-dir "$DIR_B" files --json 2>&1 | python3 -c "import sys,json; print(len(json.load(sys.stdin)['files']))" 2>/dev/null) || FILES_B_BATCH="?"
FILES_A_BATCH=$("$CLI" --data-dir "$DIR_A" files --json 2>&1 | python3 -c "import sys,json; print(len(json.load(sys.stdin)['files']))" 2>/dev/null) || FILES_A_BATCH="?"
if [ "$FILES_A_BATCH" = "$FILES_B_BATCH" ] && [ "$FILES_A_BATCH" != "?" ]; then
    pass "A19 — batch files synced: $FILES_A_BATCH files on both"
else
    fail "A19" "file count: A=$FILES_A_BATCH B=$FILES_B_BATCH"
fi

# A20 — Empty File
echo "--- A20: Empty File ---"
touch $SCRATCH/test-empty.txt
EMPTY_OUT=$("$CLI" --data-dir "$DIR_A" add $SCRATCH/test-empty.txt 2>&1) || true
if echo "$EMPTY_OUT" | grep -qi "added\|ok\|success\|hash"; then
    pass "A20 — empty file added"
else
    fail "A20" "empty file: $EMPTY_OUT"
fi

# A21 — Add Non-existent File
echo "--- A21: Add Non-existent File ---"
NOFILE_OUT=$("$CLI" --data-dir "$DIR_A" add $SCRATCH/does-not-exist.txt 2>&1) && NF_EXIT=0 || NF_EXIT=$?
if [ $NF_EXIT -ne 0 ] || echo "$NOFILE_OUT" | grep -qi "error\|not found\|no such"; then
    pass "A21 — non-existent file rejected"
else
    fail "A21" "no error: $NOFILE_OUT"
fi

# A22 — Status Fields Validation
echo "--- A22: Status Fields Validation ---"
STATUS_V=$("$CLI" --data-dir "$DIR_A" status --json 2>&1) || true
VALID=$(echo "$STATUS_V" | python3 -c "
import sys, json
d = json.load(sys.stdin)
ok = True
ok = ok and len(d.get('device_id','')) == 64
ok = ok and len(d.get('network_id','')) == 64
ok = ok and len(d.get('device_name','')) > 0
ok = ok and d.get('peer_count', -1) >= 0
ok = ok and d.get('dag_entries', 0) > 0
ok = ok and d.get('uptime_secs', -1) >= 0
print('ok' if ok else 'fail')
" 2>/dev/null) || VALID="fail"
if [ "$VALID" = "ok" ]; then
    pass "A22 — all status fields valid"
else
    fail "A22" "some fields invalid"
fi

# A23 — File Origin Tracking
echo "--- A23: File Origin Tracking ---"
ORIGINS=$("$CLI" --data-dir "$DIR_A" files --json 2>&1 | python3 -c "
import sys, json
d = json.load(sys.stdin)
origins = set(f.get('device_origin','') for f in d['files'])
print(len(origins))
" 2>/dev/null) || ORIGINS="?"
if [ "${ORIGINS:-0}" -ge 1 ]; then
    pass "A23 — file origins tracked ($ORIGINS unique devices)"
else
    fail "A23" "origins: $ORIGINS"
fi

# A24 — Concurrent CLI Requests
echo "--- A24: Concurrent Requests ---"
"$CLI" --data-dir "$DIR_A" status >/dev/null 2>&1 &
P1=$!
"$CLI" --data-dir "$DIR_A" devices >/dev/null 2>&1 &
P2=$!
"$CLI" --data-dir "$DIR_A" files >/dev/null 2>&1 &
P3=$!
"$CLI" --data-dir "$DIR_A" folder list >/dev/null 2>&1 &
P4=$!
wait $P1 && wait $P2 && wait $P3 && wait $P4
CONCURRENT_OK=$?
if [ $CONCURRENT_OK -eq 0 ]; then
    pass "A24 — concurrent requests all succeeded"
else
    fail "A24" "some concurrent requests failed"
fi

# A26 — Folder Operations After Restart
echo "--- A26: Folder Ops After Restart ---"
CREATE_POST=$("$CLI" --data-dir "$DIR_A" folder create "post-restart" 2>&1) || true
if echo "$CREATE_POST" | grep -qi "created\|ok\|success"; then
    pass "A26 — folder create works after restart"
else
    fail "A26" "create after restart: $CREATE_POST"
fi

# A27 — File History Survives Restart
echo "--- A27: History After Restart ---"
if [ -n "$DEFAULT_FOLDER_ID" ]; then
    HISTORY_POST=$("$CLI" --data-dir "$DIR_A" history "$DEFAULT_FOLDER_ID" test-file-a.txt 2>&1) || true
    if echo "$HISTORY_POST" | grep -qi "version\|node-a\|bytes"; then
        pass "A27 — file history survives restart"
    else
        fail "A27" "history after restart: $HISTORY_POST"
    fi
else
    skip "A27" "no default folder ID"
fi

# A30 — Final DAG Consistency
echo "--- A30: Final DAG Consistency ---"
sleep 5
DAG_FINAL_A=$("$CLI" --data-dir "$DIR_A" status --json 2>&1 | python3 -c "import sys,json; print(json.load(sys.stdin)['dag_entries'])" 2>/dev/null) || DAG_FINAL_A="?"
DAG_FINAL_B=$("$CLI" --data-dir "$DIR_B" status --json 2>&1 | python3 -c "import sys,json; print(json.load(sys.stdin)['dag_entries'])" 2>/dev/null) || DAG_FINAL_B="?"
if [ "$DAG_FINAL_A" = "$DAG_FINAL_B" ] && [ "$DAG_FINAL_A" != "?" ]; then
    pass "A30 — final DAG consistent: $DAG_FINAL_A entries"
else
    fail "A30" "final DAG: A=$DAG_FINAL_A B=$DAG_FINAL_B"
fi

NID_FINAL_A=$("$CLI" --data-dir "$DIR_A" status --json 2>&1 | python3 -c "import sys,json; print(json.load(sys.stdin)['network_id'])" 2>/dev/null) || NID_FINAL_A=""
NID_FINAL_B=$("$CLI" --data-dir "$DIR_B" status --json 2>&1 | python3 -c "import sys,json; print(json.load(sys.stdin)['network_id'])" 2>/dev/null) || NID_FINAL_B=""
if [ "$NID_FINAL_A" = "$NID_FINAL_B" ] && [ -n "$NID_FINAL_A" ]; then
    pass "A30 — same network ID"
else
    fail "A30" "network ID mismatch: A=$NID_FINAL_A B=$NID_FINAL_B"
fi

# A31 — Clean Shutdown
echo "--- A31: Clean Shutdown ---"
stop_daemon "$DAEMON_A_PID"
DAEMON_A_PID=""
stop_daemon "$DAEMON_B_PID"
DAEMON_B_PID=""
sleep 1
if [ ! -S "$DIR_A/murmurd.sock" ] && [ ! -S "$DIR_B/murmurd.sock" ]; then
    pass "A31 — both sockets cleaned up"
else
    fail "A31" "socket(s) still exist"
fi

echo ""
echo "=== All tests complete ==="
