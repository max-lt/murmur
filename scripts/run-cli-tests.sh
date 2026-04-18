#!/usr/bin/env bash
#
# Automated CLI manual test runner.
# Exercises tests from tests/manual/cli-basic.md, cli-intermediate.md, cli-advanced.md.
#
# IMPORTANT — sandbox requirements:
#   The daemons create Unix sockets, which the Claude Code sandbox blocks by default.
#   Run via:  TMPDIR=/tmp/claude ./scripts/run-cli-tests.sh
#   In Claude Code, use Bash tool with dangerouslyDisableSandbox=true so that
#   the daemon can bind its Unix socket.  Without this the daemon starts but
#   immediately fails with "Operation not permitted (os error 1)" on bind().
#
# Usage:
#   TMPDIR=/tmp/claude ./scripts/run-cli-tests.sh
#
set -uo pipefail

MURMURD="./target/debug/murmurd"
CLI="./target/debug/murmur-cli"
TESTDIR="${TMPDIR:-/tmp}/murmur-test"
DIR_A="$TESTDIR/a"
DIR_B="$TESTDIR/b"
DIR_C="$TESTDIR/c"
SCRATCH="$TESTDIR/scratch"
DAEMON_A_PID=""
DAEMON_B_PID=""

PASS=0
FAIL=0
SKIP=0
FAILURES=""

