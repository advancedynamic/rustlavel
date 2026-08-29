//! End-to-end tests for an application built on the framework.

use rustlavel::test_prelude::*;
use rustlavel::error_page;

fn app() -> App {
    App::bare().routes(|r| {
        r.get("/", |_req: Request| async { "<h1>Home</h1>" });
        r.get("/users/{id}", |req: Request| async move {
            match req.param_as::<u32>("id") {
                Some(id) => Response::json(Json::object([("id", id.into())])),
                None => Response::not_found(),
            }
        });
        r.post("/users", |mut req: Request| async move {
            match req.input("name") {
                Some(name) if !name.is_empty() => {
                    (201, Json::object([("name", Json::from(name))])).into_response()
                }
                _ => Response::new(Status::UNPROCESSABLE)
                    .with_json(Json::object([("error", "name is required".into())])),
            }
        });
        r.get("/boom", |_req: Request| async {
            let missing: Option<u8> = None;
            #[allow(clippy::unnecessary_literal_unwrap)]
            missing.expect("this handler panics on purpose");
            Response::ok()
        });

        r.group("/admin", |r| {
            r.middleware(|req: Request, next: Next| async move {
                if req.header("x-token") == Some("secret") {
                    next.run(req).await
                } else {
                    Response::new(Status::UNAUTHORIZED).with_text("nope")
                }
            });
            r.get("/dashboard", |_req: Request| async { "admin area" });
        });
    })
}

#[tokio::test]
async fn serves_html_json_and_route_parameters() {
    let client = app().test_client();

    client.get("/").await.assert_ok().assert_see("Home");
    client.get("/users/42").await.assert_ok().assert_json("id", 42);
    client.get("/users/abc").await.assert_not_found();
}

#[tokio::test]
async fn validates_a_posted_form() {
    let client = app().test_client();

    client.post("/users", &[("name", "Ada")]).await.assert_status(201).assert_json("name", "Ada");
    client.post("/users", &[]).await.assert_status(422).assert_json("error", "name is required");
}

#[tokio::test]
async fn group_middleware_guards_its_routes() {
    let client = app().test_client();

    client.get("/admin/dashboard").await.assert_status(401);
    client
        .send(Request::new(Method::Get, "/admin/dashboard").with_header("x-token", "secret"))
        .await
        .assert_ok()
        .assert_see("admin area");
}

#[tokio::test]
async fn a_panicking_handler_becomes_an_error_page_rather_than_a_crash() {
    error_page::set_debug(true);
    let client = app().test_client();

    let response = client.get("/boom").await.assert_status(500);
    assert!(response.body().contains("this handler panics on purpose"));

    // The rest of the application keeps serving.
    client.get("/").await.assert_ok();
    error_page::set_debug(false);
}

#[tokio::test]
async fn unknown_paths_and_methods_are_reported_distinctly() {
    let client = app().test_client();

    client.get("/nowhere").await.assert_not_found();
    client.delete("/users").await.assert_status(405).assert_header("allow", "POST");
}

/// Validation reaching a handler through `?`, which is what the generalised
/// `IntoResponse for Result` in the HTTP crate exists to allow.
#[cfg(feature = "validation")]
mod validation {
    use super::*;
    use rustlavel::validation::validate;

    fn app() -> App {
        App::bare().routes(|r| {
            r.post("/register", |mut req: Request| async move {
                let data = validate(
                    &mut req,
                    &[("email", "required|email"), ("age", "required|integer|min:18")],
                )
                .await?;

                Ok::<_, rustlavel::validation::Errors>(
                    (201, Json::object([("email", Json::from(data.string("email").unwrap_or_default()))]))
                        .into_response(),
                )
            });
        })
    }

    #[tokio::test]
    async fn valid_input_reaches_the_handler() {
        app()
            .test_client()
            .post_json(
                "/register",
                Json::object([("email", "ada@example.com".into()), ("age", 36.into())]),
            )
            .await
            .assert_status(201)
            .assert_json("email", "ada@example.com");
    }

    #[tokio::test]
    async fn invalid_input_becomes_a_422_before_the_handler_body_runs() {
        let response = app()
            .test_client()
            .post_json("/register", Json::object([("email", "not-an-email".into()), ("age", 12.into())]))
            .await
            .assert_status(422);

        let body = response.json();
        assert!(body.get("errors.email").is_some(), "email should be rejected: {}", response.body());
        assert!(body.get("errors.age").is_some(), "age should be rejected: {}", response.body());
    }
}

