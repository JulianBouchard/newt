use std::sync::Arc;

use newt_core::tools::Retention;
use proptest::prelude::*;
use rmcp::model::{CallToolResult, ContentBlock, JsonObject, MetaObject, Tool};
use serde_json::{Map, Value, json};

use super::call::tool_output;
use super::tools::tool_def;
use super::{Owner, check_pages};

fn garbage() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::from),
        any::<i64>().prop_map(Value::from),
        any::<f64>().prop_map(|number| json!(number)),
        ".{0,8}".prop_map(Value::from),
    ];
    leaf.prop_recursive(3, 16, 4, |inner| {
        prop_oneof![
            proptest::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
            proptest::collection::btree_map(".{0,5}", inner, 0..3)
                .prop_map(|map| Value::Object(map.into_iter().collect())),
        ]
    })
}

fn flag() -> impl Strategy<Value = Value> {
    prop_oneof![
        3 => any::<bool>().prop_map(Value::from),
        2 => Just(Value::Null),
        2 => Just(json!("true")),
        1 => Just(json!(1)),
        2 => garbage(),
    ]
}

fn retention() -> impl Strategy<Value = Value> {
    prop_oneof![
        4 => proptest::sample::select(vec!["ephemeral", "durable", "outward"]).prop_map(Value::from),
        2 => proptest::sample::select(vec!["Durable", "", "outward ", "OUTWARD", "none"]).prop_map(Value::from),
        2 => garbage(),
    ]
}

fn page_keys() -> impl Strategy<Value = Value> {
    prop_oneof![
        4 => proptest::collection::vec(proptest::collection::vec("[a-z]{0,3}", 0..3), 0..3).prop_map(|keys| json!(keys)),
        1 => Just(json!(["tasks"])),
        1 => Just(json!([["a", 1]])),
        1 => Just(json!({})),
        2 => garbage(),
    ]
}

fn page() -> impl Strategy<Value = Value> {
    prop_oneof![
        5 => (
            proptest::collection::vec("[a-z]{1,3}", 0..3),
            prop_oneof![Just("a.md".to_owned()), Just("b.md".to_owned()), Just(" ".to_owned()), Just(String::new()), "[a-z/.]{0,6}"],
        )
            .prop_map(|(key, path)| json!({ "key": key, "path": path, "version": "1", "content": "text" })),
        1 => garbage(),
        1 => Just(json!({ "key": ["k"], "path": "p.md", "version": 3, "content": "x" })),
        1 => Just(json!({ "key": ["k"], "path": "p.md" })),
    ]
}

fn pages() -> impl Strategy<Value = Value> {
    prop_oneof![
        5 => proptest::collection::vec(page(), 0..4).prop_map(Value::Array),
        1 => garbage(),
    ]
}

fn summary() -> impl Strategy<Value = Value> {
    prop_oneof![3 => ".{0,20}".prop_map(Value::from), 2 => garbage()]
}

fn meta_with(
    known: Vec<(&'static str, Option<Value>)>,
    unknown: Vec<(String, Value)>,
) -> Map<String, Value> {
    let mut meta = Map::new();
    for (key, value) in unknown {
        meta.insert(format!("other/{key}"), value);
    }
    for (key, value) in known {
        if let Some(value) = value {
            meta.insert(key.to_owned(), value);
        }
    }
    meta
}

fn unknown() -> impl Strategy<Value = Vec<(String, Value)>> {
    proptest::collection::vec(("[a-z]{0,4}", garbage()), 0..3)
}

fn page_key_list(value: &Value) -> Option<Vec<Vec<String>>> {
    value
        .as_array()?
        .iter()
        .map(|key| {
            key.as_array()?
                .iter()
                .map(|part| part.as_str().map(str::to_owned))
                .collect()
        })
        .collect()
}

fn tool_with(meta: Map<String, Value>) -> Tool {
    let mut schema = JsonObject::new();
    schema.insert("type".to_owned(), json!("object"));
    let mut fields = MetaObject::new();
    for (key, value) in meta {
        fields.insert(key, value);
    }
    Tool::new("probe", "Probe.", Arc::new(schema)).with_meta(fields)
}

fn expected_tool(meta: &Map<String, Value>) -> Option<(Retention, Vec<Vec<String>>, bool)> {
    let retention = match meta.get("newt/retention").map(Value::as_str) {
        None => Retention::Durable,
        Some(Some("ephemeral")) => Retention::Ephemeral,
        Some(Some("durable")) => Retention::Durable,
        Some(Some("outward")) => Retention::Outward,
        Some(_) => return None,
    };
    let invalidates = match meta.get("newt/invalidates") {
        None => Vec::new(),
        Some(value) => page_key_list(value)?,
    };
    let flag = |key: &str| match meta.get(key) {
        None => Some(false),
        Some(Value::Bool(value)) => Some(*value),
        Some(_) => None,
    };
    let idempotent = flag("newt/idempotent")?;
    let ends_turn = flag("newt/ends_turn")?;
    if retention == Retention::Outward && !idempotent {
        return None;
    }
    Some((retention, invalidates, ends_turn))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(3000))]

    #[test]
    fn tool_meta_is_applied_exactly_as_documented_or_refused_with_an_error(
        retention in proptest::option::of(retention()),
        invalidates in proptest::option::of(page_keys()),
        idempotent in proptest::option::of(flag()),
        ends_turn in proptest::option::of(flag()),
        extra in unknown(),
        speaks_newt in any::<bool>(),
        added in any::<bool>(),
    ) {
        let meta = meta_with(
            vec![
                ("newt/retention", retention),
                ("newt/invalidates", invalidates),
                ("newt/idempotent", idempotent),
                ("newt/ends_turn", ends_turn),
            ],
            extra,
        );
        let owner = if added { Owner::added("srv") } else { Owner::HOME };
        let result = tool_def(tool_with(meta.clone()), "probe", speaks_newt, &owner);
        if !speaks_newt {
            let def = result.expect("a plain server's tool always loads");
            prop_assert_eq!(def.retention, Retention::Durable);
            prop_assert!(def.invalidates.is_empty());
            prop_assert!(!def.ends_turn);
        } else {
            match (expected_tool(&meta), result) {
                (Some((retention, invalidates, ends_turn)), Ok(def)) => {
                    prop_assert_eq!(def.retention, retention);
                    prop_assert_eq!(def.ends_turn, ends_turn);
                    let scoped: Vec<_> = invalidates.into_iter().map(|key| owner.key(key)).collect();
                    prop_assert_eq!(def.invalidates, scoped);
                }
                (None, Err(message)) => prop_assert!(message.contains("newt/"), "{message}"),
                (expected, result) => prop_assert!(false, "{meta:?}: expected {expected:?}, got {:?}", result.map(|def| (def.retention, def.ends_turn))),
            }
        }
    }
}

