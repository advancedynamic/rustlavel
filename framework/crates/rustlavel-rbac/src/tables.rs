//! The five tables, and the migration that creates them.
//!
//! Written entirely through the schema builder, so the same definition is
//! correct on PostgreSQL, MySQL and SQL Server. Nothing here emits SQL of its
//! own.
//!
//! ```ignore
//! // in the generated migration registry
//! let mut registry: Vec<&'static dyn Migration> = vec![];
//! registry.extend(rustlavel_rbac::migrations());
//! ```

use rustlavel_db::migration::Migration;
use rustlavel_db::schema::{Schema, Table};
use rustlavel_core::Result;

/// The conventional table names, matching what a Laravel developer expects
/// from `laravel-permission` closely enough to be recognisable.
pub const ROLES: &str = "roles";
pub const PERMISSIONS: &str = "permissions";
pub const ROLE_PERMISSION: &str = "role_permission";
pub const USER_ROLE: &str = "user_role";
pub const USER_PERMISSION: &str = "user_permission";

/// Where the five tables live.
///
/// Configurable for two reasons. An application may already own a `permissions`
/// table and not want this one on top of it; and the framework's own
/// integration tests run concurrently against one database, so each needs a set
/// of its own — see [`TableNames::suffixed`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableNames {
    pub roles: String,
    pub permissions: String,
    pub role_permission: String,
    pub user_role: String,
    pub user_permission: String,
}

impl Default for TableNames {
    fn default() -> Self {
        TableNames {
            roles: ROLES.to_string(),
            permissions: PERMISSIONS.to_string(),
            role_permission: ROLE_PERMISSION.to_string(),
            user_role: USER_ROLE.to_string(),
            user_permission: USER_PERMISSION.to_string(),
        }
    }
}

impl TableNames {
    /// The conventional names with a suffix on each: `roles_a1`, `user_role_a1`.
    ///
    /// One test, one suffix. Two tests sharing a table would pass or fail
    /// depending on which finished first.
    pub fn suffixed(suffix: &str) -> Self {
        let base = TableNames::default();
        TableNames {
            roles: format!("{}{suffix}", base.roles),
            permissions: format!("{}{suffix}", base.permissions),
            role_permission: format!("{}{suffix}", base.role_permission),
            user_role: format!("{}{suffix}", base.user_role),
            user_permission: format!("{}{suffix}", base.user_permission),
        }
    }

    /// Check every name is a plain identifier.
    ///
    /// Done once, when a store is built, rather than on every statement: a
    /// table name is interpolated into SQL and can never be a bound parameter.
    pub fn validate(&self) -> Result<()> {
        for name in self.all() {
            rustlavel_db::validate_identifier(name)?;
        }
        Ok(())
    }

    /// All five, in dependency order: parents before the tables pointing at
    /// them.
    pub fn all(&self) -> [&str; 5] {
        [
            &self.roles,
            &self.permissions,
            &self.role_permission,
            &self.user_role,
            &self.user_permission,
        ]
    }
}

/// Create the five tables.
///
/// Shared by [`CreateRbacTables`] and by [`crate::Permissions::migrate`], so an
/// application that would rather not register a migration still gets exactly
/// the schema production has.
pub async fn create_tables(schema: &Schema<'_>, names: &TableNames) -> Result<()> {
    names.validate()?;

    schema.create(&names.roles, define_named_table).await?;
    schema.create(&names.permissions, define_named_table).await?;

    // `foreign_id("role")` would be shorter, but it derives the parent table by
    // pluralising the name it is given, and these parents are configurable — so
    // the reference is spelled out. `.index()` is included by hand for the same
    // reason: it comes free with `foreign_id` and not with `references`.
    let roles = names.roles.clone();
    let permissions = names.permissions.clone();
    schema
        .create(&names.role_permission, move |t: &mut Table| {
            t.id();
            t.big_integer("role_id").references(&roles, "id").cascade_on_delete().index();
            t.big_integer("permission_id")
                .references(&permissions, "id")
                .cascade_on_delete()
                .index();
            t.timestamps();
            // Attaching the same permission twice is not an error worth an
            // error message; it is a row the database should refuse to hold.
            t.unique(&["role_id", "permission_id"]);
        })
        .await?;

    let roles = names.roles.clone();
    schema
        .create(&names.user_role, move |t: &mut Table| {
            // No foreign key to a users table, deliberately. This crate does
            // not know what an application calls its users, whether they live
            // in this database at all, or whether the id is even a row key
            // rather than an identifier from an upstream directory. The price
            // is that deleting a user leaves rows behind, so the store offers
            // `purge_user` to clean up after one.
            t.big_integer("user_id").index();
            t.big_integer("role_id").references(&roles, "id").cascade_on_delete().index();
            t.timestamps();
            t.unique(&["user_id", "role_id"]);
        })
        .await?;

    let permissions = names.permissions.clone();
    schema
        .create(&names.user_permission, move |t: &mut Table| {
            t.big_integer("user_id").index();
            t.big_integer("permission_id")
                .references(&permissions, "id")
                .cascade_on_delete()
                .index();
            // The column that makes a direct entry able to *revoke* something a
            // role grants. Without it a direct permission could only ever add,
            // and "everyone in support may refund, except this one person"
            // would have to be modelled by inventing a role for that person.
            t.boolean("granted").default_bool(true);
            t.timestamps();
            t.unique(&["user_id", "permission_id"]);
        })
        .await?;

    Ok(())
}

