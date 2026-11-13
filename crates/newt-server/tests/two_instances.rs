//! Two newt processes sharing one Postgres: each event is answered once, by one of them.

#![expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
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
use serde_json::json;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

const SESSIONS: usize = 20;
const EVENTS_PER_SESSION: usize = 3;

struct CountingModel {
    calls: Arc<AtomicUsize>,
}

impl Model for CountingModel {
    async fn stream(
        &self,
        _request: &ModelRequest<'_>,
        _cancel: &CancellationToken,
        _on_event: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> Result<ModelResponse, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(20)).await;
        Ok(ModelResponse {
            message: Message {
                role: Role::Assistant,
                parts: vec![
                    Part::text("ok"),
                    Part::ToolCall(ToolCall {
                        id: "end".into(),
                        name: "end_turn".into(),
                        arguments: json!({ "nothing_new": "answered" }),
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

async fn instance(pool: &PgPool, calls: Arc<AtomicUsize>) -> Runner<StaticBackend, CountingModel> {
    let (topics, _listener) = spawn_listener(pool.clone()).await.unwrap();
    Runner::new(
        Store::new(pool.clone()),
        StaticBackend {
            system: "You are a helpful agent.".into(),
            model: "test".into(),
            workspace: Workspace::default(),
        },
        CountingModel { calls },
        RunnerConfig {
            lease_ttl: Duration::from_secs(2),
            sweep_every: Duration::from_millis(50),
            shutdown_grace: Duration::from_secs(1),
            ..RunnerConfig::default()
        },
        None,
        topics,
    )
}

fn event(id: &str, text: &str) -> InboundEvent {
    serde_json::from_value(json!({ "id": id, "text": text })).unwrap()
}

fn marker(session: usize, event: usize) -> String {
    format!("[s{session:02}-e{event}]")
}

async fn scalar(pool: &PgPool, sql: &'static str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}

#[sqlx::test(migrations = "../../migrations")]
async fn two_instances_answer_every_event_once_between_them(pool: PgPool) {
    let first_calls = Arc::new(AtomicUsize::new(0));
    let second_calls = Arc::new(AtomicUsize::new(0));
    let first = instance(&pool, Arc::clone(&first_calls)).await;
    let second = instance(&pool, Arc::clone(&second_calls)).await;
    let _first_loops = first.spawn_loops();
    let _second_loops = second.spawn_loops();
    let store = Store::new(pool.clone());

    for round in 0..EVENTS_PER_SESSION {
        for session in 0..SESSIONS {
            store
                .accept_event(
                    "assistant",
                    &format!("s{session:02}"),
                    &event(&format!("e{round}"), &marker(session, round)),
                )
                .await
                .unwrap();
        }
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    while scalar(
        &pool,
        "select count(*) from inbox where consumed_at is null \
           or exists (select 1 from sessions s where s.lease_owner is not null)",
    )
    .await
        > 0
    {
        assert!(Instant::now() < deadline, "events still pending after 30s");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::join!(first.shutdown(), second.shutdown());

    assert_eq!(
        scalar(
            &pool,
            "select count(*) from inbox where dead_letter is not null"
        )
        .await,
        0
    );
    for session in 0..SESSIONS {
        for round in 0..EVENTS_PER_SESSION {
            let seen: i64 = sqlx::query_scalar(
                "select count(*) from history where kind = 'message' and entry::text like $1",
            )
            .bind(format!("%{}%", marker(session, round)))
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(
                seen,
                1,
                "{} reached history {seen} times",
                marker(session, round)
            );
        }
    }
    let turns = scalar(&pool, "select count(distinct (key, turn_id)) from history").await;
    let calls = first_calls.load(Ordering::SeqCst) + second_calls.load(Ordering::SeqCst);
    assert_eq!(
        i64::try_from(calls).unwrap(),
        turns,
        "every model call belongs to exactly one committed turn"
    );
    assert!(
        first_calls.load(Ordering::SeqCst) > 0 && second_calls.load(Ordering::SeqCst) > 0,
        "both instances ran turns: {first_calls:?} and {second_calls:?}"
    );
}
