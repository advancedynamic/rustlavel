//! JSON-RPC 2.0, written on `rustlavel_core::Json`.
//!
//! MCP speaks JSON-RPC and nothing else, so this layer knows nothing about
//! tools or resources: it only understands requests, notifications, responses,
//! error objects, and batches. Keeping it separate is what lets the protocol
//! tests below be written against hand-typed JSON rather than against the
//! server's behaviour.

use rustlavel_core::Json;
use std::fmt;

/// The error codes JSON-RPC reserves, plus the one MCP adds.
pub mod codes {
    /// The payload was not valid JSON at all.
    pub const PARSE_ERROR: i64 = -32700;
    /// Valid JSON, but not a valid JSON-RPC message.
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;
    /// MCP's own reserved code for a resource URI the server does not serve.
    pub const RESOURCE_NOT_FOUND: i64 = -32002;
}

/// A request id. JSON-RPC allows a number or a string, and a client is free to
/// use either, so correlation has to handle both.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Id {
    Number(i64),
    Text(String),
}

impl Id {
    pub fn to_json(&self) -> Json {
        match self {
            Id::Number(n) => Json::from(*n),
            Id::Text(s) => Json::from(s.clone()),
        }
    }

    /// Read an id, rejecting the shapes JSON-RPC does not allow (objects,
    /// arrays, booleans) rather than coercing them.
    pub fn from_json(value: &Json) -> Option<Id> {
        match value {
            Json::Number(n) => Some(Id::Number(*n as i64)),
            Json::String(s) => Some(Id::Text(s.clone())),
            _ => None,
        }
    }
}

impl From<i64> for Id {
    fn from(value: i64) -> Self {
        Id::Number(value)
    }
}

impl From<&str> for Id {
    fn from(value: &str) -> Self {
        Id::Text(value.to_string())
    }
}

impl From<String> for Id {
    fn from(value: String) -> Self {
        Id::Text(value)
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Id::Number(n) => write!(f, "{n}"),
            Id::Text(s) => f.write_str(s),
        }
    }
}

/// A JSON-RPC error object.
#[derive(Debug, Clone, PartialEq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    /// Extra detail — the list of validation failures, the offending method.
    pub data: Option<Json>,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        RpcError { code, message: message.into(), data: None }
    }

    pub fn with_data(mut self, data: Json) -> Self {
        self.data = Some(data);
        self
    }

    pub fn parse_error(detail: impl fmt::Display) -> Self {
        RpcError::new(codes::PARSE_ERROR, format!("Parse error: {detail}"))
    }

    pub fn invalid_request(detail: impl fmt::Display) -> Self {
        RpcError::new(codes::INVALID_REQUEST, format!("Invalid Request: {detail}"))
    }

    pub fn method_not_found(method: &str) -> Self {
        RpcError::new(codes::METHOD_NOT_FOUND, format!("Method not found: {method}"))
    }

    pub fn invalid_params(detail: impl fmt::Display) -> Self {
        RpcError::new(codes::INVALID_PARAMS, format!("Invalid params: {detail}"))
    }

    pub fn internal_error(detail: impl fmt::Display) -> Self {
        RpcError::new(codes::INTERNAL_ERROR, format!("Internal error: {detail}"))
    }

    pub fn resource_not_found(uri: &str) -> Self {
        RpcError::new(codes::RESOURCE_NOT_FOUND, format!("Resource not found: {uri}"))
            .with_data(Json::object([("uri", Json::from(uri))]))
    }

    pub fn to_json(&self) -> Json {
        let mut pairs = vec![
            ("code", Json::from(self.code)),
            ("message", Json::from(self.message.clone())),
        ];
        if let Some(data) = &self.data {
            pairs.push(("data", data.clone()));
        }
        Json::object(pairs)
    }

    pub fn from_json(value: &Json) -> Option<RpcError> {
        let code = value.get("code")?.as_i64()?;
        let message = value.get("message")?.as_str()?.to_string();
        Some(RpcError { code, message, data: value.get("data").cloned() })
    }
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

/// A call that expects an answer.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub id: Id,
    pub method: String,
    /// `Json::Null` when the caller sent no params, so callers never unwrap.
    pub params: Json,
}

