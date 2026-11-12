use std::collections::BTreeMap;
use std::io::Write;

use newt_core::event::Event;
use newt_core::message::Message;
use newt_core::prompt::Effort;
use newt_core::provider::{Protocol, ProviderClient};
use newt_core::sink::EventSink;
use newt_core::tools::NoTools;
use newt_core::turn::{TurnInput, TurnSettings, run_turn};
use newt_core::workspace::Workspace;
use tokio_util::sync::CancellationToken;

const SYSTEM: &str = "You are a concise personal assistant. Read the workspace before answering. \
You cannot message the person directly in this demo: answer in plain text, then call end_turn.";

struct Quiet;

impl EventSink for Quiet {
    fn emit(&self, _event: Event) {}
}

fn env(name: &str) -> Result<String, String> {
    std::env::var(name).map_err(|_| format!("set {name}"))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let protocol = match env("NEWT_PROTOCOL")?.as_str() {
        "anthropic" => Protocol::AnthropicMessages,
        "openai" => Protocol::OpenaiResponses,
        other => {
            return Err(format!("NEWT_PROTOCOL must be anthropic or openai, not {other}").into());
        }
    };
    let client = ProviderClient::new(protocol, env("NEWT_BASE_URL")?, env("NEWT_API_KEY")?)?;
    let model = env("NEWT_MODEL")?;
    let system = match std::env::var("NEWT_SYSTEM_FILE") {
        Ok(path) => std::fs::read_to_string(path)?,
        Err(_) => SYSTEM.to_owned(),
    };
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
    };
    let mut out = std::io::stdout().lock();
    let mut history: Vec<Message> = Vec::new();
    let asks = [
        "What do you know about me? One sentence.",
        "Would a 30-person remote fintech in Berlin fit me? One sentence.",
    ];

    for (index, ask) in asks.into_iter().enumerate() {
        let turn = format!("turn-{}", index + 1);
        let outcome = run_turn(
            &client,
            &NoTools,
            TurnInput {
                session: "demo-session",
                turn: &turn,
                system: &system,
                external_tools: &[],
                history: &history,
                events: vec![Message::user_text(ask)],
                workspace: &workspace,
            },
            &settings,
            &Quiet,
            &CancellationToken::new(),
        )
        .await?;

        writeln!(out, "\n{turn}: {ask}")?;
        for (call, report) in outcome.calls.iter().enumerate() {
            writeln!(
                out,
                "  call {call}: input {} (cache hit {:.0}%), output {}, first output {:?} ms, total {} ms",
                report.usage.input_tokens,
                report.usage.cache_hit_ratio() * 100.0,
                report.usage.output_tokens,
                report.timing.first_output_ms,
                report.timing.total_ms,
            )?;
        }
        writeln!(
            out,
            "  committed {} messages to history",
            outcome.committed.len()
        )?;
        history.extend(outcome.committed);
    }
    Ok(())
}
