//! 정기 스윕 3단계: 조각에서 나온 지적을 장부와 대조해 오늘 만들 티켓을 정한다.
//! 순수 함수만 둔다(Jira·파일을 읽지 않는다). 요구 3.1~3.7, 3.10, 3.11.

use crate::sweep_review::{group, Category, Finding, TicketDraft};
use crate::sweep_state::{Carried, KeyOutcome, Ledger, RejectReason};
use std::collections::{BTreeMap, HashSet};

const DAY: u64 = 86_400;

#[derive(Debug, Clone)]
pub struct Policy {
    /// 확신 기준. 큰 파일·중복 분리는 주관이 많아 더 높게 둔다(3.1).
    pub min_conf: u32,
    pub min_conf_split: u32,
    /// 한 사람의 열린 봇 티켓 상한(5.5). 항상 적용된다.
    pub open_cap: usize,
    /// 하루 생성 상한(3.6). None 이면 제한 없음.
    pub daily_cap: Option<usize>,
    /// 크기·시점, 사유 없음 거절 뒤 다시 후보가 되기까지(3.4).
    pub cooldown_days: u64,
    /// 묶음당 최대 파일 수(3.5).
    pub max_files: usize,
}

impl Default for Policy {
    fn default() -> Self {
        Policy { min_conf: 80, min_conf_split: 85, open_cap: 10, daily_cap: None, cooldown_days: 90, max_files: 10 }
    }
}

/// 후보에서 빠진 이유. 기준을 조정할 때 근거가 된다(3.10).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Skip {
    LowConfidence,
    SameRun,
    Carried,
    /// 같은 키의 티켓이 이미 있다(열려 있다).
    Ticketed(String),
    Rejected(RejectReason),
    Cooling,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    New,
    /// 완료한 뒤 다시 나왔다. 새 티켓을 만들고 이전 티켓을 링크한다.
    Recurrence(String),
    Skip(Skip),
}

pub fn judge(f: &Finding, ledger: &Ledger, p: &Policy, now: u64) -> Verdict {
    let need = if f.category == Category::Split { p.min_conf_split } else { p.min_conf };
    if f.confidence < need {
        return Verdict::Skip(Skip::LowConfidence);
    }
    if ledger.is_carried(&f.key) {
        return Verdict::Skip(Skip::Carried);
    }
    match ledger.keys.get(&f.key) {
        None => Verdict::New,
        Some(KeyOutcome::Created { ticket, .. }) => Verdict::Skip(Skip::Ticketed(ticket.clone())),
        Some(KeyOutcome::Done { ticket, .. }) => Verdict::Recurrence(ticket.clone()),
        Some(KeyOutcome::Rejected { reason, at }) => match reason {
            RejectReason::SizeTiming | RejectReason::Unknown => {
                if now.saturating_sub(*at) >= p.cooldown_days * DAY {
                    Verdict::New
                } else {
                    Verdict::Skip(Skip::Cooling)
                }
            }
            r => Verdict::Skip(Skip::Rejected(*r)),
        },
    }
}

#[derive(Debug, Default)]
pub struct DayPlan {
    /// 후보가 된 지적. 아래 초안의 `findings` 인덱스가 가리킨다.
    pub kept: Vec<Finding>,
    pub create: Vec<TicketDraft>,
    /// 상한 때문에 이월할 초안.
    pub carry: Vec<TicketDraft>,
    pub skipped: Vec<(String, Skip)>,
    /// 재발한 키 → 이전 티켓.
    pub recurrences: BTreeMap<String, String>,
}

/// 오늘 만들 티켓과 이월할 티켓을 정한다. `score` 는 파일 경로의 위험 점수(3-a).
pub fn plan_day(
    findings: Vec<Finding>,
    ledger: &Ledger,
    p: &Policy,
    now: u64,
    open_tickets: usize,
    score: &dyn Fn(&str) -> f64,
) -> DayPlan {
    let mut plan = DayPlan::default();
    let mut seen: HashSet<String> = HashSet::new();
    for f in findings {
        if !seen.insert(f.key.clone()) {
            plan.skipped.push((f.key, Skip::SameRun));
            continue;
        }
        match judge(&f, ledger, p, now) {
            Verdict::New => plan.kept.push(f),
            Verdict::Recurrence(prev) => {
                plan.recurrences.insert(f.key.clone(), prev);
                plan.kept.push(f);
            }
            Verdict::Skip(s) => plan.skipped.push((f.key, s)),
        }
    }
    let mut drafts = group(&plan.kept, p.max_files);
    let top = |d: &TicketDraft| d.files.iter().map(|f| score(f)).fold(0.0_f64, f64::max);
    drafts.sort_by(|a, b| top(b).partial_cmp(&top(a)).unwrap_or(std::cmp::Ordering::Equal).then_with(|| a.title.cmp(&b.title)));
    let room = p.open_cap.saturating_sub(open_tickets).min(p.daily_cap.unwrap_or(usize::MAX));
    plan.carry = drafts.split_off(room.min(drafts.len()));
    plan.create = drafts;
    plan
}

