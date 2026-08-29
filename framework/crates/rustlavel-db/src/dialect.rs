//! What differs between one SQL database and another.
//!
//! The query builder, the schema builder and the migrator are written once; a
//! [`Dialect`] supplies the handful of things the databases genuinely disagree
//! about — how an identifier is quoted, what a bound parameter looks like, what
//! a column type is called, and how a generated key is read back.
//!
//! Everything a dialect answers is pure string generation, so all three are
//! tested without a database anywhere near them.

use rustlavel_core::{Error, Result};

/// A column type as the schema builder thinks of it, before any database has
/// had an opinion.
///
/// Logical rather than literal: `Timestamp` is `timestamptz` on PostgreSQL,
/// `datetime(6)` on MySQL and `datetime2` on SQL Server, and a migration should
/// not have to know that.
#[derive(Debug, Clone, PartialEq)]
pub enum ColumnType {
    /// The conventional auto-incrementing primary key.
    Id,
    /// A UUID primary key, defaulted by the database.
    UuidId,
    SmallInteger,
    Integer,
    BigInteger,
    /// Approximate; for money use [`ColumnType::Decimal`].
    Float,
    Decimal { precision: u32, scale: u32 },
    Boolean,
    String { length: u32 },
    Text,
    Json,
    Uuid,
    Date,
    Time,
    Timestamp,
    Binary,
    /// An escape hatch for a type the framework does not model.
    Raw(String),
}

/// How a database hands back the key it generated for an inserted row.
#[derive(Debug, Clone, PartialEq)]
pub enum ReturningStyle {
    /// `insert into … values (…) returning "id"` — PostgreSQL.
    Suffix,
    /// `insert into … (…) output inserted.[id] values (…)` — SQL Server puts it
    /// between the column list and `values`, so it cannot be appended.
    OutputClause,
    /// Not supported: the key is read with a second statement. MySQL.
    SeparateQuery(&'static str),
}

/// The differences between one SQL database and another.
pub trait Dialect: Send + Sync + std::fmt::Debug + 'static {
    /// `postgres`, `mysql`, `sqlserver`.
    fn name(&self) -> &'static str;

    /// Wrap one identifier so a keyword or an unusual name is still valid.
    ///
    /// The identifier has already been validated; this only quotes it.
    fn quote(&self, identifier: &str) -> String;

    /// The placeholder for the `position`-th bound parameter, counting from 1.
    fn placeholder(&self, position: usize) -> String;

    /// The type name for a logical column type.
    fn column_type(&self, kind: &ColumnType) -> String;

    /// The expression for "now", used by `timestamps()`.
    fn now(&self) -> &'static str;

    /// The expression that generates a UUID, when the database has one.
    fn uuid_default(&self) -> Option<&'static str>;

    /// How a generated key comes back from an insert.
    fn returning(&self) -> ReturningStyle;

    /// `limit … offset …`, in whatever form this database accepts.
    ///
    /// `ordered` says whether the query already has an `order by`, because SQL
    /// Server's paging syntax requires one and will not accept paging without.
    fn limit_offset(&self, limit: Option<i64>, offset: Option<i64>, ordered: bool) -> String;

    /// Whether `create table if not exists` is understood.
    fn supports_if_not_exists_table(&self) -> bool {
        true
    }

    /// Whether `create index if not exists` is understood.
    ///
    /// PostgreSQL has it; MySQL and SQL Server do not, so the schema builder
    /// emits a plain `create index` and a repeated migration would fail — which
    /// is correct, since migrations run once.
    fn supports_if_not_exists_index(&self) -> bool {
        false
    }

    /// Whether a `boolean` column really is one.
    ///
    /// MySQL stores it as `tinyint(1)` and SQL Server as `bit`, so both hand
    /// back a number where PostgreSQL hands back a boolean. The row decoder
    /// uses this to convert.
    fn booleans_are_integers(&self) -> bool {
        false
    }

    /// The longest identifier this database accepts.
    fn max_identifier_length(&self) -> usize {
        63
    }

