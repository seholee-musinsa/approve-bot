//! Append-only review history, independent of the bounded UI activity list.
use anyhow::{anyhow, Result};
use chrono::{DateTime, Datelike, FixedOffset, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Record {
    pub run_id: String,
    pub at: String,
    pub started_at: String,
    pub repo: String,
    pub pr: u64,
    pub head_sha: String,
    pub source: String,
    pub status: String,
    pub model: String,
    pub risk: String,
    pub build_sha: String,
    pub guide_hash: String,
    pub guide_source: String,
    pub elapsed_ms: u64,
    /// False means elapsed_ms is a legacy placeholder, not measured zero.
    pub timing_available: bool,
    pub cost_usd: Option<f64>,
    pub finished_cleanly: Option<bool>,
    pub review_id: Option<u64>,
    pub review_url: Option<String>,
    pub finding_ids: Vec<String>,
    pub inline_omitted: bool,
    pub error: Option<String>,
    pub result: Option<serde_json::Value>,
    /// Fresh snapshot after publication; preserves evidence of the remaining race.
    pub post_publication: Option<serde_json::Value>,
}

#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub date: String,
    pub timezone: String,
    pub corrupt_lines: usize,
    pub records: Vec<Record>,
    pub statuses: BTreeMap<String, usize>,
    pub known_cost_usd: f64,
    pub unknown_cost_runs: usize,
    pub unknown_timing_runs: usize,
    pub distinct_prs: usize,
    pub distinct_findings: usize,
    pub normal_reviews: usize,
    pub failed_reviews: usize,
    pub reapprovals: usize,
    pub incomplete_runs: usize,
    pub sources: BTreeMap<String, usize>,
}

static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static WRITE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn append(dir: &Path, record: &Record) -> Result<()> {
    append_batch(dir, std::slice::from_ref(record), false).map(|_| ())
}

fn append_batch(dir: &Path, records: &[Record], deduplicate: bool) -> Result<usize> {
    use std::io::{BufRead, BufReader, Write};
    if records.is_empty() {
        return Ok(0);
    }
    let encoded: Vec<_> = records
        .iter()
        .map(serde_json::to_vec)
        .collect::<std::result::Result<_, _>>()?;
    let _lock = WRITE_LOCK
        .lock()
        .map_err(|_| anyhow!("audit lock poisoned"))?;
    std::fs::create_dir_all(dir)?;
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).read(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(dir.join("review-audit.jsonl"))?;
    fs2::FileExt::lock_exclusive(&file)?;
    // Hold the same interprocess lock across comparison and append, so two
    // concurrent imports of the same snapshot cannot grow duplicate history.
    let mut latest = BTreeMap::new();
    if deduplicate {
        for line in BufReader::new(&file).lines() {
            let line = line?;
            if let Ok(r) = serde_json::from_str::<Record>(&line) {
                let bytes = serde_json::to_vec(&r)?;
                latest.insert((r.repo, r.run_id), bytes);
            }
        }
    }
    let mut written = 0;
    for (record, bytes) in records.iter().zip(encoded) {
        let key = (record.repo.clone(), record.run_id.clone());
        if deduplicate && latest.get(&key) == Some(&bytes) {
            continue;
        }
        // A newline first isolates a partial last record left by a crash.
        file.write_all(b"\n")?;
        file.write_all(&bytes)?;
        file.write_all(b"\n")?;
        latest.insert(key, bytes);
        written += 1;
    }
    if written > 0 {
        file.sync_data()?;
    }
    Ok(written)
}

fn offset(text: &str) -> Result<FixedOffset> {
    if text == "UTC" || text == "Z" {
        return Ok(FixedOffset::east_opt(0).unwrap());
    }
    let bytes = text.as_bytes();
    if bytes.len() != 6
        || !matches!(bytes[0], b'+' | b'-')
        || bytes[3] != b':'
        || ![1, 2, 4, 5].iter().all(|i| bytes[*i].is_ascii_digit())
    {
        return Err(anyhow!("timezone must be UTC or ±HH:MM"));
    }
    let h: i32 = text[1..3].parse()?;
    let m: i32 = text[4..6].parse()?;
    if h > 23 || m > 59 {
        return Err(anyhow!("invalid timezone offset"));
    }
    FixedOffset::east_opt((h * 3600 + m * 60) * if bytes[0] == b'-' { -1 } else { 1 })
        .ok_or_else(|| anyhow!("invalid timezone offset"))
}

