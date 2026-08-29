#!/usr/bin/env bash
#
# Configuration 1: Caddy in front of PHP-FPM.
#
# The classic Laravel deployment — the whole framework is booted, from
# autoloader to service providers, once per request. `php artisan serve` is a
# development server and is deliberately not used.
#
#     PORT=8080 ./run-fpm.sh
#
# Runs in the foreground; Ctrl-C (or SIGTERM) stops both Caddy and PHP-FPM.
#
# See deploy/Caddyfile.tmpl for why the front end is Caddy and not nginx.

set -euo pipefail

APP_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PORT="${PORT:-8080}"
PHP_BIN="${PHP_BIN:-/opt/homebrew/Cellar/php/8.5.8/bin/php}"
PHP_FPM_BIN="${PHP_FPM_BIN:-/opt/homebrew/Cellar/php/8.5.8/sbin/php-fpm}"
CADDY_BIN="${CADDY_BIN:-${APP_ROOT}/caddy}"

export PHP_INI_SCAN_DIR=":${APP_ROOT}/deploy/php.d"

mkdir -p "${APP_ROOT}/run"
rm -f "${APP_ROOT}/run/php-fpm.sock" "${APP_ROOT}/run/php-fpm.pid"

sed -e "s#__APP_ROOT__#${APP_ROOT}#g" \
    "${APP_ROOT}/deploy/php-fpm.conf.tmpl" > "${APP_ROOT}/run/php-fpm.conf"

sed -e "s#__APP_ROOT__#${APP_ROOT}#g" -e "s#__PORT__#${PORT}#g" \
    "${APP_ROOT}/deploy/Caddyfile.tmpl" > "${APP_ROOT}/run/Caddyfile"

# The production caches are a deploy step, not a boot step — building them here
# would land in the harness's startup measurement, which no real deployment
# pays. Build them only if this is a fresh checkout.
if [ ! -f "${APP_ROOT}/bootstrap/cache/config.php" ]; then
    "${APP_ROOT}/build.sh" >/dev/null
fi

cleanup() {
    trap - EXIT INT TERM
    [ -n "${CADDY_PID:-}" ] && kill "${CADDY_PID}" 2>/dev/null || true
    [ -n "${FPM_PID:-}" ] && kill "${FPM_PID}" 2>/dev/null || true
    wait 2>/dev/null || true
}
trap cleanup EXIT INT TERM

"${PHP_FPM_BIN}" --nodaemonize --fpm-config "${APP_ROOT}/run/php-fpm.conf" &
FPM_PID=$!

# Wait for the socket rather than sleeping a fixed amount: the harness measures
# time to the first response and a fixed sleep would be added to it.
for _ in $(seq 1 400); do
    [ -S "${APP_ROOT}/run/php-fpm.sock" ] && break
    sleep 0.05
done

"${CADDY_BIN}" run --config "${APP_ROOT}/run/Caddyfile" --adapter caddyfile &
CADDY_PID=$!

echo "laravel/fpm listening on http://127.0.0.1:${PORT}"

# `wait -n` would be tidier but macOS ships bash 3.2. Poll instead, off the
# request path, and exit as soon as either process dies.
while kill -0 "${FPM_PID}" 2>/dev/null && kill -0 "${CADDY_PID}" 2>/dev/null; do
    sleep 1
done
