use anyhow::Context;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

use crate::config::Database;

pub async fn connect_and_migrate(config: &Database) -> anyhow::Result<PgPool> {
    let url = std::env::var(&config.url_env)
        .with_context(|| format!("environment variable {} is not set", config.url_env))?;
    let pool = PgPoolOptions::new()
        .max_connections(config.max_connections)
        .connect(&url)
        .await
        .context("connecting to postgres")?;
    sqlx::migrate!("../../migrations")
        .run(&pool)
        .await
        .context("running migrations")?;
    Ok(pool)
}
