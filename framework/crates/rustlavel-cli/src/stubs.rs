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
"#;

pub const MAIN_RS: &str = r#"use rustlavel::prelude::*;
use {{crate_name}}::routes;

#[rustlavel::main]
async fn main() -> Result<()> {
    App::new()?
        .routes(routes::web::routes)
        .run()
        .await
}
"#;

/// The entry point for an application with the database package enabled.
///
/// The migration and seeder lists are generated files; registering them here is
/// what lets `rustlavel migrate` and `rustlavel db:seed` work, since only the
/// application can name its own migration types.
pub const MAIN_RS_DB: &str = r#"use rustlavel::prelude::*;
use {{crate_name}}::{database, routes};

#[rustlavel::main]
async fn main() -> Result<()> {
    App::new()?
        .routes(routes::web::routes)
        .migrations(database::migrations::all())
        .seeders(database::seeders::all())
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

#[rustlavel::test]
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

use rustlavel::db::migration;

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

pub const MODEL_STUB: &str = r#"use rustlavel::prelude::*;

/// The `{{table}}` table.
#[derive(Model, Default, Debug, Clone)]
#[model(table = "{{table}}")]
pub struct {{class}} {
    #[model(primary_key, generated)]
    pub id: i64,
    // pub name: String,
}
"#;

pub const SEEDER_STUB: &str = r#"use rustlavel::prelude::*;

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
use rustlavel::db::Migration;

/// Every migration, in the order they must run.
pub fn all() -> Vec<&'static dyn Migration> {
    vec![
{{entries}}
    ]
}
"#;

pub const SEEDERS_REGISTRY: &str = r#"//! Generated by `rustlavel make:seeder`. Do not edit.

{{modules}}
use rustlavel::db::Seeder;

/// Every seeder, in the order they should run.
pub fn all() -> Vec<&'static dyn Seeder> {
    vec![
{{entries}}
    ]
}
"#;

pub const MCP_TOOL_STUB: &str = r#"use rustlavel::mcp::{Schema, Tool};
use rustlavel::prelude::*;

/// Exposed to agents over MCP as `{{tool}}`.
///
/// The schema is declared once and does double duty: it becomes the tool's
/// advertised `inputSchema`, and arguments are validated against it before this
/// handler ever runs.
pub fn {{function}}() -> Tool {
    Tool::new(
        "{{tool}}",
        "Describe what this tool does, in the words an agent will read.",
        Schema::object().string("query", "What to look for"),
        |arguments: Json| async move {
            let query = arguments.get("query").and_then(Json::as_str).unwrap_or_default();

            Ok(Json::object([("result", Json::from(format!("saw {query}")))]))
        },
    )
}
"#;

/// Written into every new application so a coding agent starts with the
/// conventions instead of guessing them.
pub const AGENT_NOTES: &str = r#"# {{app_name}}

