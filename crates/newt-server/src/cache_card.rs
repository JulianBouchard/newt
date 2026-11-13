//! The cache report card: how much of each model call's input the provider
//! served from its prompt cache, and for every call that missed, what broke
//! the cache.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;

use newt_core::event::{Event, EventKind};
use newt_core::prompt::{LayerFingerprint, PartFingerprint};
use newt_core::provider::Usage;
use time::OffsetDateTime;

/// Calls with less input than this are not cached by providers.
const CACHE_MIN_TOKENS: u64 = 1024;
/// A call is a miss when less than this share of its input was cached.
const CACHE_MISS_RATIO: f64 = 0.5;
const MISSES_LISTED: usize = 20;

/// Why a call missed the cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cause {
    /// No earlier call of this session is in the log.
    FirstCall,
    ModelChanged {
        from: String,
        to: String,
    },
    /// The session moved to another prompt cache key, as when automatic
    /// buckets grow.
    KeyChanged {
        from: String,
        to: String,
    },
    /// The first piece of the prefix that differs, and roughly where it starts.
    PrefixChanged {
        part: String,
        offset: usize,
    },
    /// History no longer starts with the previous call's history.
    HistoryRewritten {
        at: usize,
        of: usize,
        compaction: bool,
    },
    /// The previous call finished longer ago than the cache lives.
    Idle {
        seconds: i64,
    },
    /// Same prefix and history, soon after: most likely served by a machine
    /// that did not hold the cache.
    Unexplained {
        seconds: i64,
    },
}

impl std::fmt::Display for Cause {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FirstCall => write!(formatter, "first call of the session in this log"),
            Self::ModelChanged { from, to } => write!(formatter, "model changed: {from} -> {to}"),
            Self::KeyChanged { from, to } => write!(formatter, "cache key changed: {from} -> {to}"),
            Self::PrefixChanged { part, offset } => {
                write!(
                    formatter,
                    "{part} changed, about {offset} bytes into the prefix"
                )
            }
            Self::HistoryRewritten { at, of, compaction } => write!(
                formatter,
                "history rewritten from message {at} of {of}{}",
                if *compaction { " (compaction)" } else { "" }
            ),
            Self::Idle { seconds } => write!(
                formatter,
                "idle {seconds} s since the previous call, longer than the cache lives"
            ),
            Self::Unexplained { seconds } => write!(
                formatter,
                "same prefix and history {seconds} s later: likely routed to a machine without the cache"
            ),
        }
    }
}

impl Cause {
    /// The cause without its numbers, to count causes by kind.
    fn kind(&self) -> String {
        match self {
            Self::PrefixChanged { part, .. } => format!("{part} changed"),
            Self::HistoryRewritten { compaction, .. } => if *compaction {
                "history rewritten by compaction"
            } else {
                "history rewritten"
            }
            .to_owned(),
            Self::KeyChanged { .. } => {
                "cache key changed (session moved to another key)".to_owned()
            }
            Self::Idle { .. } => "idle past the cache lifetime".to_owned(),
            Self::Unexplained { .. } => "unexplained (routing)".to_owned(),
            other => other.to_string(),
        }
    }
}

#[derive(Debug, Clone)]
struct Started {
    model: String,
    cache_key: String,
    at: OffsetDateTime,
    prefix: LayerFingerprint,
    history: LayerFingerprint,
}

#[derive(Debug, Clone)]
struct Finished {
    started: Started,
    at: OffsetDateTime,
}

#[derive(Debug, Clone)]
pub struct Miss {
    pub session: String,
    pub turn: String,
    pub call: u32,
    pub usage: Usage,
    pub cause: Cause,
}

/// Prices in dollars per million tokens.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Prices {
    pub input: f64,
    pub cached: f64,
    pub output: f64,
}

impl std::str::FromStr for Prices {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        let numbers: Vec<f64> = text
            .split(',')
            .map(|part| {
                part.trim()
                    .parse::<f64>()
                    .map_err(|error| format!("{part}: {error}"))
            })
            .collect::<Result<_, _>>()?;
        match numbers[..] {
            [input, cached, output] => Ok(Self {
                input,
                cached,
                output,
            }),
            _ => Err(
                "prices are three numbers: input,cached,output dollars per million tokens"
                    .to_owned(),
            ),
        }
    }
}

