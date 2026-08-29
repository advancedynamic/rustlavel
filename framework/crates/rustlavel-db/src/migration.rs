//! Migrations and seeding.
//!
//! Laravel finds migrations by scanning a directory at runtime. A compiled
//! language cannot, so the CLI generates a registry that lists them and the
//! application passes it in — the developer never edits that file by hand, and
//! the experience is the same.

use crate::schema::Schema;
use crate::{Database, Value};
use rustlavel_core::Result;

/// Where applied migrations are recorded, unless a `Migrator` says otherwise.
pub const DEFAULT_TABLE: &str = "rustlavel_migrations";

/// One migration.
///
/// `name` must be unique and sortable — the generator produces
/// `2026_08_29_000001_create_users_table`, so lexical order is time order.
pub trait Migration: Send + Sync {
    fn name(&self) -> &'static str;

    /// Apply the change.
    fn up<'a>(
        &'a self,
        schema: &'a Schema<'a>,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;

    /// Undo it. A migration that cannot be undone should say so by returning an
    /// error, rather than silently doing nothing.
    fn down<'a>(
        &'a self,
        schema: &'a Schema<'a>,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;
}

/// Define a migration without writing the pinned-future boilerplate.
///
/// ```ignore
/// migration!(
///     CreateUsersTable,
///     "2026_08_29_000001_create_users_table",
///     up: |schema| {
///         schema.create("users", |t| { t.id(); t.string("name"); }).await
///     },
///     down: |schema| { schema.drop("users").await },
/// );
/// ```
///
/// The bodies are blocks rather than closures on purpose: a closure returning
/// a future that borrows its argument cannot express the lifetime that
/// relationship needs, and the error it produces is unreadable.
#[macro_export]
macro_rules! migration {
    (
        $type:ident,
        $name:literal,
        up: |$up_schema:ident| $up:block,
        down: |$down_schema:ident| $down:block $(,)?
    ) => {
        pub struct $type;

        impl $crate::migration::Migration for $type {
            fn name(&self) -> &'static str {
                $name
            }

            fn up<'a>(
                &'a self,
                schema: &'a $crate::schema::Schema<'a>,
            ) -> ::std::pin::Pin<
                ::std::boxed::Box<dyn ::std::future::Future<Output = $crate::Result<()>> + Send + 'a>,
            > {
                let $up_schema = schema;
                ::std::boxed::Box::pin(async move { $up })
            }

            fn down<'a>(
                &'a self,
                schema: &'a $crate::schema::Schema<'a>,
            ) -> ::std::pin::Pin<
                ::std::boxed::Box<dyn ::std::future::Future<Output = $crate::Result<()>> + Send + 'a>,
            > {
                let $down_schema = schema;
                ::std::boxed::Box::pin(async move { $down })
            }
        }
    };
}

/// What a migration run did, so the CLI can report it.
#[derive(Debug, Default, PartialEq)]
pub struct MigrationReport {
    pub applied: Vec<String>,
    pub rolled_back: Vec<String>,
    pub skipped: usize,
}

/// Applies and rolls back migrations, tracking which have run.
pub struct Migrator<'a> {
    db: &'a Database,
    migrations: Vec<&'a dyn Migration>,
    /// Where applied migrations are recorded.
    ///
    /// Configurable so two suites can share one database without rolling back
    /// each other's batches — which is exactly what happened the first time the
    /// framework's own tests ran side by side.
    table: String,
}

impl<'a> Migrator<'a> {
    pub fn new(db: &'a Database, migrations: Vec<&'a dyn Migration>) -> Self {
        Migrator { db, migrations, table: DEFAULT_TABLE.to_string() }
    }

    /// Record applied migrations in a different table.
    pub fn with_table(mut self, table: &str) -> Result<Self> {
        crate::validate_identifier(table)?;
        self.table = table.to_string();
        Ok(self)
    }

    pub fn table(&self) -> &str {
        &self.table
    }

    /// Create the tracking table if it is not there yet.
    ///
    /// The batch number is what makes `migrate:rollback` undo one deployment's
    /// worth of migrations rather than one migration.
    pub async fn prepare(&self) -> Result<()> {
        self.db
            .run(&format!(
                "create table if not exists \"{}\" (\n  \
                 id bigserial primary key,\n  \
                 name varchar(255) not null unique,\n  \
                 batch integer not null,\n  \
                 ran_at timestamptz not null default now()\n)",
                self.table
            ))
            .await?;
        Ok(())
    }

    /// Names already applied, in the order they ran.
    pub async fn applied(&self) -> Result<Vec<String>> {
        let rows = self
            .db
            .select(&format!("select name from \"{}\" order by id", self.table), &[])
            .await?;
        rows.iter().map(|row| row.get::<String>("name")).collect()
    }

