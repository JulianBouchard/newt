use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::sse::SseEvent;
use crate::{Decoded, Emitter, decode};
use newt_core::message::{Message, Part, Reasoning, Role, ToolCall};
use newt_core::prompt::{Effort, ModelRequest};
use newt_core::provider::{ProviderError, StopReason, Usage, tool_arguments};

pub(super) fn request_body(request: &ModelRequest<'_>) -> Value {
    let prompt = request.prompt;
    let cached = [prompt.last_history_index(), prompt.last_message_index()];
    let messages: Vec<Value> = prompt
        .messages()
        .enumerate()
        .filter_map(|(index, message)| message_json(message, cached.contains(&Some(index))))
        .collect();
    let mut tools: Vec<Value> = prompt
        .tools
        .iter()
        .map(|tool| {
            json!({
                "name": tool.name,
                "description": tool.description,
                "input_schema": tool.input_schema,
            })
        })
        .collect();
    if let Some(last) = tools.last_mut() {
        mark_cached(last);
    }

    let mut body = Map::new();
    body.insert("model".into(), json!(request.model));
    body.insert("max_tokens".into(), json!(request.max_output_tokens));
    body.insert("stream".into(), json!(true));
    if !prompt.system.is_empty() {
        body.insert(
            "system".into(),
            json!([{ "type": "text", "text": prompt.system, "cache_control": { "type": "ephemeral" } }]),
        );
    }
    body.insert("messages".into(), Value::Array(messages));
    if !tools.is_empty() {
        body.insert("tools".into(), Value::Array(tools));
    }
    if let Some(effort) = request.effort {
        let effort = match effort {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
        };
        body.insert("output_config".into(), json!({ "effort": effort }));
    }
    Value::Object(body)
}

fn message_json(message: &Message, cached: bool) -> Option<Value> {
    let mut blocks: Vec<Value> = message.parts.iter().filter_map(part_json).collect();
    if cached
        && let Some(last) = blocks.iter_mut().rev().find(|block| {
            !matches!(
                block["type"].as_str(),
                Some("thinking" | "redacted_thinking")
            )
        })
    {
        mark_cached(last);
    }
    if blocks.is_empty() {
        return None;
    }
    let role = match message.role {
        Role::User => "user",
        Role::Assistant => "assistant",
    };
    Some(json!({ "role": role, "content": blocks }))
}

fn part_json(part: &Part) -> Option<Value> {
    Some(match part {
        Part::Text { text } if text.is_empty() => return None,
        Part::Text { text } => json!({ "type": "text", "text": text }),
        Part::ToolCall(call) => {
            json!({ "type": "tool_use", "id": call.id, "name": call.name, "input": call.arguments })
        }
        Part::ToolResult(result) => json!({
            "type": "tool_result",
            "tool_use_id": result.call_id,
            "content": result.content,
            "is_error": result.is_error,
        }),
        Part::Reasoning(Reasoning::AnthropicThinking {
            thinking,
            signature,
        }) => json!({ "type": "thinking", "thinking": thinking, "signature": signature }),
        Part::Reasoning(Reasoning::AnthropicRedacted { data }) => {
            json!({ "type": "redacted_thinking", "data": data })
        }
        Part::Reasoning(Reasoning::OpenaiReasoning { .. }) => return None,
    })
}

