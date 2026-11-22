use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

const WORDS: &[&str] = &[
    "task", "team", "remote", "note", "budget", "backend", "owner", "series", "call", "week",
    "Lisbon", "Berlin", "draft", "meeting", "great", "plan", "project", "review", "stack", "rust",
    "payments", "ledger", "shipping", "release", "next", "Thursday", "thanks", "sounds", "good",
    "yes",
];

#[derive(Debug)]
pub struct Rng(AtomicU64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(AtomicU64::new(seed | 1))
    }

    pub fn next_u64(&self) -> u64 {
        let mut z = self
            .0
            .fetch_add(0x9e37_79b9_7f4a_7c15, Ordering::Relaxed)
            .wrapping_add(0x9e37_79b9_7f4a_7c15);
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    #[expect(
        clippy::cast_precision_loss,
        reason = "53 random bits mapped onto [0, 1) need no more precision"
    )]
    pub fn unit(&self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1_u64 << 53) as f64
    }

    pub fn below(&self, bound: usize) -> usize {
        usize::try_from(self.next_u64() % u64::try_from(bound.max(1)).unwrap_or(1)).unwrap_or(0)
    }

    pub fn chance(&self, probability: f64) -> bool {
        self.unit() < probability
    }

    fn normal(&self) -> f64 {
        let u1 = self.unit().max(f64::MIN_POSITIVE);
        let u2 = self.unit();
        (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
    }

    pub fn lognormal(&self, p50: f64, p90: f64) -> f64 {
        if p50 <= 0.0 {
            return 0.0;
        }
        let sigma = ((p90.max(p50) / p50).ln() / 1.2816).max(0.0);
        (p50.ln() + sigma * self.normal()).exp()
    }

    pub fn words(&self, count: usize) -> String {
        (0..count)
            .map(|_| WORDS[self.below(WORDS.len())])
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Spread {
    pub p50: f64,
    pub p90: f64,
}

impl Spread {
    pub const fn new(p50: f64, p90: f64) -> Self {
        Self { p50, p90 }
    }

    pub fn millis(self, rng: &Rng, speed: f64) -> Duration {
        Duration::from_secs_f64(rng.lognormal(self.p50, self.p90) / 1000.0 / speed.max(0.001))
    }

    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a sampled count is small and non-negative"
    )]
    pub fn count(self, rng: &Rng) -> usize {
        rng.lognormal(self.p50, self.p90).round().max(1.0) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lognormal_samples_land_near_their_median() {
        let rng = Rng::new(7);
        let mut samples: Vec<f64> = (0..4001).map(|_| rng.lognormal(200.0, 700.0)).collect();
        samples.sort_by(f64::total_cmp);
        let median = samples[2000];
        let p90 = samples[3600];
        assert!((150.0..260.0).contains(&median), "{median}");
        assert!((500.0..950.0).contains(&p90), "{p90}");
    }
}
