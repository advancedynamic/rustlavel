//! The other side of the registry: registering yourself, and finding everyone
//! else.
//!
//! Two types, because they are two jobs and most services do only one of them.
//! A worker registers and never looks anybody up; a gateway looks everybody up
//! and registers nothing.
//!
//! **The registry being down must not stop traffic.** [`Discovery`] keeps what
//! it last read and goes on serving it when the registry cannot be reached — for
//! however long that lasts. A cache with an expiry would turn a registry outage
//! into a total outage on a timer, which is the failure everybody who has run
//! one of these remembers. What an unreachable registry costs is *changes*: a
//! new instance is not noticed and a departed one is still offered, and the
//! caller's own retries are what cover the second.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use rustlavel_client::{Client as Http, ClientResponse};
use rustlavel_core::{Error, Json, Result};
use rustlavel_http::{Method, Status};

use crate::eureka;
use crate::instance::{Instance, now};
use crate::resolve::Resolve;

/// How long a lookup is reused before asking the registry again.
///
/// Thirty seconds is Eureka's own default and is the right order of magnitude:
/// this is a cache in front of a list that changes when somebody deploys, not a
/// health check.
pub const REFRESH: u64 = 30;

/// Try each registry in turn.
///
/// In order rather than in parallel, and every one of them before giving up:
/// registries are replicas of each other, so the first that answers has the
/// answer, and asking three at once to save a few milliseconds would triple the
/// load on all three for the whole life of the system.
async fn ask(
    http: &Http,
    servers: &[String],
    method: Method,
    path: &str,
    body: Option<Json>,
) -> Result<ClientResponse> {
    let mut last: Option<Error> = None;

    for server in servers {
        let url = format!("{}{path}", server.trim_end_matches('/'));
        let mut request = http.request(method, url);
        if let Some(body) = body.clone() {
            request = request.json(body);
        }

        match request.send().await {
            // A 5xx from a registry is a registry that is having a bad time —
            // the next replica may be fine, so keep going. A 4xx is an answer:
            // every replica will say the same thing and asking again is noise.
            Ok(response) if response.status.0 >= 500 => {
                last = Some(Error::msg(format!("{server} answered HTTP {}", response.status)));
            }
            Ok(response) => return Ok(response),
            Err(error) => last = Some(error),
        }
    }

    Err(last.unwrap_or_else(|| Error::msg("no discovery server is configured")))
}

fn app_path(service: &str) -> String {
    format!("/eureka/apps/{}", service.to_ascii_uppercase())
}

/// Registers this instance and keeps the registration alive.
pub struct Registrar {
    servers: Vec<String>,
    instance: Instance,
    http: Http,
}

impl Registrar {
    pub fn new(servers: Vec<String>, instance: Instance) -> Registrar {
        Registrar {
            servers,
            instance,
            // Short, because every one of these calls is on a heartbeat: a
            // registry that has stopped answering must be given up on well
            // inside the interval, or the heartbeats queue behind each other.
            http: Http::new().timeout(Duration::from_secs(5)),
        }
    }

    pub fn http(mut self, http: Http) -> Registrar {
        self.http = http;
        self
    }

    pub fn instance(&self) -> &Instance {
        &self.instance
    }

    /// Announce this instance.
    pub async fn register(&self) -> Result<()> {
        let body = Json::object([("instance", eureka::instance_json(&self.instance))]);
        ask(
            &self.http,
            &self.servers,
            Method::Post,
            &app_path(&self.instance.service),
            Some(body),
        )
        .await?
        .error_for_status()?;
        Ok(())
    }

    /// Say you are still here.
    ///
    /// `false` means the registry does not know this instance and it must
    /// register again — which is what it answers to every client after it
    /// restarts, and the reason a restarted registry refills itself without
    /// anybody restarting anything.
    pub async fn heartbeat(&self) -> Result<bool> {
        let path = format!("{}/{}", app_path(&self.instance.service), self.instance.id);
        let response = ask(&self.http, &self.servers, Method::Put, &path, None).await?;

        if response.status == Status::NOT_FOUND {
            return Ok(false);
        }
        response.error_for_status()?;
        Ok(true)
    }

    /// Say you are going away.
    ///
    /// The difference between a clean deploy and thirty seconds of failures.
    /// Call it on shutdown.
    pub async fn deregister(&self) -> Result<()> {
        let path = format!("{}/{}", app_path(&self.instance.service), self.instance.id);
        ask(&self.http, &self.servers, Method::Delete, &path, None).await?.error_for_status()?;
        Ok(())
    }

    /// Register, then heartbeat forever in the background.
    ///
    /// Failures are logged and retried rather than returned: this runs for the
    /// life of the process, and a registry that is briefly unreachable must not
    /// end the loop that would have recovered from it.
    pub fn start(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let interval = Duration::from_secs(self.instance.renew_interval);

            if let Err(error) = self.register().await {
                rustlavel_core::warn!("discovery: could not register: {error}");
            }

            loop {
                tokio::time::sleep(interval).await;

                match self.heartbeat().await {
                    Ok(true) => {}
                    // The registry has forgotten us — it restarted, or evicted
                    // us during a partition. Registering again is the whole
                    // recovery.
                    Ok(false) => {
                        if let Err(error) = self.register().await {
                            rustlavel_core::warn!("discovery: could not re-register: {error}");
                        }
                    }
                    Err(error) => rustlavel_core::warn!("discovery: heartbeat failed: {error}"),
                }
            }
        })
    }
}