A [Rustlavel](https://github.com/advancedynamic/rustlavel) application.

## Layout

| Path                  | What lives there                     |
| --------------------- | ------------------------------------ |
| `src/routes/web.rs`   | Routes, registered from `main.rs`    |
| `src/controllers/`    | Controllers                          |
| `src/middleware/`     | Middleware functions                 |
| `config/`             | Configuration; `${VAR}` reads `.env` |
| `public/`             | Static files, served automatically   |
| `tests/`              | Application tests                    |

## Conventions

- A handler is `async fn(Request) -> impl IntoResponse`. Returning `Result` works:
  each error type decides its own response, so `?` is normal.
- Reach services with `req.state::<T>()`, not a global. Register them with
  `.state(...)` on the `App`.
- Optional packages are enabled by a feature on the `rustlavel` dependency and
  one explicit line in `main.rs` (`.plugin(...)`). There is no auto-discovery.
- Tests use the test client, which dispatches without a socket:
  `client.get("/x").await.assert_ok()`. It keeps cookies between requests.

## Commands

```bash
rustlavel serve          # run with reload
rustlavel route:list     # what routes exist
rustlavel doctor         # why won't it start
rustlavel make:controller PostController
cargo test               # the whole suite
```

## Before you finish

Run `cargo test` and `cargo clippy --all-targets`. Both should be clean.
"#;

pub const JOB_STUB: &str = r#"use rustlavel::prelude::*;

/// Runs in the background. Register it so a worker can find it by name:
///
/// ```ignore
/// let mut jobs = JobRegistry::new();
/// jobs.register::<{{class}}>();
/// ```
#[derive(Debug, Clone)]
pub struct {{class}} {
    pub id: i64,
}

impl Job for {{class}} {
    const NAME: &'static str = "{{name}}";

    fn payload(&self) -> Json {
        Json::object([("id", Json::from(self.id))])
    }

    fn from_payload(payload: &Json) -> Result<Self> {
        Ok({{class}} {
            id: payload
                .get("id")
                .and_then(Json::as_i64)
                .ok_or_else(|| Error::msg("{{name}} needs an `id` in its payload"))?,
        })
    }

    async fn handle(&self) -> Result<()> {
        info!("running {{name}} for {}", self.id);
        Ok(())
    }
}
"#;

pub const MAIL_STUB: &str = r#"use rustlavel::mail::Mailable;
use rustlavel::prelude::*;

/// Rendered from `resources/views/mail/{{view}}.rl.html`.
pub struct {{class}} {
    pub name: String,
}

impl Mailable for {{class}} {
    fn subject(&self) -> String {
        "{{title}}".to_string()
    }

    fn view(&self) -> &str {
        "mail/{{view}}"
    }

    fn context(&self) -> ViewContext {
        ViewContext::new().with("name", self.name.clone())
    }
}
"#;

pub const NOTIFICATION_STUB: &str = r#"use rustlavel::mail::{Message, Notification, Recipient, channel};
use rustlavel::prelude::*;

/// Delivered over every channel it names, from this one definition.
pub struct {{class}} {
    pub subject: String,
}

impl Notification for {{class}} {
    fn name(&self) -> &str {
        "{{name}}"
    }

    fn channels(&self) -> Vec<&'static str> {
        vec![channel::MAIL, channel::DATABASE]
    }

    /// The recipient is addressed by the notifier; this only writes the body.
    fn to_mail(&self, _recipient: &Recipient) -> Result<Message> {
        Ok(Message::new()
            .subject(&self.subject)
            .text("Write the plain-text body here.")
            .html("<p>Write the HTML body here.</p>"))
    }

    fn to_database(&self, _recipient: &Recipient) -> Result<Json> {
        Ok(Json::object([("subject", Json::from(self.subject.as_str()))]))
    }
}
"#;

// ---------------------------------------------------------------------------
// `make:crud` — the model, migration, controller and views for one resource.
// ---------------------------------------------------------------------------

pub const CRUD_MODEL_STUB: &str = r#"use rustlavel::prelude::*;

/// A row of the `{{table}}` table.
///
/// Generated by `rustlavel make:crud {{class}}`. The derive reads this struct
/// at compile time, so renaming a field here is a compile error everywhere it
/// is used rather than a `null` in production.
#[derive(Model, Default, Debug, Clone)]
#[model(table = "{{table}}")]
pub struct {{class}} {
    #[model(primary_key, generated)]
    pub id: i64,
{{fields}}
    /// Maintained by the database. The ORM reads them and never writes them.
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}
"#;

pub const CRUD_MIGRATION_STUB: &str = r#"//! Creates the `{{table}}` table.
//!
//! Generated by `rustlavel make:crud {{class}}`.

use rustlavel::db::migration;

migration!(
    {{migration_class}},
    "{{name}}",
    up: |schema| {
        schema
            .create("{{table}}", |t| {
                t.id();
{{columns}}
                t.timestamps();
            })
            .await
    },
    down: |schema| { schema.drop("{{table}}").await },
);
"#;

