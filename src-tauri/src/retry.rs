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
    /// Consecutive failures recorded for `key` (0 when it is healthy).
    pub fn attempts(&self, key: &str) -> u32 {
        self.entries.get(key).map_or(0, |f| f.attempts)
    }
    /// When `key` becomes ready again, as a unix timestamp; None when ready now.
    pub fn next_ready(&self, key: &str) -> Option<i64> {
        self.entries.get(key).map(|f| f.next)
    }
    /// A user refresh may retry local failures, but cannot waive GitHub's delay
    /// or the model's usage limit — retrying those only burns the quota.
    pub fn reset(&mut self) {
        self.entries.retain(|_, f| matches!(f.kind.as_str(), "rate_limit" | "usage_limit"));
    }
}

/// Seconds until the model's usage window resets, read from the CLI error text
/// (`You've hit your weekly limit · resets 12am (Asia/Seoul)`). The hour is a
/// wall-clock time in the named zone (a few known zones; else the local
/// offset). Floor 5 minutes, cap 7 days. None when the text names no reset.
pub fn usage_limit_reset_secs(text: &str, now: chrono::DateTime<chrono::Utc>) -> Option<u64> {
    use chrono::{FixedOffset, NaiveTime, TimeZone};
    let lower = text.to_ascii_lowercase();
    let idx = lower.find("resets")?;
    let rest = lower[idx + "resets".len()..].trim_start();
    let mut digits = String::new();
    let mut chars = rest.chars().peekable();
    while let Some(c) = chars.peek() {
        if c.is_ascii_digit() || *c == ':' {
            digits.push(*c);
            chars.next();
        } else {
            break;
        }
    }
    if digits.is_empty() {
        return None;
    }
    let tail: String = chars.collect();
    let tail = tail.trim_start();
    let (hour, minute) = match digits.split_once(':') {
        Some((h, m)) => (h.parse::<u32>().ok()?, m.parse::<u32>().ok()?),
        None => (digits.parse::<u32>().ok()?, 0),
    };
    let hour = if tail.starts_with("pm") && hour < 12 {
        hour + 12
    } else if tail.starts_with("am") && hour == 12 {
        0
    } else {
        hour
    };
    let offset_secs = tail
        .find('(')
        .and_then(|o| tail[o + 1..].split(')').next())
        .and_then(|zone| match zone.trim() {
            "asia/seoul" | "asia/tokyo" => Some(9 * 3600),
            "utc" | "etc/utc" => Some(0),
            "america/los_angeles" => Some(-7 * 3600),
            "america/new_york" => Some(-4 * 3600),
            "europe/london" => Some(3600),
            _ => None,
        })
        .unwrap_or_else(|| chrono::Local::now().offset().local_minus_utc());
    let tz = FixedOffset::east_opt(offset_secs)?;
    let local_now = now.with_timezone(&tz);
    let at = NaiveTime::from_hms_opt(hour % 24, minute % 60, 0)?;
    let mut target = tz.from_local_datetime(&local_now.date_naive().and_time(at)).single()?;
    if target <= local_now {
        target += chrono::Duration::days(1);
    }
    let secs = (target - local_now).num_seconds().max(0) as u64;
    Some(secs.clamp(300, 7 * 86_400))
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
    if text.contains("weekly limit") || text.contains("usage limit") {
        "usage_limit"
    } else if text.contains("oauth") || text.contains("authenticate") {
        "auth"
    } else if text.contains("dns") {
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

    #[test]
    fn usage_limit_backoff_survives_manual_reset_and_uses_reset_hint() {
        let mut s = RetryState::default();
        assert!(s.failure("engine", 1_000, Some(3_600), "usage_limit"));
        assert!(!s.ready("engine", 1_000 + 3_599));
        assert!(s.ready("engine", 1_000 + 3_600));
        assert_eq!(s.next_ready("engine"), Some(4_600));
        s.failure("pr#1@abc#engine", 1_000, None, "engine");
        assert_eq!(s.attempts("pr#1@abc#engine"), 1);
        s.reset();
        assert!(!s.ready("engine", 1_000), "a manual refresh must not burn the quota");
        assert!(s.ready("pr#1@abc#engine", 1_000), "local engine failures may be retried by hand");
        assert_eq!(s.attempts("missing"), 0);
    }
    #[test]
    fn reset_time_is_parsed_from_resets_12am_seoul() {
        use chrono::TimeZone;
        // 2026-10-26 14:00 KST = 05:00 UTC; "12am (Asia/Seoul)" is 10 h away.
        let now = chrono::Utc.with_ymd_and_hms(2026, 10, 26, 5, 0, 0).unwrap();
        assert_eq!(usage_limit_reset_secs("You've hit your weekly limit · resets 12am (Asia/Seoul)", now), Some(10 * 3600));
        assert_eq!(usage_limit_reset_secs("resets 3pm (Asia/Seoul)", now), Some(3600));
        assert_eq!(usage_limit_reset_secs("resets 1:30pm (UTC)", now), Some(8 * 3600 + 30 * 60));
        // Already past today → tomorrow.
        assert_eq!(usage_limit_reset_secs("resets 1pm (Asia/Seoul)", now), Some(23 * 3600));
        // Too close → floor of 5 minutes.
        assert_eq!(usage_limit_reset_secs("resets 2:03pm (Asia/Seoul)", now), Some(300));
        assert_eq!(usage_limit_reset_secs("no reset here", now), None);
        assert_eq!(usage_limit_reset_secs("resets soon", now), None);
    }
    #[test]
    fn engine_error_kinds() {
        let e = |t: &str| anyhow::anyhow!("{t}");
        assert_eq!(kind(&e("claude 엔진 오류: You've hit your weekly limit")), "usage_limit");
        assert_eq!(kind(&e("OAuth session expired")), "auth");
        assert_eq!(kind(&e("dns error")), "dns");
    }
}