#[derive(Clone)]
struct Cached {
    instances: Vec<Instance>,
    at: u64,
}

/// Looks services up, and keeps what it found.
///
/// Implements [`Resolve`], so it drops in wherever a [`Static`](crate::resolve::Static)
/// or [`Dns`](crate::resolve::Dns) resolver would.
pub struct Discovery {
    servers: Vec<String>,
    http: Http,
    refresh: u64,
    cache: Arc<RwLock<HashMap<String, Cached>>>,
}

impl Discovery {
    pub fn new(servers: Vec<String>) -> Discovery {
        Discovery {
            servers,
            http: Http::new().timeout(Duration::from_secs(5)),
            refresh: REFRESH,
            cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Use a client of your own — with a circuit breaker, or a fake.
    ///
    /// Keeps whatever is already cached, so swapping the client does not throw
    /// away the list the application is currently running on.
    pub fn http(mut self, http: Http) -> Discovery {
        self.http = http;
        self
    }

    pub fn refresh_every(mut self, seconds: u64) -> Discovery {
        self.refresh = seconds;
        self
    }

    /// The last answer, and whether it is still worth reusing.
    fn cached(&self, service: &str) -> Option<Cached> {
        self.cache.read().expect("the discovery cache lock is poisoned").get(service).cloned()
    }

    async fn fetch(&self, service: &str) -> Result<Vec<Instance>> {
        let response =
            ask(&self.http, &self.servers, Method::Get, &app_path(service), None).await?;

        // A service nobody has registered is an empty list, not an error: the
        // caller wants to answer 503 for a service with nothing running, and a
        // failure here would be indistinguishable from the registry being down.
        if response.status == Status::NOT_FOUND {
            return Ok(Vec::new());
        }

        let body = response.error_for_status()?.json()?;
        let Some(Json::Array(entries)) = body.get("application.instance") else {
            return Ok(Vec::new());
        };

        Ok(entries
            .iter()
            .filter_map(eureka::read_instance)
            // Status only — never freshness. `last_seen` was stamped by the
            // registry's clock, and comparing it to this machine's would evict
            // a healthy service over a few seconds of skew.
            .filter(|instance| instance.status.takes_traffic())
            .collect())
    }

    /// The instances of a service, from the cache or from the registry.
    pub async fn lookup(&self, service: &str) -> Result<Vec<Instance>> {
        let service = service.to_ascii_uppercase();
        let held = self.cached(&service);

        if let Some(held) = &held
            && now().saturating_sub(held.at) < self.refresh
        {
            return Ok(held.instances.clone());
        }

        match self.fetch(&service).await {
            Ok(instances) => {
                self.cache
                    .write()
                    .expect("the discovery cache lock is poisoned")
                    .insert(service, Cached { instances: instances.clone(), at: now() });
                Ok(instances)
            }
            // The registry is unreachable. Keep serving what we last knew, for
            // as long as that lasts — a stale list still routes, and an empty
            // one stops the application dead.
            Err(error) => match held {
                Some(held) => {
                    rustlavel_core::warn!(
                        "discovery: {service} served from a stale cache: {error}"
                    );
                    Ok(held.instances)
                }
                None => Err(error),
            },
        }
    }
}

impl Resolve for Discovery {
    fn instances<'a>(
        &'a self,
        service: &'a str,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<Vec<Instance>>> + Send + 'a>> {
        Box::pin(self.lookup(service))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustlavel_client::{Fake, FakeResponse};

    fn registry_with(instances: Vec<Instance>) -> Http {
        let body = eureka::application_json("ORDERS", &instances);
        Http::new().faking(Fake::new().on("/eureka/apps/ORDERS", FakeResponse::json(body)))
    }

    fn unreachable() -> Http {
        // Nothing scripted and no fallback: every request is an error, which is
        // what a registry that is down looks like from here.
        Http::new().faking(Fake::new())
    }

    #[tokio::test]
    async fn a_lookup_reads_the_registry() {
        let discovery = Discovery::new(vec!["http://registry:8761".into()])
            .http(registry_with(vec![Instance::new("orders", "10.0.0.1", 8080)]));

        let found = discovery.lookup("orders").await.unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].url(), "http://10.0.0.1:8080");
    }

    /// A service with nothing running is an empty list, not an error — the
    /// caller answers 503, and cannot do that if it cannot tell this apart from
    /// the registry being down.
    #[tokio::test]
    async fn a_service_nobody_registered_is_empty_rather_than_an_error() {
        let http = Http::new()
            .faking(Fake::new().fallback(FakeResponse::text("not found").status(404)));
        let discovery = Discovery::new(vec!["http://registry:8761".into()]).http(http);

        assert_eq!(discovery.lookup("nothing").await.unwrap().len(), 0);
    }

    /// The whole point of the cache: a registry outage costs you *changes*, not
    /// traffic.
    #[tokio::test]
    async fn the_registry_going_down_does_not_stop_the_lookups() {
        let mut discovery = Discovery::new(vec!["http://registry:8761".into()])
            .refresh_every(0)
            .http(registry_with(vec![Instance::new("orders", "10.0.0.1", 8080)]));

        assert_eq!(discovery.lookup("orders").await.unwrap().len(), 1);

        discovery = discovery.http(unreachable());
        let found = discovery.lookup("orders").await.expect("a stale answer, not a failure");
        assert_eq!(found.len(), 1, "the cached list was thrown away when the registry went down");
    }

    /// And when there is nothing cached, an unreachable registry is an error
    /// rather than an empty list: "I could not find out" and "there is nothing
    /// there" lead to different pages.
    #[tokio::test]
    async fn an_unreachable_registry_with_nothing_cached_is_an_error() {
        let discovery = Discovery::new(vec!["http://registry:8761".into()]).http(unreachable());
        assert!(discovery.lookup("orders").await.is_err());
    }

    #[tokio::test]
    async fn a_fresh_answer_is_reused_rather_than_asked_for_again() {
        let http = registry_with(vec![Instance::new("orders", "10.0.0.1", 8080)]);
        let fake = Arc::clone(http.fake().expect("a faking client"));
        let discovery = Discovery::new(vec!["http://registry:8761".into()]).http(http);

        discovery.lookup("orders").await.unwrap();
        discovery.lookup("orders").await.unwrap();
        fake.assert_count(1);
    }

    /// An instance the registry is still holding but has taken out of rotation
    /// must not be given traffic by a client that read the raw document.
    #[tokio::test]
    async fn an_instance_that_is_not_up_is_not_returned() {
        use crate::instance::Status as InstanceStatus;

        let http = registry_with(vec![
            Instance::new("orders", "10.0.0.1", 8080),
            Instance::new("orders", "10.0.0.2", 8080).status(InstanceStatus::OutOfService),
        ]);
        let discovery = Discovery::new(vec!["http://registry:8761".into()]).http(http);

        let found = discovery.lookup("orders").await.unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].host, "10.0.0.1");
    }

    /// Registries are replicas. The first one that answers has the answer, and
    /// one that is down must not be the end of it.
    #[tokio::test]
    async fn a_dead_registry_is_stepped_over() {
        let http = Http::new().faking(
            Fake::new()
                .on("dead", FakeResponse::text("boom").status(503))
                .on("alive", FakeResponse::json(eureka::application_json("ORDERS", &[]))),
        );
        let fake = Arc::clone(http.fake().unwrap());
        let discovery =
            Discovery::new(vec!["http://dead:8761".into(), "http://alive:8761".into()]).http(http);

        assert!(discovery.lookup("orders").await.is_ok());
        fake.assert_sent("alive");
    }

    #[tokio::test]
    async fn registering_posts_the_instance_where_eureka_expects_it() {
        let http = Http::new().faking(Fake::new().fallback(FakeResponse::text("")));
        let fake = Arc::clone(http.fake().unwrap());
        let registrar =
            Registrar::new(vec!["http://registry:8761".into()], Instance::new("orders", "h", 80))
                .http(http);

        registrar.register().await.unwrap();

        let sent = fake.recorded();
        assert_eq!(sent[0].method, Method::Post);
        assert!(sent[0].url.ends_with("/eureka/apps/ORDERS"), "{}", sent[0].url);
        let body = sent[0].json().expect("a JSON body");
        assert_eq!(body.get("instance.app").and_then(Json::as_str), Some("ORDERS"));
    }

    /// A refused heartbeat is how a client learns the registry restarted. It is
    /// not a failure, and treating it as one would leave the instance
    /// unregistered until somebody noticed.
    #[tokio::test]
    async fn a_heartbeat_for_a_forgotten_instance_asks_for_a_re_registration() {
        let forgotten = Http::new()
            .faking(Fake::new().fallback(FakeResponse::text("unknown").status(404)));
        let registrar =
            Registrar::new(vec!["http://registry:8761".into()], Instance::new("orders", "h", 80))
                .http(forgotten);

        assert!(!registrar.heartbeat().await.unwrap(), "a forgotten instance was told it was fine");
    }

    #[tokio::test]
    async fn deregistering_names_the_instance() {
        let http = Http::new().faking(Fake::new().fallback(FakeResponse::text("")));
        let fake = Arc::clone(http.fake().unwrap());
        let registrar = Registrar::new(
            vec!["http://registry:8761".into()],
            Instance::new("orders", "10.0.0.1", 8080),
        )
        .http(http);

        registrar.deregister().await.unwrap();

        let sent = fake.recorded();
        assert_eq!(sent[0].method, Method::Delete);
        assert!(sent[0].url.ends_with("/eureka/apps/ORDERS/10.0.0.1:8080"), "{}", sent[0].url);
    }
}