enum ExpectedResult {
    Applied,
    Dropped,
}

fn expected_result(meta: &Map<String, Value>) -> ExpectedResult {
    let summary_ok = meta.get("newt/summary").is_none_or(Value::is_string);
    let invalidate_ok = meta
        .get("newt/invalidate")
        .is_none_or(|value| page_key_list(value).is_some());
    let ends_ok = meta.get("newt/ends_turn").is_none_or(Value::is_boolean);
    let pages_ok = meta.get("newt/pages").is_none_or(|value| {
        let Ok(pages) = serde_json::from_value::<Vec<newt_core::workspace::Page>>(value.clone())
        else {
            return false;
        };
        check_pages(&pages).is_ok()
    });
    if summary_ok && invalidate_ok && ends_ok && pages_ok {
        ExpectedResult::Applied
    } else {
        ExpectedResult::Dropped
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(3000))]

    #[test]
    fn result_meta_is_applied_exactly_as_documented_or_dropped_whole(
        summary in proptest::option::of(summary()),
        invalidate in proptest::option::of(page_keys()),
        ends_turn in proptest::option::of(flag()),
        pages in proptest::option::of(pages()),
        extra in unknown(),
        is_error in any::<bool>(),
        added in any::<bool>(),
    ) {
        let meta = meta_with(
            vec![
                ("newt/summary", summary),
                ("newt/invalidate", invalidate),
                ("newt/ends_turn", ends_turn),
                ("newt/pages", pages),
            ],
            extra,
        );
        let owner = if added { Owner::added("srv") } else { Owner::HOME };
        let mut fields = MetaObject::new();
        for (key, value) in meta.clone() {
            fields.insert(key, value);
        }
        let mut result = CallToolResult::success(vec![ContentBlock::text("ok")]).with_meta(Some(fields));
        result.is_error = Some(is_error);
        let output = tool_output(result, &owner);
        prop_assert_eq!(output.is_error, is_error);
        prop_assert!(output.content.starts_with("ok"));
        match expected_result(&meta) {
            ExpectedResult::Applied => {
                prop_assert_eq!(output.content.as_str(), "ok", "{:?}", meta);
                prop_assert_eq!(output.summary.as_deref(), meta.get("newt/summary").and_then(Value::as_str));
                prop_assert_eq!(output.ends_turn, meta.get("newt/ends_turn").and_then(Value::as_bool));
                let scoped: Vec<_> = meta.get("newt/invalidate").and_then(page_key_list).unwrap_or_default().into_iter().map(|key| owner.key(key)).collect();
                prop_assert_eq!(output.invalidate, scoped);
                let count = meta.get("newt/pages").and_then(Value::as_array).map_or(0, Vec::len);
                prop_assert_eq!(output.pages.len(), count);
                for page in &output.pages {
                    prop_assert!(owner.may_own(&page.key));
                    prop_assert!(page.path.starts_with(&owner.path("")));
                }
            }
            ExpectedResult::Dropped => {
                prop_assert!(output.content.contains("could not read"), "{:?}", meta);
                prop_assert!(output.summary.is_none());
                prop_assert!(output.ends_turn.is_none());
                prop_assert!(output.pages.is_empty());
                prop_assert_eq!(output.invalidate, vec![owner.key(Vec::new())]);
            }
        }
    }
}
