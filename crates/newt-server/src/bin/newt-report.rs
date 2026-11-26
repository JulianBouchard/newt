use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};

use anyhow::Context;
use newt_core::event::{Event, EventKind};
use newt_core::provider::{CallTiming, Usage};
use newt_server::cache_card::{CacheCard, Prices};
use newt_server::events::{Record, ServerEvent, ServerEventKind};
use time::OffsetDateTime;

/// Providers keep an idle cached prefix for about five minutes.
const CACHE_TTL_SECONDS: i64 = 300;
const USAGE: &str =
    "usage: newt-report [--summary] [--prices input,cached,output] [--ttl seconds] <events.jsonl>";
const SLOW_START_FACTOR: u64 = 2;
const SLOW_START_FLOOR_MS: u64 = 1500;
const HARNESS_GAP_MS: i64 = 50;

#[derive(Debug, Default)]
struct Call {
    model: String,
    prefix_hash: String,
    history_hash: String,
    history_bytes: usize,
    scratch_bytes: usize,
    started_at: Option<OffsetDateTime>,
    finished_at: Option<OffsetDateTime>,
    timing: CallTiming,
    usage: Usage,
    response_id: Option<String>,
    failed: Option<String>,
    interrupted: bool,
}

#[derive(Debug)]
struct Tool {
    name: String,
    ms: u64,
    is_error: bool,
    finished_at: OffsetDateTime,
}

#[derive(Debug, Default)]
struct TurnReport {
    session: String,
    turn_id: String,
    started_at: Option<OffsetDateTime>,
    ended_at: Option<OffsetDateTime>,
    outcome: String,
    calls: Vec<Call>,
    tools: Vec<Tool>,
    open_tools: HashMap<String, String>,
}

fn main() -> anyhow::Result<()> {
    let mut summary = false;
    let mut prices = None;
    let mut ttl = CACHE_TTL_SECONDS;
    let mut path = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--summary" => summary = true,
            "--prices" => {
                let value = args.next().context(USAGE)?;
                prices = Some(value.parse::<Prices>().map_err(anyhow::Error::msg)?);
            }
            "--ttl" => ttl = args.next().context(USAGE)?.parse().context(USAGE)?,
            _ if !arg.starts_with("--") && path.is_none() => path = Some(arg),
            _ => anyhow::bail!(USAGE),
        }
    }
    let path = path.context(USAGE)?;
    let mut profile = newt_server::profile::Profile::default();
    let mut card = CacheCard::new(ttl);
    let file = std::fs::File::open(&path).with_context(|| format!("opening {path}"))?;
    let mut turns: Vec<TurnReport> = Vec::new();
    for (index, line) in BufReader::new(file).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let record: Record = serde_json::from_str(&line)
            .with_context(|| format!("line {} is not a newt event", index + 1))?;
        profile.observe(&record);
        match record {
            Record::Turn(event) => {
                card.observe(&event);
                apply(&mut turns, event);
            }
            Record::Server(event) => apply_server(&mut turns, event),
        }
    }
    let mut out = std::io::stdout().lock();
    let mut flags = Vec::new();
    for turn in &turns {
        if !summary {
            print_turn(&mut out, turn)?;
        }
        flag_turn(turn, &mut flags);
    }
    flag_slow_starts(&turns, &mut flags);
    writeln!(out, "\nturns: {}; flags: {}", turns.len(), flags.len())?;
    if !summary {
        for flag in flags {
            writeln!(out, "  - {flag}")?;
        }
    }
    out.write_all(card.render(prices, !summary).as_bytes())?;
    out.write_all(profile.render().as_bytes())?;
    Ok(())
}

fn turn_mut<'a>(turns: &'a mut Vec<TurnReport>, session: &str, turn: &str) -> &'a mut TurnReport {
    let position = turns
        .iter()
        .rposition(|existing| existing.session == session && existing.turn_id == turn);
    let index = position.unwrap_or_else(|| {
        turns.push(TurnReport {
            session: session.to_owned(),
            turn_id: turn.to_owned(),
            ..TurnReport::default()
        });
        turns.len() - 1
    });
    &mut turns[index]
}

