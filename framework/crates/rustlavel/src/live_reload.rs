//! Live reload: the browser follows the code.
//!
//! `rustlavel serve` already rebuilds and restarts the application when a Rust
//! file changes. What was left to the developer was the other half — switching
//! to the browser and pressing refresh. This module closes it, and only in
//! development, only when `serve` asked for it (`RUSTLAVEL_LIVE_RELOAD=1`).
//!
//! Three pieces, all inside the application so there is nothing to configure
//! and no second port:
//!
//! - `GET /_rustlavel/live` — a server-sent-events stream. The browser's
//!   `EventSource` reconnects by itself, which is the whole restart story: the
//!   old process dies, the stream drops, the new process comes up, the stream
//!   reopens, and a reopen after a drop means "the code changed, reload".
//! - `GET /_rustlavel/live.js` — the client, served from the application's own
//!   origin because a `script-src 'self'` policy (the auth kit's) refuses an
//!   inline script and a second origin alike.
//! - A middleware that adds one `<script src>` tag before `</body>` in HTML
//!   responses.
//!
//! A watcher polls `resources/` and `public/`. Templates and static files are
//! read from disk on every request in development, so changing one needs no
//! rebuild — only a browser refresh, or for a stylesheet, no refresh at all:
//! the client swaps the `<link>` in place and the page keeps its scroll
//! position, its form input and its open menus.

use rustlavel_http::{Middleware, Next, Request, Response, Router};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tokio::sync::broadcast;

const STREAM: &str = "/_rustlavel/live";
const CLIENT: &str = "/_rustlavel/live.js";
const TAG: &str = "<script src=\"/_rustlavel/live.js\" defer></script>";
/// How often the watched directories are looked at.
const POLL: Duration = Duration::from_millis(300);
/// Directories, relative to the project root, that change without a rebuild.
const WATCHED: [&str; 2] = ["resources", "public"];

const CLIENT_JS: &str = r#"(function () {
  var lost = false;
  var source = new EventSource("/_rustlavel/live");
  source.addEventListener("open", function () { if (lost) location.reload(); });
  source.addEventListener("error", function () { lost = true; });
  source.addEventListener("reload", function () { location.reload(); });
  source.addEventListener("css", function () {
    var stamp = Date.now();
    document.querySelectorAll('link[rel~="stylesheet"]').forEach(function (link) {
      var url = new URL(link.href, location.href);
      if (url.origin !== location.origin) return;
      url.searchParams.set("_lr", stamp);
      var fresh = link.cloneNode();
      fresh.href = url.href;
      fresh.onload = function () { link.remove(); };
      link.after(fresh);
    });
  });
})();
"#;

/// Whether `serve` asked for live reload. Never in production, whatever the
/// environment says: a stream that is open to every visitor is not a thing a
/// deployed application should grow because of a stray variable.
pub(crate) fn requested(production: bool) -> bool {
    !production && std::env::var("RUSTLAVEL_LIVE_RELOAD").is_ok_and(|value| value == "1")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Change {
    /// Only stylesheets changed: swap them in place.
    Css,
    /// Anything else: reload the page.
    Reload,
}

impl Change {
    fn event(self) -> &'static str {
        match self {
            Change::Css => "css",
            Change::Reload => "reload",
        }
    }
}

