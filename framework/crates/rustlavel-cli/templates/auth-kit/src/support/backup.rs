//! Logical database backups: writing one, reading one back, and the small
//! amount of path arithmetic that keeps both inside `storage/backups`.
//!
//! **Why not `pg_dump`.** It exists, it is excellent, and it is the wrong
//! answer here. It speaks to PostgreSQL only, and this framework supports
//! three databases; it is a separate binary, and requiring one on the
//! application host contradicts the single-file deploy the project already
//! ships; and shelling out means the backup's correctness depends on a version
//! of a program nobody in this repository chose. So the dump is written here,
//! through the query builder, and works the same on all three.
//!
//! **The format is newline-delimited JSON**, one object per line. Every line is
//! a complete JSON object with a single key naming what it is:
//!
//! ```text
//! {"app":"Rustlavel","at":"2026-09-03 10:11:12","backup":1,"schema":"6:2026_09_03_000100_create_settings_table"}
//! {"columns":["id","name","email","created_at"],"table":"users"}
//! {"row":[1,"Ada Lovelace","ada@example.com","2026-09-01 09:00:00"]}
//! ```
//!
//! and the last line is `{"end":{"rows":428,"tables":11}}`. Keys come out in
//! alphabetical order because `Json` keeps an object in a `BTreeMap`, which is
//! what makes two dumps of the same rows byte-identical and therefore
//! diffable.
//!
//! Four things earned the choice. It needs no SQL quoting rules, and those are
//! exactly what differs between the three databases — a dump of SQL statements
//! would have to know which one wrote it. It streams: a line is written and
//! forgotten, so a table larger than memory costs no more than a table smaller
//! than it. Anything can read it — `jq`, a spreadsheet, twenty lines of Python
//! — which matters when the thing you need a backup for is the application
//! being broken. And the trailer is what makes a truncated file *detectable*:
//! see [`parse`].
//!
//! A row's payload is a positional array rather than an object keyed by column
//! name. The header line two lines above already names the columns in order,
//! and repeating those names on every row of a million-row table triples the
//! file for no information. The cost is that a row line cannot be read without
//! its table header, which is fine: nothing reads one without the other.

use rustlavel::prelude::*;
use rustlavel::rbac::Permissions;
use rustlavel::tokio::io::AsyncWriteExt;
use std::path::{Path, PathBuf};

/// Where the files live. Relative to the working directory, like
/// `storage/sessions`.
pub const DIRECTORY: &str = "storage/backups";

/// The format version, written into every header and checked on the way back.
pub const FORMAT: i64 = 1;

/// Rows read from the database in one go while dumping.
///
/// The point of a chunk is that neither the driver nor this process ever holds
/// a whole table. Five hundred rows is small enough that a wide table still
/// fits comfortably and large enough that the per-query overhead disappears.
const CHUNK: i64 = 500;

/// The application's own tables, parents before the tables that point at them.
///
/// Written out rather than discovered, and the reason is worth stating because
/// both alternatives look more clever.
///
/// `information_schema.tables` would find them, but it answers with a *set*,
/// and a restore needs an *order* — insert `user_tokens` before `users` and a
/// foreign key rejects every row. It would also sweep up tables this
/// application does not own and must not restore: the migration ledger, whose
/// job is to describe the schema rather than be overwritten by an older
/// description of it, and whichever tables a plugin happens to have created.
/// And scoping it to "this database" is spelled three different ways across
/// PostgreSQL, MySQL and SQL Server, which is precisely the kind of knowledge
/// that belongs in a dialect and not in an application.
///
/// The migration registry has the right order but the wrong granularity: it
/// lists migrations, not tables, and one migration here creates two.
///
/// So: a list, in the order the migrations create them. Add a table, add a
/// line — the same one-line cost as adding it to `CATALOGUE` in `settings.rs`.
/// **Every table this application's own migrations create.**
///
/// Kept in step by a test that reads `database/migrations/` and fails on
/// anything created there and missing here — three tables were added in one
/// afternoon and none of them reached this list, so a "database backup"
/// silently omitted the navigation menus, the whole audit trail and the
/// password history. A hand-kept list of what to back up is a list that goes
/// stale exactly when it matters.
///
/// It still cannot cover a table the *developer* adds to their own
/// application; see [`tables`].
const OWN_TABLES: &[&str] = &[
    "users",
    "user_tokens",
    "login_attempts",
    "user_totp",
    "user_passkeys",
    "user_recovery_codes",
    "settings",
    "menu_items",
    "audit_logs",
    "password_history",
    // `backups` is deliberately absent. A dump that included it would record
    // the very row describing itself, in the `running` state it was in
    // half-way through being written, and restoring that would resurrect
    // catalogue entries for files that are no longer on disk. The list of
    // backups describes the backups; it is not part of what they contain.
];

