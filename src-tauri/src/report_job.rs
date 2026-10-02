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
}

/// 최신순.
pub fn list_reports(dir: &Path) -> Vec<ReportMeta> {
    let mut v: Vec<ReportMeta> = std::fs::read_dir(reports_dir(dir))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let title = name.strip_suffix(".md")?.to_string();
            let at = e.metadata().ok()?.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
            Some(ReportMeta { title, generated_at: at })
        })
        .collect();
    v.sort_by(|a, b| b.generated_at.cmp(&a.generated_at).then_with(|| b.title.cmp(&a.title)));
    v
}

pub fn read_report(dir: &Path, title: &str) -> Result<String> {
    std::fs::read_to_string(reports_dir(dir).join(file_name(title))).map_err(|_| anyhow!("리포트를 찾을 수 없다: {title}"))
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
    fn kind_parsing() {
        assert_eq!(kind_from("weekly").unwrap(), Kind::Weekly);
        assert!(kind_from("daily").is_err());
    }
}
