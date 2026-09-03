//! The cache itself.

use crate::keys;
use crate::region::{Region, Strategy};
use crate::stats::{Event, Stats};
use rustlavel_cache::{Cache, CacheStore};
use rustlavel_core::{Json, Result};
use rustlavel_db::row::Columns;
use rustlavel_db::{Database, Model, ModelExt, QueryBuilder, Row, Value};
use std::collections::BTreeMap;
use std::sync::Arc;

/// What is stored under a query key.
///
/// The statement is kept beside the rows and compared on the way out. The key
/// is a 64-bit fingerprint, and a collision would otherwise serve one query's
/// rows as another's — see [`keys::fingerprint`]. This makes a collision a
/// miss.
struct Entry {
    sql: String,
    generation: i64,
    rows: Vec<Json>,
}

impl Entry {
    fn to_json(&self) -> Json {
        Json::object([
            ("sql", Json::from(self.sql.as_str())),
            ("g", Json::from(self.generation)),
            ("rows", Json::Array(self.rows.clone())),
        ])
    }

    fn from_json(value: &Json) -> Option<Entry> {
        Some(Entry {
            sql: value.get("sql")?.as_str()?.to_string(),
            generation: value.get("g")?.as_i64()?,
            rows: value.get("rows")?.as_array()?.to_vec(),
        })
    }
}

/// A second-level cache for models.
///
/// Cheap to clone; every clone shares one backend and one set of counters.
#[derive(Clone)]
pub struct ModelCache {
    store: Arc<dyn Cache>,
    regions: Arc<BTreeMap<&'static str, Region>>,
    default_region: Arc<Region>,
    stats: Arc<Stats>,
    /// Whether an unregistered model is cached at all.
    ///
    /// Off. A cache that quietly starts holding every table the moment it is
    /// registered is a cache nobody chose, and the first thing it does wrong
    /// is serve a stale row from a table the author never thought about.
    opt_in: bool,
}

impl ModelCache {
    /// A cache over any [`Cache`] backend — memory, file, or Redis.
    pub fn new(store: impl Cache) -> ModelCache {
        ModelCache::from_arc(Arc::new(store))
    }

    /// The store the application already built, shared rather than duplicated.
    pub fn shared(store: &CacheStore) -> ModelCache {
        ModelCache::from_arc(store.driver_handle())
    }

    fn from_arc(store: Arc<dyn Cache>) -> ModelCache {
        ModelCache {
            store,
            regions: Arc::new(BTreeMap::new()),
            default_region: Arc::new(Region::default()),
            stats: Arc::new(Stats::default()),
            opt_in: true,
        }
    }

    /// Register a model, with settings.
    ///
    /// Panics on settings that contradict each other — a read-only region with
    /// no expiry, say. That is a startup failure by design: the alternative is
    /// a stale row six weeks from now with nothing to trace it to.
    pub fn region<M: Model>(mut self, region: Region) -> ModelCache {
        region.check(M::TABLE).expect("the cache region settings contradict each other");
        Arc::make_mut(&mut self.regions).insert(M::TABLE, region);
        self
    }

    /// Cache every model, registered or not, with these settings.
    ///
    /// For an application that has decided it wants this everywhere. A model
    /// with its own [`region`](Self::region) still uses that.
    pub fn cache_everything(mut self, region: Region) -> ModelCache {
        region.check("<default>").expect("the default region settings contradict each other");
        self.default_region = Arc::new(region);
        self.opt_in = false;
        self
    }

    pub fn stats(&self) -> &Stats {
        &self.stats
    }

    /// The settings for a table, or `None` when it is not cached.
    fn region_for(&self, table: &str) -> Option<&Region> {
        match self.regions.get(table) {
            Some(region) => Some(region),
            None if self.opt_in => None,
            None => Some(&self.default_region),
        }
    }

    // --- Reading ---------------------------------------------------------

