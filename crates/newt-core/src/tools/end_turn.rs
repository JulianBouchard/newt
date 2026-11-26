use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct EndTurn {
    #[serde(default)]
    pub recorded: Vec<String>,
    #[serde(default)]
    pub nothing_new: Option<String>,
}

impl EndTurn {
    /// With `strict`, a turn that requires `end_turn` must say what it recorded
    /// or why there was nothing to record. Otherwise both are optional.
    pub fn parse(arguments: &serde_json::Value, strict: bool) -> Result<Self, String> {
        if !arguments.is_object() {
            return Err("invalid end_turn arguments: expected an object".to_owned());
        }
        let end = Self::deserialize(arguments)
            .map_err(|error| format!("invalid end_turn arguments: {error}"))?;
        let recorded = end.recorded.iter().any(|item| !item.trim().is_empty());
        let nothing_new = end
            .nothing_new
            .as_deref()
            .is_some_and(|reason| !reason.trim().is_empty());
        if recorded && nothing_new {
            return Err("end_turn takes `recorded` or `nothing_new`, not both".to_owned());
        }
        if strict && !recorded && !nothing_new {
            return Err(
                "end_turn needs exactly one of `recorded` (what you saved this turn) or `nothing_new` (why there was nothing to save)"
                    .to_owned(),
            );
        }
        Ok(end)
    }

    /// The end of a turn that a tool declared as ending it.
    pub fn by_tool(name: &str) -> Self {
        Self {
            recorded: Vec::new(),
            nothing_new: Some(format!("ended by `{name}`")),
        }
    }
}

/// Appended to the description of every tool that ends the turn.
pub const ENDS_TURN_NOTE: &str = " Calling this tool ends your turn once every call in the same response succeeds, so put it in the same response as your last action.";

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn strict_end_turn_needs_exactly_one_of_recorded_or_nothing_new() {
        assert!(EndTurn::parse(&json!({ "recorded": ["prefers remote"] }), true).is_ok());
        assert!(EndTurn::parse(&json!({ "nothing_new": "just a greeting" }), true).is_ok());
        assert!(EndTurn::parse(&json!({}), true).is_err());
        assert!(EndTurn::parse(&json!({ "recorded": ["x"], "nothing_new": "y" }), true).is_err());
        assert!(EndTurn::parse(&json!({ "recorded": [" "] }), true).is_err());
    }

    #[test]
    fn by_default_end_turn_takes_either_field_or_none_but_not_both() {
        assert!(EndTurn::parse(&json!({}), false).is_ok());
        assert!(EndTurn::parse(&json!({ "recorded": ["prefers remote"] }), false).is_ok());
        assert!(EndTurn::parse(&json!({ "nothing_new": "just a greeting" }), false).is_ok());
        assert!(EndTurn::parse(&json!({ "recorded": ["x"], "nothing_new": "y" }), false).is_err());
    }
}
