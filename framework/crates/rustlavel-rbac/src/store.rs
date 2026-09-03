//! The store: roles, permissions, and the two questions everything else is
//! built on.
//!
//! ```ignore
//! let permissions = Permissions::from_config(db.clone(), &config)?;
//!
//! permissions.create_role("editor").await?;
//! permissions.create_permission("posts.*").await?;
//! permissions.attach_permission("editor", "posts.*").await?;
//! permissions.assign_role(41, "editor").await?;
//!
//! assert!(permissions.has_permission(41, "posts.publish").await?);
//!
//! // ...except for this one person.
//! permissions.deny(41, "posts.delete").await?;
//! assert!(!permissions.has_permission(41, "posts.delete").await?);
//! ```
//!
//! Every statement goes through the query builder, so the same code runs on
//! PostgreSQL, MySQL and SQL Server.

use crate::grants::Grants;
use crate::tables::TableNames;
use rustlavel_core::{Config, Error, Result};
use rustlavel_db::schema::Schema;
use rustlavel_db::{Database, Direction, Value};
use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

/// The role that passes every check.
pub const DEFAULT_SUPER_ROLE: &str = "super-admin";

/// How long a user's resolved grants are reused before being loaded again.
pub const DEFAULT_CACHE_TTL: Duration = Duration::from_secs(30);

/// A row from the `roles` or `permissions` table.
///
/// One type for both because they are the same table twice; a role is a named
/// bundle and a permission is a named leaf, and neither carries anything the
/// other does not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Named {
    pub id: i64,
    pub name: String,
    pub description: Option<String>,
}

/// A cached [`Grants`] and the moment it stops being trusted.
struct Cached {
    grants: Arc<Grants>,
    expires_at: Instant,
}

/// Roles, permissions, and the checks that read them.
///
/// Cheap to clone — the database handle is pooled and the cache is shared — so
/// it goes into application state once and every request gets a handle to the
/// same one. That matters: a per-request clone with a per-request cache would
/// be a cache that never hits.
#[derive(Clone)]
pub struct Permissions {
    db: Database,
    tables: TableNames,
    super_roles: BTreeSet<String>,
    ttl: Duration,
    cache: Arc<RwLock<HashMap<i64, Cached>>>,
}

