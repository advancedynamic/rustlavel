#!/usr/bin/env bash
#
# Configuration 2: Laravel Octane on FrankenPHP.
#
# The application is booted once and stays resident; each request reuses the
# already-built container, the already-compiled routes and the already-open PDO
# connection. This is the model the Rust and Java entries use, and the reason
# Laravel is in the comparison at all.
#
#     PORT=8080 ./run-octane.sh
#
# Runs in the foreground; Ctrl-C stops it.

set -euo pipefail

APP_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PORT="${PORT:-8080}"
PHP_BIN="${PHP_BIN:-/opt/homebrew/Cellar/php/8.5.8/bin/php}"

# FrankenPHP embeds its own PHP (8.5.10, against 8.5.8 for the FPM run), and
# reads the same production ini the FPM configuration uses.
export PHP_INI_SCAN_DIR=":${APP_ROOT}/deploy/php.d"

mkdir -p "${APP_ROOT}/run"

# The production caches are a deploy step, not a boot step — building them here
# would land in the harness's startup measurement, which no real deployment
# pays. Build them only if this is a fresh checkout.
if [ ! -f "${APP_ROOT}/bootstrap/cache/config.php" ]; then
    "${APP_ROOT}/build.sh" >/dev/null
fi

# Sixteen workers, matching the sixteen-connection pool the contract asks for:
# one resident worker holds one PDO handle. --max-requests=0 keeps every worker
# alive for the whole run so the harness never measures a worker restart.
#
# --caddyfile points at Octane's own stub with the per-request access log and
# the response compression removed; see deploy/Caddyfile.octane.
exec "${PHP_BIN}" "${APP_ROOT}/artisan" octane:start \
    --server=frankenphp \
    --host=127.0.0.1 \
    --port="${PORT}" \
    --admin-port="${OCTANE_ADMIN_PORT:-2019}" \
    --workers=16 \
    --max-requests=0 \
    --log-level=error \
    --caddyfile="${APP_ROOT}/deploy/Caddyfile.octane"
