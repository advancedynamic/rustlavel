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
    "axum|apps/axum|./target/release/bench-axum|target/release/bench-axum"
    "spring|apps/spring|java -Xmx512m -jar target/spring-bench.jar --spring.profiles.active=production|target/spring-bench.jar"
    "loco|apps/loco|./target/release/loco-cli start -e production -n|target/release/loco-cli"
    # Laravel twice: the way most people deploy it, and the way that keeps the
    # application resident between requests. The gap between the two is the
    # single most useful number Laravel contributes here.
    "laravel-fpm|apps/laravel|./run-fpm.sh|vendor"
    "laravel-octane|apps/laravel|./run-octane.sh|vendor"
)

stop_server() {
    local pid="$1"
    for child in $(pgrep -P "$pid" 2>/dev/null); do
        kill "$child" 2>/dev/null
    done
    kill "$pid" 2>/dev/null
    wait "$pid" 2>/dev/null

    # Whatever still holds the port goes too, or the next app cannot bind.
    local holding
    holding=$(lsof -nP -tiTCP:"$PORT" -sTCP:LISTEN 2>/dev/null)
    if [ -n "$holding" ]; then
        kill $holding 2>/dev/null
        sleep 1
        holding=$(lsof -nP -tiTCP:"$PORT" -sTCP:LISTEN 2>/dev/null)
        [ -n "$holding" ] && kill -9 $holding 2>/dev/null
    fi
    # Give the socket a moment to leave TIME_WAIT before the next bind.
    sleep 1
}

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

# A benchmark on a busy machine measures the machine.
#
# This check exists because the first comparison run here was invalid and the
# numbers were reversed by it: a previous run was still going, a second load
# generator was hammering the same port, four compilers were running, and the
# load average was 23. Every number from that run said more about what else was
# happening than about any framework.
require_a_quiet_machine() {
    local problems=0

    local holding
    holding=$(lsof -nP -tiTCP:"$PORT" -sTCP:LISTEN 2>/dev/null)
    if [ -n "$holding" ]; then
        fail "port $PORT is already in use by pid(s): $holding"
        fail "  something else is serving there; two servers on one port makes nonsense"
        problems=1
    fi

    local generators
    generators=$(pgrep -f "$(basename "$OHA") " 2>/dev/null | grep -v "^$$\$" | wc -l | tr -d ' ')
    if [ "$generators" -gt 0 ]; then
        fail "another $(basename "$OHA") is already running ($generators process(es))"
        fail "  two load generators on one machine measure each other"
        problems=1
    fi

    local runners
    runners=$(pgrep -f "bash .*run\.sh" 2>/dev/null | grep -vc "^$$\$" || true)
    if [ "${runners:-0}" -gt 0 ]; then
        fail "another run.sh is already running"
        problems=1
    fi

    local load
    load=$(uptime | sed -E 's/.*load averages?: ([0-9.]+).*/\1/')
    local cores
    cores=$(sysctl -n hw.ncpu 2>/dev/null || nproc 2>/dev/null || echo 8)
    if python3 -c "import sys; sys.exit(0 if float('$load') > float('$cores') * 0.5 else 1)"; then
        fail "load average is $load on $cores cores — the machine is busy"
        fail "  wait for it to settle, or set ALLOW_BUSY=1 and distrust the numbers"
        [ "${ALLOW_BUSY:-0}" = "1" ] || problems=1
    fi

    return $problems
}

if ! require_a_quiet_machine; then
    fail ""
    fail "Refusing to benchmark. Nothing measured here would be worth reading."
    exit 1
fi

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
        stop_server "$server"
        continue
    fi
    printf '    startup      %10s ms\n' "$startup"

    if ! verify_contract "$name"; then
        fail "  contract not met — not benchmarking this app"
        stop_server "$server"
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

    stop_server "$server"
    echo
done

python3 "$HERE/summarise.py" "$RESULTS" | tee "$RESULTS/summary.md"
log "Written to $RESULTS/summary.md"
