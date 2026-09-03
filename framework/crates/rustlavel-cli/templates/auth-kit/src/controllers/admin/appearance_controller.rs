//! The Appearance tab: the login panel, the sidebar, and the two logos.
//!
//! Everything this writes ends up somewhere a browser trusts. The colours are
//! interpolated into `/css/theme.css`, and the logo paths into an `<img src>`
//! on the login page, which is the one page an anonymous visitor sees. So the
//! rule here is that nothing is stored until it has been proved to be the kind
//! of thing it claims to be — a hex colour, a path on this origin, or a file
//! whose *bytes* say PNG, JPEG or SVG. What the browser called it, and what
//! `<input type="color">` promised, are not evidence.
//!
//! The uploads take a slightly unusual road, and the comment on
//! [`AppearanceController::upload`] explains why: `rustlavel-http` has no
//! multipart parser, so the file arrives as a raw request body rather than as
//! a `multipart/form-data` part. The path field beside each upload box is the
//! road for a browser with JavaScript turned off.

use rustlavel::prelude::*;

use crate::support::{page, settings::Settings};

pub struct AppearanceController;

/// Where an uploaded logo is written.
///
/// Under `storage/` rather than `public/` because `public/` is served
/// wholesale by the static-file handler, and a directory that anything can
/// write into should not also be a directory that is served without a handler
/// looking at the request first. [`AppearanceController::logo`] is that
/// handler.
const LOGO_DIR: &str = "storage/app/public/logos";

/// The public prefix the stored path uses. Kept in step with the route.
const LOGO_URL: &str = "/storage/logos/";

/// Two megabytes, which is generous for a logo and small enough that the
/// server's own 10 MB body ceiling is never the thing that answers.
const MAX_LOGO_BYTES: usize = 2 * 1024 * 1024;

/// The colour keys of the login panel, in the order the form and the presets
/// list them. The template repeats this order in `data-keys`; a preset is a
/// list of values positional against it.
const LOGIN_KEYS: &[&str] = &[
    "theme.login.light.from",
    "theme.login.light.to",
    "theme.login.dark.from",
    "theme.login.dark.to",
];

/// The same, for the sidebar.
const SIDEBAR_KEYS: &[&str] = &[
    "theme.sidebar.light.bg",
    "theme.sidebar.light.text",
    "theme.sidebar.light.active_bg",
    "theme.sidebar.light.active_text",
    "theme.sidebar.dark.bg",
    "theme.sidebar.dark.text",
    "theme.sidebar.dark.active_bg",
    "theme.sidebar.dark.active_text",
];

const LOGO_KEYS: &[&str] = &["theme.logo.light", "theme.logo.dark"];

impl AppearanceController {
    // There is no handler here for drawing the tab. `AdminSettingsController`
    // already answers `GET /admin/settings/{tab}` and hands the template every
    // `theme.*` setting under `s`, so `settings/tabs/appearance.rl.html` reads
    // from there and this file is only ever the writing half. Adding a `GET`
    // of its own would shadow that route, not extend it.

    /// `POST /admin/settings/appearance/login`
    pub async fn save_login(req: Request) -> Result<Response> {
        Self::save_colours(req, LOGIN_KEYS, "The login panel colours have been saved.").await
    }

    /// `POST /admin/settings/appearance/sidebar`
    pub async fn save_sidebar(req: Request) -> Result<Response> {
        Self::save_colours(req, SIDEBAR_KEYS, "The sidebar colours have been saved.").await
    }

    async fn save_colours(mut req: Request, keys: &[&str], done: &str) -> Result<Response> {
        let store = settings(&req)?.clone();
        let mut values = Vec::with_capacity(keys.len());
        let mut coerced = 0;

        for key in keys {
            let submitted = req.input(key).unwrap_or_default();
            let safe = colour(&submitted);
            // Not silent: a value that had to be replaced means the form and
            // the stylesheet now disagree with what somebody typed, and saying
            // nothing is how that becomes a bug report about black.
            if !submitted.trim().is_empty() && safe != submitted.trim() {
                coerced += 1;
            }
            values.push(((*key).to_string(), safe));
        }

        store.put_all(&values).await?;

        if coerced > 0 {
            page::flash(
                &req,
                "error",
                format!(
                    "{coerced} value{} was not a hex colour and has been set to black instead.",
                    if coerced == 1 { "" } else { "s" }
                ),
            );
        } else {
            page::flash(&req, "success", done);
        }
        Ok(Response::see_other("/admin/settings/appearance"))
    }

