//! `rustlavel make:crud <Name>` — everything one resource needs, at once.
//!
//! `make:model` gives you a struct; this gives you the screen. A model, the
//! migration that creates its table, a resourceful controller, the two views
//! behind it, and the routes that reach them — all agreeing on the same
//! columns, because they are generated from one `--fields` spec.
//!
//! Nothing here is a runtime template. Every file is written into the project
//! and belongs to it from that moment: this is a starting point to edit, not a
//! layer to configure.

use crate::console;
use crate::database;
use crate::fields::{self, Field};
use crate::naming;
use crate::project::{self, Project};
use crate::stubs::{self, render};
use std::collections::BTreeMap;
use std::path::Path;

/// Every name one resource needs, derived once so nothing disagrees.
pub struct Names {
    /// `Post`
    pub class: String,
    /// `post`
    pub module: String,
    /// `posts`
    pub table: String,
    /// `PostController`
    pub controller: String,
    /// `post_controller`
    pub controller_module: String,
    /// `/posts`
    pub base: String,
    /// `posts` — the directory under `resources/views`
    pub view_dir: String,
    /// `posts` — the prefix on every route name
    pub route_name: String,
    /// `Posts`
    pub plural_label: String,
    /// `post`
    pub singular_lower: String,
    /// `posts`
    pub plural_lower: String,
}

impl Names {
    pub fn of(name: &str) -> Names {
        // `make:crud Post` and `make:crud posts` are the same intent, so the
        // input is normalised rather than taken literally.
        let class = naming::pascal(name.trim_end_matches("Controller"));
        let module = naming::snake(&class);
        let plural_snake = naming::plural(&module);
        let plural_kebab = plural_snake.replace('_', "-");

        Names {
            controller: format!("{class}Controller"),
            controller_module: format!("{module}_controller"),
            table: plural_snake.clone(),
            base: format!("/{plural_kebab}"),
            view_dir: plural_snake.clone(),
            route_name: plural_kebab,
            plural_label: capitalise(&plural_snake.replace('_', " ")),
            singular_lower: module.replace('_', " "),
            plural_lower: plural_snake.replace('_', " "),
            class,
            module,
        }
    }
}

