//! 정기 스윕 장부. 한 바퀴(cycle)의 조각 진행 상황을 파일로 보관한다.
//! 시각과 커밋은 호출자가 넘긴다(테스트 가능하게 시계를 읽지 않는다).

use crate::sweep_review::Finding;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

/// 같은 조각이 이만큼 연속 실패하면 이번 바퀴에서는 건너뛴다.
pub const MAX_FAILURES: u32 = 3;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SliceStatus {
    Pending,
    Done { commit: String, at: u64 },
    Failed { count: u32, reason: String },
    Skipped { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SliceEntry {
    pub name: String,
    pub lines: usize,
    #[serde(flatten)]
    pub status: SliceStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cycle {
    pub no: u32,
    pub started_commit: String,
    pub started_at: u64,
    pub slices: Vec<SliceEntry>,
}

/// 티켓을 닫을 때 코멘트 첫 줄에 적는 거절 사유(요구 4.13).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectReason {
    Wrong,
    LowValue,
    SizeTiming,
    Duplicate,
    AlreadyFixed,
    Unknown,
}

/// 안정 키별 처리 결과.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum KeyOutcome {
    Created { ticket: String, at: u64 },
    Done { ticket: String, at: u64 },
    Rejected { reason: RejectReason, at: u64 },
}

/// 상한 때문에 오늘 만들지 못하고 이월한 티켓 한 건 분량의 지적.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Carried {
    pub commit: String,
    pub findings: Vec<Finding>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ledger {
    pub cycle: Option<Cycle>,
    /// 지금까지 끝낸 바퀴 수.
    pub finished_cycles: u32,
    #[serde(default)]
    pub keys: BTreeMap<String, KeyOutcome>,
    #[serde(default)]
    pub carryover: Vec<Carried>,
}

impl Cycle {
    fn is_open(e: &SliceEntry) -> bool {
        matches!(e.status, SliceStatus::Pending | SliceStatus::Failed { .. })
    }

    pub fn done_count(&self) -> usize {
        self.slices
            .iter()
            .filter(|e| matches!(e.status, SliceStatus::Done { .. }))
            .count()
    }

    pub fn is_complete(&self) -> bool {
        !self.slices.iter().any(Self::is_open)
    }
}

impl Ledger {
    /// 순위(위험 순)가 매겨진 조각 `(이름, 줄 수)` 로 새 바퀴를 시작한다. 진행 중이던 바퀴는 버린다.
    pub fn start_cycle(&mut self, ranked: &[(String, usize)], commit: &str, now: u64) {
        self.carryover.clear();
        let no = self.finished_cycles + 1;
        self.cycle = Some(Cycle {
            no,
            started_commit: commit.to_string(),
            started_at: now,
            slices: ranked
                .iter()
                .map(|(n, l)| SliceEntry {
                    name: n.clone(),
                    lines: *l,
                    status: SliceStatus::Pending,
                })
                .collect(),
        });
    }

    /// 다음에 읽을 조각. 실패한 조각은 Pending 보다 뒤로 미룬다(같은 조각에 매달리지 않도록).
    pub fn next(&self) -> Option<&SliceEntry> {
        let c = self.cycle.as_ref()?;
        c.slices
            .iter()
            .find(|e| e.status == SliceStatus::Pending)
            .or_else(|| {
                c.slices
                    .iter()
                    .find(|e| matches!(e.status, SliceStatus::Failed { .. }))
            })
    }

    pub fn mark_read(&mut self, name: &str, commit: &str, now: u64) -> bool {
        self.set(name, SliceStatus::Done { commit: commit.to_string(), at: now })
    }

    pub fn mark_failed(&mut self, name: &str, reason: &str) -> bool {
        let prev = self
            .cycle
            .as_ref()
            .and_then(|c| c.slices.iter().find(|e| e.name == name))
            .map(|e| match &e.status {
                SliceStatus::Failed { count, .. } => *count,
                _ => 0,
            });
        let Some(prev) = prev else { return false };
        let count = prev + 1;
        let status = if count >= MAX_FAILURES {
            SliceStatus::Skipped { reason: reason.to_string() }
        } else {
            SliceStatus::Failed { count, reason: reason.to_string() }
        };
        self.set(name, status)
    }

    fn set(&mut self, name: &str, status: SliceStatus) -> bool {
        match self
            .cycle
            .as_mut()
            .and_then(|c| c.slices.iter_mut().find(|e| e.name == name))
        {
            Some(e) => {
                e.status = status;
                true
            }
            None => false,
        }
    }

    /// 읽은 조각이 모두 소진되면 바퀴를 닫는다. 닫았으면 true.
    pub fn finish_if_complete(&mut self) -> bool {
        if self.cycle.as_ref().is_some_and(|c| c.is_complete()) {
            self.cycle = None;
            self.carryover.clear();
            self.finished_cycles += 1;
            true
        } else {
            false
        }
    }

    /// 바퀴를 처음으로 되돌린다. 키 이력(만든·거절한 것)은 남겨 같은 것을 다시 제안하지 않는다.
    pub fn reset_cycle(&mut self) {
        self.cycle = None;
        self.carryover.clear();
    }

    pub fn record(&mut self, key: &str, outcome: KeyOutcome) {
        self.keys.insert(key.to_string(), outcome);
    }

    pub fn is_carried(&self, key: &str) -> bool {
        self.carryover.iter().any(|c| c.findings.iter().any(|f| f.key == key))
    }

    /// 이월분 중 파일이 바뀐 것을 버린다(3.7). 버린 건수를 돌려준다.
    pub fn expire_carryover(&mut self, changed: &HashSet<String>) -> usize {
        let before = self.carryover.len();
        self.carryover.retain(|c| {
            !c.findings
                .iter()
                .any(|f| changed.contains(&f.path) || f.related.iter().any(|r| changed.contains(r)))
        });
        before - self.carryover.len()
    }

    /// 자리가 난 만큼 이월분을 앞에서부터 꺼낸다. (Jira 생성 단계에서 쓴다)
    #[allow(dead_code)]
    pub fn take_carried(&mut self, n: usize) -> Vec<Carried> {
        let n = n.min(self.carryover.len());
        self.carryover.drain(..n).collect()
    }

    /// repo 가 바뀌어 조각 구성이 달라졌을 때 순위를 새로 맞춘다.
    /// 이미 읽은(Done) 조각은 이름이 같으면 유지하고, 새 조각은 Pending, 사라진 조각은 버린다.
    /// 줄 수만 갱신하고 새 순서는 `ranked` 를 따른다.
    pub fn replan(&mut self, ranked: &[(String, usize)], commit: &str) {
        let Some(c) = self.cycle.as_mut() else { return };
        let old = std::mem::take(&mut c.slices);
        c.slices = ranked
            .iter()
            .map(|(n, l)| {
                let status = old
                    .iter()
                    .find(|e| &e.name == n)
                    .map(|e| match &e.status {
                        s @ SliceStatus::Done { .. } => s.clone(),
                        _ => SliceStatus::Pending,
                    })
                    .unwrap_or(SliceStatus::Pending);
                SliceEntry { name: n.clone(), lines: *l, status }
            })
            .collect();
        c.started_commit = commit.to_string();
    }
}

pub fn path(dir: &Path) -> PathBuf {
    dir.join("sweep-state.json")
}

/// 없으면 빈 장부. 깨진 파일은 `.bak` 으로 치우고 빈 장부로 시작한다(스윕이 멈추지 않도록).
pub fn load(dir: &Path) -> Ledger {
    let p = path(dir);
    let Ok(raw) = std::fs::read_to_string(&p) else {
        return Ledger::default();
    };
    match serde_json::from_str(&raw) {
        Ok(l) => l,
        Err(_) => {
            let _ = std::fs::rename(&p, p.with_extension("json.bak"));
            Ledger::default()
        }
    }
}

/// 임시 파일에 쓰고 rename 한다. 쓰는 도중 꺼져도 이전 장부가 남는다.
pub fn save(dir: &Path, l: &Ledger) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let p = path(dir);
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(l).map_err(std::io::Error::other)?)?;
    std::fs::rename(tmp, p)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ranked(names: &[&str]) -> Vec<(String, usize)> {
        names.iter().map(|n| (n.to_string(), 100)).collect()
    }

    fn started(names: &[&str]) -> Ledger {
        let mut l = Ledger::default();
        l.start_cycle(&ranked(names), "c1", 10);
        l
    }

    #[test]
    fn reads_in_rank_order_then_closes_cycle() {
        let mut l = started(&["a", "b"]);
        assert_eq!(l.next().unwrap().name, "a");
        l.mark_read("a", "c1", 11);
        assert_eq!(l.next().unwrap().name, "b");
        assert!(!l.finish_if_complete());
        l.mark_read("b", "c1", 12);
        assert!(l.next().is_none());
        assert!(l.finish_if_complete());
        assert_eq!(l.finished_cycles, 1);
        assert!(l.cycle.is_none());
    }

    #[test]
    fn failed_slice_waits_behind_pending_and_is_skipped_after_limit() {
        let mut l = started(&["a", "b"]);
        l.mark_failed("a", "network");
        assert_eq!(l.next().unwrap().name, "b");
        l.mark_read("b", "c1", 11);
        assert_eq!(l.next().unwrap().name, "a");
        l.mark_failed("a", "network");
        l.mark_failed("a", "network");
        assert!(matches!(
            l.cycle.as_ref().unwrap().slices[0].status,
            SliceStatus::Skipped { .. }
        ));
        assert!(l.finish_if_complete());
    }

    #[test]
    fn unknown_slice_is_rejected() {
        let mut l = started(&["a"]);
        assert!(!l.mark_read("zzz", "c", 1));
        assert!(!l.mark_failed("zzz", "x"));
    }

    #[test]
    fn replan_keeps_done_drops_removed_adds_new_in_new_order() {
        let mut l = started(&["a", "b", "c"]);
        l.mark_read("b", "c1", 11);
        l.mark_failed("c", "x");
        l.replan(&ranked(&["d", "b", "c"]), "c2");
        let c = l.cycle.as_ref().unwrap();
        let names: Vec<_> = c.slices.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["d", "b", "c"]);
        assert!(matches!(c.slices[1].status, SliceStatus::Done { .. }));
        assert_eq!(c.slices[0].status, SliceStatus::Pending);
        assert_eq!(c.slices[2].status, SliceStatus::Pending);
        assert_eq!(c.started_commit, "c2");
        assert_eq!(c.done_count(), 1);
    }

    #[test]
    fn reset_cycle_keeps_key_history_and_cycle_count() {
        let mut l = started(&["a"]);
        l.mark_read("a", "c1", 1);
        l.record("k", KeyOutcome::Created { ticket: "SID-1".into(), at: 1 });
        l.finished_cycles = 3;
        l.reset_cycle();
        assert!(l.cycle.is_none() && l.carryover.is_empty());
        assert_eq!((l.keys.len(), l.finished_cycles), (1, 3));
        l.start_cycle(&ranked(&["a"]), "c2", 2);
        assert_eq!(l.next().unwrap().name, "a");
    }

    #[test]
    fn new_cycle_numbers_continue() {
        let mut l = started(&["a"]);
        l.mark_read("a", "c1", 1);
        l.finish_if_complete();
        l.start_cycle(&ranked(&["a"]), "c2", 2);
        assert_eq!(l.cycle.as_ref().unwrap().no, 2);
    }

    #[test]
    fn save_load_roundtrip_and_corrupt_file_is_set_aside() {
        let dir = std::env::temp_dir().join(format!("sweep-state-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(load(&dir), Ledger::default());
        let mut l = started(&["a", "b"]);
        l.mark_read("a", "c1", 5);
        l.mark_failed("b", "boom");
        save(&dir, &l).unwrap();
        assert_eq!(load(&dir), l);
        std::fs::write(path(&dir), "{not json").unwrap();
        assert_eq!(load(&dir), Ledger::default());
        assert!(path(&dir).with_extension("json.bak").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
