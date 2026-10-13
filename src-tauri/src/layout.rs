//! How the posted review reads on GitHub.
//!
//! The model writes one markdown body: `# 총평`, `# 발견`, `# 검증 근거`, `# 미확인`,
//! `# 산입하지 않은 것`. Most readers need the first two; the sections that
//! show the work (what was checked, what was deliberately not flagged) are for
//! the few who want to audit it. Those are folded into `<details>` so the
//! review opens short, with a count so a ⚠️/❌ among them is still visible.
//!
//! Only the copy sent to GitHub is folded. The body kept in the app, the eval
//! output and the follow-up parser all see the original.

/// Sections that are folded. `# 미확인` and `# 질문` stay open: the author has
/// to act on them. The second pass's re-check is audit trail too — anything it
/// overturned is already a finding above, and the ❌ count stays on the summary.
const FOLDED: &[&str] = &["검증 근거", "산입하지 않은 것", "권한·계약 재점검"];

fn folded_title(line: &str) -> Option<&'static str> {
    let rest = line.strip_prefix("# ")?.trim();
    FOLDED.iter().copied().find(|t| rest.starts_with(t))
}

struct Section {
    title: &'static str,
    lines: Vec<String>,
}

fn flush(out: &mut String, s: Section) {
    let content = s.lines.join("\n");
    let content = content.trim();
    if content.is_empty() {
        return;
    }
    let top: Vec<&str> = s.lines.iter().map(String::as_str).filter(|l| l.starts_with("- ")).collect();
    let warn = top.iter().filter(|l| l.contains('⚠')).count();
    let fail = top.iter().filter(|l| l.contains('❌')).count();
    let mut summary = if top.is_empty() { s.title.to_string() } else { format!("{} {}건", s.title, top.len()) };
    if warn > 0 {
        summary.push_str(&format!(" · ⚠️ {warn}"));
    }
    if fail > 0 {
        summary.push_str(&format!(" · ❌ {fail}"));
    }
    out.push_str(&format!("<details>\n<summary>{summary}</summary>\n\n{content}\n\n</details>\n\n"));
}

/// One closing line asking authors to answer the suggestions. Weave counts a
/// suggestion as addressed by a later commit or a clear reply, and our own
/// `feedback` tally reads the replies too. Only added when suggestions were
/// posted (`n` inline + file threads), so a review without any carries no note.
pub fn with_reply_hint(body: &str, n: usize) -> String {
    if n == 0 || body.contains(REPLY_HINT_MARK) {
        return body.to_string();
    }
    format!("{}\n\n{REPLY_HINT}", body.trim_end())
}

const REPLY_HINT_MARK: &str = "한 줄 답글";
const REPLY_HINT: &str = "💬 제안마다 반영 커밋이나 한 줄 답글(반영 / 보류 / 반대 + 이유)을 남겨 주세요. 처리 여부를 다음 리뷰와 집계에 반영합니다.";

/// How many findings a list shows before folding the rest.
const LIST_VISIBLE: usize = 5;

/// `blocker 1 · minor 2` — non-zero counts only, in severity order.
pub fn counts_line(findings: &[&crate::review::Finding]) -> String {
    let order = [("blocker", "blocker"), ("major", "major"), ("minor", "minor"), ("nit", "nit"), ("question", "질문")];
    let parts: Vec<String> = order
        .iter()
        .filter_map(|(sev, label)| {
            let n = findings.iter().filter(|f| crate::review::severity_rank(&f.severity) == crate::review::severity_rank(sev)).count();
            (n > 0).then(|| format!("{label} {n}"))
        })
        .collect();
    if parts.is_empty() {
        "지적 없음".to_string()
    } else {
        parts.join(" · ")
    }
}

