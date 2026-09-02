//! End-to-end checks for the generators: run the real binary, in a real
//! directory, and look at what it wrote.
//!
//! The unit tests beside each module prove a template renders; these prove the
//! command puts the rendered files where the next command expects them — the
//! registry regenerated, the module declared, the routes wired. That is the
//! part a template test cannot see.
//!
//! Every test gets its own fixture directory named after itself, because the
//! suite runs concurrently and two `rustlavel new blog` runs in one directory
//! would be a race rather than a test.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The binary under test, built by cargo before this file runs.
const RUSTLAVEL: &str = env!("CARGO_BIN_EXE_rustlavel");

/// The framework workspace, so a scaffolded application can depend on this
/// checkout rather than on whatever is published.
fn framework() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().expect("framework root")
}

/// A directory of this test's own, emptied first so a previous run cannot make
/// this one pass.
fn fixture(name: &str) -> PathBuf {
    let directory = std::env::temp_dir().join(format!("rustlavel-cli-generators-{name}"));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("fixture directory");
    directory
}

fn run(directory: &Path, args: &[&str]) -> Output {
    Command::new(RUSTLAVEL)
        .args(args)
        .current_dir(directory)
        .output()
        .unwrap_or_else(|e| panic!("cannot run rustlavel {args:?}: {e}"))
}

