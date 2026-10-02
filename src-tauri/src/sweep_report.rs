//! 정기 스윕 리포트(요구 R.1~R.10). 이 모듈은 집계와 마크다운 렌더만 한다(읽기도 쓰기도 하지 않는다).
//! 재료는 호출부가 넘긴다: Jira 의 봇 티켓, 장부 상태, 스윕 실행 기록, 설정.
//! 사내 정보(티켓 키, 제목, 경로)가 들어가므로 결과는 설정 폴더에만 저장한다.

use crate::config::{Frequency, SweepSettings};
use crate::jira::{category_of, results, browse_url, Issue, Results};
use crate::sweep_sched::{to_local, RunEntry, Status};
use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime};
use std::collections::BTreeMap;

/// 분류별 표본이 이보다 적으면 "표본 부족"(2.10).
pub const MIN_SAMPLE: usize = 5;
/// 열린 채 이 일수를 넘으면 보류(방치).
pub const STALE_DAYS: u64 = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Weekly,
    Monthly,
}

/// 리포트가 다루는 기간. 반열린 구간 [start, end).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Period {
    pub kind: Kind,
    pub start: NaiveDate,
    pub end: NaiveDate,
}

impl Period {
    /// `now` 가 속한 기간의 바로 앞 기간. 주간은 지난주(월~일), 월간은 지난달 전체(R.2).
    pub fn previous(kind: Kind, now: NaiveDateTime) -> Period {
        let today = now.date();
        match kind {
            Kind::Weekly => {
                let this_monday = today - Duration::days(i64::from(today.weekday().num_days_from_monday()));
                Period { kind, start: this_monday - Duration::days(7), end: this_monday }
            }
            Kind::Monthly => {
                let first = NaiveDate::from_ymd_opt(today.year(), today.month(), 1).unwrap();
                let prev_last = first - Duration::days(1);
                Period { kind, start: NaiveDate::from_ymd_opt(prev_last.year(), prev_last.month(), 1).unwrap(), end: first }
            }
        }
    }

    /// `정기 스윕 주간 리포트 2026-W41` / `정기 스윕 월간 리포트 2026-10` (R.4).
    pub fn title(&self) -> String {
        match self.kind {
            Kind::Weekly => {
                let w = self.start.iso_week();
                format!("정기 스윕 주간 리포트 {}-W{:02}", w.year(), w.week())
            }
            Kind::Monthly => format!("정기 스윕 월간 리포트 {}-{:02}", self.start.year(), self.start.month()),
        }
    }

    fn contains(&self, t: NaiveDateTime) -> bool {
        t >= self.start.and_hms_opt(0, 0, 0).unwrap() && t < self.end.and_hms_opt(0, 0, 0).unwrap()
    }

    fn label(&self) -> String {
        format!("{} ~ {}", self.start, self.end - Duration::days(1))
    }
}

pub struct Input<'a> {
    pub issues: &'a [Issue],
    pub status: &'a Status,
    pub runs: &'a [RunEntry],
    pub settings: &'a SweepSettings,
    pub site_url: &'a str,
    pub now: u64,
}

fn pct(p: Option<f64>) -> String {
    p.map_or("-".to_string(), |v| format!("{v:.0}%"))
}

/// 채택률 한 칸. 표본이 모자라면 숫자 대신 "표본 부족".
fn rate_cell(r: &Results) -> String {
    let judged = r.done + r.rejected;
    if judged < MIN_SAMPLE {
        format!("표본 부족({judged}건)")
    } else {
        format!("{} ({}/{})", pct(r.adoption_percent), r.done, judged)
    }
}

fn esc(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', " ")
}

