//! Colours that survive both themes.

use rustlavel_core::Json;

/// The series colours, in the order they are handed out.
///
/// Chosen to stay legible on a white page and on a dark one, which rules out
/// the pale pastels a light-only palette can afford, and to be distinguishable
/// by someone with deuteranopia — so the set is separated by lightness as well
/// as hue, and red and green are never the only thing telling two series apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Palette {
    colours: Vec<String>,
}

/// Seven, because an eighth series on one chart is a table wearing a costume.
pub const DEFAULT_COLOURS: [&str; 7] = [
    "#2563eb", // blue
    "#ea580c", // orange
    "#0d9488", // teal
    "#c026d3", // magenta
    "#ca8a04", // amber
    "#4f46e5", // indigo
    "#dc2626", // red
];

impl Default for Palette {
    fn default() -> Palette {
        Palette { colours: DEFAULT_COLOURS.iter().map(|c| c.to_string()).collect() }
    }
}

impl Palette {
    pub fn new(colours: impl IntoIterator<Item = impl Into<String>>) -> Palette {
        let colours: Vec<String> = colours.into_iter().map(Into::into).collect();
        if colours.is_empty() {
            return Palette::default();
        }
        Palette { colours }
    }

    /// The colour at a position, wrapping round. Never panics, because the
    /// number of series is the caller's and a chart should not take the page
    /// down for having eight of them.
    pub fn at(&self, index: usize) -> &str {
        &self.colours[index % self.colours.len()]
    }

    pub fn len(&self) -> usize {
        self.colours.len()
    }

    pub fn is_empty(&self) -> bool {
        false
    }

    /// The same colour, see-through, for the area under a filled line.
    ///
    /// `#rrggbb` becomes `#rrggbb22`; anything else — a named colour, an
    /// `rgb()` — is handed back unchanged rather than mangled into something
    /// the browser will not parse.
    pub fn translucent(&self, colour: &str) -> String {
        let hex = colour.strip_prefix('#').unwrap_or("");
        if hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return format!("{colour}22");
        }
        colour.to_string()
    }

    pub fn to_json(&self) -> Json {
        Json::Array(self.colours.iter().map(|c| Json::from(c.as_str())).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours_wrap_rather_than_panicking_on_an_eighth_series() {
        let palette = Palette::default();
        assert_eq!(palette.at(0), DEFAULT_COLOURS[0]);
        assert_eq!(palette.at(7), DEFAULT_COLOURS[0]);
        assert_eq!(palette.at(9), DEFAULT_COLOURS[2]);
    }

    #[test]
    fn an_empty_palette_falls_back_rather_than_dividing_by_zero() {
        let palette = Palette::new(Vec::<String>::new());
        assert_eq!(palette.len(), DEFAULT_COLOURS.len());
        assert_eq!(palette.at(0), DEFAULT_COLOURS[0]);
    }

    #[test]
    fn only_a_six_digit_hex_gets_an_alpha_suffix() {
        let palette = Palette::default();
        assert_eq!(palette.translucent("#2563eb"), "#2563eb22");
        assert_eq!(palette.translucent("rebeccapurple"), "rebeccapurple");
        assert_eq!(palette.translucent("rgb(1,2,3)"), "rgb(1,2,3)");
        assert_eq!(palette.translucent("#abc"), "#abc", "a short hex would become invalid");
    }
}
