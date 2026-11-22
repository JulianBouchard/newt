#![expect(
    clippy::unwrap_used,
    clippy::unused_async_trait_impl,
    reason = "tests fail loudly on setup errors; the model reads like a real async one"
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
use newt_server::runner::{Runner, RunnerConfig};
use newt_server::store::Store;
use serde_json::json;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

struct NulWriter;

impl Model for NulWriter {
    async fn stream(
        &self,
        _request: &ModelRequest<'_>,
        _cancel: &CancellationToken,
        on_event: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> Result<ModelResponse, ProviderError> {
        on_event(StreamEvent::TextDelta("half\u{0}way".into()));
        Ok(ModelResponse {
            message: Message {
                role: Role::Assistant,
                parts: vec![
                    Part::text("half\u{0}way"),
                    Part::ToolCall(ToolCall {
                        id: "end\u{0}1".into(),
                        name: "end_turn".into(),
                        arguments: json!({ "nothing_new": "a\u{0}b" }),
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

#[sqlx::test(migrations = "../../migrations")]
async fn a_nul_in_model_output_still_commits_the_turn(pool: PgPool) {
    let (live, _listener) = spawn_listener(pool.clone()).await.unwrap();
    let runner = Runner::new(
        Store::new(pool.clone()),
        StaticBackend {
            system: "You are a helpful agent.".into(),
            model: "test".into(),
            workspace: Workspace::default(),
        },
        NulWriter,
        RunnerConfig {
            lease_ttl: Duration::from_secs(1),
            sweep_every: Duration::from_millis(50),
            backoff_base: Duration::from_millis(10),
            ..RunnerConfig::default()
        },
        None,
        live.clone(),
    )
    .with_live_stream(["assistant".to_owned()].into());
    std::mem::forget(runner.spawn_loops());
    let state = AppState {
        store: Store::new(pool.clone()),
        tokens: Arc::new(HashMap::from([(
            "assistant".to_owned(),
            "secret".to_owned(),
        )])),
        live,
        shutdown: CancellationToken::new(),
        instrumentation: None,
        live_meta: Arc::default(),
        live_stream: Arc::new(["assistant".to_owned()].into()),
        metrics: Arc::default(),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });

    reqwest::Client::new()
        .post(format!(
            "http://{address}/v1/agents/assistant/sessions/ana/events"
        ))
        .bearer_auth("secret")
        .json(&json!({ "id": "e1", "text": "hello" }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();

    let store = Store::new(pool.clone());
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = store.session_status("assistant", "ana").await.unwrap();
        if status
            .as_ref()
            .is_some_and(|status| status.history_entries > 0 && status.lease_owner.is_none())
        {
            assert_eq!(
                status.map(|status| status.lease_epoch),
                Some(1),
                "the turn was retried"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the turn never committed: {status:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let (dead,): (Option<String>,) =
        sqlx::query_as("select dead_letter from inbox where event_id = 'e1'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(dead, None, "the event was given up on");
}