/// Every table a dump covers, in insert order.
///
/// The roles and permissions live in `rustlavel-rbac`'s tables, whose names are
/// configurable, so they are asked for rather than assumed. They come last
/// because `user_role` holds user ids.
///
/// **This covers what the starter kit created, and nothing else.** A table you
/// add to your own application is not in it, and a dump taken from this screen
/// is therefore a backup of the kit rather than of the database. Add the name
/// here — the list is a `Vec` so that an application can extend it — or use
/// `pg_dump` for the real thing. Discovering the tables from the database
/// itself would be better, and needs a schema listing this framework does not
/// have yet.
pub fn tables(store: Option<&Permissions>) -> Vec<String> {
    let mut names: Vec<String> = OWN_TABLES.iter().map(|name| name.to_string()).collect();
    if let Some(store) = store {
        names.extend(store.tables().all().iter().map(|name| name.to_string()));
    }
    names
}

/// The first line of a file.
#[derive(Debug, Clone, PartialEq)]
pub struct Header {
    pub format: i64,
    /// What the database looked like when this was taken. See
    /// [`schema_version`].
    pub schema: String,
    /// `YYYY-MM-DD HH:MM:SS`, UTC, the same shape everything else here stores.
    pub at: String,
    pub app: String,
}

impl Header {
    fn to_line(&self) -> String {
        Json::object([
            ("backup", Json::from(self.format)),
            ("schema", Json::from(self.schema.as_str())),
            ("at", Json::from(self.at.as_str())),
            ("app", Json::from(self.app.as_str())),
        ])
        .to_string()
    }

    fn from_json(value: &Json) -> Option<Header> {
        Some(Header {
            format: value.get("backup")?.as_f64()? as i64,
            schema: value.get("schema")?.as_str()?.to_string(),
            at: value.get("at").and_then(Json::as_str).unwrap_or_default().to_string(),
            app: value.get("app").and_then(Json::as_str).unwrap_or_default().to_string(),
        })
    }
}

/// One table's worth of a parsed file.
#[derive(Debug, Clone, PartialEq)]
pub struct Section {
    pub name: String,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
}

/// A whole file, read back.
#[derive(Debug, Clone, PartialEq)]
pub struct Dump {
    pub header: Header,
    pub sections: Vec<Section>,
}

impl Dump {
    pub fn rows(&self) -> usize {
        self.sections.iter().map(|section| section.rows.len()).sum()
    }
}

/// What a restore actually did, so the page can say so rather than say "done".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Restored {
    pub tables: usize,
    pub rows: usize,
}

// --- The format ----------------------------------------------------------

/// One database value as it is written to the file.
///
/// Null, booleans, numbers and strings go over as themselves. The two that
/// cannot are tagged with a key no column value can produce on its own, since
/// a bare JSON object is not a shape this encoder ever emits for a scalar:
/// binary as `{"b64": …}` and a `json`/`jsonb` column as `{"json": …}` — the
/// second so a text column holding `{"a":1}` and a JSON column holding the same
/// thing do not read back as each other.
///
/// Integers are written as JSON numbers, which means the usual JSON limit
/// applies: an integer beyond 2^53 cannot be represented exactly, and this
/// encoder is honest about it rather than silently rounding — see
/// [`decode_value`]. Every id in this application is a `bigserial` counting up
/// from one, so the limit is theoretical here; it would not be for a snowflake
/// id, and a project that adopts one should tag integers as strings.
pub fn encode_value(value: &Value) -> Json {
    match value {
        Value::Null => Json::Null,
        Value::Bool(flag) => Json::Bool(*flag),
        Value::Int(number) => Json::Number(*number as f64),
        Value::Float(number) => Json::Number(*number),
        Value::Text(text) => Json::String(text.clone()),
        Value::Json(inner) => Json::object([("json", inner.clone())]),
        Value::Bytes(bytes) => {
            Json::object([("b64", Json::from(rustlavel::db::base64::encode(bytes)))])
        }
    }
}

