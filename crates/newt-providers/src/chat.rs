//! The Chat Completions protocol, which most hosts besides the first-party
//! APIs speak: vLLM, Ollama, Together, Groq and others.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::sse::SseEvent;
use crate::{Decoded, Emitter, decode};
use newt_core::message::{Message, Part, Role, ToolCall};
use newt_core::prompt::{Effort, ModelRequest};
use newt_core::provider::{ProviderError, StopReason, Usage, tool_arguments};

const DONE: &str = "[DONE]";

pub(super) fn request_body(request: &ModelRequest<'_>) -> Value {
    let prompt = request.prompt;
    let mut messages = vec![json!({ "role": "system", "content": prompt.system })];
    messages.extend(prompt.messages().flat_map(messages_of));
    let mut body = Map::new();
    body.insert("model".into(), json!(request.model));
    body.insert("stream".into(), json!(true));
    body.insert("stream_options".into(), json!({ "include_usage": true }));
    body.insert("messages".into(), Value::Array(messages));
    body.insert("max_tokens".into(), json!(request.max_output_tokens));
    if !prompt.tools.is_empty() {
        let tools: Vec<Value> = prompt
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.input_schema,
                    },
                })
            })
            .collect();
        body.insert("tools".into(), Value::Array(tools));
    }
    if let Some(effort) = request.effort {
        let effort = match effort {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
        };
        body.insert("reasoning_effort".into(), json!(effort));
    }
    Value::Object(body)
}

/// One newt message as chat messages: tool results become `tool` messages
/// right after the assistant message that called them, and reasoning, which
/// the protocol cannot carry back, is left out.
fn messages_of(message: &Message) -> Vec<Value> {
    let mut texts = Vec::new();
    let mut calls = Vec::new();
    let mut out = Vec::new();
    for part in &message.parts {
        match part {
            Part::Text { text } if !text.is_empty() => texts.push(text.as_str()),
            Part::ToolCall(call) => calls.push(json!({
                "id": call.id,
                "type": "function",
                "function": { "name": call.name, "arguments": call.arguments.to_string() },
            })),
            Part::ToolResult(result) => out.push(json!({
                "role": "tool",
                "tool_call_id": result.call_id,
                "content": if result.is_error {
                    format!("Error: {}", result.content)
                } else {
                    result.content.clone()
                },
            })),
            Part::Text { .. } | Part::Reasoning(_) => {}
        }
    }
    let text = texts.join("\n\n");
    match message.role {
        Role::User if !text.is_empty() => out.push(json!({ "role": "user", "content": text })),
        Role::User => {}
        Role::Assistant if text.is_empty() && calls.is_empty() => {}
        Role::Assistant => {
            let mut assistant = json!({
                "role": "assistant",
                "content": if text.is_empty() { Value::Null } else { Value::from(text) },
            });
            if !calls.is_empty() {
                assistant["tool_calls"] = Value::Array(calls);
            }
            out.push(assistant);
        }
    }
    out
}

#[derive(Debug, Deserialize)]
struct Chunk {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<ChunkUsage>,
    #[serde(default)]
    error: Option<ChunkError>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    #[serde(default)]
    delta: Delta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    refusal: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<CallDelta>>,
}

#[derive(Debug, Deserialize)]
struct CallDelta {
    #[serde(default)]
    index: Option<usize>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<FunctionDelta>,
}

#[derive(Debug, Deserialize)]
struct FunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChunkUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    prompt_tokens_details: Option<PromptDetails>,
    #[serde(default)]
    completion_tokens_details: Option<CompletionDetails>,
}

