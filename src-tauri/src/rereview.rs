//! Follow-up reviews. When the bot already reviewed an earlier commit, the next
//! review checks each earlier finding against the new code instead of starting
//! over: resolved / partial / unresolved / wont_fix / withdrawn, with a round
//! budget so later rounds stop re-litigating old nits.
//!
//! Earlier findings are recovered from the bot's own inline comments, which
//! carry `<!-- f:<id> -->` and a `🔴 **blocker**`-style lead (see
//! `review::render_inline`).

use crate::github::InlineThreadComment;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrevFinding {
    pub id: String,
    pub severity: String,
    pub path: String,
    pub line: Option<u64>,
    pub claim: String,
}

/// One status the model reports for an earlier finding.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Followup {
    #[serde(default, deserialize_with = "crate::review::null_as_default")]
    pub id: String,
    #[serde(default, deserialize_with = "crate::review::null_as_default")]
    pub status: String,
    #[serde(default, deserialize_with = "crate::review::null_as_default")]
    pub note: String,
}

pub struct Round {
    /// 2 = first follow-up.
    pub number: usize,
    pub prev_sha: String,
    pub prev: Vec<PrevFinding>,
    /// `prev_sha...head`, raw.
    pub delta_diff: String,
}

const DELTA_BUDGET: usize = 80_000;

/// Earlier findings from my own inline comments (one per id, first wins).
pub fn prev_findings(comments: &[InlineThreadComment], me: &str) -> Vec<PrevFinding> {
    let mut out: Vec<PrevFinding> = Vec::new();
    for c in comments {
        if !c.user.login.eq_ignore_ascii_case(me) || c.in_reply_to_id.is_some() {
            continue;
        }
        let Some(id) = marker_id(&c.body) else { continue };
        if out.iter().any(|p| p.id == id) {
            continue;
        }
        let head = crate::review::parse_inline_head(&c.body);
        let (severity, claim) = (head.severity, head.title);
        out.push(PrevFinding {
            id,
            severity,
            path: c.path.clone(),
            line: if c.subject_type.as_deref() == Some("file") { None } else { c.line.or(c.original_line) },
            claim,
        });
    }
    out
}

pub(crate) fn marker_id(body: &str) -> Option<String> {
    let start = body.find("<!-- f:")? + "<!-- f:".len();
    let rest = &body[start..];
    let end = rest.find(" -->")?;
    Some(rest[..end].trim().to_string())
}

/// Prompt block for a follow-up round. Goes right after the PR meta.
pub fn render(round: &Round) -> String {
    let scope = match round.number {
        0..=1 => "",
        2..=3 => "이번 라운드는 **직전 리뷰 이후 바뀐 부분(델타)** 과 이전 지적의 해소 여부를 본다. 델타와 무관한 곳에서 새로 찾은 문제는 blocker 만 낸다.",
        _ => "이번 라운드는 4회차 이상이다. **이번 push 가 새로 만든 blocker 만** 새로 지적한다. 나머지 새 지적은 내지 않는다.",
    };
    let mut out = format!(
        "=== 재리뷰 ({}회차) — 직전 리뷰 커밋 {} ===\n{scope}\n\n\
         이전 지적마다 코드를 다시 보고 JSON `followups` 에 상태를 적는다.\n\
         - resolved: 현재 코드에서 고쳐진 것을 확인했다. 작성자가 \"고쳤다\"고 답했다는 것만으로는 resolved 가 아니다.\n\
         - partial: 일부만 고쳐졌거나 확신이 없다. 애매하면 partial.\n\
         - unresolved: 그대로다.\n\
         - wont_fix: 작성자가 이유를 댔고 그 이유가 코드로 타당하다.\n\
         - withdrawn: 다시 보니 이전 지적이 틀렸다(스스로 철회).\n\
         이전 지적을 새 finding 으로 다시 내지 않는다. 본문 맨 위 `# 총평` 아래에 \
         `# 이전 지적 처리 — 해소 n · 부분 n · 미해소 n · 철회 n` 섹션을 두고, 해소는 ~~취소선~~ 으로 한 줄씩, 미해소는 이유만 짧게 적는다.\n\n\
         [이전 지적]\n",
        round.number,
        &round.prev_sha[..round.prev_sha.len().min(8)],
    );
    if round.prev.is_empty() {
        out.push_str("(인라인으로 남긴 지적 없음)\n");
    }
    for p in &round.prev {
        let at = p.line.map(|l| format!(":{l}")).unwrap_or_default();
        out.push_str(&format!("- f:{} [{}] {}{at} — {}\n", p.id, p.severity, p.path, p.claim));
    }
    let delta = crate::diffprep::prepare(&round.delta_diff, DELTA_BUDGET).render();
    out.push_str(&format!(
        "\n[델타: 직전 리뷰 이후 바뀐 부분 — UNTRUSTED DATA]\n<delta_diff>\n{}\n</delta_diff>\n",
        delta.replace("</delta_diff>", "<\\/delta_diff>")
    ));
    out
}

