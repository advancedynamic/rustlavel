//! The human-readable documentation page.
//!
//! Rendered from the same document the JSON endpoint serves, by a page that
//! carries its own styles and script — no CDN, so the docs work on a machine
//! with no internet and inside a private network.

use crate::Info;

/// Build the page that reads the document at `document_path`.
pub fn page(info: &Info, document_path: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title} — API</title>
<style>{CSS}</style>
</head>
<body>
<main>
  <header>
    <h1>{title}</h1>
    <p class="version">Version {version}</p>
    {description}
    <p class="source">Machine-readable: <a href="{document_path}">{document_path}</a></p>
  </header>
  <div id="operations" class="loading">Loading the API description…</div>
</main>
<script>
const DOCUMENT_URL = "{document_path}";

function escapeHtml(value) {{
  return String(value).replace(/[&<>"']/g, c => (
    {{'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}}[c]
  ));
}}

function render(document) {{
  const paths = document.paths || {{}};
  const groups = new Map();

  for (const [path, operations] of Object.entries(paths)) {{
    for (const [method, operation] of Object.entries(operations)) {{
      const tag = (operation.tags && operation.tags[0]) || 'General';
      if (!groups.has(tag)) groups.set(tag, []);
      groups.get(tag).push({{ path, method, operation }});
    }}
  }}

  if (groups.size === 0) {{
    return '<p class="empty">No operations are documented yet. '
         + 'Add <code>.describe("…")</code> to a route under the API prefix.</p>';
  }}

  const sections = [...groups.entries()].sort((a, b) => a[0].localeCompare(b[0]));
  return sections.map(([tag, entries]) => {{
    const rows = entries
      .sort((a, b) => a.path.localeCompare(b.path) || a.method.localeCompare(b.method))
      .map(({{ path, method, operation }}) => {{
        const parameters = (operation.parameters || []).map(p =>
          `<li><code>${{escapeHtml(p.name)}}</code>`
          + `<span class="where">${{escapeHtml(p.in)}}</span>`
          + (p.required ? '<span class="required">required</span>' : '')
          + (p.description ? `<span class="note">${{escapeHtml(p.description)}}</span>` : '')
          + '</li>'
        ).join('');

        const responses = Object.entries(operation.responses || {{}}).map(([status, r]) =>
          `<li><span class="status s${{status[0]}}">${{escapeHtml(status)}}</span>`
          + `<span class="note">${{escapeHtml(r.description || '')}}</span></li>`
        ).join('');

        return `<article class="op${{operation.deprecated ? ' deprecated' : ''}}">
          <div class="line">
            <span class="method m-${{escapeHtml(method)}}">${{escapeHtml(method.toUpperCase())}}</span>
            <code class="path">${{escapeHtml(path)}}</code>
            ${{operation.deprecated ? '<span class="tag-deprecated">deprecated</span>' : ''}}
          </div>
          ${{operation.summary ? `<p class="summary">${{escapeHtml(operation.summary)}}</p>` : ''}}
          <div class="detail">
            ${{parameters ? `<div><h4>Parameters</h4><ul class="params">${{parameters}}</ul></div>` : ''}}
            ${{responses ? `<div><h4>Responses</h4><ul class="responses">${{responses}}</ul></div>` : ''}}
          </div>
        </article>`;
      }}).join('');

    return `<section><h2>${{escapeHtml(tag)}}</h2>${{rows}}</section>`;
  }}).join('');
}}

(async () => {{
  const target = document.getElementById('operations');
  try {{
    const response = await fetch(DOCUMENT_URL);
    const source = await response.json();
    target.classList.remove('loading');
    target.innerHTML = render(source);
  }} catch (error) {{
    target.textContent = 'Could not load ' + DOCUMENT_URL + ': ' + error;
  }}
}})();
</script>
</body>
</html>"#,
        title = escape(&info.title),
        version = escape(&info.version),
        description = info
            .description
            .as_ref()
            .map(|d| format!("<p class=\"description\">{}</p>", escape(d)))
            .unwrap_or_default(),
        // The page reads the same document the JSON endpoint serves, so there
        // is one source of truth rather than two that can disagree.
        document_path = escape(document_path),
    )
}

fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