    /// The DDL for the migration tracking table.
    ///
    /// The key column has to say `primary key`: MySQL refuses an
    /// `auto_increment` column that is not one, and a real server is the only
    /// thing that will tell you so.
    fn migrations_table_sql(&self, table: &str) -> String {
        format!(
            "create table if not exists {} (\n  \
             id {} primary key,\n  \
             name {} not null unique,\n  \
             batch {} not null,\n  \
             ran_at {} not null default {}\n)",
            self.quote(table),
            self.column_type(&ColumnType::Id),
            self.column_type(&ColumnType::String { length: 255 }),
            self.column_type(&ColumnType::Integer),
            self.column_type(&ColumnType::Timestamp),
            self.now()
        )
    }

    /// The statement that drops every table, for `migrate:fresh`.
    fn drop_all_tables_sql(&self) -> String;
}

/// Quote a possibly-qualified name one part at a time.
pub fn quote_qualified(dialect: &dyn Dialect, name: &str) -> Result<String> {
    let parts: Result<Vec<String>> = name
        .split('.')
        .map(|part| {
            validate_identifier(part, dialect.max_identifier_length())
                .map(|_| dialect.quote(part))
        })
        .collect();
    Ok(parts?.join("."))
}

/// Reject anything that is not a plain identifier.
///
/// Identifiers cannot be sent as bound parameters, so every place the framework
/// interpolates one into SQL passes through here first.
pub fn validate_identifier(name: &str, max_length: usize) -> Result<()> {
    let valid = !name.is_empty()
        && name.len() <= max_length
        && name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');

    if valid {
        Ok(())
    } else {
        Err(Error::msg(format!(
            "`{name}` is not a valid SQL identifier. Identifiers may contain letters, digits and \
             underscores, must not start with a digit, and must be at most {max_length} characters."
        )))
    }
}

// --- PostgreSQL ---

#[derive(Debug, Default, Clone, Copy)]
pub struct Postgres;

impl Dialect for Postgres {
    fn name(&self) -> &'static str {
        "postgres"
    }

    fn quote(&self, identifier: &str) -> String {
        format!("\"{identifier}\"")
    }

    fn placeholder(&self, position: usize) -> String {
        format!("${position}")
    }

    fn column_type(&self, kind: &ColumnType) -> String {
        match kind {
            ColumnType::Id => "bigserial".into(),
            ColumnType::UuidId | ColumnType::Uuid => "uuid".into(),
            ColumnType::SmallInteger => "smallint".into(),
            ColumnType::Integer => "integer".into(),
            ColumnType::BigInteger => "bigint".into(),
            ColumnType::Float => "double precision".into(),
            ColumnType::Decimal { precision, scale } => format!("numeric({precision}, {scale})"),
            ColumnType::Boolean => "boolean".into(),
            ColumnType::String { length } => format!("varchar({length})"),
            ColumnType::Text => "text".into(),
            ColumnType::Json => "jsonb".into(),
            ColumnType::Date => "date".into(),
            ColumnType::Time => "time".into(),
            ColumnType::Timestamp => "timestamptz".into(),
            ColumnType::Binary => "bytea".into(),
            ColumnType::Raw(sql) => sql.clone(),
        }
    }

    fn now(&self) -> &'static str {
        "now()"
    }

    fn uuid_default(&self) -> Option<&'static str> {
        Some("gen_random_uuid()")
    }

    fn returning(&self) -> ReturningStyle {
        ReturningStyle::Suffix
    }

    fn limit_offset(&self, limit: Option<i64>, offset: Option<i64>, _ordered: bool) -> String {
        let mut out = String::new();
        if let Some(limit) = limit {
            out.push_str(&format!(" limit {}", limit.max(0)));
        }
        if let Some(offset) = offset {
            out.push_str(&format!(" offset {}", offset.max(0)));
        }
        out
    }

    fn supports_if_not_exists_index(&self) -> bool {
        true
    }

    fn drop_all_tables_sql(&self) -> String {
        "do $$ declare r record; begin \
         for r in (select tablename from pg_tables where schemaname = current_schema()) loop \
         execute 'drop table if exists ' || quote_ident(r.tablename) || ' cascade'; \
         end loop; end $$"
            .into()
    }
}

// --- MySQL ---

#[derive(Debug, Default, Clone, Copy)]
pub struct MySql;

