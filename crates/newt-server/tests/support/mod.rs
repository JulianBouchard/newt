#![expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]
#![allow(dead_code, reason = "each fuzz test file uses a different part")]

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::Arc;

use newt_server::http::{AppState, router};
use newt_server::live::Topics;
use newt_server::store::Store;
use proptest::prelude::*;
use proptest::strategy::ValueTree;
use proptest::test_runner::TestRunner;
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

pub const CASES: usize = 64;

pub const ERROR_CODES: &[(u16, &[&str])] = &[
    (400, &["invalid_request"]),
    (401, &["unauthorized"]),
    (404, &["not_found"]),
    (405, &["method_not_allowed"]),
    (
        409,
        &[
            "event_id_conflict",
            "turn_not_running",
            "session_running",
            "session_exists",
            "live_frames_gone",
        ],
    ),
    (413, &["payload_too_large"]),
    (415, &["unsupported_media_type"]),
    (429, &["inbox_full"]),
];

pub async fn start(pool: PgPool) -> String {
    let streaming = Arc::new(HashSet::from(["assistant".to_owned()]));
    let state = AppState {
        store: Store::new(pool),
        tokens: Arc::new(HashMap::from([
            ("assistant".to_owned(), "secret".to_owned()),
            ("other".to_owned(), "other-secret".to_owned()),
        ])),
        live: Topics::default(),
        shutdown: CancellationToken::new(),
        instrumentation: None,
        live_meta: Arc::default(),
        live_stream: streaming,
        metrics: Arc::default(),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });
    format!("http://{address}")
}

pub fn samples<S: Strategy>(strategy: S) -> Vec<S::Value> {
    let mut runner = TestRunner::deterministic();
    (0..CASES)
        .map(|_| strategy.new_tree(&mut runner).unwrap().current())
        .collect()
}

pub fn percent_encode(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut out, byte| {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(byte) {
            out.push(char::from(*byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
        out
    })
}

pub fn segment() -> impl Strategy<Value = String> {
    prop_oneof![
        3 => proptest::collection::vec(any::<u8>(), 0..40).prop_map(|bytes| percent_encode(&bytes)),
        2 => ".{0,30}".prop_map(|text| percent_encode(text.as_bytes())),
        1 => Just("%00".to_owned()),
        1 => Just("a%00b".to_owned()),
        1 => Just("%2F".to_owned()),
        1 => Just("%2e%2e".to_owned()),
        1 => Just("%FF%FE".to_owned()),
        1 => Just("s".repeat(201)),
        1 => Just("k".repeat(5000)),
        1 => Just("%E2%80%AE%F0%9F%98%80".to_owned()),
        2 => Just("s".to_owned()),
        1 => Just("assistant".to_owned()),
        1 => Just("other".to_owned()),
    ]
}

pub fn json_value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::from),
        any::<i64>().prop_map(Value::from),
        any::<f64>().prop_map(|number| json!(number)),
        ".{0,20}".prop_map(Value::from),
    ];
    leaf.prop_recursive(3, 24, 4, |inner| {
        prop_oneof![
            proptest::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
            proptest::collection::btree_map(".{0,6}", inner, 0..4)
                .prop_map(|map| Value::Object(map.into_iter().collect())),
        ]
    })
}

pub fn weird_text() -> impl Strategy<Value = Value> {
    prop_oneof![
        3 => ".{0,40}".prop_map(Value::from),
        1 => Just(json!("")),
        1 => Just(json!("   ")),
        1 => Just(json!("\u{0}")),
        1 => Just(json!("a\u{0}b")),
        1 => Just(json!("\u{202e}\u{1F600}\u{0301}")),
        1 => Just(json!("x".repeat(100_000))),
        1 => json_value(),
    ]
}

pub fn event_body() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        3 => (weird_text(), weird_text(), proptest::option::of(json_value()), proptest::option::of(".{0,10}"))
            .prop_map(|(id, text, meta, extra)| {
                let mut body = json!({ "id": id, "text": text });
                if let Some(meta) = meta {
                    body["meta"] = meta;
                }
                if let Some(extra) = extra {
                    body[extra] = json!(1);
                }
                body.to_string().into_bytes()
            }),
        2 => json_value().prop_map(|value| value.to_string().into_bytes()),
        2 => proptest::collection::vec(any::<u8>(), 0..200),
        1 => Just(b"{\"id\":\"a\",\"text\":\"b\"".to_vec()),
        1 => Just(vec![b'['; 5000]),
        1 => Just(b"\xEF\xBB\xBF{\"id\":\"a\",\"text\":\"b\"}".to_vec()),
        1 => Just(b"{\"id\":\"a\",\"text\":\"b\",\"id\":\"c\"}".to_vec()),
    ]
}

pub fn content_type() -> impl Strategy<Value = Option<&'static str>> {
    proptest::option::of(proptest::sample::select(vec![
        "application/json",
        "text/plain",
        "application/json; charset=utf-16",
        "multipart/form-data; boundary=x",
        "application/x-www-form-urlencoded",
        "*/*",
        "json",
    ]))
}

pub fn assert_contract(label: &str, status: StatusCode, body: &[u8]) {
    assert!(
        !status.is_server_error(),
        "{label}: {status} {}",
        String::from_utf8_lossy(body)
    );
    if !status.is_client_error() {
        return;
    }
    let value: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    assert!(
        value.is_object(),
        "{label}: {status} body is not a JSON object: {body:?}"
    );
    let object = value.as_object().unwrap();
    assert_eq!(
        object.keys().collect::<Vec<_>>(),
        ["error"],
        "{label}: {value}"
    );
    let error = object["error"].as_object().unwrap();
    let mut keys: Vec<&String> = error.keys().collect();
    keys.sort();
    assert_eq!(keys, ["code", "message"], "{label}: {value}");
    let code = error["code"].as_str().unwrap();
    assert!(error["message"].is_string(), "{label}: {value}");
    let allowed = ERROR_CODES
        .iter()
        .find(|(known, _)| *known == status.as_u16())
        .map(|(_, codes)| *codes)
        .unwrap_or_default();
    assert!(
        allowed.contains(&code),
        "{label}: {status} code {code:?} is not documented"
    );
}

pub async fn send(
    client: &reqwest::Client,
    method: Method,
    url: &str,
    token: Option<&str>,
    content_type: Option<&str>,
    body: Vec<u8>,
) -> (StatusCode, Vec<u8>) {
    let mut request = client.request(method, url).body(body);
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    if let Some(content_type) = content_type {
        request = request.header("content-type", content_type);
    }
    let response = request.send().await.unwrap();
    let status = response.status();
    if response
        .headers()
        .get("content-type")
        .is_some_and(|value| value == "text/event-stream")
    {
        return (status, Vec::new());
    }
    (status, response.bytes().await.unwrap().to_vec())
}
