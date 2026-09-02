#!/usr/bin/env bash
#
# Tear everything down and leave nothing behind.
#
# `-v` rather than a plain `down`, and that is deliberate: postgres-tls keeps
# its data in a named volume, and its TLS settings are written by an init script
# that only runs while the data directory is empty. A volume that survives a
# teardown is a server that ignores a regenerated certificate on the next run.
#
# The certificates go too, for the same reason — they and the server that was
# given them have to be created and destroyed together.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

docker compose -f "$here/compose.yml" down -v --remove-orphans
rm -rf "$here/certs"

echo "down: containers, volumes and certificates removed"
