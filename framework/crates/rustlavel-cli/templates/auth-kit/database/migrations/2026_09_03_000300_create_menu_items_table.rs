//! The navigation, as rows rather than as markup.
//!
//! A sidebar written into a template is a sidebar only a developer can change.
//! This table is what Settings → Menus edits, and what the layout reads.

use rustlavel::db::migration;

migration!(
    CreateMenuItemsTable,
    "2026_09_03_000300_create_menu_items_table",
    up: |schema| {
        schema
            .create("menu_items", |t| {
                t.id();

                // Which menu this belongs to: `sidebar`, `topbar`, `portal`.
                // A string rather than a separate table, because a menu with
                // no items is not a thing anybody needs to store.
                t.string("location").index();

                // Self-referential, and nullable for a top-level item. Not a
                // declared foreign key: the delete is handled in code, which
                // has to decide what happens to the children — promoting them
                // is usually right, and a cascade would silently take a whole
                // branch with the parent.
                t.big_integer("parent_id").nullable().index();

                t.string("label");
                // The route name this points at, when it points at one of
                // ours. Kept beside `url` rather than instead of it: an
                // external link has no route, and a route that is renamed
                // should show as broken rather than resolve to nothing.
                t.string("route").nullable();
                t.string("url").nullable();
                // A name from the icon set the page lists, not markup. Markup
                // in a database column is markup nobody escapes.
                t.string("icon").nullable();

                // What a person may need to hold to see it. Empty means
                // everyone signed in.
                t.string("permission").nullable();

                t.integer("sort_order").default_int(0).index();
                t.boolean("is_active").default_bool(true);
                // `_blank` for an external link.
                t.string("target").nullable();

                t.timestamps();
            })
            .await
    },
    down: |schema| { schema.drop("menu_items").await },
);
