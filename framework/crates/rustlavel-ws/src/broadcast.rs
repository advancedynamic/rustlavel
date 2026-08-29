//! Channels and broadcasting — the Laravel Echo half of this crate.
//!
//! A [`Broadcaster`] holds named channels. Clients subscribe over their socket;
//! the application pushes events at a channel from anywhere — a controller, a
//! queued job, a background task — and every subscriber gets them.
//!
//! ```ignore
//! let broadcaster = Broadcaster::new();
//! r.get("/broadcasting", broadcaster.route());
//!
//! // …later, from a controller:
//! broadcaster.broadcast("orders", "order.created", Json::object([("id", 7.into())]));
//! ```
//!
//! # The client protocol
//!
//! Every frame is a JSON text frame. Client to server:
//!
//! ```json
//! {"event": "subscribe",   "channel": "orders"}
//! {"event": "subscribe",   "channel": "presence-chat",
//!  "data": {"member": {"id": "7", "info": {"name": "Ada"}}}}
//! {"event": "unsubscribe", "channel": "orders"}
//! ```
//!
//! Server to client, always the same three fields:
//!
//! ```json
//! {"event": "subscribed",      "channel": "orders",        "data": null}
//! {"event": "unsubscribed",    "channel": "orders",        "data": null}
//! {"event": "error",           "channel": "private-books", "data": {"message": "…"}}
//! {"event": "order.created",   "channel": "orders",        "data": {"id": 7}}
//! {"event": "presence.here",   "channel": "presence-chat", "data": {"members": [ … ]}}
//! {"event": "presence.joined", "channel": "presence-chat", "data": {"member": { … }}}
//! {"event": "presence.left",   "channel": "presence-chat", "data": {"member": { … }}}
//! ```
//!
//! # Channel names carry their own rules
//!
//! As in Laravel, the prefix is the policy: `private-…` and `presence-…` are
//! gated by the application's [`Authorizer`], everything else is open. A
//! broadcaster with no authorizer refuses every gated channel — the safe
//! default is the one where forgetting to write the callback closes the door
//! rather than opening it.
//!
//! # Backpressure
//!
//! Fan-out never waits. Each subscriber has a fixed-depth queue (see
//! [`WebSocketConfig::send_queue`](crate::WebSocketConfig)), and a subscriber
//! whose queue is full is *dropped*, not queued for. An unbounded queue would
//! turn one browser on a bad connection into unbounded memory growth in the
//! server — a slow client must cost the server nothing but its own socket.

use crate::connection::{Sender, WebSocket};
use crate::message::Message;
use crate::route::websocket;
use rustlavel_core::{Event, Json, events};
use rustlavel_http::handler::BoxFuture;
use rustlavel_http::{Handler, Request};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

/// Identifies one connected client across every channel it has joined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SubscriberId(pub u64);

/// Who is in a presence channel.
#[derive(Debug, Clone, PartialEq)]
pub struct Member {
    /// Stable across reconnects — a user id, not a socket id.
    pub id: String,
    /// Whatever the application wants the other members to see. Never anything
    /// the member would not want published: this is sent to everyone in the room.
    pub info: Json,
}

impl Member {
    pub fn new(id: impl Into<String>, info: Json) -> Member {
        Member { id: id.into(), info }
    }

    pub fn to_json(&self) -> Json {
        Json::object([("id", Json::from(self.id.as_str())), ("info", self.info.clone())])
    }

    pub fn from_json(value: &Json) -> Option<Member> {
        let id = value.get("id")?.as_str()?.to_string();
        let info = value.get("info").cloned().unwrap_or(Json::Null);
        Some(Member { id, info })
    }
}

/// What a channel's name says about who may join it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelKind {
    Public,
    Private,
    Presence,
}

impl ChannelKind {
    /// Gated channels go through the application's authorizer.
    pub fn is_gated(self) -> bool {
        !matches!(self, ChannelKind::Public)
    }
}

/// The convention, borrowed from Echo: the prefix is the policy.
pub fn kind_of(channel: &str) -> ChannelKind {
    if channel.starts_with("presence-") {
        ChannelKind::Presence
    } else if channel.starts_with("private-") {
        ChannelKind::Private
    } else {
        ChannelKind::Public
    }
}