impl Request {
    pub fn new(id: impl Into<Id>, method: impl Into<String>, params: Json) -> Self {
        Request { id: id.into(), method: method.into(), params }
    }

    pub fn to_json(&self) -> Json {
        let mut pairs = vec![
            ("jsonrpc", Json::from(VERSION)),
            ("id", self.id.to_json()),
            ("method", Json::from(self.method.clone())),
        ];
        if !self.params.is_null() {
            pairs.push(("params", self.params.clone()));
        }
        Json::object(pairs)
    }
}

/// A call that expects no answer.
#[derive(Debug, Clone, PartialEq)]
pub struct Notification {
    pub method: String,
    pub params: Json,
}

impl Notification {
    pub fn new(method: impl Into<String>, params: Json) -> Self {
        Notification { method: method.into(), params }
    }

    pub fn to_json(&self) -> Json {
        let mut pairs =
            vec![("jsonrpc", Json::from(VERSION)), ("method", Json::from(self.method.clone()))];
        if !self.params.is_null() {
            pairs.push(("params", self.params.clone()));
        }
        Json::object(pairs)
    }
}

/// An answer: exactly one of `result` or `error`, never both.
///
/// The id is optional because an error raised before the id could be read —
/// a malformed frame — must still be reported, with a null id.
#[derive(Debug, Clone, PartialEq)]
pub struct Response {
    pub id: Option<Id>,
    pub result: Result<Json, RpcError>,
}

impl Response {
    pub fn success(id: Id, result: Json) -> Self {
        Response { id: Some(id), result: Ok(result) }
    }

    pub fn failure(id: Option<Id>, error: RpcError) -> Self {
        Response { id, result: Err(error) }
    }

    pub fn to_json(&self) -> Json {
        let id = self.id.as_ref().map_or(Json::Null, Id::to_json);
        match &self.result {
            Ok(value) => Json::object([
                ("jsonrpc", Json::from(VERSION)),
                ("id", id),
                ("result", value.clone()),
            ]),
            Err(error) => Json::object([
                ("jsonrpc", Json::from(VERSION)),
                ("id", id),
                ("error", error.to_json()),
            ]),
        }
    }
}

/// The protocol version string that must appear on every message.
pub const VERSION: &str = "2.0";

/// Any one JSON-RPC message.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    Request(Request),
    Notification(Notification),
    Response(Response),
}

impl Message {
    /// Decode one message object.
    ///
    /// The shape decides the variant: `method` with an `id` is a request,
    /// `method` without one is a notification, and `result`/`error` is an
    /// answer to something we sent.
    pub fn from_json(value: &Json) -> Result<Message, RpcError> {
        let Some(object) = value.as_object() else {
            return Err(RpcError::invalid_request("a message must be an object"));
        };
        match object.get("jsonrpc").and_then(Json::as_str) {
            Some(VERSION) => {}
            Some(other) => {
                return Err(RpcError::invalid_request(format!("unsupported version `{other}`")));
            }
            None => return Err(RpcError::invalid_request("missing the `jsonrpc` field")),
        }

        if let Some(method) = object.get("method") {
            let Some(method) = method.as_str() else {
                return Err(RpcError::invalid_request("`method` must be a string"));
            };
            let params = object.get("params").cloned().unwrap_or(Json::Null);
            return match object.get("id") {
                None => Ok(Message::Notification(Notification::new(method, params))),
                Some(raw) => match Id::from_json(raw) {
                    Some(id) => Ok(Message::Request(Request::new(id, method, params))),
                    None => Err(RpcError::invalid_request("`id` must be a string or a number")),
                },
            };
        }

        let id = object.get("id").and_then(Id::from_json);
        if let Some(error) = object.get("error") {
            let error = RpcError::from_json(error)
                .ok_or_else(|| RpcError::invalid_request("malformed error object"))?;
            return Ok(Message::Response(Response::failure(id, error)));
        }
        if let Some(result) = object.get("result") {
            let id = id.ok_or_else(|| RpcError::invalid_request("a result needs an id"))?;
            return Ok(Message::Response(Response::success(id, result.clone())));
        }

        Err(RpcError::invalid_request("neither a call nor an answer"))
    }

