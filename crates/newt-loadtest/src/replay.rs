//! Replays recorded turns through the fake model and the mock app, so a
//! runtime under test does exactly the work a real turn did: the same model calls with
//! their recorded time to first token and stream time, the same tool calls, and each
//! host tool taking its recorded latency.
//!
//! A turn file has one JSON object per line: `model_calls` and `tool_calls` with start
//! and end times in milliseconds from the turn's event. A model call without
//! `first_token` takes `estimated_first_token_ms` after its start, or 2.5 seconds. Each tool call belongs to the model call that started last before it
//! started. A model call that asked for no tools and is not the last one carries no work
//! a runtime could repeat, so it is folded into the next one: its time stays, its turn boundary does not.

use std::collections::HashMap;

use anyhow::Context;
use serde::Deserialize;
use serde_json::Value;

use crate::args::arguments_for;
use crate::plan::Call;
use crate::profile::{Kind, Profile};
use crate::sample::Rng;

#[derive(Debug, Deserialize)]
struct RecordedCall {
    start: f64,
    first_token: Option<f64>,
    end: f64,
    #[serde(default)]
    estimated_first_token_ms: Option<f64>,
}

const DEFAULT_FIRST_TOKEN_MS: f64 = 2_500.0;

#[derive(Debug, Deserialize)]
struct RecordedTool {
    name: String,
    start: f64,
    end: f64,
}

