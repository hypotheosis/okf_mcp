//! Two replicas over one Postgres and one Redis (milestone 16). Every test
//! sets `credential_ttl` and `catalog_ttl` to 1h (`TestServer::start_shared`):
//! what passes was carried by Redis announcements or by the change feed
//! (`NOTIFY`, milestone 18), not by a cache's expiry. The change-feed tests write through the store
//! on the pool, as the command line does, so no Redis announcement is sent
//! and only `NOTIFY` can carry the change. The MCP session
//! tests (milestone 16 Task 5) open a session on one replica and use it, or
//! are refused, on the other.

// The tests' futures hold whole servers, whose router's middleware stack
// nests deeply enough to overflow the default query depth (128) when their
// layout is computed, as in the library (`src/lib.rs`).
#![recursion_limit = "256"]

mod support;

use std::time::Duration;

use chrono::Utc;
use okf_mcp::archive::Files;
use okf_mcp::config::{Config, OAuthConfig};
use okf_mcp::store::audit::AuditActor;
use okf_mcp::store::keys::{self, Access, Grants, NewKey};
use okf_mcp::store::users::{self, NewUser};
use serde_json::json;
use support::mcp_raw::{
    delete_on, first_event_data, notify_on, open_session, post_on, stream_on, TOOLS_LIST,
};
use support::oauth::{sign_in_code, CimdHost, OAuthClient};
use support::redis::TestRedis;
use support::server::{call, files, Sibling, TestServer, OWNER};

/// Longest any wait for a replica to catch up may take.
const WAIT: Duration = Duration::from_secs(10);

fn pg() -> Vec<(&'static str, Files)> {
    vec![("main", files(&[("a.md", "---\ntype: Note\n---\n# A\n")]))]
}

