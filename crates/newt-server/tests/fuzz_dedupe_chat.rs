#![expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]

mod support;

use std::collections::BTreeMap;
use std::fmt::Write as _;

use proptest::prelude::*;
use reqwest::Method;
use serde_json::{Value, json};
use sqlx::PgPool;
use support::*;

fn render(value: &Value, reversed: bool, out: &mut String) {
    match value {
        Value::Object(fields) => {
            let sorted: BTreeMap<&String, &Value> = fields.iter().collect();
            let mut entries: Vec<_> = sorted.into_iter().collect();
            if reversed {
                entries.reverse();
            }
            out.push('{');
            for (index, (key, value)) in entries.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&Value::from(key.as_str()).to_string());
                out.push(':');
                render(value, reversed, out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                render(item, reversed, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

fn event_json(id: &str, text: &str, meta: &Value, reversed: bool) -> Vec<u8> {
    let mut out = format!("{{\"text\":{},\"meta\":", Value::from(text));
    render(meta, reversed, &mut out);
    let _ = write!(out, ",\"id\":{}}}", Value::from(id));
    out.into_bytes()
}

#[sqlx::test(migrations = "../../migrations")]
async fn the_same_event_id_is_a_duplicate_for_the_same_content_and_a_conflict_for_other_content(
    pool: PgPool,
) {
    let base = start(pool).await;
    let client = reqwest::Client::new();
    let url = format!("{base}/v1/agents/assistant/sessions/dedupe/events");
    let meta = json_value().prop_filter("NUL is refused", |meta| {
        !meta.to_string().contains("\\u0000")
    });
    let strategy = ("[^\\x00]{0,15}x", "[a-z]{1,8}", meta, "[a-z]{1,5}");
    for (index, (text, id, meta, other)) in samples(strategy).into_iter().enumerate() {
        let id = format!("{id}-{index}");
        let post = |body: Vec<u8>| {
            send(
                &client,
                Method::POST,
                &url,
                Some("secret"),
                Some("application/json"),
                body,
            )
        };
        let (first, first_body) = post(event_json(&id, &text, &meta, false)).await;
        assert_eq!(first, 202, "{}", String::from_utf8_lossy(&first_body));
        let first_body: Value = serde_json::from_slice(&first_body).unwrap();
        assert_eq!(first_body["duplicate"], false);

        let (again, again_body) = post(event_json(&id, &text, &meta, true)).await;
        assert_eq!(again, 202, "{}", String::from_utf8_lossy(&again_body));
        let again_body: Value = serde_json::from_slice(&again_body).unwrap();
        assert_eq!(again_body["duplicate"], true, "{meta}");
        assert_eq!(again_body["inbox_id"], first_body["inbox_id"]);

        let (changed_text, _) =
            post(event_json(&id, &format!("{text}{other}"), &meta, false)).await;
        assert_eq!(changed_text, 409, "text {text:?} + {other:?}");
        let (changed_meta, _) = post(event_json(
            &id,
            &text,
            &json!({ "extra": [meta, other] }),
            false,
        ))
        .await;
        assert_eq!(changed_meta, 409, "{meta}");
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn arbitrary_chat_bodies_never_fail_the_server(pool: PgPool) {
    let base = start(pool).await;
    let client = reqwest::Client::new();
    let part = prop_oneof![
        weird_text().prop_map(|text| json!({ "type": "text", "text": text })),
        ".{0,8}".prop_map(|kind| json!({ "type": kind, "text": "x", "url": "u" })),
        json_value().prop_map(|value| json!({ "type": "text", "text": value })),
        json_value(),
    ];
    let message = (
        prop_oneof![weird_text(), Just(json!("m1"))],
        prop_oneof![
            Just(json!("user")),
            Just(json!("assistant")),
            Just(json!("system")),
            json_value()
        ],
        proptest::option::of(proptest::collection::vec(part, 0..4)),
    )
        .prop_map(|(id, role, parts)| {
            let mut message = json!({ "id": id, "role": role });
            if let Some(parts) = parts {
                message["parts"] = Value::Array(parts);
            }
            message
        });
    let body = prop_oneof![
        3 => proptest::collection::vec(message, 0..4).prop_map(|messages| json!({ "id": "c", "messages": messages }).to_string().into_bytes()),
        1 => json_value().prop_map(|value| value.to_string().into_bytes()),
        1 => proptest::collection::vec(any::<u8>(), 0..100),
    ];
    for (body, content_type) in samples((body, content_type())) {
        let url = format!("{base}/v1/agents/assistant/sessions/chat/chat");
        let (status, answer) = send(
            &client,
            Method::POST,
            &url,
            Some("secret"),
            content_type,
            body.clone(),
        )
        .await;
        assert_contract(
            &format!(
                "chat {:?}",
                String::from_utf8_lossy(&body[..body.len().min(300)])
            ),
            status,
            &answer,
        );
    }
}
