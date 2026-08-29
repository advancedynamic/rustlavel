//! Tools: describing them, registering handlers, and running the loop.
//!
//! A tool is a function the model may ask for by name. Every provider wants
//! the arguments described as JSON Schema, which nobody enjoys writing by
//! hand, so [`Schema`] builds it:
//!
//! ```ignore
//! let tool = Tool::new("get_weather", "Look up today's weather for a city")
//!     .string("city", "The city, e.g. Oslo")
//!     .one_of("unit", "Temperature unit", &["celsius", "fahrenheit"]).optional();
//! ```

use crate::completion::{Completion, Usage};
use crate::message::Message;
use crate::provider::{BoxFuture, Provider};
use crate::request::Request;
use rustlavel_core::{Error, Json, Result};
use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;

/// A JSON Schema object, built a field at a time.
///
/// Only the subset every provider agrees on is modelled — an object of named,
/// typed, described fields, some required. Anything more exotic can still be
/// handed over with [`Schema::raw`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Schema {
    properties: Vec<(String, Json)>,
    required: Vec<String>,
    description: Option<String>,
    /// A hand-written schema, used instead of the fields when present.
    raw: Option<Json>,
}

impl Schema {
    pub fn object() -> Schema {
        Schema::default()
    }

    /// Use a schema written out by hand, for shapes this builder does not cover.
    pub fn raw(schema: Json) -> Schema {
        Schema { raw: Some(schema), ..Schema::default() }
    }

    /// Describe the object itself, which some models read as extra instruction.
    pub fn describe(mut self, description: impl Into<String>) -> Schema {
        self.description = Some(description.into());
        self
    }

    pub fn string(self, name: &str, description: &str) -> Schema {
        self.field(name, "string", description)
    }

    pub fn integer(self, name: &str, description: &str) -> Schema {
        self.field(name, "integer", description)
    }

    pub fn number(self, name: &str, description: &str) -> Schema {
        self.field(name, "number", description)
    }

    pub fn boolean(self, name: &str, description: &str) -> Schema {
        self.field(name, "boolean", description)
    }

    /// A string field limited to a fixed set of values.
    pub fn one_of(mut self, name: &str, description: &str, values: &[&str]) -> Schema {
        let values: Vec<Json> = values.iter().map(|v| Json::from(*v)).collect();
        self.put(
            name,
            Json::object([
                ("type", Json::from("string")),
                ("description", Json::from(description)),
                ("enum", Json::Array(values)),
            ]),
        );
        self
    }

    /// An array whose items are all of one primitive type.
    pub fn array_of(mut self, name: &str, description: &str, item_type: &str) -> Schema {
        self.put(
            name,
            Json::object([
                ("type", Json::from("array")),
                ("description", Json::from(description)),
                ("items", Json::object([("type", Json::from(item_type))])),
            ]),
        );
        self
    }

    /// A nested object.
    pub fn nested(mut self, name: &str, description: &str, schema: Schema) -> Schema {
        let mut inner = schema.to_json();
        if let Json::Object(map) = &mut inner {
            map.insert("description".into(), Json::from(description));
        }
        self.put(name, inner);
        self
    }

    /// Make the field just added optional.
    ///
    /// Fields are required by default because that is what a caller means far
    /// more often, and because an optional field a model omits is a `None` the
    /// handler has to think about.
    pub fn optional(mut self) -> Schema {
        self.required.pop();
        self
    }

    fn field(mut self, name: &str, kind: &str, description: &str) -> Schema {
        self.put(
            name,
            Json::object([
                ("type", Json::from(kind)),
                ("description", Json::from(description)),
            ]),
        );
        self
    }

    fn put(&mut self, name: &str, definition: Json) {
        self.properties.push((name.to_string(), definition));
        self.required.push(name.to_string());
    }

    /// The schema as the wire wants it.
    pub fn to_json(&self) -> Json {
        if let Some(raw) = &self.raw {
            return raw.clone();
        }

        let mut object = BTreeMap::new();
        object.insert("type".to_string(), Json::from("object"));
        if let Some(description) = &self.description {
            object.insert("description".to_string(), Json::from(description.as_str()));
        }
        object.insert(
            "properties".to_string(),
            Json::object(self.properties.iter().map(|(k, v)| (k.as_str(), v.clone()))),
        );
        object.insert(
            "required".to_string(),
            Json::Array(self.required.iter().map(|name| Json::from(name.as_str())).collect()),
        );
        Json::Object(object)
    }