    /// A model by primary key, from the cache when it is there.
    pub async fn find<M: Model + Send>(&self, db: &Database, key: M::Key) -> Result<Option<M>> {
        let Some(region) = self.region_for(M::TABLE).filter(|r| r.entities) else {
            return M::find(db, key).await;
        };

        let text = Json::from(key.clone().into()).to_string();
        let cache_key = keys::entity(M::TABLE, &text);

        if let Some(cached) = self.store.get(&cache_key).await? {
            self.stats.record(M::TABLE, Event::EntityHit);
            // A row that no longer parses — the model gained a column since it
            // was cached — is a miss, not an error. Refusing to serve the page
            // because an old cache entry has the wrong shape would make a
            // deployment an outage.
            if let Some(model) = row_from_json(&cached).and_then(|row| M::from_row(&row).ok()) {
                return Ok(Some(model));
            }
        } else {
            self.stats.record(M::TABLE, Event::EntityMiss);
        }

        let found = M::find(db, key).await?;
        if let Some(model) = &found {
            let row = self.select_one(db, M::TABLE, M::PRIMARY_KEY, model.key().into()).await?;
            if let Some(json) = row.as_ref().and_then(row_to_json) {
                self.put(&cache_key, json, region).await?;
            }
        }
        Ok(found)
    }

    /// A query's results, from the cache when they are there and still current.
    pub async fn get<M: Model + Send>(&self, db: &Database, query: QueryBuilder) -> Result<Vec<M>> {
        let Some(region) = self.region_for(M::TABLE).filter(|r| r.queries) else {
            return M::get(db, query).await;
        };

        let (sql, bindings) = query.to_sql(db.dialect())?;
        let cache_key = keys::query(M::TABLE, keys::fingerprint(&sql, &bindings));
        let generation = self.generation(M::TABLE, region).await?;

        // Three ways this is a miss rather than a hit, and all three matter: a
        // different statement means the fingerprint collided, an older
        // generation means somebody wrote to the table, and a row that will
        // not parse means the model changed shape since it was cached.
        let usable = self
            .store
            .get(&cache_key)
            .await?
            .as_ref()
            .and_then(Entry::from_json)
            .filter(|entry| entry.sql == sql && entry.generation == generation);

        // A row that will not parse — the model gained a column since it was
        // cached — is the third way this is a miss, and it is why the hit is
        // only recorded once every row has come back.
        if let Some(models) = usable.and_then(|entry| {
            entry
                .rows
                .iter()
                .map(|value| row_from_json(value).and_then(|row| M::from_row(&row).ok()))
                .collect::<Option<Vec<M>>>()
        }) {
            self.stats.record(M::TABLE, Event::QueryHit);
            return Ok(models);
        }
        self.stats.record(M::TABLE, Event::QueryMiss);

        let rows = query.get(db).await?;
        let models = M::hydrate(&rows)?;

        if rows.len() > region.max_rows {
            self.stats.record(M::TABLE, Event::TooLarge);
            return Ok(models);
        }

        // A row this cache cannot carry back exactly — a binary column — means
        // the whole result set goes unstored rather than half-stored.
        let Some(encoded) = rows.iter().map(row_to_json).collect::<Option<Vec<Json>>>() else {
            self.stats.record(M::TABLE, Event::Unsupported);
            return Ok(models);
        };

        let entry = Entry { sql, generation, rows: encoded };
        self.put(&cache_key, entry.to_json(), region).await?;
        Ok(models)
    }

    /// The first result, from the cache when it is there.
    pub async fn first<M: Model + Send>(
        &self,
        db: &Database,
        query: QueryBuilder,
    ) -> Result<Option<M>> {
        Ok(self.get::<M>(db, query.limit(1)).await?.into_iter().next())
    }

    // --- Writing ---------------------------------------------------------

    /// Insert, and invalidate what the insert makes stale.
    pub async fn insert<M: Model + Send + Sync>(&self, db: &Database, model: &mut M) -> Result<()> {
        model.insert(db).await?;
        self.invalidate_table::<M>().await
    }

    /// Update, and invalidate the entity and the table's cached queries.
    pub async fn update<M: Model + Send + Sync>(&self, db: &Database, model: &M) -> Result<u64> {
        let affected = model.update(db).await?;
        self.forget::<M>(model.key()).await?;
        self.invalidate_table::<M>().await?;
        Ok(affected)
    }

    /// Delete, and the same.
    pub async fn delete<M: Model + Send + Sync>(&self, db: &Database, model: &M) -> Result<u64> {
        let affected = model.delete(db).await?;
        self.forget::<M>(model.key()).await?;
        self.invalidate_table::<M>().await?;
        Ok(affected)
    }

    /// Drop one entity from the cache. The table's queries are left alone.
    ///
    /// For a write this cache did not make — a raw `UPDATE`, a migration, a
    /// change from another process. Almost always wanted together with
    /// [`invalidate_table`](Self::invalidate_table).
    pub async fn forget<M: Model>(&self, key: M::Key) -> Result<()> {
        let text = Json::from(key.into()).to_string();
        self.store.forget(&keys::entity(M::TABLE, &text)).await?;
        Ok(())
    }