#[derive(Debug, Deserialize)]
struct RecordedTurn {
    #[serde(default)]
    t_request: Option<f64>,
    #[serde(default)]
    t_turn_start: Option<f64>,
    #[serde(default)]
    t_turn_end: Option<f64>,
    #[serde(default)]
    model_calls: Vec<RecordedCall>,
    #[serde(default)]
    tool_calls: Vec<RecordedTool>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReplayCall {
    pub tool: String,
    pub latency_ms: f64,
    pub ready_ms: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub first_token_ms: f64,
    pub stream_ms: f64,
    pub calls: Vec<ReplayCall>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReplayTurn {
    pub steps: Vec<Step>,
    pub prod_start_ms: Option<f64>,
    pub prod_first_sign_ms: Option<f64>,
    pub prod_end_ms: Option<f64>,
}

/// What a recorded turn took in production, in milliseconds after its request.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProdTimes {
    pub start: Option<f64>,
    /// The earliest tool-call start or first model token. A tool span starts when the
    /// tool began executing, which is at or just after its arguments finished
    /// streaming, so this is a slightly late bound of when the first tool call showed.
    pub first_sign: Option<f64>,
    pub end: Option<f64>,
}

#[derive(Debug)]
pub struct Replay {
    turns: Vec<ReplayTurn>,
}

pub(crate) struct Planned {
    pub(crate) calls: Vec<Call>,
    pub(crate) ready_ms: Vec<f64>,
    pub(crate) reply: bool,
    pub(crate) first_token_ms: f64,
    pub(crate) stream_ms: f64,
}

fn steps_of(recorded: RecordedTurn) -> Option<ReplayTurn> {
    let since_request = |at: Option<f64>| at.map(|at| at - recorded.t_request.unwrap_or(0.0));
    let (prod_start_ms, prod_end_ms) = (
        since_request(recorded.t_turn_start),
        since_request(recorded.t_turn_end),
    );
    let first_sign = recorded
        .tool_calls
        .iter()
        .map(|tool| tool.start)
        .chain(
            recorded
                .model_calls
                .iter()
                .filter_map(|call| call.first_token),
        )
        .min_by(f64::total_cmp);
    let prod_first_sign_ms = since_request(first_sign);
    let mut calls = recorded.model_calls;
    calls.sort_by(|a, b| a.start.total_cmp(&b.start));
    if calls.is_empty() {
        return None;
    }
    let mut steps: Vec<Step> = calls
        .iter()
        .map(|call| {
            let first_token = call
                .first_token
                .filter(|first| *first >= call.start && *first <= call.end)
                .unwrap_or_else(|| {
                    let estimate = call
                        .estimated_first_token_ms
                        .unwrap_or(DEFAULT_FIRST_TOKEN_MS);
                    (call.start + estimate).min(call.end)
                });
            Step {
                first_token_ms: first_token - call.start,
                stream_ms: call.end - first_token,
                calls: Vec::new(),
            }
        })
        .collect();
    for tool in recorded.tool_calls {
        let owner = calls.iter().rposition(|call| call.start <= tool.start);
        if let Some(index) = owner {
            let owner = &calls[index];
            steps[index].calls.push(ReplayCall {
                tool: tool.name,
                latency_ms: (tool.end - tool.start).max(0.0),
                ready_ms: (tool.start - owner.start).clamp(0.0, owner.end - owner.start),
            });
        }
    }
    let last = steps.len() - 1;
    let mut kept: Vec<Step> = Vec::with_capacity(steps.len());
    let mut carried_ms = 0.0;
    for (index, mut step) in steps.into_iter().enumerate() {
        if index != last && step.calls.is_empty() {
            carried_ms += step.first_token_ms + step.stream_ms;
            continue;
        }
        step.first_token_ms += carried_ms;
        for call in &mut step.calls {
            call.ready_ms += carried_ms;
        }
        carried_ms = 0.0;
        kept.push(step);
    }
    let steps = kept;
    Some(ReplayTurn {
        steps,
        prod_start_ms,
        prod_first_sign_ms,
        prod_end_ms,
    })
}

impl Replay {
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
        let turns: Vec<ReplayTurn> = raw
            .lines()
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| serde_json::from_str::<RecordedTurn>(line).ok())
            .filter_map(steps_of)
            .collect();
        anyhow::ensure!(!turns.is_empty(), "{path} has no replayable turns");
        Ok(Self { turns })
    }

    pub fn prod_times(&self, key: u64) -> ProdTimes {
        let turn = self.turn(key);
        ProdTimes {
            start: turn.prod_start_ms,
            first_sign: turn.prod_first_sign_ms,
            end: turn.prod_end_ms,
        }
    }

    fn turn(&self, key: u64) -> &ReplayTurn {
        let index =
            usize::try_from(key % u64::try_from(self.turns.len()).unwrap_or(1)).unwrap_or(0);
        &self.turns[index]
    }

    /// The recorded step for the `index`-th model call of a turn, or `None` once every
    /// recorded step has run. The fake model counts the calls it served per turn, so
    /// replay never depends on how a runtime echoes tool results back.
    fn step(&self, key: u64, index: usize) -> Option<&Step> {
        self.turn(key).steps.get(index)
    }

    pub(crate) fn plan(
        &self,
        key: u64,
        index: usize,
        offered: &HashMap<String, Value>,
        profile: &Profile,
        rng: &Rng,
    ) -> Option<Planned> {
        let step = self.step(key, index)?;
        let (calls, ready_ms): (Vec<Call>, Vec<f64>) = step
            .calls
            .iter()
            .filter_map(|call| {
                let planned = bench_call(call, offered, profile, rng);
                if planned.is_none() && kind_of(&call.tool, profile) != Kind::End {
                    tracing::warn!(tool = %call.tool, "replayed tool is not offered by the runtime; skipped");
                }
                planned.map(|planned| (planned, call.ready_ms))
            })
            .unzip();
        Some(Planned {
            reply: calls.is_empty(),
            calls,
            ready_ms,
            first_token_ms: step.first_token_ms,
            stream_ms: step.stream_ms,
        })
    }
}

fn kind_of(name: &str, profile: &Profile) -> Kind {
    if profile.skills.iter().any(|skill| skill == name) {
        return Kind::Skill;
    }
    profile.find(name).map_or(Kind::Read, |tool| tool.kind)
}

