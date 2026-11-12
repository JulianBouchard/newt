#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "tests fail loudly on setup errors"
)]

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::post;
use futures_util::{StreamExt, stream};
use newt_core::message::{Message, Part, Reasoning, Role, ToolCall, ToolResult};
use newt_core::prompt::{Effort, Lane, ModelRequest, Prompt, ToolSpec};
use newt_core::provider::{
    Model, Protocol, ProviderClient, ProviderError, StopReason, StreamEvent,
};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

type Captured = Arc<Mutex<Option<Value>>>;

async fn serve(path: &'static str, status: StatusCode, body: Body) -> (String, Captured) {
    let captured: Captured = Arc::new(Mutex::new(None));
    let sink = Arc::clone(&captured);
    let body = Arc::new(Mutex::new(Some(body)));
    let app = Router::new().route(
        path,
        post(move |request: Bytes| {
            let sink = Arc::clone(&sink);
            let body = Arc::clone(&body);
            async move {
                *sink.lock().await = Some(serde_json::from_slice(&request).unwrap());
                let body = body.lock().await.take().unwrap();
                Response::builder()
                    .status(status)
                    .header("content-type", "text/event-stream")
                    .body(body)
                    .unwrap()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{address}"), captured)
}

fn tools() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "read_file".into(),
            description: "Read workspace pages.".into(),
            input_schema: json!({ "type": "object" }),
        },
        ToolSpec {
            name: "end_turn".into(),
            description: "End the turn.".into(),
            input_schema: json!({ "type": "object" }),
        },
    ]
}

fn history() -> Vec<Message> {
    vec![
        Message::user_text("Hi, I'm looking for a new role."),
        Message {
            role: Role::Assistant,
            parts: vec![Part::text("Great, tell me more.")],
        },
    ]
}

fn scratch() -> Vec<Message> {
    vec![
        Message::user_text("Remote only, please."),
        Message {
            role: Role::Assistant,
            parts: vec![Part::ToolCall(ToolCall {
                id: "toolu_0".into(),
                name: "read_file".into(),
                arguments: json!({ "paths": ["profile.md"] }),
            })],
        },
        Message {
            role: Role::User,
            parts: vec![Part::ToolResult(ToolResult {
                call_id: "toolu_0".into(),
                content: "# Profile".into(),
                is_error: false,
            })],
        },
    ]
}

async fn stream_once(
    client: &ProviderClient,
    cancel: &CancellationToken,
) -> (
    Result<newt_core::provider::ModelResponse, ProviderError>,
    Vec<StreamEvent>,
) {
    let tools = tools();
    let history = history();
    let scratch = scratch();
    let request = ModelRequest {
        model: "test-model",
        prompt: Prompt {
            system: "You are a helpful agent.",
            tools: &tools,
            history: &history,
            scratch: &scratch,
        },
        effort: Some(Effort::Low),
        max_output_tokens: 4096,
        cache_key: "session-42",
        lane: Lane::Interactive,
    };
    let mut events = Vec::new();
    let result = client
        .stream(&request, cancel, &mut |event| events.push(event))
        .await;
    (result, events)
}

#[tokio::test]
async fn anthropic_stream_decodes_thinking_text_tool_call_and_cache_usage() {
    let fixture = include_str!("fixtures/anthropic_tool_use.sse");
    let (base, _) = serve("/v1/messages", StatusCode::OK, Body::from(fixture)).await;
    let client = ProviderClient::new(Protocol::AnthropicMessages, base, "key").unwrap();

    let (result, events) = stream_once(&client, &CancellationToken::new()).await;
    let response = result.unwrap();

    assert_eq!(
        response.message.parts,
        vec![
            Part::Reasoning(Reasoning::AnthropicThinking {
                thinking: "Need the profile first.".into(),
                signature: "sig-abc".into(),
            }),
            Part::text("Checking your profile."),
            Part::ToolCall(ToolCall {
                id: "toolu_1".into(),
                name: "read_file".into(),
                arguments: json!({ "paths": ["profile.md"] }),
            }),
        ]
    );
    assert_eq!(response.stop, StopReason::ToolUse);
    assert_eq!(response.usage.input_tokens, 2520);
    assert_eq!(response.usage.cache_read_tokens, 2400);
    assert_eq!(response.usage.output_tokens, 42);
    assert_eq!(response.response_id.as_deref(), Some("msg_1"));
    assert_eq!(
        events,
        vec![
            StreamEvent::OutputStarted,
            StreamEvent::ToolCallStarted {
                id: "toolu_1".into(),
                name: "read_file".into()
            },
        ]
    );
    assert!(response.timing.first_tool_ms.is_some());
}

#[tokio::test]
async fn anthropic_request_puts_cache_breakpoints_on_prefix_history_and_tail() {
    let fixture = include_str!("fixtures/anthropic_tool_use.sse");
    let (base, captured) = serve("/v1/messages", StatusCode::OK, Body::from(fixture)).await;
    let client = ProviderClient::new(Protocol::AnthropicMessages, base, "key").unwrap();
    stream_once(&client, &CancellationToken::new())
        .await
        .0
        .unwrap();

    let body = captured.lock().await.take().unwrap();
    let cached = |value: &Value| value.get("cache_control").is_some();
    assert!(cached(&body["system"][0]));
    assert!(!cached(&body["tools"][0]));
    assert!(cached(&body["tools"][1]));

    let messages = body["messages"].as_array().unwrap();
    let last_block_cached: Vec<bool> = messages
        .iter()
        .map(|message| cached(message["content"].as_array().unwrap().last().unwrap()))
        .collect();
    assert_eq!(last_block_cached, vec![false, true, false, false, true]);
    assert_eq!(body["output_config"]["effort"], "low");
    assert!(body.get("thinking").is_none());
    let cache_marks = body.to_string().matches("cache_control").count();
    assert!(cache_marks <= 4, "{cache_marks} cache breakpoints");
}

#[tokio::test]
async fn openai_stream_keeps_reasoning_items_and_reports_cached_tokens() {
    let fixture = include_str!("fixtures/openai_tool_use.sse");
    let (base, captured) = serve("/responses", StatusCode::OK, Body::from(fixture)).await;
    let client = ProviderClient::new(Protocol::OpenaiResponses, base, "key").unwrap();

    let (result, events) = stream_once(&client, &CancellationToken::new()).await;
    let response = result.unwrap();

    let Part::Reasoning(Reasoning::OpenaiReasoning { item }) = &response.message.parts[0] else {
        panic!(
            "expected a reasoning item first, got {:?}",
            response.message.parts
        );
    };
    assert_eq!(item["encrypted_content"], "enc-xyz");
    assert_eq!(
        response.message.parts[1],
        Part::ToolCall(ToolCall {
            id: "call_1".into(),
            name: "read_file".into(),
            arguments: json!({ "paths": ["profile.md"] }),
        })
    );
    assert_eq!(response.usage.input_tokens, 3000);
    assert_eq!(response.usage.cache_read_tokens, 2048);
    assert_eq!(response.usage.reasoning_tokens, 64);
    assert_eq!(response.response_id.as_deref(), Some("resp_1"));
    assert_eq!(response.stop, StopReason::ToolUse);
    assert_eq!(
        events,
        vec![
            StreamEvent::OutputStarted,
            StreamEvent::ToolCallStarted {
                id: "call_1".into(),
                name: "read_file".into()
            },
        ]
    );

    let body = captured.lock().await.take().unwrap();
    assert_eq!(body["prompt_cache_key"], "session-42");
    assert_eq!(body["store"], false);
    assert_eq!(body["reasoning"]["effort"], "low");
    assert_eq!(body["instructions"], "You are a helpful agent.");
    let kinds: Vec<&str> = body["input"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["type"].as_str().or(item["role"].as_str()).unwrap())
        .collect();
    assert_eq!(
        kinds,
        vec![
            "user",
            "assistant",
            "user",
            "function_call",
            "function_call_output"
        ]
    );
}

#[tokio::test]
async fn an_error_status_surfaces_the_status_and_body() {
    let (base, _) = serve(
        "/v1/messages",
        StatusCode::TOO_MANY_REQUESTS,
        Body::from("{\"error\":\"slow down\"}"),
    )
    .await;
    let client = ProviderClient::new(Protocol::AnthropicMessages, base, "key").unwrap();

    let (result, events) = stream_once(&client, &CancellationToken::new()).await;

    match result {
        Err(ProviderError::Status { status, body }) => {
            assert_eq!(status, 429);
            assert!(body.contains("slow down"));
        }
        other => panic!("expected a status error, got {other:?}"),
    }
    assert_eq!(events, Vec::new());
}

#[tokio::test]
async fn cancelling_mid_stream_stops_the_call() {
    let first = Bytes::from_static(
        b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1}}}\n\n",
    );
    let hanging = stream::iter([Ok::<_, std::io::Error>(first)]).chain(stream::pending());
    let (base, _) = serve("/v1/messages", StatusCode::OK, Body::from_stream(hanging)).await;
    let client = ProviderClient::new(Protocol::AnthropicMessages, base, "key").unwrap();
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        trigger.cancel();
    });

    let (result, _) = tokio::time::timeout(Duration::from_secs(5), stream_once(&client, &cancel))
        .await
        .unwrap();

    assert!(
        matches!(result, Err(ProviderError::Cancelled)),
        "{result:?}"
    );
}

