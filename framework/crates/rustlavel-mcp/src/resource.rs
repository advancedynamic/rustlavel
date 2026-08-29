//! A resource: something an application lets an agent *read*.
//!
//! Where a tool is a verb, a resource is a noun with a URI — the route list,
//! the database schema, a document. An agent pulls it into context without the
//! side effects a tool call implies.

use crate::protocol::ResourceInfo;
use rustlavel_core::{Json, Result};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

pub type ResourceFuture = Pin<Box<dyn Future<Output = Result<String>> + Send>>;

/// The work behind a resource. Implemented for every `async fn() -> Result<String>`.
pub trait ResourceReader: Send + Sync + 'static {
    fn read(&self) -> ResourceFuture;
}

impl<F, Fut> ResourceReader for F
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<String>> + Send + 'static,
{
    fn read(&self) -> ResourceFuture {
        Box::pin(self())
    }
}

/// One registered resource.
#[derive(Clone)]
pub struct Resource {
    pub uri: String,
    pub name: String,
    pub description: Option<String>,
    pub mime_type: String,
    reader: Arc<dyn ResourceReader>,
}

impl Resource {
    pub fn new(
        uri: impl Into<String>,
        name: impl Into<String>,
        mime_type: impl Into<String>,
        reader: impl ResourceReader,
    ) -> Self {
        Resource {
            uri: uri.into(),
            name: name.into(),
            description: None,
            mime_type: mime_type.into(),
            reader: Arc::new(reader),
        }
    }

    /// A JSON resource, the common case for anything an application exposes.
    pub fn json(
        uri: impl Into<String>,
        name: impl Into<String>,
        reader: impl ResourceReader,
    ) -> Self {
        Resource::new(uri, name, "application/json", reader)
    }

    pub fn text(
        uri: impl Into<String>,
        name: impl Into<String>,
        reader: impl ResourceReader,
    ) -> Self {
        Resource::new(uri, name, "text/plain", reader)
    }

    pub fn describe(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    pub fn info(&self) -> ResourceInfo {
        ResourceInfo {
            uri: self.uri.clone(),
            name: self.name.clone(),
            description: self.description.clone(),
            mime_type: self.mime_type.clone(),
        }
    }

    /// Read the contents, surviving a panicking reader the same way a tool does.
    pub async fn read(&self) -> Result<String> {
        rustlavel_http::panic::install_hook();

        let reader = Arc::clone(&self.reader);
        match rustlavel_http::panic::catch(async move { reader.read().await }).await {
            Ok(result) => result,
            Err(message) => {
                rustlavel_core::error!("panic reading MCP resource `{}`: {message}", self.uri);
                Err(rustlavel_core::Error::msg(format!("`{}` panicked: {message}", self.uri)))
            }
        }
    }

    /// The `resources/read` payload: one entry, the way the specification
    /// spells a single-document resource.
    pub async fn contents(&self) -> Result<Json> {
        let text = self.read().await?;
        Ok(Json::object([(
            "contents",
            Json::Array(vec![Json::object([
                ("uri", Json::from(self.uri.clone())),
                ("mimeType", Json::from(self.mime_type.clone())),
                ("text", Json::from(text)),
            ])]),
        )]))
    }
}

impl std::fmt::Debug for Resource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Resource").field("uri", &self.uri).field("name", &self.name).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_resource_reads_its_contents_in_the_wire_shape() {
        let resource = Resource::json("app://routes", "Routes", || async {
            Ok(r#"{"routes":[]}"#.to_string())
        })
        .describe("Every registered route");

        let contents = resource.contents().await.unwrap();

        assert_eq!(contents.get("contents.0.uri").unwrap().as_str(), Some("app://routes"));
        assert_eq!(
            contents.get("contents.0.mimeType").unwrap().as_str(),
            Some("application/json")
        );
        assert_eq!(contents.get("contents.0.text").unwrap().as_str(), Some(r#"{"routes":[]}"#));
    }

    #[test]
    fn a_resource_lists_itself_with_its_description() {
        let info = Resource::text("app://readme", "Readme", || async { Ok(String::new()) })
            .describe("How to use this app")
            .info();

        assert_eq!(info.mime_type, "text/plain");
        assert_eq!(info.description.as_deref(), Some("How to use this app"));
        assert_eq!(info.to_json().get("uri").unwrap().as_str(), Some("app://readme"));
    }

    #[tokio::test]
    async fn a_panicking_reader_becomes_an_error_not_a_crash() {
        let resource = Resource::text("app://bad", "Bad", || async { panic!("no such file") });

        let error = resource.read().await.unwrap_err().to_string();
        assert!(error.contains("no such file"), "{error}");
    }
}