    /// Migrations that have not run yet.
    pub async fn pending(&self) -> Result<Vec<&'a dyn Migration>> {
        let applied = self.applied().await?;
        Ok(self
            .migrations
            .iter()
            .filter(|migration| !applied.iter().any(|name| name == migration.name()))
            .copied()
            .collect())
    }

    async fn next_batch(&self) -> Result<i64> {
        let highest = self
            .db
            .scalar::<Option<i64>>(&format!("select max(batch) from \"{}\"", self.table), &[])
            .await?
            .flatten();
        Ok(highest.unwrap_or(0) + 1)
    }

    /// Run every pending migration.
    ///
    /// Each runs in its own transaction, so a failure halfway leaves the
    /// database on a migration boundary rather than inside one.
    pub async fn run(&self) -> Result<MigrationReport> {
        self.prepare().await?;

        let pending = self.pending().await?;
        let batch = self.next_batch().await?;
        let mut report = MigrationReport {
            skipped: self.migrations.len() - pending.len(),
            ..MigrationReport::default()
        };

        for migration in pending {
            let schema = Schema::new(self.db);

            self.db.run("begin").await?;
            match migration.up(&schema).await {
                Ok(()) => {
                    self.db
                        .execute(
                            &format!(
                                "insert into \"{}\" (name, batch) values ($1, $2)",
                                self.table
                            ),
                            &[Value::from(migration.name()), Value::from(batch)],
                        )
                        .await?;
                    self.db.run("commit").await?;
                    report.applied.push(migration.name().to_string());
                    rustlavel_core::info!("migrated: {}", migration.name());
                }
                Err(error) => {
                    self.db.run("rollback").await?;
                    return Err(rustlavel_core::Error::msg(format!(
                        "migration `{}` failed and was rolled back: {error}",
                        migration.name()
                    )));
                }
            }
        }

        Ok(report)
    }

    /// Roll back the most recent batch.
    pub async fn rollback(&self) -> Result<MigrationReport> {
        self.prepare().await?;

        let batch = self
            .db
            .scalar::<Option<i64>>(&format!("select max(batch) from \"{}\"", self.table), &[])
            .await?
            .flatten();

        let Some(batch) = batch else { return Ok(MigrationReport::default()) };

        let rows = self
            .db
            .select(
                &format!(
                    "select name from \"{}\" where batch = $1 order by id desc",
                    self.table
                ),
                &[Value::from(batch)],
            )
            .await?;

        let mut report = MigrationReport::default();

        for row in rows {
            let name = row.get::<String>("name")?;
            let Some(migration) = self.migrations.iter().find(|m| m.name() == name) else {
                return Err(rustlavel_core::Error::msg(format!(
                    "cannot roll back `{name}`: it is recorded as applied but is not in the \
                     migration registry. Did the file get deleted?"
                )));
            };

            let schema = Schema::new(self.db);
            self.db.run("begin").await?;
            match migration.down(&schema).await {
                Ok(()) => {
                    self.db
                        .execute(
                            &format!("delete from \"{}\" where name = $1", self.table),
                            &[Value::from(name.as_str())],
                        )
                        .await?;
                    self.db.run("commit").await?;
                    report.rolled_back.push(name.clone());
                    rustlavel_core::info!("rolled back: {name}");
                }
                Err(error) => {
                    self.db.run("rollback").await?;
                    return Err(rustlavel_core::Error::msg(format!(
                        "rolling back `{name}` failed: {error}"
                    )));
                }
            }
        }

        Ok(report)
    }

    /// Drop every table in the schema, then migrate from scratch.
    ///
    /// Refuses to run in production: this is the command that would delete a
    /// live database, and a confirmation prompt is not available to a library.
    pub async fn fresh(&self, environment: &str) -> Result<MigrationReport> {
        if environment == "production" {
            return Err(rustlavel_core::Error::msg(
                "migrate:fresh drops every table and is refused in production. \
                 Use migrate, or set APP_ENV to something else if this really is a scratch database."
                    .to_string(),
            ));
        }

        self.db
            .run(
                "do $$ declare r record; begin \
                 for r in (select tablename from pg_tables where schemaname = current_schema()) loop \
                 execute 'drop table if exists ' || quote_ident(r.tablename) || ' cascade'; \
                 end loop; end $$",
            )
            .await?;

        self.run().await
    }

    /// Which migrations have run and which have not.
    pub async fn status(&self) -> Result<Vec<(String, bool)>> {
        self.prepare().await?;
        let applied = self.applied().await?;

        Ok(self
            .migrations
            .iter()
            .map(|migration| {
                let name = migration.name().to_string();
                let has_run = applied.contains(&name);
                (name, has_run)
            })
            .collect())
    }
}

/// One seeder.
pub trait Seeder: Send + Sync {
    fn name(&self) -> &'static str;

    fn run<'a>(
        &'a self,
        db: &'a Database,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;
}

