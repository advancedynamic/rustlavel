//! The service registry, seen from the administration area.
//!
//! **This reads a registry; it is not one.** The page asks whatever
//! `discovery.url` points at for its registry document and draws it. Running
//! the registry inside the application that administers it would mean the
//! dashboard goes down with the thing it is there to tell you about, which is
//! precisely when somebody opens it.
//!
//! The whole feature is this file, `controller.rs` and one view. Delete the
//! three, take the entry out of `modules/mod.rs`, and nothing else in the
//! application notices.

pub mod controller;

use rustlavel::prelude::*;
use rustlavel::{Plugin, Setup};

use crate::modules::Module;
use crate::support::settings::{Kind, Setting, env};

pub use controller::DiscoveryController;

/// Where the registry answers. The default is a registry on this machine,
/// which is what somebody trying it out has running.
static SETTINGS: [Setting; 1] =
    [env("discovery.url", Kind::Text, "http://127.0.0.1:8761", "DISCOVERY_URL")];

/// One permission. There is nothing to change from this screen — it is a
/// window onto somebody else's state — so there is no second verb to guard.
static PERMISSIONS: [(&str, &str); 1] =
    [("discovery.view", "See the service registry and which instances are up")];

pub struct Discovery;

impl Plugin for Discovery {
    fn name(&self) -> &'static str {
        "discovery"
    }

    fn register(self: Box<Self>, setup: &mut Setup<'_>) {
        // The same guards the rest of `/admin` carries. A module registers on
        // the bare router and inherits nothing, so it names them itself.
        setup.router.group("/admin/discovery", |discovery| {
            discovery.middleware(Authenticate::default().login_path("/login"));
            discovery.middleware(crate::support::epoch::SessionEpoch);
            discovery.middleware(crate::support::idle::IdleTimeout);

            discovery
                .get("", DiscoveryController::index)
                .name("admin.discovery")
                .middleware(Can::permission("discovery.view").login_path("/login"));
        });
    }
}

impl Module for Discovery {
    fn permissions(&self) -> &'static [(&'static str, &'static str)] {
        &PERMISSIONS
    }

    fn settings(&self) -> &'static [Setting] {
        &SETTINGS
    }
}