/// The inverse.
///
/// A JSON number with no fractional part comes back as an integer, which
/// conflates `Int(1)` with `Float(1.0)`. That conflation is harmless: the value
/// is bound as a parameter and the column's own type decides what it becomes,
/// and every one of the three databases accepts `1` into a `double precision`
/// column. The reverse — a fraction turning into an integer — cannot happen.
pub fn decode_value(value: &Json) -> Result<Value> {
    Ok(match value {
        Json::Null => Value::Null,
        Json::Bool(flag) => Value::Bool(*flag),
        Json::Number(number) => {
            if number.fract() == 0.0 && number.abs() < 9.007e15 {
                Value::Int(*number as i64)
            } else {
                Value::Float(*number)
            }
        }
        Json::String(text) => Value::Text(text.clone()),
        Json::Object(fields) => {
            if let Some(encoded) = fields.get("b64").and_then(Json::as_str) {
                let bytes = rustlavel::db::base64::decode(encoded).ok_or_else(|| {
                    Error::msg("a binary value in the backup is not valid base64")
                })?;
                Value::Bytes(bytes)
            } else if let Some(inner) = fields.get("json") {
                Value::Json(inner.clone())
            } else {
                return Err(Error::msg(
                    "a value in the backup is an object with no `b64` or `json` key, so this \
                     file was not written by a version of the backup format this build knows",
                ));
            }
        }
        Json::Array(_) => {
            return Err(Error::msg("a value in the backup is an array, which no column holds"));
        }
    })
}

fn table_line(name: &str, columns: &[String]) -> String {
    Json::object([
        ("table", Json::from(name)),
        (
            "columns",
            Json::Array(columns.iter().map(|column| Json::from(column.as_str())).collect()),
        ),
    ])
    .to_string()
}

fn row_line(values: &[Value]) -> String {
    Json::object([("row", Json::Array(values.iter().map(encode_value).collect()))]).to_string()
}

fn trailer_line(tables: usize, rows: usize) -> String {
    Json::object([(
        "end",
        Json::object([("tables", Json::from(tables as i64)), ("rows", Json::from(rows as i64))]),
    )])
    .to_string()
}