impl Permissions {
    /// A store on the conventional tables, with `super-admin` as the super role
    /// and a 30-second cache.
    pub fn new(db: Database) -> Self {
        Permissions {
            db,
            tables: TableNames::default(),
            super_roles: [DEFAULT_SUPER_ROLE.to_string()].into_iter().collect(),
            ttl: DEFAULT_CACHE_TTL,
            cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// A store on tables of your own. The names are validated here, once.
    pub fn with_tables(db: Database, tables: TableNames) -> Result<Self> {
        tables.validate()?;
        Ok(Permissions { tables, ..Permissions::new(db) })
    }

    /// Read `rbac.super_role`, `rbac.super_roles` and `rbac.cache_ttl_ms`.
    ///
    /// `rbac.super_roles` is a list — a JSON array, or the comma-separated
    /// string an `.env` variable is limited to — and wins when it is set.
    /// `rbac.super_role` is the single-value spelling, which is what almost
    /// every application wants.
    pub fn from_config(db: Database, config: &Config) -> Result<Self> {
        let mut store = Permissions::new(db);

        let listed = config.list("rbac.super_roles");
        if listed.is_empty() {
            store.super_roles =
                [config.string("rbac.super_role", DEFAULT_SUPER_ROLE)].into_iter().collect();
        } else {
            store.super_roles = listed.into_iter().collect();
        }

        // A negative or absurd TTL is a typo, not an instruction. Clamped
        // rather than rejected: refusing to boot over a cache setting would be
        // a worse failure than ignoring it.
        let ttl = config.int("rbac.cache_ttl_ms", DEFAULT_CACHE_TTL.as_millis() as i64);
        store.ttl = Duration::from_millis(ttl.clamp(0, 3_600_000) as u64);

        Ok(store)
    }

    /// Use a different super role. Pass an empty name to have none at all.
    pub fn super_role(mut self, name: impl Into<String>) -> Self {
        let name = name.into();
        self.super_roles =
            if name.is_empty() { BTreeSet::new() } else { [name].into_iter().collect() };
        self
    }

    /// Use several super roles.
    pub fn super_roles<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.super_roles = names.into_iter().map(Into::into).collect();
        self
    }

    /// How long resolved grants are cached. [`Duration::ZERO`] disables it.
    pub fn cache_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    pub fn database(&self) -> &Database {
        &self.db
    }

    pub fn tables(&self) -> &TableNames {
        &self.tables
    }

    /// The role names that pass every check.
    pub fn super_role_names(&self) -> &BTreeSet<String> {
        &self.super_roles
    }

    /// Create the tables directly, for an application not using migrations.
    pub async fn migrate(&self) -> Result<()> {
        crate::tables::create_tables(&Schema::new(&self.db), &self.tables).await
    }

    /// Drop them again. Intended for tests.
    pub async fn drop_tables(&self) -> Result<()> {
        crate::tables::drop_tables(&Schema::new(&self.db), &self.tables).await
    }

    // --- Roles ---

    /// Define a role. Fails if one by that name already exists.
    pub async fn create_role(&self, name: &str) -> Result<Named> {
        self.create_named(&self.tables.roles, "role", name, None).await
    }

    /// Define a role, with a sentence saying what it is for.
    pub async fn create_role_with(&self, name: &str, description: &str) -> Result<Named> {
        self.create_named(&self.tables.roles, "role", name, Some(description)).await
    }

    /// Rename a role. Every assignment follows it, because assignments are by
    /// id — which is the reason the tables store ids and not names.
    pub async fn rename_role(&self, from: &str, to: &str) -> Result<()> {
        self.rename_named(&self.tables.roles, "role", from, to).await
    }

    /// Delete a role. Its permission attachments and every user's assignment of
    /// it go with it, by `on delete cascade`.
    pub async fn delete_role(&self, name: &str) -> Result<()> {
        self.delete_named(&self.tables.roles, "role", name).await
    }

    /// Every role, by name.
    pub async fn roles(&self) -> Result<Vec<Named>> {
        self.list_named(&self.tables.roles).await
    }

    pub async fn find_role(&self, name: &str) -> Result<Option<Named>> {
        self.find_named(&self.tables.roles, name).await
    }

    // --- Permissions ---

    /// Define a permission. The name may be a wildcard: `posts.*`, or `*`.
    pub async fn create_permission(&self, name: &str) -> Result<Named> {
        self.create_named(&self.tables.permissions, "permission", name, None).await
    }

    pub async fn create_permission_with(&self, name: &str, description: &str) -> Result<Named> {
        self.create_named(&self.tables.permissions, "permission", name, Some(description)).await
    }

    pub async fn rename_permission(&self, from: &str, to: &str) -> Result<()> {
        self.rename_named(&self.tables.permissions, "permission", from, to).await
    }

    /// Delete a permission, and with it every attachment and direct entry.
    pub async fn delete_permission(&self, name: &str) -> Result<()> {
        self.delete_named(&self.tables.permissions, "permission", name).await
    }

    pub async fn permissions(&self) -> Result<Vec<Named>> {
        self.list_named(&self.tables.permissions).await
    }

    pub async fn find_permission(&self, name: &str) -> Result<Option<Named>> {
        self.find_named(&self.tables.permissions, name).await
    }

    // --- Roles and permissions together ---

    /// Give a role a permission. Attaching one it already has does nothing.
    pub async fn attach_permission(&self, role: &str, permission: &str) -> Result<()> {
        let role_id = self.role_id(role).await?;
        let permission_id = self.permission_id(permission).await?;

        let already = self
            .db
            .table(&self.tables.role_permission)
            .filter("role_id", role_id)
            .filter("permission_id", permission_id)
            .exists(&self.db)
            .await?;

        if !already {
            self.db
                .table(&self.tables.role_permission)
                .insert_without_id(
                    &self.db,
                    &[
                        ("role_id", Value::from(role_id)),
                        ("permission_id", Value::from(permission_id)),
                    ],
                )
                .await?;
        }

        // Any user holding this role is now wrong in the cache, and there is no
        // cheap way to know which. Everything goes.
        self.flush();
        Ok(())
    }

    /// Take a permission away from a role.
    pub async fn detach_permission(&self, role: &str, permission: &str) -> Result<()> {
        let role_id = self.role_id(role).await?;
        let permission_id = self.permission_id(permission).await?;

        self.db
            .table(&self.tables.role_permission)
            .filter("role_id", role_id)
            .filter("permission_id", permission_id)
            .delete(&self.db)
            .await?;

        self.flush();
        Ok(())
    }

    /// Make a role's permissions exactly this list, adding and removing as
    /// needed — what an admin screen's save button wants.
    pub async fn set_role_permissions(&self, role: &str, names: &[&str]) -> Result<()> {
        let wanted: BTreeSet<&str> = names.iter().copied().collect();
        let current: BTreeSet<String> = self.role_permissions(role).await?.into_iter().collect();

        for name in &wanted {
            if !current.contains(*name) {
                self.attach_permission(role, name).await?;
            }
        }
        for name in &current {
            if !wanted.contains(name.as_str()) {
                self.detach_permission(role, name).await?;
            }
        }
        Ok(())
    }

    /// The permissions attached to one role.
    pub async fn role_permissions(&self, role: &str) -> Result<Vec<String>> {
        let role_id = self.role_id(role).await?;
        self.permission_names_for_roles(&[Value::from(role_id)]).await
    }

    // --- Users ---

    /// Give a user a role. Assigning one they already hold does nothing.
    pub async fn assign_role(&self, user_id: i64, role: &str) -> Result<()> {
        let role_id = self.role_id(role).await?;

        let already = self
            .db
            .table(&self.tables.user_role)
            .filter("user_id", user_id)
            .filter("role_id", role_id)
            .exists(&self.db)
            .await?;

        if !already {
            self.db
                .table(&self.tables.user_role)
                .insert_without_id(
                    &self.db,
                    &[("user_id", Value::from(user_id)), ("role_id", Value::from(role_id))],
                )
                .await?;
        }

        self.forget(user_id);
        Ok(())
    }

    /// Take a role away from a user.
    pub async fn remove_role(&self, user_id: i64, role: &str) -> Result<()> {
        let role_id = self.role_id(role).await?;

        self.db
            .table(&self.tables.user_role)
            .filter("user_id", user_id)
            .filter("role_id", role_id)
            .delete(&self.db)
            .await?;

        self.forget(user_id);
        Ok(())
    }

    /// Grant a permission to one user, outside any role.
    pub async fn grant(&self, user_id: i64, permission: &str) -> Result<()> {
        self.set_direct(user_id, permission, true).await
    }

    /// Deny a permission to one user — the entry that overrules a role.
    ///
    /// This is not the same as taking the permission away. It is a standing
    /// instruction that survives being given the role again, and it is the
    /// highest-ranked rule in the system. Use [`Permissions::reset`] to go back
    /// to "whatever their roles say".
    pub async fn deny(&self, user_id: i64, permission: &str) -> Result<()> {
        self.set_direct(user_id, permission, false).await
    }

    /// Remove the direct entry, grant or deny, leaving the roles to decide.
    pub async fn reset(&self, user_id: i64, permission: &str) -> Result<()> {
        let permission_id = self.permission_id(permission).await?;

        self.db
            .table(&self.tables.user_permission)
            .filter("user_id", user_id)
            .filter("permission_id", permission_id)
            .delete(&self.db)
            .await?;

        self.forget(user_id);
        Ok(())
    }

    /// Forget everything about one user: their roles and their direct entries.
    ///
    /// There is no foreign key to a users table (see [`crate::tables`]), so a
    /// deleted user's rows are this crate's to clean up. Call it when a user is
    /// deleted, or their id will eventually be handed to somebody else along
    /// with their permissions.
    pub async fn purge_user(&self, user_id: i64) -> Result<()> {
        self.db
            .table(&self.tables.user_role)
            .filter("user_id", user_id)
            .delete(&self.db)
            .await?;
        self.db
            .table(&self.tables.user_permission)
            .filter("user_id", user_id)
            .delete(&self.db)
            .await?;

        self.forget(user_id);
        Ok(())
    }

    /// The names of a user's roles.
    pub async fn roles_for(&self, user_id: i64) -> Result<Vec<String>> {
        Ok(self.grants_for(user_id).await?.roles.iter().cloned().collect())
    }

    /// How many users hold a role.
    ///
    /// A count rather than the users themselves: the question this answers —
    /// "is anybody still using this role?" — comes up on a list of every role,
    /// and loading the members of each to take `len()` is the same answer for
    /// far more rows.
    pub async fn users_in_role(&self, role: &str) -> Result<i64> {
        let role_id = self.role_id(role).await?;
        self.db.table(&self.tables.user_role).filter("role_id", role_id).count(&self.db).await
    }

    /// A user's direct entries: the permission name and whether it is a grant.
    ///
    /// `false` is a deny. Exposed because "why can this user not do X" is
    /// answered by this list far more often than by anything else.
    pub async fn direct_permissions(&self, user_id: i64) -> Result<Vec<(String, bool)>> {
        let pivot = &self.tables.user_permission;
        let name = format!("{}.name", self.tables.permissions);
        let granted = format!("{pivot}.granted");

        let rows = self
            .db
            .table(pivot)
            .select(&[name.as_str(), granted.as_str()])
            .join(
                &self.tables.permissions,
                &format!("{pivot}.permission_id"),
                "=",
                &format!("{}.id", self.tables.permissions),
            )
            .filter(&format!("{pivot}.user_id"), user_id)
            .order_by(&name, Direction::Asc)
            .get(&self.db)
            .await?;

        rows.iter()
            .map(|row| Ok((row.get::<String>("name")?, truthy(row.value("granted")?))))
            .collect()
    }

    /// The permission rules that apply to a user, with the denied ones removed.
    ///
    /// Stored names, so a wildcard is listed as the wildcard. See
    /// [`Grants::effective`] for why that is not the same as "every action they
    /// can perform".
    pub async fn permissions_for(&self, user_id: i64) -> Result<Vec<String>> {
        Ok(self.grants_for(user_id).await?.effective())
    }

    /// May this user do this?
    ///
    /// The precedence is [`Grants::allows`]: an explicit deny, then a super
    /// role, then a grant from anywhere, then no.
    pub async fn has_permission(&self, user_id: i64, permission: &str) -> Result<bool> {
        Ok(self.grants_for(user_id).await?.allows(permission))
    }

    /// Does this user hold this role, by exact name?
    pub async fn has_role(&self, user_id: i64, role: &str) -> Result<bool> {
        Ok(self.grants_for(user_id).await?.has_role(role))
    }

    /// Any one of these permissions.
    pub async fn has_any_permission(&self, user_id: i64, permissions: &[&str]) -> Result<bool> {
        let grants = self.grants_for(user_id).await?;
        Ok(permissions.iter().any(|permission| grants.allows(permission)))
    }

    /// Every one of these permissions.
    pub async fn has_all_permissions(&self, user_id: i64, permissions: &[&str]) -> Result<bool> {
        let grants = self.grants_for(user_id).await?;
        Ok(permissions.iter().all(|permission| grants.allows(permission)))
    }

    /// Any one of these roles.
    pub async fn has_any_role(&self, user_id: i64, roles: &[&str]) -> Result<bool> {
        let grants = self.grants_for(user_id).await?;
        Ok(roles.iter().any(|role| grants.has_role(role)))
    }

    // --- The cache ---

    /// A user's resolved grants, from the cache when it is warm.
    ///
    /// This is the only method that reads the database for a check, and every
    /// question above goes through it. Three queries on a miss — the user's
    /// roles, the permissions those roles carry, and the direct entries —
    /// rather than one join, because three readable statements that run once
    /// every 30 seconds are worth more than one clever one.
    pub async fn grants_for(&self, user_id: i64) -> Result<Arc<Grants>> {
        if let Some(grants) = self.cached(user_id) {
            return Ok(grants);
        }

        let grants = Arc::new(self.load(user_id).await?);

        if !self.ttl.is_zero() {
            let entry =
                Cached { grants: Arc::clone(&grants), expires_at: Instant::now() + self.ttl };
            self.write_cache().insert(user_id, entry);
        }

        Ok(grants)
    }

    /// Put grants into the cache without touching the database.
    ///
    /// For warming a cache ahead of a burst of checks, and for testing a guard
    /// without a database behind it. It is *not* a way to grant anything: the
    /// entry expires like any other and the next load overwrites it.
    pub fn prime(&self, user_id: i64, grants: Grants) {
        let ttl = if self.ttl.is_zero() { DEFAULT_CACHE_TTL } else { self.ttl };
        self.write_cache()
            .insert(user_id, Cached { grants: Arc::new(grants), expires_at: Instant::now() + ttl });
    }

    /// Drop one user's cached grants.
    ///
    /// Called by every method that changes what a user is allowed, so an
    /// administrator who removes a role sees it take effect on the next
    /// request rather than in 30 seconds' time. The TTL is the backstop for
    /// changes made by *another* process, not the mechanism.
    pub fn forget(&self, user_id: i64) {
        self.write_cache().remove(&user_id);
    }

    /// Drop every cached entry.
    ///
    /// What a change to a role does, because a role's members are not known
    /// without a query and invalidating everybody is cheaper than finding out.
    pub fn flush(&self) {
        self.write_cache().clear();
    }

    /// How many users are currently cached. Diagnostics, and tests.
    pub fn cached_users(&self) -> usize {
        self.read_cache().len()
    }

    fn cached(&self, user_id: i64) -> Option<Arc<Grants>> {
        let cache = self.read_cache();
        let entry = cache.get(&user_id)?;

        // An expired entry is left in the map rather than removed here: this is
        // a read lock, and taking a write lock to tidy up would serialise every
        // check behind the one that noticed. The next load replaces it.
        (entry.expires_at > Instant::now()).then(|| Arc::clone(&entry.grants))
    }

    fn read_cache(&self) -> std::sync::RwLockReadGuard<'_, HashMap<i64, Cached>> {
        // A poisoned lock means a thread panicked while holding it. The map is
        // a cache of derived data, so the worst case is a stale entry — far
        // better than every subsequent authorization check panicking.
        self.cache.read().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn write_cache(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<i64, Cached>> {
        self.cache.write().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Read one user's roles, their roles' permissions, and their direct
    /// entries, and fold them into the shape a check wants.
    async fn load(&self, user_id: i64) -> Result<Grants> {
        let pivot = &self.tables.user_role;
        let role_id = format!("{}.id", self.tables.roles);
        let role_name = format!("{}.name", self.tables.roles);

        let role_rows = self
            .db
            .table(pivot)
            .select(&[role_id.as_str(), role_name.as_str()])
            .join(
                &self.tables.roles,
                &format!("{pivot}.role_id"),
                "=",
                &role_id,
            )
            .filter(&format!("{pivot}.user_id"), user_id)
            .get(&self.db)
            .await?;

        let mut roles = BTreeSet::new();
        let mut role_ids = Vec::new();
        for row in &role_rows {
            role_ids.push(Value::from(row.get::<i64>("id")?));
            roles.insert(row.get::<String>("name")?);
        }

        let mut granted: BTreeSet<String> = if role_ids.is_empty() {
            // `filter_in` with an empty list renders as `false`, which is
            // correct — but it is still a round trip to learn what is already
            // known here.
            BTreeSet::new()
        } else {
            self.permission_names_for_roles(&role_ids).await?.into_iter().collect()
        };

        let mut denied = BTreeSet::new();
        for (name, is_granted) in self.direct_permissions(user_id).await? {
            if is_granted {
                granted.insert(name);
            } else {
                denied.insert(name);
            }
        }

        let is_super = roles.iter().any(|role| self.super_roles.contains(role));

        Ok(Grants { roles, granted, denied, is_super })
    }

    // --- Shared query shapes ---

    /// The permission names attached to any of these roles, deduplicated.
    ///
    /// `distinct` matters: two of a user's roles carrying the same permission
    /// is normal, and without it the set below would be built from a list with
    /// repeats in it.
    async fn permission_names_for_roles(&self, role_ids: &[Value]) -> Result<Vec<String>> {
        let pivot = &self.tables.role_permission;
        let name = format!("{}.name", self.tables.permissions);

        let rows = self
            .db
            .table(pivot)
            .distinct()
            .select(&[name.as_str()])
            .join(
                &self.tables.permissions,
                &format!("{pivot}.permission_id"),
                "=",
                &format!("{}.id", self.tables.permissions),
            )
            .filter_in(&format!("{pivot}.role_id"), role_ids.to_vec())
            .order_by(&name, Direction::Asc)
            .get(&self.db)
            .await?;

        rows.iter().map(|row| row.get::<String>("name")).collect()
    }

    async fn set_direct(&self, user_id: i64, permission: &str, granted: bool) -> Result<()> {
        let permission_id = self.permission_id(permission).await?;

        // No `on conflict` clause: the three databases spell it three different
        // ways and rustlavel-db does not model it, so the portable shape is
        // update-then-insert. The unique index is still what guarantees one row
        // per pair — this only decides which statement gets there first.
        let updated = self
            .db
            .table(&self.tables.user_permission)
            .filter("user_id", user_id)
            .filter("permission_id", permission_id)
            .update(&self.db, &[("granted", Value::from(granted))])
            .await?;

        if updated == 0 {
            self.db
                .table(&self.tables.user_permission)
                .insert_without_id(
                    &self.db,
                    &[
                        ("user_id", Value::from(user_id)),
                        ("permission_id", Value::from(permission_id)),
                        ("granted", Value::from(granted)),
                    ],
                )
                .await?;
        }

        self.forget(user_id);
        Ok(())
    }

    async fn create_named(
        &self,
        table: &str,
        kind: &str,
        name: &str,
        description: Option<&str>,
    ) -> Result<Named> {
        validate_name(kind, name)?;

        if self.find_named(table, name).await?.is_some() {
            // The unique index is the real guarantee; this only turns a wire
            // protocol error into a sentence.
            return Err(Error::msg(format!("a {kind} named `{name}` already exists")));
        }

        let mut values = vec![("name", Value::from(name))];
        if let Some(text) = description {
            values.push(("description", Value::from(text)));
        }

        let id = self.db.table(table).insert(&self.db, &values).await?;

        Ok(Named { id, name: name.to_string(), description: description.map(str::to_string) })
    }

    async fn rename_named(&self, table: &str, kind: &str, from: &str, to: &str) -> Result<()> {
        validate_name(kind, to)?;
        self.require_named(table, kind, from).await?;

        if self.find_named(table, to).await?.is_some() {
            return Err(Error::msg(format!("a {kind} named `{to}` already exists")));
        }

        self.db
            .table(table)
            .filter("name", from)
            .update(&self.db, &[("name", Value::from(to))])
            .await?;

        // The id did not change, so every assignment follows the rename — but
        // the cache holds names, and those are now wrong.
        self.flush();
        Ok(())
    }

    async fn delete_named(&self, table: &str, kind: &str, name: &str) -> Result<()> {
        self.require_named(table, kind, name).await?;
        self.db.table(table).filter("name", name).delete(&self.db).await?;
        self.flush();
        Ok(())
    }

    async fn list_named(&self, table: &str) -> Result<Vec<Named>> {
        let rows =
            self.db.table(table).order_by("name", Direction::Asc).get(&self.db).await?;
        rows.iter().map(hydrate).collect()
    }

    async fn find_named(&self, table: &str, name: &str) -> Result<Option<Named>> {
        match self.db.table(table).filter("name", name).first(&self.db).await? {
            Some(row) => Ok(Some(hydrate(&row)?)),
            None => Ok(None),
        }
    }

    async fn require_named(&self, table: &str, kind: &str, name: &str) -> Result<Named> {
        self.find_named(table, name).await?.ok_or_else(|| {
            Error::msg(format!(
                "there is no {kind} named `{name}`. Create it first with \
                 `create_{kind}(\"{name}\")` — this crate will not invent one for you, because a \
                 typo that silently defines a new {kind} is a hole, not a convenience."
            ))
        })
    }

    async fn role_id(&self, name: &str) -> Result<i64> {
        Ok(self.require_named(&self.tables.roles, "role", name).await?.id)
    }

    async fn permission_id(&self, name: &str) -> Result<i64> {
        Ok(self.require_named(&self.tables.permissions, "permission", name).await?.id)
    }
}

/// Written by hand rather than derived, because [`Database`] is not `Debug`
/// and because a connection string does not belong in a log line anyway.
impl std::fmt::Debug for Permissions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Permissions")
            .field("tables", &self.tables)
            .field("super_roles", &self.super_roles)
            .field("cache_ttl", &self.ttl)
            .field("cached_users", &self.cached_users())
            .finish()
    }
}

fn hydrate(row: &rustlavel_db::Row) -> Result<Named> {
    Ok(Named {
        id: row.get::<i64>("id")?,
        name: row.get::<String>("name")?,
        description: row.get::<String>("description").ok().filter(|text| !text.is_empty()),
    })
}

/// Read a boolean column whatever the database made of it.
///
/// PostgreSQL hands back a real boolean. MySQL stores one as `tinyint` and SQL
/// Server as `bit`, and either can arrive as a number, so `row.get::<bool>()`
/// is not portable here. Keeping the tolerance in one named function is better
/// than three drivers' worth of surprise at the call sites.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Bool(flag) => *flag,
        Value::Int(n) => *n != 0,
        Value::Float(n) => *n != 0.0,
        Value::Text(text) => matches!(text.as_str(), "1" | "t" | "true" | "TRUE" | "True"),
        _ => false,
    }
}