/// Decides whether a client may join a private or presence channel.
///
/// A plain `Fn(&Request, &str) -> bool` is an authorizer, which covers the
/// common case of reading a session the auth middleware already resolved. When
/// the check needs to await — a database lookup — use [`authorize_async`].
pub trait Authorizer: Send + Sync + 'static {
    fn authorize(&self, request: Arc<Request>, channel: String) -> BoxFuture<bool>;
}

impl<F> Authorizer for F
where
    F: Fn(&Request, &str) -> bool + Send + Sync + 'static,
{
    fn authorize(&self, request: Arc<Request>, channel: String) -> BoxFuture<bool> {
        // Decided before the future is built, so the borrow never has to
        // outlive the call — which is what makes the synchronous shape work.
        let allowed = self(&request, &channel);
        Box::pin(async move { allowed })
    }
}

/// An authorizer that awaits — a database or cache lookup, say.
///
/// The request arrives as an `Arc` because it has to survive across the await,
/// and one request may be checked for several channels over the socket's life.
pub fn authorize_async<F, Fut>(check: F) -> impl Authorizer
where
    F: Fn(Arc<Request>, String) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = bool> + Send + 'static,
{
    struct Awaiting<F>(F);

    impl<F, Fut> Authorizer for Awaiting<F>
    where
        F: Fn(Arc<Request>, String) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = bool> + Send + 'static,
    {
        fn authorize(&self, request: Arc<Request>, channel: String) -> BoxFuture<bool> {
            Box::pin((self.0)(request, channel))
        }
    }

    Awaiting(check)
}

struct Subscription {
    id: SubscriberId,
    sender: Sender,
    /// Set only on presence channels.
    member: Option<Member>,
}

#[derive(Default)]
struct Registry {
    channels: BTreeMap<String, Vec<Subscription>>,
    next: u64,
}

/// What one fan-out achieved.
#[derive(Default)]
struct Delivery {
    delivered: usize,
    dropped: Vec<SubscriberId>,
}

/// The channel registry. Cheap to clone, and every clone is the same registry.
#[derive(Clone, Default)]
pub struct Broadcaster {
    inner: Arc<Mutex<Registry>>,
    authorizer: Option<Arc<dyn Authorizer>>,
}

impl Broadcaster {
    pub fn new() -> Broadcaster {
        Broadcaster::default()
    }

    /// Gate private and presence channels behind an application check.
    pub fn authorize(mut self, authorizer: impl Authorizer) -> Broadcaster {
        self.authorizer = Some(Arc::new(authorizer));
        self
    }

    /// A route that speaks the channel protocol: `r.get("/broadcasting", b.route())`.
    pub fn route(&self) -> impl Handler {
        let broadcaster = self.clone();
        websocket(move |socket, request| {
            let broadcaster = broadcaster.clone();
            async move { broadcaster.serve(socket, request).await }
        })
    }

    /// Run one client's socket until it goes away.
    pub async fn serve(&self, mut socket: WebSocket, request: Request) {
        // Shared rather than moved, because every authorisation check over the
        // life of this socket asks about the same request.
        let request = Arc::new(request);
        let id = self.reserve();
        let out = socket.sender();

        while let Some(message) = socket.recv().await {
            let Some(text) = message.as_text() else {
                let _ = out.send(refusal("", "this channel protocol speaks JSON text")).await;
                continue;
            };

            match Command::parse(text) {
                Ok(command) => self.apply(id, &out, &request, command).await,
                Err(reason) => {
                    let _ = out.send(refusal("", &reason)).await;
                }
            }
        }

        // Leaving is not optional: presence channels owe the rest of the room
        // a departure notice even when the client vanished mid-frame.
        self.disconnect(id);
    }

    /// Claim an id for a client that is about to subscribe to things.
    pub fn reserve(&self) -> SubscriberId {
        let mut registry = self.lock();
        registry.next += 1;
        SubscriberId(registry.next)
    }

    /// Add a subscriber to a public or private channel.
    pub fn subscribe(&self, channel: &str, id: SubscriberId, sender: Sender) {
        let mut registry = self.lock();
        let subscriptions = registry.channels.entry(channel.to_string()).or_default();

        // Subscribing twice is idempotent rather than two copies of everything.
        if subscriptions.iter().any(|subscription| subscription.id == id) {
            return;
        }
        subscriptions.push(Subscription { id, sender, member: None });
    }

