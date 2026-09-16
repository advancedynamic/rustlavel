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
    /// The person this was issued to, or `None` when the caller is a service.
    ///
    /// A `client_credentials` token has no resource owner — RFC 6749 §4.4 — so
    /// the authorization server omits `sub`, and that absence is the *answer*
    /// rather than a gap. Keeping it optional is what stops a service's calls
    /// being recorded against a person.
    pub subject: Option<String>,
    pub client: Option<String>,
    pub scopes: Vec<String>,
}

impl Caller {
    pub fn can(&self, scope: &str) -> bool {
        self.scopes.iter().any(|held| held == scope)
    }

    /// Who a row belongs to: the person when there is one, the service
    /// otherwise. Distinct by construction — a client id and a user id come
    /// from different tables — so a service's rows never collide with a
    /// person's.
    pub fn owner(&self) -> &str {
        self.subject.as_deref().or(self.client.as_deref()).unwrap_or("unknown")
    }

    /// Whether this is a service rather than a person. Ask before doing
    /// something only a person may do.
    pub fn is_service(&self) -> bool {
        self.subject.is_none()
    }

    /// As JSON, for a service that passes it on.
    pub fn to_json(&self) -> Json {
        Json::object([
            (
                "subject",
                match &self.subject {
                    Some(subject) => Json::from(subject.as_str()),
                    None => Json::Null,
                },
            ),
            ("is_service", Json::from(self.is_service())),
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

/// Announce this service to the registry, and keep the registration alive.
///
/// Does nothing when `DISCOVERY_URL` is unset, which is the ordinary case on a
/// platform that already tracks instances. Returns the registrar so a service
/// can `deregister().await` on the way down — the difference between a clean
/// deploy and thirty seconds of failures.
///
/// The port comes from `server.port`, the same value the server binds, so a
/// service cannot register one port and listen on another.
///
/// **`SERVICE_HOST` must be an address other machines can reach.** The registry
/// hands out what it is told, so a service that registers `127.0.0.1` has told
/// every caller in the estate to talk to itself. The default below is right for
/// one machine and wrong for every other arrangement.
pub fn join(service: &str, config: &Config) -> Option<std::sync::Arc<rustlavel::Registrar>> {
    // **Not for a console command.** `migrate`, `db:seed` and the rest run in
    // the same `main` and exit within seconds. Announcing the service there
    // would leave a registration nothing is heartbeating, and the registry
    // would go on handing that address to the gateway for the length of a
    // lease — traffic sent to a process that has already exited. Found by
    // running `migrate` and watching it register.
    if is_console() {
        return None;
    }

    let registry = rustlavel::env::env_or("DISCOVERY_URL", "");
    let port = config.int("server.port", 8000) as u16;
    join_at(&registry, service, port)
}

/// The shutdown work that says goodbye to the registry.
///
/// Pass it to `App::on_shutdown`. Without this the registrar is held, the
/// heartbeats stop when the process dies, and the registry keeps handing the
/// dead address to the gateway for the length of a lease — ninety seconds of
/// `502` on every deploy. The doc on [`join`] used to recommend calling
/// `deregister` and nothing in this workspace did; found by killing a service
/// and watching the registry still call it `up`.
pub fn farewell(
    registrar: Option<std::sync::Arc<rustlavel::Registrar>>,
) -> impl FnOnce() -> rustlavel::BoxFuture<()> + Send + Sync + 'static {
    move || {
        Box::pin(async move {
            let Some(registrar) = registrar else { return };
            match registrar.deregister().await {
                Ok(()) => info!("discovery: deregistered"),
                // Logged, never propagated: the process is on its way out, and
                // a registry that cannot be reached is a lease that lapses in
                // ninety seconds rather than a shutdown that fails.
                Err(error) => warn!("discovery: could not deregister: {error}"),
            }
        })
    }
}

/// Whether this process was started to run a command rather than to serve.
fn is_console() -> bool {
    std::env::args().nth(1).is_some_and(|argument| !argument.starts_with('-'))
}

/// The half with the decision in it, separated so it can be tested without a
/// registry, a runtime or an environment variable.
pub fn join_at(
    registry: &str,
    service: &str,
    port: u16,
) -> Option<std::sync::Arc<rustlavel::Registrar>> {
    if registry.trim().is_empty() {
        return None;
    }

    let host = rustlavel::env::env_or("SERVICE_HOST", "127.0.0.1");
    let registrar = std::sync::Arc::new(rustlavel::Registrar::new(
        vec![registry.trim().to_string()],
        rustlavel::Instance::new(service, host, port)
            .meta("zone", rustlavel::env::env_or("ZONE", "default")),
    ));

    // Registers, then heartbeats for the life of the process. Failures are
    // retried rather than returned: a registry that is briefly unreachable must
    // not end the loop that would have recovered from it.
    std::sync::Arc::clone(&registrar).start();
    Some(registrar)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_caller_holds_only_the_scopes_it_was_given() {
        let caller = Caller {
            subject: Some("42".into()),
            client: Some("checkout".into()),
            scopes: vec!["orders.read".into()],
        };
        assert!(caller.can("orders.read"));
        assert!(!caller.can("orders.write"));
        assert!(!caller.can(""));
        assert!(!caller.is_service());
        assert_eq!(caller.owner(), "42");
    }

    /// A service's calls must not be recorded against a person, and a service
    /// still needs an owner to record them against.
    #[test]
    fn a_service_is_owned_by_its_client_rather_than_by_nobody() {
        let service = Caller {
            subject: None,
            client: Some("reconciliation".into()),
            scopes: vec!["orders.read".into()],
        };

        assert!(service.is_service());
        assert_eq!(service.owner(), "reconciliation");
    }

    #[test]
    fn the_error_body_is_one_shape() {
        assert_eq!(problem("no").to_string(), r#"{"message":"no"}"#);
    }

    /// No registry configured is not an error and not a panic — it is the
    /// ordinary case on a platform that tracks instances itself. Checked
    /// without touching the environment, because tests here run concurrently
    /// in one process.
    /// A command runs for seconds and exits. A registration it left behind
    /// would be handed to the gateway for the length of a lease.
    #[test]
    fn a_console_command_is_not_a_running_instance() {
        // Argv is process-wide and these tests run concurrently, so the shape
        // is checked rather than the process: `msvc_auth migrate` is a command,
        // `msvc_auth` and `msvc_auth --verbose` are not.
        assert!(!is_console_argument(None));
        assert!(!is_console_argument(Some("--verbose")));
        assert!(is_console_argument(Some("migrate")));
        assert!(is_console_argument(Some("db:seed")));
    }

    fn is_console_argument(first: Option<&str>) -> bool {
        first.is_some_and(|argument| !argument.starts_with('-'))
    }

    #[test]
    fn joining_without_a_registry_does_nothing() {
        assert!(join_at("", "api", 9002).is_none());
        assert!(join_at("   ", "api", 9002).is_none(), "a blank setting is not a registry");
    }
}
