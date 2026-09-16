//! The database stores, against a real database.
//!
//! **These prove the two things a unit test cannot.** `consume` must spend a
//! code exactly once and `rotate` must rotate a refresh token exactly once,
//! whatever else is in flight — and an implementation that reads first and
//! writes second passes every single-threaded test while leaving the window
//! open. So both are driven from many tasks at once here, and the assertion is
//! that exactly one of them won.
//!
//! Skipped with a printed line when `OAUTH_TEST_DATABASE_URL` is unset, rather
//! than failing: a contributor without a database should be able to run the
//! suite, and a test that needs one should say so rather than look broken.

#![cfg(feature = "db")]

use std::sync::Arc;

use rustlavel_db::Database;
use rustlavel_oauth_provider::code::{AuthorizationCode, CodeStore, Consumption};
use rustlavel_oauth_provider::consent::{ConsentStore, Grant};
use rustlavel_oauth_provider::database::{
    DatabaseClientStore, DatabaseCodeStore, DatabaseConsentStore, DatabaseTokenStore, drop_schema,
    schema,
};
use rustlavel_oauth_provider::token::{AccessToken, RefreshToken, TokenStore};
use rustlavel_oauth_provider::{Client, ClientStore, Scopes};

/// A database with the four tables, or `None` when there is nothing to connect
/// to. Each test gets its own schema-free copy by dropping and recreating, and
/// the tables are name-spaced by nothing — so these run serially by sharing one
/// URL and cleaning up after themselves.
async fn database() -> Option<Database> {
    let url = std::env::var("OAUTH_TEST_DATABASE_URL").ok()?;
    let db = Database::connect(&url).await.expect("the test database is reachable");
    let builder = rustlavel_db::Schema::new(&db);
    let _ = drop_schema(&builder).await;
    schema(&builder).await.expect("the schema is created");
    Some(db)
}

macro_rules! db_or_skip {
    () => {
        match database().await {
            Some(db) => db,
            None => {
                println!("skipped: set OAUTH_TEST_DATABASE_URL to run the database store tests");
                return;
            }
        }
    };
}

fn code(hash: &str, expires_at: u64) -> AuthorizationCode {
    let mut code = AuthorizationCode::from_hash(hash);
    code.client_id = "checkout".into();
    code.user_id = "42".into();
    code.redirect_uri = "https://checkout.test/callback".into();
    code.scopes = Scopes::of(["orders.read"]);
    code.issued_at = 1000;
    code.expires_at = expires_at;
    code
}

#[tokio::test]
async fn a_client_survives_the_round_trip() {
    let db = db_or_skip!();
    let store = DatabaseClientStore::new(db.clone());

    let client = Client::confidential("checkout", "s3cret")
        .named("Checkout")
        .redirect_uri("https://checkout.test/callback")
        .scopes(Scopes::of(["orders.read", "orders.write"]))
        .first_party();
    store.create(&client, Some("s3cret")).await.expect("created");

    let found = store.find("checkout").await.expect("read").expect("the client exists");
    assert_eq!(found.id, "checkout");
    assert_eq!(found.name, "Checkout");
    assert!(found.first_party);
    assert!(found.scopes.contains("orders.write"));
    assert_eq!(found.redirect_uris, vec!["https://checkout.test/callback".to_string()]);
    // The secret verifies, and the plaintext was never stored.
    assert!(found.verify_secret("s3cret"));
    assert!(!found.verify_secret("wrong"));

    assert!(store.find("nobody").await.expect("read").is_none());
}

/// A client with no secret is public and proves itself with PKCE. Reading it
/// back as confidential would let anybody who knows the id be that client.
#[tokio::test]
async fn a_client_without_a_secret_is_read_back_as_public() {
    let db = db_or_skip!();
    let store = DatabaseClientStore::new(db.clone());

    store.create(&Client::public("mobile"), None).await.expect("created");
    let found = store.find("mobile").await.expect("read").expect("it exists");
    assert!(!found.verify_secret(""), "a public client accepted an empty secret");
    assert!(!found.verify_secret("anything"));
}