/// Latest record per (repo, run_id), then one record per publication: a
/// native run and a GitHub import of the same review are merged (native
/// execution metadata wins, finding evidence is unioned). Second value is the
/// number of malformed lines skipped. Missing file → empty.
pub fn latest_publications(dir: &Path) -> Result<(Vec<Record>, usize)> {
    use std::io::{BufRead, BufReader};
    let mut corrupt = 0;
    let file = match std::fs::File::open(dir.join("review-audit.jsonl")) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), 0)),
        Err(e) => return Err(e.into()),
    };
    fs2::FileExt::lock_shared(&file)?;
    let mut latest = BTreeMap::new();
    for line in BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Record>(&line) {
            Ok(r) if !r.run_id.is_empty() && DateTime::parse_from_rfc3339(&r.at).is_ok() => {
                latest.insert((r.repo.clone(), r.run_id.clone()), r);
            }
            _ => corrupt += 1,
        }
    }
    // A native run and a GitHub import may describe the same publication.
    // Keep execution metadata from the native run and union finding evidence.
    let mut publications: BTreeMap<(String, String), Record> = BTreeMap::new();
    for (_, mut record) in latest {
        let identity = record
            .review_id
            .map(|id| format!("review:{id}"))
            .unwrap_or_else(|| format!("run:{}", record.run_id));
        let key = (record.repo.clone(), identity);
        if let Some(mut existing) = publications.remove(&key) {
            let native = |source: &str| matches!(source, "engine" | "approval_only" | "reapproval");
            if native(&record.source) && !native(&existing.source) {
                std::mem::swap(&mut record, &mut existing);
            }
            for fid in record.finding_ids {
                if !existing.finding_ids.contains(&fid) {
                    existing.finding_ids.push(fid);
                }
            }
            if existing.review_url.is_none() {
                existing.review_url = record.review_url;
            }
            publications.insert(key, existing);
        } else {
            publications.insert(key, record);
        }
    }
    Ok((publications.into_values().collect(), corrupt))
}

pub fn report(dir: &Path, date: &str, timezone: &str) -> Result<Report> {
    let day = NaiveDate::parse_from_str(date, "%Y-%m-%d")?;
    let tz = offset(timezone)?;
    let mut out = Report {
        date: date.into(),
        timezone: timezone.into(),
        ..Report::default()
    };
    let (publications, corrupt) = latest_publications(dir)?;
    out.corrupt_lines = corrupt;
    let mut prs = BTreeSet::new();
    let mut findings = BTreeSet::new();
    for mut r in publications {
        let at = DateTime::parse_from_rfc3339(&r.at)?;
        if at.with_timezone(&tz).date_naive() != day {
            continue;
        }
        r.timing_available = r.timing_available || (r.source == "engine" && r.elapsed_ms > 0);
        if !r.timing_available {
            out.unknown_timing_runs += 1;
        }
        *out.statuses.entry(r.status.clone()).or_default() += 1;
        *out.sources.entry(r.source.clone()).or_default() += 1;
        if r.pr > 0 && !r.repo.is_empty() {
            prs.insert((r.repo.clone(), r.pr));
        }
        for fid in &r.finding_ids {
            findings.insert((r.repo.clone(), r.pr, fid.clone()));
        }
        if r.status == "reapproval" || r.status == "legacy_reapproval" {
            out.reapprovals += 1;
        } else if r.finished_cleanly == Some(false)
            || matches!(
                r.status.as_str(),
                "task_failed" | "legacy_failed" | "model_failed" | "failure_notice"
            )
        {
            out.failed_reviews += 1;
        } else if r.finished_cleanly == Some(true) {
            out.normal_reviews += 1;
        } else {
            out.incomplete_runs += 1;
        }
        if let Some(cost) = r.cost_usd.filter(|c| c.is_finite() && *c >= 0.0) {
            out.known_cost_usd += cost;
        } else {
            r.cost_usd = None;
            out.unknown_cost_runs += 1;
        }
        out.records.push(r);
    }
    out.distinct_prs = prs.len();
    out.distinct_findings = findings.len();
    out.records
        .sort_by(|a, b| a.at.cmp(&b.at).then(a.run_id.cmp(&b.run_id)));
    Ok(out)
}

pub fn fingerprint(text: &str) -> String {
    // Stable provenance identifier, not a security digest.
    let mut n = 0xcbf29ce484222325u64;
    for b in text.bytes() {
        n ^= u64::from(b);
        n = n.wrapping_mul(0x100000001b3);
    }
    format!("fnv1a64:{n:016x}")
}

impl Record {
    pub fn start(
        repo: &str,
        pr: u64,
        sha: &str,
        model: &str,
        risk: &str,
        guide: &str,
        guide_source: String,
    ) -> Self {
        let at = Utc::now();
        Self {
            run_id: format!(
                "{}-{}-{}",
                std::process::id(),
                at.timestamp_nanos_opt().unwrap_or_default(),
                SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ),
            at: at.to_rfc3339(),
            started_at: at.to_rfc3339(),
            repo: repo.into(),
            pr,
            head_sha: sha.into(),
            source: "engine".into(),
            status: "started".into(),
            model: model.into(),
            risk: risk.into(),
            build_sha: option_env!("APPROVE_BOT_BUILD_SHA")
                .unwrap_or("unknown")
                .into(),
            guide_hash: fingerprint(guide),
            guide_source,
            ..Self::default()
        }
    }
    pub fn update(&mut self, status: &str) {
        self.at = Utc::now().to_rfc3339();
        self.status = status.into();
    }
}

fn csv_cell(value: &str) -> String {
    // Spreadsheet formula injection is not a valid interpretation of audit data.
    let safe = if value.trim_start().starts_with(['=', '+', '-', '@']) {
        format!("'{value}")
    } else {
        value.to_string()
    };
    format!("\"{}\"", safe.replace('"', "\"\""))
}