/// 실행 기록 출력에서 읽은 조각 수와 지적·후보·티켓 수를 센다.
/// (출력 문구는 `sweep::run_day` 가 만든다: "오늘의 조각 …", "지적 N건 → 후보 M건 · 오늘 만들 티켓 K건 …")
fn run_totals(runs: &[&RunEntry]) -> (usize, usize, usize, usize) {
    let (mut slices, mut findings, mut cands, mut planned) = (0, 0, 0, 0);
    let num_after = |line: &str, key: &str| -> usize {
        line.split(key).nth(1).map(|r| r.trim_start().chars().take_while(|c| c.is_ascii_digit()).collect::<String>()).and_then(|d| d.parse().ok()).unwrap_or(0)
    };
    for r in runs {
        for line in r.text.lines() {
            if line.starts_with("오늘의 조각") {
                slices += 1;
            } else if line.starts_with("지적 ") {
                findings += num_after(line, "지적 ");
                cands += num_after(line, "후보 ");
                planned += num_after(line, "오늘 만들 티켓 ");
            }
        }
    }
    (slices, findings, cands, planned)
}

pub fn render(p: &Period, i: &Input) -> String {
    let in_period = |t: u64| t > 0 && p.contains(to_local(t));
    let mine: Vec<&Issue> = i.issues.iter().filter(|x| in_period(x.created)).collect();
    let mine_owned: Vec<Issue> = mine.iter().map(|x| (*x).clone()).collect();
    let period_results = results(&mine_owned, i.now, STALE_DAYS);
    let all_results = results(i.issues, i.now, STALE_DAYS);
    let runs: Vec<&RunEntry> = i.runs.iter().filter(|r| in_period(r.at)).collect();
    let (slices, findings, cands, _planned) = run_totals(&runs);
    let secs: u64 = runs.iter().map(|r| r.seconds).sum();
    let failed = runs.iter().filter(|r| !r.ok).count();

    let mut o = String::new();
    o.push_str(&format!("# {}\n\n", p.title()));
    o.push_str(&format!("기간: {} · 대상: {}\n\n", p.label(), if i.settings.repo.is_empty() { "-" } else { &i.settings.repo }));

    // (1) 요약
    o.push_str("## 1. 요약\n\n");
    o.push_str(&format!("- 실행 {}회(실패 {}회) · 읽은 조각 {}개 · 소요 {}\n", runs.len(), failed, slices, dur(secs)));
    o.push_str(&format!("- 지적 {findings}건 → 후보 {cands}건 → 만든 티켓 {}건\n", mine.len()));
    o.push_str(&match i.status.cycle_no {
        Some(n) if i.status.slices_total > 0 => format!(
            "- 바퀴 {n}: 읽은 조각 {}/{} (커버리지 {}%) · 이월 후보 {}건\n",
            i.status.slices_done,
            i.status.slices_total,
            i.status.slices_done * 100 / i.status.slices_total,
            i.status.carryover
        ),
        _ => "- 진행 중인 바퀴 없음\n".to_string(),
    });
    let mut by_cat: BTreeMap<&str, usize> = BTreeMap::new();
    for x in &mine {
        *by_cat.entry(category_of(&x.labels).unwrap_or("분류 미상")).or_default() += 1;
    }
    if !by_cat.is_empty() {
        let parts: Vec<String> = by_cat.iter().map(|(k, v)| format!("{k} {v}")).collect();
        o.push_str(&format!("- 분류별 생성: {}\n", parts.join(" · ")));
    }

    // (2) 티켓 목록
    o.push_str("\n## 2. 이 기간에 만든 티켓\n\n");
    if mine.is_empty() {
        o.push_str("없음\n");
    } else {
        o.push_str("| 티켓 | 제목 | 분류 | 상태 |\n|---|---|---|---|\n");
        for x in &mine {
            let link = match browse_url(i.site_url, &x.key) {
                u if u.is_empty() => x.key.clone(),
                u => format!("[{}]({u})", x.key),
            };
            let st = match &x.resolution {
                Some(r) => format!("{} ({r})", x.status),
                None => x.status.clone(),
            };
            o.push_str(&format!("| {link} | {} | {} | {} |\n", esc(&x.summary), category_of(&x.labels).unwrap_or("-"), esc(&st)));
        }
    }

    // (3) 결과
    o.push_str("\n## 3. 결과\n\n");
    o.push_str("| 구분 | 완료 | 거절 | 열림 | 보류 | 채택률 |\n|---|--:|--:|--:|--:|---|\n");
    let row = |name: &str, r: &Results| format!("| {name} | {} | {} | {} | {} | {} |\n", r.done, r.rejected, r.open, r.stale, rate_cell(r));
    o.push_str(&row("이 기간에 만든 것", &period_results));
    o.push_str(&row("누적(전체 봇 티켓)", &all_results));
    for cat in ["rule", "split", "debt"] {
        let sub: Vec<Issue> = i.issues.iter().filter(|x| category_of(&x.labels) == Some(cat)).cloned().collect();
        if !sub.is_empty() {
            o.push_str(&row(&format!("누적 · {cat}"), &results(&sub, i.now, STALE_DAYS)));
        }
    }
    let a = &all_results;
    if a.rejected > 0 {
        o.push_str(&format!(
            "\n거절 사유(누적): 사실 틀림 {} · 가치 낮음 {} · 크기·시점 {} · 중복 {} · 이미 해결 {} · 사유 없음 {}\n",
            a.wrong, a.low_value, a.size_timing, a.duplicate, a.already_fixed, a.no_reason
        ));
    }
    let open_all = a.open;
    o.push_str(&format!(
        "\n방치율: 열린 티켓 {open_all}건 중 {STALE_DAYS}일 넘게 처리 없는 것 {}건({})\n",
        a.stale,
        if open_all == 0 { "-".to_string() } else { format!("{}%", a.stale * 100 / open_all) }
    ));
    o.push_str(&format!(
        "전환 기준(결과 10건 이상, 채택률 60% 이상): {}\n",
        if a.ready_to_expand { "충족 — 담당자 풀과 하루 상한 확대를 검토" } else { "미충족 — 계속 쌓는다" }
    ));

    // (4) 부모 없는 봇 티켓
    let orphans: Vec<&Issue> = i.issues.iter().filter(|x| x.parent.is_none() && outcome_open(x)).collect();
    o.push_str(&format!("\n## 4. 부모 없는 봇 티켓\n\n{}건\n", orphans.len()));
    for x in orphans.iter().take(20) {
        o.push_str(&format!("- {} {}\n", x.key, esc(&x.summary)));
    }

    // (5) 담당자
    o.push_str("\n## 5. 담당자별\n\n");
    let mut who: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for x in i.issues {
        let name = x.assignee.clone().unwrap_or_else(|| "(담당자 없음)".into());
        let e = who.entry(name).or_default();
        if in_period(x.created) {
            e.0 += 1;
        }
        if outcome_open(x) {
            e.1 += 1;
        }
    }
    if who.is_empty() {
        o.push_str("없음\n");
    } else {
        o.push_str("| 담당자 | 이 기간 배정 | 열린 티켓 |\n|---|--:|--:|\n");
        for (n, (a, b)) in &who {
            o.push_str(&format!("| {} | {a} | {b} |\n", esc(n)));
        }
        let counts: Vec<usize> = who.values().map(|v| v.0).collect();
        if counts.len() > 1 {
            o.push_str(&format!("\n배정 수 최대와 최소의 차이: {}\n", counts.iter().max().unwrap() - counts.iter().min().unwrap()));
        }
    }

    // (6) 비용·시간
    o.push_str("\n## 6. 시간\n\n");
    o.push_str(&format!("- 이 기간 실행 {}회, 합계 {}, 실행당 평균 {}\n", runs.len(), dur(secs), if runs.is_empty() { "-".into() } else { dur(secs / runs.len() as u64) }));
    o.push_str("- 모델 비용은 이 리포트에 아직 집계하지 않는다(실행 기록에 비용을 남기지 않음).\n");

    // (7) 핫스팟은 키가 해시라 경로를 되짚을 수 없어 아직 다루지 않는다.
    o.push_str("\n## 7. 다음 기간 계획\n\n");
    let remaining = i.status.slices_total.saturating_sub(i.status.slices_done);
    if i.status.cycle_no.is_some() {
        let per_run = i.settings.slices_per_run.max(1) as usize;
        let runs_left = remaining.div_ceil(per_run);
        let days = match i.settings.frequency {
            Frequency::Daily => runs_left,
            Frequency::Weekly => runs_left * 7,
            Frequency::Monthly => runs_left * 30,
        };
        o.push_str(&format!("- 남은 조각 {remaining}개, 회당 {per_run}개 기준 약 {runs_left}회({days}일) 뒤 한 바퀴 완료\n"));
    } else {
        o.push_str("- 진행 중인 바퀴 없음\n");
    }
    o.push_str(&format!("- 이월 후보 {}건\n", i.status.carryover));
    o
}

