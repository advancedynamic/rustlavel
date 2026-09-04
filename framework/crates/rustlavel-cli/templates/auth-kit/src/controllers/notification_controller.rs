use rustlavel::prelude::*;

use crate::models::notification::{Notification, NotificationRead};
use crate::models::user::User;
use crate::support::{format, page};

/// How many a page holds, and how many the bell shows.
const PER_PAGE: i64 = 25;
const IN_THE_BELL: usize = 8;

pub struct NotificationController;

impl NotificationController {
    /// `GET /notifications`
    pub async fn index(req: Request) -> Result<Response> {
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let me = req.identity().and_then(|id| id.id_as::<i64>()).unwrap_or_default();
        let Some(user) = User::find(&db, me).await? else { return Ok(Response::see_other("/login")) };

        let page_number = req.query("page").and_then(|p| p.parse::<i64>().ok()).unwrap_or(1).max(1);
        let dates = format::Dates::of(&req).await;

        let (rows, unread) = Self::for_reader(&db, me).await?;
        let total = rows.len() as i64;
        let shown: Vec<Json> = rows
            .iter()
            .skip(((page_number - 1) * PER_PAGE) as usize)
            .take(PER_PAGE as usize)
            .map(|(notice, read)| Self::as_json(notice, *read, &dates))
            .collect();

        let mut context = page::shell(&req, "notifications").await;
        context = page::with_user(context, &req, &user).await?;
        context = context
            .with("notifications", Json::Array(shown))
            .with("empty", Json::from(total == 0))
            .with("unread", Json::from(unread))
            .with("may_send", Json::from(req.can("notifications.send").await?))
            .with("csrf_field", Json::from(rustlavel::auth::csrf::field(&req)));
        // `i64::div_ceil` is still unstable, and this kit compiles on the
        // toolchain it declares rather than on nightly.
        let pages = ((total + PER_PAGE - 1) / PER_PAGE).max(1);
        let from = if total == 0 { 0 } else { (page_number - 1) * PER_PAGE + 1 };
        context = context
            .with("has_pages", Json::from(pages > 1))
            .with("page_from", Json::from(from))
            .with("page_to", Json::from((page_number * PER_PAGE).min(total)))
            .with("page_total", Json::from(total))
            .with(
                "prev_url",
                match page_number > 1 {
                    true => Json::from(format!("/notifications?page={}", page_number - 1)),
                    false => Json::Null,
                },
            )
            .with(
                "next_url",
                match page_number < pages {
                    true => Json::from(format!("/notifications?page={}", page_number + 1)),
                    false => Json::Null,
                },
            );

        req.view("notifications.index", &context)
    }

    /// `GET /notifications/recent` — what the bell in the header shows.
    pub async fn recent(req: Request) -> Result<Response> {
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let me = req.identity().and_then(|id| id.id_as::<i64>()).unwrap_or_default();
        let dates = format::Dates::of(&req).await;

        let (rows, unread) = Self::for_reader(&db, me).await?;
        let items: Vec<Json> = rows
            .iter()
            .take(IN_THE_BELL)
            .map(|(notice, read)| Self::as_json(notice, *read, &dates))
            .collect();

        Ok(Response::json(Json::object([
            ("items", Json::Array(items)),
            ("unread", Json::from(unread)),
        ])))
    }

    /// `POST /notifications/read` — everything this person can see, marked read.
    pub async fn read_all(req: Request) -> Result<Response> {
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let me = req.identity().and_then(|id| id.id_as::<i64>()).unwrap_or_default();

        let (rows, _) = Self::for_reader(&db, me).await?;
        let now = crate::support::tokens::now();
        for (notice, already) in rows {
            if already {
                continue;
            }
            NotificationRead {
                notification_id: notice.id,
                user_id: me,
                read_at: Some(now.clone()),
                ..Default::default()
            }
            .insert(&db)
            .await?;
        }

        page::flash(&req, "success", "Everything is marked as read.");
        Ok(Response::see_other("/notifications"))
    }