/// A titled list of findings, most severe first, the first five open and the
/// rest folded. Each item is the headline and where it is.
pub fn findings_list(heading: &str, findings: &[&crate::review::Finding]) -> String {
    if findings.is_empty() {
        return String::new();
    }
    let mut sorted: Vec<&crate::review::Finding> = findings.to_vec();
    sorted.sort_by_key(|f| crate::review::severity_rank(&f.severity));
    let item = |f: &crate::review::Finding| {
        let (icon, _) = crate::review::severity_label(&f.severity);
        let at = match (f.path.is_empty(), f.line) {
            (true, _) => String::new(),
            (false, Some(l)) => format!(" · `{}:{l}`", f.path),
            (false, None) => format!(" · `{}`", f.path),
        };
        format!("- {icon} **{}**{at}", crate::review::finding_title(f))
    };
    let mut out = format!("**{heading} {}건**\n", sorted.len());
    for f in sorted.iter().take(LIST_VISIBLE) {
        out.push_str(&item(f));
        out.push('\n');
    }
    if sorted.len() > LIST_VISIBLE {
        let rest: Vec<String> = sorted.iter().skip(LIST_VISIBLE).map(|f| item(f)).collect();
        out.push_str(&format!(
            "<details>\n<summary>외 {}건</summary>\n\n{}\n\n</details>\n",
            rest.len(),
            rest.join("\n")
        ));
    }
    out
}

/// What the review covered, filled by code so it cannot be overstated.
#[derive(Debug, Clone, Default)]
pub struct Scope {
    pub files: usize,
    pub added: usize,
    pub deleted: usize,
    /// The model read the checked-out code, not just the diff.
    pub explored: bool,
    /// The authorization/contract second pass ran.
    pub rechecked: bool,
    /// Files past the diff budget got their own passes.
    pub continued: bool,
    /// Files the review never saw.
    pub omitted: usize,
}

/// Files changed and lines added/deleted in a unified diff.
pub fn diff_stats(diff: &str) -> (usize, usize, usize) {
    let files = crate::diffprep::changed_paths(diff).len();
    let mut added = 0;
    let mut deleted = 0;
    for l in diff.lines() {
        if l.starts_with("+++") || l.starts_with("---") {
            continue;
        }
        if l.starts_with('+') {
            added += 1;
        } else if l.starts_with('-') {
            deleted += 1;
        }
    }
    (files, added, deleted)
}

fn cell(s: &str) -> String {
    s.trim().replace('\n', " ").replace('|', "\\|")
}

/// The report table at the top of a first review.
///
/// ```text
/// | 리뷰 요약 | |
/// |---|---|
/// | **변경** | 서버 스펙에 맞춤<br>배경색 추천 기준 변경 |
/// | **판단** | 결함 없음. … |
/// | **머지 전 확인** | 새 테스트 실행 |
/// | **리뷰 범위** | 파일 26개 (+522 −217) · 코드 직접 열람 · 2차 재점검 포함 |
/// ```
///
/// The model fills the first three as short phrases (`summary`); the scope
/// row is code. Rows with nothing to say are left out. Empty when the model
/// gave no summary, so the caller keeps the prose overview instead.
pub fn report_table(summary: &crate::review::ReviewSummary, scope: &Scope) -> String {
    if summary.is_empty() {
        return String::new();
    }
    let list = |items: &[String]| {
        items.iter().map(|c| cell(c)).filter(|c| !c.is_empty()).collect::<Vec<_>>().join("<br>")
    };
    let mut rows: Vec<(&str, String)> = vec![
        ("변경", list(&summary.changes)),
        ("판단", cell(&summary.judgement)),
        ("머지 전 확인", list(&summary.checks)),
    ];
    let mut covered = vec![format!("파일 {}개 (+{} −{})", scope.files, scope.added, scope.deleted)];
    covered.push(if scope.explored { "코드 직접 열람".into() } else { "diff 만 확인".into() });
    if scope.rechecked {
        covered.push("2차 재점검 포함".into());
    }
    if scope.continued {
        covered.push("큰 PR 이라 나눠서 리뷰".into());
    }
    if scope.omitted > 0 {
        covered.push(format!("{}개 파일 못 봄", scope.omitted));
    }
    rows.push(("리뷰 범위", covered.join(" · ")));
    let mut out = String::from("| 리뷰 요약 | |\n|---|---|\n");
    for (k, v) in rows.into_iter().filter(|(_, v)| !v.is_empty()) {
        out.push_str(&format!("| **{k}** | {v} |\n"));
    }
    out.trim_end().to_string()
}

