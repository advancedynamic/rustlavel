//! Creating indices and describing what goes in them.
//!
//! An index can be created by writing a document to it, which is why so many
//! projects never write a mapping at all — and then discover that every number
//! is a `long` because the first document happened to have no decimals, that a
//! title cannot be sorted or aggregated because it was analysed, and that a
//! misspelled field name was accepted in silence and is now a permanent column
//! in the mapping. A mapping cannot be changed after the fact; the index has to
//! be rebuilt. So writing one first is not ceremony.
//!
//! ```ignore
//! client
//!     .create_index(
//!         "posts",
//!         &IndexDefinition::new()
//!             .field("title", Field::text().with_keyword())
//!             .field("tags", Field::keyword())
//!             .field("views", Field::long())
//!             .field("published_at", Field::date())
//!             .dynamic_strict()
//!             .shards(1)
//!             .replicas(0),
//!     )
//!     .await?;
//! ```

use crate::client::{SearchClient, encode};
use crate::error::{Result, SearchError};
use rustlavel_core::Json;
use rustlavel_http::Method;
use std::collections::BTreeMap;

/// How one field is stored and searched.
#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    options: BTreeMap<String, Json>,
    sub_fields: BTreeMap<String, Field>,
    properties: BTreeMap<String, Field>,
}

impl Field {
    /// Analysed free text: broken into terms, searchable with
    /// [`crate::Query::matching`].
    ///
    /// A `text` field cannot be sorted, aggregated on, or matched exactly —
    /// the original string is not kept as a whole. [`Field::with_keyword`] is
    /// how you get both, and is what dynamic mapping does for a string it
    /// discovers on its own.
    pub fn text() -> Field {
        Field::of("text")
    }

    /// An exact string: an identifier, a status, a tag.
    ///
    /// Not analysed, so it sorts, aggregates and filters — and matches only
    /// the whole value, case included.
    pub fn keyword() -> Field {
        Field::of("keyword")
    }

    pub fn long() -> Field {
        Field::of("long")
    }

    pub fn integer() -> Field {
        Field::of("integer")
    }

    pub fn double() -> Field {
        Field::of("double")
    }

    pub fn float() -> Field {
        Field::of("float")
    }

    pub fn boolean() -> Field {
        Field::of("boolean")
    }

    /// A date. Accepts ISO 8601 and epoch milliseconds unless
    /// [`Field::format`] says otherwise.
    pub fn date() -> Field {
        Field::of("date")
    }

    pub fn ip() -> Field {
        Field::of("ip")
    }

    pub fn geo_point() -> Field {
        Field::of("geo_point")
    }

    /// A nested structure, flattened into the parent document.
    ///
    /// An array of objects mapped this way loses the association between its
    /// fields — `{"a":1,"b":2}` and `{"a":2,"b":1}` become `a:[1,2] b:[1,2]`
    /// and a query for `a=1 and b=1` matches. [`Field::nested`] is what keeps
    /// each object whole, at the cost of a hidden document per entry.
    pub fn object() -> Field {
        Field::of("object")
    }

    /// A structure whose objects stay separately queryable.
    pub fn nested() -> Field {
        Field::of("nested")
    }

    /// A type this module does not name.
    pub fn of(kind: &str) -> Field {
        Field {
            options: BTreeMap::from([("type".to_string(), Json::String(kind.to_string()))]),
            sub_fields: BTreeMap::new(),
            properties: BTreeMap::new(),
        }
    }

    /// Index the whole value as a `keyword` alongside the analysed form, under
    /// `<field>.keyword`.
    ///
    /// The single most useful line in a mapping: without it, aggregating or
    /// sorting on a `text` field fails with an error suggesting you enable
    /// fielddata, which loads every term into heap and is almost never what
    /// anybody wants. `ignore_above` matches what dynamic mapping uses — a
    /// value longer than 256 characters is not worth a sort key.
    pub fn with_keyword(mut self) -> Field {
        self.sub_fields.insert(
            "keyword".to_string(),
            Field::keyword().option("ignore_above", Json::from(256)),
        );
        self
    }

