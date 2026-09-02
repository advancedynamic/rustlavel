//! The store against a real database.
//!
//! The unit tests prove the precedence rules and the matcher. These prove that
//! the five tables can actually be created on a server, that the joins return
//! what they claim to, and that a cascade really does take the rows with it —
//! none of which a test with no database can tell you.
//!
//! They run only when `DATABASE_URL` is set, so `cargo test` stays green on a
//! machine with no database:
//!
//! ```text
//! docker run -d --name rustlavel-rbac-pg -e POSTGRES_PASSWORD=secret \
//!   -e POSTGRES_USER=rustlavel -e POSTGRES_DB=rustlavel_test \
//!   -p 5432:5432 postgres:16-alpine
//! export DATABASE_URL='postgres://rustlavel:secret@127.0.0.1:5432/rustlavel_test'
//! cargo test -p rustlavel-rbac
//! ```
//!
//! Nothing here is PostgreSQL-specific — every statement comes from the query
//! and schema builders — so the same suite is worth pointing at the other two,
//! and it has been. All fifteen pass unchanged against PostgreSQL 16, MySQL 8.4
//! and Azure SQL Edge:
//!
//! ```text
//! docker run -d --name rustlavel-rbac-mysql -e MYSQL_ROOT_PASSWORD=secret \
//!   -e MYSQL_DATABASE=rustlavel_test -e MYSQL_USER=rustlavel -e MYSQL_PASSWORD=secret \
//!   -p 33306:3306 mysql:8.4
//! export DATABASE_URL='mysql://rustlavel:secret@127.0.0.1:33306/rustlavel_test'
//!
//! docker run -d --name rustlavel-rbac-mssql -e ACCEPT_EULA=1 \
//!   -e 'MSSQL_SA_PASSWORD=Rustlavel!2026' -p 51433:1433 \
//!   mcr.microsoft.com/azure-sql-edge:latest
//! export DATABASE_URL='sqlserver://sa:Rustlavel!2026@127.0.0.1:51433/master'
//! ```
//!
//! That is what caught the one genuinely portable-looking bug in this crate:
//! `granted` is a `boolean` on PostgreSQL and a number on the other two, so
//! reading it back with `row.get::<bool>()` works on exactly one database.
//!
//! Every test owns its own five tables, named with a suffix nothing else uses,
//! because the suite runs concurrently and two tests sharing a `roles` table
//! would pass or fail depending on which finished first.

use rustlavel_db::migration::Migrator;
use rustlavel_db::{Database, Value};
use rustlavel_rbac::{Permissions, TableNames, migrations};
use std::time::Duration;

/// The database URL, or a skip.
macro_rules! database_url {
    () => {
        match std::env::var("DATABASE_URL") {
            Ok(url) if !url.is_empty() => url,
            _ => {
                eprintln!("skipping: DATABASE_URL is not set");
                return;
            }
        }
    };
}

/// A store on tables named for this test alone, created from scratch.
///
/// Dropped first as well as created, so a run that panicked half way through
/// last time does not poison the next one.
async fn fresh(url: &str, suffix: &str) -> Permissions {
    let db = Database::connect(url).await.expect("DATABASE_URL should be reachable");
    let store = Permissions::with_tables(db, TableNames::suffixed(suffix)).expect("valid names");

    store.drop_tables().await.expect("dropping leftovers");
    store.migrate().await.expect("creating the tables");
    store
}

/// A second handle onto the same tables, with a cache of its own.
///
/// How a test changes the data behind another handle's back — which is exactly
/// what a second process does, and the only way to see whether the cache is
/// really a cache.
fn other_handle(store: &Permissions) -> Permissions {
    Permissions::with_tables(store.database().clone(), store.tables().clone())
        .expect("valid names")
}