fn mark_cached(block: &mut Value) {
    if let Some(object) = block.as_object_mut() {
        object.insert("cache_control".into(), json!({ "type": "ephemeral" }));
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Event {
    MessageStart {
        message: StartMessage,
    },
    ContentBlockStart {
        index: usize,
        content_block: StartBlock,
    },
    ContentBlockDelta {
        index: usize,
        delta: Delta,
    },
    ContentBlockStop {
        index: usize,
    },
    MessageDelta {
        delta: MessageDeltaBody,
        #[serde(default)]
        usage: Option<DeltaUsage>,
    },
    MessageStop,
    Error {
        error: ApiError,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct StartMessage {
    #[serde(default)]
    id: Option<String>,
    usage: StartUsage,
}

#[derive(Debug, Deserialize)]
#[expect(
    clippy::struct_field_names,
    reason = "mirrors the provider wire format"
)]
struct StartUsage {
    input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum StartBlock {
    Text,
    ToolUse {
        id: String,
        name: String,
    },
    Thinking,
    RedactedThinking {
        data: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum Delta {
    #[serde(rename = "text_delta")]
    Text { text: String },
    #[serde(rename = "input_json_delta")]
    InputJson { partial_json: String },
    #[serde(rename = "thinking_delta")]
    Thinking { thinking: String },
    #[serde(rename = "signature_delta")]
    Signature { signature: String },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct MessageDeltaBody {
    stop_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DeltaUsage {
    output_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    #[serde(rename = "type")]
    kind: String,
    message: String,
}

#[derive(Debug)]
enum Block {
    Text(String),
    Tool {
        id: String,
        name: String,
        arguments: String,
    },
    Thinking {
        thinking: String,
        signature: String,
    },
    Redacted(String),
    Ignored,
}

#[derive(Debug, Default)]
pub(super) struct Decoder {
    blocks: Vec<Block>,
    usage: Usage,
    stop: Option<StopReason>,
    response_id: Option<String>,
}

impl Decoder {
    fn block(&mut self, index: usize) -> Result<&mut Block, ProviderError> {
        self.blocks
            .get_mut(index)
            .ok_or_else(|| ProviderError::Protocol(format!("delta for unknown block {index}")))
    }

    fn start_block(&mut self, index: usize, block: Block) -> Result<(), ProviderError> {
        if index == self.blocks.len() {
            self.blocks.push(block);
            return Ok(());
        }
        match self.blocks.get_mut(index) {
            Some(slot) => {
                *slot = block;
                Ok(())
            }
            None => Err(ProviderError::Protocol(format!(
                "block {index} started before block {}",
                self.blocks.len()
            ))),
        }
    }
}

impl super::Decoder for Decoder {
    fn handle(&mut self, event: &SseEvent, emit: &mut Emitter<'_>) -> Result<bool, ProviderError> {
        match decode::<Event>(event)? {
            Event::MessageStart { message } => {
                self.response_id = message.id;
                let cache_read = message.usage.cache_read_input_tokens.unwrap_or(0);
                let cache_write = message.usage.cache_creation_input_tokens.unwrap_or(0);
                self.usage.cache_read_tokens = cache_read;
                self.usage.cache_write_tokens = cache_write;
                self.usage.input_tokens = message.usage.input_tokens + cache_read + cache_write;
            }
            Event::ContentBlockStart {
                index,
                content_block,
            } => {
                let block = match content_block {
                    StartBlock::Text => Block::Text(String::new()),
                    StartBlock::ToolUse { id, name } => {
                        emit.tool_started(&id, &name);
                        Block::Tool {
                            id,
                            name,
                            arguments: String::new(),
                        }
                    }
                    StartBlock::Thinking => Block::Thinking {
                        thinking: String::new(),
                        signature: String::new(),
                    },
                    StartBlock::RedactedThinking { data } => Block::Redacted(data),
                    StartBlock::Other => Block::Ignored,
                };
                self.start_block(index, block)?;
            }
            Event::ContentBlockDelta { index, delta } => match (self.block(index)?, delta) {
                (Block::Text(text), Delta::Text { text: more }) => {
                    emit.text(&more);
                    text.push_str(&more);
                }
                (Block::Tool { id, arguments, .. }, Delta::InputJson { partial_json }) => {
                    if !partial_json.is_empty() {
                        emit.tool_input_delta(id, &partial_json);
                    }
                    arguments.push_str(&partial_json);
                }
                (Block::Thinking { thinking, .. }, Delta::Thinking { thinking: more }) => {
                    thinking.push_str(&more);
                }
                (Block::Thinking { signature, .. }, Delta::Signature { signature: more }) => {
                    signature.push_str(&more);
                }
                (Block::Ignored, _) | (_, Delta::Other) => {}
                (block, delta) => {
                    return Err(ProviderError::Protocol(format!(
                        "delta {delta:?} does not fit block {block:?}"
                    )));
                }
            },
            Event::ContentBlockStop { index } => {
                if let Block::Tool {
                    id,
                    name,
                    arguments,
                } = self.block(index)?
                {
                    emit.tool_ready(&ToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        arguments: tool_arguments(arguments),
                    });
                }
            }
            Event::MessageDelta { delta, usage } => {
                if let Some(usage) = usage {
                    self.usage.output_tokens = usage.output_tokens;
                }
                self.stop = delta.stop_reason.map(|reason| stop_reason(&reason));
            }
            Event::MessageStop => return Ok(true),
            Event::Error { error } => {
                return Err(ProviderError::Api {
                    kind: error.kind,
                    message: error.message,
                });
            }
            Event::Other => {}
        }
        Ok(false)
    }

    fn finish(self) -> Result<Decoded, ProviderError> {
        let mut parts = Vec::new();
        for block in self.blocks {
            match block {
                Block::Text(text) if !text.is_empty() => parts.push(Part::text(text)),
                Block::Tool {
                    id,
                    name,
                    arguments,
                } => parts.push(Part::ToolCall(ToolCall {
                    id,
                    name,
                    arguments: tool_arguments(&arguments),
                })),
                Block::Thinking {
                    thinking,
                    signature,
                } => parts.push(Part::Reasoning(Reasoning::AnthropicThinking {
                    thinking,
                    signature,
                })),
                Block::Redacted(data) => {
                    parts.push(Part::Reasoning(Reasoning::AnthropicRedacted { data }));
                }
                Block::Text(_) | Block::Ignored => {}
            }
        }
        let has_tool_call = parts.iter().any(|part| matches!(part, Part::ToolCall(_)));
        let stop = self.stop.unwrap_or(if has_tool_call {
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

fn stop_reason(reason: &str) -> StopReason {
    match reason {
        "end_turn" => StopReason::EndTurn,
        "tool_use" => StopReason::ToolUse,
        "max_tokens" => StopReason::MaxTokens,
        "refusal" => StopReason::Refusal,
        other => StopReason::Other(other.to_owned()),
    }
}
