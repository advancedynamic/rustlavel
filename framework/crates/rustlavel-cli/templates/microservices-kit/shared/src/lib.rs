//! What crosses between the services, and nothing else.
//!
//! The rule this crate exists to enforce: a type here is part of a contract two
//! services have agreed on, so changing it changes both. Anything that belongs
//! to one service belongs *in* that service, where it can change without a
//! meeting.
//!
//! In particular there are no models here. A model is a table, a table belongs
//! to one database, and one database belongs to one service — the moment two
//! services share a model they share a schema, and then a migration, and then
//! they are one service deployed twice.

use rustlavel::prelude::*;

/// Who a request is for, once a token has been checked.
///
/// The gateway and every service agree on this shape, which is what lets the
/// resource server be written without knowing which way the token was verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caller {
    pub subject: String,
    pub client: Option<String>,
    pub scopes: Vec<String>,
}

impl Caller {
    pub fn can(&self, scope: &str) -> bool {
        self.scopes.iter().any(|held| held == scope)
    }

    /// As JSON, for a service that passes it on.
    pub fn to_json(&self) -> Json {
        Json::object([
            ("subject", Json::from(self.subject.as_str())),
            (
                "client",
                match &self.client {
                    Some(client) => Json::from(client.as_str()),
                    None => Json::Null,
                },
            ),
            (
                "scopes",
                Json::Array(self.scopes.iter().map(|s| Json::from(s.as_str())).collect()),
            ),
        ])
    }
}

/// The error body every service answers with.
///
/// One shape, so a client written against one service can read the failures of
/// all of them. A gateway in front of services that each invent their own error
/// format is a gateway that has unified nothing.
pub fn problem(message: &str) -> Json {
    Json::object([("message", Json::from(message))])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_caller_holds_only_the_scopes_it_was_given() {
        let caller = Caller {
            subject: "42".into(),
            client: Some("checkout".into()),
            scopes: vec!["orders.read".into()],
        };
        assert!(caller.can("orders.read"));
        assert!(!caller.can("orders.write"));
        assert!(!caller.can(""));
    }

    #[test]
    fn the_error_body_is_one_shape() {
        assert_eq!(problem("no").to_string(), r#"{"message":"no"}"#);
    }
}
