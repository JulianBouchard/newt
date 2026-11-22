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
                if self.announced.contains(text("call_id")) {
                    out.push(json!({
                        "type": "tool-input-delta",
                        "toolCallId": text("call_id"),
                        "inputTextDelta": text("delta"),
                    }));
                }
            }
            "tool_input" => {
                self.announce(&mut out, text("call_id"), text("name"));
                out.push(json!({
                    "type": "tool-input-available",
                    "toolCallId": text("call_id"),
                    "toolName": text("name"),
                    "input": frame["input"],
                }));
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
mod regressions;

#[cfg(test)]
mod tests {
    use super::*;

    fn types(chunks: &[Value]) -> Vec<&str> {
        chunks
            .iter()
            .map(|chunk| chunk["type"].as_str().unwrap_or_default())
            .collect()
    }

    fn run(frames: &[Value]) -> Vec<Value> {
        let (mut ui, mut out) = UiStream::start();
        for frame in frames {
            out.extend(ui.push(frame));
        }
        out
    }

    fn frame(kind: &str, turn: &str, extra: Value) -> Value {
        let mut frame = json!({ "type": kind, "turn": turn, "session": "a/k" });
        if let (Some(fields), Value::Object(extra)) = (frame.as_object_mut(), extra) {
            fields.extend(extra);
        }
        frame
    }

    #[test]
    fn a_turn_with_text_then_a_tool_becomes_two_steps_with_streamed_input() {
        let out = run(&[
            frame("turn_started", "t1", json!({})),
            frame("model_call_started", "t1", json!({ "call": 0 })),
            frame("text_delta", "t1", json!({ "text": "Let me " })),
            frame("text_delta", "t1", json!({ "text": "check." })),
            frame(
                "tool_started",
                "t1",
                json!({ "call_id": "c1", "name": "send" }),
            ),
            frame(
                "tool_input_delta",
                "t1",
                json!({ "call_id": "c1", "delta": "{\"te" }),
            ),
            frame(
                "tool_input_delta",
                "t1",
                json!({ "call_id": "c1", "delta": "xt\":1}" }),
            ),
            frame(
                "tool_input",
                "t1",
                json!({ "call_id": "c1", "name": "send", "input": { "text": 1 } }),
            ),
            frame("model_call_finished", "t1", json!({ "call": 0 })),
            frame(
                "tool_finished",
                "t1",
                json!({ "call_id": "c1", "name": "send", "is_error": false, "ms": 4 }),
            ),
            frame("model_call_started", "t1", json!({ "call": 1 })),
            frame("text_delta", "t1", json!({ "text": "Done." })),
            frame("model_call_finished", "t1", json!({ "call": 1 })),
            frame("turn_completed", "t1", json!({})),
        ]);

        assert_eq!(
            types(&out),
            [
                "start",
                "start-step",
                "text-start",
                "text-delta",
                "text-delta",
                "text-end",
                "tool-input-start",
                "tool-input-delta",
                "tool-input-delta",
                "tool-input-available",
                "finish-step",
                "tool-output-available",
                "start-step",
                "text-start",
                "text-delta",
                "text-end",
                "finish-step",
                "finish",
            ]
        );
        assert_eq!(out[3]["delta"], "Let me ");
        assert_ne!(out[2]["id"], out[13]["id"]);
        assert_eq!(out[7]["inputTextDelta"], "{\"te");
        assert_eq!(out[9]["input"], json!({ "text": 1 }));
        assert_eq!(out[9]["toolName"], "send");
    }

    #[test]
    fn a_failed_turn_reports_the_error_then_finishes() {
        let out = run(&[
            frame("turn_started", "t1", json!({})),
            frame("model_call_started", "t1", json!({})),
            frame("turn_failed", "t1", json!({ "error": "model went away" })),
            frame(
                "text_delta",
                "t1",
                json!({ "text": "ignored after the end" }),
            ),
        ]);

        assert_eq!(
            types(&out),
            ["start", "start-step", "finish-step", "error", "finish"]
        );
        assert_eq!(out[3]["errorText"], TURN_FAILED_TEXT);
    }

    #[test]
    fn a_tool_output_frame_carries_the_result_and_its_tool_finished_adds_nothing() {
        let (mut stream, _) = UiStream::start();
        stream.push(&frame("turn_started", "t1", json!({})));
        stream.push(&frame(
            "tool_started",
            "t1",
            json!({ "call_id": "c1", "name": "lookup" }),
        ));
        let output = stream.push(&frame(
            "tool_output",
            "t1",
            json!({ "call_id": "c1", "name": "lookup", "output": "3 orders", "is_error": false }),
        ));
        assert_eq!(
            output,
            vec![
                json!({ "type": "tool-output-available", "toolCallId": "c1", "output": "3 orders" })
            ]
        );
        let finished = stream.push(&frame(
            "tool_finished",
            "t1",
            json!({ "call_id": "c1", "name": "lookup", "is_error": false, "ms": 3 }),
        ));
        assert_eq!(finished, Vec::<Value>::new());
    }

    #[test]
    fn a_failing_tool_becomes_a_tool_output_error() {
        let out = run(&[
            frame("turn_started", "t1", json!({})),
            frame(
                "tool_started",
                "t1",
                json!({ "call_id": "c1", "name": "x" }),
            ),
            frame(
                "tool_finished",
                "t1",
                json!({ "call_id": "c1", "is_error": true, "ms": 1 }),
            ),
        ]);

        assert_eq!(
            out.last().map(|chunk| chunk["type"].clone()),
            Some(json!("tool-output-error"))
        );
    }

    #[test]
    fn frames_of_a_turn_that_ended_before_ours_joined_are_skipped() {
        let out = run(&[
            frame("text_delta", "old", json!({ "text": "not ours" })),
            frame("turn_completed", "old", json!({})),
            frame("turn_started", "new", json!({})),
            frame("text_delta", "new", json!({ "text": "ours" })),
            frame("turn_completed", "new", json!({})),
        ]);

        let text: Vec<&Value> = out
            .iter()
            .filter(|chunk| chunk["type"] == "text-delta")
            .collect();
        assert_eq!(text.len(), 1);
        assert_eq!(text[0]["delta"], "ours");
        assert_eq!(
            out.last().map(|chunk| chunk["type"].clone()),
            Some(json!("finish"))
        );
    }

    #[test]
    fn a_steered_message_joins_the_running_turn_from_its_steering_frame() {
        let out = run(&[
            frame("text_delta", "t1", json!({ "text": "before the steer" })),
            frame("turn_steered", "t1", json!({ "events": 1 })),
            frame("text_delta", "t1", json!({ "text": "after" })),
            frame("turn_completed", "t1", json!({})),
        ]);

        let text: Vec<&str> = out
            .iter()
            .filter(|chunk| chunk["type"] == "text-delta")
            .filter_map(|chunk| chunk["delta"].as_str())
            .collect();
        assert_eq!(text, ["after"]);
        assert_eq!(
            out.last().map(|chunk| chunk["type"].clone()),
            Some(json!("finish"))
        );
    }
}