    pub fn to_json(&self) -> Json {
        match self {
            Message::Request(request) => request.to_json(),
            Message::Notification(notification) => notification.to_json(),
            Message::Response(response) => response.to_json(),
        }
    }

    /// Decode a single message straight from text.
    pub fn parse(text: &str) -> Result<Message, RpcError> {
        let value = Json::parse(text).map_err(RpcError::parse_error)?;
        Message::from_json(&value)
    }
}

impl fmt::Display for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_json())
    }
}

/// One frame off a transport: a lone message or a batch of them.
///
/// The frame is kept as raw `Json` rather than decoded messages because a
/// single bad element of a batch must produce its own error response while its
/// well-formed neighbours are still answered.
#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    Single(Json),
    Batch(Vec<Json>),
}

impl Frame {
    pub fn parse(text: &str) -> Result<Frame, RpcError> {
        let value = Json::parse(text).map_err(RpcError::parse_error)?;
        match value {
            Json::Array(items) if items.is_empty() => {
                Err(RpcError::invalid_request("an empty batch has nothing to answer"))
            }
            Json::Array(items) => Ok(Frame::Batch(items)),
            single => Ok(Frame::Single(single)),
        }
    }

    /// Every element, so a caller can decode them one at a time.
    pub fn items(&self) -> &[Json] {
        match self {
            Frame::Single(value) => std::slice::from_ref(value),
            Frame::Batch(items) => items,
        }
    }

    pub fn is_batch(&self) -> bool {
        matches!(self, Frame::Batch(_))
    }
}