pub fn csv(report: &Report) -> String {
    let mut out=String::from("run_id,at,repo,pr,head_sha,status,review_id,model,elapsed_ms,cost_usd,guide_hash,build_sha,source\n");
    for r in &report.records {
        let cols = [
            r.run_id.clone(),
            r.at.clone(),
            r.repo.clone(),
            r.pr.to_string(),
            r.head_sha.clone(),
            r.status.clone(),
            r.review_id.map(|v| v.to_string()).unwrap_or_default(),
            r.model.clone(),
            if r.timing_available || (r.source == "engine" && r.elapsed_ms > 0) {
                r.elapsed_ms.to_string()
            } else {
                String::new()
            },
            r.cost_usd.map(|v| v.to_string()).unwrap_or_default(),
            r.guide_hash.clone(),
            r.build_sha.clone(),
            r.source.clone(),
        ];
        out.push_str(
            &cols
                .iter()
                .map(|s| csv_cell(s))
                .collect::<Vec<_>>()
                .join(","),
        );
        out.push('\n');
    }
    out
}

/// Legacy snapshots are deliberately labelled incomplete; no IDs/costs are invented.
fn activity_records(source: &Path) -> Result<Vec<Record>> {
    let entries: Vec<crate::state::ActivityEntry> =
        serde_json::from_slice(&std::fs::read(source)?)?;
    let mut records = Vec::new();
    for e in entries {
        let (Some(repo), Some(pr)) = (e.repo, e.pr_number) else {
            continue;
        };
        if e.detail.is_none() {
            continue;
        }
        let identity = format!("{}|{repo}|{pr}|{}", e.timestamp, e.message);
        let status = if e.message.contains("이전 리뷰 있음") {
            "legacy_reapproval"
        } else if e.message.contains("리뷰 미완료") {
            "legacy_failed"
        } else {
            "legacy_review"
        };
        records.push(Record {
            run_id: format!("legacy:{}", fingerprint(&identity)),
            at: e.timestamp.to_rfc3339(),
            started_at: e.timestamp.to_rfc3339(),
            repo,
            pr,
            source: "legacy_activity_incomplete".into(),
            status: status.into(),
            result: Some(serde_json::json!({"message":e.message,"detail":e.detail})),
            ..Record::default()
        });
    }
    Ok(records)
}

pub fn run_cli(args: &[String]) -> Result<String> {
    let mut dir = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .ok_or_else(|| anyhow!("HOME unavailable"))?
        .join("Library/Application Support/com.approvebot.app");
    let mut date = None;
    let mut timezone = "+09:00".to_string();
    let mut format = "json".to_string();
    let mut import = None;
    let mut github = None;
    let mut me = None;
    let mut i = 0;
    while i < args.len() {
        let key = &args[i];
        i += 1;
        let value = args.get(i).ok_or_else(|| anyhow!("{key} needs a value"))?;
        i += 1;
        match key.as_str() {
            "--dir" => dir = value.into(),
            "--date" => date = Some(value.clone()),
            "--timezone" => timezone = value.clone(),
            "--format" => format = value.clone(),
            "--import-activity" => import = Some(std::path::PathBuf::from(value)),
            "--import-github" => github = Some(std::path::PathBuf::from(value)),
            "--me" => me = Some(value.clone()),
            _ => return Err(anyhow!("unknown flag {key}")),
        }
    }
    let date = date.ok_or_else(|| anyhow!("--date YYYY-MM-DD is required"))?;
    NaiveDate::parse_from_str(&date, "%Y-%m-%d")?;
    offset(&timezone)?;
    if !matches!(format.as_str(), "json" | "csv") {
        return Err(anyhow!("--format must be json or csv"));
    }
    if github.is_some() && me.as_deref().is_none_or(|m| m.trim().is_empty()) {
        return Err(anyhow!("--import-github requires --me LOGIN"));
    }
    if github.is_none() && me.is_some() {
        return Err(anyhow!("--me requires --import-github"));
    }
    // Parse every source before the first write so invalid arguments or input
    // cannot leave half of a requested import behind.
    let mut imported = Vec::new();
    if let Some(source) = import {
        imported.extend(activity_records(&source)?);
    }
    if let Some(source) = github {
        let input: serde_json::Value = serde_json::from_slice(&std::fs::read(source)?)?;
        imported.extend(crate::audit_import::github_records(
            &input,
            me.as_deref().unwrap(),
        )?);
    }
    let written = append_batch(&dir, &imported, true)?;
    if !imported.is_empty() {
        eprintln!(
            "Imported {written} new or updated records; execution metadata may be incomplete"
        );
    }
    let r = report(&dir, &date, &timezone)?;
    if r.corrupt_lines > 0 {
        eprintln!("Warning: {} malformed audit lines skipped", r.corrupt_lines);
    }
    if format == "csv" {
        Ok(csv(&r))
    } else {
        Ok(serde_json::to_string_pretty(&r)?)
    }
}


/// Period metrics over the audit journal: what the bot published, what it
/// held back, and how readable the bodies were. Built from the same
/// latest-record fold as the daily report so the two never disagree on
/// which records exist. Everything here is a count over stored records; it
/// does not call GitHub or Weave.
#[derive(Debug, Default, Serialize)]
pub struct Metrics {
    pub from: String,
    pub to: String,
    pub timezone: String,
    pub corrupt_lines: usize,
    pub total: MetricBucket,
    /// ISO week (`2026-W43`) in the requested timezone → bucket.
    pub weeks: BTreeMap<String, MetricBucket>,
    pub models: BTreeMap<String, MetricBucket>,
}

