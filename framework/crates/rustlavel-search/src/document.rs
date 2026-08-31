//! Writing and reading documents.
//!
//! The one thing in this module worth reading before using it is [`bulk`]:
//! `_bulk` answers **HTTP 200 even when every document in it failed**. The
//! status code says the request was understood, not that the work was done,
//! and the outcome of each item is in the body. A caller that checks only the
//! status silently drops documents — usually the ones that mattered, because
//! the failures cluster around the unusual records — and the index looks fine
//! until somebody counts. [`BulkReport`] exists to make that impossible to
//! miss, and [`BulkReport::ok_or_error`] is one line for callers who would
//! rather have it as an error.
//!
//! [`bulk`]: SearchClient::bulk

use crate::client::{SearchClient, encode};
use crate::error::{Result, SearchError};
use rustlavel_core::Json;
use rustlavel_http::Method;

/// What a write did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteResult {
    Created,
    Updated,
    Deleted,
    /// An update whose document was already in the requested state. Nothing
    /// was written and the version did not move — which matters if you are
    /// counting writes or watching for a change.
    NoOp,
    NotFound,
    /// A result this client does not know.
    Other(String),
}

impl WriteResult {
    fn parse(value: &str) -> WriteResult {
        match value {
            "created" => WriteResult::Created,
            "updated" => WriteResult::Updated,
            "deleted" => WriteResult::Deleted,
            "noop" => WriteResult::NoOp,
            "not_found" => WriteResult::NotFound,
            other => WriteResult::Other(other.to_string()),
        }
    }
}

impl std::fmt::Display for WriteResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WriteResult::Created => f.write_str("created"),
            WriteResult::Updated => f.write_str("updated"),
            WriteResult::Deleted => f.write_str("deleted"),
            WriteResult::NoOp => f.write_str("noop"),
            WriteResult::NotFound => f.write_str("not_found"),
            WriteResult::Other(other) => f.write_str(other),
        }
    }
}

/// What the cluster said about a document it just wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedDocument {
    pub index: String,
    /// The id, which the cluster generated when none was given.
    pub id: String,
    pub version: i64,
    pub result: WriteResult,
    /// The two numbers that identify this exact version of the document, for
    /// [`SearchClient::index_document_if_unchanged`].
    pub seq_no: i64,
    pub primary_term: i64,
}

impl IndexedDocument {
    fn parse(body: &Json) -> IndexedDocument {
        IndexedDocument {
            index: string_at(body, "_index"),
            id: string_at(body, "_id"),
            version: int_at(body, "_version"),
            result: WriteResult::parse(body.get("result").and_then(Json::as_str).unwrap_or("")),
            seq_no: int_at(body, "_seq_no"),
            primary_term: int_at(body, "_primary_term"),
        }
    }
}

/// A document read back by id.
#[derive(Debug, Clone, PartialEq)]
pub struct Document {
    pub index: String,
    pub id: String,
    pub version: i64,
    pub seq_no: i64,
    pub primary_term: i64,
    pub source: Json,
}

impl Document {
    fn parse(body: &Json) -> Document {
        Document {
            index: string_at(body, "_index"),
            id: string_at(body, "_id"),
            version: int_at(body, "_version"),
            seq_no: int_at(body, "_seq_no"),
            primary_term: int_at(body, "_primary_term"),
            source: body.get("_source").cloned().unwrap_or(Json::Null),
        }
    }

    /// A field of the document, by dotted path.
    pub fn field(&self, path: &str) -> Option<&Json> {
        self.source.get(path)
    }

    /// A string field of the document, by dotted path.
    pub fn string(&self, path: &str) -> Option<&str> {
        self.source.get(path).and_then(Json::as_str)
    }
}

/// What a bulk item does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    /// Write, replacing anything already at the id.
    Index,
    /// Write only if the id is free.
    Create,
    /// Merge fields into an existing document.
    Update,
    Delete,
}

impl Action {
    fn as_str(self) -> &'static str {
        match self {
            Action::Index => "index",
            Action::Create => "create",
            Action::Update => "update",
            Action::Delete => "delete",
        }
    }
}

/// One entry in a bulk request.
#[derive(Debug, Clone, PartialEq)]
pub struct BulkOperation {
    action: Action,
    index: Option<String>,
    id: Option<String>,
    document: Json,
}

