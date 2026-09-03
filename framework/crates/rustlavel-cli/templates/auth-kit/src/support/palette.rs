//! Turning one colour an administrator picked into a whole usable ramp.
//!
//! Settings → Appearance offers a single brand colour, because that is the
//! question a person can actually answer. Everything on the page — buttons,
//! links, focus rings, the active item in the sidebar, a badge — is drawn from
//! eleven shades of it, and asking for eleven hex codes would produce a ramp
//! nobody could read.
//!
//! So the **hue** comes from the choice and the **structure** stays. The
//! lightness ladder below is the one the built-in palette uses, and it is what
//! makes white-on-brand-600 legible and brand-50 usable as a tint. Keeping it
//! means a person who picks an unusual colour gets an unusual-looking site
//! rather than an unreadable one. The chroma is scaled by how saturated their
//! choice is, so a deliberately muted brand stays muted through the whole ramp.
//!
//! The conversion is sRGB → linear → OKLab → OKLCh, with Björn Ottosson's
//! published matrices. CSS could almost do this itself with `oklch(from ...)`,
//! and one day it will; today Safari is too recent for it to be the only path.

/// `(lightness, chroma)` per step, and the step's name.
///
/// Taken from the default palette rather than invented: those numbers were
/// chosen so that 600 on white and 400 on near-black both clear WCAG AA, and a
/// ramp that drifts from them stops being safe to put text on.
const LADDER: [(&str, f64, f64); 11] = [
    ("50", 0.970, 0.014),
    ("100", 0.932, 0.032),
    ("200", 0.882, 0.059),
    ("300", 0.809, 0.105),
    ("400", 0.707, 0.165),
    ("500", 0.623, 0.214),
    ("600", 0.546, 0.245),
    ("700", 0.488, 0.243),
    ("800", 0.424, 0.199),
    ("900", 0.379, 0.146),
    ("950", 0.284, 0.109),
];

/// The chroma of the reference palette at step 500, which the chosen colour's
/// own chroma is measured against.
const REFERENCE_CHROMA: f64 = 0.214;

/// The eleven `--color-brand-*` declarations for a chosen hex colour.
///
/// Returns an empty string for anything that is not a colour, so a malformed
/// setting leaves the built-in palette alone rather than blanking the site.
pub fn brand_ramp(hex: &str) -> String {
    let Some((r, g, b)) = parse_hex(hex) else {
        return String::new();
    };
    let (_, chroma, hue) = oklch(r, g, b);

    // A grey has no hue worth keeping, and scaling by its chroma would flatten
    // the whole ramp to grey — which is not a brand, it is the absence of one.
    let scale = if chroma < 0.01 { 1.0 } else { (chroma / REFERENCE_CHROMA).clamp(0.35, 1.25) };

    LADDER
        .iter()
        .map(|(step, lightness, step_chroma)| {
            format!(
                "  --color-brand-{step}: oklch({lightness:.3} {:.3} {hue:.1});\n",
                step_chroma * scale
            )
        })
        .collect()
}

/// `#rgb` or `#rrggbb` as three channels in 0..=1.
fn parse_hex(hex: &str) -> Option<(f64, f64, f64)> {
    let digits: Vec<u8> = hex.trim().trim_start_matches('#').bytes().collect();
    let channel = |value: u32| f64::from(value) / 255.0;

    match digits.len() {
        3 => {
            let pairs: Option<Vec<u32>> = digits
                .iter()
                .map(|d| (*d as char).to_digit(16).map(|v| v * 17))
                .collect();
            let pairs = pairs?;
            Some((channel(pairs[0]), channel(pairs[1]), channel(pairs[2])))
        }
        6 => {
            let text = std::str::from_utf8(&digits).ok()?;
            let value = u32::from_str_radix(text, 16).ok()?;
            Some((
                channel((value >> 16) & 0xff),
                channel((value >> 8) & 0xff),
                channel(value & 0xff),
            ))
        }
        _ => None,
    }
}

/// sRGB in 0..=1 to OKLCh: lightness, chroma, hue in degrees.
fn oklch(r: f64, g: f64, b: f64) -> (f64, f64, f64) {
    let (r, g, b) = (linear(r), linear(g), linear(b));

    let l = (0.412_221_47 * r + 0.536_332_54 * g + 0.051_445_99 * b).cbrt();
    let m = (0.211_903_49 * r + 0.680_699_54 * g + 0.107_396_96 * b).cbrt();
    let s = (0.088_302_56 * r + 0.281_718_84 * g + 0.629_978_49 * b).cbrt();

    let lightness = 0.210_454_26 * l + 0.793_617_79 * m - 0.004_072_05 * s;
    let a = 1.977_998_5 * l - 2.428_592_2 * m + 0.450_593_7 * s;
    let b = 0.025_904_04 * l + 0.782_771_77 * m - 0.808_675_77 * s;

    let hue = b.atan2(a).to_degrees();
    (lightness, a.hypot(b), if hue < 0.0 { hue + 360.0 } else { hue })
}

/// Undo the sRGB transfer function.
fn linear(channel: f64) -> f64 {
    if channel <= 0.04045 {
        channel / 12.92
    } else {
        ((channel + 0.055) / 1.055).powf(2.4)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Known values, from Ottosson's own conversion: pure red is about
    /// L 0.628, C 0.258, h 29.2°.
    #[test]
    fn the_conversion_matches_published_values() {
        let (lightness, chroma, hue) = oklch(1.0, 0.0, 0.0);

        assert!((lightness - 0.628).abs() < 0.005, "lightness was {lightness}");
        assert!((chroma - 0.258).abs() < 0.005, "chroma was {chroma}");
        assert!((hue - 29.2).abs() < 0.5, "hue was {hue}");

        // White is fully light and has no hue to speak of.
        let (lightness, chroma, _) = oklch(1.0, 1.0, 1.0);
        assert!((lightness - 1.0).abs() < 0.001);
        assert!(chroma < 0.001);
    }

    #[test]
    fn the_ramp_keeps_the_chosen_hue_at_every_step() {
        let css = brand_ramp("#16a34a");

        assert_eq!(css.lines().count(), 11, "one declaration per step");
        let hues: Vec<&str> = css.lines().map(|line| line.rsplit(' ').next().unwrap()).collect();
        assert!(hues.windows(2).all(|pair| pair[0] == pair[1]), "the hue drifted: {hues:?}");
        assert!(css.contains("--color-brand-600:"));
        assert!(css.contains("--color-brand-50:"));
    }

    /// A grey brand must not flatten the ramp to grey. Scaling chroma by the
    /// chosen colour's own would do exactly that, so it is guarded.
    #[test]
    fn a_grey_choice_still_produces_a_ramp_with_contrast() {
        let css = brand_ramp("#888888");

        assert!(css.contains("--color-brand-600: oklch(0.546 0.245"), "got {css}");
    }

    #[test]
    fn three_digit_hex_works_and_rubbish_leaves_the_palette_alone() {
        assert_eq!(brand_ramp("#0af"), brand_ramp("#00aaff"));

        for rubbish in ["", "blue", "#12", "#1234567", "javascript:alert(1)"] {
            assert_eq!(brand_ramp(rubbish), "", "{rubbish} produced a declaration");
        }
    }
}