    pub fn is_empty(&self) -> bool {
        self.raw.is_none() && self.properties.is_empty()
    }
}

/// A function the model may ask for.
#[derive(Debug, Clone, PartialEq)]
pub struct Tool {
    pub name: String,
    /// What it does, in prose. This is the only thing the model has to go on
    /// when deciding whether to reach for it, so it earns its length.
    pub description: String,
    pub parameters: Schema,
}

impl Tool {
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Tool {
        Tool {
            name: name.into(),
            description: description.into(),
            parameters: Schema::object(),
        }
    }

    pub fn parameters(mut self, schema: Schema) -> Tool {
        self.parameters = schema;
        self
    }

    pub fn string(mut self, name: &str, description: &str) -> Tool {
        self.parameters = self.parameters.string(name, description);
        self
    }

    pub fn integer(mut self, name: &str, description: &str) -> Tool {
        self.parameters = self.parameters.integer(name, description);
        self
    }

    pub fn number(mut self, name: &str, description: &str) -> Tool {
        self.parameters = self.parameters.number(name, description);
        self
    }

    pub fn boolean(mut self, name: &str, description: &str) -> Tool {
        self.parameters = self.parameters.boolean(name, description);
        self
    }

    pub fn one_of(mut self, name: &str, description: &str, values: &[&str]) -> Tool {
        self.parameters = self.parameters.one_of(name, description, values);
        self
    }

    pub fn array_of(mut self, name: &str, description: &str, item_type: &str) -> Tool {
        self.parameters = self.parameters.array_of(name, description, item_type);
        self
    }

    /// Make the parameter just added optional.
    pub fn optional(mut self) -> Tool {
        self.parameters = self.parameters.optional();
        self
    }

    pub fn schema_json(&self) -> Json {
        self.parameters.to_json()
    }
}

/// A tool handler: takes the arguments the model chose, answers with JSON.
type Handler = Arc<dyn Fn(Json) -> BoxFuture<'static, Result<Json>> + Send + Sync>;

/// Tools plus the code that runs them.
///
/// Cloning is cheap and shares the handlers, so a toolbox can be built once at
/// boot and handed to every request.
#[derive(Clone, Default)]
pub struct Toolbox {
    tools: Vec<Tool>,
    handlers: BTreeMap<String, Handler>,
}

impl Toolbox {
    pub fn new() -> Toolbox {
        Toolbox::default()
    }

    /// Register a tool and the async function that answers it.
    pub fn add<F, Fut>(mut self, tool: Tool, handler: F) -> Toolbox
    where
        F: Fn(Json) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Json>> + Send + 'static,
    {
        let name = tool.name.clone();
        self.tools.retain(|existing| existing.name != name);
        self.tools.push(tool);
        self.handlers.insert(name, Arc::new(move |input| Box::pin(handler(input))));
        self
    }

    pub fn tools(&self) -> &[Tool] {
        &self.tools
    }

    pub fn has(&self, name: &str) -> bool {
        self.handlers.contains_key(name)
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Run one tool by name.
    pub async fn call(&self, name: &str, arguments: Json) -> Result<Json> {
        let handler = self.handlers.get(name).ok_or_else(|| {
            Error::msg(format!(
                "the model asked for a tool named `{name}`, which is not registered. \
                 Registered tools: {}",
                self.tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>().join(", ")
            ))
        })?;
        handler(arguments).await
    }
}

impl std::fmt::Debug for Toolbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Toolbox")
            .field("tools", &self.tools.iter().map(|t| &t.name).collect::<Vec<_>>())
            .finish()
    }
}

/// How many model turns a tool loop may take before giving up.
///
/// A model that keeps asking for tools forever is a bug, not a conversation,
/// and an uncapped loop bills for it.
pub const DEFAULT_MAX_ROUNDS: usize = 8;

