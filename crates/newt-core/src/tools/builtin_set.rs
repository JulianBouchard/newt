use serde::{Deserialize, Deserializer};

use super::builtin::{END_TURN, REMEMBER};
use crate::batch_tool::BATCH;
use crate::checkpoint::CHECKPOINT;
use crate::skills::LOAD_SKILL;

/// Every tool newt itself offers, in the order the model sees them.
pub const BUILTIN_TOOL_NAMES: [&str; 8] = [
    "read_file",
    "glob",
    "grep",
    END_TURN,
    REMEMBER,
    BATCH,
    CHECKPOINT,
    LOAD_SKILL,
];

/// Which of newt's built-in tools the agent is offered. Tools that are not
/// built in are not affected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuiltinTools(u8);

impl Default for BuiltinTools {
    fn default() -> Self {
        Self::ALL
    }
}

impl BuiltinTools {
    pub const ALL: Self = Self(u8::MAX);
    pub const NONE: Self = Self(0);

    fn bit(name: &str) -> Option<u8> {
        BUILTIN_TOOL_NAMES
            .iter()
            .position(|builtin| *builtin == name)
            .map(|index| 1 << index)
    }

    /// Only the named built-ins.
    ///
    /// # Errors
    ///
    /// A name that is not a built-in tool.
    pub fn only<S: AsRef<str>>(names: &[S]) -> Result<Self, String> {
        names.iter().try_fold(Self::NONE, |set, name| {
            let name = name.as_ref();
            Self::bit(name).map(|bit| Self(set.0 | bit)).ok_or_else(|| {
                format!(
                    "`{name}` is not a built-in tool; they are {}",
                    BUILTIN_TOOL_NAMES.join(", ")
                )
            })
        })
    }

    /// Whether a tool of this name is offered: always, for a tool that is not
    /// built in.
    #[must_use]
    pub fn offers(self, name: &str) -> bool {
        Self::bit(name).is_none_or(|bit| self.0 & bit != 0)
    }

    /// Refuses a set that cannot work with the other settings.
    ///
    /// # Errors
    ///
    /// `require_end_turn` without `end_turn`.
    pub fn check(self, require_end_turn: bool) -> Result<(), String> {
        if require_end_turn && !self.offers(END_TURN) {
            return Err("require_end_turn needs the built-in end_turn tool".to_owned());
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for BuiltinTools {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Written {
            Every(bool),
            Only(Vec<String>),
        }
        match Written::deserialize(deserializer)? {
            Written::Every(true) => Ok(Self::ALL),
            Written::Every(false) => Ok(Self::NONE),
            Written::Only(names) => Self::only(&names).map_err(serde::de::Error::custom),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn parsed(value: serde_json::Value) -> Result<BuiltinTools, String> {
        BuiltinTools::deserialize(value).map_err(|error| error.to_string())
    }

    #[test]
    fn true_false_and_a_list_of_names_select_the_builtins() {
        assert_eq!(parsed(json!(true)), Ok(BuiltinTools::ALL));
        assert_eq!(parsed(json!(false)), Ok(BuiltinTools::NONE));
        let some = parsed(json!(["read_file", "end_turn"])).unwrap_or(BuiltinTools::NONE);
        assert!(some.offers("read_file") && some.offers("end_turn"));
        assert!(!some.offers("batch") && !some.offers("grep"));
        assert!(
            some.offers("add_task"),
            "tools that are not built in are never filtered"
        );
    }

    #[test]
    fn an_unknown_name_is_refused_with_the_list() {
        let error = parsed(json!(["read_file", "web_search"]))
            .err()
            .unwrap_or_default();
        assert!(
            error.contains("`web_search` is not a built-in tool"),
            "{error}"
        );
        assert!(error.contains("load_skill"), "{error}");
        assert!(parsed(json!("read_file")).is_err());
    }

    #[test]
    fn require_end_turn_needs_end_turn() {
        assert!(BuiltinTools::NONE.check(false).is_ok());
        assert!(BuiltinTools::NONE.check(true).is_err());
        assert!(BuiltinTools::only(&["end_turn"]).is_ok_and(|set| set.check(true).is_ok()));
    }
}