fn call_mut(turn: &mut TurnReport, call: u32) -> &mut Call {
    let index = usize::try_from(call).unwrap_or(usize::MAX);
    while turn.calls.len() <= index {
        turn.calls.push(Call::default());
    }
    &mut turn.calls[index]
}

#[expect(
    clippy::too_many_lines,
    reason = "one flat match over every event kind reads better than splitting it"
)]
fn apply(turns: &mut Vec<TurnReport>, event: Event) {
    let at = event.at;
    match event.kind {
        EventKind::ToolProgress { .. }
        | EventKind::TextDelta { .. }
        | EventKind::ToolInputDelta { .. }
        | EventKind::ToolInput { .. }
        | EventKind::ToolOutput { .. }
        | EventKind::TurnSteered { .. }
        | EventKind::ModelCallReplayed { .. } => {}
        EventKind::TurnStarted { session, turn, .. } => {
            turn_mut(turns, &session, &turn).started_at = Some(at);
        }
        EventKind::ModelCallStarted {
            session,
            turn,
            call,
            model,
            prefix,
            history,
            scratch,
            ..
        } => {
            let entry = call_mut(turn_mut(turns, &session, &turn), call);
            entry.model = model;
            entry.prefix_hash = prefix.hash;
            entry.history_hash = history.hash;
            entry.history_bytes = history.bytes;
            entry.scratch_bytes = scratch.bytes;
            entry.started_at = Some(at);
        }
        EventKind::ModelCallFinished {
            session,
            turn,
            call,
            usage,
            timing,
            response_id,
            ..
        } => {
            let entry = call_mut(turn_mut(turns, &session, &turn), call);
            entry.usage = usage;
            entry.timing = timing;
            entry.response_id = response_id;
            entry.finished_at = Some(at);
        }
        EventKind::ModelCallInterrupted {
            session,
            turn,
            call,
        } => {
            let entry = call_mut(turn_mut(turns, &session, &turn), call);
            entry.interrupted = true;
            entry.finished_at = Some(at);
        }
        EventKind::ModelCallFailed {
            session,
            turn,
            call,
            error,
        } => {
            let entry = call_mut(turn_mut(turns, &session, &turn), call);
            entry.failed = Some(error);
            entry.finished_at = Some(at);
        }
        EventKind::ToolStarted {
            session,
            turn,
            call_id,
            name,
        } => {
            turn_mut(turns, &session, &turn)
                .open_tools
                .insert(call_id, name);
        }
        EventKind::ToolFinished {
            session,
            turn,
            call_id,
            name,
            is_error,
            ms,
            ..
        } => {
            let entry = turn_mut(turns, &session, &turn);
            entry.open_tools.remove(&call_id);
            entry.tools.push(Tool {
                name,
                ms,
                is_error,
                finished_at: at,
            });
        }
        EventKind::TurnCompleted { session, turn, .. } => {
            let entry = turn_mut(turns, &session, &turn);
            entry.ended_at = Some(at);
            "completed".clone_into(&mut entry.outcome);
        }
        EventKind::TurnFailed {
            session,
            turn,
            error,
            ..
        } => {
            let entry = turn_mut(turns, &session, &turn);
            entry.ended_at = Some(at);
            entry.outcome = format!("failed: {error}");
        }
    }
}

fn apply_server(turns: &mut Vec<TurnReport>, event: ServerEvent) {
    if let ServerEventKind::CompactionCompleted {
        session,
        turn,
        tokens_before,
        tokens_after,
    } = event.kind
    {
        turn_mut(turns, &session, &turn).outcome =
            format!("compacted history {tokens_before} -> {tokens_after} tokens");
    }
}

fn millis_between(from: Option<OffsetDateTime>, to: Option<OffsetDateTime>) -> Option<i64> {
    let span = to? - from?;
    i64::try_from(span.whole_milliseconds()).ok()
}

fn ms(value: Option<u64>) -> String {
    value.map_or_else(|| "-".to_owned(), |value| value.to_string())
}

fn short(hash: &str) -> &str {
    hash.get(..8).unwrap_or(hash)
}

