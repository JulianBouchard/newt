use std::collections::{BTreeMap, HashMap};

use serde_json::{Value, json};
use time::OffsetDateTime;

use crate::event::{Event, EventKind};
use crate::message::Message;
use crate::prompt::{Effort, fnv1a};
use crate::provider::{CallTiming, Usage};
use crate::tools::Retention;

use super::{Hooks, OtlpConfig, Profile};

const KIND_INTERNAL: u8 = 1;
const KIND_CLIENT: u8 = 3;
const STATUS_ERROR: u8 = 2;

#[derive(Debug, Default)]
pub(crate) struct CallTrace {
    model: String,
    effort: Option<Effort>,
    prefix_hash: String,
    started: Option<OffsetDateTime>,
    finished: Option<OffsetDateTime>,
    input: Vec<Message>,
    output: Option<Message>,
    usage: Usage,
    timing: CallTiming,
    response_id: Option<String>,
    error: Option<String>,
}

#[derive(Debug)]
pub(crate) struct ToolTrace {
    call_id: String,
    name: String,
    started: Option<OffsetDateTime>,
    finished: OffsetDateTime,
    ms: u64,
    retention: Retention,
    is_error: bool,
}

#[derive(Debug, Default)]
pub(crate) struct TurnTrace {
    session: String,
    turn: String,
    compaction: bool,
    started: Option<OffsetDateTime>,
    ended: Option<OffsetDateTime>,
    error: Option<String>,
    calls: BTreeMap<u32, CallTrace>,
    tools: Vec<ToolTrace>,
    tool_starts: HashMap<String, OffsetDateTime>,
}

#[derive(Debug, Default)]
pub(crate) struct Assembler {
    turns: HashMap<(String, String), TurnTrace>,
}

impl Assembler {
    fn turn(&mut self, session: &str, turn: &str) -> &mut TurnTrace {
        self.turns
            .entry((session.to_owned(), turn.to_owned()))
            .or_insert_with(|| TurnTrace {
                session: session.to_owned(),
                turn: turn.to_owned(),
                ..TurnTrace::default()
            })
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one flat match over every event kind reads better than splitting it"
    )]
    pub(crate) fn apply(&mut self, event: Event) -> Option<TurnTrace> {
        let at = event.at;
        match event.kind {
            EventKind::TurnStarted {
                session,
                turn,
                compaction,
                ..
            } => {
                let trace = self.turn(&session, &turn);
                trace.started = Some(at);
                trace.compaction = compaction;
            }
            EventKind::ModelCallStarted {
                session,
                turn,
                call,
                model,
                effort,
                prefix,
                input,
                ..
            } => {
                let entry = self.turn(&session, &turn).calls.entry(call).or_default();
                entry.model = model;
                entry.effort = effort;
                entry.prefix_hash = prefix.hash;
                entry.started = Some(at);
                entry.input = input;
            }
            EventKind::ModelCallFinished {
                session,
                turn,
                call,
                usage,
                timing,
                response_id,
                output,
                ..
            } => {
                let entry = self.turn(&session, &turn).calls.entry(call).or_default();
                entry.finished = Some(at);
                entry.usage = usage;
                entry.timing = timing;
                entry.response_id = response_id;
                entry.output = Some(output);
            }
            EventKind::ModelCallInterrupted {
                session,
                turn,
                call,
            } => {
                let entry = self.turn(&session, &turn).calls.entry(call).or_default();
                entry.finished = Some(at);
                entry.error = Some("interrupted by a new event".to_owned());
            }
            EventKind::ModelCallFailed {
                session,
                turn,
                call,
                error,
            } => {
                let entry = self.turn(&session, &turn).calls.entry(call).or_default();
                entry.finished = Some(at);
                entry.error = Some(error);
            }
            EventKind::ToolStarted {
                session,
                turn,
                call_id,
                ..
            } => {
                self.turn(&session, &turn).tool_starts.insert(call_id, at);
            }
            EventKind::ToolFinished {
                session,
                turn,
                call_id,
                name,
                retention,
                is_error,
                ms,
            } => {
                let trace = self.turn(&session, &turn);
                let started = trace.tool_starts.remove(&call_id);
                trace.tools.push(ToolTrace {
                    call_id,
                    name,
                    started,
                    finished: at,
                    ms,
                    retention,
                    is_error,
                });
            }
            EventKind::TurnCompleted { session, turn, .. } => {
                let mut trace = self.turns.remove(&(session, turn))?;
                trace.ended = Some(at);
                return Some(trace);
            }
            EventKind::TurnFailed {
                session,
                turn,
                error,
                ..
            } => {
                let mut trace = self.turns.remove(&(session, turn))?;
                trace.ended = Some(at);
                trace.error = Some(error);
                return Some(trace);
            }
            EventKind::ServerReady { .. }
            | EventKind::TurnSteered { .. }
            | EventKind::ModelCallReplayed { .. }
            | EventKind::CompactionCompleted { .. } => {}
        }
        None
    }

    pub(crate) fn drain(&mut self) -> Vec<TurnTrace> {
        self.turns
            .drain()
            .map(|(_, mut trace)| {
                trace.error = Some("the turn never finished".to_owned());
                trace
            })
            .collect()
    }
}

