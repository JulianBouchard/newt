use serde_json::Value;

use super::ApiError;
use crate::store::InboundEvent;

const NUL_MESSAGE: &str = "text must not contain a NUL character";

pub(super) fn no_nul(text: &str) -> Result<(), ApiError> {
    if text.contains('\0') {
        return Err(ApiError::Invalid(NUL_MESSAGE));
    }
    Ok(())
}

fn json_has_nul(value: &Value) -> bool {
    match value {
        Value::String(text) => text.contains('\0'),
        Value::Array(items) => items.iter().any(json_has_nul),
        Value::Object(fields) => fields
            .iter()
            .any(|(key, value)| key.contains('\0') || json_has_nul(value)),
        _ => false,
    }
}

pub(super) fn event_without_nul(event: &InboundEvent) -> Result<(), ApiError> {
    no_nul(&event.id)?;
    no_nul(&event.text)?;
    if event.meta.as_ref().is_some_and(json_has_nul) {
        return Err(ApiError::Invalid(NUL_MESSAGE));
    }
    Ok(())
}
