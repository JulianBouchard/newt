use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;

use newt_core::journal::NoJournal;
use newt_core::message::Message;
use newt_core::prompt::{Effort, Lane};
use newt_core::provider::{Protocol, ProviderClient};
use std::sync::Arc;

use newt_core::otlp::{Hooks, OtlpConfig, OtlpSink, Profile, parse_headers};
use newt_core::sink::{EventSink, JsonlSink, MultiSink};
use newt_core::steering::NoSteering;
use newt_core::tools::NoTools;
use newt_core::turn::{TurnDeps, TurnInput, TurnSettings, run_turn};
use newt_core::workspace::Workspace;
use tokio_util::sync::CancellationToken;

const SYSTEM: &str = "You are a concise personal assistant. Read the workspace before answering. \
You cannot message the person directly in this demo: answer in plain text, then call end_turn.";

type Exporter = (OtlpSink, tokio::task::JoinHandle<()>);

fn otlp_from_env() -> Result<Option<Exporter>, Box<dyn std::error::Error>> {
    let Ok(endpoint) = std::env::var("NEWT_OTLP_ENDPOINT") else {
        return Ok(None);
    };
    let headers = parse_headers(&std::env::var("NEWT_OTLP_HEADERS").unwrap_or_default())?;
    let profile = match std::env::var("NEWT_OTLP_PROFILE").as_deref() {
        Ok("langfuse") => Profile::Langfuse,
        _ => Profile::Generic,
    };
    Ok(Some(OtlpSink::spawn(
        OtlpConfig {
            endpoint,
            headers,
            profile,
            capture_content: true,
            service_name: "newt-example".to_owned(),
        },
        Hooks::default(),
        1024,
    )))
}

fn env(name: &str) -> Result<String, String> {
    std::env::var(name).map_err(|_| format!("set {name}"))
}

#[tokio::main]
#[expect(clippy::too_many_lines, reason = "an example reads top to bottom")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let protocol = match env("NEWT_PROTOCOL")?.as_str() {
        "anthropic" => Protocol::AnthropicMessages,
        "openai" => Protocol::OpenaiResponses,
        other => {
            return Err(format!("NEWT_PROTOCOL must be anthropic or openai, not {other}").into());
        }
    };
    let mut client = ProviderClient::new(protocol, env("NEWT_BASE_URL")?, env("NEWT_API_KEY")?)?;
    if let Ok(header) = std::env::var("NEWT_AUTH_HEADER") {
        client = client.with_api_key_header(header);
    }
    if let Ok(dir) = std::env::var("NEWT_CAPTURE_DIR") {
        client = client.with_capture_dir(dir);
    }
    let model = env("NEWT_MODEL")?;
    let system = match std::env::var("NEWT_SYSTEM_FILE") {
        Ok(path) => std::fs::read_to_string(path)?,
        Err(_) => SYSTEM.to_owned(),
    };
    let events_path = std::env::var("NEWT_EVENTS_FILE")
        .map_or_else(|_| PathBuf::from("newt-events.jsonl"), PathBuf::from);
    let (jsonl, writer) = JsonlSink::spawn(events_path.clone(), 1024);
    let jsonl = jsonl.capturing_content(true);
    let mut sinks: Vec<Arc<dyn EventSink>> = vec![Arc::new(jsonl.clone())];
    let otlp = otlp_from_env()?;
    if let Some((exporter_sink, _)) = &otlp {
        sinks.push(Arc::new(exporter_sink.clone()));
    }
    let sink = MultiSink(sinks);
    let session = std::env::var("NEWT_SESSION").unwrap_or_else(|_| "demo-session".to_owned());

    let workspace = Workspace::new(BTreeMap::from([
        (
            "profile.md".to_owned(),
            "# Profile\nStaff backend engineer, 9 years, payments and ledgers.\nBased in Lisbon."
                .to_owned(),
        ),
        (
            "preferences.md".to_owned(),
            "# Preferences\nRemote only. No crypto. Wants a team under 50 people.".to_owned(),
        ),
    ]));
    let settings = TurnSettings {
        model: &model,
        first_call_effort: Some(Effort::Low),
        effort: Some(Effort::Medium),
        max_output_tokens: 4096,
        max_model_calls: 6,
        max_turn_tokens: None,
        recite: true,
        require_end_turn: false,
        lane: Lane::Interactive,
        reset: false,
        compaction: None,
    };
    let mut history: Vec<Message> = Vec::new();
    let asks = [
        "What do you know about me? One sentence.",
        "Would a 30-person remote fintech in Berlin fit me? One sentence.",
    ];

    for (index, ask) in asks.into_iter().enumerate() {
        let turn = format!("turn-{}", index + 1);
        let outcome = run_turn(
            &TurnDeps {
                model: &client,
                host: &NoTools,
                steering: &NoSteering,
                journal: &NoJournal,
                sink: &sink,
                cancel: &CancellationToken::new(),
            },
            TurnInput {
                session: &session,
                cache_key: &session,
                turn: &turn,
                system: &system,
                external_tools: &[],
                history: &history,
                context: None,
                skills: &[],
                events: vec![Message::user_text(ask)],
                workspace: &workspace,
                replay: Vec::new(),
            },
            &settings,
        )
        .await?;
        history.extend(outcome.committed);
    }

    let dropped = jsonl.dropped();
    drop(sink);
    drop(jsonl);
    writer.await??;
    if let Some((exporter_sink, exporter)) = otlp {
        drop(exporter_sink);
        exporter.await?;
    }
    writeln!(
        std::io::stdout().lock(),
        "events written to {} ({dropped} dropped); run `cargo run -q -p newt-server --bin newt-report -- {}`",
        events_path.display(),
        events_path.display()
    )?;
    Ok(())
}
