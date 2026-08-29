//! The dashboard page: one self-contained HTML document.
//!
//! No CDN, no build step, no framework CSS — a debugging tool that needs a
//! network to render is useless exactly when you need it (offline, on a plane,
//! inside a locked-down network). The markup, the styles and the polling
//! script are all inlined, and the whole page is one `Response`.
//!
//! The visual language follows the development error page: system font stack,
//! a restrained warm palette, `color-scheme: light dark` with a real dark
//! block, and whitespace instead of borders wherever a border can be avoided.

use rustlavel_core::Json;

/// Everything the page needs baked in at mount time.
pub struct PageOptions {
    /// Where the routes live, so the script knows what to poll.
    pub mount: String,
    /// The application name, shown next to the wordmark.
    pub app: String,
    /// Entries at or above this many milliseconds are highlighted as slow.
    pub slow_ms: f64,
    pub capacity: usize,
}

/// The rendered page, split around the one part that changes.
///
/// Everything except the first batch of entries is fixed the moment Telescope
/// is mounted, so the shell is built once and a request only pays for
/// serialising the entries it is about to show. Embedding that first batch —
/// rather than letting the script fetch it — means the page paints with real
/// data instead of flashing an empty table for a round trip.
pub struct Page {
    head: String,
    tail: String,
}

impl Page {
    pub fn new(options: &PageOptions) -> Page {
        let config = Json::object([
            ("mount", Json::from(options.mount.as_str())),
            ("slow", Json::from(options.slow_ms)),
            ("capacity", Json::from(options.capacity)),
        ]);

        let mut head = String::with_capacity(CSS.len() + 4096);
        head.push_str("<!doctype html>\n<html lang=\"en\">\n<head>\n");
        head.push_str("<meta charset=\"utf-8\">\n");
        head.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
        head.push_str("<title>Telescope — ");
        head.push_str(&escape(&options.app));
        head.push_str("</title>\n<style>");
        head.push_str(CSS);
        head.push_str("</style>\n</head>\n<body>\n");
        head.push_str(BODY);
        head.push_str("\n<script>const TELESCOPE = ");
        head.push_str(&config.to_string());
        head.push_str(";\nconst TELESCOPE_INITIAL = ");

        let mut tail = String::with_capacity(SCRIPT.len() + 64);
        tail.push_str(";</script>\n<script>");
        tail.push_str(SCRIPT);
        tail.push_str("</script>\n</body>\n</html>\n");

        Page { head, tail }
    }

    /// Render the page around one listing payload, as `Store::to_json` builds it.
    pub fn render(&self, initial: &Json) -> String {
        let mut out = String::with_capacity(self.head.len() + self.tail.len() + 2048);
        out.push_str(&self.head);
        // Core's serializer escapes `<`, so no recorded value can close the
        // script tag it is embedded in.
        out.push_str(&initial.to_string());
        out.push_str(&self.tail);
        out
    }
}

/// The application name reaches the page through the title, so it is escaped
/// here rather than trusted.
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

const BODY: &str = r#"<header class="bar">
  <span class="mark" aria-hidden="true"></span>
  <span class="wordmark">Telescope</span>
  <span class="app" id="app-name"></span>
  <span class="spacer"></span>
  <span class="pulse" id="pulse" title="Polling"></span>
  <label class="toggle"><input type="checkbox" id="live" checked><span>Live</span></label>
  <button class="ghost" id="clear">Clear</button>
</header>

<div class="filters">
  <input id="q" type="search" placeholder="Filter recorded entries…" autocomplete="off" spellcheck="false">
  <div class="chips" id="chips"></div>
</div>

<main>
  <table>
    <thead>
      <tr>
        <th class="c-time">Time</th>
        <th class="c-kind">Kind</th>
        <th>Entry</th>
        <th class="c-badge"></th>
        <th class="c-dur">Duration</th>
      </tr>
    </thead>
    <tbody id="rows"></tbody>
  </table>
  <p class="empty" id="empty">Nothing recorded yet. Make a request and it will appear here.</p>
  <noscript><p class="empty">Telescope needs JavaScript to poll for new entries. The raw data is
  available at <code>/api/entries</code> under this path.</p></noscript>
</main>

<footer id="foot"></footer>"#;