fn hex64(parts: &[&str]) -> String {
    format!("{:016x}", fnv1a(parts.join("\u{0}").as_bytes()))
}

fn nanos(at: OffsetDateTime) -> String {
    at.unix_timestamp_nanos().to_string()
}

fn text(key: &str, value: impl Into<String>) -> Value {
    json!({ "key": key, "value": { "stringValue": value.into() } })
}

fn int(key: &str, value: u64) -> Value {
    json!({ "key": key, "value": { "intValue": value.to_string() } })
}

fn span(
    ids: (&str, &str, Option<&str>),
    name: String,
    kind: u8,
    window: (OffsetDateTime, OffsetDateTime),
    attributes: Vec<Value>,
    error: Option<&str>,
) -> Value {
    let (trace_id, span_id, parent) = ids;
    let mut span = json!({
        "traceId": trace_id,
        "spanId": span_id,
        "kind": kind,
        "startTimeUnixNano": nanos(window.0),
        "endTimeUnixNano": nanos(window.1),
    });
    span["name"] = Value::String(name);
    span["attributes"] = Value::Array(attributes);
    if let Some(parent) = parent {
        span["parentSpanId"] = json!(parent);
    }
    if let Some(error) = error {
        span["status"] = json!({ "code": STATUS_ERROR, "message": error });
    }
    span
}

fn content(messages: &impl serde::Serialize, hooks: &Hooks) -> String {
    let raw = serde_json::to_string(messages).unwrap_or_default();
    match &hooks.redactor {
        Some(redactor) => redactor.redact(raw),
        None => raw,
    }
}

struct Ids<'a> {
    trace: String,
    root: String,
    session: &'a str,
    turn: &'a str,
}

pub(crate) fn spans(trace: &TurnTrace, config: &OtlpConfig, hooks: &Hooks) -> Vec<Value> {
    let (session, turn) = (trace.session.as_str(), trace.turn.as_str());
    let ids = Ids {
        trace: format!(
            "{}{}",
            hex64(&["trace", session, turn]),
            hex64(&["trace-low", session, turn])
        ),
        root: hex64(&["root", session, turn]),
        session,
        turn,
    };
    let now = OffsetDateTime::now_utc();
    let started = trace.started.unwrap_or(now);
    let ended = trace.ended.unwrap_or(now).max(started);
    let mut out = Vec::with_capacity(1 + trace.calls.len() + trace.tools.len());
    out.push(root_span(trace, &ids, (started, ended), config, hooks));
    for (index, call) in &trace.calls {
        out.push(chat_span(
            *index,
            call,
            &ids,
            (started, ended),
            config,
            hooks,
        ));
    }
    for tool in &trace.tools {
        out.push(tool_span(tool, &ids, config));
    }
    out
}

fn root_span(
    trace: &TurnTrace,
    ids: &Ids<'_>,
    window: (OffsetDateTime, OffsetDateTime),
    config: &OtlpConfig,
    hooks: &Hooks,
) -> Value {
    let session = ids.session;
    let agent = session.split('/').next().unwrap_or(session);
    let mut attributes = vec![
        text("gen_ai.operation.name", "invoke_agent"),
        text("gen_ai.agent.name", agent),
        text("gen_ai.conversation.id", session),
        text("newt.turn.id", ids.turn),
        int(
            "newt.model_calls",
            u64::try_from(trace.calls.len()).unwrap_or(u64::MAX),
        ),
    ];
    if let Some(enricher) = &hooks.enricher {
        attributes.extend(
            enricher
                .attributes(session)
                .into_iter()
                .map(|(key, value)| text(&key, value)),
        );
    }
    let label = if trace.compaction {
        "compaction"
    } else {
        "turn"
    };
    if config.profile == Profile::Langfuse {
        attributes.push(text("langfuse.session.id", session));
        attributes.push(text("langfuse.trace.name", format!("{agent} {label}")));
        attributes.push(text("langfuse.observation.type", "agent"));
        if config.capture_content {
            if let Some(first) = trace.calls.values().next() {
                attributes.push(text(
                    "langfuse.observation.input",
                    content(&first.input, hooks),
                ));
            }
            if let Some(output) = trace
                .calls
                .values()
                .rev()
                .find_map(|call| call.output.as_ref())
            {
                attributes.push(text("langfuse.observation.output", content(output, hooks)));
            }
        }
    }
    span(
        (&ids.trace, &ids.root, None),
        format!("invoke_agent {agent}"),
        KIND_INTERNAL,
        window,
        attributes,
        trace.error.as_deref(),
    )
}

