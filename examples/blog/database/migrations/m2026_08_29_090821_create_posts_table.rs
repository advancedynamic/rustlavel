//! create_posts_table

use rustlavel::db::migration;

migration!(
    CreatePostsTable,
    "2026_08_29_090821_create_posts_table",
    up: |schema| {
        schema
            .create("posts", |t| {
                t.id();
                t.string("title");
                t.text("body");
                t.boolean("published").default_bool(false);
                t.timestamps();
                t.index(&["published"]);
            })
            .await
    },
    down: |schema| { schema.drop("posts").await },
);
