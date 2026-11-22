//! Measures recitation: the same long turns run with and without the goal
//! note, against a real model, graded by deterministic checks.
//!
//! Each case is a chain of records the model must read one by one (each
//! names the next), so the chain length sets how many model calls the turn
//! takes, and filler text sets how long each tool result is. The person's
//! message asks for one reply with four checkable properties. Cases are
//! seeded, so both arms see the same cases.
//!
//! ```text
//! NEWT_PROTOCOL=openai NEWT_BASE_URL=... NEWT_API_KEY=... NEWT_MODEL=... \
//! EVAL_OUT=results.jsonl EVAL_CASES=10 EVAL_LENGTHS=5,11,23 EVAL_SIZES=short,long \
//! EVAL_PARALLEL=3 cargo run --release -p newt-core --example recitation_eval
//! ```

use std::collections::BTreeSet;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use newt_core::journal::NoJournal;
use newt_core::message::{Message, Part, Role, ToolCall};
use newt_core::prompt::{Effort, Lane, ToolSpec};
use newt_core::provider::{Protocol, ProviderClient};
use newt_core::sink::NoSink;
use newt_core::steering::NoSteering;
use newt_core::tools::{CallContext, Retention, ToolDef, ToolHost, ToolOutput};
use newt_core::turn::{TurnDeps, TurnInput, TurnSettings, run_turn};
use newt_core::workspace::Workspace;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const WORD_LIMIT: usize = 80;
const SYSTEM: &str = "You are a careful research assistant. Read records with get_record. \
When you have what the person asked for, send your answer with the reply tool exactly once, \
then call end_turn.";
const NAMES: [&str; 24] = [
    "Fernhill",
    "Quarry Labs",
    "Brightwater",
    "Okapi",
    "Saltmarsh",
    "Tessellate",
    "Rookery",
    "Lanternfish",
    "Copperleaf",
    "Halyard",
    "Mistral Works",
    "Northpine",
    "Oriel",
    "Pebblestone",
    "Quillon",
    "Redwing",
    "Sablefin",
    "Thornbury",
    "Umbra",
    "Vellum",
    "Wickerline",
    "Yarrow",
    "Zephyrine",
    "Ashgrove",
];
const CITIES: [&str; 6] = ["Lisbon", "Berlin", "Austin", "Toronto", "Warsaw", "Dublin"];
const FILLER: [&str; 8] = [
    "The team reviews its roadmap every quarter and publishes notes internally.",
    "Engineers rotate through support for one week each cycle.",
    "Most services run on managed Postgres with a queue in front of writes.",
    "Hiring loops have four stages, the last a conversation with a founder.",
    "The office keeps a small library, and books are expensed without approval.",
    "Deploys happen several times a day behind feature flags.",
    "On-call pays a stipend and is shared across two time zones.",
    "The company held an offsite last spring and plans another next year.",
];

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: usize) -> usize {
        usize::try_from(self.next() % bound as u64).unwrap_or(0)
    }
}

#[derive(Debug, Clone)]
struct Record {
    id: String,
    name: String,
    remote: bool,
    salary: u32,
    text: String,
}

#[derive(Debug, Clone)]
struct Case {
    records: Vec<Record>,
    ask: String,
}

fn case(seed: u64, length: usize, long: bool) -> Case {
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let mut names: Vec<&str> = NAMES.to_vec();
    let mut records = Vec::with_capacity(length);
    for index in 0..length {
        let name = names.remove(rng.below(names.len())).to_owned();
        let remote = rng.below(10) < 4;
        let salary = 120 + u32::try_from(rng.below(30)).unwrap_or(0) * 5;
        let paragraphs = if long { 60 } else { 4 };
        let filler: Vec<&str> = (0..paragraphs)
            .map(|_| FILLER[rng.below(FILLER.len())])
            .collect();
        let next = if index + 1 < length {
            format!("Next record: R{}.", index + 2)
        } else {
            "This is the last record.".to_owned()
        };
        let text = format!(
            "Record R{} — {name}\nCity: {}\nWork setup: {}\nSalary band: up to ${salary}k\nTeam size: {}\n\n{}\n\n{next}",
            index + 1,
            CITIES[rng.below(CITIES.len())],
            if remote { "fully remote" } else { "on-site" },
            8 + rng.below(80),
            filler.join(" "),
        );
        records.push(Record {
            id: format!("R{}", index + 1),
            name,
            remote,
            salary,
            text,
        });
    }
    if !records.iter().any(|record| record.remote) {
        let first = &mut records[0];
        first.remote = true;
        first.text = first.text.replace("on-site", "fully remote");
    }
    let ask = format!(
        "I'm comparing companies. Start with record R1 and read every record in the chain; each one \
         names the next. Then send me one reply that (1) names every company that offers remote work, \
         (2) gives the highest salary band among those remote companies, (3) stays under {WORD_LIMIT} \
         words, and (4) ends with a question to me."
    );
    Case { records, ask }
}

