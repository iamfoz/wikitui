#!/usr/bin/env bash
# PRD §6.8 memory check: "< 150 MB RSS with 10 tabs; `low_memory` mode < 50 MB".
#
# Drives a real wikitui binary in a detached tmux session against
# `large_pages.py` (start it first: `python3 tests/mock-server/large_pages.py`),
# opens ten tabs of generated Parsoid-like articles, visits every tab once
# more, and prints VmRSS/VmHWM from /proc plus a per-mapping RSS breakdown.
# Each run uses fresh, throwaway XDG dirs, so every first open is a cold-cache
# network fetch and the reader's own data is never touched.
#
# Linux only (/proc, tmux). Usage:
#   tests/mock-server/measure_rss.sh [wikitui args...]        # e.g. --low-memory
# Environment:
#   BIN    wikitui binary (default: target/release/wikitui — build with
#          `cargo build --release` first; a debug build's RSS is not
#          representative)
#   PORT   large_pages.py port (default 8944)
#   SIZES  article sizes in KB, one tab each
#          (default: "150 300 450 600 750 900 1050 1200 1350 1500")
#   STEP   seconds to wait after each `:tab new` (default 1.5)
set -eu
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
BIN=${BIN:-$ROOT/target/release/wikitui}
PORT=${PORT:-8944}
SIZES=${SIZES:-"150 300 450 600 750 900 1050 1200 1350 1500"}
STEP=${STEP:-1.5}
XDG=$(mktemp -d)
SESSION=wikitui-rss-$$
PID=
# Kill the process itself, not just the tmux session: wikitui handles SIGHUP
# as "reload config" and currently keeps running (busy) after its terminal
# goes away, so closing the session alone would leave it behind.
trap '[ -n "$PID" ] && kill -9 "$PID" 2>/dev/null; tmux kill-session -t "$SESSION" 2>/dev/null || true; rm -rf "$XDG"' EXIT

set -- "$@"
first=${SIZES%% *}
rest=${SIZES#* }
[ "$rest" = "$SIZES" ] && rest=""

tmux new-session -d -s "$SESSION" -x 120 -y 40 \
  "env XDG_STATE_HOME=$XDG/state XDG_DATA_HOME=$XDG/data XDG_CACHE_HOME=$XDG/cache \
   XDG_CONFIG_HOME=$XDG/config HOME=$XDG WIKITUI_BASE_URL=http://127.0.0.1:$PORT \
   $BIN --no-onboarding $* Large_$first"
sleep 2
PID=$(tmux list-panes -t "$SESSION" -F '#{pane_pid}')
mem() { grep -E 'VmRSS|VmHWM' "/proc/$PID/status" | tr -s ' \t' ' ' | tr '\n' ' '; echo; }

echo "after 1 tab:     $(mem)"
for kb in $rest; do
  tmux send-keys -t "$SESSION" ':'
  sleep 0.2
  tmux send-keys -t "$SESSION" "tab new Large_$kb" Enter
  sleep "$STEP"
done
sleep 2
echo "after all tabs:  $(mem)"
# Visit every tab once more (gt), the "reader has looked at all of them" state.
for _ in $SIZES; do
  tmux send-keys -t "$SESSION" g t
  sleep 0.5
done
sleep 2
echo "after revisits:  $(mem)"

# Resident memory by mapping: [heap] (main arena), [anon] (thread arenas,
# stacks, large mmaps), the binary's own pages, shared libraries.
awk '
  /^[0-9a-f]+-[0-9a-f]+ / { name = (NF >= 6) ? $6 : "[anon]";
                            if (name ~ /wikitui/) name = "wikitui binary";
                            else if (name ~ /^\//) { n = split(name, p, "/"); name = "lib " p[n] } }
  /^Rss:/ { rss[name] += $2 }
  END { for (k in rss) if (rss[k] > 0) printf "  %8d kB  %s\n", rss[k], k }
' "/proc/$PID/smaps" | sort -rn | head -8
tmux capture-pane -t "$SESSION" -p | head -1