impl Dialect for MySql {
    fn name(&self) -> &'static str {
        "mysql"
    }

    fn quote(&self, identifier: &str) -> String {
        format!("`{identifier}`")
    }

    fn placeholder(&self, _position: usize) -> String {
        // MySQL binds by position in order, not by number.
        "?".into()
    }

    fn column_type(&self, kind: &ColumnType) -> String {
        match kind {
            ColumnType::Id => "bigint unsigned not null auto_increment".into(),
            // MySQL has no uuid type; 36 characters holds the canonical form.
            ColumnType::UuidId | ColumnType::Uuid => "char(36)".into(),
            ColumnType::SmallInteger => "smallint".into(),
            ColumnType::Integer => "int".into(),
            ColumnType::BigInteger => "bigint".into(),
            ColumnType::Float => "double".into(),
            ColumnType::Decimal { precision, scale } => format!("decimal({precision}, {scale})"),
            // `boolean` is an alias for tinyint(1); spelled out so the schema
            // says what the database actually stores.
            ColumnType::Boolean => "tinyint(1)".into(),
            ColumnType::String { length } => format!("varchar({length})"),
            ColumnType::Text => "text".into(),
            ColumnType::Json => "json".into(),
            ColumnType::Date => "date".into(),
            ColumnType::Time => "time".into(),
            // Fractional seconds are not the default and cannot be added later
            // without rewriting the table.
            ColumnType::Timestamp => "datetime(6)".into(),
            ColumnType::Binary => "longblob".into(),
            ColumnType::Raw(sql) => sql.clone(),
        }
    }

    fn now(&self) -> &'static str {
        "current_timestamp(6)"
    }

    fn uuid_default(&self) -> Option<&'static str> {
        // Only from MySQL 8.0.13, and only in an expression default; left off
        // so the schema builder does not emit something a 5.7 server rejects.
        None
    }

    fn returning(&self) -> ReturningStyle {
        ReturningStyle::SeparateQuery("select last_insert_id()")
    }

    fn limit_offset(&self, limit: Option<i64>, offset: Option<i64>, _ordered: bool) -> String {
        let mut out = String::new();
        match (limit, offset) {
            // MySQL cannot offset without a limit, so an offset alone gets the
            // largest limit the syntax allows.
            (None, Some(offset)) => {
                out.push_str(&format!(" limit 18446744073709551615 offset {}", offset.max(0)));
            }
            (Some(limit), offset) => {
                out.push_str(&format!(" limit {}", limit.max(0)));
                if let Some(offset) = offset {
                    out.push_str(&format!(" offset {}", offset.max(0)));
                }
            }
            (None, None) => {}
        }
        out
    }

    fn booleans_are_integers(&self) -> bool {
        true
    }

    fn max_identifier_length(&self) -> usize {
        64
    }

    fn drop_all_tables_sql(&self) -> String {
        // MySQL has no anonymous block, so the runner disables key checks and
        // drops what it finds. The driver runs this as several statements.
        "set foreign_key_checks = 0".into()
    }
}

// --- SQL Server ---

#[derive(Debug, Default, Clone, Copy)]
pub struct SqlServer;