#[tokio::test]
async fn a_stream_that_ends_early_is_a_protocol_error() {
    let truncated = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1}}}\n\n";
    let (base, _) = serve("/v1/messages", StatusCode::OK, Body::from(truncated)).await;
    let client = ProviderClient::new(Protocol::AnthropicMessages, base, "key").unwrap();

    let (result, _) = stream_once(&client, &CancellationToken::new()).await;

    assert!(
        matches!(result, Err(ProviderError::Protocol(_))),
        "{result:?}"
    );
}

#[tokio::test]
async fn a_tool_call_cut_off_by_max_tokens_is_flagged_not_fatal() {
    let truncated = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_9\",\"name\":\"save_note\",\"input\":{}}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"text\\\": \\\"half\"}}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"max_tokens\"},\"usage\":{\"output_tokens\":4096}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    let (base, _) = serve("/v1/messages", StatusCode::OK, Body::from(truncated)).await;
    let client = ProviderClient::new(Protocol::AnthropicMessages, base, "key").unwrap();

    let response = stream_once(&client, &CancellationToken::new())
        .await
        .0
        .unwrap();

    assert_eq!(response.stop, StopReason::MaxTokens);
    let Part::ToolCall(call) = &response.message.parts[0] else {
        panic!("expected a tool call, got {:?}", response.message.parts);
    };
    assert_eq!(call.invalid_arguments(), Some("{\"text\": \"half"));
}

