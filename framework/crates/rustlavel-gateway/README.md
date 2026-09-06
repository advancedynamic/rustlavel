# rustlavel-gateway

An API gateway: route a request to a service behind it or to somebody else's
API, and refuse what should not get through.

Part of [Rustlavel](https://github.com/advancedynamic/rustlavel), a full-stack
web framework for Rust written from scratch.

```toml
rustlavel = { version = "0.7.4", features = ["gateway"] }
```

```rust,ignore
App::new()?
    .plugin(
        Gateway::new()
            .route("/api/*", Upstream::at("http://127.0.0.1:9002"))
            .route("/oauth/*", Upstream::at("http://127.0.0.1:9001"))
            .route("/maps/*", Upstream::at("https://api.example.com").strip("/maps"))
            .require_token(),
    )
    .run()
    .await
```

It registers as the router's fallback, so the application's own routes are
matched first — a gateway that swallowed every path would hide the health and
metrics endpoints somebody needs when the services behind it are the problem.

Hop-by-hop headers do not cross in either direction. Forwarding `Connection` or
`Transfer-Encoding` makes the gateway and its upstream disagree about where a
message ends, which is request smuggling with an extra hop.

`require_token()` refuses a request carrying no credential before it costs a
hop. It does not replace the check inside each service: a gateway is not the
only door, and a check that happens only at the edge is missing the moment
somebody adds a second way in.

## Licence

MIT.