fn run_ok(directory: &Path, args: &[&str]) -> String {
    let output = run(directory, args);
    assert!(
        output.status.success(),
        "rustlavel {args:?} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

/// Scaffold an application with the packages the generated CRUD code needs.
fn scaffold(name: &str) -> PathBuf {
    let directory = fixture(name);
    run_ok(
        &directory,
        &[
            "new",
            "app",
            "--local",
            &framework().display().to_string(),
            "--with",
            "db,view,validation",
        ],
    );
    directory.join("app")
}

#[test]
fn make_crud_writes_every_file_the_resource_needs() {
    let app = scaffold("crud-files");
    let output = run_ok(&app, &["make:crud", "Post", "--fields", "title:string,body:text,published:bool"]);

    for path in [
        "src/models/post.rs",
        "src/controllers/post_controller.rs",
        "resources/views/posts/index.rl.html",
        "resources/views/posts/form.rl.html",
        "resources/views/layouts/app.rl.html",
    ] {
        assert!(app.join(path).exists(), "{path} was not written\n{output}");
    }

    // The migration's name carries a timestamp, so it is found rather than named.
    let migrations: Vec<String> = std::fs::read_dir(app.join("database/migrations"))
        .expect("migrations directory")
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.ends_with("_create_posts_table.rs"))
        .collect();
    assert_eq!(migrations.len(), 1, "expected one migration, found {migrations:?}");
}

#[test]
fn make_crud_keeps_the_model_the_migration_and_the_form_in_step() {
    let app = scaffold("crud-columns");
    run_ok(
        &app,
        &[
            "make:crud",
            "Post",
            "--fields",
            "title:string,body:text,published:bool,views:integer,email:string?",
        ],
    );

    let model = read(&app.join("src/models/post.rs"));
    assert!(model.contains("#[model(table = \"posts\")]"), "{model}");
    assert!(model.contains("pub title: String,"), "{model}");
    assert!(model.contains("pub published: bool,"), "{model}");
    assert!(model.contains("pub views: i32,"), "{model}");
    // A `?` is an Option here and a `.nullable()` in the migration.
    assert!(model.contains("pub email: Option<String>,"), "{model}");

    let migration = std::fs::read_dir(app.join("database/migrations"))
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .find(|path| path.to_string_lossy().ends_with("_create_posts_table.rs"))
        .map(|path| read(&path))
        .expect("the migration");
    assert!(migration.contains("t.string(\"title\");"), "{migration}");
    assert!(migration.contains("t.text(\"body\");"), "{migration}");
    assert!(migration.contains("t.boolean(\"published\").default_bool(false);"), "{migration}");
    assert!(migration.contains("t.integer(\"views\");"), "{migration}");
    assert!(migration.contains("t.string(\"email\").nullable();"), "{migration}");
    assert!(migration.contains("t.timestamps();"), "{migration}");

    let controller = read(&app.join("src/controllers/post_controller.rs"));
    for handler in ["index", "create", "store", "edit", "update", "destroy"] {
        assert!(controller.contains(&format!("pub async fn {handler}(")), "no {handler} handler");
    }
    assert!(controller.contains("(\"title\", \"required|string|max:255\"),"), "{controller}");
    assert!(controller.contains("(\"email\", \"nullable|string|max:255\"),"), "{controller}");
    assert!(controller.contains("validate(&mut req, RULES).await"), "{controller}");

    let form = read(&app.join("resources/views/posts/form.rl.html"));
    for column in ["title", "body", "published", "views", "email"] {
        assert!(form.contains(&format!("name=\"{column}\"")), "no input for {column}:\n{form}");
        assert!(form.contains(&format!("@if(error_{column})")), "no error for {column}");
    }
    assert!(form.contains("<textarea id=\"body\""), "{form}");
    assert!(form.contains("type=\"checkbox\""), "{form}");

    let index = read(&app.join("resources/views/posts/index.rl.html"));
    for column in ["title", "body", "published", "views", "email"] {
        assert!(index.contains(&format!("{{{{ record.{column} }}}}")), "no cell for {column}");
    }
}

#[test]
fn make_crud_registers_the_migration_declares_the_modules_and_wires_the_routes() {
    let app = scaffold("crud-registry");
    run_ok(&app, &["make:crud", "Post", "--fields", "title:string"]);

    // The registry is generated from the directory, which is what stops a
    // migration from existing but never running.
    let registry = read(&app.join("database/migrations/mod.rs"));
    assert!(registry.contains("_create_posts_table;"), "{registry}");
    assert!(registry.contains("::CreatePostsTable,"), "{registry}");

    assert!(read(&app.join("src/models/mod.rs")).contains("pub mod post;"));
    assert!(read(&app.join("src/lib.rs")).contains("pub mod models;"));
    assert!(read(&app.join("src/controllers/mod.rs")).contains("pub mod post_controller;"));

    let routes = read(&app.join("src/routes/web.rs"));
    assert!(routes.contains("use crate::controllers::post_controller::PostController;"), "{routes}");
    assert!(routes.contains("r.get(\"/posts\", PostController::index).name(\"posts.index\");"), "{routes}");
    assert!(routes.contains("r.post(\"/posts/{id}/delete\", PostController::destroy)"), "{routes}");
    // The routes go inside the function, not after it.
    let inserted = routes.find("PostController::index").expect("the route");
    let closing = routes.rfind('}').expect("the closing brace");
    assert!(inserted < closing, "the routes were written outside `fn routes`:\n{routes}");
    // And the file the scaffold wrote is still there.
    assert!(routes.contains("WelcomeController::index"), "{routes}");
}

#[test]
fn make_crud_without_fields_says_so_and_still_generates() {
    let app = scaffold("crud-skeleton");
    let output = run_ok(&app, &["make:crud", "Post"]);

    assert!(output.contains("no --fields given"), "the skeleton was not announced:\n{output}");
    let model = read(&app.join("src/models/post.rs"));
    assert!(model.contains("pub id: i64,"), "{model}");
    assert!(model.contains("pub created_at: Option<String>,"), "{model}");
    // Nothing was invented: an id and timestamps, and no other column.
    assert!(!model.contains("pub title"), "{model}");
}

#[test]
fn a_bad_field_spec_is_refused_and_names_the_token() {
    let app = scaffold("crud-bad-spec");
    let output = run(&app, &["make:crud", "Post", "--fields", "title:strng"]);

    assert!(!output.status.success(), "a bad spec should fail");
    let message = String::from_utf8_lossy(&output.stderr);
    assert!(message.contains("`strng`"), "{message}");
    assert!(message.contains("string"), "{message}");
    // And it stopped before writing anything.
    assert!(!app.join("src/models/post.rs").exists(), "a file was written despite the error");
}

#[test]
fn make_crud_refuses_to_clobber_a_resource_that_exists() {
    let app = scaffold("crud-twice");
    run_ok(&app, &["make:crud", "Post", "--fields", "title:string"]);
    let again = run(&app, &["make:crud", "Post", "--fields", "title:string"]);

    assert!(!again.status.success(), "the second run should refuse");
    let message = String::from_utf8_lossy(&again.stderr);
    assert!(message.contains("already exists"), "{message}");
}

#[test]
fn make_crud_extends_a_layout_the_project_already_has() {
    let app = scaffold("crud-layout");
    let layout = app.join("resources/views/layouts/app.rl.html");
    std::fs::create_dir_all(layout.parent().unwrap()).unwrap();
    std::fs::write(&layout, "MINE @yield(\"content\")\n").unwrap();

    run_ok(&app, &["make:crud", "Post", "--fields", "title:string"]);

    assert_eq!(read(&layout), "MINE @yield(\"content\")\n", "the layout was overwritten");
    assert!(read(&app.join("resources/views/posts/index.rl.html")).contains("@extends(\"layouts.app\")"));
}

#[test]
fn make_package_writes_a_publishable_crate() {
    let directory = fixture("package");
    run_ok(&directory, &["make:package", "rustlavel-audit"]);
    let package = directory.join("rustlavel-audit");

    // The manifest and the target are written together: a crate directory with
    // a manifest and no target breaks every workspace that contains it.
    assert!(package.join("Cargo.toml").exists());
    assert!(package.join("src/lib.rs").exists());
    assert!(package.join("README.md").exists());

    let manifest = read(&package.join("Cargo.toml"));
    for required in ["name = \"rustlavel-audit\"", "description = ", "license = ", "repository = "] {
        assert!(manifest.contains(required), "the manifest is missing {required}:\n{manifest}");
    }

    let lib = read(&package.join("src/lib.rs"));
    assert!(lib.contains("impl Plugin for Audit"), "{lib}");
    assert!(lib.contains("pub fn from_config(config: &Config) -> Audit"), "{lib}");
    assert!(lib.contains("config.bool(\"audit.enabled\", true)"), "{lib}");
    assert!(lib.contains("pub mod prelude"), "{lib}");
    assert!(lib.contains("#[cfg(test)]"), "{lib}");

    assert!(read(&package.join("README.md")).contains("a crate plus a feature flag"));
}

#[test]
fn make_package_does_not_need_an_application_around_it() {
    // The fixture is a bare directory: no Cargo.toml, nothing depending on the
    // framework. A package is not part of an application, so this must work.
    let directory = fixture("package-standalone");
    let output = run(&directory, &["make:package", "audit"]);

    assert!(
        output.status.success(),
        "make:package should not require a project\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(directory.join("audit/src/lib.rs").exists());
}

/// The check that matters: the generated application is a program that builds.
///
/// Ignored by default because it compiles the framework and an application from
/// scratch — minutes, not seconds. Run it with:
///
/// ```bash
/// cargo test -p rustlavel-cli -- --ignored
/// ```
#[test]
#[ignore = "compiles a whole scaffolded application; run with --ignored"]
fn the_generated_crud_application_compiles() {
    let app = scaffold("crud-compiles");
    run_ok(
        &app,
        &[
            "make:crud",
            "Post",
            "--fields",
            "title:string,body:text,published:bool,views:integer,rating:float,\
             price:decimal,due:date,seen_at:timestamp,email:string?,rank:bigint?",
        ],
    );

    let build = Command::new("cargo")
        .arg("build")
        .current_dir(&app)
        .output()
        .expect("cannot run cargo build");

    assert!(
        build.status.success(),
        "the generated application did not compile:\n{}",
        String::from_utf8_lossy(&build.stderr)
    );
}