#[tokio::test]
async fn a_mid_stream_overload_is_a_retryable_api_error() {
    let overloaded = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10}}}\n\n",
        "event: error\n",
        "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n",
    );
    let (base, _) = serve("/v1/messages", StatusCode::OK, Body::from(overloaded)).await;
    let client = ProviderClient::new(Protocol::AnthropicMessages, base, "key").unwrap();

    let error = stream_once(&client, &CancellationToken::new())
        .await
        .0
        .unwrap_err();

    assert!(matches!(&error, ProviderError::Api { kind, .. } if kind == "overloaded_error"));
    assert!(error.retryable());
}

#[tokio::test]
async fn an_api_key_header_route_sends_that_header_and_no_bearer_token() {
    let seen: Arc<Mutex<Option<axum::http::HeaderMap>>> = Arc::new(Mutex::new(None));
    let sink = Arc::clone(&seen);
    let app = Router::new().route(
        "/responses",
        post(move |headers: axum::http::HeaderMap| {
            let sink = Arc::clone(&sink);
            async move {
                *sink.lock().await = Some(headers);
                Response::builder()
                    .header("content-type", "text/event-stream")
                    .header("apim-request-id", "req-42")
                    .body(Body::from(include_str!("fixtures/openai_tool_use.sse")))
                    .unwrap()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = ProviderClient::new(
        Protocol::OpenaiResponses,
        format!("http://{address}"),
        "azure-key",
    )
    .unwrap()
    .with_api_key_header("api-key");

    let response = stream_once(&client, &CancellationToken::new())
        .await
        .0
        .unwrap();

    let headers = seen.lock().await.take().unwrap();
    assert_eq!(headers.get("api-key").unwrap(), "azure-key");
    assert!(headers.get("authorization").is_none());
    assert_eq!(
        response
            .request_ids
            .get("apim-request-id")
            .map(String::as_str),
        Some("req-42")
    );
}

#[tokio::test]
async fn capture_mode_writes_the_request_and_raw_stream_without_the_key() {
    let fixture = include_str!("fixtures/anthropic_tool_use.sse");
    let (base, _) = serve("/v1/messages", StatusCode::OK, Body::from(fixture)).await;
    let dir = tempfile::tempdir().unwrap();
    let client = ProviderClient::new(Protocol::AnthropicMessages, base, "secret-key")
        .unwrap()
        .with_capture_dir(dir.path());

    stream_once(&client, &CancellationToken::new())
        .await
        .0
        .unwrap();

    let request = std::fs::read_to_string(dir.path().join("0000-request.json")).unwrap();
    let stream = std::fs::read_to_string(dir.path().join("0000-response.sse")).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&request).unwrap()["model"],
        "test-model"
    );
    assert!(!request.contains("secret-key"));
    assert!(stream.starts_with("HTTP 200 OK\n\n"));
    assert!(stream.ends_with(fixture));
}