#[derive(Debug, Default, Serialize)]
pub struct MetricBucket {
    pub runs: usize,
    pub statuses: BTreeMap<String, usize>,
    /// `approved` + `commented`: reviews that reached GitHub as a review object.
    pub published: usize,
    pub approved: usize,
    /// Approvals whose result carried no finding at all.
    pub approved_without_findings: usize,
    /// Approvals that still carried a major finding with confidence ≥ 80.
    pub approved_with_confident_major: usize,
    pub commented: usize,
    /// Model failures that were posted to the PR as a review.
    pub failure_notices_posted: usize,
    /// Model failures kept local (`model_failed`) plus posted ones, by kind.
    pub failures_by_kind: BTreeMap<String, usize>,
    pub findings: usize,
    pub findings_by_severity: BTreeMap<String, usize>,
    /// Findings with confidence below 70 never get an inline thread.
    pub findings_below_inline_confidence: usize,
    /// Model bodies with more than one `# 검증 근거` heading.
    pub bodies_with_duplicate_evidence: usize,
    /// Model bodies that still wrote a `# 이전 지적 처리` or `# 총평` heading.
    pub bodies_with_legacy_headings: usize,
    pub body_chars_median: Option<u64>,
    /// Length of the body as posted (gate line, table, folds); only for
    /// records written after the field existed.
    pub posted_chars_median: Option<u64>,
    pub posted_details_median: Option<u64>,
    pub known_cost_usd: f64,
    pub unknown_cost_runs: usize,
    pub elapsed_ms_median: Option<u64>,
    #[serde(skip)]
    body_chars: Vec<u64>,
    #[serde(skip)]
    posted_chars: Vec<u64>,
    #[serde(skip)]
    posted_details: Vec<u64>,
    #[serde(skip)]
    elapsed: Vec<u64>,
}

/// Coarse class of an engine error, for backoff policy and the metrics table.
pub fn failure_kind(error: &str) -> &'static str {
    let text = error.to_ascii_lowercase();
    if text.contains("weekly limit") || text.contains("usage limit") || (text.contains("hit your") && text.contains("limit")) {
        "usage_limit"
    } else if text.contains("oauth") || text.contains("authenticate") || text.contains("login") {
        "auth"
    } else if text.contains("timed out") || text.contains("timeout") {
        "timeout"
    } else if text.contains("파싱") || text.contains("parse") {
        "parse"
    } else {
        "other"
    }
}

fn median(values: &mut Vec<u64>) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    Some(values[values.len() / 2])
}

fn heading_count(body: &str, heading: &str) -> usize {
    body.lines()
        .filter(|l| l.trim_end() == heading || l.starts_with(&format!("{heading} ")) || l.starts_with(&format!("{heading}—")))
        .count()
}

impl MetricBucket {
    fn add(&mut self, r: &Record) {
        self.runs += 1;
        *self.statuses.entry(r.status.clone()).or_default() += 1;
        let posted = matches!(r.status.as_str(), "approved" | "commented");
        if posted {
            self.published += 1;
        }
        let findings: Vec<&serde_json::Value> = r
            .result
            .as_ref()
            .and_then(|v| v.get("findings"))
            .and_then(|f| f.as_array())
            .map(|a| a.iter().collect())
            .unwrap_or_default();
        let confident_major = findings.iter().any(|f| {
            f.get("severity").and_then(|s| s.as_str()) == Some("major")
                && f.get("confidence").and_then(|c| c.as_f64()).unwrap_or(0.0) >= 80.0
        });
        match r.status.as_str() {
            "approved" => {
                self.approved += 1;
                if findings.is_empty() {
                    self.approved_without_findings += 1;
                }
                if confident_major {
                    self.approved_with_confident_major += 1;
                }
            }
            "commented" => self.commented += 1,
            "failure_notice" => {
                self.failure_notices_posted += 1;
                *self.failures_by_kind.entry(failure_kind(r.error.as_deref().unwrap_or_else(|| body_of(r))).into()).or_default() += 1;
            }
            "model_failed" => {
                *self.failures_by_kind.entry(failure_kind(r.error.as_deref().unwrap_or_else(|| body_of(r))).into()).or_default() += 1;
            }
            _ => {}
        }
        if posted {
            for f in &findings {
                self.findings += 1;
                let sev = f.get("severity").and_then(|s| s.as_str()).unwrap_or("unknown");
                *self.findings_by_severity.entry(sev.into()).or_default() += 1;
                if f.get("confidence").and_then(|c| c.as_f64()).unwrap_or(0.0) < 70.0 {
                    self.findings_below_inline_confidence += 1;
                }
            }
            let body = body_of(r);
            if !body.is_empty() {
                self.body_chars.push(body.chars().count() as u64);
                if heading_count(body, "# 검증 근거") > 1 {
                    self.bodies_with_duplicate_evidence += 1;
                }
                if heading_count(body, "# 이전 지적 처리") > 0 || heading_count(body, "# 총평") > 0 {
                    self.bodies_with_legacy_headings += 1;
                }
            }
            if let Some(n) = r.result.as_ref().and_then(|v| v.get("posted_len")).and_then(|n| n.as_u64()) {
                self.posted_chars.push(n);
            }
            if let Some(n) = r.result.as_ref().and_then(|v| v.get("posted_details")).and_then(|n| n.as_u64()) {
                self.posted_details.push(n);
            }
            if r.timing_available && r.elapsed_ms > 0 {
                self.elapsed.push(r.elapsed_ms);
            }
            match r.cost_usd.filter(|c| c.is_finite() && *c >= 0.0) {
                Some(c) => self.known_cost_usd += c,
                None => self.unknown_cost_runs += 1,
            }
        }
    }
    fn finish(&mut self) {
        self.body_chars_median = median(&mut self.body_chars);
        self.posted_chars_median = median(&mut self.posted_chars);
        self.posted_details_median = median(&mut self.posted_details);
        self.elapsed_ms_median = median(&mut self.elapsed);
    }
}

