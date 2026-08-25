use crate::config::AppConfig;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{Mutex, Notify};

const MAX_LOG: usize = 200;

fn activity_path(dir: &Path) -> PathBuf {
    dir.join("activity.json")
}

/// Load the persisted activity log (newest-first). Missing/corrupt → empty.
fn load_activity(dir: &Path) -> VecDeque<ActivityEntry> {
    std::fs::read_to_string(activity_path(dir))
        .ok()
        .and_then(|s| serde_json::from_str::<Vec<ActivityEntry>>(&s).ok())
        .map(|v| v.into_iter().take(MAX_LOG).collect())
        .unwrap_or_default()
}

/// Persist the activity log so review entries (and their full bodies) survive an
/// app restart — otherwise the in-memory log is lost and the reviews vanish from
/// the UI. Best-effort; a write failure just means this snapshot isn't saved.
fn save_activity(dir: &Path, log: &VecDeque<ActivityEntry>) {
    let _ = std::fs::create_dir_all(dir);
    if let Ok(json) = serde_json::to_string(&log.iter().collect::<Vec<_>>()) {
        let _ = std::fs::write(activity_path(dir), json);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionStatus {
    pub connected: bool,
    pub username: Option<String>,
    pub rate_limit_remaining: Option<u64>,
    pub rate_limit_total: Option<u64>,
    pub last_error: Option<String>,
    pub checked_at: DateTime<Utc>,
}

impl ConnectionStatus {
    pub fn disconnected(reason: impl Into<String>) -> Self {
        Self {
            connected: false,
            username: None,
            rate_limit_remaining: None,
            rate_limit_total: None,
            last_error: Some(reason.into()),
            checked_at: Utc::now(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActivityKind {
    Approved,
    Skipped,
    Error,
    Info,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivityEntry {
    pub timestamp: DateTime<Utc>,
    pub kind: ActivityKind,
    pub repo: Option<String>,
    pub pr_number: Option<u64>,
    pub pr_title: Option<String>,
    pub author: Option<String>,
    pub url: Option<String>,
    pub message: String,
    /// Optional long-form detail (e.g. the full review body) shown collapsed in
    /// the activity log, expandable on click. Serde-defaulted for backward compat.
    #[serde(default)]
    pub detail: Option<String>,
}

pub struct AppState {
    pub config_dir: PathBuf,
    pub config: Mutex<AppConfig>,
    pub token: Mutex<Option<String>>,
    pub current_user: Mutex<Option<String>>,
    pub status: Mutex<ConnectionStatus>,
    pub activity: Mutex<VecDeque<ActivityEntry>>,
    pub poll_signal: Notify,
    pub gh_login_busy: Mutex<bool>,
}

impl AppState {
    pub fn new(config_dir: PathBuf, config: AppConfig) -> Arc<Self> {
        let activity = load_activity(&config_dir);
        Arc::new(Self {
            config_dir,
            config: Mutex::new(config),
            token: Mutex::new(None),
            current_user: Mutex::new(None),
            status: Mutex::new(ConnectionStatus::disconnected("not yet connected")),
            activity: Mutex::new(activity),
            poll_signal: Notify::new(),
            gh_login_busy: Mutex::new(false),
        })
    }

    pub async fn push_activity(&self, entry: ActivityEntry) {
        let mut log = self.activity.lock().await;
        if log.len() >= MAX_LOG {
            log.pop_back();
        }
        log.push_front(entry);
        save_activity(&self.config_dir, &log);
    }

    pub async fn clear_activity(&self) {
        let mut log = self.activity.lock().await;
        log.clear();
        save_activity(&self.config_dir, &log);
    }

    pub async fn recent_activity(&self, limit: usize) -> Vec<ActivityEntry> {
        let log = self.activity.lock().await;
        log.iter().take(limit).cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_round_trips_through_disk() {
        let dir = std::env::temp_dir().join(format!("ab-state-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut log = VecDeque::new();
        log.push_front(ActivityEntry {
            timestamp: Utc::now(),
            kind: ActivityKind::Approved,
            repo: Some("o/r".into()),
            pr_number: Some(7),
            pr_title: Some("t".into()),
            author: Some("a".into()),
            url: None,
            message: "approved".into(),
            detail: Some("# 리뷰 본문\n전문 유지".into()),
        });
        save_activity(&dir, &log);
        let loaded = load_activity(&dir);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].detail.as_deref(), Some("# 리뷰 본문\n전문 유지"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_activity_file_is_empty() {
        let dir = std::env::temp_dir().join(format!("ab-state-none-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(load_activity(&dir).is_empty());
    }
}
