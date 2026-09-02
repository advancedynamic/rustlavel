#!/usr/bin/env bash
#
# Generate the certificates the TLS suites need.
#
# Two of the eight integration suites cannot run without a certificate, and
# neither can invent one for itself:
#
#   framework/crates/rustlavel-db/tests/tls.rs
#       wants a CA plus a server certificate that names `localhost`, because
#       `postgres_verifies_the_certificate_against_a_named_ca` connects with
#       `sslmode=verify-full` — which checks the hostname, so a certificate
#       without `subjectAltName=DNS:localhost,IP:127.0.0.1` fails even though
#       the chain is perfectly good.
#
#   framework/crates/rustlavel-ldap/tests/tls.rs
#       wants a self-signed certificate with the same names. It is deliberately
#       *not* signed by the CA above: every successful handshake in that file
#       asks for `dangerously_accept_any_certificate`, and the last test proves
#       that flag is load-bearing by watching verification reject this exact
#       certificate. Chaining it to a CA the tests could trust would quietly
#       delete that test's meaning.
#
# Everything lands in docker/certs/, which is git-ignored. Re-running is safe;
# it regenerates from scratch so a half-written run cannot linger.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
certs="$here/certs"

rm -rf "$certs"
mkdir -p "$certs/ldap"

# --- PostgreSQL: a private CA, and a server certificate signed by it ---------
#
# This mirrors the `openssl` block in the header of rustlavel-db/tests/tls.rs.
# The one difference is that it runs on the host rather than through
# `docker exec`, so `docker/certs/ca.crt` is already where PG_TLS_CA can point
# at it without copying anything back out of a container.
openssl req -new -x509 -days 3650 -nodes \
  -out "$certs/ca.crt" -keyout "$certs/ca.key" \
  -subj "/CN=rustlavel-test-ca" 2>/dev/null

openssl req -new -nodes \
  -out "$certs/server.csr" -keyout "$certs/server.key" \
  -subj "/CN=localhost" 2>/dev/null

# The extensions are the whole point. `verify-full` fails without the SAN, and
# a certificate that forgot `CA:FALSE`/`serverAuth` is the kind of thing that
# works against one TLS stack and not the next.
openssl x509 -req -in "$certs/server.csr" \
  -CA "$certs/ca.crt" -CAkey "$certs/ca.key" -CAcreateserial \
  -out "$certs/server.crt" -days 3650 \
  -extfile <(printf 'subjectAltName=DNS:localhost,IP:127.0.0.1\nbasicConstraints=CA:FALSE\nextendedKeyUsage=serverAuth\n') \
  2>/dev/null

rm -f "$certs/server.csr" "$certs/ca.srl"

# --- OpenLDAP: one self-signed certificate, used as its own CA ---------------
openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
  -keyout "$certs/ldap/server.key" -out "$certs/ldap/server.crt" \
  -subj "/CN=localhost" \
  -addext "subjectAltName=DNS:localhost,IP:127.0.0.1" \
  -addext "basicConstraints=CA:FALSE" \
  -addext "extendedKeyUsage=serverAuth" 2>/dev/null
cp "$certs/ldap/server.crt" "$certs/ldap/ca.crt"

# The Bitnami image runs OpenLDAP as an unprivileged user, so it has to be able
# to read the key it was handed. PostgreSQL is the opposite — it refuses a key
# anyone else can read — which is why its copy is installed with mode 600 by
# docker/init/postgres-tls.sh from inside the container rather than mounted
# straight into the data directory.
chmod 644 "$certs/ldap"/*

echo "certificates written to $certs"
