use std::collections::{HashMap, HashSet};

use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::checkpoint::CHECKPOINT;
use crate::commit::{UNSUMMARIZED_RESULT_LIMIT, truncate};
use crate::message::{ToolCall, ToolResult};
use crate::prompt::ToolSpec;
use crate::provider::tool_arguments;
use crate::tools::{END_TURN, Retention, ToolDef, ToolOutput};

pub const BATCH: &str = "batch";
const MAX_CALLS: usize = 10;
const MODEL_NAMESPACE: &str = "functions.";

pub(crate) fn batch_tool() -> ToolDef {
    ToolDef::new(
        ToolSpec {
            name: BATCH.to_owned(),
            description: format!(
                "Run up to {MAX_CALLS} tool calls in one step. They run in parallel, except an outward call (one that sends or acts outside), which runs last; at most one per batch. Calls see the workspace as it was before the batch, so read a page you change in the next step. `fields` keeps only those JSON fields of a result (dot paths such as `role.title`), `limit` caps how many list items come back."
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "calls": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": MAX_CALLS,
                        "items": {
                            "type": "object",
                            "properties": {
                                "name": { "type": "string", "description": "The tool's name exactly as listed, with no namespace." },
                                "arguments": { "type": "object" },
                                "fields": { "type": "array", "items": { "type": "string" } },
                                "limit": { "type": "integer", "minimum": 1 },
                            },
                            "required": ["name", "arguments"],
                        },
                    },
                },
                "required": ["calls"],
            }),
        },
        Retention::Ephemeral,
    )
}

#[derive(Deserialize)]
struct BatchArgs {
    calls: Vec<InnerArgs>,
}

#[derive(Deserialize)]
struct InnerArgs {
    name: String,
    arguments: Value,
    #[serde(default)]
    fields: Vec<String>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Projection {
    fields: Vec<String>,
    limit: Option<usize>,
}

enum Group {
    Single(usize),
    Batch { id: String, inner: Vec<usize> },
}

pub(crate) struct Expanded {
    pub(crate) calls: Vec<ToolCall>,
    pub(crate) preset: HashMap<String, ToolOutput>,
    pub(crate) projections: HashMap<usize, Projection>,
    groups: Vec<Group>,
}

pub(crate) fn expand(tool_calls: &[ToolCall], retention: &HashMap<String, Retention>) -> Expanded {
    let mut expanded = Expanded {
        calls: Vec::with_capacity(tool_calls.len()),
        preset: HashMap::new(),
        projections: HashMap::new(),
        groups: Vec::with_capacity(tool_calls.len()),
    };
    for call in tool_calls {
        let call = &ToolCall {
            name: resolve_name(&call.name, retention),
            ..call.clone()
        };
        if call.name != BATCH || call.invalid_arguments().is_some() {
            expanded.groups.push(Group::Single(expanded.calls.len()));
            expanded.calls.push(call.clone());
            continue;
        }
        match parse(call, retention) {
            Ok(inner) => {
                let mut slots = Vec::with_capacity(inner.len());
                for (index, args) in inner.into_iter().enumerate() {
                    let slot = expanded.calls.len();
                    if !args.fields.is_empty() || args.limit.is_some() {
                        expanded.projections.insert(
                            slot,
                            Projection {
                                fields: args.fields,
                                limit: args.limit,
                            },
                        );
                    }
                    expanded.calls.push(ToolCall {
                        id: format!("{}.{index}", call.id),
                        name: args.name,
                        arguments: inner_arguments(args.arguments),
                    });
                    slots.push(slot);
                }
                expanded.groups.push(Group::Batch {
                    id: call.id.clone(),
                    inner: slots,
                });
            }
            Err(message) => {
                expanded
                    .preset
                    .insert(call.id.clone(), ToolOutput::error(message));
                expanded.groups.push(Group::Single(expanded.calls.len()));
                expanded.calls.push(call.clone());
            }
        }
    }
    expanded
}

/// The tool a model meant: a name with the `functions.` namespace some models
/// add resolves to the tool without it, when only that one exists.
pub(crate) fn resolve_name(name: &str, retention: &HashMap<String, Retention>) -> String {
    match name.strip_prefix(MODEL_NAMESPACE) {
        Some(bare) if !retention.contains_key(name) && retention.contains_key(bare) => {
            bare.to_owned()
        }
        _ => name.to_owned(),
    }
}

fn inner_arguments(arguments: Value) -> Value {
    match arguments {
        Value::Object(_) => arguments,
        Value::Null => json!({}),
        Value::String(raw) => tool_arguments(&raw),
        other => tool_arguments(&other.to_string()),
    }
}

fn parse(
    call: &ToolCall,
    retention: &HashMap<String, Retention>,
) -> Result<Vec<InnerArgs>, String> {
    let mut args = BatchArgs::deserialize(&call.arguments)
        .map_err(|error| format!("invalid arguments for batch: {error}"))?;
    for inner in &mut args.calls {
        inner.name = resolve_name(&inner.name, retention);
    }
    if args.calls.is_empty() || args.calls.len() > MAX_CALLS {
        return Err(format!("a batch holds between 1 and {MAX_CALLS} calls"));
    }
    if let Some(nested) = args
        .calls
        .iter()
        .find(|inner| [BATCH, END_TURN, CHECKPOINT].contains(&inner.name.as_str()))
    {
        return Err(format!("`{}` cannot run inside a batch", nested.name));
    }
    let outward = args
        .calls
        .iter()
        .filter(|inner| retention.get(&inner.name) == Some(&Retention::Outward))
        .count();
    if outward > 1 {
        return Err(
            "a batch holds at most one outward call: send the others in separate steps".to_owned(),
        );
    }
    Ok(args.calls)
}

pub(crate) fn project(content: &str, projection: &Projection) -> String {
    let Ok(mut value) = serde_json::from_str::<Value>(content) else {
        return content.to_owned();
    };
    let mut hidden = 0;
    if let Some(limit) = projection.limit {
        match &mut value {
            Value::Array(items) => hidden += cap(items, limit),
            Value::Object(fields) => {
                for field in fields.values_mut() {
                    if let Value::Array(items) = field {
                        hidden += cap(items, limit);
                    }
                }
            }
            _ => {}
        }
    }
    if !projection.fields.is_empty() {
        value = pick(&value, &projection.fields);
    }
    let mut out = value.to_string();
    if hidden > 0 {
        out.push_str("\n(");
        out.push_str(&hidden.to_string());
        out.push_str(" more items not shown)");
    }
    out
}

fn cap(items: &mut Vec<Value>, limit: usize) -> usize {
    let hidden = items.len().saturating_sub(limit);
    items.truncate(limit);
    hidden
}

fn pick(value: &Value, paths: &[String]) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(|item| pick(item, paths)).collect()),
        Value::Object(_) => {
            let mut picked = Map::new();
            for path in paths {
                let segments: Vec<&str> = path.split('.').collect();
                if let Some(found) = lookup(value, &segments) {
                    insert(&mut picked, &segments, found.clone());
                }
            }
            Value::Object(picked)
        }
        other => other.clone(),
    }
}

