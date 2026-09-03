//! The Backup tab of the Settings page: take one, fetch one, put one back.
//!
//! The work is all in `support/backup.rs`; this file is the four HTTP verbs
//! around it, the permission check on each, and the variables the tab renders
//! from. The one piece of judgement that lives here rather than there is the
//! status column: a row is inserted as `running` before the first byte is
//! written and only becomes `ready` once the file is closed and its size is
//! known. A backup that died half-way must never look like one you can restore
//! from, and this is the half of that rule the database is responsible for —
//! the other half is the end marker the file format requires.
//!
//! The tab itself is rendered by `AdminSettingsController`, like every other
//! tab. This controller contributes [`BackupController::context`] to that
//! render rather than owning a page of its own, so the tab strip is built in
//! one place.

use rustlavel::prelude::*;

use crate::controllers::admin::users_controller::rbac;
use crate::support::{backup, page, tokens};

pub struct BackupController;

impl BackupController {
    /// Everything the tab renders.
    ///
    /// Called from `AdminSettingsController::show_tab` when the slug is
    /// `backup`, the same way the Language tab is given its list of locales.
    ///
    /// The route into that page is guarded by `settings.manage`, which is not
    /// the same permission as being allowed to see what is in the database, so
    /// `backups.view` is checked here and the panel says so when it is missing.
    pub async fn context(req: &Request, context: ViewContext) -> Result<ViewContext> {
        let context = context
            .with("can_create_backup", Json::from(req.can("backups.create").await?))
            .with("can_restore_backup", Json::from(req.can("backups.restore").await?))
            .with("can_delete_backup", Json::from(req.can("backups.delete").await?))
            .with("can_view_backups", Json::from(req.can("backups.view").await?));

        if !req.can("backups.view").await? {
            return Ok(context
                .with("q", Json::from(""))
                .with("backups_empty", Json::from(true))
                .with("backups", Json::Array(Vec::new())));
        }

        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();

        let search = req.query("q").unwrap_or_default().trim().to_string();
        let mut query = db.table("backups").latest("created_at");
        if !search.is_empty() {
            query = query.filter_like("name", format!("%{search}%"));
        }

        let mut rows = Vec::new();
        for row in query.get(&db).await? {
            let id = row.get::<i64>("id").unwrap_or_default();
            let name = row.get::<String>("name").unwrap_or_default();
            let status = row.get::<String>("status").unwrap_or_else(|_| "failed".into());
            let bytes = row.get::<i64>("bytes").unwrap_or(0);
            let ready = status == "ready";

            rows.push(Json::object([
                ("id", Json::from(id)),
                ("name", Json::from(name.as_str())),
                ("size", Json::from(backup::humanise_bytes(bytes))),
                (
                    "when",
                    Json::from(tokens::humanise(&row.get::<String>("created_at").unwrap_or_default())),
                ),
                ("status", Json::from(status.as_str())),
                (
                    "status_label",
                    Json::from(match status.as_str() {
                        "ready" => "Ready",
                        "running" => "Running",
                        _ => "Failed",
                    }),
                ),
                // The badge class rather than three `@if`s in the template: the
                // status and the colour that means it belong in one place.
                (
                    "status_class",
                    Json::from(match status.as_str() {
                        "ready" => "badge-success",
                        "running" => "badge-warning",
                        _ => "badge-danger",
                    }),
                ),
                ("ready", Json::from(ready)),
                ("note", row.get::<String>("note").map(Json::from).unwrap_or(Json::Null)),
            ]));
        }

        Ok(context
            .with("q", Json::from(search.as_str()))
            .with("backups_empty", Json::from(rows.is_empty()))
            .with("backups", Json::Array(rows)))
    }

