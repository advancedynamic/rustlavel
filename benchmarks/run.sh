#!/usr/bin/env bash
#
# The benchmark runner.
#
# Deliberately a shell script rather than a program: a benchmark harness that
# nobody can read is a benchmark nobody should believe, and every decision that
# affects a number is meant to be visible in one file.
#
#   ./run.sh                 # every app, every case
#   ./run.sh rustlavel axum  # only these apps
#
# Results land in results/<timestamp>/ as raw oha JSON plus a summary table.

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RESULTS="$HERE/results/$(date +%Y%m%d-%H%M%S)"
OHA="${OHA:-$HOME/.cargo/bin/oha}"
PORT="${PORT:-8080}"
DATABASE_URL="${DATABASE_URL:-postgres://bench:bench@127.0.0.1:55440/bench}"

# Each measurement runs this long at this concurrency, repeated this many times;
# the median run is reported. One run of anything is a story, not a measurement.
DURATION="${DURATION:-10s}"
CONCURRENCY="${CONCURRENCY:-64}"
REPEATS="${REPEATS:-3}"
WARMUP="${WARMUP:-5s}"

# The eight cases, in the contract's order.
CASES=(
    "plaintext:/plaintext"
    "json:/json"
    "routing:/users/42/posts/hello-world"
    "middleware:/middleware"
    "json-big:/json-big"
    "db-user:/db/user/42"
    "db-posts:/db/posts"
    "template:/template"
)

# name:start-command:artifact-path
# The start command is run from the app's own directory.
declare -a APPS=(
    "rustlavel|apps/rustlavel|./target/release/bench-rustlavel|target/release/bench-rustlavel"
)

log()  { printf '\033[38;5;68m%s\033[0m\n' "$*"; }
warn() { printf '\033[38;5;179m%s\033[0m\n' "$*"; }
fail() { printf '\033[38;5;167m%s\033[0m\n' "$*"; }

# Wait until the app answers, and say how long it took.
#
# This is the startup number, and it is measured the way it matters: from launch
# to the first request that actually succeeds, not to the log line claiming the
# server is ready.
await_ready() {
    local started deadline elapsed
    started=$(python3 -c 'import time; print(time.time())')
    deadline=$((SECONDS + 120))

    while [ "$SECONDS" -lt "$deadline" ]; do
        if curl -fsS -m 2 "http://127.0.0.1:$PORT/plaintext" >/dev/null 2>&1; then
            elapsed=$(python3 -c "import time; print(f'{(time.time() - $started) * 1000:.0f}')")
            echo "$elapsed"
            return 0
        fi
        sleep 0.05
    done
    echo "-1"
    return 1
}

# Every endpoint answers 200 before anything is timed.
#
# Without this a 500 error page benchmarks as a very fast response, and the
# framework that is broken wins. It has already happened once here.
verify_contract() {
    local app="$1" broken=0
    for entry in "${CASES[@]}"; do
        local path="${entry#*:}" name="${entry%%:*}" status
        status=$(curl -s -o /dev/null -w '%{http_code}' -m 10 "http://127.0.0.1:$PORT$path")
        if [ "$status" != "200" ]; then
            fail "  $app: $name ($path) answered $status, not 200"
            broken=1
        fi
    done

    local headers
    headers=$(curl -s -D- -o /dev/null -m 10 "http://127.0.0.1:$PORT/middleware" \
        | grep -ci '^x-bench-[1-5]:')
    if [ "$headers" -ne 5 ]; then
        fail "  $app: /middleware carried $headers of the 5 x-bench headers"
        broken=1
    fi

    return $broken
}

# Resident set size in MB, for the process and any children it forked.
memory_mb() {
    local pid="$1" total
    total=$(ps -o rss= -p "$pid" 2>/dev/null | awk '{s+=$1} END {print s+0}')
    for child in $(pgrep -P "$pid" 2>/dev/null); do
        total=$((total + $(ps -o rss= -p "$child" 2>/dev/null | awk '{s+=$1} END {print s+0}')))
    done
    echo $((total / 1024))
}

run_case() {
    local app="$1" case_name="$2" path="$3"
    local best_rps=0 best_file=""

    for run in $(seq 1 "$REPEATS"); do
        local out="$RESULTS/$app.$case_name.$run.json"
        "$OHA" -z "$DURATION" -c "$CONCURRENCY" --no-tui --output-format json \
            "http://127.0.0.1:$PORT$path" > "$out" 2>/dev/null

        local rps
        rps=$(python3 -c "
import json,sys
try:
    print(json.load(open('$out'))['summary']['requestsPerSec'])
except Exception:
    print(0)
")
        # The median of three would be better than the best of three, and the
        # summary below computes it from all the files; this only tracks which
        # run to mention if something goes wrong.
        if python3 -c "import sys; sys.exit(0 if $rps > $best_rps else 1)"; then
            best_rps="$rps"; best_file="$out"
        fi
    done
    printf '    %-12s %10.0f req/s\n' "$case_name" "$best_rps"
}

mkdir -p "$RESULTS"
log "Results → $RESULTS"
log "$DURATION at concurrency $CONCURRENCY, $REPEATS runs per case, median reported"
echo

if [ ! -x "$OHA" ]; then
    fail "oha not found at $OHA — install it with: cargo install oha --locked"
    exit 1
fi

WANTED=("$@")
for spec in "${APPS[@]}"; do
    IFS='|' read -r name dir start artifact <<< "$spec"

    if [ ${#WANTED[@]} -gt 0 ] && [[ ! " ${WANTED[*]} " =~ " $name " ]]; then
        continue
    fi

    log "── $name"
    if [ ! -d "$HERE/$dir" ]; then
        warn "  skipping: $dir does not exist yet"
        continue
    fi

    (cd "$HERE/$dir" && PORT="$PORT" DATABASE_URL="$DATABASE_URL" $start \
        > "$RESULTS/$name.server.log" 2>&1) &
    server=$!

    startup=$(await_ready)
    if [ "$startup" = "-1" ]; then
        fail "  never became ready — see $RESULTS/$name.server.log"
        kill "$server" 2>/dev/null
        continue
    fi
    printf '    startup      %10s ms\n' "$startup"

    if ! verify_contract "$name"; then
        fail "  contract not met — not benchmarking this app"
        kill "$server" 2>/dev/null
        wait "$server" 2>/dev/null
        continue
    fi

    "$OHA" -z "$WARMUP" -c "$CONCURRENCY" --no-tui --output-format json \
        "http://127.0.0.1:$PORT/plaintext" >/dev/null 2>&1

    for entry in "${CASES[@]}"; do
        run_case "$name" "${entry%%:*}" "${entry#*:}"
    done

    memory=$(memory_mb "$server")
    printf '    memory       %10s MB\n' "$memory"

    size="n/a"
    if [ -e "$HERE/$dir/$artifact" ]; then
        size=$(du -sm "$HERE/$dir/$artifact" 2>/dev/null | cut -f1)
    fi
    printf '    artifact     %10s MB\n' "$size"

    printf '{"app":"%s","startup_ms":%s,"memory_mb":%s,"artifact_mb":"%s"}\n' \
        "$name" "$startup" "$memory" "$size" > "$RESULTS/$name.meta.json"

    kill "$server" 2>/dev/null
    wait "$server" 2>/dev/null
    echo
done

python3 "$HERE/summarise.py" "$RESULTS" | tee "$RESULTS/summary.md"
log "Written to $RESULTS/summary.md"
