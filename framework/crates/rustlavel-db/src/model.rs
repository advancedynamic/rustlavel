//! The ORM: `#[derive(Model)]` plus the active-record methods it unlocks.
//!
//! Laravel's Eloquent resolves everything at runtime — column names, relations,
//! and attributes all live in arrays. Here the derive reads the struct at
//! compile time, so a renamed column is a compile error rather than a `null`
//! that shows up in production.

use crate::builder::QueryBuilder;
use crate::value::{FromValue, Value};
use crate::{Database, Row};
use rustlavel_core::{Error, Json, Result};
use std::collections::HashMap;

/// Implemented by `#[derive(Model)]`; not written by hand.
pub trait Model: Sized + Default + Send + Sync {
    /// The primary key's type — usually `i64`.
    type Key: FromValue + Into<Value> + Clone + Send + Sync + std::fmt::Debug;

    const TABLE: &'static str;
    const PRIMARY_KEY: &'static str;
    /// The selectable columns, already quoted: `"id", "name"`.
    const COLUMNS: &'static str;

    fn from_row(row: &Row) -> Result<Self>;
    fn key(&self) -> Self::Key;
    fn set_key(&mut self, key: Self::Key);

    /// The columns written on insert and update — everything except the
    /// primary key and the timestamps the database maintains.
    fn values(&self) -> Vec<(&'static str, Value)>;
}

/// The query and persistence methods every model gets.
///
/// A blanket implementation, so a model author writes no code for any of this.
pub trait ModelExt: Model {
    /// Start a query against this model's table.
    fn query() -> QueryBuilder {
        QueryBuilder::new(Self::TABLE)
    }

    /// Run a builder and map the rows into models.
    fn hydrate(rows: &[Row]) -> Result<Vec<Self>> {
        rows.iter().map(Self::from_row).collect()
    }

    fn all(db: &Database) -> impl Future<Output = Result<Vec<Self>>> + Send
    where
        Self: Send,
    {
        async move { Self::hydrate(&Self::query().get(db).await?) }
    }

    /// Find by primary key.
    fn find(db: &Database, key: Self::Key) -> impl Future<Output = Result<Option<Self>>> + Send
    where
        Self: Send,
    {
        async move {
            let row = Self::query().filter(Self::PRIMARY_KEY, key).first(db).await?;
            row.as_ref().map(Self::from_row).transpose()
        }
    }

    /// Find by primary key, or fail with a message naming the model.
    fn find_or_fail(db: &Database, key: Self::Key) -> impl Future<Output = Result<Self>> + Send
    where
        Self: Send,
    {
        async move {
            let described = format!("{:?}", key);
            Self::find(db, key).await?.ok_or_else(|| {
                Error::msg(format!(
                    "no {} with {} = {described}",
                    Self::TABLE,
                    Self::PRIMARY_KEY
                ))
            })
        }
    }

    /// Run a prepared builder and hydrate the results.
    fn get(db: &Database, query: QueryBuilder) -> impl Future<Output = Result<Vec<Self>>> + Send
    where
        Self: Send,
    {
        async move { Self::hydrate(&query.get(db).await?) }
    }

    fn first(db: &Database, query: QueryBuilder) -> impl Future<Output = Result<Option<Self>>> + Send
    where
        Self: Send,
    {
        async move {
            let row = query.first(db).await?;
            row.as_ref().map(Self::from_row).transpose()
        }
    }

    fn count(db: &Database) -> impl Future<Output = Result<i64>> + Send
    where
        Self: Send,
    {
        async move { Self::query().count(db).await }
    }

    /// Insert this record and adopt the key the database generated.
    fn insert(&mut self, db: &Database) -> impl Future<Output = Result<()>> + Send
    where
        Self: Send,
    {
        async move {
            let values = self.values();
            let borrowed: Vec<(&str, Value)> =
                values.iter().map(|(name, value)| (*name, value.clone())).collect();

            let row = QueryBuilder::new(Self::TABLE)
                .insert_returning(db, &borrowed, Self::PRIMARY_KEY)
                .await?;
            self.set_key(Self::Key::from_value(&row)?);
            Ok(())
        }
    }

    /// Update the row this record's key points at.
    fn update(&self, db: &Database) -> impl Future<Output = Result<u64>> + Send
    where
        Self: Send + Sync,
    {
        async move {
            let values = self.values();
            let borrowed: Vec<(&str, Value)> =
                values.iter().map(|(name, value)| (*name, value.clone())).collect();

            Self::query()
                .filter(Self::PRIMARY_KEY, self.key())
                .update(db, &borrowed)
                .await
        }
    }

    fn delete(&self, db: &Database) -> impl Future<Output = Result<u64>> + Send
    where
        Self: Send + Sync,
    {
        async move { Self::query().filter(Self::PRIMARY_KEY, self.key()).delete(db).await }
    }

