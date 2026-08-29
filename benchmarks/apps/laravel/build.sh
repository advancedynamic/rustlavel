#!/usr/bin/env bash
#
# The deploy step. Everything here is what a real Laravel deployment runs once,
# on the build machine — not on every boot, which is why the run scripts do not
# run it and why the harness's startup number is not measuring a cache rebuild.
#
#     ./build.sh
#
# Produces: vendor/ with a no-dev optimised autoloader, and bootstrap/cache/
# with the config, event, route and view caches.

set -euo pipefail

APP_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PHP_BIN="${PHP_BIN:-/opt/homebrew/Cellar/php/8.5.8/bin/php}"
COMPOSER_BIN="${COMPOSER_BIN:-/opt/homebrew/bin/composer}"

cd "${APP_ROOT}"

"${PHP_BIN}" "${COMPOSER_BIN}" install --no-dev --optimize-autoloader --no-interaction

# `optimize` is config:cache + event:cache + route:cache + view:cache. They are
# spelled out as well so it is obvious which four caches the benchmark runs
# with, and so a Laravel version that changes what `optimize` covers cannot
# quietly drop one.
"${PHP_BIN}" artisan config:cache --no-interaction
"${PHP_BIN}" artisan event:cache --no-interaction
"${PHP_BIN}" artisan route:cache --no-interaction
"${PHP_BIN}" artisan view:cache --no-interaction
"${PHP_BIN}" artisan optimize --no-interaction

# Octane's FrankenPHP binary. ~170 MB, gitignored, downloaded once.
if [ ! -x "${APP_ROOT}/frankenphp" ]; then
    "${PHP_BIN}" artisan octane:install --server=frankenphp --no-interaction
fi

# Caddy, the front end for the PHP-FPM configuration. See deploy/Caddyfile.tmpl
# for why it is Caddy and not nginx.
if [ ! -x "${APP_ROOT}/caddy" ]; then
    CADDY_VERSION=2.11.4
    curl -sSL -o "${APP_ROOT}/run-caddy.tar.gz" \
        "https://github.com/caddyserver/caddy/releases/download/v${CADDY_VERSION}/caddy_${CADDY_VERSION}_mac_arm64.tar.gz"
    tar -xzf "${APP_ROOT}/run-caddy.tar.gz" -C "${APP_ROOT}" caddy
    rm -f "${APP_ROOT}/run-caddy.tar.gz"
    chmod +x "${APP_ROOT}/caddy"
    xattr -d com.apple.quarantine "${APP_ROOT}/caddy" 2>/dev/null || true
fi

echo "built."