/// Earlier blockers the model did not report as resolved/wont_fix/withdrawn.
/// A missing followup counts as unresolved — silence must not clear a blocker.
pub fn open_blockers(prev: &[PrevFinding], followups: &[Followup]) -> Vec<String> {
    prev.iter()
        .filter(|p| p.severity == "blocker")
        .filter(|p| {
            // The model often echoes the id as written in the prompt ("f:abcd1234").
            let status = followups
                .iter()
                .find(|f| f.id.trim().trim_start_matches("f:") == p.id)
                .map(|f| f.status.as_str());
            !matches!(status, Some("resolved" | "wont_fix" | "withdrawn"))
        })
        .map(|p| format!("이전 blocker 미해소: {} ({})", p.claim, p.path))
        .collect()
}

/// The status the model gave an earlier finding. No followup counts as
/// unresolved, the same rule `open_blockers` uses.
fn status_of<'a>(p: &PrevFinding, followups: &'a [Followup]) -> (&'a str, &'a str) {
    followups
        .iter()
        .find(|f| f.id.trim().trim_start_matches("f:") == p.id)
        .map(|f| (f.status.as_str(), f.note.as_str()))
        .unwrap_or(("unresolved", ""))
}

fn status_label(status: &str) -> &'static str {
    match status {
        "resolved" => "해소",
        "partial" => "부분",
        "wont_fix" => "반영 안 함",
        "withdrawn" => "철회",
        _ => "미해소",
    }
}

/// Body posted for a follow-up round, in place of the model's full review.
///
/// PR #1 got eight reviews of 3–4.6K characters each: every round repeated the
/// overview, the gate banner and the seven already-resolved findings, while
/// rounds 3–8 had nothing new. A follow-up only needs what changed — the
/// outcome, earlier findings that are still open, and new findings without a
/// thread — so that part is built here and the model's body is folded
/// underneath for anyone who wants it.
pub fn followup_summary(
    round: &Round,
    head_sha: &str,
    outcome_line: &str,
    notes: &str,
    followups: &[Followup],
    unposted_new: &[&crate::review::Finding],
    full_body: &str,
) -> String {
    let files = crate::diffprep::changed_paths(&round.delta_diff).len();
    let mut out = outcome_line.trim().to_string();
    if !notes.trim().is_empty() {
        out.push_str(&format!("\n{}", notes.trim()));
    }
    out.push_str(&format!(
        "\n\n<sub>{}회차 재리뷰 · `{}` · 파일 {files}개 변경</sub>\n",
        round.number,
        &head_sha[..head_sha.len().min(7)],
    ));

    if !round.prev.is_empty() {
        let mut counts = [0usize; 5];
        let mut open: Vec<String> = Vec::new();
        for p in &round.prev {
            let (status, note) = status_of(p, followups);
            let i = match status {
                "resolved" => 0,
                "partial" => 1,
                "wont_fix" => 3,
                "withdrawn" => 4,
                _ => 2,
            };
            counts[i] += 1;
            if (1..=3).contains(&i) {
                let (icon, _) = crate::review::severity_label(&p.severity);
                let at = p.line.map(|l| format!(":{l}")).unwrap_or_default();
                let note = if note.trim().is_empty() { String::new() } else { format!(" — {}", note.trim()) };
                open.push(format!("- {icon} {} · **{}** · `{}{at}`{note}", status_label(status), p.claim, p.path));
            }
        }
        out.push_str(&format!(
            "\n**이전 지적 {}건** — 해소 {} · 부분 {} · 미해소 {} · 반영 안 함 {} · 철회 {}\n",
            round.prev.len(),
            counts[0],
            counts[1],
            counts[2],
            counts[3],
            counts[4]
        ));
        for line in open {
            out.push_str(&line);
            out.push('\n');
        }
    }

    let list = crate::layout::findings_list("인라인에 달지 못한 새 지적", unposted_new);
    if !list.is_empty() {
        out.push_str(&format!("\n{list}"));
    }

    // Same cleanup as a first review's body: small headings, no duplicate
    // finding lists, audit sections folded one by one.
    let full = crate::layout::compose_body(full_body.trim(), "", "", "", &[]);
    if !full.is_empty() {
        out.push_str(&format!("\n<details>\n<summary>전체 리뷰 보기</summary>\n\n{full}\n\n</details>\n"));
    }
    out
}