impl BulkOperation {
    /// Write a document, replacing whatever was at the id.
    ///
    /// With no [`BulkOperation::id`] the cluster generates one, which is
    /// faster — it can skip the lookup that a given id forces — and right for
    /// append-only data like logs.
    pub fn index(document: Json) -> BulkOperation {
        BulkOperation { action: Action::Index, index: None, id: None, document }
    }

    /// Write a document only if nothing is at the id yet.
    ///
    /// Fails that item with a version conflict rather than overwriting, which
    /// makes a bulk load safe to repeat after a partial failure.
    pub fn create(id: impl Into<String>, document: Json) -> BulkOperation {
        BulkOperation {
            action: Action::Create,
            index: None,
            id: Some(id.into()),
            document,
        }
    }

    /// Merge these fields into the document, leaving the rest alone.
    pub fn update(id: impl Into<String>, fields: Json) -> BulkOperation {
        BulkOperation {
            action: Action::Update,
            index: None,
            id: Some(id.into()),
            document: Json::object([("doc", fields)]),
        }
    }

    pub fn delete(id: impl Into<String>) -> BulkOperation {
        BulkOperation {
            action: Action::Delete,
            index: None,
            id: Some(id.into()),
            document: Json::Null,
        }
    }

    /// Give this operation an id, for an [`BulkOperation::index`] that should
    /// overwrite a known document.
    pub fn id(mut self, id: impl Into<String>) -> BulkOperation {
        self.id = Some(id.into());
        self
    }

    /// Send this one to a different index than the rest of the request.
    pub fn in_index(mut self, index: impl Into<String>) -> BulkOperation {
        self.index = Some(index.into());
        self
    }

    /// The two lines this operation contributes to the NDJSON body — one, for
    /// a delete.
    fn write_into(&self, out: &mut String) {
        let mut header = Vec::new();
        if let Some(index) = &self.index {
            header.push(("_index".to_string(), Json::String(index.clone())));
        }
        if let Some(id) = &self.id {
            header.push(("_id".to_string(), Json::String(id.clone())));
        }

        let header = Json::object([(
            self.action.as_str().to_string(),
            Json::object(header),
        )]);

        // Compact, because a newline anywhere inside one of these lines ends
        // the document early and the cluster reports a parse failure that
        // points at the next line.
        out.push_str(&header.to_string());
        out.push('\n');

        if self.action != Action::Delete {
            out.push_str(&self.document.to_string());
            out.push('\n');
        }
    }
}

/// One item's outcome in a bulk request.
#[derive(Debug, Clone, PartialEq)]
pub struct BulkItem {
    /// `index`, `create`, `update` or `delete`.
    pub action: String,
    pub index: String,
    pub id: String,
    /// The per-item HTTP status. This, not the request's, is what says whether
    /// the document was written.
    pub status: u16,
    pub result: Option<WriteResult>,
    /// Why it failed, classified exactly as a standalone request would be — so
    /// [`SearchError::is_retryable`] applies per item.
    pub error: Option<SearchError>,
}

impl BulkItem {
    fn parse(body: &Json) -> BulkItem {
        // Each item is a one-key object naming the action it was.
        let (action, detail) = body
            .as_object()
            .and_then(|map| map.iter().next())
            .map(|(action, detail)| (action.clone(), detail.clone()))
            .unwrap_or_else(|| (String::new(), Json::Null));

        let status = int_at(&detail, "status").clamp(0, u16::MAX as i64) as u16;
        let index = string_at(&detail, "_index");
        let id = string_at(&detail, "_id");

        let error = detail.get("error").map(|_| {
            // The per-item error body is the same envelope a standalone
            // request would have failed with, one level down, so the same
            // classifier reads it and the same retryability applies.
            SearchError::from_response(
                status,
                &index,
                &Json::object([("error", detail.get("error").cloned().unwrap_or(Json::Null))])
                    .to_string(),
            )
        });

        BulkItem {
            action,
            index,
            id,
            status,
            result: detail.get("result").and_then(Json::as_str).map(WriteResult::parse),
            error,
        }
    }

    pub fn is_failure(&self) -> bool {
        self.error.is_some() || !(200..300).contains(&self.status)
    }
}

