# The servers the integration suites need

Eight suites in this repository assert against real servers rather than fakes,
and every one of them skips itself when the environment variable it reads is
unset. That is deliberate — `cargo test --workspace --all-features` has to stay
green on a machine with no Docker — but it means running them is opt-in, and
opting in used to mean reconstructing eight `docker run` invocations out of
eight file headers.

This directory is that reconstruction, done once. `.github/workflows/nightly.yml`
uses the same files, so a red nightly job is reproduced by the same command it
ran.

## The commands

```sh
./docker/up.sh                     # every server, and the setup they need
eval "$(./docker/env.sh)"          # every variable the suites read
cd framework && cargo test --workspace --all-features
./docker/down.sh                   # containers, volumes and certificates
```

`up.sh` and `env.sh` take the same optional group names, so a single suite does
not cost you SQL Server:

```sh
./docker/up.sh ldap
eval "$(./docker/env.sh ldap)"
cd framework && cargo test -p rustlavel-ldap --all-features
```

| Group | Servers | Suites |
|---|---|---|
| `db` | PostgreSQL ×2, MySQL, SQL Server, OpenBao | `rustlavel-db/tests/{tls,revocation,rotation}.rs` |
| `ldap` | OpenLDAP ×2 | `rustlavel-ldap/tests/{directory,tls}.rs` |
| `search` | Elasticsearch | `rustlavel-search/tests/elasticsearch.rs` |
| `otel` | OpenTelemetry Collector | `rustlavel-otel/tests/collector.rs` |
| `secrets` | OpenBao, PostgreSQL | `rustlavel-vault/tests/openbao.rs` |

Ask `env.sh` only for groups you have actually started. A suite *skips* when its
variable is unset and *fails* when the variable is set and nothing is listening,
so exporting the whole set against half a stack is the one way to manufacture a
false red.

If a suite prints `skipping: … is not set` when you expected it to run, the
variable you exported is not the one it reads. `env.sh` names the test file each
variable belongs to; that is the place to check.

## What `up.sh` does that `docker compose up` cannot

Three things, which is why there is a script and not just a compose file.

**It generates certificates first.** `rustlavel-db/tests/tls.rs` connects with
`sslmode=verify-full`, so it needs a CA and a server certificate carrying
`subjectAltName=DNS:localhost,IP:127.0.0.1` — a certificate without the SAN
fails that test even though its chain is perfect. `certs.sh` writes them into
`docker/certs/` (git-ignored), and `postgres-tls` mounts them at start-up.
`rustlavel-ldap/tests/tls.rs` gets a separate self-signed certificate, chained to
nothing on purpose: the last test in that file proves certificate verification is
a real check by watching it reject exactly this certificate.

**It fetches MySQL's CA out of the container.** MySQL generates its own on first
start, so it does not exist until the server is up. It has no `subjectAltName`
at all, which is why that suite verifies it with `sslmode=verify-ca` rather than
`verify-full`.

**It configures OpenBao's database secrets engine.** A dev-mode OpenBao has
nothing mounted and would skip the half of `openbao.rs` that matters, along with
all of `rotation.rs`. `openbao-init.sh` points it at the `postgres-vault`
container and creates the two roles those files name.

## Notes on individual servers

**SQL Server** is the slow one: a ~1.5 GB amd64-only image that needs 2 GB of
RAM and about a minute before it accepts a login, and on Apple Silicon it runs
under emulation. Its health check is correspondingly patient. It earns the wait —
it is the database that behaves differently from the other two, refusing to drop
a login that is connected at all, which is the measurement the whole rotation
design rests on. It appears in no other CI job in this repository.

**Ports are deliberately unusual** — 55432, 33306, 55433, 51433, 3389, 3636,
3390, 9200, 4318, 18200 — so this set does not collide with a PostgreSQL or a
MySQL you already have listening.

**`down.sh` removes volumes**, not just containers. `postgres-tls` writes its TLS
settings from an init script that only runs while the data directory is empty, so
a volume that survives a teardown is a server that ignores the next certificate
you generate. The certificates go with it, for the same reason: they and the
server that was handed them have to live and die together.

## Reproducing a nightly failure

The workflow's five jobs are the five groups above. A job that went red ran, in
order:

```sh
./docker/up.sh   <group>
./docker/env.sh  <group>     # appended to $GITHUB_ENV there, eval'd here
cd framework && cargo test -p <crate> --all-features --test <suite> -- --nocapture
```

and finished with `./docker/down.sh` whatever happened.