fn body_of(r: &Record) -> &str {
    r.result
        .as_ref()
        .and_then(|v| v.get("body"))
        .and_then(|b| b.as_str())
        .unwrap_or("")
}

pub fn metrics(dir: &Path, from: &str, to: &str, timezone: &str) -> Result<Metrics> {
    let start = NaiveDate::parse_from_str(from, "%Y-%m-%d")?;
    let end = NaiveDate::parse_from_str(to, "%Y-%m-%d")?;
    if end < start {
        return Err(anyhow!("--to must not be before --from"));
    }
    let tz = offset(timezone)?;
    let (publications, corrupt) = latest_publications(dir)?;
    let mut out = Metrics {
        from: from.into(),
        to: to.into(),
        timezone: timezone.into(),
        corrupt_lines: corrupt,
        ..Metrics::default()
    };
    for mut r in publications {
        let at = DateTime::parse_from_rfc3339(&r.at)?.with_timezone(&tz).date_naive();
        if at < start || at > end {
            continue;
        }
        r.timing_available = r.timing_available || (r.source == "engine" && r.elapsed_ms > 0);
        let week = at.iso_week();
        let week_key = format!("{}-W{:02}", week.year(), week.week());
        out.total.add(&r);
        out.weeks.entry(week_key).or_default().add(&r);
        let model = if r.model.trim().is_empty() { "none".to_string() } else { r.model.clone() };
        out.models.entry(model).or_default().add(&r);
    }
    out.total.finish();
    out.weeks.values_mut().for_each(MetricBucket::finish);
    out.models.values_mut().for_each(MetricBucket::finish);
    Ok(out)
}

fn pct(part: usize, whole: usize) -> String {
    if whole == 0 { "-".into() } else { format!("{:.0}%", part as f64 * 100.0 / whole as f64) }
}

fn opt(n: Option<u64>) -> String {
    n.map(|v| v.to_string()).unwrap_or_else(|| "-".into())
}

/// One markdown table, one row per bucket. Percentages are of the bucket's
/// published reviews (or approvals, for the approval columns).
pub fn metrics_markdown(m: &Metrics) -> String {
    let mut out = format!(
        "리뷰 지표 {}~{} ({})\n\n| 구간 | 게시 | 승인 | 지적 0건 승인 | 확신 major 승인 | 실패 게시 | 모델 실패 | 지적 | question | 검증근거 중복 | 옛 제목 | 본문 중앙값 | 게시 본문 중앙값 | 비용 $ |\n|---|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|\n",
        m.from, m.to, m.timezone
    );
    let row = |name: &str, b: &MetricBucket| {
        let failed: usize = b.failures_by_kind.values().sum();
        let failed_text = if b.failures_by_kind.is_empty() {
            "0".to_string()
        } else {
            format!("{failed} ({})", b.failures_by_kind.iter().map(|(k, v)| format!("{k} {v}")).collect::<Vec<_>>().join(", "))
        };
        format!(
            "| {name} | {} | {} | {} ({}) | {} | {} | {} | {} | {} ({}) | {} | {} | {} | {} | {:.2} |\n",
            b.published,
            b.approved,
            b.approved_without_findings,
            pct(b.approved_without_findings, b.approved),
            b.approved_with_confident_major,
            b.failure_notices_posted,
            failed_text,
            b.findings,
            b.findings_by_severity.get("question").copied().unwrap_or(0),
            pct(b.findings_by_severity.get("question").copied().unwrap_or(0), b.findings),
            b.bodies_with_duplicate_evidence,
            b.bodies_with_legacy_headings,
            opt(b.body_chars_median),
            opt(b.posted_chars_median),
            b.known_cost_usd,
        )
    };
    out.push_str(&row("전체", &m.total));
    for (k, b) in &m.weeks {
        out.push_str(&row(k, b));
    }
    for (k, b) in &m.models {
        out.push_str(&row(&format!("모델 {k}"), b));
    }
    if m.corrupt_lines > 0 {
        out.push_str(&format!("\n손상된 줄 {}개는 건너뜀.\n", m.corrupt_lines));
    }
    out
}

