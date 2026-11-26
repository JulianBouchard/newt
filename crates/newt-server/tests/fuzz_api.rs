#![expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]

mod support;

use std::fmt::Write as _;

use proptest::prelude::*;
use reqwest::Method;
use serde_json::json;
use sqlx::PgPool;
use support::*;

#[sqlx::test(migrations = "../../migrations")]
async fn arbitrary_event_bodies_never_fail_the_server_and_every_refusal_has_the_documented_shape(
    pool: PgPool,
) {
    let base = start(pool).await;
    let client = reqwest::Client::new();
    for (body, content_type) in samples((event_body(), content_type())) {
        let url = format!("{base}/v1/agents/assistant/sessions/s/events");
        let (status, answer) = send(
            &client,
            Method::POST,
            &url,
            Some("secret"),
            content_type,
            body.clone(),
        )
        .await;
        assert_contract(
            &format!(
                "events {:?}",
                String::from_utf8_lossy(&body[..body.len().min(200)])
            ),
            status,
            &answer,
        );
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn arbitrary_paths_never_fail_the_server_on_any_route(pool: PgPool) {
    let base = start(pool).await;
    let client = reqwest::Client::new();
    let routes = ["events", "cancel", "chat", "live", "history", "fork", ""];
    let methods = [Method::GET, Method::POST, Method::DELETE, Method::PUT];
    let cases = samples((
        segment(),
        segment(),
        0..routes.len(),
        0..methods.len(),
        event_body(),
    ));
    for (agent, key, route, method, body) in cases {
        let path = if route == 6 {
            String::new()
        } else {
            format!("/{}", routes[route])
        };
        let url = format!("{base}/v1/agents/{agent}/sessions/{key}{path}");
        let (status, answer) = send(
            &client,
            methods[method].clone(),
            &url,
            Some("secret"),
            Some("application/json"),
            body,
        )
        .await;
        assert_contract(&format!("{} {url}", methods[method]), status, &answer);
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn arbitrary_routes_answer_in_the_documented_shape(pool: PgPool) {
    let base = start(pool).await;
    let client = reqwest::Client::new();
    for (path, method) in samples((proptest::collection::vec(segment(), 0..6), 0..4usize)) {
        let methods = [Method::GET, Method::POST, Method::DELETE, Method::PATCH];
        let url = format!("{base}/{}", path.join("/"));
        let (status, answer) = send(
            &client,
            methods[method].clone(),
            &url,
            None,
            None,
            Vec::new(),
        )
        .await;
        assert_contract(&url, status, &answer);
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_token_opens_exactly_its_own_agent(pool: PgPool) {
    let base = start(pool).await;
    let client = reqwest::Client::new();
    let headers = proptest::option::of(prop_oneof![
        Just("Bearer secret".to_owned()),
        Just("Bearer other-secret".to_owned()),
        Just("bearer secret".to_owned()),
        Just("Bearer  secret".to_owned()),
        Just("Bearer ".to_owned()),
        Just("Basic c2VjcmV0".to_owned()),
        "[ -~]{0,30}",
    ]);
    for (agent, header) in samples((
        proptest::sample::select(vec!["assistant", "other", "nobody"]),
        headers,
    )) {
        let url = format!("{base}/v1/agents/{agent}/sessions/auth/events");
        let mut request = client
            .post(&url)
            .body(json!({ "id": "a", "text": "b" }).to_string());
        if let Some(header) = &header {
            request = request.header("authorization", header);
        }
        let status = request.send().await.unwrap().status();
        let valid = matches!(
            (agent, header.as_deref()),
            ("assistant", Some("Bearer secret")) | ("other", Some("Bearer other-secret"))
        );
        assert_eq!(status == 202, valid, "{agent} {header:?} -> {status}");
        assert!(valid || status == 401, "{agent} {header:?} -> {status}");
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn arbitrary_cancel_bodies_never_fail_the_server(pool: PgPool) {
    let base = start(pool).await;
    let client = reqwest::Client::new();
    let url = format!("{base}/v1/agents/assistant/sessions/s/cancel");
    let body = prop_oneof![
        json_value().prop_map(|value| value.to_string().into_bytes()),
        weird_text().prop_map(|turn| json!({ "turn": turn }).to_string().into_bytes()),
        proptest::collection::vec(any::<u8>(), 0..60),
        Just(Vec::new()),
    ];
    for (body, content_type) in samples((body, content_type())) {
        let (status, answer) = send(
            &client,
            Method::POST,
            &url,
            Some("secret"),
            content_type,
            body.clone(),
        )
        .await;
        assert_contract(
            &format!("cancel {:?}", String::from_utf8_lossy(&body)),
            status,
            &answer,
        );
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn arbitrary_live_cursors_never_fail_the_server(pool: PgPool) {
    let base = start(pool).await;
    let client = reqwest::Client::new();
    let seed = format!("{base}/v1/agents/assistant/sessions/live/events");
    let (status, _) = send(
        &client,
        Method::POST,
        &seed,
        Some("secret"),
        None,
        json!({ "id": "a", "text": "b" }).to_string().into_bytes(),
    )
    .await;
    assert_eq!(status, 202);
    let value = prop_oneof![
        any::<i64>().prop_map(|number| number.to_string()),
        Just(i64::MAX.to_string()),
        Just(i64::MIN.to_string()),
        Just("99999999999999999999999".to_owned()),
        Just("-1".to_owned()),
        Just(String::new()),
        Just("1.5".to_owned()),
        Just("%00".to_owned()),
        "[ -~]{0,12}".prop_map(|text| percent_encode(text.as_bytes())),
    ];
    for (query, header) in samples((
        proptest::option::of(value.clone()),
        proptest::option::of(value),
    )) {
        let mut url = format!("{base}/v1/agents/assistant/sessions/live/live");
        if let Some(query) = &query {
            let _ = write!(url, "?cursor={query}");
        }
        let mut request = client.get(&url).bearer_auth("secret");
        if let Some(value) = header
            .as_deref()
            .and_then(|header| reqwest::header::HeaderValue::from_str(header).ok())
        {
            request = request.header("last-event-id", value);
        }
        let response = request.send().await.unwrap();
        let status = response.status();
        if status == 200 {
            continue;
        }
        let body = response.bytes().await.unwrap();
        assert_contract(&format!("live {url} {header:?}"), status, &body);
    }
}

async fn send_oversized(client: &reqwest::Client, url: &str) -> (reqwest::StatusCode, Vec<u8>) {
    let oversized = || vec![b' '; 3 * 1024 * 1024];
    for _ in 0..19 {
        let sent = client
            .post(url)
            .bearer_auth("secret")
            .body(oversized())
            .send()
            .await;
        if let Ok(response) = sent {
            let status = response.status();
            return (status, response.bytes().await.unwrap().to_vec());
        }
    }
    send(client, Method::POST, url, Some("secret"), None, oversized()).await
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_body_over_the_limit_is_a_413_in_the_documented_shape(pool: PgPool) {
    let base = start(pool).await;
    let client = reqwest::Client::new();
    for route in ["events", "chat", "cancel", "fork"] {
        let url = format!("{base}/v1/agents/assistant/sessions/s/{route}");
        let (status, answer) = send_oversized(&client, &url).await;
        assert_eq!(status, 413, "{route}");
        assert_contract(route, status, &answer);
    }
}
