//! The schema builder — what a migration writes.
//!
//! ```ignore
//! schema.create("users", |t| {
//!     t.id();
//!     t.string("name");
//!     t.string("email").unique();
//!     t.timestamps();
//! });
//! ```
//!
//! Every identifier is validated, so a migration cannot smuggle statements in
//! through a column name.

use crate::{Database, quote_identifier, validate_identifier};
use rustlavel_core::Result;

/// A column being defined.
#[derive(Debug, Clone)]
pub struct Column {
    name: String,
    sql_type: String,
    nullable: bool,
    unique: bool,
    primary: bool,
    default: Option<String>,
    /// `(table, column)` for a foreign key.
    references: Option<(String, String)>,
    on_delete: Option<&'static str>,
    index: bool,
}

impl Column {
    fn new(name: &str, sql_type: impl Into<String>) -> Self {
        Column {
            name: name.to_string(),
            sql_type: sql_type.into(),
            nullable: false,
            unique: false,
            primary: false,
            default: None,
            references: None,
            on_delete: None,
            index: false,
        }
    }

    /// Allow NULL. Columns are NOT NULL by default, which is the safer default
    /// and the opposite of what SQL gives you.
    pub fn nullable(&mut self) -> &mut Self {
        self.nullable = true;
        self
    }

    pub fn unique(&mut self) -> &mut Self {
        self.unique = true;
        self
    }

    pub fn primary(&mut self) -> &mut Self {
        self.primary = true;
        self
    }

    pub fn index(&mut self) -> &mut Self {
        self.index = true;
        self
    }

    /// A literal default. Strings are quoted; use [`Column::default_raw`] for
    /// an expression such as `now()`.
    pub fn default(&mut self, value: &str) -> &mut Self {
        self.default = Some(format!("'{}'", value.replace('\'', "''")));
        self
    }

    pub fn default_int(&mut self, value: i64) -> &mut Self {
        self.default = Some(value.to_string());
        self
    }

    pub fn default_bool(&mut self, value: bool) -> &mut Self {
        self.default = Some(if value { "true".into() } else { "false".into() });
        self
    }

    /// A default expression, written verbatim. Only migration authors reach
    /// this, never user input.
    pub fn default_raw(&mut self, expression: &str) -> &mut Self {
        self.default = Some(expression.to_string());
        self
    }

    /// A foreign key to another table's column.
    pub fn references(&mut self, table: &str, column: &str) -> &mut Self {
        self.references = Some((table.to_string(), column.to_string()));
        self
    }

    /// `on delete cascade` — only meaningful with [`Column::references`].
    pub fn cascade_on_delete(&mut self) -> &mut Self {
        self.on_delete = Some("cascade");
        self
    }

    pub fn null_on_delete(&mut self) -> &mut Self {
        self.on_delete = Some("set null");
        self
    }

    fn to_sql(&self) -> Result<String> {
        validate_identifier(&self.name)?;
        let mut sql = format!("\"{}\" {}", self.name, self.sql_type);

        if self.primary {
            sql.push_str(" primary key");
        } else if !self.nullable {
            sql.push_str(" not null");
        }

        if self.unique && !self.primary {
            sql.push_str(" unique");
        }

        if let Some(default) = &self.default {
            sql.push_str(&format!(" default {default}"));
        }

        if let Some((table, column)) = &self.references {
            validate_identifier(column)?;
            sql.push_str(&format!(
                " references {} (\"{column}\")",
                quote_identifier(table)?
            ));
            if let Some(action) = self.on_delete {
                sql.push_str(&format!(" on delete {action}"));
            }
        }

        Ok(sql)
    }
}

/// The table being created or altered.
#[derive(Default)]
pub struct Table {
    columns: Vec<Column>,
    /// Multi-column indexes and uniques, which a single column cannot express.
    indexes: Vec<(Vec<String>, bool)>,
    drops: Vec<String>,
}

impl Table {
    fn add(&mut self, column: Column) -> &mut Column {
        self.columns.push(column);
        self.columns.last_mut().expect("just pushed")
    }

    /// `id bigserial primary key` — the conventional key every table gets.
    pub fn id(&mut self) -> &mut Column {
        let mut column = Column::new("id", "bigserial");
        column.primary = true;
        self.add(column)
    }

    /// A UUID primary key, for tables whose ids are exposed publicly.
    pub fn uuid_id(&mut self) -> &mut Column {
        let mut column = Column::new("id", "uuid");
        column.primary = true;
        column.default = Some("gen_random_uuid()".into());
        self.add(column)
    }

    pub fn string(&mut self, name: &str) -> &mut Column {
        self.add(Column::new(name, "varchar(255)"))
    }

    pub fn string_with(&mut self, name: &str, length: u32) -> &mut Column {
        self.add(Column::new(name, format!("varchar({length})")))
    }

    pub fn text(&mut self, name: &str) -> &mut Column {
        self.add(Column::new(name, "text"))
    }