    /// Join a presence channel: announce the member, and report who is already
    /// here so the newcomer can render the room immediately.
    pub fn join(
        &self,
        channel: &str,
        id: SubscriberId,
        sender: Sender,
        member: Member,
    ) -> Vec<Member> {
        let present = {
            let mut registry = self.lock();
            let subscriptions = registry.channels.entry(channel.to_string()).or_default();
            // A rejoin replaces the earlier presence rather than duplicating it.
            subscriptions.retain(|subscription| subscription.id != id);

            let present: Vec<Member> =
                subscriptions.iter().filter_map(|s| s.member.clone()).collect();
            subscriptions.push(Subscription { id, sender, member: Some(member.clone()) });
            present
        };

        let announcement = envelope(
            "presence.joined",
            channel,
            Json::object([("member", member.to_json())]),
        );
        // Everyone but the joiner, who is getting the full roster instead.
        self.deliver(channel, &Message::json(&announcement), Some(id));

        present
    }

    /// Remove a subscriber from one channel.
    pub fn unsubscribe(&self, channel: &str, id: SubscriberId) {
        let departed = {
            let mut registry = self.lock();
            let mut departed = None;
            if let Some(subscriptions) = registry.channels.get_mut(channel) {
                if let Some(position) =
                    subscriptions.iter().position(|subscription| subscription.id == id)
                {
                    departed = subscriptions.remove(position).member;
                }
                if subscriptions.is_empty() {
                    // Empty channels are forgotten, so the map cannot grow one
                    // entry per channel name a client ever typed.
                    registry.channels.remove(channel);
                }
            }
            departed
        };

        if let Some(member) = departed {
            self.announce_departure(channel, &member);
        }
    }

    /// Remove a subscriber from every channel it joined.
    pub fn disconnect(&self, id: SubscriberId) {
        let departures = {
            let mut registry = self.lock();
            let mut departures = Vec::new();

            registry.channels.retain(|channel, subscriptions| {
                if let Some(position) =
                    subscriptions.iter().position(|subscription| subscription.id == id)
                    && let Some(member) = subscriptions.remove(position).member
                {
                    departures.push((channel.clone(), member));
                }
                !subscriptions.is_empty()
            });
            departures
        };

        for (channel, member) in departures {
            self.announce_departure(&channel, &member);
        }
    }

    /// Send an event to everyone on a channel. Returns how many got it.
    pub fn broadcast(&self, channel: &str, event: &str, data: Json) -> usize {
        let message = Message::json(&envelope(event, channel, data));
        let delivery = self.deliver(channel, &message, None);

        // Subscribers that could not keep up are gone; take them out of every
        // channel so the next broadcast does not pay for them again.
        for id in &delivery.dropped {
            self.disconnect(*id);
        }

        if events::has_subscribers() {
            // Channel and counts, never the payload: a broadcast can carry an
            // order total or a private message, and Telescope is not the place
            // for either.
            Event::new("broadcast.sent")
                .with("channel", channel)
                .with("event", event)
                .with("subscribers", delivery.delivered)
                .with("dropped", delivery.dropped.len())
                .dispatch();
        }

        delivery.delivered
    }

    pub fn subscribers(&self, channel: &str) -> usize {
        self.lock().channels.get(channel).map_or(0, Vec::len)
    }

    pub fn members(&self, channel: &str) -> Vec<Member> {
        self.lock()
            .channels
            .get(channel)
            .map(|subscriptions| {
                subscriptions.iter().filter_map(|s| s.member.clone()).collect()
            })
            .unwrap_or_default()
    }

    /// Every channel that currently has at least one subscriber.
    pub fn channels(&self) -> Vec<String> {
        self.lock().channels.keys().cloned().collect()
    }

    /// Handle one parsed client command.
    async fn apply(
        &self,
        id: SubscriberId,
        out: &Sender,
        request: &Arc<Request>,
        command: Command,
    ) {
        match command {
            Command::Subscribe { channel, member } => {
                let kind = kind_of(&channel);
                if kind.is_gated() && !self.is_authorized(request, &channel).await {
                    let _ = out
                        .send(refusal(&channel, "not authorised to join this channel"))
                        .await;
                    return;
                }

                if kind == ChannelKind::Presence {
                    let Some(member) = member else {
                        let _ = out
                            .send(refusal(
                                &channel,
                                "a presence channel needs `data.member` with an `id`",
                            ))
                            .await;
                        return;
                    };

                    let present = self.join(&channel, id, out.clone(), member);
                    let _ = out.send(Message::json(&envelope("subscribed", &channel, Json::Null))).await;

                    let roster = Json::object([(
                        "members",
                        Json::Array(present.iter().map(Member::to_json).collect()),
                    )]);
                    let _ =
                        out.send(Message::json(&envelope("presence.here", &channel, roster))).await;
                    return;
                }

                self.subscribe(&channel, id, out.clone());
                let _ = out.send(Message::json(&envelope("subscribed", &channel, Json::Null))).await;
            }
            Command::Unsubscribe { channel } => {
                self.unsubscribe(&channel, id);
                let _ =
                    out.send(Message::json(&envelope("unsubscribed", &channel, Json::Null))).await;
            }
        }
    }

