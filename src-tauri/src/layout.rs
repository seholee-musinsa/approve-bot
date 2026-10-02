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
/// to act on them.
const FOLDED: &[&str] = &["검증 근거", "산입하지 않은 것"];

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
}
