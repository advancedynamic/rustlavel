//! Your own routes. The starter kit's live in `auth.rs`.

use rustlavel::prelude::*;

pub fn routes(r: &mut Router) {
    r.get("/", |_req: Request| async move { Response::redirect("/dashboard") }).name("home");
}