pub const CRUD_CONTROLLER_STUB: &str = r#"//! CRUD for the `{{table}}` table.
//!
//! Generated by `rustlavel make:crud {{class}}`, and yours from now on. Six
//! handlers, which is what a resource needs and no more: a list, a form, the
//! two writes behind it, and a delete.

use rustlavel::prelude::*;

use crate::models::{{module}}::{{class}};

/// What every write is checked against before a row is touched.
///
/// One list, so the form, the columns and the rules cannot drift apart. A
/// checkbox is `nullable` rather than `required` because a browser does not
/// send an unticked one at all.
const RULES: &[(&str, &str)] = &[
{{rules}}
];

pub struct {{controller}};

impl {{controller}} {
    /// `GET {{base}}` — the list.
    pub async fn index(req: Request) -> Result<Response> {
        let db = Self::db(&req)?;
        let records = {{class}}::get(&db, {{class}}::query().latest("id")).await?;

        let rows: Vec<Json> = records.iter().map({{class}}::to_json).collect();
        req.view(
            "{{view_dir}}/index",
            &ViewContext::new()
                .with("csrf_field", Json::from({{csrf_index}}))
                .with("page_title", Json::from("{{plural_label}}"))
                .with("empty", Json::from(rows.is_empty()))
                .with("records", Json::Array(rows)),
        )
    }

    /// `GET {{base}}/create` — the empty form.
    pub async fn create(req: Request) -> Result<Response> {
        let blank: Vec<(&str, String)> =
            RULES.iter().map(|(field, _)| (*field, String::new())).collect();
        req.view("{{view_dir}}/form", &Self::form(&req, &blank, &Errors::new(), None))
    }

    /// `POST {{base}}` — create one.
    pub async fn store(mut req: Request) -> Result<Response> {
        let db = Self::db(&req)?;

        // `validate` hands back the checked subset, or the messages. This form
        // is posted by a browser, so a failure gets the page again with what
        // was typed still in it; a bare 422 would be a blank screen.
        match validate(&mut req, RULES).await {
            Ok(valid) => {
                let mut record = Self::filled({{class}}::default(), &valid);
                record.insert(&db).await?;
                Ok(Response::see_other("{{base}}"))
            }
            Err(errors) => {
                let typed = Self::typed(&mut req);
                req.view("{{view_dir}}/form", &Self::form(&req, &typed, &errors, None))
            }
        }
    }

    /// `GET {{base}}/{id}/edit` — the form, filled in.
    pub async fn edit(req: Request) -> Result<Response> {
        let db = Self::db(&req)?;
        let id = req.param_as::<i64>("id").unwrap_or_default();
        let Some(record) = {{class}}::find(&db, id).await? else {
            return Ok(Response::not_found());
        };

        let values = Self::stored(&record);
        req.view("{{view_dir}}/form", &Self::form(&req, &values, &Errors::new(), Some(id)))
    }

    /// `POST {{base}}/{id}` — save the changes.
    pub async fn update(mut req: Request) -> Result<Response> {
        let db = Self::db(&req)?;
        let id = req.param_as::<i64>("id").unwrap_or_default();
        let Some(record) = {{class}}::find(&db, id).await? else {
            return Ok(Response::not_found());
        };

        match validate(&mut req, RULES).await {
            Ok(valid) => {
                Self::filled(record, &valid).update(&db).await?;
                Ok(Response::see_other("{{base}}"))
            }
            Err(errors) => {
                let typed = Self::typed(&mut req);
                req.view("{{view_dir}}/form", &Self::form(&req, &typed, &errors, Some(id)))
            }
        }
    }