fn outcome_open(i: &Issue) -> bool {
    !i.done
}

fn dur(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}초")
    } else if secs < 3600 {
        format!("{}분 {}초", secs / 60, secs % 60)
    } else {
        format!("{}시간 {}분", secs / 3600, (secs % 3600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn dt(y: i32, m: u32, d: u32, h: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, m, d).unwrap().and_hms_opt(h, 0, 0).unwrap()
    }

    fn unix(t: NaiveDateTime) -> u64 {
        chrono::Local.from_local_datetime(&t).single().unwrap().timestamp() as u64
    }

    #[test]
    fn previous_week_is_last_monday_to_sunday_and_titled_by_iso_week() {
        // 2026-10-05 월요일 오전 → 지난주 9/28~10/4 (ISO 2026-W40)
        let p = Period::previous(Kind::Weekly, dt(2026, 10, 5, 9));
        assert_eq!((p.start, p.end), (NaiveDate::from_ymd_opt(2026, 9, 28).unwrap(), NaiveDate::from_ymd_opt(2026, 10, 5).unwrap()));
        assert_eq!(p.title(), "정기 스윕 주간 리포트 2026-W40");
        // 수요일에 만들어도 같은 지난주.
        assert_eq!(Period::previous(Kind::Weekly, dt(2026, 10, 7, 9)), p);
    }

    #[test]
    fn previous_month_handles_january() {
        let p = Period::previous(Kind::Monthly, dt(2026, 1, 1, 9));
        assert_eq!((p.start, p.end), (NaiveDate::from_ymd_opt(2025, 12, 1).unwrap(), NaiveDate::from_ymd_opt(2026, 1, 1).unwrap()));
        assert_eq!(p.title(), "정기 스윕 월간 리포트 2025-12");
        assert_eq!(Period::previous(Kind::Monthly, dt(2026, 10, 1, 9)).title(), "정기 스윕 월간 리포트 2026-09");
    }

    fn issue(key: &str, created: NaiveDateTime, cat: Option<&str>, res: Option<&str>, assignee: &str, parent: bool) -> Issue {
        let mut labels = vec!["KTLO".to_string(), "bot-created".to_string(), format!("sweep-{key}")];
        if let Some(c) = cat {
            labels.push(format!("sweepcat-{c}"));
        }
        Issue {
            key: key.into(),
            labels,
            done: res.is_some(),
            resolution: res.map(String::from),
            comments: vec![],
            summary: format!("[KTLO] 제목 {key} | 파이프"),
            created: unix(created),
            parent: parent.then(|| "SID-1".to_string()),
            status: if res.is_some() { "완료".into() } else { "SUGGESTED".into() },
            assignee: Some(assignee.into()),
        }
    }

    fn status() -> Status {
        Status {
            running: false,
            cycle_no: Some(1),
            slices_done: 10,
            slices_total: 90,
            next_slice: None,
            carryover: 3,
            created_keys: 0,
            done_keys: 0,
            rejected_keys: 0,
            finished_cycles: 0,
            last_run: None,
            next_run: None,
        }
    }

    #[test]
    fn report_has_every_section_with_synthetic_data() {
        let p = Period::previous(Kind::Weekly, dt(2026, 10, 5, 9)); // 9/28 ~ 10/4
        let issues = vec![
            issue("a1", dt(2026, 9, 29, 10), Some("rule"), Some("Done"), "가", true),
            issue("a2", dt(2026, 9, 30, 10), Some("split"), None, "가", false),
            issue("a3", dt(2026, 9, 1, 10), Some("debt"), Some("Duplicate"), "나", true), // 기간 밖
        ];
        let runs = vec![
            RunEntry { at: unix(dt(2026, 9, 29, 2)), ok: true, seconds: 120, text: "오늘의 조각 x\n지적 15건 → 후보 8건 · 오늘 만들 티켓 6건 · 이월 0건\n".into() },
            RunEntry { at: unix(dt(2026, 9, 30, 2)), ok: false, seconds: 60, text: "--- 조각 1 실패 ---\n".into() },
            RunEntry { at: unix(dt(2026, 8, 1, 2)), ok: true, seconds: 999, text: "오늘의 조각 y\n".into() }, // 기간 밖
        ];
        let settings = SweepSettings { repo: "o/r".into(), ..Default::default() };
        let md = render(&p, &Input { issues: &issues, status: &status(), runs: &runs, settings: &settings, site_url: "https://x.atlassian.net", now: unix(dt(2026, 10, 5, 9)) });
        for h in ["# 정기 스윕 주간 리포트 2026-W40", "## 1. 요약", "## 2. 이 기간에 만든 티켓", "## 3. 결과", "## 4. 부모 없는 봇 티켓", "## 5. 담당자별", "## 6. 시간", "## 7. 다음 기간 계획"] {
            assert!(md.contains(h), "{h}");
        }
                assert!(md.contains("실행 2회(실패 1회)"));
        assert!(md.contains("읽은 조각 1개"));
        assert!(md.contains("지적 15건 → 후보 8건 → 만든 티켓 2건"));
        assert!(md.contains("[a1](https://x.atlassian.net/browse/a1)"));
        assert!(md.contains("제목 a1 \\| 파이프"), "표 안의 | 는 이스케이프");
        assert!(!md.contains("a3](") , "기간 밖 티켓은 목록에 없다");
        assert!(md.contains("커버리지 11%"));
        assert!(md.contains("남은 조각 80개"));
        // 표본이 5건 미만이라 숫자 대신 표본 부족.
        assert!(md.contains("표본 부족"));
        assert!(md.contains("분류별 생성: rule 1 · split 1"));
        assert!(md.contains("| 가 | 2 | 1 |"));
    }

    #[test]
    fn adoption_shows_a_rate_once_the_sample_is_big_enough() {
        let p = Period::previous(Kind::Weekly, dt(2026, 10, 5, 9));
        let mut issues = vec![];
        for n in 0..5 {
            issues.push(issue(&format!("d{n}"), dt(2026, 9, 29, 10), Some("rule"), Some(if n < 4 { "Done" } else { "Won't Do" }), "가", true));
        }
        let settings = SweepSettings::default();
        let md = render(&p, &Input { issues: &issues, status: &status(), runs: &[], settings: &settings, site_url: "", now: unix(dt(2026, 10, 5, 9)) });
        assert!(md.contains("80% (4/5)"));
        assert!(md.contains("| d0 | [KTLO]"), "주소가 없으면 링크 없이 키만 쓴다");
    }

    #[test]
    fn empty_inputs_still_render() {
        let p = Period::previous(Kind::Monthly, dt(2026, 10, 1, 9));
        let settings = SweepSettings::default();
        let idle = Status { cycle_no: None, slices_total: 0, slices_done: 0, ..status() };
        let md = render(&p, &Input { issues: &[], status: &idle, runs: &[], settings: &settings, site_url: "", now: unix(dt(2026, 10, 1, 9)) });
        assert!(md.contains("진행 중인 바퀴 없음") && md.contains("없음"));
    }
}