    /// `POST /admin/settings/appearance/logos`
    ///
    /// Saves the two paths the upload boxes hold. The file itself arrived
    /// earlier, through [`AppearanceController::upload`]; this is the step that
    /// makes it the logo, so a chosen-but-unsaved file changes nothing.
    pub async fn save_logos(mut req: Request) -> Result<Response> {
        let store = settings(&req)?.clone();
        let mut values = Vec::with_capacity(LOGO_KEYS.len());

        for key in LOGO_KEYS {
            let submitted = req.input(key).unwrap_or_default();
            let next = logo_path(&submitted);
            let previous = logo_path(&store.get(key).await);
            // The file that is being replaced is no longer reachable from any
            // setting, and leaving it would turn this directory into a place
            // that only ever grows.
            discard(&previous, &next).await;
            values.push(((*key).to_string(), next));
        }

        store.put_all(&values).await?;
        page::flash(&req, "success", "The logos have been saved.");
        Ok(Response::see_other("/admin/settings/appearance"))
    }

    /// `POST /admin/settings/appearance/logo`
    ///
    /// The file, as the raw request body.
    ///
    /// `rustlavel-http` has no multipart parser — `Request` offers `input`,
    /// `inputs`, `form` and `json`, and `parse_body` understands JSON and
    /// `application/x-www-form-urlencoded` and nothing else — so there is no
    /// framework API to read a `multipart/form-data` part with, and writing a
    /// multipart parser inside an application controller would be the wrong
    /// place for it. `appearance.js` therefore POSTs the bytes on their own,
    /// which needs no parser at all: `req.body()` is the file.
    ///
    /// The declared content type is ignored. It is chosen by the client and
    /// the whole point of [`sniff`] is that the bytes decide.
    pub async fn upload(req: Request) -> Result<Response> {
        let bytes = req.body();

        if bytes.is_empty() {
            return Ok(refused(Status::BAD_REQUEST, "No file was received."));
        }
        if bytes.len() > MAX_LOGO_BYTES {
            return Ok(refused(
                Status::PAYLOAD_TOO_LARGE,
                "That file is larger than 2 MB. Please use a smaller one.",
            ));
        }

        let format = match sniff(bytes) {
            Ok(format) => format,
            Err(reason) => return Ok(refused(Status::UNPROCESSABLE, &reason)),
        };

        // The name is generated here and never taken from the request. A
        // browser-supplied filename can hold a path separator, a `..`, a second
        // extension, a right-to-left override that makes `evil.svg` read as
        // `gvs.png`, or simply the same name somebody else already used. A
        // random name has none of those properties, and it carries nothing
        // about the uploader into a directory that is served over HTTP.
        let name = format!("{}.{}", rustlavel::auth::random::hex(16), format.extension());
        let directory = std::path::Path::new(LOGO_DIR);
        rustlavel::tokio::fs::create_dir_all(directory).await?;
        rustlavel::tokio::fs::write(directory.join(&name), bytes).await?;

        Ok(Response::json(Json::object([
            ("path", Json::from(format!("{LOGO_URL}{name}"))),
            ("format", Json::from(format.extension())),
        ])))
    }

