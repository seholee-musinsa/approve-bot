//! Daily repo sweep, step 1: static triage.
//!
//! Before any model reads a line, cheap and exact checks narrow a large repo to
//! where review is worth paying for: big files, suppression and TODO markers,
//! a few rule violations that can be matched on text, and recent churn. Files
//! are grouped into slices of reviewable size and ranked by risk.
//!
//! Nothing here calls GitHub or a model. `approve-bot sweep-once --local <dir>`
//! prints the ranking for a checkout; the output quotes paths from a private
//! repo, so it is not meant to be committed (public repo).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;

/// A file this long is a split candidate on its own.
pub const BIG_FILE_LINES: usize = 800;
/// Lines one slice may hold before it is split by sub-directory.
pub const SLICE_MAX_LINES: usize = 25_000;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Markers {
    /// `eslint-disable`, `biome-ignore`, `@ts-expect-error`.
    pub suppressions: u32,
    pub any_casts: u32,
    /// `TODO` and `FIXME` (a bare `NOTE` is not debt).
    pub todos: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub rule: &'static str,
    pub line: usize,
}

#[derive(Debug, Clone)]
pub struct FileStat {
    pub path: String,
    pub lines: usize,
    pub markers: Markers,
    pub violations: Vec<Violation>,
    /// Commits touching the file in the churn window.
    pub churn: u32,
}

/// Source worth reviewing: TypeScript that people write and read.
pub fn is_source(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    if !(name.ends_with(".ts") || name.ends_with(".tsx")) || name.ends_with(".d.ts") {
        return false;
    }
    if [".gen.", ".test.", ".spec.", ".stories.", ".interactions."].iter().any(|m| name.contains(m)) {
        return false;
    }
    const SKIP: &[&str] = &["node_modules/", "/.next/", "/dist/", "/e2e/", "__tests__/", "__mocks__/", "storybook-static/"];
    !SKIP.iter().any(|s| path.contains(s)) && !path.starts_with("dist/") && !path.starts_with(".next/")
}

fn layer_rank(path: &str) -> Option<u8> {
    ["layers/apis/", "layers/services/", "layers/features/", "layers/apps/"]
        .iter()
        .position(|p| path.starts_with(p))
        .map(|i| i as u8)
}

fn import_rank(line: &str) -> Option<u8> {
    let from = line.split("from '").nth(1).or_else(|| line.split("from \"").nth(1))?;
    // `services-` is also a shared-package prefix, so only the unambiguous
    // layers count: `features-` and `apps-` exist only as layers.
    if from.starts_with("@musinsa/features-") {
        Some(2)
    } else if from.starts_with("@musinsa/apps-") {
        Some(3)
    } else {
        None
    }
}

/// A class name token that starts with `mcds:` (the prefix is for packages/mcds only).
fn has_mcds_prefix(line: &str) -> bool {
    line.match_indices("mcds:").any(|(i, _)| {
        i == 0 || matches!(line.as_bytes()[i - 1], b' ' | b'"' | b'\'' | b'`')
    })
}

pub fn scan_file(path: &str, content: &str) -> (usize, Markers, Vec<Violation>) {
    let in_service_app = path.starts_with("layers/features/") || path.starts_with("layers/apps/");
    let rank = layer_rank(path);
    let mut m = Markers::default();
    let mut v = Vec::new();
    let mut lines = 0;
    for (i, line) in content.lines().enumerate() {
        lines += 1;
        let n = i + 1;
        if line.contains("eslint-disable") || line.contains("biome-ignore") || line.contains("@ts-expect-error") {
            m.suppressions += 1;
        }
        if line.contains(" as any") || line.contains("<any>") {
            m.any_casts += 1;
        }
        if line.contains("TODO") || line.contains("FIXME") {
            m.todos += 1;
        }
        if in_service_app && has_mcds_prefix(line) {
            v.push(Violation { rule: "mcds-prefix", line: n });
        }
        if line.contains("amazonaws.com") && !line.contains("dkr.ecr") {
            v.push(Violation { rule: "s3-origin", line: n });
        }
        if line.contains("public-read") || line.contains("x-amz-acl") {
            v.push(Violation { rule: "public-acl", line: n });
        }
        if let (Some(r), Some(imp)) = (rank, import_rank(line)) {
            if imp > r {
                v.push(Violation { rule: "upward-import", line: n });
            }
        }
    }
    (lines, m, v)
}

