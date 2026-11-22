use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{Value, json};

use crate::audit::Audit;
use crate::cache::{CacheRules, CacheScope, PrefixCache, Router};
use crate::plan::Finish;
use crate::profile::{ModelTiming, Profile};
use crate::sample::Rng;

const CACHE_TTL: Duration = Duration::from_mins(5);
/// What the load driver's messages start with, so a harness reminder mid-turn is never read as a new turn.

#[derive(Debug, Default)]
pub struct Stats {
    pub calls: AtomicU64,
    pub input_tokens: AtomicU64,
    pub cached_tokens: AtomicU64,
    pub output_tokens: AtomicU64,
    pub tool_calls: AtomicU64,
    pub failures: AtomicU64,
    pub instructions_bytes: AtomicU64,
    pub tools_bytes: AtomicU64,
    pub input_bytes: AtomicU64,
}

impl Stats {
    pub fn snapshot(&self) -> Value {
        json!({
            "calls": self.calls.load(Ordering::Relaxed),
            "input_tokens": self.input_tokens.load(Ordering::Relaxed),
            "cached_tokens": self.cached_tokens.load(Ordering::Relaxed),
            "output_tokens": self.output_tokens.load(Ordering::Relaxed),
            "tool_calls": self.tool_calls.load(Ordering::Relaxed),
            "failures": self.failures.load(Ordering::Relaxed),
            "instructions_bytes": self.instructions_bytes.load(Ordering::Relaxed),
            "tools_bytes": self.tools_bytes.load(Ordering::Relaxed),
            "input_bytes": self.input_bytes.load(Ordering::Relaxed),
        })
    }
}

#[derive(Debug)]
pub struct FakeModel {
    pub(super) rng: Rng,
    pub(super) timing: ModelTiming,
    pub(super) speed: f64,
    pub(super) failure_rate: f64,
    pub(super) cache: PrefixCache,
    pub(super) ids: AtomicU64,
    pub(super) scope: CacheScope,
    pub(super) router: Router,
    pub(super) audit: Audit,
    pub(super) dump_dir: Option<std::path::PathBuf>,
    pub(super) turn_marker: Option<String>,
    pub(super) finish: Finish,
    pub(super) replay: Option<crate::replay::Replay>,
    pub(super) replay_progress: std::sync::Mutex<HashMap<u64, usize>>,
    pub(super) intervals: crate::intervals::IntervalLog,
    pub(super) profile: Profile,
    pub stats: Stats,
}

#[derive(Debug, Clone, Default)]
pub struct FakeOptions {
    pub speed: f64,
    pub failure_rate: f64,
    pub seed: u64,
    pub cache_scope: CacheScope,
    pub dump_dir: Option<std::path::PathBuf>,
    pub turn_marker: Option<String>,
    pub decision_log: Option<std::path::PathBuf>,
    pub cache_log: Option<std::path::PathBuf>,
    pub finish: Finish,
    pub interval_log: Option<std::path::PathBuf>,
    pub replay: Option<std::path::PathBuf>,
    pub profile: Profile,
}

impl FakeModel {
    pub fn new(timing: ModelTiming, options: FakeOptions) -> std::io::Result<Arc<Self>> {
        let FakeOptions {
            speed,
            failure_rate,
            seed: rng_seed,
            cache_scope,
            dump_dir,
            turn_marker,
            decision_log,
            cache_log,
            finish,
            interval_log,
            replay,
            profile,
        } = options;
        let replay = replay
            .map(|path| crate::replay::Replay::load(&path.to_string_lossy()))
            .transpose()
            .map_err(std::io::Error::other)?;
        let machines = match cache_scope {
            CacheScope::Routed { machines } => machines,
            CacheScope::Key | CacheScope::Prefix => 1,
        };
        Ok(Arc::new(Self {
            rng: Rng::new(rng_seed),
            timing,
            speed,
            failure_rate,
            cache: PrefixCache::new(CacheRules {
                ttl: CACHE_TTL.div_f64(speed.max(0.001)),
                readable_after: timing.cache_readable_after.div_f64(speed.max(0.001)),
            }),
            ids: AtomicU64::new(rng_seed),
            scope: cache_scope,
            router: Router::new(machines),
            audit: Audit::new(decision_log.as_deref(), cache_log.as_deref())?,
            dump_dir,
            turn_marker: turn_marker.or_else(|| Some(crate::driver::TURN_MARKER.to_owned())),
            finish,
            replay,
            replay_progress: std::sync::Mutex::new(HashMap::new()),
            intervals: crate::intervals::IntervalLog::open(interval_log.as_deref())?,
            profile,
            stats: Stats::default(),
        }))
    }

    pub(crate) fn next_id(&self) -> u64 {
        self.ids.fetch_add(1, Ordering::Relaxed)
    }
}
