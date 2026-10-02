//! 정기 스윕 스케줄러. 실행 시각 계산은 순수 함수로 두고(시계를 읽지 않는다),
//! 백그라운드 루프가 1분마다 "지금 돌릴 때인가"를 묻는다.
//!
//! 앱이 꺼져 있던 동안 지나간 시각은 켜질 때 한 번만 보정한다(밀린 횟수만큼 돌지 않는다).
//! 스윕을 처음 켠 시점에는 과거 시각을 소급해 돌지 않고, 켠 시각부터 센다.

use crate::config::{CreateMode, Frequency, SweepSettings};
use crate::state::AppState;
use chrono::{Datelike, Duration, Local, NaiveDate, NaiveDateTime, TimeZone};
use std::sync::atomic::Ordering;
use std::sync::Arc;

fn at(date: NaiveDate, s: &SweepSettings) -> NaiveDateTime {
    date.and_hms_opt(s.hour, s.minute, 0).unwrap_or_else(|| date.and_hms_opt(0, 0, 0).unwrap())
}

fn month_slot(year: i32, month: u32, s: &SweepSettings) -> NaiveDateTime {
    // month_day 는 1..=28 로 제한돼 어느 달에도 있다.
    at(NaiveDate::from_ymd_opt(year, month, s.month_day.clamp(1, 28)).unwrap(), s)
}

fn prev_month(year: i32, month: u32) -> (i32, u32) {
    if month == 1 { (year - 1, 12) } else { (year, month - 1) }
}

fn next_month(year: i32, month: u32) -> (i32, u32) {
    if month == 12 { (year + 1, 1) } else { (year, month + 1) }
}

/// `now` 이전(같은 시각 포함)에 가장 최근인 예정 시각.
pub fn last_slot(s: &SweepSettings, now: NaiveDateTime) -> NaiveDateTime {
    match s.frequency {
        Frequency::Daily => {
            let t = at(now.date(), s);
            if t > now { t - Duration::days(1) } else { t }
        }
        Frequency::Weekly => {
            let today = now.date().weekday().num_days_from_monday();
            let back = (today + 7 - s.weekday.min(6)) % 7;
            let t = at(now.date() - Duration::days(i64::from(back)), s);
            if t > now { t - Duration::days(7) } else { t }
        }
        Frequency::Monthly => {
            let t = month_slot(now.year(), now.month(), s);
            if t > now {
                let (y, m) = prev_month(now.year(), now.month());
                month_slot(y, m, s)
            } else {
                t
            }
        }
    }
}

/// `now` 이후 처음 오는 예정 시각.
pub fn next_slot(s: &SweepSettings, now: NaiveDateTime) -> NaiveDateTime {
    let last = last_slot(s, now);
    match s.frequency {
        Frequency::Daily => last + Duration::days(1),
        Frequency::Weekly => last + Duration::days(7),
        Frequency::Monthly => {
            let (y, m) = next_month(last.year(), last.month());
            month_slot(y, m, s)
        }
    }
}

/// 마지막 실행 뒤에 지나간 예정 시각이 있으면 지금 돌 때다.
pub fn due(s: &SweepSettings, last_run: NaiveDateTime, now: NaiveDateTime) -> bool {
    s.enabled && !s.repo.is_empty() && last_slot(s, now) > last_run
}

fn to_local(secs: u64) -> NaiveDateTime {
    Local.timestamp_opt(secs as i64, 0).single().map(|d| d.naive_local()).unwrap_or_default()
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// 설정에서 `sweep-once --day` 인자를 만든다.
pub fn run_flags(s: &SweepSettings) -> Vec<String> {
    let mut f = vec![
        "--repo".to_string(),
        s.repo.clone(),
        "--day".to_string(),
        "--max-lines".to_string(),
        s.max_slice_lines.to_string(),
        "--max-files".to_string(),
        s.max_files_per_ticket.to_string(),
    ];
    f.push(if s.create_mode == CreateMode::Auto { "--create".to_string() } else { "--save".to_string() });
    f
}

// ---- 스윕 로그 --------------------------------------------------------------------

const MAX_LOG: usize = 100;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct RunEntry {
    pub at: u64,
    pub ok: bool,
    pub seconds: u64,
    /// 실행 출력(길면 앞부분만).
    pub text: String,
}

fn log_path(dir: &std::path::Path) -> std::path::PathBuf {
    dir.join("sweep-log.json")
}

pub fn load_log(dir: &std::path::Path) -> Vec<RunEntry> {
    std::fs::read_to_string(log_path(dir)).ok().and_then(|r| serde_json::from_str(&r).ok()).unwrap_or_default()
}

pub fn append_log(dir: &std::path::Path, e: RunEntry) -> std::io::Result<()> {
    let mut log = load_log(dir);
    log.insert(0, e);
    log.truncate(MAX_LOG);
    let p = log_path(dir);
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&log).map_err(std::io::Error::other)?)?;
    std::fs::rename(tmp, p)
}