#[derive(Debug, Default)]
struct Totals {
    calls: u64,
    input: u64,
    cached: u64,
    output: u64,
}

impl Totals {
    fn add(&mut self, usage: &Usage) {
        self.calls += 1;
        self.input += usage.input_tokens;
        self.cached += usage.cache_read_tokens;
        self.output += usage.output_tokens + usage.reasoning_tokens;
    }

    #[expect(
        clippy::cast_precision_loss,
        reason = "token counts are far below 2^52"
    )]
    fn cached_share(&self) -> f64 {
        if self.input == 0 {
            return 0.0;
        }
        self.cached as f64 / self.input as f64
    }

    #[expect(
        clippy::cast_precision_loss,
        reason = "token counts are far below 2^52"
    )]
    fn cost(&self, prices: Prices) -> f64 {
        let uncached = self.input.saturating_sub(self.cached);
        (uncached as f64 * prices.input
            + self.cached as f64 * prices.cached
            + self.output as f64 * prices.output)
            / 1_000_000.0
    }
}

/// Builds the card from events in log order.
#[derive(Debug)]
pub struct CacheCard {
    ttl_seconds: i64,
    started: HashMap<(String, String, u32), Started>,
    compacted: HashMap<(String, String), bool>,
    previous: HashMap<String, Finished>,
    totals: Totals,
    sessions: BTreeMap<String, Totals>,
    pub misses: Vec<Miss>,
}

impl CacheCard {
    /// `ttl_seconds` is how long the provider keeps a cached prefix.
    pub fn new(ttl_seconds: i64) -> Self {
        Self {
            ttl_seconds,
            started: HashMap::new(),
            compacted: HashMap::new(),
            previous: HashMap::new(),
            totals: Totals::default(),
            sessions: BTreeMap::new(),
            misses: Vec::new(),
        }
    }

    pub fn observe(&mut self, event: &Event) {
        match &event.kind {
            EventKind::TurnStarted {
                session,
                turn,
                compaction,
                ..
            } => {
                self.compacted
                    .insert((session.clone(), turn.clone()), *compaction);
            }
            EventKind::ModelCallStarted {
                session,
                turn,
                call,
                model,
                cache_key,
                prefix,
                history,
                ..
            } => {
                self.started.insert(
                    (session.clone(), turn.clone(), *call),
                    Started {
                        model: model.clone(),
                        cache_key: cache_key.clone(),
                        at: event.at,
                        prefix: prefix.clone(),
                        history: history.clone(),
                    },
                );
            }
            EventKind::ModelCallFinished {
                session,
                turn,
                call,
                usage,
                ..
            } => {
                let Some(started) = self.started.remove(&(session.clone(), turn.clone(), *call))
                else {
                    return;
                };
                self.finish(session, turn, *call, started, usage, event.at);
            }
            _ => {}
        }
    }

    fn finish(
        &mut self,
        session: &str,
        turn: &str,
        call: u32,
        started: Started,
        usage: &Usage,
        at: OffsetDateTime,
    ) {
        self.totals.add(usage);
        self.sessions
            .entry(session.to_owned())
            .or_default()
            .add(usage);
        if usage.input_tokens >= CACHE_MIN_TOKENS && usage.cache_hit_ratio() < CACHE_MISS_RATIO {
            let compaction = self
                .compacted
                .get(&(session.to_owned(), turn.to_owned()))
                .copied()
                .unwrap_or(false);
            let cause = cause(
                self.previous.get(session),
                &started,
                compaction,
                self.ttl_seconds,
            );
            self.misses.push(Miss {
                session: session.to_owned(),
                turn: turn.to_owned(),
                call,
                usage: *usage,
                cause,
            });
        }
        self.previous
            .insert(session.to_owned(), Finished { started, at });
    }

