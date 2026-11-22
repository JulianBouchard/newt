//! Splits each turn into the runtime's own time and the time it spent waiting on the
//! simulated model and tools, then scales the waiting back to real speed.
//!
//! The fake model and the mock app log when they were busy on each turn's behalf
//! (`crate::intervals`). Inside a turn's window, the union of those intervals is the
//! simulated wait; the rest is the runtime. At real speed the turn takes
//! `own + wait * speed`, measured from one run however fast it went.

use std::collections::HashMap;
use std::fmt::Write as _;

use anyhow::Context;
use serde_json::{Value, json};

use crate::driver::{TurnSample, percentile};
use crate::plan::seed_of;

pub fn write_turns(path: &str, samples: &[TurnSample]) -> anyhow::Result<()> {
    let mut out = String::new();
    for sample in samples {
        let row = json!({
            "session": sample.session,
            "turn": sample.turn,
            "posted_at_ms": sample.posted_at_ms,
            "accept_ms": sample.accept_ms,
            "start_ms": sample.start_ms,
            "first_sign_ms": sample.first_sign_ms,
            "end_ms": sample.end_to_end_ms,
            "ok": sample.ok,
        });
        out.push_str(&row.to_string());
        out.push('\n');
    }
    std::fs::write(path, out).with_context(|| format!("writing {path}"))
}

fn lines(path: &str) -> anyhow::Result<Vec<Value>> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).with_context(|| format!("a line of {path}")))
        .collect()
}

fn busy_within(intervals: &[(f64, f64)], from: f64, to: f64) -> f64 {
    let mut clipped: Vec<(f64, f64)> = intervals
        .iter()
        .map(|&(start, end)| (start.max(from), end.min(to)))
        .filter(|(start, end)| end > start)
        .collect();
    clipped.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut busy = 0.0;
    let mut reach = f64::NEG_INFINITY;
    for (start, end) in clipped {
        if end <= reach {
            continue;
        }
        busy += end - start.max(reach);
        reach = end;
    }
    busy
}

#[derive(Default)]
struct Columns {
    real_start: Vec<f64>,
    real_first_sign: Vec<f64>,
    own_first_sign: Vec<f64>,
    real_end: Vec<f64>,
    own_start: Vec<f64>,
    own_end: Vec<f64>,
    waiting_end: Vec<f64>,
    against_prod: Vec<Paired>,
}

struct Paired {
    prod_start: Option<f64>,
    bench_start: f64,
    prod_first_sign: Option<f64>,
    bench_first_sign: Option<f64>,
    prod_end: f64,
    bench_end: f64,
}

/// Each replayed turn against what the same turn took in production.
fn prod_report(out: &mut String, paired: &[Paired]) {
    let _ = writeln!(
        out,
        "against production (the same turns, replayed): {} turns",
        paired.len()
    );
    if paired.is_empty() {
        return;
    }
    let mut prod_end: Vec<f64> = paired.iter().map(|pair| pair.prod_end).collect();
    let mut bench_end: Vec<f64> = paired.iter().map(|pair| pair.bench_end).collect();
    let mut prod_start: Vec<f64> = paired.iter().filter_map(|pair| pair.prod_start).collect();
    let mut bench_start: Vec<f64> = paired
        .iter()
        .filter(|pair| pair.prod_start.is_some())
        .map(|pair| pair.bench_start)
        .collect();
    let mut prod_sign: Vec<f64> = paired
        .iter()
        .filter_map(|pair| pair.prod_first_sign.zip(pair.bench_first_sign))
        .map(|(prod, _)| prod)
        .collect();
    let mut bench_sign: Vec<f64> = paired
        .iter()
        .filter_map(|pair| pair.prod_first_sign.zip(pair.bench_first_sign))
        .map(|(_, bench)| bench)
        .collect();
    line(out, "prod  event -> turn start", &mut prod_start);
    line(out, "bench event -> turn start", &mut bench_start);
    line(out, "prod  event -> first sign of life", &mut prod_sign);
    line(out, "bench event -> first sign of life", &mut bench_sign);
    line(out, "prod  event -> turn end", &mut prod_end);
    line(out, "bench event -> turn end", &mut bench_end);
    let mut errors: Vec<f64> = paired
        .iter()
        .filter(|pair| pair.prod_end > 0.0)
        .map(|pair| (pair.bench_end - pair.prod_end) / pair.prod_end * 100.0)
        .collect();
    let within = errors.iter().filter(|error| error.abs() <= 15.0).count();
    let total = errors.len();
    let median = percentile(&mut errors, 0.5).unwrap_or(f64::NAN);
    let p10 = percentile(&mut errors, 0.1).unwrap_or(f64::NAN);
    let p90 = percentile(&mut errors, 0.9).unwrap_or(f64::NAN);
    let _ = writeln!(
        out,
        "  per-turn error of turn end: median {median:+.1}%, p10 {p10:+.1}%, p90 {p90:+.1}%, within 15%: {within} of {total}"
    );
}

