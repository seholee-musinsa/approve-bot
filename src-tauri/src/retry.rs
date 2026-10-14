//! Per-resource cooldowns. A failing repository must not stop healthy ones.
use std::collections::HashMap;

#[derive(Debug, Default)]
pub struct RetryState {
    entries: HashMap<String, Failure>,
}
#[derive(Debug)]
struct Failure {
    attempts: u32,
    next: i64,
    kind: String,
}

impl RetryState {
    pub fn ready(&self, key: &str, now: i64) -> bool {
        self.entries.get(key).is_none_or(|f| now >= f.next)
    }
    pub fn failure(&mut self, key: &str, now: i64, hint: Option<u64>, kind: &str) -> bool {
        let prior = self.entries.get(key);
        let notify = prior.is_none_or(|f| f.kind != kind);
        let attempts = prior.map_or(1, |f| f.attempts.saturating_add(1));
        let delay = 30u64
            .saturating_mul(1u64 << attempts.saturating_sub(1).min(5))
            .min(900)
            .max(hint.unwrap_or(0));
        self.entries.insert(
            key.into(),
            Failure {
                attempts,
                next: now.saturating_add(delay.min(i64::MAX as u64) as i64),
                kind: kind.into(),
            },
        );
        notify
    }
    pub fn success(&mut self, key: &str) -> Option<u32> {
        self.entries.remove(key).map(|f| f.attempts)
    }
    /// A user refresh may retry local failures, but cannot waive GitHub's delay.
    pub fn reset(&mut self) {
        self.entries.retain(|_, f| f.kind == "rate_limit");
    }
}

#[derive(Debug, thiserror::Error)]
#[error("GitHub rate limit; retry after {seconds} seconds")]
pub struct Throttle {
    pub seconds: u64,
}

pub fn hint(error: &anyhow::Error) -> Option<u64> {
    error
        .chain()
        .find_map(|e| e.downcast_ref::<Throttle>().map(|t| t.seconds))
}

pub fn kind(error: &anyhow::Error) -> &'static str {
    if hint(error).is_some() {
        return "rate_limit";
    }
    let text = format!("{error:#}").to_ascii_lowercase();
    if text.contains("dns") {
        "dns"
    } else if text.contains("timed out") || text.contains("timeout") {
        "timeout"
    } else if text.contains("500")
        || text.contains("502")
        || text.contains("503")
        || text.contains("504")
    {
        "server"
    } else {
        "request"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cooldown_is_per_resource_and_backoff_resets_on_recovery() {
        let mut s = RetryState::default();
        assert!(s.failure("a", 100, None, "dns"));
        assert!(!s.ready("a", 101));
        assert!(s.ready("b", 101));
        assert!(s.ready("a", 130));
        assert!(!s.failure("a", 130, None, "dns"));
        assert!(!s.ready("a", 159));
        assert!(s.ready("a", 190));
        assert_eq!(s.success("a"), Some(2));
        assert!(s.ready("a", 191));
        assert!(s.failure("a", 200, None, "timeout"));
    }
    #[test]
    fn manual_refresh_resets_local_backoff_but_not_server_rate_limits() {
        let mut s = RetryState::default();
        s.failure("a", 0, Some(3600), "rate_limit");
        assert!(!s.ready("a", 3599));
        assert!(s.ready("a", 3600));
        s.failure("b", 0, None, "dns");
        s.reset();
        assert!(s.ready("b", 1));
        assert!(!s.ready("a", 1));
        assert!(s.ready("a", 3600));
    }
}