/// What a bulk request did, item by item.
///
/// The request's HTTP status is 200 whether every item succeeded or none did.
/// [`BulkReport::failed`] and [`BulkReport::failures`] are the only honest
/// account of what happened.
#[derive(Debug, Clone, PartialEq)]
pub struct BulkReport {
    /// Milliseconds the cluster spent.
    pub took: u64,
    /// The cluster's own summary: true if any item failed.
    pub errors: bool,
    /// One entry per operation sent, in the order they were sent.
    pub items: Vec<BulkItem>,
}

impl BulkReport {
    fn parse(body: &Json) -> Result<BulkReport> {
        let items = body.get("items").and_then(Json::as_array).ok_or_else(|| {
            SearchError::Malformed {
                context: "_bulk".to_string(),
                message: "the reply has no `items`".to_string(),
            }
        })?;

        Ok(BulkReport {
            took: int_at(body, "took").max(0) as u64,
            errors: body.get("errors").and_then(Json::as_bool).unwrap_or(false),
            items: items.iter().map(BulkItem::parse).collect(),
        })
    }

    fn empty() -> BulkReport {
        BulkReport { took: 0, errors: false, items: Vec::new() }
    }

    /// Whether every item was written.
    pub fn all_succeeded(&self) -> bool {
        !self.items.iter().any(BulkItem::is_failure)
    }

    pub fn succeeded(&self) -> usize {
        self.items.iter().filter(|item| !item.is_failure()).count()
    }

    pub fn failed(&self) -> usize {
        self.items.iter().filter(|item| item.is_failure()).count()
    }

    /// The items that were not written, and why.
    pub fn failures(&self) -> Vec<&BulkItem> {
        self.items.iter().filter(|item| item.is_failure()).collect()
    }

    /// The positions in the operation list that failed.
    ///
    /// The cluster answers in the order it was asked, so these index straight
    /// back into the slice that was sent — which is what makes resubmitting
    /// only the failures possible without matching on ids, and works for the
    /// auto-generated ids where there is nothing to match on.
    pub fn failed_positions(&self) -> Vec<usize> {
        self.items
            .iter()
            .enumerate()
            .filter(|(_, item)| item.is_failure())
            .map(|(at, _)| at)
            .collect()
    }

    /// The failures that a later attempt could fix — a rejected write under
    /// load, not a document that does not fit the mapping.
    ///
    /// Resubmitting these and only these is the correct response to a bulk
    /// load that partly failed; resubmitting everything would either duplicate
    /// documents or re-attempt work that cannot succeed.
    pub fn retryable_positions(&self) -> Vec<usize> {
        self.items
            .iter()
            .enumerate()
            .filter(|(_, item)| item.error.as_ref().is_some_and(SearchError::is_retryable))
            .map(|(at, _)| at)
            .collect()
    }

    /// Turn any partial failure into an error.
    ///
    /// For the caller who wants `?` to notice rather than inspecting items.
    /// The report is still there in full on the success path.
    pub fn ok_or_error(&self) -> Result<()> {
        if self.all_succeeded() {
            return Ok(());
        }

        let first = self
            .failures()
            .first()
            .map(|item| match &item.error {
                Some(error) => format!("`{}`: {error}", item.id),
                None => format!("`{}`: status {}", item.id, item.status),
            })
            .unwrap_or_default();

        Err(SearchError::BulkPartialFailure {
            failed: self.failed(),
            total: self.items.len(),
            first,
        })
    }
}

impl std::fmt::Display for BulkReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} of {} written in {}ms", self.succeeded(), self.items.len(), self.took)
    }
}

impl SearchClient {
    /// Write a document at a known id, replacing anything already there.
    pub async fn index_document(
        &self,
        index: &str,
        id: &str,
        document: Json,
    ) -> Result<IndexedDocument> {
        let path = format!("{}/_doc/{}", encode(index), encode(id));
        let body = self.json(Method::Put, &path, &context(index, id), Some(document)).await?;
        Ok(IndexedDocument::parse(&body))
    }

