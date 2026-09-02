#!/bin/bash
#
# Turn TLS on in the `postgres-tls` container.
#
# The header of framework/crates/rustlavel-db/tests/tls.rs does this with
# `docker exec` and a restart. A compose file cannot exec, so the same work
# happens here instead: the postgres entrypoint runs everything in
# /docker-entrypoint-initdb.d against a temporary server and *then* starts the
# real one, so a setting appended to postgresql.conf here is in force by the
# time anything connects.
#
# The certificates are mounted read-only at /certs, generated on the host by
# docker/certs.sh. They are copied rather than used in place because PostgreSQL
# refuses to start with a key file that is group- or world-readable, or one
# owned by somebody other than itself or root — and a bind-mounted file carries
# whatever ownership it had on the host, which is neither.
set -euo pipefail

install -m 600 -o postgres /certs/server.key "$PGDATA/server.key"
install -m 644 -o postgres /certs/server.crt "$PGDATA/server.crt"
install -m 644 -o postgres /certs/ca.crt "$PGDATA/root.crt"

cat >>"$PGDATA/postgresql.conf" <<'CONF'

# Added by docker/init/postgres-tls.sh, so that rustlavel-db/tests/tls.rs has a
# server that can actually negotiate.
ssl = on
ssl_cert_file = 'server.crt'
ssl_key_file = 'server.key'
ssl_ca_file = 'root.crt'
CONF