/// Weights for the risk score. Tuned by hand; kept in one place so a later
/// change is a one-line edit (and, later, a setting).
#[derive(Debug, Clone, Copy)]
pub struct Weights {
    /// Per 100 lines above the big-file threshold, capped at `size_cap`.
    pub size: f64,
    pub size_cap: f64,
    /// Per commit in the churn window, capped at `churn_cap` commits.
    pub churn: f64,
    pub churn_cap: u32,
    pub marker: f64,
    pub violation: f64,
}

impl Default for Weights {
    fn default() -> Self {
        Self { size: 1.0, size_cap: 15.0, churn: 0.5, churn_cap: 40, marker: 0.3, violation: 3.0 }
    }
}

pub fn score(f: &FileStat, w: &Weights) -> f64 {
    let over = f.lines.saturating_sub(BIG_FILE_LINES / 2) as f64 / 100.0;
    let size = (over * w.size).clamp(0.0, w.size_cap);
    let churn = f.churn.min(w.churn_cap) as f64 * w.churn;
    // A big file that also changes a lot is the likeliest refactor target.
    let hot = if f.lines >= BIG_FILE_LINES && f.churn >= 10 { 5.0 } else { 0.0 };
    let markers = (f.markers.suppressions + f.markers.any_casts + f.markers.todos) as f64 * w.marker;
    let violations = f.violations.len() as f64 * w.violation;
    size + churn + hot + markers + violations
}

#[derive(Debug, Clone)]
pub struct Slice {
    pub name: String,
    pub files: Vec<String>,
    pub lines: usize,
    pub score: f64,
}

/// Directory at `depth` components, e.g. `layers/features/products` at 3.
fn dir_key(path: &str, depth: usize) -> String {
    let parts: Vec<&str> = path.split('/').collect();
    let take = depth.min(parts.len().saturating_sub(1)).max(1);
    parts[..take].join("/")
}

fn base_depth(path: &str) -> usize {
    if path.starts_with("layers/") {
        3
    } else {
        2
    }
}

/// Group files into slices of at most `max_lines`. A directory that does not
/// fit is split by its next path component; a single file larger than the
/// limit is a slice of its own. Small directories are merged afterwards.
pub fn pack_slices(files: &[FileStat], w: &Weights, max_lines: usize) -> Vec<Slice> {
    let mut by_dir: BTreeMap<String, Vec<&FileStat>> = BTreeMap::new();
    for f in files {
        by_dir.entry(dir_key(&f.path, base_depth(&f.path))).or_default().push(f);
    }
    let mut pieces: Vec<Slice> = Vec::new();
    for (dir, group) in by_dir {
        split_group(&dir, base_depth(&group[0].path), group, w, max_lines, &mut pieces);
    }
    merge_small(pieces, max_lines)
}

fn make_slice(name: &str, group: &[&FileStat], w: &Weights) -> Slice {
    Slice {
        name: name.to_string(),
        files: group.iter().map(|f| f.path.clone()).collect(),
        lines: group.iter().map(|f| f.lines).sum(),
        score: group.iter().map(|f| score(f, w)).sum(),
    }
}

fn split_group(name: &str, depth: usize, group: Vec<&FileStat>, w: &Weights, max_lines: usize, out: &mut Vec<Slice>) {
    let total: usize = group.iter().map(|f| f.lines).sum();
    let can_split = group.iter().any(|f| f.path.split('/').count() > depth + 1);
    if total <= max_lines {
        out.push(make_slice(name, &group, w));
        return;
    }
    if !can_split {
        // No sub-directory left: cut the files into runs that fit. A file over
        // the limit on its own is a slice named by its path.
        let mut files: Vec<&FileStat> = group;
        files.sort_by(|a, b| a.path.cmp(&b.path));
        let mut run: Vec<&FileStat> = Vec::new();
        let mut run_lines = 0;
        let mut n = 0;
        let mut flush = |run: &mut Vec<&FileStat>, run_lines: &mut usize, out: &mut Vec<Slice>| {
            if run.is_empty() {
                return;
            }
            n += 1;
            let label = if run.len() == 1 { run[0].path.clone() } else { format!("{name} ({n})") };
            out.push(make_slice(&label, run, w));
            run.clear();
            *run_lines = 0;
        };
        for f in files {
            if !run.is_empty() && run_lines + f.lines > max_lines {
                flush(&mut run, &mut run_lines, out);
            }
            run.push(f);
            run_lines += f.lines;
        }
        flush(&mut run, &mut run_lines, out);
        return;
    }
    // Files directly in this directory stay together; each sub-directory is its own group.
    let mut children: BTreeMap<String, Vec<&FileStat>> = BTreeMap::new();
    for f in group {
        children.entry(dir_key(&f.path, depth + 1)).or_default().push(f);
    }
    for (child, g) in children {
        if g.len() == 1 && g[0].lines > max_lines {
            out.push(make_slice(&g[0].path, &g, w));
        } else {
            split_group(&child, depth + 1, g, w, max_lines, out);
        }
    }
}

