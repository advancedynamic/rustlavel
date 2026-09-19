//! One line, or one set of bars.

use crate::palette::Palette;
use rustlavel_core::Json;

/// A named run of numbers.
///
/// A gap in the data is `None`, not zero: Chart.js breaks the line there,
/// which is what "we have no reading for March" looks like. Zero is a reading.
#[derive(Debug, Clone, PartialEq)]
pub struct Series {
    pub label: String,
    pub values: Vec<Option<f64>>,
    /// Overrides the palette. `None` lets the chart assign one by position,
    /// which is what keeps two charts on a page agreeing about which colour
    /// means which series.
    pub colour: Option<String>,
    /// Bars and lines only: stack with every other series naming the same
    /// group.
    pub stack: Option<String>,
    /// Lines only: fill the area beneath.
    pub filled: bool,
    /// Lines only. 0 is straight segments; Chart.js's own default is 0.4,
    /// which invents a curve between points nobody measured.
    pub tension: f64,
    /// Plot against the right-hand axis. For a second unit — rupiah on the
    /// left, a count on the right.
    pub right_axis: bool,
}

impl Series {
    pub fn new(label: impl Into<String>, values: impl IntoIterator<Item = f64>) -> Series {
        Series {
            label: label.into(),
            values: values.into_iter().map(Some).collect(),
            colour: None,
            stack: None,
            filled: false,
            tension: 0.0,
            right_axis: false,
        }
    }

    /// With gaps. `None` breaks the line rather than drawing it through zero.
    pub fn sparse(label: impl Into<String>, values: impl IntoIterator<Item = Option<f64>>) -> Series {
        Series { values: values.into_iter().collect(), ..Series::new(label, []) }
    }

    pub fn colour(mut self, colour: impl Into<String>) -> Series {
        self.colour = Some(colour.into());
        self
    }

    pub fn stack(mut self, group: impl Into<String>) -> Series {
        self.stack = Some(group.into());
        self
    }

    pub fn filled(mut self) -> Series {
        self.filled = true;
        self
    }

    /// Round the corners between points. Only honest when the thing measured
    /// really is continuous.
    pub fn smooth(mut self) -> Series {
        self.tension = 0.35;
        self
    }

    pub fn right_axis(mut self) -> Series {
        self.right_axis = true;
        self
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// The Chart.js dataset object. `index` picks the palette entry when the
    /// series named no colour.
    pub(crate) fn to_json(&self, index: usize, palette: &Palette, per_point: bool) -> Json {
        let colour = self.colour.clone().unwrap_or_else(|| palette.at(index).to_string());
        let data = Json::Array(
            self.values.iter().map(|value| value.map_or(Json::Null, Json::from)).collect(),
        );

        // A pie or doughnut has one dataset and a colour per slice; everything
        // else has one colour per dataset.
        let background = if per_point {
            Json::Array((0..self.values.len()).map(|n| Json::from(palette.at(n))).collect())
        } else if self.filled {
            Json::from(palette.translucent(&colour).as_str())
        } else {
            Json::from(colour.as_str())
        };

        let mut fields = vec![
            ("label", Json::from(self.label.as_str())),
            ("data", data),
            ("borderColor", Json::from(colour.as_str())),
            ("backgroundColor", background),
            ("borderWidth", Json::from(2)),
            ("fill", Json::from(self.filled)),
            ("tension", Json::from(self.tension)),
            // Draw the line across a `None` rather than leaving a gap? No.
            ("spanGaps", Json::from(false)),
        ];
        if let Some(stack) = &self.stack {
            fields.push(("stack", Json::from(stack.as_str())));
        }
        if self.right_axis {
            fields.push(("yAxisID", Json::from("right")));
        }
        Json::object(fields)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_gap_is_null_and_is_not_bridged() {
        let series = Series::sparse("visits", [Some(1.0), None, Some(3.0)]);
        let json = series.to_json(0, &Palette::default(), false);

        let data = json.get("data").and_then(Json::as_array).unwrap();
        assert_eq!(data.len(), 3);
        assert!(data[1].is_null(), "a missing reading became a number");
        assert_eq!(
            json.get("spanGaps").and_then(Json::as_bool),
            Some(false),
            "the line would be drawn straight through a month nobody measured"
        );
    }

    /// Chart.js defaults `tension` to 0.4 — a curve through points that were
    /// never measured. A reading is a reading; the default here is straight.
    #[test]
    fn lines_are_straight_unless_asked_to_be_smooth() {
        assert_eq!(Series::new("a", [1.0]).tension, 0.0);
        assert_eq!(Series::new("a", [1.0]).smooth().tension, 0.35);
    }

    #[test]
    fn a_named_colour_beats_the_palette() {
        let palette = Palette::default();
        let by_position = Series::new("a", [1.0]).to_json(1, &palette, false);
        assert_eq!(by_position.get("borderColor").and_then(Json::as_str), Some(palette.at(1)));

        let named = Series::new("a", [1.0]).colour("#123456").to_json(1, &palette, false);
        assert_eq!(named.get("borderColor").and_then(Json::as_str), Some("#123456"));
    }

    #[test]
    fn a_slice_chart_colours_each_point_rather_than_the_set() {
        let json = Series::new("share", [1.0, 2.0, 3.0]).to_json(0, &Palette::default(), true);
        let background = json.get("backgroundColor").and_then(Json::as_array).unwrap();
        assert_eq!(background.len(), 3, "every slice would have been the same colour");
    }
}