#[tokio::test]
async fn roles_and_permissions_are_created_listed_renamed_and_deleted() {
    let url = database_url!();
    let store = fresh(&url, "_crud").await;

    let editor = store.create_role_with("editor", "May write posts").await.unwrap();
    store.create_role("reviewer").await.unwrap();
    store.create_permission("posts.create").await.unwrap();
    store.create_permission("posts.publish").await.unwrap();

    assert!(editor.id > 0);
    assert_eq!(editor.description.as_deref(), Some("May write posts"));

    let names: Vec<String> = store.roles().await.unwrap().into_iter().map(|r| r.name).collect();
    assert_eq!(names, ["editor", "reviewer"]);

    let names: Vec<String> =
        store.permissions().await.unwrap().into_iter().map(|p| p.name).collect();
    assert_eq!(names, ["posts.create", "posts.publish"]);

    // A role with no description reads back as one, not as an empty string.
    assert_eq!(store.find_role("reviewer").await.unwrap().unwrap().description, None);

    // Names are unique, and the message says so rather than leaking a
    // constraint violation from the wire protocol.
    let error = store.create_role("editor").await.unwrap_err().to_string();
    assert!(error.contains("already exists"), "{error}");

    store.rename_role("reviewer", "approver").await.unwrap();
    assert!(store.find_role("reviewer").await.unwrap().is_none());
    assert!(store.find_role("approver").await.unwrap().is_some());

    store.delete_role("approver").await.unwrap();
    assert!(store.find_role("approver").await.unwrap().is_none());

    // Deleting something that is not there is an error with instructions, not
    // a silent success — a typo in an admin screen should say so.
    let error = store.delete_role("approver").await.unwrap_err().to_string();
    assert!(error.contains("there is no role named `approver`"), "{error}");

    store.drop_tables().await.unwrap();
}

#[tokio::test]
async fn a_role_carries_its_permissions_to_the_users_who_hold_it() {
    let url = database_url!();
    let store = fresh(&url, "_role").await;

    store.create_role("editor").await.unwrap();
    store.create_permission("posts.create").await.unwrap();
    store.create_permission("posts.delete").await.unwrap();
    store.attach_permission("editor", "posts.create").await.unwrap();

    assert!(!store.has_permission(41, "posts.create").await.unwrap());

    store.assign_role(41, "editor").await.unwrap();

    assert!(store.has_permission(41, "posts.create").await.unwrap());
    assert!(!store.has_permission(41, "posts.delete").await.unwrap());
    assert!(store.has_role(41, "editor").await.unwrap());
    assert_eq!(store.roles_for(41).await.unwrap(), ["editor"]);
    assert_eq!(store.permissions_for(41).await.unwrap(), ["posts.create"]);

    // Nobody else is affected by any of it.
    assert!(!store.has_permission(42, "posts.create").await.unwrap());
    assert!(store.permissions_for(42).await.unwrap().is_empty());

    store.remove_role(41, "editor").await.unwrap();
    assert!(!store.has_permission(41, "posts.create").await.unwrap());

    store.drop_tables().await.unwrap();
}

#[tokio::test]
async fn a_direct_grant_needs_no_role_and_a_direct_deny_overrules_one() {
    let url = database_url!();
    let store = fresh(&url, "_direct").await;

    store.create_role("support").await.unwrap();
    store.create_permission("billing.view").await.unwrap();
    store.create_permission("billing.refund").await.unwrap();
    store.attach_permission("support", "billing.view").await.unwrap();
    store.attach_permission("support", "billing.refund").await.unwrap();
    store.assign_role(41, "support").await.unwrap();
    store.assign_role(42, "support").await.unwrap();

    // A grant with no role behind it.
    store.grant(7, "billing.view").await.unwrap();
    assert!(store.has_permission(7, "billing.view").await.unwrap());
    assert!(store.roles_for(7).await.unwrap().is_empty());

    // "Everyone in support may refund, except this one person."
    store.deny(42, "billing.refund").await.unwrap();

    assert!(store.has_permission(41, "billing.refund").await.unwrap());
    assert!(!store.has_permission(42, "billing.refund").await.unwrap(), "the deny has to win");
    assert!(store.has_permission(42, "billing.view").await.unwrap(), "and only over that one");

    assert_eq!(
        store.direct_permissions(42).await.unwrap(),
        [("billing.refund".to_string(), false)],
        "the answer to `why can 42 not refund` is one row"
    );
    assert_eq!(store.permissions_for(42).await.unwrap(), ["billing.view"]);

    // A deny is not the absence of a grant: giving them the role again changes
    // nothing, because the deny is still standing.
    store.remove_role(42, "support").await.unwrap();
    store.assign_role(42, "support").await.unwrap();
    assert!(!store.has_permission(42, "billing.refund").await.unwrap());

    // `reset` is what takes the deny back.
    store.reset(42, "billing.refund").await.unwrap();
    assert!(store.has_permission(42, "billing.refund").await.unwrap());
    assert!(store.direct_permissions(42).await.unwrap().is_empty());

    store.drop_tables().await.unwrap();
}