fn capitalise(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// `rustlavel make:crud --help`.
pub fn help() {
    console::heading("rustlavel make:crud <Name>");
    console::info("Everything one resource needs: a model, the migration that creates its");
    console::info("table, a resourceful controller, an index and a form view, and the routes.\n");
    console::info(&console::bold("USAGE"));
    console::info("  rustlavel make:crud Post --fields \"title:string,body:text,published:bool\"");
    console::info("  rustlavel make:crud Post          # an id/timestamps skeleton, no columns\n");
    console::info(&console::bold("FIELD TYPES"));
    console::info("  string  text  integer  bigint  float  decimal  bool  date  timestamp");
    console::info("  A trailing `?` makes the column nullable: email:string?\n");
    console::info(&console::bold("WHERE THE ROUTES GO"));
    console::info("  Appended to src/routes/web.rs, inside `pub fn routes`, with the import");
    console::info("  above it. A resource nobody wired up is a resource that does not work,");
    console::info("  and pasting six lines by hand is six chances to paste five. The edit is");
    console::info("  only made when the file still has the shape the scaffold wrote — one");
    console::info("  `pub fn routes(r: &mut Router)`. If it has been reshaped, the block is");
    console::info("  printed instead and nothing is touched.\n");
    console::info(&console::dim("Needs the db, view and validation packages."));
    println!();
}

pub fn run(project: &Project, name: &str, rest: &[String]) -> Result<(), String> {
    let mut spec: Option<String> = None;
    let mut iter = rest.iter();
    while let Some(argument) = iter.next() {
        match argument.as_str() {
            "-h" | "--help" => {
                help();
                return Ok(());
            }
            "--fields" => {
                spec = Some(
                    iter.next()
                        .ok_or(
                            "--fields needs a list, as in --fields \"title:string,body:text\"",
                        )?
                        .clone(),
                );
            }
            other if other.starts_with("--fields=") => {
                spec = Some(other["--fields=".len()..].to_string());
            }
            other => return Err(format!("unknown option `{other}` for make:crud")),
        }
    }

    let columns = match &spec {
        Some(spec) => fields::parse(spec)?,
        None => Vec::new(),
    };
    let names = Names::of(name);

    console::heading(&format!("Creating CRUD for {}", console::accent(&names.class)));
    if columns.is_empty() {
        // Said out loud, because a skeleton that silently has no columns is a
        // surprise three files later.
        console::info(&console::dim(
            "no --fields given: generating an id/timestamps skeleton with no columns.",
        ));
    }

    model(project, &names, &columns)?;
    migration(project, &names, &columns)?;
    controller(project, &names, &columns)?;
    views(project, &names, &columns)?;
    let wired = routes(project, &names)?;

    warn_about_missing_packages(project);

    let next = if wired {
        format!(
            "  rustlavel migrate\n  rustlavel serve      # then open {}",
            names.base
        )
    } else {
        format!(
            "  Paste these into src/routes/web.rs:\n{}\n  rustlavel migrate",
            render_routes(&names)
        )
    };
    console::success(&format!("{} is ready.\n\n{next}", names.class));
    Ok(())
}

fn model(project: &Project, names: &Names, columns: &[Field]) -> Result<(), String> {
    let declarations = columns.iter().map(Field::declaration).collect::<Vec<_>>().join("\n");

    let mut values = BTreeMap::new();
    values.insert("class", names.class.clone());
    values.insert("table", names.table.clone());
    values.insert("fields", declarations);

    let path = project.root.join("src/models").join(format!("{}.rs", names.module));
    write_new(&path, &render(stubs::CRUD_MODEL_STUB, &values))?;
    console::created(&relative(project, &path));

    declare(project, "src/models/mod.rs", &names.module)?;
    ensure_module(project, "models")
}

fn migration(project: &Project, names: &Names, columns: &[Field]) -> Result<(), String> {
    let stamp = database::timestamp();
    let file = format!("{stamp}_create_{}_table", names.table);
    let lines = columns.iter().map(Field::migration_line).collect::<Vec<_>>().join("\n");

    let mut values = BTreeMap::new();
    values.insert("class", names.class.clone());
    values.insert("migration_class", format!("Create{}Table", naming::pascal(&names.table)));
    values.insert("name", file.clone());
    values.insert("table", names.table.clone());
    values.insert("columns", lines);

    let path = project.root.join("database/migrations").join(format!("m{file}.rs"));
    write_new(&path, &render(stubs::CRUD_MIGRATION_STUB, &values))?;
    console::created(&relative(project, &path));

    database::regenerate_registry(project, "database/migrations", stubs::MIGRATIONS_REGISTRY, "m")?;
    database::ensure_database_module(project)
}

fn controller(project: &Project, names: &Names, columns: &[Field]) -> Result<(), String> {
    let rules = columns
        .iter()
        .map(|field| format!("    (\"{}\", \"{}\"),", field.name, field.rule()))
        .collect::<Vec<_>>()
        .join("\n");

    let stored = columns
        .iter()
        .map(|field| format!("            (\"{}\", {}),", field.name, field.to_form_value("record")))
        .collect::<Vec<_>>()
        .join("\n");

    let filled = columns
        .iter()
        .map(|field| format!("        record.{} = {};", field.name, field.read_validated()))
        .collect::<Vec<_>>()
        .join("\n");

    let mut values = BTreeMap::new();
    values.insert("class", names.class.clone());
    values.insert("controller", names.controller.clone());
    values.insert("module", names.module.clone());
    values.insert("table", names.table.clone());
    values.insert("base", names.base.clone());
    values.insert("view_dir", names.view_dir.clone());
    values.insert("plural_label", names.plural_label.clone());
    values.insert("singular_lower", names.singular_lower.clone());
    values.insert("rules", rules);
    values.insert("stored", stored);
    values.insert("filled", filled);
    // With no columns there is nothing to assign, so `mut` and the argument
    // would both be unused — and a generator that emits a warning on its first
    // build has taught the developer to ignore warnings.
    values.insert("filled_mut", if columns.is_empty() { String::new() } else { "mut ".into() });
    values.insert("filled_valid", if columns.is_empty() { "_valid".into() } else { "valid".into() });

    let path = project
        .root
        .join("src/controllers")
        .join(format!("{}.rs", names.controller_module));
    // The `.with("csrf_field", …)` line, or nothing. The same reading the views
    // get: with the auth package off there is no helper to call, and with it on
    // a form that omits the token is refused by the middleware, so a generated
    // form without one is a form that does not work and does not say why.
    let has_auth = feature_enabled(project, "auth");
    values.insert(
        "csrf_index",
        if has_auth { "rustlavel::auth::csrf::field(&req)" } else { "\"\"" }.to_string(),
    );
    values.insert(
        "csrf_form",
        if has_auth { "rustlavel::auth::csrf::field(req)" } else { "\"\"" }.to_string(),
    );
    // Underscored when there is no token to read from it, so a project with the
    // auth package off does not build with a warning in its own code. A lint in
    // generated code is a lint in somebody else's compiler output.
    values.insert("form_req", if has_auth { "req" } else { "_req" }.to_string());

    write_new(&path, &render(stubs::CRUD_CONTROLLER_STUB, &values))?;
    console::created(&relative(project, &path));

    declare(project, "src/controllers/mod.rs", &names.controller_module)
}

fn views(project: &Project, names: &Names, columns: &[Field]) -> Result<(), String> {
    // A project scaffolded with `--with auth-kit` already has a layout, and
    // extending it is what makes the generated screens look like the rest of
    // the application instead of like a generator's idea of one.
    let layout_path = project.root.join("resources/views/layouts/app.rl.html");
    if !layout_path.exists() {
        write_new(&layout_path, stubs::CRUD_LAYOUT_VIEW)?;
        console::created(&relative(project, &layout_path));
    }

    let headers = columns
        .iter()
        .map(|field| format!("          <th>{}</th>", field.label()))
        .collect::<Vec<_>>()
        .join("\n");
    let cells = columns
        .iter()
        .map(|field| format!("            <td>{{{{ record.{} }}}}</td>", field.name))
        .collect::<Vec<_>>()
        .join("\n");
    let inputs = columns.iter().map(Field::input_html).collect::<Vec<_>>().join("\n");

    let mut values = BTreeMap::new();
    values.insert("layout", "app".to_string());
    values.insert("base", names.base.clone());
    values.insert("plural_label", names.plural_label.clone());
    values.insert("plural_lower", names.plural_lower.clone());
    values.insert("singular_lower", names.singular_lower.clone());
    values.insert("headers", headers);
    values.insert("cells", cells);
    values.insert("inputs", inputs);

    // The hidden CSRF input, or nothing.
    //
    // Decided when the file is written rather than when it compiles, because a
    // generator can simply look: with the auth package on, the middleware is
    // there and a form without a token is refused; without it, the helper does
    // not exist to call. A comment telling somebody to add it later is a form
    // that does not work and does not say why.
    let csrf = if feature_enabled(project, "auth") {
        "    {!! csrf_field !!}\n".to_string()
    } else {
        String::new()
    };
    values.insert("csrf", csrf);

    let directory = project.root.join("resources/views").join(&names.view_dir);
    for (file, template) in
        [("index.rl.html", stubs::CRUD_INDEX_VIEW), ("form.rl.html", stubs::CRUD_FORM_VIEW)]
    {
        let path = directory.join(file);
        write_new(&path, &render(template, &values))?;
        console::created(&relative(project, &path));
    }
    Ok(())
}

fn render_routes(names: &Names) -> String {
    let mut values = BTreeMap::new();
    values.insert("class", names.class.clone());
    values.insert("controller", names.controller.clone());
    values.insert("base", names.base.clone());
    values.insert("route_name", names.route_name.clone());
    render(stubs::CRUD_ROUTES_STUB, &values)
}

/// Wire the routes into `src/routes/web.rs`.
///
/// Editing somebody's file is not something a generator should do lightly, so
/// it only happens when the file still has the shape the scaffold wrote — one
/// `pub fn routes(r: &mut Router)`. Anything else and the block is printed to
/// paste, which is worse than nothing exactly zero times.
fn routes(project: &Project, names: &Names) -> Result<bool, String> {
    let path = project.root.join("src/routes/web.rs");
    let Ok(existing) = std::fs::read_to_string(&path) else { return Ok(false) };

    if existing.contains(&format!("{}::index", names.controller)) {
        console::info(&console::dim("src/routes/web.rs already has these routes."));
        return Ok(true);
    }

    const ANCHOR: &str = "pub fn routes(r: &mut Router) {";
    let Some(start) = existing.find(ANCHOR) else { return Ok(false) };
    let Some(end) = closing_brace(&existing, start + ANCHOR.len() - 1) else { return Ok(false) };

    let import = format!(
        "use crate::controllers::{}::{};\n",
        names.controller_module, names.controller
    );
    let mut updated = String::with_capacity(existing.len() + 512);
    updated.push_str(&existing[..start]);
    if !existing.contains(&import) {
        updated.push_str(&import);
        updated.push('\n');
    }
    updated.push_str(&existing[start..end]);
    updated.push_str(&render_routes(names));
    updated.push_str(&existing[end..]);

    std::fs::write(&path, updated).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    console::updated(&relative(project, &path));
    Ok(true)
}

/// The index of the `}` matching the `{` at `open`, ignoring braces inside
/// strings, characters and comments.
fn closing_brace(source: &str, open: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    if bytes.get(open) != Some(&b'{') {
        return None;
    }

    let mut depth = 0i32;
    let mut index = open;
    while index < bytes.len() {
        match bytes[index] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(index);
                }
            }
            b'"' => index = skip_string(bytes, index)?,
            b'\'' => index = skip_char(bytes, index),
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                index = bytes[index..].iter().position(|b| *b == b'\n').map_or(bytes.len(), |at| index + at);
                continue;
            }
            _ => {}
        }
        index += 1;
    }
    None
}

