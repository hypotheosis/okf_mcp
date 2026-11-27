//! End-to-end tests of the HTTP server (design "Testing"): keys, hosts,
//! `/healthz`, catalog selection, the session binding and the archive
//! routes, over real HTTP with rmcp's client and `reqwest`.

mod support;

use okf_mcp::config::Config;
use okf_mcp::store::keys::{self, Access};
use support::mcp_raw::{first_event_data, notify_on, open_session, stream_on, INITIALIZE};
use support::server::{call, files, structured, Client, TestServer};

/// POSTs an `initialize` to `/mcp`, with a bearer key and a `Host`, if given.
async fn post_initialize(
    server: &TestServer,
    key: Option<&str>,
    host: Option<&str>,
) -> reqwest::StatusCode {
    let mut request = reqwest::Client::new()
        .post(server.mcp_url())
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(INITIALIZE);
    if let Some(key) = key {
        request = request.bearer_auth(key);
    }
    if let Some(host) = host {
        request = request.header("host", host);
    }
    request.send().await.unwrap().status()
}

/// The connection's selection, as `list_catalogs` reports it.
async fn selection(client: Client) -> serde_json::Value {
    let result = call(&client, "list_catalogs", serde_json::json!({})).await;
    structured(&result)["selection"].clone()
}

/// One catalog, `pg`, with one note.
fn pg() -> Vec<(&'static str, okf_mcp::archive::Files)> {
    vec![("pg", files(&[("a.md", "---\ntype: Note\n---\n# A\n")]))]
}

