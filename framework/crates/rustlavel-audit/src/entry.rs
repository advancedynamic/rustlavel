//! One audit entry, and the builder that fills it in.

use rustlavel_core::Json;

/// A row in the trail, as it goes in and as it comes back out.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub id: i64,
    /// Who did it. `None` for something the system did on its own, or for an
    /// action taken before signing in — a failed login has no user yet.
    pub user_id: Option<i64>,
    /// Their name at the time, not as it is now.
    pub user_name: Option<String>,
    pub event: String,
    pub model_type: Option<String>,
    pub model_id: Option<String>,
    pub description: Option<String>,
    pub properties: Json,
    pub ip_address: Option<String>,
    pub user_agent: Option<String>,
    /// `YYYY-MM-DD HH:MM:SS` in UTC, the way the rest of the framework stores
    /// a time: it sorts as text, and all three databases keep it identically.
    pub created_at: String,
}

/// `Json` has no `Default`, and giving one to `properties` here would be
/// choosing between "no properties" and "an empty object" for every caller.
/// `Json::Null` is the honest one: nothing was recorded.
impl Default for Entry {
    fn default() -> Entry {
        Entry {
            id: 0,
            user_id: None,
            user_name: None,
            event: String::new(),
            model_type: None,
            model_id: None,
            description: None,
            properties: Json::Null,
            ip_address: None,
            user_agent: None,
            created_at: String::new(),
        }
    }
}

impl Entry {
    pub fn new(event: impl Into<String>) -> Entry {
        Entry { event: event.into(), ..Default::default() }
    }

    /// A one-line summary, falling back to something readable when the caller
    /// left the description off.
    pub fn summary(&self) -> String {
        if let Some(description) = self.description.as_deref().filter(|d| !d.is_empty()) {
            return description.to_string();
        }
        let who = self.user_name.as_deref().unwrap_or("Somebody");
        match (&self.model_type, &self.model_id) {
            (Some(kind), Some(id)) => format!("{who} performed {} on {kind} #{id}", self.event),
            _ => format!("{who} performed {}", self.event),
        }
    }
}

/// Fills in an [`Entry`] and writes it.
///
/// Made by [`Trail::event`](crate::Trail::event) or, with the request context
/// already filled in, by `req.audit(...)`.
pub struct Builder {
    pub(crate) trail: crate::store::Trail,
    pub(crate) entry: Entry,
}

impl Builder {
    /// Who did it. Takes the name as well, because a name read back later is
    /// the name the account has now, which is not what happened.
    pub fn by(mut self, id: i64, name: impl Into<String>) -> Builder {
        self.entry.user_id = Some(id);
        self.entry.user_name = Some(name.into());
        self
    }

    /// The actor's name, when the id is not to hand.
    ///
    /// Separate from [`by`](Self::by) because the two are not always known
    /// together: a request that is *in the middle of* signing somebody in has
    /// their name but no identity on the request yet, and one recording a
    /// failed attempt has neither.
    pub fn named(mut self, name: impl Into<String>) -> Builder {
        self.entry.user_name = Some(name.into());
        self
    }

    /// What was acted on: a type name and its key.
    pub fn on(mut self, kind: impl Into<String>, id: impl std::fmt::Display) -> Builder {
        self.entry.model_type = Some(kind.into());
        self.entry.model_id = Some(id.to_string());
        self
    }

    /// The sentence somebody will read on the audit page.
    pub fn describe(mut self, description: impl Into<String>) -> Builder {
        self.entry.description = Some(description.into());
        self
    }

    pub fn from_ip(mut self, ip: impl Into<String>) -> Builder {
        self.entry.ip_address = Some(ip.into());
        self
    }

    pub fn user_agent(mut self, agent: impl Into<String>) -> Builder {
        self.entry.user_agent = Some(agent.into());
        self
    }

    /// One extra field. Call it as often as needed.
    pub fn with(mut self, key: &str, value: impl Into<Json>) -> Builder {
        let mut properties = match std::mem::replace(&mut self.entry.properties, Json::Null) {
            Json::Object(map) => map,
            _ => Default::default(),
        };
        properties.insert(key.to_string(), value.into());
        self.entry.properties = Json::Object(properties);
        self
    }

    /// The before and after of a change, as one property each.
    pub fn changed(self, from: impl Into<Json>, to: impl Into<Json>) -> Builder {
        self.with("from", from).with("to", to)
    }

    /// The entry as it stands, without writing it. For a test.
    pub fn entry(&self) -> &Entry {
        &self.entry
    }

    /// Write it.
    pub async fn save(self) -> rustlavel_core::Result<i64> {
        self.trail.write(self.entry).await
    }

    /// Write it, and swallow a failure into a warning.
    ///
    /// For a caller on a path that must not fail because the trail is
    /// unavailable — a sign-out, say. Losing an entry is bad; refusing to sign
    /// somebody out because the audit table is missing is worse, and it is the
    /// kind of coupling that takes an application down.
    pub async fn record(self) {
        let event = self.entry.event.clone();
        if let Err(error) = self.save().await {
            rustlavel_core::warn!("could not write the audit entry for {event}: {error}");
        }
    }
}
