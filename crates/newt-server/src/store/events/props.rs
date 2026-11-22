use proptest::prelude::*;
use serde_json::{Map, Value, json};

use super::{InboundEvent, InboundKind, Policy, Priority};

fn json_value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::from),
        any::<i64>().prop_map(Value::from),
        any::<u64>().prop_map(Value::from),
        any::<f64>().prop_map(|number| json!(number)),
        ".{0,12}".prop_map(Value::from),
        Just(json!("\"}],\\")),
    ];
    leaf.prop_recursive(4, 32, 4, |inner| {
        prop_oneof![
            proptest::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
            proptest::collection::btree_map(".{0,6}", inner, 0..4)
                .prop_map(|map| Value::Object(map.into_iter().collect())),
        ]
    })
}

fn event(text: &str, meta: Option<Value>) -> InboundEvent {
    InboundEvent {
        id: "id".to_owned(),
        kind: InboundKind::Message,
        policy: Policy::Steer,
        priority: Priority::Interactive,
        text: text.to_owned(),
        meta,
    }
}

fn reversed_keys(value: &Value) -> String {
    match value {
        Value::Object(fields) => {
            let mut entries: Vec<(&String, &Value)> = fields.iter().collect();
            entries.sort_by_key(|entry| entry.0);
            entries.reverse();
            let rendered: Vec<String> = entries
                .into_iter()
                .map(|(key, value)| {
                    format!("{}:{}", Value::from(key.as_str()), reversed_keys(value))
                })
                .collect();
            format!("{{{}}}", rendered.join(","))
        }
        Value::Array(items) => {
            let rendered: Vec<String> = items.iter().map(reversed_keys).collect();
            format!("[{}]", rendered.join(","))
        }
        other => other.to_string(),
    }
}

fn change_nth_leaf(value: &Value, target: usize, seen: &mut usize) -> Option<Value> {
    match value {
        Value::Object(fields) => {
            let mut changed = Map::new();
            let mut done = false;
            for (key, field) in fields {
                match (done, change_nth_leaf(field, target, seen)) {
                    (false, Some(new)) => {
                        changed.insert(key.clone(), new);
                        done = true;
                    }
                    _ => {
                        changed.insert(key.clone(), field.clone());
                    }
                }
            }
            done.then_some(Value::Object(changed))
        }
        Value::Array(items) => {
            let mut changed = Vec::new();
            let mut done = false;
            for item in items {
                match (done, change_nth_leaf(item, target, seen)) {
                    (false, Some(new)) => {
                        changed.push(new);
                        done = true;
                    }
                    _ => changed.push(item.clone()),
                }
            }
            done.then_some(Value::Array(changed))
        }
        leaf => {
            *seen += 1;
            if *seen - 1 != target {
                return None;
            }
            Some(match leaf {
                Value::Null => json!(false),
                Value::Bool(flag) => json!(!flag),
                Value::Number(number) => match number.as_i64() {
                    Some(int) if int < i64::MAX => json!(int + 1),
                    _ => json!("number"),
                },
                Value::String(text) => json!(format!("{text}x")),
                other => other.clone(),
            })
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    #[test]
    fn object_key_order_in_meta_never_changes_the_hash(text in ".{0,20}", meta in json_value()) {
        let reparsed: Value = serde_json::from_str(&reversed_keys(&meta)).expect("rendered JSON parses");
        prop_assert_eq!(
            event(&text, Some(meta)).content_hash(),
            event(&text, Some(reparsed)).content_hash()
        );
    }

    #[test]
    fn changing_the_text_changes_the_hash(text in ".{0,20}", extra in ".{1,5}", meta in proptest::option::of(json_value())) {
        prop_assert_ne!(
            event(&text, meta.clone()).content_hash(),
            event(&format!("{text}{extra}"), meta).content_hash()
        );
    }

    #[test]
    fn changing_any_one_meta_value_changes_the_hash(text in ".{0,20}", meta in json_value(), pick in any::<usize>()) {
        let leaves = {
            let mut count = 0;
            let mut probe = |value: &Value| {
                let mut stack = vec![value];
                while let Some(value) = stack.pop() {
                    match value {
                        Value::Object(fields) => stack.extend(fields.values()),
                        Value::Array(items) => stack.extend(items),
                        _ => count += 1,
                    }
                }
            };
            probe(&meta);
            count
        };
        prop_assume!(leaves > 0);
        let mut seen = 0;
        let changed = change_nth_leaf(&meta, pick % leaves, &mut seen).expect("a leaf exists");
        prop_assume!(changed != meta);
        prop_assert_ne!(
            event(&text, Some(meta)).content_hash(),
            event(&text, Some(changed)).content_hash()
        );
    }

    #[test]
    fn text_and_meta_cannot_trade_content(text in ".{0,12}", other in ".{0,12}") {
        prop_assume!(text != other);
        prop_assert_ne!(
            event(&text, Some(json!(other.clone()))).content_hash(),
            event(&other, Some(json!(text.clone()))).content_hash(),
            "swapped text and meta must differ unless they are equal: {} {}", text, other
        );
    }
}
