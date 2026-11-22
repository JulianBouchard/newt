//! Maps one turn's live feed frames onto the chunks of the AI SDK's UI
//! message stream.

use std::collections::HashSet;

use serde_json::{Value, json};

#[derive(Debug, Default)]
pub(super) struct UiStream {
    owner: Option<String>,
    step_open: bool,
    text_open: Option<String>,
    texts: usize,
    announced: HashSet<String>,
    answered: HashSet<String>,
    input_sent: HashSet<String>,
    finished: bool,
}

impl UiStream {
    pub(super) fn start() -> (Self, Vec<Value>) {
        (Self::default(), vec![json!({ "type": "start" })])
    }

    pub(super) fn finished(&self) -> bool {
        self.finished
    }

    pub(super) fn push(&mut self, frame: &Value) -> Vec<Value> {
        let mut out = Vec::new();
        if self.finished || !self.accepts(frame) {
            return out;
        }
        let text = |name: &str| frame[name].as_str().unwrap_or_default();
        match text("type") {
            "tool_started" | "tool_input_delta" | "tool_input" | "tool_output"
            | "tool_finished"
                if text("call_id").is_empty() => {}
            "model_call_started" => {
                self.close_step(&mut out);
                self.open_step(&mut out);
            }
            "model_call_finished" => self.close_step(&mut out),
            "text_delta" => {
                self.open_step(&mut out);
                let id = self.open_text(&mut out);
                out.push(json!({ "type": "text-delta", "id": id, "delta": text("text") }));
            }
            "tool_started" => self.announce(&mut out, text("call_id"), text("name")),
            "tool_input_delta" => {
                if self.announced.contains(text("call_id"))
                    && !self.answered.contains(text("call_id"))
                {
                    out.push(json!({
                        "type": "tool-input-delta",
                        "toolCallId": text("call_id"),
                        "inputTextDelta": text("delta"),
                    }));
                }
            }
            "tool_input" => {
                self.announce(&mut out, text("call_id"), text("name"));
                if !self.answered.contains(text("call_id"))
                    && self.input_sent.insert(text("call_id").to_owned())
                {
                    out.push(json!({
                        "type": "tool-input-available",
                        "toolCallId": text("call_id"),
                        "toolName": text("name"),
                        "input": frame["input"],
                    }));
                }
            }
            "tool_output" => {
                if self.announced.contains(text("call_id"))
                    && self.answered.insert(text("call_id").to_owned())
                {
                    out.push(tool_output_chunk(frame, text("call_id")));
                }
            }
            "tool_finished" => {
                if self.announced.contains(text("call_id"))
                    && self.answered.insert(text("call_id").to_owned())
                {
                    out.push(tool_finished_chunk(frame, text("call_id")));
                }
            }
            "turn_completed" => {
                self.close_step(&mut out);
                out.push(json!({ "type": "finish", "finishReason": "stop" }));
                self.finished = true;
            }
            "turn_failed" => {
                self.close_step(&mut out);
                out.push(json!({ "type": "error", "errorText": TURN_FAILED_TEXT }));
                out.push(json!({ "type": "finish", "finishReason": "error" }));
                self.finished = true;
            }
            _ => {}
        }
        out
    }

    fn accepts(&mut self, frame: &Value) -> bool {
        let Some(turn) = frame["turn"].as_str() else {
            return false;
        };
        if self.owner.is_none()
            && matches!(
                frame["type"].as_str(),
                Some("turn_started" | "turn_steered")
            )
        {
            self.owner = Some(turn.to_owned());
        }
        self.owner.as_deref() == Some(turn)
    }

    fn open_step(&mut self, out: &mut Vec<Value>) {
        if !self.step_open {
            self.step_open = true;
            out.push(json!({ "type": "start-step" }));
        }
    }

    fn close_step(&mut self, out: &mut Vec<Value>) {
        self.close_text(out);
        if self.step_open {
            self.step_open = false;
            out.push(json!({ "type": "finish-step" }));
        }
    }

    fn open_text(&mut self, out: &mut Vec<Value>) -> String {
        if let Some(id) = &self.text_open {
            return id.clone();
        }
        self.texts += 1;
        let id = format!("text-{}", self.texts);
        out.push(json!({ "type": "text-start", "id": id }));
        self.text_open = Some(id.clone());
        id
    }

    fn close_text(&mut self, out: &mut Vec<Value>) {
        if let Some(id) = self.text_open.take() {
            out.push(json!({ "type": "text-end", "id": id }));
        }
    }

    fn announce(&mut self, out: &mut Vec<Value>, call_id: &str, name: &str) {
        self.open_step(out);
        self.close_text(out);
        if self.announced.insert(call_id.to_owned()) {
            out.push(json!({
                "type": "tool-input-start",
                "toolCallId": call_id,
                "toolName": name,
            }));
        }
    }
}

const TURN_FAILED_TEXT: &str = "the agent could not finish this turn";

fn tool_output_chunk(frame: &Value, call_id: &str) -> Value {
    let output = frame["output"].as_str().unwrap_or_default();
    if frame["is_error"].as_bool().unwrap_or(false) {
        json!({ "type": "tool-output-error", "toolCallId": call_id, "errorText": output })
    } else {
        json!({ "type": "tool-output-available", "toolCallId": call_id, "output": output })
    }
}

fn tool_finished_chunk(frame: &Value, call_id: &str) -> Value {
    if frame["is_error"].as_bool().unwrap_or(false) {
        json!({
            "type": "tool-output-error",
            "toolCallId": call_id,
            "errorText": "the tool call failed",
        })
    } else {
        json!({
            "type": "tool-output-available",
            "toolCallId": call_id,
            "output": { "ms": frame["ms"] },
        })
    }
}

#[cfg(test)]
mod props;
#[cfg(test)]
mod regressions;
#[cfg(test)]
mod tests;
