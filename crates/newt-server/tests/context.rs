#![expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use newt_server::backend::Backend;
use newt_server::context::{ContextProviderBackend, ProviderEndpoint};
use serde_json::{Value, json};

const TOKEN: &str = "provider-test-token";

#[derive(Default)]
struct Provider {
    version: Mutex<String>,
    bootstrap: Mutex<Value>,
    profile: Mutex<String>,
    bootstraps: AtomicUsize,
    page_fetches: AtomicUsize,
    sessions: Mutex<Vec<String>>,
}

impl Provider {
    fn new() -> Arc<Self> {
        let provider = Self::default();
        *provider.version.lock().unwrap() = "v1".into();
        *provider.profile.lock().unwrap() = "Remote only".into();
        *provider.bootstrap.lock().unwrap() = json!({
            "version": "v1",
            "instructions": "You help one customer.",
            "model": "default",
            "first_call_effort": "low",
            "max_model_calls": 6,
        });
        Arc::new(provider)
    }

    fn page(&self) -> Value {
        json!({
            "key": ["customer", "profile"],
            "path": "profile.md",
            "version": "1",
            "content": *self.profile.lock().unwrap(),
        })
    }
}

fn authorized(headers: &HeaderMap) -> Result<(), StatusCode> {
    let expected = format!("Bearer {TOKEN}");
    match headers.get("authorization") {
        Some(value) if value.as_bytes() == expected.as_bytes() => Ok(()),
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}

async fn bootstrap(
    State(provider): State<Arc<Provider>>,
    headers: HeaderMap,
) -> Result<Json<Value>, StatusCode> {
    authorized(&headers)?;
    provider.bootstraps.fetch_add(1, Ordering::SeqCst);
    Ok(Json(provider.bootstrap.lock().unwrap().clone()))
}

async fn turn(
    State(provider): State<Arc<Provider>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, StatusCode> {
    authorized(&headers)?;
    provider
        .sessions
        .lock()
        .unwrap()
        .push(body["session"].as_str().unwrap().to_owned());
    let version = provider.version.lock().unwrap().clone();
    Ok(Json(
        json!({ "version": version, "pages": [provider.page()] }),
    ))
}

async fn pages(
    State(provider): State<Arc<Provider>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, StatusCode> {
    authorized(&headers)?;
    provider.page_fetches.fetch_add(1, Ordering::SeqCst);
    assert_eq!(body["session"], "customer-7");
    assert_eq!(body["keys"], json!([["customer", "profile"]]));
    Ok(Json(json!({ "pages": [provider.page()] })))
}

async fn serve(provider: &Arc<Provider>, token: &str) -> ContextProviderBackend {
    let app = axum::Router::new()
        .route("/agent/bootstrap", get(bootstrap))
        .route("/agent/turn", post(turn))
        .route("/agent/pages", post(pages))
        .with_state(Arc::clone(provider));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    ContextProviderBackend::new(
        BTreeMap::from([(
            "assistant".to_owned(),
            ProviderEndpoint {
                base_url: format!("http://{address}/agent/"),
                token: token.to_owned(),
            },
        )]),
        ["default".to_owned()],
    )
    .unwrap()
}

#[tokio::test]
async fn a_turn_gets_instructions_settings_and_pages_from_the_provider() {
    let provider = Provider::new();
    let backend = serve(&provider, TOKEN).await;
    let prepared = backend.prepare("assistant", "customer-7").await.unwrap();
    assert_eq!(prepared.system, "You help one customer.");
    assert_eq!(prepared.model, "default");
    assert_eq!(prepared.max_model_calls, 6);
    assert_eq!(prepared.max_output_tokens, 4096);
    assert_eq!(
        prepared
            .workspace
            .read("profile.md")
            .await
            .unwrap()
            .as_deref(),
        Some("Remote only")
    );
    assert_eq!(*provider.sessions.lock().unwrap(), vec!["customer-7"]);
}

#[tokio::test]
async fn the_bootstrap_is_fetched_once_per_version() {
    let provider = Provider::new();
    let backend = serve(&provider, TOKEN).await;
    backend.prepare("assistant", "customer-7").await.unwrap();
    backend.prepare("assistant", "customer-8").await.unwrap();
    assert_eq!(provider.bootstraps.load(Ordering::SeqCst), 1);

    *provider.version.lock().unwrap() = "v2".into();
    provider.bootstrap.lock().unwrap()["version"] = json!("v2");
    provider.bootstrap.lock().unwrap()["instructions"] = json!("New instructions.");
    let prepared = backend.prepare("assistant", "customer-7").await.unwrap();
    assert_eq!(provider.bootstraps.load(Ordering::SeqCst), 2);
    assert_eq!(prepared.system, "New instructions.");
}

#[tokio::test]
async fn an_invalidated_page_is_refetched_from_the_provider() {
    let provider = Provider::new();
    let backend = serve(&provider, TOKEN).await;
    let prepared = backend.prepare("assistant", "customer-7").await.unwrap();
    *provider.profile.lock().unwrap() = "Remote or Berlin".into();
    prepared.workspace.invalidate(&[vec!["customer".to_owned()]]);
    assert_eq!(
        prepared
            .workspace
            .read("profile.md")
            .await
            .unwrap()
            .as_deref(),
        Some("Remote or Berlin")
    );
    assert_eq!(provider.page_fetches.load(Ordering::SeqCst), 1);
}

async fn prepare_error(provider: &Arc<Provider>, token: &str) -> String {
    let backend = serve(provider, token).await;
    backend
        .prepare("assistant", "customer-7")
        .await
        .unwrap_err()
        .to_string()
}

#[tokio::test]
async fn a_bootstrap_naming_an_unknown_model_fails_loudly() {
    let provider = Provider::new();
    provider.bootstrap.lock().unwrap()["model"] = json!("gpt-unknown");
    let error = prepare_error(&provider, TOKEN).await;
    assert!(error.contains("model `gpt-unknown`"), "{error}");
}

#[tokio::test]
async fn a_bootstrap_with_an_unknown_field_fails_loudly() {
    let provider = Provider::new();
    provider.bootstrap.lock().unwrap()["instructionz"] = json!("typo");
    let error = prepare_error(&provider, TOKEN).await;
    assert!(error.contains("instructionz"), "{error}");
}

#[tokio::test]
async fn a_bootstrap_for_another_version_fails_loudly() {
    let provider = Provider::new();
    provider.bootstrap.lock().unwrap()["version"] = json!("v0");
    let error = prepare_error(&provider, TOKEN).await;
    assert!(error.contains("version `v0`"), "{error}");
}

#[tokio::test]
async fn a_rejected_token_surfaces_the_status() {
    let provider = Provider::new();
    let error = prepare_error(&provider, "wrong-token").await;
    assert!(error.contains("401"), "{error}");
    assert!(
        !error.contains("wrong-token"),
        "the token never appears in errors"
    );
}

#[tokio::test]
async fn an_unknown_agent_fails_loudly() {
    let provider = Provider::new();
    let backend = serve(&provider, TOKEN).await;
    let error = backend
        .prepare("nobody", "customer-7")
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("not in server.toml"), "{error}");
}

#[tokio::test]
async fn an_unreachable_mcp_server_in_the_bootstrap_fails_loudly() {
    let provider = Provider::new();
    provider.bootstrap.lock().unwrap()["mcp_servers"] =
        json!([{ "name": "app", "url": "http://127.0.0.1:9/mcp" }]);
    let error = prepare_error(&provider, TOKEN).await;
    assert!(error.contains("MCP server `app`"), "{error}");
}

#[test]
fn the_provider_token_never_appears_in_debug_output() {
    let endpoint = ProviderEndpoint {
        base_url: "https://provider.test".into(),
        token: "super-secret-token".into(),
    };
    assert!(!format!("{endpoint:?}").contains("super-secret-token"));
}
