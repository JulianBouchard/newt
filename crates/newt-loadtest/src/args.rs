use serde_json::{Map, Value, json};

use crate::profile::{Kind, Profile};
use crate::sample::Rng;

pub fn arguments_for(
    name: &str,
    kind: Kind,
    schema: &Value,
    rng: &Rng,
    profile: &Profile,
) -> Value {
    match (name, kind) {
        (_, Kind::Skill) => {
            let skill = &profile.skills[rng.below(profile.skills.len())];
            if name == "read_file" {
                read_one(
                    schema,
                    &format!("skills/{skill}.md"),
                    &profile.file_path_prefix,
                )
            } else {
                let offered = property(schema, "name")
                    .and_then(|name| name.get("enum"))
                    .and_then(Value::as_array)
                    .filter(|names| !names.is_empty());
                offered.map_or_else(
                    || json!({ "name": skill }),
                    |names| json!({ "name": names[rng.below(names.len())] }),
                )
            }
        }
        ("read_file", _) => read_one(
            schema,
            &profile.pages[rng.below(profile.pages.len())],
            &profile.file_path_prefix,
        ),
        ("grep", _) => json!({ "pattern": "order|refund" }),
        ("glob", _) => json!({ "pattern": "**/*.md" }),
        (_, Kind::Send) => send(schema, rng),
        _ => from_schema(schema, rng, 0),
    }
}

fn property<'a>(schema: &'a Value, name: &str) -> Option<&'a Value> {
    schema
        .get("properties")
        .and_then(|properties| properties.get(name))
}

fn read_one(schema: &Value, path: &str, prefix: &str) -> Value {
    if property(schema, "filePath").is_some() {
        json!({ "filePath": format!("{prefix}{path}") })
    } else {
        json!({ "paths": [path] })
    }
}

fn send(schema: &Value, rng: &Rng) -> Value {
    let Some(channels) = property(schema, "channel")
        .and_then(|channel| channel.get("enum"))
        .and_then(Value::as_array)
    else {
        return json!({ "text": rng.words(12) });
    };
    let channel = channels
        .iter()
        .find(|channel| channel.as_str() == Some("web"))
        .or_else(|| channels.first())
        .cloned()
        .unwrap_or(Value::Null);
    json!({ "command": "send", "channel": channel, "message": rng.words(12) })
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
