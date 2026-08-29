//! The query builder — `DB::table("users").where(...).get()` in Rust.
//!
//! Every value goes out as a bound parameter and every identifier is validated
//! and quoted, so a builder chain cannot produce injectable SQL even when the
//! column name came from user input.

use crate::value::Value;
use crate::dialect::{Dialect, ReturningStyle, quote_qualified, validate_identifier};
use crate::{Database, Row};
use rustlavel_core::{Error, Json, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Asc,
    Desc,
}

impl Direction {
    fn as_sql(self) -> &'static str {
        match self {
            Direction::Asc => "asc",
            Direction::Desc => "desc",
        }
    }
}

/// One condition in a `where` clause.
#[derive(Debug, Clone)]
enum Condition {
    Comparison { column: String, operator: String, value: Value, or: bool },
    In { column: String, values: Vec<Value>, negated: bool, or: bool },
    Null { column: String, negated: bool, or: bool },
    Between { column: String, low: Value, high: Value, or: bool },
    /// A nested group, so `where(a).where(|q| q.or(b).or(c))` keeps its meaning.
    Group { conditions: Vec<Condition>, or: bool },
}

impl Condition {
    fn is_or(&self) -> bool {
        match self {
            Condition::Comparison { or, .. }
            | Condition::In { or, .. }
            | Condition::Null { or, .. }
            | Condition::Between { or, .. }
            | Condition::Group { or, .. } => *or,
        }
    }
}

#[derive(Debug, Clone)]
struct Join {
    kind: &'static str,
    table: String,
    left: String,
    operator: String,
    right: String,
}

/// A statement being assembled.
#[derive(Debug, Clone)]
pub struct QueryBuilder {
    table: String,
    columns: Vec<String>,
    conditions: Vec<Condition>,
    joins: Vec<Join>,
    order: Vec<(String, Direction)>,
    group: Vec<String>,
    limit: Option<i64>,
    offset: Option<i64>,
    distinct: bool,
}

impl QueryBuilder {
    pub fn new(table: impl Into<String>) -> Self {
        QueryBuilder {
            table: table.into(),
            columns: Vec::new(),
            conditions: Vec::new(),
            joins: Vec::new(),
            order: Vec::new(),
            group: Vec::new(),
            limit: None,
            offset: None,
            distinct: false,
        }
    }

    /// Choose columns. Without this the query selects everything.
    pub fn select(mut self, columns: &[&str]) -> Self {
        self.columns = columns.iter().map(|c| (*c).to_string()).collect();
        self
    }

    pub fn distinct(mut self) -> Self {
        self.distinct = true;
        self
    }

    /// `where column = value`.
    pub fn filter(self, column: &str, value: impl Into<Value>) -> Self {
        self.filter_op(column, "=", value)
    }

    /// `where column <operator> value`, with the operator checked against a
    /// list — an operator cannot be smuggled in from user input.
    pub fn filter_op(mut self, column: &str, operator: &str, value: impl Into<Value>) -> Self {
        self.conditions.push(Condition::Comparison {
            column: column.to_string(),
            operator: operator.to_string(),
            value: value.into(),
            or: false,
        });
        self
    }

    pub fn or_filter(mut self, column: &str, value: impl Into<Value>) -> Self {
        self.conditions.push(Condition::Comparison {
            column: column.to_string(),
            operator: "=".to_string(),
            value: value.into(),
            or: true,
        });
        self
    }

    pub fn filter_in(mut self, column: &str, values: Vec<Value>) -> Self {
        self.conditions.push(Condition::In {
            column: column.to_string(),
            values,
            negated: false,
            or: false,
        });
        self
    }

    pub fn filter_not_in(mut self, column: &str, values: Vec<Value>) -> Self {
        self.conditions.push(Condition::In {
            column: column.to_string(),
            values,
            negated: true,
            or: false,
        });
        self
    }

    pub fn filter_null(mut self, column: &str) -> Self {
        self.conditions.push(Condition::Null { column: column.to_string(), negated: false, or: false });
        self
    }

