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
    pub(crate) calls: &'a [(String, String, String)],
    pub(crate) text: Option<String>,
    pub(crate) input_tokens: u64,
    pub(crate) cached: u64,
    pub(crate) tail_tokens: usize,
    pub(crate) schedule: Option<Schedule>,
}

/// When each part of a replayed response arrives, in milliseconds after its first
/// token: each tool call when its arguments are complete, and the end of the response.
/// With a schedule the first element of each event is its delay after the previous
/// event in microseconds, instead of a token count.
pub(crate) struct Schedule {
    pub(crate) call_ready_ms: Vec<f64>,
    pub(crate) end_ms: f64,
}

fn is_call_event(kind: &str, data: &Value) -> bool {
    kind.starts_with("response.function_call_arguments")
        || (kind.starts_with("response.output_item")
            && data["item"]["type"].as_str() == Some("function_call"))
}

const MIN_ARGUMENT_CHUNKS: usize = 6;
const MAX_ARGUMENT_CHUNKS: usize = 10;
const CHARS_PER_EXTRA_CHUNK: usize = 48;

fn split_arguments(arguments: &str) -> Vec<&str> {
    let wanted =
        (MIN_ARGUMENT_CHUNKS + arguments.len() / CHARS_PER_EXTRA_CHUNK).min(MAX_ARGUMENT_CHUNKS);
    let size = arguments.len().div_ceil(wanted).max(1);
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < arguments.len() {
        let mut end = (start + size).min(arguments.len());
        while !arguments.is_char_boundary(end) {
            end += 1;
        }
        chunks.push(&arguments[start..end]);
        start = end;
    }
    if chunks.is_empty() {
        chunks.push("");
    }
    chunks
}

fn is_arguments_delta(kind: &str) -> bool {
    kind == "response.function_call_arguments.delta"
}

