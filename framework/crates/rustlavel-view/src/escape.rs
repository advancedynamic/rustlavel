//! HTML escaping — the one thing a template engine must never get wrong.

/// Escape text so it can be interpolated anywhere in an HTML document body or
/// attribute value.
///
/// `{{ }}` runs this on every value, always. There is no "escape this one"
/// helper and no way to turn it off: a view that genuinely needs markup asks
/// for it loudly with `{!! !!}`, so the dangerous case is the one that stands
/// out in review.
///
/// Both quote characters are escaped, which is what makes unquoted-ish
/// attribute contexts (`<a title={{ x }}>`) safe rather than merely usually
/// safe.
pub fn escape(input: &str) -> String {
    // The overwhelming majority of interpolated values are clean; walking twice
    // is cheaper than allocating a second string for them.
    if !input.bytes().any(|byte| matches!(byte, b'&' | b'<' | b'>' | b'"' | b'\'')) {
        return input.to_string();
    }

    let mut out = String::with_capacity(input.len() + 16);
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_every_character_that_can_change_markup() {
        assert_eq!(escape("a & b"), "a &amp; b");
        assert_eq!(escape("<b>"), "&lt;b&gt;");
        assert_eq!(escape("say \"hi\""), "say &quot;hi&quot;");
        assert_eq!(escape("it's"), "it&#39;s");
    }

    #[test]
    fn a_script_tag_in_data_cannot_break_out() {
        let escaped = escape("<script>alert('xss')</script>");

        assert!(!escaped.contains('<'));
        assert!(!escaped.contains('>'));
        assert_eq!(escaped, "&lt;script&gt;alert(&#39;xss&#39;)&lt;/script&gt;");
    }

    #[test]
    fn clean_text_passes_through_unchanged() {
        assert_eq!(escape("Ada Lovelace — 1843"), "Ada Lovelace — 1843");
    }
}