/// 이월할 초안을 장부에 저장한다(기준 커밋과 함께).
pub fn park(ledger: &mut Ledger, plan: &DayPlan, commit: &str) {
    for d in &plan.carry {
        ledger.carryover.push(Carried {
            commit: commit.to_string(),
            findings: d.findings.iter().map(|&i| plan.kept[i].clone()).collect(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sweep_review::{stable_key, Effort};

    fn f(path: &str, cat: Category, sym: &str, conf: u32) -> Finding {
        Finding {
            category: cat,
            kind: "k".into(),
            path: path.into(),
            line: None,
            symbol: sym.into(),
            claim: String::new(),
            evidence: String::new(),
            fix: String::new(),
            fix_code: String::new(),
            confidence: conf,
            effort: Effort::M,
            rule_ref: String::new(),
            related: vec![],
            title: String::new(),
            prerequisite: String::new(),
            key: stable_key(path, cat.name(), sym),
            slice: String::new(),
            commit: String::new(),
        }
    }

    fn rej(l: &mut Ledger, x: &Finding, r: RejectReason, at: u64) {
        l.record(&x.key, KeyOutcome::Rejected { reason: r, at });
    }

    #[test]
    fn confidence_threshold_is_higher_for_split() {
        let l = Ledger::default();
        let p = Policy::default();
        assert_eq!(judge(&f("a.ts", Category::Debt, "x", 80), &l, &p, 0), Verdict::New);
        assert_eq!(judge(&f("a.ts", Category::Debt, "x", 79), &l, &p, 0), Verdict::Skip(Skip::LowConfidence));
        assert_eq!(judge(&f("a.ts", Category::Split, "x", 84), &l, &p, 0), Verdict::Skip(Skip::LowConfidence));
        assert_eq!(judge(&f("a.ts", Category::Split, "x", 85), &l, &p, 0), Verdict::New);
    }

    #[test]
    fn rejection_rules_follow_the_reason() {
        let p = Policy::default();
        let x = f("a.ts", Category::Debt, "x", 90);
        let t0 = 1_000_000;
        for r in [RejectReason::Wrong, RejectReason::LowValue, RejectReason::Duplicate, RejectReason::AlreadyFixed] {
            let mut l = Ledger::default();
            rej(&mut l, &x, r, t0);
            assert_eq!(judge(&x, &l, &p, t0 + 400 * DAY), Verdict::Skip(Skip::Rejected(r)), "{r:?}");
        }
        for r in [RejectReason::SizeTiming, RejectReason::Unknown] {
            let mut l = Ledger::default();
            rej(&mut l, &x, r, t0);
            assert_eq!(judge(&x, &l, &p, t0 + 89 * DAY), Verdict::Skip(Skip::Cooling));
            assert_eq!(judge(&x, &l, &p, t0 + 90 * DAY), Verdict::New);
        }
    }

    #[test]
    fn open_ticket_blocks_and_done_ticket_means_recurrence() {
        let p = Policy::default();
        let x = f("a.ts", Category::Debt, "x", 90);
        let mut l = Ledger::default();
        l.record(&x.key, KeyOutcome::Created { ticket: "SID-1".into(), at: 1 });
        assert_eq!(judge(&x, &l, &p, 2), Verdict::Skip(Skip::Ticketed("SID-1".into())));
        l.record(&x.key, KeyOutcome::Done { ticket: "SID-1".into(), at: 3 });
        assert_eq!(judge(&x, &l, &p, 4), Verdict::Recurrence("SID-1".into()));
    }

    #[test]
    fn same_key_twice_in_one_run_counts_once() {
        let plan = plan_day(
            vec![f("a.ts", Category::Debt, "x", 90), f("a.ts", Category::Debt, "x", 95)],
            &Ledger::default(),
            &Policy::default(),
            0,
            0,
            &|_| 0.0,
        );
        assert_eq!(plan.kept.len(), 1);
        assert_eq!(plan.skipped, vec![(stable_key("a.ts", "debt", "x"), Skip::SameRun)]);
    }

    #[test]
    fn open_cap_splits_create_and_carry_by_risk_and_is_never_exceeded() {
        // split 은 파일마다 티켓 한 건이라 3건이 나온다.
        let fs = vec![
            f("low.ts", Category::Split, "a", 90),
            f("high.ts", Category::Split, "b", 90),
            f("mid.ts", Category::Split, "c", 90),
        ];
        let score = |p: &str| match p {
            "high.ts" => 3.0,
            "mid.ts" => 2.0,
            _ => 1.0,
        };
        let p = Policy::default();
        let plan = plan_day(fs.clone(), &Ledger::default(), &p, 0, 9, &score);
        assert_eq!(plan.create.len(), 1);
        assert_eq!(plan.create[0].files[0], "high.ts");
        assert_eq!(plan.carry.len(), 2);
        assert_eq!(plan.carry[0].files[0], "mid.ts");
        // 상한에 이르면 하나도 만들지 않는다.
        let full = plan_day(fs.clone(), &Ledger::default(), &p, 0, 10, &score);
        assert!(full.create.is_empty());
        assert_eq!(full.carry.len(), 3);
        // 하루 상한이 더 작으면 그 수까지만.
        let capped = Policy { daily_cap: Some(2), ..Policy::default() };
        let plan = plan_day(fs, &Ledger::default(), &capped, 0, 0, &score);
        assert_eq!((plan.create.len(), plan.carry.len()), (2, 1));
    }

    #[test]
    fn parked_drafts_expire_when_their_files_change_and_block_duplicates() {
        let p = Policy::default();
        let mut l = Ledger::default();
        let plan = plan_day(vec![f("a.ts", Category::Split, "x", 90)], &l, &p, 0, 10, &|_| 1.0);
        park(&mut l, &plan, "c1");
        assert_eq!(l.carryover.len(), 1);
        // 같은 지적이 다시 나와도 이월분과 겹치므로 후보에서 뺀다.
        assert_eq!(judge(&f("a.ts", Category::Split, "x", 90), &l, &p, 0), Verdict::Skip(Skip::Carried));
        // 다른 파일이 바뀐 것은 영향 없고, 이 파일이 바뀌면 버린다.
        assert_eq!(l.expire_carryover(&HashSet::from(["b.ts".to_string()])), 0);
        assert_eq!(l.expire_carryover(&HashSet::from(["a.ts".to_string()])), 1);
        assert!(l.carryover.is_empty());
    }

    #[test]
    fn finishing_a_cycle_drops_carryover_and_take_carried_drains_in_order() {
        let p = Policy::default();
        let mut l = Ledger::default();
        l.start_cycle(&[("s".to_string(), 1)], "c1", 0);
        let plan = plan_day(
            vec![f("a.ts", Category::Split, "x", 90), f("b.ts", Category::Split, "y", 90)],
            &l,
            &p,
            0,
            10,
            &|p| if p == "a.ts" { 2.0 } else { 1.0 },
        );
        park(&mut l, &plan, "c1");
        let first = l.take_carried(1);
        assert_eq!(first[0].findings[0].path, "a.ts");
        assert_eq!(l.carryover.len(), 1);
        l.mark_read("s", "c1", 1);
        assert!(l.finish_if_complete());
        assert!(l.carryover.is_empty());
    }

    #[test]
    fn carried_findings_keep_their_origin_through_the_ledger_file() {
        let mut l = Ledger::default();
        let mut x = f("a.ts", Category::Split, "x", 90);
        x.slice = "layers/features/mamud".into();
        x.commit = "abc123".into();
        let plan = plan_day(vec![x], &l, &Policy::default(), 0, 10, &|_| 1.0);
        park(&mut l, &plan, "abc123");
        let back: Ledger = serde_json::from_str(&serde_json::to_string(&l).unwrap()).unwrap();
        let got = &back.carryover[0].findings[0];
        assert_eq!((got.slice.as_str(), got.commit.as_str()), ("layers/features/mamud", "abc123"));
    }

    #[test]
    fn old_ledger_file_without_new_fields_still_loads() {
        let l: Ledger = serde_json::from_str(r#"{"cycle":null,"finished_cycles":2}"#).unwrap();
        assert_eq!(l.finished_cycles, 2);
        assert!(l.keys.is_empty() && l.carryover.is_empty());
    }
}