#[derive(Debug, Deserialize)]
struct PromptDetails {
    #[serde(default)]
    cached_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct CompletionDetails {
    #[serde(default)]
    reasoning_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct ChunkError {
    message: String,
    #[serde(default)]
    code: Option<Value>,
    #[serde(default, rename = "type")]
    kind: Option<String>,
}

#[derive(Debug, Default)]
struct PendingCall {
    id: String,
    name: String,
    arguments: String,
    announced: bool,
    ready: bool,
}

#[derive(Debug, Default)]
pub(super) struct Decoder {
    text: String,
    refused: bool,
    calls: BTreeMap<usize, PendingCall>,
    usage: Usage,
    stop: Option<StopReason>,
    response_id: Option<String>,
}

impl Decoder {
    fn release_calls_before(&mut self, index: usize, emit: &mut Emitter<'_>) {
        for call in self.calls.range_mut(..index).map(|(_, call)| call) {
            if call.announced && !call.ready {
                call.ready = true;
                emit.tool_ready(&ToolCall {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: tool_arguments(&call.arguments),
                });
            }
        }
    }
}

impl super::Decoder for Decoder {
    fn handle(&mut self, event: &SseEvent, emit: &mut Emitter<'_>) -> Result<bool, ProviderError> {
        if event.data.trim() == DONE {
            self.release_calls_before(usize::MAX, emit);
            return Ok(true);
        }
        let chunk: Chunk = decode(event)?;
        if let Some(error) = chunk.error {
            let kind = error
                .kind
                .or_else(|| {
                    error
                        .code
                        .map(|code| code.to_string().trim_matches('"').to_owned())
                })
                .unwrap_or_else(|| "error".to_owned());
            return Err(ProviderError::Api {
                kind,
                message: error.message,
            });
        }
        if self.response_id.is_none() {
            self.response_id = chunk.id;
        }
        if let Some(usage) = chunk.usage {
            self.usage = Usage {
                input_tokens: usage.prompt_tokens,
                cache_read_tokens: usage.prompt_tokens_details.map_or(0, |d| d.cached_tokens),
                cache_write_tokens: 0,
                output_tokens: usage.completion_tokens,
                reasoning_tokens: usage
                    .completion_tokens_details
                    .map_or(0, |d| d.reasoning_tokens),
            };
        }
        for choice in chunk.choices {
            if let Some(content) = choice.delta.content.filter(|content| !content.is_empty()) {
                emit.text(&content);
                self.text.push_str(&content);
            }
            if let Some(refusal) = choice.delta.refusal.filter(|refusal| !refusal.is_empty()) {
                emit.text(&refusal);
                self.refused = true;
                self.text.push_str(&refusal);
            }
            for delta in choice.delta.tool_calls.unwrap_or_default() {
                let last = self.calls.keys().next_back().copied();
                let index = match (delta.index, last) {
                    (Some(index), _) => index,
                    (None, Some(last)) if delta.id.is_none() => last,
                    (None, Some(last)) => last.saturating_add(1),
                    (None, None) => 0,
                };
                self.release_calls_before(index, emit);
                let call = self.calls.entry(index).or_default();
                if let Some(id) = delta.id {
                    call.id = id;
                }
                if let Some(function) = delta.function {
                    if let Some(name) = function.name {
                        call.name.push_str(&name);
                    }
                    if let Some(arguments) = function.arguments {
                        call.arguments.push_str(&arguments);
                        if call.announced && !arguments.is_empty() {
                            emit.tool_input_delta(&call.id, &arguments);
                        }
                    }
                }
                if !call.announced && !call.id.is_empty() && !call.name.is_empty() {
                    call.announced = true;
                    emit.tool_started(&call.id, &call.name);
                    if !call.arguments.is_empty() {
                        emit.tool_input_delta(&call.id, &call.arguments);
                    }
                }
            }
            if choice.finish_reason.is_some() {
                self.release_calls_before(usize::MAX, emit);
            }
            match choice.finish_reason.as_deref() {
                Some("length") => self.stop = Some(StopReason::MaxTokens),
                Some("content_filter") => self.refused = true,
                Some("stop" | "tool_calls" | "function_call") | None => {}
                Some(other) => self.stop = Some(StopReason::Other(other.to_owned())),
            }
        }
        Ok(false)
    }

    fn finish(self) -> Result<Decoded, ProviderError> {
        let mut parts = Vec::new();
        if !self.text.is_empty() {
            parts.push(Part::text(self.text));
        }
        if let Some((index, _)) = self.calls.iter().find(|(_, call)| !call.announced) {
            return Err(ProviderError::Protocol(format!(
                "tool call {index} ended without an id and a name"
            )));
        }
        let has_tool_call = !self.calls.is_empty();
        parts.extend(self.calls.into_values().map(|call| {
            Part::ToolCall(ToolCall {
                id: call.id,
                name: call.name,
                arguments: tool_arguments(&call.arguments),
            })
        }));
        let stop = self.stop.unwrap_or(if self.refused {
            StopReason::Refusal
        } else if has_tool_call {
            StopReason::ToolUse
        } else {
            StopReason::EndTurn
        });
        Ok(Decoded {
            parts,
            usage: self.usage,
            stop,
            response_id: self.response_id,
        })
    }
}
