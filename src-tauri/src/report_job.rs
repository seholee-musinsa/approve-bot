//! 리포트 만들기와 저장. 재료(Jira 의 봇 티켓, 장부, 실행 기록)를 읽어 `sweep_report` 로 렌더하고
//! 설정 폴더의 `reports/` 에 마크다운으로 남긴다. Jira 는 읽기만 한다.

use crate::config::{AppConfig, Frequency, SweepSettings};
use crate::sweep_report::{self, Input, Kind, Period};
use anyhow::{anyhow, Result};
use chrono::NaiveDateTime;
use serde::Serialize;
use std::path::{Path, PathBuf};

pub fn reports_dir(dir: &Path) -> PathBuf {
    dir.join("reports")
}

/// 제목을 파일 이름으로(경로 문자를 걷어낸다).
fn file_name(title: &str) -> String {
    let safe: String = title.chars().map(|c| if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') { '_' } else { c }).collect();
    format!("{safe}.md")
}

pub fn save_report(dir: &Path, title: &str, markdown: &str) -> Result<()> {
    let d = reports_dir(dir);
    std::fs::create_dir_all(&d)?;
    let p = d.join(file_name(title));
    let tmp = p.with_extension("md.tmp");
    std::fs::write(&tmp, markdown)?;
    std::fs::rename(tmp, p)?;
    Ok(())
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ReportMeta {
    pub title: String,
    /// 만든 시각(파일 수정 시각), unix 초.
    pub generated_at: u64,
    /// Confluence 에 게시했다면 페이지 주소(주소를 알 수 없으면 빈 문자열). 게시 전이면 None.
    pub published_url: Option<String>,
}

fn published_path(dir: &Path) -> PathBuf {
    reports_dir(dir).join("published.json")
}

/// 제목 → 페이지 주소.
pub fn load_published(dir: &Path) -> std::collections::BTreeMap<String, String> {
    std::fs::read_to_string(published_path(dir)).ok().and_then(|r| serde_json::from_str(&r).ok()).unwrap_or_default()
}

fn mark_published(dir: &Path, title: &str, url: &str) -> Result<()> {
    let mut m = load_published(dir);
    m.insert(title.to_string(), url.to_string());
    std::fs::create_dir_all(reports_dir(dir))?;
    let p = published_path(dir);
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&m)?)?;
    std::fs::rename(tmp, p)?;
    Ok(())
}

/// 최신순.
pub fn list_reports(dir: &Path) -> Vec<ReportMeta> {
    let published = load_published(dir);
    let mut v: Vec<ReportMeta> = std::fs::read_dir(reports_dir(dir))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let title = name.strip_suffix(".md")?.to_string();
            let published_url = published.get(&title).cloned();
            let at = e.metadata().ok()?.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
            Some(ReportMeta { title, generated_at: at, published_url })
        })
        .collect();
    v.sort_by(|a, b| b.generated_at.cmp(&a.generated_at).then_with(|| b.title.cmp(&a.title)));
    v
}

pub fn read_report(dir: &Path, title: &str) -> Result<String> {
    std::fs::read_to_string(reports_dir(dir).join(file_name(title))).map_err(|_| anyhow!("리포트를 찾을 수 없다: {title}"))
}

/// 저장된 리포트를 Confluence 에 게시한다(같은 제목이면 갱신, R.6). 쓰기 호출이다.
/// space 와 부모 페이지가 설정돼 있어야 한다. 성공하면 게시 기록을 남기고 주소를 돌려준다.
pub fn publish(dir: &Path, cfg: &AppConfig, title: &str) -> Result<String> {
    let r = &cfg.report;
    if r.space_key.is_empty() || r.parent_page_id.is_empty() {
        return Err(anyhow!("Confluence space 와 부모 페이지 ID 를 먼저 입력해 주세요"));
    }
    let jc = crate::jira::load_config(dir)?;
    if jc.cloud_id.is_empty() {
        return Err(anyhow!("Cloud ID 가 없다(티켓 설정에서 저장)"));
    }
    let md = read_report(dir, title)?;
    let storage = crate::confluence::md_to_storage(&md);
    let client = crate::confluence::Confluence::connect(&jc.cloud_id)?;
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let url = rt.block_on(client.publish(&r.space_key, &r.parent_page_id, title, &storage))?;
    mark_published(dir, title, &url)?;
    Ok(url)
}

/// 읽기: 설정한 space·부모 페이지가 실제로 있는지 확인한다.
pub fn check_parent(dir: &Path, cfg: &AppConfig) -> Result<String> {
    let r = &cfg.report;
    if r.space_key.is_empty() || r.parent_page_id.is_empty() {
        return Err(anyhow!("space 와 부모 페이지 ID 를 먼저 입력해 주세요"));
    }
    let jc = crate::jira::load_config(dir)?;
    let client = crate::confluence::Confluence::connect(&jc.cloud_id)?;
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let (title, space) = rt.block_on(client.page_brief(&r.parent_page_id))?;
    if !space.eq_ignore_ascii_case(&r.space_key) {
        return Err(anyhow!("부모 페이지 {} 는 space {space} 에 있다(설정: {})", r.parent_page_id, r.space_key));
    }
    Ok(format!("부모 페이지 확인: {title} ({space})"))
}

