//! Model provider clients for newt. Each speaks one wire protocol
//! (Anthropic Messages, `OpenAI` Responses, Chat Completions), streams the
//! answer, and implements newt-core's [`Model`](newt_core::provider::Model).
//! The turn loop needs only that trait; this crate is one way to provide it.

mod anthropic;
mod capture;
mod chat;
mod client;
mod openai;
mod protocol;
pub mod sse;
mod stream;

pub use client::ProviderClient;
pub use protocol::Protocol;
use stream::{Decoded, Decoder, Emitter, decode};

use newt_core::provider::ProviderError;

/// A reqwest failure as a provider error: a timeout, or any other.
pub(crate) fn http(error: reqwest::Error) -> ProviderError {
    if error.is_timeout() {
        ProviderError::Timeout(Box::new(error))
    } else {
        ProviderError::Http(Box::new(error))
    }
}

impl From<sse::SseError> for ProviderError {
    fn from(error: sse::SseError) -> Self {
        Self::Sse(error.to_string())
    }
}
