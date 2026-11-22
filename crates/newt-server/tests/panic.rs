#![expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use newt_core::message::{Message, Part, Role, ToolCall};
use newt_core::prompt::ModelRequest;
use newt_core::provider::{
    CallTiming, Model, ModelResponse, ProviderError, StopReason, StreamEvent, Usage,
};
use newt_core::workspace::Workspace;
use newt_server::backend::StaticBackend;
use newt_server::live::Topics;
use newt_server::runner::{Runner, RunnerConfig};
use newt_server::store::{InboundEvent, Store};
use serde_json::json;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

struct PanicsOnce {
    panicked: AtomicBool,
}

impl Model for PanicsOnce {
    async fn stream(
        &self,
        _request: &ModelRequest<'_>,
        _cancel: &CancellationToken,
        _on_event: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> Result<ModelResponse, ProviderError> {
        tokio::task::yield_now().await;
        assert!(self.panicked.swap(true, Ordering::SeqCst), "forced panic");
        Ok(ModelResponse {
            message: Message {
                role: Role::Assistant,
                parts: vec![
                    Part::text("recovered"),
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

#[sqlx::test(migrations = "../../migrations")]
async fn a_panicking_turn_releases_its_lease_and_the_event_runs_again_at_once(pool: PgPool) {
    let store = Store::new(pool.clone());
    let event: InboundEvent = serde_json::from_value(json!({ "id": "e1", "text": "hi" })).unwrap();
    store
        .accept_event("assistant", "p1", &event, false)
        .await
        .unwrap();
    let runner = Runner::new(
        store.clone(),
        StaticBackend {
            system: "You are a helpful agent.".into(),
            model: "test".into(),
            workspace: Workspace::default(),
        },
        PanicsOnce {
            panicked: AtomicBool::new(false),
        },
        RunnerConfig {
            lease_ttl: Duration::from_secs(60),
            sweep_every: Duration::from_millis(50),
            backoff_base: Duration::from_millis(10),
            ..RunnerConfig::default()
        },
        None,
        Topics::default(),
    );
    let _loops = runner.spawn_loops();

    let started = Instant::now();
    loop {
        let status = store
            .session_status("assistant", "p1")
            .await
            .unwrap()
            .unwrap();
        if status.pending_events == 0 && status.lease_owner.is_none() {
            assert_eq!(status.history_entries, 2);
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the session stalled on its lease: {status:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    runner.shutdown().await;
}
