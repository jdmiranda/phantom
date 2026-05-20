#!/usr/bin/env bash
# phantom-loop-forever.sh
#
# Self-restarting wrapper for `phantom loop run`. Each iteration:
#   1. Fetches origin and snapshots origin/main SHA
#   2. Builds phantom --release
#   3. Launches the loop daemon
#   4. Spawns a poller that watches origin/main and SIGTERMs the daemon
#      when upstream advances (so autonomous PR merges trigger a restart
#      onto the new code)
#   5. On daemon exit, logs the code and either exits (SIGINT) or restarts
#
# Logs land in `<repo>/.phantom/forever-logs/iter-<N>-<UTC>.log`.
#
# Env overrides:
#   PHANTOM_FOREVER_REPO       — repo root (default: pwd)
#   PHANTOM_FOREVER_LOOPS      — comma-separated loop ids
#   PHANTOM_FOREVER_POLL_SECS  — upstream-poll cadence in seconds (default: 300)
#   PHANTOM_FOREVER_NO_PULL    — set non-empty to skip auto-pull on restart

set -u

REPO="${PHANTOM_FOREVER_REPO:-$(pwd)}"
LOOPS="${PHANTOM_FOREVER_LOOPS:-implementer,reviewer,pr_finder_review,pr_finder_impl}"
POLL_SECS="${PHANTOM_FOREVER_POLL_SECS:-300}"
LOG_DIR="$REPO/.phantom/forever-logs"

mkdir -p "$LOG_DIR"
cd "$REPO"

stamp() { date -u +'%Y-%m-%dT%H:%M:%SZ'; }
log()   { printf '[forever %s] %s\n' "$(stamp)" "$*"; }

trap 'log "SIGINT received, exiting wrapper"; exit 0' INT TERM

iteration=0
while true; do
  iteration=$((iteration + 1))
  TS=$(date -u +%Y%m%dT%H%M%SZ)
  LOG_FILE="$LOG_DIR/iter-${iteration}-${TS}.log"
  : > "$LOG_FILE"

  {
    log "==== iteration $iteration begin ===="
    log "fetching origin"
    git fetch origin --quiet 2>&1 || true
    if [ -z "${PHANTOM_FOREVER_NO_PULL:-}" ] && git rev-parse --abbrev-ref HEAD | grep -qx main; then
      log "on main; fast-forward pulling"
      git pull --ff-only origin main 2>&1 || true
    fi
    BASELINE_REMOTE=$(git rev-parse origin/main 2>/dev/null || echo "")
    BASELINE_HEAD=$(git rev-parse --short HEAD 2>/dev/null || echo "")
    BASELINE_BRANCH=$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo "")
    log "branch=$BASELINE_BRANCH HEAD=$BASELINE_HEAD origin/main=$BASELINE_REMOTE"

    log "cargo build --release --bin phantom"
    if ! cargo build --release --bin phantom 2>&1; then
      log "BUILD FAILED — sleeping 60s before retry"
      sleep 60
      continue
    fi
    log "build ok"

    log "launching: phantom loop run --repo $REPO --loops $LOOPS"
    ./target/release/phantom loop run --repo "$REPO" --loops "$LOOPS" 2>&1 &
    DAEMON_PID=$!
    log "daemon PID=$DAEMON_PID"

    # Upstream poller: SIGTERM the daemon when origin/main advances so the
    # next iteration rebuilds against the new code. Runs in a subshell so
    # killing it does not terminate the parent script.
    (
      while kill -0 "$DAEMON_PID" 2>/dev/null; do
        sleep "$POLL_SECS"
        git -C "$REPO" fetch origin --quiet 2>/dev/null || continue
        NEW_REMOTE=$(git -C "$REPO" rev-parse origin/main 2>/dev/null || echo "")
        if [ -n "$NEW_REMOTE" ] && [ "$NEW_REMOTE" != "$BASELINE_REMOTE" ]; then
          log "origin/main moved $BASELINE_REMOTE -> $NEW_REMOTE; SIGTERM daemon for restart"
          kill -TERM "$DAEMON_PID" 2>/dev/null || true
          break
        fi
      done
    ) &
    POLLER_PID=$!

    wait "$DAEMON_PID" 2>/dev/null
    EXIT=$?
    kill "$POLLER_PID" 2>/dev/null || true
    wait "$POLLER_PID" 2>/dev/null || true

    log "daemon exited with code $EXIT"

    if [ "$EXIT" -eq 130 ]; then
      log "SIGINT propagated from daemon; exiting wrapper"
      exit 0
    fi

    log "sleeping 5s before next iteration"
    sleep 5
  } 2>&1 | tee -a "$LOG_FILE"
done
