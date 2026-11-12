use std::collections::{HashMap, HashSet};

use crate::message::{Message, Part, ToolResult};
use crate::tools::Retention;

const UNSUMMARIZED_RESULT_LIMIT: usize = 500;

pub(crate) struct CommitRules<'a> {
    pub retention: &'a HashMap<String, Retention>,
    pub summaries: &'a HashMap<String, String>,
    pub harness_notes: &'a HashSet<usize>,
}

pub(crate) fn committed_messages(scratch: Vec<Message>, rules: &CommitRules<'_>) -> Vec<Message> {
    let kept_calls: HashSet<String> = scratch
        .iter()
        .flat_map(Message::tool_calls)
        .filter(|call| {
            rules
                .retention
                .get(&call.name)
                .is_some_and(|retention| *retention != Retention::Ephemeral)
        })
        .map(|call| call.id.clone())
        .collect();

    scratch
        .into_iter()
        .enumerate()
        .filter(|(index, _)| !rules.harness_notes.contains(index))
        .filter_map(|(_, message)| {
            let parts: Vec<Part> = message
                .parts
                .into_iter()
                .filter_map(|part| match part {
                    Part::Text { text } => Some(Part::Text { text }),
                    Part::ToolCall(call) if kept_calls.contains(&call.id) => {
                        Some(Part::ToolCall(call))
                    }
                    Part::ToolResult(result) if kept_calls.contains(&result.call_id) => {
                        Some(Part::ToolResult(compact(result, rules.summaries)))
                    }
                    Part::ToolCall(_) | Part::ToolResult(_) | Part::Reasoning(_) => None,
                })
                .collect();
            (!parts.is_empty()).then_some(Message {
                role: message.role,
                parts,
            })
        })
        .collect()
}

fn compact(result: ToolResult, summaries: &HashMap<String, String>) -> ToolResult {
    let content = match summaries.get(&result.call_id) {
        Some(summary) => summary.clone(),
        None => truncate(&result.content, UNSUMMARIZED_RESULT_LIMIT),
    };
    ToolResult { content, ..result }
}

fn truncate(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::message::{Reasoning, Role, ToolCall};

    fn call(id: &str, name: &str) -> Part {
        Part::ToolCall(ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: json!({}),
        })
    }

    fn result(id: &str, content: &str) -> Part {
        Part::ToolResult(ToolResult {
            call_id: id.into(),
            content: content.into(),
            is_error: false,
        })
    }

    #[test]
    fn reads_reasoning_and_harness_notes_are_dropped_writes_are_kept_compact() {
        let scratch = vec![
            Message::user_text("I'm on vacation until Monday, hold everything."),
            Message {
                role: Role::Assistant,
                parts: vec![
                    Part::Reasoning(Reasoning::AnthropicThinking {
                        thinking: "check prefs".into(),
                        signature: "s".into(),
                    }),
                    call("r1", "read_file"),
                ],
            },
            Message {
                role: Role::User,
                parts: vec![result("r1", "--- profile.md ---\nlots of text")],
            },
            Message {
                role: Role::Assistant,
                parts: vec![
                    Part::text("Holding messages until Monday."),
                    call("w1", "set_hold"),
                    call("e1", "end_turn"),
                ],
            },
            Message {
                role: Role::User,
                parts: vec![
                    result("w1", "{\"hold\":{\"id\":7,...}}"),
                    result("e1", "Turn ended."),
                ],
            },
            Message::user_text("harness reminder"),
        ];
        let retention = HashMap::from([
            ("read_file".to_owned(), Retention::Ephemeral),
            ("end_turn".to_owned(), Retention::Ephemeral),
            ("set_hold".to_owned(), Retention::Durable),
        ]);
        let summaries = HashMap::from([("w1".to_owned(), "Hold set until Monday".to_owned())]);
        let harness_notes = HashSet::from([5]);

        let committed = committed_messages(
            scratch,
            &CommitRules {
                retention: &retention,
                summaries: &summaries,
                harness_notes: &harness_notes,
            },
        );

        assert_eq!(
            committed,
            vec![
                Message::user_text("I'm on vacation until Monday, hold everything."),
                Message {
                    role: Role::Assistant,
                    parts: vec![
                        Part::text("Holding messages until Monday."),
                        call("w1", "set_hold"),
                    ],
                },
                Message {
                    role: Role::User,
                    parts: vec![result("w1", "Hold set until Monday")],
                },
            ]
        );
    }

    #[test]
    fn a_kept_result_without_summary_is_truncated_on_a_char_boundary() {
        let long = "é".repeat(UNSUMMARIZED_RESULT_LIMIT + 10);
        let compacted = compact(
            ToolResult {
                call_id: "x".into(),
                content: long,
                is_error: false,
            },
            &HashMap::new(),
        );
        assert_eq!(
            compacted.content.chars().count(),
            UNSUMMARIZED_RESULT_LIMIT + 1
        );
        assert!(compacted.content.ends_with('…'));
    }
}