    async fn is_authorized(&self, request: &Arc<Request>, channel: &str) -> bool {
        match &self.authorizer {
            Some(authorizer) => {
                authorizer.authorize(Arc::clone(request), channel.to_string()).await
            }
            // No callback registered: refuse. Forgetting to write the check
            // should close the channel, not open it to everyone.
            None => false,
        }
    }

    fn announce_departure(&self, channel: &str, member: &Member) {
        let message = Message::json(&envelope(
            "presence.left",
            channel,
            Json::object([("member", member.to_json())]),
        ));
        // Failures are not chased here: a subscriber that has also gone away
        // gets noticed by the next broadcast, and cascading departure notices
        // through departure notices would be unbounded work on a bad day.
        self.deliver(channel, &message, None);
    }

    /// Push a message at a channel, collecting the subscribers that failed.
    fn deliver(&self, channel: &str, message: &Message, skip: Option<SubscriberId>) -> Delivery {
        let mut delivery = Delivery::default();
        let registry = self.lock();

        let Some(subscriptions) = registry.channels.get(channel) else {
            return delivery;
        };
        for subscription in subscriptions {
            if Some(subscription.id) == skip {
                continue;
            }
            // `try_send`, never `send`: a full queue means this client is too
            // slow, and the whole fan-out must not wait behind it.
            match subscription.sender.try_send(message.clone()) {
                Ok(()) => delivery.delivered += 1,
                Err(_) => delivery.dropped.push(subscription.id),
            }
        }
        delivery
    }