    /// Make every cached query for this table a miss.
    ///
    /// One counter, bumped. There is no way to enumerate the result sets a
    /// write invalidates — a cached `WHERE role = 'admin'` is invalidated by
    /// an insert whose shape this cache never sees — so the generation is what
    /// stands in for that enumeration.
    pub async fn invalidate_table<M: Model>(&self) -> Result<()> {
        let Some(region) = self.region_for(M::TABLE) else { return Ok(()) };
        if region.strategy == Strategy::ReadOnly {
            // The caller promised there would be no writes. Bumping anyway
            // would make the promise meaningless and hide the mistake.
            return Ok(());
        }
        self.store.increment(&keys::generation(M::TABLE), 1).await?;
        self.stats.record(M::TABLE, Event::Invalidated);
        Ok(())
    }

    /// Everything this cache put in the store, for every table.
    ///
    /// Bumps each registered table's generation rather than flushing the
    /// backend, because the backend is shared: flushing it would take the
    /// session store and the rate limiter with it.
    pub async fn invalidate_all(&self) -> Result<()> {
        for (table, region) in self.regions.iter() {
            if region.strategy == Strategy::ReadOnly {
                continue;
            }
            self.store.increment(&keys::generation(table), 1).await?;
            self.stats.record(table, Event::Invalidated);
        }
        Ok(())
    }

    // --- Plumbing --------------------------------------------------------

    /// The table's current generation.
    ///
    /// A read-only region skips this entirely, which is one round trip less
    /// per query and the whole reason the strategy exists.
    async fn generation(&self, table: &str, region: &Region) -> Result<i64> {
        if region.strategy == Strategy::ReadOnly {
            return Ok(0);
        }
        Ok(self.store.get(&keys::generation(table)).await?.and_then(|v| v.as_i64()).unwrap_or(0))
    }

    async fn put(&self, key: &str, value: Json, region: &Region) -> Result<()> {
        match region.ttl {
            Some(ttl) => self.store.put(key, value, ttl).await,
            None => self.store.forever(key, value).await,
        }
    }

    /// Re-read one row so the cached entity has every column, not just the
    /// ones `Model::values` writes.
    ///
    /// `to_json` on the model would lose whatever the database maintains —
    /// `created_at`, a computed default — and `from_row` would then fail on
    /// the way back out. Caching the row the database returned avoids the
    /// whole question.
    async fn select_one(
        &self,
        db: &Database,
        table: &str,
        primary_key: &str,
        key: Value,
    ) -> Result<Option<Row>> {
        let rows = db.table(table).filter(primary_key, key).limit(1).get(db).await?;
        Ok(rows.into_iter().next())
    }
}

impl std::fmt::Debug for ModelCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelCache")
            .field("driver", &self.store.driver())
            .field("regions", &self.regions.keys().collect::<Vec<_>>())
            .field("opt_in", &self.opt_in)
            .finish()
    }
}

/// A row as a JSON object: column name to value.
///
/// `None` when the row holds something JSON cannot carry back. See
/// [`value_to_json`] — this is the check that stops a binary column being
/// quietly turned into the string `"<12 bytes>"` on the way through the cache.
fn row_to_json(row: &Row) -> Option<Json> {
    let mut fields = Vec::with_capacity(row.columns().len());
    for name in row.columns() {
        let value = row.value(name).ok()?;
        fields.push((name.clone(), value_to_json(value)?));
    }
    Some(Json::object(fields))
}

/// And back. `None` for anything that is not an object, which is what a
/// corrupted or outgrown entry looks like.
fn row_from_json(value: &Json) -> Option<Row> {
    let Json::Object(map) = value else { return None };
    let columns: Vec<String> = map.keys().cloned().collect();
    let values: Vec<Value> = map.values().map(json_to_value).collect();
    Some(Row::new(Columns::new(columns), values))
}

/// One value, on the way in.
///
/// **`Value::from(json)` is not the inverse of `Json::from(value)`** — it wraps
/// whatever it is given in a JSON *column* — so the pair is written here
/// instead. The one case that cannot round-trip is `Bytes`: the built-in
/// conversion renders it as `"<12 bytes>"`, which reads back as text and hands
/// a caller a string where their blob was. That is a corrupted row, so it is
/// refused: the query runs and its result is simply not cached.
fn value_to_json(value: &Value) -> Option<Json> {
    match value {
        Value::Bytes(_) => None,
        other => Some(Json::from(other.clone())),
    }
}

