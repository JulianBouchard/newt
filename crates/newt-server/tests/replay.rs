#![expect(
    clippy::unwrap_used,
    clippy::unused_async_trait_impl,
    reason = "tests fail loudly on setup errors; the echo model reads like a real async one"
)]

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use newt_core::message::{Message, Part, Role, ToolCall};
use newt_core::prompt::ModelRequest;
use newt_core::provider::{
    CallTiming, Model, ModelResponse, ProviderError, StopReason, StreamEvent, Usage,
};
use newt_core::workspace::Workspace;
use newt_server::backend::StaticBackend;
use newt_server::http::{AppState, router};
use newt_server::live::spawn_listener;
use newt_server::replay::Replay;
use newt_server::runner::{Runner, RunnerConfig};
use newt_server::store::Store;
use serde_json::json;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

/// Answers the latest event, saying which agent answers: the cache key
/// starts with the agent's name.
struct Echo;

impl Model for Echo {
    async fn stream(
        &self,
        request: &ModelRequest<'_>,
        _cancel: &CancellationToken,
        _on_event: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> Result<ModelResponse, ProviderError> {
        let agent = request.cache_key.split('/').next().unwrap_or_default();
        let latest = request
            .prompt
            .messages()
            .filter(|message| message.role == Role::User)
            .flat_map(|message| &message.parts)
            .filter_map(|part| match part {
                Part::Text { text } => Some(text.clone()),
                _ => None,
            })
            .last()
            .unwrap_or_default();
        Ok(ModelResponse {
            message: Message {
                role: Role::Assistant,
                parts: vec![
                    Part::text(format!("{agent} heard: {latest}")),
                    Part::ToolCall(ToolCall {
                        id: format!("end-{latest}"),
                        name: "end_turn".into(),
                        arguments: json!({ "nothing_new": "a chat" }),
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

async fn start(pool: &PgPool) -> String {
    let (live, _listener) = spawn_listener(pool.clone()).await.unwrap();
    let runner = Runner::new(
        Store::new(pool.clone()),
        StaticBackend {
            system: "You are a helpful agent.".into(),
            model: "test".into(),
            workspace: Workspace::default(),
        },
        Echo,
        RunnerConfig {
            lease_ttl: Duration::from_secs(1),
            sweep_every: Duration::from_millis(50),
            backoff_base: Duration::from_millis(10),
            ..RunnerConfig::default()
        },
        None,
        live.clone(),
    );
    std::mem::forget(runner.spawn_loops());
    let state = AppState {
        store: Store::new(pool.clone()),
        tokens: Arc::new(HashMap::from([
            ("assistant".to_owned(), "secret".to_owned()),
            ("canary".to_owned(), "secret".to_owned()),
        ])),
        live,
        shutdown: CancellationToken::new(),
        instrumentation: None,
        live_meta: Arc::default(),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });
    format!("http://{address}")
}

async fn chat(base: &str, store: &Store, id: &str, text: &str) {
    let before = store
        .session_status("assistant", "ana")
        .await
        .unwrap()
        .map_or(0, |status| status.history_entries);
    reqwest::Client::new()
        .post(format!("{base}/v1/agents/assistant/sessions/ana/events"))
        .bearer_auth("secret")
        .json(&json!({ "id": id, "text": text }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while store
        .session_status("assistant", "ana")
        .await
        .unwrap()
        .is_none_or(|status| status.history_entries == before || status.lease_owner.is_some())
    {
        assert!(Instant::now() < deadline, "the turn never committed");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_replay_forks_the_early_turns_and_runs_the_rest_on_another_agent(pool: PgPool) {
    let base = start(&pool).await;
    let store = Store::new(pool.clone());
    chat(&base, &store, "e1", "add milk").await;
    chat(&base, &store, "e2", "and eggs").await;

    let mut out = Vec::new();
    Replay {
        base_url: base.clone(),
        token: "secret".into(),
        from: "assistant/ana".parse().unwrap(),
        into: "canary/ana".parse().unwrap(),
        from_turn: 1,
        turns: None,
    }
    .run(&mut out)
    .await
    .unwrap();
    let out = String::from_utf8(out).unwrap();
    assert_eq!(
        out,
        "\nturn 1\n  > and eggs\n  then:\n    assistant heard: and eggs\n  now:\n    canary heard: and eggs\n"
    );
    let canary = store
        .session_status("canary", "ana")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(canary.history_entries, 4);

    let again = Replay {
        base_url: base,
        token: "secret".into(),
        from: "assistant/ana".parse().unwrap(),
        into: "canary/ana".parse().unwrap(),
        from_turn: 0,
        turns: None,
    }
    .run(&mut Vec::new())
    .await;
    assert!(again.unwrap_err().to_string().contains("already exists"));
}
