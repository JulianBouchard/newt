#![expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use newt_loadtest::driver::{LoadPlan, NewtTarget, Target, run};
use newt_loadtest::fake_model::{self, FakeModel, FakeOptions};
use newt_loadtest::mock_app::{self, MockApp};
use newt_loadtest::profile::{Profile, REFERENCE_TIMING};
use newt_server::config::ServerConfig;
use newt_server::home::{HomeBackend, HomeServer};
use newt_server::http::{AppState, router};
use newt_server::models::Routes;
use newt_server::runner::{Runner, RunnerConfig};
use newt_server::store::Store;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

const TOKEN: &str = "loadtest-token";
const SPEED: f64 = 50.0;

async fn listen(router: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{address}")
}

#[sqlx::test(migrations = "../../migrations")]
async fn load_against_newt_completes_every_turn_without_a_real_model(pool: PgPool) {
    let model = FakeModel::new(
        REFERENCE_TIMING,
        FakeOptions {
            speed: SPEED,
            seed: 11,
            ..FakeOptions::default()
        },
    )
    .unwrap();
    let model_url = listen(fake_model::router(Arc::clone(&model))).await;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let app_url = format!("http://{}", listener.local_addr().unwrap());
    let app = MockApp::new(
        SPEED,
        12,
        0.0,
        false,
        30_000,
        Profile::default(),
        newt_loadtest::intervals::IntervalLog::default(),
    );
    tokio::spawn({
        let router = mock_app::router(Arc::clone(&app));
        async move { axum::serve(listener, router).await.unwrap() }
    });

    let config_path =
        std::env::temp_dir().join(format!("newt-loadtest-{}.toml", std::process::id()));
    std::fs::write(
        &config_path,
        format!(
            "server = {{ listen = \"127.0.0.1:0\" }}\ndatabase = {{ url_env = \"DATABASE_URL\" }}\n\
             [routes.fake]\nprotocol = \"openai-responses\"\nmodel = \"fake-model\"\nbase_url = \"{model_url}/v1\"\n\
             key_env = \"CARGO_MANIFEST_DIR\"\nmax_concurrent = 64\n[models.default]\nchain = [\"fake\"]\n"
        ),
    )
    .unwrap();
    let config = ServerConfig::load(&config_path).unwrap();
    let (live, _listener) = newt_server::live::spawn_listener(pool.clone())
        .await
        .unwrap();
    let runner = Runner::builder(
        Store::new(pool.clone()),
        HomeBackend::new(
            BTreeMap::from([(
                "assistant".to_owned(),
                HomeServer {
                    url: format!("{app_url}/mcp"),
                    token: TOKEN.into(),
                },
            )]),
            ["default".to_owned()],
        ),
        Routes::from_config(&config).unwrap(),
    )
    .config(RunnerConfig {
        max_sessions: 32,
        ..RunnerConfig::default()
    })
    .live(live.clone())
    .build();
    let _loops = runner.spawn_loops();
    let newt_url = listen(router(AppState {
        store: Store::new(pool),
        tokens: Arc::new(HashMap::from([("assistant".to_owned(), TOKEN.to_owned())])),
        live,
        shutdown: CancellationToken::new(),
        observers: newt_server::events::Observers::default(),
        live_meta: Arc::default(),
        live_stream: Arc::default(),
        metrics: Arc::default(),
    }))
    .await;

    let (samples, _) = run(
        Target::Newt(NewtTarget {
            base: newt_url,
            agent: "assistant".into(),
            token: TOKEN.into(),
        }),
        LoadPlan {
            sessions: 6,
            turns: 2,
            concurrency: 6,
            think: Duration::from_millis(10),
            think_max: Duration::ZERO,
            ramp: Duration::ZERO,
            timeout: Duration::from_secs(60),
            speed: 1.0,
        },
        "t",
    )
    .await;

    let failed: Vec<_> = samples.iter().filter(|sample| !sample.ok).collect();
    assert!(failed.is_empty(), "{failed:?}");
    assert_eq!(samples.len(), 12);
    assert!(samples.iter().all(|sample| sample.start_ms.is_some()));
    let calls = model.stats.calls.load(Ordering::Relaxed);
    assert!(
        calls >= 24,
        "each turn needs several model calls, saw {calls}"
    );
    assert!(
        app.tool_calls.load(Ordering::Relaxed) > 0,
        "the fake model calls mock tools"
    );
    std::fs::remove_file(config_path).unwrap();
}