    /// Take a backup.
    ///
    /// The row goes in first, as `running`, and is corrected afterwards. Doing
    /// it the other way round — write the file, then record it — means a crash
    /// between the two leaves a file nothing knows about, which is the failure
    /// nobody notices. This way a crash leaves a row that says `running`
    /// forever, which is visible on the page and cannot be restored from.
    pub async fn store(req: Request) -> Result<Response> {
        if !req.can("backups.create").await? {
            return Ok(forbidden());
        }
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let me = req.identity().and_then(|id| id.id_as::<i64>());

        let at = tokens::now();
        let name = backup::name_for(&at);
        let destination = backup::path_for(&name)?;
        let schema = backup::schema_version(&db).await?;

        if db.table("backups").filter("name", name.as_str()).exists(&db).await? {
            // Two clicks in the same second. The unique index would refuse the
            // insert anyway; this turns that into a sentence.
            page::flash(&req, "warning", "A backup was taken a moment ago. Try again in a second.");
            return Ok(Response::see_other(BACK));
        }

        let id = db
            .table("backups")
            .insert(
                &db,
                &[
                    ("name", name.as_str().into()),
                    ("path", destination.display().to_string().into()),
                    ("bytes", 0.into()),
                    ("status", "running".into()),
                    ("created_by", me.into()),
                    ("created_at", at.as_str().into()),
                    ("updated_at", at.as_str().into()),
                ],
            )
            .await?;

        if let Some(audit) = crate::support::audit::of(&req, "backups.created") {
            audit.on("Backup", id).describe(format!("Took the backup {name}")).record().await;
        }

        let header = backup::Header {
            format: backup::FORMAT,
            schema,
            at: at.clone(),
            app: req.config().string("app.name", "Rustlavel"),
        };
        let names = backup::tables(Some(&rbac(&req)?));

        match backup::write(&db, &names, &header, &destination).await {
            Ok(bytes) => {
                // Only now. Everything above this line could have failed with a
                // file on disk; nothing below it can.
                db.table("backups")
                    .filter("id", id)
                    .update(
                        &db,
                        &[
                            ("bytes", (bytes as i64).into()),
                            ("status", "ready".into()),
                            ("updated_at", tokens::now().into()),
                        ],
                    )
                    .await?;
                page::flash(
                    &req,
                    "success",
                    format!("Backup {name} is ready ({}).", backup::humanise_bytes(bytes as i64)),
                );
            }
            Err(error) => {
                error!("the backup {name} failed: {error}");
                db.table("backups")
                    .filter("id", id)
                    .update(
                        &db,
                        &[
                            ("status", "failed".into()),
                            ("note", error.to_string().into()),
                            ("updated_at", tokens::now().into()),
                        ],
                    )
                    .await?;
                page::flash(&req, "error", format!("The backup failed: {error}"));
            }
        }

        Ok(Response::see_other(BACK))
    }

    /// Send the file.
    ///
    /// The path is rebuilt from the row's `name` with `backup::path_for`, and
    /// never taken from the row's `path` column. The two agree today, but only
    /// one of them is validated, and the day somebody writes to that column by
    /// hand — a fixture, a migration, a support script — is the day the
    /// unvalidated one starts serving `../../.env`.
    pub async fn download(req: Request) -> Result<Response> {
        if !req.can("backups.view").await? {
            return Ok(forbidden());
        }
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let Some((name, ready)) = Self::locate(&db, &req).await? else {
            return Ok(Response::not_found());
        };
        if !ready {
            page::flash(&req, "error", "That backup did not finish, so there is nothing to send.");
            return Ok(Response::see_other(BACK));
        }

        let path = backup::path_for(&name)?;
        let Ok(body) = rustlavel::tokio::fs::read(&path).await else {
            page::flash(&req, "error", format!("The file for {name} is no longer on disk."));
            return Ok(Response::see_other(BACK));
        };

        // The whole file, in memory, once: `Response` holds its body as a
        // `Vec<u8>` and the framework has no streaming body yet, so calling
        // this "streaming" would be a lie. It is the honest limit on how large
        // a backup this button can hand back.
        //
        // `name` has already been through `valid_name`, so it is letters,
        // digits, hyphens and underscores — nothing that could close the quote
        // in the header or smuggle a second one.
        Ok(Response::ok()
            .with_body(body)
            .with_header("content-type", "application/x-ndjson")
            .with_header("content-disposition", format!("attachment; filename=\"{name}.ndjson\""))
            // A dump is every row in the database. It must not sit in a proxy.
            .with_header("cache-control", "no-store, private"))
    }

