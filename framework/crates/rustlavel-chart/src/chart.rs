//! The configuration Chart.js reads.

use crate::palette::Palette;
use crate::series::Series;
use rustlavel_core::Json;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Line,
    Bar,
    /// Bars lying down. For category names too long to stand under an axis —
    /// which is most category names.
    HorizontalBar,
    Doughnut,
    Pie,
    Radar,
    /// Points only. `Chart::scatter` takes pairs rather than labels.
    Scatter,
}

impl Kind {
    /// What Chart.js calls it. `HorizontalBar` is a `bar` turned on its side by
    /// an option, not a type of its own — it was one in Chart.js 2 and the name
    /// still gets written by people who remember that.
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Line => "line",
            Kind::Bar | Kind::HorizontalBar => "bar",
            Kind::Doughnut => "doughnut",
            Kind::Pie => "pie",
            Kind::Radar => "radar",
            Kind::Scatter => "scatter",
        }
    }

    /// Whether one dataset is coloured per point rather than as a whole.
    pub fn colours_each_point(self) -> bool {
        matches!(self, Kind::Doughnut | Kind::Pie)
    }

    /// Whether the chart has x and y axes to configure at all.
    pub fn has_axes(self) -> bool {
        matches!(self, Kind::Line | Kind::Bar | Kind::HorizontalBar | Kind::Scatter)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Legend {
    Top,
    Right,
    Bottom,
    Left,
    Hidden,
}

impl Legend {
    fn as_str(self) -> &'static str {
        match self {
            Legend::Top => "top",
            Legend::Right => "right",
            Legend::Bottom => "bottom",
            Legend::Left => "left",
            Legend::Hidden => "top",
        }
    }
}

/// One axis.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Axis {
    pub title: Option<String>,
    /// Start the scale at zero.
    ///
    /// **On by default for the value axis, and that is a judgement.** A bar
    /// chart whose axis starts at 95 makes a 1% difference look like a
    /// doubling. Charts that lie usually lie here. Call [`Axis::zoomed`] when
    /// the reading genuinely lives in a narrow band far from zero — a
    /// temperature, a latency percentile — and the shape is the point.
    pub begin_at_zero: bool,
    pub min: Option<f64>,
    pub max: Option<f64>,
    /// `"Rp "`, `"$"`.
    pub prefix: Option<String>,
    /// `"%"`, `" ms"`.
    pub suffix: Option<String>,
    pub stacked: bool,
    pub hidden: bool,
}

impl Axis {
    pub fn new() -> Axis {
        Axis { begin_at_zero: true, ..Axis::default() }
    }

    pub fn title(mut self, title: impl Into<String>) -> Axis {
        self.title = Some(title.into());
        self
    }

    /// Let the scale start where the data does. Say why in the surrounding
    /// copy; a reader cannot see an axis they did not look at.
    pub fn zoomed(mut self) -> Axis {
        self.begin_at_zero = false;
        self
    }

    pub fn range(mut self, min: f64, max: f64) -> Axis {
        self.min = Some(min);
        self.max = Some(max);
        self.begin_at_zero = false;
        self
    }

    pub fn prefix(mut self, prefix: impl Into<String>) -> Axis {
        self.prefix = Some(prefix.into());
        self
    }

    pub fn suffix(mut self, suffix: impl Into<String>) -> Axis {
        self.suffix = Some(suffix.into());
        self
    }

    pub fn stacked(mut self) -> Axis {
        self.stacked = true;
        self
    }

    pub fn hidden(mut self) -> Axis {
        self.hidden = true;
        self
    }

    fn to_json(&self, horizontal_categories: bool) -> Json {
        let mut fields = vec![
            ("display", Json::from(!self.hidden)),
            ("stacked", Json::from(self.stacked)),
        ];
        if !horizontal_categories {
            fields.push(("beginAtZero", Json::from(self.begin_at_zero)));
        }
        if let Some(min) = self.min {
            fields.push(("min", Json::from(min)));
        }
        if let Some(max) = self.max {
            fields.push(("max", Json::from(max)));
        }
        if let Some(title) = &self.title {
            fields.push((
                "title",
                Json::object([("display", Json::from(true)), ("text", Json::from(title.as_str()))]),
            ));
        }
        // Read by the init script, which installs a tick callback. Chart.js
        // wants a function here and a function cannot survive JSON.
        if self.prefix.is_some() || self.suffix.is_some() {
            fields.push((
                "rustlavelTicks",
                Json::object([
                    ("prefix", self.prefix.as_deref().map_or(Json::Null, Json::from)),
                    ("suffix", self.suffix.as_deref().map_or(Json::Null, Json::from)),
                ]),
            ));
        }
        Json::object(fields)
    }
}