    /// `POST /notifications` — send one, to somebody or to everybody.
    pub async fn store(mut req: Request) -> Result<Response> {
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let me = req.identity().and_then(|id| id.id_as::<i64>()).unwrap_or_default();

        let title = req.input("title").unwrap_or_default().trim().to_string();
        if title.is_empty() {
            page::flash(&req, "error", "A notification needs a title.");
            return Ok(Response::see_other("/notifications"));
        }

        // Blank, "all" or "everybody" is a broadcast. Anything else has to be
        // a person who exists — sending into the void is worse than an error.
        let to = req.input("user_id").unwrap_or_default().trim().to_string();
        let user_id = match to.as_str() {
            "" | "all" | "everybody" => None,
            other => match other.parse::<i64>().ok() {
                Some(id) if User::find(&db, id).await?.is_some() => Some(id),
                _ => {
                    page::flash(&req, "error", format!("There is no person with the id `{other}`."));
                    return Ok(Response::see_other("/notifications"));
                }
            },
        };

        let body = req.input("body").unwrap_or_default().trim().to_string();
        let level = match req.input("level").unwrap_or_default().as_str() {
            level @ ("success" | "warning" | "danger") => level.to_string(),
            _ => "info".to_string(),
        };

        Notification {
            user_id,
            level,
            title: title.clone(),
            body: (!body.is_empty()).then_some(body),
            url: req.input("url").filter(|u| !u.trim().is_empty()),
            sent_by: Some(me),
            created_at: Some(crate::support::tokens::now()),
            ..Default::default()
        }
        .insert(&db)
        .await?;

        if let Some(audit) = crate::support::audit::of(&req, "notifications.sent") {
            audit
                .describe(match user_id {
                    Some(id) => format!("Sent \"{title}\" to person #{id}"),
                    None => format!("Announced \"{title}\" to everybody"),
                })
                .record()
                .await;
        }

        page::flash(
            &req,
            "success",
            match user_id {
                Some(_) => "Sent.".to_string(),
                None => "Announced to everybody.".to_string(),
            },
        );
        Ok(Response::see_other("/notifications"))
    }

    /// This reader's notices, newest first, each with whether they have read it.
    ///
    /// Two queries and a join in memory rather than a `LEFT JOIN`: the query
    /// builder here is deliberately simple, and a person's notice list is
    /// small enough that the difference is not measurable.
    async fn for_reader(db: &Database, me: i64) -> Result<(Vec<(Notification, bool)>, i64)> {
        let mut rows = Notification::get(
            db,
            Notification::query().order_by("id", rustlavel::db::Direction::Desc),
        )
        .await?;
        // Addressed to this person, or to nobody in particular.
        rows.retain(|n| n.user_id.is_none() || n.user_id == Some(me));

        let reads = NotificationRead::get(
            db,
            NotificationRead::query().filter("user_id", me),
        )
        .await?;
        let read: std::collections::BTreeSet<i64> =
            reads.into_iter().map(|r| r.notification_id).collect();

        let unread = rows.iter().filter(|n| !read.contains(&n.id)).count() as i64;
        Ok((rows.into_iter().map(|n| { let seen = read.contains(&n.id); (n, seen) }).collect(), unread))
    }

    fn as_json(notice: &Notification, read: bool, dates: &format::Dates) -> Json {
        Json::object([
            ("id", Json::from(notice.id)),
            ("title", Json::from(notice.title.as_str())),
            ("body", Json::from(notice.body.clone().unwrap_or_default())),
            ("href", Json::from(notice.url.clone().unwrap_or_default())),
            ("has_url", Json::from(notice.url.is_some())),
            ("tint", Json::from(notice.tint())),
            ("level", Json::from(notice.level.as_str())),
            ("broadcast", Json::from(notice.is_broadcast())),
            ("read", Json::from(read)),
            ("when", Json::from(dates.moment(notice.created_at.as_deref().unwrap_or_default()))),
            ("ago", Json::from(dates.ago(
                notice.created_at.as_deref().unwrap_or_default(),
                &crate::support::tokens::now(),
            ))),
        ])
    }
}
