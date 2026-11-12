use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write as _;
use std::sync::Mutex;

use serde_json::{Value, json};

use crate::validate::{looks_like_error, violations};

const FORGET_SEEN_ABOVE: usize = 1_000_000;

#[derive(Debug, Default)]
pub struct Audit {
    seen: Mutex<HashSet<String>>,
    outputs: Mutex<BTreeMap<String, [u64; 2]>>,
    invalid: Mutex<BTreeMap<String, u64>>,
    unanswered: Mutex<HashMap<String, Unanswered>>,
    log: Option<Mutex<std::fs::File>>,
}

#[derive(Debug)]
struct Unanswered {
    tools: Vec<String>,
    done: usize,
    target: usize,
}

fn locked<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn output_text(item: &Value) -> String {
    match item.get("output") {
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

impl Audit {
    pub fn new(decision_log: Option<&std::path::Path>) -> std::io::Result<Self> {
        let log = decision_log
            .map(|path| {
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
            })
            .transpose()?
            .map(Mutex::new);
        Ok(Self {
            log,
            ..Self::default()
        })
    }

    pub fn observe_outputs(&self, input: &[Value]) {
        let names: HashMap<&str, &str> = input
            .iter()
            .filter(|item| item.get("type").and_then(Value::as_str) == Some("function_call"))
            .filter_map(|item| Some((item.get("call_id")?.as_str()?, item.get("name")?.as_str()?)))
            .collect();
        let mut seen = locked(&self.seen);
        if seen.len() > FORGET_SEEN_ABOVE {
            seen.clear();
        }
        let mut unanswered = locked(&self.unanswered);
        for item in input
            .iter()
            .filter(|item| item.get("type").and_then(Value::as_str) == Some("function_call"))
        {
            if let Some(call_id) = item.get("call_id").and_then(Value::as_str) {
                unanswered.remove(call_id);
            }
        }
        drop(unanswered);
        let mut outputs = locked(&self.outputs);
        for item in input
            .iter()
            .filter(|item| item.get("type").and_then(Value::as_str) == Some("function_call_output"))
        {
            let Some(call_id) = item.get("call_id").and_then(Value::as_str) else {
                continue;
            };
            if !seen.insert(call_id.to_owned()) {
                continue;
            }
            let name = names.get(call_id).copied().unwrap_or("?");
            let counts = outputs.entry(name.to_owned()).or_default();
            counts[0] += 1;
            if looks_like_error(&output_text(item)) {
                counts[1] += 1;
            }
        }
    }

    pub fn check_call(&self, name: &str, schema: &Value, arguments: &Value) {
        let found = violations(schema, arguments);
        if found.is_empty() {
            return;
        }
        tracing::warn!(tool = name, problems = ?found, "fake model sent arguments the tool schema rejects");
        *locked(&self.invalid).entry(name.to_owned()).or_default() += 1;
    }

    pub fn emitted(&self, calls: &[(String, String, String)], done: usize, target: usize) {
        let Some((_, _, first)) = calls.first() else {
            return;
        };
        let mut unanswered = locked(&self.unanswered);
        if unanswered.len() > FORGET_SEEN_ABOVE {
            unanswered.clear();
        }
        unanswered.insert(
            first.clone(),
            Unanswered {
                tools: calls.iter().map(|(name, _, _)| name.clone()).collect(),
                done,
                target,
            },
        );
    }

    pub fn log_decision(&self, decision: &Value) {
        if let Some(log) = &self.log {
            let mut file = locked(log);
            if let Err(error) = writeln!(file, "{decision}") {
                tracing::warn!(%error, "could not write the decision log");
            }
        }
    }

    pub fn snapshot(&self) -> Value {
        let outputs = locked(&self.outputs);
        let (calls, errors) = outputs
            .values()
            .fold((0, 0), |(calls, errors), [count, failed]| {
                (calls + count, errors + failed)
            });
        json!({
            "tool_outputs": calls,
            "tool_errors": errors,
            "tool_errors_by_tool": outputs
                .iter()
                .filter(|(_, [_, failed])| *failed > 0)
                .map(|(name, [count, failed])| (name.clone(), json!([failed, count])))
                .collect::<serde_json::Map<_, _>>(),
            "invalid_arguments_by_tool": *locked(&self.invalid),
            "unanswered_tool_replies": self.unanswered_summary(),
        })
    }

    fn unanswered_summary(&self) -> Value {
        let unanswered = locked(&self.unanswered);
        let mut by_tools: BTreeMap<String, u64> = BTreeMap::new();
        let mut short_by = 0;
        for reply in unanswered.values() {
            *by_tools.entry(reply.tools.join("+")).or_default() += 1;
            short_by += reply.target.saturating_sub(reply.done + reply.tools.len());
        }
        json!({
            "replies": unanswered.len(),
            "tool_calls_left_in_turn_plan": short_by,
            "by_tools": by_tools,
        })
    }
}
