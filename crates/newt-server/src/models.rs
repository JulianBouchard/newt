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
            };
            let mut client = ProviderClient::new(protocol, &route.base_url, key)?;
            if let Some(header) = &route.api_key_header {
                client = client.with_api_key_header(header);
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

impl Model for Routes {
    async fn stream(
        &self,
        request: &ModelRequest<'_>,
        cancel: &CancellationToken,
        on_event: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> Result<ModelResponse, ProviderError> {
        let route = self
            .aliases
            .get(request.model)
            .and_then(|chain| chain.first())
            .ok_or_else(|| ProviderError::UnknownModel(request.model.to_owned()))?;
        let _admitted = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(ProviderError::Cancelled),
            admitted = route.admission.admit(request.lane) => admitted?,
        };
        let routed = ModelRequest {
            model: &route.model,
            ..*request
        };
        route.client.stream(&routed, cancel, on_event).await
    }
}

#[cfg(test)]
mod tests {
    #![expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]

    use std::time::Duration;

    use super::*;

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