    /// Put a backup back.
    ///
    /// The dangerous half. See [`backup::restore`] for exactly what it
    /// guarantees and what it does not — in particular that it is one
    /// transaction, and that it does not restore what it did not dump.
    pub async fn restore(req: Request) -> Result<Response> {
        if !req.can("backups.restore").await? {
            return Ok(forbidden());
        }
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let Some((name, ready)) = Self::locate(&db, &req).await? else {
            return Ok(Response::not_found());
        };
        if !ready {
            page::flash(&req, "error", "That backup did not finish and cannot be restored from.");
            return Ok(Response::see_other(BACK));
        }

        let path = backup::path_for(&name)?;
        let Ok(source) = rustlavel::tokio::fs::read_to_string(&path).await else {
            page::flash(&req, "error", format!("The file for {name} is no longer on disk."));
            return Ok(Response::see_other(BACK));
        };

        // Three refusals before a single row is touched: the file must parse
        // and carry its end marker, the schema it was taken from must be the
        // schema in front of us, and every table it names must be one this
        // application dumps.
        let dump = match backup::parse(&source) {
            Ok(dump) => dump,
            Err(error) => {
                page::flash(&req, "error", format!("{name} cannot be restored: {error}"));
                return Ok(Response::see_other(BACK));
            }
        };

        let current = backup::schema_version(&db).await?;
        if dump.header.schema != current {
            page::flash(
                &req,
                "error",
                format!(
                    "{name} was taken from schema {} and this database is at {current}. \
                     Restoring rows into a different shape is how a database ends up with \
                     columns full of the wrong thing, so it is refused.",
                    dump.header.schema
                ),
            );
            return Ok(Response::see_other(BACK));
        }

        let names = backup::tables(Some(&rbac(&req)?));
        match backup::restore(&db, &names, &dump).await {
            Ok(done) => {
                warn!("the database was restored from the backup {name} by user {:?}", req.identity().and_then(|id| id.id_as::<i64>()));
                // **The settings cache is now stale.** A restore writes to
                // the database without going through the save path that
                // invalidates it, so the process keeps serving the values it
                // had — including the generated stylesheet. Somebody
                // restoring a backup to undo a bad change would reload the
                // page, see the bad change still there, and reasonably
                // conclude the restore had not worked.
                if let Some(settings) = req.state::<crate::support::settings::Settings>() {
                    settings.forget();
                }

                // The single most consequential thing anybody can do from this
                // application: it replaces every account in it. If one entry
                // in the trail matters, it is this one.
                if let Some(audit) = crate::support::audit::of(&req, "backups.restored") {
                    audit
                        .on("Backup", name.as_str())
                        .describe(format!("Restored the database from {name}"))
                        .with("rows", Json::from(done.rows as i64))
                        .with("tables", Json::from(done.tables as i64))
                        .record()
                        .await;
                }
                page::flash(
                    &req,
                    "success",
                    format!(
                        "Restored {} rows across {} tables from {name}. Everyone signed in \
                         before this may need to sign in again.",
                        done.rows, done.tables
                    ),
                );
            }
            Err(error) => {
                // The transaction rolled back, so the database is what it was.
                // Saying so is the point: a half-restored database that reports
                // success is the worst outcome this page has.
                error!("restoring from {name} failed: {error}");
                page::flash(
                    &req,
                    "error",
                    format!(
                        "The restore failed and was rolled back, so nothing changed: {error}"
                    ),
                );
            }
        }

        Ok(Response::see_other(BACK))
    }

    /// Forget a backup: the row and the file.
    pub async fn destroy(req: Request) -> Result<Response> {
        if !req.can("backups.delete").await? {
            return Ok(forbidden());
        }
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let id = req.param_as::<i64>("id").unwrap_or_default();
        let Some((name, _)) = Self::locate(&db, &req).await? else {
            return Ok(Response::not_found());
        };

        // Same rule as the download: the path is derived from the validated
        // name, so the only file this can ever unlink is one inside
        // `storage/backups`.
        let path = backup::path_for(&name)?;
        let _ = rustlavel::tokio::fs::remove_file(&path).await;
        // The file first, then the row. A row without its file is a visible
        // "no longer on disk"; a file without its row is invisible and stays
        // there forever.
        db.table("backups").filter("id", id).delete(&db).await?;

        if let Some(audit) = crate::support::audit::of(&req, "backups.deleted") {
            audit.on("Backup", name.as_str()).describe(format!("Deleted the backup {name}")).record().await;
        }
        page::flash(&req, "warning", format!("Backup {name} has been deleted."));
        Ok(Response::see_other(BACK))
    }

    /// The row behind an `{id}` in the URL: its name and whether it finished.
    ///
    /// Every action goes through here, so the id is looked up exactly once and
    /// the name that comes back is one this application wrote.
    async fn locate(db: &Database, req: &Request) -> Result<Option<(String, bool)>> {
        let id = req.param_as::<i64>("id").unwrap_or_default();
        let Some(row) = db.table("backups").filter("id", id).first(db).await? else {
            return Ok(None);
        };
        let name = row.get::<String>("name").unwrap_or_default();
        if !backup::valid_name(&name) {
            // A row whose name would not pass validation cannot have been
            // written by this code. Refuse it rather than repair it.
            warn!("the backup row {id} has a name this application would not have written");
            return Ok(None);
        }
        Ok(Some((name, row.get::<String>("status").unwrap_or_default() == "ready")))
    }
}

/// Where every action returns to: the tab it was clicked on.
const BACK: &str = "/admin/settings/backup";

/// A refusal, for the check inside the handler.
///
/// The routes carry a `Can` guard as well, and that is the one that normally
/// answers. This is the second lock: a route registered without its guard, or
/// moved into another group, should fail closed rather than quietly become
/// public.
fn forbidden() -> Response {
    Response::new(rustlavel::Status::FORBIDDEN)
        .with_html("<h1>403</h1><p>You do not have permission to manage backups.</p>")
}