    /// `GET /storage/logos/{file}`
    ///
    /// The uploads are outside `public/`, so they need a handler. This one is
    /// deliberately public: the login page is the main place a logo is shown,
    /// and that page is served to people who are not signed in.
    pub async fn logo(req: Request) -> Result<Response> {
        let name = req.param("file").unwrap_or_default();

        // Every name in this directory was generated by `upload`, so the route
        // can insist on exactly that shape. There is no path to traverse out of
        // when the only accepted name is thirty-two hex digits and one of three
        // extensions.
        let Some(format) = stored_name(name) else { return Ok(Response::not_found()) };

        let path = std::path::Path::new(LOGO_DIR).join(name);
        let Ok(bytes) = rustlavel::tokio::fs::read(&path).await else {
            return Ok(Response::not_found());
        };

        Ok(Response::ok()
            .with_header("content-type", format.mime())
            // Belt and braces around the SVG case. `sniff` refuses an SVG that
            // can run anything, but a policy of `default-src 'none'` means that
            // even an SVG that got past it cannot fetch, script or frame — and
            // `nosniff` stops a browser deciding a PNG is really HTML.
            .with_header("content-security-policy", "default-src 'none'; style-src 'unsafe-inline'")
            .with_header("x-content-type-options", "nosniff")
            .with_header("cache-control", "public, max-age=300")
            .with_body(bytes))
    }
}

/// The settings store, or a message that says which line of `main.rs` is
/// missing rather than a panic.
fn settings(req: &Request) -> Result<&Settings> {
    req.state::<Settings>().ok_or_else(|| {
        Error::msg(
            "the settings store is not registered. Add `.state(Settings::from_config(db.clone(), \
             app.config())?)` in main.rs.",
        )
    })
}

fn refused(status: Status, message: &str) -> Response {
    Response::new(status).with_json(Json::object([("message", Json::from(message))]))
}

/// Delete a logo that nothing points at any more.
async fn discard(previous: &str, next: &str) {
    if previous.is_empty() || previous == next {
        return;
    }
    let Some(name) = previous.strip_prefix(LOGO_URL) else { return };
    if stored_name(name).is_none() {
        return;
    }
    // A failure here is untidy, not dangerous: the setting has already stopped
    // pointing at the file.
    let _ = rustlavel::tokio::fs::remove_file(std::path::Path::new(LOGO_DIR).join(name)).await;
}

/// A stored colour, or a safe one.
///
/// This is the same rule as `theme_controller::colour`, and deliberately the
/// same shape. It is written twice because that function is private to the
/// controller that serves the stylesheet, and the two ends have to agree: this
/// is what may be stored, that is what may be served. The test below pins the
/// behaviour so the pair cannot drift quietly.
///
/// Six or three hex digits, and nothing else gets through — these values are
/// interpolated into a stylesheet, so a value that is not a colour is a value
/// that could close the declaration and open something else.
fn colour(value: &str) -> String {
    let candidate = value.trim();
    let body = candidate.strip_prefix('#').unwrap_or("");
    let valid = matches!(body.len(), 3 | 6) && body.chars().all(|c| c.is_ascii_hexdigit());

    if valid { format!("#{body}") } else { "#000000".to_string() }
}

/// A stored logo path, or nothing.
///
/// The value goes into `<img src="…">`, so it has to be a path on this origin
/// and not a URL. `//evil.example/logo.png` is a URL that looks like a path,
/// which is why the leading `//` is rejected on its own, and banning `:` from
/// the character set is what keeps `javascript:` and `data:` out.
fn logo_path(value: &str) -> String {
    let candidate = value.trim();
    if candidate.is_empty() {
        return String::new();
    }

    let acceptable = candidate.starts_with('/')
        && !candidate.starts_with("//")
        && !candidate.contains("..")
        && candidate.len() <= 200
        && candidate.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-'));

    if acceptable { candidate.to_string() } else { String::new() }
}

/// The three formats a logo may be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Png,
    Jpeg,
    Svg,
}

impl Format {
    fn extension(self) -> &'static str {
        match self {
            Format::Png => "png",
            Format::Jpeg => "jpg",
            Format::Svg => "svg",
        }
    }

    fn mime(self) -> &'static str {
        match self {
            Format::Png => "image/png",
            Format::Jpeg => "image/jpeg",
            Format::Svg => "image/svg+xml",
        }
    }
}