impl Dialect for SqlServer {
    fn name(&self) -> &'static str {
        "sqlserver"
    }

    fn quote(&self, identifier: &str) -> String {
        format!("[{identifier}]")
    }

    fn placeholder(&self, position: usize) -> String {
        format!("@P{position}")
    }

    fn column_type(&self, kind: &ColumnType) -> String {
        match kind {
            ColumnType::Id => "bigint identity(1,1)".into(),
            ColumnType::UuidId | ColumnType::Uuid => "uniqueidentifier".into(),
            ColumnType::SmallInteger => "smallint".into(),
            ColumnType::Integer => "int".into(),
            ColumnType::BigInteger => "bigint".into(),
            ColumnType::Float => "float".into(),
            ColumnType::Decimal { precision, scale } => format!("decimal({precision}, {scale})"),
            ColumnType::Boolean => "bit".into(),
            // `n` prefixed: the framework speaks UTF-8, and nvarchar is the type
            // that stores it without a collation surprise.
            ColumnType::String { length } => format!("nvarchar({length})"),
            ColumnType::Text | ColumnType::Json => "nvarchar(max)".into(),
            ColumnType::Date => "date".into(),
            ColumnType::Time => "time".into(),
            ColumnType::Timestamp => "datetime2".into(),
            ColumnType::Binary => "varbinary(max)".into(),
            ColumnType::Raw(sql) => sql.clone(),
        }
    }

    fn now(&self) -> &'static str {
        "sysutcdatetime()"
    }

    fn uuid_default(&self) -> Option<&'static str> {
        Some("newid()")
    }

    fn returning(&self) -> ReturningStyle {
        ReturningStyle::OutputClause
    }

    fn limit_offset(&self, limit: Option<i64>, offset: Option<i64>, ordered: bool) -> String {
        if limit.is_none() && offset.is_none() {
            return String::new();
        }

        // `offset … fetch next …` is only legal after an `order by`, so an
        // unordered paged query gets a placeholder ordering rather than a
        // syntax error the caller cannot explain.
        let mut out = String::new();
        if !ordered {
            out.push_str(" order by (select null)");
        }
        out.push_str(&format!(" offset {} rows", offset.unwrap_or(0).max(0)));
        if let Some(limit) = limit {
            out.push_str(&format!(" fetch next {} rows only", limit.max(0)));
        }
        out
    }

    fn supports_if_not_exists_table(&self) -> bool {
        false
    }

    fn booleans_are_integers(&self) -> bool {
        true
    }

    fn max_identifier_length(&self) -> usize {
        128
    }

    fn migrations_table_sql(&self, table: &str) -> String {
        // No `if not exists`; the catalogue is checked instead.
        format!(
            "if object_id('{table}', 'U') is null create table {} (\n  \
             [id] bigint identity(1,1) primary key,\n  \
             [name] nvarchar(255) not null unique,\n  \
             [batch] int not null,\n  \
             [ran_at] datetime2 not null default sysutcdatetime()\n)",
            self.quote(table)
        )
    }

    fn drop_all_tables_sql(&self) -> String {
        "exec sp_MSforeachtable 'alter table ? nocheck constraint all'; \
         exec sp_MSforeachtable 'drop table ?'"
            .into()
    }
}

