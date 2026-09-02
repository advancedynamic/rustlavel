//! API resources: one place per type that decides how it appears as JSON.
//!
//! Without this, every controller that returns a user decides for itself
//! which columns to send, how to format the dates, and whether the password
//! hash is included — and one of them will get it wrong. With it, a `User`
//! has exactly one JSON shape, declared once:
//!
//! ```ignore
//! pub struct UserResource;
//!
//! impl JsonResource for UserResource {
//!     type Model = User;
//!
//!     fn to_json(user: &User) -> Json {
//!         attributes()
//!             .set("id", user.id)
//!             .set("name", &user.name)
//!             .set("email", &user.email)
//!             .when(user.is_admin, "permissions", || Json::from(vec!["*"]))
//!             .when_some("avatar_url", user.avatar_url.as_deref())
//!             .finish()
//!     }
//! }
//!
//! // In a controller:
//! UserResource::make(&user)                         // {"data": {…}}
//! UserResource::make(&user).created()               // 201
//! UserResource::collection(&users)                  // {"data": [{…}, …]}
//! UserResource::collection(&page.hydrate()?)
//!     .paginate(page.current_page, page.per_page, page.total)
//!     .path("/api/users")                           // + "meta" and "links"
//! ```
//!
//! The shapes are Laravel's, down to the key names in `meta` and `links`, so
//! a front end written against a Laravel API needs no changes.

use crate::response::{IntoResponse, Response};
use crate::status::Status;
use rustlavel_core::Json;
use std::collections::BTreeMap;

/// How a model becomes JSON. Implement it once per type that leaves the server.
pub trait JsonResource {
    type Model;

    /// The representation of one model.
    fn to_json(model: &Self::Model) -> Json;

    /// The key the data sits under. `Some("data")` by default, as in Laravel;
    /// `None` sends the object or array bare.
    fn wrap() -> Option<&'static str> {
        Some("data")
    }

    fn make(model: &Self::Model) -> ResourceResponse {
        ResourceResponse::new(Self::to_json(model), Self::wrap())
    }

    fn collection<'a, I>(models: I) -> ResourceResponse
    where
        I: IntoIterator<Item = &'a Self::Model>,
        Self::Model: 'a,
    {
        let items = models.into_iter().map(Self::to_json).collect();
        ResourceResponse::new(Json::Array(items), Self::wrap())
    }
}

/// A resource on its way out: the data, whatever travels beside it, and the
/// status it goes with.
#[derive(Debug, Clone)]
pub struct ResourceResponse {
    data: Json,
    wrap: Option<&'static str>,
    additional: BTreeMap<String, Json>,
    meta: BTreeMap<String, Json>,
    links: BTreeMap<String, Json>,
    pagination: Option<Pagination>,
    path: String,
    status: Status,
    headers: Vec<(String, String)>,
}

impl ResourceResponse {
    pub fn new(data: Json, wrap: Option<&'static str>) -> Self {
        ResourceResponse {
            data,
            wrap,
            additional: BTreeMap::new(),
            meta: BTreeMap::new(),
            links: BTreeMap::new(),
            pagination: None,
            path: String::new(),
            status: Status::OK,
            headers: Vec::new(),
        }
    }

    /// Add a top-level key beside the data.
    pub fn additional(mut self, key: &str, value: impl Into<Json>) -> Self {
        self.additional.insert(key.to_string(), value.into());
        self
    }

    /// Add a key under `meta`.
    pub fn meta(mut self, key: &str, value: impl Into<Json>) -> Self {
        self.meta.insert(key.to_string(), value.into());
        self
    }

    /// Add a key under `links`.
    pub fn link(mut self, key: &str, value: impl Into<Json>) -> Self {
        self.links.insert(key.to_string(), value.into());
        self
    }

    /// The path the pagination links are built on. Without it they are
    /// relative — `?page=2` — which every client resolves correctly against
    /// the URL it just requested.
    pub fn path(mut self, path: &str) -> Self {
        self.path = path.to_string();
        self
    }

