use newt_core::message::Part;
use newt_core::provider::{Protocol, StopReason, StreamEvent};
use serde_json::{Value, json};

use super::{Fixture, Outcome, check_tool_stream, run_chunks, single_chunk};

fn run(protocol: Protocol, path: &'static str, lines: &[String]) -> Outcome {
    let fixture = Fixture {
        protocol,
        path,
        body: "",
    };
    let text = lines.concat();
    run_chunks(fixture, single_chunk(&text))
}

fn chat(chunks: &[Value], done: bool) -> Outcome {
    let mut lines: Vec<String> = chunks
        .iter()
        .map(|chunk| format!("data: {chunk}\n\n"))
        .collect();
    if done {
        lines.push("data: [DONE]\n\n".to_owned());
    }
    run(Protocol::OpenaiChat, "/chat/completions", &lines)
}

fn event(name: &str, data: &Value) -> String {
    format!("event: {name}\ndata: {data}\n\n")
}

fn anthropic(events: &[(&str, Value)]) -> Outcome {
    let lines: Vec<String> = events
        .iter()
        .map(|(name, data)| event(name, data))
        .collect();
    run(Protocol::AnthropicMessages, "/v1/messages", &lines)
}

fn responses(events: &[(&str, Value)]) -> Outcome {
    let lines: Vec<String> = events
        .iter()
        .map(|(name, data)| event(name, data))
        .collect();
    run(Protocol::OpenaiResponses, "/responses", &lines)
}

fn tool_calls(outcome: &Outcome) -> Vec<(String, String, Value)> {
    let summary = outcome.result.as_ref().unwrap_or_else(|error| {
        panic!("expected a response, got {error}");
    });
    summary
        .parts
        .iter()
        .filter_map(|part| match part {
            Part::ToolCall(call) => {
                Some((call.id.clone(), call.name.clone(), call.arguments.clone()))
            }
            _ => None,
        })
        .collect()
}

fn delta(delta: Value, finish: Value) -> Value {
    let mut chunk = json!({ "id": "chatcmpl_x", "choices": [{ "index": 0 }] });
    chunk["choices"][0]["delta"] = delta;
    chunk["choices"][0]["finish_reason"] = finish;
    chunk
}

fn call_delta(index: Option<u64>, id: Option<&str>, name: Option<&str>, arguments: &str) -> Value {
    let mut call = json!({ "type": "function", "function": { "arguments": arguments } });
    if let Some(index) = index {
        call["index"] = json!(index);
    }
    if let Some(id) = id {
        call["id"] = json!(id);
    }
    if let Some(name) = name {
        call["function"]["name"] = json!(name);
    }
    json!({ "tool_calls": [call] })
}

#[test]
fn chat_accepts_null_content_and_null_tool_calls_in_deltas() {
    let outcome = chat(
        &[
            delta(
                json!({ "role": "assistant", "content": null, "tool_calls": null }),
                Value::Null,
            ),
            delta(
                json!({ "content": "Hello", "tool_calls": null }),
                Value::Null,
            ),
            delta(json!({}), json!("stop")),
        ],
        true,
    );
    let summary = outcome.result.expect("a response");
    assert_eq!(summary.parts, vec![Part::text("Hello")]);
}

#[test]
fn chat_readies_every_call_when_the_stream_ends_without_a_finish_reason() {
    let outcome = chat(
        &[
            delta(
                call_delta(Some(0), Some("call_1"), Some("read_file"), "{\"paths\":[]}"),
                Value::Null,
            ),
            delta(
                call_delta(Some(1), Some("call_2"), Some("end_turn"), "{}"),
                Value::Null,
            ),
        ],
        true,
    );
    assert_eq!(check_tool_stream(&outcome), Ok(()));
    let readied = outcome
        .events
        .iter()
        .filter(|event| matches!(event, StreamEvent::ToolCallReady(_)))
        .count();
    assert_eq!(readied, 2);
}