/// Read a whole file back.
///
/// **A file with no trailer is rejected**, and that is the single most
/// important line in this module. A dump that died half-way — disk full, the
/// process killed, the connection dropped — leaves a file that looks perfectly
/// well-formed right up to where it stops, and every row in it is real. Reading
/// that back would restore a database silently missing whatever came after the
/// cut, which is worse than restoring nothing at all, because nothing about the
/// result announces itself as wrong. The counts in the trailer are checked too,
/// so a file truncated and then somehow re-terminated still fails.
pub fn parse(source: &str) -> Result<Dump> {
    let mut lines = source.lines().enumerate().filter(|(_, line)| !line.trim().is_empty());

    let (_, first) = lines
        .next()
        .ok_or_else(|| Error::msg("this backup file is empty"))?;
    let header = Header::from_json(&Json::parse(first).map_err(|error| {
        Error::msg(format!("the first line of this backup is not JSON: {error}"))
    })?)
    .ok_or_else(|| {
        Error::msg("the first line of this backup is not a header, so it is not a backup file")
    })?;

    if header.format != FORMAT {
        return Err(Error::msg(format!(
            "this backup is in format {} and this build writes and reads format {FORMAT}",
            header.format
        )));
    }

    let mut sections: Vec<Section> = Vec::new();
    let mut trailer: Option<(usize, usize)> = None;

    for (index, line) in lines {
        let number = index + 1;
        if trailer.is_some() {
            return Err(Error::msg(format!(
                "line {number} of this backup comes after the end marker, so the file has been \
                 appended to or two files have been concatenated"
            )));
        }

        let value = Json::parse(line)
            .map_err(|error| Error::msg(format!("line {number} of this backup is not JSON: {error}")))?;

        if let Some(name) = value.get("table").and_then(Json::as_str) {
            let columns = value
                .get("columns")
                .and_then(Json::as_array)
                .ok_or_else(|| {
                    Error::msg(format!("the table on line {number} does not name its columns"))
                })?
                .iter()
                .map(|column| {
                    column
                        .as_str()
                        .map(str::to_string)
                        .ok_or_else(|| Error::msg(format!("a column name on line {number} is not text")))
                })
                .collect::<Result<Vec<String>>>()?;
            sections.push(Section { name: name.to_string(), columns, rows: Vec::new() });
        } else if let Some(values) = value.get("row").and_then(Json::as_array) {
            let section = sections.last_mut().ok_or_else(|| {
                Error::msg(format!("line {number} is a row before any table has been named"))
            })?;
            if values.len() != section.columns.len() {
                return Err(Error::msg(format!(
                    "line {number} has {} values but `{}` was declared with {} columns",
                    values.len(),
                    section.name,
                    section.columns.len()
                )));
            }
            section.rows.push(values.iter().map(decode_value).collect::<Result<Vec<Value>>>()?);
        } else if let Some(end) = value.get("end") {
            trailer = Some((
                end.get("tables").and_then(Json::as_f64).unwrap_or(-1.0) as usize,
                end.get("rows").and_then(Json::as_f64).unwrap_or(-1.0) as usize,
            ));
        } else {
            return Err(Error::msg(format!(
                "line {number} of this backup is not a table, a row or the end marker"
            )));
        }
    }

    let dump = Dump { header, sections };
    let Some((tables, rows)) = trailer else {
        return Err(Error::msg(
            "this backup has no end marker, so it was never finished — the process that wrote it \
             stopped part-way and the file is missing everything that came after. It cannot be \
             restored from.",
        ));
    };
    if tables != dump.sections.len() || rows != dump.rows() {
        return Err(Error::msg(format!(
            "this backup says it holds {tables} tables and {rows} rows but {} tables and {} rows \
             are actually in the file, so it has been truncated or edited",
            dump.sections.len(),
            dump.rows()
        )));
    }
    Ok(dump)
}

// --- Where the files live ------------------------------------------------

/// Whether a name may be turned into a path.
///
/// Letters, digits, hyphen and underscore, and nothing else — no dot, no
/// slash, no separator of any kind on any platform. A name like `../../.env`
/// or `..\\..\\.env` therefore never gets as far as being joined to anything.
///
/// This is stricter than it needs to be today, because today the only names
/// that exist are timestamps this code generated. It is written as a rule
/// rather than as a comment on the generator because the day somebody adds a
/// "name your backup" field is the day the generator stops being the only
/// source, and the check has to already be here for that to be safe.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 100
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// The file a name refers to, or a refusal.
///
/// Belt and braces: the name is validated, joined, and then the result is
/// checked to be a direct child of the directory. The second check is what
/// survives somebody loosening the first.
pub fn path_for(name: &str) -> Result<PathBuf> {
    if !valid_name(name) {
        return Err(Error::msg(format!(
            "`{name}` is not a backup name. A name is letters, digits, hyphens and underscores, \
             which is what stops one from pointing at a file outside {DIRECTORY}."
        )));
    }
    let directory = Path::new(DIRECTORY);
    let path = directory.join(format!("{name}.ndjson"));
    if path.parent() != Some(directory) {
        return Err(Error::msg(format!("`{name}` does not resolve inside {DIRECTORY}")));
    }
    Ok(path)
}

/// The name a backup taken at `at` is filed under: `2026-09-03-101112`.
///
/// The timestamp is the name, so the directory sorts into the order the
/// backups were taken and two taken in the same second cannot collide silently
/// — the `name` column is unique, so the second one is refused.
pub fn name_for(at: &str) -> String {
    at.chars().filter(|c| c.is_ascii_digit()).enumerate().fold(
        String::with_capacity(18),
        |mut name, (index, digit)| {
            if index == 4 || index == 6 {
                name.push('-');
            }
            if index == 8 {
                name.push('-');
            }
            name.push(digit);
            name
        },
    )
}