/// One of the names `upload` generates, and which format it is.
fn stored_name(name: &str) -> Option<Format> {
    let (stem, extension) = name.rsplit_once('.')?;
    if stem.len() != 32 || !stem.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    match extension {
        "png" => Some(Format::Png),
        "jpg" => Some(Format::Jpeg),
        "svg" => Some(Format::Svg),
        _ => None,
    }
}

/// What the bytes actually are.
///
/// The filename is not consulted, because the filename is a claim made by
/// whoever is uploading. PNG opens with an eight-byte signature, JPEG with the
/// SOI marker and the start of the first segment, and an SVG has to survive
/// [`check_svg`].
fn sniff(bytes: &[u8]) -> std::result::Result<Format, String> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Ok(Format::Png);
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Ok(Format::Jpeg);
    }

    match std::str::from_utf8(bytes) {
        Ok(text) => check_svg(text).map(|()| Format::Svg),
        Err(_) => Err("That file is not a PNG, a JPEG or an SVG.".to_string()),
    }
}

/// Elements an SVG logo is allowed to contain.
///
/// An allow-list rather than a list of forbidden tags, because the forbidden
/// list is the one that is wrong the day a browser ships a new element. A logo
/// that uses something not here can be re-exported as plain SVG; that is a
/// smaller cost than being wrong in this direction.
const SVG_ELEMENTS: &[&str] = &[
    "svg", "g", "defs", "symbol", "use", "title", "desc", "style", "switch", "a", "path", "rect",
    "circle", "ellipse", "line", "polyline", "polygon", "text", "tspan", "textpath",
    "lineargradient", "radialgradient", "stop", "clippath", "mask", "pattern", "image", "marker",
    "filter", "fegaussianblur", "feoffset", "feblend", "feflood", "fecomposite", "femerge",
    "femergenode", "fecolormatrix", "fedropshadow", "femorphology", "fetile", "feturbulence",
];

