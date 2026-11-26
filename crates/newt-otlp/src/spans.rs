use serde_json::{Value, json};
use time::OffsetDateTime;

use newt_core::trace::{
    attempt, model_call_span_id, root_span_id, session_hash, tool_span_id, trace_id,
};

use crate::assemble::{CallTrace, ToolTrace, TurnTrace};
use crate::{Hooks, OtlpConfig, Profile};

const KIND_INTERNAL: u8 = 1;
const KIND_CLIENT: u8 = 3;
const STATUS_ERROR: u8 = 2;

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

fn status_message(error: Option<&str>, config: &OtlpConfig, hooks: &Hooks) -> Option<String> {
    let error = error?;
    if !config.capture_content {
        return Some("failed".to_owned());
    }
    Some(match &hooks.redactor {
        Some(redactor) => redactor.redact(error.to_owned()),
        None => error.to_owned(),
    })
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
    attempt: String,
    conversation: String,
    agent: &'a str,
    turn: &'a str,
}

pub(crate) fn spans(trace: &TurnTrace, config: &OtlpConfig, hooks: &Hooks) -> Vec<Value> {
    let (session, turn) = (trace.session.as_str(), trace.turn.as_str());
    let attempt = trace.started.map(attempt).unwrap_or_default();
    let ids = Ids {
        trace: trace_id(session, turn, &attempt),
        root: root_span_id(session, turn, &attempt),
        session,
        conversation: if config.capture_content {
            session.to_owned()
        } else {
            session_hash(session)
        },
        agent: session.split('/').next().unwrap_or(session),
        attempt,
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
    let agent = ids.agent;
    let conversation = ids.conversation.as_str();
    let mut attributes = vec![
        text("gen_ai.operation.name", "invoke_agent"),
        text("gen_ai.agent.name", agent),
        text("gen_ai.conversation.id", conversation),
        text("newt.turn.id", ids.turn),
        int(
            "newt.model_calls",
            u64::try_from(trace.calls.len()).unwrap_or(u64::MAX),
        ),
    ];
    if let Some(enricher) = &hooks.enricher {
        attributes.extend(
            enricher
                .attributes(&trace.session)
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
        attributes.push(text("langfuse.session.id", conversation));
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
        status_message(trace.error.as_deref(), config, hooks).as_deref(),
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
    let call_id = model_call_span_id(&ids.conversation, ids.turn, &ids.attempt, index);
    span(
        (&ids.trace, &call_id, Some(&ids.root)),
        format!("chat {}", call.model),
        KIND_CLIENT,
        (call_started, call_ended),
        attributes,
        status_message(call.error.as_deref(), config, hooks).as_deref(),
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
    let tool_started =
        tool.finished - time::Duration::milliseconds(i64::try_from(tool.ms).unwrap_or(0));
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
    let tool_id = tool_span_id(ids.session, ids.turn, &ids.attempt, &tool.call_id);
    span(
        (&ids.trace, &tool_id, Some(&ids.root)),
        format!("execute_tool {}", tool.name),
        KIND_INTERNAL,
        (tool_started.min(tool.finished), tool.finished),
        attributes,
        tool.is_error.then_some("the tool returned an error"),
    )
}
