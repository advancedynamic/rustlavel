#![allow(elided_lifetimes_in_paths)]
#![allow(clippy::wildcard_imports)]
pub use sea_orm_migration::prelude::*;

pub struct Migrator;

// The benchmark fixture (`benchmarks/schema.sql`) is created and seeded outside
// the application, and every app under `apps/` reads the same tables. There is
// therefore nothing for the migrator to do — but `loco_rs::cli::main` and
// `create_app` are both generic over a `MigratorTrait`, so the type still has
// to exist. It carries no migrations.
#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            // inject-above (do not remove this comment)
        ]
    }
}
