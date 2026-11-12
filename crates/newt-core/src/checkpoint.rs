use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::message::ToolCall;
use crate::prompt::ToolSpec;
use crate::tools::{Retention, ToolDef};

pub const CHECKPOINT: &str = "checkpoint";

pub const COMPACTION_INSTRUCTION: &str = "Newt is compacting this session: the older part of this conversation will be replaced by a checkpoint you write now.\n\
1. If anything above should outlast it and is not recorded yet, save it now with remember or your own tools. Send nothing.\n\
2. Then call checkpoint on its own. memory: durable facts about the person (preferences, constraints, standing decisions), newest statement wins. open_threads: questions or tasks still in progress. commitments: promises you made that are not done; every commitment in the current checkpoint must appear again, or under resolved if it is done or cancelled. decisions: what was decided and why. tone: how this person likes to be spoken to.\n\
Write for a reader who has not seen the conversation. Leave out what the workspace already shows.";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Checkpoint {
    #[serde(default)]
    pub memory: Vec<String>,
    #[serde(default)]
    pub open_threads: Vec<String>,
    #[serde(default)]
    pub commitments: Vec<String>,
    #[serde(default)]
    pub resolved: Vec<String>,
    #[serde(default)]
    pub decisions: Vec<String>,
    #[serde(default)]
    pub tone: String,
}

pub(crate) fn checkpoint_tool() -> ToolDef {
    let list = json!({ "type": "array", "items": { "type": "string" } });
    ToolDef::new(
        ToolSpec {
            name: CHECKPOINT.to_owned(),
            description: "Only when newt asks you to compact: write the checkpoint that replaces the older part of the conversation.".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "memory": list,
                    "open_threads": list,
                    "commitments": list,
                    "resolved": list,
                    "decisions": list,
                    "tone": { "type": "string" },
                },
                "required": ["memory", "open_threads", "commitments", "decisions", "tone"],
            }),
        },
        Retention::Ephemeral,
    )
}

impl Checkpoint {
    pub(crate) fn accept(
        call: &ToolCall,
        open_commitments: &[String],
        max_chars: usize,
    ) -> Result<Self, String> {
        let checkpoint = Self::deserialize(&call.arguments)
            .map_err(|error| format!("invalid checkpoint: {error}"))?;
        let filled = [
            &checkpoint.memory,
            &checkpoint.open_threads,
            &checkpoint.commitments,
            &checkpoint.decisions,
        ]
        .iter()
        .any(|items| items.iter().any(|item| !item.trim().is_empty()))
            || !checkpoint.tone.trim().is_empty();
        if !filled {
            return Err("Checkpoint refused: it is empty, so the whole conversation would be forgotten. Write what the next turns need.".to_owned());
        }
        let size = checkpoint.render().chars().count();
        if size > max_chars {
            return Err(format!(
                "Checkpoint refused: it is {size} characters, the limit is {max_chars}. Keep what the next turns need and drop the rest."
            ));
        }
        let kept = |commitment: &str| {
            checkpoint
                .commitments
                .iter()
                .chain(&checkpoint.resolved)
                .any(|listed| listed.trim() == commitment)
        };
        let missing: Vec<&str> = open_commitments
            .iter()
            .map(|commitment| commitment.trim())
            .filter(|commitment| !commitment.is_empty() && !kept(commitment))
            .collect();
        if !missing.is_empty() {
            return Err(format!(
                "Checkpoint refused: these open commitments are missing: {}. List each again under commitments, word for word, or under resolved if it is done or cancelled.",
                missing.join(" | ")
            ));
        }
        Ok(checkpoint)
    }

    pub fn render(&self) -> String {
        let mut out = String::from("<checkpoint>\nEarlier conversation, summarised.\n");
        for (title, items) in [
            ("Memory", &self.memory),
            ("Open threads", &self.open_threads),
            ("Commitments", &self.commitments),
            ("Decisions", &self.decisions),
        ] {
            if items.is_empty() {
                continue;
            }
            out.push_str(title);
            out.push_str(":\n");
            for item in items {
                out.push_str("- ");
                out.push_str(item.trim());
                out.push('\n');
            }
        }
        if !self.tone.trim().is_empty() {
            out.push_str("Tone: ");
            out.push_str(self.tone.trim());
            out.push('\n');
        }
        out.push_str("</checkpoint>");
        out
    }
}

#[cfg(test)]
mod tests {
    #![expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]

    use super::*;

    fn call(arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: CHECKPOINT.into(),
            arguments,
        }
    }

    #[test]
    fn a_checkpoint_must_carry_or_resolve_every_open_commitment() {
        let open = vec![
            "Send Maria the Acme brief".to_owned(),
            "Call back Friday".to_owned(),
        ];
        let dropped = call(json!({
            "memory": [], "open_threads": [], "decisions": [], "tone": "",
            "commitments": ["Send Maria the Acme brief"],
        }));
        let error = Checkpoint::accept(&dropped, &open, 10_000).unwrap_err();
        assert!(error.contains("Call back Friday"), "{error}");
        assert!(!error.contains("Acme"), "{error}");

        let resolved = call(json!({
            "memory": [], "open_threads": [], "decisions": [], "tone": "",
            "commitments": [" Send Maria the Acme brief "],
            "resolved": ["Call back Friday"],
        }));
        assert!(Checkpoint::accept(&resolved, &open, 10_000).is_ok());
    }

    #[test]
    fn empty_or_oversized_checkpoints_are_refused() {
        let empty = call(json!({
            "memory": [" "], "open_threads": [], "commitments": [], "decisions": [], "tone": "",
        }));
        assert!(
            Checkpoint::accept(&empty, &[], 10_000)
                .unwrap_err()
                .contains("empty")
        );
        let big = call(json!({
            "memory": ["x".repeat(500)], "open_threads": [], "commitments": [], "decisions": [], "tone": "",
        }));
        assert!(
            Checkpoint::accept(&big, &[], 200)
                .unwrap_err()
                .contains("limit is 200")
        );
        assert!(Checkpoint::accept(&big, &[], 10_000).is_ok());
    }

    #[test]
    fn render_lists_only_filled_sections() {
        let checkpoint = Checkpoint {
            memory: vec!["Remote only".into()],
            tone: "Short and direct".into(),
            ..Checkpoint::default()
        };
        assert_eq!(
            checkpoint.render(),
            "<checkpoint>\nEarlier conversation, summarised.\nMemory:\n- Remote only\nTone: Short and direct\n</checkpoint>"
        );
    }
}
