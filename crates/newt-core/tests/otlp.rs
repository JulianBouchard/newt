#![expect(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "tests fail loudly on setup errors"
)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::post;
use newt_core::event::{Event, EventKind};
use newt_core::message::{Message, Part, Role, ToolCall};
use newt_core::otlp::{Hooks, OtlpConfig, OtlpSink, Profile, Redactor, parse_headers};
use newt_core::prompt::LayerFingerprint;
use newt_core::provider::{CallTiming, StopReason, Usage};
use newt_core::sink::EventSink;
use newt_core::tools::Retention;
use serde_json::Value;

type Captured = Arc<Mutex<Vec<(Option<String>, Value)>>>;

async fn collector() -> (String, Captured) {
    let captured: Captured = Arc::default();
    let app =
        axum::Router::new()
            .route(
                "/v1/traces",
                post(
                    |State(captured): State<Captured>,
                     headers: HeaderMap,
                     Json(body): Json<Value>| async move {
                        let auth = headers
                            .get("authorization")
                            .and_then(|value| value.to_str().ok())
                            .map(str::to_owned);
                        captured.lock().unwrap().push((auth, body));
                    },
                ),
            )
            .with_state(Arc::clone(&captured));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{address}/v1/traces"), captured)
}

fn fingerprint() -> LayerFingerprint {
    LayerFingerprint::of(&[Message::user_text("prefix")][..], 1)
}

fn turn_events(turn: &str, secret: &str) -> Vec<Event> {
    let session = "assistant/customer-7".to_owned();
    let turn = turn.to_owned();
    let call = |call| EventKind::ModelCallStarted {
        session: session.clone(),
        turn: turn.clone(),
        call,
        model: "fake-model".into(),
        effort: None,
        prefix: fingerprint(),
        history: fingerprint(),
        scratch: fingerprint(),
        input: vec![Message::user_text(format!("my phone is {secret}"))],
    };
    let finished = |call, output: Message| EventKind::ModelCallFinished {
        session: session.clone(),
        turn: turn.clone(),
        call,
        usage: Usage {
            input_tokens: 1000,
            cache_read_tokens: 800,
            output_tokens: 50,
            ..Usage::default()
        },
        timing: CallTiming {
            first_output_ms: Some(400),
            total_ms: 900,
            ..CallTiming::default()
        },
        stop: StopReason::ToolUse,
        response_id: Some("resp_1".into()),
        request_ids: BTreeMap::new(),
        output,
    };
    let tool_call = Message {
        role: Role::Assistant,
        parts: vec![Part::ToolCall(ToolCall {
            id: "r1".into(),
            name: "read_file".into(),
            arguments: serde_json::json!({ "paths": ["profile.md"] }),
        })],
    };
    [
        EventKind::TurnStarted {
            session: session.clone(),
            turn: turn.clone(),
            history_messages: 0,
            events: 1,
            compaction: false,
        },
        call(0),
        EventKind::ToolStarted {
            session: session.clone(),
            turn: turn.clone(),
            call_id: "r1".into(),
            name: "read_file".into(),
        },
        finished(0, tool_call),
        EventKind::ToolFinished {
            session: session.clone(),
            turn: turn.clone(),
            call_id: "r1".into(),
            name: "read_file".into(),
            retention: Retention::Ephemeral,
            is_error: false,
            ms: 3,
        },
        call(1),
        finished(1, Message::user_text("done")),
        EventKind::TurnCompleted {
            session: session.clone(),
            turn: turn.clone(),
            model_calls: 2,
            committed_messages: 1,
        },
    ]
    .into_iter()
    .map(Event::now)
    .collect()
}

struct MaskDigits;

impl Redactor for MaskDigits {
    fn redact(&self, content: String) -> String {
        content
            .chars()
            .map(|c| if c.is_ascii_digit() { '#' } else { c })
            .collect()
    }
}

fn attribute<'a>(span: &'a Value, key: &str) -> Option<&'a Value> {
    span["attributes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|attribute| attribute["key"] == key)
        .map(|attribute| &attribute["value"])
}

async fn export(
    profile: Profile,
    capture_content: bool,
    turns: &[&str],
) -> Vec<(Option<String>, Value)> {
    export_events(
        profile,
        capture_content,
        turns
            .iter()
            .flat_map(|turn| turn_events(turn, "555 0100"))
            .collect(),
    )
    .await
}

async fn export_events(
    profile: Profile,
    capture_content: bool,
    events: Vec<Event>,
) -> Vec<(Option<String>, Value)> {
    let (endpoint, captured) = collector().await;
    let (sink, exporter) = OtlpSink::spawn(
        OtlpConfig {
            endpoint,
            headers: parse_headers("Authorization=Basic abc, x-langfuse-ingestion-version=4")
                .unwrap(),
            profile,
            capture_content,
            service_name: "newt-test".into(),
        },
        Hooks {
            enricher: None,
            redactor: Some(Arc::new(MaskDigits)),
        },
        64,
    );
    for event in events {
        sink.emit(event);
    }
    drop(sink);
    exporter.await.unwrap();
    captured.lock().unwrap().clone()
}

