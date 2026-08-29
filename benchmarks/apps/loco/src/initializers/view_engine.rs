//! Registers Loco's own view layer (Tera) so `/template` renders through the
//! framework rather than through string concatenation.
//!
//! This is the initializer Loco's server-side-rendering starter generates,
//! minus the i18n loader — the benchmark has no locales.

use async_trait::async_trait;
use axum::{Extension, Router as AxumRouter};
use loco_rs::{
    app::{AppContext, Initializer},
    controller::views::{engines, ViewEngine},
    Result,
};

#[allow(clippy::module_name_repetitions)]
pub struct ViewEngineInitializer;

#[async_trait]
impl Initializer for ViewEngineInitializer {
    fn name(&self) -> String {
        "view-engine".to_string()
    }

    async fn after_routes(&self, router: AxumRouter, _ctx: &AppContext) -> Result<AxumRouter> {
        let tera_engine = engines::TeraView::build()?;
        Ok(router.layer(Extension(ViewEngine::from(tera_engine))))
    }
}