#[tokio::test]
async fn granting_then_denying_the_same_permission_leaves_one_row() {
    let url = database_url!();
    let store = fresh(&url, "_flip").await;

    store.create_permission("billing.refund").await.unwrap();

    store.grant(41, "billing.refund").await.unwrap();
    store.deny(41, "billing.refund").await.unwrap();
    store.grant(41, "billing.refund").await.unwrap();

    // The unique index would have rejected a second row; this checks the store
    // updates rather than relying on the database to complain.
    let rows = store
        .database()
        .table(&store.tables().user_permission)
        .filter("user_id", 41i64)
        .count(store.database())
        .await
        .unwrap();

    assert_eq!(rows, 1);
    assert_eq!(store.direct_permissions(41).await.unwrap(), [("billing.refund".into(), true)]);

    store.drop_tables().await.unwrap();
}

#[tokio::test]
async fn a_stored_wildcard_satisfies_a_specific_check() {
    let url = database_url!();
    let store = fresh(&url, "_wild").await;

    store.create_role("editor").await.unwrap();
    store.create_permission("posts.*").await.unwrap();
    store.attach_permission("editor", "posts.*").await.unwrap();
    store.assign_role(41, "editor").await.unwrap();

    assert!(store.has_permission(41, "posts.create").await.unwrap());
    assert!(store.has_permission(41, "posts.comments.moderate").await.unwrap());
    assert!(!store.has_permission(41, "users.create").await.unwrap());
    assert!(!store.has_permission(41, "posts").await.unwrap());

    // The stored name is what is listed. The wildcard is a rule, not a set.
    assert_eq!(store.permissions_for(41).await.unwrap(), ["posts.*"]);

    // ...and a deny still reaches inside it.
    store.create_permission("posts.delete").await.unwrap();
    store.deny(41, "posts.delete").await.unwrap();
    assert!(!store.has_permission(41, "posts.delete").await.unwrap());
    assert!(store.has_permission(41, "posts.create").await.unwrap());

    store.drop_tables().await.unwrap();
}

#[tokio::test]
async fn the_super_role_passes_everything_except_an_explicit_deny() {
    let url = database_url!();
    let store = fresh(&url, "_super").await;

    store.create_role("super-admin").await.unwrap();
    store.assign_role(41, "super-admin").await.unwrap();

    assert!(store.has_permission(41, "anything.at.all").await.unwrap());
    assert!(store.has_permission(41, "a.permission.nobody.defined").await.unwrap());

    // And here is the risk, demonstrated: the role's permission list is empty,
    // and that empty list is not what it can do.
    assert!(store.role_permissions("super-admin").await.unwrap().is_empty());
    assert!(store.permissions_for(41).await.unwrap().is_empty());

    store.create_permission("billing.refund").await.unwrap();
    store.deny(41, "billing.refund").await.unwrap();
    assert!(!store.has_permission(41, "billing.refund").await.unwrap());
    assert!(store.has_permission(41, "billing.view").await.unwrap());

    // Configured off, the name means nothing.
    let plain = Permissions::with_tables(store.database().clone(), store.tables().clone())
        .unwrap()
        .super_role("");
    assert!(!plain.has_permission(41, "anything.at.all").await.unwrap());

    store.drop_tables().await.unwrap();
}

#[tokio::test]
async fn a_mutation_invalidates_the_cache_and_a_cache_hit_avoids_the_database() {
    let url = database_url!();
    let store = fresh(&url, "_cache").await;

    store.create_role("editor").await.unwrap();
    store.create_permission("posts.create").await.unwrap();
    store.attach_permission("editor", "posts.create").await.unwrap();
    store.assign_role(41, "editor").await.unwrap();

    assert!(store.has_permission(41, "posts.create").await.unwrap());
    assert_eq!(store.cached_users(), 1, "the answer should now be cached");

    // A second handle changes the rows behind this one's back, the way another
    // process would. The cached answer is stale, and that is the point of it.
    other_handle(&store).remove_role(41, "editor").await.unwrap();
    assert!(
        store.has_permission(41, "posts.create").await.unwrap(),
        "a cache that noticed this would not be a cache"
    );

    // Its own `forget` clears it, and now it sees the world as it is.
    store.forget(41);
    assert!(!store.has_permission(41, "posts.create").await.unwrap());

    // A change made *through* this handle takes effect at once — no waiting
    // out the TTL, which is the whole reason every mutator invalidates.
    store.assign_role(41, "editor").await.unwrap();
    assert!(store.has_permission(41, "posts.create").await.unwrap());
    store.remove_role(41, "editor").await.unwrap();
    assert!(!store.has_permission(41, "posts.create").await.unwrap());

    // A change to the *role* invalidates everybody, because who holds it is
    // not known without a query.
    store.assign_role(41, "editor").await.unwrap();
    assert!(store.has_permission(41, "posts.create").await.unwrap());
    store.detach_permission("editor", "posts.create").await.unwrap();
    assert_eq!(store.cached_users(), 0);
    assert!(!store.has_permission(41, "posts.create").await.unwrap());

    store.drop_tables().await.unwrap();
}