/// The index of the closing quote of the string literal opening at `start`.
fn skip_string(bytes: &[u8], start: usize) -> Option<usize> {
    let mut index = start + 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 1,
            b'"' => return Some(index),
            _ => {}
        }
        index += 1;
    }
    None
}

/// A `'` is a lifetime far more often than a character literal in this file, so
/// only a genuine `'x'` or `'\n'` is skipped and anything else is left alone.
fn skip_char(bytes: &[u8], start: usize) -> usize {
    if bytes.get(start + 1) == Some(&b'\\') {
        return bytes[start + 2..]
            .iter()
            .position(|b| *b == b'\'')
            .map_or(start, |at| start + 2 + at);
    }
    if bytes.get(start + 2) == Some(&b'\'') {
        return start + 2;
    }
    start
}

/// Whether the project has a `rustlavel` feature enabled.
///
/// Read from `Cargo.toml` by hand, the same way the warning below does. Getting
/// this wrong costs a warning or a missing hidden input, never a broken build,
/// which is why it is allowed to be approximate.
pub(crate) fn feature_enabled(project: &Project, feature: &str) -> bool {
    let manifest = std::fs::read_to_string(project.root.join("Cargo.toml")).unwrap_or_default();
    manifest
        .lines()
        .find(|line| {
            line.trim_start().starts_with("rustlavel ") || line.trim_start().starts_with("rustlavel=")
        })
        .is_some_and(|line| line.contains(&format!("\"{feature}\"")))
}