    pub fn integer(&mut self, name: &str) -> &mut Column {
        self.add(Column::new(name, "integer"))
    }

    pub fn big_integer(&mut self, name: &str) -> &mut Column {
        self.add(Column::new(name, "bigint"))
    }

    pub fn float(&mut self, name: &str) -> &mut Column {
        self.add(Column::new(name, "double precision"))
    }

    /// An exact decimal — money belongs here, never in a float.
    pub fn decimal(&mut self, name: &str, precision: u32, scale: u32) -> &mut Column {
        self.add(Column::new(name, format!("numeric({precision}, {scale})")))
    }

    pub fn boolean(&mut self, name: &str) -> &mut Column {
        self.add(Column::new(name, "boolean"))
    }

    pub fn json(&mut self, name: &str) -> &mut Column {
        self.add(Column::new(name, "jsonb"))
    }

    pub fn uuid(&mut self, name: &str) -> &mut Column {
        self.add(Column::new(name, "uuid"))
    }

    pub fn date(&mut self, name: &str) -> &mut Column {
        self.add(Column::new(name, "date"))
    }

    pub fn timestamp(&mut self, name: &str) -> &mut Column {
        self.add(Column::new(name, "timestamptz"))
    }

    pub fn binary(&mut self, name: &str) -> &mut Column {
        self.add(Column::new(name, "bytea"))
    }

    /// A foreign key column named after the table it points at:
    /// `t.foreign_id("user")` becomes `user_id bigint references users (id)`.
    pub fn foreign_id(&mut self, singular: &str) -> &mut Column {
        let name = format!("{singular}_id");
        let table = crate::migration::pluralize(singular);
        let mut column = Column::new(&name, "bigint");
        column.references = Some((table, "id".to_string()));
        column.index = true;
        self.add(column)
    }

    /// `created_at` and `updated_at`, both defaulting to now.
    pub fn timestamps(&mut self) {
        self.add(Column::new("created_at", "timestamptz")).default_raw("now()");
        self.add(Column::new("updated_at", "timestamptz")).default_raw("now()");
    }

    /// A nullable `deleted_at`, for soft deletes.
    pub fn soft_deletes(&mut self) {
        self.add(Column::new("deleted_at", "timestamptz")).nullable();
    }

    /// An index across several columns.
    pub fn index(&mut self, columns: &[&str]) {
        self.indexes.push((columns.iter().map(|c| (*c).to_string()).collect(), false));
    }

    pub fn unique(&mut self, columns: &[&str]) {
        self.indexes.push((columns.iter().map(|c| (*c).to_string()).collect(), true));
    }

    /// Drop a column. Only meaningful inside `alter`.
    pub fn drop_column(&mut self, name: &str) {
        self.drops.push(name.to_string());
    }
}

/// Emits DDL and runs it.
pub struct Schema<'a> {
    db: &'a Database,
}

impl<'a> Schema<'a> {
    pub fn new(db: &'a Database) -> Self {
        Schema { db }
    }

    /// Create a table.
    pub async fn create(&self, table: &str, define: impl FnOnce(&mut Table)) -> Result<()> {
        for statement in create_statements(table, define)? {
            self.db.run(&statement).await?;
        }
        Ok(())
    }

    /// Add or drop columns on an existing table.
    pub async fn alter(&self, table: &str, define: impl FnOnce(&mut Table)) -> Result<()> {
        for statement in alter_statements(table, define)? {
            self.db.run(&statement).await?;
        }
        Ok(())
    }

    pub async fn drop(&self, table: &str) -> Result<()> {
        self.db.run(&format!("drop table if exists {} cascade", quote_identifier(table)?)).await?;
        Ok(())
    }

    pub async fn rename(&self, from: &str, to: &str) -> Result<()> {
        self.db
            .run(&format!(
                "alter table {} rename to {}",
                quote_identifier(from)?,
                quote_identifier(to)?
            ))
            .await?;
        Ok(())
    }

    /// Whether a table exists — how the migration runner decides what to do.
    pub async fn has_table(&self, table: &str) -> Result<bool> {
        let found = self
            .db
            .scalar::<i64>(
                "select count(*) from information_schema.tables \
                 where table_schema = current_schema() and table_name = $1",
                &[crate::Value::from(table)],
            )
            .await?;
        Ok(found.unwrap_or(0) > 0)
    }

    pub async fn has_column(&self, table: &str, column: &str) -> Result<bool> {
        let found = self
            .db
            .scalar::<i64>(
                "select count(*) from information_schema.columns \
                 where table_schema = current_schema() and table_name = $1 and column_name = $2",
                &[crate::Value::from(table), crate::Value::from(column)],
            )
            .await?;
        Ok(found.unwrap_or(0) > 0)
    }
}