/// Ask the model, run whatever tools it asks for, feed the results back, and
/// repeat until it answers in words.
///
/// Usage is summed across every round: one loop is one operation as far as the
/// caller's budget is concerned.
pub async fn run_tools(
    provider: &dyn Provider,
    request: &Request,
    toolbox: &Toolbox,
    max_rounds: usize,
) -> Result<Completion> {
    let mut request = request.clone();
    if request.tools.is_empty() {
        request.tools = toolbox.tools().to_vec();
    }

    let mut total = Usage::default();

    for round in 0..max_rounds.max(1) {
        let mut completion = provider.complete(&request).await?;
        total.add(completion.usage);

        if !completion.wants_tools() {
            completion.usage = total;
            return Ok(completion);
        }

        request.messages.push(completion.to_message());
        for call in &completion.tool_calls {
            let message = match toolbox.call(&call.name, call.arguments.clone()).await {
                Ok(output) => Message::tool_result(&call.id, &call.name, output),
                // A handler that fails is reported to the model, not to the
                // caller: the model can usually recover, and if it cannot the
                // loop ends normally with an explanation.
                Err(error) => Message::tool_error(&call.id, &call.name, error.to_string()),
            };
            request.messages.push(message);
        }

        if round + 1 == max_rounds.max(1) {
            return Err(Error::msg(format!(
                "the model still wanted tools after {max_rounds} rounds; \
                 raise the limit or check that the tools answer usefully"
            )));
        }
    }

    unreachable!("the loop returns or errors on its last round")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn weather_tool() -> Tool {
        Tool::new("get_weather", "Look up today's weather")
            .string("city", "The city to look up")
            .one_of("unit", "Temperature unit", &["celsius", "fahrenheit"])
            .optional()
    }

    #[test]
    fn a_schema_is_built_without_hand_writing_json() {
        let schema = weather_tool().schema_json();

        assert_eq!(schema.get("type").unwrap().as_str(), Some("object"));
        assert_eq!(schema.get("properties.city.type").unwrap().as_str(), Some("string"));
        assert_eq!(
            schema.get("properties.city.description").unwrap().as_str(),
            Some("The city to look up")
        );
        assert_eq!(schema.get("properties.unit.enum.0").unwrap().as_str(), Some("celsius"));

        // `city` is required; `unit` was marked optional.
        let required = schema.get("required").unwrap().as_array().unwrap();
        assert_eq!(required.len(), 1);
        assert_eq!(required[0].as_str(), Some("city"));
    }

    #[test]
    fn schemas_describe_arrays_and_nested_objects() {
        let schema = Schema::object()
            .describe("A search")
            .array_of("terms", "Words to look for", "string")
            .nested("page", "Paging", Schema::object().integer("size", "How many"))
            .to_json();

        assert_eq!(schema.get("description").unwrap().as_str(), Some("A search"));
        assert_eq!(schema.get("properties.terms.items.type").unwrap().as_str(), Some("string"));
        assert_eq!(schema.get("properties.page.type").unwrap().as_str(), Some("object"));
        assert_eq!(schema.get("properties.page.description").unwrap().as_str(), Some("Paging"));
        assert_eq!(
            schema.get("properties.page.properties.size.type").unwrap().as_str(),
            Some("integer")
        );
    }

    #[test]
    fn a_hand_written_schema_is_passed_through_untouched() {
        let raw = Json::object([("type", "string".into())]);
        assert_eq!(Schema::raw(raw.clone()).to_json(), raw);
    }

    #[test]
    fn marking_optional_with_no_fields_is_harmless() {
        assert!(Schema::object().optional().to_json().get("required").is_some());
    }

    #[tokio::test]
    async fn a_toolbox_runs_the_handler_it_was_given() {
        let toolbox = Toolbox::new().add(weather_tool(), |input: Json| async move {
            let city = input.get("city").and_then(Json::as_str).unwrap_or("nowhere").to_string();
            Ok(Json::object([("city", Json::from(city)), ("degrees", 7.into())]))
        });

        assert_eq!(toolbox.len(), 1);
        assert!(toolbox.has("get_weather"));

        let answer = toolbox
            .call("get_weather", Json::object([("city", "Oslo".into())]))
            .await
            .unwrap();

        assert_eq!(answer.get("city").unwrap().as_str(), Some("Oslo"));
        assert_eq!(answer.get("degrees").unwrap().as_i64(), Some(7));
    }

    #[tokio::test]
    async fn an_unknown_tool_names_the_ones_that_do_exist() {
        let toolbox = Toolbox::new().add(weather_tool(), |_| async { Ok(Json::Null) });

        let error = toolbox.call("get_tides", Json::Null).await.unwrap_err().to_string();

        assert!(error.contains("get_tides"), "{error}");
        assert!(error.contains("get_weather"), "{error}");
    }

    #[test]
    fn registering_a_tool_twice_replaces_it() {
        let toolbox = Toolbox::new()
            .add(Tool::new("t", "first"), |_| async { Ok(Json::Null) })
            .add(Tool::new("t", "second"), |_| async { Ok(Json::Null) });

        assert_eq!(toolbox.len(), 1);
        assert_eq!(toolbox.tools()[0].description, "second");
        assert!(format!("{toolbox:?}").contains('t'));
    }
}
