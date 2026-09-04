//! Notifications: one person's, or everybody's.
//!
//! `user_id` null is the whole point of the shape. A notice for one person and
//! an announcement to everybody are the same thing to a reader, so they are the
//! same row — a broadcast is one addressed to nobody in particular. The
//! alternative, two tables and a union at read time, makes the list harder to
//! page and the read state harder to keep.

use rustlavel::db::migration;

migration!(
    CreateNotificationsTable,
    "2026_09_04_000100_create_notifications_table",
    up: |schema| {
        schema
            .create("notifications", |t| {
                t.id();

                // Null means everybody. Not a declared foreign key: a notice
                // outlives the account it was addressed to, the same way an
                // audit entry does.
                t.big_integer("user_id").nullable().index();

                // `info`, `success`, `warning`, `danger`. Anything else is
                // rendered as `info` rather than as nothing.
                t.string("level").default("info");

                t.string("title");
                t.text("body").nullable();

                // Where it takes you, if anywhere.
                t.string("url").nullable();

                // Who sent it. An announcement somebody has to answer for.
                t.big_integer("sent_by").nullable();

                t.timestamps();
            })
            .await?;

        // Read state is per person even for a broadcast, which is why it
        // cannot be a column on the row above: a flag there would let the
        // first reader mark an announcement read for everybody.
        schema
            .create("notification_reads", |t| {
                t.id();
                t.big_integer("notification_id").index();
                t.big_integer("user_id").index();
                t.timestamps();
            })
            .await
    },
    down: |schema| {
        schema.drop("notification_reads").await?;
        schema.drop("notifications").await
    },
);