/// `roles` and `permissions` have the same shape, so they share a definition.
fn define_named_table(t: &mut Table) {
    t.id();
    t.string("name").unique();
    // What this role or permission is for, in words. An admin screen that can
    // only show `billing.refund` makes the person granting it guess.
    t.text("description").nullable();
    t.timestamps();
}

/// Drop the five, children first so the foreign keys do not object.
pub async fn drop_tables(schema: &Schema<'_>, names: &TableNames) -> Result<()> {
    names.validate()?;

    for table in [
        &names.user_permission,
        &names.user_role,
        &names.role_permission,
        &names.permissions,
        &names.roles,
    ] {
        schema.drop(table).await?;
    }
    Ok(())
}

rustlavel_db::migration!(
    CreateRbacTables,
    "2026_09_02_000001_create_rbac_tables",
    up: |schema| { crate::tables::create_tables(schema, &TableNames::default()).await },
    down: |schema| { crate::tables::drop_tables(schema, &TableNames::default()).await },
);

/// The migrations this package needs, for the application's registry.
///
/// A `Vec` of one today. It is a `Vec` because a later release that adds a
/// column must add a second migration rather than edit this one — an applied
/// migration is history, and history does not get rewritten under a database
/// that has already run it.
pub fn migrations() -> Vec<&'static dyn Migration> {
    vec![&CreateRbacTables]
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustlavel_db::dialect::{MySql, Postgres, SqlServer};
    use rustlavel_db::schema::create_statements;

    /// Render one table's DDL for one database, without a database.
    fn ddl(dialect: &dyn rustlavel_db::Dialect, table: &str, define: fn(&mut Table)) -> String {
        create_statements(dialect, table, define).expect("valid definition").join(";\n")
    }

    #[test]
    fn the_default_names_are_the_conventional_ones() {
        let names = TableNames::default();

        assert_eq!(
            names.all(),
            ["roles", "permissions", "role_permission", "user_role", "user_permission"]
        );
        assert!(names.validate().is_ok());
    }

    #[test]
    fn a_suffix_reaches_every_table() {
        let names = TableNames::suffixed("_t9");

        assert_eq!(
            names.all(),
            [
                "roles_t9",
                "permissions_t9",
                "role_permission_t9",
                "user_role_t9",
                "user_permission_t9"
            ]
        );
        assert!(names.validate().is_ok());
    }

    #[test]
    fn a_table_name_that_is_not_an_identifier_is_refused() {
        let names =
            TableNames { roles: "roles; drop table users".to_string(), ..TableNames::default() };

        let error = names.validate().expect_err("a name carrying a statement must be refused");
        assert!(error.to_string().contains("not a valid SQL identifier"), "{error}");
    }

    #[test]
    fn the_named_tables_render_on_all_three_databases() {
        for dialect in [&Postgres as &dyn rustlavel_db::Dialect, &MySql, &SqlServer] {
            let sql = ddl(dialect, "roles", define_named_table);

            assert!(sql.contains("create table"), "{}: {sql}", dialect.name());
            assert!(sql.to_lowercase().contains("unique"), "{}: {sql}", dialect.name());
            assert!(sql.contains("description"), "{}: {sql}", dialect.name());
        }
    }

    #[test]
    fn the_granted_column_is_a_boolean_defaulting_to_true() {
        // MySQL and SQL Server have no `true` literal, so the default has to
        // come out as `1` there. Getting this wrong is a migration that fails
        // on two databases out of three and only in production.
        let define = |t: &mut Table| {
            t.boolean("granted").default_bool(true);
        };

        let postgres = ddl(&Postgres, "user_permission", define);
        assert!(postgres.contains("default true"), "{postgres}");

        for dialect in [&MySql as &dyn rustlavel_db::Dialect, &SqlServer] {
            let sql = ddl(dialect, "user_permission", define);
            assert!(sql.contains("default 1"), "{}: {sql}", dialect.name());
        }
    }

    #[test]
    fn the_migration_is_named_the_way_the_cli_names_them() {
        let name = CreateRbacTables.name();

        assert_eq!(name, "2026_09_02_000001_create_rbac_tables");
        let (date, rest) = name.split_at(11);
        assert!(date.chars().all(|c| c.is_ascii_digit() || c == '_'), "{date}");
        assert!(rest.starts_with("000001_"), "{rest}");
    }

    #[test]
    fn the_registry_exposes_every_migration() {
        let registry = migrations();

        assert_eq!(registry.len(), 1);
        assert_eq!(registry[0].name(), "2026_09_02_000001_create_rbac_tables");
    }
}
