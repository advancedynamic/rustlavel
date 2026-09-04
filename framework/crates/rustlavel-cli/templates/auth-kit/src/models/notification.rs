use rustlavel::prelude::*;

/// A notice for one person, or for everybody.
///
/// `user_id` of `None` is an announcement: it is addressed to nobody in
/// particular, which is how everybody gets it. Keeping the two in one table
/// means a reader's list is one query and one order, rather than a union that
/// has to be paged by hand.
#[derive(Model, Default, Debug, Clone)]
#[model(table = "notifications")]
pub struct Notification {
    #[model(primary_key, generated)]
    pub id: i64,
    pub user_id: Option<i64>,
    pub level: String,
    pub title: String,
    pub body: Option<String>,
    pub url: Option<String>,
    pub sent_by: Option<i64>,
    pub created_at: Option<String>,
}

/// One person having read one notice.
///
/// A separate row rather than a flag on the notice, because a broadcast is read
/// by each person separately and a flag would let the first reader mark it read
/// for everyone.
#[derive(Model, Default, Debug, Clone)]
#[model(table = "notification_reads")]
pub struct NotificationRead {
    #[model(primary_key, generated)]
    pub id: i64,
    pub notification_id: i64,
    pub user_id: i64,
    pub read_at: Option<String>,
}

impl Notification {
    pub fn is_broadcast(&self) -> bool {
        self.user_id.is_none()
    }

    /// The tint a level gets. Anything unrecognised reads as `info` rather
    /// than as nothing, so a hand-written level still renders.
    pub fn tint(&self) -> &'static str {
        match self.level.as_str() {
            "success" => "badge-success",
            "warning" => "badge-warning",
            "danger" => "badge-danger",
            _ => "badge-brand",
        }
    }
}
