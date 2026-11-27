//! Prompt cache keys: which of an agent's keys a session uses, and, in auto
//! mode, how many keys an agent spreads its sessions over.
//!
//! Sessions of one agent share a system prompt and tools, so sharing a key
//! lets a new session reuse a prefix other sessions already cached. `OpenAI`
//! spreads one key over more machines past roughly 15 requests a minute, and a
//! request that lands on another machine misses. Auto mode counts each agent's
//! model calls over the last minute and keeps every key under
//! [`CALLS_PER_KEY`] a minute: it adds keys as soon as the rate needs them and
//! drops them only after the rate has stayed low for [`SHRINK_AFTER`].
//! Sessions map to keys by jump consistent hashing, so going from n to n + 1
//! keys moves only one session in n + 1 to another key. A session whose
//! provider cache is still warm keeps the key it used last while that key
//! stays under [`CALLS_PER_KEY`], so adding keys moves no session that is
//! fine where it is: new and idle sessions, and those on a busy key, take
//! their hashed key.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Deserialize;

/// Model calls a minute one key should carry at most, under `OpenAI`'s ~15.
pub const CALLS_PER_KEY: usize = 12;
/// How long the rate must stay low before an agent gives up keys.
pub const SHRINK_AFTER: Duration = Duration::from_mins(10);
const MAX_BUCKETS: u32 = 256;
/// How long a session keeps its last key: about as long as a provider keeps
/// an idle prefix cached. After that the cache is cold and a new key is free.
const STICK_FOR: Duration = Duration::from_mins(10);
/// Past this many remembered sessions, those idle longer than [`STICK_FOR`]
/// are forgotten. After each sweep the next one waits until the map has
/// doubled, so a map of recent sessions is not swept on every call.
const FORGET_ABOVE: usize = 4096;
const WINDOW: Duration = Duration::from_mins(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum Buckets {
    Fixed(u32),
    Auto(Auto),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Auto {
    Auto,
}

impl Buckets {
    pub const AUTO: Self = Self::Auto(Auto::Auto);
}

#[derive(Debug)]
struct Rate {
    calls: VecDeque<Instant>,
    buckets: u32,
    low_since: Option<Instant>,
}

#[derive(Debug)]
pub struct CacheKeys {
    buckets: Buckets,
    agents: Mutex<HashMap<String, Rate>>,
    last: Mutex<HashMap<(String, String), (u32, Instant)>>,
    forget_above: std::sync::atomic::AtomicUsize,
    per_key: Mutex<HashMap<String, VecDeque<Instant>>>,
}

impl CacheKeys {
    pub fn new(buckets: Buckets) -> Self {
        Self {
            buckets,
            agents: Mutex::new(HashMap::new()),
            last: Mutex::new(HashMap::new()),
            forget_above: std::sync::atomic::AtomicUsize::new(FORGET_ABOVE),
            per_key: Mutex::new(HashMap::new()),
        }
    }

    /// The key a session's model calls carry: `{agent}/{bucket}`.
    pub fn key(&self, agent: &str, session: &str) -> String {
        self.key_at(agent, session, Instant::now())
    }

    fn key_at(&self, agent: &str, session: &str, now: Instant) -> String {
        let count = self.buckets(agent);
        let busy = |bucket: u32| self.calls_on(&format!("{agent}/{bucket}"), now) >= CALLS_PER_KEY;
        let mut last = match self.last.lock() {
            Ok(last) => last,
            Err(poisoned) => poisoned.into_inner(),
        };
        let id = (agent.to_owned(), session.to_owned());
        let bucket = match last.get(&id) {
            Some((bucket, used))
                if *bucket < count && now.duration_since(*used) < STICK_FOR && !busy(*bucket) =>
            {
                *bucket
            }
            _ => jump(fnv1a(session), count),
        };
        last.insert(id, (bucket, now));
        if last.len() > self.forget_above.load(std::sync::atomic::Ordering::Relaxed) {
            last.retain(|_, (_, used)| now.duration_since(*used) < STICK_FOR);
            self.forget_above.store(
                (last.len() * 2).max(FORGET_ABOVE),
                std::sync::atomic::Ordering::Relaxed,
            );
        }
        format!("{agent}/{bucket}")
    }

    pub fn buckets(&self, agent: &str) -> u32 {
        match self.buckets {
            Buckets::Fixed(count) => count.max(1),
            Buckets::Auto(_) => self.agents().get(agent).map_or(1, |rate| rate.buckets),
        }
    }

    /// Counts one model call of `agent` on `key`, made now.
    pub fn record_call(&self, agent: &str, key: &str) {
        self.record_call_at(agent, key, Instant::now());
    }

    /// Model calls on `key` in the last minute.
    fn calls_on(&self, key: &str, now: Instant) -> usize {
        let per_key = match self.per_key.lock() {
            Ok(per_key) => per_key,
            Err(poisoned) => poisoned.into_inner(),
        };
        per_key.get(key).map_or(0, |calls| {
            calls
                .iter()
                .filter(|call| now.duration_since(**call) <= WINDOW)
                .count()
        })
    }

    fn record_call_at(&self, agent: &str, key: &str, now: Instant) {
        if matches!(self.buckets, Buckets::Fixed(_)) {
            return;
        }
        {
            let mut per_key = match self.per_key.lock() {
                Ok(per_key) => per_key,
                Err(poisoned) => poisoned.into_inner(),
            };
            let calls = per_key.entry(key.to_owned()).or_default();
            calls.push_back(now);
            while calls
                .front()
                .is_some_and(|call| now.duration_since(*call) > WINDOW)
            {
                calls.pop_front();
            }
        }
        let mut agents = self.agents();
        let rate = agents.entry(agent.to_owned()).or_insert_with(|| Rate {
            calls: VecDeque::new(),
            buckets: 1,
            low_since: None,
        });
        rate.calls.push_back(now);
        while rate
            .calls
            .front()
            .is_some_and(|call| now.duration_since(*call) > WINDOW)
        {
            rate.calls.pop_front();
        }
        let needed = u32::try_from(rate.calls.len().div_ceil(CALLS_PER_KEY))
            .unwrap_or(MAX_BUCKETS)
            .clamp(1, MAX_BUCKETS);
        if needed >= rate.buckets {
            if needed > rate.buckets {
                tracing::info!(
                    agent,
                    from = rate.buckets,
                    to = needed,
                    "more prompt cache keys for a higher call rate"
                );
            }
            rate.buckets = needed;
            rate.low_since = None;
            return;
        }
        let low_since = *rate.low_since.get_or_insert(now);
        if now.duration_since(low_since) >= SHRINK_AFTER {
            tracing::info!(
                agent,
                from = rate.buckets,
                to = needed,
                "fewer prompt cache keys after a lower call rate"
            );
            rate.buckets = needed;
            rate.low_since = None;
        }
    }

    fn agents(&self) -> std::sync::MutexGuard<'_, HashMap<String, Rate>> {
        match self.agents.lock() {
            Ok(agents) => agents,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

fn fnv1a(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// Lamping and Veach's jump consistent hash: a bucket in `0..buckets` that
/// changes for only one key in `buckets + 1` when a bucket is added.
#[expect(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the algorithm works in floating point on values below 2^32"
)]
fn jump(mut key: u64, buckets: u32) -> u32 {
    let mut bucket: i64 = -1;
    let mut next: i64 = 0;
    while next < i64::from(buckets.max(1)) {
        bucket = next;
        key = key.wrapping_mul(2_862_933_555_777_941_757).wrapping_add(1);
        next = ((bucket + 1) as f64 * ((1_u64 << 31) as f64 / ((key >> 33) + 1) as f64)) as i64;
    }
    bucket as u32
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::time::{Duration, Instant};

    use super::{Buckets, CALLS_PER_KEY, CacheKeys, FORGET_ABOVE, SHRINK_AFTER, STICK_FOR, jump};

    #[test]
    fn a_map_of_recent_sessions_is_swept_once_per_doubling_not_on_every_call() {
        let keys = CacheKeys::new(Buckets::Fixed(1));
        let now = std::time::Instant::now();
        for session in 0..=FORGET_ABOVE {
            keys.key_at("assistant", &session.to_string(), now);
        }
        let after_first_sweep = keys.forget_above.load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(after_first_sweep, (FORGET_ABOVE + 1) * 2);
        keys.key_at("assistant", "one more", now);
        assert_eq!(
            keys.forget_above.load(std::sync::atomic::Ordering::Relaxed),
            after_first_sweep,
            "one more recent session sweeps nothing"
        );
    }

    #[test]
    fn a_fixed_count_spreads_sessions_and_keeps_each_on_one_key() {
        let one = CacheKeys::new(Buckets::Fixed(1));
        assert_eq!(one.key("assistant", "customer-1"), "assistant/0");
        assert_eq!(one.key("assistant", "customer-2"), "assistant/0");
        let eight = CacheKeys::new(Buckets::Fixed(8));
        assert_eq!(eight.key("assistant", "c-1"), eight.key("assistant", "c-1"));
        let keys: BTreeSet<String> = (0..200)
            .map(|customer| eight.key("assistant", &format!("customer-{customer}")))
            .collect();
        assert_eq!(keys.len(), 8);
    }

    #[test]
    fn adding_a_bucket_moves_only_the_sessions_the_new_bucket_takes() {
        let moved = (0..10_000_u64)
            .filter(|key| {
                let key = key.wrapping_mul(0x9e37_79b9_7f4a_7c15);
                let before = jump(key, 4);
                let after = jump(key, 5);
                assert!(after == before || after == 4);
                after != before
            })
            .count();
        assert!((1_700..2_300).contains(&moved), "{moved}");
    }

    #[test]
    fn auto_adds_keys_as_the_rate_climbs_and_drops_them_only_after_a_quiet_spell() {
        let keys = CacheKeys::new(Buckets::AUTO);
        let start = Instant::now();
        assert_eq!(keys.buckets("assistant"), 1);
        for call in 0..(CALLS_PER_KEY * 3) {
            keys.record_call_at(
                "assistant",
                "assistant/0",
                start + Duration::from_millis(call as u64 * 100),
            );
        }
        assert_eq!(keys.buckets("assistant"), 3);
        assert_eq!(keys.buckets("other"), 1);

        let quiet = start + Duration::from_mins(5);
        keys.record_call_at("assistant", "assistant/0", quiet);
        assert_eq!(keys.buckets("assistant"), 3);
        keys.record_call_at(
            "assistant",
            "assistant/0",
            quiet + Duration::from_secs(SHRINK_AFTER.as_secs() - 1),
        );
        assert_eq!(keys.buckets("assistant"), 3);
        keys.record_call_at("assistant", "assistant/0", quiet + SHRINK_AFTER);
        assert_eq!(keys.buckets("assistant"), 1);
    }

    #[test]
    fn a_warm_session_keeps_its_key_until_that_key_gets_busy() {
        let keys = CacheKeys::new(Buckets::AUTO);
        let start = Instant::now();
        let sessions: Vec<String> = (0..200).map(|n| format!("customer-{n}")).collect();
        for session in &sessions {
            assert_eq!(keys.key_at("assistant", session, start), "assistant/0");
        }
        for call in 0..(CALLS_PER_KEY * 8) {
            let other = format!("assistant/{}", 1 + call % 7);
            keys.record_call_at(
                "assistant",
                &other,
                start + Duration::from_millis(call as u64 * 100),
            );
        }
        assert_eq!(keys.buckets("assistant"), 8);
        let soon = start + Duration::from_secs(30);
        for session in &sessions {
            assert_eq!(keys.key_at("assistant", session, soon), "assistant/0");
        }
        let newcomers: BTreeSet<String> = (0..200)
            .map(|n| keys.key_at("assistant", &format!("new-{n}"), soon))
            .collect();
        assert_eq!(newcomers.len(), 8);

        for call in 0..CALLS_PER_KEY {
            keys.record_call_at(
                "assistant",
                "assistant/0",
                soon + Duration::from_millis(call as u64),
            );
        }
        let busy = soon + Duration::from_secs(1);
        let moved: BTreeSet<String> = sessions
            .iter()
            .map(|session| keys.key_at("assistant", session, busy))
            .collect();
        assert_eq!(moved.len(), keys.buckets("assistant") as usize);
    }

    #[test]
    fn an_idle_session_takes_its_hashed_key() {
        let keys = CacheKeys::new(Buckets::Fixed(8));
        let start = Instant::now();
        let first = keys.key_at("assistant", "customer-1", start);
        assert_eq!(
            keys.key_at("assistant", "customer-1", start + STICK_FOR),
            first
        );
        assert_eq!(
            first,
            format!("assistant/{}", jump(super::fnv1a("customer-1"), 8))
        );
    }

    #[test]
    fn the_setting_reads_a_count_or_auto() {
        #[derive(serde::Deserialize)]
        struct Setting {
            buckets: Buckets,
        }
        let parse = |text: &str| toml::from_str::<Setting>(text).map(|setting| setting.buckets);
        assert_eq!(parse("buckets = 4").ok(), Some(Buckets::Fixed(4)));
        assert_eq!(parse("buckets = \"auto\"").ok(), Some(Buckets::AUTO));
        assert!(parse("buckets = \"many\"").is_err());
    }
}