fn lookup<'a>(value: &'a Value, segments: &[&str]) -> Option<&'a Value> {
    segments
        .iter()
        .try_fold(value, |current, segment| current.get(*segment))
}

fn insert(target: &mut Map<String, Value>, segments: &[&str], value: Value) {
    let Some((last, parents)) = segments.split_last() else {
        return;
    };
    let mut current = target;
    for segment in parents {
        let entry = current
            .entry((*segment).to_owned())
            .or_insert_with(|| Value::Object(Map::new()));
        let Value::Object(next) = entry else {
            return;
        };
        current = next;
    }
    current.insert((*last).to_owned(), value);
}

pub(crate) struct Folded {
    pub(crate) results: Vec<ToolResult>,
    pub(crate) summaries: HashMap<String, String>,
    pub(crate) kept: HashSet<String>,
    pub(crate) batch_ms: HashMap<String, u64>,
}

pub(crate) fn fold(
    expanded: &Expanded,
    mut results: Vec<Option<ToolResult>>,
    mut summaries: HashMap<String, String>,
    retention: &HashMap<String, Retention>,
    durations: &[u64],
) -> Folded {
    let mut folded = Folded {
        results: Vec::with_capacity(expanded.groups.len()),
        summaries: HashMap::new(),
        kept: HashSet::new(),
        batch_ms: HashMap::new(),
    };
    for group in &expanded.groups {
        match group {
            Group::Single(slot) => {
                if let Some(result) = results.get_mut(*slot).and_then(Option::take) {
                    if let Some(summary) = summaries.remove(&result.call_id) {
                        folded.summaries.insert(result.call_id.clone(), summary);
                    }
                    folded.results.push(result);
                }
            }
            Group::Batch { id, inner } => {
                let slowest = inner
                    .iter()
                    .filter_map(|slot| durations.get(*slot))
                    .max()
                    .copied()
                    .unwrap_or(0);
                folded.batch_ms.insert(id.clone(), slowest);
                let mut lines = Vec::with_capacity(inner.len());
                let mut kept_lines = Vec::new();
                let mut failures = 0;
                for (position, slot) in inner.iter().enumerate() {
                    let Some(result) = results.get_mut(*slot).and_then(Option::take) else {
                        continue;
                    };
                    let name = expanded
                        .calls
                        .get(*slot)
                        .map_or("?", |call| call.name.as_str());
                    let summary = summaries.remove(&result.call_id);
                    if retention
                        .get(name)
                        .is_some_and(|class| *class != Retention::Ephemeral)
                    {
                        kept_lines.push(match (&summary, result.is_error) {
                            (Some(summary), false) => summary.clone(),
                            (_, true) => format!(
                                "{name} failed: {}",
                                truncate(&result.content, UNSUMMARIZED_RESULT_LIMIT)
                            ),
                            (None, false) => format!(
                                "{name}: {}",
                                truncate(&result.content, UNSUMMARIZED_RESULT_LIMIT)
                            ),
                        });
                    }
                    let status = if result.is_error {
                        failures += 1;
                        " (failed)"
                    } else {
                        ""
                    };
                    lines.push(format!(
                        "[{}] {name}{status}: {}",
                        position + 1,
                        result.content
                    ));
                }
                if !kept_lines.is_empty() {
                    folded.kept.insert(id.clone());
                    folded.summaries.insert(id.clone(), kept_lines.join("; "));
                }
                folded.results.push(ToolResult {
                    call_id: id.clone(),
                    content: lines.join("\n"),
                    is_error: failures == inner.len(),
                });
            }
        }
    }
    folded
}

#[cfg(test)]
mod props;
#[cfg(test)]
mod tests;
