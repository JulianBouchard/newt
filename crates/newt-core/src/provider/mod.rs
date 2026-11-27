//! What the turn loop needs from a model: the [`Model`] trait and the
//! types it speaks. The provider clients that implement it live in
//! newt-providers.

mod arguments;
#[cfg(test)]
mod fuzz;
mod types;

pub use arguments::tool_arguments;
pub use types::{CallTiming, Model, ModelResponse, ProviderError, StopReason, StreamEvent, Usage};