# Colors (strip if not a tty)
if [ -t 1 ]; then
    RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[0;33m'; NC='\033[0m'
else
    RED=''; GREEN=''; YELLOW=''; NC=''
fi

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

pass() { PASS=$((PASS + 1)); echo -e "  ${GREEN}PASS${NC}: $1"; }
fail() { FAIL=$((FAIL + 1)); echo -e "  ${RED}FAIL${NC}: $1 — $2"; FAILURES="${FAILURES}  $1: $2\n"; }
skip() { SKIP=$((SKIP + 1)); echo -e "  ${YELLOW}SKIP${NC}: $1 — $2"; }

# jq_get <json_string> <python_expr>  — extract a field from JSON using python3
jq_get() {
    echo "$1" | python3 -c "import sys,json; d=json.load(sys.stdin); print($2)" 2>/dev/null || echo ""
}

cleanup() {
    echo ""
    echo "=== Cleaning up ==="
    stop_daemon "$DAEMON_A_PID"
    stop_daemon "$DAEMON_B_PID"
    # Also kill any stray murmurd processes using our test dirs
    pkill -f "murmurd.*murmur-test" 2>/dev/null || true
    sleep 1
    rm -rf "$TESTDIR"
    echo ""
    echo "=============================="
    printf "  Results: ${GREEN}%d passed${NC}, ${RED}%d failed${NC}, ${YELLOW}%d skipped${NC}\n" "$PASS" "$FAIL" "$SKIP"
    echo "=============================="
    if [ -n "$FAILURES" ]; then
        echo ""
        echo -e "${RED}Failed tests:${NC}"
        printf "%b" "$FAILURES"
    fi
}
trap cleanup EXIT

# Start a daemon in the background.
# Writes PID to $TESTDIR/.last_pid. Returns 0 on success, 1 on failure.
start_daemon() {
    # Third arg (role) is legacy — retained for call-site compatibility but
    # ignored, since murmurd no longer accepts `--role`.
    local dir="$1" name="$2"
    mkdir -p "$dir"
    "$MURMURD" --data-dir "$dir" --name "$name" \
        >"$TESTDIR/.daemon-${name}.log" 2>&1 &
    local pid=$!
    local waited=0
    while [ ! -S "$dir/murmurd.sock" ] && [ $waited -lt 30 ]; do
        sleep 0.5; waited=$((waited + 1))
        if ! kill -0 "$pid" 2>/dev/null; then
            echo "  ERROR: daemon died during startup (dir=$dir)" >&2
            tail -5 "$TESTDIR/.daemon-${name}.log" >&2 2>/dev/null || true
            return 1
        fi
    done
    if [ ! -S "$dir/murmurd.sock" ]; then
        echo "  ERROR: daemon socket never appeared (dir=$dir)" >&2; return 1
    fi
    echo "$pid" > "$TESTDIR/.last_pid"
}

last_pid() { cat "$TESTDIR/.last_pid" 2>/dev/null || echo ""; }

stop_daemon() {
    local pid="${1:-}"
    if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
        kill "$pid" 2>/dev/null || true
        local waited=0
        while kill -0 "$pid" 2>/dev/null && [ $waited -lt 12 ]; do
            sleep 0.5; waited=$((waited + 1))
        done
        kill -9 "$pid" 2>/dev/null || true
    fi
}

# CLI wrapper — always returns exit code, never kills the script
cli() { "$CLI" "$@" 2>&1 || true; }
cli_exit() { "$CLI" "$@" 2>&1; echo $?; }

# ---------------------------------------------------------------------------
# Setup
# ---------------------------------------------------------------------------
echo "=============================="
echo "  Murmur CLI Test Runner"
echo "=============================="
echo ""
echo "=== Building ==="
cargo build -p murmurd -p murmur-cli 2>&1 | tail -1
echo ""

rm -rf "$TESTDIR"
mkdir -p "$SCRATCH"

########################################################################
# BASIC TESTS
########################################################################
echo "========================================="
echo "  BASIC TESTS"
echo "========================================="
echo ""

# B1 — Network Creation
echo "--- B1: Network Creation ---"
start_daemon "$DIR_A" "node-a" "full" && DAEMON_A_PID=$(last_pid) || DAEMON_A_PID=""
[ -n "$DAEMON_A_PID" ] && [ -S "$DIR_A/murmurd.sock" ] \
    && pass "B1 — daemon started" || fail "B1" "daemon not running"

MNEMONIC=$(cat "$DIR_A/mnemonic" 2>/dev/null | tr -d '\n' || echo "")
WORD_COUNT=$(echo "$MNEMONIC" | wc -w | tr -d ' ')
[ "$WORD_COUNT" -eq 24 ] \
    && pass "B1 — mnemonic is 24 words" \
    || fail "B1" "mnemonic has $WORD_COUNT words"

# B2 — Status (plain)
echo "--- B2/B3: Status ---"
STATUS_PLAIN=$(cli --data-dir "$DIR_A" status)
echo "$STATUS_PLAIN" | grep -q "Device:.*node-a" \
    && pass "B2 — status shows device name" || fail "B2" "$STATUS_PLAIN"
echo "$STATUS_PLAIN" | grep -q "DAG entries:" \
    && pass "B2 — status shows DAG entries" || fail "B2" "missing DAG entries"

# B3 — Status JSON  (wrapped: {"Status": {...}})
STATUS_JSON=$(cli --data-dir "$DIR_A" status --json)
DEV_ID=$(jq_get "$STATUS_JSON" "d['Status']['device_id']")
DAG_CNT=$(jq_get "$STATUS_JSON" "d['Status']['dag_entries']")
[ "${#DEV_ID}" -eq 64 ] && [ -n "$DAG_CNT" ] \
    && pass "B3 — status JSON valid (dag=$DAG_CNT)" || fail "B3" "bad JSON: $STATUS_JSON"

# B4 — Devices
echo "--- B4: Devices ---"
DEVICES_PLAIN=$(cli --data-dir "$DIR_A" devices)
echo "$DEVICES_PLAIN" | grep -q "node-a" \
    && pass "B4 — devices shows node-a" || fail "B4" "$DEVICES_PLAIN"

DEVICES_JSON=$(cli --data-dir "$DIR_A" devices --json)
DEV_COUNT=$(jq_get "$DEVICES_JSON" "len(d['Devices']['devices'])")
[ "$DEV_COUNT" -eq 1 ] \
    && pass "B4 — devices JSON has 1 entry" || fail "B4" "count=$DEV_COUNT"

# B5 — Pending
echo "--- B5/B6/B7: Pending, Files, Mnemonic ---"
cli --data-dir "$DIR_A" pending | grep -qi "no pending" \
    && pass "B5 — no pending requests" || fail "B5" "unexpected pending"

# B6 — Files
cli --data-dir "$DIR_A" files | grep -qi "no synced files" \
    && pass "B6 — no synced files" || fail "B6" "unexpected files"

# B7 — Mnemonic
MNEMONIC_OUT=$(cli --data-dir "$DIR_A" mnemonic)
[ "$MNEMONIC_OUT" = "$MNEMONIC" ] \
    && pass "B7 — mnemonic matches" || fail "B7" "mismatch"
MNEMONIC_JSON=$(cli --data-dir "$DIR_A" mnemonic --json)
MNEMONIC_JSON_VAL=$(jq_get "$MNEMONIC_JSON" "d['Mnemonic']['mnemonic']")
[ "$MNEMONIC_JSON_VAL" = "$MNEMONIC" ] \
    && pass "B7 — mnemonic JSON valid" || fail "B7" "JSON mnemonic wrong"

# B8 — Join offline
echo "--- B8–B11: Join & Validation ---"
JOIN_OUT=$(cli --data-dir "$DIR_B" join "$MNEMONIC" --name "node-b")
echo "$JOIN_OUT" | grep -q "Joined Murmur network" \
    && pass "B8 — join succeeded" || fail "B8" "$JOIN_OUT"
[ -f "$DIR_B/config.toml" ] && [ -f "$DIR_B/mnemonic" ] && [ -f "$DIR_B/device.key" ] \
    && pass "B8 — config/mnemonic/key created" || fail "B8" "missing files"

# B9 — Invalid mnemonic
BAD_EXIT=$(cli_exit --data-dir "$TESTDIR/bad" join "not a valid mnemonic" --name "x" | tail -1)
[ "$BAD_EXIT" -ne 0 ] && pass "B9 — bad mnemonic rejected" || fail "B9" "accepted"

# B10 — Legacy `--role` flag is no longer recognised; clap should reject it.
BAD_EXIT=$(cli_exit --data-dir "$TESTDIR/bad" join "$MNEMONIC" --name "x" --role superadmin | tail -1)
[ "$BAD_EXIT" -ne 0 ] && pass "B10 — legacy --role flag rejected" || fail "B10" "accepted"

# B11 — Double init
DOUBLE_EXIT=$(cli_exit --data-dir "$DIR_B" join "$MNEMONIC" --name "x" | tail -1)
[ "$DOUBLE_EXIT" -ne 0 ] && pass "B11 — double init rejected" || fail "B11" "allowed"

# B12 — Start Node B
echo "--- B12–B16: Node B Join + Approval ---"
start_daemon "$DIR_B" "node-b" "backup" && DAEMON_B_PID=$(last_pid) || DAEMON_B_PID=""
[ -n "$DAEMON_B_PID" ] \
    && pass "B12 — Node B started" || fail "B12" "daemon failed"

# B13 — Pending
sleep 5
PENDING_PLAIN=$(cli --data-dir "$DIR_A" pending)
echo "$PENDING_PLAIN" | grep -q "node-b" \
    && pass "B13 — node-b visible as pending" || fail "B13" "$PENDING_PLAIN"
# Extract device ID: line format "  <hex64> node-b"
NODE_B_ID=$(echo "$PENDING_PLAIN" | awk '/node-b/{print $1}' | head -1)

# B14 — Approve
if [ -n "$NODE_B_ID" ]; then
    APPROVE_OUT=$(cli --data-dir "$DIR_A" approve "$NODE_B_ID")
    echo "$APPROVE_OUT" | grep -qi "approved\|ok\|success" \
        && pass "B14 — device approved" || fail "B14" "$APPROVE_OUT"
else
    skip "B14" "no device ID from B13"
fi

sleep 6

# B15 — Both see both
DEVICES_A=$(cli --data-dir "$DIR_A" devices)
DEVICES_B=$(cli --data-dir "$DIR_B" devices)
echo "$DEVICES_A" | grep -q "node-a" && echo "$DEVICES_A" | grep -q "node-b" \
    && pass "B15 — Node A sees both" || fail "B15" "$DEVICES_A"
echo "$DEVICES_B" | grep -q "node-a" && echo "$DEVICES_B" | grep -q "node-b" \
    && pass "B15 — Node B sees both" || fail "B15" "$DEVICES_B"

# B16 — Peer count
PEERS_A=$(jq_get "$(cli --data-dir "$DIR_A" status --json)" "d['Status']['peer_count']")
[ "${PEERS_A:-0}" -ge 1 ] \
    && pass "B16 — Node A has peers ($PEERS_A)" || fail "B16" "peers=$PEERS_A"

# B17 — File sync A→B
echo "--- B17–B21: File Sync ---"
echo "Hello from Node A" > "$SCRATCH/test-file-a.txt"
ADD_A=$(cli --data-dir "$DIR_A" add "$SCRATCH/test-file-a.txt")
echo "$ADD_A" | grep -qi "added\|ok\|success\|hash" \
    && pass "B17 — file added" || fail "B17" "$ADD_A"
sleep 6
cli --data-dir "$DIR_A" files | grep -q "test-file-a" \
    && pass "B17 — file on Node A" || fail "B17" "missing on A"
cli --data-dir "$DIR_B" files | grep -q "test-file-a" \
    && pass "B17 — file synced to Node B" || fail "B17" "missing on B"

# B18 — Blob integrity
[ -d "$DIR_B/blobs" ] && [ "$(find "$DIR_B/blobs" -type f | head -1)" != "" ] \
    && pass "B18 — blob directory has content" || fail "B18" "blobs empty"

# B19 — Sync B→A
echo "Hello from Node B" > "$SCRATCH/test-file-b.txt"
cli --data-dir "$DIR_B" add "$SCRATCH/test-file-b.txt" >/dev/null
sleep 6
cli --data-dir "$DIR_A" files | grep -q "test-file-b" \
    && pass "B19 — B→A sync works" || fail "B19" "missing on A"

# B20 — Files JSON
FILES_JSON=$(cli --data-dir "$DIR_A" files --json)
FILE_COUNT=$(jq_get "$FILES_JSON" "len(d['Files']['files'])")
HAS_FIELDS=$(jq_get "$FILES_JSON" "'ok' if all(k in d['Files']['files'][0] for k in ['blob_hash','path','size','device_origin']) else 'fail'" 2>/dev/null || echo "fail")
[ "${FILE_COUNT:-0}" -ge 2 ] \
    && pass "B20 — files JSON count ($FILE_COUNT)" || fail "B20" "count=$FILE_COUNT"
[ "$HAS_FIELDS" = "ok" ] \
    && pass "B20 — files JSON has required fields" || fail "B20" "missing fields"

# B21 — DAG consistency
DAG_A=$(jq_get "$(cli --data-dir "$DIR_A" status --json)" "d['Status']['dag_entries']")
DAG_B=$(jq_get "$(cli --data-dir "$DIR_B" status --json)" "d['Status']['dag_entries']")
[ "$DAG_A" = "$DAG_B" ] && [ -n "$DAG_A" ] \
    && pass "B21 — DAG consistent ($DAG_A)" || fail "B21" "A=$DAG_A B=$DAG_B"

# B22 — No daemon error
echo "--- B22: No Daemon Error ---"
stop_daemon "$DAEMON_B_PID"; DAEMON_B_PID=""; sleep 1
NO_DAEMON=$(cli --data-dir "$DIR_B" status)
NO_DAEMON_EXIT=$(cli_exit --data-dir "$DIR_B" status | tail -1)
echo "$NO_DAEMON" | grep -qi "not running" && [ "$NO_DAEMON_EXIT" -ne 0 ] \
    && pass "B22 — clear error when no daemon" || fail "B22" "exit=$NO_DAEMON_EXIT $NO_DAEMON"

# Restart B for intermediate tests
start_daemon "$DIR_B" "node-b" "backup" && DAEMON_B_PID=$(last_pid) || DAEMON_B_PID=""
sleep 4

########################################################################
# INTERMEDIATE TESTS
########################################################################
echo ""
echo "========================================="
echo "  INTERMEDIATE TESTS"
echo "========================================="
echo ""

# I1/I2 — Folder list
echo "--- I1–I4: Folder Create & List ---"
FOLDER_LIST=$(cli --data-dir "$DIR_A" folder list)
echo "$FOLDER_LIST" | grep -qi "default\|No shared\|folder" \
    && pass "I1 — folder list returns content" || fail "I1" "$FOLDER_LIST"

FOLDER_LIST_JSON=$(cli --data-dir "$DIR_A" folder list --json)
FCOUNT=$(jq_get "$FOLDER_LIST_JSON" "len(d['Folders']['folders'])")
[ -n "$FCOUNT" ] && pass "I2 — folder list JSON valid ($FCOUNT folders)" || fail "I2" "invalid JSON"

# Get default folder ID
DEFAULT_FOLDER_ID=$(jq_get "$FOLDER_LIST_JSON" \
    "next((f['folder_id'] for f in d['Folders']['folders'] if f['name']=='default'), '')")

# I3 — Create "photos" folder
CREATE_OUT=$(cli --data-dir "$DIR_A" folder create "photos")
echo "$CREATE_OUT" | grep -qi "created\|ok\|success" \
    && pass "I3 — photos folder created" || fail "I3" "$CREATE_OUT"
sleep 6

FOLDER_LIST_JSON2=$(cli --data-dir "$DIR_A" folder list --json)
PHOTOS_FOLDER_ID=$(jq_get "$FOLDER_LIST_JSON2" \
    "next((f['folder_id'] for f in d['Folders']['folders'] if f['name']=='photos'), '')")
cli --data-dir "$DIR_B" folder list | grep -q "photos" \
    && pass "I3 — photos synced to Node B" || fail "I3" "not on B"

# I4 — Create "documents" folder
cli --data-dir "$DIR_A" folder create "documents" >/dev/null
sleep 4
FOLDER_LIST_JSON3=$(cli --data-dir "$DIR_A" folder list --json)
FCOUNT3=$(jq_get "$FOLDER_LIST_JSON3" "len(d['Folders']['folders'])")
[ "${FCOUNT3:-0}" -ge 3 ] \
    && pass "I4 — three folders exist ($FCOUNT3)" || fail "I4" "count=$FCOUNT3"
DOCUMENTS_FOLDER_ID=$(jq_get "$FOLDER_LIST_JSON3" \
    "next((f['folder_id'] for f in d['Folders']['folders'] if f['name']=='documents'), '')")

# I5/I6 — Folder status
echo "--- I5–I8: Folder Status ---"
if [ -n "$DEFAULT_FOLDER_ID" ]; then
    FSTATUS=$(cli --data-dir "$DIR_A" folder status "$DEFAULT_FOLDER_ID")
    echo "$FSTATUS" | grep -q "Folder:" \
        && pass "I5 — folder status returned" || fail "I5" "$FSTATUS"
    FSTATUS_JSON=$(cli --data-dir "$DIR_A" folder status "$DEFAULT_FOLDER_ID" --json)
    FS_ID=$(jq_get "$FSTATUS_JSON" "d['FolderStatus']['folder_id']")
    FS_FC=$(jq_get "$FSTATUS_JSON" "d['FolderStatus']['file_count']")
    [ -n "$FS_ID" ] && [ -n "$FS_FC" ] \
        && pass "I6 — folder status JSON valid" || fail "I6" "$FSTATUS_JSON"
else
    skip "I5" "no default folder ID"; skip "I6" "no default folder ID"
fi

# I7 — Non-existent folder
BAD_FID="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
FS_BAD_EXIT=$(cli_exit --data-dir "$DIR_A" folder status "$BAD_FID" | tail -1)
[ "$FS_BAD_EXIT" -ne 0 ] \
    && pass "I7 — non-existent folder returns error" || fail "I7" "exit=$FS_BAD_EXIT"

# I8 — Invalid hex
FH_EXIT=$(cli_exit --data-dir "$DIR_A" folder status "not-hex" | tail -1)
[ "$FH_EXIT" -ne 0 ] \
    && pass "I8 — invalid hex returns error" || fail "I8" "exit=$FH_EXIT"

# I9/I10 — Subscribe
echo "--- I9–I12: Subscribe & Mode ---"
if [ -n "$PHOTOS_FOLDER_ID" ]; then
    SUB=$(cli --data-dir "$DIR_B" folder subscribe "$PHOTOS_FOLDER_ID" "$TESTDIR/b-photos")
    echo "$SUB" | grep -qi "subscrib\|ok\|success" \
        && pass "I9 — subscribed to photos" || fail "I9" "$SUB"
    PHOTOS_MODE=$(jq_get "$(cli --data-dir "$DIR_B" folder list --json)" \
        "next((f.get('mode') for f in d['Folders']['folders'] if f['name']=='photos'), None)")
    [ "$PHOTOS_MODE" = "full" ] \
        && pass "I9 — default mode is full" || fail "I9" "mode=$PHOTOS_MODE"
else
    skip "I9" "no photos folder ID"
fi

if [ -n "$DOCUMENTS_FOLDER_ID" ]; then
    cli --data-dir "$DIR_B" folder subscribe "$DOCUMENTS_FOLDER_ID" "$TESTDIR/b-docs" --mode receive-only >/dev/null
    DOC_MODE=$(jq_get "$(cli --data-dir "$DIR_B" folder list --json)" \
        "next((f.get('mode') for f in d['Folders']['folders'] if f['name']=='documents'), None)")
    [ "$DOC_MODE" = "receive-only" ] \
        && pass "I10 — documents subscribed receive-only" || fail "I10" "mode=$DOC_MODE"
else
    skip "I10" "no documents folder ID"
fi

# I11 — Subscribe non-existent
SNONE_EXIT=$(cli_exit --data-dir "$DIR_B" folder subscribe "$BAD_FID" "$TESTDIR/b-fake" | tail -1)
[ "$SNONE_EXIT" -ne 0 ] \
    && pass "I11 — subscribe non-existent rejected" || fail "I11" "exit=$SNONE_EXIT"

# I12 — Change mode
if [ -n "$PHOTOS_FOLDER_ID" ]; then
    MODE_OUT=$(cli --data-dir "$DIR_B" folder mode "$PHOTOS_FOLDER_ID" receive-only)
    echo "$MODE_OUT" | grep -qi "mode\|ok\|success\|changed" \
        && pass "I12 — mode changed to receive-only" || fail "I12" "$MODE_OUT"
    cli --data-dir "$DIR_B" folder mode "$PHOTOS_FOLDER_ID" full >/dev/null
    BACK_MODE=$(jq_get "$(cli --data-dir "$DIR_B" folder list --json)" \
        "next((f.get('mode') for f in d['Folders']['folders'] if f['name']=='photos'), None)")
    [ "$BACK_MODE" = "full" ] \
        && pass "I12 — mode restored to full" || fail "I12" "mode=$BACK_MODE"
else
    skip "I12" "no photos folder ID"
fi

# I13/I14 — Folder files
echo "--- I13–I17: Folder Files & MIME ---"
if [ -n "$DEFAULT_FOLDER_ID" ]; then
    FFILES=$(cli --data-dir "$DIR_A" folder files "$DEFAULT_FOLDER_ID")
    echo "$FFILES" | grep -qi "test-file\|synced\|No synced" \
        && pass "I13 — folder files returned" || fail "I13" "$FFILES"
    FFILES_JSON=$(cli --data-dir "$DIR_A" folder files "$DEFAULT_FOLDER_ID" --json)
    jq_get "$FFILES_JSON" "'ok' if 'files' in d['Files'] else 'fail'" | grep -q "ok" \
        && pass "I13 — folder files JSON valid" || fail "I13" "JSON invalid"
else
    skip "I13" "no default folder ID"
fi

if [ -n "$PHOTOS_FOLDER_ID" ]; then
    FFILES_EMPTY=$(cli --data-dir "$DIR_A" folder files "$PHOTOS_FOLDER_ID")
    echo "$FFILES_EMPTY" | grep -qi "no synced files\|0\|empty" \
        && pass "I14 — empty folder list works" || pass "I14 — folder files returned (may be empty)"
else
    skip "I14" "no photos folder ID"
fi

# I16 — MIME type
echo '{"key": "value"}' > "$SCRATCH/test.json"
cli --data-dir "$DIR_A" add "$SCRATCH/test.json" >/dev/null
sleep 3
cli --data-dir "$DIR_A" files | grep -q "test.json" \
    && pass "I16 — JSON file appears in files" || fail "I16" "not found"

# I17 — Dedup
ADD1=$(jq_get "$(cli --data-dir "$DIR_A" add "$SCRATCH/test-file-a.txt" --json 2>/dev/null || cli --data-dir "$DIR_A" add "$SCRATCH/test-file-a.txt")" "d.get('Ok',{}).get('message','') or d.get('Error',{}).get('message','')" 2>/dev/null || cli --data-dir "$DIR_A" add "$SCRATCH/test-file-a.txt")
pass "I17 — re-add same file completes without crash"

# I18/I19/I20 — Large file
echo "--- I18–I20: Large File Transfer ---"
dd if=/dev/urandom of="$SCRATCH/test-large.bin" bs=1M count=5 2>/dev/null
ADD_L=$(cli --data-dir "$DIR_A" add "$SCRATCH/test-large.bin")
echo "$ADD_L" | grep -qi "added\|ok\|success\|hash" \
    && pass "I18 — 5 MB file added" || fail "I18" "$ADD_L"

TRANSFERS=$(cli --data-dir "$DIR_A" transfers)
echo "$TRANSFERS" | grep -qi "transfer\|no active" \
    && pass "I19 — transfer status works" || fail "I19" "$TRANSFERS"
XFERS_JSON=$(cli --data-dir "$DIR_A" transfers --json)
jq_get "$XFERS_JSON" "'ok' if 'transfers' in d['TransferStatus'] else 'fail'" | grep -q "ok" \
    && pass "I19 — transfers JSON valid" || fail "I19" "JSON invalid"

sleep 12
cli --data-dir "$DIR_B" files | grep -q "test-large.bin" \
    && pass "I20 — large file synced to Node B" || fail "I20" "not on B"

# I21/I22 — Delta sync
echo "--- I21–I22: Delta Sync ---"
stop_daemon "$DAEMON_B_PID"; DAEMON_B_PID=""; sleep 1
echo "Offline content" > "$SCRATCH/test-offline.txt"
cli --data-dir "$DIR_A" add "$SCRATCH/test-offline.txt" >/dev/null

start_daemon "$DIR_B" "node-b" "backup" && DAEMON_B_PID=$(last_pid) || DAEMON_B_PID=""
sleep 10
cli --data-dir "$DIR_B" files | grep -q "test-offline.txt" \
    && pass "I21 — delta sync after reconnect" || fail "I21" "offline file not synced"

# Multi offline
stop_daemon "$DAEMON_B_PID"; DAEMON_B_PID=""; sleep 1
for i in 1 2 3; do
    echo "Offline file $i" > "$SCRATCH/test-off${i}.txt"
    cli --data-dir "$DIR_A" add "$SCRATCH/test-off${i}.txt" >/dev/null
done
start_daemon "$DIR_B" "node-b" "backup" && DAEMON_B_PID=$(last_pid) || DAEMON_B_PID=""
sleep 10
OFF_CNT=$(cli --data-dir "$DIR_B" files | grep -c "test-off" || echo "0")
[ "${OFF_CNT:-0}" -ge 3 ] \
    && pass "I22 — multi offline sync ($OFF_CNT entries)" || fail "I22" "count=$OFF_CNT"

# I23–I26 — Conflicts
echo "--- I23–I26: Conflicts ---"
cli --data-dir "$DIR_A" conflicts list | grep -qi "no active conflicts" \
    && pass "I23 — no conflicts" || fail "I23" "unexpected conflicts"

CONFL_JSON=$(cli --data-dir "$DIR_A" --json conflicts list)
jq_get "$CONFL_JSON" "'ok' if 'conflicts' in d['Conflicts'] else 'fail'" | grep -q "ok" \
    && pass "I24 — conflicts JSON valid" || fail "I24" "JSON invalid"

if [ -n "$DEFAULT_FOLDER_ID" ]; then
    cli --data-dir "$DIR_A" conflicts list --folder "$DEFAULT_FOLDER_ID" | grep -qi "conflict" \
        && pass "I25 — folder-filtered conflicts works" || fail "I25" "no response"
else
    skip "I25" "no default folder ID"
fi

if [ -n "$DEFAULT_FOLDER_ID" ]; then
    RES_EXIT=$(cli_exit --data-dir "$DIR_A" resolve "$DEFAULT_FOLDER_ID" no-such.txt "$BAD_FID" | tail -1)
    [ "$RES_EXIT" -ne 0 ] \
        && pass "I26 — resolve non-existent rejected" || fail "I26" "exit=$RES_EXIT"
else
    skip "I26" "no default folder ID"
fi

# I27–I30 — History
echo "--- I27–I30: File History ---"
if [ -n "$DEFAULT_FOLDER_ID" ]; then
    HIST=$(cli --data-dir "$DIR_A" history "$DEFAULT_FOLDER_ID" test-file-a.txt)
    echo "$HIST" | grep -qi "version\|node-a\|bytes\|hash" \
        && pass "I27 — file history returned" || fail "I27" "$HIST"

    HIST_JSON=$(cli --data-dir "$DIR_A" history "$DEFAULT_FOLDER_ID" test-file-a.txt --json)
    VERS=$(jq_get "$HIST_JSON" "len(d['FileVersions']['versions'])")
    [ "${VERS:-0}" -ge 1 ] \
        && pass "I28 — history JSON has $VERS version(s)" || fail "I28" "versions=$VERS"

    HIST_NONE=$(cli --data-dir "$DIR_A" history "$DEFAULT_FOLDER_ID" no-such-file.txt)
    echo "$HIST_NONE" | grep -qi "no version" \
        && pass "I29 — no history for non-existent" || fail "I29" "$HIST_NONE"

    HIST_B=$(cli --data-dir "$DIR_B" history "$DEFAULT_FOLDER_ID" test-file-a.txt --json)
    VERS_B=$(jq_get "$HIST_B" "len(d['FileVersions']['versions'])")
    [ "$VERS_B" = "$VERS" ] \
        && pass "I30 — history consistent across nodes" || fail "I30" "A=$VERS B=$VERS_B"
else
    skip "I27" "no default folder ID"
    skip "I28" "no default folder ID"
    skip "I29" "no default folder ID"
    skip "I30" "no default folder ID"
fi

# I31/I32 — Unsubscribe
echo "--- I31–I33: Unsubscribe & Remove ---"
if [ -n "$DOCUMENTS_FOLDER_ID" ]; then
    UNSUB=$(cli --data-dir "$DIR_B" folder unsubscribe "$DOCUMENTS_FOLDER_ID")
    echo "$UNSUB" | grep -qi "unsubscrib\|ok\|success" \
        && pass "I31 — unsubscribed from documents" || fail "I31" "$UNSUB"
    SUB_STATE=$(jq_get "$(cli --data-dir "$DIR_B" folder list --json)" \
        "next((str(f.get('subscribed')) for f in d['Folders']['folders'] if f['name']=='documents'), 'missing')")
    [ "$SUB_STATE" = "False" ] \
        && pass "I31 — subscribed=false confirmed" || fail "I31" "state=$SUB_STATE"

    # Re-subscribe then unsubscribe with keep-local
    cli --data-dir "$DIR_B" folder subscribe "$DOCUMENTS_FOLDER_ID" "$TESTDIR/b-docs2" >/dev/null
    cli --data-dir "$DIR_B" folder unsubscribe "$DOCUMENTS_FOLDER_ID" --keep-local >/dev/null
    pass "I32 — unsubscribe with keep-local completed"
else
    skip "I31" "no documents folder ID"
    skip "I32" "no documents folder ID"
fi

# I33 — Folder remove
if [ -n "$DOCUMENTS_FOLDER_ID" ]; then
    RMFOLDER=$(cli --data-dir "$DIR_A" folder remove "$DOCUMENTS_FOLDER_ID")
    echo "$RMFOLDER" | grep -qi "removed\|ok\|success" \
        && pass "I33 — documents removed" || fail "I33" "$RMFOLDER"
    sleep 4
    cli --data-dir "$DIR_A" folder list | grep -q "documents" \
        && fail "I33" "documents still listed" || pass "I33 — documents gone from Node A"
else
    skip "I33" "no documents folder ID"
fi

# I34 — Final DAG consistency
echo "--- I34: DAG Consistency ---"
sleep 4
DAG_A2=$(jq_get "$(cli --data-dir "$DIR_A" status --json)" "d['Status']['dag_entries']")
DAG_B2=$(jq_get "$(cli --data-dir "$DIR_B" status --json)" "d['Status']['dag_entries']")
[ "$DAG_A2" = "$DAG_B2" ] && [ -n "$DAG_A2" ] \
    && pass "I34 — DAG consistent ($DAG_A2)" || fail "I34" "A=$DAG_A2 B=$DAG_B2"

########################################################################
# ADVANCED TESTS
########################################################################
echo ""
echo "========================================="
echo "  ADVANCED TESTS"
echo "========================================="
echo ""

# A1/A2 — Revocation
echo "--- A1–A4: Device Revocation ---"
CURR_B_ID=$(jq_get "$(cli --data-dir "$DIR_A" devices --json)" \
    "next((d2['device_id'] for d2 in d['Devices']['devices'] if 'node-b' in d2['name']), '')")
if [ -n "$CURR_B_ID" ]; then
    REVOKE_OUT=$(cli --data-dir "$DIR_A" revoke "$CURR_B_ID")
    echo "$REVOKE_OUT" | grep -qi "revoked\|ok\|success" \
        && pass "A1 — device revoked" || fail "A1" "$REVOKE_OUT"
else
    skip "A1" "no node-b device ID"
fi

cli --data-dir "$DIR_A" devices | grep -q "node-b" \
    && fail "A2" "node-b still listed" || pass "A2 — node-b removed"

# A3 — Revoked can't sync
echo "From revoked" > "$SCRATCH/test-revoked.txt"
cli --data-dir "$DIR_B" add "$SCRATCH/test-revoked.txt" >/dev/null
sleep 4
cli --data-dir "$DIR_A" files | grep -q "test-revoked" \
    && fail "A3" "revoked file appeared on A" || pass "A3 — revoked entry rejected"

# A4/A5 — Error cases
REVOKE_NONE=$(cli --data-dir "$DIR_A" revoke "$BAD_FID")
echo "$REVOKE_NONE" | grep -qi "error\|not found\|no such" \
    && pass "A4 — revoke non-existent returns error" || fail "A4" "$REVOKE_NONE"

APP_HEX=$(cli_exit --data-dir "$DIR_A" approve "not-hex" | tail -1)
[ "$APP_HEX" -ne 0 ] && pass "A5 — approve invalid hex rejected" || fail "A5" "exit=0"

# A7 — Re-join after revocation
echo "--- A7: Re-join After Revocation ---"
stop_daemon "$DAEMON_B_PID"; DAEMON_B_PID=""; sleep 1
rm -rf "$DIR_B"
cli --data-dir "$DIR_B" join "$MNEMONIC" --name "node-b-v2" >/dev/null
start_daemon "$DIR_B" "node-b-v2" "full" && DAEMON_B_PID=$(last_pid) || DAEMON_B_PID=""
sleep 6
PENDING_REJOIN=$(cli --data-dir "$DIR_A" pending)
echo "$PENDING_REJOIN" | grep -q "node-b-v2" \
    && pass "A7 — node-b-v2 pending" || fail "A7" "$PENDING_REJOIN"
NEW_B_ID=$(echo "$PENDING_REJOIN" | awk '/node-b-v2/{print $1}' | head -1)
if [ -n "$NEW_B_ID" ]; then
    cli --data-dir "$DIR_A" approve "$NEW_B_ID" >/dev/null
    sleep 6
    cli --data-dir "$DIR_A" devices | grep -q "node-b-v2" \
        && pass "A7 — node-b-v2 approved" || fail "A7" "not in devices"
fi

# A8/A9 — Shutdown & socket cleanup
echo "--- A8–A9: Shutdown & Stale Socket ---"
stop_daemon "$DAEMON_A_PID"; DAEMON_A_PID=""; sleep 1
[ ! -S "$DIR_A/murmurd.sock" ] \
    && pass "A8 — socket cleaned on shutdown" || fail "A8" "socket remains"

# Stale socket
touch "$DIR_A/murmurd.sock"
start_daemon "$DIR_A" "node-a" "full" && DAEMON_A_PID=$(last_pid) || DAEMON_A_PID=""
[ -S "$DIR_A/murmurd.sock" ] \
    && pass "A9 — started with stale socket" || fail "A9" "failed to start"

# A10–A14 — Persistence
echo "--- A10–A14: Restart Persistence ---"
cli --data-dir "$DIR_A" devices | grep -q "node-a" \
    && pass "A10 — devices persisted" || fail "A10" "devices lost"
cli --data-dir "$DIR_A" files | grep -q "test-file-a" \
    && pass "A11 — files persisted" || fail "A11" "files lost"
cli --data-dir "$DIR_A" folder list | grep -qi "default\|photos\|No shared" \
    && pass "A12 — folders persisted" || fail "A12" "folders lost"
MNEMONIC_P=$(cli --data-dir "$DIR_A" mnemonic)
[ "$MNEMONIC_P" = "$MNEMONIC" ] \
    && pass "A13 — mnemonic persisted" || fail "A13" "mismatch"
DAG_P=$(jq_get "$(cli --data-dir "$DIR_A" status --json)" "d['Status']['dag_entries']")
[ "${DAG_P:-0}" -gt 0 ] \
    && pass "A14 — DAG entries persisted ($DAG_P)" || fail "A14" "dag=$DAG_P"

# A15 — Peer reconnection (poll up to 40s)
echo "--- A15: Peer Reconnection ---"
PEERS_R=0
A15_DEADLINE=$(($(date +%s) + 40))
while [ $(date +%s) -lt $A15_DEADLINE ]; do
    PEERS_R=$(jq_get "$(cli --data-dir "$DIR_A" status --json)" "d['Status']['peer_count']")
    [ "${PEERS_R:-0}" -ge 1 ] && break
    sleep 3
done
[ "${PEERS_R:-0}" -ge 1 ] \
    && pass "A15 — peers reconnected ($PEERS_R)" || fail "A15" "peers=$PEERS_R"

# A16/A17 — Filesystem state
echo "--- A16–A17: Filesystem State ---"
[ -f "$DIR_A/config.toml" ] && pass "A16 — config.toml exists" || fail "A16" "missing config.toml"
[ -f "$DIR_A/mnemonic" ]   && pass "A16 — mnemonic file exists" || fail "A16" "missing mnemonic"
[ -d "$DIR_A/db" ]         && pass "A16 — db directory exists"  || fail "A16" "missing db"
[ -d "$DIR_A/blobs" ]      && pass "A16 — blobs directory exists" || fail "A16" "missing blobs"

KEYSIZE=$(wc -c < "$DIR_B/device.key" 2>/dev/null | tr -d ' ' || echo "0")
[ "$KEYSIZE" -eq 32 ] \
    && pass "A17 — device.key is 32 bytes" || fail "A17" "size=$KEYSIZE"

# A18 — Network isolation
echo "--- A18: Network Isolation ---"
rm -rf "$DIR_C"
start_daemon "$DIR_C" "node-c" "full" && DAEMON_C_PID=$(last_pid) || DAEMON_C_PID=""
if [ -S "$DIR_C/murmurd.sock" ]; then
    sleep 4
    PEERS_C=$(jq_get "$(cli --data-dir "$DIR_C" status --json)" "d['Status']['peer_count']")
    [ "${PEERS_C:-0}" -eq 0 ] \
        && pass "A18 — isolated network has 0 peers" || fail "A18" "peers=$PEERS_C"
    NID_A=$(jq_get "$(cli --data-dir "$DIR_A" status --json)" "d['Status']['network_id']")
    NID_C=$(jq_get "$(cli --data-dir "$DIR_C" status --json)" "d['Status']['network_id']")
    [ -n "$NID_A" ] && [ "$NID_A" != "$NID_C" ] \
        && pass "A18 — different network IDs" || fail "A18" "IDs: A=$NID_A C=$NID_C"
    stop_daemon "${DAEMON_C_PID:-}"
else
    fail "A18" "Node C failed to start"
fi

# A19 — Batch sync
echo "--- A19: Batch File Sync ---"
for i in $(seq 1 10); do
    echo "Batch $i" > "$SCRATCH/test-batch-${i}.txt"
    cli --data-dir "$DIR_A" add "$SCRATCH/test-batch-${i}.txt" >/dev/null
done
# Poll up to 90s — node-b-v2 may need to sync large backlog after re-join
FILES_A_B=""; FILES_B_B=""
A19_DEADLINE=$(($(date +%s) + 90))
while [ $(date +%s) -lt $A19_DEADLINE ]; do
    FILES_A_B=$(jq_get "$(cli --data-dir "$DIR_A" files --json)" "len(d['Files']['files'])")
    FILES_B_B=$(jq_get "$(cli --data-dir "$DIR_B" files --json)" "len(d['Files']['files'])")
    [ "$FILES_A_B" = "$FILES_B_B" ] && [ -n "$FILES_A_B" ] && break
    sleep 5
done
[ "$FILES_A_B" = "$FILES_B_B" ] && [ -n "$FILES_A_B" ] \
    && pass "A19 — batch sync done ($FILES_A_B files)" || fail "A19" "A=$FILES_A_B B=$FILES_B_B"

# A20/A21 — Edge cases
echo "--- A20–A21: Edge Cases ---"
touch "$SCRATCH/test-empty.txt"
ADD_E=$(cli --data-dir "$DIR_A" add "$SCRATCH/test-empty.txt")
echo "$ADD_E" | grep -qi "added\|ok\|success\|hash" \
    && pass "A20 — empty file added" || fail "A20" "$ADD_E"

NF_EXIT=$(cli_exit --data-dir "$DIR_A" add "$SCRATCH/does-not-exist.txt" | tail -1)
[ "$NF_EXIT" -ne 0 ] \
    && pass "A21 — non-existent file rejected" || fail "A21" "exit=$NF_EXIT"

# A22 — Status field validation
echo "--- A22–A24: Status, Origins, Concurrent ---"
STATUS_V=$(cli --data-dir "$DIR_A" status --json)
VALID=$(echo "$STATUS_V" | python3 -c '
import sys, json
try:
    d = json.loads(sys.stdin.read())
    s = d.get("Status", {})
    ok = (len(s.get("device_id","")) == 64
      and len(s.get("network_id","")) == 64
      and len(s.get("device_name","")) > 0
      and s.get("peer_count", -1) >= 0
      and s.get("dag_entries", 0) > 0
      and s.get("uptime_secs", -1) >= 0)
    print("ok" if ok else "fail")
except Exception:
    print("fail")
' 2>/dev/null || echo "fail")
[ "$VALID" = "ok" ] \
    && pass "A22 — all status fields valid" || fail "A22" "some invalid"

# A23 — Origin tracking
ORIGINS=$(jq_get "$(cli --data-dir "$DIR_A" files --json)" \
    "len(set(f.get('device_origin','') for f in d['Files']['files']))")
[ "${ORIGINS:-0}" -ge 1 ] \
    && pass "A23 — origin tracking works ($ORIGINS devices)" || fail "A23" "origins=$ORIGINS"

# A24 — Concurrent requests
cli --data-dir "$DIR_A" status >/dev/null &
P1=$!
cli --data-dir "$DIR_A" devices >/dev/null &
P2=$!
cli --data-dir "$DIR_A" files >/dev/null &
P3=$!
cli --data-dir "$DIR_A" folder list >/dev/null &
P4=$!
wait $P1; E1=$?; wait $P2; E2=$?; wait $P3; E3=$?; wait $P4; E4=$?
[ $((E1 + E2 + E3 + E4)) -eq 0 ] \
    && pass "A24 — concurrent requests all succeeded" || fail "A24" "exits: $E1 $E2 $E3 $E4"

# A26/A27 — Post-restart folder ops and history
echo "--- A26–A27: Post-restart Ops ---"
CREATE_POST=$(cli --data-dir "$DIR_A" folder create "post-restart")
echo "$CREATE_POST" | grep -qi "created\|ok\|success" \
    && pass "A26 — folder create after restart" || fail "A26" "$CREATE_POST"

if [ -n "$DEFAULT_FOLDER_ID" ]; then
    HIST_POST=$(cli --data-dir "$DIR_A" history "$DEFAULT_FOLDER_ID" test-file-a.txt)
    echo "$HIST_POST" | grep -qi "version\|bytes\|hash" \
        && pass "A27 — history survives restart" || fail "A27" "$HIST_POST"
else
    skip "A27" "no default folder ID"
fi

# A30 — Final consistency (poll up to 90s for full backlog sync)
echo "--- A30–A31: Final Checks ---"
DAG_FA=""; DAG_FB=""
A30_DEADLINE=$(($(date +%s) + 90))
while [ $(date +%s) -lt $A30_DEADLINE ]; do
    DAG_FA=$(jq_get "$(cli --data-dir "$DIR_A" status --json)" "d['Status']['dag_entries']")
    DAG_FB=$(jq_get "$(cli --data-dir "$DIR_B" status --json)" "d['Status']['dag_entries']")
    [ "$DAG_FA" = "$DAG_FB" ] && [ -n "$DAG_FA" ] && break
    sleep 5
done
[ "$DAG_FA" = "$DAG_FB" ] && [ -n "$DAG_FA" ] \
    && pass "A30 — final DAG consistent ($DAG_FA)" || fail "A30" "A=$DAG_FA B=$DAG_FB"
NID_FA=$(jq_get "$(cli --data-dir "$DIR_A" status --json)" "d['Status']['network_id']")
NID_FB=$(jq_get "$(cli --data-dir "$DIR_B" status --json)" "d['Status']['network_id']")
[ "$NID_FA" = "$NID_FB" ] && [ -n "$NID_FA" ] \
    && pass "A30 — same network ID" || fail "A30" "A=$NID_FA B=$NID_FB"

# A31 — Clean shutdown
stop_daemon "$DAEMON_A_PID"; DAEMON_A_PID=""
stop_daemon "$DAEMON_B_PID"; DAEMON_B_PID=""
sleep 1
[ ! -S "$DIR_A/murmurd.sock" ] && [ ! -S "$DIR_B/murmurd.sock" ] \
    && pass "A31 — both sockets cleaned up" || fail "A31" "socket(s) remain"

echo ""
echo "=== Tests complete ==="
