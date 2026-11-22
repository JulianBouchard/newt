use proptest::prelude::*;
use serde_json::{Map, Value};

use super::tool_arguments;
use crate::message::INVALID_ARGUMENTS_KEY;

fn json_value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(|number| Value::Number(number.into())),
        any::<String>().prop_map(Value::String),
    ];
    leaf.prop_recursive(4, 48, 6, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..6).prop_map(Value::Array),
            prop::collection::btree_map(any::<String>(), inner, 0..6)
                .prop_map(|map| Value::Object(map.into_iter().collect::<Map<_, _>>())),
        ]
    })
}

fn json_object() -> impl Strategy<Value = Value> {
    json_value().prop_filter("object", Value::is_object)
}

proptest! {
    #[test]
    fn any_string_becomes_an_object(raw in any::<String>()) {
        prop_assert!(tool_arguments(&raw).is_object());
    }

    #[test]
    fn any_serialized_object_round_trips(object in json_object(), pad in "[ \t\r\n]{0,3}") {
        let raw = format!("{pad}{object}{pad}");
        prop_assert_eq!(tool_arguments(&raw), object);
    }

    #[test]
    fn serialized_non_objects_are_flagged_with_the_raw_text(value in json_value()) {
        prop_assume!(!value.is_object());
        let raw = value.to_string();
        let flagged = tool_arguments(&raw);
        prop_assert_eq!(flagged[INVALID_ARGUMENTS_KEY].as_str(), Some(raw.as_str()));
    }

    #[test]
    fn a_truncated_object_is_flagged_not_repaired(object in json_object(), cut in any::<prop::sample::Index>()) {
        let raw = object.to_string();
        let boundaries: Vec<usize> = raw.char_indices().map(|(index, _)| index).skip(1).collect();
        prop_assume!(!boundaries.is_empty());
        let end = boundaries[cut.index(boundaries.len())];
        let prefix = &raw[..end];
        prop_assume!(serde_json::from_str::<Value>(prefix).is_err() && !prefix.trim().is_empty());
        let flagged = tool_arguments(prefix);
        prop_assert_eq!(flagged[INVALID_ARGUMENTS_KEY].as_str(), Some(prefix));
    }
}

#[test]
fn deeply_nested_input_is_flagged_without_overflowing_the_stack() {
    let raw = format!("{}{}", "[".repeat(100_000), "]".repeat(100_000));
    assert!(tool_arguments(&raw).get(INVALID_ARGUMENTS_KEY).is_some());
    let raw = format!("{}1{}", "{\"a\":".repeat(100_000), "}".repeat(100_000));
    assert!(tool_arguments(&raw).get(INVALID_ARGUMENTS_KEY).is_some());
}
