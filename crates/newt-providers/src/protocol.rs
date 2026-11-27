use serde::Deserialize;

/// The wire protocol of a model provider, as `server.toml` names it
/// (`anthropic-messages`, `openai-responses`, `openai-chat`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Protocol {
    AnthropicMessages,
    OpenaiResponses,
    /// Chat Completions, which vLLM, Ollama and most other hosts speak.
    OpenaiChat,
}

impl Protocol {
    /// The header that carries the API key unless a route names another.
    pub const fn key_header(self) -> &'static str {
        match self {
            Self::AnthropicMessages => "x-api-key",
            Self::OpenaiResponses | Self::OpenaiChat => "authorization",
        }
    }
}
