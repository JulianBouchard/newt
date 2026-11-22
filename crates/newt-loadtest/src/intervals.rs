//! When the fake model and the mock app were busy on a turn's behalf, written as JSON
//! lines so a run can split each turn into the runtime's own time and the simulated
//! model and tool time.
//!
//! Turns are tagged by a hash of the message that started them. The fake model adds the
//! tag to every tool call's `text` argument, so the mock app can attribute its calls
//! without knowing the runtime.

use std::fmt::Write as _;
use std::io::Write as _;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

const TAG: &str = " #turn=";
const LATENCY: &str = " #latency_ms=";

pub fn epoch_ms(at: SystemTime) -> f64 {
    at.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
        * 1000.0
}

pub fn turn_key(text: &str) -> Option<u64> {
    let (_, rest) = text.split_once("load message ")?;
    let key = rest.split(':').next()?;
    Some(crate::plan::seed_of(key))
}

pub fn tag_arguments(arguments: &mut Value, turn: u64) {
    if let Some(Value::String(text)) = arguments.get_mut("text") {
        let _ = write!(text, "{TAG}{turn:016x}");
    }
}

pub fn tag_latency(arguments: &mut Value, latency_ms: f64) {
    if let Some(Value::String(text)) = arguments.get_mut("text") {
        let _ = write!(text, "{LATENCY}{latency_ms:.0}");
    }
}

pub fn latency_of(arguments: Option<&serde_json::Map<String, Value>>) -> Option<Duration> {
    let text = arguments?.get("text")?.as_str()?;
    let (_, rest) = text.rsplit_once(LATENCY)?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse::<u64>().ok().map(Duration::from_millis)
}

pub fn turn_of(arguments: Option<&serde_json::Map<String, Value>>) -> Option<u64> {
    let text = arguments?.get("text")?.as_str()?;
    let (_, hex) = text.rsplit_once(TAG)?;
    u64::from_str_radix(hex.get(..16)?, 16).ok()
}

#[derive(Debug, Default)]
pub struct IntervalLog(Option<Mutex<std::fs::File>>);

impl IntervalLog {
    pub fn open(path: Option<&std::path::Path>) -> std::io::Result<Self> {
        Ok(Self(match path {
            Some(path) => Some(Mutex::new(std::fs::File::create(path)?)),
            None => None,
        }))
    }

    pub fn record(&self, turn: u64, what: &str, started: SystemTime, busy: Duration) {
        let Some(file) = &self.0 else {
            return;
        };
        let start = epoch_ms(started);
        let line = json!({
            "turn": format!("{turn:016x}"),
            "what": what,
            "start_ms": start,
            "end_ms": start + busy.as_secs_f64() * 1000.0,
        });
        if let Ok(mut file) = file.lock() {
            let _ = writeln!(file, "{line}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tagged_argument_reads_back_its_turn() {
        let mut arguments = json!({ "text": "hello there" });
        tag_arguments(&mut arguments, 0xdead_beef_0000_0001);
        assert_eq!(turn_of(arguments.as_object()), Some(0xdead_beef_0000_0001));
        assert_eq!(turn_of(json!({ "text": "no tag" }).as_object()), None);
    }
}
