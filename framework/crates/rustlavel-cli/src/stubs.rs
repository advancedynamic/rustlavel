//! Templates the generators write out.
//!
//! Placeholders are `{{name}}`-style and filled by [`render`].

use std::collections::BTreeMap;

/// Replace every `{{key}}` in a stub.
pub fn render(template: &str, values: &BTreeMap<&str, String>) -> String {
    let mut out = template.to_string();
    for (key, value) in values {
        out = out.replace(&format!("{{{{{key}}}}}"), value);
    }
    out
}

pub const CARGO_TOML: &str = r#"[package]
name = "{{name}}"
version = "0.1.0"
edition = "2024"

[lib]
name = "{{name}}"
path = "src/lib.rs"

[[bin]]
name = "{{name}}"
path = "src/main.rs"

[dependencies]
rustlavel = { {{dependency}} }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
"#;

pub const MAIN_RS: &str = r#"use rustlavel::prelude::*;
use {{crate_name}}::routes;

#[tokio::main]
async fn main() -> Result<()> {
    App::new()?
        .routes(routes::web::routes)
        .run()
        .await
}
"#;

pub const ROUTES_MOD: &str = r#"pub mod web;
"#;

pub const ROUTES_WEB: &str = r#"//! Application routes.
//!
//! Registered once from `main.rs`, which is the compile-time equivalent of
//! Laravel loading `routes/web.php`.

use rustlavel::prelude::*;

use crate::controllers::welcome_controller::WelcomeController;

pub fn routes(r: &mut Router) {
    r.get("/", WelcomeController::index).name("home");

    // r.group("/admin", |r| {
    //     r.get("/dashboard", DashboardController::index);
    // });
}
"#;

pub const CONTROLLERS_MOD: &str = r#"pub mod welcome_controller;
"#;

pub const WELCOME_CONTROLLER: &str = r##"use rustlavel::prelude::*;

pub struct WelcomeController;

impl WelcomeController {
    pub async fn index(req: Request) -> Response {
        let name = req.config().string("app.name", "Rustlavel");
        Response::html(page(&name))
    }
}

