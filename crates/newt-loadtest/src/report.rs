use std::fmt::Write as _;
use std::time::Duration;

use serde_json::Value;

use crate::driver::{TurnSample, percentile};

fn line(label: &str, mut values: Vec<f64>) -> String {
    let show =
        |value: Option<f64>| value.map_or_else(|| "-".to_owned(), |value| format!("{value:.0}"));
    let p50 = percentile(&mut values, 0.5);
    let p90 = percentile(&mut values, 0.9);
    let p99 = percentile(&mut values, 0.99);
    let max = values.last().copied();
    format!(
        "{label:<22} {:>9} {:>9} {:>9} {:>9}   (n={})\n",
        show(p50),
        show(p90),
        show(p99),
        show(max),
        values.len()
    )
}

fn delta(before: &Value, after: &Value, key: &str) -> u64 {
    let read = |value: &Value| value.get(key).and_then(Value::as_u64).unwrap_or(0);
    read(after).saturating_sub(read(before))
}

#[expect(
    clippy::cast_precision_loss,
    reason = "counts and token totals far below 2^52"
)]
pub fn render(
    samples: &[TurnSample],
    wall: Duration,
    model: (&Value, &Value),
    app: (&Value, &Value),
) -> String {
    let mut out = String::new();
    let completed = samples.iter().filter(|sample| sample.ok).count();
    let failed = samples
        .iter()
        .filter(|sample| sample.end_to_end_ms.is_some() && !sample.ok)
        .count();
    let errors = samples
        .iter()
        .filter(|sample| sample.error.is_some())
        .count();
    let _ = writeln!(
        out,
        "turns: {} sent, {completed} completed, {failed} failed, {errors} errors in {:.1}s ({:.2} turns/s)",
        samples.len(),
        wall.as_secs_f64(),
        completed as f64 / wall.as_secs_f64().max(0.001)
    );
    let _ = writeln!(
        out,
        "\n{:<22} {:>9} {:>9} {:>9} {:>9}",
        "ms", "p50", "p90", "p99", "max"
    );
    out.push_str(&line(
        "read receipt",
        samples.iter().map(|sample| sample.accept_ms).collect(),
    ));
    out.push_str(&line(
        "event to turn start",
        samples
            .iter()
            .filter_map(|sample| sample.start_ms)
            .collect(),
    ));
    out.push_str(&line(
        "turn duration",
        samples
            .iter()
            .filter_map(|sample| Some(sample.end_to_end_ms? - sample.start_ms?))
            .collect(),
    ));
    out.push_str(&line(
        "event to turn end",
        samples
            .iter()
            .filter_map(|sample| sample.end_to_end_ms)
            .collect(),
    ));
    let calls = delta(model.0, model.1, "calls");
    let input = delta(model.0, model.1, "input_tokens");
    let cached = delta(model.0, model.1, "cached_tokens");
    let _ = writeln!(
        out,
        "\nmodel: {calls} calls ({:.1} per turn), {input} input tokens ({:.0}% cached), {} output tokens, {} model failures",
        calls as f64 / completed.max(1) as f64,
        100.0 * cached as f64 / input.max(1) as f64,
        delta(model.0, model.1, "output_tokens"),
        delta(model.0, model.1, "failures"),
    );
    let _ = writeln!(
        out,
        "tools: {} MCP calls, {} sends",
        delta(app.0, app.1, "tool_calls"),
        delta(app.0, app.1, "sends")
    );
    let mut reasons: Vec<&str> = samples
        .iter()
        .filter_map(|sample| sample.error.as_deref())
        .collect();
    reasons.sort_unstable();
    reasons.dedup();
    if !reasons.is_empty() {
        let _ = writeln!(out, "error kinds: {}", reasons.join(", "));
    }
    out
}