fn line(out: &mut String, label: &str, values: &mut [f64]) {
    let p50 = percentile(values, 0.5).unwrap_or(f64::NAN);
    let p90 = percentile(values, 0.9).unwrap_or(f64::NAN);
    let _ = writeln!(out, "  {label:<36} p50 {p50:>9.0} ms   p90 {p90:>9.0} ms");
}

pub fn render(
    turns_path: &str,
    interval_paths: &[String],
    speed: f64,
    replay: Option<&crate::replay::Replay>,
) -> anyhow::Result<String> {
    anyhow::ensure!(speed > 0.0, "--speed must be above 0");
    let mut by_turn: HashMap<u64, Vec<(f64, f64)>> = HashMap::new();
    for path in interval_paths {
        for row in lines(path)? {
            let (Some(turn), Some(start), Some(end)) = (
                row["turn"]
                    .as_str()
                    .and_then(|hex| u64::from_str_radix(hex, 16).ok()),
                row["start_ms"].as_f64(),
                row["end_ms"].as_f64(),
            ) else {
                continue;
            };
            by_turn.entry(turn).or_default().push((start, end));
        }
    }
    let mut columns = Columns::default();
    let mut skipped = 0;
    for row in lines(turns_path)? {
        let (Some(session), Some(turn), Some(posted), Some(start), Some(end), Some(true)) = (
            row["session"].as_u64(),
            row["turn"].as_u64(),
            row["posted_at_ms"].as_f64(),
            row["start_ms"].as_f64(),
            row["end_ms"].as_f64(),
            row["ok"].as_bool(),
        ) else {
            skipped += 1;
            continue;
        };
        let key = seed_of(&format!("{session}-{turn}"));
        let intervals = by_turn.get(&key).map_or(&[][..], Vec::as_slice);
        let wait_start = busy_within(intervals, posted, posted + start);
        let wait_end = busy_within(intervals, posted, posted + end);
        let first_sign = row["first_sign_ms"].as_f64().map(|first_sign| {
            let wait_sign = busy_within(intervals, posted, posted + first_sign);
            columns.own_first_sign.push(first_sign - wait_sign);
            let real = first_sign - wait_sign + wait_sign * speed;
            columns.real_first_sign.push(real);
            real
        });
        columns.own_start.push(start - wait_start);
        columns
            .real_start
            .push(start - wait_start + wait_start * speed);
        columns.own_end.push(end - wait_end);
        columns.waiting_end.push(wait_end * speed);
        columns.real_end.push(end - wait_end + wait_end * speed);
        if let Some(prod) = replay.map(|replay| replay.prod_times(key))
            && let Some(prod_end) = prod.end
        {
            columns.against_prod.push(Paired {
                prod_start: prod.start,
                prod_first_sign: prod.first_sign,
                bench_first_sign: first_sign,
                bench_start: start - wait_start + wait_start * speed,
                prod_end,
                bench_end: end - wait_end + wait_end * speed,
            });
        }
    }
    let mut out = String::new();
    let _ = writeln!(
        out,
        "turns: {} split, {skipped} failed or unfinished (run at speed {speed})",
        columns.own_end.len()
    );
    let _ = writeln!(out, "at real speed (runtime + model and tools):");
    line(&mut out, "event -> turn start", &mut columns.real_start);
    line(
        &mut out,
        "event -> first sign of life",
        &mut columns.real_first_sign,
    );
    line(&mut out, "event -> turn end", &mut columns.real_end);
    let _ = writeln!(out, "  of which waiting on model and tools:");
    line(&mut out, "  per turn", &mut columns.waiting_end);
    let _ = writeln!(out, "the runtime's own share:");
    line(&mut out, "event -> turn start", &mut columns.own_start);
    line(
        &mut out,
        "event -> first sign of life",
        &mut columns.own_first_sign,
    );
    line(&mut out, "event -> turn end", &mut columns.own_end);
    if replay.is_some() {
        prod_report(&mut out, &columns.against_prod);
    }
    Ok(out)
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]
mod tests {
    use super::*;

    #[test]
    fn first_sign_of_life_is_written_and_reported_next_to_turn_start_and_end() {
        let samples = [TurnSample {
            session: 0,
            turn: 0,
            posted_at_ms: 1000.0,
            start_ms: Some(40.0),
            first_sign_ms: Some(120.0),
            end_to_end_ms: Some(900.0),
            ok: true,
            ..TurnSample::default()
        }];
        let dir = std::env::temp_dir().join(format!("newt-decompose-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("turns.jsonl");
        let path = path.to_str().unwrap();

        write_turns(path, &samples).unwrap();
        let report = render(path, &[], 1.0, None).unwrap();

        assert_eq!(lines(path).unwrap()[0]["first_sign_ms"], 120.0);
        let sign = report
            .lines()
            .find(|line| line.contains("event -> first sign of life"))
            .unwrap();
        assert!(sign.contains("p50       120 ms"), "{sign}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn overlapping_and_outside_intervals_count_once_and_only_inside_the_window() {
        let intervals = [(0.0, 10.0), (5.0, 15.0), (20.0, 30.0), (95.0, 120.0)];
        assert!((busy_within(&intervals, 2.0, 100.0) - 28.0).abs() < 1e-9);
    }
}
