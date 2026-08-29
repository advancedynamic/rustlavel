//! Response shapes for the benchmark endpoints. See `benchmarks/CONTRACT.md`
//! — every field name and type here is fixed by the contract.

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct Message {
    pub message: &'static str,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Params {
    pub id: i64,
    pub slug: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Depth {
    pub depth: u32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BigRow {
    pub id: i64,
    pub name: String,
    pub email: String,
    pub active: bool,
    pub score: f64,
}

impl BigRow {
    #[must_use]
    pub fn new(id: i64) -> Self {
        Self {
            id,
            name: format!("User {id}"),
            email: format!("user{id}@example.test"),
            active: id % 2 == 0,
            #[allow(clippy::cast_precision_loss)]
            score: id as f64 * 1.5,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Author {
    pub id: i32,
    pub name: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Post {
    pub id: i32,
    pub title: String,
    pub author: Option<Author>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TemplateRow {
    pub id: i64,
    pub name: String,
}
