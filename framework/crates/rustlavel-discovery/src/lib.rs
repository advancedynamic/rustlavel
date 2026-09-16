//! rustlavel-discovery: a service registry, and the client that uses it.
//!
//! **Read this before reaching for it.** On Kubernetes, ECS or Nomad you do not
//! need a registry: the platform already knows which instances are ready and
//! already balances across them, and a registry there is a second copy of that
//! fact which will eventually disagree with the first. Use
//! [`Dns`](resolve::Dns). This crate is for the case the platforms do not
//! cover — services on plain machines or across two clouds, or a Spring estate
//! already speaking Eureka that a Rustlavel service has to join.
//!
//! Three answers to "where is this service" sit behind one trait, so moving
//! between them is a line of configuration rather than a rewrite:
//!
//! - [`Static`](resolve::Static) — the list you wrote. Where everybody starts,
//!   and where a great many applications correctly stay.
//! - [`Dns`](resolve::Dns) — a name the platform already resolves.
//! - [`Discovery`](client::Discovery) — this registry, with a cache that
//!   outlives the registry being down.
//!
//! # Running a registry
//!
//! ```ignore
//! App::new()
//!     .plugin(RegistryServer::new().peers(Peers::new(nodes).excluding(own_url)))
//!     .run()
//!     .await
//! ```
//!
//! It speaks Eureka's wire format, so a Spring Cloud service registers against
//! it with its stock configuration and nothing else changed.
//!
//! # Joining one
//!
//! ```ignore
//! let registrar = Arc::new(Registrar::new(nodes, Instance::new("orders", host, port)));
//! registrar.clone().start();
//! ```
//!
//! # What this deliberately does not do
//!
//! The registry is in memory and there is no timer evicting anything. Both are
//! on purpose. Persistence would buy nothing — every instance re-registers
//! within a heartbeat of a restart, so the state rebuilds itself faster than it
//! could be loaded, and a durable copy would only ever be *wrong* on the way
//! back up. And staleness is decided by whoever reads, not by a sweep: it is
//! what lets the registry notice that *most* of itself went quiet at once and
//! conclude that the network broke rather than that the estate died. See
//! [`registry`].

pub mod client;
pub mod eureka;
#[cfg(feature = "gateway")]
pub mod gateway;
pub mod instance;
pub mod registry;
pub mod replicate;
pub mod resolve;
pub mod server;
pub mod ui;

pub use client::{Discovery, Registrar};
#[cfg(feature = "gateway")]
pub use gateway::Balancer;
pub use instance::{Instance, Status};
pub use registry::{Mode, Registry};
pub use replicate::{Change, Peers};
pub use resolve::{Balance, Dns, Resolve, RoundRobin, Static};
pub use server::RegistryServer;