/// A chart, as a builder.
#[derive(Debug, Clone, PartialEq)]
pub struct Chart {
    pub kind: Kind,
    pub title: Option<String>,
    pub labels: Vec<String>,
    pub series: Vec<Series>,
    pub palette: Palette,
    pub legend: Legend,
    pub x: Axis,
    pub y: Axis,
    /// A second value axis on the right, for a series in another unit.
    pub right: Option<Axis>,
    /// Let the canvas take the height its container gives it.
    pub aspect_ratio: Option<f64>,
    /// Points that are pairs rather than readings against a label.
    points: Vec<(String, Vec<(f64, f64)>)>,
}

impl Chart {
    pub fn new(kind: Kind) -> Chart {
        Chart {
            kind,
            title: None,
            labels: Vec::new(),
            series: Vec::new(),
            palette: Palette::default(),
            legend: Legend::Top,
            x: Axis { begin_at_zero: false, ..Axis::default() },
            y: Axis::new(),
            right: None,
            aspect_ratio: None,
            points: Vec::new(),
        }
    }

    pub fn line(title: impl Into<String>) -> Chart {
        Chart::new(Kind::Line).title(title)
    }

    pub fn bar(title: impl Into<String>) -> Chart {
        Chart::new(Kind::Bar).title(title)
    }

    pub fn horizontal_bar(title: impl Into<String>) -> Chart {
        Chart::new(Kind::HorizontalBar).title(title)
    }

    pub fn doughnut(title: impl Into<String>) -> Chart {
        Chart::new(Kind::Doughnut).title(title)
    }

    pub fn pie(title: impl Into<String>) -> Chart {
        Chart::new(Kind::Pie).title(title)
    }

    /// Points in two dimensions, which have no category labels at all.
    pub fn scatter(title: impl Into<String>, label: impl Into<String>, points: impl IntoIterator<Item = (f64, f64)>) -> Chart {
        let mut chart = Chart::new(Kind::Scatter).title(title);
        chart.points.push((label.into(), points.into_iter().collect()));
        chart.x = Axis::new();
        chart
    }

    pub fn title(mut self, title: impl Into<String>) -> Chart {
        self.title = Some(title.into());
        self
    }

    /// Drop the title from the chart itself — for a card whose heading already
    /// says it, so a screen reader does not read it twice.
    pub fn untitled(mut self) -> Chart {
        self.title = None;
        self
    }

    pub fn labels(mut self, labels: impl IntoIterator<Item = impl Into<String>>) -> Chart {
        self.labels = labels.into_iter().map(Into::into).collect();
        self
    }

    pub fn series(mut self, series: Series) -> Chart {
        self.series.push(series);
        self
    }

    /// A single unnamed run — the common case for a doughnut or a sparkline.
    pub fn values(self, label: impl Into<String>, values: impl IntoIterator<Item = f64>) -> Chart {
        self.series(Series::new(label, values))
    }

    pub fn palette(mut self, palette: Palette) -> Chart {
        self.palette = palette;
        self
    }

    pub fn legend(mut self, legend: Legend) -> Chart {
        self.legend = legend;
        self
    }

    pub fn x(mut self, axis: Axis) -> Chart {
        self.x = axis;
        self
    }

    pub fn y(mut self, axis: Axis) -> Chart {
        self.y = axis;
        self
    }

    /// A second value axis on the right, for [`Series::right_axis`].
    pub fn right(mut self, axis: Axis) -> Chart {
        self.right = Some(axis);
        self
    }

    /// Stack every series, on both axes that can be stacked.
    pub fn stacked(mut self) -> Chart {
        self.x = Axis { stacked: true, ..self.x };
        self.y = Axis { stacked: true, ..self.y };
        self
    }

    pub fn aspect_ratio(mut self, ratio: f64) -> Chart {
        self.aspect_ratio = Some(ratio);
        self
    }

