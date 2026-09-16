# rustlavel-discovery

A service registry and the client that uses it, for the [Rustlavel](https://github.com/advancedynamic/rustlavel) framework. Eureka-compatible on the wire, so a Spring Cloud service can register against it unmodified.

## Read this before reaching for it

**On Kubernetes, ECS or Nomad you do not need a registry.** The platform already knows which instances are ready and already balances across them. A registry there is a second copy of that fact, and two sources for one fact disagree eventually. Use `Dns`.

This crate is for what the platforms do not cover: services on plain machines, an estate spread across two clouds, or a Spring system already speaking Eureka that a Rustlavel service has to join.

## Three answers, one trait

```rust
use rustlavel::discovery::{Discovery, Dns, Resolve, Static};
```

| Resolver | Where the addresses come from |
|---|---|
| `Static` | The list you wrote. Where everybody starts, and where a great many applications correctly stay. |
| `Dns` | A name the platform already resolves. The Kubernetes answer. |
| `Discovery` | This registry, cached so that the registry being down does not stop traffic. |

Moving between them is a line of configuration, not a rewrite.

## Running a registry

```rust
use rustlavel::prelude::*;
use rustlavel::discovery::{Peers, RegistryServer};

App::new()
    .plugin(
        RegistryServer::new()
            .peers(Peers::new(nodes).excluding(&own_url)),
    )
    .run()
    .await
```

It serves Eureka's routes — `POST /eureka/apps/{app}`, `PUT /eureka/apps/{app}/{id}`, `DELETE`, the status endpoint and both reads — plus a dashboard at `/discovery` and its document at `/discovery/registry.json`.

The dashboard has **no authentication in front of it**. It lists every host and port you run. Put it behind whatever the application already uses, or `.dashboard(None)` and read the `Registry` from a page of your own — which is what the auth kit's screen does.

## Joining one

```rust
use rustlavel::discovery::{Instance, Registrar};

let registrar = Arc::new(Registrar::new(
    nodes,
    Instance::new("orders", host, port).meta("zone", "jakarta-a"),
));
registrar.clone().start();           // registers, then heartbeats
```

On shutdown, `registrar.deregister().await` — the difference between a clean deploy and thirty seconds of failures.

## Routing a gateway by name

With both the `gateway` and `discovery` features on:

```rust
let balancer = Arc::new(Balancer::new(Discovery::new(registries)));

Gateway::new()
    .route("/api/orders/*", Upstream::service("orders", balancer.clone()).strip("/api"))
    .route("/api/billing/*", Upstream::service("billing", balancer).strip("/api"))
```

A service that resolves to nothing is `503`, not a request sent to a host nobody invented.

## The decisions, and why

**The registry is in memory and nothing is persisted.** Every instance re-registers within a heartbeat of a restart, so the state rebuilds itself faster than it could be loaded — and a durable copy would only ever be *wrong* on the way back up.

**Nothing is evicted on a timer.** Instances carry a `last_seen` and a *read* decides who is alive. That is what lets the registry notice that most of itself went quiet at once and conclude the network broke rather than that the estate died. Eureka calls this self-preservation; it is its most misunderstood feature, and people disable it because "stale instances got traffic" — which is true, and is the smaller of the two failures. A registry that empties during a blink turns the blink into an outage that outlasts it, because everything then has to re-register before anything can find anything.

A registry with fewer than eight instances never enters that state: a proportion threshold applied to a handful fires on the first ordinary failure.

**Replication is peer forwarding, not consensus.** A registry is a cache of where things are, and the answer is already slightly out of date when it is given. Paying for consensus would buy agreement about a stale fact and would mean a node that loses quorum stops answering — exactly the outage the registry exists to survive. Every forwarded write carries a marker (`?isReplication=true`, and a header for anything that strips query strings), and a marked write is applied and never forwarded on: one hop, never two.

**The client's cache has no expiry.** When the registry cannot be reached, `Discovery` goes on serving what it last read, for however long that lasts. An outage costs you *changes* — a new instance is not noticed, a departed one is still offered — and the caller's own retries cover the second. A cache that expired would turn a registry outage into a total outage on a timer.

## Licence

MIT OR Apache-2.0
