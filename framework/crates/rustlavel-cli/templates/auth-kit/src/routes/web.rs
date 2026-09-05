//! Your own routes. The starter kit's live in `auth.rs`.

use rustlavel::prelude::*;

pub fn routes(r: &mut Router) {
    // Where the application opens is a setting, not a literal. `support::home`
    // resolves it — see the note there for why it holds a route name.
    r.get("/", |req: Request| async move {
        Response::redirect(crate::support::home::path(&req).await)
    })
    .name("home");
}
