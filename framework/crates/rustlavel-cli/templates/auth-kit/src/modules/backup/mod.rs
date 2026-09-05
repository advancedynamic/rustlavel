//! Taking a copy of the database, and putting one back.
//!
//! The first feature to move out of the layered directories and into one of
//! its own, chosen because nothing points at it: no other module reads it, so
//! the move could not break anything else. Everything it needs is here — the
//! dump format, the schedule arithmetic, the controller, its permissions and
//! its settings — and `mod.rs` is the whole of what the application has to
//! know about it.

pub mod archive;
pub mod controller;
pub mod schedule;

use rustlavel::prelude::*;
use rustlavel::{Plugin, Setup};

use crate::modules::Module;
use crate::support::settings::{Kind, Setting, choice, env};

pub use controller::BackupController;

/// The same guard the rest of the administration area uses.
fn guard(permission: &str) -> Can {
    Can::permission(permission).login_path("/login")
}

/// How often, if at all. `disabled` is the default and means never.
const SCHEDULES: &[(&str, &str)] = &[
    ("disabled", "Never"),
    ("daily", "Every day"),
    ("weekly", "Every week"),
    ("monthly", "Every month"),
];

/// How many to keep. Zero is all of them.
const RETENTIONS: &[(&str, &str)] = &[
    ("0", "Keep every backup"),
    ("3", "Keep the last 3"),
    ("7", "Keep the last 7"),
    ("14", "Keep the last 14"),
    ("30", "Keep the last 30"),
];

/// The settings this feature reads, beside the code that reads them.
static SETTINGS: [Setting; 3] = [
    choice("backup.schedule", "disabled", SCHEDULES),
    choice("backup.retention", "0", RETENTIONS),
    env("backup.path", Kind::Text, "storage/backups", "BACKUP_PATH"),
];

/// One permission per verb, so a role can be given the read half without the
/// half that deletes.
static PERMISSIONS: [(&str, &str); 4] = [
    ("backups.view", "See the list of database backups"),
    ("backups.create", "Take a backup"),
    ("backups.restore", "Restore the database from a backup"),
    ("backups.delete", "Delete a backup"),
];

pub struct Backup;

impl Plugin for Backup {
    fn name(&self) -> &'static str {
        "backup"
    }

    fn register(self: Box<Self>, setup: &mut Setup<'_>) {
        // **The group's middleware comes with the routes.** These lived inside
        // `r.group("/admin", ...)`, which applied `Authenticate` and
        // `IdleTimeout` to everything in it. A module registering on the bare
        // router inherits neither, so a move that forgot them would have put
        // four unauthenticated routes into the application — one of which
        // restores the database. A module owns its routes, and owning them
        // means owning what guards them.
        setup.router.group("/admin/settings/backup", |backup| {
            backup.middleware(Authenticate::default().login_path("/login"));
            backup.middleware(crate::support::epoch::SessionEpoch);
            backup.middleware(crate::support::idle::IdleTimeout);

            // One permission per verb, so a role can be given the read half
            // without the half that deletes.
            backup.post("/create", BackupController::store).middleware(guard("backups.create"));
            backup.get("/{id}/download", BackupController::download).middleware(guard("backups.view"));
            backup.post("/{id}/restore", BackupController::restore).middleware(guard("backups.restore"));
            backup.post("/{id}/delete", BackupController::destroy).middleware(guard("backups.delete"));
        });
    }
}

impl Module for Backup {
    fn permissions(&self) -> &'static [(&'static str, &'static str)] {
        &PERMISSIONS
    }

    fn settings(&self) -> &'static [Setting] {
        &SETTINGS
    }
}
