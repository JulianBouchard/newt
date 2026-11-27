#![expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use newt_core::message::{Message, Part, Role, ToolCall};
use newt_core::prompt::ModelRequest;
use newt_core::provider::{
    CallTiming, Model, ModelResponse, ProviderError, StopReason, StreamEvent, Usage,
};
use newt_core::workspace::Workspace;
use newt_server::backend::StaticBackend;
use newt_server::http::{AppState, router};
use newt_server::live::spawn_listener;
use newt_server::runner::{Runner, RunnerConfig};
use newt_server::store::Store;
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

struct TwoStepModel {
    calls: AtomicUsize,
}

fn response(parts: Vec<Part>) -> ModelResponse {
    ModelResponse {
        message: Message {
            role: Role::Assistant,
            parts,
        },
        usage: Usage::default(),
        stop: StopReason::ToolUse,
        timing: CallTiming::default(),
        response_id: None,
        request_ids: BTreeMap::new(),
    }
}

fn stream_call(on_event: &mut (dyn FnMut(StreamEvent) + Send), call: &ToolCall, chunks: &[&str]) {
    on_event(StreamEvent::ToolCallStarted {
        id: call.id.clone(),
        name: call.name.clone(),
    });
    for chunk in chunks {
        on_event(StreamEvent::ToolInputDelta {
            id: call.id.clone(),
            delta: (*chunk).to_owned(),
        });
    }
    on_event(StreamEvent::ToolCallReady(call.clone()));
}

impl Model for TwoStepModel {
    async fn stream(
        &self,
        _request: &ModelRequest<'_>,
        _cancel: &CancellationToken,
        on_event: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> Result<ModelResponse, ProviderError> {
        tokio::task::yield_now().await;
        on_event(StreamEvent::OutputStarted);
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            on_event(StreamEvent::TextDelta("Let me ".into()));
            on_event(StreamEvent::TextDelta("look.".into()));
            let read = ToolCall {
                id: "read-1".into(),
                name: "read_file".into(),
                arguments: json!({ "paths": ["missing.md"] }),
            };
            stream_call(on_event, &read, &["{\"paths\":", " [\"missing.md\"]}"]);
            return Ok(response(vec![
                Part::text("Let me look."),
                Part::ToolCall(read),
            ]));
        }
        on_event(StreamEvent::TextDelta("All done.".into()));
        let end = ToolCall {
            id: "end-1".into(),
            name: "end_turn".into(),
            arguments: json!({ "nothing_new": "chat" }),
        };
        stream_call(on_event, &end, &["{\"nothing_new\":\"chat\"}"]);
        Ok(response(vec![Part::text("All done."), Part::ToolCall(end)]))
    }
}

async fn start(pool: PgPool, streaming: bool) -> (String, impl Sized) {
    let (live, _listener) = spawn_listener(pool.clone()).await.unwrap();
    let agents: HashSet<String> = if streaming {
        HashSet::from(["assistant".to_owned()])
    } else {
        HashSet::new()
    };
    let runner = Runner::builder(
        Store::new(pool.clone()),
        StaticBackend {
            system: "You are a helpful agent.".into(),
            model: "test".into(),
            workspace: Workspace::default(),
        },
        TwoStepModel {
            calls: AtomicUsize::new(0),
        },
    )
    .config(RunnerConfig::default())
    .live(live.clone())
    .live_stream(agents.clone())
    .build();
    let loops = runner.spawn_loops();
    let state = AppState {
        store: Store::new(pool),
        tokens: Arc::new(HashMap::from([(
            "assistant".to_owned(),
            "secret".to_owned(),
        )])),
        live,
        shutdown: CancellationToken::new(),
        observers: newt_server::events::Observers::default(),
        live_meta: Arc::default(),
        live_stream: Arc::new(agents),
        metrics: Arc::default(),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });
    (format!("http://{address}"), loops)
}

fn chat_body(message_id: &str, text: &str) -> Value {
    json!({
        "id": "chat-1",
        "trigger": "submit-message",
        "messages": [
            { "id": "earlier", "role": "user", "parts": [{ "type": "text", "text": "old" }] },
            { "id": message_id, "role": "user", "parts": [{ "type": "text", "text": text }] },
        ],
    })
}