#[tokio::test(flavor = "multi_thread")]
async fn healthz_answers_without_a_key() {
    let server = TestServer::start(pg(), Config::default()).await;

    let response = reqwest::get(format!("{}/healthz", server.base))
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(
        body,
        serde_json::json!({"database": "ok", "cached_catalogs": 0, "clock": null, "redis": null, "rate_limits": null})
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_refuses_a_missing_or_unknown_key() {
    let server = TestServer::start(pg(), Config::default()).await;
    let key = server.key("reader", Access::Read, &[], None).await;

    assert_eq!(post_initialize(&server, None, None).await, 401);
    assert_eq!(post_initialize(&server, Some("not-a-key"), None).await, 401);
    assert_eq!(post_initialize(&server, Some(&key), None).await, 200);
    assert!(server.client("not-a-key", None).await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_legacy_initialize_reply_has_no_empty_priming_event() {
    let server = TestServer::start(pg(), Config::default()).await;
    let key = server.key("reader", Access::Read, &[], None).await;

    let response = reqwest::Client::new()
        .post(server.mcp_url())
        .bearer_auth(&key)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(INITIALIZE)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    let body = response.text().await.unwrap();
    let first: serde_json::Value = serde_json::from_str(first_event_data(&body))
        .unwrap_or_else(|error| panic!("first event is not JSON ({error}): {body:?}"));
    assert_eq!(first["id"], 1);
    assert_eq!(first["result"]["protocolVersion"], "2025-11-25");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_outside_allowed_hosts_is_refused() {
    let server = TestServer::start(pg(), Config::default()).await;
    let key = server.key("reader", Access::Read, &[], None).await;

    assert_eq!(
        post_initialize(&server, Some(&key), Some("evil.example")).await,
        403
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_revoked_key_stops_working_after_a_poll() {
    let server = TestServer::start(pg(), Config::default()).await;
    let key = server.key("reader", Access::Read, &[], Some("pg")).await;
    let client = server.client(&key, None).await.unwrap();
    structured(&call(&client, "list_catalogs", serde_json::json!({})).await);

    crate::support::server::applied(&server.app, async {
        keys::revoke(
            &server.db.pool,
            "reader",
            &okf_mcp::store::audit::AuditActor::cli(),
            chrono::Utc::now(),
        )
        .await
        .unwrap();
    })
    .await;

    let arguments = serde_json::json!({}).as_object().unwrap().clone();
    // Bounded: if the client retried a 401 instead of failing, the test
    // must fail rather than hang.
    let call = client.call_tool(
        rmcp::model::CallToolRequestParams::new("list_catalogs").with_arguments(arguments),
    );
    let after = tokio::time::timeout(std::time::Duration::from_secs(10), call)
        .await
        .expect("the call answers within 10 s");
    assert!(after.is_err(), "a revoked key still worked: {after:?}");
    // The server's side, independent of how the client reports it.
    assert_eq!(post_initialize(&server, Some(&key), None).await, 401);
}

#[tokio::test(flavor = "multi_thread")]
async fn selection_takes_the_header_then_the_key_default_then_the_server_default() {
    let catalogs = vec![
        ("pg", files(&[])),
        ("work", files(&[])),
        ("home", files(&[])),
    ];
    let config = Config {
        default_catalog: Some("home".to_string()),
        ..Config::default()
    };
    let server = TestServer::start(catalogs, config).await;
    let with_default = server.key("a", Access::Read, &[], Some("work")).await;
    let without_default = server.key("b", Access::Read, &[], None).await;
    let only_pg = server.key("c", Access::Read, &["pg"], Some("pg")).await;

    let header = selection(server.client(&with_default, Some("pg,work")).await.unwrap()).await;
    let key_default = selection(server.client(&with_default, None).await.unwrap()).await;
    let server_default = selection(server.client(&without_default, None).await.unwrap()).await;
    let client = server.client(&only_pg, None).await.unwrap();
    let listed = call(&client, "list_catalogs", serde_json::json!({})).await;

    assert_eq!(header, serde_json::json!(["pg", "work"]));
    assert_eq!(key_default, serde_json::json!(["work"]));
    assert_eq!(server_default, serde_json::json!(["home"]));
    assert_eq!(structured(&listed)["catalogs"].as_array().unwrap().len(), 1);
}

/// The names in `list_catalogs`' `catalogs`, and its `selection` and
/// `write_target`.
async fn offered(client: &Client) -> (Vec<String>, serde_json::Value, serde_json::Value) {
    let result = call(client, "list_catalogs", serde_json::json!({})).await;
    let listed = structured(&result);
    let names = listed["catalogs"]
        .as_array()
        .expect("catalogs")
        .iter()
        .map(|catalog| catalog["name"].as_str().expect("name").to_string())
        .collect();
    (
        names,
        listed["selection"].clone(),
        listed["write_target"].clone(),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn a_connection_list_is_the_session_scope_whatever_its_spelling() {
    let catalogs = vec![
        ("pg", files(&[("a.md", "---\ntype: Note\n---\n# A\n")])),
        ("work", files(&[])),
        ("home", files(&[])),
    ];
    let server = TestServer::start(catalogs, Config::default()).await;
    // Reaches `pg` and `work`, defaulting to `pg`.
    let key = server
        .key("a", Access::Read, &["pg", "work"], Some("pg"))
        .await;
    let query_url = format!("{}?catalogs=work", server.mcp_url());
    let clients = [
        server.client(&key, Some("work")).await.unwrap(),
        server
            .client_with(&key, &[("x-okf-catalog", "work")])
            .await
            .unwrap(),
        server.client_at(&key, &query_url, &[]).await.unwrap(),
    ];

    for client in &clients {
        let escaped = call(
            client,
            "get_concept",
            serde_json::json!({"catalog": "pg", "path": "/a.md"}),
        )
        .await;

        assert_eq!(
            offered(client).await,
            (
                vec!["work".to_string()],
                serde_json::json!(["work"]),
                serde_json::json!("work")
            )
        );
        assert_eq!(escaped.is_error, Some(true), "{escaped:?}");
        let body = escaped.structured_content.expect("the error's JSON");
        assert_eq!(body["error"], "CatalogNotSelected");
        assert_eq!(body["catalog"], "pg");
    }

    // Both spellings: one list, the plural's names first.
    let both = server
        .client_with(&key, &[("x-okf-catalogs", "work"), ("x-okf-catalog", "pg")])
        .await
        .unwrap();
    // Without a list, the key's grants are offered and its default selected.
    let unlisted = server.client(&key, None).await.unwrap();
    // A name the key does not reach is a diagnostic, never a fallback.
    let ungranted = server.client(&key, Some("work,home")).await.unwrap();
    let result = call(&ungranted, "list_catalogs", serde_json::json!({})).await;
    let ungranted_listed = structured(&result);

    assert_eq!(
        offered(&both).await,
        (
            vec!["pg".to_string(), "work".to_string()],
            serde_json::json!(["work", "pg"]),
            serde_json::json!("work")
        )
    );
    assert_eq!(
        offered(&unlisted).await,
        (
            vec!["pg".to_string(), "work".to_string()],
            serde_json::json!(["pg"]),
            serde_json::json!("pg")
        )
    );
    assert_eq!(ungranted_listed["catalogs"].as_array().unwrap().len(), 1);
    assert_eq!(ungranted_listed["selection"], serde_json::json!([]));
    assert!(ungranted_listed["selection_error"]
        .as_str()
        .unwrap()
        .contains("\"home\""));
}

/// `GET /catalogs/{catalog}/export` with a bearer key.
async fn get_export(server: &TestServer, catalog: &str, key: &str) -> reqwest::Response {
    reqwest::Client::new()
        .get(format!("{}/catalogs/{catalog}/export", server.base))
        .bearer_auth(key)
        .send()
        .await
        .unwrap()
}

/// `POST /catalogs/{catalog}/import<query>` with a bearer key and `body`.
async fn post_import(
    server: &TestServer,
    catalog: &str,
    key: &str,
    query: &str,
    body: Vec<u8>,
) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{}/catalogs/{catalog}/import{query}", server.base))
        .bearer_auth(key)
        .header("content-type", "application/gzip")
        .body(body)
        .send()
        .await
        .unwrap()
}

/// `pg` with two documents, and an empty `work`.
fn two_catalogs() -> Vec<(&'static str, okf_mcp::archive::Files)> {
    vec![
        (
            "pg",
            files(&[
                ("a.md", "---\ntype: Note\ntitle: A\n---\n# A\n"),
                ("index.md", "# Index\n"),
            ]),
        ),
        ("work", files(&[])),
    ]
}

#[tokio::test(flavor = "multi_thread")]
async fn export_gives_a_reachable_catalog_as_a_tarball() {
    let server = TestServer::start(two_catalogs(), Config::default()).await;
    let reader = server.key("reader", Access::Read, &["pg"], None).await;

    let response = get_export(&server, "pg", &reader).await;
    let elsewhere = get_export(&server, "work", &reader).await.status();
    let unknown = get_export(&server, "nope", &reader).await.status();

    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "application/gzip");
    assert_eq!(
        response.headers()["content-disposition"],
        "attachment; filename=\"untitled-catalog-pg.tar.gz\""
    );
    let bytes = response.bytes().await.unwrap();
    let exported = okf_mcp::archive::read_tarball(&bytes[..], 1 << 20).unwrap();
    assert_eq!(exported.keys().collect::<Vec<_>>(), ["a.md", "index.md"]);
    assert_eq!(elsewhere, 404);
    assert_eq!(unknown, 404);
}

#[tokio::test(flavor = "multi_thread")]
async fn import_needs_write_refuses_a_full_catalog_without_replace_and_is_read_at_once() {
    let server = TestServer::start(two_catalogs(), Config::default()).await;
    let reader = server.key("reader", Access::Read, &[], None).await;
    // Import needs `write` on the catalog (issue #260), within the grants.
    let writer = server.key("writer", Access::Write, &["work"], None).await;
    let stranger = server.key("stranger", Access::Write, &["pg"], None).await;
    let admin = server.key("admin", Access::Admin, &[], Some("work")).await;
    let tarball = get_export(&server, "pg", &admin)
        .await
        .bytes()
        .await
        .unwrap()
        .to_vec();

    let as_reader = post_import(&server, "work", &reader, "", tarball.clone())
        .await
        .status();
    let elsewhere = post_import(&server, "work", &stranger, "", tarball.clone())
        .await
        .status();
    let unknown = post_import(&server, "nope", &admin, "", tarball.clone())
        .await
        .status();
    let first = post_import(&server, "work", &writer, "", tarball.clone()).await;
    let first_status = first.status();
    let report: serde_json::Value = first.json().await.unwrap();
    let again = post_import(&server, "work", &writer, "", tarball.clone()).await;
    let again_status = again.status();
    let conflict: serde_json::Value = again.json().await.unwrap();
    let replaced: serde_json::Value =
        post_import(&server, "work", &admin, "?replace=true", tarball)
            .await
            .json()
            .await
            .unwrap();
    let garbage = post_import(&server, "work", &admin, "", b"not a tarball".to_vec()).await;
    let garbage_status = garbage.status();
    let refusal: serde_json::Value = garbage.json().await.unwrap();
    // No poll in between: the route swapped the snapshot in itself.
    let client = server.client(&admin, None).await.unwrap();
    let concept = call(&client, "get_concept", serde_json::json!({"path": "a"})).await;

    assert_eq!(as_reader, 403);
    assert_eq!(elsewhere, 404, "a writer outside its grants");
    assert_eq!(unknown, 404);
    assert_eq!(first_status, 200, "a writer imports");
    assert_eq!(report["added"], serde_json::json!(["a.md", "index.md"]));
    assert_eq!(again_status, 409);
    assert!(conflict["message"]
        .as_str()
        .unwrap()
        .contains("?replace=true"));
    assert_eq!(replaced["unchanged"], 2);
    assert_eq!(garbage_status, 400);
    assert_eq!(refusal["error"], "ImportRefused");
    assert_eq!(structured(&concept)["catalog"], "work");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_import_larger_than_max_import_bytes_is_refused() {
    let config = Config {
        max_import_bytes: bytesize::ByteSize::b(1024),
        ..Config::default()
    };
    let server = TestServer::start(two_catalogs(), config).await;
    let admin = server.key("admin", Access::Admin, &[], None).await;

    // Over the limit plus its 64 KiB of gzip slack.
    let status = post_import(&server, "work", &admin, "", vec![0u8; 128 * 1024])
        .await
        .status();

    assert_eq!(status, 413);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_legacy_session_request_reply_has_no_empty_priming_event() {
    let server = TestServer::start(pg(), Config::default()).await;
    let key = server.key("reader", Access::Read, &[], None).await;
    let session = open_session(&server.mcp_url(), &key).await;
    assert_eq!(notify_on(&server.mcp_url(), &key, &session).await, 202);

    let response = reqwest::Client::new()
        .post(server.mcp_url())
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("mcp-session-id", &session)
        .header("mcp-protocol-version", "2025-11-25")
        .bearer_auth(&key)
        .body(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    let body = response.text().await.unwrap();
    let first: serde_json::Value = serde_json::from_str(first_event_data(&body))
        .unwrap_or_else(|error| panic!("first event is not JSON ({error}): {body:?}"));
    assert_eq!(first["id"], 2);
    assert!(first["result"]["tools"].is_array());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_answers_only_the_key_that_opened_it() {
    let server = TestServer::start(pg(), Config::default()).await;
    let owner = server.key("owner", Access::Read, &[], Some("pg")).await;
    let intruder = server.key("intruder", Access::Read, &[], Some("pg")).await;
    let session = open_session(&server.mcp_url(), &owner).await;

    let foreign_post = notify_on(&server.mcp_url(), &intruder, &session).await;
    let foreign_stream = stream_on(&server.mcp_url(), &intruder, &session).await;
    let unknown = notify_on(&server.mcp_url(), &owner, "not-a-session").await;
    let own = notify_on(&server.mcp_url(), &owner, &session).await;
    // The owner's stream is admitted, so the refusal above is the binding's,
    // not a refusal of every `GET` that names a session.
    let own_stream = stream_on(&server.mcp_url(), &owner, &session).await;

    assert_eq!(foreign_post, 404);
    assert_eq!(foreign_stream, 404);
    assert_eq!(unknown, 404);
    assert_eq!(own, 202);
    assert_eq!(own_stream, 200);
}

/// Issue #666: `whoami` over a real session names the key's user with their
/// label, the actor, the reach and the server's clock, and no key id or name.
#[tokio::test(flavor = "multi_thread")]
async fn whoami_names_the_user_the_reach_and_the_clock() {
    let server = TestServer::start(two_catalogs(), Config::default()).await;
    server.set_user_label("tester", "Test Person").await;
    let key = server
        .key("whoami-key", Access::Write, &["pg"], Some("pg"))
        .await;
    let client = server.client(&key, None).await.unwrap();

    let before = chrono::Utc::now();
    let result = call(&client, "whoami", serde_json::json!({})).await;
    let me = structured(&result);

    assert_eq!(me["user"], "tester");
    assert_eq!(me["user_label"], "Test Person");
    assert_eq!(me["actor"], "whoami-key");
    assert_eq!(me["credential"], "api_key");
    assert_eq!(me["client_name"], serde_json::Value::Null);
    assert_eq!(me["access"], "write");
    assert_eq!(me["all_catalogs"], false);
    assert_eq!(
        me["catalogs"],
        serde_json::json!([{"name": "pg", "level": "write"}])
    );
    assert_eq!(me["default_catalog"], "pg");
    let now = chrono::DateTime::parse_from_rfc3339(me["now"].as_str().unwrap()).unwrap();
    assert!(
        (now.with_timezone(&chrono::Utc) - before)
            .num_seconds()
            .abs()
            < 60
    );
    let text = me.to_string();
    assert!(!text.contains(&key), "the secret must not appear");
}

/// Issue #1174: `guide` over a real session returns the instructions the server sent at
/// `initialize`, byte for byte, for a client that drops them.
#[tokio::test(flavor = "multi_thread")]
async fn guide_returns_the_instructions_sent_at_initialize() {
    let server = TestServer::start(two_catalogs(), Config::default()).await;
    let key = server
        .key("guide-key", Access::Read, &["pg"], Some("pg"))
        .await;
    let client = server.client(&key, None).await.unwrap();

    let sent = client
        .peer_info()
        .and_then(|info| info.instructions.clone())
        .expect("the server sends instructions");
    let result = call(&client, "guide", serde_json::json!({})).await;

    assert_eq!(structured(&result)["instructions"], sent);
}