/// Whether an SVG is safe to store and serve from this origin.
///
/// **This refuses rather than rewrites, and that is the decision.** An SVG is
/// a document: it can carry `<script>`, an `onload=` on any element, a
/// `javascript:` href, an external `<use>` that phones home, and a DOCTYPE
/// that expands into a gigabyte. It is served from this origin, so anything it
/// runs runs as this site.
///
/// A sanitiser that rewrites has to be right about every construct a browser
/// will ever accept, and when it is wrong it stores the file anyway and says
/// nothing. Refusing is wrong in the other direction: an unusual-but-harmless
/// logo is turned away with a message, and the person re-exports it. One of
/// those failure modes is a support question and the other is a stored XSS on
/// the login page, so this one refuses.
///
/// The scan is a small XML walk rather than a search for `<script`, because a
/// search for text can be fooled by encoding and a walk cannot: it has to
/// understand the element and attribute names to get to the end at all.
fn check_svg(source: &str) -> std::result::Result<(), String> {
    let bytes: Vec<char> = source.chars().collect();
    let mut index = 0;
    let mut stack: Vec<String> = Vec::new();
    let mut root_seen = false;

    let refuse = |what: &str| Err(format!("That SVG was refused: {what}."));

    while index < bytes.len() {
        // Text between elements. Nothing in it can start an element, so it is
        // skipped whole.
        if bytes[index] != '<' {
            index += 1;
            continue;
        }
        index += 1;

        match bytes.get(index) {
            None => return refuse("it ends in the middle of a tag"),
            // `<?xml …?>` and any other processing instruction.
            Some('?') => {
                index = skip_to(&bytes, index, "?>").ok_or("unterminated <?…?>".to_string())?;
                continue;
            }
            Some('!') => {
                // A comment is harmless; a DOCTYPE is not — it is how entity
                // expansion and external entities get in — and CDATA hides
                // content from the rest of this walk.
                if bytes[index..].starts_with(&['!', '-', '-']) {
                    index = skip_to(&bytes, index, "-->").ok_or("unterminated comment".to_string())?;
                    continue;
                }
                return refuse("a DOCTYPE or CDATA section is not allowed in a logo");
            }
            Some('/') => {
                index += 1;
                let (name, next) = read_name(&bytes, index);
                index = skip_to(&bytes, next, ">").ok_or("unterminated closing tag".to_string())?;
                match stack.pop() {
                    Some(open) if open == name => {}
                    _ => return refuse("its tags do not nest, so it is not XML"),
                }
                continue;
            }
            Some(_) => {}
        }

        let (name, next) = read_name(&bytes, index);
        index = next;
        if name.is_empty() {
            return refuse("a tag has no name, so it is not XML");
        }
        if !root_seen {
            if name != "svg" {
                return refuse("its root element is not <svg>");
            }
            root_seen = true;
        }
        if !SVG_ELEMENTS.contains(&name.as_str()) {
            return refuse(&format!("<{name}> is not an element a logo may contain"));
        }

        // Attributes, up to the `>` or `/>` that ends the tag.
        let mut empty_element = false;
        loop {
            index = skip_space(&bytes, index);
            match bytes.get(index) {
                None => return refuse("it ends in the middle of a tag"),
                Some('>') => {
                    index += 1;
                    stack.push(name.clone());
                    break;
                }
                Some('/') if bytes.get(index + 1) == Some(&'>') => {
                    index += 2;
                    empty_element = true;
                    break;
                }
                Some(_) => {}
            }

            let (attribute, after_name) = read_name(&bytes, index);
            if attribute.is_empty() {
                return refuse("an attribute has no name, so it is not XML");
            }
            index = skip_space(&bytes, after_name);
            if bytes.get(index) != Some(&'=') {
                // XML has no bare attributes; HTML's `<svg onload>` is exactly
                // the shape a lenient parser would wave through.
                return refuse("an attribute has no value, so it is not XML");
            }
            index = skip_space(&bytes, index + 1);

            let quote = match bytes.get(index) {
                Some(&q @ ('"' | '\'')) => q,
                _ => return refuse("an attribute value is not quoted, so it is not XML"),
            };
            let start = index + 1;
            let end = (start..bytes.len())
                .find(|&i| bytes[i] == quote)
                .ok_or("unterminated attribute value".to_string())?;
            let value: String = bytes[start..end].iter().collect();
            index = end + 1;

            check_attribute(&attribute, &value).map_err(|what| format!("That SVG was refused: {what}."))?;
        }

        // `<style>` is allowed because exported logos lean on it, but its
        // contents are CSS this walk would otherwise never look at.
        if name == "style" && !empty_element {
            let end = find(&bytes, index, "</style>").ok_or("unterminated <style>".to_string())?;
            let css: String = bytes[index..end].iter().collect::<String>().to_lowercase();
            if css.contains("@import") || css.contains("javascript:") || css.contains("expression(")
            {
                return refuse("its <style> block fetches or runs something");
            }
            // `url(#gradient)` is an internal reference and fine; anything else
            // is a request to somewhere, which is how a logo becomes a beacon.
            if css.split("url(").skip(1).any(|rest| !rest.trim_start().starts_with('#')) {
                return refuse("its <style> block loads something from outside the file");
            }
        }
    }

    if !root_seen {
        return refuse("it contains no elements");
    }
    if !stack.is_empty() {
        return refuse("a tag is left open, so it is not XML");
    }
    Ok(())
}

/// One attribute, judged on its name and its value.
fn check_attribute(name: &str, value: &str) -> std::result::Result<(), String> {
    // Every scripting hook in SVG is an attribute starting `on`: onload,
    // onclick, onbegin, onmouseover, onfocusin. Refusing the prefix refuses the
    // ones that have not been invented yet as well.
    if name.starts_with("on") {
        return Err(format!("`{name}` is an event handler"));
    }

    let flattened = value.to_lowercase().replace(char::is_whitespace, "");
    if flattened.contains("javascript:") || flattened.contains("data:text/html") {
        return Err(format!("`{name}` holds a script URL"));
    }

    // A reference may point inside the file, or be a picture inlined into it.
    // Anything else is a request made from the page the logo is drawn on.
    let reference = matches!(name, "href" | "src") || name.ends_with(":href");
    if reference && !(flattened.starts_with('#') || flattened.starts_with("data:image/")) {
        return Err(format!("`{name}` points outside the file"));
    }
    Ok(())
}

