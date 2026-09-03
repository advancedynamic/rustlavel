//! A very small PDF writer, for the Export PDF button on the audit page.
//!
//! Not a general PDF library and not trying to be one. It writes what a table
//! export needs and nothing else: a page size, one built-in font, lines of
//! text at fixed positions, and a rule. That is enough for a printable audit
//! trail, and it means the export does not pull in a dependency the project's
//! rules would not allow anyway.
//!
//! The one real subtlety is the cross-reference table at the end. A PDF reader
//! finds every object through byte offsets recorded there, so the offsets have
//! to be counted as the file is built — get one wrong and the file opens as
//! blank rather than as an error, which is the worst way for this to fail.

/// Points. A4 at 72dpi, landscape, because an audit row is wide.
const WIDTH: f64 = 841.89;
const HEIGHT: f64 = 595.28;
const MARGIN: f64 = 32.0;

/// Helvetica's own metric: 0.5 em is close enough to average for laying out a
/// monospaced-looking table without embedding a font.
const CHAR_WIDTH: f64 = 0.5;

/// One page's worth of drawing commands.
#[derive(Default)]
struct Page {
    content: String,
}

/// A document being written.
pub struct Document {
    pages: Vec<Page>,
    current: Page,
    y: f64,
    title: String,
}

impl Document {
    pub fn new(title: impl Into<String>) -> Document {
        Document { pages: Vec::new(), current: Page::default(), y: HEIGHT - MARGIN, title: title.into() }
    }

    /// How much room is left before the page has to break.
    pub fn remaining(&self) -> f64 {
        self.y - MARGIN
    }

    pub fn heading(&mut self, text: &str) {
        self.line_at(MARGIN, 15.0, text, true);
        self.y -= 6.0;
    }

    pub fn text(&mut self, text: &str, size: f64) {
        self.line_at(MARGIN, size, text, false);
    }

    /// A row of cells at fixed x positions, each truncated to its width.
    pub fn row(&mut self, cells: &[(f64, f64, String)], size: f64, bold: bool) {
        if self.remaining() < size * 1.6 {
            self.page_break();
        }
        self.y -= size * 1.4;
        for (x, width, text) in cells {
            let limit = ((width / (size * CHAR_WIDTH)) as usize).max(1);
            self.draw(*x, self.y, size, &truncate(text, limit), bold);
        }
    }

    pub fn rule(&mut self) {
        self.y -= 4.0;
        self.current.content.push_str(&format!(
            "0.8 G 0.5 w {MARGIN} {:.2} m {:.2} {:.2} l S\n",
            self.y,
            WIDTH - MARGIN,
            self.y
        ));
        self.y -= 2.0;
    }

    pub fn page_break(&mut self) {
        let finished = std::mem::take(&mut self.current);
        self.pages.push(finished);
        self.y = HEIGHT - MARGIN;
    }

    fn line_at(&mut self, x: f64, size: f64, text: &str, bold: bool) {
        if self.remaining() < size * 1.6 {
            self.page_break();
        }
        self.y -= size * 1.4;
        self.draw(x, self.y, size, text, bold);
    }

    fn draw(&mut self, x: f64, y: f64, size: f64, text: &str, bold: bool) {
        let font = if bold { "/F2" } else { "/F1" };
        self.current.content.push_str(&format!(
            "BT {font} {size} Tf 0 g {x:.2} {y:.2} Td ({}) Tj ET\n",
            escape(text)
        ));
    }

    /// The finished file.
    pub fn finish(mut self) -> Vec<u8> {
        let last = std::mem::take(&mut self.current);
        if !last.content.is_empty() || self.pages.is_empty() {
            self.pages.push(last);
        }

        // Object 1 is the catalogue, 2 the page tree, 3 and 4 the two fonts,
        // then a pair per page: the page object and its content stream.
        let first_page_object = 5;
        let page_ids: Vec<usize> =
            (0..self.pages.len()).map(|i| first_page_object + i * 2).collect();

        let mut objects: Vec<String> = Vec::new();
        objects.push("<< /Type /Catalog /Pages 2 0 R >>".to_string());
        objects.push(format!(
            "<< /Type /Pages /Count {} /Kids [{}] >>",
            self.pages.len(),
            page_ids.iter().map(|id| format!("{id} 0 R")).collect::<Vec<_>>().join(" ")
        ));
        objects.push("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string());
        objects.push("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica-Bold >>".to_string());

        let mut streams: Vec<String> = Vec::new();
        for (index, page) in self.pages.iter().enumerate() {
            let content_id = page_ids[index] + 1;
            objects.push(format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {WIDTH:.2} {HEIGHT:.2}] \
                 /Resources << /Font << /F1 3 0 R /F2 4 0 R >> >> /Contents {content_id} 0 R >>"
            ));
            objects.push(String::new()); // placeholder, filled below
            streams.push(page.content.clone());
        }

