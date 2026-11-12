use serde_json::Value;

pub fn violations(schema: &Value, value: &Value) -> Vec<String> {
    let mut found = Vec::new();
    check(schema, value, "$", &mut found);
    found
}

fn type_matches(expected: &str, value: &Value) -> bool {
    match expected {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        _ => true,
    }
}

fn check(schema: &Value, value: &Value, at: &str, found: &mut Vec<String>) {
    let allowed: Vec<&str> = match schema.get("type") {
        Some(Value::String(kind)) => vec![kind.as_str()],
        Some(Value::Array(kinds)) => kinds.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    if !allowed.is_empty() && !allowed.iter().any(|kind| type_matches(kind, value)) {
        found.push(format!("{at} is not {}", allowed.join(" or ")));
        return;
    }
    if let Some(choices) = schema.get("enum").and_then(Value::as_array)
        && !choices.contains(value)
    {
        found.push(format!("{at} is not one of the allowed values"));
    }
    if let Some(object) = value.as_object() {
        let properties = schema.get("properties").and_then(Value::as_object);
        for name in schema
            .get("required")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            if !object.contains_key(name) {
                found.push(format!("{at}.{name} is required"));
            }
        }
        for (name, field) in object {
            match properties.and_then(|properties| properties.get(name)) {
                Some(property) => check(property, field, &format!("{at}.{name}"), found),
                None if schema.get("additionalProperties") == Some(&Value::Bool(false)) => {
                    found.push(format!("{at}.{name} is not allowed"));
                }
                None => {}
            }
        }
    }
    if let (Some(items), Some(array)) = (schema.get("items"), value.as_array()) {
        for (index, item) in array.iter().enumerate() {
            check(items, item, &format!("{at}[{index}]"), found);
        }
    }
}

pub fn looks_like_error(output: &str) -> bool {
    const MARKERS: [&str; 6] = [
        "\"error\"",
        "Error:",
        "Invalid input",
        "Type validation failed",
        "no such page",
        "File not found",
    ];
    MARKERS.iter().any(|marker| output.contains(marker))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn read_file() -> Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["filePath"],
            "properties": { "filePath": { "type": "string" }, "limit": { "type": "integer" } },
        })
    }

    #[test]
    fn a_wrong_argument_shape_is_reported() {
        assert_eq!(
            violations(&read_file(), &json!({ "paths": ["memory.md"] })),
            vec!["$.filePath is required", "$.paths is not allowed"]
        );
        assert_eq!(
            violations(&read_file(), &json!({ "filePath": 3 })),
            vec!["$.filePath is not string"]
        );
    }

    #[test]
    fn matching_arguments_and_enums_pass() {
        assert_eq!(
            violations(&read_file(), &json!({ "filePath": "customer/memory.md" })),
            Vec::<String>::new()
        );
        let send =
            json!({ "type": "object", "properties": { "command": { "enum": ["send", "react"] } } });
        assert_eq!(
            violations(&send, &json!({ "command": "send" })),
            Vec::<String>::new()
        );
        assert_eq!(
            violations(&send, &json!({ "command": "shout" })),
            vec!["$.command is not one of the allowed values"]
        );
    }

    #[test]
    fn error_outputs_are_recognised() {
        assert!(looks_like_error(
            "{\"error\":\"Error: File not found: /workspace/memory.md\"}"
        ));
        assert!(!looks_like_error("--- memory.md ---\n# Memory"));
    }
}