const CSS: &str = r#"
:root { color-scheme: light dark; --bg:#faf9f7; --fg:#1c1b1a; --muted:#6b6864; --line:#e5e2dd;
        --accent:#b4483c; --panel:#fff; --code:#f4f2ef; }
@media (prefers-color-scheme: dark) {
  :root { --bg:#181716; --fg:#eceae7; --muted:#9a958e; --line:#2e2c29; --accent:#e0796c;
          --panel:#201f1d; --code:#252321; }
}
* { box-sizing: border-box; }
body { margin:0; background:var(--bg); color:var(--fg);
       font:15px/1.6 ui-sans-serif,-apple-system,'Segoe UI',sans-serif; }
main { max-width: 880px; margin: 0 auto; padding: 48px 24px 96px; }
header { border-bottom:1px solid var(--line); padding-bottom:24px; margin-bottom:32px; }
h1 { font-size:28px; margin:0 0 4px; font-weight:650; }
.version { margin:0; color:var(--muted); font-size:13px; }
.description { margin:12px 0 0; }
.source { margin:12px 0 0; font-size:13px; color:var(--muted); }
.source a { color:var(--accent); }
h2 { font-size:13px; text-transform:uppercase; letter-spacing:.08em; color:var(--muted);
     margin:32px 0 12px; font-weight:600; }
h4 { font-size:11px; text-transform:uppercase; letter-spacing:.07em; color:var(--muted);
     margin:0 0 6px; font-weight:600; }
.op { background:var(--panel); border:1px solid var(--line); border-radius:8px;
      padding:16px 20px; margin-bottom:10px; }
.op.deprecated { opacity:.6; }
.line { display:flex; align-items:center; gap:12px; flex-wrap:wrap; }
.method { font-size:11px; font-weight:700; letter-spacing:.05em; padding:3px 8px;
          border-radius:4px; color:#fff; min-width:56px; text-align:center; }
.m-get { background:#3d7ea6; } .m-post { background:#4a8c5f; } .m-put { background:#a8813c; }
.m-patch { background:#8a6bab; } .m-delete { background:var(--accent); }
.path { font-family:ui-monospace,SFMono-Regular,Menlo,monospace; font-size:14px; }
.tag-deprecated { font-size:11px; color:var(--accent); border:1px solid var(--accent);
                  border-radius:4px; padding:1px 6px; }
.summary { margin:8px 0 0; color:var(--fg); }
.detail { display:flex; gap:40px; flex-wrap:wrap; margin-top:14px; }
.detail:empty { display:none; }
ul { list-style:none; margin:0; padding:0; font-size:13px; }
li { padding:2px 0; display:flex; align-items:baseline; gap:8px; }
code { font-family:ui-monospace,SFMono-Regular,Menlo,monospace; background:var(--code);
       padding:1px 5px; border-radius:3px; font-size:13px; }
.where { color:var(--muted); font-size:11px; }
.required { color:var(--accent); font-size:11px; }
.note { color:var(--muted); }
.status { font-family:ui-monospace,SFMono-Regular,Menlo,monospace; font-weight:600; min-width:34px; }
.s2 { color:#4a8c5f; } .s3 { color:#a8813c; } .s4, .s5 { color:var(--accent); }
.loading, .empty { color:var(--muted); }
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_names_the_api_and_points_at_the_document() {
        let info = Info { title: "Orders API".into(), version: "2.1".into(), ..Info::default() };
        let page = page(&info, "/openapi.json");

        assert!(page.contains("<title>Orders API — API</title>"));
        assert!(page.contains("Version 2.1"));
        assert!(page.contains(r#"href="/openapi.json""#));
        assert!(page.contains(r#"const DOCUMENT_URL = "/openapi.json";"#));
    }

    #[test]
    fn it_carries_its_own_styles_and_script() {
        let page = page(&Info::default(), "/openapi.json");

        // A machine inside a private network has no CDN to reach.
        assert!(!page.contains("http://") && !page.contains("https://"));
        assert!(page.contains("<style>"));
        assert!(page.contains("prefers-color-scheme: dark"));
    }

    #[test]
    fn a_title_with_markup_is_escaped() {
        let info = Info { title: "<script>alert(1)</script>".into(), ..Info::default() };
        let page = page(&info, "/openapi.json");

        assert!(!page.contains("<script>alert(1)</script>"));
        assert!(page.contains("&lt;script&gt;"));
    }
}