fn print_turn(out: &mut impl Write, turn: &TurnReport) -> std::io::Result<()> {
    let wall = millis_between(turn.started_at, turn.ended_at);
    let model: u64 = turn.calls.iter().map(|call| call.timing.total_ms).sum();
    writeln!(
        out,
        "\n{} / {}  {}  wall {} ms, in model calls {} ms, everything else {} ms",
        turn.session,
        turn.turn_id,
        turn.outcome,
        wall.map_or_else(|| "-".to_owned(), |wall| wall.to_string()),
        model,
        wall.map_or_else(
            || "-".to_owned(),
            |wall| (wall - i64::try_from(model).unwrap_or(i64::MAX)).to_string()
        ),
    )?;
    writeln!(
        out,
        "  call  headers  1st-event  1st-output  total   input  cached  output  reasoning  prefix    history   hist-KB  scratch-KB  response"
    )?;
    for (index, call) in turn.calls.iter().enumerate() {
        writeln!(
            out,
            "  {index:<4}  {:>7}  {:>9}  {:>10}  {:>5}  {:>6}  {:>5.0}%  {:>6}  {:>9}  {}  {}  {:>7}  {:>10}  {}{}",
            ms(call.timing.headers_ms),
            ms(call.timing.first_event_ms),
            ms(call.timing.first_output_ms),
            call.timing.total_ms,
            call.usage.input_tokens,
            call.usage.cache_hit_ratio() * 100.0,
            call.usage.output_tokens,
            call.usage.reasoning_tokens,
            short(&call.prefix_hash),
            short(&call.history_hash),
            call.history_bytes / 1024,
            call.scratch_bytes / 1024,
            call.response_id.as_deref().unwrap_or("-"),
            match (&call.failed, call.interrupted) {
                (Some(error), _) => format!("  FAILED: {error}"),
                (None, true) => "  interrupted by a new event".to_owned(),
                (None, false) => String::new(),
            },
        )?;
    }
    for tool in &turn.tools {
        writeln!(
            out,
            "  tool {:<20} {:>6} ms{}",
            tool.name,
            tool.ms,
            if tool.is_error { "  error" } else { "" }
        )?;
    }
    Ok(())
}

fn flag_turn(turn: &TurnReport, flags: &mut Vec<String>) {
    let label = format!("{}/{}", turn.session, turn.turn_id);
    if turn.outcome.is_empty() {
        flags.push(format!("{label}: turn never completed or failed"));
    }
    for (call_id, name) in &turn.open_tools {
        flags.push(format!(
            "{label}: tool {name} ({call_id}) started but never finished"
        ));
    }
    for (index, call) in turn.calls.iter().enumerate() {
        if let Some(error) = &call.failed {
            flags.push(format!("{label} call {index}: model call failed: {error}"));
        }
    }
    for pair in turn.calls.windows(2) {
        let (Some(finished), Some(next_started)) = (pair[0].finished_at, pair[1].started_at) else {
            continue;
        };
        let longest_tool = turn
            .tools
            .iter()
            .filter(|tool| tool.finished_at >= finished && tool.finished_at <= next_started)
            .map(|tool| tool.ms)
            .max()
            .unwrap_or(0);
        let gap = (next_started - finished).whole_milliseconds();
        let gap = i64::try_from(gap).unwrap_or(i64::MAX) - i64::try_from(longest_tool).unwrap_or(0);
        if gap > HARNESS_GAP_MS {
            flags.push(format!(
                "{label}: {gap} ms between model calls not explained by tool time"
            ));
        }
    }
}

fn flag_slow_starts(turns: &[TurnReport], flags: &mut Vec<String>) {
    let mut headers: Vec<u64> = turns
        .iter()
        .flat_map(|turn| turn.calls.iter())
        .filter_map(|call| call.timing.headers_ms)
        .collect();
    if headers.is_empty() {
        return;
    }
    headers.sort_unstable();
    let median = headers[headers.len() / 2];
    for turn in turns {
        for (index, call) in turn.calls.iter().enumerate() {
            if let Some(header_ms) = call.timing.headers_ms
                && header_ms > SLOW_START_FLOOR_MS
                && header_ms > median * SLOW_START_FACTOR
            {
                flags.push(format!(
                    "{}/{} call {index}: response headers took {header_ms} ms (median {median} ms)",
                    turn.session, turn.turn_id
                ));
            }
        }
    }
}
