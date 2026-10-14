//! Auditable feedback from review threads. Replies are reports, never proof that
//! code changed. Bot follow-ups and independent verification are separate evidence.

use crate::github::InlineThreadComment;
use crate::rereview::marker_id;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Fixed,
    Partial,
    Deferred,
    Rejected,
    Replied,
    NoReply,
}

#[derive(Debug, Clone, Serialize)]
pub struct Evidence {
    pub comment_id: u64,
    pub login: String,
    pub created_at: Option<String>,
    pub url: Option<String>,
    /// References mentioned in the reply, not independently inspected commits.
    pub commit_refs: Vec<String>,
    pub outcome: Outcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assessment: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Posted {
    pub id: String,
    pub severity: String,
    pub confidence: Option<u32>,
    pub has_code: bool,
    /// Latest substantive author report; bot assessments never overwrite this.
    pub outcome: Outcome,
    pub reviewer: String,
    pub pr: Option<u64>,
    pub source: Evidence,
    pub author_known: bool,
    pub author_reported: Vec<Evidence>,
    pub participant_reported: Vec<Evidence>,
    pub bot_assessed: Vec<Evidence>,
    /// This collector does not inspect code, so it never populates this field.
    pub commit_verified: Vec<Evidence>,
}

fn contains_any(s: &str, phrases: &[&str]) -> bool {
    phrases.iter().any(|p| s.contains(p))
}

fn reply_outcome(body: &str) -> Outcome {
    // Quoted reports and code examples are not the speaker's own claims.
    let mut code = false;
    let text = body
        .lines()
        .filter(|line| {
            if line.trim_start().starts_with("```") {
                code = !code;
                return false;
            }
            !code && !line.trim_start().starts_with('>')
        })
        .collect::<Vec<_>>()
        .join("\n")
        .to_lowercase()
        .replace('’', "'")
        .replace(" but ", ".")
        .replace("하지만", ".");
    let mut fixed = false;
    let mut rejected = false;
    let mut deferred = false;
    let mut partial = false;
    for clause in text
        .split(['.', '\n', ';', ','])
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if clause.ends_with('?') {
            continue;
        }
        let negative = contains_any(
            clause,
            &[
                "반영하지 않",
                "반영은 하지",
                "수정하지 않",
                "적용하지 않",
                "그대로 두",
                "기존 구현을 유지",
                "기존 동작을 유지",
                "의도한",
                "의도된",
                "철회",
                "not fixed",
                "not done",
                "not implemented",
                "not corrected",
                "did not fix",
                "didn't fix",
                "won't fix",
                "wont fix",
            ],
        );
        let future = contains_any(
            clause,
            &[
                "보류",
                "수정할 예정",
                "반영할 예정",
                "적용할 예정",
                "수정하겠습니다",
                "반영하겠습니다",
                "적용하겠습니다",
                "나중에",
                "후속 개선",
                "넣겠습니다",
                "will fix",
                "will apply",
                "deferred",
                "plan to fix",
                "not yet fixed",
                "will be fixed",
                "haven't fixed",
                "hasn't fixed",
                "haven't implemented",
                "hasn't implemented",
                "have not yet",
                "has not yet",
                "not yet done",
                "not yet implemented",
                "not yet corrected",
                "not implemented yet",
                "still broken",
                "still unresolved",
                "remains unresolved",
                "미해결",
                "아직 반영하지",
                "아직 수정하지",
                "아직 적용하지",
            ],
        );
        let words: Vec<_> = clause.split(|c: char| !c.is_alphanumeric()).collect();
        let completed = contains_any(
            clause,
            &[
                "수정:",
                "수정 :",
                "수정했",
                "수정완료",
                "수정 완료",
                "반영했",
                "반영완료",
                "반영 완료",
                "적용했",
                "고쳤",
                "지웠",
                "걷어냈",
                "추가했",
                "바꿨",
                "정정했",
                "맞췄",
                "넣었습니다",
                "갱신했",
            ],
        ) || words
            .iter()
            .any(|w| ["fixed", "done", "implemented", "corrected"].contains(w));
        rejected |= negative && !future;
        deferred |= future;
        // Negation/future wins within a clause; separate positive clauses can
        // still establish a partial response instead of an inflated full fix.
        fixed |= completed && !negative && !future;
        partial |= contains_any(
            clause,
            &[
                "일부만",
                "부분 반영",
                "부분 수정",
                "partially",
                "partial fix",
            ],
        ) && !negative
            && !future;
    }
    if partial || (fixed && (rejected || deferred)) {
        Outcome::Partial
    } else if rejected {
        Outcome::Rejected
    } else if deferred {
        Outcome::Deferred
    } else if fixed {
        Outcome::Fixed
    } else {
        Outcome::Replied
    }
}