/// Telescope is the framework's showcase feature; this proves it mounts, sees
/// real traffic, and stays out of production.
#[cfg(feature = "telescope")]
mod telescope {
    use super::*;
    use rustlavel::Telescope;

    #[tokio::test]
    async fn records_requests_and_serves_a_dashboard() {
        let client = App::bare()
            .plugin(Telescope::default())
            .routes(|r| {
                r.get("/orders", |_req: Request| async { "orders" });
            })
            .test_client();

        client.get("/orders").await.assert_ok();

        // Give the event a moment to reach the recorder.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        client
            .get("/telescope")
            .await
            .assert_ok()
            .assert_header("content-type", "text/html; charset=utf-8");

        let entries = client.get("/telescope/api/entries").await.assert_ok();
        assert!(
            entries.body().contains("/orders"),
            "the recorded request should be listed: {}",
            entries.body()
        );
    }

    #[tokio::test]
    async fn refuses_to_mount_in_production() {
        let app = App::bare();
        app.config().set("app.env", "production");

        let client = app.plugin(Telescope::default()).test_client();

        client.get("/telescope").await.assert_not_found();
    }
}

/// API documentation generated from the routes, with nothing repeated.
#[cfg(feature = "openapi")]
mod openapi {
    use super::*;
    use rustlavel::openapi::Info;

    #[tokio::test]
    async fn documents_the_api_routes_and_serves_a_page() {
        let client = App::bare()
            .routes(|r| {
                r.get("/", |_req: Request| async { "home" });
                r.get("/api/orders/{id}", |_req: Request| async { "order" })
                    .name("orders.show")
                    .describe("Fetch one order")
                    .tag("Orders")
                    .param("id", "The order's id")
                    .responds(200, "The order")
                    .responds(404, "No such order");
            })
            .openapi(Info { title: "Orders API".into(), version: "1.0".into(), ..Info::default() })
            .test_client();

        let document = client.get("/openapi.json").await.assert_ok();

        document
            .assert_json("info.title", "Orders API")
            .assert_json("paths./api/orders/{id}.get.summary", "Fetch one order")
            .assert_json("paths./api/orders/{id}.get.operationId", "orders.show")
            .assert_json("paths./api/orders/{id}.get.responses.404.description", "No such order")
            // The browser page is not an API and stays out of the document.
            .assert_json_missing("paths./");

        client.get("/openapi").await.assert_ok().assert_see("Orders API");
    }
}

/// WebSocket over a real socket, which is the only way to exercise the server's
/// upgrade path — the test client dispatches without one.
#[cfg(feature = "ws")]
mod websocket {
    use super::*;
    use rustlavel::server::Server;
    use rustlavel::ws::{Message as WsMessage, WebSocket, websocket};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn echo_server() -> std::net::SocketAddr {
        let app = App::bare().routes(|r| {
            r.get(
                "/ws",
                websocket(|mut socket: WebSocket, _req: Request| async move {
                    while let Some(message) = socket.recv().await {
                        if let WsMessage::Text(text) = message {
                            let _ = socket.send(WsMessage::Text(format!("echo: {text}"))).await;
                        }
                    }
                }),
            );
        });

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);

        let (router, context) = app.take_parts();
        tokio::spawn(async move {
            let _ = Server::new(router, context).listen(address.to_string()).await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        address
    }

    #[tokio::test]
    async fn a_client_can_handshake_and_exchange_a_message() {
        let address = echo_server().await;
        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();

        // The key is the RFC's own example, so the expected accept value is a
        // published constant rather than something we computed ourselves.
        let request = format!(
            "GET /ws HTTP/1.1\r\nHost: {address}\r\nUpgrade: websocket\r\n\
             Connection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
             Sec-WebSocket-Version: 13\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).await.unwrap();

        let mut buffer = [0u8; 1024];
        let read = stream.read(&mut buffer).await.unwrap();
        let response = String::from_utf8_lossy(&buffer[..read]).to_string();

        assert!(response.starts_with("HTTP/1.1 101 Switching Protocols"), "{response}");
        assert!(response.contains("s3pPLMBiTxaQ9kYGzzhZRbK+xOo="), "{response}");

        // A masked text frame saying "hi".
        let frame = [0x81, 0x82, 0x00, 0x00, 0x00, 0x00, b'h', b'i'];
        stream.write_all(&frame).await.unwrap();

        let read = stream.read(&mut buffer).await.unwrap();
        let echoed = String::from_utf8_lossy(&buffer[..read]).to_string();
        assert!(echoed.contains("echo: hi"), "{echoed:?}");
    }
}