fn chat_span(
    index: u32,
    call: &CallTrace,
    ids: &Ids<'_>,
    turn_window: (OffsetDateTime, OffsetDateTime),
    config: &OtlpConfig,
    hooks: &Hooks,
) -> Value {
    let call_started = call.started.unwrap_or(turn_window.0);
    let call_ended = call.finished.unwrap_or(turn_window.1).max(call_started);
    let usage = call.usage;
    let mut attributes = vec![
        text("gen_ai.operation.name", "chat"),
        text("gen_ai.request.model", call.model.clone()),
        int("gen_ai.usage.input_tokens", usage.input_tokens),
        int("gen_ai.usage.output_tokens", usage.output_tokens),
        int(
            "gen_ai.usage.cache_read.input_tokens",
            usage.cache_read_tokens,
        ),
        int(
            "gen_ai.usage.cache_creation.input_tokens",
            usage.cache_write_tokens,
        ),
        text("newt.prefix.hash", call.prefix_hash.clone()),
        int("newt.timing.total_ms", call.timing.total_ms),
    ];
    if let Some(effort) = call.effort {
        attributes.push(text(
            "gen_ai.request.reasoning_effort",
            format!("{effort:?}").to_lowercase(),
        ));
    }
    if let Some(response_id) = &call.response_id {
        attributes.push(text("gen_ai.response.id", response_id.clone()));
    }
    for (key, value) in [
        ("newt.timing.headers_ms", call.timing.headers_ms),
        ("newt.timing.first_output_ms", call.timing.first_output_ms),
        ("newt.timing.first_tool_ms", call.timing.first_tool_ms),
    ] {
        if let Some(value) = value {
            attributes.push(int(key, value));
        }
    }
    if config.capture_content {
        attributes.push(text("gen_ai.input.messages", content(&call.input, hooks)));
        if let Some(output) = &call.output {
            attributes.push(text("gen_ai.output.messages", content(output, hooks)));
        }
    }
    if config.profile == Profile::Langfuse {
        attributes.extend(langfuse_generation(call, call_started, config, hooks));
    }
    let call_id = hex64(&["call", ids.session, ids.turn, &index.to_string()]);
    span(
        (&ids.trace, &call_id, Some(&ids.root)),
        format!("chat {}", call.model),
        KIND_CLIENT,
        (call_started, call_ended),
        attributes,
        call.error.as_deref(),
    )
}

fn langfuse_generation(
    call: &CallTrace,
    call_started: OffsetDateTime,
    config: &OtlpConfig,
    hooks: &Hooks,
) -> Vec<Value> {
    let usage = call.usage;
    let mut attributes = vec![
        text("langfuse.observation.type", "generation"),
        text("langfuse.observation.model.name", call.model.clone()),
        text(
            "langfuse.observation.usage_details",
            json!({
                "input": usage
                    .input_tokens
                    .saturating_sub(usage.cache_read_tokens + usage.cache_write_tokens),
                "input_cached_tokens": usage.cache_read_tokens,
                "input_cache_creation_tokens": usage.cache_write_tokens,
                "output": usage.output_tokens,
                "total": usage.input_tokens + usage.output_tokens,
            })
            .to_string(),
        ),
    ];
    if let Some(first_output) = call.timing.first_output_ms {
        let at =
            call_started + time::Duration::milliseconds(i64::try_from(first_output).unwrap_or(0));
        if let Ok(formatted) = at.format(&time::format_description::well_known::Rfc3339) {
            attributes.push(text(
                "langfuse.observation.completion_start_time",
                formatted,
            ));
        }
    }
    if config.capture_content {
        attributes.push(text(
            "langfuse.observation.input",
            content(&call.input, hooks),
        ));
        if let Some(output) = &call.output {
            attributes.push(text("langfuse.observation.output", content(output, hooks)));
        }
    }
    attributes
}

fn tool_span(tool: &ToolTrace, ids: &Ids<'_>, config: &OtlpConfig) -> Value {
    let tool_started = tool.started.unwrap_or_else(|| {
        tool.finished - time::Duration::milliseconds(i64::try_from(tool.ms).unwrap_or(0))
    });
    let mut attributes = vec![
        text("gen_ai.operation.name", "execute_tool"),
        text("gen_ai.tool.name", tool.name.clone()),
        text("gen_ai.tool.call.id", tool.call_id.clone()),
        text(
            "newt.retention",
            format!("{:?}", tool.retention).to_lowercase(),
        ),
    ];
    if config.profile == Profile::Langfuse {
        attributes.push(text("langfuse.observation.type", "tool"));
    }
    let tool_id = hex64(&["tool", ids.session, ids.turn, &tool.call_id]);
    span(
        (&ids.trace, &tool_id, Some(&ids.root)),
        format!("execute_tool {}", tool.name),
        KIND_INTERNAL,
        (tool_started.min(tool.finished), tool.finished),
        attributes,
        tool.is_error.then_some("the tool returned an error"),
    )
}