fn spans(body: &Value) -> Vec<Value> {
    body["resourceSpans"][0]["scopeSpans"][0]["spans"]
        .as_array()
        .unwrap()
        .clone()
}

#[tokio::test]
async fn each_turn_is_one_trace_with_chat_and_tool_spans_under_its_root() {
    let exports = export(Profile::Langfuse, true, &["t1", "t2"]).await;
    assert_eq!(exports.len(), 2, "one export per finished turn");
    assert_eq!(exports[0].0.as_deref(), Some("Basic abc"));

    let first = spans(&exports[0].1);
    assert_eq!(first.len(), 4, "root, two chat calls, one tool");
    let root = &first[0];
    assert_eq!(root["name"], "invoke_agent assistant");
    assert!(root.get("parentSpanId").is_none());
    for child in &first[1..] {
        assert_eq!(child["traceId"], root["traceId"]);
        assert_eq!(child["parentSpanId"], root["spanId"]);
    }
    assert_eq!(
        attribute(root, "langfuse.session.id").unwrap()["stringValue"],
        "assistant/customer-7"
    );
    assert_eq!(
        attribute(root, "gen_ai.conversation.id").unwrap()["stringValue"],
        "assistant/customer-7"
    );

    let chat = &first[1];
    assert_eq!(chat["name"], "chat fake-model");
    assert_eq!(
        attribute(chat, "langfuse.observation.type").unwrap()["stringValue"],
        "generation"
    );
    let usage: Value = serde_json::from_str(
        attribute(chat, "langfuse.observation.usage_details").unwrap()["stringValue"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(usage["input"], 200);
    assert_eq!(usage["input_cached_tokens"], 800);
    let input = attribute(chat, "gen_ai.input.messages").unwrap()["stringValue"]
        .as_str()
        .unwrap();
    assert!(input.contains("### ####"), "{input}");
    assert!(!input.contains("555"), "the redactor runs before export");
    assert_eq!(first[3]["name"], "execute_tool read_file");

    let second = spans(&exports[1].1);
    assert_ne!(
        second[0]["traceId"], root["traceId"],
        "a new turn is a new trace"
    );
    assert_eq!(
        attribute(&second[0], "langfuse.session.id"),
        attribute(root, "langfuse.session.id")
    );
}

#[tokio::test]
async fn ids_are_stable_so_a_reexport_of_the_same_attempt_overwrites() {
    let events = turn_events("t1", "555 0100");
    let once = export_events(Profile::Generic, true, events.clone()).await;
    let again = export_events(Profile::Generic, true, events).await;
    let ids = |exports: &[(Option<String>, Value)]| -> Vec<(Value, Value)> {
        spans(&exports[0].1)
            .iter()
            .map(|span| (span["traceId"].clone(), span["spanId"].clone()))
            .collect()
    };
    assert_eq!(ids(&once), ids(&again));
    assert_eq!(spans(&once[0].1)[0]["traceId"].as_str().unwrap().len(), 32);
}

#[tokio::test]
async fn without_content_capture_no_messages_leave_the_process() {
    let exports = export(Profile::Langfuse, false, &["t1"]).await;
    let body = exports[0].1.to_string();
    assert!(!body.contains("my phone"), "{body}");
    assert!(!body.contains("profile.md"), "{body}");
}

#[test]
fn malformed_headers_are_rejected() {
    assert!(parse_headers("Authorization").is_err());
    assert_eq!(parse_headers("").unwrap(), Vec::new());
}

#[tokio::test]
async fn without_content_capture_errors_and_session_keys_stay_private() {
    let mut events = turn_events("t1", "555 0100");
    events.pop();
    events.push(Event::now(EventKind::TurnFailed {
        session: "assistant/customer-7".into(),
        turn: "t1".into(),
        error: "provider said: my phone is 555 0100".into(),
        committed_messages: 0,
    }));
    let exports = export_events(Profile::Langfuse, false, events).await;
    let body = exports[0].1.to_string();
    assert!(!body.contains("555"), "{body}");
    assert!(!body.contains("customer-7"), "{body}");
    assert_eq!(spans(&exports[0].1)[0]["status"]["message"], "failed");
}

#[tokio::test]
async fn ids_differ_across_turns_calls_and_tools() {
    let exports = export(Profile::Generic, true, &["t1", "t2"]).await;
    let mut span_ids: Vec<Value> = exports
        .iter()
        .flat_map(|(_, body)| spans(body))
        .map(|span| span["spanId"].clone())
        .collect();
    let total = span_ids.len();
    span_ids.sort_by_key(ToString::to_string);
    span_ids.dedup();
    assert_eq!(span_ids.len(), total);
}
