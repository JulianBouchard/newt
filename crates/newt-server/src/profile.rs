use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;

use newt_core::event::{Event, EventKind};
use time::OffsetDateTime;

use crate::events::{Record, ServerEvent, ServerEventKind, WakeSource};

#[derive(Debug, Default, Clone)]
struct Waiting {
    accepted: Option<OffsetDateTime>,
    woken: Option<OffsetDateTime>,
    claimed: Option<OffsetDateTime>,
    prepared: Option<OffsetDateTime>,
}

#[derive(Debug, Default)]
pub struct Profile {
    waiting: HashMap<String, Waiting>,
    stages: BTreeMap<&'static str, Vec<f64>>,
    wakes: BTreeMap<&'static str, usize>,
    claims_refused: usize,
    running: Vec<f64>,
    pool_idle: Vec<f64>,
    pool_size: Vec<f64>,
    sweeps: Vec<f64>,
    previous_call_end: HashMap<String, OffsetDateTime>,
}

fn ms(from: OffsetDateTime, to: OffsetDateTime) -> f64 {
    (to - from).as_seconds_f64() * 1000.0
}

#[expect(
    clippy::cast_precision_loss,
    reason = "durations and counts far below 2^52"
)]
fn number(value: u64) -> f64 {
    value as f64
}

impl Profile {
    fn stage(&mut self, name: &'static str, value: f64) {
        self.stages.entry(name).or_default().push(value);
    }

    fn observe_pool_wait(&mut self, kind: &ServerEventKind) {
        let (stage, waited) = match kind {
            ServerEventKind::EventAccepted { pool_wait_ms, .. } => {
                ("read receipt: waiting for a connection", pool_wait_ms)
            }
            ServerEventKind::SessionClaimed { pool_wait_ms, .. } => {
                ("claim: waiting for a connection", pool_wait_ms)
            }
            ServerEventKind::TurnPrepared { pool_wait_ms, .. } => {
                ("history load: waiting for a connection", pool_wait_ms)
            }
            _ => return,
        };
        self.stage(stage, number(*waited));
    }

    pub fn observe(&mut self, record: &Record) {
        match record {
            Record::Turn(event) => self.observe_turn(event),
            Record::Server(event) => self.observe_server(event),
        }
    }

    fn observe_server(&mut self, event: &ServerEvent) {
        self.observe_pool_wait(&event.kind);
        let at = event.at;
        match &event.kind {
            ServerEventKind::EventAccepted {
                session, accept_ms, ..
            } => {
                self.stage("read receipt (server)", number(*accept_ms));
                let waiting = self.waiting.entry(session.clone()).or_default();
                if waiting.accepted.is_none() {
                    *waiting = Waiting {
                        accepted: Some(at),
                        ..Waiting::default()
                    };
                }
            }
            ServerEventKind::SessionWoken { session, source } => {
                let name = match source {
                    WakeSource::Notify => "notify",
                    WakeSource::Sweep => "sweep",
                    WakeSource::Direct => "direct",
                };
                *self.wakes.entry(name).or_default() += 1;
                if let Some(waiting) = self.waiting.get_mut(session)
                    && waiting.accepted.is_some()
                    && waiting.woken.is_none()
                {
                    waiting.woken = Some(at);
                }
            }
            ServerEventKind::SessionClaimed {
                session,
                claimed,
                claim_ms,
                ..
            } => {
                self.stage("claim query", number(*claim_ms));
                if !claimed {
                    self.claims_refused += 1;
                } else if let Some(waiting) = self.waiting.get_mut(session)
                    && waiting.accepted.is_some()
                    && waiting.claimed.is_none()
                {
                    waiting.claimed = Some(at);
                }
            }
            ServerEventKind::TurnPrepared {
                session,
                prepare_ms,
                history_ms,
                ..
            } => {
                self.stage("prepare (home server)", number(*prepare_ms));
                self.stage("history load", number(*history_ms));
                if let Some(waiting) = self.waiting.get_mut(session)
                    && waiting.accepted.is_some()
                {
                    waiting.prepared = Some(at);
                }
            }
            ServerEventKind::TurnCommitted { commit_ms, .. } => {
                self.stage("commit", number(*commit_ms));
            }
            ServerEventKind::RunnerSample {
                running,
                pool_size,
                pool_idle,
                sweep_ms,
                ..
            } => {
                self.running
                    .push(number(u64::try_from(*running).unwrap_or(u64::MAX)));
                self.pool_size.push(f64::from(*pool_size));
                self.pool_idle
                    .push(number(u64::try_from(*pool_idle).unwrap_or(u64::MAX)));
                self.sweeps.push(number(*sweep_ms));
            }
            ServerEventKind::ServerReady { .. } | ServerEventKind::CompactionCompleted { .. } => {}
        }
    }

