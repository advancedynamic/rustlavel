//! A result row.

use crate::value::{FromValue, Value};
use rustlavel_core::{Error, Json, Result};
use std::sync::Arc;

/// The column names of a result set, shared by every row in it.
pub type Columns = Arc<Vec<String>>;

#[derive(Debug, Clone)]
pub struct Row {
    columns: Columns,
    values: Vec<Value>,
}

impl Row {
    pub fn new(columns: Columns, values: Vec<Value>) -> Self {
        Row { columns, values }
    }

    pub fn columns(&self) -> &[String] {
        &self.columns
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Read a column by name, converted to `T`.
    ///
    /// The error names the column, because "invalid type" with no column name
    /// is the least useful message a database layer can produce.
    pub fn get<T: FromValue>(&self, column: &str) -> Result<T> {
        let value = self.value(column)?;
        T::from_value(value).map_err(|e| Error::msg(format!("column `{column}`: {e}")))
    }

    /// Read a column by position.
    pub fn get_at<T: FromValue>(&self, index: usize) -> Result<T> {
        let value = self
            .values
            .get(index)
            .ok_or_else(|| Error::msg(format!("no column at index {index}")))?;
        T::from_value(value)
    }

    /// Read a column, falling back when it is absent or NULL.
    pub fn get_or<T: FromValue>(&self, column: &str, default: T) -> T {
        self.value(column).ok().and_then(|v| T::from_value(v).ok()).unwrap_or(default)
    }

    pub fn value(&self, column: &str) -> Result<&Value> {
        let index = self
            .columns
            .iter()
            .position(|name| name == column)
            .ok_or_else(|| self.unknown_column(column))?;
        Ok(&self.values[index])
    }

    pub fn has(&self, column: &str) -> bool {
        self.columns.iter().any(|name| name == column)
    }

    fn unknown_column(&self, column: &str) -> Error {
        Error::msg(format!(
            "no column `{column}` in this result. Available: {}",
            self.columns.join(", ")
        ))
    }

    /// The row as a JSON object — what an API handler usually wants next.
    pub fn to_json(&self) -> Json {
        Json::object(
            self.columns
                .iter()
                .cloned()
                .zip(self.values.iter().cloned().map(Json::from))
                .collect::<Vec<_>>(),
        )
    }
}

/// Turn a whole result set into a JSON array.
pub fn rows_to_json(rows: &[Row]) -> Json {
    Json::Array(rows.iter().map(Row::to_json).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row() -> Row {
        let columns = Arc::new(vec!["id".to_string(), "name".to_string(), "note".to_string()]);
        Row::new(columns, vec![Value::Int(7), Value::Text("Ada".into()), Value::Null])
    }

    #[test]
    fn reads_columns_by_name_and_position() {
        let row = row();

        assert_eq!(row.get::<i64>("id").unwrap(), 7);
        assert_eq!(row.get::<String>("name").unwrap(), "Ada");
        assert_eq!(row.get::<Option<String>>("note").unwrap(), None);
        assert_eq!(row.get_at::<i64>(0).unwrap(), 7);
    }

    #[test]
    fn an_unknown_column_lists_what_is_available() {
        let error = row().get::<i64>("nope").unwrap_err().to_string();

        assert!(error.contains("no column `nope`"));
        assert!(error.contains("id, name, note"));
    }

    #[test]
    fn a_type_error_names_the_column() {
        let error = row().get::<i64>("name").unwrap_err().to_string();
        assert!(error.contains("column `name`"));
    }

    #[test]
    fn converts_to_json() {
        assert_eq!(
            row().to_json().to_string(),
            r#"{"id":7,"name":"Ada","note":null}"#
        );
    }
}
