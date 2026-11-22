use std::collections::{HashMap, HashSet};

use proptest::prelude::*;
use serde_json::{Value, json};

use super::UiStream;

const FRAME_TYPES: &[&str] = &[
    "turn_started",
    "turn_steered",
    "model_call_started",
    "model_call_finished",
    "text_delta",
    "tool_started",
    "tool_input_delta",
    "tool_input",
    "tool_output",
    "tool_finished",
    "turn_completed",
    "turn_failed",
    "tool_progress",
    "something_new",
    "",
];

fn garbage() -> impl Strategy<Value = Value> {
    prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::from),
        any::<i64>().prop_map(Value::from),
        ".{0,12}".prop_map(Value::from),
        Just(json!([])),
        Just(json!({ "a": [1, null] })),
    ]
}

fn field(strings: &'static [&'static str]) -> impl Strategy<Value = Option<Value>> {
    prop_oneof![
        3 => proptest::sample::select(strings).prop_map(|s| Some(Value::from(s))),
        1 => Just(None),
        1 => garbage().prop_map(Some),
    ]
}

fn frame() -> impl Strategy<Value = Value> {
    (
        prop_oneof![
            8 => proptest::sample::select(FRAME_TYPES).prop_map(|s| Some(Value::from(s))),
            1 => Just(None),
            1 => garbage().prop_map(Some),
        ],
        field(&["t1", "t2", ""]),
        field(&["c1", "c2", "c3", ""]),
        field(&["send", "lookup", ""]),
        field(&["x", "", "hello"]),
        field(&["x", "", "{\"a\":"]),
        field(&["x", "output text"]),
        any::<bool>(),
        garbage(),
    )
        .prop_map(
            |(kind, turn, call, name, text, delta, output, is_error, input)| {
                let mut frame = serde_json::Map::new();
                let mut put = |key: &str, value: Option<Value>| {
                    if let Some(value) = value {
                        frame.insert(key.to_owned(), value);
                    }
                };
                put("type", kind);
                put("turn", turn);
                put("call_id", call);
                put("name", name);
                put("text", text);
                put("delta", delta);
                put("output", output);
                put("is_error", Some(Value::from(is_error)));
                put("ms", Some(json!(3)));
                put("input", Some(input));
                Value::Object(frame)
            },
        )
}

fn run(frames: &[Value]) -> Vec<Value> {
    let (mut ui, mut out) = UiStream::start();
    for frame in frames {
        out.extend(ui.push(frame));
    }
    out
}

fn kind(chunk: &Value) -> &str {
    chunk["type"].as_str().unwrap_or_default()
}

fn call_of(chunk: &Value) -> &str {
    chunk["toolCallId"].as_str().unwrap_or_default()
}

fn check(out: &[Value]) -> Result<(), String> {
    if out.first().map(kind) != Some("start") {
        return Err("first chunk is not start".into());
    }
    if out.iter().filter(|chunk| kind(chunk) == "start").count() != 1 {
        return Err("start appears more than once".into());
    }
    let finishes: Vec<usize> = (0..out.len())
        .filter(|&i| kind(&out[i]) == "finish")
        .collect();
    if finishes.len() > 1 || finishes.first().is_some_and(|&i| i + 1 != out.len()) {
        return Err("finish repeated or followed by chunks".into());
    }

    let mut step_open = false;
    let mut text_open: Option<String> = None;
    let mut announced = HashSet::new();
    let mut answered = HashSet::new();
    let mut inputs: HashMap<String, usize> = HashMap::new();
    for chunk in &out[1..] {
        let id = call_of(chunk).to_owned();
        match kind(chunk) {
            "start-step" if step_open => return Err("start-step inside a step".into()),
            "start-step" => step_open = true,
            "finish-step" if !step_open => return Err("finish-step without a step".into()),
            "finish-step" => {
                if text_open.is_some() {
                    return Err("finish-step with text open".into());
                }
                step_open = false;
            }
            "text-start" if text_open.is_some() || !step_open => {
                return Err("text-start nested or outside a step".into());
            }
            "text-start" => text_open = chunk["id"].as_str().map(str::to_owned),
            "text-delta" if text_open.is_none() || text_open.as_deref() != chunk["id"].as_str() => {
                return Err("text-delta outside its text block".into());
            }
            "text-end" if text_open.is_none() || text_open.as_deref() != chunk["id"].as_str() => {
                return Err("text-end without its text-start".into());
            }
            "text-end" => text_open = None,
            "tool-input-start" => {
                if id.is_empty() {
                    return Err("tool-input-start with an empty call id".into());
                }
                if !step_open {
                    return Err("tool-input-start outside a step".into());
                }
                if text_open.is_some() {
                    return Err("tool-input-start with text open".into());
                }
                if !announced.insert(id) {
                    return Err("call announced twice".into());
                }
            }
            "tool-input-delta" | "tool-input-available" => {
                if !announced.contains(&id) {
                    return Err(format!("{} before tool-input-start", kind(chunk)));
                }
                if answered.contains(&id) {
                    return Err(format!("{} after the call's output", kind(chunk)));
                }
                if kind(chunk) == "tool-input-available" {
                    let seen = inputs.entry(id).or_default();
                    *seen += 1;
                    if *seen > 1 {
                        return Err("tool-input-available twice for one call".into());
                    }
                }
            }
            "tool-output-available" | "tool-output-error" => {
                if !announced.contains(&id) {
                    return Err("output before tool-input-start".into());
                }
                if !answered.insert(id) {
                    return Err("second output for one call".into());
                }
            }
            _ => {}
        }
    }
    if !finishes.is_empty() && (step_open || text_open.is_some()) {
        return Err("finish with a step or text still open".into());
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    #[test]
    fn arbitrary_frame_sequences_make_a_well_formed_ui_stream(
        frames in proptest::collection::vec(frame(), 0..40)
    ) {
        let out = run(&frames);
        prop_assert_eq!(check(&out), Ok(()), "chunks: {:?}", out);
    }

    #[test]
    fn a_turn_that_starts_properly_and_then_gets_garbage_stays_well_formed(
        frames in proptest::collection::vec(frame(), 0..40)
    ) {
        let mut all = vec![json!({ "type": "turn_started", "turn": "t1" })];
        all.extend(frames);
        let out = run(&all);
        prop_assert_eq!(check(&out), Ok(()), "chunks: {:?}", out);
    }
}
