use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Context;
use newt_core::event::{Event, EventKind};
use newt_core::otlp::{Hooks, OtlpConfig, OtlpSink, parse_headers};
use newt_core::sink::{EventSink, JsonlSink, MultiSink, SinkError};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

use newt_server::config::ServerConfig;
use newt_server::context::{ContextProviderBackend, McpAgent, ProviderEndpoint};
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
    if let Some((sink, _)) = &jsonl {
        sink.emit(Event::now(EventKind::ServerReady { boot_ms }));
    }

    let access = agent_access(&config)?;
    let (live_topics, listener_task) = live::spawn_listener(pool.clone())
        .await
        .context("subscribing to Postgres notifications")?;
    let instrumentation = instrumentation(jsonl.as_ref(), otlp.as_ref());
    let http_instrumentation = instrumentation.clone();
    let backend = ContextProviderBackend::new(access.providers, config.models.keys().cloned())?
        .with_mcp_agents(access.mcp);
    let runner = Runner::new(
        Store::new(pool.clone()),
        backend,
        Routes::from_config(&config)?,
        RunnerConfig {
            max_sessions: config.runner.max_sessions,
            cache_key_buckets: config.runner.cache_key_buckets,
            ..RunnerConfig::default()
        },
        instrumentation,
        live_topics.clone(),
    );
    let loops = runner.spawn_loops();
    let shutdown = CancellationToken::new();
    let state = AppState {
        store: Store::new(pool),
        tokens: Arc::new(access.tokens),
        live: live_topics,
        shutdown: shutdown.clone(),
        instrumentation: http_instrumentation,
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

fn instrumentation(
    jsonl: Option<&(JsonlSink, JoinHandle<Result<(), SinkError>>)>,
    otlp: Option<&(OtlpSink, JoinHandle<()>)>,
) -> Option<Arc<dyn EventSink>> {
    let mut sinks: Vec<Arc<dyn EventSink>> = Vec::new();
    if let Some((sink, _)) = jsonl {
        sinks.push(Arc::new(sink.clone()));
    }
    if let Some((sink, _)) = otlp {
        sinks.push(Arc::new(sink.clone()));
    }
    (!sinks.is_empty()).then(|| Arc::new(MultiSink(sinks)) as Arc<dyn EventSink>)
}

struct AgentAccess {
    tokens: HashMap<String, String>,
    providers: BTreeMap<String, ProviderEndpoint>,
    mcp: BTreeMap<String, McpAgent>,
}

fn agent_access(config: &ServerConfig) -> anyhow::Result<AgentAccess> {
    let mut access = AgentAccess {
        tokens: HashMap::new(),
        providers: BTreeMap::new(),
        mcp: BTreeMap::new(),
    };
    for (name, agent) in &config.agents {
        let token = std::env::var(&agent.token_env)
            .with_context(|| format!("environment variable {} is not set", agent.token_env))?;
        if token.trim().is_empty() {
            anyhow::bail!("environment variable {} is empty", agent.token_env);
        }
        match (&agent.context_provider, &agent.mcp, &agent.model) {
            (Some(base_url), _, _) => {
                access.providers.insert(
                    name.clone(),
                    ProviderEndpoint {
                        base_url: base_url.clone(),
                        token: token.clone(),
                    },
                );
            }
            (None, Some(url), Some(model)) => {
                access.mcp.insert(
                    name.clone(),
                    McpAgent {
                        url: url.clone(),
                        model: model.clone(),
                        token: token.clone(),
                    },
                );
            }
            _ => anyhow::bail!("agent `{name}` needs exactly one of context_provider and mcp"),
        }
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
