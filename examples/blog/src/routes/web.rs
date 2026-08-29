//! The application's routes.

use rustlavel::prelude::*;

use crate::controllers::post_controller::PostController;
use crate::controllers::welcome_controller::WelcomeController;

pub fn routes(r: &mut Router) {
    r.get("/", WelcomeController::index).name("home");

    r.get("/posts", PostController::index).name("posts.index");
    // Written before `/posts/{id}` for readability; the router sorts static
    // segments ahead of parameters either way.
    r.get("/posts/new", PostController::create).name("posts.create");
    r.post("/posts", PostController::store).name("posts.store");
    r.get("/posts/{id}", PostController::show).name("posts.show");
}