async fn chat(base: &str, body: &Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base}/v1/agents/assistant/sessions/c1/chat"))
        .bearer_auth("secret")
        .json(body)
        .send()
        .await
        .unwrap()
}

async fn chunks(response: reqwest::Response) -> Vec<String> {
    response
        .text()
        .await
        .unwrap()
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(str::to_owned)
        .collect()
}

fn collapsed_types(chunks: &[String]) -> Vec<String> {
    let mut types: Vec<String> = Vec::new();
    for chunk in chunks {
        let Ok(value) = serde_json::from_str::<Value>(chunk) else {
            continue;
        };
        let kind = value["type"].as_str().unwrap().to_owned();
        if kind == "tool-output-available"
            || types.last() == Some(&kind) && kind.ends_with("-delta")
        {
            continue;
        }
        types.push(kind);
    }
    types
}

#[sqlx::test(migrations = "../../migrations")]
async fn chat_streams_one_turn_as_an_ai_sdk_ui_message_stream(pool: PgPool) {
    let (base, _loops) = start(pool, true).await;

    let response = chat(&base, &chat_body("m1", "any news?")).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-vercel-ai-ui-message-stream"], "v1");
    let chunks = chunks(response).await;

    assert_eq!(chunks.last().map(String::as_str), Some("[DONE]"));
    assert_eq!(
        collapsed_types(&chunks),
        [
            "start",
            "start-step",
            "text-start",
            "text-delta",
            "text-end",
            "tool-input-start",
            "tool-input-delta",
            "tool-input-available",
            "finish-step",
            "start-step",
            "text-start",
            "text-delta",
            "text-end",
            "tool-input-start",
            "tool-input-delta",
            "tool-input-available",
            "finish-step",
            "finish",
        ]
    );
    let parsed: Vec<Value> = chunks
        .iter()
        .filter_map(|chunk| serde_json::from_str(chunk).ok())
        .collect();
    let text: String = parsed
        .iter()
        .filter(|chunk| chunk["type"] == "text-delta")
        .map(|chunk| chunk["delta"].as_str().unwrap())
        .collect();
    assert_eq!(text, "Let me look.All done.");
    let input_text: String = parsed
        .iter()
        .filter(|chunk| chunk["type"] == "tool-input-delta" && chunk["toolCallId"] == "read-1")
        .map(|chunk| chunk["inputTextDelta"].as_str().unwrap())
        .collect();
    assert_eq!(input_text, "{\"paths\": [\"missing.md\"]}");
    let available = parsed
        .iter()
        .find(|chunk| chunk["type"] == "tool-input-available" && chunk["toolCallId"] == "read-1")
        .unwrap();
    assert_eq!(available["toolName"], "read_file");
    assert_eq!(available["input"], json!({ "paths": ["missing.md"] }));
    assert!(
        parsed.iter().any(
            |chunk| chunk["type"] == "tool-output-available" && chunk["toolCallId"] == "read-1"
        )
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn posting_the_same_message_again_replays_its_turn_without_running_another(pool: PgPool) {
    let (base, _loops) = start(pool, true).await;
    let first = chunks(chat(&base, &chat_body("m1", "any news?")).await).await;

    let again = chunks(chat(&base, &chat_body("m1", "any news?")).await).await;

    assert_eq!(collapsed_types(&again), collapsed_types(&first));
    assert!(again.iter().any(|chunk| chunk.contains("All done.")));
}

#[sqlx::test(migrations = "../../migrations")]
async fn chat_is_refused_for_an_agent_that_does_not_stream(pool: PgPool) {
    let (base, _loops) = start(pool, false).await;

    let response = chat(&base, &chat_body("m1", "hi")).await;

    assert_eq!(response.status(), 400);
    let body: Value = response.json().await.unwrap();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("live_stream")
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn chat_needs_the_agent_token_and_a_user_message(pool: PgPool) {
    let (base, _loops) = start(pool, true).await;
    let anonymous = reqwest::Client::new()
        .post(format!("{base}/v1/agents/assistant/sessions/c1/chat"))
        .json(&chat_body("m1", "hi"))
        .send()
        .await
        .unwrap();
    assert_eq!(anonymous.status(), 401);

    let empty = chat(&base, &json!({ "messages": [] })).await;
    assert_eq!(empty.status(), 400);
}