    /// Write a document and let the cluster choose the id.
    ///
    /// Faster than giving one: with no id there is nothing to look up, so the
    /// write skips the check for an existing version. Right for append-only
    /// data, wrong for anything that has to be updated later by a key you
    /// already know.
    pub async fn index_new_document(
        &self,
        index: &str,
        document: Json,
    ) -> Result<IndexedDocument> {
        let path = format!("{}/_doc", encode(index));
        let body = self.json(Method::Post, &path, index, Some(document)).await?;
        Ok(IndexedDocument::parse(&body))
    }

    /// Write a document only if the id is free.
    ///
    /// Fails with [`SearchError::VersionConflict`] rather than overwriting,
    /// which is how an insert is expressed here — Elasticsearch has no other
    /// way to say "only if it does not exist".
    pub async fn create_document(
        &self,
        index: &str,
        id: &str,
        document: Json,
    ) -> Result<IndexedDocument> {
        let path = format!("{}/_create/{}", encode(index), encode(id));
        let body = self.json(Method::Put, &path, &context(index, id), Some(document)).await?;
        Ok(IndexedDocument::parse(&body))
    }

    /// Write a document only if it has not changed since it was read.
    ///
    /// The two numbers come from the [`Document`] or [`IndexedDocument`] that
    /// was read; passing them turns read-modify-write into something safe
    /// against a concurrent writer, which it otherwise is not. If somebody
    /// else wrote in between, this fails with
    /// [`SearchError::VersionConflict`] instead of quietly discarding their
    /// change — which is the entire reason the numbers exist.
    pub async fn index_document_if_unchanged(
        &self,
        index: &str,
        id: &str,
        document: Json,
        seq_no: i64,
        primary_term: i64,
    ) -> Result<IndexedDocument> {
        let path = format!(
            "{}/_doc/{}?if_seq_no={seq_no}&if_primary_term={primary_term}",
            encode(index),
            encode(id)
        );
        let body = self.json(Method::Put, &path, &context(index, id), Some(document)).await?;
        Ok(IndexedDocument::parse(&body))
    }

    /// Read a document by id.
    ///
    /// `None` means the index is there and the document is not, which is
    /// ordinary. A missing *index* is still an error, because that is a
    /// deployment problem rather than a missing record, and reporting the two
    /// the same way is how a typo in an index name survives to production.
    pub async fn get_document(&self, index: &str, id: &str) -> Result<Option<Document>> {
        let path = format!("{}/_doc/{}", encode(index), encode(id));
        let raw = self.send(Method::Get, &path, &context(index, id), None).await?;

        if !raw.is_success() {
            // A missing document is a 404 whose body says `"found": false`
            // and carries no `error` at all; a missing index is a 404 with the
            // usual error envelope. Nothing but the body separates them.
            let body = Json::parse(&raw.body).unwrap_or(Json::Null);
            if raw.status == 404 && body.get("found").and_then(Json::as_bool) == Some(false) {
                return Ok(None);
            }
            return Err(raw.error(&context(index, id)));
        }

        Ok(Some(Document::parse(&raw.into_json(&path)?)))
    }

    /// Whether a document exists, without transferring it.
    ///
    /// `_source=false` leaves the reply at the handful of metadata fields,
    /// which is as close to a `HEAD` as this can get. It is not one because a
    /// real cluster answers a `HEAD` with a chunked framing it then never
    /// terminates, leaving the reader waiting for a body that never comes.
    pub async fn document_exists(&self, index: &str, id: &str) -> Result<bool> {
        let path = format!("{}/_doc/{}?_source=false", encode(index), encode(id));
        self.exists_at(&path, &context(index, id)).await
    }

    /// Merge fields into a document, leaving the rest of it alone.
    ///
    /// Only the named fields change. Note that a partial update is a read,
    /// a merge and a full rewrite inside the cluster, so it costs more than
    /// indexing the whole document — it saves bandwidth, not work.
    pub async fn update_document(
        &self,
        index: &str,
        id: &str,
        fields: Json,
    ) -> Result<IndexedDocument> {
        let path = format!("{}/_update/{}", encode(index), encode(id));
        let body = self
            .json(
                Method::Post,
                &path,
                &context(index, id),
                Some(Json::object([("doc", fields)])),
            )
            .await?;
        Ok(IndexedDocument::parse(&body))
    }

