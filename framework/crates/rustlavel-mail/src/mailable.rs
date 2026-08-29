//! A type that describes its own email.
//!
//! An HTML email is a template like any other page: it lives in
//! `resources/views`, it is rendered by [`rustlavel_view::Engine`], and it is
//! edited by whoever edits the rest of the HTML.

use crate::message::Message;
use rustlavel_core::Result;
use rustlavel_view::{Context, Engine};

/// Something that knows what email it is.
///
/// ```ignore
/// struct OrderShipped { order: Json }
///
/// impl Mailable for OrderShipped {
///     fn subject(&self) -> String { format!("Order #{} has shipped", self.order["id"]) }
///     fn view(&self) -> &str { "mail.orders.shipped" }
///     fn context(&self) -> Context { Context::new().with("order", self.order.clone()) }
/// }
/// ```
pub trait Mailable {
    fn subject(&self) -> String;

    /// The view rendered as the HTML part: `"mail.orders.shipped"`.
    fn view(&self) -> &str;

    /// The data the view is rendered with.
    fn context(&self) -> Context {
        Context::new()
    }

    /// A view rendered as the plain-text part.
    ///
    /// Leave it out and the text part is derived from the HTML — see
    /// [`Mailable::build`] for why there is always one.
    fn text_view(&self) -> Option<&str> {
        None
    }

    /// Render this mailable into a message with no recipient yet.
    ///
    /// Both parts are always produced. Sending HTML alone costs deliverability
    /// — spam filters score a missing text part — and it fails outright for
    /// anyone reading mail in a terminal, on a watch, or with a screen reader.
    /// A derived text part is worse than a written one and far better than
    /// none, so the default is to derive it and the override is one method.
    fn build(&self, engine: &Engine) -> Result<Message> {
        let context = self.context();
        let html = engine.render(self.view(), &context)?;

        let text = match self.text_view() {
            Some(view) => engine.render(view, &context)?,
            None => html_to_text(&html),
        };

        Ok(Message::new().subject(self.subject()).text(text).html(html))
    }
}

/// The block-level tags that end a line of text.
const BREAKS_LINE: &[&str] = &[
    "p", "div", "br", "tr", "h1", "h2", "h3", "h4", "h5", "h6", "ul", "ol", "table", "section",
    "article", "header", "footer", "blockquote", "pre", "hr",
];

/// Turn rendered HTML into a readable plain-text part.
///
/// Not a browser: block tags become line breaks, list items get a bullet, and
/// a link keeps its target in parentheses — because a text part whose links
/// have vanished is not a fallback, it is a dead end.
pub fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut chars = html.chars().peekable();
    let mut skipping: Option<String> = None;
    let mut link: Option<String> = None;

    while let Some(ch) = chars.next() {
        if ch != '<' {
            if skipping.is_some() {
                continue;
            }
            push_text(&mut out, ch);
            continue;
        }

        let mut tag = String::new();
        let mut quote: Option<char> = None;
        for ch in chars.by_ref() {
            match quote {
                Some(q) if ch == q => quote = None,
                Some(_) => {}
                None if ch == '"' || ch == '\'' => quote = Some(ch),
                None if ch == '>' => break,
                None => {}
            }
            tag.push(ch);
        }

        let closing = tag.starts_with('/');
        let name: String = tag
            .trim_start_matches('/')
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();

        // A comment or a doctype is not a tag; drop it whole.
        if tag.starts_with('!') {
            continue;
        }

        if let Some(open) = &skipping {
            if closing && &name == open {
                skipping = None;
            }
            continue;
        }

        match name.as_str() {
            // Script and style contents are code, not text.
            "script" | "style" if !closing => skipping = Some(name.clone()),
            "a" if !closing => link = attribute(&tag, "href"),
            "a" if closing => {
                if let Some(href) = link.take()
                    && !href.is_empty()
                    && !out.ends_with(&href)
                {
                    out.push_str(&format!(" ({href})"));
                }
            }
            "li" if !closing => {
                trim_trailing_spaces(&mut out);
                out.push_str("\n- ");
            }
            other if BREAKS_LINE.contains(&other) => {
                trim_trailing_spaces(&mut out);
                out.push('\n');
            }
            _ => {}
        }
    }

    tidy(&out)
}