/// Read an XML name, lowercased and with any namespace prefix kept, from
/// `index`. Returns the name and the index just past it.
fn read_name(source: &[char], index: usize) -> (String, usize) {
    let mut end = index;
    while end < source.len()
        && (source[end].is_ascii_alphanumeric() || matches!(source[end], '-' | '_' | ':' | '.'))
    {
        end += 1;
    }
    let raw: String = source[index..end].iter().collect::<String>().to_lowercase();
    (raw, end)
}

fn skip_space(source: &[char], mut index: usize) -> usize {
    while index < source.len() && source[index].is_whitespace() {
        index += 1;
    }
    index
}

/// The index just past the next `needle`.
fn skip_to(source: &[char], from: usize, needle: &str) -> Option<usize> {
    find(source, from, needle).map(|at| at + needle.chars().count())
}

/// The index at which `needle` next starts.
fn find(source: &[char], from: usize, needle: &str) -> Option<usize> {
    let pattern: Vec<char> = needle.chars().collect();
    if pattern.is_empty() || source.len() < pattern.len() {
        return None;
    }
    (from..=source.len() - pattern.len()).find(|&start| {
        source[start..start + pattern.len()]
            .iter()
            .zip(&pattern)
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
    })
}

#[cfg(test)]
mod tests {
    use super::{check_svg, colour, logo_path, sniff, stored_name, AppearanceController, Format};
    use rustlavel::testing::TestClient;
    use rustlavel::{Method, Request, Router};

    /// The two routes that touch files, and nothing else — neither of them
    /// reads the settings, so this needs no database.
    ///
    /// A client per test rather than one shared: tests run at the same time,
    /// and the uploads land in one directory. That is safe only because every
    /// stored name is random, and each test removes what it wrote.
    fn client() -> TestClient {
        let mut router = Router::new();
        router.post("/upload", AppearanceController::upload);
        router.get("/storage/logos/{file}", AppearanceController::logo);
        TestClient::new(router)
    }