pub fn run_metrics_cli(args: &[String]) -> Result<String> {
    let mut dir = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .ok_or_else(|| anyhow!("HOME unavailable"))?
        .join("Library/Application Support/com.approvebot.app");
    let mut from = None;
    let mut to = None;
    let mut timezone = "+09:00".to_string();
    let mut format = "md".to_string();
    let mut i = 0;
    while i < args.len() {
        let key = &args[i];
        i += 1;
        let value = args.get(i).ok_or_else(|| anyhow!("{key} needs a value"))?;
        i += 1;
        match key.as_str() {
            "--dir" => dir = value.into(),
            "--from" => from = Some(value.clone()),
            "--to" => to = Some(value.clone()),
            "--timezone" => timezone = value.clone(),
            "--format" => format = value.clone(),
            _ => return Err(anyhow!("unknown flag {key}")),
        }
    }
    let from = from.ok_or_else(|| anyhow!("--from YYYY-MM-DD is required"))?;
    let to = to.ok_or_else(|| anyhow!("--to YYYY-MM-DD is required"))?;
    if !matches!(format.as_str(), "json" | "md") {
        return Err(anyhow!("--format must be json or md"));
    }
    let m = metrics(&dir, &from, &to, &timezone)?;
    if format == "json" {
        Ok(serde_json::to_string_pretty(&m)?)
    } else {
        Ok(metrics_markdown(&m))
    }
}

