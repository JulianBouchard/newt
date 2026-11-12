use serde_json::{Map, Value, json};

use crate::profile::Kind;
use crate::sample::Rng;

pub const PAGES: &[&str] = &[
    "since-last-turn.md",
    "memory.md",
    "calendar.md",
    "match-page.md",
    "profile/summary.md",
    "profile/preferences.md",
    "conversation/timeline/latest.md",
    "applications.md",
];

pub const SKILLS: &[&str] = &["writing", "intro", "screening", "followups", "matching"];

pub fn arguments_for(name: &str, kind: Kind, schema: &Value, rng: &Rng) -> Value {
    match (name, kind) {
        (_, Kind::Skill) if name == "read_file" => {
            json!({ "paths": [format!("skills/{}.md", SKILLS[rng.below(SKILLS.len())])] })
        }
        ("read_file", _) => {
            let first = PAGES[rng.below(PAGES.len())];
            let second = PAGES[rng.below(PAGES.len())];
            json!({ "paths": [first, second] })
        }
        ("grep", _) => json!({ "pattern": "role|intro" }),
        ("glob", _) => json!({ "pattern": "**/*.md" }),
        (_, Kind::Send) => json!({ "text": rng.words(12) }),
        _ => from_schema(schema, rng, 0),
    }
}

fn from_schema(schema: &Value, rng: &Rng, depth: usize) -> Value {
    if depth > 4 {
        return Value::Null;
    }
    if let Some(choices) = schema.get("enum").and_then(Value::as_array)
        && let Some(choice) = choices.get(rng.below(choices.len()))
    {
        return choice.clone();
    }
    match schema.get("type").and_then(Value::as_str) {
        Some("object") | None if schema.get("properties").is_some() => {
            let properties = schema.get("properties").and_then(Value::as_object);
            let required: Vec<&str> = schema
                .get("required")
                .and_then(Value::as_array)
                .map(|names| names.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let mut object = Map::new();
            for (name, property) in properties.into_iter().flatten() {
                if required.contains(&name.as_str()) {
                    object.insert(name.clone(), from_schema(property, rng, depth + 1));
                }
            }
            Value::Object(object)
        }
        Some("object") | None => json!({}),
        Some("string") => json!(rng.words(3)),
        Some("integer" | "number") => json!(1 + rng.below(5)),
        Some("boolean") => json!(rng.chance(0.5)),
        Some("array") => {
            let item = schema.get("items").cloned().unwrap_or(Value::Null);
            json!([from_schema(&item, rng, depth + 1)])
        }
        Some(_) => Value::Null,
    }
}
