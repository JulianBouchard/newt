use anyhow::Context;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let token = std::env::var("ASSISTANT_TOKEN")
        .context("set ASSISTANT_TOKEN, the token newt also uses for this agent")?;
    let listen = std::env::var("NEWT_DEMO_LISTEN").unwrap_or_else(|_| "127.0.0.1:4000".to_owned());
    let public_url = std::env::var("NEWT_DEMO_URL").unwrap_or_else(|_| format!("http://{listen}"));
    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    tracing::info!(%listen, "demo assistant ready: context provider at /assistant, tools at /mcp");
    axum::serve(
        listener,
        newt_demo::app(newt_demo::Demo::new(token, &public_url)),
    )
    .with_graceful_shutdown(async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::error!(%error, "could not listen for ctrl-c");
        }
    })
    .await
    .context("serving")
}