fn deltas_per_call(events: &[(usize, &'static str, Value)]) -> Vec<usize> {
    let mut counts = Vec::new();
    for (_, kind, _) in events {
        if *kind == "response.output_item.added" {
            counts.push(0);
        } else if is_arguments_delta(kind)
            && let Some(count) = counts.last_mut()
        {
            *count += 1;
        }
    }
    counts
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    reason = "delays of at most minutes, in whole microseconds, and a few hundred events"
)]
fn scheduled(events: &mut [(usize, &'static str, Value)], schedule: &Schedule) {
    let end = schedule.end_ms.max(0.0);
    let ready = |call: usize| {
        schedule
            .call_ready_ms
            .get(call.saturating_sub(1))
            .copied()
            .unwrap_or(end)
            .clamp(0.0, end)
    };
    let text_events = events
        .iter()
        .filter(|(_, kind, data)| {
            *kind != "response.created"
                && *kind != "response.completed"
                && !is_call_event(kind, data)
        })
        .count();
    let call_deltas = deltas_per_call(events);
    let first_window_start = if text_events > 0 { ready(1) * 0.5 } else { 0.0 };
    let text_end = if call_deltas.is_empty() {
        end
    } else {
        first_window_start
    };
    let window_start = |call: usize| {
        if call <= 1 {
            first_window_start
        } else {
            ready(call - 1)
        }
    };
    let mut call = 0_usize;
    let mut delta = 0_usize;
    let mut text = 0_usize;
    let mut previous = 0.0_f64;
    for (delay, kind, data) in events.iter_mut() {
        let at = if *kind == "response.created" {
            0.0
        } else if *kind == "response.completed" {
            end
        } else if is_call_event(kind, data) {
            if *kind == "response.output_item.added" {
                call += 1;
                delta = 0;
                window_start(call)
            } else if is_arguments_delta(kind) {
                delta += 1;
                let total = call_deltas
                    .get(call.saturating_sub(1))
                    .copied()
                    .unwrap_or(1)
                    .max(1);
                let start = window_start(call);
                start + (ready(call) - start).max(0.0) * delta as f64 / total as f64
            } else {
                ready(call)
            }
        } else {
            text += 1;
            text_end * text as f64 / text_events.max(1) as f64
        };
        let at = at.max(previous);
        *delay = ((at - previous) * 1000.0).round() as usize;
        previous = at;
    }
}

struct FunctionCall<'a> {
    item_id: &'a str,
    index: usize,
    call_id: &'a str,
    name: &'a str,
    arguments: &'a str,
    tokens: usize,
}

impl FunctionCall<'_> {
    fn push_events(&self, events: &mut Vec<(usize, &'static str, Value)>) -> Value {
        let Self {
            item_id,
            index,
            call_id,
            name,
            arguments,
            tokens,
        } = *self;
        events.push((0, "response.output_item.added", json!({
            "output_index": index,
            "item": { "type": "function_call", "id": item_id, "call_id": call_id, "name": name, "arguments": "", "status": "in_progress" },
        })));
        let chunks = split_arguments(arguments);
        let per_chunk = tokens / chunks.len();
        let first_chunk = tokens - per_chunk * (chunks.len() - 1);
        for (position, chunk) in chunks.iter().enumerate() {
            let delay = if position == 0 {
                first_chunk
            } else {
                per_chunk
            };
            events.push((
                delay,
                "response.function_call_arguments.delta",
                json!({ "item_id": item_id, "output_index": index, "delta": chunk }),
            ));
        }
        events.push((
            0,
            "response.function_call_arguments.done",
            json!({ "item_id": item_id, "output_index": index, "arguments": arguments }),
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
        item
    }
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
    for (index, (name, arguments, call_id)) in reply.calls.iter().enumerate() {
        let index = index + offset;
        let item_id = format!("fc_{:016x}", model_id(model));
        let tokens = arguments.len() / 4 + 8;
        output_tokens += tokens;
        let call = FunctionCall {
            item_id: &item_id,
            index,
            call_id,
            name,
            arguments,
            tokens,
        };
        output.push(call.push_events(&mut events));
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
    events.push((
        reply.tail_tokens,
        "response.completed",
        json!({ "response": done }),
    ));
    if let Some(schedule) = &reply.schedule {
        scheduled(&mut events, schedule);
    }
    numbered(events)
}

fn model_id(model: &FakeModel) -> u64 {
    model.next_id()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call_events(arguments: &str) -> Vec<(usize, &'static str, Value)> {
        let mut events = vec![(0, "response.created", json!({}))];
        FunctionCall {
            item_id: "fc_1",
            index: 0,
            call_id: "call_1",
            name: "send",
            arguments,
            tokens: 40,
        }
        .push_events(&mut events);
        events.push((0, "response.completed", json!({})));
        events
    }

    fn deltas(events: &[(usize, &'static str, Value)]) -> Vec<String> {
        events
            .iter()
            .filter(|(_, kind, _)| is_arguments_delta(kind))
            .map(|(_, _, data)| data["delta"].as_str().unwrap_or_default().to_owned())
            .collect()
    }

    #[test]
    fn arguments_stream_in_six_to_ten_chunks_that_add_up_to_the_arguments() {
        for arguments in [
            r#"{"text":"short"}"#,
            &format!(r#"{{"text":"{}"}}"#, "é and more words ".repeat(20)),
            &format!(r#"{{"text":"{}"}}"#, "x".repeat(4000)),
        ] {
            let events = call_events(arguments);
            let chunks = deltas(&events);
            assert!((6..=10).contains(&chunks.len()), "{} chunks", chunks.len());
            assert_eq!(chunks.concat(), arguments);
        }
        let tokens: usize = call_events(r#"{"text":"short"}"#)
            .iter()
            .filter(|(_, kind, _)| is_arguments_delta(kind))
            .map(|(delay, _, _)| delay)
            .sum();
        assert_eq!(tokens, 40);
    }

    #[test]
    fn scheduled_arguments_spread_up_to_the_moment_the_call_is_ready() {
        let mut events = call_events(r#"{"text":"a reply that takes a while to write"}"#);
        scheduled(
            &mut events,
            &Schedule {
                call_ready_ms: vec![1000.0],
                end_ms: 1200.0,
            },
        );
        let mut at = 0.0;
        let mut times = Vec::new();
        for (delay, kind, _) in &events {
            #[expect(clippy::cast_precision_loss, reason = "small test delays")]
            let step = *delay as f64 / 1000.0;
            at += step;
            times.push((*kind, at));
        }
        let delta_times: Vec<f64> = times
            .iter()
            .filter(|(kind, _)| is_arguments_delta(kind))
            .map(|(_, at)| *at)
            .collect();
        assert!(delta_times.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(delta_times[0] > 0.0 && delta_times[0] < 1000.0);
        assert!((delta_times.last().copied().unwrap_or_default() - 1000.0).abs() < 1.0);
        let done_at = |wanted: &str| {
            times
                .iter()
                .find(|(kind, _)| *kind == wanted)
                .map(|(_, at)| *at)
                .unwrap_or_default()
        };
        assert!((done_at("response.function_call_arguments.done") - 1000.0).abs() < 1.0);
        assert!((done_at("response.completed") - 1200.0).abs() < 1.0);
    }
}
