# rustlavel-chart

Charts for [Rustlavel](https://github.com/advancedynamic/rustlavel): a typed builder that produces a Chart.js configuration, and the vendored library to draw it — no CDN, no inline script.

## This package vendors somebody else's library

That is unusual here, and deliberate. Chart.js is 69 KB gzipped and renders to a `<canvas>`; writing the same thing from scratch means an axis scaler, a tick formatter, a legend layout engine and a hit-tester, and the result still has no hover, no tooltips and no zoom. The copy lives in `assets/`, under its own MIT licence, with its copyright banner intact.

## Using it

```rust
App::new()?.plugin(Charts::default())      // serves the two scripts
```

```rust
let revenue = Chart::line("Revenue")
    .labels(["Jan", "Feb", "Mar"])
    .series(Series::new("2026", [1200.0, 1800.0, 1500.0]).filled())
    .series(Series::sparse("2025", [Some(900.0), None, Some(1400.0)]))
    .y(Axis::new().prefix("Rp "));

req.view("dashboard", &ViewContext::new().with("revenue", revenue.to_json()))
```

```html
<script src="/vendor/chart/chart.umd.js" defer></script>
<script src="/vendor/chart/rustlavel-chart.js" defer></script>

<canvas data-chart="{{ revenue }}" height="220"></canvas>
```

`Chart::line`, `bar`, `horizontal_bar`, `doughnut`, `pie`, `scatter`. A horizontal bar is a `bar` with the axes swapped, so the axis you set on `y` stays the category axis whichever way round it is drawn.

## The configuration travels in an escaped attribute

`{{ }}` turns every `"` into `&quot;` and every `<` into `&lt;`; the browser reverses that when it reads `dataset.chart`, and the round trip is exact. A label reading `a" onmouseover="alert(1)` arrives in Chart.js as that string — measured, not assumed.

This is why there is no inline `<script>` anywhere in this package. A page that writes `<script>new Chart(…)</script>` needs `script-src 'unsafe-inline'`, and a policy with `unsafe-inline` does not stop injected script — which is the one thing a Content-Security-Policy is for.

**Under `default-src 'self'` your stylesheet must be external too.** That policy blocks an inline `<style>` exactly as firmly as an inline `<script>`, and the theme hook below is CSS. The auth kit already ships its stylesheet as a file, so there is nothing to do there; a page with a `<style>` block will find its rules silently not applied.

## Following the page's theme

Chart.js draws its own labels and grid lines, and its defaults are for a white page. The init script reads two CSS custom properties and hands them over, so a chart follows the theme the rest of the page is using:

```css
:root { --chart-ink: #1f2937; --chart-grid: #e5e7eb; }
@media (prefers-color-scheme: dark) {
  :root { --chart-ink: #e5e7eb; --chart-grid: #374151; }
}
```

Set neither and Chart.js's own defaults apply.

## Choices that are not Chart.js's

- **The value axis begins at zero.** A bar chart whose axis starts at 95 turns a 1% difference into a doubling. `Axis::zoomed()` when the reading genuinely lives in a narrow band far from zero.
- **Lines are straight.** Chart.js defaults `tension` to 0.4, drawing a curve through points nobody measured. `Series::smooth()` asks for it.
- **A gap is a gap.** `Series::sparse` takes `Option<f64>`, and `spanGaps` is off — a month with no reading breaks the line rather than being drawn as zero.
- **Seven colours**, separated by lightness as well as hue so they survive both themes and deuteranopia, wrapping rather than panicking on an eighth series.

## `problems()`

Returns what is wrong with a chart rather than refusing to draw it: a series shorter than its labels, a doughnut given several series, a chart with nothing in it. Worth an assertion in a dashboard's tests — Chart.js silently draws the shorter of two mismatched lengths.

## Licence

MIT. The vendored Chart.js is MIT too; see `assets/LICENSE-chart.js.md`.