/// Say what the generated code needs but the project has not enabled.
///
/// A warning rather than a refusal: the check reads a `Cargo.toml` by hand, and
/// a generator that will not run because it misread a manifest is worse than
/// one that says what to turn on.
fn warn_about_missing_packages(project: &Project) {
    let manifest = std::fs::read_to_string(project.root.join("Cargo.toml")).unwrap_or_default();
    let Some(features) = manifest
        .lines()
        .find(|line| line.trim_start().starts_with("rustlavel ") || line.trim_start().starts_with("rustlavel="))
    else {
        return;
    };

    let missing: Vec<&str> = ["db", "view", "validation"]
        .into_iter()
        .filter(|package| !features.contains(&format!("\"{package}\"")))
        .collect();
    if missing.is_empty() {
        return;
    }

    console::info(&console::dim(&format!(
        "the generated code needs the {} package{} — add {} to the rustlavel features in Cargo.toml.",
        missing.join(", "),
        if missing.len() == 1 { "" } else { "s" },
        missing.iter().map(|p| format!("\"{p}\"")).collect::<Vec<_>>().join(", ")
    )));
}

fn declare(project: &Project, mod_path: &str, module: &str) -> Result<(), String> {
    let path = project.root.join(mod_path);
    if project::declare_module(&path, module)? {
        console::updated(&relative(project, &path));
    }
    Ok(())
}