struct Host {
    records: Vec<Record>,
    visited: Mutex<BTreeSet<String>>,
    replies: Mutex<Vec<String>>,
}

impl ToolHost for Host {
    fn call(
        &self,
        call: &ToolCall,
        _context: &CallContext,
    ) -> impl std::future::Future<Output = ToolOutput> + Send {
        std::future::ready(self.answer(call))
    }
}

impl Host {
    fn answer(&self, call: &ToolCall) -> ToolOutput {
        match call.name.as_str() {
            "get_record" => {
                let id = call.arguments["id"].as_str().unwrap_or_default();
                match self.records.iter().find(|record| record.id == id) {
                    Some(record) => {
                        lock(&self.visited).insert(record.id.clone());
                        ToolOutput::ok(record.text.clone())
                    }
                    None => ToolOutput::error(format!(
                        "No record {id}. Records are R1 to R{}.",
                        self.records.len()
                    )),
                }
            }
            "reply" => {
                let text = call.arguments["text"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                lock(&self.replies).push(text);
                ToolOutput::ok("Sent.")
            }
            other => ToolOutput::error(format!("No tool {other}.")),
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn tools() -> Vec<ToolDef> {
    vec![
        ToolDef::new(
            ToolSpec {
                name: "get_record".into(),
                description: "Read one record by id, such as R1.".into(),
                input_schema: json!({"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]}),
            },
            Retention::Ephemeral,
        ),
        ToolDef::new(
            ToolSpec {
                name: "reply".into(),
                description: "Send your answer to the person.".into(),
                input_schema: json!({"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}),
            },
            Retention::Durable,
        ),
    ]
}

fn grade(case: &Case, host: &Host) -> Value {
    let replies = lock(&host.replies).clone();
    let reply = replies.last().cloned().unwrap_or_default();
    let lower = reply.to_lowercase();
    let remote: Vec<&Record> = case.records.iter().filter(|record| record.remote).collect();
    let best = remote.iter().map(|record| record.salary).max().unwrap_or(0);
    let names_remote = remote
        .iter()
        .all(|record| lower.contains(&record.name.to_lowercase()));
    let names_only_remote = case
        .records
        .iter()
        .filter(|record| !record.remote)
        .all(|record| !lower.contains(&record.name.to_lowercase()));
    let salary = reply.contains(&format!("{best}k")) || reply.contains(&format!("{best},000"));
    let words = reply.split_whitespace().count();
    let short = !reply.is_empty() && words < WORD_LIMIT;
    let question = reply.trim_end().ends_with('?');
    let checks = [names_remote, salary, short, question];
    let passed = checks.iter().filter(|check| **check).count();
    json!({
        "visited": lock(&host.visited).len(),
        "replies": replies.len(),
        "names_remote": names_remote,
        "names_only_remote": names_only_remote,
        "salary": salary,
        "short": short,
        "words": words,
        "question": question,
        "score": f64::from(u32::try_from(passed).unwrap_or(0)) / 4.0,
        "all": passed == checks.len(),
    })
}

fn env(name: &str) -> Result<String, String> {
    std::env::var(name).map_err(|_| format!("set {name}"))
}

fn list<T: std::str::FromStr>(name: &str, default: &str) -> Vec<T> {
    std::env::var(name)
        .unwrap_or_else(|_| default.to_owned())
        .split(',')
        .filter_map(|item| item.trim().parse().ok())
        .collect()
}

struct Job {
    case: u64,
    length: usize,
    size: String,
    recite: bool,
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
    let mut client = ProviderClient::new(protocol, env("NEWT_BASE_URL")?, env("NEWT_API_KEY")?)?;
    if let Ok(header) = std::env::var("NEWT_AUTH_HEADER") {
        client = client.with_api_key_header(header);
    }
    let client = Arc::new(client);
    let model = Arc::new(env("NEWT_MODEL")?);
    let out = Arc::new(Mutex::new(
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(env("EVAL_OUT")?)?,
    ));
    let cases: u64 = std::env::var("EVAL_CASES")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(10);
    let lengths: Vec<usize> = list("EVAL_LENGTHS", "5,11,23");
    let sizes: Vec<String> = list("EVAL_SIZES", "short,long");
    let arms: Vec<bool> = list::<String>("EVAL_ARMS", "on,off")
        .into_iter()
        .map(|arm| arm == "on")
        .collect();
    let parallel: usize = std::env::var("EVAL_PARALLEL")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(3);
    let run = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();

    let mut jobs = Vec::new();
    for case in 0..cases {
        for &length in &lengths {
            for size in &sizes {
                for &recite in &arms {
                    jobs.push(Job {
                        case,
                        length,
                        size: size.clone(),
                        recite,
                    });
                }
            }
        }
    }
    let gate = Arc::new(tokio::sync::Semaphore::new(parallel.max(1)));
    let mut running = tokio::task::JoinSet::new();
    for job in jobs {
        let (client, model, out, gate) = (
            Arc::clone(&client),
            Arc::clone(&model),
            Arc::clone(&out),
            Arc::clone(&gate),
        );
        running.spawn(async move {
            let Ok(_permit) = gate.acquire_owned().await else {
                return;
            };
            let row = run_job(&client, &model, &job, run).await;
            let mut file = lock(&out);
            if let Err(error) = writeln!(file, "{row}") {
                let _ = writeln!(std::io::stderr(), "could not write a result: {error}");
            }
            let _ = writeln!(std::io::stdout().lock(), "{row}");
        });
    }
    while running.join_next().await.is_some() {}
    Ok(())
}

async fn run_job(client: &ProviderClient, model: &str, job: &Job, run: u64) -> Value {
    let long = job.size == "long";
    let seed = job.case * 1000 + job.length as u64 * 10 + u64::from(long);
    let case = case(seed, job.length, long);
    let host = Host {
        records: case.records.clone(),
        visited: Mutex::new(BTreeSet::new()),
        replies: Mutex::new(Vec::new()),
    };
    let tools = tools();
    let workspace = Workspace::default();
    let arm = if job.recite { "on" } else { "off" };
    let session = format!("recite-{run}-{seed}-{arm}");
    let settings = TurnSettings {
        model,
        first_call_effort: Some(Effort::Low),
        effort: Some(Effort::Medium),
        max_output_tokens: 4096,
        max_model_calls: u32::try_from(job.length).unwrap_or(30) + 8,
        max_turn_tokens: None,
        recite: job.recite,
        require_end_turn: false,
        builtin_tools: newt_core::tools::BuiltinTools::ALL,
        lane: Lane::Interactive,
        reset: false,
        compaction: None,
    };
    let started = Instant::now();
    let outcome = run_turn(
        &TurnDeps {
            model: client,
            host: &host,
            steering: &NoSteering,
            journal: &NoJournal,
            sink: &NoSink,
            cancel: &CancellationToken::new(),
        },
        TurnInput {
            session: &session,
            cache_key: &session,
            turn: "t1",
            system: SYSTEM,
            external_tools: &tools,
            history: &[],
            context: None,
            skills: &[],
            events: vec![Message::user_text(case.ask.clone())],
            workspace: &workspace,
            replay: Vec::new(),
        },
        &settings,
    )
    .await;
    let ms = started.elapsed().as_millis();
    let mut row = grade(&case, &host);
    let (calls, error) = match &outcome {
        Ok(outcome) => (outcome.calls.clone(), None),
        Err(failure) => (failure.calls.clone(), Some(failure.error.to_string())),
    };
    let input: u64 = calls.iter().map(|call| call.usage.input_tokens).sum();
    let cached: u64 = calls.iter().map(|call| call.usage.cache_read_tokens).sum();
    let output: u64 = calls.iter().map(|call| call.usage.output_tokens).sum();
    if let Value::Object(fields) = &mut row {
        fields.insert("run".into(), json!(run));
        fields.insert("case".into(), json!(job.case));
        fields.insert("length".into(), json!(job.length));
        fields.insert("size".into(), json!(job.size));
        fields.insert("recite".into(), json!(job.recite));
        fields.insert("model".into(), json!(model));
        fields.insert("model_calls".into(), json!(calls.len()));
        fields.insert("input_tokens".into(), json!(input));
        fields.insert("cached_tokens".into(), json!(cached));
        fields.insert("output_tokens".into(), json!(output));
        fields.insert("ms".into(), json!(ms));
        fields.insert("error".into(), json!(error));
        fields.insert("final_text".into(), json!(final_text(&outcome)));
    }
    row
}

fn final_text(
    outcome: &Result<newt_core::turn::TurnOutcome, newt_core::turn::TurnFailure>,
) -> String {
    let Ok(outcome) = outcome else {
        return String::new();
    };
    outcome
        .committed
        .iter()
        .rev()
        .filter(|message| message.role == Role::Assistant)
        .flat_map(|message| message.parts.iter())
        .find_map(|part| match part {
            Part::Text { text } if !text.is_empty() => Some(text.clone()),
            _ => None,
        })
        .unwrap_or_default()
}