/// Retries `check` every 50 ms until it is true, or panics naming `what`
/// after [`WAIT`].
macro_rules! eventually {
    // A macro rather than a function so the check can `.await` (an async
    // closure borrowing the test's locals is not expressible here).
    ($what:expr, $check:expr) => {{
        let waited = tokio::time::timeout(WAIT, async {
            while !$check {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
        assert!(waited.is_ok(), "timed out waiting until {}", $what);
    }};
}

/// Two replicas on one Redis, with an admin key known to both.
async fn pair(redis: &TestRedis) -> (TestServer, Sibling, String) {
    let a = TestServer::start_shared(pg(), redis, |_| Config::default()).await;
    let b = a.sibling(redis, |_| Config::default()).await;
    let admin = a.key("admin", Access::Admin, &[], None).await;
    // B has not polled: wait until it holds the admin key through Redis.
    eventually!(
        "B knows the admin key",
        b.app.find_key(&admin).await.unwrap().is_some()
    );
    (a, b, admin)
}

/// `POST`s, `PUT`s or `DELETE`s `path` on `base` as `key`, asserting success.
async fn api(method: reqwest::Method, base: &str, path: &str, key: &str, body: serde_json::Value) {
    let mut request = reqwest::Client::new()
        .request(method, format!("{base}{path}"))
        .bearer_auth(key);
    if !body.is_null() {
        request = request.json(&body);
    }
    let answer = request.send().await.expect("api call");
    assert!(answer.status().is_success(), "{path}: {}", answer.status());
}

/// Revokes through the API on A: the key is refused on B at once.
async fn revoked_key_is_refused_on_the_other(redis: &TestRedis) {
    let (a, b, admin) = pair(redis).await;
    let key = a.key("reader", Access::Read, &[], None).await;
    eventually!(
        "B knows the new key",
        b.app.find_key(&key).await.unwrap().is_some()
    );

    let id = b
        .app
        .find_key(&key)
        .await
        .unwrap()
        .expect("cached")
        .key_id
        .clone();
    api(
        reqwest::Method::POST,
        &a.base,
        &format!("/api/v1/keys/{id}/revoke"),
        &admin,
        serde_json::Value::Null,
    )
    .await;

    eventually!(
        "B refuses the revoked key",
        b.app.find_key(&key).await.unwrap().is_none()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_key_revoked_on_one_replica_is_refused_on_the_other_at_once() {
    let redis = TestRedis::start().await;
    revoked_key_is_refused_on_the_other(&redis).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_key_revoked_on_one_replica_is_refused_on_the_other_in_cluster_mode() {
    let redis = TestRedis::start_cluster().await;
    revoked_key_is_refused_on_the_other(&redis).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_user_deleted_on_one_replica_loses_access_on_the_other() {
    let redis = TestRedis::start().await;
    let (a, b, admin) = pair(&redis).await;
    let user = NewUser {
        name: "alice",
        display_name: &okf_mcp::labels::DisplayName::default(),
    };
    users::create(&a.db.pool, &user, &AuditActor::cli(), Utc::now())
        .await
        .expect("create alice");
    let new = NewKey {
        name: "alice-key",
        access: Access::Read,
        maintenance_report: false,
        grants: Grants::All,
        default_catalog: None,
        actor: "alice",
        user: "alice",
        self_issued: false,
        expires_at: None,
        levels: &[],
    };
    let key = keys::create(&a.db.pool, &new, &AuditActor::cli(), Utc::now())
        .await
        .expect("create alice's key");
    eventually!(
        "B knows alice's key",
        b.app.find_key(&key).await.unwrap().is_some()
    );

    api(
        reqwest::Method::DELETE,
        &a.base,
        "/api/v1/users/alice",
        &admin,
        serde_json::Value::Null,
    )
    .await;

    eventually!(
        "B no longer finds alice's key",
        b.app.find_key(&key).await.unwrap().is_none()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_team_suspended_on_one_replica_closes_its_catalogs_on_the_other() {
    let redis = TestRedis::start().await;
    let (a, b, admin) = pair(&redis).await;
    let reader = a.key("reader", Access::Read, &[], None).await;
    eventually!(
        "B knows the reader",
        b.app.find_key(&reader).await.unwrap().is_some()
    );
    let args = json!({"catalog": "main", "path": "a"});
    let client = tokio::time::timeout(WAIT, async {
        let url = b.mcp_url();
        // `client_at` is a method of the first replica's harness; the URL is B's.
        a.client_at(&reader, &url, &[]).await
    })
    .await
    .expect("connecting to B in time")
    .expect("a client on B");
    let before = call(&client, "get_concept", args.clone()).await;
    assert_ne!(before.is_error, Some(true), "{before:?}");

    api(
        reqwest::Method::PUT,
        &a.base,
        &format!("/api/v1/teams/{OWNER}/suspension"),
        &admin,
        json!({"suspended": true, "notice": "Contact the operator."}),
    )
    .await;

    let refused = tokio::time::timeout(WAIT, async {
        loop {
            let answer = call(&client, "get_concept", args.clone()).await;
            if answer.is_error == Some(true) {
                return answer;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("B refuses the suspended team's catalog in time");
    let text = format!("{refused:?}").to_lowercase();
    assert!(text.contains("suspend"), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_grant_revoked_on_one_replica_is_refused_on_the_other() {
    let redis = TestRedis::start().await;
    let config = |base: &str| Config {
        public_url: Some(base.to_string()),
        oauth: OAuthConfig {
            cimd_allow_private: true,
            ..OAuthConfig::default()
        },
        ..Config::default()
    };
    let a = TestServer::start_shared(pg(), &redis, config).await;
    let b = a.sibling(&redis, config).await;
    let host = CimdHost::start().await;
    let client = OAuthClient::new(&a, &host.client_id, &host.redirect_uri);
    let resource = client.discover().await.resource;
    let code = client
        .authorize(&sign_in_code(&a, Access::Read, &["main"]).await, &resource)
        .await;
    let tokens = client.exchange(&code, &resource).await;
    assert_eq!(tokens.status, 200, "{}", tokens.body);
    let oauth = a.app.oauth().expect("OAuth is on");
    let grant_id = oauth
        .access
        .verify(&a.app, &oauth.settings.resource, &tokens.access())
        .await
        .expect("verify")
        .expect("a principal")
        .key_id
        .clone();
    eventually!(
        "B knows the grant",
        b.app.find_grant(&grant_id).await.unwrap().is_some()
    );

    keys::revoke_by_id(&a.db.pool, &grant_id, &AuditActor::cli(), Utc::now())
        .await
        .expect("revoke the grant");

    eventually!(
        "B refuses the revoked grant",
        b.app.find_grant(&grant_id).await.unwrap().is_none()
    );
}

/// Restarts Redis and checks a handle built from the old URL reaches it
/// again, and (in a cluster) that the node announces the same port again.
async fn a_handle_survives_a_restart(redis: &TestRedis) {
    let handle = support::server::redis_connect(redis).await;
    let url_before = redis.url.clone();
    if redis.stop().await.is_none() {
        // A shared server: it cannot be stopped for one test.
        return;
    }
    redis.start_again().await.expect("a container to start");

    assert_eq!(redis.url, url_before, "the URL is the one the handle uses");
    let mut connection = handle.connection();
    eventually!("the old handle reaches the restarted server", {
        redis::cmd("PING")
            .query_async::<String>(&mut connection)
            .await
            .is_ok()
    });
    if redis.cluster {
        let port = redis
            .url
            .rsplit(':')
            .next()
            .expect("a port")
            .trim_end_matches("/0");
        let announced: Vec<String> = redis::cmd("CONFIG")
            .arg("GET")
            .arg("cluster-announce-port")
            .query_async(&mut connection)
            .await
            .expect("config get");
        assert_eq!(announced.get(1).map(String::as_str), Some(port));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_handle_built_before_a_restart_reaches_redis_afterwards() {
    let redis = TestRedis::start().await;
    a_handle_survives_a_restart(&redis).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_handle_built_before_a_restart_reaches_the_cluster_afterwards() {
    let redis = TestRedis::start_cluster().await;
    a_handle_survives_a_restart(&redis).await;
}

// ---- MCP sessions shared between replicas (Task 5) ----

/// Two replicas on one Redis with `config`, and a reader key (default catalog
/// `main`) that both know.
async fn session_pair(redis: &TestRedis, config: Config) -> (TestServer, Sibling, String) {
    let a = TestServer::start_shared(pg(), redis, |_| config.clone()).await;
    let b = a.sibling(redis, |_| config.clone()).await;
    let key = a.key("reader", Access::Read, &[], Some("main")).await;
    eventually!(
        "B knows the reader key",
        b.app.find_key(&key).await.unwrap().is_some()
    );
    (a, b, key)
}

/// Opens a session on `mcp_url` and sends its `initialized` notification.
async fn open_ready(mcp_url: &str, key: &str) -> String {
    let session = open_session(mcp_url, key).await;
    assert_eq!(notify_on(mcp_url, key, &session).await, 202);
    session
}

/// `tools/list` on `session`: the status.
async fn tools_list(mcp_url: &str, key: &str, session: &str) -> reqwest::StatusCode {
    post_on(mcp_url, key, session, TOOLS_LIST).await.status()
}

/// The binding key and the state key of `session` on `redis`'s prefix.
fn session_keys(handle: &okf_mcp::shared::Redis, session: &str) -> (String, String) {
    let id = okf_mcp::shared::hashed(session);
    (handle.key("mcp:bind", &id), handle.key("mcp:state", &id))
}

/// Runs a raw command (test set-up and inspection).
async fn raw<T: redis::FromRedisValue>(handle: &okf_mcp::shared::Redis, cmd: &mut redis::Cmd) -> T {
    cmd.query_async(&mut handle.connection())
        .await
        .expect("a raw redis command")
}

/// How many of `keys` exist.
async fn existing(handle: &okf_mcp::shared::Redis, keys: &[&str]) -> i64 {
    let mut command = redis::cmd("EXISTS");
    for key in keys {
        command.arg(*key);
    }
    raw(handle, &mut command).await
}

/// Opens on A, then lists and calls a tool on B, which restores from Redis.
async fn session_moves_between_replicas(redis: &TestRedis) {
    let (a, b, key) = session_pair(redis, Config::default()).await;

    let session = open_ready(&a.mcp_url(), &key).await;
    let listed = post_on(&b.mcp_url(), &key, &session, TOOLS_LIST).await;
    let listed_status = listed.status();
    let listed_body = listed.text().await.expect("the tools/list body");
    let called = post_on(
        &b.mcp_url(),
        &key,
        &session,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"get_concept","arguments":{"path":"a"}}}"#,
    )
    .await;
    let called_status = called.status();
    let called_body = called.text().await.expect("the tools/call body");

    assert_eq!(listed_status, 200);
    let first: serde_json::Value =
        serde_json::from_str(first_event_data(&listed_body)).expect("a JSON event");
    assert!(first["result"]["tools"].is_array(), "{listed_body}");
    assert_eq!(called_status, 200);
    let answer: serde_json::Value =
        serde_json::from_str(first_event_data(&called_body)).expect("a JSON event");
    assert_eq!(answer["id"], 3, "{called_body}");
    assert!(answer["result"].is_object(), "{called_body}");
    assert_ne!(answer["result"]["isError"], json!(true), "{called_body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_opened_on_one_replica_serves_tools_on_the_other() {
    let redis = TestRedis::start().await;
    session_moves_between_replicas(&redis).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_opened_on_one_replica_serves_tools_on_the_other_in_cluster_mode() {
    let redis = TestRedis::start_cluster().await;
    session_moves_between_replicas(&redis).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn another_key_on_that_session_is_refused_on_either_replica() {
    let redis = TestRedis::start().await;
    let (a, b, owner) = session_pair(&redis, Config::default()).await;
    let intruder = a.key("intruder", Access::Read, &[], Some("main")).await;
    eventually!(
        "B knows the intruder key",
        b.app.find_key(&intruder).await.unwrap().is_some()
    );
    let session = open_ready(&a.mcp_url(), &owner).await;

    let mut refusals = Vec::new();
    for url in [a.mcp_url(), b.mcp_url()] {
        refusals.push(tools_list(&url, &intruder, &session).await);
        refusals.push(stream_on(&url, &intruder, &session).await);
    }
    let own_on_b = tools_list(&b.mcp_url(), &owner, &session).await;

    assert_eq!(refusals, vec![reqwest::StatusCode::NOT_FOUND; 4]);
    assert_eq!(own_on_b, 200, "the owner is still admitted");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_binding_without_its_state_is_refused_on_every_replica() {
    let redis = TestRedis::start().await;
    let (a, b, key) = session_pair(&redis, Config::default()).await;
    let handle = support::server::redis_connect(&redis).await;
    let session = open_ready(&a.mcp_url(), &key).await;
    let (_, state) = session_keys(&handle, &session);

    // As an eviction would.
    let _: i64 = raw(&handle, redis::cmd("DEL").arg(&state)).await;
    // A still holds a local worker for the session: the script, not a
    // failed restore, must refuse it.
    let on_a = tools_list(&a.mcp_url(), &key, &session).await;
    let on_b = tools_list(&b.mcp_url(), &key, &session).await;

    assert_eq!(on_a, 404);
    assert_eq!(on_b, 404);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_state_without_a_binding_is_never_restored() {
    let redis = TestRedis::start().await;
    let (a, b, key) = session_pair(&redis, Config::default()).await;
    let handle = support::server::redis_connect(&redis).await;
    let session = open_ready(&a.mcp_url(), &key).await;
    let (bind, _) = session_keys(&handle, &session);

    let _: i64 = raw(&handle, redis::cmd("DEL").arg(&bind)).await;
    let on_b = tools_list(&b.mcp_url(), &key, &session).await;
    let on_a = tools_list(&a.mcp_url(), &key, &session).await;

    assert_eq!(on_b, 404);
    assert_eq!(on_a, 404, "the binding is the check, not the local worker");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_revoked_keys_session_is_refused_on_every_replica() {
    let redis = TestRedis::start().await;
    let (a, b, admin) = pair(&redis).await;
    let key = a.key("reader", Access::Read, &[], Some("main")).await;
    eventually!(
        "B knows the reader key",
        b.app.find_key(&key).await.unwrap().is_some()
    );
    let session = open_ready(&a.mcp_url(), &key).await;
    assert_eq!(tools_list(&b.mcp_url(), &key, &session).await, 200);

    let id = a
        .app
        .find_key(&key)
        .await
        .unwrap()
        .expect("cached")
        .key_id
        .clone();
    api(
        reqwest::Method::POST,
        &a.base,
        &format!("/api/v1/keys/{id}/revoke"),
        &admin,
        serde_json::Value::Null,
    )
    .await;
    eventually!(
        "B refuses the revoked key",
        b.app.find_key(&key).await.unwrap().is_none()
    );

    assert_eq!(tools_list(&a.mcp_url(), &key, &session).await, 401);
    assert_eq!(tools_list(&b.mcp_url(), &key, &session).await, 401);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_idle_session_expires_in_redis() {
    let redis = TestRedis::start().await;
    let config = Config {
        session_idle_timeout: Duration::from_secs(2),
        ..Config::default()
    };
    let (a, b, key) = session_pair(&redis, config).await;
    let handle = support::server::redis_connect(&redis).await;
    let session = open_ready(&a.mcp_url(), &key).await;
    let (bind, state) = session_keys(&handle, &session);

    let bind_ttl: i64 = raw(&handle, redis::cmd("PTTL").arg(&bind)).await;
    let state_ttl: i64 = raw(&handle, redis::cmd("PTTL").arg(&state)).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let on_b = tools_list(&b.mcp_url(), &key, &session).await;
    let left = existing(&handle, &[&bind, &state]).await;

    assert!((1..=2000).contains(&bind_ttl), "{bind_ttl}");
    assert!((1..=2000).contains(&state_ttl), "{state_ttl}");
    assert_eq!(on_b, 404);
    assert_eq!(left, 0, "both keys expired");
}

#[tokio::test(flavor = "multi_thread")]
async fn use_on_one_replica_keeps_the_session_alive_for_the_other() {
    let redis = TestRedis::start().await;
    let config = Config {
        session_idle_timeout: Duration::from_secs(3),
        ..Config::default()
    };
    let (a, b, key) = session_pair(&redis, config).await;
    let session = open_ready(&a.mcp_url(), &key).await;

    let mut on_b = Vec::new();
    for _ in 0..5 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        on_b.push(tools_list(&b.mcp_url(), &key, &session).await);
    }
    // Five seconds without use on A: its own worker idled out, so A
    // restores the session from Redis, whose TTL B kept sliding.
    let on_a = tools_list(&a.mcp_url(), &key, &session).await;

    assert_eq!(on_b, vec![reqwest::StatusCode::OK; 5]);
    assert_eq!(on_a, 200);
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_ends_the_session_everywhere() {
    let redis = TestRedis::start().await;
    let (a, b, owner) = session_pair(&redis, Config::default()).await;
    let intruder = a.key("intruder", Access::Read, &[], Some("main")).await;
    eventually!(
        "B knows the intruder key",
        b.app.find_key(&intruder).await.unwrap().is_some()
    );
    let handle = support::server::redis_connect(&redis).await;

    // Another key's DELETE on B is refused and releases nothing.
    let kept = open_ready(&a.mcp_url(), &owner).await;
    let (kept_bind, kept_state) = session_keys(&handle, &kept);
    let foreign_delete = delete_on(&b.mcp_url(), &intruder, &kept).await;
    let kept_left = existing(&handle, &[&kept_bind, &kept_state]).await;

    // B has never received a POST or GET for this session, so it holds no
    // local copy (review of PR #552).
    let session = open_ready(&a.mcp_url(), &owner).await;
    let (bind, state) = session_keys(&handle, &session);
    let deleted = delete_on(&b.mcp_url(), &owner, &session).await;
    let left = existing(&handle, &[&bind, &state]).await;
    let on_a = tools_list(&a.mcp_url(), &owner, &session).await;
    let on_b = tools_list(&b.mcp_url(), &owner, &session).await;

    assert_eq!(foreign_delete, 404);
    assert_eq!(kept_left, 2, "another key's DELETE releases nothing");
    assert_eq!(deleted, 202);
    assert_eq!(left, 0, "both keys are gone");
    assert_eq!(on_a, 404);
    assert_eq!(on_b, 404);
}

#[tokio::test(flavor = "multi_thread")]
async fn redis_down_answers_503_not_a_fallback() {
    let redis = TestRedis::start().await;
    let (a, _b, key) = session_pair(&redis, Config::default()).await;
    let session = open_ready(&a.mcp_url(), &key).await;
    if redis.stop().await.is_none() {
        // A shared server: it cannot be stopped for one test.
        return;
    }

    // A holds the session locally, but the binding check is authoritative.
    let status = tokio::time::timeout(
        Duration::from_secs(30),
        tools_list(&a.mcp_url(), &key, &session),
    )
    .await
    .expect("an answer within 30 s while Redis is down");

    assert_eq!(status, 503);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rejected_delete_leaves_the_session_in_place() {
    let redis = TestRedis::start().await;
    let (a, b, key) = session_pair(&redis, Config::default()).await;
    let handle = support::server::redis_connect(&redis).await;
    let session = open_ready(&a.mcp_url(), &key).await;
    let (bind, state) = session_keys(&handle, &session);

    // The owner's DELETE passes the binding check, then rmcp refuses its
    // `Host` (DNS-rebinding protection) before closing anything.
    let rejected = reqwest::Client::new()
        .delete(b.mcp_url())
        .header("host", "evil.example")
        .header("mcp-session-id", &session)
        .header("mcp-protocol-version", "2025-11-25")
        .bearer_auth(&key)
        .send()
        .await
        .expect("the server answers")
        .status();
    let left = existing(&handle, &[&bind, &state]).await;
    let on_a = tools_list(&a.mcp_url(), &key, &session).await;
    let on_b = tools_list(&b.mcp_url(), &key, &session).await;

    assert_eq!(rejected, 403);
    assert_eq!(left, 2, "a rejected DELETE releases nothing");
    assert_eq!(on_a, 200);
    assert_eq!(on_b, 200);
}

/// Sets the `default` user's permission `rule` (`-mget`, `+mget`) through an
/// admin user on its own connection, so the rule can be lifted again.
async fn default_user_may(redis: &TestRedis, rule: &str) {
    let client = redis::Client::open(redis.url.as_str()).expect("the test redis URL");
    let mut admin = client
        .get_multiplexed_async_connection()
        .await
        .expect("an admin connection");
    let _: () = redis::cmd("ACL")
        .arg("SETUSER")
        .arg("okf-test-admin")
        .arg("on")
        .arg("nopass")
        .arg("+@all")
        .arg("~*")
        .arg("&*")
        .query_async(&mut admin)
        .await
        .expect("create the admin user");
    let _: () = redis::cmd("AUTH")
        .arg("okf-test-admin")
        .arg("unused")
        .query_async(&mut admin)
        .await
        .expect("authenticate as the admin user");
    let _: () = redis::cmd("ACL")
        .arg("SETUSER")
        .arg("default")
        .arg(rule)
        .query_async(&mut admin)
        .await
        .expect("change the default user's permissions");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_redis_failure_after_admission_answers_503_not_404() {
    let shared_server = [
        support::redis::SERVER_URL_VAR,
        support::redis::CLUSTER_URL_VAR,
    ]
    .iter()
    .any(|var| std::env::var(var).is_ok_and(|url| !url.is_empty()));
    if shared_server {
        // Changing the default user would break every test sharing it.
        return;
    }
    let redis = TestRedis::start().await;
    let (a, b, key) = session_pair(&redis, Config::default()).await;
    let session = open_ready(&a.mcp_url(), &key).await;

    // ADMIT (a script of GET, EXISTS and PEXPIRE) still runs; the restore's
    // MGET on B, which holds no local copy, is refused with NOPERM.
    default_user_may(&redis, "-mget").await;
    let failed = tools_list(&b.mcp_url(), &key, &session).await;
    default_user_may(&redis, "+mget").await;
    let retried = tools_list(&b.mcp_url(), &key, &session).await;

    assert_eq!(failed, 503, "retryable, not rmcp's 404");
    assert_eq!(retried, 200, "the session was there all along");
}

/// One `pg`-like catalog that publishes cleanly: a root index, a folder
/// index and one concept.
fn indexed() -> Vec<(&'static str, Files)> {
    vec![(
        "main",
        files(&[
            (
                "index.md",
                "# Main\n\n# Sections\n\n* [Infra](/infra/index.md) - systems\n",
            ),
            (
                "infra/index.md",
                "# Infra\n\n# Concepts\n\n* [X](x.md) - x\n",
            ),
            (
                "infra/x.md",
                "---\ntype: Note\ntitle: X\ndescription: X.\n---\n# X\n",
            ),
        ]),
    )]
}

/// Opens a changeset on `client`, replaces `old` with `new` in
/// `/index.md`, and publishes.
async fn publish_index_edit(client: &support::server::Client, old: &str, new: &str) {
    let handle = support::server::open_changeset(client, Some("main")).await;
    let edited = call(
        client,
        "edit_document",
        json!({"path": "/index.md", "old_str": old, "new_str": new, "changeset": handle}),
    )
    .await;
    support::server::structured(&edited);
    let published = call(
        client,
        "publish",
        json!({"message": "test", "changeset": handle}),
    )
    .await;
    assert_eq!(
        support::server::structured(&published)["published"],
        json!(true),
        "{published:?}"
    );
}

/// Waits (bounded by [`WAIT`]) until `read_document` of `/index.md` on
/// `client` contains `text`. Propagation is asynchronous and best effort, so
/// this checks the change arrives promptly, not that the first read is fresh.
async fn until_index_reads(client: &support::server::Client, text: &str) -> bool {
    tokio::time::timeout(WAIT, async {
        loop {
            let read = call(
                client,
                "read_document",
                json!({"path": "/index.md", "catalog": "main"}),
            )
            .await;
            let body = support::server::structured(&read)["text"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            if body.contains(text) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .is_ok()
}

/// Issue #659's acceptance criterion on two real servers with
/// `catalog_ttl = 1h`: a publish on A propagates to B well within it (a
/// bounded wait, not a first-read guarantee). B's
/// subscriber answers events in order, so once B reads the first publish
/// (by its announcement or by the catch-up when B subscribed), the second
/// can reach B only by its announcement.
#[tokio::test(flavor = "multi_thread")]
async fn a_publish_on_one_replica_propagates_to_the_other_promptly() {
    let redis = TestRedis::start().await;
    let a = TestServer::start_shared(indexed(), &redis, |_| Config::default()).await;
    let b = a.sibling(&redis, |_| Config::default()).await;
    let writer = a.key("writer", Access::Write, &[], Some("main")).await;
    eventually!(
        "B knows the writer",
        b.app.find_key(&writer).await.unwrap().is_some()
    );
    let on_a = a.client(&writer, None).await.expect("a client on A");
    let on_b = tokio::time::timeout(WAIT, a.client_at(&writer, &b.mcp_url(), &[]))
        .await
        .expect("connecting to B in time")
        .expect("a client on B");

    publish_index_edit(&on_a, "- systems", "- the systems").await;
    let first = until_index_reads(&on_b, "the systems").await;
    publish_index_edit(&on_a, "- the systems", "- all systems").await;
    let second = until_index_reads(&on_b, "all systems").await;

    assert!(first, "the first publish reached B within the wait");
    assert!(
        second,
        "the second publish reached B through its announcement"
    );
}

/// Two serving replicas, both ticking and both running the housekeeping
/// worker (issue #693): each task's first slot is scheduled once in the
/// cluster and run once, by whichever worker took it, and the
/// `cluster_stats` row it writes is there for both replicas' gauges.
#[tokio::test(flavor = "multi_thread")]
async fn housekeeping_runs_each_slot_once_across_two_serving_replicas() {
    let redis = TestRedis::start().await;
    let a = TestServer::start_shared_serving(pg(), &redis, |_| Config::default(), true).await;
    let b = a.sibling_serving(&redis, |_| Config::default(), true).await;
    let pool = a.db.pool.clone();
    let tasks = okf_mcp::housekeeping::Task::ALL;

    // Both tickers pushed every task's current slot at start-up. The
    // workers poll every 10 seconds, so allow a few polls; wait too until
    // both workers have registered.
    let done = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let finished: i64 = sqlx::query_scalar(
                "SELECT count(DISTINCT split_part(idempotency_key, ':', 1)) FROM apalis.jobs \
                 WHERE job_type = $1 AND status = 'Done'",
            )
            .bind(okf_mcp::housekeeping::QUEUE)
            .fetch_one(&pool)
            .await
            .expect("count the finished jobs");
            // Each worker registers once it starts polling, which can be
            // after the other has already run every job.
            let workers: i64 =
                sqlx::query_scalar("SELECT count(*) FROM apalis.workers WHERE worker_type = $1")
                    .bind(okf_mcp::housekeeping::QUEUE)
                    .fetch_one(&pool)
                    .await
                    .expect("count the workers");
            if finished == tasks.len() as i64 && workers == 2 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await;
    let jobs: Vec<(String, String, i32)> = sqlx::query_as(
        "SELECT idempotency_key, status, attempts FROM apalis.jobs \
         WHERE job_type = $1 ORDER BY idempotency_key",
    )
    .bind(okf_mcp::housekeeping::QUEUE)
    .fetch_all(&pool)
    .await
    .expect("list the jobs");
    let workers: i64 =
        sqlx::query_scalar("SELECT count(*) FROM apalis.workers WHERE worker_type = $1")
            .bind(okf_mcp::housekeeping::QUEUE)
            .fetch_one(&pool)
            .await
            .expect("count the workers");
    let stats = okf_mcp::housekeeping::read_stats(&pool)
        .await
        .expect("read the cluster stats");
    drop(b);
    drop(a);

    assert!(done.is_ok(), "every task ran within the wait: {jobs:?}");
    assert_eq!(workers, 2, "each replica runs a housekeeping worker");
    for task in tasks {
        let slots: Vec<&(String, String, i32)> = jobs
            .iter()
            .filter(|(key, ..)| key.starts_with(&format!("{}:", task.as_str())))
            .collect();
        // Within a minute or two only the every-minute tasks can reach a
        // second slot: at most three slots for each cluster or store job
        // (the key without its slot, `<task>` or `<task>:<store>`).
        assert!(!slots.is_empty(), "{task:?}: {jobs:?}");
        let mut series: Vec<&str> = slots
            .iter()
            .filter_map(|(key, ..)| key.rsplit_once(':').map(|(series, _)| series))
            .collect();
        series.sort_unstable();
        for name in &series {
            let count = series.iter().filter(|other| *other == name).count();
            assert!(count <= 3, "{task:?}: {jobs:?}");
        }
        // Tasks on catalog data have a job on the store `main` (plan Task 16).
        assert_eq!(
            series.contains(&format!("{}:main", task.as_str()).as_str()),
            task.runs_per_store(),
            "{task:?}: {jobs:?}"
        );
        assert_eq!(
            series.contains(&task.as_str()),
            task.runs_on_cluster(),
            "{task:?}: {jobs:?}"
        );
        assert!(
            slots
                .iter()
                .all(|(_, status, attempts)| status != "Done" || *attempts == 1),
            "a finished slot ran once: {jobs:?}"
        );
    }
    assert!(stats.is_some(), "the cluster_stats job wrote its row");
}

// ---- The change feed (milestone 18 Task 2) ----

/// A catalog imported through the store on the pool, as `okf-mcp import`
/// does, announces nothing over Redis: both replicas serve the new
/// generation because the import's transaction sent a `catalog` `NOTIFY`.
#[tokio::test(flavor = "multi_thread")]
async fn a_catalog_imported_through_the_store_is_served_on_both_replicas() {
    let redis = TestRedis::start().await;
    let (a, b, _admin) = pair(&redis).await;
    let before = a
        .app
        .catalog("main")
        .await
        .unwrap()
        .expect("main")
        .current()
        .generation();
    let audit = AuditActor::cli();
    let options = okf_mcp::store::import::ImportOptions {
        replace: true,
        scope: okf_mcp::store::import::ImportScope::Everything,
        expect_generation: None,
        expect_catalog_id: None,
        actor: "test/import",
        audit: &audit,
        now: Utc::now(),
    };
    let report = okf_mcp::store::import::import(
        &support::stores::main_store(&a.db.pool),
        "main",
        files(&[("b.md", "---\ntype: Note\n---\n# B\n")]),
        &options,
    )
    .await
    .expect("import");
    let generation = report.changed.expect("the import wrote").generation;
    assert!(generation > before);

    // Each replica's own lookup: the row its directory holds, and the
    // snapshot its cache serves for that row.
    async fn at(app: &okf_mcp::app::App, generation: i64) -> bool {
        app.catalog("main")
            .await
            .unwrap()
            .is_some_and(|state| state.current().generation() == generation)
    }
    eventually!(
        "A serves the imported generation",
        at(&a.app, generation).await
    );
    eventually!(
        "B serves the imported generation",
        at(&b.app, generation).await
    );
}

/// A catalog created through the store on one replica's pool, as
/// `okf-mcp catalog create` does, is readable on the other with no poll and
/// no Redis announcement: the create's `catalog_meta` `NOTIFY` makes B's
/// directory forget that the name was absent, and the catalog loads on its
/// first read there.
#[tokio::test(flavor = "multi_thread")]
async fn a_catalog_created_on_one_replica_is_readable_on_the_other() {
    let redis = TestRedis::start().await;
    let (a, b, admin) = pair(&redis).await;
    // B looks for it first, so its directory remembers that it is absent.
    assert!(b.app.catalog("notes").await.unwrap().is_none());

    let new = okf_mcp::store::catalogs::NewCatalog {
        name: "notes",
        display_name: &okf_mcp::labels::DisplayName::default(),
        description: "",
        owner_team: OWNER,
    };
    support::stores::create_catalog(&a.db.pool, &new, &AuditActor::cli(), Utc::now())
        .await
        .expect("create notes");

    async fn readable(app: &okf_mcp::app::App) -> bool {
        app.catalog("notes").await.unwrap().is_some()
    }
    eventually!("B serves the new catalog", readable(&b.app).await);
    // And over MCP, through B's tool path.
    let client = tokio::time::timeout(WAIT, async {
        let url = b.mcp_url();
        a.client_at(&admin, &url, &[]).await
    })
    .await
    .expect("connecting to B in time")
    .expect("a client on B");
    let read = call(
        &client,
        "read_document",
        json!({"catalog": "notes", "path": "/index.md"}),
    )
    .await;
    assert_ne!(
        read.is_error,
        Some(true),
        "B reads the new catalog: {read:?}"
    );
}

/// Revokes `key` through the store on the pool, as `okf-mcp key revoke`
/// does: no `settle`, no poll, no Redis announcement.
async fn revoke_through_the_store(a: &TestServer, key: &str) {
    let id = a
        .app
        .find_key(key)
        .await
        .unwrap()
        .expect("cached")
        .key_id
        .clone();
    keys::revoke_by_id(&a.db.pool, &id, &AuditActor::cli(), Utc::now())
        .await
        .expect("revoke");
}

/// The case milestone 16 could not cover without the poll: a revocation
/// made outside any server reaches both replicas through `NOTIFY` alone.
#[tokio::test(flavor = "multi_thread")]
async fn a_key_revoked_through_the_store_is_refused_on_the_other_replica() {
    let redis = TestRedis::start().await;
    let (a, b, _admin) = pair(&redis).await;
    let key = a.key("reader", Access::Read, &[], None).await;
    eventually!(
        "B knows the key",
        b.app.find_key(&key).await.unwrap().is_some()
    );

    revoke_through_the_store(&a, &key).await;

    eventually!(
        "B refuses the key",
        b.app.find_key(&key).await.unwrap().is_none()
    );
    eventually!(
        "A refuses the key",
        a.app.find_key(&key).await.unwrap().is_none()
    );
}

/// Ends every change-feed `LISTEN` backend of the test's database (both
/// replicas'), as a dropped connection would.
///
/// # Returns
/// How many it ended.
async fn kill_listener_connections(pool: &sqlx::PgPool) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM (SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
         WHERE query LIKE 'LISTEN%' AND query LIKE '%okf_invalidate%' \
               AND datname = current_database() AND pid <> pg_backend_pid()) t",
    )
    .fetch_one(pool)
    .await
    .expect("end the listener backends")
}

/// A listener whose connection was killed reconnects and catches up, so a
/// revocation made through the store right after is still refused on B,
/// whether B hears its `NOTIFY` or reads it in the catch-up.
#[tokio::test(flavor = "multi_thread")]
async fn a_killed_listener_connection_reloads_on_reconnect() {
    let redis = TestRedis::start().await;
    let (a, b, _admin) = pair(&redis).await;
    let key = a.key("reader", Access::Read, &[], None).await;
    eventually!(
        "B knows the key",
        b.app.find_key(&key).await.unwrap().is_some()
    );

    let ended = kill_listener_connections(&a.db.pool).await;
    assert!(ended >= 1, "at least B's listener backend was ended");
    revoke_through_the_store(&a, &key).await;

    eventually!(
        "B refuses the key after reconnecting",
        b.app.find_key(&key).await.unwrap().is_none()
    );
}