    /// Every series is as long as the labels, and there is something to draw.
    ///
    /// Not enforced — a chart is not worth refusing to render a page over —
    /// but a dashboard can call it in a test, and the first line of a
    /// mismatched chart is a silently truncated one.
    pub fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if self.series.is_empty() && self.points.is_empty() {
            problems.push("the chart has no series, so it will draw an empty grid".to_string());
        }
        if self.kind != Kind::Scatter {
            for series in &self.series {
                if series.len() != self.labels.len() {
                    problems.push(format!(
                        "series `{}` has {} values against {} labels; Chart.js draws the shorter of the two",
                        series.label,
                        series.len(),
                        self.labels.len()
                    ));
                }
            }
        }
        if self.kind.colours_each_point() && self.series.len() > 1 {
            problems.push(format!(
                "a {} shows one set of slices; {} series were given and the rest will be drawn as rings nobody can read",
                self.kind.as_str(),
                self.series.len()
            ));
        }
        problems
    }

    /// The configuration object, ready for a `data-chart` attribute.
    pub fn to_json(&self) -> Json {
        let datasets: Vec<Json> = if self.kind == Kind::Scatter {
            self.points
                .iter()
                .enumerate()
                .map(|(index, (label, points))| {
                    Json::object([
                        ("label", Json::from(label.as_str())),
                        (
                            "data",
                            Json::Array(
                                points
                                    .iter()
                                    .map(|(x, y)| {
                                        Json::object([("x", Json::from(*x)), ("y", Json::from(*y))])
                                    })
                                    .collect(),
                            ),
                        ),
                        ("borderColor", Json::from(self.palette.at(index))),
                        ("backgroundColor", Json::from(self.palette.at(index))),
                    ])
                })
                .collect()
        } else {
            self.series
                .iter()
                .enumerate()
                .map(|(index, series)| series.to_json(index, &self.palette, self.kind.colours_each_point()))
                .collect()
        };

        let mut plugins = vec![(
            "legend",
            Json::object([
                ("display", Json::from(self.legend != Legend::Hidden)),
                ("position", Json::from(self.legend.as_str())),
            ]),
        )];
        if let Some(title) = &self.title {
            plugins.push((
                "title",
                Json::object([("display", Json::from(true)), ("text", Json::from(title.as_str()))]),
            ));
        }

        let mut options = vec![
            ("responsive", Json::from(true)),
            ("maintainAspectRatio", Json::from(self.aspect_ratio.is_some())),
            ("plugins", Json::object(plugins)),
        ];
        if let Some(ratio) = self.aspect_ratio {
            options.push(("aspectRatio", Json::from(ratio)));
        }
        if self.kind == Kind::HorizontalBar {
            options.push(("indexAxis", Json::from("y")));
        }
        if self.kind.has_axes() {
            // Turned on its side, the categories are on y and the values on x.
            let flipped = self.kind == Kind::HorizontalBar;
            let (category, value) = if flipped { (&self.y, &self.x) } else { (&self.x, &self.y) };
            let mut scales = vec![
                ("x", if flipped { value.to_json(false) } else { category.to_json(true) }),
                ("y", if flipped { category.to_json(true) } else { value.to_json(false) }),
            ];
            if let Some(right) = &self.right {
                let mut axis = right.to_json(false);
                if let Json::Object(fields) = &mut axis {
                    fields.insert("position".to_string(), Json::from("right"));
                    // Two grids drawn over each other is a moiré pattern, not
                    // a chart.
                    fields.insert(
                        "grid".to_string(),
                        Json::object([("drawOnChartArea", Json::from(false))]),
                    );
                }
                scales.push(("right", axis));
            }
            options.push(("scales", Json::object(scales)));
        }

        Json::object([
            ("type", Json::from(self.kind.as_str())),
            (
                "data",
                Json::object([
                    ("labels", Json::Array(self.labels.iter().map(|l| Json::from(l.as_str())).collect())),
                    ("datasets", Json::Array(datasets)),
                ]),
            ),
            ("options", Json::object(options)),
        ])
    }
}

