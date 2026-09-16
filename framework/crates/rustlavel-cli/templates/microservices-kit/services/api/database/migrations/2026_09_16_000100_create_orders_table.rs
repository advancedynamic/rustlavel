//! The resource server's own table.
//!
//! **There is no user table here, and there must not be.** `subject` holds the
//! id the authorization server put in the token, and nothing else about the
//! person — no name, no email, no role. The moment this service stores those it
//! has a second copy of a record it does not own, and the two drift.

use rustlavel::db::migration;

migration!(
    CreateOrdersTable,
    "2026_09_16_000100_create_orders_table",
    up: |schema| {
        schema
            .create("orders", |t| {
                t.id();
                // The `sub` claim, as text. Not a foreign key: the users live
                // in another service's database, and a constraint across a
                // network boundary is not a constraint.
                t.string("subject");
                t.string("reference").unique();
                t.string("description");
                // Minor units — cents, sen — as an integer. A float here is how
                // money goes missing.
                t.big_integer("amount_minor");
                t.string("currency");
                t.string("status");
                t.timestamps();
            })
            .await?;

        Ok(())
    },
    down: |schema| { schema.drop("orders").await },
);