    /// This record as JSON, for an API response.
    fn to_json(&self) -> Json
    where
        Self: Sync,
    {
        let mut fields: Vec<(String, Json)> = vec![(
            Self::PRIMARY_KEY.to_string(),
            Json::from(self.key().into()),
        )];
        fields.extend(
            self.values()
                .into_iter()
                .map(|(name, value)| (name.to_string(), Json::from(value))),
        );
        Json::object(fields)
    }
}

impl<T: Model> ModelExt for T {}

/// Load the children of many parents in a single query.
///
/// This is the answer to N+1: the caller gets one query for the parents and one
/// for all of their children, however many parents there are.
///
/// ```ignore
/// let users = User::all(&db).await?;
/// let posts = has_many::<User, Post>(&db, &users, "user_id").await?;
/// ```
pub async fn has_many<P, C>(
    db: &Database,
    parents: &[P],
    foreign_key: &str,
) -> Result<Vec<Vec<C>>>
where
    P: Model,
    C: Model,
{
    if parents.is_empty() {
        return Ok(Vec::new());
    }

    let keys: Vec<Value> = parents.iter().map(|parent| parent.key().into()).collect();
    let rows = C::query().filter_in(foreign_key, keys).get(db).await?;

    // Group by the foreign key once, then hand each parent its slice.
    let mut grouped: HashMap<String, Vec<C>> = HashMap::new();
    for row in &rows {
        let owner = row.value(foreign_key)?.to_display();
        grouped.entry(owner).or_default().push(C::from_row(row)?);
    }

    Ok(parents
        .iter()
        .map(|parent| {
            let key: Value = parent.key().into();
            grouped.remove(&key.to_display()).unwrap_or_default()
        })
        .collect())
}

/// Load the parent of many children in a single query.
pub async fn belongs_to<C, P>(
    db: &Database,
    children: &[C],
    foreign_key: &str,
) -> Result<Vec<Option<P>>>
where
    C: Model,
    P: Model,
{
    if children.is_empty() {
        return Ok(Vec::new());
    }

    // The child's own row is not available here, so the foreign keys are read
    // back from the database in the same query that fetches the parents.
    let child_keys: Vec<Value> = children.iter().map(|child| child.key().into()).collect();

    let pairs = QueryBuilder::new(C::TABLE)
        .select(&[C::PRIMARY_KEY, foreign_key])
        .filter_in(C::PRIMARY_KEY, child_keys)
        .get(db)
        .await?;

    let mut owner_of: HashMap<String, String> = HashMap::new();
    let mut parent_keys: Vec<Value> = Vec::new();
    for row in &pairs {
        let child = row.value(C::PRIMARY_KEY)?.to_display();
        let parent = row.value(foreign_key)?.clone();
        if !parent.is_null() {
            owner_of.insert(child, parent.to_display());
            parent_keys.push(parent);
        }
    }

    let parent_rows = P::query().filter_in(P::PRIMARY_KEY, parent_keys).get(db).await?;

    // Rows are kept rather than models: `Model` does not require `Clone`, and
    // re-hydrating from the row gives each child a complete parent.
    let mut rows_by_key: HashMap<String, &Row> = HashMap::new();
    for row in &parent_rows {
        rows_by_key.insert(row.value(P::PRIMARY_KEY)?.to_display(), row);
    }

    children
        .iter()
        .map(|child| {
            let key: Value = child.key().into();
            match owner_of.get(&key.to_display()).and_then(|parent| rows_by_key.get(parent)) {
                Some(row) => P::from_row(row).map(Some),
                None => Ok(None),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // A hand-written Model, standing in for what the derive emits. It keeps
    // this crate's tests independent of the proc-macro crate.
    #[derive(Default, Debug, PartialEq)]
    struct User {
        id: i64,
        name: String,
        email: Option<String>,
    }

    impl Model for User {
        type Key = i64;

        const TABLE: &'static str = "users";
        const PRIMARY_KEY: &'static str = "id";
        const COLUMNS: &'static str = "\"id\", \"name\", \"email\"";

        fn from_row(row: &Row) -> Result<Self> {
            Ok(User {
                id: row.get("id")?,
                name: row.get("name")?,
                email: row.get("email")?,
            })
        }

        fn key(&self) -> i64 {
            self.id
        }

        fn set_key(&mut self, key: i64) {
            self.id = key;
        }

        fn values(&self) -> Vec<(&'static str, Value)> {
            vec![
                ("name", Value::from(self.name.clone())),
                ("email", Value::from(self.email.clone())),
            ]
        }
    }

    fn row(id: i64, name: &str, email: Option<&str>) -> Row {
        let columns =
            std::sync::Arc::new(vec!["id".to_string(), "name".to_string(), "email".to_string()]);
        Row::new(
            columns,
            vec![
                Value::Int(id),
                Value::Text(name.into()),
                email.map_or(Value::Null, |e| Value::Text(e.into())),
            ],
        )
    }

    #[test]
    fn hydrates_rows_into_models() {
        let users = User::hydrate(&[row(1, "Ada", Some("ada@example.com")), row(2, "Grace", None)])
            .unwrap();

        assert_eq!(users[0].name, "Ada");
        assert_eq!(users[0].email.as_deref(), Some("ada@example.com"));
        assert_eq!(users[1].email, None);
    }

    #[test]
    fn a_query_targets_the_models_table() {
        let (sql, _) = User::query().filter("name", "Ada").to_sql().unwrap();
        assert_eq!(sql, r#"select * from "users" where "name" = $1"#);
    }

    #[test]
    fn json_includes_the_primary_key_and_every_value() {
        let user = User { id: 7, name: "Ada".into(), email: None };

        assert_eq!(user.to_json().to_string(), r#"{"email":null,"id":7,"name":"Ada"}"#);
    }

    #[test]
    fn a_missing_column_names_itself() {
        let columns = std::sync::Arc::new(vec!["id".to_string()]);
        let incomplete = Row::new(columns, vec![Value::Int(1)]);

        let error = User::from_row(&incomplete).unwrap_err().to_string();
        assert!(error.contains("no column `name`"));
    }
}
