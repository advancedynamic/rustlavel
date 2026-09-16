//! The registry over real HTTP.
//!
//! The unit tests reach the pieces directly; this exercises the thing a Spring
//! client actually talks to — the routes, the status codes and the wire format
//! together. Every failure found in this crate's shape so far has been at a
//! seam rather than inside a function, so the seams get their own file.

use rustlavel_client::{Client as Http, Fake, FakeResponse};
use rustlavel_core::{Config, Context, Json};
use rustlavel_discovery::replicate::{Peers, REPLICATION_HEADER};
use rustlavel_discovery::{Registry, RegistryServer};
use rustlavel_http::{Plugin, Request, Router, Setup, TestClient};

/// A running registry, and the registry it holds, so a test can read what the
/// routes did without going back through HTTP to find out.
fn registry_server(server: RegistryServer) -> (TestClient, Registry) {
    let registry = server.registry().clone();

    let mut router = Router::new();
    let config = Config::default();
    let mut context = Some(Context::builder());
    let mut setup = Setup { router: &mut router, config: &config, context: &mut context };
    Box::new(server).register(&mut setup);

    let context = context.expect("the plugin put the builder back").build();
    (TestClient::new(router).with_context(context), registry)
}

fn spring_registration(host: &str) -> Json {
    Json::parse(&format!(
        r#"{{"instance":{{
            "instanceId":"orders-{host}",
            "hostName":"{host}",
            "app":"ORDERS",
            "status":"UP",
            "port":{{"$":8080,"@enabled":"true"}},
            "securePort":{{"$":443,"@enabled":"false"}},
            "leaseInfo":{{"renewalIntervalInSecs":30}},
            "metadata":{{"zone":"jakarta-a"}}
        }}}}"#
    ))
    .expect("a valid registration")
}

#[tokio::test]
async fn a_client_registers_heartbeats_and_is_found() {
    let (client, registry) = registry_server(RegistryServer::new());

    // 204, which is what Eureka answers. Some clients read a 200-with-body as
    // a failure.
    client
        .post_json("/eureka/apps/ORDERS", spring_registration("10.0.0.1"))
        .await
        .assert_status(204);
    assert_eq!(registry.all().len(), 1);

    client.put_json("/eureka/apps/ORDERS/orders-10.0.0.1", Json::Null).await.assert_ok();

    client
        .get("/eureka/apps/ORDERS")
        .await
        .assert_ok()
        .assert_json("application.name", "ORDERS")
        .assert_json("application.instance.0.hostName", "10.0.0.1")
        .assert_json("application.instance.0.app", "ORDERS");

    client.get("/eureka/apps").await.assert_ok().assert_json("applications.versions__delta", "1");
}

/// The contract that makes a registry restart self-healing: a client whose
/// heartbeat is refused registers again, without anybody restarting it.
#[tokio::test]
async fn a_heartbeat_for_something_unknown_is_a_404() {
    let (client, _) = registry_server(RegistryServer::new());
    client.put_json("/eureka/apps/ORDERS/nobody", Json::Null).await.assert_status(404);
}

#[tokio::test]
async fn a_registration_with_no_app_is_refused_rather_than_stored() {
    let (client, registry) = registry_server(RegistryServer::new());

    client
        .post_json("/eureka/apps/ORDERS", Json::object([("instance", Json::object([] as [(&str, Json); 0]))]))
        .await
        .assert_status(400);
    assert!(registry.all().is_empty(), "a nameless registration was stored");
}

/// Draining: the instance keeps its registration and loses its traffic.
#[tokio::test]
async fn setting_a_status_takes_an_instance_out_of_rotation() {
    let (client, registry) = registry_server(RegistryServer::new());
    client.post_json("/eureka/apps/ORDERS", spring_registration("10.0.0.1")).await;

    client
        .put_json("/eureka/apps/ORDERS/orders-10.0.0.1/status?value=OUT_OF_SERVICE", Json::Null)
        .await
        .assert_ok();

    assert_eq!(registry.all().len(), 1, "a draining instance lost its registration");
    // Nothing is left that takes traffic, so the read is a 404 rather than an
    // empty application document — that is what a Eureka client expects for a
    // service with nothing up.
    client.get("/eureka/apps/ORDERS").await.assert_status(404);
}

#[tokio::test]
async fn cancelling_removes_the_instance() {
    let (client, registry) = registry_server(RegistryServer::new());
    client.post_json("/eureka/apps/ORDERS", spring_registration("10.0.0.1")).await;

    client.delete("/eureka/apps/ORDERS/orders-10.0.0.1").await.assert_ok();
    assert!(registry.all().is_empty());
}

/// A node that forwarded what it received would send it back where it came
/// from, forever.
///
/// Both directions, because only checking the marked one would let a bug that
/// forwards *nothing* pass: a registration that reaches one node and no other
/// is the quieter half of the same failure.
#[tokio::test]
async fn a_client_write_is_forwarded_and_a_replicated_one_is_not() {
    for (marked, expected) in [(false, 1), (true, 0)] {
        let http = Http::new().faking(Fake::new().fallback(FakeResponse::text("")));
        let peer = std::sync::Arc::clone(http.fake().expect("a faking client"));
        let server = RegistryServer::new()
            .peers(Peers::new(vec!["http://peer-b:8761".into()]).http(http));
        let (client, registry) = registry_server(server);

        let target = match marked {
            true => "/eureka/apps/ORDERS?isReplication=true",
            false => "/eureka/apps/ORDERS",
        };
        let mut request = Request::new(rustlavel_http::Method::Post, target)
            .with_json(spring_registration("10.0.0.1"));
        if marked {
            request.headers_mut().set(REPLICATION_HEADER, "true");
        }

        client.send(request).await.assert_status(204);
        assert_eq!(registry.all().len(), 1, "marked={marked}: the registration was dropped");

        // Replication is spawned so it does not sit in front of the client's
        // response, so give the task a bounded chance to run rather than
        // sleeping a fixed time and hoping.
        for _ in 0..50 {
            if peer.count() >= expected.max(1) {
                break;
            }
            tokio::task::yield_now().await;
        }

        assert_eq!(
            peer.count(),
            expected,
            "marked={marked}: the peer saw {} forwarded writes",
            peer.count()
        );
    }
}

#[tokio::test]
async fn the_dashboard_renders_and_its_document_is_readable() {
    let (client, _) = registry_server(RegistryServer::new());
    client.post_json("/eureka/apps/ORDERS", spring_registration("10.0.0.1")).await;

    client
        .get("/discovery")
        .await
        .assert_ok()
        .assert_see("ORDERS")
        .assert_see("http://10.0.0.1:8080")
        .assert_see("jakarta-a");

    client
        .get("/discovery/registry.json")
        .await
        .assert_ok()
        .assert_json("counts.services", 1.0)
        .assert_json("counts.up", 1.0)
        .assert_json("mode", "EVICTING")
        .assert_json("services.0.name", "ORDERS");
}

/// It lists every host and port in the estate. Taking it away has to work.
#[tokio::test]
async fn the_dashboard_can_be_switched_off() {
    let (client, _) = registry_server(RegistryServer::new().dashboard(None));
    client.get("/discovery").await.assert_not_found();

    // The document stays: it is what the auth-kit page reads, and that page has
    // its own authentication in front of it.
    client.get("/discovery/registry.json").await.assert_ok();
}