    pub fn render(&self, prices: Option<Prices>, list_misses: bool) -> String {
        let mut out = String::from("\ncache report card\n");
        let totals = &self.totals;
        let _ = write!(
            out,
            "  calls {}, input {} tokens, cached {:.1}%, uncached {} tokens",
            totals.calls,
            totals.input,
            totals.cached_share() * 100.0,
            totals.input.saturating_sub(totals.cached),
        );
        if let Some(prices) = prices {
            let _ = write!(out, ", cost ${:.4}", totals.cost(prices));
        }
        out.push('\n');
        let _ = writeln!(
            out,
            "  misses: {} (under {:.0}% cached, on calls of {CACHE_MIN_TOKENS}+ input tokens)",
            self.misses.len(),
            CACHE_MISS_RATIO * 100.0
        );
        let mut kinds: BTreeMap<String, (usize, u64)> = BTreeMap::new();
        for miss in &self.misses {
            let entry = kinds.entry(miss.cause.kind()).or_default();
            entry.0 += 1;
            entry.1 += miss
                .usage
                .input_tokens
                .saturating_sub(miss.usage.cache_read_tokens);
        }
        let mut kinds: Vec<(String, (usize, u64))> = kinds.into_iter().collect();
        kinds.sort_by_key(|(_, (_, uncached))| std::cmp::Reverse(*uncached));
        for (kind, (count, uncached)) in kinds {
            let _ = writeln!(
                out,
                "    {count:>4}  {uncached:>10} uncached tokens  {kind}"
            );
        }
        if list_misses {
            for miss in self.misses.iter().take(MISSES_LISTED) {
                let _ = writeln!(
                    out,
                    "  {}/{} call {}: {:.0}% of {} cached: {}",
                    miss.session,
                    miss.turn,
                    miss.call,
                    miss.usage.cache_hit_ratio() * 100.0,
                    miss.usage.input_tokens,
                    miss.cause
                );
            }
            if self.misses.len() > MISSES_LISTED {
                let _ = writeln!(out, "  ... and {} more", self.misses.len() - MISSES_LISTED);
            }
        }
        out
    }
}

fn cause(previous: Option<&Finished>, current: &Started, compaction: bool, ttl: i64) -> Cause {
    let Some(previous) = previous else {
        return Cause::FirstCall;
    };
    if previous.started.model != current.model {
        return Cause::ModelChanged {
            from: previous.started.model.clone(),
            to: current.model.clone(),
        };
    }
    if previous.started.cache_key != current.cache_key {
        return Cause::KeyChanged {
            from: previous.started.cache_key.clone(),
            to: current.cache_key.clone(),
        };
    }
    if previous.started.prefix.hash != current.prefix.hash {
        let (part, offset) = first_change(&previous.started.prefix.parts, &current.prefix.parts);
        return Cause::PrefixChanged { part, offset };
    }
    let before = &previous.started.history.parts;
    let after = &current.history.parts;
    if let Some(at) = before
        .iter()
        .zip(after)
        .position(|(left, right)| left.hash != right.hash)
        .or_else(|| (after.len() < before.len()).then_some(after.len()))
    {
        return Cause::HistoryRewritten {
            at,
            of: before.len(),
            compaction,
        };
    }
    let seconds = (current.at - previous.at).whole_seconds();
    if seconds > ttl {
        return Cause::Idle { seconds };
    }
    Cause::Unexplained { seconds }
}

/// The name of the first part that differs and the bytes before it. Parts
/// added at the end are named as added.
fn first_change(before: &[PartFingerprint], after: &[PartFingerprint]) -> (String, usize) {
    let mut offset = 0;
    for (index, part) in after.iter().enumerate() {
        match before.get(index) {
            Some(old) if old.name == part.name && old.hash == part.hash => offset += part.bytes,
            Some(old) if old.name == part.name => return (part.name.clone(), offset),
            Some(_) | None if before.iter().any(|old| old.name == part.name) => {
                return (format!("tool order around {}", part.name), offset);
            }
            _ => return (format!("{} (added)", part.name), offset),
        }
    }
    match before.get(after.len()) {
        Some(removed) => (format!("{} (removed)", removed.name), offset),
        None => ("the prefix".to_owned(), 0),
    }
}