    pub fn filter_not_null(mut self, column: &str) -> Self {
        self.conditions.push(Condition::Null { column: column.to_string(), negated: true, or: false });
        self
    }

    pub fn filter_between(mut self, column: &str, low: impl Into<Value>, high: impl Into<Value>) -> Self {
        self.conditions.push(Condition::Between {
            column: column.to_string(),
            low: low.into(),
            high: high.into(),
            or: false,
        });
        self
    }

    /// `where column like pattern`.
    pub fn filter_like(self, column: &str, pattern: impl Into<Value>) -> Self {
        self.filter_op(column, "like", pattern)
    }

    /// A parenthesised group of conditions.
    pub fn group_filter(mut self, build: impl FnOnce(QueryBuilder) -> QueryBuilder) -> Self {
        let nested = build(QueryBuilder::new(self.table.clone()));
        if !nested.conditions.is_empty() {
            self.conditions.push(Condition::Group { conditions: nested.conditions, or: false });
        }
        self
    }

    pub fn or_group_filter(mut self, build: impl FnOnce(QueryBuilder) -> QueryBuilder) -> Self {
        let nested = build(QueryBuilder::new(self.table.clone()));
        if !nested.conditions.is_empty() {
            self.conditions.push(Condition::Group { conditions: nested.conditions, or: true });
        }
        self
    }

    pub fn join(self, table: &str, left: &str, operator: &str, right: &str) -> Self {
        self.add_join("inner join", table, left, operator, right)
    }

    pub fn left_join(self, table: &str, left: &str, operator: &str, right: &str) -> Self {
        self.add_join("left join", table, left, operator, right)
    }

    fn add_join(
        mut self,
        kind: &'static str,
        table: &str,
        left: &str,
        operator: &str,
        right: &str,
    ) -> Self {
        self.joins.push(Join {
            kind,
            table: table.to_string(),
            left: left.to_string(),
            operator: operator.to_string(),
            right: right.to_string(),
        });
        self
    }

    pub fn order_by(mut self, column: &str, direction: Direction) -> Self {
        self.order.push((column.to_string(), direction));
        self
    }

    pub fn latest(self, column: &str) -> Self {
        self.order_by(column, Direction::Desc)
    }

    pub fn group_by(mut self, columns: &[&str]) -> Self {
        self.group = columns.iter().map(|c| (*c).to_string()).collect();
        self
    }

    pub fn limit(mut self, limit: i64) -> Self {
        self.limit = Some(limit);
        self
    }

    pub fn offset(mut self, offset: i64) -> Self {
        self.offset = Some(offset);
        self
    }

    /// Limit and offset for a 1-based page.
    pub fn page(self, page: i64, per_page: i64) -> Self {
        let page = page.max(1);
        self.limit(per_page).offset((page - 1) * per_page)
    }

    // --- SQL generation ---