impl Record {
    /// Remember how the review looked on GitHub: length and number of folds of
    /// the body actually posted, so readability can be measured without
    /// storing a second copy of the body.
    pub fn note_posted(&mut self, posted_body: &str) {
        let result = self.result.get_or_insert_with(|| serde_json::json!({}));
        if let Some(obj) = result.as_object_mut() {
            obj.insert("posted_len".into(), serde_json::json!(posted_body.chars().count()));
            obj.insert("posted_details".into(), serde_json::json!(posted_body.matches("<details>").count()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn dir() -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "audit-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
    fn row(id: &str, at: &str) -> Record {
        Record {
            run_id: id.into(),
            at: at.into(),
            started_at: at.into(),
            source: "engine".into(),
            status: "started".into(),
            ..Record::default()
        }
    }
    #[test]
    fn history_survives_ui_limit_and_updates_do_not_double_count() {
        let p = dir();
        for i in 0..205 {
            append(&p, &row(&i.to_string(), "2026-10-14T15:00:00Z")).unwrap();
        }
        let mut r = row("0", "2026-10-14T15:01:00Z");
        r.status = "approved".into();
        r.cost_usd = Some(0.5);
        append(&p, &r).unwrap();
        let report = report(&p, "2026-10-15", "+09:00").unwrap();
        assert_eq!(report.records.len(), 205);
        assert_eq!(report.statuses["approved"], 1);
        assert_eq!(report.known_cost_usd, 0.5);
        assert_eq!(report.unknown_cost_runs, 204);
        std::fs::remove_dir_all(p).unwrap();
    }
    #[test]
    fn local_day_boundary_and_corruption_are_explicit() {
        let p = dir();
        append(&p, &row("before", "2026-10-14T14:59:59Z")).unwrap();
        append(&p, &row("start", "2026-10-14T15:00:00Z")).unwrap();
        use std::io::Write;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(p.join("review-audit.jsonl"))
            .unwrap()
            .write_all(b"{broken")
            .unwrap();
        append(&p, &row("after", "2026-10-14T16:00:00Z")).unwrap();
        let r = report(&p, "2026-10-15", "+09:00").unwrap();
        assert_eq!(r.records.len(), 2);
        assert_eq!(r.corrupt_lines, 1);
        assert!(report(&p, "2026-02-30", "+09:00").is_err());
        assert!(report(&p, "2026-10-15", "+99:00").is_err());
        std::fs::remove_dir_all(p).unwrap();
    }
    #[test]
    fn imported_and_native_reviews_count_once_with_native_cost_and_repo_scoping() {
        let p = dir();
        let mut imported = row("github:7", "2026-10-14T01:00:00Z");
        imported.repo = "example/one".into();
        imported.pr = 1;
        imported.review_id = Some(7);
        imported.source = "github_import".into();
        imported.status = "commented".into();
        imported.finished_cleanly = Some(true);
        imported.finding_ids = vec!["a".into()];
        append(&p, &imported).unwrap();
        let mut native = imported.clone();
        native.run_id = "native".into();
        native.source = "engine".into();
        native.cost_usd = Some(1.25);
        native.elapsed_ms = 20;
        native.finding_ids = vec!["b".into()];
        append(&p, &native).unwrap();
        append(&p, &imported).unwrap();
        let mut another = imported.clone();
        another.repo = "example/two".into();
        append(&p, &another).unwrap();
        let r = report(&p, "2026-10-14", "UTC").unwrap();
        assert_eq!(r.records.len(), 2);
        assert_eq!(r.known_cost_usd, 1.25);
        assert_eq!(r.unknown_cost_runs, 1);
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["distinct_prs"], 2);
        assert_eq!(json["distinct_findings"], 3);
        assert_eq!(json["normal_reviews"], 2);
        assert_eq!(json["unknown_timing_runs"], 1);
        assert_eq!(json["sources"]["engine"], 1);
        std::fs::remove_dir_all(p).unwrap();
    }

    #[test]
    fn github_import_cli_is_repeatable_and_invalid_options_do_not_write() {
        let p = dir();
        let input = p.join("input.json");
        let output = p.join("audit");
        std::fs::write(&input,serde_json::json!({"repo":"example/repo","number":1,"reviews":[
            {"id":7,"user":{"login":"bot"},"state":"COMMENTED","body":"<!-- approve-bot:review -->","submitted_at":"2026-10-14T01:00:00Z"},
            {"id":8,"user":{"login":"bot"},"state":"COMMENTED","body":"<!-- approve-bot:review-failed -->","submitted_at":"2026-10-14T01:00:00Z"},
            {"id":9,"user":{"login":"bot"},"state":"APPROVED","body":"","submitted_at":"2026-10-14T01:00:00Z"}
        ],"comments":[]}).to_string()).unwrap();
        let args = vec![
            "--dir".into(),
            output.to_string_lossy().into(),
            "--date".into(),
            "2026-10-14".into(),
            "--import-github".into(),
            input.to_string_lossy().into(),
            "--me".into(),
            "bot".into(),
        ];
        let mut bad = args.clone();
        bad.extend(["--format".into(), "yaml".into()]);
        assert!(run_cli(&bad).is_err());
        assert!(!output.exists());
        let first: serde_json::Value = serde_json::from_str(&run_cli(&args).unwrap()).unwrap();
        let initial_size = std::fs::metadata(output.join("review-audit.jsonl"))
            .unwrap()
            .len();
        let second: serde_json::Value = serde_json::from_str(&run_cli(&args).unwrap()).unwrap();
        assert_eq!(first, second);
        assert_eq!(
            std::fs::metadata(output.join("review-audit.jsonl"))
                .unwrap()
                .len(),
            initial_size,
            "identical repeated imports must not grow the journal"
        );
        assert_eq!(first["records"].as_array().unwrap().len(), 3);
        assert_eq!(first["failed_reviews"], 1);
        assert_eq!(first["reapprovals"], 1);
        assert_eq!(first["normal_reviews"], 1);
        assert_eq!(first["unknown_timing_runs"], 3);
        assert_eq!(first["records"][0]["timing_available"], false);
        std::fs::remove_dir_all(p).unwrap();
    }

    #[test]
    fn csv_escapes_quotes_newlines_formulas_and_leaves_unknown_timing_blank() {
        let mut r = row("=SUM(1,2)", "2026-10-14T01:00:00Z");
        r.repo = "a/\"b\nc".into();
        r.source = "github_import".into();
        let text = csv(&Report {
            records: vec![r],
            ..Report::default()
        });
        assert!(text.contains("\"'=SUM(1,2)\""));
        assert!(text.contains("\"a/\"\"b\nc\""));
        let cells = text.lines().last().unwrap();
        assert!(
            !cells.contains("\"0\",\"\",\"\",\"\",\"github_import\""),
            "unknown elapsed time must not be measured zero"
        );
    }
    #[test]
    fn native_bare_approval_metadata_wins_over_imported_approval() {
        for source in ["approval_only", "reapproval"] {
            let p = dir();
            let mut imported = row("github:7", "2026-10-14T01:00:00Z");
            imported.repo = "example/repo".into();
            imported.pr = 1;
            imported.review_id = Some(7);
            imported.source = "github_import".into();
            imported.status = "reapproval".into();
            append(&p, &imported).unwrap();
            let mut native = imported.clone();
            native.run_id = "zz-native".into();
            native.source = source.into();
            native.cost_usd = Some(0.0);
            native.timing_available = true;
            append(&p, &native).unwrap();
            let report = report(&p, "2026-10-14", "UTC").unwrap();
            assert_eq!(report.records.len(), 1);
            assert_eq!(report.records[0].source, source);
            assert_eq!(report.unknown_cost_runs, 0);
            assert_eq!(report.unknown_timing_runs, 0);
            std::fs::remove_dir_all(p).unwrap();
        }
    }

    fn published(id: &str, at: &str, status: &str, result: serde_json::Value) -> Record {
        let mut r = row(id, at);
        r.repo = "example/repo".into();
        r.pr = 1;
        r.status = status.into();
        r.model = "claude-opus-5-5".into();
        r.finished_cleanly = Some(status != "failure_notice" && status != "model_failed");
        r.result = Some(result);
        r
    }
    #[test]
    fn metrics_count_zero_finding_approvals_failure_notices_and_duplicate_sections() {
        let p = dir();
        append(&p, &published("a", "2026-10-20T01:00:00Z", "approved", serde_json::json!({"body":"# 검증 근거\n- ✅ x","findings":[]}))).unwrap();
        append(&p, &published("b", "2026-10-20T02:00:00Z", "approved", serde_json::json!({"body":"# 검증 근거\n- ✅ x\n# 추가 리뷰 (1) — 앞선 리뷰가 보지 못한 파일\n# 검증 근거\n- ✅ y",
            "findings":[{"severity":"major","confidence":85.0},{"severity":"question","confidence":60.0}],"posted_len":1200,"posted_details":3}))).unwrap();
        append(&p, &published("c", "2026-10-20T03:00:00Z", "commented", serde_json::json!({"body":"# 총평\n좋음\n# 이전 지적 처리 — 해소 0 · 부분 0 · 미해소 0 · 철회 0\n# 검증 근거\n- ✅","findings":[{"severity":"minor","confidence":90.0}]}))).unwrap();
        let mut fail = published("d", "2026-10-20T04:00:00Z", "failure_notice", serde_json::json!({"body":"claude 엔진 오류: You've hit your weekly limit · resets 12am (Asia/Seoul)"}));
        fail.model = "claude-sonnet-5-5".into();
        append(&p, &fail).unwrap();
        let mut local = published("e", "2026-10-20T05:00:00Z", "model_failed", serde_json::json!({}));
        local.error = Some("claude 엔진 오류: OAuth session expired".into());
        append(&p, &local).unwrap();
        // A superseded status of the same run must not count twice.
        let mut again = published("a", "2026-10-20T01:00:01Z", "approved", serde_json::json!({"body":"# 검증 근거\n- ✅ x","findings":[]}));
        again.cost_usd = Some(0.5);
        append(&p, &again).unwrap();
        // Outside the range.
        append(&p, &published("z", "2026-10-29T01:00:00Z", "approved", serde_json::json!({"findings":[]}))).unwrap();

        let m = metrics(&p, "2026-10-19", "2026-10-25", "+09:00").unwrap();
        let t = &m.total;
        assert_eq!(t.runs, 5);
        assert_eq!(t.published, 3);
        assert_eq!(t.approved, 2);
        assert_eq!(t.approved_without_findings, 1);
        assert_eq!(t.approved_with_confident_major, 1);
        assert_eq!(t.commented, 1);
        assert_eq!(t.failure_notices_posted, 1);
        assert_eq!(t.failures_by_kind["usage_limit"], 1);
        assert_eq!(t.failures_by_kind["auth"], 1);
        assert_eq!(t.findings, 3);
        assert_eq!(t.findings_by_severity["question"], 1);
        assert_eq!(t.findings_below_inline_confidence, 1);
        assert_eq!(t.bodies_with_duplicate_evidence, 1);
        assert_eq!(t.bodies_with_legacy_headings, 1);
        assert_eq!(t.posted_chars_median, Some(1200));
        assert_eq!(t.posted_details_median, Some(3));
        assert_eq!(t.known_cost_usd, 0.5);
        assert_eq!(t.unknown_cost_runs, 2);
        assert_eq!(m.models["claude-sonnet-5-5"].failure_notices_posted, 1);
        assert_eq!(m.models["claude-opus-5-5"].published, 3);
        let md = metrics_markdown(&m);
        assert!(md.contains("| 전체 | 3 | 2 | 1 (50%) | 1 | 1 | 2 (auth 1, usage_limit 1) | 3 | 1 (33%) | 1 | 1 |"), "{md}");
        let json = serde_json::to_value(&m).unwrap();
        assert!(json["total"].get("body_chars").is_none(), "scratch vectors are not serialized");
        std::fs::remove_dir_all(p).unwrap();
    }
    #[test]
    fn metrics_group_by_iso_week_in_the_given_timezone() {
        let p = dir();
        // Sunday 23:59 KST and Monday 00:00 KST straddle the ISO week boundary.
        append(&p, &published("sun", "2026-10-18T14:59:59Z", "approved", serde_json::json!({"findings":[]}))).unwrap();
        append(&p, &published("mon", "2026-10-18T15:00:00Z", "approved", serde_json::json!({"findings":[]}))).unwrap();
        let m = metrics(&p, "2026-10-01", "2026-10-31", "+09:00").unwrap();
        assert_eq!(m.weeks["2026-W42"].approved, 1);
        assert_eq!(m.weeks["2026-W43"].approved, 1);
        let utc = metrics(&p, "2026-10-01", "2026-10-31", "UTC").unwrap();
        assert_eq!(utc.weeks["2026-W42"].approved, 2);
        assert!(metrics(&p, "2026-10-31", "2026-10-01", "UTC").is_err());
        std::fs::remove_dir_all(p).unwrap();
    }
    #[test]
    fn failure_kinds_and_posted_note() {
        assert_eq!(failure_kind("claude 엔진 오류: You've hit your weekly limit · resets 12am (Asia/Seoul)"), "usage_limit");
        assert_eq!(failure_kind("claude 엔진 오류: OAuth session expired"), "auth");
        assert_eq!(failure_kind("claude 실행 실패: Request timed out"), "timeout");
        assert_eq!(failure_kind("claude 출력 파싱 실패: EOF"), "parse");
        assert_eq!(failure_kind("boom"), "other");
        let mut r = row("x", "2026-10-20T01:00:00Z");
        r.note_posted("a\n<details>b</details>\n<details>c</details>");
        assert_eq!(r.result.as_ref().unwrap()["posted_len"], 43);
        assert_eq!(r.result.as_ref().unwrap()["posted_details"], 2);
    }
    #[test]
    fn metrics_cli_reads_a_directory_and_formats_markdown() {
        let p = dir();
        append(&p, &published("a", "2026-10-20T01:00:00Z", "approved", serde_json::json!({"findings":[]}))).unwrap();
        let args: Vec<String> = ["--dir", p.to_str().unwrap(), "--from", "2026-10-20", "--to", "2026-10-20", "--timezone", "UTC"].iter().map(|s| s.to_string()).collect();
        let md = run_metrics_cli(&args).unwrap();
        assert!(md.starts_with("리뷰 지표 2026-10-20~2026-10-20 (UTC)"), "{md}");
        let mut json_args = args.clone();
        json_args.extend(["--format".into(), "json".into()]);
        let v: serde_json::Value = serde_json::from_str(&run_metrics_cli(&json_args).unwrap()).unwrap();
        assert_eq!(v["total"]["approved"], 1);
        assert!(run_metrics_cli(&["--from".into(), "2026-10-20".into()]).is_err());
        std::fs::remove_dir_all(p).unwrap();
    }
}
