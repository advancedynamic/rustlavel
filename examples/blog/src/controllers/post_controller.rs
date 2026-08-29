use rustlavel::prelude::*;
use rustlavel::validation::{Errors, validate};

use crate::models::post::Post;

pub struct PostController;

impl PostController {
    /// The list of published posts.
    pub async fn index(req: Request) -> Result<Response> {
        let db = req.state::<Database>().expect("the database is registered in main.rs");
        let posts = Post::get(db, Post::published()).await?;

        req.view(
            "posts/index",
            &ViewContext::new().with("posts", Json::Array(posts.iter().map(Post::to_json).collect())),
        )
    }

    /// One post, or a 404.
    pub async fn show(req: Request) -> Result<Response> {
        let db = req.state::<Database>().expect("the database is registered in main.rs");

        let Some(id) = req.param_as::<i64>("id") else {
            return Ok(Response::not_found());
        };
        let Some(post) = Post::find(db, id).await? else {
            return Ok(Response::not_found());
        };

        req.view("posts/show", &ViewContext::new().with("post", post.to_json()))
    }

    /// The form for a new post.
    pub async fn create(req: Request) -> Result<Response> {
        req.view("posts/create", &ViewContext::new())
    }

    /// Store a submitted post.
    ///
    /// A validation failure returns through `?` as a 422: each error type
    /// decides its own response, so nothing is unwrapped here.
    pub async fn store(mut req: Request) -> Result<Response, Errors> {
        let data = validate(
            &mut req,
            &[("title", "required|string|max:120"), ("body", "required|string|min:10")],
        )
        .await?;

        let db = req.state::<Database>().expect("the database is registered in main.rs");

        let mut post = Post {
            title: data.string("title").unwrap_or_default(),
            body: data.string("body").unwrap_or_default(),
            published: true,
            ..Post::default()
        };

        match post.insert(db).await {
            // See-other, so a refresh does not post the form again.
            Ok(()) => Ok(Response::see_other(format!("/posts/{}", post.id))),
            // A database failure is not a validation failure; let the error
            // page explain it.
            Err(error) => Ok(error.into_response()),
        }
    }
}