/// Serialize answers back onto a transport.
///
/// `None` means "send nothing": a frame made only of notifications has no
/// reply, which JSON-RPC is explicit about.
pub fn encode(responses: Vec<Json>, batched: bool) -> Option<String> {
    match (responses.is_empty(), batched) {
        (true, _) => None,
        (false, true) => Some(Json::Array(responses).to_string()),
        (false, false) => responses.into_iter().next().map(|value| value.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_encodes_to_the_wire_shape() {
        let request = Request::new(
            1i64,
            "tools/call",
            Json::object([("name", Json::from("weather"))]),
        );

        assert_eq!(
            request.to_json().to_string(),
            r#"{"id":1,"jsonrpc":"2.0","method":"tools/call","params":{"name":"weather"}}"#
        );
    }

    #[test]
    fn a_request_without_params_omits_the_field() {
        let request = Request::new("abc", "tools/list", Json::Null);
        assert_eq!(request.to_json().to_string(), r#"{"id":"abc","jsonrpc":"2.0","method":"tools/list"}"#);
    }

    #[test]
    fn decodes_a_hand_written_request() {
        let message = Message::parse(r#"{"jsonrpc":"2.0","id":7,"method":"ping","params":{}}"#).unwrap();

        match message {
            Message::Request(request) => {
                assert_eq!(request.id, Id::Number(7));
                assert_eq!(request.method, "ping");
                assert!(request.params.as_object().unwrap().is_empty());
            }
            other => panic!("expected a request, got {other:?}"),
        }
    }

    #[test]
    fn a_message_without_an_id_decodes_as_a_notification() {
        let message = Message::parse(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).unwrap();

        assert_eq!(
            message,
            Message::Notification(Notification::new("notifications/initialized", Json::Null))
        );
    }

    #[test]
    fn decodes_both_kinds_of_answer() {
        let ok = Message::parse(r#"{"jsonrpc":"2.0","id":"a","result":{"ok":true}}"#).unwrap();
        let bad =
            Message::parse(r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"nope"}}"#)
                .unwrap();

        match ok {
            Message::Response(response) => {
                assert_eq!(response.id, Some(Id::Text("a".into())));
                assert_eq!(response.result.unwrap().get("ok").unwrap().as_bool(), Some(true));
            }
            other => panic!("expected a response, got {other:?}"),
        }
        match bad {
            Message::Response(response) => {
                assert_eq!(response.id, None);
                assert_eq!(response.result.unwrap_err().code, codes::PARSE_ERROR);
            }
            other => panic!("expected a response, got {other:?}"),
        }
    }

    #[test]
    fn string_and_number_ids_both_survive_a_round_trip() {
        for id in [Id::Number(-3), Id::Text("req-9".into())] {
            let encoded = Request::new(id.clone(), "ping", Json::Null).to_json();
            let Message::Request(decoded) = Message::from_json(&encoded).unwrap() else {
                panic!("not a request");
            };
            assert_eq!(decoded.id, id);
        }
    }

    #[test]
    fn malformed_json_is_a_parse_error() {
        let error = Message::parse("{ not json").unwrap_err();
        assert_eq!(error.code, codes::PARSE_ERROR);
    }

    #[test]
    fn a_missing_version_field_is_an_invalid_request() {
        let error = Message::parse(r#"{"id":1,"method":"ping"}"#).unwrap_err();
        assert_eq!(error.code, codes::INVALID_REQUEST);

        let wrong = Message::parse(r#"{"jsonrpc":"1.0","id":1,"method":"ping"}"#).unwrap_err();
        assert_eq!(wrong.code, codes::INVALID_REQUEST);
    }

    #[test]
    fn a_non_object_message_is_an_invalid_request() {
        assert_eq!(Message::parse("42").unwrap_err().code, codes::INVALID_REQUEST);
        assert_eq!(
            Message::parse(r#"{"jsonrpc":"2.0","id":1}"#).unwrap_err().code,
            codes::INVALID_REQUEST
        );
    }

    #[test]
    fn an_id_that_is_an_object_is_rejected() {
        let error = Message::parse(r#"{"jsonrpc":"2.0","id":{"a":1},"method":"ping"}"#).unwrap_err();
        assert_eq!(error.code, codes::INVALID_REQUEST);
    }

    #[test]
    fn a_batch_frame_keeps_every_element() {
        let frame = Frame::parse(
            r#"[{"jsonrpc":"2.0","id":1,"method":"ping"},{"jsonrpc":"2.0","method":"hi"},7]"#,
        )
        .unwrap();

        assert!(frame.is_batch());
        assert_eq!(frame.items().len(), 3);
        // The bad element only fails when it is decoded, not when the frame is.
        assert_eq!(
            Message::from_json(&frame.items()[2]).unwrap_err().code,
            codes::INVALID_REQUEST
        );
    }

    #[test]
    fn an_empty_batch_is_an_invalid_request() {
        assert_eq!(Frame::parse("[]").unwrap_err().code, codes::INVALID_REQUEST);
    }

    #[test]
    fn a_single_frame_still_exposes_one_item() {
        let frame = Frame::parse(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#).unwrap();
        assert!(!frame.is_batch());
        assert_eq!(frame.items().len(), 1);
    }

    #[test]
    fn encoding_answers_matches_the_frame_it_answers() {
        let one = Response::success(Id::Number(1), Json::from(true)).to_json();

        assert_eq!(
            encode(vec![one.clone()], false).unwrap(),
            r#"{"id":1,"jsonrpc":"2.0","result":true}"#
        );
        assert!(encode(vec![one], true).unwrap().starts_with('['));
        // Notifications only: nothing to send back.
        assert_eq!(encode(Vec::new(), true), None);
        assert_eq!(encode(Vec::new(), false), None);
    }

    #[test]
    fn error_objects_carry_their_data_through_a_round_trip() {
        let error = RpcError::invalid_params("city must be a string")
            .with_data(Json::object([("field", Json::from("city"))]));
        let decoded = RpcError::from_json(&error.to_json()).unwrap();

        assert_eq!(decoded, error);
        assert_eq!(decoded.code, codes::INVALID_PARAMS);
        assert_eq!(decoded.data.unwrap().get("field").unwrap().as_str(), Some("city"));
    }

    #[test]
    fn the_reserved_codes_are_the_ones_the_specification_names() {
        assert_eq!(RpcError::parse_error("x").code, -32700);
        assert_eq!(RpcError::invalid_request("x").code, -32600);
        assert_eq!(RpcError::method_not_found("x").code, -32601);
        assert_eq!(RpcError::invalid_params("x").code, -32602);
        assert_eq!(RpcError::internal_error("x").code, -32603);
        assert_eq!(RpcError::resource_not_found("file:///x").code, -32002);
    }
}