/// Sections the posted body drops: the findings are listed by code (only the
/// ones without a thread), so the model's own list would repeat the threads.
const DROPPED: &[&str] = &["발견", "질문", "줄을 짚지 못한 지적"];

/// A sentence that states the score or the verdict ("4.0/5점, Approve 합니다").
/// The first line already states the outcome from the gate, and a held review
/// saying "Approve" contradicted it.
fn is_verdict_sentence(s: &str) -> bool {
    s.contains("/5점") || s.contains("Approve 합니다") || s.contains("보완 부탁드립니다")
}

/// The overview paragraphs, one sentence per line, without score/verdict
/// sentences or the `**blocker n · …**` count line. Fenced blocks (mermaid)
/// and list/table/quote lines are left as written.
fn clean_overview(lines: &[String]) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut in_fence = false;
    for line in lines {
        let t = line.trim_start();
        if t.starts_with("```") {
            in_fence = !in_fence;
            out.push(line.clone());
            continue;
        }
        if in_fence || t.starts_with(['-', '|', '>', '<', '*']) && !t.starts_with("**blocker") {
            out.push(line.clone());
            continue;
        }
        if t.starts_with("**blocker") {
            continue;
        }
        let kept: Vec<String> = crate::review::sentence_lines(line)
            .lines()
            .filter(|s| !is_verdict_sentence(s))
            .map(str::to_string)
            .collect();
        if kept.is_empty() && !line.trim().is_empty() {
            continue;
        }
        out.push(kept.join("\n"));
    }
    collapse_blank(&out.join("\n"))
}

/// Only the fenced blocks of some lines (a mermaid diagram), in order.
fn fenced_only(lines: &[String]) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut in_fence = false;
    for line in lines {
        let opens = line.trim_start().starts_with("```");
        if opens || in_fence {
            out.push(line);
        }
        if opens {
            in_fence = !in_fence;
            if !in_fence {
                out.push("");
            }
        }
    }
    out.join("\n").trim().to_string()
}

fn collapse_blank(s: &str) -> String {
    let mut out = String::new();
    let mut blank = 0;
    for line in s.lines() {
        if line.trim().is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.trim().to_string()
}

/// The first review's body as posted.
///
/// ```text
/// {verdict line from the gate}           ← code
/// {other hold notes}                     ← code
///
/// {overview, 2–3 sentences}              ← model, `# 총평` heading removed
///
/// **인라인에 달지 못한 지적 n건**         ← code, threads are not repeated
///
/// ### 미확인 …                            ← model, open, small heading
/// ▶ 검증 근거 · ▶ 산입하지 않은 것 …      ← model, folded
/// ```
///
/// Big `#` headings become `###`: the body is short now, and an H1 on GitHub
/// is louder than anything in it deserves.
pub fn compose_body(
    model_body: &str,
    verdict_line: &str,
    notes: &str,
    report: &str,
    unposted: &[&crate::review::Finding],
) -> String {
    let mut overview: Vec<String> = Vec::new();
    let mut rest = String::new();
    let mut title: Option<String> = None; // None = before the first heading
    let mut seen_overview = false;
    let mut in_fence = false;
    let flush = |title: &Option<String>, lines: &mut Vec<String>, overview: &mut Vec<String>, rest: &mut String, seen: &mut bool| {
        let body: Vec<String> = std::mem::take(lines);
        match title.as_deref() {
            None => overview.extend(body),
            Some(t) if t.starts_with("총평") && !*seen => {
                *seen = true;
                overview.extend(body);
            }
            Some(t) if DROPPED.iter().any(|d| t.starts_with(d)) => {}
            Some(t) => {
                let text = body.join("\n");
                if text.trim().is_empty() {
                    return;
                }
                // Each folded section is folded on its own: a `###` section after
                // it would otherwise be swallowed into the `<details>`.
                if FOLDED.iter().any(|f| t.starts_with(f)) {
                    rest.push_str(&fold_sections(&format!("# {t}\n\n{}", text.trim())));
                } else {
                    rest.push_str(&format!("### {t}\n\n{}", text.trim()));
                }
                rest.push_str("\n\n");
            }
        }
    };
    let mut lines: Vec<String> = Vec::new();
    for line in model_body.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
        }
        if !in_fence && line.starts_with("# ") {
            flush(&title, &mut lines, &mut overview, &mut rest, &mut seen_overview);
            title = Some(line[2..].trim().to_string());
            continue;
        }
        lines.push(line.to_string());
    }
    flush(&title, &mut lines, &mut overview, &mut rest, &mut seen_overview);

    let mut out = verdict_line.trim().to_string();
    if !notes.trim().is_empty() {
        out.push_str(&format!("\n{}", notes.trim()));
    }
    // The report table replaces the prose overview; a diagram the model drew
    // there (mermaid) is kept under it.
    let overview = if report.trim().is_empty() { clean_overview(&overview) } else { fenced_only(&overview) };
    if !report.trim().is_empty() {
        out.push_str(&format!("\n\n{}", report.trim()));
    }
    if !overview.is_empty() {
        out.push_str(&format!("\n\n{overview}"));
    }
    let list = findings_list("인라인에 달지 못한 지적", unposted);
    if !list.is_empty() {
        out.push_str(&format!("\n\n{}", list.trim_end()));
    }
    if !rest.trim().is_empty() {
        out.push_str(&format!("\n\n{}", rest.trim()));
    }
    out.trim().to_string()
}

