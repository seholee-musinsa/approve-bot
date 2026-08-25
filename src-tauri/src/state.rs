use crate::config::AppConfig;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{Mutex, Notify};

const MAX_LOG: usize = 200;

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
        Arc::new(Self {
            config_dir,
            config: Mutex::new(config),
            token: Mutex::new(None),
            current_user: Mutex::new(None),
            status: Mutex::new(ConnectionStatus::disconnected("not yet connected")),
            activity: Mutex::new(VecDeque::with_capacity(MAX_LOG)),
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
    }

    pub async fn clear_activity(&self) {
        self.activity.lock().await.clear();
    }

    pub async fn recent_activity(&self, limit: usize) -> Vec<ActivityEntry> {
        let log = self.activity.lock().await;
        log.iter().take(limit).cloned().collect()
    }
}
