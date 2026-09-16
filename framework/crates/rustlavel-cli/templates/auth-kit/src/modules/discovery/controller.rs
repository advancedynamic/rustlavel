//! Fetching the registry document and turning it into a page.
//!
//! **A registry that is down must render, not 500.** This screen is opened
//! when something is wrong, and an error page that says only "internal server
//! error" would have taken away the one tool that could have said which
//! service is missing. An unreachable registry is a panel on the page saying
//! so, with the address it tried.

use rustlavel::prelude::*;

use crate::controllers::admin::users_controller::with_current_user;
use crate::support::{page, stats};

/// How long the registry has to answer.
///
/// Short: this is a dashboard, and a person waiting on a page learns nothing
/// from the tenth second that they did not know by the third.
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

pub struct DiscoveryController;

impl DiscoveryController {
    pub async fn index(req: Request) -> Result<Response> {
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();

        let base = match req.state::<crate::support::settings::Settings>() {
            Some(settings) => settings.get("discovery.url").await,
            None => String::new(),
        };
        let base = match base.trim() {
            "" => "http://127.0.0.1:8761".to_string(),
            set => set.trim_end_matches('/').to_string(),
        };

        let mut context = page::shell(&req, "discovery").await;
        context = with_current_user(context, &req, &db).await?;
        context = context.with("registry_url", Json::from(base.as_str()));

        let document = fetch(&base).await;

        let document = match document {
            Ok(document) => document,
            Err(problem) => {
                // The address is on the page too, because "could not reach the
                // registry" without saying which one sends somebody looking in
                // the wrong configuration file.
                return req.view(
                    "admin/discovery/index",
                    &context
                        .with("unreachable", Json::from(true))
                        .with("problem", Json::from(problem))
                        .with("services", Json::Array(Vec::new()))
                        .with("services_empty", Json::from(true)),
                );
            }
        };

        let services = document.get("services").and_then(Json::as_array).unwrap_or(&[]).to_vec();
        let counts = |key: &str| {
            document.get(&format!("counts.{key}")).and_then(Json::as_f64).unwrap_or(0.0) as i64
        };
        let preserving =
            document.get("mode").and_then(Json::as_str) == Some("SELF_PRESERVATION");

        let cards = Json::Array(vec![
            stats::card("Services", counts("services"), stats::BRAND, stats::ICON_LAYERS),
            stats::card("Instances", counts("instances"), stats::PEOPLE, stats::ICON_FOLDER),
            stats::card("Taking traffic", counts("up"), stats::GOOD, stats::ICON_CHECK),
            stats::card("Silent", counts("stale"), stats::QUIET, stats::ICON_DOCUMENT),
        ]);

        req.view(
            "admin/discovery/index",
            &context
                .with("unreachable", Json::from(false))
                .with("self_preservation", Json::from(preserving))
                .with("services_empty", Json::from(services.is_empty()))
                .with("services", Json::Array(services))
                .with("stats", stats::formatted(&req, cards).await),
        )
    }
}

/// Ask the registry for its document, or say why not.
///
/// The message is the one shown on the page. It names what was tried and what
/// happened, and nothing else — this screen is behind a permission, but a
/// transport error string is not a place to be inventive.
async fn fetch(base: &str) -> std::result::Result<Json, String> {
    let url = format!("{base}/discovery/registry.json");
    let client = rustlavel::client::Client::new().timeout(TIMEOUT);

    let response = match client.get(&url).send().await {
        Ok(response) => response,
        Err(error) => return Err(format!("{error}")),
    };

    if !response.is_success() {
        return Err(format!("the registry answered HTTP {}", response.status));
    }

    response.json().map_err(|error| format!("the registry sent something unreadable: {error}"))
}
