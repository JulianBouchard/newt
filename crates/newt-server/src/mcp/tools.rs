use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use newt_core::prompt::ToolSpec;
use newt_core::tools::{Retention, ToolDef};
use newt_core::workspace::PageKey;
use rmcp::model::{JsonObject, Tool};
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::connection::Client;
use super::{
    MAX_TOOL_NAME, META_ENDS_TURN, META_IDEMPOTENCY_KEY, META_IDEMPOTENT, META_INVALIDATES,
    META_RETENTION, Owner, Remote, TOOL_SEPARATOR,
};

/// A tool as the model will see it: its name, its definition and where calls
/// to it go. Fails for a tool newt cannot offer.
pub(super) fn offer(
    tool: Tool,
    server: &str,
    client: &Arc<Client>,
    speaks_newt: bool,
    owner: &Owner,
    stale: &Arc<AtomicBool>,
) -> Result<(String, ToolDef, Remote), String> {
    let tool_name = tool.name.to_string();
    let name = match &owner.0 {
        Some(prefix) => format!("{prefix}{TOOL_SEPARATOR}{tool_name}"),
        None => tool_name.clone(),
    };
    if name.len() > MAX_TOOL_NAME {
        return Err(format!(
            "MCP server `{server}`: tool name `{name}` is longer than the {MAX_TOOL_NAME} characters model APIs accept"
        ));
    }
    let def = tool_def(tool, &name, speaks_newt, owner)
        .map_err(|error| format!("MCP server `{server}`, tool `{tool_name}`: {error}"))?;
    let remote = Remote {
        client: Arc::clone(client),
        server: server.to_owned(),
        tool: tool_name,
        speaks_newt,
        owner: owner.clone(),
        stale: Arc::clone(stale),
    };
    Ok((name, def, remote))
}

pub(super) fn meta_field<T: DeserializeOwned>(
    meta: Option<&JsonObject>,
    key: &str,
) -> Result<Option<T>, String> {
    meta.and_then(|meta| meta.get(key))
        .map(|value| {
            T::deserialize(value).map_err(|error| format!("`_meta.{key}` is invalid: {error}"))
        })
        .transpose()
}

/// A tool as the model sees it. A server that speaks newt decides its tool's
/// retention and invalidations; any other server's tools are `durable` and
/// invalidate nothing, whatever their `_meta` says.
pub(super) fn tool_def(
    tool: Tool,
    name: &str,
    speaks_newt: bool,
    owner: &Owner,
) -> Result<ToolDef, String> {
    let (retention, invalidates, ends_turn, idempotent) = if speaks_newt {
        let meta = tool.meta.as_deref();
        let retention: Retention = meta_field(meta, META_RETENTION)?.unwrap_or(Retention::Durable);
        let invalidates: Vec<PageKey> = meta_field(meta, META_INVALIDATES)?.unwrap_or_default();
        let idempotent: bool = meta_field(meta, META_IDEMPOTENT)?.unwrap_or(false);
        let ends_turn: bool = meta_field(meta, META_ENDS_TURN)?.unwrap_or(false);
        if retention == Retention::Outward && !idempotent {
            return Err(format!(
                "outward tools must set `_meta.{META_IDEMPOTENT}: true` and honour `_meta.{META_IDEMPOTENCY_KEY}`, or a crash could send twice"
            ));
        }
        (retention, invalidates, ends_turn, idempotent)
    } else {
        (Retention::Durable, Vec::new(), false, false)
    };
    let mut def = ToolDef::new(
        ToolSpec {
            name: name.to_owned(),
            description: tool
                .description
                .map(|text| text.to_string())
                .unwrap_or_default(),
            input_schema: Value::Object((*tool.input_schema).clone()),
        },
        retention,
    );
    def.invalidates = invalidates.into_iter().map(|key| owner.key(key)).collect();
    def.ends_turn = ends_turn;
    def.idempotent = idempotent;
    Ok(def)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_with_meta(meta: serde_json::Value) -> Tool {
        let mut schema = rmcp::model::JsonObject::new();
        schema.insert("type".to_owned(), serde_json::json!("object"));
        let mut fields = rmcp::model::MetaObject::new();
        if let serde_json::Value::Object(meta) = meta {
            for (key, value) in meta {
                fields.insert(key, value);
            }
        }
        Tool::new("no_reply", "Stay silent.", Arc::new(schema)).with_meta(fields)
    }

    #[test]
    fn a_newt_server_can_declare_a_tool_that_ends_the_turn() {
        let ends = |meta, speaks_newt| {
            tool_def(tool_with_meta(meta), "no_reply", speaks_newt, &Owner::HOME)
                .map(|def| def.ends_turn)
        };
        assert_eq!(
            ends(serde_json::json!({ "newt/ends_turn": true }), true),
            Ok(true)
        );
        assert_eq!(ends(serde_json::json!({}), true), Ok(false));
        assert_eq!(
            ends(serde_json::json!({ "newt/ends_turn": true }), false),
            Ok(false),
            "a server that does not speak newt cannot end turns"
        );
        let invalid = tool_with_meta(serde_json::json!({ "newt/ends_turn": "yes" }));
        assert!(tool_def(invalid, "no_reply", true, &Owner::HOME).is_err());
    }
}