/// Mount the stream, the client and the injector, and start watching.
///
/// Must be called inside a Tokio runtime, because the watcher is a task.
pub(crate) fn install(router: &mut Router, root: &Path) {
    let (sender, _) = broadcast::channel::<Change>(16);

    let stream_sender = sender.clone();
    router.get(STREAM, move |_req: Request| {
        let mut receiver = stream_sender.subscribe();
        async move {
            let (tx, rx) = rustlavel_http::sse::channel(8);
            tokio::spawn(async move {
                // Ends with Ctrl-C too. An open stream counts as an in-flight
                // request, so without this the server waits out its whole
                // ten-second drain for a tab nobody is looking at.
                let mut stop = Box::pin(tokio::signal::ctrl_c());
                loop {
                    let received = tokio::select! {
                        received = receiver.recv() => received,
                        _ = &mut stop => break,
                    };
                    match received {
                        Ok(change) => {
                            if tx
                                .send(rustlavel_http::SseEvent::named(change.event(), ""))
                                .await
                                .is_err()
                            {
                                break; // the tab was closed
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
            Response::events(rx)
        }
    });

    router.get(CLIENT, |_req: Request| async {
        Response::ok()
            .with_header("content-type", "text/javascript; charset=utf-8")
            .with_header("cache-control", "no-store")
            .with_body(CLIENT_JS)
    });

    router.middleware(Injector);

    let dirs: Vec<PathBuf> = WATCHED.iter().map(|dir| root.join(dir)).collect();
    tokio::spawn(async move {
        let mut seen = scan(&dirs);
        loop {
            tokio::time::sleep(POLL).await;
            let scanned = dirs.clone();
            let Ok(now) = tokio::task::spawn_blocking(move || scan(&scanned)).await else {
                continue;
            };
            if let Some(change) = diff(&seen, &now) {
                // No receiver means no tab is open, which is fine.
                let _ = sender.send(change);
            }
            seen = now;
        }
    });
}

/// Adds the client script to HTML responses.
struct Injector;

impl Middleware for Injector {
    fn handle(&self, request: Request, next: Next) -> rustlavel_http::BoxFuture<Response> {
        Box::pin(async move {
            let mut response = next.run(request).await;
            let is_html = response
                .headers
                .get("content-type")
                .is_some_and(|value| value.starts_with("text/html"));
            // A compressed body cannot be edited, and nothing but a text body
            // has a `</body>` to put the tag in front of.
            if is_html
                && !response.headers.contains("content-encoding")
                && let Ok(html) = std::str::from_utf8(&response.body)
                && let Some(injected) = inject(html)
            {
                response.body = injected.into_bytes();
            }
            response
        })
    }
}

/// `html` with the client tag in front of the last `</body>`, or `None` when
/// there is no body to put it in or it is already there.
fn inject(html: &str) -> Option<String> {
    if html.contains(TAG) {
        return None;
    }
    // ASCII lowercasing keeps byte offsets, so the index is valid in `html`.
    let at = html.to_ascii_lowercase().rfind("</body>")?;
    let mut out = String::with_capacity(html.len() + TAG.len());
    out.push_str(&html[..at]);
    out.push_str(TAG);
    out.push_str(&html[at..]);
    Some(out)
}

type Snapshot = BTreeMap<PathBuf, SystemTime>;

fn scan(dirs: &[PathBuf]) -> Snapshot {
    let mut out = Snapshot::new();
    for dir in dirs {
        walk(dir, &mut out);
    }
    out
}

fn walk(path: &Path, out: &mut Snapshot) {
    let Ok(entries) = std::fs::read_dir(path) else { return };
    for entry in entries.flatten() {
        let child = entry.path();
        let Ok(metadata) = entry.metadata() else { continue };
        if metadata.is_dir() {
            walk(&child, out);
        } else if let Ok(modified) = metadata.modified() {
            out.insert(child, modified);
        }
    }
}

/// What kind of reload the difference between two scans calls for, if any.
fn diff(before: &Snapshot, after: &Snapshot) -> Option<Change> {
    let mut changed = before
        .iter()
        .filter(|(path, time)| after.get(*path) != Some(time))
        .map(|(path, _)| path)
        .chain(after.keys().filter(|path| !before.contains_key(*path)))
        .peekable();

    changed.peek()?;
    let only_css = changed.all(|path| path.extension().is_some_and(|ext| ext == "css"));
    Some(if only_css { Change::Css } else { Change::Reload })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn the_tag_goes_before_the_last_closing_body() {
        let page = "<html><body><p>a</p></BODY></html>";
        let out = inject(page).unwrap();
        assert_eq!(out, format!("<html><body><p>a</p>{TAG}</BODY></html>"));
    }

    #[test]
    fn a_fragment_without_a_body_is_left_alone() {
        assert_eq!(inject("<p>partial</p>"), None);
    }

    #[test]
    fn the_tag_is_never_added_twice() {
        let once = inject("<body></body>").unwrap();
        assert_eq!(inject(&once), None);
    }

    #[test]
    fn a_body_inside_a_script_string_does_not_take_the_tag() {
        let page = "<body><script>var s = '</body>';</script></body>";
        let out = inject(page).unwrap();
        assert!(out.ends_with(&format!("{TAG}</body>")), "{out}");
    }

    #[test]
    fn nothing_changed_means_no_event() {
        let a: Snapshot = [(PathBuf::from("a.css"), at(1))].into();
        assert_eq!(diff(&a, &a.clone()), None);
    }

    #[test]
    fn a_stylesheet_alone_is_swapped_not_reloaded() {
        let before: Snapshot = [(PathBuf::from("app.css"), at(1))].into();
        let after: Snapshot = [(PathBuf::from("app.css"), at(2))].into();
        assert_eq!(diff(&before, &after), Some(Change::Css));
    }

    #[test]
    fn a_template_reloads_the_page_even_beside_a_stylesheet() {
        let before: Snapshot =
            [(PathBuf::from("app.css"), at(1)), (PathBuf::from("home.html"), at(1))].into();
        let after: Snapshot =
            [(PathBuf::from("app.css"), at(2)), (PathBuf::from("home.html"), at(2))].into();
        assert_eq!(diff(&before, &after), Some(Change::Reload));
    }

    #[test]
    fn a_new_and_a_deleted_file_both_count() {
        let empty = Snapshot::new();
        let one: Snapshot = [(PathBuf::from("new.html"), at(1))].into();
        assert_eq!(diff(&empty, &one), Some(Change::Reload));
        assert_eq!(diff(&one, &empty), Some(Change::Reload));
    }

    #[test]
    fn production_never_turns_it_on() {
        // SAFETY: no other test in this crate reads this variable.
        unsafe { std::env::set_var("RUSTLAVEL_LIVE_RELOAD", "1") };
        assert!(!requested(true));
        assert!(requested(false));
        unsafe { std::env::remove_var("RUSTLAVEL_LIVE_RELOAD") };
        assert!(!requested(false));
    }
}