fn commit_refs(body: &str) -> Vec<String> {
    let mut refs = Vec::new();
    // Split ASCII identifiers from Korean suffixes (e.g. abc1234에서).
    for token in body.split(|c: char| !c.is_ascii_alphanumeric() && c != '_') {
        if (7..=40).contains(&token.len())
            && token.bytes().all(|b| b.is_ascii_hexdigit())
            && !refs.iter().any(|r| r == token)
        {
            refs.push(token.to_lowercase());
        }
    }
    refs
}

fn evidence(
    c: &InlineThreadComment,
    outcome: Outcome,
    repo: Option<&str>,
    pr: Option<u64>,
) -> Evidence {
    Evidence {
        comment_id: c.id,
        login: c.user.login.clone(),
        created_at: c.created_at.clone(),
        url: repo
            .zip(pr)
            .map(|(r, p)| format!("https://github.com/{r}/pull/{p}#discussion_r{}", c.id)),
        commit_refs: commit_refs(&c.body),
        outcome,
        assessment: None,
    }
}

fn latest(evidence: &[Evidence]) -> Outcome {
    evidence
        .iter()
        .rev()
        .find(|e| e.outcome != Outcome::Replied)
        .map(|e| e.outcome)
        .unwrap_or(if evidence.is_empty() {
            Outcome::NoReply
        } else {
            Outcome::Replied
        })
}

/// Legacy helper: absent a PR author identity, replies remain participant reports.
#[cfg(test)]
pub fn classify(comments: &[InlineThreadComment], me: &str) -> Vec<Posted> {
    classify_pr(comments, Some(me), None, None, None, false)
}

