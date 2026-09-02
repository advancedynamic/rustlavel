//! The `rustlavel` binary — the artisan of rustlavel.
//!
//! Commands that only touch files (`new`, `make:*`) run here. Commands that
//! need the application itself (`route:list`, and later `migrate`, `db:seed`,
//! `queue:work`) are forwarded to the project's own binary, because in a
//! compiled language the application is the only thing that knows its routes.

mod auth_kit;
mod build;
mod console;
mod database;
mod doctor;
mod make;
mod naming;
mod new;
mod project;
mod serve;
mod storage;
mod stubs;

use project::Project;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (command, rest) = match args.split_first() {
        Some((command, rest)) => (command.as_str(), rest),
        None => {
            help();
            return;
        }
    };

    let result = match command {
        "new" => new::run(rest),
        "serve" => serve::run(rest),
        "doctor" => doctor::run(),
        "build" => build::run(rest),
        "make:docker" => Project::discover().and_then(|p| build::make_docker(&p)),
        "storage:link" => Project::discover().and_then(|p| storage::link(&p, rest)),
        "key:generate" => key_generate(),
        "help" | "--help" | "-h" => {
            help();
            Ok(())
        }
        "version" | "--version" | "-V" => {
            println!("rustlavel {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        generator if generator.starts_with("make:") => make::run(generator, rest),
        forwarded => forward(forwarded, rest),
    };

    if let Err(message) = result {
        console::error(&message);
        std::process::exit(1);
    }
}

/// Hand a command to the application binary.
fn forward(command: &str, args: &[String]) -> Result<(), String> {
    let project = Project::discover()?;

    let status = std::process::Command::new("cargo")
        .arg("run")
        .arg("--quiet")
        .arg("--bin")
        .arg(&project.crate_name)
        .arg("--")
        .arg(command)
        .args(args)
        .current_dir(&project.root)
        .status()
        .map_err(|e| format!("cannot run cargo: {e}"))?;

    match status.code() {
        Some(0) | None => Ok(()),
        Some(code) => std::process::exit(code),
    }
}

fn help() {
    println!(
        "\n{} {}\n",
        console::bold("rustlavel"),
        console::dim(env!("CARGO_PKG_VERSION"))
    );
    println!("{}", console::bold("USAGE"));
    println!("  rustlavel <command> [options]\n");

    println!("{}", console::bold("APPLICATION"));
    row("new <name>", "Create a new application");
    row("  --with <pkgs>", "Enable packages: db, view, auth, cache, ai, …");
    row("serve", "Run the app, reloading when files change");
    row("route:list", "Show the registered routes");
    row("migrate", "Run pending migrations");
    row("migrate:rollback", "Undo the last batch of migrations");
    row("migrate:fresh", "Drop every table and migrate from scratch");
    row("migrate:status", "Show which migrations have run");
    row("db:seed", "Run the seeders");
    row("queue:work", "Process background jobs");
    row("queue:failed", "List jobs that gave up");
    row("schedule:run", "Run the scheduler");
    row("doctor", "Diagnose why the app will not start");
    row("build", "Build the single deployable binary");
    row("key:generate", "Generate APP_KEY into .env");
    row("storage:link", "Link public/storage to storage/app/public");
    println!();

    println!("{}", console::bold("GENERATORS"));
    row("make:controller <Name>", "Create a controller");
    row("make:middleware <name>", "Create a middleware function");
    row("make:model <Name>", "Create a model");
    row("make:migration <name>", "Create a migration");
    row("make:seeder <Name>", "Create a seeder");
    row("make:job <Name>", "Create a background job");
    row("make:mail <Name>", "Create a mailable");
    row("make:notification <Name>", "Create a notification");
    row("make:mcp-tool <name>", "Create a tool agents can call over MCP");
    row("make:docker", "Create a two-stage Dockerfile");
    println!();

    println!("{}", console::bold("OPTIONS"));
    row("--port <port>", "Port for `serve` (default 8000)");
    row("--no-watch", "Run `serve` without the file watcher");
    println!();
}

/// Write a fresh `APP_KEY` into `.env`, creating the line if it is absent.
///
/// The key seeds encryption and session signing, so it is generated from OS
/// entropy and never derived from anything guessable.
fn key_generate() -> Result<(), String> {
    let project = Project::discover()?;
    let path = project.root.join(".env");

    let mut bytes = [0u8; 32];
    {
        use std::io::Read;
        let mut source = std::fs::File::open("/dev/urandom")
            .map_err(|e| format!("cannot read system entropy: {e}"))?;
        source.read_exact(&mut bytes).map_err(|e| format!("cannot read system entropy: {e}"))?;
    }
    // The `base64:` prefix is part of the value: it tells the auth package the
    // key is encoded rather than raw, the way Laravel's APP_KEY does.
    let key = format!("base64:{}", base64(&bytes));

    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let updated = if existing.lines().any(|line| line.trim_start().starts_with("APP_KEY=")) {
        existing
            .lines()
            .map(|line| {
                if line.trim_start().starts_with("APP_KEY=") {
                    format!("APP_KEY={key}")
                } else {
                    line.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n"
    } else {
        format!("{existing}APP_KEY={key}\n")
    };

    std::fs::write(&path, updated).map_err(|e| format!("cannot write .env: {e}"))?;
    console::success("APP_KEY written to .env.");
    Ok(())
}

fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);

    for chunk in input.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let bits = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(bits >> 18 & 0x3f) as usize] as char);
        out.push(ALPHABET[(bits >> 12 & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 { ALPHABET[(bits >> 6 & 0x3f) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { ALPHABET[(bits & 0x3f) as usize] as char } else { '=' });
    }
    out
}

fn row(command: &str, description: &str) {
    // Pad before colouring: escape codes would otherwise count toward the width.
    let padded = format!("{command:<26}");
    println!("  {}{}", console::accent(&padded), console::dim(description));
}
