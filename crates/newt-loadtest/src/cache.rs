use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;

const BLOCK_BYTES: usize = 512;
const TOKENS_PER_BLOCK: u64 = 128;
const MIN_CACHED_TOKENS: u64 = 1024;
const EVICT_ABOVE: usize = 2_000_000;

#[derive(Debug, Clone, Copy)]
pub struct CacheRules {
    pub ttl: Duration,
    pub readable_after: Duration,
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    written: Instant,
    used: Instant,
}

#[derive(Debug)]
pub struct PrefixCache {
    rules: CacheRules,
    entries: Mutex<HashMap<(u64, u64), Entry>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lookup {
    pub input_tokens: u64,
    pub cached_tokens: u64,
}

fn fnv(seed: u64, bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .fold(seed ^ 0xcbf2_9ce4_8422_2325, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
        })
}

pub fn prompt_bytes(body: &Value) -> Vec<u8> {
    let mut bytes = Vec::new();
    for field in ["instructions", "tools"] {
        if let Some(value) = body.get(field) {
            bytes.extend_from_slice(value.to_string().as_bytes());
            bytes.push(0);
        }
    }
    for item in body
        .get("input")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        bytes.extend_from_slice(item.to_string().as_bytes());
        bytes.push(0);
    }
    bytes
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CacheScope {
    #[default]
    Key,
    Prefix,
    Routed {
        machines: u64,
    },
}

const ROUTE_HEAD_BYTES: usize = 1024;
const OVERFLOW_PER_MINUTE: usize = 15;

#[derive(Debug)]
pub struct Router {
    machines: u64,
    recent: Mutex<HashMap<u64, VecDeque<Instant>>>,
}

impl Router {
    pub fn new(machines: u64) -> Self {
        Self {
            machines: machines.max(1),
            recent: Mutex::new(HashMap::new()),
        }
    }

    pub fn machine(&self, cache_key: Option<&str>, prompt: &[u8], roll: u64) -> u64 {
        let route = cache_key.map_or_else(
            || fnv(0, prompt.get(..ROUTE_HEAD_BYTES).unwrap_or(prompt)),
            |key| fnv(1, key.as_bytes()),
        );
        let now = Instant::now();
        let mut recent = match self.recent.lock() {
            Ok(recent) => recent,
            Err(poisoned) => poisoned.into_inner(),
        };
        let window = recent.entry(route).or_default();
        while window
            .front()
            .is_some_and(|at| now.duration_since(*at) >= Duration::from_mins(1))
        {
            window.pop_front();
        }
        window.push_back(now);
        let home = route % self.machines;
        if window.len() <= OVERFLOW_PER_MINUTE || self.machines == 1 {
            home
        } else {
            (home + 1 + roll % (self.machines - 1)) % self.machines
        }
    }
}

impl PrefixCache {
    pub fn new(rules: CacheRules) -> Self {
        Self {
            rules,
            entries: Mutex::new(HashMap::new()),
        }
    }

    pub fn lookup_and_store(&self, key: &str, prompt: &[u8]) -> Lookup {
        let scope = fnv(0, key.as_bytes());
        let input_tokens = u64::try_from(prompt.len() / 4).unwrap_or(u64::MAX);
        let mut chain = 0;
        let hashes: Vec<u64> = prompt
            .chunks(BLOCK_BYTES)
            .filter(|block| block.len() == BLOCK_BYTES)
            .map(|block| {
                chain = fnv(chain, block);
                chain
            })
            .collect();
        let now = Instant::now();
        let mut entries = match self.entries.lock() {
            Ok(entries) => entries,
            Err(poisoned) => poisoned.into_inner(),
        };
        let matched = hashes
            .iter()
            .take_while(|hash| {
                entries.get(&(scope, **hash)).is_some_and(|entry| {
                    now.duration_since(entry.written) >= self.rules.readable_after
                        && now.duration_since(entry.used) < self.rules.ttl
                })
            })
            .count();
        for hash in &hashes {
            entries
                .entry((scope, *hash))
                .and_modify(|entry| {
                    if now.duration_since(entry.used) >= self.rules.ttl {
                        entry.written = now;
                    }
                    entry.used = now;
                })
                .or_insert(Entry {
                    written: now,
                    used: now,
                });
        }
        if entries.len() > EVICT_ABOVE {
            let ttl = self.rules.ttl;
            entries.retain(|_, entry| now.duration_since(entry.used) < ttl);
        }
        let cached = u64::try_from(matched).unwrap_or(0) * TOKENS_PER_BLOCK;
        Lookup {
            input_tokens,
            cached_tokens: if cached >= MIN_CACHED_TOKENS {
                cached.min(input_tokens)
            } else {
                0
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(readable_after: Duration) -> CacheRules {
        CacheRules {
            ttl: Duration::from_mins(5),
            readable_after,
        }
    }

    #[test]
    fn one_key_stays_on_one_machine_until_it_passes_the_overflow_rate() {
        let router = Router::new(4);
        let home = router.machine(Some("assistant/0"), b"prompt", 0);
        for roll in 1..OVERFLOW_PER_MINUTE as u64 {
            assert_eq!(router.machine(Some("assistant/0"), b"prompt", roll), home);
        }
        let spilled: std::collections::BTreeSet<u64> = (0..40)
            .map(|roll| router.machine(Some("assistant/0"), b"prompt", roll))
            .collect();
        assert!(spilled.len() > 1);
    }

    #[test]
    fn without_a_key_the_prompt_head_picks_the_machine() {
        let router = Router::new(8);
        let head = vec![b'a'; ROUTE_HEAD_BYTES];
        let first = [head.clone(), b"customer one".to_vec()].concat();
        let second = [head, b"customer two".to_vec()].concat();
        assert_eq!(
            router.machine(None, &first, 0),
            router.machine(None, &second, 0)
        );
    }

    fn prompt(prefix: &str, tail: &str) -> Vec<u8> {
        format!("{prefix}{tail}").into_bytes()
    }

    #[test]
    fn an_appended_prompt_hits_and_a_rewritten_one_misses() {
        let cache = PrefixCache::new(rules(Duration::ZERO));
        let base = "x".repeat(8_000);
        assert_eq!(
            cache
                .lookup_and_store("k", &prompt(&base, ""))
                .cached_tokens,
            0
        );
        let appended = cache.lookup_and_store("k", &prompt(&base, &"y".repeat(2_000)));
        assert_eq!(
            appended.cached_tokens, 1_920,
            "15 whole blocks of the old prompt"
        );
        let mut rewritten = base.clone().into_bytes();
        rewritten[100] = b'z';
        assert_eq!(cache.lookup_and_store("k", &rewritten).cached_tokens, 0);
        assert_eq!(
            cache
                .lookup_and_store("other", &prompt(&base, ""))
                .cached_tokens,
            0,
            "keys do not share a cache"
        );
    }

    #[test]
    fn a_prefix_written_moments_ago_is_not_readable_yet() {
        let cache = PrefixCache::new(rules(Duration::from_secs(60)));
        let base = "x".repeat(8_000);
        cache.lookup_and_store("k", base.as_bytes());
        assert_eq!(
            cache.lookup_and_store("k", base.as_bytes()).cached_tokens,
            0
        );
    }

    #[test]
    fn short_prompts_never_cache() {
        let cache = PrefixCache::new(rules(Duration::ZERO));
        let short = "x".repeat(3_000);
        cache.lookup_and_store("k", short.as_bytes());
        assert_eq!(
            cache.lookup_and_store("k", short.as_bytes()).cached_tokens,
            0
        );
    }
}
