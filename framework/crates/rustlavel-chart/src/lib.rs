//! rustlavel-chart: charts, drawn in the browser, described in Rust.
//!
//! **This package vendors somebody else's library, and that is unusual here.**
//! Chart.js is 69 KB gzipped and renders to a `<canvas>`; drawing the same
//! thing from scratch would mean writing an axis scaler, a tick formatter, a
//! legend layout engine and a hit-tester, and the result would still not have
//! hover, tooltips or zoom. The copy lives in `assets/`, under its own MIT
//! licence, and is served from this application's own origin — see [`plugin`].
//!
//! The Rust half never draws anything. It builds the *configuration* Chart.js
//! reads, which is a JSON object, and hands it to the page:
//!
//! ```ignore
//! let chart = Chart::line("Revenue")
//!     .labels(["Jan", "Feb", "Mar"])
//!     .series(Series::new("2026", [1_200.0, 1_800.0, 1_500.0]))
//!     .series(Series::new("2025", [900.0, 1_100.0, 1_400.0]));
//!
//! Ok(view("dashboard", context! { revenue: chart.to_json() })?)
//! ```
//!
//! ```html
//! <canvas data-chart="{{ revenue }}"></canvas>
//! ```
//!
//! # Why an attribute, and why `{{ }}`
//!
//! The configuration travels in an ordinary escaped attribute. `{{ }}` turns
//! every `"` into `&quot;` and every `<` into `&lt;`, the browser reverses that
//! when it reads `dataset.chart`, and the round trip is exact — so a label
//! reading `</script>` is a label, not an escape.
//!
//! This is the whole reason there is no inline `<script>` anywhere in this
//! package. A page that writes `<script>new Chart(…)</script>` needs
//! `script-src 'unsafe-inline'`, and a policy with `unsafe-inline` does not
//! stop injected script — which is the one thing a Content-Security-Policy is
//! for. The auth kit ships `default-src 'self'` with no `unsafe-inline`, and
//! everything here is built to satisfy that policy rather than to ask for an
//! exception to it.

pub mod chart;
pub mod palette;
pub mod plugin;
pub mod series;

pub use chart::{Axis, Chart, Kind, Legend};
pub use palette::Palette;
pub use plugin::{CHART_JS_PATH, Charts, INIT_JS_PATH};
pub use series::Series;

pub use rustlavel_core::{Error, Json, Result};
