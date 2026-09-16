//! The domain this service owns.
//!
//! Deliberately small, and deliberately real: one table, one read, one write,
//! and a scope check on each. A resource server that answers only `/api/me`
//! demonstrates the token plumbing and nothing about the shape of a service —
//! and it leaves a database connected at boot that no route ever reads.
//!
//! **Rows are scoped to the caller's owner.** A person's rows belong to their
//! subject; a service's belong to its client id, because a `client_credentials`
//! token has no person behind it. The two can never collide — they come from
//! different tables — and a service that could read a person's rows by holding
//! the right scope would be the whole point of the separation lost.
//!
//! The original of this said "the caller's subject", which was a `String` that
//! a machine-to-machine token never had. Not because the token cannot be
//! trusted, but because a resource server that filters only by the id in the
//! URL is one guessed id away from handing over somebody else's orders. The
//! subject comes from the token; the id comes from the request.

use rustlavel::prelude::*;

use crate::Caller;

/// What a caller must hold. Two, not one: a service that checks `orders` for
/// both reading and writing cannot issue a token that may only read.
pub const READ: &str = "orders.read";
pub const WRITE: &str = "orders.write";

pub fn routes(r: &mut Router) {
    r.get("/api/orders", index);
    r.post("/api/orders", store);
}

/// The caller, or the response that says why there isn't one.
fn caller(req: &Request, scope: &str) -> std::result::Result<Caller, Response> {
    let Some(caller) = req.extension::<Caller>() else {
        return Err(Response::new(Status::UNAUTHORIZED)
            .with_json(crate::problem("Unauthenticated.")));
    };

    if !caller.can(scope) {
        // 403, not 404. The caller is known and the answer is no — telling them
        // the route does not exist would send them looking for a typo.
        return Err(Response::new(Status::FORBIDDEN)
            .with_json(crate::problem(&format!("This token does not carry `{scope}`."))));
    }
    Ok(caller.clone())
}

async fn index(req: Request) -> Result<Response> {
    let caller = match caller(&req, READ) {
        Ok(caller) => caller,
        Err(response) => return Ok(response),
    };
    let db = req.state::<Database>().expect("the database is registered in main.rs");

    // Filtered by the subject in the token, never by anything in the request.
    let rows = db
        .table("orders")
        .filter("subject", caller.owner())
        .order_by("id", rustlavel::db::Direction::Desc)
        .limit(50)
        .get(db)
        .await?;

    Ok(Response::json(Json::object([(
        "data",
        Json::Array(rows.iter().map(as_json).collect()),
    )])))
}

async fn store(mut req: Request) -> Result<Response> {
    let caller = match caller(&req, WRITE) {
        Ok(caller) => caller,
        Err(response) => return Ok(response),
    };

    let body = req.json().cloned().unwrap_or(Json::Null);
    let description = body.get("description").and_then(Json::as_str).unwrap_or("").trim().to_string();
    let currency = body.get("currency").and_then(Json::as_str).unwrap_or("IDR").trim().to_uppercase();

    // Minor units, as an integer, and rejected rather than rounded. A request
    // that says 10.5 means cents this service would have to guess at.
    let amount = match body.get("amount_minor").and_then(Json::as_f64) {
        Some(amount) if amount.fract() == 0.0 && amount > 0.0 => amount as i64,
        _ => {
            return Ok(Response::new(Status::UNPROCESSABLE).with_json(crate::problem(
                "`amount_minor` must be a whole number of minor units, above zero.",
            )));
        }
    };

    if description.is_empty() {
        return Ok(Response::new(Status::UNPROCESSABLE)
            .with_json(crate::problem("`description` is required.")));
    }

    // **The caller's idempotency key, or one of ours.** A payment sent twice
    // because a phone lost signal mid-request is the failure this prevents, and
    // it costs one unique index.
    let reference = match req.header("idempotency-key").map(str::trim).filter(|key| !key.is_empty()) {
        Some(key) => format!("{}:{key}", caller.owner()),
        None => format!("{}:{}", caller.owner(), rustlavel::auth::random::hex(12)),
    };

    let db = req.state::<Database>().expect("the database is registered in main.rs").clone();

    // Already recorded under this key: answer with what was stored rather than
    // storing it again. Retrying must be safe, or clients stop retrying.
    if let Some(existing) =
        db.table("orders").filter("reference", reference.as_str()).first(&db).await?
    {
        return Ok(Response::new(Status::OK).with_json(as_json(&existing)));
    }

    db.table("orders")
        .insert(
            &db,
            &[
                ("subject", caller.owner().into()),
                ("reference", reference.as_str().into()),
                ("description", description.as_str().into()),
                ("amount_minor", amount.into()),
                ("currency", currency.as_str().into()),
                ("status", "recorded".into()),
            ],
        )
        .await?;

    let stored = db
        .table("orders")
        .filter("reference", reference.as_str())
        .first(&db)
        .await?
        .ok_or_else(|| Error::msg("the order was inserted and could not be read back"))?;

    Ok(Response::new(Status::CREATED).with_json(as_json(&stored)))
}

fn as_json(row: &rustlavel::db::Row) -> Json {
    // `amount_minor` stays a number and the rest are text. A column read as the
    // wrong type is an error rather than a default, so each falls back to null
    // instead of to a value that would look like data.
    let text = |column: &str| row.get::<String>(column).map_or(Json::Null, Json::from);
    let number = |column: &str| row.get::<i64>(column).map_or(Json::Null, Json::from);

    Json::object([
        ("id", number("id")),
        ("reference", text("reference")),
        ("description", text("description")),
        ("amount_minor", number("amount_minor")),
        ("currency", text("currency")),
        ("status", text("status")),
    ])
}
