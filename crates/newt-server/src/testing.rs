//! A model whose calls a test can hold and release, for tests that need
//! turns to stay running.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use newt_core::message::{Message, Part, Role, ToolCall};
use newt_core::prompt::ModelRequest;
use newt_core::provider::{
    CallTiming, Model, ModelResponse, ProviderError, StopReason, StreamEvent, Usage,
};
use serde_json::json;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

/// Each model call takes one permit from `gate` before it answers with a
/// reply that ends the turn, and gives up with [`ProviderError::Cancelled`]
/// when the turn is cancelled first.
#[derive(Debug, Clone)]
pub struct GatedModel {
    pub gate: Arc<Semaphore>,
    pub started: Arc<AtomicUsize>,
}

impl GatedModel {
    #[must_use]
    pub fn closed() -> Self {
        Self {
            gate: Arc::new(Semaphore::new(0)),
            started: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn release(&self, calls: usize) {
        self.gate.add_permits(calls);
    }

    pub fn calls_started(&self) -> usize {
        self.started.load(Ordering::SeqCst)
    }
}

impl Model for GatedModel {
    async fn stream(
        &self,
        _request: &ModelRequest<'_>,
        cancel: &CancellationToken,
        _on_event: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> Result<ModelResponse, ProviderError> {
        self.started.fetch_add(1, Ordering::SeqCst);
        tokio::select! {
            () = cancel.cancelled() => return Err(ProviderError::Cancelled),
            permit = self.gate.acquire() => {
                permit.map_err(|error| ProviderError::Protocol(error.to_string()))?.forget();
            }
        }
        Ok(ModelResponse {
            message: Message {
                role: Role::Assistant,
                parts: vec![
                    Part::text("done"),
                    Part::ToolCall(ToolCall {
                        id: "end".into(),
                        name: "end_turn".into(),
                        arguments: json!({ "nothing_new": "just a chat" }),
                    }),
                ],
            },
            usage: Usage::default(),
            stop: StopReason::ToolUse,
            timing: CallTiming::default(),
            response_id: None,
            request_ids: BTreeMap::new(),
        })
    }
}
