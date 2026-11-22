use std::collections::HashMap;
use std::io::Write as _;
use std::time::Duration;

use anyhow::{Context, bail};
use newt_loadtest::cache::CacheScope;
use newt_loadtest::driver::{LoadPlan, NewtTarget, Target, run};
use newt_loadtest::eve::EveTarget;
use newt_loadtest::fake_model::{self, FakeModel, FakeOptions};
use newt_loadtest::mock_app::{self, MockApp};
use newt_loadtest::profile::{Profile, REFERENCE_TIMING};
use serde_json::Value;
use tracing_subscriber::EnvFilter;

const USAGE: &str = "usage:
  newt-loadtest fake-model [--listen 127.0.0.1:9100] [--speed 1] [--failure-rate 0] [--seed 1]
                           [--cache-scope key|prefix|routed] [--machines 4]
                           [--dump-dir DIR] [--turn-marker TEXT] [--finish reply-then-end|reply-with-end|end-only] [--decision-log FILE] [--cache-log FILE] [--interval-log FILE] [--replay TURNS.jsonl] [--profile FILE]
  newt-loadtest mock-app   [--listen 127.0.0.1:9200] [--speed 1] [--seed 2] [--render-ms 0] [--reply-ends-turn false] [--prompt-bytes 30000] [--interval-log FILE] [--profile FILE]
                           [--stdio true]   serve the agent over stdin and stdout instead
  newt-loadtest run [--runtime newt|eve] --base http://127.0.0.1:7878 --agent assistant --token-env TOKEN
                    [--sessions 20] [--turns 3] [--concurrency 20] [--think-ms 2000] [--think-max-ms THINK] [--ramp-s 0] [--timeout-s 300]
                    [--fake http://127.0.0.1:9100] [--mock http://127.0.0.1:9200] [--speed 1] [--turns-log FILE]
  newt-loadtest decompose --turns FILE --intervals FILE,FILE --speed S [--replay TURNS.jsonl]";

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
    let base = flags
        .text("base", "http://127.0.0.1:7878")
        .trim_end_matches('/')
        .to_owned();
    let token = std::env::var(&token_env).with_context(|| format!("set {token_env}"))?;
    let target = match flags.text("runtime", "newt").as_str() {
        "newt" => Target::Newt(NewtTarget {
            base,
            agent: flags.text("agent", "assistant"),
            token,
        }),
        "eve" => Target::Eve(EveTarget::new(base, token)),
        other => bail!("--runtime must be newt or eve, not `{other}`"),
    };
    let plan = LoadPlan {
        sessions: flags.number("sessions", 20)?,
        turns: flags.number("turns", 3)?,
        concurrency: flags.number("concurrency", 20)?,
        think: Duration::from_millis(flags.number("think-ms", 2000)?),
        think_max: Duration::from_millis(flags.number("think-max-ms", 0)?),
        ramp: Duration::from_secs(flags.number("ramp-s", 0)?),
        timeout: Duration::from_secs(flags.number("timeout-s", 300)?),
        speed: flags.number("speed", 1.0)?,
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
    if let Some(path) = flags.0.get("turns-log") {
        newt_loadtest::decompose::write_turns(path, &samples)?;
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
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
                    finish: match flags.text("finish", "reply-then-end").as_str() {
                        "end-only" => fake_model::Finish::EndOnly,
                        "reply-then-end" => fake_model::Finish::ReplyThenEnd,
                        "reply-with-end" => fake_model::Finish::ReplyWithEnd,
                        other => bail!(
                            "--finish must be end-only, reply-then-end or reply-with-end, not `{other}`"
                        ),
                    },
                    interval_log: flags.0.get("interval-log").map(std::path::PathBuf::from),
                    replay: flags.0.get("replay").map(std::path::PathBuf::from),
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
            let app = MockApp::new(
                flags.number("speed", 1.0)?,
                flags.number("seed", 2)?,
                flags.number("render-ms", 0.0)?,
                flags.text("reply-ends-turn", "false") == "true",
                flags.number("prompt-bytes", 30_000)?,
                profile(&flags)?,
                newt_loadtest::intervals::IntervalLog::open(
                    flags.0.get("interval-log").map(std::path::Path::new),
                )?,
            );
            if flags.text("stdio", "false") == "true" {
                return mock_app::serve_stdio(app).await;
            }
            serve(
                &flags.text("listen", "127.0.0.1:9200"),
                mock_app::router(app),
            )
            .await
        }
        "run" => run_load(&flags).await,
        "decompose" => {
            let intervals: Vec<String> = flags
                .text("intervals", "")
                .split(',')
                .filter(|path| !path.is_empty())
                .map(str::to_owned)
                .collect();
            let replay = flags
                .0
                .get("replay")
                .map(|path| newt_loadtest::replay::Replay::load(path))
                .transpose()?;
            let report = newt_loadtest::decompose::render(
                &flags.text("turns", ""),
                &intervals,
                flags.number("speed", 1.0)?,
                replay.as_ref(),
            )?;
            std::io::stdout().lock().write_all(report.as_bytes())?;
            Ok(())
        }
        other => bail!("unknown command `{other}`\n{USAGE}"),
    }
}