const CSS: &str = r#"
:root { color-scheme: light dark; --bg:#faf9f7; --fg:#1c1b1a; --muted:#6b6864; --line:#e5e2dd;
        --accent:#b4483c; --panel:#fff; --code:#f4f2ef; --hover:#f2efea; --chip-l:38%; --chip-a:12%; }
@media (prefers-color-scheme: dark) {
  :root { --bg:#181716; --fg:#eceae7; --muted:#9a958e; --line:#2e2c29; --accent:#e0796c;
          --panel:#201f1d; --code:#252321; --hover:#232120; --chip-l:70%; --chip-a:18%; }
}
* { box-sizing: border-box; }
body { margin:0; background:var(--bg); color:var(--fg);
       font:14px/1.55 ui-sans-serif,-apple-system,'Segoe UI',sans-serif; }
code, .mono { font-family:ui-monospace,SFMono-Regular,Menlo,monospace; }

.bar { position:sticky; top:0; z-index:5; display:flex; align-items:center; gap:10px;
       padding:14px 24px; background:var(--bg); border-bottom:1px solid var(--line); }
.mark { width:9px; height:9px; border-radius:50%; background:var(--accent);
        box-shadow:0 0 0 3px color-mix(in srgb, var(--accent) 18%, transparent); }
.wordmark { font-weight:650; letter-spacing:-.01em; }
.app { color:var(--muted); font-size:13px; }
.app:not(:empty)::before { content:'·'; margin-right:8px; }
.spacer { flex:1; }
.pulse { width:6px; height:6px; border-radius:50%; background:var(--line); transition:background .25s; }
.pulse.on { background:var(--accent); }
.toggle { display:inline-flex; align-items:center; gap:6px; font-size:13px; color:var(--muted);
          user-select:none; cursor:pointer; }
.toggle input { accent-color:var(--accent); margin:0; }
.ghost { font:inherit; font-size:13px; color:var(--muted); background:none; cursor:pointer;
         border:1px solid var(--line); border-radius:6px; padding:4px 12px; }
.ghost:hover { color:var(--fg); border-color:var(--muted); }

.filters { display:flex; align-items:center; gap:14px; flex-wrap:wrap;
           padding:16px 24px 12px; }
#q { flex:1; min-width:220px; font:inherit; font-size:13px; color:var(--fg); background:var(--panel);
     border:1px solid var(--line); border-radius:7px; padding:7px 11px; }
#q:focus { outline:none; border-color:var(--accent); }
#q::placeholder { color:var(--muted); }
.chips { display:flex; gap:6px; flex-wrap:wrap; }
.chip { font-size:12px; color:var(--muted); background:none; border:1px solid var(--line);
        border-radius:999px; padding:3px 11px; cursor:pointer; font-family:inherit; white-space:nowrap; }
.chip:hover { color:var(--fg); }
.chip.on { color:var(--fg); border-color:var(--fg); }
.chip .n { color:var(--muted); margin-left:6px; font-variant-numeric:tabular-nums; }

main { padding:0 24px 64px; }
table { width:100%; border-collapse:collapse; }
thead th { text-align:left; font-size:11px; font-weight:600; text-transform:uppercase;
           letter-spacing:.08em; color:var(--muted); padding:8px 12px; border-bottom:1px solid var(--line); }
.c-time { width:104px; } .c-kind { width:132px; } .c-badge { width:96px; } .c-dur { width:110px; }
tbody tr.row { border-bottom:1px solid var(--line); cursor:pointer; }
tbody tr.row:hover { background:var(--hover); }
tbody tr.row.open { background:var(--hover); }
tbody td { padding:9px 12px; vertical-align:top; }
td.time { color:var(--muted); font-size:12px; font-variant-numeric:tabular-nums;
          font-family:ui-monospace,SFMono-Regular,Menlo,monospace; white-space:nowrap; }
td.summary { font-family:ui-monospace,SFMono-Regular,Menlo,monospace; font-size:12.5px;
             word-break:break-word; }
td.dur { text-align:right; white-space:nowrap; position:relative;
         font-family:ui-monospace,SFMono-Regular,Menlo,monospace; font-size:12px;
         font-variant-numeric:tabular-nums; color:var(--muted); }
td.dur .bar { position:absolute; right:12px; bottom:4px; height:2px; border-radius:1px;
              background:color-mix(in srgb, var(--muted) 40%, transparent); }
td.dur.slow { color:var(--accent); font-weight:600; }
td.dur.slow .bar { background:var(--accent); }

.kind { font-size:11.5px; border-radius:5px; padding:2px 7px; white-space:nowrap;
        font-family:ui-monospace,SFMono-Regular,Menlo,monospace;
        background:hsl(var(--h) 55% 50% / var(--chip-a)); color:hsl(var(--h) 45% var(--chip-l)); }
.badge { font-size:11.5px; border-radius:5px; padding:2px 7px; white-space:nowrap;
         font-variant-numeric:tabular-nums; border:1px solid transparent; }
.badge.ok { color:#2f7d52; background:color-mix(in srgb,#2f7d52 12%, transparent); }
.badge.warn { color:#9a6a17; background:color-mix(in srgb,#9a6a17 14%, transparent); }
.badge.error { color:var(--accent); background:color-mix(in srgb, var(--accent) 14%, transparent); }
.badge.muted { color:var(--muted); background:color-mix(in srgb, var(--muted) 12%, transparent); }
@media (prefers-color-scheme: dark) {
  .badge.ok { color:#6cc48f; } .badge.warn { color:#d9a548; }
}

tr.detail > td { padding:0 12px 20px; background:var(--hover); }
.panel { background:var(--panel); border:1px solid var(--line); border-radius:8px; padding:16px 18px; }
.panel h2 { font-size:11px; text-transform:uppercase; letter-spacing:.08em; color:var(--muted);
            margin:0 0 10px; font-weight:600; }
.panel + .panel { margin-top:12px; }
.fields { width:100%; border-collapse:collapse; }
.fields th { text-align:left; width:150px; color:var(--muted); font-weight:500; font-size:12.5px;
             padding:3px 12px 3px 0; vertical-align:top; word-break:break-word; }
.fields td { font-family:ui-monospace,SFMono-Regular,Menlo,monospace; font-size:12.5px;
             padding:3px 0; white-space:pre-wrap; word-break:break-word; }
.related { list-style:none; margin:0; padding:0; }
.related li { display:flex; gap:10px; align-items:baseline; padding:3px 0; font-size:12.5px;
              font-family:ui-monospace,SFMono-Regular,Menlo,monospace; }
.related li .d { margin-left:auto; color:var(--muted); font-variant-numeric:tabular-nums; }

/* On a narrow screen the timestamp is the first thing worth giving up: the
   order of the rows already says when, and the summary is the point. */
@media (max-width: 700px) {
  .bar, .filters, main, footer { padding-left:14px; padding-right:14px; }
  .c-time, td.time { display:none; }
  .c-kind { width:auto; }
  .c-badge, .c-dur { width:auto; }
}

.empty { color:var(--muted); font-size:13px; padding:48px 12px; text-align:center; }
.empty[hidden] { display:none; }
footer { padding:0 24px 40px; color:var(--muted); font-size:12px; }
"#;

const SCRIPT: &str = r#"
const API = TELESCOPE.mount + '/api/entries';
const state = { entries: [], kinds: [], total: 0, kind: '', q: '', max: 0, open: null, details: {} };

const $ = (id) => document.getElementById(id);
const esc = (s) => String(s).replace(/[&<>"']/g, (c) =>
  ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]);

// An unknown kind still needs a stable colour, so the hue is derived from the
// name: `ai.call` will look like itself the first time it is ever recorded.
// The band skips the reds, which are spoken for — an entry is not a failure
// just because its name happened to hash there.
function hue(kind) {
  let h = 0;
  for (let i = 0; i < kind.length; i++) h = (h * 31 + kind.charCodeAt(i)) % 310;
  return 25 + h;
}

function time(ms) {
  const d = new Date(ms);
  const p = (n, w) => String(n).padStart(w, '0');
  return p(d.getHours(), 2) + ':' + p(d.getMinutes(), 2) + ':' + p(d.getSeconds(), 2)
    + '.' + p(d.getMilliseconds(), 3);
}

function duration(ms) {
  if (ms === null || ms === undefined) return '';
  if (ms >= 1000) return (ms / 1000).toFixed(2) + ' s';
  if (ms >= 10) return ms.toFixed(0) + ' ms';
  return ms.toFixed(1) + ' ms';
}

function visible() {
  const q = state.q.toLowerCase();
  return state.entries.filter((e) =>
    !q || e.summary.toLowerCase().includes(q) || e.kind.toLowerCase().includes(q));
}

function renderChips() {
  const total = state.kinds.reduce((sum, k) => sum + k.count, 0);
  const all = [{ kind: '', label: 'All', count: total }]
    .concat(state.kinds.map((k) => ({ kind: k.kind, label: k.kind, count: k.count })));
  $('chips').innerHTML = all.map((k) =>
    '<button class="chip' + (state.kind === k.kind ? ' on' : '') + '" data-kind="' + esc(k.kind)
    + '">' + esc(k.label) + '<span class="n">' + k.count + '</span></button>').join('');
}

function renderRows() {
  const rows = visible();
  const slowest = rows.reduce((m, e) => Math.max(m, e.duration_ms || 0), 0);
  $('empty').hidden = rows.length > 0;
  $('empty').textContent = state.entries.length
    ? 'No entry matches this filter.'
    : 'Nothing recorded yet. Make a request and it will appear here.';

  let html = '';
  for (const e of rows) {
    const slow = e.duration_ms !== null && e.duration_ms >= TELESCOPE.slow;
    const width = slowest > 0 && e.duration_ms ? Math.max(2, (e.duration_ms / slowest) * 72) : 0;
    html += '<tr class="row' + (state.open === e.id ? ' open' : '') + '" data-id="' + e.id + '">'
      + '<td class="time">' + time(e.at) + '</td>'
      + '<td><span class="kind" style="--h:' + hue(e.kind) + '">' + esc(e.kind) + '</span></td>'
      + '<td class="summary">' + esc(e.summary) + '</td>'
      + '<td>' + (e.badge ? '<span class="badge ' + esc(e.tone) + '">' + esc(e.badge) + '</span>' : '')
      + '</td>'
      + '<td class="dur' + (slow ? ' slow' : '') + '">' + duration(e.duration_ms)
      + (width ? '<span class="bar" style="width:' + width.toFixed(0) + 'px"></span>' : '')
      + '</td></tr>';
    if (state.open === e.id) html += '<tr class="detail"><td colspan="5">' + detail(e.id) + '</td></tr>';
  }
  $('rows').innerHTML = html;
}

function detail(id) {
  const data = state.details[id];
  if (!data) return '<div class="panel"><h2>Loading</h2></div>';

  const fields = Object.entries(data.entry.fields).map(([k, v]) =>
    '<tr><th>' + esc(k) + '</th><td>' + esc(typeof v === 'string' ? v : JSON.stringify(v, null, 1))
    + '</td></tr>').join('') || '<tr><th>—</th><td>no fields</td></tr>';

  let html = '<div class="panel"><h2>' + esc(data.entry.kind) + '</h2>'
    + '<table class="fields">' + fields + '</table></div>';

  if (data.related && data.related.length) {
    html += '<div class="panel"><h2>Recorded during this request</h2><ul class="related">'
      + data.related.map((r) =>
        '<li><span class="kind" style="--h:' + hue(r.kind) + '">' + esc(r.kind) + '</span>'
        + '<span>' + esc(r.summary) + '</span>'
        + '<span class="d">' + duration(r.duration_ms) + '</span></li>').join('')
      + '</ul></div>';
  }
  return html;
}

function renderFoot() {
  $('foot').textContent = state.total + ' of ' + TELESCOPE.capacity
    + ' entries held in memory · anything above ' + TELESCOPE.slow + ' ms is marked slow';
}

function render() { renderChips(); renderRows(); renderFoot(); }

function query(extra) {
  const parts = ['limit=200'];
  if (state.kind) parts.push('kind=' + encodeURIComponent(state.kind));
  if (extra) parts.push(extra);
  return API + '?' + parts.join('&');
}

function absorb(data) {
  state.kinds = data.kinds;
  state.total = data.total;
}

async function reload() {
  const data = await fetch(query()).then((r) => r.json());
  state.entries = data.entries;
  state.max = data.entries.reduce((m, e) => Math.max(m, e.id), state.max);
  absorb(data);
  render();
}

async function poll() {
  if (!$('live').checked || document.hidden) return;
  const data = await fetch(query('after=' + state.max)).then((r) => r.json());
  if (data.entries.length) {
    state.max = data.entries.reduce((m, e) => Math.max(m, e.id), state.max);
    state.entries = data.entries.concat(state.entries).slice(0, TELESCOPE.capacity);
  }
  absorb(data);
  render();
  $('pulse').classList.add('on');
  setTimeout(() => $('pulse').classList.remove('on'), 200);
}

async function open(id) {
  if (state.open === id) { state.open = null; render(); return; }
  state.open = id;
  render();
  if (!state.details[id]) {
    state.details[id] = await fetch(API + '/' + id).then((r) => (r.ok ? r.json() : null));
  }
  render();
}

$('rows').addEventListener('click', (event) => {
  const row = event.target.closest('tr.row');
  if (row) open(Number(row.dataset.id));
});
$('chips').addEventListener('click', (event) => {
  const chip = event.target.closest('.chip');
  if (!chip) return;
  state.kind = chip.dataset.kind;
  state.max = 0;
  state.open = null;
  reload();
});
$('q').addEventListener('input', (event) => { state.q = event.target.value; renderRows(); });
$('clear').addEventListener('click', async () => {
  await fetch(API, { method: 'DELETE' });
  state.entries = []; state.details = {}; state.open = null;
  reload();
});
document.addEventListener('keydown', (event) => {
  if (event.key === '/' && document.activeElement !== $('q')) { event.preventDefault(); $('q').focus(); }
  if (event.key === 'Escape') { state.open = null; $('q').blur(); render(); }
});

$('app-name').textContent = document.title.replace(/^Telescope — /, '');

// The first batch came embedded in the page, so there is nothing to wait for.
state.entries = TELESCOPE_INITIAL.entries;
state.max = state.entries.reduce((m, e) => Math.max(m, e.id), 0);
absorb(TELESCOPE_INITIAL);
render();
setInterval(poll, 2000);
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> PageOptions {
        PageOptions {
            mount: "/telescope".to_string(),
            app: "Rustlavel".to_string(),
            slow_ms: 100.0,
            capacity: 500,
        }
    }

    fn render(options: &PageOptions) -> String {
        Page::new(options).render(&Json::object([("entries", Json::Array(Vec::new()))]))
    }

    #[test]
    fn the_page_is_self_contained() {
        let html = render(&options());

        assert!(html.starts_with("<!doctype html>"));
        assert!(html.contains("<style>"));
        assert!(html.contains("<script>"));
        // Nothing may be fetched from the network: no CDN, no external assets.
        assert!(!html.contains("http://"));
        assert!(!html.contains("https://"));
        assert!(!html.contains("<link"));
    }

    #[test]
    fn the_page_carries_its_mount_point_and_thresholds() {
        let html = render(&options());

        assert!(html.contains("\"mount\":\"/telescope\""));
        assert!(html.contains("\"slow\":100"));
        assert!(html.contains("\"capacity\":500"));
    }

    #[test]
    fn the_page_follows_the_error_page_house_style() {
        let html = render(&options());

        assert!(html.contains("color-scheme: light dark"));
        assert!(html.contains("@media (prefers-color-scheme: dark)"));
        assert!(html.contains("ui-sans-serif"));
    }

    #[test]
    fn the_first_batch_of_entries_is_embedded_rather_than_fetched() {
        let page = Page::new(&options());
        let html = page.render(&Json::object([(
            "entries",
            Json::Array(vec![Json::object([("summary", Json::from("GET /users"))])]),
        )]));

        assert!(html.contains("const TELESCOPE_INITIAL = "));
        assert!(html.contains("GET /users"));
    }

    #[test]
    fn an_application_name_cannot_inject_markup() {
        let html = render(&PageOptions { app: "<script>x</script>".into(), ..options() });

        assert!(!html.contains("<title>Telescope — <script>"));
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn a_recorded_value_cannot_close_the_script_tag_it_is_embedded_in() {
        let page = Page::new(&options());
        let html = page.render(&Json::object([("entries", Json::from("</script><img>"))]));

        assert!(!html.contains("</script><img>"));
        assert!(html.contains("\\u003c/script>"));
    }
}