pub fn page(name: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{name}</title>
<style>
  :root {{ color-scheme: light dark; }}
  body {{ margin:0; min-height:100vh; display:grid; place-content:center; text-align:center;
         font:16px/1.6 ui-sans-serif,-apple-system,'Segoe UI',sans-serif;
         background:#faf9f7; color:#1c1b1a; }}
  @media (prefers-color-scheme: dark) {{ body {{ background:#181716; color:#eceae7; }} }}
  h1 {{ font-size:32px; margin:0 0 8px; font-weight:650; }}
  p {{ margin:0; color:#6b6864; }}
  code {{ font-family:ui-monospace,SFMono-Regular,Menlo,monospace; font-size:14px; }}
</style>
</head>
<body>
  <main>
    <h1>{name}</h1>
    <p>Your application is running.</p>
    <p><code>src/routes/web.rs</code></p>
  </main>
</body>
</html>"#
    )
}
"##;

pub const CONTROLLER_STUB: &str = r#"use rustlavel::prelude::*;

pub struct {{class}};

impl {{class}} {
    pub async fn index(_req: Request) -> Response {
        Response::json(Json::object([("message", "{{class}}::index".into())]))
    }

    pub async fn show(req: Request) -> Response {
        match req.param("id") {
            Some(id) => Response::json(Json::object([("id", id.into())])),
            None => Response::not_found(),
        }
    }
}
"#;

pub const MIDDLEWARE_STUB: &str = r#"use rustlavel::prelude::*;

/// Runs before and after the handler. Not calling `next.run(...)` stops the
/// request here — which is how a guard redirects instead of continuing.
pub async fn {{function}}(req: Request, next: Next) -> Response {
    next.run(req).await
}
"#;

pub const TEST_STUB: &str = r#"use rustlavel::test_prelude::*;

fn app() -> App {
    App::bare().routes({{crate_name}}::routes::web::routes)
}

#[tokio::test]
async fn the_home_page_renders() {
    app().test_client().get("/").await.assert_ok();
}
"#;

pub const ENV: &str = r#"APP_NAME="{{app_name}}"
APP_ENV=local
APP_DEBUG=true
APP_URL=http://localhost:8000

SERVER_HOST=127.0.0.1
SERVER_PORT=8000

LOG_LEVEL=debug
"#;

pub const CONFIG_APP: &str = r#"{
  "name": "${APP_NAME:{{app_name}}}",
  "env": "${APP_ENV:local}",
  "url": "${APP_URL:http://localhost:8000}",
  "timezone": "UTC",
  "locale": "en"
}
"#;

pub const GITIGNORE: &str = r#"target/
.env
.DS_Store
"#;

pub const README: &str = r#"# {{app_name}}

Built with [Rustlavel](https://github.com/advancedynamic/rustlavel).

## Getting started

```bash
rustlavel serve     # http://localhost:8000, reloads on change
```

## Layout

| Path                  | What lives there                       |
| --------------------- | -------------------------------------- |
| `src/routes/web.rs`   | Routes                                 |
| `src/controllers/`    | Controllers                            |
| `config/`             | Configuration, `${VAR}` reads `.env`   |
| `public/`             | Static files                           |
| `tests/`              | Application tests                      |

## Commands

```bash
rustlavel serve
rustlavel route:list
rustlavel make:controller PostController
rustlavel make:middleware ensure_admin
```
"#;

pub const PUBLIC_KEEP: &str = "Static files placed here are served automatically.\n";

pub const MIGRATION_STUB: &str = r#"//! {{description}}

use rustlavel_db::migration;
use rustlavel_db::prelude::*;

migration!(
    {{class}},
    "{{name}}",
    up: |schema| {
        schema
            .create("{{table}}", |t| {
                t.id();
                // t.string("name");
                t.timestamps();
            })
            .await
    },
    down: |schema| { schema.drop("{{table}}").await },
);
"#;

pub const MODEL_STUB: &str = r#"use rustlavel_db::prelude::*;

/// The `{{table}}` table.
#[derive(Model, Default, Debug, Clone)]
#[model(table = "{{table}}")]
pub struct {{class}} {
    #[model(primary_key, generated)]
    pub id: i64,
    // pub name: String,
}
"#;

pub const SEEDER_STUB: &str = r#"use rustlavel_db::prelude::*;

pub struct {{class}};

impl Seeder for {{class}} {
    fn name(&self) -> &'static str {
        "{{class}}"
    }

    fn run<'a>(
        &'a self,
        db: &'a Database,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            let mut faker = Faker::new(1);

            for _ in 0..10 {
                db.table("{{table}}")
                    .insert_without_id(db, &[("name", faker.name().into())])
                    .await?;
            }

            Ok(())
        })
    }
}
"#;

/// The generated registry. Written by the CLI, never edited by hand — this is
/// how a compiled language gets Laravel's "drop a file in and it runs".
pub const MIGRATIONS_REGISTRY: &str = r#"//! Generated by `rustlavel make:migration`. Do not edit.
//!
//! A compiled program cannot discover migrations by scanning a directory, so
//! the CLI keeps this list in step with the files beside it.

{{modules}}
use rustlavel_db::Migration;

/// Every migration, in the order they must run.
pub fn all() -> Vec<&'static dyn Migration> {
    vec![
{{entries}}
    ]
}
"#;

pub const SEEDERS_REGISTRY: &str = r#"//! Generated by `rustlavel make:seeder`. Do not edit.

{{modules}}
use rustlavel_db::Seeder;

/// Every seeder, in the order they should run.
pub fn all() -> Vec<&'static dyn Seeder> {
    vec![
{{entries}}
    ]
}
"#;