fn bench_call(
    call: &ReplayCall,
    offered: &HashMap<String, Value>,
    profile: &Profile,
    rng: &Rng,
) -> Option<Call> {
    let kind = kind_of(&call.tool, profile);
    let name = match kind {
        Kind::Skill => profile
            .skill_tools
            .iter()
            .find(|name| offered.contains_key(*name))?
            .clone(),
        Kind::Workspace | Kind::End if offered.contains_key(&call.tool) => call.tool.clone(),
        Kind::Workspace => "read_file".to_owned(),
        Kind::End => profile
            .tools
            .iter()
            .filter(|tool| tool.kind == Kind::End)
            .map(|tool| tool.name.clone())
            .find(|name| offered.contains_key(name))?,
        Kind::Read | Kind::Write | Kind::Send => call.tool.clone(),
    };
    let schema = offered.get(&name)?;
    if kind == Kind::End {
        let arguments = if schema["properties"].get("nothing_new").is_some() {
            serde_json::json!({ "nothing_new": "replayed turn" })
        } else {
            arguments_for(&name, kind, schema, rng, profile)
        };
        return Some(Call { name, arguments });
    }
    let mut arguments = arguments_for(&name, kind, schema, rng, profile);
    crate::intervals::tag_latency(&mut arguments, call.latency_ms);
    Some(Call { name, arguments })
}

#[cfg(test)]
mod tests {
    #![expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]

    use super::*;

    fn turn(line: &str) -> ReplayTurn {
        steps_of(serde_json::from_str(line).unwrap()).unwrap()
    }

    #[test]
    fn tools_belong_to_the_call_that_ended_before_them_and_empty_middle_calls_drop() {
        let recorded = turn(
            r#"{"model_calls":[
                {"start":100,"first_token":900,"end":1500},
                {"start":1600,"first_token":2000,"end":2100},
                {"start":2200,"first_token":2600,"end":3000},
                {"start":3800,"first_token":4500,"end":5200}],
               "tool_calls":[
                {"name":"read_file","start":1510,"end":1520},
                {"name":"lookupOrder","start":1505,"end":1800},
                {"name":"sendMessage","start":3010,"end":3700}]}"#,
        );
        assert_eq!(recorded.steps.len(), 3);
        assert_eq!(recorded.steps[0].calls.len(), 2);
        assert!((recorded.steps[0].first_token_ms - 800.0).abs() < 1e-9);
        assert!((recorded.steps[0].stream_ms - 600.0).abs() < 1e-9);
        assert_eq!(recorded.steps[1].calls[0].tool, "sendMessage");
        assert!((recorded.steps[1].calls[0].latency_ms - 690.0).abs() < 1e-9);
        assert_eq!(recorded.steps[2].calls.len(), 0);
    }

    #[test]
    fn production_first_sign_is_the_earliest_tool_start_or_first_token_after_the_request() {
        let with_token = turn(
            r#"{"t_request":1000,"model_calls":[{"start":1100,"first_token":1900,"end":2500}],
               "tool_calls":[{"name":"read_file","start":2300,"end":2400}]}"#,
        );
        assert_eq!(with_token.prod_first_sign_ms, Some(900.0));
        let tool_first = turn(
            r#"{"t_request":1000,"model_calls":[{"start":1100,"end":2500}],
               "tool_calls":[{"name":"b","start":2300,"end":2400},{"name":"a","start":1700,"end":1800}]}"#,
        );
        assert_eq!(tool_first.prod_first_sign_ms, Some(700.0));
    }

    #[test]
    fn steps_come_in_recorded_order_and_run_out() {
        let replay = Replay {
            turns: vec![turn(
                r#"{"model_calls":[{"start":0,"first_token":10,"end":20},{"start":30,"first_token":40,"end":50}],
                   "tool_calls":[{"name":"a","start":21,"end":22},{"name":"b","start":21,"end":25}]}"#,
            )],
        };
        assert_eq!(replay.step(0, 0).unwrap().calls.len(), 2);
        assert_eq!(replay.step(0, 1).unwrap().calls.len(), 0);
        assert!(replay.step(0, 2).is_none());
    }
}