/// Greedily merge neighbouring small slices of the same layer so the day's
/// budget is not spent on a dozen three-file directories.
fn merge_small(mut pieces: Vec<Slice>, max_lines: usize) -> Vec<Slice> {
    pieces.sort_by(|a, b| a.name.cmp(&b.name));
    let layer = |s: &Slice| s.name.split('/').take(2).collect::<Vec<_>>().join("/");
    let mut out: Vec<Slice> = Vec::new();
    for p in pieces {
        match out.last_mut() {
            Some(last) if layer(last) == layer(&p) && last.lines + p.lines <= max_lines / 2 && p.lines <= max_lines / 4 => {
                last.name = format!("{} + {}", last.name, p.name.rsplit('/').next().unwrap_or(&p.name));
                last.files.extend(p.files);
                last.lines += p.lines;
                last.score += p.score;
            }
            _ => out.push(p),
        }
    }
    out
}

/// How slices are ordered. A day's cost grows with the lines a slice holds, so
/// `Density` (score per 1,000 lines) is the value per unit of work, while `Sum`
/// favours big slices simply because they hold more files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RankBy {
    Sum,
    Density,
}

/// Score per 1,000 lines. Slices under 1,000 lines count as 1,000 so a tiny slice
/// with one hit does not outrank real hot spots.
pub fn density(s: &Slice) -> f64 {
    s.score / (s.lines.max(1000) as f64) * 1000.0
}

/// Highest first; ties by name so the order is stable between runs.
pub fn rank(mut slices: Vec<Slice>, by: RankBy) -> Vec<Slice> {
    let key = |s: &Slice| match by {
        RankBy::Sum => s.score,
        RankBy::Density => density(s),
    };
    slices.sort_by(|a, b| key(b).partial_cmp(&key(a)).unwrap_or(std::cmp::Ordering::Equal).then(a.name.cmp(&b.name)));
    slices
}

// ---- reading a checkout ------------------------------------------------------