    /// Which analyser to break the text with — `"english"` for stemming and
    /// stop words, a custom one defined in the index settings.
    pub fn analyzer(self, analyzer: &str) -> Field {
        self.option("analyzer", Json::String(analyzer.to_string()))
    }

    /// The date formats this field accepts, `||`-separated.
    pub fn format(self, format: &str) -> Field {
        self.option("format", Json::String(format.to_string()))
    }

    /// Store the field but never search it, which saves the index for
    /// something only ever read back with the document.
    pub fn not_indexed(self) -> Field {
        self.option("index", Json::Bool(false))
    }

    /// Accept a value that does not parse instead of rejecting the document.
    ///
    /// The field is left out of the index for that document and stays in
    /// `_source`. Worth considering for ingest of data you do not control,
    /// where one bad row should not fail a whole bulk request — and worth
    /// avoiding elsewhere, because the value becomes invisible to search with
    /// nothing to say so.
    pub fn ignore_malformed(self) -> Field {
        self.option("ignore_malformed", Json::Bool(true))
    }

    /// A field of an `object` or `nested` field.
    pub fn field(mut self, name: impl Into<String>, field: Field) -> Field {
        self.properties.insert(name.into(), field);
        self
    }

    /// Another sub-field beside the main one.
    pub fn sub_field(mut self, name: impl Into<String>, field: Field) -> Field {
        self.sub_fields.insert(name.into(), field);
        self
    }

    /// Any other mapping parameter.
    pub fn option(mut self, name: impl Into<String>, value: Json) -> Field {
        self.options.insert(name.into(), value);
        self
    }

    /// The JSON this field maps to.
    pub fn json(&self) -> Json {
        let mut body = self.options.clone();

        if !self.sub_fields.is_empty() {
            body.insert("fields".to_string(), properties_of(&self.sub_fields));
        }
        if !self.properties.is_empty() {
            body.insert("properties".to_string(), properties_of(&self.properties));
        }

        Json::Object(body)
    }
}

/// An index about to be created.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct IndexDefinition {
    fields: BTreeMap<String, Field>,
    settings: BTreeMap<String, Json>,
    dynamic: Option<Json>,
    aliases: Vec<String>,
}

impl IndexDefinition {
    pub fn new() -> IndexDefinition {
        IndexDefinition::default()
    }

    pub fn field(mut self, name: impl Into<String>, field: Field) -> IndexDefinition {
        self.fields.insert(name.into(), field);
        self
    }

    /// How many primary shards. Fixed for the life of the index.
    pub fn shards(self, shards: u32) -> IndexDefinition {
        self.setting("number_of_shards", Json::from(shards))
    }

    /// How many copies of each shard.
    ///
    /// Zero is right for a test or a single-node cluster and wrong everywhere
    /// else: with no replica, one lost node is lost data. The default is one,
    /// which is also why a single-node cluster reports yellow.
    pub fn replicas(self, replicas: u32) -> IndexDefinition {
        self.setting("number_of_replicas", Json::from(replicas))
    }

    /// How often new documents become searchable, as a duration like `"30s"`,
    /// or `"-1"` to refresh only on demand while bulk-loading.
    pub fn refresh_interval(self, interval: &str) -> IndexDefinition {
        self.setting("refresh_interval", Json::String(interval.to_string()))
    }

    pub fn setting(mut self, name: impl Into<String>, value: Json) -> IndexDefinition {
        self.settings.insert(name.into(), value);
        self
    }

    /// Reject a document containing a field the mapping does not declare.
    ///
    /// The default is to accept it and add it to the mapping permanently, so a
    /// typo in one document becomes a field in the index for good, and the
    /// misspelled data is invisible to every query written against the correct
    /// name. Strict turns that into an error at write time, where it can be
    /// fixed.
    pub fn dynamic_strict(mut self) -> IndexDefinition {
        self.dynamic = Some(Json::String("strict".to_string()));
        self
    }

    /// Accept unmapped fields but do not index them: they stay readable in
    /// `_source` and are not searchable.
    pub fn dynamic_false(mut self) -> IndexDefinition {
        self.dynamic = Some(Json::Bool(false));
        self
    }

