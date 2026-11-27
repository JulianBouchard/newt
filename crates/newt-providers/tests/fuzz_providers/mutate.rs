use proptest::prelude::*;
use serde_json::{Value, json};

fn replacement(kind: u8) -> Value {
    match kind {
        0 => Value::Null,
        1 => json!(7),
        2 => json!("text"),
        3 => json!([]),
        4 => json!({}),
        5 => json!(-1.5e300),
        _ => json!(true),
    }
}

fn mutate_value(value: &mut Value, target: usize, kind: u8, counter: &mut usize) {
    if *counter == target {
        *value = replacement(kind);
        *counter += 1;
        return;
    }
    *counter += 1;
    match value {
        Value::Array(items) => {
            for item in items {
                mutate_value(item, target, kind, counter);
            }
        }
        Value::Object(map) => {
            for (_, item) in map.iter_mut() {
                mutate_value(item, target, kind, counter);
            }
        }
        _ => {}
    }
}

pub(super) fn mutate(
    body: &str,
    edits: &[(prop::sample::Index, prop::sample::Index, u8)],
) -> String {
    let mut lines: Vec<String> = body.lines().map(str::to_owned).collect();
    for (line_pick, node_pick, kind) in edits {
        let data_lines: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, line)| line.starts_with("data: {"))
            .map(|(index, _)| index)
            .collect();
        let line = data_lines[line_pick.index(data_lines.len())];
        let Ok(mut value) = serde_json::from_str::<Value>(&lines[line]["data: ".len()..]) else {
            continue;
        };
        let mut nodes = 0;
        count_nodes(&value, &mut nodes);
        let target = node_pick.index(nodes);
        let mut counter = 0;
        mutate_value(&mut value, target, *kind, &mut counter);
        lines[line] = format!("data: {value}");
    }
    let mut text = lines.join("\n");
    text.push('\n');
    text
}

fn count_nodes(value: &Value, nodes: &mut usize) {
    *nodes += 1;
    match value {
        Value::Array(items) => items.iter().for_each(|item| count_nodes(item, nodes)),
        Value::Object(map) => map.values().for_each(|item| count_nodes(item, nodes)),
        _ => {}
    }
}
