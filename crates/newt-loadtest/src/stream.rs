use std::sync::atomic::Ordering;

use axum::response::sse::Event;
use serde_json::{Value, json};

use crate::fake_model::FakeModel;

fn event(kind: &str, mut data: Value) -> Event {
    data["type"] = json!(kind);
    Event::default().event(kind).data(data.to_string())
}

pub(crate) fn response_events(
    model: &FakeModel,
    calls: &[(String, String)],
    text: Option<String>,
    input_tokens: u64,
    cached: u64,
) -> Vec<(usize, Event)> {
    let id = format!("resp_fake_{:016x}", model_id(model));
    let mut events = vec![(
        0,
        event(
            "response.created",
            json!({ "response": { "id": id, "status": "in_progress" } }),
        ),
    )];
    let mut output = Vec::new();
    let mut output_tokens = 0;
    if let Some(text) = text {
        let item_id = format!("msg_{:016x}", model_id(model));
        events.push((0, event("response.output_item.added", json!({
            "output_index": 0,
            "item": { "type": "message", "id": item_id, "role": "assistant", "content": [] },
        }))));
        for word in text.split(' ') {
            output_tokens += 1;
            events.push((1, event("response.output_text.delta", json!({
                "item_id": item_id, "output_index": 0, "content_index": 0, "delta": format!("{word} "),
            }))));
        }
        let item = json!({
            "type": "message", "id": item_id, "role": "assistant", "status": "completed",
            "content": [{ "type": "output_text", "text": text, "annotations": [] }],
        });
        events.push((
            0,
            event(
                "response.output_item.done",
                json!({ "output_index": 0, "item": item }),
            ),
        ));
        output.push(item);
    }
    for (index, (name, arguments)) in calls.iter().enumerate() {
        let item_id = format!("fc_{:016x}", model_id(model));
        let call_id = format!("call_{:016x}", model_id(model));
        let tokens = arguments.len() / 4 + 8;
        output_tokens += tokens;
        events.push((0, event("response.output_item.added", json!({
            "output_index": index,
            "item": { "type": "function_call", "id": item_id, "call_id": call_id, "name": name, "arguments": "" },
        }))));
        events.push((
            tokens,
            event(
                "response.function_call_arguments.delta",
                json!({
                    "item_id": item_id, "output_index": index, "delta": arguments,
                }),
            ),
        ));
        events.push((
            0,
            event(
                "response.function_call_arguments.done",
                json!({
                    "item_id": item_id, "output_index": index, "arguments": arguments,
                }),
            ),
        ));
        let item = json!({
            "type": "function_call", "id": item_id, "call_id": call_id, "name": name,
            "arguments": arguments, "status": "completed",
        });
        events.push((
            0,
            event(
                "response.output_item.done",
                json!({ "output_index": index, "item": item }),
            ),
        ));
        output.push(item);
    }
    let output_tokens = u64::try_from(output_tokens).unwrap_or(u64::MAX);
    model
        .stats
        .output_tokens
        .fetch_add(output_tokens, Ordering::Relaxed);
    events.push((
        0,
        event(
            "response.completed",
            json!({ "response": {
        "id": id,
        "status": "completed",
        "output": output,
        "usage": {
            "input_tokens": input_tokens,
            "input_tokens_details": { "cached_tokens": cached },
            "output_tokens": output_tokens,
            "output_tokens_details": { "reasoning_tokens": 0 },
            "total_tokens": input_tokens + output_tokens,
        },
    } }),
        ),
    ));
    events
}

fn model_id(model: &FakeModel) -> u64 {
    model.next_id()
}
