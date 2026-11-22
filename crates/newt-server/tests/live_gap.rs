#![expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use newt_server::http::{AppState, router};
use newt_server::live::Topics;
use newt_server::store::{InboundEvent, Store};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

async fn serve(pool: PgPool) -> String {
    let state = AppState {
        store: Store::new(pool),
        tokens: Arc::new(HashMap::from([(
            "assistant".to_owned(),
            "secret".to_owned(),
        )])),
        live: Topics::default(),
        shutdown: CancellationToken::new(),
        instrumentation: None,
        live_meta: Arc::default(),
        live_stream: Arc::default(),
        metrics: Arc::default(),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });
    format!("http://{address}")
}

async fn first_frames(base: &str, query: &str, count: usize) -> Vec<(Option<String>, Value)> {
    let mut response = reqwest::Client::new()
        .get(format!("{base}/v1/agents/assistant/sessions/s/live{query}"))
        .bearer_auth("secret")
        .send()
        .await
        .unwrap();
    let mut text = String::new();
    let mut frames = Vec::new();
    while frames.len() < count {
        let chunk = tokio::time::timeout(Duration::from_secs(5), response.chunk())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        text.push_str(core::str::from_utf8(&chunk).unwrap());
        while let Some((frame, rest)) = text.split_once("\n\n") {
            let (mut id, mut data) = (None, None);
            for line in frame.lines() {
                if let Some(value) = line.strip_prefix("id:") {
                    id = Some(value.trim().to_owned());
                }
                if let Some(value) = line.strip_prefix("data:") {
                    data = Some(serde_json::from_str::<Value>(value.trim()).unwrap());
                }
            }
            if let Some(data) = data {
                frames.push((id, data));
            }
            text = rest.to_owned();
        }
    }
    frames
}

async fn session_with_frames_kept_from(pool: &PgPool, first_kept: i64) {
    let store = Store::new(pool.clone());
    let event: InboundEvent = serde_json::from_value(json!({ "id": "e1", "text": "hi" })).unwrap();
    store
        .accept_event("assistant", "s", &event, false)
        .await
        .unwrap();
    let frames = (0..5)
        .map(|index| json!({ "type": "tool_started", "n": index }))
        .collect();
    store
        .append_live_batch("assistant", "s", frames, false)
        .await
        .unwrap();
    sqlx::query("delete from live_events where seq < $1")
        .bind(first_kept)
        .execute(pool)
        .await
        .unwrap();
}

#[sqlx::test(migrations = "../../migrations")]
async fn resuming_from_before_the_oldest_kept_frame_starts_with_a_gap_frame(pool: PgPool) {
    session_with_frames_kept_from(&pool, 4).await;
    let base = serve(pool).await;

    let frames = first_frames(&base, "?cursor=1", 4).await;

    assert_eq!(frames[0].0, None, "a gap frame carries no id");
    assert_eq!(frames[0].1, json!({ "type": "gap", "from": 2, "to": 3 }));
    let ids: Vec<_> = frames[1..]
        .iter()
        .map(|(id, _)| id.clone().unwrap())
        .collect();
    assert_eq!(ids, ["4", "5", "6"]);
}

#[sqlx::test(migrations = "../../migrations")]
async fn last_event_id_resumes_get_the_same_gap(pool: PgPool) {
    session_with_frames_kept_from(&pool, 4).await;
    let base = serve(pool).await;
    let mut response = reqwest::Client::new()
        .get(format!("{base}/v1/agents/assistant/sessions/s/live"))
        .header("last-event-id", "2")
        .bearer_auth("secret")
        .send()
        .await
        .unwrap();
    let chunk = response.chunk().await.unwrap().unwrap();
    let text = core::str::from_utf8(&chunk).unwrap().to_owned();
    assert!(text.contains(r#""type":"gap""#), "{text}");
    assert!(
        text.contains(r#""from":3"#) && text.contains(r#""to":3"#),
        "{text}"
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn no_gap_when_the_cursor_is_current_or_a_fresh_start(pool: PgPool) {
    session_with_frames_kept_from(&pool, 4).await;
    let base = serve(pool).await;

    let current = first_frames(&base, "?cursor=3", 1).await;
    assert_eq!(current[0].0.as_deref(), Some("4"));
    let fresh = first_frames(&base, "", 1).await;
    assert_eq!(fresh[0].0.as_deref(), Some("4"));
    let caught_up = first_frames(&base, "?cursor=5", 1).await;
    assert_eq!(caught_up[0].0.as_deref(), Some("6"));
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_gap_covers_frames_when_retention_deleted_all_of_them(pool: PgPool) {
    session_with_frames_kept_from(&pool, 100).await;
    let store = Store::new(pool);
    assert_eq!(
        store.live_gap("assistant", "s", 2).await.unwrap(),
        Some((3, 6))
    );
    assert_eq!(store.live_gap("assistant", "s", 6).await.unwrap(), None);
    assert_eq!(
        store.live_gap("assistant", "nobody", 2).await.unwrap(),
        None
    );
}