    /// `POST {{base}}/{id}/delete` — remove it.
    ///
    /// A POST rather than a DELETE because an HTML form can only send GET and
    /// POST, and a link that deletes is a link something will follow.
    pub async fn destroy(req: Request) -> Result<Response> {
        let db = Self::db(&req)?;
        let id = req.param_as::<i64>("id").unwrap_or_default();
        if let Some(record) = {{class}}::find(&db, id).await? {
            record.delete(&db).await?;
        }
        Ok(Response::see_other("{{base}}"))
    }

    /// The form's values as the record holds them.
    fn stored(record: &{{class}}) -> Vec<(&'static str, String)> {
        vec![
{{stored}}
        ]
    }

    /// The form's values exactly as they were typed, so a rejected form does
    /// not empty itself while somebody reads the error.
    fn typed(req: &mut Request) -> Vec<(&'static str, String)> {
        RULES.iter().map(|(field, _)| (*field, req.input(field).unwrap_or_default())).collect()
    }

    /// Copy the checked values onto a record.
    fn filled({{filled_mut}}record: {{class}}, {{filled_valid}}: &Validated) -> {{class}} {
{{filled}}
        record
    }

    /// The variables the create and the edit form both read.
    fn form({{form_req}}: &Request, values: &[(&str, String)], errors: &Errors, id: Option<i64>) -> ViewContext {
        let mut context = ViewContext::new()
            .with("csrf_field", Json::from({{csrf_form}}))
            .with(
                "page_title",
                Json::from(if id.is_some() { "Edit {{singular_lower}}" } else { "New {{singular_lower}}" }),
            )
            .with(
                "submit_label",
                Json::from(if id.is_some() { "Save changes" } else { "Create {{singular_lower}}" }),
            )
            .with(
                "action",
                Json::from(match id {
                    Some(id) => format!("{{base}}/{id}"),
                    None => "{{base}}".to_string(),
                }),
            );

        for (field, value) in values {
            context = context
                .with(format!("value_{field}"), Json::from(value.as_str()))
                // A checkbox is sent as "1" or not sent at all.
                .with(format!("checked_{field}"), Json::from(value.as_str() == "1"))
                .with(
                    format!("error_{field}"),
                    errors.first(field).map_or(Json::Null, Json::from),
                );
        }
        context
    }

    /// The pool, or a message naming what is missing rather than a panic.
    fn db(req: &Request) -> Result<Database> {
        req.state::<Database>().cloned().ok_or_else(|| {
            Error::msg("no Database is registered — add `.state(db)` to the App in main.rs")
        })
    }
}
"#;

/// The routes for one resource, appended to `src/routes/web.rs` or printed.
pub const CRUD_ROUTES_STUB: &str = r#"
    // {{class}} — generated by `rustlavel make:crud {{class}}`.
    r.get("{{base}}", {{controller}}::index).name("{{route_name}}.index");
    r.get("{{base}}/create", {{controller}}::create).name("{{route_name}}.create");
    r.post("{{base}}", {{controller}}::store).name("{{route_name}}.store");
    r.get("{{base}}/{id}/edit", {{controller}}::edit).name("{{route_name}}.edit");
    r.post("{{base}}/{id}", {{controller}}::update).name("{{route_name}}.update");
    r.post("{{base}}/{id}/delete", {{controller}}::destroy).name("{{route_name}}.destroy");
"#;

pub const CRUD_INDEX_VIEW: &str = r#"@extends("layouts.{{layout}}")
@section("title", "{{plural_label}}")

@section("content")
  <div class="page-head">
    <h1>{{ page_title }}</h1>
    <a class="btn-primary" href="{{base}}/create">New {{singular_lower}}</a>
  </div>

  <div class="table-wrap">
    <table class="table">
      <thead>
        <tr>
          <th>#</th>
{{headers}}
          <th><span class="sr-only">Actions</span></th>
        </tr>
      </thead>
      <tbody>
        @foreach(records as record)
          <tr>
            <td class="muted">{{ record.id }}</td>
{{cells}}
            <td class="row-actions">
              <a class="btn-ghost" href="{{base}}/{{ record.id }}/edit">Edit</a>
              <form method="post" action="{{base}}/{{ record.id }}/delete">
            {{csrf}}
                <button type="submit" class="btn-danger">Delete</button>
              </form>
            </td>
          </tr>
        @endforeach
      </tbody>
    </table>
  </div>

  @if(empty)<p class="empty">No {{plural_lower}} yet.</p>@endif