/// Reject a name that would only cause confusion.
///
/// Names never reach SQL except as bound parameters, so this is not about
/// injection. It is about a role called `" admin"` sitting next to one called
/// `"admin"` in a list, looking identical, and behaving differently.
fn validate_name(kind: &str, name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(Error::msg(format!("a {kind} needs a name")));
    }
    if name.trim() != name {
        return Err(Error::msg(format!(
            "the {kind} name `{name}` has leading or trailing whitespace, which would make it \
             indistinguishable from `{}` in a list", name.trim()
        )));
    }
    if name.len() > 255 {
        return Err(Error::msg(format!(
            "the {kind} name is {} characters; the column holds 255",
            name.len()
        )));
    }
    if name.chars().any(|c| c.is_control()) {
        return Err(Error::msg(format!("the {kind} name contains a control character")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustlavel_db::DatabaseConfig;

    /// A handle that is never connected. Everything below asks questions that
    /// are answered before a statement would be sent.
    fn offline() -> Database {
        Database::lazy(
            DatabaseConfig::from_url("postgres://nobody:nothing@127.0.0.1:1/none")
                .expect("a well-formed URL"),
        )
        .expect("lazy does not connect")
    }

    fn grants(roles: &[&str], granted: &[&str], denied: &[&str], is_super: bool) -> Grants {
        Grants {
            roles: roles.iter().map(|r| (*r).to_string()).collect(),
            granted: granted.iter().map(|p| (*p).to_string()).collect(),
            denied: denied.iter().map(|p| (*p).to_string()).collect(),
            is_super,
        }
    }

    #[test]
    fn the_defaults_are_super_admin_and_thirty_seconds() {
        let store = Permissions::new(offline());

        assert!(store.super_role_names().contains(DEFAULT_SUPER_ROLE));
        assert_eq!(store.ttl, Duration::from_secs(30));
        assert_eq!(store.tables(), &TableNames::default());
    }

    #[test]
    fn configuration_supplies_the_super_role_and_the_ttl() {
        let config = Config::new();
        config.set("rbac.super_role", "root");
        config.set("rbac.cache_ttl_ms", 5_000);

        let store = Permissions::from_config(offline(), &config).expect("valid configuration");

        assert_eq!(store.super_role_names().iter().collect::<Vec<_>>(), ["root"]);
        assert_eq!(store.ttl, Duration::from_millis(5_000));
    }

    #[test]
    fn a_list_of_super_roles_wins_over_the_single_one() {
        let config = Config::new();
        config.set("rbac.super_role", "root");
        // The `.env` spelling: one string, commas inside it.
        config.set("rbac.super_roles", "owner, root");

        let store = Permissions::from_config(offline(), &config).expect("valid configuration");

        assert_eq!(store.super_role_names().iter().collect::<Vec<_>>(), ["owner", "root"]);
    }

    #[test]
    fn an_absurd_ttl_is_clamped_rather_than_fatal() {
        for (configured, expected) in [(-1, 0u64), (999_999_999, 3_600_000)] {
            let config = Config::new();
            config.set("rbac.cache_ttl_ms", configured);

            let store = Permissions::from_config(offline(), &config).unwrap();
            assert_eq!(store.ttl, Duration::from_millis(expected));
        }
    }

    #[test]
    fn the_super_role_can_be_turned_off_entirely() {
        let store = Permissions::new(offline()).super_role("");

        assert!(store.super_role_names().is_empty());
    }

    #[test]
    fn a_bad_table_name_is_refused_when_the_store_is_built() {
        let names = TableNames { user_role: "user role".to_string(), ..TableNames::default() };

        assert!(Permissions::with_tables(offline(), names).is_err());
    }

    #[tokio::test]
    async fn a_primed_cache_answers_without_a_database() {
        // The handle points at a port nothing listens on, so any query would
        // fail: every answer below comes from the cache.
        let store = Permissions::new(offline());
        store.prime(41, grants(&["editor"], &["posts.*"], &["posts.delete"], false));

        assert!(store.has_role(41, "editor").await.unwrap());
        assert!(!store.has_role(41, "admin").await.unwrap());
        assert!(store.has_permission(41, "posts.publish").await.unwrap());
        assert!(!store.has_permission(41, "posts.delete").await.unwrap());
        assert_eq!(store.permissions_for(41).await.unwrap(), ["posts.*"]);
        assert_eq!(store.roles_for(41).await.unwrap(), ["editor"]);
    }

    #[tokio::test]
    async fn a_cache_miss_is_not_answered_with_a_guess() {
        // Nobody primed user 7, so the store has to go and look — and when it
        // cannot, the answer is an error, never `false` and never `true`.
        let store = Permissions::new(offline());

        assert!(store.has_permission(7, "users.create").await.is_err());
    }

    #[test]
    fn forget_drops_one_user_and_flush_drops_all() {
        let store = Permissions::new(offline());
        store.prime(1, grants(&[], &["a"], &[], false));
        store.prime(2, grants(&[], &["b"], &[], false));
        assert_eq!(store.cached_users(), 2);

        store.forget(1);
        assert_eq!(store.cached_users(), 1);

        store.flush();
        assert_eq!(store.cached_users(), 0);
    }

    #[tokio::test]
    async fn an_expired_entry_is_not_used() {
        let store = Permissions::new(offline()).cache_ttl(Duration::from_millis(20));
        store.prime(41, grants(&[], &["posts.publish"], &[], false));

        assert!(store.has_permission(41, "posts.publish").await.unwrap());

        tokio::time::sleep(Duration::from_millis(40)).await;

        // Expired: the store goes back to the database, which is not there.
        assert!(store.has_permission(41, "posts.publish").await.is_err());
    }

    #[test]
    fn a_name_with_edges_that_cannot_be_seen_is_refused() {
        assert!(validate_name("role", "admin").is_ok());
        assert!(validate_name("permission", "users.*").is_ok());
        assert!(validate_name("permission", "*").is_ok());

        assert!(validate_name("role", "").is_err());
        assert!(validate_name("role", " admin").is_err());
        assert!(validate_name("role", "admin ").is_err());
        assert!(validate_name("role", "ad\nmin").is_err());
        assert!(validate_name("role", &"x".repeat(256)).is_err());
    }

    #[test]
    fn a_boolean_column_is_read_whatever_shape_it_arrives_in() {
        assert!(truthy(&Value::Bool(true)));
        assert!(!truthy(&Value::Bool(false)));
        // MySQL's tinyint and SQL Server's bit.
        assert!(truthy(&Value::Int(1)));
        assert!(!truthy(&Value::Int(0)));
        assert!(truthy(&Value::Text("t".into())));
        assert!(!truthy(&Value::Text("f".into())));
        // Anything unrecognised is not a grant. Failing closed, here too.
        assert!(!truthy(&Value::Null));
    }
}
