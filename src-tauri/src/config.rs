use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub repositories: Vec<String>,
    pub allowed_authors: Vec<String>,
    pub polling_interval_seconds: u64,
    pub auto_approve_enabled: bool,
    pub approval_message: String,
    pub skip_drafts: bool,
    // Backward-compatible: existing config.json written before this field will
    // deserialize with the default (notifications on) instead of failing.
    #[serde(default = "default_true")]
    pub notifications_enabled: bool,
}

fn default_true() -> bool {
    true
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            repositories: vec![],
            allowed_authors: vec![],
            polling_interval_seconds: 60,
            auto_approve_enabled: true,
            approval_message: String::new(),
            skip_drafts: true,
            notifications_enabled: true,
        }
    }
}

impl AppConfig {
    pub fn clamp(&mut self) {
        if self.polling_interval_seconds < 30 {
            self.polling_interval_seconds = 30;
        }
        if self.polling_interval_seconds > 3600 {
            self.polling_interval_seconds = 3600;
        }
        // dedupe + lowercase author names
        self.allowed_authors = dedup(
            self.allowed_authors
                .iter()
                .map(|s| s.trim().trim_start_matches('@').to_lowercase())
                .filter(|s| !s.is_empty()),
        );
        self.repositories = dedup(
            self.repositories
                .iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
        );
    }
}

fn dedup<I: Iterator<Item = String>>(iter: I) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = vec![];
    for s in iter {
        if seen.insert(s.clone()) {
            out.push(s);
        }
    }
    out
}

pub fn config_path(base_dir: &std::path::Path) -> PathBuf {
    base_dir.join("config.json")
}

pub fn load(base_dir: &std::path::Path) -> Result<AppConfig> {
    let path = config_path(base_dir);
    if !path.exists() {
        return Ok(AppConfig::default());
    }
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("failed reading {}", path.display()))?;
    let mut cfg: AppConfig = serde_json::from_str(&raw)
        .with_context(|| format!("failed parsing {}", path.display()))?;
    cfg.clamp();
    Ok(cfg)
}

pub fn save(base_dir: &std::path::Path, cfg: &AppConfig) -> Result<()> {
    fs::create_dir_all(base_dir)
        .with_context(|| format!("failed creating dir {}", base_dir.display()))?;
    let path = config_path(base_dir);
    let raw = serde_json::to_string_pretty(cfg)?;
    fs::write(&path, raw).with_context(|| format!("failed writing {}", path.display()))?;
    Ok(())
}
