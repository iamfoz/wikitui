#!/usr/bin/env bash
# Re-runs every PRD §6.8 measurement recorded in docs/PERFORMANCE.md: the
# criterion benches, the release-mode budget rows CI gates on, and the
# end-to-end pty harness (every scenario, default and low_memory). Takes
# about an hour on the 4-core reference VM, most of it the cache-hit-rate
# simulation; results land in perf-results.json (harness) and on stdout.
#
# Measure on a quiet machine: anything else compiling or running skews
# every number here (the harness refuses to start while a stray wikitui or
# perf mock is still running, but can't see other load).
#
# Usage: tests/perf/reproduce.sh [harness args...]   e.g. --n 30
set -eu
cd "$(dirname "$0")/../.."

echo "load: $(cut -d' ' -f1-3 /proc/loadavg) (want well under $(nproc))"
if pgrep -x rustc >/dev/null; then
    echo "warning: rustc is running — numbers taken now will be inflated" >&2
fi

cargo build --release
cargo bench --bench perf
cargo test --release perf_budget -- --nocapture --test-threads=1
python3 tests/perf/harness.py --low-memory --out perf-results.json "$@"