/// What the database's schema looks like right now.
///
/// The count of applied migrations and the newest one's name. The name alone
/// would call two differently-migrated databases equal whenever the newest
/// migration happened to match — a rollback followed by a different forward
/// step does exactly that — and the count catches it for the price of nothing.
pub async fn schema_version(db: &Database) -> Result<String> {
    let table = rustlavel::db::migration::DEFAULT_TABLE;
    let applied = db.table(table).count(db).await?;
    let newest = db
        .table(table)
        .select(&["name"])
        .order_by("id", rustlavel::db::Direction::Desc)
        .limit(1)
        .first(db)
        .await?
        .and_then(|row| row.get::<String>("name").ok())
        .unwrap_or_else(|| "none".to_string());
    Ok(format!("{applied}:{newest}"))
}

// --- Writing one ---------------------------------------------------------

/// Write a dump and return how many bytes it came to.
///
/// The file is written to `<name>.ndjson.part` and renamed only once the
/// trailer is on disk and the handle is flushed. Between the rename being
/// atomic and the trailer being required by [`parse`], there are two
/// independent reasons a half-written dump cannot be mistaken for a whole one:
/// it is not at the name a restore looks for, and it would be rejected if it
/// were. The caller adds a third by leaving the row's status at `running`
/// until this returns.
pub async fn write(
    db: &Database,
    names: &[String],
    header: &Header,
    destination: &Path,
) -> Result<u64> {
    if let Some(parent) = destination.parent() {
        rustlavel::tokio::fs::create_dir_all(parent).await?;
    }
    let partial = PathBuf::from(format!("{}.part", destination.display()));

    let mut file = rustlavel::tokio::io::BufWriter::new(rustlavel::tokio::fs::File::create(&partial).await?);
    let mut written_tables = 0usize;
    let mut written_rows = 0usize;

    // Anything that goes wrong from here leaves the `.part` file behind rather
    // than at the real name, so a later run overwrites it and nothing ever
    // reads it.
    let outcome = async {
        file.write_all(header.to_line().as_bytes()).await?;
        file.write_all(b"\n").await?;

        for name in names {
            // One row, to learn the columns before any page is read.
            //
            // The ordering has to be known for the *first* query, not the
            // second: pages ordered differently from one another overlap and
            // skip, which is the failure a backup can least afford because
            // nothing about the file says it happened.
            let probe = db.table(name).limit(1).get(db).await?;
            let ordering: Vec<String> =
                probe.first().map(|row| row.columns().to_vec()).unwrap_or_default();

            let mut offset = 0i64;
            let mut columns: Option<Vec<String>> = None;

            // An empty table skips the pages and falls through to the header
            // written below, which is what tells a restore to empty it.
            while !ordering.is_empty() {
                // Ordered so the pages do not overlap or skip: an unordered
                // `offset` is only as stable as the database feels like being,
                // which is not a property to build a backup on.
                //
                // **Ordered by every column, not by `id`.** A pivot table has
                // no surrogate key — `user_role` and `user_permission` in this
                // application's own RBAC tables do not have one — and so does
                // any table a developer gives a composite key. Ordering by `id`
                // made those a hard failure on the first page: the backup
                // reported the exact SQL and stopped, which is the right way to
                // fail and still the wrong thing to have failed at.
                let mut query = db.table(name).limit(CHUNK).offset(offset);
                for column in ordering.iter() {
                    query = query.order_by(column, rustlavel::db::Direction::Asc);
                }
                let rows = query.get(db).await?;
                if rows.is_empty() {
                    break;
                }

                if columns.is_none() {
                    let names: Vec<String> = rows[0].columns().to_vec();
                    file.write_all(table_line(name, &names).as_bytes()).await?;
                    file.write_all(b"\n").await?;
                    written_tables += 1;
                    columns = Some(names);
                }

                for row in &rows {
                    let values: Vec<Value> = (0..row.len())
                        .map(|index| row.value_at(index).cloned().unwrap_or(Value::Null))
                        .collect();
                    file.write_all(row_line(&values).as_bytes()).await?;
                    file.write_all(b"\n").await?;
                    written_rows += 1;
                }

                if (rows.len() as i64) < CHUNK {
                    break;
                }
                offset += CHUNK;
            }

            // An empty table still gets a header, with no columns. The header
            // is what tells a restore to empty the table rather than leave
            // whatever is in it now, and with no rows to write there is nothing
            // for a column list to describe.
            if columns.is_none() {
                file.write_all(table_line(name, &[]).as_bytes()).await?;
                file.write_all(b"\n").await?;
                written_tables += 1;
            }
        }

        file.write_all(trailer_line(written_tables, written_rows).as_bytes()).await?;
        file.write_all(b"\n").await?;
        file.flush().await?;
        // `flush` empties this process's buffer into the kernel's. `sync_all`
        // is what gets it onto the disk, and a backup that only exists in a
        // page cache is not a backup.
        file.into_inner().sync_all().await?;
        Ok::<(), Error>(())
    }
    .await;

    if let Err(error) = outcome {
        let _ = rustlavel::tokio::fs::remove_file(&partial).await;
        return Err(error);
    }

    let bytes = rustlavel::tokio::fs::metadata(&partial).await?.len();
    rustlavel::tokio::fs::rename(&partial, destination).await?;
    Ok(bytes)
}