#[tokio::test]
async fn an_expired_entry_is_reloaded() {
    let url = database_url!();
    let store = fresh(&url, "_ttl").await.cache_ttl(Duration::from_millis(50));

    store.create_role("editor").await.unwrap();
    store.create_permission("posts.create").await.unwrap();
    store.attach_permission("editor", "posts.create").await.unwrap();
    store.assign_role(41, "editor").await.unwrap();

    assert!(store.has_permission(41, "posts.create").await.unwrap());

    other_handle(&store).remove_role(41, "editor").await.unwrap();
    assert!(store.has_permission(41, "posts.create").await.unwrap(), "still inside the TTL");

    tokio::time::sleep(Duration::from_millis(80)).await;

    assert!(
        !store.has_permission(41, "posts.create").await.unwrap(),
        "the TTL is the backstop for a change another process made"
    );

    store.drop_tables().await.unwrap();
}

#[tokio::test]
async fn deleting_a_role_takes_its_attachments_and_assignments_with_it() {
    let url = database_url!();
    let store = fresh(&url, "_cascade").await;

    store.create_role("editor").await.unwrap();
    store.create_permission("posts.create").await.unwrap();
    store.attach_permission("editor", "posts.create").await.unwrap();
    store.assign_role(41, "editor").await.unwrap();

    let db = store.database();
    assert_eq!(
        db.table(&store.tables().user_role).count(db).await.unwrap(),
        1,
        "the assignment is there to start with"
    );

    store.delete_role("editor").await.unwrap();

    // `on delete cascade`, not application code: the rows are gone because the
    // database took them.
    assert_eq!(db.table(&store.tables().user_role).count(db).await.unwrap(), 0);
    assert_eq!(db.table(&store.tables().role_permission).count(db).await.unwrap(), 0);
    assert!(!store.has_permission(41, "posts.create").await.unwrap());

    // The same for a permission: deleting it takes the attachments and the
    // direct entries.
    store.create_role("editor").await.unwrap();
    store.attach_permission("editor", "posts.create").await.unwrap();
    store.grant(7, "posts.create").await.unwrap();
    store.delete_permission("posts.create").await.unwrap();

    assert_eq!(db.table(&store.tables().role_permission).count(db).await.unwrap(), 0);
    assert_eq!(db.table(&store.tables().user_permission).count(db).await.unwrap(), 0);

    store.drop_tables().await.unwrap();
}

#[tokio::test]
async fn a_roles_permissions_can_be_set_wholesale_and_a_user_purged() {
    let url = database_url!();
    let store = fresh(&url, "_sync").await;

    store.create_role("editor").await.unwrap();
    for name in ["posts.create", "posts.publish", "posts.delete"] {
        store.create_permission(name).await.unwrap();
    }

    store.set_role_permissions("editor", &["posts.create", "posts.publish"]).await.unwrap();
    assert_eq!(store.role_permissions("editor").await.unwrap(), ["posts.create", "posts.publish"]);

    // Adds `delete`, removes `publish`, leaves `create` alone.
    store.set_role_permissions("editor", &["posts.create", "posts.delete"]).await.unwrap();
    assert_eq!(store.role_permissions("editor").await.unwrap(), ["posts.create", "posts.delete"]);

    store.assign_role(41, "editor").await.unwrap();
    store.deny(41, "posts.delete").await.unwrap();
    assert_eq!(store.permissions_for(41).await.unwrap(), ["posts.create"]);

    // There is no foreign key to a users table, so a deleted user is this
    // crate's to clean up. Without this, id 41 comes back one day as somebody
    // else, holding these permissions.
    store.purge_user(41).await.unwrap();
    assert!(store.roles_for(41).await.unwrap().is_empty());
    assert!(store.direct_permissions(41).await.unwrap().is_empty());
    assert!(!store.has_permission(41, "posts.create").await.unwrap());

    store.drop_tables().await.unwrap();
}

#[tokio::test]
async fn attaching_and_assigning_twice_is_not_an_error() {
    let url = database_url!();
    let store = fresh(&url, "_twice").await;

    store.create_role("editor").await.unwrap();
    store.create_permission("posts.create").await.unwrap();

    store.attach_permission("editor", "posts.create").await.unwrap();
    store.attach_permission("editor", "posts.create").await.unwrap();
    store.assign_role(41, "editor").await.unwrap();
    store.assign_role(41, "editor").await.unwrap();

    assert_eq!(store.role_permissions("editor").await.unwrap(), ["posts.create"]);
    assert_eq!(store.roles_for(41).await.unwrap(), ["editor"]);

    store.drop_tables().await.unwrap();
}

