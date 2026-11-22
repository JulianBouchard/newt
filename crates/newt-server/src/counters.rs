//! Counters and the slot-wait histogram, and their Prometheus text form.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::metrics::Load;

/// Something that happened that `/metrics` counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Counter {
    EventsAccepted,
    EventsDuplicate,
    EventsLimited,
    EventsConflicted,
    TurnsStarted,
    TurnsCompleted,
    TurnsFailed,
    TurnsCancelled,
    CommitsFenced,
    DeadLetters,
    Retries,
    WakesDropped,
}

const COUNTERS: usize = 12;

struct Family {
    name: &'static str,
    help: &'static str,
    label: &'static str,
    series: &'static [(&'static str, Counter)],
}

const FAMILIES: &[Family] = &[
    Family {
        name: "newt_events_total",
        help: "Events posted to the event API, by outcome.",
        label: "outcome",
        series: &[
            ("accepted", Counter::EventsAccepted),
            ("duplicate", Counter::EventsDuplicate),
            ("limited", Counter::EventsLimited),
            ("conflicted", Counter::EventsConflicted),
        ],
    },
    Family {
        name: "newt_turns_total",
        help: "Turns by outcome; started counts every turn that began.",
        label: "outcome",
        series: &[
            ("started", Counter::TurnsStarted),
            ("completed", Counter::TurnsCompleted),
            ("failed", Counter::TurnsFailed),
            ("cancelled", Counter::TurnsCancelled),
        ],
    },
    Family {
        name: "newt_commits_fenced_total",
        help: "Turn commits discarded because the session's lease had moved.",
        label: "",
        series: &[("", Counter::CommitsFenced)],
    },
    Family {
        name: "newt_dead_letters_total",
        help: "Events given up on after a turn could not answer them.",
        label: "",
        series: &[("", Counter::DeadLetters)],
    },
    Family {
        name: "newt_retries_total",
        help: "Sessions released with a backoff to be retried.",
        label: "",
        series: &[("", Counter::Retries)],
    },
    Family {
        name: "newt_wakes_dropped_total",
        help: "Wakes skipped because too many sessions were already waiting for a slot; the sweep retries them.",
        label: "",
        series: &[("", Counter::WakesDropped)],
    },
];

const WAIT_BOUNDS_SECONDS: [f64; 9] = [0.001, 0.005, 0.025, 0.1, 0.5, 1.0, 5.0, 30.0, 120.0];

#[derive(Debug, Default)]
pub struct Counters {
    counts: [AtomicU64; COUNTERS],
    wait_buckets: [AtomicU64; WAIT_BOUNDS_SECONDS.len() + 1],
    wait_micros: AtomicU64,
}

impl Counters {
    pub fn add(&self, counter: Counter, amount: u64) {
        self.counts[counter as usize].fetch_add(amount, Ordering::Relaxed);
    }

    pub fn get(&self, counter: Counter) -> u64 {
        self.counts[counter as usize].load(Ordering::Relaxed)
    }

    pub fn observe_slot_wait(&self, waited: Duration) {
        let seconds = waited.as_secs_f64();
        let bucket = WAIT_BOUNDS_SECONDS
            .iter()
            .position(|bound| seconds <= *bound)
            .unwrap_or(WAIT_BOUNDS_SECONDS.len());
        self.wait_buckets[bucket].fetch_add(1, Ordering::Relaxed);
        let micros = u64::try_from(waited.as_micros()).unwrap_or(u64::MAX);
        self.wait_micros.fetch_add(micros, Ordering::Relaxed);
    }

    /// The Prometheus text exposition of the counters, the slot-wait
    /// histogram and the given load gauges.
    pub fn render(&self, load: Load) -> String {
        let mut out = String::new();
        for family in FAMILIES {
            header(&mut out, family.name, family.help, "counter");
            for (value, counter) in family.series {
                let count = self.get(*counter);
                if family.label.is_empty() {
                    line(&mut out, family.name, "", count);
                } else {
                    let labels = format!("{{{}=\"{value}\"}}", family.label);
                    line(&mut out, family.name, &labels, count);
                }
            }
        }
        for (name, help, value) in [
            (
                "newt_running_turns",
                "Turns running on this instance.",
                load.running,
            ),
            (
                "newt_waiting_turns",
                "Woken sessions waiting for a turn slot.",
                load.waiting,
            ),
            (
                "newt_max_sessions",
                "Turn slots this instance runs with.",
                load.max_sessions,
            ),
        ] {
            header(&mut out, name, help, "gauge");
            line(&mut out, name, "", value);
        }
        header(
            &mut out,
            "newt_oldest_wait_seconds",
            "How long the longest-waiting session has waited for a slot.",
            "gauge",
        );
        line(
            &mut out,
            "newt_oldest_wait_seconds",
            "",
            Duration::from_millis(load.oldest_wait_ms).as_secs_f64(),
        );
        self.render_slot_wait(&mut out);
        out
    }

    fn render_slot_wait(&self, out: &mut String) {
        let name = "newt_slot_wait_seconds";
        header(
            out,
            name,
            "Time woken sessions waited for a turn slot.",
            "histogram",
        );
        let mut cumulative = 0;
        for (index, bound) in WAIT_BOUNDS_SECONDS.iter().enumerate() {
            cumulative += self.wait_buckets[index].load(Ordering::Relaxed);
            line(
                out,
                &format!("{name}_bucket"),
                &format!("{{le=\"{bound}\"}}"),
                cumulative,
            );
        }
        cumulative += self.wait_buckets[WAIT_BOUNDS_SECONDS.len()].load(Ordering::Relaxed);
        line(out, &format!("{name}_bucket"), "{le=\"+Inf\"}", cumulative);
        line(
            out,
            &format!("{name}_sum"),
            "",
            Duration::from_micros(self.wait_micros.load(Ordering::Relaxed)).as_secs_f64(),
        );
        line(out, &format!("{name}_count"), "", cumulative);
    }
}

fn header(out: &mut String, name: &str, help: &str, kind: &str) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} {kind}");
}

fn line(out: &mut String, name: &str, labels: &str, value: impl std::fmt::Display) {
    let _ = writeln!(out, "{name}{labels} {value}");
}