/// Turn `{{permalink:path:10-14}}` placeholders into commit-pinned links on
/// their own line, which GitHub renders as an embedded code snippet. Without
/// a head commit (eval, preview) they become plain `path:10-14`.
pub fn expand_permalinks(text: &str, owner: &str, repo: &str, sha: &str) -> String {
    let open = crate::review::PERMALINK_OPEN;
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(open) {
        out.push_str(&rest[..start]);
        let after = &rest[start + open.len()..];
        let Some(end) = after.find("}}") else {
            out.push_str(&rest[start..]);
            return out;
        };
        let r = &after[..end];
        let rendered = match r.rsplit_once(':') {
            Some((path, lines)) if !sha.is_empty() && !owner.is_empty() => {
                let anchor = match lines.split_once('-') {
                    Some((a, b)) if a != b => format!("L{a}-L{b}"),
                    Some((a, _)) => format!("L{a}"),
                    None => format!("L{lines}"),
                };
                format!("https://github.com/{owner}/{repo}/blob/{sha}/{path}#{anchor}")
            }
            _ => format!("`{r}`"),
        };
        out.push_str(&rendered);
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    out
}

/// Fold the evidence sections of a review body. A body with none of them comes
/// back unchanged.
pub fn fold_sections(body: &str) -> String {
    if !body.lines().any(|l| folded_title(l).is_some()) {
        return body.to_string();
    }
    let mut out = String::new();
    let mut current: Option<Section> = None;
    let mut in_fence = false;
    for line in body.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
        }
        if !in_fence && line.starts_with("# ") {
            if let Some(s) = current.take() {
                flush(&mut out, s);
            }
            match folded_title(line) {
                Some(title) => current = Some(Section { title, lines: vec![] }),
                None => {
                    out.push_str(line);
                    out.push('\n');
                }
            }
            continue;
        }
        match current.as_mut() {
            Some(s) => s.lines.push(line.to_string()),
            None => {
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    if let Some(s) = current.take() {
        flush(&mut out, s);
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const BODY: &str = "# 총평\n\n요약입니다. 5/5점\n\n# 발견\n\n- 🟡 `a.ts:1` — 문제\n\n# 검증 근거\n\n- ✅ 첫째 — 확인\n  - 자세히\n- ⚠️ 둘째 — 확신 낮음\n- ❌ 셋째 — 틀림\n\n# 미확인\n\n- 서버 응답 — 못 봄\n\n# 산입하지 않은 것\n\n- 다른 곳의 같은 결함 — 이 PR 탓 아님\n";

    #[test]
    fn reply_hint_only_when_suggestions_were_posted_and_never_twice() {
        let body = "# 총평\n\n좋아요";
        assert_eq!(with_reply_hint(body, 0), body, "no suggestions, no note");
        let with = with_reply_hint(body, 2);
        assert!(with.starts_with(body) && with.trim_end().ends_with("집계에 반영합니다."));
        assert!(with.contains("한 줄 답글"));
        assert_eq!(with_reply_hint(&with, 3), with, "an already-hinted body is left alone");
    }

    #[test]
    fn folds_evidence_sections_and_keeps_the_rest_open() {
        let out = fold_sections(BODY);
        assert!(out.starts_with("# 총평\n\n요약입니다. 5/5점\n\n# 발견\n\n- 🟡 `a.ts:1` — 문제"));
        assert!(out.contains("<details>\n<summary>검증 근거 3건 · ⚠️ 1 · ❌ 1</summary>"), "{out}");
        assert!(out.contains("- ✅ 첫째 — 확인\n  - 자세히"), "nested bullets stay inside");
        assert!(out.contains("<summary>산입하지 않은 것 1건</summary>"), "{out}");
        assert!(out.contains("# 미확인\n\n- 서버 응답 — 못 봄"), "미확인 stays open");
        assert!(!out.contains("# 검증 근거"), "the heading becomes the summary");
        assert_eq!(out.matches("</details>").count(), 2);
    }

    #[test]
    fn body_without_those_sections_is_unchanged() {
        let plain = "# 총평\n\n문제 없음\r\n\n# 발견\n\n- 없음";
        assert_eq!(fold_sections(plain), plain);
        assert_eq!(fold_sections(""), "");
    }

    #[test]
    fn heading_inside_a_code_fence_is_not_a_section() {
        let body = "# 총평\n\n```md\n# 검증 근거\n- 예시\n```\n\n# 검증 근거\n\n- ✅ 실제\n";
        let out = fold_sections(body);
        assert_eq!(out.matches("<details>").count(), 1, "{out}");
        assert!(out.contains("```md\n# 검증 근거\n- 예시\n```"), "the fenced example is left alone");
    }

    #[test]
    fn empty_folded_section_is_dropped_and_untitled_counts_are_plain() {
        let out = fold_sections("# 총평\n\n끝\n\n# 검증 근거\n\n# 발견\n\n- 없음");
        assert!(!out.contains("<details>"), "{out}");
        let out = fold_sections("# 검증 근거\n\n문장으로만 적은 근거입니다.");
        assert!(out.contains("<summary>검증 근거</summary>"), "{out}");
    }

    #[test]
    fn recheck_section_is_folded_with_its_fail_count() {
        let out = fold_sections("# 총평\n\n요약\n\n# 권한·계약 재점검\n\n- ✅ 첫째\n- ❌ 둘째\n");
        assert!(out.contains("<summary>권한·계약 재점검 2건 · ❌ 1</summary>"), "{out}");
        assert!(!out.contains("# 권한·계약 재점검"));
    }

    fn f(sev: &str, title: &str, path: &str, line: Option<u64>) -> crate::review::Finding {
        crate::review::Finding {
            severity: sev.into(),
            title: title.into(),
            claim: format!("{title} 원인"),
            path: path.into(),
            line,
            ..Default::default()
        }
    }

    // PR #1 round 1, as the model wrote it under the old guide.
    const OLD_STYLE: &str = "# 총평\n\nTurborepo 뼈대를 세팅하는 PR입니다. 머지를 막을 결함은 없습니다. 4.0/5점, Approve 합니다.\n\n**blocker 0 · major 0 · minor 4 · 질문 1**\n\n# 발견\n\n- 🔵 `a.ts:14` — 헤더가 사라짐\n\n# 검증 근거\n\n- ✅ 첫째 — 확인\n- ⚠️ 둘째 — 확신 낮음\n\n# 미확인\n\n- 빌드 결과 — node_modules 없음\n\n# 질문\n\n- 의도인가요\n\n# 산입하지 않은 것\n\n- 다른 곳 — 이 PR 탓 아님\n";

    #[test]
    fn composed_body_opens_with_the_gate_and_drops_the_model_lists() {
        let nit = f("nit", "같은 테스트를 두 번 돌립니다", "package.json", Some(13));
        let out = compose_body(OLD_STYLE, "👤 **사람 승인 필요** — 민감 파일 변경(.npmrc) · minor 4 · 질문 1", "", "", &[&nit]);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "👤 **사람 승인 필요** — 민감 파일 변경(.npmrc) · minor 4 · 질문 1");
        // Overview: no heading, one sentence per line, no score/verdict sentence, no count line.
        assert!(out.contains("\n\nTurborepo 뼈대를 세팅하는 PR입니다.\n머지를 막을 결함은 없습니다.\n\n"), "{out}");
        assert!(!out.contains("# 총평") && !out.contains("/5점") && !out.contains("Approve") && !out.contains("**blocker 0"), "{out}");
        // The model's own finding/question lists are gone; code lists what has no thread.
        assert!(!out.contains("# 발견") && !out.contains("헤더가 사라짐") && !out.contains("# 질문"), "{out}");
        assert!(out.contains("**인라인에 달지 못한 지적 1건**\n- 🔘 **같은 테스트를 두 번 돌립니다** · `package.json:13`"), "{out}");
        // Open sections get a small heading; audit sections fold.
        assert!(out.contains("### 미확인\n\n- 빌드 결과 — node_modules 없음"), "{out}");
        let open = out.find("### 미확인").unwrap();
        assert!(out[..open].matches("<details>").count() == out[..open].matches("</details>").count(), "미확인 must not sit inside a fold: {out}");
        assert!(out.contains("<summary>검증 근거 2건 · ⚠️ 1</summary>"), "{out}");
        assert!(out.contains("<summary>산입하지 않은 것 1건</summary>"), "{out}");
        assert!(!out.lines().any(|l| l.starts_with("# ")), "no H1 left: {out}");
    }

    #[test]
    fn composed_body_keeps_mermaid_and_works_without_headings() {
        let body = "# 총평\n\n흐름입니다. 아래를 보세요.\n\n```mermaid\nflowchart LR\n  A. --> B\n```\n";
        let out = compose_body(body, "✅ **승인** · 지적 없음", "> ⚠️ **게이트**: 큰 PR", "", &[]);
        assert!(out.starts_with("✅ **승인** · 지적 없음\n> ⚠️ **게이트**: 큰 PR\n\n흐름입니다.\n아래를 보세요."), "{out}");
        assert!(out.contains("```mermaid\nflowchart LR\n  A. --> B\n```"), "{out}");
        assert!(!out.contains("인라인에 달지 못한"));
        let plain = compose_body("헤딩 없이 쓴 본문입니다.", "✅ **승인** · 지적 없음", "", "", &[]);
        assert_eq!(plain, "✅ **승인** · 지적 없음\n\n헤딩 없이 쓴 본문입니다.");
    }

    #[test]
    fn findings_list_sorts_and_folds_past_five() {
        let fs: Vec<crate::review::Finding> = ["nit", "question", "minor", "blocker", "major", "minor", "nit"]
            .iter()
            .enumerate()
            .map(|(i, s)| f(s, &format!("제목{i}"), "a.ts", Some(i as u64 + 1)))
            .collect();
        let refs: Vec<&crate::review::Finding> = fs.iter().collect();
        let out = findings_list("지적", &refs);
        let items: Vec<&str> = out.lines().filter(|l| l.starts_with("- ")).collect();
        assert_eq!(items.len(), 7);
        assert!(items[0].contains("🔴") && items[1].contains("🟡") && items[6].contains("❔"), "{out}");
        assert!(out.contains("<summary>외 2건</summary>"), "{out}");
        assert!(out.find("<details>").unwrap() > out.find(items[4]).unwrap());
        assert_eq!(counts_line(&refs), "blocker 1 · major 1 · minor 2 · nit 2 · 질문 1");
        assert_eq!(counts_line(&[]), "지적 없음");
    }

    #[test]
    fn permalinks_expand_against_the_head_or_fall_back_to_code() {
        let t = "근거\n\n{{permalink:src/a.ts:10-14}}\n\n{{permalink:b.ts:3}}";
        let out = expand_permalinks(t, "musinsa", "repo", "abc123");
        assert!(out.contains("\n\nhttps://github.com/musinsa/repo/blob/abc123/src/a.ts#L10-L14\n\n"), "{out}");
        assert!(out.ends_with("https://github.com/musinsa/repo/blob/abc123/b.ts#L3"), "{out}");
        assert_eq!(expand_permalinks(t, "", "", ""), "근거\n\n`src/a.ts:10-14`\n\n`b.ts:3`");
        assert_eq!(expand_permalinks("그대로", "o", "r", "s"), "그대로");
        // A one-line range is a plain line anchor.
        assert_eq!(expand_permalinks("{{permalink:d.md:1192-1192}}", "o", "r", "s"), "https://github.com/o/r/blob/s/d.md#L1192");
    }

    fn summary() -> crate::review::ReviewSummary {
        crate::review::ReviewSummary {
            changes: vec!["브랜드 DA 광고 제출을 서버 스펙에 맞춤".into(), "배경색 추천 기준 변경".into(), " ".into()],
            judgement: "결함 없음. 우회 주소 `a|b` 도 막음".into(),
            checks: vec![],
        }
    }

    #[test]
    fn report_table_has_fixed_rows_and_code_filled_scope() {
        let scope = Scope { files: 26, added: 522, deleted: 217, explored: true, rechecked: true, ..Default::default() };
        let t = report_table(&summary(), &scope);
        assert_eq!(
            t,
            "| 리뷰 요약 | |\n|---|---|\n\
             | **변경** | 브랜드 DA 광고 제출을 서버 스펙에 맞춤<br>배경색 추천 기준 변경 |\n\
             | **판단** | 결함 없음. 우회 주소 `a\\|b` 도 막음 |\n\
             | **리뷰 범위** | 파일 26개 (+522 −217) · 코드 직접 열람 · 2차 재점검 포함 |"
        );
        let thin = Scope { files: 1, added: 2, deleted: 0, omitted: 3, ..Default::default() };
        assert!(report_table(&summary(), &thin).ends_with("| **리뷰 범위** | 파일 1개 (+2 −0) · diff 만 확인 · 3개 파일 못 봄 |"));
        assert_eq!(report_table(&crate::review::ReviewSummary::default(), &scope), "", "no summary, no table");
    }

    #[test]
    fn report_replaces_the_prose_overview_but_keeps_a_diagram() {
        let body = "# 총평\n\n긴 총평입니다. 두 번째 문장입니다.\n\n```mermaid\nflowchart LR\n  A --> B\n```\n\n# 미확인\n\n- 빌드 — 못 봄\n";
        let table = report_table(&summary(), &Scope { files: 2, added: 3, deleted: 1, explored: true, ..Default::default() });
        let out = compose_body(body, "✅ **승인** · 지적 없음", "", &table, &[]);
        assert!(out.starts_with("✅ **승인** · 지적 없음\n\n| 리뷰 요약 | |"), "{out}");
        assert!(!out.contains("긴 총평"), "{out}");
        assert!(out.contains("|\n\n```mermaid\nflowchart LR\n  A --> B\n```\n\n### 미확인"), "{out}");
    }

    #[test]
    fn diff_stats_count_files_and_lines() {
        let d = "diff --git a/a.ts b/a.ts\n--- a/a.ts\n+++ b/a.ts\n@@ -1,2 +1,2 @@\n-x\n+y\n+z\n ctx\ndiff --git a/b.ts b/b.ts\n--- a/b.ts\n+++ b/b.ts\n@@ -1 +0,0 @@\n-gone\n";
        assert_eq!(diff_stats(d), (2, 2, 2));
    }
}
