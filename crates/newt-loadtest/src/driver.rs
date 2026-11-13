use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use newt_core::sse::SseParser;
use serde_json::{Value, json};
use tokio::sync::{Semaphore, mpsc, oneshot};

use crate::sample::Rng;

const SIGNAL_BUFFER: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Started,
    Finished { ok: bool },
}

#[derive(Debug, Clone)]
pub struct NewtTarget {
    pub base: String,
    pub agent: String,
    pub token: String,
}

impl NewtTarget {
    async fn post(
        &self,
        http: &reqwest::Client,
        key: &str,
        id: &str,
        text: &str,
    ) -> Result<(), String> {
        let response = http
            .post(format!(
                "{}/v1/agents/{}/sessions/{key}/events",
                self.base, self.agent
            ))
            .bearer_auth(&self.token)
            .json(&json!({ "id": id, "text": text }))
            .send()
            .await
            .map_err(|error| error.without_url().to_string())?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(format!("HTTP {}", response.status()))
        }
    }

    async fn follow(
        &self,
        http: reqwest::Client,
        key: String,
        connected: oneshot::Sender<()>,
        signals: mpsc::Sender<Signal>,
    ) {
        let request = http
            .get(format!(
                "{}/v1/agents/{}/sessions/{key}/live",
                self.base, self.agent
            ))
            .bearer_auth(&self.token)
            .send()
            .await;
        let Ok(response) = request else {
            return;
        };
        if connected.send(()).is_err() {
            return;
        }
        let mut body = response.bytes_stream();
        let mut parser = SseParser::default();
        while let Some(Ok(chunk)) = body.next().await {
            let Ok(events) = parser.feed(&chunk) else {
                return;
            };
            for event in events {
                let Ok(data) = serde_json::from_str::<Value>(&event.data) else {
                    continue;
                };
                let signal = match data.get("type").and_then(Value::as_str) {
                    Some("turn_started") => Signal::Started,
                    Some("turn_completed") => Signal::Finished { ok: true },
                    Some("turn_failed") => Signal::Finished { ok: false },
                    _ => continue,
                };
                if signals.send(signal).await.is_err() {
                    return;
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct LoadPlan {
    pub sessions: usize,
    pub turns: usize,
    pub concurrency: usize,
    /// The pause before a session's next message: uniform between `think`
    /// and `think_max`.
    pub think: Duration,
    pub think_max: Duration,
    /// Sessions start at uniform offsets in this window rather than all at
    /// once.
    pub ramp: Duration,
    pub timeout: Duration,
}

#[derive(Debug, Clone, Default)]
pub struct TurnSample {
    pub accept_ms: f64,
    pub start_ms: Option<f64>,
    pub end_to_end_ms: Option<f64>,
    pub ok: bool,
    pub error: Option<String>,
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

async fn run_session(
    target: Arc<NewtTarget>,
    http: reqwest::Client,
    key: String,
    plan: LoadPlan,
    seed: usize,
) -> Vec<TurnSample> {
    let (sender, mut signals) = mpsc::channel(SIGNAL_BUFFER);
    let (connected, follower_ready) = oneshot::channel();
    let follower = tokio::spawn({
        let target = Arc::clone(&target);
        let http = http.clone();
        let key = key.clone();
        async move { target.follow(http, key, connected, sender).await }
    });
    let mut samples = Vec::with_capacity(plan.turns);
    if !matches!(
        tokio::time::timeout(plan.timeout, follower_ready).await,
        Ok(Ok(()))
    ) {
        follower.abort();
        samples.push(TurnSample {
            error: Some("could not open the live feed".into()),
            ..TurnSample::default()
        });
        return samples;
    }
    let rng = Rng::new(u64::try_from(seed).unwrap_or(0) + 1);
    tokio::time::sleep(pause(&rng, Duration::ZERO, plan.ramp)).await;
    for turn in 0..plan.turns {
        let posted = Instant::now();
        let text = format!("load message {seed}-{turn}: any news on the role?");
        let accepted = target
            .post(&http, &key, &format!("{key}-{turn}"), &text)
            .await;
        let mut sample = TurnSample {
            accept_ms: ms(posted.elapsed()),
            ..TurnSample::default()
        };
        if let Err(error) = accepted {
            sample.error = Some(error);
            samples.push(sample);
            continue;
        }
        let deadline = tokio::time::Instant::now() + plan.timeout;
        loop {
            match tokio::time::timeout_at(deadline, signals.recv()).await {
                Ok(Some(Signal::Started)) if sample.start_ms.is_none() => {
                    sample.start_ms = Some(ms(posted.elapsed()));
                }
                Ok(Some(Signal::Started)) => {}
                Ok(Some(Signal::Finished { ok })) => {
                    sample.end_to_end_ms = Some(ms(posted.elapsed()));
                    sample.ok = ok;
                    break;
                }
                Ok(None) => {
                    sample.error = Some("the live feed closed".into());
                    break;
                }
                Err(_) => {
                    sample.error = Some("timed out".into());
                    break;
                }
            }
        }
        samples.push(sample);
        tokio::time::sleep(pause(&rng, plan.think, plan.think_max)).await;
    }
    follower.abort();
    samples
}

/// A pause drawn uniformly between `low` and `high`.
fn pause(rng: &Rng, low: Duration, high: Duration) -> Duration {
    high.checked_sub(low)
        .map_or(low, |spread| low + spread.mul_f64(rng.unit()))
}

pub async fn run(target: NewtTarget, plan: LoadPlan, run_id: &str) -> (Vec<TurnSample>, Duration) {
    let target = Arc::new(target);
    let http = reqwest::Client::builder()
        .pool_max_idle_per_host(plan.concurrency.max(1) * 2)
        .build()
        .unwrap_or_default();
    let gate = Arc::new(Semaphore::new(plan.concurrency.max(1)));
    let started = Instant::now();
    let mut tasks = tokio::task::JoinSet::new();
    for index in 0..plan.sessions {
        let gate = Arc::clone(&gate);
        let target = Arc::clone(&target);
        let http = http.clone();
        let key = format!("{run_id}-{index}");
        tasks.spawn(async move {
            let Ok(_permit) = gate.acquire_owned().await else {
                return Vec::new();
            };
            run_session(target, http, key, plan, index).await
        });
    }
    let mut samples = Vec::new();
    while let Some(result) = tasks.join_next().await {
        samples.extend(result.unwrap_or_default());
    }
    (samples, started.elapsed())
}

pub fn percentile(values: &mut [f64], fraction: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        reason = "an index into a short sample list"
    )]
    let index = ((values.len() - 1) as f64 * fraction).round() as usize;
    values.get(index).copied()
}
