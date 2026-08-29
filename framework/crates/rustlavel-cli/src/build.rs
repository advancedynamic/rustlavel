//! `rustlavel build` — produce the single binary that gets deployed.
//!
//! This is the card Rust holds that PHP cannot: no runtime to install, no
//! PHP-FPM, no `composer install` on the server. One file goes up, and it
//! serves.

use crate::console;
use crate::project::Project;
use std::process::Command;

pub fn run(args: &[String]) -> Result<(), String> {
    let project = Project::discover()?;

    let mut target: Option<String> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--target" => {
                target = Some(iter.next().ok_or("--target needs a triple")?.clone());
            }
            other => return Err(format!("unknown option `{other}`")),
        }
    }

    console::heading(&format!("Building {}", console::accent(&project.crate_name)));
    if let Some(triple) = &target {
        console::info(&console::dim(&format!("cross-compiling for {triple}")));
    }

    let mut command = Command::new("cargo");
    command
        .arg("build")
        .arg("--release")
        .arg("--bin")
        .arg(&project.crate_name)
        .current_dir(&project.root);

    if let Some(triple) = &target {
        command.arg("--target").arg(triple);
    }

    let status = command.status().map_err(|e| format!("cannot run cargo: {e}"))?;
    if !status.success() {
        return Err("the release build failed".into());
    }

    let binary = match &target {
        Some(triple) => project.root.join("target").join(triple).join("release").join(&project.crate_name),
        None => project.root.join("target/release").join(&project.crate_name),
    };

    let size = std::fs::metadata(&binary).map(|m| m.len()).unwrap_or(0);

    console::success(&format!(
        "{}\n  {} — {}\n\n  Copy that one file to the server, put .env beside it, and run it.\n  \
         No runtime to install.",
        "Built.",
        binary.display(),
        human_size(size)
    ));

    if !project.root.join("public").read_dir().map(|mut d| d.next().is_some()).unwrap_or(false) {
        return Ok(());
    }

    // Static files still live beside the binary; embedding them is a later
    // step, and pretending otherwise would surprise someone on deploy day.
    console::info(&console::dim(
        "public/ is served from disk — copy it alongside the binary.",
    ));
    Ok(())
}

/// A Dockerfile that builds and runs the application in two stages.
pub fn make_docker(project: &Project) -> Result<(), String> {
    let path = project.root.join("Dockerfile");
    if path.exists() {
        return Err("Dockerfile already exists".into());
    }

    let contents = DOCKERFILE.replace("{{name}}", &project.crate_name);
    std::fs::write(&path, contents).map_err(|e| format!("cannot write Dockerfile: {e}"))?;
    console::created("Dockerfile");

    let ignore = project.root.join(".dockerignore");
    if !ignore.exists() {
        std::fs::write(&ignore, "target/\n.git/\n.env\n").map_err(|e| e.to_string())?;
        console::created(".dockerignore");
    }

    console::success(&format!(
        "Dockerfile created.\n\n  docker build -t {name} .\n  docker run -p 8000:8000 {name}",
        name = project.crate_name
    ));
    Ok(())
}

const DOCKERFILE: &str = r#"# Build the single binary, then ship it on a base image with no runtime.
FROM rust:1-slim AS build
WORKDIR /app
COPY . .
RUN cargo build --release --bin {{name}}

FROM debian:stable-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=build /app/target/release/{{name}} /app/{{name}}
COPY config ./config
COPY public ./public
ENV SERVER_HOST=0.0.0.0 SERVER_PORT=8000 APP_ENV=production APP_DEBUG=false
EXPOSE 8000
# The health endpoint the framework adds to every application.
HEALTHCHECK --interval=30s --timeout=3s CMD ["/app/{{name}}", "--health"]
CMD ["/app/{{name}}"]
"#;

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut size = bytes as f64;
    let mut unit = 0;

    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }

    if unit == 0 { format!("{bytes} B") } else { format!("{size:.1} {}", UNITS[unit]) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_binary_sizes_readably() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2.0 KB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn the_dockerfile_names_the_application() {
        let rendered = DOCKERFILE.replace("{{name}}", "blog");

        assert!(rendered.contains("cargo build --release --bin blog"));
        assert!(rendered.contains("COPY --from=build /app/target/release/blog /app/blog"));
        assert!(!rendered.contains("{{name}}"));
        // Production defaults must not leave the debug error page on.
        assert!(rendered.contains("APP_DEBUG=false"));
    }
}