/// Run seeders in order.
pub async fn seed(db: &Database, seeders: &[&dyn Seeder]) -> Result<Vec<String>> {
    let mut ran = Vec::new();

    for seeder in seeders {
        seeder.run(db).await.map_err(|e| {
            rustlavel_core::Error::msg(format!("seeder `{}` failed: {e}", seeder.name()))
        })?;
        rustlavel_core::info!("seeded: {}", seeder.name());
        ran.push(seeder.name().to_string());
    }

    Ok(ran)
}

/// Pluralize an English noun for a table name.
///
/// Duplicated from the CLI's naming module on purpose: the CLI is a separate
/// binary that applications do not depend on, and a foreign key needs this at
/// runtime.
pub fn pluralize(word: &str) -> String {
    let lower = word.to_lowercase();

    for (singular, plural) in [
        ("person", "people"),
        ("child", "children"),
        ("man", "men"),
        ("woman", "women"),
        ("tooth", "teeth"),
        ("foot", "feet"),
        ("mouse", "mice"),
        ("goose", "geese"),
    ] {
        if lower.ends_with(singular) {
            return format!("{}{plural}", &word[..word.len() - singular.len()]);
        }
    }

    if lower.ends_with('s') && !lower.ends_with("us") && !lower.ends_with("ss") {
        return word.to_string();
    }
    if let Some(stem) = lower.strip_suffix('y')
        && !stem.ends_with(['a', 'e', 'i', 'o', 'u']) {
            return format!("{}ies", &word[..word.len() - 1]);
        }
    if lower.ends_with(['s', 'x', 'z']) || lower.ends_with("ch") || lower.ends_with("sh") {
        return format!("{word}es");
    }
    format!("{word}s")
}

/// A tiny deterministic generator for factories and seeders.
///
/// Deterministic on purpose: a seeded database that differs run to run makes a
/// failing test impossible to reproduce.
pub struct Faker {
    state: u64,
}

impl Faker {
    pub fn new(seed: u64) -> Self {
        Faker { state: seed.max(1) }
    }

    fn next(&mut self) -> u64 {
        // xorshift64: small, fast, and repeatable.
        self.state ^= self.state << 13;
        self.state ^= self.state >> 7;
        self.state ^= self.state << 17;
        self.state
    }

    pub fn number(&mut self, low: i64, high: i64) -> i64 {
        if high <= low {
            return low;
        }
        low + (self.next() % (high - low + 1) as u64) as i64
    }

    pub fn boolean(&mut self) -> bool {
        self.next().is_multiple_of(2)
    }

    pub fn pick<'a, T>(&mut self, options: &'a [T]) -> &'a T {
        &options[(self.next() % options.len() as u64) as usize]
    }

    pub fn name(&mut self) -> String {
        const FIRST: &[&str] = &[
            "Ada", "Grace", "Alan", "Linus", "Barbara", "Ken", "Margaret", "Dennis", "Radia", "Guido",
        ];
        const LAST: &[&str] = &[
            "Lovelace", "Hopper", "Turing", "Torvalds", "Liskov", "Thompson", "Hamilton", "Ritchie",
            "Perlman", "Rossum",
        ];
        format!("{} {}", self.pick(FIRST), self.pick(LAST))
    }

    pub fn email(&mut self) -> String {
        let name = self.name().to_lowercase().replace(' ', ".");
        format!("{name}{}@example.com", self.number(1, 9999))
    }

    pub fn sentence(&mut self) -> String {
        const WORDS: &[&str] = &[
            "rust", "framework", "query", "handler", "migration", "route", "template", "record",
            "worker", "cache",
        ];
        let count = self.number(4, 9) as usize;
        let mut words: Vec<String> = (0..count).map(|_| self.pick(WORDS).to_string()).collect();
        words[0] = {
            let first = &words[0];
            let mut chars = first.chars();
            match chars.next() {
                Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        };
        format!("{}.", words.join(" "))
    }

    pub fn slug(&mut self) -> String {
        self.sentence().trim_end_matches('.').to_lowercase().replace(' ', "-")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pluralizes_for_foreign_keys() {
        assert_eq!(pluralize("user"), "users");
        assert_eq!(pluralize("category"), "categories");
        assert_eq!(pluralize("person"), "people");
        assert_eq!(pluralize("status"), "statuses");
    }

    #[test]
    fn the_faker_is_reproducible() {
        let mut first = Faker::new(42);
        let mut second = Faker::new(42);

        assert_eq!(first.name(), second.name());
        assert_eq!(first.email(), second.email());
        assert_eq!(first.number(1, 100), second.number(1, 100));
    }

    #[test]
    fn different_seeds_diverge() {
        assert_ne!(Faker::new(1).sentence(), Faker::new(2).sentence());
    }

    #[test]
    fn faker_numbers_stay_in_range() {
        let mut faker = Faker::new(7);
        for _ in 0..200 {
            let value = faker.number(5, 10);
            assert!((5..=10).contains(&value), "{value} out of range");
        }
    }
}