    /// Build the `select` statement and its parameters for one database.
    ///
    /// The dialect supplies the quoting, the placeholders and the paging
    /// syntax, so the same builder chain is correct on PostgreSQL, MySQL and
    /// SQL Server without the caller knowing which is underneath.
    pub fn to_sql(&self, dialect: &dyn Dialect) -> Result<(String, Vec<Value>)> {
        let mut params = Vec::new();
        let mut sql = String::from("select ");

        if self.distinct {
            sql.push_str("distinct ");
        }

        if self.columns.is_empty() {
            sql.push('*');
        } else {
            let rendered: Result<Vec<String>> =
                self.columns.iter().map(|c| column_ref(dialect, c)).collect();
            sql.push_str(&rendered?.join(", "));
        }

        sql.push_str(" from ");
        sql.push_str(&quote_qualified(dialect, &self.table)?);

        for join in &self.joins {
            check_operator(&join.operator)?;
            sql.push_str(&format!(
                " {} {} on {} {} {}",
                join.kind,
                quote_qualified(dialect, &join.table)?,
                column_ref(dialect, &join.left)?,
                join.operator,
                column_ref(dialect, &join.right)?
            ));
        }

        if !self.conditions.is_empty() {
            sql.push_str(" where ");
            sql.push_str(&render_conditions(dialect, &self.conditions, &mut params)?);
        }

        if !self.group.is_empty() {
            let rendered: Result<Vec<String>> =
                self.group.iter().map(|c| column_ref(dialect, c)).collect();
            sql.push_str(&format!(" group by {}", rendered?.join(", ")));
        }

        if !self.order.is_empty() {
            let rendered: Result<Vec<String>> = self
                .order
                .iter()
                .map(|(column, direction)| {
                    Ok(format!("{} {}", column_ref(dialect, column)?, direction.as_sql()))
                })
                .collect();
            sql.push_str(&format!(" order by {}", rendered?.join(", ")));
        }

        // Limit and offset are numbers the builder controls, never user text.
        // The dialect decides how they are spelled, and whether an ordering has
        // to be invented for them to be legal.
        sql.push_str(&dialect.limit_offset(self.limit, self.offset, !self.order.is_empty()));

        Ok((sql, params))
    }

    /// The `select count(*)` form of this query, ignoring order and paging.
    pub fn to_count_sql(&self, dialect: &dyn Dialect) -> Result<(String, Vec<Value>)> {
        let counting = QueryBuilder {
            columns: vec!["count(*) as aggregate".to_string()],
            order: Vec::new(),
            limit: None,
            offset: None,
            ..self.clone()
        };
        counting.to_sql(dialect)
    }

    fn to_insert_sql(
        &self,
        dialect: &dyn Dialect,
        rows: &[Vec<(String, Value)>],
        returning: Option<&str>,
    ) -> Result<(String, Vec<Value>)> {
        let first = rows.first().ok_or_else(|| Error::msg("insert needs at least one row"))?;
        let columns: Vec<String> = first.iter().map(|(name, _)| name.clone()).collect();

        let rendered: Result<Vec<String>> = columns
            .iter()
            .map(|c| {
                validate_identifier(c, dialect.max_identifier_length()).map(|_| dialect.quote(c))
            })
            .collect();

        let mut params = Vec::new();
        let mut placeholders = Vec::new();

        for row in rows {
            if row.len() != columns.len() {
                return Err(Error::msg(
                    "every row in a bulk insert must have the same columns".to_string(),
                ));
            }
            let mut slots = Vec::with_capacity(row.len());
            for (name, value) in row {
                if !columns.contains(name) {
                    return Err(Error::msg(format!(
                        "column `{name}` is missing from the first row of this insert"
                    )));
                }
                params.push(value.clone());
                slots.push(dialect.placeholder(params.len()));
            }
            placeholders.push(format!("({})", slots.join(", ")));
        }

        let table = quote_qualified(dialect, &self.table)?;
        let columns_sql = rendered?.join(", ");
        let values_sql = placeholders.join(", ");

        // Where the "hand the new key back" clause goes is structural, not
        // cosmetic: PostgreSQL appends it, SQL Server puts it before the column
        // list, and MySQL has no clause at all.
        let sql = match (returning, dialect.returning()) {
            (None, _) | (Some(_), ReturningStyle::SeparateQuery(_)) => {
                format!("insert into {table} ({columns_sql}) values {values_sql}")
            }
            (Some(column), ReturningStyle::Suffix) => {
                validate_identifier(column, dialect.max_identifier_length())?;
                format!(
                    "insert into {table} ({columns_sql}) values {values_sql} returning {}",
                    dialect.quote(column)
                )
            }
            (Some(column), ReturningStyle::OutputClause) => {
                validate_identifier(column, dialect.max_identifier_length())?;
                // T-SQL puts OUTPUT between the column list and VALUES. Before
                // the column list it parses as a second column list and fails
                // with a count mismatch — which a real server confirmed.
                format!(
                    "insert into {table} ({columns_sql}) output inserted.{} values {values_sql}",
                    dialect.quote(column)
                )
            }
        };

        Ok((sql, params))
    }

