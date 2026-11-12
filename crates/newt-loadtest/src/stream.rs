use std::sync::atomic::Ordering;

use axum::response::sse::Event;
use serde_json::{Value, json};

use crate::fake_model::FakeModel;

fn event(kind: &str, mut data: Value) -> Event {
    data["type"] = json!(kind);
    Event::default().event(kind).data(data.to_string())
}

fn numbered(events: Vec<(usize, &'static str, Value)>) -> Vec<(usize, Event)> {
    events
        .into_iter()
        .enumerate()
        .map(|(sequence, (tokens, kind, mut data))| {
            data["sequence_number"] = json!(sequence);
            (tokens, event(kind, data))
        })
        .collect()
}

pub(crate) struct Reply<'a> {
    pub(crate) model_name: &'a str,
    pub(crate) calls: &'a [(String, String)],
    pub(crate) text: Option<String>,
    pub(crate) input_tokens: u64,
    pub(crate) cached: u64,
}

pub(crate) fn response_events(model: &FakeModel, reply: Reply<'_>) -> Vec<(usize, Event)> {
    let id = format!("resp_fake_{:016x}", model_id(model));
    let created_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let header = json!({ "id": id, "object": "response", "created_at": created_at, "model": reply.model_name });
    let mut response = header.clone();
    response["status"] = json!("in_progress");
    response["output"] = json!([]);
    let mut events: Vec<(usize, &'static str, Value)> =
        vec![(0, "response.created", json!({ "response": response }))];
    let mut output = Vec::new();
    let mut output_tokens = 0;
    if let Some(text) = reply.text {
        let item_id = format!("msg_{:016x}", model_id(model));
        events.push((0, "response.output_item.added", json!({
            "output_index": 0,
            "item": { "type": "message", "id": item_id, "role": "assistant", "status": "in_progress", "content": [] },
        })));
        for word in text.split(' ') {
            output_tokens += 1;
            events.push((1, "response.output_text.delta", json!({
                "item_id": item_id, "output_index": 0, "content_index": 0, "delta": format!("{word} "), "logprobs": [],
            })));
        }
        let item = json!({
            "type": "message", "id": item_id, "role": "assistant", "status": "completed",
            "content": [{ "type": "output_text", "text": text, "annotations": [], "logprobs": [] }],
        });
        events.push((
            0,
            "response.output_item.done",
            json!({ "output_index": 0, "item": item }),
        ));
        output.push(item);
    }
    let offset = output.len();
    for (index, (name, arguments)) in reply.calls.iter().enumerate() {
        let index = index + offset;
        let item_id = format!("fc_{:016x}", model_id(model));
        let call_id = format!("call_{:016x}", model_id(model));
        let tokens = arguments.len() / 4 + 8;
        output_tokens += tokens;
        events.push((0, "response.output_item.added", json!({
            "output_index": index,
            "item": { "type": "function_call", "id": item_id, "call_id": call_id, "name": name, "arguments": "", "status": "in_progress" },
        })));
        events.push((
            tokens,
            "response.function_call_arguments.delta",
            json!({
                "item_id": item_id, "output_index": index, "delta": arguments,
            }),
        ));
        events.push((
            0,
            "response.function_call_arguments.done",
            json!({
                "item_id": item_id, "output_index": index, "arguments": arguments,
            }),
        ));
        let item = json!({
            "type": "function_call", "id": item_id, "call_id": call_id, "name": name,
            "arguments": arguments, "status": "completed",
        });
        events.push((
            0,
            "response.output_item.done",
            json!({ "output_index": index, "item": item }),
        ));
        output.push(item);
    }
    let output_tokens = u64::try_from(output_tokens).unwrap_or(u64::MAX);
    model
        .stats
        .output_tokens
        .fetch_add(output_tokens, Ordering::Relaxed);
    let mut done = header;
    done["status"] = json!("completed");
    done["output"] = json!(output);
    done["usage"] = json!({
        "input_tokens": reply.input_tokens,
        "input_tokens_details": { "cached_tokens": reply.cached },
        "output_tokens": output_tokens,
        "output_tokens_details": { "reasoning_tokens": 0 },
        "total_tokens": reply.input_tokens + output_tokens,
    });
    events.push((0, "response.completed", json!({ "response": done })));
    numbered(events)
}

fn model_id(model: &FakeModel) -> u64 {
    model.next_id()
}
