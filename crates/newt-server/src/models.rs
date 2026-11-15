use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context;
use newt_core::prompt::{Lane, ModelRequest};
use newt_core::provider::{
    Model, ModelResponse, Protocol as CoreProtocol, ProviderClient, ProviderError, StreamEvent,
};
use tokio::sync::{Semaphore, SemaphorePermit};
use tokio_util::sync::CancellationToken;

use crate::config::{Protocol, ServerConfig};

#[derive(Debug)]
pub struct Admission {
    all: Semaphore,
    background: Semaphore,
}

#[derive(Debug)]
pub struct Admitted<'a> {
    _background: Option<SemaphorePermit<'a>>,
    _slot: SemaphorePermit<'a>,
}

impl Admission {
    pub fn new(max_concurrent: u32) -> Self {
        let slots = usize::try_from(max_concurrent.max(1)).unwrap_or(usize::MAX);
        Self {
            all: Semaphore::new(slots),
            background: Semaphore::new(slots.saturating_sub(1).max(1)),
        }
    }

    pub async fn admit(&self, lane: Lane) -> Result<Admitted<'_>, ProviderError> {
        let background = match lane {
            Lane::Interactive => None,
            Lane::Background => Some(
                self.background
                    .acquire()
                    .await
                    .map_err(|_| ProviderError::Protocol("admission closed".to_owned()))?,
            ),
        };
        let slot = self
            .all
            .acquire()
            .await
            .map_err(|_| ProviderError::Protocol("admission closed".to_owned()))?;
        Ok(Admitted {
            _background: background,
            _slot: slot,
        })
    }

    pub fn available(&self) -> usize {
        self.all.available_permits()
    }
}

#[derive(Debug)]
struct Route {
    client: ProviderClient,
    model: String,
    admission: Admission,
}

#[derive(Debug, Default)]
pub struct Routes {
    aliases: HashMap<String, Vec<Arc<Route>>>,
}

impl Routes {
    pub fn from_config(config: &ServerConfig) -> anyhow::Result<Self> {
        let mut routes = HashMap::new();
        for (name, route) in &config.routes {
            let key = std::env::var(&route.key_env)
                .with_context(|| format!("environment variable {} is not set", route.key_env))?;
            let protocol = match route.protocol {
                Protocol::AnthropicMessages => CoreProtocol::AnthropicMessages,
                Protocol::OpenaiResponses => CoreProtocol::OpenaiResponses,
                Protocol::OpenaiChat => CoreProtocol::OpenaiChat,
            };
            let mut client = ProviderClient::new(protocol, &route.base_url, key)?;
            if let Some(header) = &route.api_key_header {
                client = client.with_api_key_header(header);
            }
            if !route.replay_reasoning {
                client = client.without_reasoning_replay();
            }
            routes.insert(
                name.clone(),
                Arc::new(Route {
                    client,
                    model: route.model.clone(),
                    admission: Admission::new(route.max_concurrent),
                }),
            );
        }
        let aliases = config
            .models
            .iter()
            .map(|(alias, chain)| {
                let chain = chain
                    .chain
                    .iter()
                    .filter_map(|name| routes.get(name).cloned())
                    .collect();
                (alias.clone(), chain)
            })
            .collect();
        Ok(Self { aliases })
    }
}

const ATTEMPTS_PER_ROUTE: u32 = 2;
const RETRY_PAUSE: std::time::Duration = std::time::Duration::from_millis(200);

impl Route {
    async fn stream(
        &self,
        request: &ModelRequest<'_>,
        cancel: &CancellationToken,
        on_event: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> Result<ModelResponse, ProviderError> {
        let _admitted = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(ProviderError::Cancelled),
            admitted = self.admission.admit(request.lane) => admitted?,
        };
        let routed = ModelRequest {
            model: &self.model,
            ..*request
        };
        self.client.stream(&routed, cancel, on_event).await
    }
}