fn git(root: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new("git").args(args).current_dir(root).output()?;
    if !out.status.success() {
        return Err(anyhow::anyhow!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Commits per path over the last `days` days.
pub fn churn_by_path(root: &Path, days: u32) -> anyhow::Result<HashMap<String, u32>> {
    let since = format!("--since={days}.days");
    let log = git(root, &["log", &since, "--name-only", "--pretty=format:"])?;
    let mut m: HashMap<String, u32> = HashMap::new();
    for l in log.lines().filter(|l| !l.is_empty()) {
        *m.entry(l.to_string()).or_default() += 1;
    }
    Ok(m)
}

/// Scan the tracked source files of a checkout.
pub fn scan_checkout(root: &Path, churn: &HashMap<String, u32>) -> anyhow::Result<Vec<FileStat>> {
    let listed = git(root, &["ls-files", "-z"])?;
    let mut out = Vec::new();
    for path in listed.split('\0').filter(|p| !p.is_empty() && is_source(p)) {
        let Ok(content) = std::fs::read_to_string(root.join(path)) else { continue };
        let (lines, markers, violations) = scan_file(path, &content);
        out.push(FileStat { path: path.to_string(), lines, markers, violations, churn: churn.get(path).copied().unwrap_or(0) });
    }
    Ok(out)
}

/// How many tickets the static findings alone would make, before any model
/// judgement. An upper bound: the model review and the confidence filter only
/// remove candidates. Counted three ways so the grouping rule can be chosen
/// from numbers rather than guessed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Volume {
    /// Files with at least one candidate (a rule hit, a marker, or >= BIG_FILE_LINES).
    pub candidate_files: usize,
    pub rule_files: usize,
    pub rule_kinds: usize,
    pub suppression_files: usize,
    pub any_files: usize,
    pub todo_files: usize,
    pub big_files: usize,
    /// One ticket per candidate file.
    pub by_file: usize,
    /// rule and debt grouped by kind (cut into tickets of at most `max_files`
    /// files), split one ticket per big file.
    pub by_kind: usize,
    /// One ticket per slice that holds a candidate.
    pub by_slice: usize,
    /// Like `by_kind`, but grouped inside each slice. A day reads one slice, so
    /// this is what the daily volume actually adds up to.
    pub by_kind_in_slice: usize,
}

fn tickets_for(files: usize, max_files: usize) -> usize {
    files.div_ceil(max_files.max(1))
}

pub fn volume(files: &[FileStat], slices: &[Slice], max_files: usize) -> Volume {
    let mut v = Volume::default();
    let mut rules: Vec<&str> = Vec::new();
    let mut candidate: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for f in files {
        let is_big = f.lines >= BIG_FILE_LINES;
        let has_marker = f.markers.suppressions + f.markers.any_casts + f.markers.todos > 0;
        if !f.violations.is_empty() {
            v.rule_files += 1;
            for x in &f.violations {
                if !rules.contains(&x.rule) {
                    rules.push(x.rule);
                }
            }
        }
        v.suppression_files += (f.markers.suppressions > 0) as usize;
        v.any_files += (f.markers.any_casts > 0) as usize;
        v.todo_files += (f.markers.todos > 0) as usize;
        v.big_files += is_big as usize;
        if !f.violations.is_empty() || has_marker || is_big {
            candidate.insert(f.path.as_str());
        }
    }
    v.rule_kinds = rules.len();
    v.candidate_files = candidate.len();
    v.by_file = candidate.len();
    v.by_kind = tickets_for(v.rule_files, max_files)
        + tickets_for(v.suppression_files, max_files)
        + tickets_for(v.any_files, max_files)
        + tickets_for(v.todo_files, max_files)
        + v.big_files;
    v.by_slice = slices.iter().filter(|s| s.files.iter().any(|p| candidate.contains(p.as_str()))).count();
    let by_path: std::collections::HashMap<&str, &FileStat> = files.iter().map(|f| (f.path.as_str(), f)).collect();
    for sl in slices {
        let (mut rule, mut sup, mut any, mut todo, mut big) = (0, 0, 0, 0, 0);
        for f in sl.files.iter().filter_map(|p| by_path.get(p.as_str())) {
            rule += (!f.violations.is_empty()) as usize;
            sup += (f.markers.suppressions > 0) as usize;
            any += (f.markers.any_casts > 0) as usize;
            todo += (f.markers.todos > 0) as usize;
            big += (f.lines >= BIG_FILE_LINES) as usize;
        }
        v.by_kind_in_slice += tickets_for(rule, max_files) + tickets_for(sup, max_files) + tickets_for(any, max_files) + tickets_for(todo, max_files) + big;
    }
    v
}

fn volume_report(v: &Volume, max_files: usize) -> String {
    format!(
        "\n정적 후보 규모(모델 판단 전의 상한)\n\
         | 후보 | 파일 수 |\n|---|--:|\n\
         | 규칙 위반 | {} |\n| suppression(eslint-disable 등) | {} |\n| as any | {} |\n| TODO·FIXME | {} |\n| {}줄 이상 큰 파일 | {} |\n| 후보가 하나라도 있는 파일 | {} |\n\n\
         티켓 수 환산(묶음 방식별)\n| 묶음 방식 | 티켓 수 |\n|---|--:|\n\
         | 파일 단위 | {} |\n| 종류별, 전체를 한 번에 묶음(종류마다 최대 {max_files}개 파일씩, 큰 파일은 하나씩) | {} |\n| 종류별, 그날 읽은 조각 안에서 묶음(실제 하루 생성량의 합) | {} |\n| 조각 단위 | {} |\n",
        v.rule_files, v.suppression_files, v.any_files, v.todo_files, BIG_FILE_LINES, v.big_files, v.candidate_files,
        v.by_file, v.by_kind, v.by_kind_in_slice, v.by_slice
    )
}

/// A merged slice is named after every directory in it; the report shows the
/// first two and how many more.
fn short_name(name: &str) -> String {
    let parts: Vec<&str> = name.split(" + ").collect();
    if parts.len() <= 2 {
        return name.to_string();
    }
    format!("{} + {} 외 {}개", parts[0], parts[1], parts.len() - 2)
}

/// Plain-text report for `sweep-once`.
pub fn report(files: &[FileStat], slices: &[Slice], w: &Weights, top: usize) -> String {
    let total_lines: usize = files.iter().map(|f| f.lines).sum();
    let mut s = format!("소스 {}개 · {}줄 · 조각 {}개\n", files.len(), total_lines, slices.len());
    let mut by_rule: BTreeMap<&str, usize> = BTreeMap::new();
    for f in files {
        for v in &f.violations {
            *by_rule.entry(v.rule).or_default() += 1;
        }
    }
    if by_rule.is_empty() {
        s.push_str("규칙 위반: 없음\n");
    } else {
        let parts: Vec<String> = by_rule.iter().map(|(r, n)| format!("{r} {n}")).collect();
        s.push_str(&format!("규칙 위반(줄 수): {}\n", parts.join(" · ")));
    }
    s.push_str(&format!("\n위험 점수 상위 조각 {top}개\n| # | 조각 | 줄 | 파일 | 점수 | 천 줄당 |\n|--:|---|--:|--:|--:|--:|\n"));
    for (i, sl) in slices.iter().take(top).enumerate() {
        s.push_str(&format!("| {} | {} | {} | {} | {:.0} | {:.1} |\n", i + 1, short_name(&sl.name), sl.lines, sl.files.len(), sl.score, density(sl)));
    }
    let mut hot: Vec<&FileStat> = files.iter().collect();
    hot.sort_by(|a, b| score(b, w).partial_cmp(&score(a, w)).unwrap_or(std::cmp::Ordering::Equal).then(a.path.cmp(&b.path)));
    s.push_str(&format!("\n위험 점수 상위 파일 {top}개\n| # | 파일 | 줄 | 변경 | 마커 | 위반 | 점수 |\n|--:|---|--:|--:|--:|--:|--:|\n"));
    for (i, f) in hot.iter().take(top).enumerate() {
        let m = f.markers.suppressions + f.markers.any_casts + f.markers.todos;
        s.push_str(&format!("| {} | {} | {} | {} | {} | {} | {:.0} |\n", i + 1, f.path, f.lines, f.churn, m, f.violations.len(), score(f, w)));
    }
    s
}

/// `approve-bot sweep-once --local <dir> [--days 90] [--top 12] [--max-lines 25000] [--max-files 10] [--rank density|sum]`
/// `approve-bot sweep-once --repo owner/name [--cache-dir <dir>] [--clear-cache] ...` reads an app-owned cache clone (kept fresh, see `repocache`) instead of a local folder.
pub fn run_cli(flags: &[String]) -> anyhow::Result<String> {
    let mut local = String::new();
    let mut repo = String::new();
    let mut cache_root: Option<String> = None;
    let mut clear_cache = false;
    let mut days: u32 = 90;
    let mut top: usize = 12;
    let mut max_lines: usize = SLICE_MAX_LINES;
    let mut max_files: usize = 10;
    // Value per unit of work: a day's cost grows with the lines a slice holds.
    let mut by = RankBy::Density;
    let mut i = 0;
    while i < flags.len() {
        let name = flags[i].as_str();
        let value = |i: &mut usize| -> anyhow::Result<String> {
            *i += 1;
            flags.get(*i).cloned().ok_or_else(|| anyhow::anyhow!("{name} needs a value"))
        };
        match name {
            "--local" => local = value(&mut i)?,
            "--repo" => repo = value(&mut i)?,
            "--cache-dir" => cache_root = Some(value(&mut i)?),
            "--clear-cache" => clear_cache = true,
            "--days" => days = value(&mut i)?.parse()?,
            "--top" => top = value(&mut i)?.parse()?,
            "--max-lines" => max_lines = value(&mut i)?.parse()?,
            "--max-files" => max_files = value(&mut i)?.parse()?,
            "--rank" => {
                by = match value(&mut i)?.as_str() {
                    "sum" => RankBy::Sum,
                    "density" => RankBy::Density,
                    other => return Err(anyhow::anyhow!("--rank must be sum or density, got {other}")),
                }
            }
            other => return Err(anyhow::anyhow!("unknown flag: {other}")),
        }
        i += 1;
    }
    if !repo.is_empty() && !local.is_empty() {
        return Err(anyhow::anyhow!("--repo and --local cannot be used together"));
    }
    if repo.is_empty() && local.is_empty() {
        return Err(anyhow::anyhow!("--repo owner/name or --local <checkout dir> is required"));
    }
    let w = Weights::default();
    let mut notes = String::new();
    let files = if repo.is_empty() {
        let root = Path::new(&local);
        scan_checkout(root, &churn_by_path(root, days)?)?
    } else {
        let (owner, name) = crate::github::split_repo(&repo)?;
        let root = match &cache_root {
            Some(d) => PathBuf::from(d),
            None => crate::eval_cli::config_dir()?.join("sweep"),
        };
        let dir = crate::repocache::cache_dir(&root, owner, name);
        if clear_cache {
            crate::repocache::clear(&dir);
            return Ok(format!("캐시를 지웠다: {}", dir.display()));
        }
        notes.push_str(&refresh_cache(owner, name, &dir)?);
        let scan = |d: &Path| -> anyhow::Result<Vec<FileStat>> { scan_checkout(d, &churn_by_path(d, days)?) };
        match scan(&dir) {
            Ok(f) => f,
            // The sync was clean but the history cannot be read: the cache is
            // damaged somewhere the sync did not look. Rebuild it once.
            Err(e) if crate::repocache::is_corruption(&e.to_string()) => {
                notes.push_str("이력을 읽지 못해(캐시 손상) 캐시를 지우고 다시 받는다\n");
                crate::repocache::clear(&dir);
                notes.push_str(&refresh_cache(owner, name, &dir)?);
                scan(&dir)?
            }
            Err(e) => return Err(e),
        }
    };
    let slices = rank(pack_slices(&files, &w, max_lines), by);
    let mut out = notes;
    out.push_str(&report(&files, &slices, &w, top));
    out.push_str(&volume_report(&volume(&files, &slices, max_files), max_files));
    Ok(out)
}

/// Bring the cache clone of `owner/name` up to date and describe what happened:
/// every failed attempt with its cause and the fix tried, then the result.
fn refresh_cache(owner: &str, name: &str, dir: &Path) -> anyhow::Result<String> {
    use crate::repocache::{self, Policy, RealGit};
    let token = crate::auth::fetch_gh_token().map_err(|e| anyhow::anyhow!("GitHub 토큰을 얻지 못함(gh 로그인 필요): {e}"))?;
    let git = RealGit::new(Some(token.clone()));
    let url = format!("https://github.com/{owner}/{name}.git");
    let again = || crate::auth::fetch_gh_token().ok();
    let started = std::time::Instant::now();
    let mut log = Vec::new();
    let result = repocache::sync(&git, dir, &url, &Policy::default(), &again, Some(&token), &mut log);
    let mut text = String::new();
    for a in &log {
        text.push_str(&format!("갱신 시도 {} ({}): 원인 {} → {} [{}]\n", a.n, a.step, a.failure.label(), a.action, a.detail));
    }
    match result {
        Ok(ok) => {
            let mb = repocache::dir_size(dir) as f64 / 1_048_576.0;
            text.push_str(&format!(
                "캐시 {} · 기본 브랜치 {} · 기준 커밋 {} · {} {:.1}초 · 용량 {:.0}MB{}\n",
                dir.display(),
                ok.branch,
                ok.commit.chars().take(8).collect::<String>(),
                if ok.cloned { "처음 클론" } else { "갱신" },
                started.elapsed().as_secs_f64(),
                mb,
                if ok.shallow { " · ⚠️ 얕은 클론이라 변경 빈도가 0으로 나온다" } else { "" },
            ));
            Ok(text)
        }
        Err(e) => Err(anyhow::anyhow!("{text}{e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stat(path: &str, lines: usize, churn: u32) -> FileStat {
        FileStat { path: path.into(), lines, markers: Markers::default(), violations: vec![], churn }
    }

    #[test]
    fn only_hand_written_typescript_is_source() {
        assert!(is_source("layers/features/products/src/A.tsx"));
        assert!(is_source("packages/mcds/src/index.ts"));
        for p in [
            "layers/features/products/src/A.test.tsx",
            "layers/features/products/src/A.stories.tsx",
            "layers/apis/x/src/foo.gen.ts",
            "layers/apis/x/types.d.ts",
            "layers/apps/a/e2e/support/stub.ts",
            "layers/apps/a/.next/server/x.ts",
            "README.md",
            "layers/apps/a/next.config.js",
        ] {
            assert!(!is_source(p), "{p}");
        }
    }

    #[test]
    fn counts_markers_without_counting_plain_notes() {
        let src = "// NOTE: not debt\n// TODO: later\nconst a = x as any;\n// eslint-disable-next-line\n// biome-ignore lint: why\n// FIXME y\n";
        let (lines, m, v) = scan_file("layers/features/a/src/x.ts", src);
        assert_eq!(lines, 6);
        assert_eq!(m, Markers { suppressions: 2, any_casts: 1, todos: 2 });
        assert!(v.is_empty());
    }

    #[test]
    fn flags_each_text_matchable_rule_where_it_applies() {
        let class = "<span className=\"flex mcds:bg-blue-95\" />\n";
        let (_, _, v) = scan_file("layers/features/a/src/X.tsx", class);
        assert_eq!(v, vec![Violation { rule: "mcds-prefix", line: 1 }]);
        // The prefix is the package's own convention, so it is fine there.
        assert!(scan_file("packages/mcds/src/Tag.tsx", class).2.is_empty());
        // `mcds:` inside another token is not a class.
        assert!(scan_file("layers/features/a/src/X.tsx", "const k = 'xmcds:y';\n").2.is_empty());

        let (_, _, v) = scan_file("layers/apps/a/src/x.ts", "const u = 'https://b.s3.ap-northeast-2.amazonaws.com/k';\nconst a = { ACL: 'public-read' };\n");
        let rules: Vec<&str> = v.iter().map(|x| x.rule).collect();
        assert_eq!(rules, vec!["s3-origin", "public-acl"]);
        assert!(scan_file("a.ts", "image: 1.dkr.ecr.amazonaws.com/x\n").2.is_empty(), "ECR is a registry, not S3");
    }

    #[test]
    fn a_lower_layer_importing_a_higher_one_is_flagged_but_not_the_reverse() {
        let up = "import { A } from '@musinsa/features-curator';\n";
        assert_eq!(scan_file("layers/services/curator/src/s.ts", up).2, vec![Violation { rule: "upward-import", line: 1 }]);
        assert_eq!(scan_file("layers/apis/curator/src/a.ts", "import x from '@musinsa/apps-curator';\n").2.len(), 1);
        assert!(scan_file("layers/apps/curator/src/a.ts", up).2.is_empty(), "apps may use features");
        assert!(scan_file("layers/features/a/src/a.ts", "import { x } from '@musinsa/apis-mamud';\n").2.is_empty(), "type and schema imports are common, not a rule hit");
        assert!(scan_file("packages/x/src/a.ts", up).2.is_empty(), "packages have no layer");
    }

    #[test]
    fn score_rewards_big_hot_files_and_violations() {
        let w = Weights::default();
        let small_quiet = stat("layers/features/a/src/a.ts", 100, 0);
        let big_quiet = stat("layers/features/a/src/b.ts", 2000, 0);
        let big_hot = stat("layers/features/a/src/c.ts", 2000, 40);
        let mut bad = stat("layers/features/a/src/d.ts", 100, 0);
        bad.violations = vec![Violation { rule: "mcds-prefix", line: 1 }];
        assert!(score(&small_quiet, &w) < 0.01);
        assert!(score(&big_hot, &w) > score(&big_quiet, &w));
        assert!(score(&big_quiet, &w) > score(&small_quiet, &w));
        assert!(score(&bad, &w) >= 3.0);
        let capped = stat("a.ts", 1_000_000, 1000);
        assert!(score(&capped, &w) <= w.size_cap + 40.0 * w.churn + 5.0 + 0.001, "caps hold");
    }

    #[test]
    fn a_domain_over_the_limit_is_split_by_sub_directory_and_small_ones_merge() {
        let w = Weights::default();
        let files = vec![
            stat("layers/features/products/src/a/x.ts", 400, 0),
            stat("layers/features/products/src/a/y.ts", 400, 0),
            stat("layers/features/products/src/b/z.ts", 600, 0),
            stat("layers/features/tiny1/src/t.ts", 50, 0),
            stat("layers/features/tiny2/src/t.ts", 50, 0),
            stat("packages/mcds/src/m.ts", 100, 0),
        ];
        let slices = pack_slices(&files, &w, 1000);
        let names: Vec<&str> = slices.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"layers/features/products/src/a"), "{names:?}");
        assert!(names.contains(&"layers/features/products/src/b"), "{names:?}");
        assert!(slices.iter().any(|s| s.name.contains(" + ") && s.files.len() == 2), "the two tiny domains merge: {names:?}");
        let all: usize = slices.iter().map(|s| s.files.len()).sum();
        assert_eq!(all, files.len(), "every file lands in exactly one slice");
        assert!(slices.iter().all(|s| s.lines <= 1000), "{names:?}");
    }

    #[test]
    fn one_file_over_the_limit_is_its_own_slice_and_ranking_is_stable() {
        let w = Weights::default();
        let files = vec![stat("layers/services/p/src/huge.ts", 5000, 3), stat("layers/services/p/src/small.ts", 10, 0)];
        let slices = rank(pack_slices(&files, &w, 1000), RankBy::Sum);
        assert!(slices.iter().any(|s| s.name == "layers/services/p/src/huge.ts" && s.files.len() == 1));
        let again = rank(pack_slices(&files, &w, 1000), RankBy::Sum);
        let a: Vec<&str> = slices.iter().map(|s| s.name.as_str()).collect();
        let b: Vec<&str> = again.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(a, b);
        assert!(slices[0].score >= slices[slices.len() - 1].score);
    }

    #[test]
    fn density_ranks_a_small_hot_slice_above_a_big_mild_one() {
        let big = Slice { name: "big".into(), files: vec![], lines: 20_000, score: 400.0 };
        let small = Slice { name: "small".into(), files: vec![], lines: 5_000, score: 200.0 };
        let tiny = Slice { name: "tiny".into(), files: vec![], lines: 100, score: 30.0 };
        let by_sum: Vec<String> = rank(vec![big.clone(), small.clone(), tiny.clone()], RankBy::Sum).into_iter().map(|s| s.name).collect();
        assert_eq!(by_sum, vec!["big", "small", "tiny"]);
        let by_density: Vec<String> = rank(vec![big, small, tiny], RankBy::Density).into_iter().map(|s| s.name).collect();
        assert_eq!(by_density, vec!["small", "tiny", "big"], "tiny counts as 1,000 lines: 30/1000*1000 = 30 > big's 20");
    }

    #[test]
    fn volume_counts_tickets_three_ways() {
        let w = Weights::default();
        let mut a = stat("layers/features/a/src/a.tsx", 100, 0);
        a.violations = vec![Violation { rule: "mcds-prefix", line: 1 }];
        let mut b = stat("layers/features/a/src/b.tsx", 100, 0);
        b.violations = vec![Violation { rule: "mcds-prefix", line: 2 }];
        b.markers = Markers { suppressions: 1, any_casts: 0, todos: 2 };
        let big = stat("layers/features/z/src/big.ts", 900, 0);
        let quiet = stat("layers/features/z/src/quiet.ts", 50, 0);
        let files = vec![a, b, big, quiet];
        let slices = pack_slices(&files, &w, 25_000);
        let v = volume(&files, &slices, 10);
        assert_eq!((v.rule_files, v.rule_kinds, v.suppression_files, v.todo_files, v.big_files), (2, 1, 1, 1, 1));
        assert_eq!(v.candidate_files, 3, "the quiet file is not a candidate");
        assert_eq!(v.by_file, 3);
        // rule 2 files -> 1, suppression 1 -> 1, todo 1 -> 1, any 0 -> 0, big 1 -> 1
        assert_eq!(v.by_kind, 4);
        assert_eq!(v.by_slice, 1, "two small slices of the same layer are merged into one");
        // A kind with more files than the cap is cut into several tickets.
        assert_eq!(volume(&files, &slices, 1).by_kind, 2 + 1 + 1 + 1);
        // All four files land in one slice here, so grouping inside the slice
        // gives the same count as grouping the whole repo.
        assert_eq!(v.by_kind_in_slice, v.by_kind);
        assert_eq!(volume(&[], &[], 10), Volume::default());
    }

    #[test]
    fn long_merged_slice_names_are_shortened_in_the_report() {
        assert_eq!(short_name("a/x"), "a/x");
        assert_eq!(short_name("a/x + y"), "a/x + y");
        assert_eq!(short_name("a/x + y + z + w"), "a/x + y 외 2개");
    }

    #[test]
    fn report_lists_rule_counts_slices_and_files() {
        let w = Weights::default();
        let mut f = stat("layers/features/a/src/x.tsx", 900, 12);
        f.violations = vec![Violation { rule: "mcds-prefix", line: 3 }];
        let files = vec![f];
        let slices = rank(pack_slices(&files, &w, 1000), RankBy::Sum);
        let r = report(&files, &slices, &w, 5);
        assert!(r.contains("소스 1개 · 900줄 · 조각 1개"), "{r}");
        assert!(r.contains("mcds-prefix 1"), "{r}");
        assert!(r.contains("| 1 | layers/features/a/src/x.tsx | 900 | 12 |"), "{r}");
    }
}