    /// Delete a document, answering whether there was one.
    pub async fn delete_document(&self, index: &str, id: &str) -> Result<bool> {
        let path = format!("{}/_doc/{}", encode(index), encode(id));
        let raw = self.send(Method::Delete, &path, &context(index, id), None).await?;

        if raw.status == 404 {
            let body = Json::parse(&raw.body).unwrap_or(Json::Null);
            if body.get("result").and_then(Json::as_str) == Some("not_found") {
                return Ok(false);
            }
        }
        if !raw.is_success() {
            return Err(raw.error(&context(index, id)));
        }

        Ok(true)
    }

    /// Write many documents in one request.
    ///
    /// **Read the report.** `_bulk` answers 200 as long as the cluster could
    /// parse the request, whatever happened to the documents in it, so
    /// `?` on this call is not a check that anything was written.
    /// [`BulkReport::ok_or_error`] is the one-line version of that check;
    /// [`BulkReport::failures`] and [`BulkReport::retryable_positions`] are
    /// the version that lets you resubmit only what is worth resubmitting.
    ///
    /// `index` is the default for operations that do not name one of their own.
    pub async fn bulk(&self, index: &str, operations: &[BulkOperation]) -> Result<BulkReport> {
        self.send_bulk(&format!("{}/_bulk", encode(index)), index, operations).await
    }

    /// A bulk request spanning several indices, where every operation names
    /// its own with [`BulkOperation::in_index`].
    pub async fn bulk_across_indices(&self, operations: &[BulkOperation]) -> Result<BulkReport> {
        self.send_bulk("_bulk", "_bulk", operations).await
    }

    async fn send_bulk(
        &self,
        path: &str,
        context: &str,
        operations: &[BulkOperation],
    ) -> Result<BulkReport> {
        // An empty `_bulk` body is a 400 saying the request body is required,
        // which is a confusing way to be told that a loop had nothing in it.
        if operations.is_empty() {
            return Ok(BulkReport::empty());
        }

        let mut body = String::new();
        for operation in operations {
            operation.write_into(&mut body);
        }

        let response = self.ndjson(path, context, body).await?;
        BulkReport::parse(&response)
    }
}

/// How a document is named in an error message.
fn context(index: &str, id: &str) -> String {
    format!("{index}/{id}")
}

fn string_at(body: &Json, path: &str) -> String {
    body.get(path).and_then(Json::as_str).unwrap_or_default().to_string()
}