    fn to_update_sql(
        &self,
        dialect: &dyn Dialect,
        values: &[(String, Value)],
    ) -> Result<(String, Vec<Value>)> {
        if values.is_empty() {
            return Err(Error::msg("update needs at least one column"));
        }

        let mut params = Vec::new();
        let mut assignments = Vec::with_capacity(values.len());

        for (column, value) in values {
            validate_identifier(column, dialect.max_identifier_length())?;
            params.push(value.clone());
            assignments.push(format!(
                "{} = {}",
                dialect.quote(column),
                dialect.placeholder(params.len())
            ));
        }

        let mut sql = format!(
            "update {} set {}",
            quote_qualified(dialect, &self.table)?,
            assignments.join(", ")
        );

        if self.conditions.is_empty() {
            // An unfiltered update rewrites the whole table. Requiring an
            // explicit filter turns a catastrophe into a compile-time-ish error.
            return Err(Error::msg(
                "refusing to update every row: add a filter, or call `update_all` if that is really intended"
                    .to_string(),
            ));
        }

        sql.push_str(" where ");
        sql.push_str(&render_conditions(dialect, &self.conditions, &mut params)?);
        Ok((sql, params))
    }

    fn to_delete_sql(&self, dialect: &dyn Dialect, allow_all: bool) -> Result<(String, Vec<Value>)> {
        let mut params = Vec::new();
        let mut sql = format!("delete from {}", quote_qualified(dialect, &self.table)?);

        if self.conditions.is_empty() {
            if !allow_all {
                return Err(Error::msg(
                    "refusing to delete every row: add a filter, or call `delete_all` if that is really intended"
                        .to_string(),
                ));
            }
        } else {
            sql.push_str(" where ");
            sql.push_str(&render_conditions(dialect, &self.conditions, &mut params)?);
        }

        Ok((sql, params))
    }

    // --- Execution ---

    /// Run the query and return every matching row.
    pub async fn get(&self, db: &Database) -> Result<Vec<Row>> {
        let (sql, params) = self.to_sql(db.dialect())?;
        db.select(&sql, &params).await
    }

    /// The first matching row, if any.
    pub async fn first(&self, db: &Database) -> Result<Option<Row>> {
        let (sql, params) = self.clone().limit(1).to_sql(db.dialect())?;
        db.select_one(&sql, &params).await
    }

    /// Rows as a JSON array, ready to return from an API handler.
    pub async fn get_json(&self, db: &Database) -> Result<Json> {
        Ok(crate::rows_to_json(&self.get(db).await?))
    }

    pub async fn count(&self, db: &Database) -> Result<i64> {
        let (sql, params) = self.to_count_sql(db.dialect())?;
        Ok(db.scalar::<i64>(&sql, &params).await?.unwrap_or(0))
    }

    pub async fn exists(&self, db: &Database) -> Result<bool> {
        Ok(self.count(db).await? > 0)
    }

    /// Insert one row, returning its `id`.
    pub async fn insert(&self, db: &Database, values: &[(&str, Value)]) -> Result<i64> {
        let row: Vec<(String, Value)> =
            values.iter().map(|(name, value)| ((*name).to_string(), value.clone())).collect();
        let (sql, params) =
            self.to_insert_sql(db.dialect(), std::slice::from_ref(&row), Some("id"))?;

        db.select_one(&sql, &params)
            .await?
            .ok_or_else(|| Error::msg("insert returned no id"))?
            .get_at::<i64>(0)
    }

    /// Insert one row and return one column of it — how a model picks up the
    /// key the database generated.
    pub async fn insert_returning(
        &self,
        db: &Database,
        values: &[(&str, Value)],
        column: &str,
    ) -> Result<Value> {
        let row: Vec<(String, Value)> =
            values.iter().map(|(name, value)| ((*name).to_string(), value.clone())).collect();
        let (sql, params) =
            self.to_insert_sql(db.dialect(), std::slice::from_ref(&row), Some(column))?;

        let returned = db
            .select_one(&sql, &params)
            .await?
            .ok_or_else(|| Error::msg(format!("insert did not return `{column}`")))?;
        returned.value(column).cloned()
    }