        let mut out = Vec::new();
        out.extend_from_slice(b"%PDF-1.4\n");
        let mut offsets = Vec::with_capacity(objects.len());

        let mut stream_index = 0;
        for (index, body) in objects.iter().enumerate() {
            offsets.push(out.len());
            let number = index + 1;

            if body.is_empty() {
                // A content stream: its length has to be declared, so it is
                // written here rather than kept as a string above.
                let content = &streams[stream_index];
                stream_index += 1;
                out.extend_from_slice(
                    format!("{number} 0 obj\n<< /Length {} >>\nstream\n", content.len()).as_bytes(),
                );
                out.extend_from_slice(content.as_bytes());
                out.extend_from_slice(b"endstream\nendobj\n");
            } else {
                out.extend_from_slice(format!("{number} 0 obj\n{body}\nendobj\n").as_bytes());
            }
        }

        let xref_at = out.len();
        out.extend_from_slice(format!("xref\n0 {}\n", objects.len() + 1).as_bytes());
        out.extend_from_slice(b"0000000000 65535 f \n");
        for offset in &offsets {
            out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R /Info << /Title ({}) >> >>\nstartxref\n{xref_at}\n%%EOF\n",
                objects.len() + 1,
                escape(&self.title)
            )
            .as_bytes(),
        );

        out
    }
}

/// The three characters that are syntax inside a PDF string, and anything a
/// byte-oriented reader would choke on.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '(' | ')' | '\\' => {
                out.push('\\');
                out.push(character);
            }
            // WinAnsi is a one-byte encoding, and there is no embedded font to
            // widen it. A character outside it becomes a question mark rather
            // than a mis-rendered box.
            c if (c as u32) < 32 => out.push(' '),
            c if (c as u32) < 127 => out.push(c),
            _ => out.push('?'),
        }
    }
    out
}

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let kept: String = text.chars().take(limit.saturating_sub(1)).collect();
    format!("{kept}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_file_is_a_pdf_with_a_cross_reference_table_that_points_at_real_objects() {
        let mut doc = Document::new("Audit Logs");
        doc.heading("Audit Logs");
        doc.rule();
        doc.row(&[(32.0, 100.0, "2026-09-03".into()), (140.0, 200.0, "Ada Lovelace".into())], 9.0, false);

        let bytes = doc.finish();
        let text = String::from_utf8_lossy(&bytes);

        assert!(text.starts_with("%PDF-1.4"), "not a PDF");
        assert!(text.ends_with("%%EOF\n"), "no trailer");
        assert!(text.contains("/Type /Catalog"));

        // Every offset in the xref must land on the start of the object it
        // claims. A reader that follows a wrong one shows a blank document
        // rather than an error, so this is the assertion that matters.
        let xref_at: usize = text
            .rsplit("startxref\n")
            .next()
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .parse()
            .unwrap();
        let xref = &text[xref_at..];
        for (index, line) in xref.lines().skip(2).take_while(|l| l.ends_with(" n ")).enumerate() {
            let offset: usize = line.split_whitespace().next().unwrap().parse().unwrap();
            let expected = format!("{} 0 obj", index + 1);
            assert!(
                text[offset..].starts_with(&expected),
                "object {} is not at {offset}: found {:?}",
                index + 1,
                &text[offset..offset + 20.min(text.len() - offset)]
            );
        }
    }

    #[test]
    fn a_long_document_breaks_onto_more_pages() {
        let mut doc = Document::new("Long");
        for i in 0..200 {
            doc.row(&[(32.0, 400.0, format!("row {i}"))], 9.0, false);
        }
        let text = String::from_utf8_lossy(&doc.finish()).to_string();

        let pages = text.matches("/Type /Page ").count();
        assert!(pages > 1, "200 rows fitted on one page, which they do not");
        assert!(text.contains(&format!("/Count {pages}")), "the page tree disagrees with the pages");
    }

    #[test]
    fn parentheses_and_backslashes_cannot_end_a_string_early() {
        assert_eq!(escape("a(b)c\\d"), "a\\(b\\)c\\\\d");
        assert_eq!(escape("tab\there"), "tab here");
        assert_eq!(escape("café"), "caf?");
    }

    #[test]
    fn a_cell_is_truncated_to_what_fits() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 8), "hello w…");
    }
}
