//! The registry: where the services say they are, and where the gateway looks.
//!
//! **Optional, and worth knowing when to skip.** On Kubernetes or ECS the
//! platform already knows which instances are ready, and running this as well
//! means two sources for one fact — which disagree eventually. Delete this
//! service and point the gateway at the platform's own names instead. It earns
//! its place on plain machines, across two clouds, or beside a Spring estate
//! that already speaks Eureka.
//!
//! **It has no database**, for the same reason the gateway has none. Every
//! instance re-registers within a heartbeat of a restart, so the state rebuilds
//! itself faster than it could be loaded — and a stored copy would only ever be
//! wrong on the way back up.
//!
//! Run more than one. `PEERS` is every node including this one; each node drops
//! its own address and forwards writes to the rest.

use rustlavel::prelude::*;
use rustlavel::discovery::{Peers, RegistryServer};

#[rustlavel::main]
async fn main() -> Result<()> {
    let app = App::new()?;

    // Every node, including this one. Operators paste the same list into every
    // node's configuration, because keeping three different lists correct is
    // how one of them ends up wrong — so this node takes itself out.
    let own = rustlavel::env::env_or("REGISTRY_URL", "http://127.0.0.1:8761");
    let peers: Vec<String> = rustlavel::env::env_or("PEERS", "")
        .split(',')
        .map(str::trim)
        .filter(|address| !address.is_empty())
        .map(str::to_string)
        .collect();

    if peers.is_empty() {
        info!(
            "discovery: running alone. Set PEERS to every node's URL to replicate."
        );
    }

    app.plugin(
        RegistryServer::new()
            .peers(Peers::new(peers).excluding(&own))
            // **No authentication in front of it.** The dashboard lists every
            // host and port in the estate, which is a map of it for anybody who
            // reaches this port. That is survivable on a private network and is
            // not survivable anywhere else — so either keep this port private,
            // or pass `None` here and read the registry from the auth kit's
            // screen, which has a sign-in in front of it.
            .dashboard(Some("/discovery")),
    )
    .routes(routes)
    .run()
    .await
}

fn routes(r: &mut Router) {
    // A registry is a deployed service like any other, and an orchestrator
    // probes it the same way. Without this it restarts on a liveness check that
    // has nothing to check, or never starts on a readiness one.
    //
    // Deliberately not part of the `RegistryServer` plugin: `/health` is the
    // application's path to spend, and a plugin that claimed it would collide
    // with an application that already serves one.
    r.get("/health", |_req: Request| async { Json::object([("status", Json::from("up"))]) });
}
