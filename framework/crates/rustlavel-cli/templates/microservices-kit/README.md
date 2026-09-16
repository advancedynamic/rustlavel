# {{name}}

Four services and a shared crate, scaffolded by [Rustlavel](https://github.com/advancedynamic/rustlavel).

| Service | What it is |
|---|---|
| `services/gateway` | One door in front of the rest. No database, on purpose. |
| `services/auth` | An OAuth 2.1 authorization server. Owns users, clients, codes and tokens. |
| `services/api` | The resource server. Owns your domain, and never reads the auth schema. |
| `services/registry` | Optional service discovery. Delete it if your platform already tracks instances. |
| `shared/` | The types that cross between them, and nothing else. |

Each builds and deploys on its own — `cargo build --workspace` produces a binary
per service. That is the point, and the reason this is a workspace rather than
one crate with four `[[bin]]` entries: sharing a model between services is the
coupling microservices exist to avoid.

## Getting it running

**1. Two databases.** One per service; they must not be the same one.

```sh
createdb {{crate_name}}_auth
createdb {{crate_name}}_api
```

Then point `AUTH_DATABASE_URL` and `API_DATABASE_URL` at them in `.env`.

**2. Migrate each service into its own database.**

```sh
cargo run -p {{crate_name}}_auth -- migrate
cargo run -p {{crate_name}}_api  -- migrate
```

**3. Seed, and keep what it prints.**

```sh
cargo run -p {{crate_name}}_auth -- db:seed
```

It creates two clients and prints their secrets **once**. Paste both into
`.env`. They are separate on purpose: the introspection credential lives in the
resource server's own configuration, so giving it the scopes your domain checks
would let anybody who reads that file mint a token that writes.

**4. Run them.** Four services share one `.env`, so the port cannot live in it —
they would all read the same `SERVER_PORT`. A real environment variable beats
anything in the file:

```sh
SERVER_PORT=9001 cargo run -p {{crate_name}}_auth
SERVER_PORT=9002 cargo run -p {{crate_name}}_api
SERVER_PORT=9000 cargo run -p {{crate_name}}_gateway
```

**5. Ask for a token, then use it.**

```sh
curl -X POST http://127.0.0.1:9000/oauth/token \
  -d grant_type=client_credentials \
  -d client_id=demo -d client_secret=… \
  --data-urlencode 'scope=orders.read orders.write'

curl -X POST http://127.0.0.1:9000/api/orders \
  -H "authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' \
  -H 'idempotency-key: whatever-you-like' \
  -d '{"description":"A thing","amount_minor":50000,"currency":"IDR"}'
```

`http://127.0.0.1:9000/health` asks every service and names the one that is
unwell.

## Service discovery, if you want it

Leave `DISCOVERY_URL` blank and the gateway uses the two addresses in `.env`.
That is the right answer on Kubernetes or ECS, where the platform already knows
which instances are ready — a registry there is a second copy of that fact, and
two sources for one fact disagree eventually.

Set it and the arrangement changes: the gateway resolves `auth` and `api` for
every request, each service announces itself on the way up and says goodbye on
the way down, and a new instance takes traffic within thirty seconds of starting.

```sh
SERVER_PORT=8761 cargo run -p {{crate_name}}_registry
```

`SERVICE_HOST` is the one to get right — it must be an address *other machines*
can reach. The registry hands out what it is told, so a service that registers
`127.0.0.1` has told the whole estate to talk to itself.

## Things worth knowing before you change anything

**The resource server checks tokens itself.** The gateway refuses a request with
no credential, and that saves a hop and nothing else: anything on the network
can reach a service directly, and a check that happens only at the edge is
missing the moment somebody adds a second way in.

**`/oauth/*` is deliberately open at the gateway.** Getting a token is what you
do *before* you have one. Everything else requires a credential.

**Tokens survive an authorization server restart.** Clients, codes, access and
refresh tokens and consent are each backed by a table, so a deploy is a deploy
rather than a mass sign-out. The in-memory stores the framework also ships are
for tests.

**Revocation takes up to thirty seconds**, not effect immediately. The resource
server caches introspection answers for that long; the alternative is a request
to the authorization server on every single call. Choosing `signed` instead
trades those thirty seconds for an hour — revocation then waits for the token to
expire.

**Every service answers `/health`.** If you add one, give it that route — the
gateway's aggregate check probes each service, and a 404 there makes the whole
system report `degraded`.

## Layout

```
services/gateway/      routing, rate limiting, the aggregate health check
services/auth/         the authorization server, its migrations and seeders
services/api/          your domain — `src/orders.rs` is the example to replace
services/registry/     optional service discovery
shared/                Caller, problem(), join() — the contract between them
```
