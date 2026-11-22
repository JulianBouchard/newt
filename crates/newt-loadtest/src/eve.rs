use std::collections::HashMap;
use std::time::Duration;

use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc, oneshot};

use crate::driver::Signal;

const BASIC_USER: &str = "bench";
const NOT_READY_RETRIES: usize = 200;
const NOT_READY_BACKOFF: Duration = Duration::from_millis(100);

#[derive(Debug)]
pub struct EveTarget {
    pub base: String,
    pub token: String,
    sessions: Mutex<HashMap<String, String>>,
}

impl EveTarget {
    pub fn new(base: String, token: String) -> Self {
        Self {
            base,
            token,
            sessions: Mutex::default(),
        }
    }

    async fn create(&self, http: &reqwest::Client) -> Result<String, String> {
        let response = http
            .post(format!("{}/eve/v1/session", self.base))
            .basic_auth(BASIC_USER, Some(&self.token))
            .send()
            .await
            .map_err(|error| error.without_url().to_string())?;
        if !response.status().is_success() {
            return Err(format!("HTTP {}", response.status()));
        }
        let body: Value = response
            .json()
            .await
            .map_err(|error| error.without_url().to_string())?;
        body.get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| "no session id".to_owned())
    }

    pub(crate) async fn post(
        &self,
        http: &reqwest::Client,
        key: &str,
        text: &str,
    ) -> Result<(), String> {
        let session = self
            .sessions
            .lock()
            .await
            .get(key)
            .cloned()
            .ok_or_else(|| "no session".to_owned())?;
        for _ in 0..NOT_READY_RETRIES {
            let response = http
                .post(format!("{}/eve/v1/session/{session}", self.base))
                .basic_auth(BASIC_USER, Some(&self.token))
                .json(&json!({ "message": text }))
                .send()
                .await
                .map_err(|error| error.without_url().to_string())?;
            match response.status() {
                status if status.is_success() => return Ok(()),
                reqwest::StatusCode::CONFLICT => tokio::time::sleep(NOT_READY_BACKOFF).await,
                status => return Err(format!("HTTP {status}")),
            }
        }
        Err("session never became ready".to_owned())
    }

    pub(crate) async fn follow(
        &self,
        http: reqwest::Client,
        key: String,
        connected: oneshot::Sender<()>,
        signals: mpsc::Sender<Signal>,
    ) {
        let Ok(session) = self.create(&http).await else {
            return;
        };
        self.sessions.lock().await.insert(key, session.clone());
        let request = http
            .get(format!("{}/eve/v1/session/{session}/stream", self.base))
            .basic_auth(BASIC_USER, Some(&self.token))
            .send()
            .await;
        let Ok(response) = request else {
            return;
        };
        if !response.status().is_success() || connected.send(()).is_err() {
            return;
        }
        let mut body = response.bytes_stream();
        let mut pending = Vec::new();
        while let Some(Ok(chunk)) = body.next().await {
            pending.extend_from_slice(&chunk);
            while let Some(end) = pending.iter().position(|byte| *byte == b'\n') {
                let line: Vec<u8> = pending.drain(..=end).collect();
                let Ok(event) = serde_json::from_slice::<Value>(&line) else {
                    continue;
                };
                let signal = match event.get("type").and_then(Value::as_str) {
                    Some("turn.started") => Signal::Started,
                    Some("actions.requested" | "action.input.appended" | "message.appended") => {
                        Signal::Sign
                    }
                    Some("turn.completed") => Signal::Finished { ok: true },
                    Some("turn.failed") => Signal::Finished { ok: false },
                    _ => continue,
                };
                if signals.send(signal).await.is_err() {
                    return;
                }
            }
        }
    }
}
