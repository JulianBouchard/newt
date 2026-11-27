#![expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]

use std::collections::{BTreeMap, HashSet};
use std::time::{Duration, Instant};

use newt_core::message::{Message, Part, Role, ToolCall};
use newt_core::prompt::ModelRequest;
use newt_core::provider::{
    CallTiming, Model, ModelResponse, ProviderError, StopReason, StreamEvent, Usage,
};
use newt_core::workspace::Workspace;
use newt_server::backend::StaticBackend;
use newt_server::live::spawn_listener;
use newt_server::runner::{Runner, RunnerConfig};
use newt_server::store::{InboundEvent, Store};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

const DELTAS: [&str; 3] = ["Hello", ", ", "world."];
const INPUT_CHUNKS: [&str; 3] = ["{\"nothing_new\"", ":\"just a ", "chat\"}"];

struct Talker;

impl Model for Talker {
    async fn stream(
        &self,
        _request: &ModelRequest<'_>,
        _cancel: &CancellationToken,
        on_event: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> Result<ModelResponse, ProviderError> {
        tokio::task::yield_now().await;
        on_event(StreamEvent::OutputStarted);
        for delta in DELTAS {
            on_event(StreamEvent::TextDelta(delta.to_owned()));
        }
        on_event(StreamEvent::ToolCallStarted {
            id: "end".into(),
            name: "end_turn".into(),
        });
        for chunk in INPUT_CHUNKS {
            on_event(StreamEvent::ToolInputDelta {
                id: "end".into(),
                delta: chunk.to_owned(),
            });
        }
        on_event(StreamEvent::ToolCallReady(ToolCall {
            id: "end".into(),
            name: "end_turn".into(),
            arguments: json!({ "nothing_new": "just a chat" }),
        }));
        Ok(ModelResponse {
            message: Message {
                role: Role::Assistant,
                parts: vec![
                    Part::text(DELTAS.concat()),
                    Part::ToolCall(ToolCall {
                        id: "end".into(),
                        name: "end_turn".into(),
                        arguments: json!({ "nothing_new": "just a chat" }),
                    }),
                ],
            },
            usage: Usage::default(),
            stop: StopReason::ToolUse,
            timing: CallTiming::default(),
            response_id: None,
            request_ids: BTreeMap::new(),
        })
    }
}

async fn live_frames(pool: &PgPool, agent: &str) -> Vec<Value> {
    let store = Store::new(pool.clone());
    let event: InboundEvent = serde_json::from_value(json!({ "id": "e1", "text": "hi" })).unwrap();
    store
        .accept_event(agent, "t1", &event, false)
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let frames: Vec<Value> = store
            .live_since(agent, "t1", 0, 100)
            .await
            .unwrap()
            .into_iter()
            .map(|row| row.event)
            .collect();
        if frames.iter().any(|frame| frame["type"] == "turn_completed") {
            return frames;
        }
        assert!(Instant::now() < deadline, "turn never completed");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn frames_with_opt_in(pool: PgPool, opted_in: bool) -> Vec<Value> {
    let (live, _listener) = spawn_listener(pool.clone()).await.unwrap();
    let runner = Runner::builder(
        Store::new(pool.clone()),
        StaticBackend {
            system: "You are a helpful agent.".into(),
            model: "test".into(),
            workspace: Workspace::default(),
        },
        Talker,
    )
    .config(RunnerConfig::default())
    .live(live)
    .live_stream(if opted_in {
        HashSet::from(["assistant".to_owned()])
    } else {
        HashSet::new()
    })
    .build();
    let _loops = runner.spawn_loops();
    live_frames(&pool, "assistant").await
}

#[sqlx::test(migrations = "../../migrations")]
async fn an_opted_in_agent_streams_text_deltas_before_the_turn_completes(pool: PgPool) {
    let frames = frames_with_opt_in(pool, true).await;

    let kinds: Vec<&str> = frames
        .iter()
        .map(|frame| frame["type"].as_str().unwrap())
        .collect();
    let completed = kinds.iter().position(|kind| *kind == "turn_completed");
    let deltas: Vec<&Value> = frames
        .iter()
        .filter(|frame| frame["type"] == "text_delta")
        .collect();
    assert_ne!(deltas.len(), 0);
    assert!(
        kinds
            .iter()
            .rposition(|kind| *kind == "text_delta")
            .zip(completed)
            .is_some_and(|(last_delta, completed)| last_delta < completed)
    );
    assert!(
        deltas
            .iter()
            .all(|frame| frame["session"] == "assistant/t1")
    );
    let text: String = deltas
        .iter()
        .map(|frame| frame["text"].as_str().unwrap())
        .collect();
    assert_eq!(text, "Hello, world.");
}

#[sqlx::test(migrations = "../../migrations")]
async fn an_opted_in_agent_streams_tool_input_deltas_before_the_full_input(pool: PgPool) {
    let frames = frames_with_opt_in(pool, true).await;

    let position = |kind: &str| frames.iter().position(|frame| frame["type"] == kind);
    let last_delta = frames
        .iter()
        .rposition(|frame| frame["type"] == "tool_input_delta")
        .unwrap();
    let started = position("tool_started").unwrap();
    let input = position("tool_input").unwrap();
    assert!(started < position("tool_input_delta").unwrap());
    assert!(last_delta < input);
    assert!(input < position("turn_completed").unwrap());
    let streamed: String = frames
        .iter()
        .filter(|frame| frame["type"] == "tool_input_delta")
        .map(|frame| {
            assert_eq!(
                (frame["call_id"].as_str(), frame["name"].as_str()),
                (Some("end"), Some("end_turn"))
            );
            frame["delta"].as_str().unwrap()
        })
        .collect();
    assert_eq!(streamed, INPUT_CHUNKS.concat());
    assert_eq!(
        frames[input]["input"],
        json!({ "nothing_new": "just a chat" })
    );
    assert_eq!(frames[input]["call_id"], "end");
}

#[sqlx::test(migrations = "../../migrations")]
async fn an_opted_in_agent_gets_slim_model_call_frames_for_step_boundaries(pool: PgPool) {
    let frames = frames_with_opt_in(pool, true).await;

    let started = frames
        .iter()
        .find(|frame| frame["type"] == "model_call_started")
        .unwrap();
    let mut fields: Vec<&str> = started
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    fields.sort_unstable();
    assert_eq!(fields, ["at", "call", "model", "session", "turn", "type"]);
    assert!(
        frames
            .iter()
            .any(|frame| frame["type"] == "model_call_finished")
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn an_agent_that_did_not_opt_in_gets_only_the_plain_frames(pool: PgPool) {
    let frames = frames_with_opt_in(pool, false).await;

    for frame in &frames {
        assert!(
            !matches!(
                frame["type"].as_str().unwrap(),
                "text_delta"
                    | "tool_input_delta"
                    | "tool_input"
                    | "model_call_started"
                    | "model_call_finished"
            ),
            "unexpected frame {frame}"
        );
    }
    assert!(frames.iter().any(|frame| frame["type"] == "tool_started"));
}
