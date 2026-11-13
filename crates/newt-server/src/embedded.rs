//! A Postgres server newt runs itself, for installs with no database to
//! point at: `database = { embedded = "<dir>" }`.
//!
//! The first start downloads the Postgres binaries for this platform (checked
//! against their published hash) into `~/.theseus/postgresql`, which every
//! later start reuses, and creates the database cluster in `<dir>/data`. Each
//! start runs the server on a free local port and stops it when newt stops. A
//! server left running by a newt that was killed is stopped first.

use std::path::Path;
use std::time::Duration;

use anyhow::Context;
use postgresql_embedded::{PostgreSQL, Settings, Status, VersionReq};

const VERSION: &str = "=18.2.0";
const DATABASE: &str = "newt";
const COMMAND_TIMEOUT: Duration = Duration::from_mins(1);

/// The running server. Dropping it stops the server.
#[derive(Debug)]
pub struct Embedded {
    _postgres: PostgreSQL,
}

impl Embedded {
    /// Starts the server in `dir` and returns it with the URL of newt's
    /// database on it.
    pub async fn start(dir: &Path) -> anyhow::Result<(Self, String)> {
        std::fs::create_dir_all(dir).with_context(|| {
            format!("creating the embedded database directory {}", dir.display())
        })?;
        let dir = dir.canonicalize()?;
        let mut postgres = PostgreSQL::new(settings(&dir)?);
        if postgres.status() == Status::Started
            && let Err(error) = postgres.stop().await
        {
            tracing::warn!(%error, "could not stop a server left from an earlier run; starting anyway");
        }
        postgres
            .setup()
            .await
            .context("installing and initializing the embedded Postgres")?;
        postgres
            .start()
            .await
            .context("starting the embedded Postgres")?;
        if !postgres.database_exists(DATABASE).await? {
            postgres.create_database(DATABASE).await?;
        }
        let url = postgres.settings().url(DATABASE);
        tracing::info!(
            dir = %dir.display(),
            port = postgres.settings().port,
            version = %postgres.settings().version,
            "embedded postgres ready"
        );
        Ok((
            Self {
                _postgres: postgres,
            },
            url,
        ))
    }
}

fn settings(dir: &Path) -> anyhow::Result<Settings> {
    let mut settings = Settings::new();
    remove_empty_dir(&settings.data_dir);
    if let Some(parent) = settings.password_file.parent() {
        remove_empty_dir(parent);
    }
    settings.version = VersionReq::parse(VERSION)?;
    settings.data_dir = dir.join("data");
    settings.password_file = dir.join("password");
    if settings.password_file.exists() {
        std::fs::read_to_string(&settings.password_file)?
            .trim()
            .clone_into(&mut settings.password);
    }
    settings.temporary = false;
    settings.timeout = Some(COMMAND_TIMEOUT);
    Ok(settings)
}

/// `Settings::new` creates two empty temporary directories this module never
/// uses.
fn remove_empty_dir(dir: &Path) {
    if let Err(error) = std::fs::remove_dir(dir) {
        tracing::debug!(%error, dir = %dir.display(), "could not remove an unused temporary directory");
    }
}