/// Append one character of text, collapsing runs of whitespace.
fn push_text(out: &mut String, ch: char) {
    if ch.is_whitespace() {
        if !out.ends_with(' ') && !out.ends_with('\n') && !out.is_empty() {
            out.push(' ');
        }
        return;
    }
    out.push(ch);
}

fn trim_trailing_spaces(out: &mut String) {
    while out.ends_with(' ') {
        out.pop();
    }
}

/// Read one attribute out of a tag's text.
fn attribute(tag: &str, name: &str) -> Option<String> {
    let lowered = tag.to_ascii_lowercase();
    let at = lowered.find(&format!("{name}="))?;
    let rest = tag[at + name.len() + 1..].trim_start();

    let value = match rest.chars().next()? {
        quote @ ('"' | '\'') => rest[1..].split(quote).next()?,
        _ => rest.split_whitespace().next()?,
    };

    Some(decode_entities(value))
}

/// Collapse the blank lines the tag walk leaves behind, and decode entities.
fn tidy(text: &str) -> String {
    let decoded = decode_entities(text);
    let mut out = String::with_capacity(decoded.len());
    let mut blank_run = 0;

    for line in decoded.split('\n') {
        let line = line.trim();
        if line.is_empty() {
            blank_run += 1;
            // One blank line separates paragraphs; more is just leftovers.
            if blank_run > 1 || out.is_empty() {
                continue;
            }
        } else {
            blank_run = 0;
        }
        out.push_str(line);
        out.push('\n');
    }

    out.trim_end().to_string()
}

