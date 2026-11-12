use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::{Decoded, Emitter, ProviderError, StopReason, Usage, decode, tool_arguments};
use crate::message::{Message, Part, Reasoning, Role, ToolCall};
use crate::prompt::{Effort, ModelRequest};
use crate::sse::SseEvent;

pub(super) fn request_body(request: &ModelRequest<'_>) -> Value {
    let prompt = request.prompt;
    let input: Vec<Value> = prompt.messages().flat_map(message_items).collect();
    let mut body = Map::new();
    body.insert("model".into(), json!(request.model));
    body.insert("stream".into(), json!(true));
    body.insert("store".into(), json!(false));
    body.insert("instructions".into(), json!(prompt.system));
    body.insert("input".into(), Value::Array(input));
    body.insert("max_output_tokens".into(), json!(request.max_output_tokens));
    body.insert("prompt_cache_key".into(), json!(request.cache_key));
    if !prompt.tools.is_empty() {
        let tools: Vec<Value> = prompt
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.input_schema,
                    "strict": false,
                })
            })
            .collect();
        body.insert("tools".into(), Value::Array(tools));
        body.insert("parallel_tool_calls".into(), json!(true));
    }
    if let Some(effort) = request.effort {
        let effort = match effort {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
        };
        body.insert("reasoning".into(), json!({ "effort": effort }));
        body.insert("include".into(), json!(["reasoning.encrypted_content"]));
    }
    Value::Object(body)
}