// ---- 루프 -------------------------------------------------------------------------

pub fn spawn(state: Arc<AppState>) {
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            tick(&state).await;
        }
    });
}

async fn tick(state: &Arc<AppState>) {
    let s = state.config.lock().await.sweep.clone();
    if !s.enabled || s.repo.is_empty() {
        return;
    }
    // PR 리뷰가 도는 동안에는 기다린다(같은 모델 사용량·클론을 두고 다투지 않게).
    if state.pass_running.load(Ordering::SeqCst) {
        return;
    }
    let dir = state.config_dir.clone();
    let mut ledger = crate::sweep_state::load(&dir);
    let now = now_secs();
    let Some(last) = ledger.last_run_at else {
        // 처음 켠 시점: 소급해 돌지 않고 여기서부터 센다.
        ledger.last_run_at = Some(now);
        let _ = crate::sweep_state::save(&dir, &ledger);
        return;
    };
    if !due(&s, to_local(last), to_local(now)) {
        return;
    }
    // 실행 기록을 먼저 남겨 같은 시각에 다시 돌지 않게 한다(실패해도 다음 예정 시각까지 기다린다).
    ledger.last_run_at = Some(now);
    let _ = crate::sweep_state::save(&dir, &ledger);
    run_once(state, &s).await;
}

/// 설정대로 한 번 돌리고 결과를 로그에 남긴다. 이미 도는 중이면 false.
pub async fn run_once(state: &Arc<AppState>, s: &SweepSettings) -> bool {
    if state.sweep_running.swap(true, Ordering::SeqCst) {
        return false;
    }
    let dir = state.config_dir.clone();
    let now = now_secs();
    let started = std::time::Instant::now();
    let mut text = String::new();
    let mut ok = true;
    for n in 0..s.slices_per_run {
        let flags = run_flags(s);
        let (tx, rx) = tokio::sync::oneshot::channel();
        // 모델 호출과 Jira 호출이 자체 런타임을 쓰므로 별도 스레드에서 돌린다.
        std::thread::spawn(move || {
            let _ = tx.send(crate::sweep::run_cli(&flags).map_err(|e| format!("{e:#}")));
        });
        match rx.await {
            Ok(Ok(out)) => {
                text.push_str(&format!("--- 조각 {} ---\n{out}\n", n + 1));
                if out.contains("이번 바퀴의 조각을 모두 읽었다") {
                    break;
                }
            }
            Ok(Err(e)) => {
                ok = false;
                text.push_str(&format!("--- 조각 {} 실패 ---\n{e}\n", n + 1));
                break;
            }
            Err(_) => {
                ok = false;
                text.push_str("실행이 중단됐다\n");
                break;
            }
        }
    }
    let _ = append_log(
        &dir,
        RunEntry { at: now, ok, seconds: started.elapsed().as_secs(), text: text.chars().take(20_000).collect() },
    );
    state.sweep_running.store(false, Ordering::SeqCst);
    true
}

// ---- 화면용 상태 --------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize)]
pub struct Status {
    pub running: bool,
    /// 진행 중인 바퀴: (번호, 읽은 조각, 전체 조각, 다음 조각 이름)
    pub cycle_no: Option<u32>,
    pub slices_done: usize,
    pub slices_total: usize,
    pub next_slice: Option<String>,
    pub carryover: usize,
    pub created_keys: usize,
    pub done_keys: usize,
    pub rejected_keys: usize,
    pub finished_cycles: u32,
    pub last_run: Option<String>,
    pub next_run: Option<String>,
}

