#!/usr/bin/env bash
# gui.sh — GUI automation helpers for Claude Code to interact with desktop apps
# Usage: source this file, then call the functions.
# All functions require X11 access (sandbox must be disabled for X11 socket).

export DISPLAY=:1
export XAUTHORITY=/tmp/claude-1000/Xauthority

SCREENSHOT_DIR="/tmp/claude-1000"
SCREENSHOT_PATH="${SCREENSHOT_DIR}/screenshot.png"
SCREEN_WIDTH=3840
SCREEN_HEIGHT=1600

# ─── Screenshot ───────────────────────────────────────────────────────────────

# Capture the full screen
screenshot() {
    ffmpeg -f x11grab -video_size "${SCREEN_WIDTH}x${SCREEN_HEIGHT}" \
        -i "${DISPLAY}" -frames:v 1 -y "${SCREENSHOT_PATH}" \
        -loglevel error 2>&1
    echo "${SCREENSHOT_PATH}"
}

# Capture a specific window by name (substring match)
screenshot_window() {
    local name="$1"
    local wid
    wid=$(xdotool search --name "$name" | head -1)
    if [ -z "$wid" ]; then
        echo "ERROR: No window matching '$name'" >&2
        return 1
    fi
    local geom
    geom=$(xdotool getwindowgeometry --shell "$wid")
    eval "$geom"  # sets X, Y, WIDTH, HEIGHT, WINDOW
    ffmpeg -f x11grab -video_size "${WIDTH}x${HEIGHT}" \
        -i "${DISPLAY}+${X},${Y}" -frames:v 1 -y "${SCREENSHOT_PATH}" \
        -loglevel error 2>&1
    echo "${SCREENSHOT_PATH}"
}

# ─── Mouse ────────────────────────────────────────────────────────────────────

# Click at absolute screen coordinates
click() {
    local x="$1" y="$2"
    xdotool mousemove "$x" "$y" click 1
}

# Double-click at coordinates
dclick() {
    local x="$1" y="$2"
    xdotool mousemove "$x" "$y" click --repeat 2 1
}

# Right-click at coordinates
rclick() {
    local x="$1" y="$2"
    xdotool mousemove "$x" "$y" click 3
}

# ─── Keyboard ─────────────────────────────────────────────────────────────────

# Type text string (with small delay between keys for reliability)
typetext() {
    xdotool type --delay 20 "$1"
}

# Send a key combination (e.g., "ctrl+a", "Return", "Tab", "ctrl+shift+t")
key() {
    xdotool key "$1"
}

# ─── Window Management ───────────────────────────────────────────────────────

# Focus a window by name (substring match)
focus() {
    local name="$1"
    local wid
    wid=$(xdotool search --name "$name" | head -1)
    if [ -z "$wid" ]; then
        echo "ERROR: No window matching '$name'" >&2
        return 1
    fi
    xdotool windowactivate "$wid"
    echo "Focused window $wid ($name)"
}

# List all visible windows
windows() {
    xdotool search --onlyvisible --name "" 2>/dev/null | while read -r wid; do
        local name
        name=$(xdotool getwindowname "$wid" 2>/dev/null)
        if [ -n "$name" ]; then
            echo "$wid  $name"
        fi
    done
}

# Get window geometry
wingeom() {
    local name="$1"
    local wid
    wid=$(xdotool search --name "$name" | head -1)
    if [ -z "$wid" ]; then
        echo "ERROR: No window matching '$name'" >&2
        return 1
    fi
    xdotool getwindowgeometry "$wid"
}

# ─── Convenience ──────────────────────────────────────────────────────────────

# Click a button: focus window, then click at (x, y) relative to window
winclick() {
    local name="$1" rx="$2" ry="$3"
    local wid
    wid=$(xdotool search --name "$name" | head -1)
    if [ -z "$wid" ]; then
        echo "ERROR: No window matching '$name'" >&2
        return 1
    fi
    xdotool windowactivate "$wid"
    local geom
    geom=$(xdotool getwindowgeometry --shell "$wid")
    eval "$geom"
    local ax=$((X + rx))
    local ay=$((Y + ry))
    xdotool mousemove "$ax" "$ay" click 1
}

echo "GUI helpers loaded. Functions: screenshot, screenshot_window, click, dclick, rclick, typetext, key, focus, windows, wingeom, winclick"
