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

use crate::dialect::{ColumnType, Dialect, quote_qualified, validate_identifier};
use crate::Database;
use rustlavel_core::Result;

/// A column's default value.
///
/// `Now` and `Uuid` stay symbolic until the statement is rendered, because only
/// then is it known whether the expression is `now()`, `current_timestamp(6)`
/// or `sysutcdatetime()`.
#[derive(Debug, Clone)]
enum Default {
    /// Already rendered, including any quoting.
    Literal(String),
    Now,
    Uuid,
}

/// A column being defined.
#[derive(Debug, Clone)]
pub struct Column {
    name: String,
    kind: ColumnType,
    nullable: bool,
    unique: bool,
    primary: bool,
    default: Option<Default>,
    /// `(table, column)` for a foreign key.
    references: Option<(String, String)>,
    on_delete: Option<&'static str>,
    index: bool,
}

impl Column {
    fn new(name: &str, kind: ColumnType) -> Self {
        Column {
            name: name.to_string(),
            kind,
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
        self.default = Some(Default::Literal(format!("'{}'", value.replace('\'', "''"))));
        self
    }

    pub fn default_int(&mut self, value: i64) -> &mut Self {
        self.default = Some(Default::Literal(value.to_string()));
        self
    }

    /// MySQL and SQL Server store a boolean as a number, so `true` is written
    /// as `1` where `true` would not parse.
    pub fn default_bool(&mut self, value: bool) -> &mut Self {
        self.default = Some(Default::Literal(if value { "TRUE_LITERAL" } else { "FALSE_LITERAL" }.into()));
        self
    }

    /// Default to the current time, in whatever the database calls it.
    pub fn default_now(&mut self) -> &mut Self {
        self.default = Some(Default::Now);
        self
    }

    /// A default expression, written verbatim. Only migration authors reach
    /// this, never user input.
    pub fn default_raw(&mut self, expression: &str) -> &mut Self {
        self.default = Some(Default::Literal(expression.to_string()));
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

    fn to_sql(&self, dialect: &dyn Dialect) -> Result<String> {
        validate_identifier(&self.name, dialect.max_identifier_length())?;
        let mut sql = format!("{} {}", dialect.quote(&self.name), dialect.column_type(&self.kind));

        if self.primary {
            sql.push_str(" primary key");
        } else if !self.nullable {
            sql.push_str(" not null");
        }

        if self.unique && !self.primary {
            sql.push_str(" unique");
        }

        if let Some(default) = &self.default {
            let rendered = match default {
                Default::Now => dialect.now().to_string(),
                Default::Uuid => match dialect.uuid_default() {
                    Some(expression) => expression.to_string(),
                    // MySQL has no portable one, so the application supplies
                    // the value rather than the schema pretending otherwise.
                    None => return Err(rustlavel_core::Error::msg(format!(
                        "`{}` cannot default a uuid column: {} has no expression for it. \
                         Generate the id in the application instead.",
                        self.name,
                        dialect.name()
                    ))),
                },
                Default::Literal(literal) if literal == "TRUE_LITERAL" => {
                    if dialect.booleans_are_integers() { "1".into() } else { "true".into() }
                }
                Default::Literal(literal) if literal == "FALSE_LITERAL" => {
                    if dialect.booleans_are_integers() { "0".into() } else { "false".into() }
                }
                Default::Literal(literal) => literal.clone(),
            };
            sql.push_str(&format!(" default {rendered}"));
        }

        // No inline `references` here: MySQL parses one and silently creates
        // no constraint at all, so foreign keys are emitted as table-level
        // constraints by `foreign_key_sql` instead.

        Ok(sql)
    }

    /// The table-level constraint for this column's foreign key, if it has one.
    fn foreign_key_sql(&self, dialect: &dyn Dialect, table: &str) -> Result<Option<String>> {
        let Some((target, target_column)) = &self.references else { return Ok(None) };

        let limit = dialect.max_identifier_length();
        validate_identifier(target_column, limit)?;

        // Laravel's naming, so a constraint can be found by the column it is on.
        let name = format!("{table}_{}_foreign", self.name);
        validate_identifier(&name, limit)?;

        let mut sql = format!(
            "constraint {} foreign key ({}) references {} ({})",
            dialect.quote(&name),
            dialect.quote(&self.name),
            quote_qualified(dialect, target)?,
            dialect.quote(target_column)
        );
        if let Some(action) = self.on_delete {
            sql.push_str(&format!(" on delete {action}"));
        }
        Ok(Some(sql))
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
        let mut column = Column::new("id", ColumnType::Id);
        column.primary = true;
        self.add(column)
    }

    /// A UUID primary key, for tables whose ids are exposed publicly.
    pub fn uuid_id(&mut self) -> &mut Column {
        let mut column = Column::new("id", ColumnType::UuidId);
        column.primary = true;
        column.default = Some(Default::Uuid);
        self.add(column)
    }

    pub fn string(&mut self, name: &str) -> &mut Column {
        self.add(Column::new(name, ColumnType::String { length: 255 }))
    }

    pub fn string_with(&mut self, name: &str, length: u32) -> &mut Column {
        self.add(Column::new(name, ColumnType::String { length }))
    }

    pub fn text(&mut self, name: &str) -> &mut Column {
        self.add(Column::new(name, ColumnType::Text))
    }

    pub fn integer(&mut self, name: &str) -> &mut Column {
        self.add(Column::new(name, ColumnType::Integer))
    }

    pub fn big_integer(&mut self, name: &str) -> &mut Column {
        self.add(Column::new(name, ColumnType::BigInteger))
    }

    pub fn float(&mut self, name: &str) -> &mut Column {
        self.add(Column::new(name, ColumnType::Float))
    }

    /// An exact decimal — money belongs here, never in a float.
    pub fn decimal(&mut self, name: &str, precision: u32, scale: u32) -> &mut Column {
        self.add(Column::new(name, ColumnType::Decimal { precision, scale }))
    }

    pub fn boolean(&mut self, name: &str) -> &mut Column {
        self.add(Column::new(name, ColumnType::Boolean))
    }

    pub fn json(&mut self, name: &str) -> &mut Column {
        self.add(Column::new(name, ColumnType::Json))
    }

    pub fn uuid(&mut self, name: &str) -> &mut Column {
        self.add(Column::new(name, ColumnType::Uuid))
    }

    pub fn date(&mut self, name: &str) -> &mut Column {
        self.add(Column::new(name, ColumnType::Date))
    }

    pub fn timestamp(&mut self, name: &str) -> &mut Column {
        self.add(Column::new(name, ColumnType::Timestamp))
    }

    pub fn binary(&mut self, name: &str) -> &mut Column {
        self.add(Column::new(name, ColumnType::Binary))
    }

    /// A foreign key column named after the table it points at:
    /// `t.foreign_id("user")` becomes `user_id bigint references users (id)`.
    pub fn foreign_id(&mut self, singular: &str) -> &mut Column {
        let name = format!("{singular}_id");
        let table = crate::migration::pluralize(singular);
        let mut column = Column::new(&name, ColumnType::BigInteger);
        column.references = Some((table, "id".to_string()));
        column.index = true;
        self.add(column)
    }

    /// `created_at` and `updated_at`, both defaulting to now.
    ///
    /// The expression is filled in when the statements are rendered, because
    /// only then is the database known.
    pub fn timestamps(&mut self) {
        self.add(Column::new("created_at", ColumnType::Timestamp)).default_now();
        self.add(Column::new("updated_at", ColumnType::Timestamp)).default_now();
    }

    /// A nullable `deleted_at`, for soft deletes.
    pub fn soft_deletes(&mut self) {
        self.add(Column::new("deleted_at", ColumnType::Timestamp)).nullable();
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
        for statement in create_statements(self.db.dialect(), table, define)? {
            self.db.run(&statement).await?;
        }
        Ok(())
    }

    /// Add or drop columns on an existing table.
    pub async fn alter(&self, table: &str, define: impl FnOnce(&mut Table)) -> Result<()> {
        for statement in alter_statements(self.db.dialect(), table, define)? {
            self.db.run(&statement).await?;
        }
        Ok(())
    }

    pub async fn drop(&self, table: &str) -> Result<()> {
        let dialect = self.db.dialect();
        let quoted = quote_qualified(dialect, table)?;
        // `cascade` is PostgreSQL's; the others drop dependent constraints on
        // their own terms.
        let sql = match dialect.name() {
            "postgres" => format!("drop table if exists {quoted} cascade"),
            "sqlserver" => format!("drop table if exists {quoted}"),
            _ => format!("drop table if exists {quoted}"),
        };
        self.db.run(&sql).await?;
        Ok(())
    }

    pub async fn rename(&self, from: &str, to: &str) -> Result<()> {
        self.db
            .run(&format!(
                "alter table {} rename to {}",
                quote_qualified(self.db.dialect(), from)?,
                quote_qualified(self.db.dialect(), to)?
            ))
            .await?;
        Ok(())
    }

    /// Whether a table exists — how the migration runner decides what to do.
    pub async fn has_table(&self, table: &str) -> Result<bool> {
        let dialect = self.db.dialect();
        let found = self
            .db
            .scalar::<i64>(
                &format!(
                    "select count(*) from information_schema.tables \
                     where table_schema = {} and table_name = {}",
                    dialect.current_schema_expression(),
                    dialect.placeholder(1)
                ),
                &[crate::Value::from(table)],
            )
            .await?;
        Ok(found.unwrap_or(0) > 0)
    }

    pub async fn has_column(&self, table: &str, column: &str) -> Result<bool> {
        let dialect = self.db.dialect();
        let found = self
            .db
            .scalar::<i64>(
                &format!(
                    "select count(*) from information_schema.columns \
                     where table_schema = {} and table_name = {} and column_name = {}",
                    dialect.current_schema_expression(),
                    dialect.placeholder(1),
                    dialect.placeholder(2)
                ),
                &[crate::Value::from(table), crate::Value::from(column)],
            )
            .await?;
        Ok(found.unwrap_or(0) > 0)
    }
}

/// The statements a `create` produces: the table, then its indexes.
pub fn create_statements(
    dialect: &dyn Dialect,
    table: &str,
    define: impl FnOnce(&mut Table),
) -> Result<Vec<String>> {
    let mut definition = Table::default();
    define(&mut definition);

    let quoted = quote_qualified(dialect, table)?;
    let mut lines: Vec<String> = definition
        .columns
        .iter()
        .map(|column| column.to_sql(dialect))
        .collect::<Result<_>>()?;

    for column in &definition.columns {
        if let Some(constraint) = column.foreign_key_sql(dialect, table)? {
            lines.push(constraint);
        }
    }

    let mut statements = vec![format!("create table {quoted} (\n  {}\n)", lines.join(",\n  "))];
    statements.extend(index_statements(dialect, table, &definition)?);
    Ok(statements)
}

/// The statements an `alter` produces.
pub fn alter_statements(
    dialect: &dyn Dialect,
    table: &str,
    define: impl FnOnce(&mut Table),
) -> Result<Vec<String>> {
    let mut definition = Table::default();
    define(&mut definition);

    let quoted = quote_qualified(dialect, table)?;
    let mut statements = Vec::new();

    for column in &definition.columns {
        statements.push(format!(
            "alter table {quoted} {} {}",
            dialect.add_column_clause(),
            column.to_sql(dialect)?
        ));
        // The constraint is a second statement: a column has to exist before
        // anything can be declared about it.
        if let Some(constraint) = column.foreign_key_sql(dialect, table)? {
            statements.push(format!("alter table {quoted} add {constraint}"));
        }
    }
    for name in &definition.drops {
        validate_identifier(name, dialect.max_identifier_length())?;
        statements.push(format!("alter table {quoted} drop column {}", dialect.quote(name)));
    }

    statements.extend(index_statements(dialect, table, &definition)?);
    Ok(statements)
}

fn index_statements(
    dialect: &dyn Dialect,
    table: &str,
    definition: &Table,
) -> Result<Vec<String>> {
    let quoted = quote_qualified(dialect, table)?;
    // Only PostgreSQL understands `if not exists` on an index. Everywhere else
    // a repeated create is an error — which is correct, since a migration runs
    // exactly once.
    let guard = if dialect.supports_if_not_exists_index() { "if not exists " } else { "" };
    let limit = dialect.max_identifier_length();
    let mut statements = Vec::new();

    for column in definition.columns.iter().filter(|c| c.index) {
        validate_identifier(&column.name, limit)?;
        let name = format!("{table}_{}_index", column.name);
        validate_identifier(&name, limit)?;
        statements.push(format!(
            "create index {guard}{} on {quoted} ({})",
            dialect.quote(&name),
            dialect.quote(&column.name)
        ));
    }

    for (columns, unique) in &definition.indexes {
        for column in columns {
            validate_identifier(column, limit)?;
        }
        let name =
            format!("{table}_{}_{}", columns.join("_"), if *unique { "unique" } else { "index" });
        validate_identifier(&name, limit)?;
        let quoted_columns: Vec<String> = columns.iter().map(|c| dialect.quote(c)).collect();
        statements.push(format!(
            "create {}index {guard}{} on {quoted} ({})",
            if *unique { "unique " } else { "" },
            dialect.quote(&name),
            quoted_columns.join(", ")
        ));
    }

    Ok(statements)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::{MySql, Postgres, SqlServer};

    #[test]
    fn builds_a_create_table_statement() {
        let statements = create_statements(&Postgres, "users", |t| {
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
        let statements = create_statements(&Postgres, "posts", |t| {
            t.id();
            t.foreign_id("user").cascade_on_delete();
        })
        .unwrap();

        // A table-level constraint, not an inline `references`: MySQL parses an
        // inline one and silently creates no foreign key at all.
        assert!(statements[0].contains("\"user_id\" bigint not null,"), "{}", statements[0]);
        assert!(
            statements[0].contains(
                "constraint \"posts_user_id_foreign\" foreign key (\"user_id\") \
                 references \"users\" (\"id\") on delete cascade"
            ),
            "{}",
            statements[0]
        );
        assert_eq!(
            statements[1],
            "create index if not exists \"posts_user_id_index\" on \"posts\" (\"user_id\")"
        );
    }

    #[test]
    fn composite_indexes_get_their_own_statements() {
        let statements = create_statements(&Postgres, "memberships", |t| {
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
        let statements = alter_statements(&Postgres, "users", |t| {
            t.string("nickname").nullable();
            t.drop_column("legacy_flag");
        })
        .unwrap();

        assert_eq!(statements[0], "alter table \"users\" add column \"nickname\" varchar(255)");
        assert_eq!(statements[1], "alter table \"users\" drop column \"legacy_flag\"");

        // SQL Server rejects `add column` but requires `drop column`.
        let sqlserver = alter_statements(&SqlServer, "users", |t| {
            t.string("nickname").nullable();
            t.drop_column("legacy_flag");
        })
        .unwrap();
        assert_eq!(sqlserver[0], "alter table [users] add [nickname] nvarchar(255)");
        assert_eq!(sqlserver[1], "alter table [users] drop column [legacy_flag]");
    }

    #[test]
    fn a_malicious_column_name_is_rejected() {
        let error = create_statements(&Postgres, "users", |t| {
            t.string("name\"; drop table users; --");
        })
        .unwrap_err();

        assert!(error.to_string().contains("not a valid SQL identifier"));
    }

    #[test]
    fn a_string_default_is_quoted_and_escaped() {
        let statements = create_statements(&Postgres, "t", |t| {
            t.string("motto").default("it's fine");
        })
        .unwrap();

        assert!(statements[0].contains("default 'it''s fine'"));
    }

    #[test]
    fn one_schema_definition_produces_correct_ddl_for_every_database() {
        let define = |t: &mut Table| {
            t.id();
            t.string("email").unique();
            t.boolean("active").default_bool(true);
            t.timestamps();
        };

        let postgres = create_statements(&Postgres, "users", define).unwrap();
        assert!(postgres[0].contains("\"id\" bigserial primary key"), "{}", postgres[0]);
        assert!(postgres[0].contains("\"active\" boolean not null default true"));
        assert!(postgres[0].contains("default now()"));

        let mysql = create_statements(&MySql, "users", define).unwrap();
        assert!(
            mysql[0].contains("`id` bigint not null auto_increment primary key"),
            "{}",
            mysql[0]
        );
        // MySQL stores a boolean as a number, so `true` would not parse.
        assert!(mysql[0].contains("`active` tinyint(1) not null default 1"), "{}", mysql[0]);
        assert!(mysql[0].contains("default current_timestamp(6)"));

        let sqlserver = create_statements(&SqlServer, "users", define).unwrap();
        assert!(sqlserver[0].contains("[id] bigint identity(1,1) primary key"), "{}", sqlserver[0]);
        assert!(sqlserver[0].contains("[active] bit not null default 1"), "{}", sqlserver[0]);
        assert!(sqlserver[0].contains("default sysutcdatetime()"));
    }

    #[test]
    fn only_postgres_guards_an_index_with_if_not_exists() {
        let define = |t: &mut Table| {
            t.id();
            t.integer("team_id").index();
        };

        assert!(create_statements(&Postgres, "m", define).unwrap()[1].contains("if not exists"));
        // Elsewhere a repeated create is an error, which is correct: a
        // migration runs exactly once.
        assert!(!create_statements(&MySql, "m", define).unwrap()[1].contains("if not exists"));
        assert!(!create_statements(&SqlServer, "m", define).unwrap()[1].contains("if not exists"));
    }

    #[test]
    fn a_uuid_default_mysql_cannot_express_is_refused_rather_than_faked() {
        let define = |t: &mut Table| {
            t.uuid_id();
        };

        assert!(create_statements(&Postgres, "t", define).unwrap()[0].contains("gen_random_uuid()"));
        assert!(create_statements(&SqlServer, "t", define).unwrap()[0].contains("newid()"));

        let error = create_statements(&MySql, "t", define).unwrap_err().to_string();
        assert!(error.contains("has no expression for it"), "{error}");
    }

    #[test]
    fn soft_deletes_add_a_nullable_timestamp() {
        let statements = create_statements(&Postgres, "posts", |t| {
            t.id();
            t.soft_deletes();
        })
        .unwrap();

        assert!(statements[0].contains("\"deleted_at\" timestamptz"));
        assert!(!statements[0].contains("\"deleted_at\" timestamptz not null"));
    }
}
