#![expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use newt_core::message::ToolCall;
use newt_core::tools::{CallContext, ToolHost};
use newt_demo::{Demo, app};
use newt_server::mcp::{McpAuth, McpServer, Toolset};
use serde_json::{Value, json};

const TOKEN: &str = "demo-test-token";

async fn connect() -> (std::sync::Arc<Demo>, Toolset) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let demo = Demo::new(TOKEN.into(), &format!("http://{address}"));
    let router = app(std::sync::Arc::clone(&demo));
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let toolset = Toolset::connect(
        &[McpServer {
            name: "demo".into(),
            url: format!("http://{address}/mcp"),
            headers: BTreeMap::from([("authorization".to_owned(), format!("Bearer {TOKEN}"))]),
            auth: McpAuth::None,
        }],
        "agent-token",
    )
    .await
    .unwrap();
    (demo, toolset)
}

fn call(name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: format!("c-{name}"),
        name: name.into(),
        arguments,
    }
}

#[tokio::test]
async fn clearing_tasks_waits_for_the_persons_approval() {
    let (demo, toolset) = connect().await;
    let host = toolset.host("ana");
    for text in ["call the bank", "book the dentist"] {
        let added = host
            .call(
                &call("add_task", json!({ "text": text })),
                &CallContext::default(),
            )
            .await;
        assert!(!added.is_error, "{}", added.content);
    }

    let asked = host
        .call(&call("clear_tasks", json!({})), &CallContext::default())
        .await;
    assert!(asked.is_error);
    assert!(
        asked.content.contains("Delete all 2 tasks?"),
        "{}",
        asked.content
    );
    assert_eq!(demo.tasks("ana").len(), 2);

    let declined = host
        .call(
            &call(
                "clear_tasks",
                json!({ "_input": { "answers": { "confirm": "decline" }, "state": "clear:2" } }),
            ),
            &CallContext::default(),
        )
        .await;
    assert!(
        declined.content.contains("\"approved\":false"),
        "{}",
        declined.content
    );
    assert_eq!(demo.tasks("ana").len(), 2);

    let approved = host
        .call(
            &call("clear_tasks", json!({ "_input": { "answers": { "confirm": { "approved": true } }, "state": "clear:2" } })),
            &CallContext::default(),
        )
        .await;
    assert_eq!(
        (approved.content.as_str(), approved.is_error),
        ("{\"cleared\":2}", false)
    );
    assert_eq!(demo.tasks("ana"), Vec::<String>::new());
}

#[tokio::test]
async fn the_weekly_review_runs_as_a_task_newt_polls_until_done() {
    let (_demo, toolset) = connect().await;
    let host = toolset.host("ana");
    host.call(
        &call("add_task", json!({ "text": "renew passport" })),
        &CallContext::default(),
    )
    .await;
    let started = Instant::now();
    let review = host
        .call(&call("weekly_review", json!({})), &CallContext::default())
        .await;
    assert!(!review.is_error, "{}", review.content);
    assert!(
        review
            .content
            .contains("Week in review: 1 open tasks, 0 done"),
        "{}",
        review.content
    );
    assert!(started.elapsed() >= Duration::from_secs(2));
}
