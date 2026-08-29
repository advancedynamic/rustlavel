//! Choosing a locale per request.

use crate::Translator;
use rustlavel_http::{Middleware, Next, Request, Response};
use rustlavel_http::handler::BoxFuture;

/// The locale chosen for this request, attached for handlers and views to read.
#[derive(Debug, Clone)]
pub struct Locale(pub String);

/// Picks a locale from the URL, then a cookie, then `Accept-Language`.
///
/// The order matters: an explicit `?lang=id` is someone asking, a cookie is
/// someone who asked before, and the header is only a hint from the browser.
pub struct DetectLocale {
    translator: Translator,
    /// Query parameter and cookie name, both configurable because sites differ.
    parameter: String,
    cookie: String,
}

impl DetectLocale {
    pub fn new(translator: Translator) -> Self {
        DetectLocale {
            translator,
            parameter: "lang".to_string(),
            cookie: "locale".to_string(),
        }
    }

    pub fn parameter(mut self, name: &str) -> Self {
        self.parameter = name.to_string();
        self
    }

    pub fn cookie(mut self, name: &str) -> Self {
        self.cookie = name.to_string();
        self
    }

    fn detect(&self, request: &Request) -> String {
        if let Some(locale) = request.query(&self.parameter)
            && self.translator.has_locale(locale) {
                return locale.to_string();
            }

        if let Some(locale) = request.cookie(&self.cookie)
            && self.translator.has_locale(&locale) {
                return locale;
            }

        if let Some(header) = request.header("accept-language")
            && let Some(locale) = best_match(header, &self.translator.locales()) {
                return locale;
            }

        self.translator.default_locale()
    }
}

impl Middleware for DetectLocale {
    fn handle(&self, mut request: Request, next: Next) -> BoxFuture<Response> {
        let locale = self.detect(&request);
        request.extend(Locale(locale.clone()));

        Box::pin(async move {
            // Telling caches the response varies by language keeps a shared
            // cache from serving Indonesian to an English reader.
            next.run(request).await.with_header("content-language", locale)
        })
    }
}

/// Pick the best available locale from an `Accept-Language` header.
///
/// Quality values are honoured, and a language tag matches a bare language:
/// `id-ID` will happily take `id`.
pub fn best_match(header: &str, available: &[String]) -> Option<String> {
    let mut candidates: Vec<(f32, String)> = header
        .split(',')
        .filter_map(|part| {
            let mut pieces = part.split(';');
            let tag = pieces.next()?.trim().to_ascii_lowercase();
            if tag.is_empty() {
                return None;
            }
            let quality = pieces
                .find_map(|piece| piece.trim().strip_prefix("q=").and_then(|q| q.parse().ok()))
                .unwrap_or(1.0);
            Some((quality, tag))
        })
        .collect();

    // Highest quality first; ties keep the order the client sent.
    candidates.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    for (_, tag) in candidates {
        if tag == "*" {
            return available.first().cloned();
        }
        if let Some(exact) = available.iter().find(|a| a.eq_ignore_ascii_case(&tag)) {
            return Some(exact.clone());
        }
        let base = tag.split('-').next().unwrap_or(&tag);
        if let Some(loose) = available.iter().find(|a| a.eq_ignore_ascii_case(base)) {
            return Some(loose.clone());
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustlavel_core::Json;
    use rustlavel_http::{Method, Router, TestClient};

    fn translator() -> Translator {
        let translator = Translator::new();
        translator.insert("en", Json::parse(r#"{"hi":"Hello"}"#).unwrap());
        translator.insert("id", Json::parse(r#"{"hi":"Halo"}"#).unwrap());
        translator
    }

    fn app() -> TestClient {
        let mut router = Router::new();
        let translator = translator();
        router.middleware(DetectLocale::new(translator.clone()));
        router.get("/", move |req: Request| {
            let translator = translator.clone();
            async move {
                let locale = req.extension::<Locale>().map(|l| l.0.clone()).unwrap_or_default();
                translator.get_in(&locale, "hi", &[])
            }
        });
        TestClient::new(router)
    }

    #[tokio::test]
    async fn an_explicit_query_parameter_wins() {
        app().get("/?lang=id").await.assert_ok().assert_see("Halo").assert_header("content-language", "id");
    }

    #[tokio::test]
    async fn a_cookie_is_used_when_no_parameter_is_given() {
        app()
            .send(Request::new(Method::Get, "/").with_header("cookie", "locale=id"))
            .await
            .assert_see("Halo");
    }

    #[tokio::test]
    async fn the_accept_language_header_is_the_last_resort() {
        app()
            .send(Request::new(Method::Get, "/").with_header("accept-language", "id-ID,id;q=0.9,en;q=0.5"))
            .await
            .assert_see("Halo");
    }

    #[tokio::test]
    async fn an_unknown_locale_falls_back_to_the_default() {
        app().get("/?lang=fr").await.assert_see("Hello");
    }

    #[test]
    fn header_matching_honours_quality_and_base_languages() {
        let available = vec!["en".to_string(), "id".to_string()];

        assert_eq!(best_match("id-ID", &available).as_deref(), Some("id"));
        assert_eq!(best_match("fr;q=0.9,en;q=0.8", &available).as_deref(), Some("en"));
        assert_eq!(best_match("en;q=0.2,id;q=0.9", &available).as_deref(), Some("id"));
        assert_eq!(best_match("*", &available).as_deref(), Some("en"));
        assert_eq!(best_match("fr,de", &available), None);
    }
}
