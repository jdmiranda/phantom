#!/bin/bash
# MCP-driven visual regression test for Phantom.
#
# Connects to a running Phantom instance via the MCP Unix socket,
# runs through UI scenarios, captures screenshots at each step,
# and outputs a JSON report with paths for visual verification.
#
# Usage:
#   ./tests/mcp_visual_test.sh [socket_path]

set -euo pipefail

SOCK="${1:-$(ls -t /tmp/phantom-mcp-*.sock 2>/dev/null | head -1)}"
OUTDIR="/tmp/phantom-visual-tests/$(date +%Y%m%d_%H%M%S)"
REPORT="$OUTDIR/report.json"

if [ -z "$SOCK" ] || [ ! -S "$SOCK" ]; then
    echo "ERROR: No Phantom MCP socket found. Is Phantom running?" >&2
    exit 1
fi

mkdir -p "$OUTDIR"
echo "Socket: $SOCK"
echo "Output: $OUTDIR"
echo ""

PASS=0
FAIL=0

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

mcp_call() {
    local method="$1"
    local params="$2"
    echo "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$method\",\"params\":$params}" \
        | nc -U -w 5 "$SOCK" 2>/dev/null
}

screenshot() {
    local name="$1"
    local path="$OUTDIR/${name}.png"
    local result
    result=$(mcp_call "tools/call" "{\"name\":\"phantom.screenshot\",\"arguments\":{\"path\":\"$path\"}}")
    if echo "$result" | grep -q '"path"'; then
        echo "  📸 $name"
        PASS=$((PASS + 1))
    else
        echo "  ❌ $name (screenshot failed)"
        FAIL=$((FAIL + 1))
    fi
}

send_key() {
    mcp_call "tools/call" "{\"name\":\"phantom.send_key\",\"arguments\":{\"key\":\"$1\"}}" > /dev/null 2>&1
}

phantom_cmd() {
    mcp_call "tools/call" "{\"name\":\"phantom.command\",\"arguments\":{\"command\":\"$1\"}}" > /dev/null 2>&1
}

run_command() {
    mcp_call "tools/call" "{\"name\":\"phantom.run_command\",\"arguments\":{\"command\":\"$1\"}}" > /dev/null 2>&1
}

get_context() {
    mcp_call "tools/call" "{\"name\":\"phantom.get_context\",\"arguments\":{}}"
}

# ---------------------------------------------------------------------------
# Test scenarios
# ---------------------------------------------------------------------------

echo "=== Phantom Visual Regression Tests ==="
echo ""

# Test 1: Current state
echo "Test 1: Capture current state"
screenshot "01_current_state"

# Test 2: Dismiss boot if active
echo "Test 2: Dismiss boot → terminal ready"
send_key "Enter"
sleep 0.5
send_key "Enter"
sleep 0.5
screenshot "02_terminal_ready"

# Test 3: Switch to Pip-Boy theme via phantom.command
echo "Test 3: Switch to Pip-Boy theme"
phantom_cmd "theme pipboy"
sleep 0.5
screenshot "03_pipboy_theme"

# Test 4: Run a command
echo "Test 4: Run echo command"
run_command "echo '=== PHANTOM VISUAL TEST ==='"
sleep 0.5
screenshot "04_command_output"

# Test 5: Switch to Amber theme
echo "Test 5: Switch to Amber theme"
phantom_cmd "theme amber"
sleep 0.5
screenshot "05_amber_theme"

# Test 6: Debug HUD
echo "Test 6: Debug shader HUD"
phantom_cmd "debug"
sleep 0.3
screenshot "06_debug_hud"
phantom_cmd "debug"
sleep 0.2

# Test 7: Plain mode (no CRT effects)
echo "Test 7: Plain mode"
phantom_cmd "plain"
sleep 0.3
screenshot "07_plain_mode"

# Test 8: Restore Phosphor theme
echo "Test 8: Restore Phosphor theme"
phantom_cmd "theme phosphor"
sleep 0.5
screenshot "08_phosphor_restored"

# Test 9: Project context
echo "Test 9: Project context"
CONTEXT=$(get_context)
if echo "$CONTEXT" | grep -q '"name"'; then
    echo "  ✓ context returned"
    PASS=$((PASS + 1))
else
    echo "  ❌ context failed"
    FAIL=$((FAIL + 1))
fi

# ---------------------------------------------------------------------------
# Report
# ---------------------------------------------------------------------------

SCREENSHOTS=$(ls "$OUTDIR"/*.png 2>/dev/null | wc -l | tr -d ' ')

cat > "$REPORT" << REPORTEOF
{
  "timestamp": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
  "socket": "$SOCK",
  "output_dir": "$OUTDIR",
  "screenshots": $SCREENSHOTS,
  "passed": $PASS,
  "failed": $FAIL,
  "tests": [
    {"name": "current_state", "file": "01_current_state.png"},
    {"name": "terminal_ready", "file": "02_terminal_ready.png"},
    {"name": "pipboy_theme", "file": "03_pipboy_theme.png"},
    {"name": "command_output", "file": "04_command_output.png"},
    {"name": "amber_theme", "file": "05_amber_theme.png"},
    {"name": "debug_hud", "file": "06_debug_hud.png"},
    {"name": "plain_mode", "file": "07_plain_mode.png"},
    {"name": "phosphor_restored", "file": "08_phosphor_restored.png"}
  ]
}
REPORTEOF

echo ""
echo "=== Results: $PASS passed, $FAIL failed, $SCREENSHOTS screenshots ==="
echo "Report: $REPORT"
echo "View:   open $OUTDIR"