fn int_at(body: &Json, path: &str) -> i64 {
    body.get(path).and_then(Json::as_i64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustlavel_client::{Fake, FakeResponse};

    fn json(body: &str) -> Json {
        Json::parse(body).expect("valid JSON in a test")
    }

    fn client(body: &str, status: u16) -> SearchClient {
        SearchClient::new("http://localhost:9200")
            .faking(Fake::new().fallback(FakeResponse::text(body).status(status)))
    }

    #[tokio::test]
    async fn indexing_a_document_reads_back_what_the_cluster_assigned() {
        let client = client(
            r#"{"_index":"posts","_id":"1","_version":1,"result":"created","_shards":{"total":2,"successful":1,"failed":0},"_seq_no":0,"_primary_term":1}"#,
            201,
        );

        let written = client
            .index_document("posts", "1", json(r#"{"title":"Rust"}"#))
            .await
            .unwrap();

        assert_eq!(written.result, WriteResult::Created);
        assert_eq!(written.id, "1");
        assert_eq!(written.seq_no, 0);
        assert_eq!(written.primary_term, 1);

        let sent = &client.fake().unwrap().recorded()[0];
        assert_eq!(sent.method, Method::Put);
        assert_eq!(sent.url, "http://localhost:9200/posts/_doc/1");
    }

    #[tokio::test]
    async fn a_document_with_no_id_is_posted_so_the_cluster_generates_one() {
        let client = client(
            r#"{"_index":"logs","_id":"kSXqQpEBc1nQ3Z6yTQ8p","_version":1,"result":"created","_shards":{"total":2,"successful":1,"failed":0},"_seq_no":3,"_primary_term":1}"#,
            201,
        );

        let written = client.index_new_document("logs", json(r#"{"level":"warn"}"#)).await.unwrap();

        assert_eq!(written.id, "kSXqQpEBc1nQ3Z6yTQ8p");
        let sent = &client.fake().unwrap().recorded()[0];
        assert_eq!(sent.method, Method::Post, "a PUT would need an id in the path");
        assert_eq!(sent.url, "http://localhost:9200/logs/_doc");
    }

    #[tokio::test]
    async fn creating_a_document_that_exists_is_a_version_conflict_not_an_overwrite() {
        let body = r#"{"error":{"root_cause":[{"type":"version_conflict_engine_exception","reason":"[1]: version conflict, document already exists (current version [1])","index_uuid":"3zLDcCFFTASdSaKQTa93Xg","shard":"0","index":"posts"}],"type":"version_conflict_engine_exception","reason":"[1]: version conflict, document already exists (current version [1])","index_uuid":"3zLDcCFFTASdSaKQTa93Xg","shard":"0","index":"posts"},"status":409}"#;
        let client = client(body, 409);

        let error =
            client.create_document("posts", "1", json(r#"{"title":"x"}"#)).await.unwrap_err();

        assert!(error.is_conflict(), "got {error:?}");
        assert_eq!(
            client.fake().unwrap().recorded()[0].url,
            "http://localhost:9200/posts/_create/1"
        );
    }

    #[tokio::test]
    async fn an_optimistic_write_sends_the_two_numbers_that_make_it_safe() {
        let client = client(
            r#"{"_index":"posts","_id":"1","_version":2,"result":"updated","_shards":{"total":2,"successful":1,"failed":0},"_seq_no":1,"_primary_term":1}"#,
            200,
        );

        client
            .index_document_if_unchanged("posts", "1", json(r#"{"title":"x"}"#), 0, 1)
            .await
            .unwrap();

        let url = &client.fake().unwrap().recorded()[0].url;
        assert!(url.contains("if_seq_no=0"), "got {url}");
        assert!(url.contains("if_primary_term=1"), "got {url}");
    }

    #[tokio::test]
    async fn a_missing_document_is_none_and_a_missing_index_is_still_an_error() {
        // Both are 404 and only the body tells them apart. Reporting them the
        // same way is how a typo in an index name reaches production.
        let missing_document =
            client(r#"{"_index":"posts","_id":"nope","found":false}"#, 404);
        assert_eq!(missing_document.get_document("posts", "nope").await.unwrap(), None);

        let missing_index = client(
            r#"{"error":{"root_cause":[{"type":"index_not_found_exception","reason":"no such index [pots]","resource.type":"index_or_alias","resource.id":"pots","index_uuid":"_na_","index":"pots"}],"type":"index_not_found_exception","reason":"no such index [pots]","resource.type":"index_or_alias","resource.id":"pots","index_uuid":"_na_","index":"pots"},"status":404}"#,
            404,
        );
        assert!(
            missing_index.get_document("pots", "1").await.unwrap_err().is_index_not_found(),
            "a missing index must not read as a missing document"
        );
    }

    #[tokio::test]
    async fn reading_a_document_returns_the_source_and_the_concurrency_numbers() {
        let client = client(
            r#"{"_index":"posts","_id":"1","_version":1,"_seq_no":0,"_primary_term":1,"found":true,"_source":{"title":"Rust","views":12}}"#,
            200,
        );

        let document = client.get_document("posts", "1").await.unwrap().unwrap();

        assert_eq!(document.string("title"), Some("Rust"));
        assert_eq!(document.field("views").and_then(Json::as_i64), Some(12));
        assert_eq!((document.seq_no, document.primary_term), (0, 1));
    }

    #[tokio::test]
    async fn an_update_that_changed_nothing_reports_a_noop() {
        // Worth telling apart from an update: the version does not move, so a
        // caller watching for a change would wait forever.
        let client = client(
            r#"{"_index":"posts","_id":"1","_version":2,"result":"noop","_shards":{"total":0,"successful":0,"failed":0},"_seq_no":1,"_primary_term":1}"#,
            200,
        );

        let updated = client.update_document("posts", "1", json(r#"{"title":"Rust"}"#)).await.unwrap();

        assert_eq!(updated.result, WriteResult::NoOp);
        let sent = &client.fake().unwrap().recorded()[0];
        assert_eq!(sent.url, "http://localhost:9200/posts/_update/1");
        assert_eq!(sent.body_text(), r#"{"doc":{"title":"Rust"}}"#);
    }

    #[tokio::test]
    async fn deleting_something_that_is_not_there_is_false_rather_than_an_error() {
        let present = client(
            r#"{"_index":"posts","_id":"1","_version":3,"result":"deleted","_shards":{"total":2,"successful":1,"failed":0},"_seq_no":2,"_primary_term":1}"#,
            200,
        );
        assert!(present.delete_document("posts", "1").await.unwrap());

        let absent = client(
            r#"{"_index":"posts","_id":"nope","_version":1,"result":"not_found","_shards":{"total":2,"successful":1,"failed":0},"_seq_no":4,"_primary_term":1}"#,
            404,
        );
        assert!(!absent.delete_document("posts", "nope").await.unwrap());
    }

    #[tokio::test]
    async fn an_id_that_would_change_the_url_is_encoded() {
        let client = client(r#"{"_index":"posts","_id":"a/b","result":"created"}"#, 201);

        client.index_document("posts", "a/b", Json::Null).await.unwrap();

        assert_eq!(
            client.fake().unwrap().recorded()[0].url,
            "http://localhost:9200/posts/_doc/a%2Fb"
        );
    }

    #[test]
    fn a_bulk_body_is_newline_delimited_with_a_trailing_newline() {
        // The last newline is not optional: without it the cluster answers
        // "The bulk request must be terminated by a newline".
        let mut body = String::new();
        for operation in [
            BulkOperation::index(json(r#"{"title":"a"}"#)).id("1"),
            BulkOperation::create("2", json(r#"{"title":"b"}"#)),
            BulkOperation::update("1", json(r#"{"views":9}"#)),
            BulkOperation::delete("3"),
            BulkOperation::index(json(r#"{"title":"c"}"#)).in_index("other"),
        ] {
            operation.write_into(&mut body);
        }

        assert_eq!(
            body,
            concat!(
                "{\"index\":{\"_id\":\"1\"}}\n{\"title\":\"a\"}\n",
                "{\"create\":{\"_id\":\"2\"}}\n{\"title\":\"b\"}\n",
                "{\"update\":{\"_id\":\"1\"}}\n{\"doc\":{\"views\":9}}\n",
                "{\"delete\":{\"_id\":\"3\"}}\n",
                "{\"index\":{\"_index\":\"other\"}}\n{\"title\":\"c\"}\n",
            )
        );
        assert!(body.ends_with('\n'));
    }

    #[tokio::test]
    async fn a_bulk_request_declares_the_content_type_the_endpoint_is_defined_to_take() {
        let client = client(r#"{"took":1,"errors":false,"items":[]}"#, 200);

        client.bulk("posts", &[BulkOperation::index(Json::Null)]).await.unwrap();

        let sent = &client.fake().unwrap().recorded()[0];
        assert_eq!(sent.headers.get("content-type"), Some("application/x-ndjson"));
        assert_eq!(sent.url, "http://localhost:9200/posts/_bulk");
    }

    #[tokio::test]
    async fn an_empty_bulk_sends_nothing_instead_of_asking_for_a_400() {
        let client = client("", 200);

        let report = client.bulk("posts", &[]).await.unwrap();

        assert_eq!(report.items.len(), 0);
        assert!(report.all_succeeded());
        client.fake().unwrap().assert_count(0);
    }

    /// The trap this module exists for.
    ///
    /// `_bulk` answered 200. Two of the four documents were not written, and
    /// nothing outside the body says so — a caller that checked the status,
    /// or that used `?` and moved on, has just lost them silently.
    ///
    /// The body is copied from a real 8.15 response to a bulk of four
    /// documents against a strict mapping.
    #[tokio::test]
    async fn a_bulk_that_returned_200_can_still_have_lost_documents() {
        let body = r#"{"took":26,"errors":true,"items":[{"index":{"_index":"posts","_id":"1","_version":1,"result":"created","_shards":{"total":2,"successful":1,"failed":0},"_seq_no":0,"_primary_term":1,"status":201}},{"index":{"_index":"posts","_id":"2","status":400,"error":{"type":"document_parsing_exception","reason":"[1:11] failed to parse field [views] of type [long] in document with id '2'. Preview of field's value: 'many'","caused_by":{"type":"illegal_argument_exception","reason":"For input string: \"many\""}}}},{"create":{"_index":"posts","_id":"3","status":409,"error":{"type":"version_conflict_engine_exception","reason":"[3]: version conflict, document already exists (current version [1])","index_uuid":"3zLDcCFFTASdSaKQTa93Xg","shard":"0","index":"posts"}}},{"index":{"_index":"posts","_id":"4","_version":1,"result":"created","_shards":{"total":2,"successful":1,"failed":0},"_seq_no":1,"_primary_term":1,"status":201}}]}"#;
        let client = client(body, 200);

        let report = client
            .bulk(
                "posts",
                &[
                    BulkOperation::index(Json::Null).id("1"),
                    BulkOperation::index(Json::Null).id("2"),
                    BulkOperation::create("3", Json::Null),
                    BulkOperation::index(Json::Null).id("4"),
                ],
            )
            .await
            // The request itself succeeded. That is the whole problem.
            .unwrap();

        assert_eq!(report.succeeded(), 2);
        assert_eq!(report.failed(), 2);
        assert!(!report.all_succeeded());
        assert_eq!(report.failed_positions(), vec![1, 2]);

        // Each failure keeps the reason, classified the way a standalone
        // request would have been.
        let failures = report.failures();
        assert!(matches!(failures[0].error, Some(SearchError::MapperParsing { .. })));
        assert!(matches!(failures[1].error, Some(SearchError::VersionConflict { .. })));
        assert!(
            failures[0].error.as_ref().unwrap().to_string().contains("For input string"),
            "the cause is the half that says what to fix"
        );

        // Neither of these is worth resubmitting, and saying so is the point:
        // a blind retry of the whole batch would duplicate the two that worked.
        assert!(report.retryable_positions().is_empty());

        let error = report.ok_or_error().unwrap_err();
        assert!(matches!(error, SearchError::BulkPartialFailure { failed: 2, total: 4, .. }));
        assert!(error.to_string().contains("2 of 4"), "got {error}");
    }

    #[tokio::test]
    async fn a_bulk_rejected_for_load_names_the_items_worth_sending_again() {
        // Under write pressure the cluster rejects individual items with a 429
        // and accepts the rest. Resubmitting only these is the correct
        // response; resubmitting the batch would duplicate the successes.
        let body = r#"{"took":3,"errors":true,"items":[{"index":{"_index":"logs","_id":"1","_version":1,"result":"created","_shards":{"total":2,"successful":1,"failed":0},"_seq_no":0,"_primary_term":1,"status":201}},{"index":{"_index":"logs","_id":"2","status":429,"error":{"type":"es_rejected_execution_exception","reason":"rejected execution of coordinating operation [coordinating_and_primary_bytes=0, replica_bytes=0, all_bytes=0, coordinating_operation_bytes=206, max_coordinating_and_primary_bytes=105630924]"}}}]}"#;
        let client = client(body, 200);

        let report = client
            .bulk(
                "logs",
                &[
                    BulkOperation::index(Json::Null).id("1"),
                    BulkOperation::index(Json::Null).id("2"),
                ],
            )
            .await
            .unwrap();

        assert_eq!(report.retryable_positions(), vec![1]);
        assert_eq!(report.failed_positions(), vec![1]);
        assert_eq!(report.to_string(), "1 of 2 written in 3ms");
    }

    #[tokio::test]
    async fn a_bulk_where_everything_worked_needs_no_inspection() {
        let body = r#"{"took":8,"errors":false,"items":[{"index":{"_index":"posts","_id":"1","_version":1,"result":"created","_shards":{"total":2,"successful":1,"failed":0},"_seq_no":0,"_primary_term":1,"status":201}}]}"#;
        let client = client(body, 200);

        let report = client.bulk("posts", &[BulkOperation::index(Json::Null).id("1")]).await.unwrap();

        assert!(report.all_succeeded());
        assert!(report.ok_or_error().is_ok());
        assert_eq!(report.items[0].result, Some(WriteResult::Created));
    }

    #[tokio::test]
    async fn a_bulk_across_indices_goes_to_the_root_endpoint() {
        let client = client(r#"{"took":1,"errors":false,"items":[]}"#, 200);

        client
            .bulk_across_indices(&[BulkOperation::index(Json::Null).in_index("posts")])
            .await
            .unwrap();

        assert_eq!(client.fake().unwrap().recorded()[0].url, "http://localhost:9200/_bulk");
    }
}