/// 아직 게시하지 않은 리포트 제목들(R.7 의 재시도 대상).
pub fn unpublished(dir: &Path) -> Vec<String> {
    let done = load_published(dir);
    list_reports(dir).into_iter().map(|m| m.title).filter(|t| !done.contains_key(t)).collect()
}

pub fn kind_from(s: &str) -> Result<Kind> {
    match s {
        "weekly" => Ok(Kind::Weekly),
        "monthly" => Ok(Kind::Monthly),
        other => Err(anyhow!("알 수 없는 리포트 종류: {other}")),
    }
}

/// 지난 기간 리포트를 만들어 저장하고 제목을 돌려준다. Jira 읽기 호출이 있어 동기 함수다(자체 런타임).
pub fn generate(dir: &Path, cfg: &AppConfig, kind: Kind, now_local: NaiveDateTime, now: u64) -> Result<String> {
    let jc = crate::jira::load_config(dir)?;
    if jc.cloud_id.is_empty() {
        return Err(anyhow!("Jira 설정(cloud id)이 없어 리포트를 만들 수 없다"));
    }
    let client = crate::jira::Jira::connect(&jc)?;
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let (issues, site) = rt.block_on(async {
        let issues = client.search(&crate::jira::jql_bot_all(&jc), 500).await?;
        let site = if jc.site_url.is_empty() { client.site_url().await.unwrap_or_default() } else { jc.site_url.clone() };
        Ok::<_, anyhow::Error>((issues, site))
    })?;
    let status = crate::sweep_sched::status_now(dir, &cfg.sweep, false);
    let runs = crate::sweep_sched::load_log(dir);
    let period = Period::previous(kind, now_local);
    let md = sweep_report::render(&period, &Input { issues: &issues, status: &status, runs: &runs, settings: &cfg.sweep, site_url: &site, now });
    let title = period.title();
    save_report(dir, &title, &md)?;
    Ok(title)
}

/// 리포트 주기를 스윕 예정 시각 계산에 태우기 위한 설정(주간: 월요일, 월간: 1일).
pub fn slot_settings(kind: Kind, hour: u32) -> SweepSettings {
    SweepSettings {
        enabled: true,
        repo: "report".into(),
        frequency: if kind == Kind::Weekly { Frequency::Weekly } else { Frequency::Monthly },
        weekday: 0,
        month_day: 1,
        hour,
        minute: 0,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("report-job-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn save_list_read_roundtrip_with_unsafe_title_chars() {
        let d = tmp("rt");
        save_report(&d, "정기 스윕 주간 리포트 2026-W40", "# 본문").unwrap();
        save_report(&d, "a/b:c", "x").unwrap();
        let l = list_reports(&d);
        assert_eq!(l.len(), 2);
        assert_eq!(read_report(&d, "정기 스윕 주간 리포트 2026-W40").unwrap(), "# 본문");
        assert_eq!(read_report(&d, "a/b:c").unwrap(), "x");
        assert!(read_report(&d, "없음").is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn same_title_overwrites_instead_of_duplicating() {
        let d = tmp("idem");
        save_report(&d, "t", "1").unwrap();
        save_report(&d, "t", "2").unwrap();
        assert_eq!(list_reports(&d).len(), 1);
        assert_eq!(read_report(&d, "t").unwrap(), "2");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn report_slots_are_monday_and_first_of_month() {
        use crate::sweep_sched::{due, last_slot};
        let w = slot_settings(Kind::Weekly, 9);
        let mon = NaiveDate::from_ymd_opt(2026, 10, 5).unwrap().and_hms_opt(9, 30, 0).unwrap();
        assert_eq!(last_slot(&w, mon), NaiveDate::from_ymd_opt(2026, 10, 5).unwrap().and_hms_opt(9, 0, 0).unwrap());
        let m = slot_settings(Kind::Monthly, 9);
        let first = NaiveDate::from_ymd_opt(2026, 11, 1).unwrap().and_hms_opt(10, 0, 0).unwrap();
        let last_gen = NaiveDate::from_ymd_opt(2026, 10, 1).unwrap().and_hms_opt(9, 5, 0).unwrap();
        assert!(due(&m, last_gen, first));
        assert!(!due(&m, first, first + chrono::Duration::days(3)));
    }

    #[test]
    fn published_index_marks_titles_and_finds_the_unpublished() {
        let d = tmp("pub");
        save_report(&d, "a", "1").unwrap();
        save_report(&d, "b", "2").unwrap();
        assert_eq!(unpublished(&d).len(), 2);
        mark_published(&d, "a", "https://w/wiki/spaces/S/pages/1").unwrap();
        assert_eq!(unpublished(&d), vec!["b".to_string()]);
        let l = list_reports(&d);
        assert_eq!(l.iter().find(|m| m.title == "a").unwrap().published_url.as_deref(), Some("https://w/wiki/spaces/S/pages/1"));
        assert_eq!(l.iter().find(|m| m.title == "b").unwrap().published_url, None);
        // published.json 은 리포트 목록에 끼지 않는다.
        assert_eq!(l.len(), 2);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn publish_refuses_without_space_and_parent() {
        let d = tmp("nopub");
        save_report(&d, "t", "x").unwrap();
        let cfg = AppConfig::default();
        assert!(publish(&d, &cfg, "t").unwrap_err().to_string().contains("space"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn kind_parsing() {
        assert_eq!(kind_from("weekly").unwrap(), Kind::Weekly);
        assert!(kind_from("daily").is_err());
    }
}