    /// Another name this index answers to.
    pub fn alias(mut self, alias: impl Into<String>) -> IndexDefinition {
        self.aliases.push(alias.into());
        self
    }

    /// The mappings half on its own, which is what `_mapping` takes.
    pub fn mappings(&self) -> Json {
        let mut body = BTreeMap::new();

        if let Some(dynamic) = &self.dynamic {
            body.insert("dynamic".to_string(), dynamic.clone());
        }
        if !self.fields.is_empty() {
            body.insert("properties".to_string(), properties_of(&self.fields));
        }

        Json::Object(body)
    }

    /// The whole create-index body.
    pub fn body(&self) -> Json {
        let mut body = BTreeMap::new();

        if !self.settings.is_empty() {
            body.insert("settings".to_string(), Json::Object(self.settings.clone()));
        }

        let mappings = self.mappings();
        if !mappings.as_object().map(BTreeMap::is_empty).unwrap_or(true) {
            body.insert("mappings".to_string(), mappings);
        }

        if !self.aliases.is_empty() {
            let aliases = self
                .aliases
                .iter()
                .map(|name| (name.clone(), Json::Object(BTreeMap::new())))
                .collect();
            body.insert("aliases".to_string(), Json::Object(aliases));
        }

        Json::Object(body)
    }
}

fn properties_of(fields: &BTreeMap<String, Field>) -> Json {
    Json::Object(fields.iter().map(|(name, field)| (name.clone(), field.json())).collect())
}

impl SearchClient {
    /// Create an index.
    ///
    /// Fails with [`SearchError::IndexAlreadyExists`] if it is already there.
    pub async fn create_index(&self, name: &str, definition: &IndexDefinition) -> Result<()> {
        let path = encode(name);
        self.json(Method::Put, &path, name, Some(definition.body())).await?;
        Ok(())
    }