#[tokio::test]
async fn two_roles_carrying_the_same_permission_list_it_once() {
    let url = database_url!();
    let store = fresh(&url, "_overlap").await;

    store.create_role("editor").await.unwrap();
    store.create_role("reviewer").await.unwrap();
    store.create_permission("posts.read").await.unwrap();
    store.attach_permission("editor", "posts.read").await.unwrap();
    store.attach_permission("reviewer", "posts.read").await.unwrap();
    store.assign_role(41, "editor").await.unwrap();
    store.assign_role(41, "reviewer").await.unwrap();

    assert_eq!(store.permissions_for(41).await.unwrap(), ["posts.read"]);
    assert_eq!(store.roles_for(41).await.unwrap(), ["editor", "reviewer"]);
    assert!(store.has_any_role(41, &["approver", "reviewer"]).await.unwrap());
    assert!(store.has_any_permission(41, &["posts.read", "posts.write"]).await.unwrap());
    assert!(!store.has_all_permissions(41, &["posts.read", "posts.write"]).await.unwrap());

    // Losing one of the two roles does not lose the permission.
    store.remove_role(41, "editor").await.unwrap();
    assert!(store.has_permission(41, "posts.read").await.unwrap());

    store.drop_tables().await.unwrap();
}

#[tokio::test]
async fn a_permission_that_was_never_defined_cannot_be_granted() {
    let url = database_url!();
    let store = fresh(&url, "_undefined").await;

    let error = store.grant(41, "posts.creat").await.unwrap_err().to_string();

    assert!(error.contains("there is no permission named `posts.creat`"), "{error}");
    assert!(error.contains("create_permission"), "{error}");

    let error = store.assign_role(41, "editr").await.unwrap_err().to_string();
    assert!(error.contains("there is no role named `editr`"), "{error}");

    store.drop_tables().await.unwrap();
}

#[tokio::test]
async fn a_direct_grant_survives_the_round_trip_as_a_boolean() {
    let url = database_url!();
    let store = fresh(&url, "_bools").await;

    store.create_permission("a").await.unwrap();
    store.create_permission("b").await.unwrap();
    store.grant(41, "a").await.unwrap();
    store.deny(41, "b").await.unwrap();

    // Read back through the store, and read back raw. `granted` is a boolean on
    // PostgreSQL and a number on MySQL and SQL Server, and the store has to
    // cope with both — this checks the value at least came back distinguishable.
    assert_eq!(
        store.direct_permissions(41).await.unwrap(),
        [("a".to_string(), true), ("b".to_string(), false)]
    );

    let db = store.database();
    let denied = db
        .table(&store.tables().user_permission)
        .filter("granted", Value::from(false))
        .count(db)
        .await
        .unwrap();
    assert_eq!(denied, 1);

    store.drop_tables().await.unwrap();
}

/// The migration, run by the real migrator, on the conventional table names.
///
/// Alone among these tests it uses the default names, because that is what it
/// is checking. It therefore drops `roles`, `permissions` and the three pivots
/// before and after — do not point `DATABASE_URL` at a database whose own
/// `roles` table you would miss.
#[tokio::test]
async fn the_migration_creates_and_rolls_back_the_conventional_tables() {
    let url = database_url!();
    let db = Database::connect(&url).await.expect("DATABASE_URL should be reachable");

    let store = Permissions::new(db.clone());
    store.drop_tables().await.expect("dropping leftovers");

    // Its own tracking table, so this does not roll back another suite's batch.
    let registry = migrations();
    let migrator = Migrator::new(&db, registry).with_table("rbac_migrations_test").unwrap();
    db.run("drop table if exists rbac_migrations_test").await.unwrap();

    let report = migrator.run().await.unwrap();
    assert_eq!(report.applied, ["2026_09_02_000001_create_rbac_tables"]);

    // The store works on what the migration built, with no `migrate()` call.
    store.create_role("editor").await.unwrap();
    store.create_permission("posts.*").await.unwrap();
    store.attach_permission("editor", "posts.*").await.unwrap();
    store.assign_role(41, "editor").await.unwrap();
    assert!(store.has_permission(41, "posts.publish").await.unwrap());

    let report = migrator.rollback().await.unwrap();
    assert_eq!(report.rolled_back, ["2026_09_02_000001_create_rbac_tables"]);

    let schema = rustlavel_db::Schema::new(&db);
    for table in TableNames::default().all() {
        assert!(!schema.has_table(table).await.unwrap(), "{table} should be gone");
    }

    db.run("drop table if exists rbac_migrations_test").await.unwrap();
}