fn fmt(t: NaiveDateTime) -> String {
    t.format("%Y-%m-%d %H:%M").to_string()
}

pub fn status(dir: &std::path::Path, s: &SweepSettings, running: bool, now: NaiveDateTime) -> Status {
    use crate::sweep_state::KeyOutcome;
    let l = crate::sweep_state::load(dir);
    let count = |f: fn(&KeyOutcome) -> bool| l.keys.values().filter(|o| f(o)).count();
    Status {
        running,
        cycle_no: l.cycle.as_ref().map(|c| c.no),
        slices_done: l.cycle.as_ref().map_or(0, |c| c.done_count()),
        slices_total: l.cycle.as_ref().map_or(0, |c| c.slices.len()),
        next_slice: l.next().map(|e| e.name.clone()),
        carryover: l.carryover.len(),
        created_keys: count(|o| matches!(o, KeyOutcome::Created { .. })),
        done_keys: count(|o| matches!(o, KeyOutcome::Done { .. })),
        rejected_keys: count(|o| matches!(o, KeyOutcome::Rejected { .. })),
        finished_cycles: l.finished_cycles,
        last_run: l.last_run_at.map(|t| fmt(to_local(t))),
        next_run: (s.enabled && !s.repo.is_empty()).then(|| fmt(next_slot(s, now))),
    }
}

