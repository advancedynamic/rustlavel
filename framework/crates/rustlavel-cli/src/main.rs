//! The `rustlavel` binary — the artisan of rustlavel.
//!
//! Commands that only touch files (`new`, `make:*`) run here. Commands that
//! need the application itself (`route:list`, and later `migrate`, `db:seed`,
//! `queue:work`) are forwarded to the project's own binary, because in a
//! compiled language the application is the only thing that knows its routes.

mod console;
mod database;
mod make;
mod naming;
mod new;
mod project;
mod serve;
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
    row("serve", "Run the app, reloading when files change");
    row("route:list", "Show the registered routes");
    row("migrate", "Run pending migrations");
    row("migrate:rollback", "Undo the last batch of migrations");
    row("migrate:fresh", "Drop every table and migrate from scratch");
    row("migrate:status", "Show which migrations have run");
    row("db:seed", "Run the seeders");
    println!();

    println!("{}", console::bold("GENERATORS"));
    row("make:controller <Name>", "Create a controller");
    row("make:middleware <name>", "Create a middleware function");
    row("make:model <Name>", "Create a model");
    row("make:migration <name>", "Create a migration");
    row("make:seeder <Name>", "Create a seeder");
    println!();

    println!("{}", console::bold("OPTIONS"));
    row("--port <port>", "Port for `serve` (default 8000)");
    row("--no-watch", "Run `serve` without the file watcher");
    println!();
}

fn row(command: &str, description: &str) {
    // Pad before colouring: escape codes would otherwise count toward the width.
    let padded = format!("{command:<26}");
    println!("  {}{}", console::accent(&padded), console::dim(description));
}
