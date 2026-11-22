use std::collections::HashMap;

use serde_json::{Value, json};

use crate::args::arguments_for;
use crate::profile::{Kind, Profile};
use crate::sample::Rng;

const COMPACTING: &str = "Newt is compacting";

pub(crate) struct Turn {
    pub(crate) done_calls: usize,
    pub(crate) replied: bool,
    pub(crate) last_user: String,
    pub(crate) compacting: bool,
}

fn user_text(item: &Value) -> String {
    match item.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect(),
        _ => String::new(),
    }
}

/// A user message that starts a turn: not a goal reminder, which quotes the
/// turn's events back to the model in the middle of a long turn.
fn is_user(item: &Value) -> bool {
    item.get("role").and_then(Value::as_str) == Some("user")
        && !user_text(item).starts_with("<goal>")
}

pub(crate) fn read_turn(input: &[Value], marker: Option<&str>) -> Turn {
    let last_user = marker
        .and_then(|marker| {
            input
                .iter()
                .rposition(|item| is_user(item) && user_text(item).contains(marker))
        })
        .or_else(|| input.iter().rposition(is_user));
    let start = last_user.map_or(0, |index| index + 1);
    let compacting = input
        .iter()
        .filter(|item| is_user(item))
        .any(|item| user_text(item).starts_with(COMPACTING));
    Turn {
        done_calls: input
            .get(start..)
            .unwrap_or_default()
            .iter()
            .filter(|item| item.get("type").and_then(Value::as_str) == Some("function_call_output"))
            .count(),
        replied: input.get(start..).unwrap_or_default().iter().any(|item| {
            item.get("role").and_then(Value::as_str) == Some("assistant")
                && !user_text(item).is_empty()
        }),
        last_user: last_user
            .and_then(|index| input.get(index))
            .map(user_text)
            .unwrap_or_default(),
        compacting,
    }
}

pub(crate) fn seed_of(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

pub(crate) struct Call {
    pub(crate) name: String,
    pub(crate) arguments: Value,
}

pub(crate) fn pick_tools(
    profile: &Profile,
    rng: &Rng,
    tools: &HashMap<String, Value>,
    count: usize,
) -> Vec<Call> {
    let available: Vec<_> = profile
        .tools
        .iter()
        .filter(|tool| !matches!(tool.kind, Kind::End))
        .filter(|tool| {
            tools.contains_key(&tool.name)
                || (tool.kind == Kind::Skill
                    && profile
                        .skill_tools
                        .iter()
                        .any(|name| tools.contains_key(name)))
        })
        .collect();
    let total: f64 = available.iter().map(|tool| tool.share).sum();
    let mut calls = Vec::with_capacity(count);
    let mut sent = false;
    while calls.len() < count && total > 0.0 {
        let mut roll = rng.unit() * total;
        let Some(tool) = available.iter().find(|tool| {
            roll -= tool.share;
            roll <= 0.0
        }) else {
            break;
        };
        if tool.kind == Kind::Send && std::mem::replace(&mut sent, true) {
            continue;
        }
        let name = if tool.kind == Kind::Skill {
            let Some(name) = profile
                .skill_tools
                .iter()
                .find(|name| tools.contains_key(*name))
            else {
                continue;
            };
            name
        } else {
            tool.name.as_str()
        };
        let schema = tools.get(name).cloned().unwrap_or(Value::Null);
        calls.push(Call {
            name: name.to_owned(),
            arguments: arguments_for(name, tool.kind, &schema, rng, profile),
        });
    }
    calls
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Finish {
    EndOnly,
    #[default]
    ReplyThenEnd,
    ReplyWithEnd,
}

pub(crate) struct Ending {
    pub(crate) calls: Vec<Call>,
    pub(crate) reply: bool,
}

pub(crate) fn finish(tools: &HashMap<String, Value>, turn: &Turn, mode: Finish) -> Ending {
    if turn.compacting && tools.contains_key("checkpoint") {
        return Ending {
            calls: vec![Call {
                name: "checkpoint".into(),
                arguments: json!({ "memory": ["load test"], "open_threads": [], "commitments": [], "decisions": [], "tone": "short" }),
            }],
            reply: false,
        };
    }
    if !tools.contains_key("end_turn") {
        return Ending {
            calls: Vec::new(),
            reply: true,
        };
    }
    let end = || {
        vec![Call {
            name: "end_turn".into(),
            arguments: json!({ "nothing_new": "load test turn" }),
        }]
    };
    match mode {
        Finish::EndOnly => Ending {
            calls: end(),
            reply: false,
        },
        Finish::ReplyWithEnd => Ending {
            calls: end(),
            reply: true,
        },
        Finish::ReplyThenEnd if turn.replied => Ending {
            calls: end(),
            reply: false,
        },
        Finish::ReplyThenEnd => Ending {
            calls: Vec::new(),
            reply: true,
        },
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn user(text: &str) -> Value {
        json!({ "role": "user", "content": [{ "type": "input_text", "text": text }] })
    }

    fn output() -> Value {
        json!({ "type": "function_call_output", "call_id": "c", "output": "ok" })
    }

    #[test]
    fn a_marker_keeps_the_turn_going_past_injected_user_messages() {
        let input = vec![
            user("load test message 1 of 3"),
            output(),
            user("Did you forget to respond?"),
            output(),
        ];
        let marked = read_turn(&input, Some("load test message"));
        assert_eq!(marked.done_calls, 2);
        assert_eq!(marked.last_user, "load test message 1 of 3");
        let unmarked = read_turn(&input, None);
        assert_eq!(unmarked.done_calls, 1);
    }

    #[test]
    fn one_reply_never_sends_twice() {
        let profile = Profile::default();
        let tools: HashMap<String, Value> = profile
            .tools
            .iter()
            .map(|tool| (tool.name.clone(), json!({ "type": "object" })))
            .collect();
        let rng = Rng::new(7);
        for _ in 0..200 {
            let calls = pick_tools(&profile, &rng, &tools, 6);
            let sends = calls
                .iter()
                .filter(|call| call.name == "send_message")
                .count();
            assert!(sends <= 1);
        }
    }
}
