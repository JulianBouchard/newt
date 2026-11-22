use super::*;

fn projection(fields: &[&str], limit: Option<usize>) -> Projection {
    Projection {
        fields: fields.iter().map(|field| (*field).to_owned()).collect(),
        limit,
    }
}

#[test]
fn fields_keep_only_the_named_paths_in_every_list_item() {
    let content = r#"[{"id":1,"role":{"title":"Staff","salary":1},"notes":"long"},{"id":2,"role":{"title":"Lead"}}]"#;
    assert_eq!(
        project(content, &projection(&["id", "role.title"], None)),
        r#"[{"id":1,"role":{"title":"Staff"}},{"id":2,"role":{"title":"Lead"}}]"#
    );
}

#[test]
fn limit_caps_lists_and_says_how_many_were_hidden() {
    let content = r#"{"roles":[1,2,3,4],"total":4}"#;
    assert_eq!(
        project(content, &projection(&[], Some(2))),
        "{\"roles\":[1,2],\"total\":4}\n(2 more items not shown)"
    );
}

#[test]
fn non_json_results_pass_through() {
    assert_eq!(
        project("plain text", &projection(&["a"], Some(1))),
        "plain text"
    );
}

fn batch_call(arguments: Value) -> ToolCall {
    ToolCall {
        id: "b1".into(),
        name: BATCH.into(),
        arguments,
    }
}

fn retention() -> HashMap<String, Retention> {
    HashMap::from([
        ("send_message".to_owned(), Retention::Outward),
        ("save_note".to_owned(), Retention::Durable),
    ])
}

#[test]
fn a_batch_expands_into_inner_calls_with_derived_ids() {
    let expanded = expand(
        &[batch_call(json!({ "calls": [
            { "name": "save_note", "arguments": { "text": "x" } },
            { "name": "send_message", "arguments": {}, "limit": 3 },
        ] }))],
        &retention(),
    );
    let ids: Vec<&str> = expanded.calls.iter().map(|call| call.id.as_str()).collect();
    assert_eq!(ids, vec!["b1.0", "b1.1"]);
    assert!(expanded.projections.contains_key(&1));
    assert!(expanded.preset.is_empty());
}

#[test]
fn a_namespace_prefix_the_model_adds_resolves_to_the_tool() {
    let expanded = expand(
        &[batch_call(json!({ "calls": [
            { "name": "functions.save_note", "arguments": { "text": "x" } },
            { "name": "functions.unknown", "arguments": {} },
        ] }))],
        &retention(),
    );
    let names: Vec<&str> = expanded
        .calls
        .iter()
        .map(|call| call.name.as_str())
        .collect();
    assert_eq!(names, vec!["save_note", "functions.unknown"]);
}

#[test]
fn two_prefixed_outward_calls_are_still_refused() {
    let expanded = expand(
        &[batch_call(json!({ "calls": [
            { "name": "functions.send_message", "arguments": {} },
            { "name": "functions.send_message", "arguments": {} },
        ] }))],
        &retention(),
    );
    assert!(expanded.preset["b1"].is_error);
}

#[test]
fn invalid_batches_become_errors_without_running_anything() {
    for arguments in [
        json!({ "calls": [] }),
        json!({ "calls": [{ "name": "batch", "arguments": {} }] }),
        json!({ "calls": [{ "name": "end_turn", "arguments": {} }] }),
        json!({ "calls": [
            { "name": "send_message", "arguments": {} },
            { "name": "send_message", "arguments": {} },
        ] }),
    ] {
        let expanded = expand(&[batch_call(arguments)], &retention());
        assert_eq!(expanded.calls.len(), 1);
        assert!(expanded.preset["b1"].is_error);
    }
}

#[test]
fn arguments_sent_as_a_json_string_become_the_object() {
    let expanded = expand(
        &[batch_call(json!({ "calls": [
            { "name": "save_note", "arguments": "{\"text\":\"x\"}" },
            { "name": "save_note", "arguments": "not json" },
        ] }))],
        &retention(),
    );
    assert_eq!(expanded.calls[0].arguments, json!({ "text": "x" }));
    assert!(expanded.calls[1].invalid_arguments().is_some());
}