    /// Describe page-number pagination, with Laravel's `meta` and `links`.
    ///
    /// Three numbers rather than a `Page`, so this crate does not have to know
    /// about the database — and so it works for a page that came from
    /// anywhere else.
    pub fn paginate(mut self, current_page: i64, per_page: i64, total: i64) -> Self {
        let per_page = per_page.max(1);
        let last_page = (total + per_page - 1) / per_page;
        let last_page = last_page.max(1);
        let (from, to) = if total == 0 {
            (Json::Null, Json::Null)
        } else {
            let from = (current_page - 1) * per_page + 1;
            (Json::from(from), Json::from((from + per_page - 1).min(total)))
        };

        self.meta.insert("current_page".into(), Json::from(current_page));
        self.meta.insert("from".into(), from);
        self.meta.insert("last_page".into(), Json::from(last_page));
        self.meta.insert("per_page".into(), Json::from(per_page));
        self.meta.insert("to".into(), to);
        self.meta.insert("total".into(), Json::from(total));
        self.pagination = Some(Pagination::Pages { current: current_page, last: last_page });
        self
    }

    /// Describe cursor pagination: a `next_cursor` that is `null` at the end.
    pub fn cursor(mut self, next_cursor: Option<String>, per_page: i64) -> Self {
        self.meta.insert("per_page".into(), Json::from(per_page));
        self.meta.insert("next_cursor".into(), next_cursor.clone().map_or(Json::Null, Json::from));
        self.pagination = Some(Pagination::Cursor { next: next_cursor });
        self
    }

    /// The `links` object: whatever pagination implies, then whatever was
    /// added by hand, which wins on a clash.
    fn links(&self) -> BTreeMap<String, Json> {
        let mut links = BTreeMap::new();
        let url = |query: String| Json::from(format!("{}?{query}", self.path));
        match &self.pagination {
            Some(Pagination::Pages { current, last }) => {
                links.insert("first".into(), url("page=1".into()));
                links.insert("last".into(), url(format!("page={last}")));
                links.insert("prev".into(), if *current > 1 { url(format!("page={}", current - 1)) } else { Json::Null });
                links.insert("next".into(), if current < last { url(format!("page={}", current + 1)) } else { Json::Null });
            }
            Some(Pagination::Cursor { next }) => {
                links.insert("prev".into(), Json::Null);
                links.insert(
                    "next".into(),
                    next.as_ref().map_or(Json::Null, |c| url(format!("cursor={}", crate::url::encode(c)))),
                );
            }
            None => {}
        }
        links.extend(self.links.iter().map(|(k, v)| (k.clone(), v.clone())));
        links
    }

    pub fn with_status(mut self, status: impl Into<Status>) -> Self {
        self.status = status.into();
        self
    }

    /// `201 Created`, for the response to a successful store.
    pub fn created(self) -> Self {
        self.with_status(Status::CREATED)
    }

    pub fn with_header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.to_string(), value.into()));
        self
    }

    /// The body as it will be sent.
    pub fn to_json(&self) -> Json {
        let links = self.links();
        let has_siblings = !self.additional.is_empty() || !self.meta.is_empty() || !links.is_empty();

        // Unwrapped data can only stand alone. The moment it has company —
        // pagination, an extra key — it goes under `data` regardless, because
        // there is nowhere else for the company to go. This is Laravel's rule.
        let key = match self.wrap {
            Some(key) => key,
            None if has_siblings => "data",
            None => return self.data.clone(),
        };

        let mut object = BTreeMap::new();
        object.insert(key.to_string(), self.data.clone());
        if !links.is_empty() {
            object.insert("links".into(), Json::Object(links));
        }
        if !self.meta.is_empty() {
            object.insert("meta".into(), Json::Object(self.meta.clone()));
        }
        for (k, v) in &self.additional {
            object.insert(k.clone(), v.clone());
        }
        Json::Object(object)
    }
}

#[derive(Debug, Clone)]
enum Pagination {
    Pages { current: i64, last: i64 },
    Cursor { next: Option<String> },
}

impl IntoResponse for ResourceResponse {
    fn into_response(self) -> Response {
        let mut response = Response::new(self.status).with_json(self.to_json());
        for (name, value) in &self.headers {
            response.headers.set(name, value.clone());
        }
        response
    }
}

