#!/usr/bin/env bash
#
# Print the environment the integration suites read.
#
# This is the single definition of those variables. The nightly workflow appends
# the output to $GITHUB_ENV and a laptop evaluates it, so CI and a contributor
# cannot end up running against different settings — which is the failure mode
# that makes "it passes for me" take an afternoon.
#
#   eval "$(./docker/env.sh)"                    everything
#   eval "$(./docker/env.sh db)"                 just the databases
#   ./docker/env.sh --bare search >> "$GITHUB_ENV"
#
# Each line carries `export` unless `--bare` is given, and the default is the
# one that matters: without it, `eval` sets a shell variable that `cargo` — a
# child process — never sees, and every suite skips while reporting a pass.
# That is indistinguishable from success, which is why this defaults to the
# form a person types and `$GITHUB_ENV`, which cannot take `export`, asks for
# the other one explicitly.
#
# Only the requested groups are printed, and that matters: a suite skips when
# its variable is unset but *fails* when the variable is set and nothing is
# listening. Asking for a group whose containers are not up is the one way to
# turn a skip into a false red.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
certs="$here/certs"

prefix="export "
groups=()
for argument in "$@"; do
  case "$argument" in
    --bare) prefix="" ;;
    # `+=` on an unset array is an error under `set -u` in older bash, so the
    # array is initialised above rather than declared as it is first appended.
    *) groups+=("$argument") ;;
  esac
done

if [ ${#groups[@]} -eq 0 ]; then
  groups=(db ldap search otel secrets)
fi

want() { case " ${groups[*]} " in *" $1 "*) return 0 ;; *) return 1 ;; esac; }

if want db; then
  # framework/crates/rustlavel-db/tests/tls.rs. The host is `localhost` rather
  # than 127.0.0.1 because `postgres_verifies_the_certificate_against_a_named_ca`
  # connects with sslmode=verify-full, and the certificate docker/certs.sh
  # writes names localhost.
  echo "${prefix}PG_TLS_URL=postgres://rustlavel:secret@localhost:55432/rustlavel_test"
  echo "${prefix}PG_TLS_CA=$certs/ca.crt"
  # Same file. MySQL's own certificate carries no subjectAltName, so that suite
  # verifies it with sslmode=verify-ca and the host may stay numeric.
  # framework/crates/rustlavel-db/tests/{postgres,conformance}.rs,
  # rustlavel-queue/tests/postgres.rs and rustlavel-rbac/tests/store.rs all read
  # DATABASE_URL. Without it fifteen tests skip while reporting a pass, which is
  # the one failure that looks exactly like success. The TLS container serves
  # double duty here; these suites name-space their own tables.
  echo "${prefix}DATABASE_URL=postgres://rustlavel:secret@127.0.0.1:55432/rustlavel_test?sslmode=disable"
  echo "${prefix}MYSQL_URL=mysql://root:secret@127.0.0.1:33306/rustlavel_test"
  echo "${prefix}MSSQL_URL=sqlserver://sa:Rustlavel!2026@127.0.0.1:51433/master"

  echo "${prefix}MYSQL_TLS_URL=mysql://rustlavel:secret@127.0.0.1:33306/rustlavel_test"
  echo "${prefix}MYSQL_TLS_CA=$certs/mysql-ca.pem"

  # framework/crates/rustlavel-db/tests/revocation.rs. Each of these needs an
  # account that can create and drop other accounts, because what the suite
  # measures is what happens to an open connection when its account is deleted.
  echo "${prefix}REVOCATION_PG_URL=postgres://postgres:rootpass@127.0.0.1:55433/appdb?sslmode=disable"
  # No query string on the MySQL one. That suite appends `?sslmode=require`
  # itself — a fresh account under caching_sha2_password needs an encrypted
  # channel — and a URL that already carries a query turns into
  # `require?sslmode=require`, which the parser rightly refuses.
  echo "${prefix}REVOCATION_MYSQL_URL=mysql://root:secret@127.0.0.1:33306/appdb"
  echo "${prefix}REVOCATION_MSSQL_URL=sqlserver://sa:Rustlavel!2026@127.0.0.1:51433/master"

  # framework/crates/rustlavel-db/tests/rotation.rs. No credentials in the URL:
  # the whole point is that they arrive from the store at connect time and can
  # be replaced under a live pool.
  echo "${prefix}ROTATION_DB_URL=postgres://127.0.0.1:55433/appdb?sslmode=disable"
  echo "${prefix}VAULT_ADDR=http://127.0.0.1:18200"
  echo "${prefix}VAULT_TOKEN=root-token"
fi

if want ldap; then
  # framework/crates/rustlavel-ldap/tests/directory.rs — the container with no
  # certificate, where the suite proves the plain-text refusals fire.
  echo "${prefix}LDAP_TEST_URL=ldap://127.0.0.1:3389"
  # framework/crates/rustlavel-ldap/tests/tls.rs. `localhost`, not 127.0.0.1:
  # the hostname is what certificate verification checks, and the last test in
  # that file is about verification being real.
  echo "${prefix}LDAPS_TEST_URL=ldaps://localhost:3636"
  echo "${prefix}LDAP_STARTTLS_TEST_URL=ldap://localhost:3390"
fi

if want search; then
  # framework/crates/rustlavel-search/tests/elasticsearch.rs. The user and
  # password are set so the client's basic-auth path is exercised; see the
  # elasticsearch service in compose.yml for why security is left on.
  echo "${prefix}ELASTICSEARCH_URL=http://127.0.0.1:9200"
  echo "${prefix}ELASTICSEARCH_USER=elastic"
  echo "${prefix}ELASTICSEARCH_PASSWORD=rustlavel-secret"
fi

if want otel; then
  # framework/crates/rustlavel-otel/tests/collector.rs. The container name is
  # not decoration: the suite runs `docker logs` against it and reads the
  # collector's own output to check the payload was decoded into the span that
  # was meant.
  echo "${prefix}OTEL_TEST_ENDPOINT=http://127.0.0.1:4318"
  echo "${prefix}OTEL_TEST_CONTAINER=rustlavel-otel"
fi

if want secrets; then
  # framework/crates/rustlavel-vault/tests/openbao.rs.
  echo "${prefix}VAULT_ADDR=http://127.0.0.1:18200"
  echo "${prefix}VAULT_TOKEN=root-token"
  # The role docker/openbao-init.sh creates, and the database as the *test
  # process* reaches it — OpenBao itself talks to the same server over the
  # compose network under a different name.
  echo "${prefix}VAULT_TEST_DATABASE_ROLE=app-readwrite"
  echo "${prefix}VAULT_TEST_DATABASE_HOST=127.0.0.1:55433/appdb"
fi