    fn observe_turn(&mut self, event: &Event) {
        let at = event.at;
        match &event.kind {
            EventKind::TurnStarted { session, .. } => self.turn_started(session, at),
            EventKind::ModelCallStarted {
                session,
                turn,
                call,
                ..
            } if *call > 0 => {
                if let Some(previous) = self.previous_call_end.get(&format!("{session}/{turn}")) {
                    let gap = ms(*previous, at);
                    self.stage("gap before a model call", gap);
                }
            }
            EventKind::ModelCallFinished { session, turn, .. } => {
                self.previous_call_end
                    .insert(format!("{session}/{turn}"), at);
            }
            EventKind::TurnCompleted { session, turn, .. }
            | EventKind::TurnFailed { session, turn, .. } => {
                self.previous_call_end.remove(&format!("{session}/{turn}"));
            }
            _ => {}
        }
    }

    fn turn_started(&mut self, session: &str, at: OffsetDateTime) {
        let Some(waiting) = self.waiting.remove(session) else {
            return;
        };
        let Some(accepted) = waiting.accepted else {
            return;
        };
        self.stage("event to turn start", ms(accepted, at));
        let woken = waiting.woken;
        let claimed = waiting.claimed;
        if let Some(woken) = woken {
            self.stage("  accept -> woken", ms(accepted, woken));
        }
        if let (Some(woken), Some(claimed)) = (woken, claimed) {
            self.stage("  woken -> claimed", ms(woken, claimed));
        }
        if let (Some(claimed), Some(prepared)) = (claimed, waiting.prepared) {
            self.stage("  claimed -> prepared", ms(claimed, prepared));
        }
        if let Some(prepared) = waiting.prepared {
            self.stage("  prepared -> started", ms(prepared, at));
        }
        if woken.is_none() {
            self.stage("  picked up without a wake", ms(accepted, at));
        }
    }

    pub fn render(&self) -> String {
        let mut out = String::from(
            "\npickup and overhead (ms)            p50       p90       p99       max       n\n",
        );
        for (name, values) in &self.stages {
            out.push_str(&row(name, values));
        }
        let wakes: Vec<String> = self
            .wakes
            .iter()
            .map(|(name, count)| format!("{name} {count}"))
            .collect();
        let _ = writeln!(
            out,
            "\nwakes: {}; claims refused (lease busy): {}",
            wakes.join(", "),
            self.claims_refused
        );
        out.push_str(&row("runner: running sessions", &self.running));
        out.push_str(&row("runner: db pool size", &self.pool_size));
        out.push_str(&row("runner: db pool idle", &self.pool_idle));
        out.push_str(&row("runner: sweep", &self.sweeps));
        out
    }
}

fn row(name: &str, values: &[f64]) -> String {
    if values.is_empty() {
        return String::new();
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let at = |fraction: f64| {
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss,
            reason = "an index into a sample list"
        )]
        let index = ((sorted.len() - 1) as f64 * fraction).round() as usize;
        sorted.get(index).copied().unwrap_or_default()
    };
    format!(
        "{name:<34} {:>9.0} {:>9.0} {:>9.0} {:>9.0} {:>7}\n",
        at(0.5),
        at(0.9),
        at(0.99),
        at(1.0),
        sorted.len()
    )
}
