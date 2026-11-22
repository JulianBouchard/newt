use serde_json::{Value, json};

use super::UiStream;

fn push_all(frames: &[Value]) -> Vec<Value> {
    let (mut ui, mut out) = UiStream::start();
    for frame in frames {
        out.extend(ui.push(frame));
    }
    out
}

fn turn_started() -> Value {
    json!({ "type": "turn_started", "turn": "t1" })
}

#[test]
fn a_tool_frame_without_a_call_id_is_ignored() {
    let out = push_all(&[
        turn_started(),
        json!({ "type": "tool_started", "turn": "t1", "name": "send" }),
        json!({ "type": "tool_input", "turn": "t1", "call_id": "", "name": "send", "input": {} }),
    ]);

    assert_eq!(out, vec![json!({ "type": "start" })]);
}

