use proptest::prelude::*;

use super::*;

fn json_value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(|n| json!(n)),
        any::<f64>().prop_map(|n| json!(n)),
        "[a-z.\"{}:, ]{0,12}".prop_map(Value::String),
        any::<String>().prop_map(Value::String),
    ];
    leaf.prop_recursive(4, 48, 6, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..6).prop_map(Value::Array),
            prop::collection::hash_map("[a-z._]{0,6}", inner, 0..6)
                .prop_map(|map| Value::Object(map.into_iter().collect())),
        ]
    })
}

const NAMES: &[&str] = &[
    "save_note",
    "send_message",
    "read_file",
    "batch",
    "end_turn",
    "checkpoint",
    "functions.save_note",
    "functions.send_message",
    "functions.batch",
    "functions.end_turn",
    "functions.unknown",
    "unknown",
    "",
    "functions.",
    "Functions.save_note",
];

fn tools() -> HashMap<String, Retention> {
    HashMap::from([
        ("send_message".to_owned(), Retention::Outward),
        ("save_note".to_owned(), Retention::Durable),
        ("read_file".to_owned(), Retention::Ephemeral),
        (BATCH.to_owned(), Retention::Ephemeral),
        (END_TURN.to_owned(), Retention::Ephemeral),
        (CHECKPOINT.to_owned(), Retention::Durable),
    ])
}

fn argument() -> impl Strategy<Value = Value> {
    prop_oneof![
        Just(json!({})),
        Just(json!({ "text": "x" })),
        Just(json!("{\"text\":\"x\"}")),
        Just(json!("{\"text\":")),
        Just(json!("[1]")),
        Just(Value::Null),
        Just(json!(3)),
        Just(json!([1, 2])),
        json_value(),
    ]
}

fn inner_call() -> impl Strategy<Value = Value> {
    (
        prop::sample::select(NAMES),
        argument(),
        prop::option::of(prop::collection::vec("[a-z.]{0,5}", 0..3)),
        prop::option::of(0usize..20),
    )
        .prop_map(|(name, arguments, fields, limit)| {
            let mut call = json!({ "name": name, "arguments": arguments });
            if let Some(fields) = fields {
                call["fields"] = json!(fields);
            }
            if let Some(limit) = limit {
                call["limit"] = json!(limit);
            }
            call
        })
}

fn resolved(name: &str, tools: &HashMap<String, Retention>) -> String {
    match name.strip_prefix("functions.") {
        Some(bare) if !tools.contains_key(name) && tools.contains_key(bare) => bare.to_owned(),
        _ => name.to_owned(),
    }
}

fn batch(arguments: Value) -> ToolCall {
    ToolCall {
        id: "b".into(),
        name: BATCH.into(),
        arguments,
    }
}