#[tokio::test]
async fn a_code_is_spent_once_and_replayed_after() {
    let db = db_or_skip!();
    let store = DatabaseCodeStore::new(db.clone());

    store.store(code("hash-1", 9999)).await.expect("stored");

    match store.consume("hash-1", "family-a", 2000).await.expect("consumed") {
        Consumption::Fresh(code) => {
            assert_eq!(code.client_id, "checkout");
            assert_eq!(code.user_id, "42");
            assert_eq!(code.redirect_uri, "https://checkout.test/callback");
            assert!(code.scopes.contains("orders.read"));
        }
        other => panic!("the first exchange was not fresh: {other:?}"),
    }

    // The second must name the family, so the caller knows what to revoke.
    match store.consume("hash-1", "family-b", 2001).await.expect("consumed") {
        Consumption::Replayed { family } => assert_eq!(family.as_deref(), Some("family-a")),
        other => panic!("a spent code was not reported as a replay: {other:?}"),
    }

    assert!(matches!(
        store.consume("nothing", "family-c", 2002).await.expect("consumed"),
        Consumption::Unknown
    ));
}

#[tokio::test]
async fn an_expired_code_is_reported_as_expired() {
    let db = db_or_skip!();
    let store = DatabaseCodeStore::new(db.clone());

    store.store(code("hash-expired", 1500)).await.expect("stored");
    assert!(matches!(
        store.consume("hash-expired", "family-a", 2000).await.expect("consumed"),
        Consumption::Expired
    ));

    // And it is still spent, so presenting it again is a replay rather than a
    // second "expired" — which would tell an attacker that nothing happened.
    assert!(matches!(
        store.consume("hash-expired", "family-b", 2001).await.expect("consumed"),
        Consumption::Replayed { .. }
    ));
}

/// **The test this file exists for.** Sixteen tasks present the same code at
/// once; exactly one may be told `Fresh`. An implementation that reads then
/// writes passes every test above and fails this one.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn only_one_of_many_simultaneous_exchanges_spends_a_code() {
    let db = db_or_skip!();
    let store = Arc::new(DatabaseCodeStore::new(db.clone()));
    store.store(code("hash-race", 9999)).await.expect("stored");

    let mut racing = Vec::new();
    for n in 0..16 {
        let store = Arc::clone(&store);
        racing.push(tokio::spawn(async move {
            store.consume("hash-race", &format!("family-{n}"), 2000).await
        }));
    }

    let mut fresh = 0;
    let mut replayed = 0;
    for task in racing {
        match task.await.expect("the task finished").expect("the store answered") {
            Consumption::Fresh(_) => fresh += 1,
            Consumption::Replayed { .. } => replayed += 1,
            other => panic!("unexpected answer under contention: {other:?}"),
        }
    }

    assert_eq!(fresh, 1, "{fresh} exchanges were told the code was fresh; exactly one may be");
    assert_eq!(replayed, 15);
}

fn access(id: &str, hash: &str, family: &str) -> AccessToken {
    let mut token = AccessToken::from_hash(id, hash);
    token.client_id = "checkout".into();
    token.user_id = Some("42".into());
    token.scopes = Scopes::of(["orders.read"]);
    token.family = family.into();
    token.issued_at = 1000;
    token.expires_at = 9999;
    token
}

fn refresh(id: &str, hash: &str, family: &str) -> RefreshToken {
    let mut token = RefreshToken::from_hash(id, hash);
    token.client_id = "checkout".into();
    token.user_id = Some("42".into());
    token.scopes = Scopes::of(["orders.read"]);
    token.family = family.into();
    token.access_token_id = "access-1".into();
    token.issued_at = 1000;
    token.expires_at = 9999;
    token
}

#[tokio::test]
async fn tokens_survive_the_round_trip() {
    let db = db_or_skip!();
    let store = DatabaseTokenStore::new(db.clone());

    store.store_access(access("access-1", "ahash", "family-a")).await.expect("stored");
    store.store_refresh(refresh("refresh-1", "rhash", "family-a")).await.expect("stored");

    let found = store.find_access("ahash").await.expect("read").expect("it exists");
    assert_eq!(found.id, "access-1");
    assert_eq!(found.user_id.as_deref(), Some("42"));
    assert_eq!(found.family, "family-a");
    assert!(found.is_live(2000));

    let found = store.find_refresh("rhash").await.expect("read").expect("it exists");
    assert_eq!(found.access_token_id, "access-1");
    assert!(!found.rotated);

    assert!(store.find_access("nothing").await.expect("read").is_none());
}

/// `None` and `Some("")` are different facts: a machine's token has no user,
/// and an empty user id would be a bug that looks like a person.
#[tokio::test]
async fn a_machines_token_has_no_user() {
    let db = db_or_skip!();
    let store = DatabaseTokenStore::new(db.clone());

    let mut token = access("access-machine", "mhash", "family-m");
    token.user_id = None;
    store.store_access(token).await.expect("stored");

    let found = store.find_access("mhash").await.expect("read").expect("it exists");
    assert_eq!(found.user_id, None, "a client-credentials token gained a user");
}