/// Start building the attributes of a resource.
pub fn attributes() -> Attributes {
    Attributes::default()
}

/// An object under construction, with the conditionals a resource needs.
///
/// The point of `when` is the key that is *absent*, not null. A client told
/// `"permissions": null` has to decide what null means; a client not told
/// about permissions at all knows it is not allowed to see them.
#[derive(Debug, Default, Clone)]
pub struct Attributes {
    fields: BTreeMap<String, Json>,
}

impl Attributes {
    pub fn set(mut self, key: &str, value: impl Into<Json>) -> Self {
        self.fields.insert(key.to_string(), value.into());
        self
    }

    /// Include the key only when the condition holds. The value is computed
    /// lazily, so it may be expensive or may only be valid when the condition
    /// is true.
    pub fn when(mut self, condition: bool, key: &str, value: impl FnOnce() -> Json) -> Self {
        if condition {
            self.fields.insert(key.to_string(), value());
        }
        self
    }

    /// Include the key only when there is a value — `Some`, not `None`.
    pub fn when_some<T: Into<Json>>(mut self, key: &str, value: Option<T>) -> Self {
        if let Some(value) = value {
            self.fields.insert(key.to_string(), value.into());
        }
        self
    }

    /// Merge another object's keys in — for a nested resource, or a shared
    /// set of timestamps.
    pub fn merge(mut self, other: Json) -> Self {
        if let Json::Object(fields) = other {
            self.fields.extend(fields);
        }
        self
    }

    pub fn finish(self) -> Json {
        Json::Object(self.fields)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct User {
        id: i64,
        name: String,
        email: String,
        password_hash: String,
        is_admin: bool,
        avatar: Option<String>,
    }

    fn alice() -> User {
        User {
            id: 1,
            name: "Alice".into(),
            email: "alice@example.com".into(),
            password_hash: "$argon2id$…".into(),
            is_admin: true,
            avatar: None,
        }
    }

    fn bob() -> User {
        User { id: 2, name: "Bob".into(), is_admin: false, avatar: Some("/b.png".into()), ..alice() }
    }

    struct UserResource;

    impl JsonResource for UserResource {
        type Model = User;

        fn to_json(user: &User) -> Json {
            attributes()
                .set("id", user.id)
                .set("name", user.name.as_str())
                .set("email", user.email.as_str())
                .when(user.is_admin, "permissions", || Json::from(vec!["*"]))
                .when_some("avatar", user.avatar.as_deref())
                .finish()
        }
    }

    struct BareUser;

    impl JsonResource for BareUser {
        type Model = User;
        fn to_json(user: &User) -> Json {
            Json::object([("id", Json::from(user.id))])
        }
        fn wrap() -> Option<&'static str> {
            None
        }
    }

    #[test]
    fn a_single_resource_is_wrapped_in_data() {
        let json = UserResource::make(&alice()).to_json();
        assert_eq!(json.get("data.id").and_then(Json::as_i64), Some(1));
        assert_eq!(json.get("data.name").and_then(Json::as_str), Some("Alice"));
        assert!(json.get("data.password_hash").is_none(), "only what the resource names leaves");
    }

    #[test]
    fn conditional_attributes_are_absent_not_null() {
        let admin = UserResource::to_json(&alice());
        let plain = UserResource::to_json(&bob());

        assert!(admin.get("permissions").is_some());
        assert!(plain.get("permissions").is_none(), "absent, so the client knows it may not ask");
        assert!(admin.get("avatar").is_none());
        assert_eq!(plain.get("avatar").and_then(Json::as_str), Some("/b.png"));
        let _ = alice().password_hash;
    }

    #[test]
    fn a_collection_is_an_array_under_data() {
        let users = vec![alice(), bob()];
        let json = UserResource::collection(&users).to_json();
        let data = json.get("data").and_then(Json::as_array).expect("an array");
        assert_eq!(data.len(), 2);
        assert_eq!(data[1].get("name").and_then(Json::as_str), Some("Bob"));
    }

    #[test]
    fn wrapping_can_be_turned_off() {
        let json = BareUser::make(&alice()).to_json();
        assert_eq!(json.get("id").and_then(Json::as_i64), Some(1));
        assert!(json.get("data").is_none());
    }