fn ensure_module(project: &Project, name: &str) -> Result<(), String> {
    let lib = project.root.join("src/lib.rs");
    let contents = std::fs::read_to_string(&lib).unwrap_or_default();
    let declaration = format!("pub mod {name};");
    if contents.contains(&declaration) {
        return Ok(());
    }
    std::fs::write(&lib, format!("{contents}{declaration}\n")).map_err(|e| e.to_string())?;
    console::updated(&relative(project, &lib));
    Ok(())
}

fn write_new(path: &Path, contents: &str) -> Result<(), String> {
    if path.exists() {
        return Err(format!("{} already exists", path.display()));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(path, contents).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

fn relative(project: &Project, path: &Path) -> String {
    path.strip_prefix(&project.root).unwrap_or(path).display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_every_name_from_the_model() {
        let names = Names::of("Post");

        assert_eq!(names.class, "Post");
        assert_eq!(names.module, "post");
        assert_eq!(names.table, "posts");
        assert_eq!(names.controller, "PostController");
        assert_eq!(names.controller_module, "post_controller");
        assert_eq!(names.base, "/posts");
        assert_eq!(names.view_dir, "posts");
        assert_eq!(names.route_name, "posts");
        assert_eq!(names.plural_label, "Posts");
        assert_eq!(names.singular_lower, "post");
    }

    #[test]
    fn a_two_word_model_stays_readable_everywhere() {
        let names = Names::of("UserProfile");

        assert_eq!(names.table, "user_profiles");
        assert_eq!(names.controller, "UserProfileController");
        assert_eq!(names.controller_module, "user_profile_controller");
        // Snake for a Rust path, kebab for a URL.
        assert_eq!(names.view_dir, "user_profiles");
        assert_eq!(names.base, "/user-profiles");
        assert_eq!(names.plural_label, "User profiles");
        assert_eq!(names.singular_lower, "user profile");
    }

    #[test]
    fn irregular_plurals_reach_the_table_and_the_url() {
        let names = Names::of("Category");
        assert_eq!(names.table, "categories");
        assert_eq!(names.base, "/categories");

        let person = Names::of("Person");
        assert_eq!(person.table, "people");
        assert_eq!(person.base, "/people");
    }

    #[test]
    fn a_controller_suffix_is_not_doubled() {
        assert_eq!(Names::of("PostController").controller, "PostController");
    }

    /// The check that matters for a template: nothing `{{like_this}}` survives
    /// into a file somebody is about to compile.
    #[test]
    fn every_generated_file_renders_with_no_placeholder_left() {
        let names = Names::of("Post");
        let columns =
            fields::parse("title:string,body:text,published:bool,views:integer,price:decimal,email:string?")
                .unwrap();

        for (label, rendered) in rendered_files(&names, &columns) {
            assert!(
                !has_placeholder(&rendered),
                "{label} still holds a {{{{placeholder}}}}:\n{rendered}"
            );
        }
    }

    #[test]
    fn a_skeleton_with_no_fields_also_renders_completely() {
        let names = Names::of("Post");

        for (label, rendered) in rendered_files(&names, &[]) {
            assert!(!has_placeholder(&rendered), "{label} still holds a placeholder");
        }
    }

    #[test]
    fn the_controller_names_the_model_the_table_and_the_routes() {
        let names = Names::of("Post");
        let columns = fields::parse("title:string,published:bool").unwrap();
        let controller = rendered_files(&names, &columns)
            .into_iter()
            .find(|(label, _)| *label == "controller")
            .map(|(_, body)| body)
            .unwrap();

        assert!(controller.contains("use crate::models::post::Post;"));
        assert!(controller.contains("pub struct PostController;"));
        for handler in ["index", "create", "store", "edit", "update", "destroy"] {
            assert!(controller.contains(&format!("pub async fn {handler}(")), "missing {handler}");
        }
        assert!(controller.contains("(\"title\", \"required|string|max:255\"),"));
        assert!(controller.contains("(\"published\", \"nullable|boolean\"),"));
        assert!(controller.contains("record.published = valid.boolean(\"published\").unwrap_or(false);"));
        assert!(controller.contains("Response::see_other(\"/posts\")"));
        assert!(controller.contains("posts/index"));
    }

    #[test]
    fn the_skeleton_controller_does_not_bind_an_unused_argument() {
        let names = Names::of("Post");
        let controller = rendered_files(&names, &[])
            .into_iter()
            .find(|(label, _)| *label == "controller")
            .map(|(_, body)| body)
            .unwrap();

        assert!(controller.contains("fn filled(record: Post, _valid: &Validated)"), "{controller}");
    }

    #[test]
    fn the_views_show_every_column_and_the_routes_reach_them() {
        let names = Names::of("Post");
        let columns = fields::parse("title:string,body:text,published:bool").unwrap();
        let files = rendered_files(&names, &columns);
        let file = |wanted: &str| {
            files.iter().find(|(label, _)| label == &wanted).map(|(_, body)| body.clone()).unwrap()
        };

        let index = file("index view");
        assert!(index.contains("{{ record.title }}"));
        assert!(index.contains("{{ record.published }}"));
        assert!(index.contains("href=\"/posts/create\""));
        assert!(index.contains("action=\"/posts/{{ record.id }}/delete\""));

        let form = file("form view");
        assert!(form.contains("name=\"title\""));
        assert!(form.contains("<textarea id=\"body\""));
        assert!(form.contains("type=\"checkbox\""));
        assert!(form.contains("@if(error_title)"));

        let routes = file("routes");
        assert!(routes.contains("r.get(\"/posts\", PostController::index).name(\"posts.index\");"));
        assert!(routes.contains("r.post(\"/posts/{id}/delete\", PostController::destroy)"));
    }

    #[test]
    fn finds_the_end_of_the_routes_function() {
        let source = "pub fn routes(r: &mut Router) {\n    r.get(\"/\", home);\n}\n";
        let open = source.find('{').unwrap();
        let close = closing_brace(source, open).unwrap();
        assert_eq!(&source[close..close + 1], "}");
        assert_eq!(close, source.len() - 2);
    }

    #[test]
    fn a_brace_inside_a_string_or_a_comment_is_not_counted() {
        let source = "fn routes() {\n    r.get(\"/a/{id}\", h); // } not this one\n    let c = '}';\n}\n";
        let open = source.find('{').unwrap();
        let close = closing_brace(source, open).unwrap();
        assert_eq!(close, source.len() - 2, "matched the wrong brace in:\n{source}");
    }

    fn has_placeholder(rendered: &str) -> bool {
        // The view engine's own `{{ spaced }}` variables are not placeholders;
        // only `{{unspaced}}` is one this crate should have filled in.
        let mut rest = rendered;
        while let Some(at) = rest.find("{{") {
            let after = &rest[at + 2..];
            if let Some(end) = after.find("}}")
                && !after[..end].contains(char::is_whitespace)
                && !after[..end].is_empty()
                && after[..end].chars().all(|c| c.is_ascii_lowercase() || c == '_')
            {
                return true;
            }
            rest = after;
        }
        false
    }

    /// Every template `make:crud` writes, rendered exactly as the command does.
    fn rendered_files(names: &Names, columns: &[Field]) -> Vec<(&'static str, String)> {
        let declarations = columns.iter().map(Field::declaration).collect::<Vec<_>>().join("\n");
        let mut model = BTreeMap::new();
        model.insert("class", names.class.clone());
        model.insert("table", names.table.clone());
        model.insert("fields", declarations);

        let mut migration = BTreeMap::new();
        migration.insert("class", names.class.clone());
        migration.insert("migration_class", format!("Create{}Table", naming::pascal(&names.table)));
        migration.insert("name", format!("2026_01_01_000000_create_{}_table", names.table));
        migration.insert("table", names.table.clone());
        migration.insert(
            "columns",
            columns.iter().map(Field::migration_line).collect::<Vec<_>>().join("\n"),
        );

        let mut controller = BTreeMap::new();
        controller.insert("class", names.class.clone());
        controller.insert("controller", names.controller.clone());
        controller.insert("module", names.module.clone());
        controller.insert("table", names.table.clone());
        controller.insert("base", names.base.clone());
        controller.insert("view_dir", names.view_dir.clone());
        controller.insert("plural_label", names.plural_label.clone());
        controller.insert("singular_lower", names.singular_lower.clone());
        // The auth-dependent substitutions. The test renders the `auth` shape,
        // which is the one with something in it to leave behind.
        controller.insert("csrf_index", "rustlavel::auth::csrf::field(&req)".to_string());
        controller.insert("csrf_form", "rustlavel::auth::csrf::field(req)".to_string());
        controller.insert("form_req", "req".to_string());
        controller.insert(
            "rules",
            columns
                .iter()
                .map(|f| format!("    (\"{}\", \"{}\"),", f.name, f.rule()))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        controller.insert(
            "stored",
            columns
                .iter()
                .map(|f| format!("            (\"{}\", {}),", f.name, f.to_form_value("record")))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        controller.insert(
            "filled",
            columns
                .iter()
                .map(|f| format!("        record.{} = {};", f.name, f.read_validated()))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        controller
            .insert("filled_mut", if columns.is_empty() { String::new() } else { "mut ".into() });
        controller.insert(
            "filled_valid",
            if columns.is_empty() { "_valid".into() } else { "valid".into() },
        );

        let mut view = BTreeMap::new();
        view.insert("layout", "app".to_string());
        view.insert("base", names.base.clone());
        view.insert("plural_label", names.plural_label.clone());
        view.insert("plural_lower", names.plural_lower.clone());
        view.insert("singular_lower", names.singular_lower.clone());
        view.insert(
            "headers",
            columns.iter().map(|f| format!("          <th>{}</th>", f.label())).collect::<Vec<_>>().join("\n"),
        );
        view.insert(
            "cells",
            columns
                .iter()
                .map(|f| format!("            <td>{{{{ record.{} }}}}</td>", f.name))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        view.insert("inputs", columns.iter().map(Field::input_html).collect::<Vec<_>>().join("\n"));
        view.insert("csrf", "    {!! csrf_field !!}\n".to_string());

        vec![
            ("model", render(stubs::CRUD_MODEL_STUB, &model)),
            ("migration", render(stubs::CRUD_MIGRATION_STUB, &migration)),
            ("controller", render(stubs::CRUD_CONTROLLER_STUB, &controller)),
            ("index view", render(stubs::CRUD_INDEX_VIEW, &view)),
            ("form view", render(stubs::CRUD_FORM_VIEW, &view)),
            ("layout", stubs::CRUD_LAYOUT_VIEW.to_string()),
            ("routes", render_routes(names)),
        ]
    }
}
