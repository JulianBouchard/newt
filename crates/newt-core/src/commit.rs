//! What of a finished turn becomes the session's history. The turn hands
//! its messages to a [`HistoryPolicy`], after taking out what never belongs
//! in history (harness notes, reasoning), and checks what the policy gives
//! back: every tool call kept with its result, and every result with its
//! call.

use std::collections::{HashMap, HashSet};

use crate::message::{Message, Part, ToolResult};
use crate::tools::Retention;

pub(crate) const UNSUMMARIZED_RESULT_LIMIT: usize = 500;

/// Decides what of a finished turn's messages becomes history.
pub trait HistoryPolicy: Send + Sync {
    fn commit(&self, turn: TurnRecord<'_>) -> Vec<Message>;
}

/// A finished turn, with harness notes and reasoning already taken out.
#[derive(Debug)]
pub struct TurnRecord<'a> {
    pub messages: Vec<Message>,
    /// Each tool's retention, by tool name.
    pub retention: &'a HashMap<String, Retention>,
    /// Each call's `newt/summary`, by call id.
    pub summaries: &'a HashMap<String, String>,
    /// Calls the turn asked to keep whatever their retention.
    pub kept: &'a HashSet<String>,
}

/// The default: `ephemeral` calls are dropped, and a kept call's result is
/// its summary, or its first 500 characters when it has none.
#[derive(Debug, Clone, Copy, Default)]
pub struct Retained;

impl HistoryPolicy for Retained {
    fn commit(&self, turn: TurnRecord<'_>) -> Vec<Message> {
        let kept_calls: HashSet<String> = turn
            .messages
            .iter()
            .flat_map(Message::tool_calls)
            .filter(|call| {
                turn.kept.contains(&call.id)
                    || turn
                        .retention
                        .get(&call.name)
                        .is_some_and(|retention| *retention != Retention::Ephemeral)
            })
            .map(|call| call.id.clone())
            .collect();
        turn.messages
            .into_iter()
            .filter_map(|message| {
                let parts: Vec<Part> = message
                    .parts
                    .into_iter()
                    .filter_map(|part| match part {
                        Part::ToolCall(call) if !kept_calls.contains(&call.id) => None,
                        Part::ToolResult(result) if !kept_calls.contains(&result.call_id) => None,
                        Part::ToolResult(result) => {
                            Some(Part::ToolResult(compact(result, turn.summaries)))
                        }
                        part => Some(part),
                    })
                    .collect();
                (!parts.is_empty()).then_some(Message {
                    role: message.role,
                    parts,
                })
            })
            .collect()
    }
}

/// Keeps every tool call and its full result in history.
#[derive(Debug, Clone, Copy, Default)]
pub struct KeepAllToolCalls;

impl HistoryPolicy for KeepAllToolCalls {
    fn commit(&self, turn: TurnRecord<'_>) -> Vec<Message> {
        turn.messages
    }
}

pub(crate) struct CommitRules<'a> {
    pub retention: &'a HashMap<String, Retention>,
    pub summaries: &'a HashMap<String, String>,
    pub kept: &'a HashSet<String>,
    pub harness_notes: &'a HashSet<usize>,
}

/// The history a turn commits: its scratch without harness notes and
/// reasoning, as `policy` keeps it. An error when the policy kept a tool
/// call without its result or a result without its call, which no provider
/// accepts back.
pub(crate) fn committed_messages(
    policy: &dyn HistoryPolicy,
    scratch: Vec<Message>,
    rules: &CommitRules<'_>,
) -> Result<Vec<Message>, String> {
    let messages = scratch
        .into_iter()
        .enumerate()
        .filter(|(index, _)| !rules.harness_notes.contains(index))
        .filter_map(|(_, message)| {
            let parts: Vec<Part> = message
                .parts
                .into_iter()
                .filter(|part| !matches!(part, Part::Reasoning(_)))
                .collect();
            (!parts.is_empty()).then_some(Message {
                role: message.role,
                parts,
            })
        })
        .collect();
    let committed = policy.commit(TurnRecord {
        messages,
        retention: rules.retention,
        summaries: rules.summaries,
        kept: rules.kept,
    });
    check_pairs(&committed)?;
    Ok(committed)
}

fn check_pairs(messages: &[Message]) -> Result<(), String> {
    let calls: HashSet<&str> = messages
        .iter()
        .flat_map(Message::tool_calls)
        .map(|call| call.id.as_str())
        .collect();
    let results: HashSet<&str> = messages
        .iter()
        .flat_map(|message| &message.parts)
        .filter_map(|part| match part {
            Part::ToolResult(result) => Some(result.call_id.as_str()),
            _ => None,
        })
        .collect();
    match calls.symmetric_difference(&results).next() {
        Some(id) => Err(format!(
            "the history policy kept tool call `{id}` without its pair"
        )),
        None => Ok(()),
    }
}

fn compact(result: ToolResult, summaries: &HashMap<String, String>) -> ToolResult {
    let content = match summaries.get(&result.call_id) {
        Some(summary) => summary.clone(),
        None => truncate(&result.content, UNSUMMARIZED_RESULT_LIMIT),
    };
    ToolResult { content, ..result }
}

pub(crate) fn truncate(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    #![expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]

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

        let rules = CommitRules {
            retention: &retention,
            summaries: &summaries,
            kept: &HashSet::new(),
            harness_notes: &harness_notes,
        };
        let committed = committed_messages(&Retained, scratch.clone(), &rules).unwrap();
        let everything = committed_messages(&KeepAllToolCalls, scratch, &rules).unwrap();
        assert_eq!(
            everything.len(),
            5,
            "only the harness note goes: {everything:?}"
        );
        assert!(
            everything
                .iter()
                .flat_map(|message| &message.parts)
                .all(|part| !matches!(part, Part::Reasoning(_))),
            "reasoning never reaches history"
        );
        assert!(format!("{everything:?}").contains("lots of text"));

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

    struct DropsResults;

    impl HistoryPolicy for DropsResults {
        fn commit(&self, turn: TurnRecord<'_>) -> Vec<Message> {
            turn.messages
                .into_iter()
                .filter(|message| {
                    !message
                        .parts
                        .iter()
                        .any(|part| matches!(part, Part::ToolResult(_)))
                })
                .collect()
        }
    }

    #[test]
    fn a_policy_that_splits_a_call_from_its_result_is_refused() {
        let scratch = vec![
            Message {
                role: Role::Assistant,
                parts: vec![call("w1", "set_hold")],
            },
            Message {
                role: Role::User,
                parts: vec![result("w1", "done")],
            },
        ];
        let rules = CommitRules {
            retention: &HashMap::new(),
            summaries: &HashMap::new(),
            kept: &HashSet::new(),
            harness_notes: &HashSet::new(),
        };
        let error = committed_messages(&DropsResults, scratch, &rules).unwrap_err();
        assert!(error.contains("`w1`"), "{error}");
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