/// One value, on the way out.
///
/// A JSON number becomes `Int` when it is whole and `Float` when it is not.
/// The asymmetry is safe in this direction because `FromValue for f64` accepts
/// an `Int` — a float column holding `7.0` comes back as `Int(7)` and still
/// reads as `7.0` — while the reverse, handing an integer column a float,
/// would not.
fn json_to_value(value: &Json) -> Value {
    match value {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Bool(*b),
        Json::Number(n) if n.fract() == 0.0 && n.abs() < 9.007_199_254_740_992e15 => {
            Value::Int(*n as i64)
        }
        Json::Number(n) => Value::Float(*n),
        Json::String(s) => Value::Text(s.clone()),
        other => Value::Json(other.clone()),
    }
}

/// The store, for a caller that wants to reach past this.
impl ModelCache {
    pub fn store(&self) -> &Arc<dyn Cache> {
        &self.store
    }
}

/// Not reachable without a database, so the round trip is what gets tested.
#[cfg(test)]
mod tests {
    use super::*;

    fn row(pairs: &[(&str, Value)]) -> Row {
        let columns: Vec<String> = pairs.iter().map(|(n, _)| (*n).to_string()).collect();
        let values: Vec<Value> = pairs.iter().map(|(_, v)| v.clone()).collect();
        Row::new(Columns::new(columns), values)
    }

    #[test]
    fn a_row_survives_the_round_trip_through_json() {
        let original = row(&[
            ("id", Value::from(7i64)),
            ("name", Value::from("Ada Lovelace")),
            ("verified", Value::from(true)),
            ("deleted_at", Value::Null),
        ]);

        let back = row_from_json(&row_to_json(&original).expect("not encodable")).expect("did not parse");

        assert_eq!(back.get::<i64>("id").unwrap(), 7);
        assert_eq!(back.get::<String>("name").unwrap(), "Ada Lovelace");
        assert!(back.get::<bool>("verified").unwrap());
        assert_eq!(back.columns().len(), 4, "a column was lost");
    }

    /// A binary column cannot survive JSON — the built-in conversion renders
    /// it as `"<3 bytes>"` — so the row is refused rather than corrupted.
    #[test]
    fn a_row_with_a_binary_column_is_not_cached_at_all() {
        let with_blob = row(&[("id", Value::from(1i64)), ("avatar", Value::Bytes(vec![1, 2, 3]))]);

        assert!(row_to_json(&with_blob).is_none(), "a blob was silently turned into text");
    }

    /// A whole number comes back as an integer, which is what an id column
    /// needs; a fractional one stays a float.
    #[test]
    fn numbers_come_back_as_the_variant_the_column_needs() {
        assert_eq!(json_to_value(&Json::Number(7.0)), Value::Int(7));
        assert_eq!(json_to_value(&Json::Number(-7.0)), Value::Int(-7));
        assert_eq!(json_to_value(&Json::Number(7.5)), Value::Float(7.5));
        assert_eq!(json_to_value(&Json::Null), Value::Null);
        assert_eq!(json_to_value(&Json::Bool(true)), Value::Bool(true));
        assert_eq!(json_to_value(&Json::from("x")), Value::Text("x".into()));
    }

    /// A cached entry from before a column was added must not fail the
    /// request. It is a miss.
    #[test]
    fn rubbish_in_the_cache_reads_as_a_miss_rather_than_an_error() {
        assert!(row_from_json(&Json::from("not an object")).is_none());
        assert!(row_from_json(&Json::Null).is_none());
        assert!(row_from_json(&Json::Array(vec![])).is_none());
    }

    #[test]
    fn a_query_entry_carries_the_statement_it_was_taken_from() {
        let entry = Entry {
            sql: "select * from users where role = ?".into(),
            generation: 3,
            rows: vec![Json::object([("id", Json::from(1))])],
        };

        let back = Entry::from_json(&entry.to_json()).expect("did not parse");
        assert_eq!(back.sql, entry.sql);
        assert_eq!(back.generation, 3);
        assert_eq!(back.rows.len(), 1);

        // Anything else is a miss.
        assert!(Entry::from_json(&Json::from("nonsense")).is_none());
        assert!(Entry::from_json(&Json::object([("sql", Json::from("x"))])).is_none());
    }
}