/// Build a dialect from its name.
pub fn by_name(name: &str) -> Result<Box<dyn Dialect>> {
    match name.to_ascii_lowercase().as_str() {
        "postgres" | "postgresql" | "pgsql" => Ok(Box::new(Postgres)),
        "mysql" | "mariadb" => Ok(Box::new(MySql)),
        "sqlserver" | "mssql" => Ok(Box::new(SqlServer)),
        other => Err(Error::msg(format!(
            "`{other}` is not a database this framework speaks. Available: postgres, mysql, sqlserver."
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all() -> Vec<Box<dyn Dialect>> {
        vec![Box::new(Postgres), Box::new(MySql), Box::new(SqlServer)]
    }

    #[test]
    fn each_dialect_quotes_the_way_its_database_expects() {
        assert_eq!(Postgres.quote("users"), "\"users\"");
        assert_eq!(MySql.quote("users"), "`users`");
        assert_eq!(SqlServer.quote("users"), "[users]");
    }

    #[test]
    fn placeholders_differ_in_kind_not_just_spelling() {
        assert_eq!(Postgres.placeholder(1), "$1");
        assert_eq!(Postgres.placeholder(3), "$3");

        // MySQL binds positionally, so every placeholder is the same token.
        assert_eq!(MySql.placeholder(1), "?");
        assert_eq!(MySql.placeholder(3), "?");

        assert_eq!(SqlServer.placeholder(3), "@P3");
    }

    #[test]
    fn a_qualified_name_is_quoted_one_part_at_a_time() {
        assert_eq!(
            quote_qualified(&Postgres, "public.users").unwrap(),
            "\"public\".\"users\""
        );
        assert_eq!(quote_qualified(&MySql, "shop.orders").unwrap(), "`shop`.`orders`");
        assert_eq!(quote_qualified(&SqlServer, "dbo.users").unwrap(), "[dbo].[users]");
    }

    #[test]
    fn an_injected_identifier_is_rejected_by_every_dialect() {
        for dialect in all() {
            for hostile in ["users; drop table users", "a b", "1abc", "", "us\"er"] {
                assert!(
                    quote_qualified(dialect.as_ref(), hostile).is_err(),
                    "{} accepted {hostile:?}",
                    dialect.name()
                );
            }
        }
    }

    #[test]
    fn identifier_length_limits_follow_the_database() {
        let long = "a".repeat(100);

        assert!(validate_identifier(&long, Postgres.max_identifier_length()).is_err());
        assert!(validate_identifier(&long, MySql.max_identifier_length()).is_err());
        assert!(validate_identifier(&long, SqlServer.max_identifier_length()).is_ok());
    }

    #[test]
    fn the_key_column_is_auto_incrementing_everywhere() {
        assert_eq!(Postgres.column_type(&ColumnType::Id), "bigserial");
        assert_eq!(MySql.column_type(&ColumnType::Id), "bigint unsigned not null auto_increment");
        assert_eq!(SqlServer.column_type(&ColumnType::Id), "bigint identity(1,1)");
    }

    #[test]
    fn text_and_json_map_to_what_each_database_actually_has() {
        assert_eq!(Postgres.column_type(&ColumnType::Json), "jsonb");
        assert_eq!(MySql.column_type(&ColumnType::Json), "json");
        // SQL Server has no JSON type; it stores the document as text.
        assert_eq!(SqlServer.column_type(&ColumnType::Json), "nvarchar(max)");
    }

    #[test]
    fn a_string_column_carries_its_length_everywhere() {
        let kind = ColumnType::String { length: 120 };

        assert_eq!(Postgres.column_type(&kind), "varchar(120)");
        assert_eq!(MySql.column_type(&kind), "varchar(120)");
        assert_eq!(SqlServer.column_type(&kind), "nvarchar(120)");
    }

    #[test]
    fn paging_uses_each_databases_own_syntax() {
        assert_eq!(Postgres.limit_offset(Some(10), Some(20), true), " limit 10 offset 20");
        assert_eq!(MySql.limit_offset(Some(10), Some(20), true), " limit 10 offset 20");
        assert_eq!(
            SqlServer.limit_offset(Some(10), Some(20), true),
            " offset 20 rows fetch next 10 rows only"
        );
    }

    #[test]
    fn sql_server_supplies_an_ordering_when_paging_has_none() {
        // `offset` is a syntax error without `order by`, and a caller cannot
        // debug an error the builder could have avoided.
        let paged = SqlServer.limit_offset(Some(10), None, false);
        assert!(paged.starts_with(" order by (select null)"), "{paged}");

        // With an ordering already present, none is added.
        assert!(!SqlServer.limit_offset(Some(10), None, true).contains("order by"));
    }

    #[test]
    fn mysql_cannot_offset_without_a_limit() {
        let offset_only = MySql.limit_offset(None, Some(20), true);

        assert!(offset_only.contains("limit 18446744073709551615"), "{offset_only}");
        assert!(offset_only.ends_with("offset 20"));
    }

    #[test]
    fn no_paging_produces_no_clause() {
        for dialect in all() {
            assert_eq!(dialect.limit_offset(None, None, true), "", "{}", dialect.name());
        }
    }

    #[test]
    fn generated_keys_come_back_differently() {
        assert_eq!(Postgres.returning(), ReturningStyle::Suffix);
        assert_eq!(SqlServer.returning(), ReturningStyle::OutputClause);
        assert_eq!(
            MySql.returning(),
            ReturningStyle::SeparateQuery("select last_insert_id()")
        );
    }

    #[test]
    fn the_migration_table_is_valid_for_each_database() {
        let postgres = Postgres.migrations_table_sql("rustlavel_migrations");
        assert!(postgres.contains("create table if not exists \"rustlavel_migrations\""));
        assert!(postgres.contains("bigserial primary key"));

        let mysql = MySql.migrations_table_sql("rustlavel_migrations");
        assert!(mysql.contains("`rustlavel_migrations`"));
        // MySQL rejects an auto_increment column that is not a key.
        assert!(mysql.contains("auto_increment primary key"), "{mysql}");

        // SQL Server has no `if not exists`, so it checks the catalogue.
        let sqlserver = SqlServer.migrations_table_sql("rustlavel_migrations");
        assert!(sqlserver.starts_with("if object_id("));
        assert!(sqlserver.contains("identity(1,1)"));
    }

    #[test]
    fn dialects_are_found_by_the_names_people_use() {
        for (name, expected) in [
            ("postgres", "postgres"),
            ("postgresql", "postgres"),
            ("mysql", "mysql"),
            ("mariadb", "mysql"),
            ("sqlserver", "sqlserver"),
            ("mssql", "sqlserver"),
            ("MySQL", "mysql"),
        ] {
            assert_eq!(by_name(name).unwrap().name(), expected, "for {name}");
        }
    }

    #[test]
    fn an_unknown_database_lists_the_ones_that_exist() {
        let error = by_name("oracle").unwrap_err().to_string();

        assert!(error.contains("postgres, mysql, sqlserver"), "{error}");
    }
}