/// **The second test this file exists for.** Two requests presenting the same
/// live refresh token must not both be told yes — the loser is how a stolen
/// token is caught.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn only_one_of_many_simultaneous_refreshes_rotates_a_token() {
    let db = db_or_skip!();
    let store = Arc::new(DatabaseTokenStore::new(db.clone()));
    store.store_refresh(refresh("refresh-race", "rrhash", "family-r")).await.expect("stored");

    let mut racing = Vec::new();
    for _ in 0..16 {
        let store = Arc::clone(&store);
        racing.push(tokio::spawn(async move { store.rotate("refresh-race").await }));
    }

    let mut won = 0;
    for task in racing {
        if task.await.expect("the task finished").expect("the store answered") {
            won += 1;
        }
    }
    assert_eq!(won, 1, "{won} requests were allowed to rotate the same token; exactly one may be");

    // And it stays rotated, so a later presentation is caught too.
    assert!(!store.rotate("refresh-race").await.expect("answered"));
    assert!(store.find_refresh("rrhash").await.expect("read").expect("exists").rotated);
}

/// Revoking a family must take the access tokens with it. Leaving them live
/// would keep the window open for their full lifetime — the window the
/// revocation exists to close.
#[tokio::test]
async fn revoking_a_family_takes_both_kinds_of_token() {
    let db = db_or_skip!();
    let store = DatabaseTokenStore::new(db.clone());

    store.store_access(access("a1", "a1h", "doomed")).await.expect("stored");
    store.store_access(access("a2", "a2h", "doomed")).await.expect("stored");
    store.store_refresh(refresh("r1", "r1h", "doomed")).await.expect("stored");
    store.store_access(access("a3", "a3h", "innocent")).await.expect("stored");

    let revoked = store.revoke_family("doomed").await.expect("revoked");
    assert_eq!(revoked, 3, "the access tokens were left live");

    assert!(store.find_access("a1h").await.expect("read").expect("exists").revoked);
    assert!(store.find_refresh("r1h").await.expect("read").expect("exists").revoked);
    assert!(!store.find_access("a3h").await.expect("read").expect("exists").revoked);

    // Revoking again changes nothing, so a repeated report is not a second
    // count of the same tokens.
    assert_eq!(store.revoke_family("doomed").await.expect("revoked"), 0);
}

#[tokio::test]
async fn expired_tokens_are_purged_and_live_ones_are_not() {
    let db = db_or_skip!();
    let store = DatabaseTokenStore::new(db.clone());

    let mut stale = access("old", "oldh", "f");
    stale.expires_at = 1200;
    store.store_access(stale).await.expect("stored");
    store.store_access(access("live", "liveh", "f")).await.expect("stored");

    assert_eq!(store.purge(2000).await.expect("purged"), 1);
    assert!(store.find_access("oldh").await.expect("read").is_none());
    assert!(store.find_access("liveh").await.expect("read").is_some());
}

#[tokio::test]
async fn consent_is_replaced_rather_than_accumulated() {
    let db = db_or_skip!();
    let store = DatabaseConsentStore::new(db.clone());

    store
        .record(Grant {
            client_id: "checkout".into(),
            user_id: "42".into(),
            scopes: Scopes::of(["orders.read", "orders.write"]),
            granted_at: 1000,
        })
        .await
        .expect("recorded");

    // The scopes on the screen the user just approved are the scopes they
    // agreed to. Accumulating would mean a grant nobody ever saw in full.
    store
        .record(Grant {
            client_id: "checkout".into(),
            user_id: "42".into(),
            scopes: Scopes::of(["orders.read"]),
            granted_at: 2000,
        })
        .await
        .expect("recorded");

    let grant = store.find("checkout", "42").await.expect("read").expect("it exists");
    assert!(grant.scopes.contains("orders.read"));
    assert!(!grant.scopes.contains("orders.write"), "an earlier grant survived the replacement");
    assert_eq!(grant.granted_at, 2000);

    store.forget("checkout", "42").await.expect("forgotten");
    assert!(store.find("checkout", "42").await.expect("read").is_none());
    // And one person withdrawing consent does not withdraw anybody else's.
    assert!(store.find("checkout", "99").await.expect("read").is_none());
}