@endsection
"#;

pub const CRUD_FORM_VIEW: &str = r#"@extends("layouts.{{layout}}")
@section("title", page_title)

@section("content")
  <div class="page-head">
    <h1>{{ page_title }}</h1>
    <a class="btn-ghost" href="{{base}}">Back</a>
  </div>

  <form class="form" method="post" action="{{ action }}" novalidate>
{{csrf}}{{inputs}}
    <div class="form-actions">
      <button type="submit" class="btn-primary">{{ submit_label }}</button>
      <a class="btn-ghost" href="{{base}}">Cancel</a>
    </div>
  </form>
@endsection
"#;

/// Written only when the project has no layout of its own, so a project built
/// with `--with auth-kit` keeps the one it already has.
pub const CRUD_LAYOUT_VIEW: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>@yield("title")</title>
<style>
  :root { color-scheme: light dark; --line:#e6e3df; --ink:#1c1b1a; --dim:#6b6864; --bg:#faf9f7; --card:#fff; --accent:#b45309; }
  @media (prefers-color-scheme: dark) {
    :root { --line:#302e2c; --ink:#eceae7; --dim:#9a9691; --bg:#181716; --card:#201f1d; --accent:#f59e0b; }
  }
  * { box-sizing: border-box; }
  body { margin:0; background:var(--bg); color:var(--ink);
         font:15px/1.6 ui-sans-serif,-apple-system,'Segoe UI',sans-serif; }
  main { max-width:60rem; margin:0 auto; padding:2.5rem 1.25rem 4rem; }
  h1 { font-size:1.5rem; font-weight:650; margin:0; }
  a { color:var(--accent); }
  .page-head { display:flex; align-items:center; justify-content:space-between; gap:1rem; margin-bottom:1.5rem; }
  .table-wrap { overflow-x:auto; background:var(--card); border:1px solid var(--line); border-radius:.6rem; }
  .table { width:100%; border-collapse:collapse; font-size:.9rem; }
  .table th, .table td { text-align:left; padding:.6rem .85rem; border-bottom:1px solid var(--line); vertical-align:top; }
  .table tr:last-child td { border-bottom:0; }
  .table th { font-size:.75rem; text-transform:uppercase; letter-spacing:.04em; color:var(--dim); }
  .row-actions { display:flex; gap:.4rem; justify-content:flex-end; }
  .row-actions form { margin:0; }
  .muted, .empty { color:var(--dim); }
  .empty { margin-top:1rem; font-size:.9rem; }
  .form { background:var(--card); border:1px solid var(--line); border-radius:.6rem; padding:1.25rem; }
  .field { margin-bottom:1.1rem; }
  .field-label { display:block; font-size:.8rem; font-weight:600; margin-bottom:.3rem; }
  .field-input { width:100%; padding:.5rem .65rem; border:1px solid var(--line); border-radius:.4rem;
                 background:var(--bg); color:inherit; font:inherit; }
  .field-inline { display:flex; align-items:center; gap:.5rem; font-size:.9rem; }
  .field-check { width:1rem; height:1rem; }
  .field-error { margin:.35rem 0 0; font-size:.8rem; color:#dc2626; }
  .form-actions { display:flex; align-items:center; gap:.6rem; margin-top:1.25rem; }
  .btn-primary, .btn-ghost, .btn-danger {
    display:inline-block; padding:.42rem .8rem; border-radius:.4rem; font-size:.85rem;
    font-weight:550; text-decoration:none; border:1px solid transparent; cursor:pointer; }
  .btn-primary { background:var(--accent); color:#fff; }
  .btn-ghost { border-color:var(--line); color:inherit; background:transparent; }
  .btn-danger { border-color:var(--line); background:transparent; color:#dc2626; }
  .sr-only { position:absolute; width:1px; height:1px; overflow:hidden; clip:rect(0 0 0 0); }
</style>
</head>
<body>
  <main>
    @yield("content")
  </main>
</body>
</html>
"#;

// ---------------------------------------------------------------------------
// `make:package` — a crate for somebody writing a third-party package.
// ---------------------------------------------------------------------------

pub const PACKAGE_CARGO_TOML: &str = r#"[package]
name = "{{crate_name}}"
version = "0.1.0"
edition = "2024"
# `cargo publish` refuses without a description and a licence, so both are here
# from the first commit rather than discovered on release day.
description = "{{description}}"
license = "MIT OR Apache-2.0"
repository = "https://github.com/your-name/{{crate_name}}"
documentation = "https://docs.rs/{{crate_name}}"
readme = "README.md"
keywords = ["rustlavel", "web", "plugin"]
categories = ["web-programming"]

[dependencies]
# A package depends on the pieces it uses, never on the `rustlavel` meta-crate:
# that would be a cycle the moment the meta-crate offers a feature for it.
rustlavel-core = "{{version}}"
rustlavel-http = "{{version}}"
"#;

pub const PACKAGE_LIB_RS: &str = r#"//! {{crate_name}}: {{description}}.
//!
//! A rustlavel package is a crate plus one explicit line in the application's
//! `main.rs`. There is no discovery and no reflection — an application that
//! does not name this package never compiles a line of it:
//!
//! ```ignore
//! use {{lib_name}}::{{struct_name}};
//!
//! App::new()?
//!     .plugin({{struct_name}}::from_config(&config))
//!     .run()
//!     .await
//! ```
//!
//! ## Configuration
//!
//! Everything this package reads lives under its own `{{config_prefix}}.*` key,
//! so `config/{{config_prefix}}.json` is the whole story:
//!
//! | Key                        | Meaning                            | Default        |
//! | -------------------------- | ---------------------------------- | -------------- |
//! | `{{config_prefix}}.enabled`  | Register anything at all           | `true`         |
//! | `{{config_prefix}}.path`     | Where the package mounts its route | `{{route}}`  |

use rustlavel_core::Config;
use rustlavel_http::plugin::{Plugin, Setup};
use rustlavel_http::{Request, Response};

/// {{description}}.
#[derive(Debug, Clone)]
pub struct {{struct_name}} {
    enabled: bool,
    path: String,
}

impl {{struct_name}} {
    /// The defaults, for an application that configures it in code.
    pub fn new() -> {{struct_name}} {
        {{struct_name}} { enabled: true, path: "{{route}}".to_string() }
    }

    /// Read the package's own `{{config_prefix}}.*` keys.
    ///
    /// Nothing outside that prefix is touched: a package that reaches into
    /// another one's configuration is a package that breaks when the other one
    /// is renamed.
    pub fn from_config(config: &Config) -> {{struct_name}} {
        {{struct_name}} {
            enabled: config.bool("{{config_prefix}}.enabled", true),
            path: config.string("{{config_prefix}}.path", "{{route}}"),
        }
    }

    /// Mount somewhere other than the default.
    pub fn path(mut self, path: impl Into<String>) -> {{struct_name}} {
        self.path = path.into();
        self
    }
}

impl Default for {{struct_name}} {
    fn default() -> {{struct_name}} {
        {{struct_name}}::new()
    }
}

impl Plugin for {{struct_name}} {
    /// Shown by `rustlavel route:list` and in the boot log.
    fn name(&self) -> &'static str {
        "{{config_prefix}}"
    }

    fn register(self: Box<Self>, setup: &mut Setup<'_>) {
        // Turned off in configuration means nothing is registered — not a
        // route that answers 404, which would still be a route.
        if !self.enabled {
            return;
        }

        setup.router.get(&self.path, |_req: Request| async move {
            Response::json(rustlavel_core::Json::object([(
                "package",
                rustlavel_core::Json::from("{{crate_name}}"),
            )]))
        });
    }
}

/// What an application imports to use this package.
pub mod prelude {
    pub use crate::{{struct_name}};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_only_its_own_configuration() {
        let config = Config::new();
        config.set("{{config_prefix}}.path", "/internal{{route}}");
        config.set("other.path", "/somebody-elses");

        let package = {{struct_name}}::from_config(&config);

        assert_eq!(package.path, "/internal{{route}}");
        assert!(package.enabled, "a package is on unless its own key says otherwise");
    }
}
"#;

pub const PACKAGE_README: &str = r#"# {{crate_name}}

{{description}}.

## Installing it

A rustlavel package is **a crate plus a feature flag on the meta-crate** — never
an addition to `rustlavel-core`. That is the whole convention, and it is what
keeps a build free of the packages it does not use.

An application adds the crate:

```toml
[dependencies]
{{crate_name}} = "0.1"
```

and names it once, explicitly, in `main.rs`:

```rust
use {{lib_name}}::{{struct_name}};

#[rustlavel::main]
async fn main() -> Result<()> {
    App::new()?
        .plugin({{struct_name}}::from_config(&config))
        .run()
        .await
}
```

There is no service-provider scan and no auto-discovery. If a package is not
named in `main.rs`, it is not running — and if it is not in `Cargo.toml`, it was
never compiled.

## If this package is ever folded into the framework

It becomes `rustlavel-{{config_prefix}}`, an optional dependency of the
`rustlavel` meta-crate behind a `{{config_prefix}}` feature:

```toml
[features]
{{config_prefix}} = ["dep:rustlavel-{{config_prefix}}"]
```

Nothing is added to `rustlavel-core`. Core stays the config loader, the JSON
type, the context and the event bus; everything else is opt-in.

## Configuration

`config/{{config_prefix}}.json`:

```json
{
  "enabled": true,
  "path": "{{route}}"
}
```

Every key this package reads is under `{{config_prefix}}.`. It reads nothing
else, so renaming another package cannot break this one.

## Tests

```bash
cargo test
cargo clippy --all-targets -- -D warnings
```
"#;

pub const PACKAGE_GITIGNORE: &str = r#"target/
Cargo.lock
.DS_Store
"#;

// ---------------------------------------------------------------------------
// `tinker` — the scratch crate a snippet is compiled inside.
// ---------------------------------------------------------------------------

pub const TINKER_CARGO_TOML: &str = r#"# Generated by `rustlavel tinker`. Rewritten on every run; do not edit.
[package]
name = "tinker"
version = "0.0.0"
edition = "2024"
publish = false

# Its own workspace root, so cargo does not try to adopt this crate into the
# application it is sitting inside — or into whatever workspace is above that.
[workspace]

[dependencies]
"{{package}}" = { path = "{{project}}" }
rustlavel = {{dependency}}
"#;

pub const TINKER_MAIN_RS: &str = r#"// Generated by `rustlavel tinker`. Rewritten on every snippet.
#![allow(unused_imports, unused_variables, unused_mut, dead_code, unreachable_code)]
#![allow(clippy::all, clippy::pedantic)]

use rustlavel::prelude::*;
use {{crate_name}}::*;

#[rustlavel::main]
async fn main() -> Result<()> {
    // The application's `.env`, so a snippet sees the same configuration the
    // application does. Nothing else is set up: there is no App, no router and
    // no database connection unless the snippet opens one.
    rustlavel::env::load("{{env_path}}")?;

{{snippet}}

    Ok(())
}
"#;
