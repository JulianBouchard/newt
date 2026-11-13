use anyhow::{Context, bail};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

use crate::config::Database;

#[cfg(feature = "embedded-postgres")]
type Server = crate::embedded::Embedded;
#[cfg(not(feature = "embedded-postgres"))]
type Server = std::convert::Infallible;

/// The pool, and the embedded server behind it when newt runs its own.
/// Dropping it stops that server.
#[derive(Debug)]
pub struct Connected {
    pub pool: PgPool,
    _server: Option<Server>,
}

pub async fn connect_and_migrate(config: &Database) -> anyhow::Result<Connected> {
    let (url, server) = match (&config.url_env, &config.embedded) {
        (Some(url_env), None) => {
            let url = std::env::var(url_env)
                .with_context(|| format!("environment variable {url_env} is not set"))?;
            (url, None)
        }
        (None, Some(dir)) => {
            let (server, url) = start_embedded(dir).await?;
            (url, Some(server))
        }
        _ => bail!("database needs exactly one of url_env and embedded"),
    };
    let pool = PgPoolOptions::new()
        .max_connections(config.max_connections)
        .min_connections(config.max_connections)
        .connect(&url)
        .await
        .context("connecting to postgres")?;
    sqlx::migrate!("../../migrations")
        .run(&pool)
        .await
        .context("running migrations")?;
    Ok(Connected {
        pool,
        _server: server,
    })
}

#[cfg(feature = "embedded-postgres")]
async fn start_embedded(dir: &std::path::Path) -> anyhow::Result<(Server, String)> {
    crate::embedded::Embedded::start(dir).await
}

#[cfg(not(feature = "embedded-postgres"))]
fn start_embedded(_dir: &std::path::Path) -> std::future::Ready<anyhow::Result<(Server, String)>> {
    std::future::ready(Err(anyhow::anyhow!(
        "this newt was built without the embedded-postgres feature; set database.url_env"
    )))
}