/// The handful of entities that actually appear in rendered HTML.
fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }

    let mut out = String::with_capacity(text.len());
    let mut rest = text;

    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];

        // Bounded, and on character boundaries: an entity is short, and `&`
        // may be followed by anything at all.
        let terminator =
            tail.char_indices().take(12).find(|(_, c)| *c == ';').map(|(at, _)| at);
        let Some(end) = terminator else {
            out.push('&');
            rest = &tail[1..];
            continue;
        };

        let entity = &tail[1..end];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" => Some('\''),
            "nbsp" => Some(' '),
            numeric if numeric.starts_with("#x") || numeric.starts_with("#X") => {
                u32::from_str_radix(&numeric[2..], 16).ok().and_then(char::from_u32)
            }
            numeric if numeric.starts_with('#') => {
                numeric[1..].parse::<u32>().ok().and_then(char::from_u32)
            }
            _ => None,
        };

        match decoded {
            Some(ch) => {
                out.push(ch);
                rest = &tail[end + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }

    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustlavel_core::Json;
    use rustlavel_view::EXTENSION;
    use std::path::PathBuf;

    /// Each test writes its own view directory, because tests run at the same
    /// time and a shared fixture would be rewritten under one of them.
    fn views(test: &str, files: &[(&str, &str)]) -> PathBuf {
        let root = std::env::temp_dir().join(format!("rustlavel-mail-views-{test}"));
        let _ = std::fs::remove_dir_all(&root);
        for (name, source) in files {
            let path = root.join(format!("{}.{EXTENSION}", name.replace('.', "/")));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, source).unwrap();
        }
        root
    }

    struct OrderShipped {
        order: Json,
    }

    impl Mailable for OrderShipped {
        fn subject(&self) -> String {
            format!("Order #{} has shipped", self.order.get("id").unwrap())
        }

        fn view(&self) -> &str {
            "mail.orders.shipped"
        }

        fn context(&self) -> Context {
            Context::new().with("order", self.order.clone())
        }
    }

    fn an_order() -> OrderShipped {
        OrderShipped {
            order: Json::object([("id", Json::from(7)), ("customer", Json::from("Ada & Co"))]),
        }
    }

    #[test]
    fn a_mailable_renders_its_view_through_the_real_engine() {
        let engine = Engine::new(views(
            "mailable",
            &[(
                "mail.orders.shipped",
                "@extends(\"mail.layout\")\n\
                 @section(\"body\")\n\
                 <h1>Order #{{ order.id }}</h1>\n\
                 <p>Thanks, {{ order.customer }}.</p>\n\
                 <p><a href=\"https://example.com/orders/7\">Track it</a></p>\n\
                 @endsection\n",
            ), (
                "mail.layout",
                "<html><body>@yield(\"body\")</body></html>",
            )],
        ));

        let message = an_order().build(&engine).unwrap();

        assert_eq!(message.subject_text(), "Order #7 has shipped");

        let html = message.html_body().unwrap();
        assert!(html.contains("<h1>Order #7</h1>"), "{html}");
        // The engine escapes, so the ampersand is an entity in the HTML…
        assert!(html.contains("Ada &amp; Co"), "{html}");

        // …and a plain ampersand again in the derived text part.
        let text = message.text_body().unwrap();
        assert_eq!(
            text,
            "Order #7\n\nThanks, Ada & Co.\n\nTrack it (https://example.com/orders/7)"
        );
    }

    #[test]
    fn a_written_text_view_wins_over_the_derived_one() {
        struct Welcome;
        impl Mailable for Welcome {
            fn subject(&self) -> String {
                "Welcome".into()
            }
            fn view(&self) -> &str {
                "mail.welcome"
            }
            fn text_view(&self) -> Option<&str> {
                Some("mail.welcome_text")
            }
        }

        let engine = Engine::new(views(
            "text-view",
            &[
                ("mail.welcome", "<p>Welcome aboard</p>"),
                ("mail.welcome_text", "Welcome aboard, in words."),
            ],
        ));

        let message = Welcome.build(&engine).unwrap();
        assert_eq!(message.text_body(), Some("Welcome aboard, in words."));
        assert_eq!(message.html_body(), Some("<p>Welcome aboard</p>"));
    }

    #[test]
    fn a_missing_view_fails_by_name_rather_than_sending_an_empty_email() {
        let engine = Engine::new(views("missing-view", &[]));
        let error = an_order().build(&engine).unwrap_err().to_string();

        assert!(error.contains("mail.orders.shipped"), "{error}");
    }

    #[test]
    fn html_becomes_text_that_keeps_the_structure_and_the_links() {
        let text = html_to_text(
            "<html><head><style>p { color: red }</style></head>\
             <body><h1>Hello</h1><p>First line<br>second line</p>\
             <ul><li>one</li><li>two</li></ul>\
             <p>Visit <a href=\"https://example.com\">the site</a>.</p>\
             <script>alert('no')</script></body></html>",
        );

        assert_eq!(
            text,
            "Hello\n\nFirst line\nsecond line\n\n- one\n- two\n\nVisit the site (https://example.com)."
        );
    }

    #[test]
    fn entities_come_back_as_characters() {
        assert_eq!(html_to_text("<p>Ada &amp; Grace &lt;3</p>"), "Ada & Grace <3");
        assert_eq!(html_to_text("<p>caf&#233; &#x41;</p>"), "caf\u{e9} A");
        assert_eq!(html_to_text("<p>5 &notanentity; 6</p>"), "5 &notanentity; 6");
        assert_eq!(html_to_text("<p>a&nbsp;b</p>"), "a b");
    }

    #[test]
    fn a_link_whose_text_is_the_url_is_not_repeated() {
        assert_eq!(
            html_to_text("<a href=\"https://example.com\">https://example.com</a>"),
            "https://example.com"
        );
    }

    #[test]
    fn whitespace_in_the_html_source_does_not_reach_the_text() {
        let text = html_to_text("<p>\n    Lots   of\n    space\n</p>\n\n<p>Next</p>");
        assert_eq!(text, "Lots of space\n\nNext");
    }
}