pub fn status_now(dir: &std::path::Path, s: &SweepSettings, running: bool) -> Status {
    status(dir, s, running, to_local(now_secs()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dt(y: i32, m: u32, d: u32, h: u32, mi: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, m, d).unwrap().and_hms_opt(h, mi, 0).unwrap()
    }

    fn on(freq: Frequency) -> SweepSettings {
        SweepSettings { enabled: true, repo: "o/r".into(), frequency: freq, hour: 2, minute: 30, ..Default::default() }
    }

    #[test]
    fn daily_slots_before_and_after_the_hour() {
        let s = on(Frequency::Daily);
        assert_eq!(last_slot(&s, dt(2026, 10, 2, 3, 0)), dt(2026, 10, 2, 2, 30));
        assert_eq!(last_slot(&s, dt(2026, 10, 2, 1, 0)), dt(2026, 10, 1, 2, 30));
        assert_eq!(last_slot(&s, dt(2026, 10, 2, 2, 30)), dt(2026, 10, 2, 2, 30));
        assert_eq!(next_slot(&s, dt(2026, 10, 2, 3, 0)), dt(2026, 10, 3, 2, 30));
        assert_eq!(next_slot(&s, dt(2026, 10, 2, 1, 0)), dt(2026, 10, 2, 2, 30));
    }

    #[test]
    fn weekly_uses_the_chosen_weekday() {
        // 2026-10-02 는 금요일. 월요일(0) 설정.
        let s = on(Frequency::Weekly);
        assert_eq!(last_slot(&s, dt(2026, 10, 2, 12, 0)), dt(2026, 9, 28, 2, 30));
        assert_eq!(next_slot(&s, dt(2026, 10, 2, 12, 0)), dt(2026, 10, 5, 2, 30));
        // 월요일 이른 아침(예정 시각 전)은 지난주 월요일.
        assert_eq!(last_slot(&s, dt(2026, 10, 5, 1, 0)), dt(2026, 9, 28, 2, 30));
        assert_eq!(last_slot(&s, dt(2026, 10, 5, 2, 30)), dt(2026, 10, 5, 2, 30));
    }

    #[test]
    fn monthly_handles_month_and_year_edges() {
        let mut s = on(Frequency::Monthly);
        s.month_day = 15;
        assert_eq!(last_slot(&s, dt(2026, 10, 20, 0, 0)), dt(2026, 10, 15, 2, 30));
        assert_eq!(last_slot(&s, dt(2026, 10, 10, 0, 0)), dt(2026, 9, 15, 2, 30));
        assert_eq!(last_slot(&s, dt(2027, 1, 10, 0, 0)), dt(2026, 12, 15, 2, 30));
        assert_eq!(next_slot(&s, dt(2026, 12, 20, 0, 0)), dt(2027, 1, 15, 2, 30));
    }

    #[test]
    fn due_catches_up_once_after_the_app_was_off() {
        let s = on(Frequency::Daily);
        // 어제 03:00 에 마지막으로 돌았고, 사흘 뒤 켜졌다 → 한 번 돈다.
        let last = dt(2026, 10, 1, 3, 0);
        let now = dt(2026, 10, 4, 9, 0);
        assert!(due(&s, last, now));
        // 돈 직후에는 다시 돌지 않는다(밀린 횟수만큼 반복하지 않는다).
        assert!(!due(&s, now, dt(2026, 10, 4, 9, 1)));
        // 예정 시각 전이면 아직이다.
        assert!(!due(&s, dt(2026, 10, 4, 3, 0), dt(2026, 10, 4, 23, 59)));
        assert!(due(&s, dt(2026, 10, 4, 3, 0), dt(2026, 10, 5, 2, 30)));
    }

    #[test]
    fn disabled_or_empty_repo_is_never_due() {
        let mut s = on(Frequency::Daily);
        let (last, now) = (dt(2026, 10, 1, 0, 0), dt(2026, 10, 9, 0, 0));
        assert!(due(&s, last, now));
        s.enabled = false;
        assert!(!due(&s, last, now));
        s.enabled = true;
        s.repo.clear();
        assert!(!due(&s, last, now));
    }

    #[test]
    fn settings_clamp_and_old_config_loads_with_defaults() {
        let mut s = SweepSettings { weekday: 9, month_day: 31, hour: 99, minute: 99, slices_per_run: 0, max_slice_lines: 5, max_files_per_ticket: 0, ..Default::default() };
        s.clamp();
        assert_eq!((s.weekday, s.month_day, s.hour, s.minute, s.slices_per_run), (6, 28, 23, 59, 1));
        assert_eq!((s.max_slice_lines, s.max_files_per_ticket), (2_000, 1));
        let old: crate::config::AppConfig = serde_json::from_str(
            r#"{"repositories":[],"allowed_authors":[],"polling_interval_seconds":60,"auto_approve_enabled":true,"approval_message":"","skip_drafts":true}"#,
        )
        .unwrap();
        assert!(!old.sweep.enabled);
        assert_eq!(old.sweep.max_slice_lines, 25_000);
    }

    #[test]
    fn run_flags_follow_create_mode() {
        let mut s = on(Frequency::Daily);
        assert!(run_flags(&s).contains(&"--save".to_string()) && !run_flags(&s).contains(&"--create".to_string()));
        s.create_mode = CreateMode::Auto;
        assert!(run_flags(&s).contains(&"--create".to_string()));
        assert_eq!(&run_flags(&s)[..2], ["--repo", "o/r"]);
    }

    #[test]
    fn status_reports_progress_and_next_run() {
        use crate::sweep_state::{KeyOutcome, Ledger};
        let dir = std::env::temp_dir().join(format!("sweep-status-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut l = Ledger::default();
        l.start_cycle(&[("a".to_string(), 1), ("b".to_string(), 1)], "c1", 0);
        l.mark_read("a", "c1", 1);
        l.record("k1", KeyOutcome::Created { ticket: "T-1".into(), at: 1 });
        crate::sweep_state::save(&dir, &l).unwrap();
        let s = on(Frequency::Daily);
        let st = status(&dir, &s, false, dt(2026, 10, 2, 3, 0));
        assert_eq!((st.slices_done, st.slices_total, st.next_slice.as_deref()), (1, 2, Some("b")));
        assert_eq!((st.created_keys, st.done_keys, st.rejected_keys), (1, 0, 0));
        assert_eq!(st.next_run.as_deref(), Some("2026-10-03 02:30"));
        let off = status(&dir, &SweepSettings::default(), false, dt(2026, 10, 2, 3, 0));
        assert!(off.next_run.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn log_is_newest_first_and_capped() {
        let dir = std::env::temp_dir().join(format!("sweep-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..(MAX_LOG as u64 + 5) {
            append_log(&dir, RunEntry { at: i, ok: true, seconds: 1, text: String::new() }).unwrap();
        }
        let log = load_log(&dir);
        assert_eq!(log.len(), MAX_LOG);
        assert_eq!(log[0].at, MAX_LOG as u64 + 4);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
