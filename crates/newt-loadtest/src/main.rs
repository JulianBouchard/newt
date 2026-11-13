use std::collections::HashMap;
use std::io::Write as _;
use std::time::Duration;

use anyhow::{Context, bail};
use newt_loadtest::cache::CacheScope;
use newt_loadtest::driver::{LoadPlan, NewtTarget, run};
use newt_loadtest::fake_model::{self, FakeModel, FakeOptions};
use newt_loadtest::mock_app::{self, MockApp};
use newt_loadtest::profile::{Profile, REFERENCE_TIMING};
use serde_json::Value;
use tracing_subscriber::EnvFilter;

const USAGE: &str = "usage:
  newt-loadtest fake-model [--listen 127.0.0.1:9100] [--speed 1] [--failure-rate 0] [--seed 1]
                           [--cache-scope key|prefix|routed] [--machines 4]
                           [--dump-dir DIR] [--turn-marker TEXT] [--decision-log FILE] [--cache-log FILE] [--profile FILE]
  newt-loadtest mock-app   [--listen 127.0.0.1:9200] [--speed 1] [--seed 2] [--render-ms 0] [--lazy-pages false] [--profile FILE]
  newt-loadtest run --base http://127.0.0.1:7878 --agent assistant --token-env TOKEN
                    [--sessions 20] [--turns 3] [--concurrency 20] [--think-ms 2000] [--think-max-ms THINK] [--ramp-s 0] [--timeout-s 300]
                    [--fake http://127.0.0.1:9100] [--mock http://127.0.0.1:9200]";

struct Flags(HashMap<String, String>);

impl Flags {
    fn parse(args: &[String]) -> anyhow::Result<Self> {
        let mut flags = HashMap::new();
        let mut rest = args.iter();
        while let Some(name) = rest.next() {
            let Some(name) = name.strip_prefix("--") else {
                bail!("unexpected argument `{name}`\n{USAGE}");
            };
            let value = rest
                .next()
                .with_context(|| format!("--{name} needs a value"))?;
            flags.insert(name.to_owned(), value.clone());
        }
        Ok(Self(flags))
    }

    fn text(&self, name: &str, default: &str) -> String {
        self.0
            .get(name)
            .cloned()
            .unwrap_or_else(|| default.to_owned())
    }

    fn number<T: std::str::FromStr>(&self, name: &str, default: T) -> anyhow::Result<T> {
        self.0.get(name).map_or(Ok(default), |value| {
            value
                .parse()
                .map_err(|_| anyhow::anyhow!("--{name} must be a number"))
        })
    }
}

fn profile(flags: &Flags) -> anyhow::Result<Profile> {
    let Some(path) = flags.0.get("profile") else {
        return Ok(Profile::default());
    };
    let raw = std::fs::read_to_string(path).with_context(|| format!("reading profile {path}"))?;
    Profile::parse(&raw).with_context(|| format!("invalid profile {path}"))
}

fn cache_scope(flags: &Flags) -> anyhow::Result<CacheScope> {
    Ok(match flags.text("cache-scope", "key").as_str() {
        "key" => CacheScope::Key,
        "prefix" => CacheScope::Prefix,
        "routed" => CacheScope::Routed {
            machines: flags.number("machines", 4)?,
        },
        other => bail!("--cache-scope must be key, prefix or routed, not `{other}`"),
    })
}

async fn serve(listen: &str, router: axum::Router) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    tracing::info!(%listen, "listening");
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            if let Err(error) = tokio::signal::ctrl_c().await {
                tracing::error!(%error, "could not listen for ctrl-c");
            }
        })
        .await
        .context("serving")
}

async fn stats(http: &reqwest::Client, base: Option<&String>) -> Value {
    let Some(base) = base else {
        return Value::Null;
    };
    match http.get(format!("{base}/stats")).send().await {
        Ok(response) => response.json().await.unwrap_or(Value::Null),
        Err(_) => Value::Null,
    }
}

async fn run_load(flags: &Flags) -> anyhow::Result<()> {
    let token_env = flags.text("token-env", "LOADTEST_TOKEN");
    let target = NewtTarget {
        base: flags
            .text("base", "http://127.0.0.1:7878")
            .trim_end_matches('/')
            .to_owned(),
        agent: flags.text("agent", "assistant"),
        token: std::env::var(&token_env).with_context(|| format!("set {token_env}"))?,
    };
    let plan = LoadPlan {
        sessions: flags.number("sessions", 20)?,
        turns: flags.number("turns", 3)?,
        concurrency: flags.number("concurrency", 20)?,
        think: Duration::from_millis(flags.number("think-ms", 2000)?),
        think_max: Duration::from_millis(flags.number("think-max-ms", 0)?),
        ramp: Duration::from_secs(flags.number("ramp-s", 0)?),
        timeout: Duration::from_secs(flags.number("timeout-s", 300)?),
    };
    let fake = flags.0.get("fake");
    let mock = flags.0.get("mock");
    let http = reqwest::Client::new();
    let (model_before, app_before) = (stats(&http, fake).await, stats(&http, mock).await);
    let run_id = format!("load-{}", std::process::id());
    let (samples, wall) = run(target, plan, &run_id).await;
    let (model_after, app_after) = (stats(&http, fake).await, stats(&http, mock).await);
    let report = newt_loadtest::report::render(
        &samples,
        wall,
        (&model_before, &model_after),
        (&app_before, &app_after),
    );
    std::io::stdout().lock().write_all(report.as_bytes())?;
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some((command, rest)) = args.split_first() else {
        bail!("{USAGE}");
    };
    let flags = Flags::parse(rest)?;
    match command.as_str() {
        "fake-model" => {
            let model = FakeModel::new(
                REFERENCE_TIMING,
                FakeOptions {
                    speed: flags.number("speed", 1.0)?,
                    failure_rate: flags.number("failure-rate", 0.0)?,
                    seed: flags.number("seed", 1)?,
                    cache_scope: cache_scope(&flags)?,
                    dump_dir: flags.0.get("dump-dir").map(std::path::PathBuf::from),
                    turn_marker: flags.0.get("turn-marker").cloned(),
                    decision_log: flags.0.get("decision-log").map(std::path::PathBuf::from),
                    cache_log: flags.0.get("cache-log").map(std::path::PathBuf::from),
                    profile: profile(&flags)?,
                },
            )?;
            serve(
                &flags.text("listen", "127.0.0.1:9100"),
                fake_model::router(model),
            )
            .await
        }
        "mock-app" => {
            let listen = flags.text("listen", "127.0.0.1:9200");
            let app = MockApp::new(
                &format!("http://{listen}"),
                flags.number("speed", 1.0)?,
                flags.number("seed", 2)?,
                flags.number("render-ms", 0.0)?,
                flags.text("lazy-pages", "false") == "true",
                profile(&flags)?,
            );
            serve(&listen, mock_app::router(app)).await
        }
        "run" => run_load(&flags).await,
        other => bail!("unknown command `{other}`\n{USAGE}"),
    }
}