// --- Reading one back ----------------------------------------------------

/// Put a dump back into the database.
///
/// **What this guarantees.** The file's schema version must equal the
/// database's, so a dump taken before a migration cannot be poured into the
/// shape that migration produced. The file must carry its end marker and its
/// counts must agree, so a dump that died half-way is refused rather than
/// half-applied — see [`parse`]. Every table and column name in the file is
/// checked against the list this application dumps and against
/// `validate_identifier` before it reaches a statement, because those are the
/// only two strings here that cannot be bound as parameters. And the whole
/// thing runs in one transaction: each table is emptied and refilled, and if
/// anything fails the transaction is rolled back, so the database is either
/// entirely the backup or entirely what it was.
///
/// **What this does not guarantee, said plainly.**
///
/// * It does not restore what it did not dump. Sessions on disk, the migration
///   ledger, and the `backups` catalogue itself are untouched, so people are
///   signed out into a database that no longer knows them and the backup list
///   still names files that may no longer exist.
/// * It does not reset an identity sequence. On PostgreSQL the `bigserial`
///   sequence keeps whatever value it had, so the next insert can collide with
///   a restored id; `setval` is PostgreSQL's spelling of the fix and this file
///   is deliberately dialect-agnostic, so it is left to the operator.
/// * **It does not work on SQL Server as written.** Restoring rows means
///   writing their ids back, and SQL Server refuses an explicit value for an
///   `identity` column without `SET IDENTITY_INSERT … ON` around the insert.
///   That is a dialect's knowledge, not an application's, and it does not
///   belong in this file. The restore will fail loudly there, and roll back,
///   rather than do something surprising.
/// * It is not a point-in-time restore. The dump is taken table by table
///   without a snapshot, so a write that lands between two tables being read is
///   in one and not the other. For a backup taken from a live application this
///   is the honest description; taking one from a quiet moment is the fix.
/// * It does not stream. Writing a dump does — a line is written and forgotten
///   — but [`parse`] builds the whole file in memory before the first row is
///   inserted, because the counts in the trailer cannot be checked until the
///   trailer has been read, and checking them is the entire point. So a restore
///   costs roughly the size of the file in memory. Making it streaming means
///   giving up the truncation check or reading the file twice, and of those
///   three the current one is the only one that cannot quietly restore half a
///   database.
pub async fn restore(db: &Database, allowed: &[String], dump: &Dump) -> Result<Restored> {
    for section in &dump.sections {
        if !allowed.iter().any(|name| name == &section.name) {
            return Err(Error::msg(format!(
                "this backup contains a table called `{}`, which this application does not \
                 back up and will not write to",
                section.name
            )));
        }
        rustlavel::db::validate_identifier(&section.name)?;
        for column in &section.columns {
            rustlavel::db::validate_identifier(column)?;
        }
    }

    let mut transaction = db.begin().await?;
    let mut done = Restored::default();

    // Children first when emptying, parents first when filling: the reverse of
    // the order they were created in, then the order itself.
    for section in dump.sections.iter().rev() {
        transaction
            .execute(&format!("delete from {}", db.dialect().quote(&section.name)), &[])
            .await?;
    }

    for section in &dump.sections {
        if section.columns.is_empty() {
            done.tables += 1;
            continue;
        }
        let columns: Vec<String> =
            section.columns.iter().map(|column| db.dialect().quote(column)).collect();
        let placeholders: Vec<String> =
            (1..=section.columns.len()).map(|n| db.dialect().placeholder(n)).collect();
        let sql = format!(
            "insert into {} ({}) values ({})",
            db.dialect().quote(&section.name),
            columns.join(", "),
            placeholders.join(", ")
        );

        for row in &section.rows {
            transaction.execute(&sql, row).await?;
            done.rows += 1;
        }
        done.tables += 1;
    }

    transaction.commit().await?;
    Ok(done)
}

