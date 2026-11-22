use super::Settings;
use crate::mcp::Toolset;
use newt_core::journal::TurnChoice;
use newt_core::prompt::Effort;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What the home answers to `newt/turn`. Every field is optional.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TurnContext {
    #[serde(default)]
    pub(super) pinned: Option<String>,
    #[serde(default)]
    pub(super) instructions: Option<String>,
    #[serde(default)]
    pub(super) constraints: Vec<String>,
    #[serde(default)]
    pub(super) state: Option<Value>,
    #[serde(default)]
    pub(super) turn_state: Option<Value>,
    #[serde(default)]
    pub(super) model: Option<String>,
    #[serde(default)]
    pub(super) effort: Option<Effort>,
    #[serde(default)]
    pub(super) first_call_effort: Option<Effort>,
    #[serde(default)]
    pub(super) reset: bool,
    #[serde(default, rename = "_meta")]
    pub(super) _meta: Option<Value>,
}

#[derive(Serialize)]
pub(super) struct TurnCall<'a> {
    pub(super) session: &'a str,
    pub(super) events: &'a [crate::backend::TurnEvent],
    pub(super) state: Option<&'a Value>,
}

impl TurnContext {
    pub(super) fn choice(&self, settings: &Settings) -> Option<TurnChoice> {
        let chose = self.model.is_some()
            || self.effort.is_some()
            || self.first_call_effort.is_some()
            || self.reset;
        chose.then(|| TurnChoice {
            model: self.model.clone().unwrap_or_else(|| settings.model.clone()),
            first_call_effort: self.first_call_effort.or(settings.first_call_effort),
            effort: self.effort.or(settings.effort),
            reset: self.reset,
        })
    }

    pub(super) fn render(&self, unavailable: &[String]) -> Option<String> {
        let instructions = self
            .instructions
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty());
        let constraints: Vec<&str> = self
            .constraints
            .iter()
            .map(|constraint| constraint.trim())
            .filter(|constraint| !constraint.is_empty())
            .collect();
        if instructions.is_none() && constraints.is_empty() && unavailable.is_empty() {
            return None;
        }
        let mut out = String::from("<turn_context>\n");
        if let Some(instructions) = instructions {
            out.push_str("<instructions>\n");
            out.push_str(instructions);
            out.push_str("\n</instructions>\n");
        }
        if !constraints.is_empty() {
            out.push_str("<active_constraints>\n");
            for constraint in constraints {
                out.push_str("- ");
                out.push_str(constraint);
                out.push('\n');
            }
            out.push_str("</active_constraints>\n");
        }
        if !unavailable.is_empty() {
            out.push_str("<unavailable_servers>\n");
            out.push_str(&unavailable.join(", "));
            out.push_str(
                ": unavailable right now, so their tools and pages are missing this turn.\n",
            );
            out.push_str("</unavailable_servers>\n");
        }
        out.push_str("</turn_context>");
        Some(out)
    }
}

#[derive(Debug)]
pub(super) struct Loaded {
    pub(super) settings: Settings,
    pub(super) toolset: Toolset,
}