fn classify_pr(
    comments: &[InlineThreadComment],
    me: Option<&str>,
    author: Option<&str>,
    repo: Option<&str>,
    pr: Option<u64>,
    all: bool,
) -> Vec<Posted> {
    let mut out: Vec<Posted> = Vec::new();
    for root in comments {
        if root.in_reply_to_id.is_some()
            || (!all && !me.is_some_and(|m| root.user.login.eq_ignore_ascii_case(m)))
        {
            continue;
        }
        let id = match marker_id(&root.body) {
            Some(id) => id,
            None if all => format!("comment:{}", root.id),
            None => continue,
        };
        if out.iter().any(|p| p.id == id) {
            continue;
        }
        let mut head = crate::review::parse_inline_head(&root.body);
        // Untagged third-party review comments have unknown severity, not an
        // automatic question label that would erase them from legacy reports.
        if all
            && marker_id(&root.body).is_none()
            && head.severity == "question"
            && !root.body.contains("**question**")
            && !root.body.contains("sev:question")
        {
            head.severity = "unknown".into();
        }
        let mut replies: Vec<_> = comments
            .iter()
            .filter(|c| c.in_reply_to_id == Some(root.id))
            .collect();
        // RFC3339 offsets are normalized. Missing dates fall back to GitHub IDs.
        replies.sort_by_key(|r| {
            (
                r.created_at
                    .as_deref()
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                    .map(|t| t.timestamp_millis())
                    .unwrap_or(i64::MAX),
                r.id,
            )
        });
        let mut author_reported = Vec::new();
        let mut participant_reported = Vec::new();
        let mut bot_assessed = Vec::new();
        for reply in replies {
            if reply.user.login.eq_ignore_ascii_case(&root.user.login) {
                let verdict = [
                    ("resolved", Outcome::Fixed),
                    ("partial", Outcome::Partial),
                    ("wont_fix", Outcome::Rejected),
                    ("unresolved", Outcome::Replied),
                ]
                .into_iter()
                .find(|(s, _)| reply.body.contains(&format!("<!-- fu:{s} -->")));
                if let Some((status, outcome)) = verdict {
                    let mut e = evidence(reply, outcome, repo, pr);
                    e.assessment = Some(status.into());
                    bot_assessed.push(e);
                }
            } else {
                let e = evidence(reply, reply_outcome(&reply.body), repo, pr);
                if author.is_some_and(|a| reply.user.login.eq_ignore_ascii_case(a)) {
                    author_reported.push(e);
                } else {
                    participant_reported.push(e);
                }
            }
        }
        let outcome = latest(if author.is_some() {
            &author_reported
        } else {
            &participant_reported
        });
        out.push(Posted {
            id,
            severity: head.severity,
            confidence: head.confidence,
            has_code: root.body.contains("```"),
            outcome,
            reviewer: root.user.login.clone(),
            pr,
            source: evidence(root, Outcome::NoReply, repo, pr),
            author_known: author.is_some(),
            author_reported,
            participant_reported,
            bot_assessed,
            commit_verified: Vec::new(),
        });
    }
    out
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct Tally {
    pub n: usize,
    pub fixed: usize,
    pub partial: usize,
    pub deferred: usize,
    pub rejected: usize,
    pub replied: usize,
    pub no_reply: usize,
}
impl Tally {
    fn add(&mut self, o: Outcome) {
        self.n += 1;
        match o {
            Outcome::Fixed => self.fixed += 1,
            Outcome::Partial => self.partial += 1,
            Outcome::Deferred => self.deferred += 1,
            Outcome::Rejected => self.rejected += 1,
            Outcome::Replied => self.replied += 1,
            Outcome::NoReply => self.no_reply += 1,
        }
    }
    fn pct(part: usize, whole: usize) -> usize {
        if whole == 0 {
            0
        } else {
            part * 100 / whole
        }
    }
}
fn band(c: Option<u32>) -> &'static str {
    match c {
        Some(90..) => "확신 90+",
        Some(80..=89) => "확신 80~89",
        Some(70..=79) => "확신 70~79",
        Some(_) => "확신 70 미만",
        None => "확신 없음",
    }
}
fn group_by(posted: &[Posted], key: impl Fn(&Posted) -> String) -> BTreeMap<String, Tally> {
    let mut m: BTreeMap<String, Tally> = BTreeMap::new();
    for p in posted {
        m.entry(key(p)).or_default().add(p.outcome);
    }
    m
}
fn table(title: &str, groups: &BTreeMap<String, Tally>) -> String {
    let mut s = format!("\n{title}\n| 구분 | 지적 | 수정 보고 | 수정% | 거절% | 무응답% | 부분 | 보류 |\n|---|--:|--:|--:|--:|--:|--:|--:|\n");
    for (k, t) in groups {
        s.push_str(&format!(
            "| {k} | {} | {} | {} | {} | {} | {} | {} |\n",
            t.n,
            t.fixed,
            Tally::pct(t.fixed, t.n),
            Tally::pct(t.rejected, t.n),
            Tally::pct(t.no_reply, t.n),
            t.partial,
            t.deferred
        ));
    }
    s
}
fn summary(posted: &[Posted]) -> serde_json::Value {
    let mut resolution = Tally::default();
    let mut questions = Tally::default();
    for p in posted {
        if p.severity == "question" {
            questions.add(p.outcome);
        } else {
            resolution.add(p.outcome);
        }
    }
    serde_json::json!({"resolution":resolution,"questions":questions,"basis":"latest_author_report; question_excluded; not_code_verification"})
}
pub fn summarize(posted: &[Posted]) -> String {
    if posted.is_empty() {
        return "내가 게시한 지적(f:id 마커)을 찾지 못함.\n".into();
    }
    let resolution: Vec<_> = posted
        .iter()
        .filter(|p| p.severity != "question")
        .cloned()
        .collect();
    let mut out = format!(
        "게시한 지적 {}건 (resolution 분모 {}, question {}건 별도)\n",
        posted.len(),
        resolution.len(),
        posted.len() - resolution.len()
    );
    out.push_str(&table(
        "심각도별 (question 별도)",
        &group_by(posted, |p| p.severity.clone()),
    ));
    out.push_str(&table(
        "코드 블록 (question 제외)",
        &group_by(&resolution, |p| {
            if p.has_code { "있음" } else { "없음" }.into()
        }),
    ));
    out.push_str(&table(
        "확신도별 (question 제외)",
        &group_by(&resolution, |p| band(p.confidence).into()),
    ));
    out.push_str("\n※ 수정%는 작성자 보고의 휴리스틱 분류이며 코드 검증률이 아니다. 부분·보류는 수정에 포함하지 않는다. 답글 없는 수정은 미확인이다.\n");
    out
}

