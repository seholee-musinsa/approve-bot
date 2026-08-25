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

    // --- Claude review engine (all serde-defaulted for backward compat) ---
    /// When true, run a `claude -p` review before approving; the score gates
    /// the approve. When false, fall back to legacy blind approve.
    #[serde(default = "default_true")]
    pub review_enabled: bool,
    /// Model passed to `claude -p`.
    #[serde(default = "default_review_model")]
    pub review_model: String,
    /// Override path to the review guide md. Empty = app config dir's
    /// `review-guide.md`, else the bundled default.
    #[serde(default)]
    pub review_guide_path: String,
    /// Minimum score (0-5) to auto-approve; below this posts a COMMENT review.
    #[serde(default = "default_min_score")]
    pub min_approve_score: f64,
    /// Extended-thinking token budget for the review (0 = off).
    #[serde(default = "default_thinking")]
    pub review_thinking_tokens: u32,
    /// Deep review: clone the PR head and let the model explore it read-only
    /// (more accurate, slower). Clone failure falls back to diff-only.
    #[serde(default = "default_true")]
    pub review_deep: bool,
    /// Once this PR already has an engine review, don't re-review new commits —
    /// just approve them (saves Claude quota + avoids duplicate review comments).
    /// The first review always runs; this only affects subsequent commits.
    #[serde(default = "default_true")]
    pub approve_only_after_review: bool,
    /// Attach inline line comments to reviews (resolvable threads). Turn off if a
    /// repo enables "require conversation resolution" and the threads become a
    /// merge bottleneck.
    #[serde(default = "default_true")]
    pub inline_comments_enabled: bool,
}

fn default_true() -> bool {
    true
}

fn default_review_model() -> String {
    "claude-sonnet-5".to_string()
}

fn default_min_score() -> f64 {
    4.0
}

fn default_thinking() -> u32 {
    4000
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
            review_enabled: true,
            review_model: default_review_model(),
            review_guide_path: String::new(),
            min_approve_score: default_min_score(),
            review_thinking_tokens: default_thinking(),
            review_deep: true,
            approve_only_after_review: true,
            inline_comments_enabled: true,
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
        if !self.min_approve_score.is_finite() {
            self.min_approve_score = 4.0;
        }
        self.min_approve_score = self.min_approve_score.clamp(0.0, 5.0);
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