    /// Create an index unless it exists, answering whether it was created.
    ///
    /// Implemented by attempting the create and forgiving the one failure that
    /// means "already there" — not by checking first. Two application
    /// instances booting together both pass a check and one still loses the
    /// create, so the check would turn a harmless race into an intermittent
    /// crash at startup.
    pub async fn create_index_if_missing(
        &self,
        name: &str,
        definition: &IndexDefinition,
    ) -> Result<bool> {
        match self.create_index(name, definition).await {
            Ok(()) => Ok(true),
            Err(SearchError::IndexAlreadyExists { .. }) => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Delete an index and everything in it, answering whether there was one.
    pub async fn delete_index(&self, name: &str) -> Result<bool> {
        let path = encode(name);
        match self.json(Method::Delete, &path, name, None).await {
            Ok(_) => Ok(true),
            Err(SearchError::IndexNotFound { .. }) => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Whether the index — or an alias by that name — exists.
    ///
    /// `filter_path` matches nothing on purpose: the whole reply would be the
    /// index's settings and mappings, and the only part being asked about is
    /// the status code.
    pub async fn index_exists(&self, name: &str) -> Result<bool> {
        self.exists_at(&format!("{}?filter_path=none", encode(name)), name).await
    }

    /// Add fields to an existing index's mapping.
    ///
    /// Adding is all this can do. Changing the type of a field that already
    /// exists is refused, because the data is already indexed the old way;
    /// that needs a new index and a reindex.
    pub async fn put_mapping(&self, name: &str, definition: &IndexDefinition) -> Result<()> {
        let path = format!("{}/_mapping", encode(name));
        self.json(Method::Put, &path, name, Some(definition.mappings())).await?;
        Ok(())
    }

    /// The mapping as the cluster holds it, including whatever dynamic mapping
    /// added along the way.
    pub async fn get_mapping(&self, name: &str) -> Result<Json> {
        let path = format!("{}/_mapping", encode(name));
        self.json(Method::Get, &path, name, None).await
    }

    /// Make everything written so far searchable, now.
    ///
    /// Elasticsearch is near-real-time, not real-time: an indexed document is
    /// durable immediately and *searchable* only after the next refresh, which
    /// is one second away by default. That gap is the single most common cause
    /// of a test that passes alone and fails in a suite, and of a "save then
    /// redirect to the list" that shows the old list.
    ///
    /// Call it after writing and before searching in a test, and after a bulk
    /// load. Do not call it after every write in production — each refresh
    /// makes a new segment, and doing it per document is how an index ends up
    /// spending its day merging.
    pub async fn refresh(&self, name: &str) -> Result<()> {
        let path = format!("{}/_refresh", encode(name));
        self.json(Method::Post, &path, name, None).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustlavel_client::{Fake, FakeResponse};

    fn json(body: &str) -> Json {
        Json::parse(body).expect("valid JSON in a test")
    }

    #[test]
    fn a_definition_sends_only_the_sections_that_have_something_in_them() {
        assert_eq!(IndexDefinition::new().body().to_string(), "{}");

        let definition = IndexDefinition::new().field("title", Field::text());
        assert_eq!(
            definition.body().to_string(),
            r#"{"mappings":{"properties":{"title":{"type":"text"}}}}"#
        );
    }

    #[test]
    fn a_text_field_with_a_keyword_gets_the_sub_field_aggregations_need() {
        // Without this, aggregating on `title` fails and the error suggests
        // fielddata, which is the wrong fix.
        let definition = IndexDefinition::new().field("title", Field::text().with_keyword());

        assert_eq!(
            definition.mappings().to_string(),
            r#"{"properties":{"title":{"fields":{"keyword":{"ignore_above":256,"type":"keyword"}},"type":"text"}}}"#
        );
    }

    #[test]
    fn settings_mappings_and_aliases_all_land_in_the_create_body() {
        let definition = IndexDefinition::new()
            .shards(1)
            .replicas(0)
            .refresh_interval("30s")
            .dynamic_strict()
            .alias("posts-current")
            .field("views", Field::long());

        assert_eq!(
            definition.body().to_string(),
            r#"{"aliases":{"posts-current":{}},"mappings":{"dynamic":"strict","properties":{"views":{"type":"long"}}},"settings":{"number_of_replicas":0,"number_of_shards":1,"refresh_interval":"30s"}}"#
        );
    }

    #[test]
    fn a_nested_field_carries_its_own_properties() {
        let field = Field::nested()
            .field("street", Field::text())
            .field("postcode", Field::keyword());

        assert_eq!(
            field.json().to_string(),
            r#"{"properties":{"postcode":{"type":"keyword"},"street":{"type":"text"}},"type":"nested"}"#
        );
    }

    #[test]
    fn field_options_reach_the_mapping() {
        assert_eq!(
            Field::text().analyzer("english").json().to_string(),
            r#"{"analyzer":"english","type":"text"}"#
        );
        assert_eq!(
            Field::date().format("yyyy-MM-dd").json().to_string(),
            r#"{"format":"yyyy-MM-dd","type":"date"}"#
        );
        assert_eq!(
            Field::keyword().not_indexed().json().to_string(),
            r#"{"index":false,"type":"keyword"}"#
        );
        assert_eq!(
            Field::long().ignore_malformed().json().to_string(),
            r#"{"ignore_malformed":true,"type":"long"}"#
        );
    }

    #[tokio::test]
    async fn creating_an_index_puts_the_body_at_the_index_url() {
        let client = SearchClient::new("http://localhost:9200").faking(
            Fake::new().fallback(FakeResponse::json(json(
                r#"{"acknowledged":true,"shards_acknowledged":true,"index":"posts"}"#,
            ))),
        );

        client
            .create_index("posts", &IndexDefinition::new().field("title", Field::text()))
            .await
            .unwrap();

        let sent = &client.fake().unwrap().recorded()[0];
        assert_eq!(sent.method, Method::Put);
        assert_eq!(sent.url, "http://localhost:9200/posts");
        assert!(sent.body_text().contains(r#""title":{"type":"text"}"#));
    }

    #[tokio::test]
    async fn creating_an_index_that_exists_is_forgiven_only_when_it_was_asked_to_be() {
        let body = r#"{"error":{"root_cause":[{"type":"resource_already_exists_exception","reason":"index [posts/9jNXbCyMS4WTRDMCsQiSuQ] already exists","index_uuid":"9jNXbCyMS4WTRDMCsQiSuQ","index":"posts"}],"type":"resource_already_exists_exception","reason":"index [posts/9jNXbCyMS4WTRDMCsQiSuQ] already exists","index_uuid":"9jNXbCyMS4WTRDMCsQiSuQ","index":"posts"},"status":400}"#;
        let definition = IndexDefinition::new();

        let client = SearchClient::new("http://localhost:9200")
            .faking(Fake::new().fallback(FakeResponse::text(body).status(400)));
        assert!(matches!(
            client.create_index("posts", &definition).await,
            Err(SearchError::IndexAlreadyExists { .. })
        ));

        let client = SearchClient::new("http://localhost:9200")
            .faking(Fake::new().fallback(FakeResponse::text(body).status(400)));
        assert!(
            !client.create_index_if_missing("posts", &definition).await.unwrap(),
            "it existed, so nothing was created"
        );
    }

    #[tokio::test]
    async fn deleting_an_index_that_is_not_there_is_not_an_error() {
        let body = r#"{"error":{"root_cause":[{"type":"index_not_found_exception","reason":"no such index [gone]","resource.type":"index_or_alias","resource.id":"gone","index_uuid":"_na_","index":"gone"}],"type":"index_not_found_exception","reason":"no such index [gone]","resource.type":"index_or_alias","resource.id":"gone","index_uuid":"_na_","index":"gone"},"status":404}"#;
        let client = SearchClient::new("http://localhost:9200")
            .faking(Fake::new().fallback(FakeResponse::text(body).status(404)));

        assert!(!client.delete_index("gone").await.unwrap());
    }

    #[tokio::test]
    async fn existence_asks_for_as_little_as_possible_and_reads_a_404_as_an_answer() {
        // Deliberately a GET: a real cluster answers a HEAD with a chunked
        // framing it then never terminates. See `SearchClient::exists_at`.
        let present = SearchClient::new("http://localhost:9200")
            .faking(Fake::new().fallback(FakeResponse::json(json("{}"))));
        assert!(present.index_exists("posts").await.unwrap());

        let sent = &present.fake().unwrap().recorded()[0];
        assert_eq!(sent.method, Method::Get);
        assert_eq!(sent.url, "http://localhost:9200/posts?filter_path=none");

        let absent = SearchClient::new("http://localhost:9200")
            .faking(Fake::new().fallback(FakeResponse::text("").status(404)));
        assert!(!absent.index_exists("posts").await.unwrap());
    }

    #[tokio::test]
    async fn a_mapping_update_sends_only_the_mappings_half() {
        let client = SearchClient::new("http://localhost:9200")
            .faking(Fake::new().fallback(FakeResponse::json(json(r#"{"acknowledged":true}"#))));

        // The settings are ignored rather than sent: `_mapping` answers 400 for
        // a body containing them.
        client
            .put_mapping(
                "posts",
                &IndexDefinition::new().shards(3).field("summary", Field::text()),
            )
            .await
            .unwrap();

        let sent = &client.fake().unwrap().recorded()[0];
        assert_eq!(sent.url, "http://localhost:9200/posts/_mapping");
        assert_eq!(sent.body_text(), r#"{"properties":{"summary":{"type":"text"}}}"#);
    }

    #[tokio::test]
    async fn refreshing_posts_to_the_refresh_endpoint() {
        let client = SearchClient::new("http://localhost:9200").faking(Fake::new().fallback(
            FakeResponse::json(json(r#"{"_shards":{"total":2,"successful":1,"failed":0}}"#)),
        ));

        client.refresh("posts").await.unwrap();

        let sent = &client.fake().unwrap().recorded()[0];
        assert_eq!(sent.method, Method::Post);
        assert_eq!(sent.url, "http://localhost:9200/posts/_refresh");
    }
}