proptest! {
    #[test]
    fn any_batch_arguments_expand_without_panicking(arguments in json_value()) {
        let expanded = expand(&[batch(arguments)], &tools());
        prop_assert_eq!(expanded.groups.len(), 1);
        prop_assert!(!expanded.calls.is_empty());
    }

    #[test]
    fn a_batch_is_accepted_exactly_when_the_rules_allow_it(
        calls in prop::collection::vec(inner_call(), 0..13),
    ) {
        let tools = tools();
        let names: Vec<String> = calls
            .iter()
            .map(|call| resolved(call["name"].as_str().unwrap_or_default(), &tools))
            .collect();
        let nested = names.iter().any(|name| [BATCH, END_TURN, CHECKPOINT].contains(&name.as_str()));
        let outward = names
            .iter()
            .filter(|name| tools.get(name.as_str()) == Some(&Retention::Outward))
            .count();
        let accepted = (1..=MAX_CALLS).contains(&calls.len()) && !nested && outward <= 1;

        let expanded = expand(&[batch(json!({ "calls": calls }))], &tools);

        prop_assert_eq!(expanded.preset.contains_key("b"), !accepted);
        if accepted {
            prop_assert_eq!(expanded.calls.len(), calls.len());
            for (index, call) in expanded.calls.iter().enumerate() {
                prop_assert_eq!(&call.id, &format!("b.{index}"));
                prop_assert_eq!(&call.name, &names[index]);
                prop_assert!(call.arguments.is_object());
            }
        } else {
            prop_assert_eq!(expanded.calls.len(), 1);
            prop_assert!(expanded.preset["b"].is_error);
        }
    }

    #[test]
    fn calls_that_are_not_batches_pass_through_with_only_their_name_resolved(
        name in prop::sample::select(NAMES.iter().copied().filter(|name| resolved(name, &tools()) != BATCH).collect::<Vec<_>>()),
        arguments in json_value(),
    ) {
        let call = ToolCall { id: "c".into(), name: name.to_owned(), arguments };
        let expanded = expand(std::slice::from_ref(&call), &tools());
        let expected = ToolCall { name: resolved(name, &tools()), ..call };
        prop_assert_eq!(&expanded.calls, &vec![expected]);
        prop_assert!(expanded.preset.is_empty());
    }

    #[test]
    fn projecting_any_content_never_panics_and_keeps_json_json(
        value in json_value(),
        fields in prop::collection::vec("[a-z.]{0,6}", 0..4),
        limit in prop::option::of(0usize..5),
    ) {
        let content = value.to_string();
        let projected = project(&content, &Projection { fields, limit });
        let body = projected.split("\n(").next().unwrap_or_default();
        prop_assert!(serde_json::from_str::<Value>(body).is_ok());
    }

    #[test]
    fn a_limit_caps_every_top_level_list(
        items in prop::collection::vec(json_value(), 0..12),
        limit in 1usize..6,
    ) {
        let content = json!({ "items": items.clone(), "more": items.clone() }).to_string();
        let projected = project(&content, &Projection { fields: Vec::new(), limit: Some(limit) });
        let body = projected.split("\n(").next().unwrap_or_default();
        let value: Value = serde_json::from_str(body).unwrap_or_default();
        prop_assert!(value["items"].as_array().is_some_and(|kept| kept.len() == items.len().min(limit)));
        let hidden = 2 * items.len().saturating_sub(limit);
        prop_assert_eq!(projected.contains("more items not shown"), hidden > 0);
    }

    #[test]
    fn folding_gives_one_result_per_model_call_in_order(
        calls in prop::collection::vec(inner_call(), 1..6),
        failed in prop::collection::vec(any::<bool>(), 6),
        missing in prop::collection::vec(any::<bool>(), 8),
    ) {
        let tools = tools();
        let model_calls = vec![
            ToolCall { id: "a".into(), name: "read_file".into(), arguments: json!({}) },
            batch(json!({ "calls": calls })),
            ToolCall { id: "z".into(), name: "save_note".into(), arguments: json!({}) },
        ];
        let expanded = expand(&model_calls, &tools);
        let results: Vec<Option<ToolResult>> = expanded
            .calls
            .iter()
            .enumerate()
            .map(|(slot, call)| {
                (!missing[slot % missing.len()] || expanded.preset.contains_key(&call.id)).then(|| ToolResult {
                    call_id: call.id.clone(),
                    content: format!("out {slot}"),
                    is_error: failed[slot % failed.len()],
                })
            })
            .collect();
        let durations = vec![1; expanded.calls.len()];
        let folded = fold(&expanded, results, HashMap::new(), &tools, &durations);
        let ids: Vec<&str> = folded.results.iter().map(|result| result.call_id.as_str()).collect();
        prop_assert!(ids.iter().all(|id| ["a", "b", "z"].contains(id)));
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        prop_assert_eq!(sorted.len(), ids.len());
        prop_assert!(ids.windows(2).all(|pair| pair[0] < pair[1]));
        prop_assert!(ids.contains(&"b"));
    }
}
