#!/usr/bin/env bash
#
# Bring up the servers for one or more groups of suites, and do the two pieces
# of setup a compose file cannot express.
#
#   ./docker/up.sh                     everything
#   ./docker/up.sh db                  just the databases
#   ./docker/up.sh ldap search         two groups
#
# The groups are the same ones the nightly workflow splits into jobs, so that a
# contributor reproducing a red job runs exactly what the job ran. See
# docker/README.md.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
compose=(docker compose -f "$here/compose.yml")

groups=("$@")
if [ ${#groups[@]} -eq 0 ]; then
  groups=(db ldap search otel secrets)
fi

services=()
want() { case " ${groups[*]} " in *" $1 "*) return 0 ;; *) return 1 ;; esac; }

# rustlavel-db/tests/{tls,revocation,rotation}.rs. OpenBao is in this group as
# well as its own, because rotation.rs is a database test that needs a secret
# store to issue the credentials it rotates between.
want db && services+=(postgres-tls mysql postgres-vault mssql openbao)
# rustlavel-ldap/tests/{directory,tls}.rs.
want ldap && services+=(ldap ldaps)
# rustlavel-search/tests/elasticsearch.rs.
want search && services+=(elasticsearch)
# rustlavel-otel/tests/collector.rs.
want otel && services+=(otel)
# rustlavel-vault/tests/openbao.rs, and the database it hands out accounts in.
want secrets && services+=(postgres-vault openbao)

if [ ${#services[@]} -eq 0 ]; then
  echo "usage: $0 [db|ldap|search|otel|secrets ...]" >&2
  exit 2
fi

# The certificates come first: postgres-tls mounts them at start-up, and its
# init script only runs while the data directory is empty, so a server started
# before they exist stays without TLS until the volume is destroyed.
#
# Only generated when missing, and deliberately so. Regenerating them under a
# running postgres-tls would leave PG_TLS_CA pointing at a CA that did not sign
# the certificate the server is already using, and `verify-full` would fail for
# a reason that has nothing to do with the code. `./docker/down.sh` clears both
# together.
if [ ! -f "$here/certs/ca.crt" ]; then
  "$here/certs.sh"
fi

# --wait blocks until every health check passes, so anything after this line can
# assume a server that answers. SQL Server is the slow one; see its comment in
# compose.yml.
"${compose[@]}" up -d --wait "${services[@]}"

# MySQL generates its own CA on first start, so it can only be fetched once the
# server is up. rustlavel-db/tests/tls.rs reads it through MYSQL_TLS_CA.
if printf '%s\n' "${services[@]}" | grep -qx mysql; then
  docker exec rustlavel-mysql cat /var/lib/mysql/ca.pem >"$here/certs/mysql-ca.pem"
  echo "MySQL's generated CA written to $here/certs/mysql-ca.pem"
fi

# A dev-mode OpenBao has nothing mounted, and the tests that matter want it
# issuing real PostgreSQL accounts.
if printf '%s\n' "${services[@]}" | grep -qx openbao; then
  "$here/openbao-init.sh"
fi

echo
echo "up: ${services[*]}"
echo "now: eval \"\$(${here}/env.sh ${groups[*]})\""
