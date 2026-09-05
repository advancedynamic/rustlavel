//! The application's features, one directory each.
//!
//! A module owns everything one feature needs: its routes, its controller, its
//! migrations, the permissions that guard it, and the settings that configure
//! it. The alternative — a directory per technical layer, every controller
//! together and every model together — is what this application had, and the
//! cost of it is that changing one feature means opening six files in four
//! directories and nothing in the tree says those six belong together.
//!
//! **Nothing is discovered.** `all()` below is a hand-written list, the way
//! `main.rs` is. A module that registers itself by existing is a module whose
//! registration nobody can find.
//!
//! Modules may use `support` and each other only downward and only where the
//! edge is written down. Today there are none: `backup` uses `support` and
//! nothing else uses `backup`, which is why it went first.

pub mod backup;

use rustlavel::Plugin;

/// A feature, and everything it needs to work.
///
/// Extends `Plugin`, which already carries routes, middleware and state. The
/// four below are the parts a plugin cannot register today because the
/// application collects them centrally — so they are collected from here
/// instead, and `main.rs` flattens the list rather than naming each feature
/// four times.
pub trait Module: Plugin {
    /// Tables this feature owns. Run in the order the modules are listed.
    fn migrations(&self) -> Vec<&'static dyn rustlavel::db::Migration> {
        Vec::new()
    }

    /// Rows this feature needs before it can be used.
    fn seeders(&self) -> Vec<&'static dyn rustlavel::db::Seeder> {
        Vec::new()
    }

    /// What may be granted, as `(name, description)`.
    fn permissions(&self) -> &'static [(&'static str, &'static str)] {
        &[]
    }

    /// Settings this feature reads, declared beside the code that reads them
    /// rather than in one list of every key the application has.
    fn settings(&self) -> &'static [crate::support::settings::Setting] {
        &[]
    }
}

/// Every module, in the order they are registered and migrated.
pub fn all() -> Vec<Box<dyn Module>> {
    vec![Box::new(backup::Backup)]
}

/// Every migration the modules own, flattened for `App::migrations`.
pub fn migrations() -> Vec<&'static dyn rustlavel::db::Migration> {
    all().iter().flat_map(|module| module.migrations()).collect()
}

/// The migrations belonging to the named modules, and no others.
///
/// For an application where each tenant enables a different subset: one
/// company runs accounting and finance and no HR, a subsidiary runs only
/// sales. Provisioning that tenant means creating its database and running
/// *these* migrations into it — a tenant that never enabled HR must not be
/// given HR's tables.
///
/// ```no_run
/// # use rustlavel::db::{Database, Migrator};
/// # // Named rather than `crate::`: a doctest is its own crate, linking this
/// # // one by name, and the placeholder becomes that name when the project is
/// # // scaffolded.
/// # async fn provision(tenant: &Database, enabled: &[&str]) -> rustlavel::Result<()> {
/// let migrator = Migrator::new(tenant, {{crate_name}}::modules::migrations_for(enabled));
/// migrator.prepare().await?;
/// migrator.run().await?;
/// # Ok(())
/// # }
/// ```
///
/// A name that matches no module contributes nothing rather than failing: the
/// list usually comes from a database column, and one stale row should not stop
/// a tenant being provisioned. Check the names against `all()` where they are
/// *set*, which is where somebody can still do something about a typo.
pub fn migrations_for(names: &[&str]) -> Vec<&'static dyn rustlavel::db::Migration> {
    all()
        .iter()
        .filter(|module| names.contains(&module.name()))
        .flat_map(|module| module.migrations())
        .collect()
}

/// Every module's name, for a screen that offers them.
pub fn names() -> Vec<&'static str> {
    all().iter().map(|module| module.name()).collect()
}

/// Every seeder the modules own.
pub fn seeders() -> Vec<&'static dyn rustlavel::db::Seeder> {
    all().iter().flat_map(|module| module.seeders()).collect()
}

/// Every permission the modules declare, for the seeder that creates them.
pub fn permissions() -> Vec<(&'static str, &'static str)> {
    all().iter().flat_map(|module| module.permissions().iter().copied()).collect()
}

/// Every setting the modules declare, for the catalogue.
pub fn settings() -> Vec<crate::support::settings::Setting> {
    all().iter().flat_map(|module| module.settings().iter().copied()).collect()
}