/// A size a person can read: `4.2 MB`.
pub fn humanise_bytes(bytes: i64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut size = bytes.max(0) as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{} B", bytes.max(0)) } else { format!("{size:.1} {}", UNITS[unit]) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory of this test's own.
    ///
    /// The suite runs concurrently, so a shared fixture directory would make
    /// two tests pass or fail depending on which finished first.
    fn scratch(name: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("rustlavel-backup-tests/{name}"));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("could not make the scratch directory");
        directory
    }

    /// Header, two tables, awkward values, trailer.
    fn sample() -> String {
        let header = Header {
            format: FORMAT,
            schema: "9:2026_09_03_000100_create_settings_table".into(),
            at: "2026-09-03 10:11:12".into(),
            app: "Bktest".into(),
        };
        let columns = vec!["id".to_string(), "name".to_string(), "note".to_string()];
        let rows = vec![
            vec![Value::Int(1), Value::Text("Ada".into()), Value::Null],
            vec![
                Value::Int(2),
                // A quote, a backslash and a newline: the three characters that
                // break every hand-rolled quoting scheme, which is the point of
                // not having one.
                Value::Text("she said \"hello\"\nand \\ left".into()),
                Value::Bool(false),
            ],
            vec![Value::Int(3), Value::Float(1.5), Value::Bytes(vec![0, 1, 254, 255])],
        ];

        let mut out = String::new();
        out.push_str(&header.to_line());
        out.push('\n');
        out.push_str(&table_line("users", &columns));
        out.push('\n');
        for row in &rows {
            out.push_str(&row_line(row));
            out.push('\n');
        }
        out.push_str(&table_line("settings", &columns));
        out.push('\n');
        out.push_str(&trailer_line(2, rows.len()));
        out.push('\n');
        out
    }

    #[test]
    fn the_format_round_trips_every_kind_of_value() {
        let dump = parse(&sample()).expect("the sample should parse");

        assert_eq!(dump.header.format, FORMAT);
        assert_eq!(dump.header.schema, "9:2026_09_03_000100_create_settings_table");
        assert_eq!(dump.sections.len(), 2);
        assert_eq!(dump.sections[0].name, "users");
        assert_eq!(dump.sections[0].columns, ["id", "name", "note"]);
        assert_eq!(dump.sections[1].rows.len(), 0, "the empty table has a header and no rows");

        let rows = &dump.sections[0].rows;
        assert_eq!(rows[0], [Value::Int(1), Value::Text("Ada".into()), Value::Null]);
        assert_eq!(rows[1][1], Value::Text("she said \"hello\"\nand \\ left".into()));
        assert_eq!(rows[1][2], Value::Bool(false));
        assert_eq!(rows[2][1], Value::Float(1.5));
        assert_eq!(rows[2][2], Value::Bytes(vec![0, 1, 254, 255]));
    }

    #[test]
    fn a_row_with_a_newline_in_it_stays_on_one_line() {
        // The format is newline-delimited, so a value holding a newline has to
        // be escaped or every reader loses count. JSON does this for us; the
        // test is here so that a future "optimisation" that stops using JSON
        // has to notice.
        let line = row_line(&[Value::Text("one\ntwo".into())]);
        assert_eq!(line.lines().count(), 1, "a newline escaped into the line: {line}");
    }

    #[test]
    fn a_truncated_file_is_refused_rather_than_half_restored() {
        let whole = sample();
        // Cut it where a dump that ran out of disk would stop: after a complete
        // row, with everything still valid JSON.
        let lines: Vec<&str> = whole.lines().collect();
        let cut = lines[..lines.len() - 2].join("\n");

        let error = parse(&cut).expect_err("a file with no end marker must not parse");
        let message = error.to_string();
        assert!(message.contains("never finished"), "unhelpful message: {message}");

        // And one that kept its trailer but lost a row in the middle.
        let mut kept: Vec<&str> = whole.lines().collect();
        kept.remove(3);
        let error = parse(&kept.join("\n")).expect_err("the counts must be checked too");
        assert!(error.to_string().contains("truncated or edited"), "{error}");
    }

    #[test]
    fn an_empty_or_foreign_file_is_refused() {
        assert!(parse("").is_err(), "an empty file is not a backup");
        assert!(parse("hello\n").is_err(), "text is not a backup");
        assert!(parse("{\"table\":\"users\"}\n").is_err(), "a file with no header is not a backup");

        let wrong = format!(
            "{}\n{}\n",
            Json::object([("backup", Json::from(99)), ("schema", Json::from("x"))]),
            trailer_line(0, 0)
        );
        let error = parse(&wrong).expect_err("a future format must be refused");
        assert!(error.to_string().contains("format 99"), "{error}");
    }

    #[test]
    fn a_name_cannot_escape_the_directory() {
        for crafted in [
            "../../../etc/passwd",
            "..",
            ".",
            "a/b",
            "a\\b",
            "/etc/passwd",
            ".hidden",
            "with space",
            "semi;colon",
            "",
        ] {
            assert!(!valid_name(crafted), "`{crafted}` was accepted as a name");
            let path = path_for(crafted);
            assert!(path.is_err(), "`{crafted}` produced a path: {:?}", path.ok());
        }

        // And the names that are actually used still work, inside the directory.
        let path = path_for("2026-09-03-101112").expect("a timestamp is a valid name");
        assert_eq!(path, Path::new(DIRECTORY).join("2026-09-03-101112.ndjson"));
        assert_eq!(path.parent(), Some(Path::new(DIRECTORY)));
        assert!(valid_name("nightly_2026"));
    }

    #[test]
    fn a_name_is_the_timestamp_with_its_punctuation_stripped() {
        assert_eq!(name_for("2026-09-03 10:11:12"), "2026-09-03-101112");
        assert!(valid_name(&name_for("2026-09-03 10:11:12")));
    }

    #[rustlavel::test]
    async fn the_size_recorded_is_the_size_on_disk() {
        // No database here, so the file is written the way `write` writes it
        // and then measured the way the controller measures it. What is being
        // pinned is that the number stored in `bytes` is the finished file's
        // length and not, say, the length before the trailer.
        let directory = scratch("size");
        let destination = directory.join("2026-09-03-101112.ndjson");
        let source = sample();

        rustlavel::tokio::fs::write(&destination, &source).await.expect("write");
        let bytes = rustlavel::tokio::fs::metadata(&destination).await.expect("stat").len();

        assert_eq!(bytes, source.len() as u64);
        assert!(bytes > 0);
        assert_eq!(humanise_bytes(bytes as i64), format!("{bytes} B"));
        assert_eq!(humanise_bytes(1536), "1.5 KB");
        assert_eq!(humanise_bytes(0), "0 B");

        // And the file that was written is one a restore would accept.
        let read = rustlavel::tokio::fs::read_to_string(&destination).await.expect("read");
        assert_eq!(parse(&read).expect("round trip").rows(), 3);

        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_partial_file_is_not_at_the_name_a_restore_looks_for() {
        // The other half of the "a dead backup must not look restorable" rule:
        // the file is written under `.part` and renamed at the end, so even a
        // reader that skipped the trailer check would not find it.
        let directory = scratch("partial");
        let destination = directory.join("2026-09-03-101112.ndjson");
        let partial = PathBuf::from(format!("{}.part", destination.display()));

        std::fs::write(&partial, "{\"backup\":1}\n").expect("write");
        assert!(!destination.exists(), "a half-written dump must not sit at the real name");
        assert!(!valid_name("2026-09-03-101112.ndjson.part"), "a `.part` name has a dot in it");

        std::fs::rename(&partial, &destination).expect("rename");
        assert!(destination.exists());

        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn the_backups_table_is_never_part_of_a_backup() {
        assert!(!OWN_TABLES.contains(&"backups"), "a dump must not contain its own catalogue");
        assert!(OWN_TABLES.contains(&"users"));
        // Parents before children, so a restore's inserts satisfy the keys.
        let users = OWN_TABLES.iter().position(|t| *t == "users").unwrap();
        let tokens = OWN_TABLES.iter().position(|t| *t == "user_tokens").unwrap();
        assert!(users < tokens, "user_tokens points at users and must be filled after it");
    }
}
