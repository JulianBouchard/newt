use serde_json::{Value, json};

use crate::message::INVALID_ARGUMENTS_KEY;

/// A tool call's arguments as the model wrote them: the object when they
/// parse as one, `{}` when empty, and otherwise the raw text under
/// [`INVALID_ARGUMENTS_KEY`] so the tool can say what went wrong.
pub fn tool_arguments(raw: &str) -> Value {
    if raw.trim().is_empty() {
        return json!({});
    }
    match serde_json::from_str::<Value>(raw) {
        Ok(value @ Value::Object(_)) => value,
        _ => json!({ INVALID_ARGUMENTS_KEY: raw }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_arguments_keep_objects_and_flag_everything_else() {
        assert_eq!(tool_arguments(""), json!({}));
        assert_eq!(tool_arguments("{\"a\":1}"), json!({ "a": 1 }));
        assert_eq!(
            tool_arguments("{\"a\": [1, 2"),
            json!({ INVALID_ARGUMENTS_KEY: "{\"a\": [1, 2" })
        );
        assert_eq!(
            tool_arguments("[1]"),
            json!({ INVALID_ARGUMENTS_KEY: "[1]" })
        );
    }
}