    #[test]
    fn unwrapped_data_is_wrapped_anyway_once_it_has_company() {
        let json = BareUser::make(&alice()).additional("version", "2").to_json();
        assert_eq!(json.get("data.id").and_then(Json::as_i64), Some(1));
        assert_eq!(json.get("version").and_then(Json::as_str), Some("2"));
    }

    #[test]
    fn pagination_produces_laravels_meta_and_links() {
        let users = vec![alice(), bob()];
        let json = UserResource::collection(&users).paginate(2, 2, 5).path("/api/users").to_json();

        assert_eq!(json.get("meta.current_page").and_then(Json::as_i64), Some(2));
        assert_eq!(json.get("meta.per_page").and_then(Json::as_i64), Some(2));
        assert_eq!(json.get("meta.total").and_then(Json::as_i64), Some(5));
        assert_eq!(json.get("meta.last_page").and_then(Json::as_i64), Some(3));
        assert_eq!(json.get("meta.from").and_then(Json::as_i64), Some(3));
        assert_eq!(json.get("meta.to").and_then(Json::as_i64), Some(4));

        assert_eq!(json.get("links.first").and_then(Json::as_str), Some("/api/users?page=1"));
        assert_eq!(json.get("links.last").and_then(Json::as_str), Some("/api/users?page=3"));
        assert_eq!(json.get("links.prev").and_then(Json::as_str), Some("/api/users?page=1"));
        assert_eq!(json.get("links.next").and_then(Json::as_str), Some("/api/users?page=3"));
    }

    #[test]
    fn the_first_and_last_pages_have_null_neighbours() {
        let users = vec![alice()];
        let first = UserResource::collection(&users).paginate(1, 10, 25).to_json();
        assert!(first.get("links.prev").unwrap().is_null());
        assert_eq!(first.get("links.next").and_then(Json::as_str), Some("?page=2"), "relative without a path");

        let last = UserResource::collection(&users).paginate(3, 10, 25).to_json();
        assert!(last.get("links.next").unwrap().is_null());
        assert_eq!(last.get("meta.to").and_then(Json::as_i64), Some(25), "clamped to the total");
    }

    #[test]
    fn an_empty_page_has_null_from_and_to_and_one_last_page() {
        let json = UserResource::collection(&Vec::<User>::new()).paginate(1, 10, 0).to_json();
        assert!(json.get("meta.from").unwrap().is_null());
        assert!(json.get("meta.to").unwrap().is_null());
        assert_eq!(json.get("meta.last_page").and_then(Json::as_i64), Some(1));
    }

    #[test]
    fn cursor_pagination_encodes_the_cursor_into_the_link() {
        let users = vec![alice()];
        let json = UserResource::collection(&users)
            .cursor(Some("id>42&x".into()), 10)
            .path("/api/users")
            .to_json();
        assert_eq!(json.get("meta.next_cursor").and_then(Json::as_str), Some("id>42&x"));
        assert_eq!(json.get("links.next").and_then(Json::as_str), Some("/api/users?cursor=id%3E42%26x"));

        let end = UserResource::collection(&users).cursor(None, 10).to_json();
        assert!(end.get("meta.next_cursor").unwrap().is_null());
        assert!(end.get("links.next").unwrap().is_null());
    }

    #[test]
    fn it_becomes_a_json_response_with_the_chosen_status_and_headers() {
        let response = UserResource::make(&alice())
            .created()
            .with_header("location", "/api/users/1")
            .into_response();
        assert_eq!(response.status, Status::CREATED);
        assert!(response.headers.content_type().unwrap().starts_with("application/json"));
        assert_eq!(response.headers.get("location"), Some("/api/users/1"));
        assert!(response.body_string().contains("\"Alice\""));
    }

    #[test]
    fn attributes_merge_nested_objects() {
        let json = attributes()
            .set("id", 1)
            .merge(Json::object([("created_at", Json::from("2026-01-01"))]))
            .merge(Json::from("not an object, ignored"))
            .finish();
        assert_eq!(json.get("created_at").and_then(Json::as_str), Some("2026-01-01"));
        assert_eq!(json.as_object().unwrap().len(), 2);
    }
}
