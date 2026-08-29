use rustlavel::prelude::*;

pub struct WelcomeController;

impl WelcomeController {
    pub async fn index(req: Request) -> Result<Response> {
        let name = req.config().string("app.name", "Blog");
        req.view("welcome", &ViewContext::new().with("name", name))
    }
}
