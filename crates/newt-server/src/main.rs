use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Context;
use newt_core::event::{Event, EventKind};
use newt_core::sink::{EventSink, JsonlSink};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

use newt_server::config::ServerConfig;
use newt_server::context::{ContextProviderBackend, ProviderEndpoint};
use newt_server::http::{self, AppState};
use newt_server::models::Routes;
use newt_server::runner::{Runner, RunnerConfig};
use newt_server::store::Store;
use newt_server::{db, live};

const EVENT_BUFFER: usize = 1024;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let started = Instant::now();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config_path =
        std::env::var_os("NEWT_CONFIG").map_or_else(|| PathBuf::from("server.toml"), PathBuf::from);
    let config = ServerConfig::load(&config_path)?;
    let missing = config.missing_secrets();
    if !missing.is_empty() {
        anyhow::bail!("unset environment variables: {}", missing.join(", "));
    }
    for (name, route) in &config.routes {
        tracing::info!(
            route = %name,
            protocol = ?route.protocol,
            base_url = %route.base_url,
            max_concurrent = route.max_concurrent,
            "route configured"
        );
    }

    let jsonl = config
        .events
        .jsonl
        .clone()
        .map(|path| JsonlSink::spawn(path, EVENT_BUFFER));

    let pool = db::connect_and_migrate(&config.database).await?;
    let listener = TcpListener::bind(config.server.listen)
        .await
        .with_context(|| format!("binding {}", config.server.listen))?;

    let boot_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    tracing::info!(
        boot_ms,
        listen = %config.server.listen,
        agents = config.agents.len(),
        routes = config.routes.len(),
        "newt ready"
    );
    if let Some((sink, _)) = &jsonl {
        sink.emit(Event::now(EventKind::ServerReady { boot_ms }));
    }

    let (tokens, endpoints) = agent_access(&config)?;
    let (live_topics, _listener_task) = live::spawn_listener(pool.clone())
        .await
        .context("subscribing to Postgres notifications")?;
    let instrumentation = jsonl
        .as_ref()
        .map(|(sink, _)| Arc::new(sink.clone()) as Arc<dyn EventSink>);
    let backend = ContextProviderBackend::new(endpoints, config.models.keys().cloned())?;
    let runner = Runner::new(
        Store::new(pool.clone()),
        backend,
        Routes::from_config(&config)?,
        RunnerConfig::default(),
        instrumentation,
        live_topics.clone(),
    );
    let loops = runner.spawn_loops();
    let shutdown = CancellationToken::new();
    let state = AppState {
        store: Store::new(pool),
        tokens: Arc::new(tokens),
        live: live_topics,
        shutdown: shutdown.clone(),
    };

    let stopping = runner.clone();
    axum::serve(listener, http::router(state))
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            shutdown.cancel();
            stopping.shutdown().await;
        })
        .await
        .context("serving http")?;
    drop(runner);
    loops.await.context("runner loops panicked")?;

    if let Some((sink, writer)) = jsonl {
        drop(sink);
        writer.await.context("event writer panicked")??;
    }
    Ok(())
}

type AgentAccess = (HashMap<String, String>, BTreeMap<String, ProviderEndpoint>);

fn agent_access(config: &ServerConfig) -> anyhow::Result<AgentAccess> {
    let mut tokens = HashMap::new();
    let mut endpoints = BTreeMap::new();
    for (name, agent) in &config.agents {
        let token = std::env::var(&agent.token_env)
            .with_context(|| format!("environment variable {} is not set", agent.token_env))?;
        if token.trim().is_empty() {
            anyhow::bail!("environment variable {} is empty", agent.token_env);
        }
        endpoints.insert(
            name.clone(),
            ProviderEndpoint {
                base_url: agent.context_provider.clone(),
                token: token.clone(),
            },
        );
        tokens.insert(name.clone(), token);
    }
    Ok((tokens, endpoints))
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::error!(%error, "could not listen for ctrl-c");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => {
                tracing::error!(%error, "could not listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    tracing::info!("shutdown signal received");
}