impl Model for Routes {
    async fn stream(
        &self,
        request: &ModelRequest<'_>,
        cancel: &CancellationToken,
        on_event: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> Result<ModelResponse, ProviderError> {
        let chain = self
            .aliases
            .get(request.model)
            .filter(|chain| !chain.is_empty())
            .ok_or_else(|| ProviderError::UnknownModel(request.model.to_owned()))?;
        let mut last_error = None;
        for route in chain {
            for attempt in 0..ATTEMPTS_PER_ROUTE {
                if attempt > 0 {
                    tokio::select! {
                        biased;
                        () = cancel.cancelled() => return Err(ProviderError::Cancelled),
                        () = tokio::time::sleep(RETRY_PAUSE * attempt) => {}
                    }
                }
                let mut streamed = false;
                let result = route
                    .stream(request, cancel, &mut |event| {
                        streamed = true;
                        on_event(event);
                    })
                    .await;
                match result {
                    Err(error) if !streamed && error.retryable() => {
                        tracing::warn!(model = %route.model, attempt, %error, "model call failed before streaming, trying again");
                        last_error = Some(error);
                    }
                    other => return other,
                }
            }
        }
        Err(last_error.unwrap_or_else(|| ProviderError::UnknownModel(request.model.to_owned())))
    }
}

#[cfg(test)]
mod tests {
    #![expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use axum::http::StatusCode;
    use newt_core::prompt::Prompt;

    use super::*;

    const OPENAI_STREAM: &str = include_str!("../../newt-core/tests/fixtures/openai_tool_use.sse");

    async fn provider(status: StatusCode, body: &'static str) -> (String, Arc<AtomicUsize>) {
        let hits = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&hits);
        let app = axum::Router::new().route(
            "/responses",
            axum::routing::post(move || {
                let counted = Arc::clone(&counted);
                async move {
                    counted.fetch_add(1, Ordering::SeqCst);
                    (status, [("content-type", "text/event-stream")], body)
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (base, hits)
    }

    fn route(base: &str) -> Arc<Route> {
        Arc::new(Route {
            client: ProviderClient::new(CoreProtocol::OpenaiResponses, base, "key").unwrap(),
            model: "upstream".to_owned(),
            admission: Admission::new(4),
        })
    }

    async fn call(routes: &Routes) -> Result<ModelResponse, ProviderError> {
        let request = ModelRequest {
            model: "default",
            prompt: Prompt {
                system: "Be brief.",
                tools: &[],
                history: &[],
                scratch: &[],
            },
            effort: None,
            max_output_tokens: 64,
            cache_key: "agent/0",
            lane: Lane::Interactive,
        };
        routes
            .stream(&request, &CancellationToken::new(), &mut |_| {})
            .await
    }

    #[tokio::test]
    async fn an_overloaded_route_is_retried_once_then_the_chain_falls_back() {
        let (overloaded, overloaded_hits) = provider(StatusCode::SERVICE_UNAVAILABLE, "busy").await;
        let (healthy, healthy_hits) = provider(StatusCode::OK, OPENAI_STREAM).await;
        let routes = Routes {
            aliases: HashMap::from([(
                "default".to_owned(),
                vec![route(&overloaded), route(&healthy)],
            )]),
        };

        call(&routes).await.unwrap();

        assert_eq!(overloaded_hits.load(Ordering::SeqCst), 2);
        assert_eq!(healthy_hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_request_the_provider_rejects_is_not_retried() {
        let (rejecting, rejecting_hits) = provider(StatusCode::BAD_REQUEST, "bad request").await;
        let (healthy, healthy_hits) = provider(StatusCode::OK, OPENAI_STREAM).await;
        let routes = Routes {
            aliases: HashMap::from([(
                "default".to_owned(),
                vec![route(&rejecting), route(&healthy)],
            )]),
        };

        let error = call(&routes).await.unwrap_err();

        assert!(
            matches!(error, ProviderError::Status { status: 400, .. }),
            "{error:?}"
        );
        assert_eq!(rejecting_hits.load(Ordering::SeqCst), 1);
        assert_eq!(healthy_hits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn background_turns_never_take_the_last_slot() {
        let admission = Admission::new(2);
        let first = admission.admit(Lane::Background).await.unwrap();
        let second =
            tokio::time::timeout(Duration::from_millis(50), admission.admit(Lane::Background))
                .await;
        assert!(second.is_err(), "a second background call must wait");
        let interactive = tokio::time::timeout(
            Duration::from_millis(50),
            admission.admit(Lane::Interactive),
        )
        .await;
        assert!(
            interactive.is_ok(),
            "an interactive call gets the reserved slot"
        );
        assert_eq!(admission.available(), 0);
        drop(first);
    }
}