#[test]
fn chat_keeps_parallel_calls_apart_when_the_host_omits_the_index() {
    let outcome = chat(
        &[
            delta(
                call_delta(None, Some("call_1"), Some("read_file"), "{\"paths\":[]}"),
                Value::Null,
            ),
            delta(
                call_delta(None, Some("call_2"), Some("end_turn"), "{}"),
                Value::Null,
            ),
            delta(json!({}), json!("tool_calls")),
        ],
        true,
    );
    let calls = tool_calls(&outcome);
    assert_eq!(
        calls,
        vec![
            (
                "call_1".to_owned(),
                "read_file".to_owned(),
                json!({ "paths": [] })
            ),
            ("call_2".to_owned(), "end_turn".to_owned(), json!({})),
        ]
    );
}

#[test]
fn chat_never_returns_a_call_without_an_id_or_a_name() {
    let outcome = chat(
        &[
            delta(
                call_delta(Some(0), None, Some("read_file"), "{}"),
                Value::Null,
            ),
            delta(call_delta(Some(1), Some("call_2"), None, "{}"), Value::Null),
            delta(json!({}), json!("tool_calls")),
        ],
        true,
    );
    if let Ok(summary) = &outcome.result {
        for part in &summary.parts {
            if let Part::ToolCall(call) = part {
                assert!(!call.id.is_empty() && !call.name.is_empty(), "{call:?}");
            }
        }
    }
}

fn message_start() -> (&'static str, Value) {
    (
        "message_start",
        json!({ "type": "message_start", "message": { "id": "msg_x", "usage": { "input_tokens": 5 } } }),
    )
}

#[test]
fn anthropic_ignores_server_tool_blocks_citations_pings_and_unknown_deltas() {
    let outcome = anthropic(&[
        message_start(),
        ("ping", json!({ "type": "ping" })),
        (
            "content_block_start",
            json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "server_tool_use", "id": "srv_1", "name": "web_search", "input": {} } }),
        ),
        (
            "content_block_delta",
            json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "input_json_delta", "partial_json": "{\"q\":1}" } }),
        ),
        (
            "content_block_stop",
            json!({ "type": "content_block_stop", "index": 0 }),
        ),
        (
            "content_block_start",
            json!({ "type": "content_block_start", "index": 1, "content_block": { "type": "text", "text": "" } }),
        ),
        (
            "content_block_delta",
            json!({ "type": "content_block_delta", "index": 1, "delta": { "type": "citations_delta", "citation": {} } }),
        ),
        (
            "content_block_delta",
            json!({ "type": "content_block_delta", "index": 1, "delta": { "type": "text_delta", "text": "Hi" } }),
        ),
        (
            "content_block_stop",
            json!({ "type": "content_block_stop", "index": 1 }),
        ),
        (
            "message_delta",
            json!({ "type": "message_delta", "delta": { "stop_reason": "pause_turn", "stop_sequence": null }, "usage": { "output_tokens": 3 } }),
        ),
        ("message_stop", json!({ "type": "message_stop" })),
    ]);
    let summary = outcome.result.expect("a response");
    assert_eq!(summary.parts, vec![Part::text("Hi")]);
    assert_eq!(summary.stop, StopReason::Other("pause_turn".to_owned()));
}

#[test]
fn anthropic_tool_use_without_input_deltas_has_empty_arguments() {
    let outcome = anthropic(&[
        message_start(),
        (
            "content_block_start",
            json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "tool_use", "id": "toolu_1", "name": "end_turn", "input": {} } }),
        ),
        (
            "content_block_delta",
            json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "input_json_delta", "partial_json": "" } }),
        ),
        (
            "content_block_stop",
            json!({ "type": "content_block_stop", "index": 0 }),
        ),
        (
            "message_delta",
            json!({ "type": "message_delta", "delta": { "stop_reason": "tool_use" }, "usage": { "output_tokens": 3 } }),
        ),
        ("message_stop", json!({ "type": "message_stop" })),
    ]);
    assert_eq!(check_tool_stream(&outcome), Ok(()));
    assert_eq!(
        tool_calls(&outcome),
        vec![("toolu_1".to_owned(), "end_turn".to_owned(), json!({}))]
    );
}