    /// Insert one row without asking for a generated key.
    pub async fn insert_without_id(&self, db: &Database, values: &[(&str, Value)]) -> Result<u64> {
        let row: Vec<(String, Value)> =
            values.iter().map(|(name, value)| ((*name).to_string(), value.clone())).collect();
        let (sql, params) = self.to_insert_sql(db.dialect(), std::slice::from_ref(&row), None)?;
        db.execute(&sql, &params).await
    }

    /// Insert many rows in one statement.
    pub async fn insert_many(&self, db: &Database, rows: &[Vec<(String, Value)>]) -> Result<u64> {
        if rows.is_empty() {
            return Ok(0);
        }
        let (sql, params) = self.to_insert_sql(db.dialect(), rows, None)?;
        db.execute(&sql, &params).await
    }

    /// Update the rows this query matches. Requires a filter.
    pub async fn update(&self, db: &Database, values: &[(&str, Value)]) -> Result<u64> {
        let owned: Vec<(String, Value)> =
            values.iter().map(|(name, value)| ((*name).to_string(), value.clone())).collect();
        let (sql, params) = self.to_update_sql(db.dialect(), &owned)?;
        db.execute(&sql, &params).await
    }

    /// Delete the rows this query matches. Requires a filter.
    pub async fn delete(&self, db: &Database) -> Result<u64> {
        let (sql, params) = self.to_delete_sql(db.dialect(), false)?;
        db.execute(&sql, &params).await
    }

    /// Delete every row in the table. Deliberately separate from `delete`.
    pub async fn delete_all(&self, db: &Database) -> Result<u64> {
        let (sql, params) = self.to_delete_sql(db.dialect(), true)?;
        db.execute(&sql, &params).await
    }
}

/// Render a condition list, threading parameter numbering through it.
fn render_conditions(
    dialect: &dyn Dialect,
    conditions: &[Condition],
    params: &mut Vec<Value>,
) -> Result<String> {
    let mut out = String::new();

    for (index, condition) in conditions.iter().enumerate() {
        if index > 0 {
            out.push_str(if condition.is_or() { " or " } else { " and " });
        }

        match condition {
            Condition::Comparison { column, operator, value, .. } => {
                check_operator(operator)?;
                params.push(value.clone());
                out.push_str(&format!(
                    "{} {operator} {}",
                    column_ref(dialect, column)?,
                    dialect.placeholder(params.len())
                ));
            }
            Condition::In { column, values, negated, .. } => {
                if values.is_empty() {
                    // `in ()` is a syntax error; an empty set matches nothing.
                    out.push_str(if *negated { "true" } else { "false" });
                    continue;
                }
                let mut slots = Vec::with_capacity(values.len());
                for value in values {
                    params.push(value.clone());
                    slots.push(dialect.placeholder(params.len()));
                }
                out.push_str(&format!(
                    "{} {}in ({})",
                    column_ref(dialect, column)?,
                    if *negated { "not " } else { "" },
                    slots.join(", ")
                ));
            }
            Condition::Null { column, negated, .. } => {
                out.push_str(&format!(
                    "{} is {}null",
                    column_ref(dialect, column)?,
                    if *negated { "not " } else { "" }
                ));
            }
            Condition::Between { column, low, high, .. } => {
                params.push(low.clone());
                let low_slot = dialect.placeholder(params.len());
                params.push(high.clone());
                out.push_str(&format!(
                    "{} between {low_slot} and {}",
                    column_ref(dialect, column)?,
                    dialect.placeholder(params.len())
                ));
            }
            Condition::Group { conditions, .. } => {
                out.push_str(&format!("({})", render_conditions(dialect, conditions, params)?));
            }
        }
    }

    Ok(out)
}