fn message_items(message: &Message) -> Vec<Value> {
    let mut items = Vec::new();
    let mut user_text = Vec::new();
    for part in &message.parts {
        match (message.role, part) {
            (_, Part::Text { text }) if text.is_empty() => {}
            (Role::User, Part::Text { text }) => {
                user_text.push(json!({ "type": "input_text", "text": text }));
            }
            (Role::User, Part::ToolResult(result)) => {
                let output = if result.is_error {
                    format!("Error: {}", result.content)
                } else {
                    result.content.clone()
                };
                items.push(json!({
                    "type": "function_call_output",
                    "call_id": result.call_id,
                    "output": output,
                }));
            }
            (Role::Assistant, Part::Text { text }) => {
                items.push(json!({ "role": "assistant", "content": text }));
            }
            (Role::Assistant, Part::ToolCall(call)) => items.push(json!({
                "type": "function_call",
                "call_id": call.id,
                "name": call.name,
                "arguments": call.arguments.to_string(),
            })),
            (Role::Assistant, Part::Reasoning(Reasoning::OpenaiReasoning { item })) => {
                items.push(item.clone());
            }
            _ => {}
        }
    }
    if !user_text.is_empty() {
        items.push(json!({ "role": "user", "content": user_text }));
    }
    items
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum Event {
    #[serde(rename = "response.output_item.added")]
    ItemAdded { item: AddedItem },
    #[serde(rename = "response.output_text.delta")]
    TextDelta,
    #[serde(rename = "response.output_item.done")]
    ItemDone { item: Value },
    #[serde(rename = "response.completed")]
    Completed { response: Completion },
    #[serde(rename = "response.incomplete")]
    Incomplete { response: Completion },
    #[serde(rename = "response.failed")]
    Failed { response: Failure },
    #[serde(rename = "error")]
    Error {
        #[serde(default)]
        code: Option<String>,
        message: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AddedItem {
    FunctionCall {
        call_id: String,
        name: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum DoneItem {
    Message {
        content: Vec<ContentPart>,
    },
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    Reasoning,
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentPart {
    OutputText {
        text: String,
    },
    Refusal {
        refusal: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct Completion {
    #[serde(default)]
    id: Option<String>,
    usage: Option<ResponseUsage>,
    #[serde(default)]
    incomplete_details: Option<IncompleteDetails>,
}

#[derive(Debug, Deserialize)]
struct ResponseUsage {
    input_tokens: u64,
    #[serde(default)]
    input_tokens_details: Option<InputDetails>,
    output_tokens: u64,
    #[serde(default)]
    output_tokens_details: Option<OutputDetails>,
}

#[derive(Debug, Deserialize)]
struct InputDetails {
    #[serde(default)]
    cached_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct OutputDetails {
    #[serde(default)]
    reasoning_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct IncompleteDetails {
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Failure {
    error: Option<FailureError>,
}

#[derive(Debug, Deserialize)]
struct FailureError {
    #[serde(default)]
    code: Option<String>,
    message: String,
}

#[derive(Debug, Default)]
pub(super) struct Decoder {
    parts: Vec<Part>,
    usage: Usage,
    stop: Option<StopReason>,
    refused: bool,
    response_id: Option<String>,
}

impl Decoder {
    fn complete(&mut self, completion: Completion) {
        self.response_id = completion.id;
        if let Some(usage) = completion.usage {
            self.usage = Usage {
                input_tokens: usage.input_tokens,
                cache_read_tokens: usage.input_tokens_details.map_or(0, |d| d.cached_tokens),
                cache_write_tokens: 0,
                output_tokens: usage.output_tokens,
                reasoning_tokens: usage
                    .output_tokens_details
                    .map_or(0, |d| d.reasoning_tokens),
            };
        }
        if let Some(reason) = completion.incomplete_details.and_then(|d| d.reason) {
            self.stop = Some(if reason == "max_output_tokens" {
                StopReason::MaxTokens
            } else {
                StopReason::Other(reason)
            });
        }
    }

    fn collect(&mut self, event: &SseEvent, item: Value) -> Result<(), ProviderError> {
        let done = DoneItem::deserialize(&item).map_err(|source| ProviderError::Decode {
            event: event
                .event
                .clone()
                .unwrap_or_else(|| "response.output_item.done".to_owned()),
            snippet: event.data.chars().take(200).collect(),
            source,
        })?;
        match done {
            DoneItem::Message { content } => {
                let mut text = String::new();
                for part in content {
                    match part {
                        ContentPart::OutputText { text: more } => text.push_str(&more),
                        ContentPart::Refusal { refusal } => {
                            self.refused = true;
                            text.push_str(&refusal);
                        }
                        ContentPart::Other => {}
                    }
                }
                if !text.is_empty() {
                    self.parts.push(Part::text(text));
                }
            }
            DoneItem::FunctionCall {
                call_id,
                name,
                arguments,
            } => self.parts.push(Part::ToolCall(ToolCall {
                id: call_id,
                name,
                arguments: tool_arguments(&arguments),
            })),
            DoneItem::Reasoning => self
                .parts
                .push(Part::Reasoning(Reasoning::OpenaiReasoning { item })),
            DoneItem::Other => {}
        }
        Ok(())
    }
}

impl super::Decoder for Decoder {
    fn handle(&mut self, event: &SseEvent, emit: &mut Emitter<'_>) -> Result<bool, ProviderError> {
        match decode::<Event>(event)? {
            Event::ItemAdded {
                item: AddedItem::FunctionCall { call_id, name },
            } => emit.tool_started(&call_id, &name),
            Event::TextDelta => emit.output(),
            Event::ItemDone { item } => self.collect(event, item)?,
            Event::Completed { response } | Event::Incomplete { response } => {
                self.complete(response);
                return Ok(true);
            }
            Event::Failed { response } => {
                let (kind, message) = response.error.map_or_else(
                    || ("response_failed".to_owned(), "response failed".to_owned()),
                    |error| {
                        (
                            error.code.unwrap_or_else(|| "response_failed".to_owned()),
                            error.message,
                        )
                    },
                );
                return Err(ProviderError::Api { kind, message });
            }
            Event::Error { code, message } => {
                return Err(ProviderError::Api {
                    kind: code.unwrap_or_else(|| "error".to_owned()),
                    message,
                });
            }
            Event::ItemAdded { .. } | Event::Other => {}
        }
        Ok(false)
    }

    fn finish(self) -> Result<Decoded, ProviderError> {
        let has_tool_call = self
            .parts
            .iter()
            .any(|part| matches!(part, Part::ToolCall(_)));
        let stop = self.stop.unwrap_or(if self.refused {
            StopReason::Refusal
        } else if has_tool_call {
            StopReason::ToolUse
        } else {
            StopReason::EndTurn
        });
        Ok(Decoded {
            parts: self.parts,
            usage: self.usage,
            stop,
            response_id: self.response_id,
        })
    }
}
