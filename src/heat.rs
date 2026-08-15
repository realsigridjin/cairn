use parking_lot::Mutex;
use std::collections::HashMap;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct HeatPolicy {
    pub window: Duration,
    pub min_queries: u32,
    pub remote_bytes_fraction: f64,
    pub max_entries: usize,
}

impl Default for HeatPolicy {
    fn default() -> Self {
        Self {
            window: Duration::from_secs(600),
            min_queries: 3,
            remote_bytes_fraction: 0.35,
            max_entries: 100_000,
        }
    }
}

#[derive(Debug, Clone)]
struct Heat {
    started: Instant,
    touched: Instant,
    queries: u32,
    remote_bytes: u64,
    object_size: u64,
}

#[derive(Debug)]
pub struct HeatTracker {
    policy: HeatPolicy,
    inner: Mutex<HashMap<String, Heat>>,
}

impl HeatTracker {
    pub fn new(policy: HeatPolicy) -> Self {
        Self {
            policy,
            inner: Mutex::new(HashMap::new()),
        }
    }

    pub fn record_query(&self, key: &str, remote_bytes: u64, object_size: u64) -> bool {
        let now = Instant::now();
        let mut g = self.inner.lock();
        if g.len() >= self.policy.max_entries && !g.contains_key(key) {
            prune(&mut g, now, self.policy.window, self.policy.max_entries);
        }
        let h = g.entry(key.to_string()).or_insert(Heat {
            started: now,
            touched: now,
            queries: 0,
            remote_bytes: 0,
            object_size,
        });
        if now.duration_since(h.started) > self.policy.window {
            *h = Heat {
                started: now,
                touched: now,
                queries: 0,
                remote_bytes: 0,
                object_size,
            };
        }
        h.touched = now;
        h.queries = h.queries.saturating_add(1);
        h.remote_bytes = h.remote_bytes.saturating_add(remote_bytes);
        h.object_size = object_size;
        self.promoted(h)
    }

    pub fn should_promote(&self, key: &str) -> bool {
        let now = Instant::now();
        let g = self.inner.lock();
        let Some(h) = g.get(key) else { return false };
        if now.duration_since(h.started) > self.policy.window {
            return false;
        }
        self.promoted(h)
    }

    fn promoted(&self, h: &Heat) -> bool {
        h.queries >= self.policy.min_queries
            || (h.object_size > 0
                && (h.remote_bytes as f64 / h.object_size as f64)
                    >= self.policy.remote_bytes_fraction)
    }
}

fn prune(g: &mut HashMap<String, Heat>, now: Instant, window: Duration, max_entries: usize) {
    g.retain(|_, h| now.duration_since(h.touched) <= window);
    if g.len() < max_entries {
        return;
    }
    let mut oldest: Vec<_> = g.iter().map(|(k, h)| (h.touched, k.clone())).collect();
    oldest.sort_by_key(|x| x.0);
    let target = max_entries.saturating_mul(9) / 10;
    for (_, key) in oldest.into_iter().take(g.len().saturating_sub(target)) {
        g.remove(&key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probes_do_not_increment_heat() {
        let tracker = HeatTracker::new(HeatPolicy {
            window: Duration::from_secs(60),
            min_queries: 2,
            remote_bytes_fraction: 2.0,
            max_entries: 100,
        });
        assert!(!tracker.should_promote("k"));
        assert!(!tracker.record_query("k", 0, 100));
        assert!(!tracker.should_promote("k"));
        assert!(tracker.record_query("k", 0, 100));
        assert!(tracker.should_promote("k"));
    }
}