/// The operators a builder is allowed to emit.
///
/// An allowlist rather than escaping: there is no legitimate reason for an
/// operator to be anything else, and a typo becomes an error instead of SQL.
fn check_operator(operator: &str) -> Result<()> {
    const ALLOWED: &[&str] = &[
        "=", "!=", "<>", "<", "<=", ">", ">=", "like", "not like", "ilike", "not ilike", "@>", "<@",
        "?", "is distinct from",
    ];

    if ALLOWED.contains(&operator.to_ascii_lowercase().as_str()) {
        Ok(())
    } else {
        Err(Error::msg(format!(
            "`{operator}` is not an allowed comparison operator. Allowed: {}",
            ALLOWED.join(", ")
        )))
    }
}

/// Render a column reference, which may be qualified (`users.id`), aliased
/// (`count(*) as aggregate` is passed through) or plain.
fn column_ref(dialect: &dyn Dialect, column: &str) -> Result<String> {
    if column == "*" {
        return Ok("*".to_string());
    }

    // The builder generates a handful of aggregate expressions itself; anything
    // else must be a plain identifier.
    if let Some(rest) = column.strip_prefix("count(*) as ") {
        validate_identifier(rest, dialect.max_identifier_length())?;
        return Ok(format!("count(*) as {}", dialect.quote(rest)));
    }

    for prefix in ["count", "sum", "avg", "min", "max"] {
        if let Some(inner) = column
            .strip_prefix(&format!("{prefix}("))
            .and_then(|rest| rest.strip_suffix(')'))
        {
            let rendered =
                if inner == "*" { "*".to_string() } else { quote_qualified(dialect, inner)? };
            return Ok(format!("{prefix}({rendered})"));
        }
    }

    quote_qualified(dialect, column)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::{MySql, Postgres, SqlServer};

    /// The default in these tests, since the SQL they assert is PostgreSQL's.
    /// The dialect-specific behaviour is covered by dialect.rs and by the
    /// cross-dialect tests at the end of this module.
    fn sql_of(builder: QueryBuilder) -> (String, Vec<Value>) {
        builder.to_sql(&Postgres).unwrap()
    }

    #[test]
    fn builds_a_plain_select() {
        let (sql, params) = sql_of(QueryBuilder::new("users"));

        assert_eq!(sql, r#"select * from "users""#);
        assert!(params.is_empty());
    }

    #[test]
    fn values_become_numbered_parameters() {
        let (sql, params) = sql_of(
            QueryBuilder::new("users")
                .select(&["id", "name"])
                .filter("email", "ada@example.com")
                .filter_op("age", ">", 18),
        );

        assert_eq!(
            sql,
            r#"select "id", "name" from "users" where "email" = $1 and "age" > $2"#
        );
        assert_eq!(params, vec![Value::Text("ada@example.com".into()), Value::Int(18)]);
    }

    #[test]
    fn a_value_can_never_change_the_statement() {
        let (sql, params) = sql_of(QueryBuilder::new("users").filter("name", "'; drop table users; --"));

        assert_eq!(sql, r#"select * from "users" where "name" = $1"#);
        assert_eq!(params[0], Value::Text("'; drop table users; --".into()));
    }

    #[test]
    fn an_injected_identifier_is_rejected() {
        let error = QueryBuilder::new("users").filter("name; drop table users", 1).to_sql(&Postgres).unwrap_err();
        assert!(error.to_string().contains("not a valid SQL identifier"));

        assert!(QueryBuilder::new("users; drop table users").to_sql(&Postgres).is_err());
    }

    #[test]
    fn an_unknown_operator_is_rejected() {
        let error = QueryBuilder::new("users").filter_op("id", "; drop", 1).to_sql(&Postgres).unwrap_err();
        assert!(error.to_string().contains("not an allowed comparison operator"));
    }

    #[test]
    fn renders_in_null_and_between() {
        let (sql, params) = sql_of(
            QueryBuilder::new("posts")
                .filter_in("status", vec![Value::from("draft"), Value::from("live")])
                .filter_not_null("published_at")
                .filter_between("views", 10, 100),
        );

        assert_eq!(
            sql,
            r#"select * from "posts" where "status" in ($1, $2) and "published_at" is not null and "views" between $3 and $4"#
        );
        assert_eq!(params.len(), 4);
    }

    #[test]
    fn an_empty_in_list_matches_nothing_instead_of_breaking() {
        let (sql, _) = sql_of(QueryBuilder::new("posts").filter_in("id", vec![]));
        assert_eq!(sql, r#"select * from "posts" where false"#);
    }

    #[test]
    fn groups_keep_or_conditions_together() {
        let (sql, params) = sql_of(
            QueryBuilder::new("users")
                .filter("active", true)
                .group_filter(|q| q.filter("role", "admin").or_filter("role", "owner")),
        );

        assert_eq!(
            sql,
            r#"select * from "users" where "active" = $1 and ("role" = $2 or "role" = $3)"#
        );
        assert_eq!(params.len(), 3);
    }

    #[test]
    fn joins_order_and_paging() {
        let (sql, _) = sql_of(
            QueryBuilder::new("posts")
                .select(&["posts.title", "users.name"])
                .join("users", "posts.user_id", "=", "users.id")
                .latest("posts.created_at")
                .page(3, 20),
        );

        assert_eq!(
            sql,
            r#"select "posts"."title", "users"."name" from "posts" inner join "users" on "posts"."user_id" = "users"."id" order by "posts"."created_at" desc limit 20 offset 40"#
        );
    }

    #[test]
    fn counting_drops_order_and_paging() {
        let (sql, _) = QueryBuilder::new("users")
            .filter("active", true)
            .latest("created_at")
            .page(2, 10)
            .to_count_sql(&Postgres)
            .unwrap();

        assert_eq!(
            sql,
            r#"select count(*) as "aggregate" from "users" where "active" = $1"#
        );
    }

    #[test]
    fn builds_an_insert_with_a_returning_clause() {
        let row = vec![
            ("name".to_string(), Value::from("Ada")),
            ("email".to_string(), Value::from("ada@example.com")),
        ];
        let (sql, params) =
            QueryBuilder::new("users").to_insert_sql(&Postgres, std::slice::from_ref(&row), Some("id")).unwrap();

        assert_eq!(
            sql,
            r#"insert into "users" ("name", "email") values ($1, $2) returning "id""#
        );
        assert_eq!(params.len(), 2);
    }

    #[test]
    fn a_bulk_insert_numbers_every_row() {
        let rows = vec![
            vec![("name".to_string(), Value::from("a"))],
            vec![("name".to_string(), Value::from("b"))],
        ];
        let (sql, params) = QueryBuilder::new("users").to_insert_sql(&Postgres, &rows, None).unwrap();

        assert_eq!(sql, r#"insert into "users" ("name") values ($1), ($2)"#);
        assert_eq!(params.len(), 2);
    }

    #[test]
    fn update_and_delete_refuse_to_touch_every_row_by_accident() {
        let values = vec![("name".to_string(), Value::from("x"))];

        let error = QueryBuilder::new("users").to_update_sql(&Postgres, &values).unwrap_err();
        assert!(error.to_string().contains("refusing to update every row"));

        let error = QueryBuilder::new("users").to_delete_sql(&Postgres, false).unwrap_err();
        assert!(error.to_string().contains("refusing to delete every row"));

        // The explicit forms are allowed.
        assert!(QueryBuilder::new("users").to_delete_sql(&Postgres, true).is_ok());
        assert!(
            QueryBuilder::new("users")
                .filter("id", 1)
                .to_update_sql(&Postgres, &values)
                .is_ok()
        );
    }

    #[test]
    fn one_chain_produces_correct_sql_for_every_database() {
        let query = QueryBuilder::new("users")
            .select(&["id", "name"])
            .filter("active", true)
            .latest("created_at")
            .page(2, 10);

        let (postgres, _) = query.to_sql(&Postgres).unwrap();
        assert_eq!(
            postgres,
            r#"select "id", "name" from "users" where "active" = $1 order by "created_at" desc limit 10 offset 10"#
        );

        let (mysql, _) = query.to_sql(&MySql).unwrap();
        assert_eq!(
            mysql,
            "select `id`, `name` from `users` where `active` = ? order by `created_at` desc limit 10 offset 10"
        );

        let (sqlserver, _) = query.to_sql(&SqlServer).unwrap();
        assert_eq!(
            sqlserver,
            "select [id], [name] from [users] where [active] = @P1 order by [created_at] desc offset 10 rows fetch next 10 rows only"
        );
    }

    #[test]
    fn parameters_are_numbered_or_not_according_to_the_database() {
        let query = QueryBuilder::new("posts").filter("a", 1).filter("b", 2).filter("c", 3);

        let (postgres, params) = query.to_sql(&Postgres).unwrap();
        assert!(postgres.ends_with("$1 and \"b\" = $2 and \"c\" = $3"), "{postgres}");
        assert_eq!(params.len(), 3);

        // MySQL binds positionally, so every placeholder is the same token and
        // the order of the parameter list is the only thing that matters.
        let (mysql, params) = query.to_sql(&MySql).unwrap();
        assert!(mysql.ends_with("? and `b` = ? and `c` = ?"), "{mysql}");
        assert_eq!(params.len(), 3);
    }

    #[test]
    fn a_generated_key_is_asked_for_in_each_databases_own_way() {
        let row = vec![("name".to_string(), Value::from("Ada"))];
        let rows = std::slice::from_ref(&row);

        let (postgres, _) =
            QueryBuilder::new("users").to_insert_sql(&Postgres, rows, Some("id")).unwrap();
        assert!(postgres.ends_with(r#"returning "id""#), "{postgres}");

        // SQL Server puts it before the column list, so it cannot be appended.
        let (sqlserver, _) =
            QueryBuilder::new("users").to_insert_sql(&SqlServer, rows, Some("id")).unwrap();
        assert_eq!(
            sqlserver,
            "insert into [users] ([name]) output inserted.[id] values (@P1)"
        );

        // MySQL has no such clause; the key is read with a second statement.
        let (mysql, _) = QueryBuilder::new("users").to_insert_sql(&MySql, rows, Some("id")).unwrap();
        assert_eq!(mysql, "insert into `users` (`name`) values (?)");
    }

    #[test]
    fn sql_server_gets_an_ordering_it_can_page_with() {
        // `offset` is a syntax error without `order by`, and the builder
        // supplies one rather than letting the database complain.
        let (sql, _) = QueryBuilder::new("users").page(3, 20).to_sql(&SqlServer).unwrap();

        assert!(sql.contains("order by (select null)"), "{sql}");
        assert!(sql.ends_with("offset 40 rows fetch next 20 rows only"), "{sql}");
    }

    #[test]
    fn an_injected_identifier_is_rejected_whatever_the_database() {
        for dialect in [&Postgres as &dyn Dialect, &MySql, &SqlServer] {
            let error = QueryBuilder::new("users")
                .filter("name; drop table users", 1)
                .to_sql(dialect)
                .unwrap_err();

            assert!(
                error.to_string().contains("not a valid SQL identifier"),
                "{} accepted it",
                dialect.name()
            );
        }
    }

    #[test]
    fn an_update_numbers_values_before_conditions() {
        let values = vec![("name".to_string(), Value::from("Ada"))];
        let (sql, params) =
            QueryBuilder::new("users").filter("id", 7).to_update_sql(&Postgres, &values).unwrap();

        assert_eq!(sql, r#"update "users" set "name" = $1 where "id" = $2"#);
        assert_eq!(params, vec![Value::Text("Ada".into()), Value::Int(7)]);
    }
}