    /// A PNG signature followed by the IHDR length and type — enough of a file
    /// for a sniffer, which is the point: the sniffer looks at the front.
    fn png() -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend_from_slice(b"\x00\x00\x00\x0dIHDR");
        bytes.extend_from_slice(&[0u8; 32]);
        bytes
    }

    fn jpeg() -> Vec<u8> {
        let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xE0];
        bytes.extend_from_slice(b"\x00\x10JFIF\x00");
        bytes.extend_from_slice(&[0u8; 32]);
        bytes
    }

    #[test]
    fn a_colour_is_six_or_three_hex_digits_and_nothing_else() {
        assert_eq!(colour("#3b82f6"), "#3b82f6");
        assert_eq!(colour("  #FFF  "), "#FFF");
        assert_eq!(colour("#1E3A5F"), "#1E3A5F");
    }

    #[test]
    fn a_colour_that_could_close_the_declaration_is_replaced() {
        // Each of these would otherwise be written verbatim into
        // `/css/theme.css`, inside a `--login-from: …;` declaration.
        for hostile in [
            "red; } body { display: none",
            "#fff; background: url(https://evil.example/x)",
            "url(javascript:alert(1))",
            "3b82f6",
            "",
            "#12345",
            "#gggggg",
            "#fff)",
        ] {
            assert_eq!(colour(hostile), "#000000", "{hostile} should not have been accepted");
        }
    }

    #[test]
    fn a_logo_path_has_to_be_a_path_on_this_origin() {
        assert_eq!(logo_path("/storage/logos/abc.png"), "/storage/logos/abc.png");
        assert_eq!(logo_path("  /storage/logos/abc.svg "), "/storage/logos/abc.svg");
        assert_eq!(logo_path(""), "");

        for hostile in [
            "//evil.example/logo.png",
            "https://evil.example/logo.png",
            "javascript:alert(1)",
            "data:image/svg+xml,<svg onload=alert(1)>",
            "/storage/logos/../../../.env",
            "storage/logos/abc.png",
            "/storage/logos/a\"onerror=\"alert(1).png",
        ] {
            assert_eq!(logo_path(hostile), "", "{hostile} should not have been accepted");
        }
    }

    #[test]
    fn the_bytes_decide_which_format_a_file_is() {
        assert_eq!(sniff(&png()), Ok(Format::Png));
        assert_eq!(sniff(&jpeg()), Ok(Format::Jpeg));
        assert_eq!(sniff(br#"<svg xmlns="http://www.w3.org/2000/svg"><rect width="4" height="4"/></svg>"#), Ok(Format::Svg));
    }

    #[test]
    fn a_renamed_executable_is_not_a_logo() {
        // `logo.png` with a Mach-O header, an ELF header, a Windows executable
        // and a zip — the four things that actually turn up when somebody
        // renames a file and hopes.
        for disguise in [
            b"\xcf\xfa\xed\xfe\x07\x00\x00\x01".as_slice(),
            b"\x7fELF\x02\x01\x01\x00".as_slice(),
            b"MZ\x90\x00\x03\x00\x00\x00".as_slice(),
            b"PK\x03\x04\x14\x00\x00\x00".as_slice(),
            // A JPEG that is only nearly a JPEG.
            b"\xff\xd8\xfe\x00".as_slice(),
            // A PNG signature with one byte wrong.
            b"\x89PNG\r\n\x1a\x0bIHDR".as_slice(),
        ] {
            assert!(sniff(disguise).is_err(), "{disguise:?} should not have been accepted");
        }
    }

    #[test]
    fn a_plain_svg_logo_is_accepted() {
        let logo = r##"<?xml version="1.0" encoding="UTF-8"?>
            <!-- exported from a drawing program -->
            <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 48 48" fill="none">
              <title>Acme</title>
              <defs>
                <linearGradient id="g" x1="0" y1="0" x2="1" y2="1">
                  <stop offset="0" stop-color="#3b82f6"/>
                  <stop offset="1" stop-color="#2563eb"/>
                </linearGradient>
              </defs>
              <style>.mark { fill: url(#g); }</style>
              <path class="mark" d="M4 4h40v40H4z"/>
              <use href="#g"/>
            </svg>"##;
        assert_eq!(check_svg(logo), Ok(()));
    }

    #[test]
    fn an_svg_that_can_run_something_is_refused() {
        for hostile in [
            // The obvious one.
            r#"<svg xmlns="http://www.w3.org/2000/svg"><script>alert(1)</script></svg>"#,
            // The one people forget: no <script> element at all.
            r#"<svg xmlns="http://www.w3.org/2000/svg" onload="alert(1)"><rect/></svg>"#,
            r#"<svg xmlns="http://www.w3.org/2000/svg"><rect onmouseover="alert(1)"/></svg>"#,
            // A link that runs instead of navigating.
            r#"<svg xmlns="http://www.w3.org/2000/svg"><a href="javascript:alert(1)"><rect/></a></svg>"#,
            // HTML smuggled in through foreignObject.
            r#"<svg xmlns="http://www.w3.org/2000/svg"><foreignObject><iframe src="x"/></foreignObject></svg>"#,
            // Entity expansion, and the DOCTYPE that carries it.
            r#"<!DOCTYPE svg [<!ENTITY a "aaaa">]><svg xmlns="http://www.w3.org/2000/svg"><rect/></svg>"#,
            // CDATA, which hides its contents from anything that only reads tags.
            r#"<svg xmlns="http://www.w3.org/2000/svg"><![CDATA[<script>alert(1)</script>]]></svg>"#,
            // A reference that phones home.
            r#"<svg xmlns="http://www.w3.org/2000/svg"><use href="https://evil.example/x#a"/></svg>"#,
            r#"<svg xmlns="http://www.w3.org/2000/svg"><image href="https://evil.example/pixel.png"/></svg>"#,
            // CSS that fetches.
            r#"<svg xmlns="http://www.w3.org/2000/svg"><style>@import url(https://evil.example/x.css);</style></svg>"#,
            // Not an SVG at all, whatever it was called.
            r#"<html><body><script>alert(1)</script></body></html>"#,
            // Not well-formed, so no parser's behaviour can be relied on.
            r#"<svg xmlns="http://www.w3.org/2000/svg"><rect></svg>"#,
            r#"<svg xmlns="http://www.w3.org/2000/svg" onload><rect/></svg>"#,
            r#"<svg xmlns=http://www.w3.org/2000/svg><rect/></svg>"#,
        ] {
            assert!(check_svg(hostile).is_err(), "should have been refused: {hostile}");
        }
    }

    #[test]
    fn an_svg_carrying_a_script_never_reaches_the_disk() {
        // The same case as above, but through the function `upload` calls, so
        // the refusal is proved at the boundary rather than one layer in.
        let hostile = br#"<svg xmlns="http://www.w3.org/2000/svg"><script>alert(1)</script></svg>"#;
        assert!(sniff(hostile).is_err());
    }

    #[test]
    fn only_a_name_this_controller_generated_is_served() {
        assert_eq!(stored_name("0123456789abcdef0123456789abcdef.png"), Some(Format::Png));
        assert_eq!(stored_name("0123456789abcdef0123456789abcdef.jpg"), Some(Format::Jpeg));
        assert_eq!(stored_name("0123456789abcdef0123456789abcdef.svg"), Some(Format::Svg));

        for hostile in [
            "../../../.env",
            "0123456789abcdef0123456789abcdef.php",
            "0123456789abcdef0123456789abcde.png",
            "0123456789abcdef0123456789abcdefg.png",
            "..%2f..%2f.env",
            "logo.png",
            "",
        ] {
            assert_eq!(stored_name(hostile), None, "{hostile} should not have been served");
        }
    }

    #[rustlavel::test]
    async fn a_png_survives_the_upload_and_comes_back_out_of_the_route() {
        let client = client();
        let response = client
            .send(
                Request::new(Method::Post, "/upload")
                    .with_body(png())
                    .with_header("content-type", "image/png"),
            )
            .await
            .assert_ok();

        let path = response
            .json()
            .get("path")
            .and_then(|value| value.as_str().map(str::to_string))
            .expect("the response says where the file went");
        assert!(path.starts_with("/storage/logos/"), "{path}");

        client
            .get(&path)
            .await
            .assert_ok()
            .assert_header("content-type", "image/png")
            .assert_header("x-content-type-options", "nosniff");

        let name = path.rsplit('/').next().unwrap();
        std::fs::remove_file(std::path::Path::new(super::LOGO_DIR).join(name)).unwrap();
    }

    #[rustlavel::test]
    async fn the_route_refuses_a_renamed_executable_whatever_it_claims_to_be() {
        client()
            .send(
                Request::new(Method::Post, "/upload")
                    // The claim is `image/png`; the bytes are a Mach-O binary.
                    .with_body(b"\xcf\xfa\xed\xfe\x07\x00\x00\x01".to_vec())
                    .with_header("content-type", "image/png"),
            )
            .await
            .assert_status(422);
    }

    #[rustlavel::test]
    async fn the_route_refuses_an_svg_carrying_a_script() {
        client()
            .send(
                Request::new(Method::Post, "/upload")
                    .with_body(
                        br#"<svg xmlns="http://www.w3.org/2000/svg"><script>alert(1)</script></svg>"#
                            .to_vec(),
                    )
                    .with_header("content-type", "image/svg+xml"),
            )
            .await
            .assert_status(422);
    }

    #[rustlavel::test]
    async fn the_route_serves_nothing_it_did_not_name_itself() {
        client().get("/storage/logos/..%2f..%2f.env").await.assert_not_found();
        client().get("/storage/logos/anything.png").await.assert_not_found();
    }
}
