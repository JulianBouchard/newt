use proptest::prelude::*;
use serde_json::{Value, json};

use super::EndTurn;

fn json_value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(|number| Value::Number(number.into())),
        any::<String>().prop_map(Value::String),
    ];
    leaf.prop_recursive(3, 24, 4, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
            prop::collection::btree_map("(recorded|nothing_new|[a-z]{1,4})", inner, 0..4)
                .prop_map(|map| Value::Object(map.into_iter().collect())),
        ]
    })
}

fn text() -> impl Strategy<Value = String> {
    prop_oneof![Just(String::new()), "[ \t\n]{0,3}", any::<String>()]
}

fn filled(text: &str) -> bool {
    !text.trim().is_empty()
}

proptest! {
    #[test]
    fn arbitrary_json_never_panics(value in json_value(), strict in any::<bool>()) {
        let _ = EndTurn::parse(&value, strict);
    }

    #[test]
    fn end_turn_follows_the_protocol_for_well_typed_arguments(
        recorded in prop::option::of(prop::collection::vec(text(), 0..4)),
        nothing_new in prop::option::of(text()),
        strict in any::<bool>(),
    ) {
        let mut arguments = serde_json::Map::new();
        if let Some(recorded) = &recorded {
            arguments.insert("recorded".into(), json!(recorded));
        }
        if let Some(reason) = &nothing_new {
            arguments.insert("nothing_new".into(), json!(reason));
        }
        let has_recorded = recorded.iter().flatten().any(|item| filled(item));
        let has_reason = nothing_new.as_deref().is_some_and(filled);
        let parsed = EndTurn::parse(&Value::Object(arguments), strict);
        let allowed = !(has_recorded && has_reason) && (!strict || has_recorded || has_reason);
        prop_assert_eq!(parsed.is_ok(), allowed);
    }

    #[test]
    fn a_parsed_end_turn_keeps_the_fields_it_was_given(
        recorded in prop::collection::vec(text(), 0..4),
        strict in any::<bool>(),
    ) {
        let parsed = EndTurn::parse(&json!({ "recorded": recorded }), strict);
        if let Ok(end) = parsed {
            prop_assert_eq!(end.recorded, recorded);
            prop_assert_eq!(end.nothing_new, None);
        }
    }
}