#[derive(Deserialize)]
struct InputPull {
    number: u64,
    author: String,
    comments: Vec<InlineThreadComment>,
}
#[derive(Deserialize)]
struct Input {
    pulls: Vec<InputPull>,
}
fn render(
    repo: &str,
    posted: &[Posted],
    pulls: usize,
    errors: &[String],
    format: &str,
) -> anyhow::Result<String> {
    if format == "json" {
        Ok(serde_json::to_string_pretty(
            &serde_json::json!({"schema_version":1,"repo":repo,"pulls":pulls,"incomplete":!errors.is_empty(),"errors":errors,"summary":summary(posted),"findings":posted}),
        )?)
    } else {
        Ok(format!("{repo} PR {pulls}건\n{}", summarize(posted)))
    }
}

/// Online collection or offline JSON: {"pulls":[{"number":1,"author":"login","comments":[GitHub REST comment]}]}.
/// --input - reads stdin; --all-reviewers includes unmarked roots for thread-stats.
pub fn run_cli(flags: &[String]) -> anyhow::Result<String> {
    let mut repo = String::new();
    let mut limit: usize = 50;
    let mut me = None;
    let mut input = None;
    let mut format = "text".to_string();
    let mut all = false;
    let mut i = 0;
    while i < flags.len() {
        let name = flags[i].as_str();
        let value = |i: &mut usize| -> anyhow::Result<String> {
            *i += 1;
            flags
                .get(*i)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("{name} needs a value"))
        };
        match name {
            "--repo" => repo = value(&mut i)?,
            "--limit" => limit = value(&mut i)?.parse()?,
            "--me" => me = Some(value(&mut i)?),
            "--input" => input = Some(value(&mut i)?),
            "--format" => format = value(&mut i)?,
            "--all-reviewers" => all = true,
            other => anyhow::bail!("unknown flag: {other}"),
        }
        i += 1;
    }
    anyhow::ensure!(
        ["text", "json"].contains(&format.as_str()),
        "--format must be text or json"
    );
    if let Some(path) = input {
        anyhow::ensure!(
            me.is_some() || all,
            "--input requires --me or --all-reviewers"
        );
        let data: Input = if path == "-" {
            serde_json::from_reader(std::io::stdin().lock())?
        } else {
            serde_json::from_reader(std::fs::File::open(path)?)?
        };
        let posted: Vec<_> = data
            .pulls
            .iter()
            .flat_map(|p| {
                classify_pr(
                    &p.comments,
                    me.as_deref(),
                    Some(&p.author),
                    if repo.is_empty() { None } else { Some(&repo) },
                    Some(p.number),
                    all,
                )
            })
            .collect();
        return render(&repo, &posted, data.pulls.len(), &[], &format);
    }
    let (owner, name) = crate::github::split_repo(&repo)?;
    let client = crate::github::GitHubClient::new(crate::auth::fetch_gh_token()?);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let me = match me {
            Some(m) => m,
            None => client.get_user().await?.0.login,
        };
        let pulls = client.list_recent_closed_pulls(owner, name, limit).await?;
        let mut posted = Vec::new();
        let mut errors = Vec::new();
        for pr in &pulls {
            match client.list_review_comments(owner, name, pr.number).await {
                Ok(comments) => posted.extend(classify_pr(
                    &comments,
                    Some(&me),
                    Some(&pr.user.login),
                    Some(&repo),
                    Some(pr.number),
                    all,
                )),
                Err(e) => {
                    let message = format!("PR {} 건너뜀: {e}", pr.number);
                    eprintln!("{message}");
                    errors.push(message);
                }
            }
        }
        render(&repo, &posted, pulls.len(), &errors, &format)
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::GhUser;

    fn c(id: u64, login: &str, body: &str, reply_to: Option<u64>) -> InlineThreadComment {
        InlineThreadComment {
            id,
            user: GhUser {
                login: login.into(),
            },
            path: "src/a.ts".into(),
            line: Some(10),
            original_line: None,
            body: body.into(),
            in_reply_to_id: reply_to,
            created_at: None,
            subject_type: None,
        }
    }

    const FIXED: &str = "🟡 **major** (확신 85) — 빈 목록이 통과\n\n**수정**: length 도 검사\n\n```suggestion\nx\n```\n\n<!-- f:aaaa1111 -->";
    const BARE: &str = "🔵 **minor** (확신 72) — 이름\n\n<!-- f:bbbb2222 -->";
    const REJ: &str = "🟡 **major** (확신 90) — 정책 위반\n\n**수정**: 바꿈\n\n<!-- f:cccc3333 -->";

    #[test]
    fn classifies_each_thread_by_the_authors_reply() {
        let comments = vec![
            c(1, "bot", FIXED, None),
            c(2, "author", "**수정: abc123** 조건을 추가했습니다", Some(1)),
            c(3, "bot", BARE, None),
            c(4, "bot", REJ, None),
            c(5, "author", "이 레포 정책상 반영하지 않습니다", Some(4)),
            c(6, "human", "다른 사람 코멘트", None),
        ];
        let p = classify(&comments, "bot");
        assert_eq!(p.len(), 3, "only my marked threads");
        assert_eq!(p[0].outcome, Outcome::Fixed);
        assert_eq!(
            (p[0].severity.as_str(), p[0].confidence, p[0].has_code),
            ("major", Some(85), true)
        );
        assert_eq!(p[1].outcome, Outcome::NoReply);
        assert_eq!(
            (p[1].severity.as_str(), p[1].confidence, p[1].has_code),
            ("minor", Some(72), false)
        );
        assert_eq!(p[2].outcome, Outcome::Rejected);
    }

    #[test]
    fn my_own_follow_up_replies_do_not_count_as_the_authors_answer() {
        let comments = vec![
            c(1, "bot", BARE, None),
            c(2, "bot", "수정되었는지 다시 확인했습니다", Some(1)),
        ];
        assert_eq!(classify(&comments, "bot")[0].outcome, Outcome::NoReply);
    }

    #[test]
    fn mixed_replies_are_partial_and_unrelated_replies_are_replied() {
        let both = vec![
            c(1, "bot", BARE, None),
            c(2, "a", "수정: 1건. 나머지는 반영하지 않음", Some(1)),
        ];
        assert_eq!(
            format!("{:?}", classify(&both, "bot")[0].outcome),
            "Partial"
        );
        let other = vec![
            c(1, "bot", BARE, None),
            c(2, "a", "이건 왜 그런가요?", Some(1)),
        ];
        assert_eq!(classify(&other, "bot")[0].outcome, Outcome::Replied);
    }

    #[test]
    fn duplicate_ids_count_once_and_summary_has_rates() {
        let comments = vec![
            c(1, "bot", FIXED, None),
            c(2, "bot", FIXED, None),
            c(3, "a", "수정: x", Some(1)),
        ];
        let p = classify(&comments, "bot");
        assert_eq!(p.len(), 1);
        let s = summarize(&p);
        assert!(s.contains("게시한 지적 1건"));
        assert!(s.contains("| major | 1 | 1 | 100 | 0 | 0 |"), "{s}");
        assert!(s.contains("| 있음 | 1 | 1 | 100 | 0 | 0 |"), "{s}");
        assert!(s.contains("확신 80~89"));
    }

    #[test]
    fn empty_input_says_so() {
        assert!(summarize(&[]).contains("찾지 못함"));
    }

    #[test]
    fn new_format_threads_keep_severity_and_confidence() {
        let mut f = crate::review::Finding::default();
        f.severity = "minor".into();
        f.confidence = Some(77.0);
        f.title = "쿼리가 사라집니다".into();
        f.claim = "pathname 만 담습니다.".into();
        f.fix = "search 도 담습니다.".into();
        let body = crate::review::render_inline_for_test(&f);
        let p = classify(&[c(1, "bot", &body, None)], "bot");
        assert_eq!(
            (p[0].severity.as_str(), p[0].confidence),
            ("minor", Some(77))
        );
    }
    #[test]
    fn completed_reports_accept_prefixes_but_negation_and_intentions_do_not() {
        for (body, expected) in [
            ("abc1234에서 반영했습니다", "Fixed"),
            ("✅ 반영했습니다", "Fixed"),
            ("주석을 정정했습니다 (abc1234)", "Fixed"),
            ("README 를 코드와 맞췄습니다", "Fixed"),
            ("레거시 경로를 넣었습니다", "Fixed"),
            ("반영하지 않겠습니다", "Rejected"),
            ("not fixed", "Rejected"),
            ("수정할 예정입니다", "Deferred"),
            ("will fix later", "Deferred"),
            ("일부만 수정했습니다", "Partial"),
            ("수정했습니다. 나머지는 반영하지 않습니다", "Partial"),
            ("> fixed\n아직 확인 중입니다", "Replied"),
        ] {
            let p = classify(&[c(1, "bot", BARE, None), c(2, "a", body, Some(1))], "bot");
            assert_eq!(format!("{:?}", p[0].outcome), expected, "{body}");
        }
    }

    #[test]
    fn latest_substantive_reply_wins_in_chronological_order() {
        let mut earlier = c(30, "a", "fixed", Some(1));
        earlier.created_at = Some("2026-10-14T01:00:00Z".into());
        let mut later = c(20, "a", "반영하지 않겠습니다", Some(1));
        later.created_at = Some("2026-10-14T02:00:00Z".into());
        let p = classify(
            &[
                c(1, "bot", BARE, None),
                later,
                earlier,
                c(40, "a", "감사합니다", Some(1)),
            ],
            "bot",
        );
        assert_eq!(p[0].outcome, Outcome::Rejected);
    }

    #[test]
    fn offline_export_separates_evidence_and_excludes_questions() {
        let path =
            std::env::temp_dir().join(format!("feedback-evidence-{}.json", std::process::id()));
        let data = serde_json::json!({"pulls": [{"number": 12, "author": "author", "comments": [
            {"id":1,"user":{"login":"bot"},"path":"a","body":"<!-- f:aaaa1111 -->\n<!-- meta sev:minor c:80 -->","created_at":"2026-10-14T00:00:00Z"},
            {"id":2,"user":{"login":"author"},"path":"a","body":"✅ fixed in abc1234","in_reply_to_id":1,"created_at":"2026-10-14T01:00:00Z"},
            {"id":3,"user":{"login":"bot"},"path":"a","body":"abc1234 <!-- fu:resolved -->","in_reply_to_id":1},
            {"id":4,"user":{"login":"bot"},"path":"a","body":"❓ **question** — why?\n<!-- f:bbbb2222 -->"},
            {"id":5,"user":{"login":"other"},"path":"a","body":"fixed","in_reply_to_id":4}
        ]} ]});
        std::fs::write(&path, data.to_string()).unwrap();
        let result = run_cli(&[
            "--input".into(),
            path.to_string_lossy().into(),
            "--me".into(),
            "bot".into(),
            "--repo".into(),
            "example/project".into(),
            "--format".into(),
            "json".into(),
        ]);
        std::fs::remove_file(path).unwrap();
        let json: serde_json::Value =
            serde_json::from_str(&result.expect("offline JSON export")).unwrap();
        assert_eq!(json["summary"]["resolution"]["n"], 1);
        assert_eq!(json["summary"]["resolution"]["fixed"], 1);
        assert_eq!(json["summary"]["questions"]["n"], 1);
        assert_eq!(json["findings"][0]["author_reported"][0]["comment_id"], 2);
        assert_eq!(
            json["findings"][0]["author_reported"][0]["commit_refs"][0],
            "abc1234"
        );
        assert_eq!(
            json["findings"][0]["author_reported"][0]["created_at"],
            "2026-10-14T01:00:00Z"
        );
        assert_eq!(json["findings"][0]["bot_assessed"][0]["outcome"], "fixed");
        assert_eq!(
            json["findings"][0]["commit_verified"],
            serde_json::json!([])
        );
        assert_eq!(json["findings"][1]["outcome"], "no_reply");
    }

    #[test]
    fn promises_negation_and_preserved_behavior_do_not_inflate_or_dilute_fixes() {
        for (body, expected) in [
            ("not yet fixed", "Deferred"),
            ("will be fixed tomorrow", "Deferred"),
            ("fixed the hint, but won't fix the policy", "Partial"),
            ("반영했습니다. 취소 안내를 유지합니다", "Fixed"),
            ("only partially fixed", "Partial"),
            ("일부만 반영하겠습니다", "Deferred"),
        ] {
            assert_eq!(format!("{:?}", reply_outcome(body)), expected, "{body}");
        }
    }

    #[test]
    fn later_fix_supersedes_deferral_and_bot_disagreement_stays_separate() {
        let p = classify_pr(
            &[
                c(1, "bot", BARE, None),
                c(2, "author", "보류합니다", Some(1)),
                c(3, "bot", "<!-- fu:wont_fix -->", Some(1)),
                c(4, "author", "abc1234에서 반영했습니다", Some(1)),
            ],
            Some("bot"),
            Some("author"),
            None,
            Some(1),
            false,
        );
        assert_eq!(p[0].outcome, Outcome::Fixed);
        assert_eq!(p[0].author_reported.len(), 2);
        assert_eq!(p[0].bot_assessed[0].outcome, Outcome::Rejected);
        assert!(p[0].commit_verified.is_empty());
    }

    #[test]
    fn all_reviewer_export_preserves_unknown_severity_and_bot_status() {
        let p = classify_pr(
            &[
                c(1, "reviewer", "This branch drops a required field.", None),
                c(2, "reviewer", "<!-- fu:unresolved -->", Some(1)),
            ],
            None,
            Some("author"),
            None,
            Some(1),
            true,
        );
        assert_eq!(p[0].severity, "unknown");
        let json = serde_json::to_value(&p[0]).unwrap();
        assert_eq!(json["bot_assessed"][0]["assessment"], "unresolved");
        assert_eq!(summary(&p)["resolution"]["n"], 1);
    }
    #[test]
    fn unresolved_and_negated_completion_reports_never_count_as_fixed() {
        for (body, expected) in [
            ("Not implemented.", Outcome::Rejected),
            ("I haven't fixed it.", Outcome::Deferred),
            ("This is not yet done.", Outcome::Deferred),
            ("Fixed A, but B is still broken.", Outcome::Partial),
            ("아직 반영하지 않았습니다.", Outcome::Deferred),
            (
                "일부 수정했습니다. 나머지는 미해결입니다.",
                Outcome::Partial,
            ),
            ("Fixed A, B remains unresolved.", Outcome::Partial),
            (
                "I have not implemented the requested change.",
                Outcome::Rejected,
            ),
        ] {
            assert_eq!(reply_outcome(body), expected, "{body}");
        }
    }
}