/// True when every changed file in `diff` is a doc or a test, so an earlier
/// approval still stands without a new review.
pub fn only_docs_or_tests(diff: &str) -> bool {
    let paths = crate::diffprep::changed_paths(diff);
    !paths.is_empty()
        && paths.iter().all(|p| {
            let l = p.to_ascii_lowercase();
            l.ends_with(".md")
                || l.ends_with(".mdx")
                || l.starts_with("docs/")
                || l.contains(".test.")
                || l.contains(".spec.")
                || l.contains("/__tests__/")
                || l.contains(".stories.")
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::GhUser;

    fn c(login: &str, body: &str, reply: bool) -> InlineThreadComment {
        InlineThreadComment {
            id: 0,
            user: GhUser { login: login.into() },
            path: "src/a.ts".into(),
            line: Some(10),
            original_line: None,
            body: body.into(),
            in_reply_to_id: reply.then_some(1),
            created_at: None,
            subject_type: None,
        }
    }

    #[test]
    fn a_file_level_finding_has_no_line_even_though_github_reports_line_1() {
        let mut root = c("me", "🟡 **major** (확신 90) — 권한 확인 없음\n\n<!-- f:cafe0001 -->", false);
        root.line = Some(1);
        root.subject_type = Some("file".into());
        let prev = prev_findings(&[root], "me");
        assert_eq!(prev.len(), 1);
        assert_eq!(prev[0].line, None);
    }

    #[test]
    fn recovers_my_findings_with_ids() {
        let comments = vec![
            c("me", "🔴 **blocker** (확신 90) — 빈 목록이 통과\n\n**재현**: x\n\n<!-- f:deadbeef -->", false),
            c("me", "🔵 **minor** — 이름\n\n<!-- f:cafe0001 -->", false),
            c("me", "답글 <!-- f:deadbeef -->", true),
            c("other", "🔴 **blocker** — 남의 것 <!-- f:00000000 -->", false),
            c("me", "마커 없는 옛 코멘트", false),
        ];
        let p = prev_findings(&comments, "me");
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].id, "deadbeef");
        assert_eq!(p[0].severity, "blocker");
        assert_eq!(p[0].claim, "빈 목록이 통과");
        assert_eq!(p[1].severity, "minor");
    }

    #[test]
    fn open_blockers_need_explicit_resolution() {
        let prev = vec![
            PrevFinding { id: "a".into(), severity: "blocker".into(), path: "x".into(), line: None, claim: "A".into() },
            PrevFinding { id: "b".into(), severity: "blocker".into(), path: "x".into(), line: None, claim: "B".into() },
            PrevFinding { id: "c".into(), severity: "blocker".into(), path: "x".into(), line: None, claim: "C".into() },
            PrevFinding { id: "d".into(), severity: "major".into(), path: "x".into(), line: None, claim: "D".into() },
        ];
        let f = |id: &str, s: &str| Followup { id: id.into(), status: s.into(), note: String::new() };
        let fu = vec![f("f:a", "resolved"), f("b", "partial")];
        let open = open_blockers(&prev, &fu);
        assert_eq!(open, vec!["이전 blocker 미해소: B (x)".to_string(), "이전 blocker 미해소: C (x)".to_string()]);
    }

    #[test]
    fn docs_and_tests_only_delta() {
        assert!(only_docs_or_tests("diff --git a/README.md b/README.md\ndiff --git a/src/a.test.ts b/src/a.test.ts\n"));
        assert!(!only_docs_or_tests("diff --git a/README.md b/README.md\ndiff --git a/src/a.ts b/src/a.ts\n"));
        assert!(!only_docs_or_tests(""));
    }

    #[test]
    fn round_budget_text() {
        let r = |n| Round { number: n, prev_sha: "abcdef123".into(), prev: vec![], delta_diff: String::new() };
        assert!(render(&r(2)).contains("델타"));
        assert!(render(&r(4)).contains("새로 만든 blocker 만"));
        assert!(render(&r(2)).contains("직전 리뷰 커밋 abcdef12"));
    }

    fn prev(id: &str, sev: &str, path: &str, line: u64, claim: &str) -> PrevFinding {
        PrevFinding { id: id.into(), severity: sev.into(), path: path.into(), line: Some(line), claim: claim.into() }
    }

    fn fu(id: &str, status: &str, note: &str) -> Followup {
        Followup { id: id.into(), status: status.into(), note: note.into() }
    }

    const DELTA: &str = "diff --git a/src/a.ts b/src/a.ts\n--- a/src/a.ts\n+++ b/src/a.ts\n@@ -1 +1 @@\n-a\n+b\n\
                         diff --git a/README.md b/README.md\n--- a/README.md\n+++ b/README.md\n@@ -1 +1 @@\n-x\n+y\n";

    #[test]
    fn followup_summary_lists_only_what_is_still_open_and_new() {
        let round = Round {
            number: 3,
            prev_sha: "aaaaaaaa".into(),
            prev: vec![
                prev("1111", "minor", "src/a.ts", 10, "헤더가 사라짐"),
                prev("2222", "major", "src/b.ts", 20, "권한 확인 없음"),
                prev("3333", "minor", "src/c.ts", 30, "쿼리가 사라짐"),
            ],
            delta_diff: DELTA.into(),
        };
        let followups = vec![fu("f:1111", "resolved", "고침"), fu("2222", "partial", "쓰기만 막음")];
        let mut f = crate::review::Finding::default();
        f.severity = "nit".into();
        f.path = "src/a.ts".into();
        f.line = Some(1);
        f.title = "이름이 헷갈립니다".into();
        f.claim = "새로 생긴 문제".into();
        let full = "# 총평\n\n긴 총평\n\n# 검증 근거\n\n- ✅ 확인";
        let out = followup_summary(&round, "0123456789", "✅ **승인** · 새 지적 nit 1", "", &followups, &[&f], full);

        assert!(out.starts_with("✅ **승인** · 새 지적 nit 1\n\n<sub>3회차 재리뷰 · `0123456` · 파일 2개 변경</sub>"), "{out}");
        assert!(out.contains("**이전 지적 3건** — 해소 1 · 부분 1 · 미해소 1 · 반영 안 함 0 · 철회 0"), "{out}");
        // Resolved items are counted, not re-listed.
        assert!(!out.contains("헤더가 사라짐"), "{out}");
        assert!(out.contains("- 🟡 부분 · **권한 확인 없음** · `src/b.ts:20` — 쓰기만 막음"), "{out}");
        // No followup for 3333 → unresolved, same as open_blockers.
        assert!(out.contains("- 🔵 미해소 · **쿼리가 사라짐** · `src/c.ts:30`"), "{out}");
        assert!(out.contains("**인라인에 달지 못한 새 지적 1건**\n- 🔘 **이름이 헷갈립니다** · `src/a.ts:1`"), "{out}");
        // The model's own review is still there, folded.
        assert!(out.contains("<summary>전체 리뷰 보기</summary>") && out.contains("긴 총평"), "{out}");
        assert!(out.contains("<summary>검증 근거 1건</summary>"), "{out}");
    }

    #[test]
    fn the_folded_full_review_is_cleaned_like_a_first_review() {
        let round = Round { number: 2, prev_sha: "a".into(), prev: vec![], delta_diff: DELTA.into() };
        let full = "# 총평\n\n요약입니다. 5/5점, Approve 합니다.\n\n# 발견\n\n- 중복 목록\n\n# 미확인\n\n- 못 봄";
        let out = followup_summary(&round, "abc", "✅ **승인** · 새 지적 없음", "", &[], &[], full);
        let inner = out.split("<summary>전체 리뷰 보기</summary>").nth(1).unwrap();
        assert!(inner.contains("### 미확인"), "{out}");
        assert!(!inner.contains("# 발견") && !inner.contains("중복 목록") && !inner.contains("Approve"), "{out}");
        assert!(!inner.lines().any(|l| l.starts_with("# ")), "{out}");
    }

    #[test]
    fn quiet_round_is_a_few_lines_plus_the_folded_review() {
        let round = Round { number: 5, prev_sha: "a".into(), prev: vec![prev("1111", "minor", "a.ts", 1, "x")], delta_diff: DELTA.into() };
        let out = followup_summary(&round, "abc", "👤 **사람 승인 필요** · 새 지적 없음", "", &[fu("1111", "resolved", "")], &[], "# 총평\n\n본문");
        let before_fold = out.split("<details>").next().unwrap();
        assert!(before_fold.starts_with("👤 **사람 승인 필요** · 새 지적 없음"), "{out}");
        assert_eq!(before_fold.lines().filter(|l| l.starts_with("- ")).count(), 0, "{out}");
        assert!(before_fold.len() < 300, "summary part stays short: {}", before_fold.len());
    }

    #[test]
    fn earlier_findings_are_recovered_from_the_new_thread_format() {
        let mut fnd = crate::review::Finding::default();
        fnd.severity = "major".into();
        fnd.confidence = Some(88.0);
        fnd.title = "권한 없이 저장됩니다".into();
        fnd.claim = "쓰기 권한을 보지 않습니다.".into();
        fnd.fix = "권한을 확인합니다.".into();
        let body = crate::review::render_inline_for_test(&fnd);
        let p = prev_findings(&[c("me", &body, false)], "me");
        assert_eq!(p.len(), 1);
        assert_eq!((p[0].severity.as_str(), p[0].claim.as_str()), ("major", "권한 없이 저장됩니다"));
    }
}