/// The statements a `create` produces: the table, then its indexes.
pub fn create_statements(table: &str, define: impl FnOnce(&mut Table)) -> Result<Vec<String>> {
    let mut definition = Table::default();
    define(&mut definition);

    let quoted = quote_identifier(table)?;
    let columns: Result<Vec<String>> = definition.columns.iter().map(Column::to_sql).collect();

    let mut statements =
        vec![format!("create table {quoted} (\n  {}\n)", columns?.join(",\n  "))];
    statements.extend(index_statements(table, &definition)?);
    Ok(statements)
}

/// The statements an `alter` produces.
pub fn alter_statements(table: &str, define: impl FnOnce(&mut Table)) -> Result<Vec<String>> {
    let mut definition = Table::default();
    define(&mut definition);

    let quoted = quote_identifier(table)?;
    let mut statements = Vec::new();

    for column in &definition.columns {
        statements.push(format!("alter table {quoted} add column {}", column.to_sql()?));
    }
    for name in &definition.drops {
        validate_identifier(name)?;
        statements.push(format!("alter table {quoted} drop column \"{name}\""));
    }

    statements.extend(index_statements(table, &definition)?);
    Ok(statements)
}

fn index_statements(table: &str, definition: &Table) -> Result<Vec<String>> {
    let quoted = quote_identifier(table)?;
    let mut statements = Vec::new();

    for column in definition.columns.iter().filter(|c| c.index) {
        validate_identifier(&column.name)?;
        statements.push(format!(
            "create index if not exists \"{table}_{}_index\" on {quoted} (\"{}\")",
            column.name, column.name
        ));
    }

    for (columns, unique) in &definition.indexes {
        for column in columns {
            validate_identifier(column)?;
        }
        let name = format!("{table}_{}_{}", columns.join("_"), if *unique { "unique" } else { "index" });
        validate_identifier(&name)?;
        let quoted_columns: Vec<String> = columns.iter().map(|c| format!("\"{c}\"")).collect();
        statements.push(format!(
            "create {}index if not exists \"{name}\" on {quoted} ({})",
            if *unique { "unique " } else { "" },
            quoted_columns.join(", ")
        ));
    }

    Ok(statements)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_a_create_table_statement() {
        let statements = create_statements("users", |t| {
            t.id();
            t.string("name");
            t.string("email").unique();
            t.boolean("active").default_bool(true);
            t.text("bio").nullable();
            t.timestamps();
        })
        .unwrap();

        assert_eq!(
            statements[0],
            "create table \"users\" (\n  \
             \"id\" bigserial primary key,\n  \
             \"name\" varchar(255) not null,\n  \
             \"email\" varchar(255) not null unique,\n  \
             \"active\" boolean not null default true,\n  \
             \"bio\" text,\n  \
             \"created_at\" timestamptz not null default now(),\n  \
             \"updated_at\" timestamptz not null default now()\n)"
        );
    }

    #[test]
    fn a_foreign_id_points_at_the_pluralized_table_and_gets_an_index() {
        let statements = create_statements("posts", |t| {
            t.id();
            t.foreign_id("user").cascade_on_delete();
        })
        .unwrap();

        assert!(statements[0].contains(
            "\"user_id\" bigint not null references \"users\" (\"id\") on delete cascade"
        ));
        assert_eq!(
            statements[1],
            "create index if not exists \"posts_user_id_index\" on \"posts\" (\"user_id\")"
        );
    }

    #[test]
    fn composite_indexes_get_their_own_statements() {
        let statements = create_statements("memberships", |t| {
            t.id();
            t.integer("team_id");
            t.integer("user_id");
            t.unique(&["team_id", "user_id"]);
        })
        .unwrap();

        assert_eq!(
            statements[1],
            "create unique index if not exists \"memberships_team_id_user_id_unique\" \
             on \"memberships\" (\"team_id\", \"user_id\")"
        );
    }

    #[test]
    fn alter_adds_and_drops_columns() {
        let statements = alter_statements("users", |t| {
            t.string("nickname").nullable();
            t.drop_column("legacy_flag");
        })
        .unwrap();

        assert_eq!(statements[0], "alter table \"users\" add column \"nickname\" varchar(255)");
        assert_eq!(statements[1], "alter table \"users\" drop column \"legacy_flag\"");
    }

    #[test]
    fn a_malicious_column_name_is_rejected() {
        let error = create_statements("users", |t| {
            t.string("name\"; drop table users; --");
        })
        .unwrap_err();

        assert!(error.to_string().contains("not a valid SQL identifier"));
    }

    #[test]
    fn a_string_default_is_quoted_and_escaped() {
        let statements = create_statements("t", |t| {
            t.string("motto").default("it's fine");
        })
        .unwrap();

        assert!(statements[0].contains("default 'it''s fine'"));
    }

    #[test]
    fn soft_deletes_add_a_nullable_timestamp() {
        let statements = create_statements("posts", |t| {
            t.id();
            t.soft_deletes();
        })
        .unwrap();

        assert!(statements[0].contains("\"deleted_at\" timestamptz"));
        assert!(!statements[0].contains("\"deleted_at\" timestamptz not null"));
    }
}