    /// A poisoned registry is recovered rather than propagated: one panicking
    /// subscriber must not take the whole broadcaster down with it.
    fn lock(&self) -> MutexGuard<'_, Registry> {
        self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// The one envelope shape every server-sent frame uses.
pub fn envelope(event: &str, channel: &str, data: Json) -> Json {
    Json::object([
        ("event", Json::from(event)),
        ("channel", Json::from(channel)),
        ("data", data),
    ])
}

fn refusal(channel: &str, reason: &str) -> Message {
    Message::json(&envelope(
        "error",
        channel,
        Json::object([("message", Json::from(reason))]),
    ))
}

/// What a client asked for.
#[derive(Debug, Clone, PartialEq)]
enum Command {
    Subscribe { channel: String, member: Option<Member> },
    Unsubscribe { channel: String },
}

impl Command {
    fn parse(text: &str) -> Result<Command, String> {
        let value =
            Json::parse(text).map_err(|error| format!("payload is not valid JSON: {error}"))?;

        let event = value
            .get("event")
            .and_then(Json::as_str)
            .ok_or_else(|| "missing an `event` field".to_string())?;

        let channel = value
            .get("channel")
            .and_then(Json::as_str)
            .filter(|channel| !channel.is_empty())
            .ok_or_else(|| format!("`{event}` needs a `channel`"))?
            .to_string();

        match event {
            "subscribe" => {
                let member = value.get("data.member").and_then(Member::from_json);
                Ok(Command::Subscribe { channel, member })
            }
            "unsubscribe" => Ok(Command::Unsubscribe { channel }),
            other => Err(format!("`{other}` is not a channel command")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::{WebSocketConfig, channel};
    use crate::frame::{Frame, OpCode, Role};
    use rustlavel_http::{Method, Upgraded};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::mpsc::Receiver;

    fn request() -> Arc<Request> {
        Arc::new(Request::new(Method::Get, "/broadcasting"))
    }

    /// The next message a subscriber would have received, as JSON.
    fn next(inbox: &mut Receiver<Message>) -> Json {
        let message = inbox.try_recv().expect("a message was waiting");
        Json::parse(message.as_text().expect("channel frames are text")).unwrap()
    }

    #[test]
    fn fans_a_message_out_to_every_subscriber_of_that_channel() {
        let broadcaster = Broadcaster::new();
        let (first, mut first_inbox) = channel(8);
        let (second, mut second_inbox) = channel(8);
        let (elsewhere, mut elsewhere_inbox) = channel(8);

        broadcaster.subscribe("orders", broadcaster.reserve(), first);
        broadcaster.subscribe("orders", broadcaster.reserve(), second);
        broadcaster.subscribe("invoices", broadcaster.reserve(), elsewhere);

        let delivered = broadcaster
            .broadcast("orders", "order.created", Json::object([("id", Json::from(7))]));

        assert_eq!(delivered, 2);
        let expected = Json::parse(
            r#"{"channel":"orders","data":{"id":7},"event":"order.created"}"#,
        )
        .unwrap();
        assert_eq!(next(&mut first_inbox), expected);
        assert_eq!(next(&mut second_inbox), expected);
        assert!(elsewhere_inbox.try_recv().is_err(), "other channels are untouched");
    }

    #[test]
    fn broadcasting_to_an_empty_channel_reaches_nobody_and_is_not_an_error() {
        assert_eq!(Broadcaster::new().broadcast("nobody", "tick", Json::Null), 0);
    }

    #[test]
    fn unsubscribing_stops_the_messages() {
        let broadcaster = Broadcaster::new();
        let (sender, mut inbox) = channel(8);
        let id = broadcaster.reserve();

        broadcaster.subscribe("orders", id, sender);
        broadcaster.unsubscribe("orders", id);

        assert_eq!(broadcaster.broadcast("orders", "order.created", Json::Null), 0);
        assert!(inbox.try_recv().is_err());
        assert_eq!(broadcaster.channels(), Vec::<String>::new());
    }

    #[test]
    fn subscribing_twice_does_not_double_the_delivery() {
        let broadcaster = Broadcaster::new();
        let (sender, mut inbox) = channel(8);
        let id = broadcaster.reserve();

        broadcaster.subscribe("orders", id, sender.clone());
        broadcaster.subscribe("orders", id, sender);

        assert_eq!(broadcaster.broadcast("orders", "tick", Json::Null), 1);
        assert!(inbox.try_recv().is_ok());
        assert!(inbox.try_recv().is_err());
    }

    #[test]
    fn a_subscriber_that_cannot_keep_up_is_dropped_rather_than_queued_for() {
        let broadcaster = Broadcaster::new();
        // A queue of one: this client is one message behind from the start.
        let (slow, _slow_inbox) = channel(1);
        let (quick, mut quick_inbox) = channel(8);

        broadcaster.subscribe("orders", broadcaster.reserve(), slow);
        broadcaster.subscribe("orders", broadcaster.reserve(), quick);

        // The first fills the slow client's only slot.
        assert_eq!(broadcaster.broadcast("orders", "tick", Json::Null), 2);
        // The second has nowhere to put it, so that subscriber goes away —
        // memory must not grow because one browser stopped reading.
        assert_eq!(broadcaster.broadcast("orders", "tick", Json::Null), 1);

        assert_eq!(broadcaster.subscribers("orders"), 1, "the slow client was dropped");
        // The client that was keeping up is unaffected and got both.
        assert!(quick_inbox.try_recv().is_ok());
        assert!(quick_inbox.try_recv().is_ok());
    }

    #[test]
    fn a_subscriber_whose_socket_is_gone_is_dropped_too() {
        let broadcaster = Broadcaster::new();
        let (sender, inbox) = channel(8);
        broadcaster.subscribe("orders", broadcaster.reserve(), sender);
        drop(inbox);

        assert_eq!(broadcaster.broadcast("orders", "tick", Json::Null), 0);
        assert_eq!(broadcaster.subscribers("orders"), 0);
    }

    #[test]
    fn channel_names_declare_their_own_policy() {
        assert_eq!(kind_of("orders"), ChannelKind::Public);
        assert_eq!(kind_of("private-orders"), ChannelKind::Private);
        assert_eq!(kind_of("presence-chat"), ChannelKind::Presence);
        assert!(!ChannelKind::Public.is_gated());
        assert!(ChannelKind::Private.is_gated());
    }

    #[tokio::test]
    async fn a_private_channel_refuses_a_subscribe_the_application_did_not_authorise() {
        let broadcaster = Broadcaster::new()
            .authorize(|_request: &Request, channel: &str| channel == "private-mine");
        let (out, mut inbox) = channel(8);
        let id = broadcaster.reserve();

        broadcaster
            .apply(
                id,
                &out,
                &request(),
                Command::Subscribe { channel: "private-yours".into(), member: None },
            )
            .await;

        let refused = next(&mut inbox);
        assert_eq!(refused.get("event").and_then(Json::as_str), Some("error"));
        assert_eq!(refused.get("channel").and_then(Json::as_str), Some("private-yours"));
        assert_eq!(
            refused.get("data.message").and_then(Json::as_str),
            Some("not authorised to join this channel")
        );
        assert_eq!(broadcaster.subscribers("private-yours"), 0);
    }

    #[tokio::test]
    async fn a_private_channel_admits_a_subscribe_the_application_authorised() {
        let broadcaster = Broadcaster::new()
            .authorize(|_request: &Request, channel: &str| channel == "private-mine");
        let (out, mut inbox) = channel(8);

        broadcaster
            .apply(
                broadcaster.reserve(),
                &out,
                &request(),
                Command::Subscribe { channel: "private-mine".into(), member: None },
            )
            .await;

        assert_eq!(next(&mut inbox).get("event").and_then(Json::as_str), Some("subscribed"));
        assert_eq!(broadcaster.subscribers("private-mine"), 1);
    }

    #[tokio::test]
    async fn an_async_authorisation_check_is_awaited() {
        let broadcaster = Broadcaster::new().authorize(authorize_async(
            |_request: Arc<Request>, channel: String| async move {
                // Stands in for a database lookup.
                tokio::task::yield_now().await;
                channel.ends_with("-allowed")
            },
        ));
        let (out, mut inbox) = channel(8);

        broadcaster
            .apply(
                broadcaster.reserve(),
                &out,
                &request(),
                Command::Subscribe { channel: "private-allowed".into(), member: None },
            )
            .await;

        assert_eq!(next(&mut inbox).get("event").and_then(Json::as_str), Some("subscribed"));
    }

    #[tokio::test]
    async fn a_broadcaster_with_no_authorizer_closes_every_gated_channel() {
        let broadcaster = Broadcaster::new();
        let (out, mut inbox) = channel(8);

        for gated in ["private-books", "presence-chat"] {
            broadcaster
                .apply(
                    broadcaster.reserve(),
                    &out,
                    &request(),
                    Command::Subscribe { channel: gated.into(), member: None },
                )
                .await;

            assert_eq!(next(&mut inbox).get("event").and_then(Json::as_str), Some("error"));
            assert_eq!(broadcaster.subscribers(gated), 0);
        }
    }

    #[test]
    fn joining_a_presence_channel_announces_the_member_and_hands_back_the_room() {
        let broadcaster = Broadcaster::new();
        let (ada, mut ada_inbox) = channel(8);
        let (alan, mut alan_inbox) = channel(8);

        let already = broadcaster.join(
            "presence-chat",
            broadcaster.reserve(),
            ada,
            Member::new("1", Json::object([("name", Json::from("Ada"))])),
        );
        assert!(already.is_empty(), "the first to arrive finds an empty room");

        let present = broadcaster.join(
            "presence-chat",
            broadcaster.reserve(),
            alan,
            Member::new("2", Json::object([("name", Json::from("Alan"))])),
        );

        // The joiner is told who was already here…
        assert_eq!(present.len(), 1);
        assert_eq!(present[0].id, "1");
        // …and everyone else is told about the joiner, but not about themselves.
        let announcement = next(&mut ada_inbox);
        assert_eq!(announcement.get("event").and_then(Json::as_str), Some("presence.joined"));
        assert_eq!(announcement.get("data.member.id").and_then(Json::as_str), Some("2"));
        assert!(alan_inbox.try_recv().is_err(), "a joiner is not told about itself");

        assert_eq!(broadcaster.members("presence-chat").len(), 2);
    }

    #[test]
    fn leaving_a_presence_channel_is_announced_to_the_rest() {
        let broadcaster = Broadcaster::new();
        let (ada, mut ada_inbox) = channel(8);
        let (alan, _alan_inbox) = channel(8);
        let ada_id = broadcaster.reserve();
        let alan_id = broadcaster.reserve();

        broadcaster.join("presence-chat", ada_id, ada, Member::new("1", Json::Null));
        broadcaster.join("presence-chat", alan_id, alan, Member::new("2", Json::Null));
        let _ = ada_inbox.try_recv(); // the join notice

        broadcaster.unsubscribe("presence-chat", alan_id);

        let departure = next(&mut ada_inbox);
        assert_eq!(departure.get("event").and_then(Json::as_str), Some("presence.left"));
        assert_eq!(departure.get("data.member.id").and_then(Json::as_str), Some("2"));
        assert_eq!(broadcaster.members("presence-chat").len(), 1);
    }

    #[test]
    fn disconnecting_leaves_every_channel_and_announces_each_departure() {
        let broadcaster = Broadcaster::new();
        let (watcher, mut watcher_inbox) = channel(8);
        let (leaving, _leaving_inbox) = channel(8);
        let leaving_id = broadcaster.reserve();

        broadcaster.join("presence-chat", broadcaster.reserve(), watcher, Member::new("1", Json::Null));
        broadcaster.join("presence-chat", leaving_id, leaving.clone(), Member::new("2", Json::Null));
        broadcaster.subscribe("orders", leaving_id, leaving);
        let _ = watcher_inbox.try_recv(); // the join notice

        broadcaster.disconnect(leaving_id);

        assert_eq!(
            next(&mut watcher_inbox).get("event").and_then(Json::as_str),
            Some("presence.left")
        );
        assert_eq!(broadcaster.subscribers("orders"), 0);
        assert_eq!(broadcaster.channels(), ["presence-chat"]);
    }

    #[tokio::test]
    async fn a_presence_subscribe_without_a_member_is_refused() {
        let broadcaster = Broadcaster::new().authorize(|_r: &Request, _c: &str| true);
        let (out, mut inbox) = channel(8);

        broadcaster
            .apply(
                broadcaster.reserve(),
                &out,
                &request(),
                Command::Subscribe { channel: "presence-chat".into(), member: None },
            )
            .await;

        let refused = next(&mut inbox);
        assert_eq!(refused.get("event").and_then(Json::as_str), Some("error"));
        assert!(
            refused.get("data.message").and_then(Json::as_str).unwrap().contains("data.member"),
            "{refused}"
        );
    }

    #[tokio::test]
    async fn a_presence_subscribe_answers_with_the_roster() {
        let broadcaster = Broadcaster::new().authorize(|_r: &Request, _c: &str| true);
        let (ada, _ada_inbox) = channel(8);
        broadcaster.join("presence-chat", broadcaster.reserve(), ada, Member::new("1", Json::Null));

        let (out, mut inbox) = channel(8);
        broadcaster
            .apply(
                broadcaster.reserve(),
                &out,
                &request(),
                Command::Subscribe {
                    channel: "presence-chat".into(),
                    member: Some(Member::new("2", Json::Null)),
                },
            )
            .await;

        assert_eq!(next(&mut inbox).get("event").and_then(Json::as_str), Some("subscribed"));
        let here = next(&mut inbox);
        assert_eq!(here.get("event").and_then(Json::as_str), Some("presence.here"));
        assert_eq!(here.get("data.members.0.id").and_then(Json::as_str), Some("1"));
    }

    #[test]
    fn parses_the_documented_client_commands() {
        assert_eq!(
            Command::parse(r#"{"event":"subscribe","channel":"orders"}"#).unwrap(),
            Command::Subscribe { channel: "orders".into(), member: None }
        );
        assert_eq!(
            Command::parse(r#"{"event":"unsubscribe","channel":"orders"}"#).unwrap(),
            Command::Unsubscribe { channel: "orders".into() }
        );
        assert_eq!(
            Command::parse(
                r#"{"event":"subscribe","channel":"presence-chat",
                    "data":{"member":{"id":"7","info":{"name":"Ada"}}}}"#
            )
            .unwrap(),
            Command::Subscribe {
                channel: "presence-chat".into(),
                member: Some(Member::new("7", Json::object([("name", Json::from("Ada"))]))),
            }
        );
    }

    #[test]
    fn malformed_commands_say_what_was_wrong() {
        assert!(Command::parse("not json").unwrap_err().contains("valid JSON"));
        assert!(Command::parse("{}").unwrap_err().contains("`event`"));
        assert!(
            Command::parse(r#"{"event":"subscribe"}"#).unwrap_err().contains("`channel`")
        );
        assert!(
            Command::parse(r#"{"event":"dance","channel":"orders"}"#)
                .unwrap_err()
                .contains("not a channel command")
        );
    }

    #[test]
    fn a_broadcast_is_reported_without_its_payload() {
        let _guard = crate::testing::events_lock();
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&recorded);
        // Other tests broadcast at the same time on the same process-wide bus,
        // so this subscriber only keeps its own channel — and never unwraps a
        // lock it might have poisoned, which would fail an unrelated test.
        events::subscribe(move |event: &Event| {
            let mine = event.kind == "broadcast.sent"
                && event.field("channel").and_then(Json::as_str) == Some("orders-instrumented");
            if mine {
                sink.lock().unwrap_or_else(|p| p.into_inner()).push(event.fields.clone());
            }
        });

        let broadcaster = Broadcaster::new();
        let (sender, _inbox) = channel(8);
        broadcaster.subscribe("orders-instrumented", broadcaster.reserve(), sender);
        broadcaster.broadcast(
            "orders-instrumented",
            "order.created",
            Json::object([("card", Json::from("4111111111111111"))]),
        );

        let fields = recorded.lock().unwrap_or_else(|p| p.into_inner());
        let fields = fields.first().expect("one broadcast was recorded");
        assert_eq!(fields.get("channel").and_then(Json::as_str), Some("orders-instrumented"));
        assert_eq!(fields.get("event").and_then(Json::as_str), Some("order.created"));
        assert_eq!(fields.get("subscribers").and_then(Json::as_i64), Some(1));
        assert_eq!(fields.get("dropped").and_then(Json::as_i64), Some(0));
        assert!(!fields.contains_key("data"), "the payload is never recorded");
    }

    #[tokio::test]
    async fn the_broadcaster_mounts_as_an_ordinary_route() {
        let mut router = rustlavel_http::Router::new();
        router.get("/broadcasting", Broadcaster::new().route());
        router.finalize();

        let request = Request::new(Method::Get, "/broadcasting")
            .with_header("upgrade", "websocket")
            .with_header("connection", "Upgrade")
            .with_header("sec-websocket-version", "13")
            .with_header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==");

        let response = router.dispatch(request).await;

        assert_eq!(response.status, rustlavel_http::Status(101));
        assert!(response.upgrades());
    }

    /// The whole path over an in-memory pipe: a client subscribes, the
    /// application broadcasts, the client receives it.
    #[tokio::test]
    async fn a_client_subscribes_over_its_socket_and_receives_a_broadcast() {
        let broadcaster = Broadcaster::new();
        let (mut client, server) = tokio::io::duplex(8192);
        let (reader, writer) = tokio::io::split(server);
        let socket = WebSocket::new(
            Upgraded {
                reader: Box::new(reader),
                writer: Box::new(writer),
                buffered: Frame::client(
                    OpCode::Text,
                    br#"{"event":"subscribe","channel":"orders"}"#.to_vec(),
                    [4, 3, 2, 1],
                )
                .encode(),
            },
            WebSocketConfig { idle_timeout: None, ..WebSocketConfig::default() },
        );

        let serving = tokio::spawn({
            let broadcaster = broadcaster.clone();
            async move {
                broadcaster.serve(socket, Request::new(Method::Get, "/broadcasting")).await;
            }
        });

        // Read the acknowledgement, which also proves the subscribe landed.
        let mut inbox = Vec::new();
        let ack = read_frame(&mut client, &mut inbox).await;
        assert_eq!(
            Json::parse(std::str::from_utf8(&ack.payload).unwrap())
                .unwrap()
                .get("event")
                .and_then(Json::as_str),
            Some("subscribed")
        );

        assert_eq!(broadcaster.broadcast("orders", "order.created", Json::from(7)), 1);
        let event = read_frame(&mut client, &mut inbox).await;
        let payload = Json::parse(std::str::from_utf8(&event.payload).unwrap()).unwrap();
        assert_eq!(payload.get("event").and_then(Json::as_str), Some("order.created"));
        assert_eq!(payload.get("data").and_then(Json::as_i64), Some(7));

        // Hanging up removes the subscriber without anyone having to say so.
        client.shutdown().await.unwrap();
        drop(client);
        serving.await.unwrap();
        assert_eq!(broadcaster.subscribers("orders"), 0);
    }

    async fn read_frame(
        client: &mut tokio::io::DuplexStream,
        buffer: &mut Vec<u8>,
    ) -> Frame {
        loop {
            if let Some((frame, used)) = Frame::decode(buffer, Role::Client, 1 << 20).unwrap() {
                buffer.drain(..used);
                return frame;
            }
            let mut chunk = [0u8; 4096];
            let read = client.read(&mut chunk).await.unwrap();
            assert!(read > 0, "the server closed unexpectedly");
            buffer.extend_from_slice(&chunk[..read]);
        }
    }
}