#[test]
fn anthropic_block_events_out_of_order_are_errors() {
    let outcome = anthropic(&[
        message_start(),
        (
            "content_block_stop",
            json!({ "type": "content_block_stop", "index": 3 }),
        ),
    ]);
    assert!(outcome.result.is_err());
    let outcome = anthropic(&[
        message_start(),
        (
            "content_block_start",
            json!({ "type": "content_block_start", "index": 5, "content_block": { "type": "text" } }),
        ),
    ]);
    assert!(outcome.result.is_err());
}

fn completed() -> (&'static str, Value) {
    (
        "response.completed",
        json!({ "type": "response.completed", "response": { "id": "resp_x", "status": "completed", "usage": { "input_tokens": 1, "output_tokens": 1, "input_tokens_details": null, "output_tokens_details": null } } }),
    )
}

#[test]
fn responses_readies_a_call_whose_item_has_no_id() {
    let outcome = responses(&[
        (
            "response.output_item.added",
            json!({ "type": "response.output_item.added", "output_index": 0, "item": { "type": "function_call", "call_id": "call_1", "name": "end_turn", "arguments": "" } }),
        ),
        (
            "response.output_item.done",
            json!({ "type": "response.output_item.done", "output_index": 0, "item": { "type": "function_call", "call_id": "call_1", "name": "end_turn", "arguments": "{}" } }),
        ),
        completed(),
    ]);
    assert_eq!(check_tool_stream(&outcome), Ok(()));
    assert_eq!(tool_calls(&outcome).len(), 1);
}

#[test]
fn responses_ignores_other_item_kinds_and_joins_message_parts() {
    let outcome = responses(&[
        (
            "response.output_item.added",
            json!({ "type": "response.output_item.added", "output_index": 0, "item": { "type": "web_search_call", "id": "ws_1" } }),
        ),
        (
            "response.output_item.done",
            json!({ "type": "response.output_item.done", "output_index": 0, "item": { "type": "web_search_call", "id": "ws_1", "status": "completed" } }),
        ),
        (
            "response.output_item.done",
            json!({ "type": "response.output_item.done", "output_index": 1, "item": { "type": "custom_tool_call", "call_id": "c", "name": "n", "input": "x" } }),
        ),
        (
            "response.reasoning_summary_text.delta",
            json!({ "type": "response.reasoning_summary_text.delta", "delta": "hmm" }),
        ),
        (
            "response.output_item.done",
            json!({ "type": "response.output_item.done", "output_index": 2, "item": { "type": "message", "content": [{ "type": "output_text", "text": "A", "annotations": [] }, { "type": "output_text", "text": "B" }] } }),
        ),
        completed(),
    ]);
    let summary = outcome.result.expect("a response");
    assert_eq!(summary.parts, vec![Part::text("AB")]);
}

#[test]
fn responses_drops_a_call_cut_off_by_the_token_limit() {
    let outcome = responses(&[
        (
            "response.output_item.added",
            json!({ "type": "response.output_item.added", "output_index": 0, "item": { "type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "end_turn", "arguments": "" } }),
        ),
        (
            "response.function_call_arguments.delta",
            json!({ "type": "response.function_call_arguments.delta", "item_id": "fc_1", "delta": "{\"recor" }),
        ),
        (
            "response.incomplete",
            json!({ "type": "response.incomplete", "response": { "id": "resp_x", "status": "incomplete", "incomplete_details": { "reason": "max_output_tokens" }, "usage": null } }),
        ),
    ]);
    let summary = outcome.result.expect("a response");
    assert_eq!(summary.stop, StopReason::MaxTokens);
    assert_eq!(summary.parts, Vec::<Part>::new());
}