impl From<Chart> for Json {
    fn from(chart: Chart) -> Json {
        chart.to_json()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn revenue() -> Chart {
        Chart::line("Revenue")
            .labels(["Jan", "Feb", "Mar"])
            .series(Series::new("2026", [1.0, 2.0, 3.0]))
    }

    #[test]
    fn the_shape_is_what_chart_js_reads() {
        let json = revenue().to_json();
        assert_eq!(json.get("type").and_then(Json::as_str), Some("line"));
        assert_eq!(
            json.get("data.labels").and_then(Json::as_array).map(|l| l.len()),
            Some(3)
        );
        assert_eq!(
            json.get("data.datasets").and_then(Json::as_array).map(|d| d.len()),
            Some(1)
        );
        assert_eq!(json.get("options.plugins.title.text").and_then(Json::as_str), Some("Revenue"));
    }

    /// A bar chart whose axis starts at 95 turns a 1% difference into a
    /// doubling. The default has to be the honest one.
    #[test]
    fn the_value_axis_starts_at_zero_unless_asked_otherwise() {
        let json = revenue().to_json();
        assert_eq!(json.get("options.scales.y.beginAtZero").and_then(Json::as_bool), Some(true));

        let zoomed = revenue().y(Axis::new().zoomed()).to_json();
        assert_eq!(zoomed.get("options.scales.y.beginAtZero").and_then(Json::as_bool), Some(false));
    }

    #[test]
    fn a_horizontal_bar_is_a_bar_with_the_axes_swapped() {
        let chart = Chart::horizontal_bar("By channel")
            .labels(["Virtual account", "QRIS"])
            .values("count", [10.0, 20.0])
            .y(Axis::new().title("Channel"))
            .x(Axis::new().prefix("Rp "));
        let json = chart.to_json();

        assert_eq!(json.get("type").and_then(Json::as_str), Some("bar"));
        assert_eq!(json.get("options.indexAxis").and_then(Json::as_str), Some("y"));
        // The category axis is y, and it carries the axis the caller set on y.
        assert_eq!(json.get("options.scales.y.title.text").and_then(Json::as_str), Some("Channel"));
        // The value axis is x, and it is the one that begins at zero.
        assert_eq!(json.get("options.scales.x.beginAtZero").and_then(Json::as_bool), Some(true));
        assert_eq!(
            json.get("options.scales.x.rustlavelTicks.prefix").and_then(Json::as_str),
            Some("Rp ")
        );
    }

    #[test]
    fn a_second_axis_does_not_draw_a_second_grid() {
        let json = revenue()
            .series(Series::new("orders", [1.0, 2.0, 3.0]).right_axis())
            .right(Axis::new().title("Orders"))
            .to_json();

        assert_eq!(json.get("options.scales.right.position").and_then(Json::as_str), Some("right"));
        assert_eq!(
            json.get("options.scales.right.grid.drawOnChartArea").and_then(Json::as_bool),
            Some(false)
        );
        assert_eq!(
            json.get("data.datasets").and_then(Json::as_array).unwrap()[1]
                .get("yAxisID")
                .and_then(Json::as_str),
            Some("right")
        );
    }

    #[test]
    fn a_scatter_carries_pairs_rather_than_labels() {
        let json = Chart::scatter("Latency", "p95", [(1.0, 20.0), (2.0, 24.0)]).to_json();
        let points = json.get("data.datasets").and_then(Json::as_array).unwrap()[0]
            .get("data")
            .and_then(Json::as_array)
            .unwrap()
            .to_vec();
        assert_eq!(points.len(), 2);
        assert_eq!(points[1].get("x").and_then(Json::as_f64), Some(2.0));
        assert_eq!(points[1].get("y").and_then(Json::as_f64), Some(24.0));
    }

    #[test]
    fn problems_names_the_mismatch_rather_than_drawing_a_short_line() {
        let chart = Chart::line("x").labels(["Jan", "Feb", "Mar"]).series(Series::new("a", [1.0, 2.0]));
        let problems = chart.problems();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("2 values against 3 labels"), "{}", problems[0]);

        assert!(Chart::line("x").problems()[0].contains("no series"));
        assert!(revenue().problems().is_empty());

        let rings = Chart::doughnut("x")
            .labels(["a"])
            .values("one", [1.0])
            .series(Series::new("two", [1.0]));
        assert!(rings.problems()[0].contains("one set of slices"), "{:?}", rings.problems());
    }

    #[test]
    fn a_hidden_legend_is_hidden_but_still_has_a_valid_position() {
        let json = revenue().legend(Legend::Hidden).to_json();
        assert_eq!(json.get("options.plugins.legend.display").and_then(Json::as_bool), Some(false));
        assert_eq!(json.get("options.plugins.legend.position").and_then(Json::as_str), Some("top"));
    }
}
