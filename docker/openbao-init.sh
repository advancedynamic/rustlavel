#!/usr/bin/env bash
#
# Configure OpenBao's database secrets engine against `postgres-vault`.
#
# A dev-mode OpenBao starts with nothing mounted, so on its own it can answer
# the token, AppRole and key/value assertions and skips the interesting half.
# The tests that matter want it handing out *real* PostgreSQL accounts:
#
#   framework/crates/rustlavel-db/tests/rotation.rs
#       reads `database/creds/app` twice and proves a live pool can move from
#       the first account to the second and then survive the first lease being
#       revoked. Role name: `app`.
#
#   framework/crates/rustlavel-vault/tests/openbao.rs
#       proves a dynamic credential logs in and stops working the moment its
#       lease is revoked — not expired in OpenBao's bookkeeping, but deleted
#       from the database. Role name: `app-readwrite`, from
#       VAULT_TEST_DATABASE_ROLE.
#
# Two roles rather than one because the two files name different ones, and a
# test that quietly reuses the other suite's role would be asserting something
# about this script instead of about the code.
#
# The connection URL is written from *OpenBao's* point of view, so it uses the
# compose service name and the container port. The tests reach the same server
# from the host, at 127.0.0.1:55433 — that is what VAULT_TEST_DATABASE_HOST is.
#
# Safe to re-run: every write is a PUT to a fixed path.
set -euo pipefail

address="${VAULT_ADDR:-http://127.0.0.1:18200}"
token="${VAULT_TOKEN:-root-token}"

bao() {
  local path="$1" body="$2"
  local status
  status=$(curl -s -o /tmp/openbao-init.out -w '%{http_code}' \
    -H "X-Vault-Token: $token" -X POST \
    -d "$body" "${address%/}/v1/$path")
  # 200 and 204 are both success; a mount that already exists answers 400 with
  # "path is already in use", which is fine on a second run.
  case "$status" in
    2*) ;;
    400) grep -q 'already in use' /tmp/openbao-init.out \
           || { echo "POST $path failed: $(cat /tmp/openbao-init.out)" >&2; exit 1; } ;;
    *) echo "POST $path failed with $status: $(cat /tmp/openbao-init.out)" >&2; exit 1 ;;
  esac
}

# Wait for the API rather than trusting the health check alone: the container
# reports healthy the moment `bao status` succeeds, which is a shade before the
# dev-mode root token is usable.
for _ in $(seq 1 60); do
  curl -sf -H "X-Vault-Token: $token" "${address%/}/v1/sys/mounts" >/dev/null && break
  sleep 1
done

bao sys/mounts/database '{"type":"database"}'

bao database/config/appdb '{
  "plugin_name": "postgresql-database-plugin",
  "allowed_roles": ["app", "app-readwrite"],
  "connection_url": "postgresql://{{username}}:{{password}}@postgres-vault:5432/appdb?sslmode=disable",
  "username": "postgres",
  "password": "rootpass"
}'

# rotation.rs only ever runs `select current_user`, so USAGE on the schema is
# all its accounts need.
bao database/roles/app '{
  "db_name": "appdb",
  "default_ttl": "1h",
  "max_ttl": "24h",
  "creation_statements": ["CREATE ROLE \"{{name}}\" WITH LOGIN PASSWORD '"'"'{{password}}'"'"' VALID UNTIL '"'"'{{expiration}}'"'"'; GRANT USAGE ON SCHEMA public TO \"{{name}}\";"]
}'

bao database/roles/app-readwrite '{
  "db_name": "appdb",
  "default_ttl": "1h",
  "max_ttl": "24h",
  "creation_statements": ["CREATE ROLE \"{{name}}\" WITH LOGIN PASSWORD '"'"'{{password}}'"'"' VALID UNTIL '"'"'{{expiration}}'"'"'; GRANT ALL PRIVILEGES ON ALL TABLES IN SCHEMA public TO \"{{name}}\";"]
}'

# Prove it end to end rather than assuming: if the plugin cannot reach
# PostgreSQL, this is where it says so, instead of six tests later.
if ! curl -sf -H "X-Vault-Token: $token" "${address%/}/v1/database/creds/app" >/dev/null; then
  echo "the database secrets engine is mounted but cannot issue a credential" >&2
  exit 1
fi

echo "OpenBao configured: roles app, app-readwrite against postgres-vault"
