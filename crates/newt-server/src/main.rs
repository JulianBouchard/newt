use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Context;
use newt_core::sink::JsonlSink;
use newt_otlp::{Hooks, OtlpConfig, OtlpSink};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

use newt_server::config::{ServerConfig, parse_headers};
use newt_server::events::{Record, ServerEvent, ServerEventKind};
use newt_server::home::{HomeBackend, HomeServer};
use newt_server::http::{self, AppState};
use newt_server::models::Routes;
use newt_server::runner::{Runner, RunnerConfig};
use newt_server::store::Store;
use newt_server::{db, live};

const EVENT_BUFFER: usize = 1024;

#[tokio::main]
#[expect(clippy::too_many_lines, reason = "startup reads top to bottom")]
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

    let jsonl = config.events.jsonl.clone().map(|path| {
        let (sink, writer) = JsonlSink::spawn(path, EVENT_BUFFER);
        (
            sink.capturing_content(config.events.capture_content),
            writer,
        )
    });
    let otlp = otlp_sink(&config)?;

    let database = db::connect_and_migrate(&config.database).await?;
    let pool = database.pool.clone();
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
    if let Some((log, _)) = &jsonl {
        log.write(Record::Server(ServerEvent::now(
            ServerEventKind::ServerReady { boot_ms },
        )));
    }

    let access = agent_access(&config)?;
    let (live_topics, listener_task) = live::spawn_listener(pool.clone())
        .await
        .context("subscribing to Postgres notifications")?;
    let backend = HomeBackend::new(access.homes, config.models.keys().cloned());
    let mut runner = Runner::builder(
        Store::new(pool.clone()),
        backend,
        Routes::from_config(&config)?,
    )
    .config(RunnerConfig {
        max_sessions: config.runner.max_sessions,
        reserved_interactive: config.runner.reserved_interactive(),
        max_waiting: config.runner.max_waiting(),
        cache_key_buckets: config.runner.cache_key_buckets,
        retention: config.retention.retention(),
        ..RunnerConfig::default()
    })
    .live(live_topics.clone())
    .live_stream(access.live_stream.clone());
    if let Some((log, _)) = &jsonl {
        runner = runner.observe(Arc::new(log.clone()));
    }
    if let Some((traces, _)) = &otlp {
        runner = runner.observe(Arc::new(traces.clone()));
    }
    let runner = runner.build();
    let loops = runner.spawn_loops();
    let shutdown = CancellationToken::new();
    let state = AppState {
        store: Store::new(pool).with_max_pending(access.max_pending),
        tokens: Arc::new(access.tokens),
        live: live_topics,
        shutdown: shutdown.clone(),
        observers: runner.observers(),
        live_meta: Arc::new(access.live_meta),
        live_stream: Arc::new(access.live_stream),
        metrics: runner.metrics(),
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
    if let Some((sink, exporter)) = otlp {
        drop(sink);
        exporter.await.context("trace exporter panicked")?;
    }
    listener_task.abort();
    database.pool.close().await;
    drop(database);
    Ok(())
}

fn otlp_sink(config: &ServerConfig) -> anyhow::Result<Option<(OtlpSink, JoinHandle<()>)>> {
    let Some(tracing) = &config.tracing else {
        return Ok(None);
    };
    let headers = match &tracing.headers_env {
        Some(name) => parse_headers(
            &std::env::var(name)
                .with_context(|| format!("environment variable {name} is not set"))?,
        )
        .map_err(|error| anyhow::anyhow!("{name}: {error}"))?,
        None => Vec::new(),
    };
    let otlp = OtlpConfig {
        endpoint: tracing.endpoint.clone(),
        headers,
        profile: tracing.profile,
        capture_content: tracing.capture_content,
        service_name: tracing.service_name.clone(),
    };
    tracing::info!(endpoint = %otlp.endpoint, profile = ?otlp.profile, "exporting traces");
    Ok(Some(OtlpSink::spawn(otlp, Hooks::default(), EVENT_BUFFER)))
}

struct AgentAccess {
    tokens: HashMap<String, String>,
    homes: BTreeMap<String, HomeServer>,
    live_meta: HashSet<String>,
    live_stream: HashSet<String>,
    max_pending: HashMap<String, usize>,
}

fn agent_access(config: &ServerConfig) -> anyhow::Result<AgentAccess> {
    let mut access = AgentAccess {
        tokens: HashMap::new(),
        homes: BTreeMap::new(),
        live_meta: HashSet::new(),
        live_stream: HashSet::new(),
        max_pending: HashMap::new(),
    };
    for (name, agent) in &config.agents {
        let token = std::env::var(&agent.token_env)
            .with_context(|| format!("environment variable {} is not set", agent.token_env))?;
        if token.trim().is_empty() {
            anyhow::bail!("environment variable {} is empty", agent.token_env);
        }
        access.homes.insert(
            name.clone(),
            HomeServer {
                url: agent.mcp.clone(),
                token: token.clone(),
            },
        );
        if agent.live_meta {
            access.live_meta.insert(name.clone());
        }
        if agent.live_stream {
            access.live_stream.insert(name.clone());
        }
        access
            .max_pending
            .insert(name.clone(), agent.max_pending_events);
        access.tokens.insert(name.clone(), token);
    }
    Ok(access)
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
