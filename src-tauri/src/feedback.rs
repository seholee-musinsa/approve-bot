//! Operational feedback: what happened to the findings the bot posted.
//!
//! Every inline finding carries `<!-- f:<id> -->` (see `review::render_inline`),
//! so the bot's own comments are enough to recover what it said (severity,
//! confidence, whether it came with code) and how the author answered in the
//! thread. Nothing is stored: `approve-bot feedback --repo owner/name` reads
//! GitHub and prints a table to stdout. Output quotes no PR content, but it is
//! still not meant to be committed (public repo).
//!
//! The author's answer is read from the first word, the way people reply
//! here: `수정: <sha>` / `반영했습니다` = fixed, `반영하지 않` / `의도한` = rejected.
//! A fix made without a reply is invisible, so every rate is a lower bound.

use crate::github::InlineThreadComment;
use crate::rereview::marker_id;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Fixed,
    Rejected,
    /// Someone answered, but neither as a fix nor as a rejection.
    Replied,
    NoReply,
}

#[derive(Debug, Clone)]
pub struct Posted {
    pub id: String,
    pub severity: String,
    pub confidence: Option<u32>,
    /// A ```suggestion or plain code block came with the finding.
    pub has_code: bool,
    pub outcome: Outcome,
}

const FIXED_LEADS: &[&str] = &["수정", "반영", "적용", "고쳤", "지웠", "걷어", "추가했", "바꿨", "fixed", "done"];
const REJECTED_PHRASES: &[&str] = &["반영하지 않", "반영은 하지", "그대로 두", "유지합니다", "의도한", "의도된", "철회"];

/// Text after leading whitespace and bold markers, lowercased.
fn lead(body: &str) -> String {
    body.trim_start().trim_start_matches('*').trim_start().to_lowercase()
}

fn outcome_of(replies: &[&InlineThreadComment]) -> Outcome {
    if replies.is_empty() {
        return Outcome::NoReply;
    }
    // Fixed wins when both appear ("수정: …, 나머지는 반영하지 않음") — the
    // finding did lead to a change.
    if replies.iter().any(|r| {
        let l = lead(&r.body);
        FIXED_LEADS.iter().any(|p| l.starts_with(p))
    }) {
        return Outcome::Fixed;
    }
    if replies.iter().any(|r| REJECTED_PHRASES.iter().any(|p| r.body.contains(p))) {
        return Outcome::Rejected;
    }
    Outcome::Replied
}

/// `🔴 **blocker** (확신 90) — …` → 90.
fn confidence_of(first_line: &str) -> Option<u32> {
    let rest = first_line.split_once("(확신 ")?.1;
    rest.split(')').next()?.trim().parse().ok()
}

/// The bot's own finding threads in one PR, with how each ended.
pub fn classify(comments: &[InlineThreadComment], me: &str) -> Vec<Posted> {
    let mut out: Vec<Posted> = Vec::new();
    for root in comments {
        if !root.user.login.eq_ignore_ascii_case(me) || root.in_reply_to_id.is_some() {
            continue;
        }
        let Some(id) = marker_id(&root.body) else { continue };
        if out.iter().any(|p| p.id == id) {
            continue;
        }
        let first = root.body.lines().next().unwrap_or("");
        let severity = ["blocker", "major", "minor", "nit"]
            .into_iter()
            .find(|s| first.contains(&format!("**{s}**")))
            .unwrap_or("question")
            .to_string();
        let replies: Vec<&InlineThreadComment> = comments
            .iter()
            .filter(|c| c.in_reply_to_id == Some(root.id) && !c.user.login.eq_ignore_ascii_case(me))
            .collect();
        out.push(Posted {
            id,
            severity,
            confidence: confidence_of(first),
            has_code: root.body.contains("```"),
            outcome: outcome_of(&replies),
        });
    }
    out
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Tally {
    pub n: usize,
    pub fixed: usize,
    pub rejected: usize,
    pub replied: usize,
}

impl Tally {
    fn add(&mut self, o: Outcome) {
        self.n += 1;
        match o {
            Outcome::Fixed => self.fixed += 1,
            Outcome::Rejected => self.rejected += 1,
            Outcome::Replied => self.replied += 1,
            Outcome::NoReply => {}
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
    let mut s = format!("\n{title}\n| 구분 | 지적 | 수정 | 수정% | 거절% | 무응답% |\n|---|--:|--:|--:|--:|--:|\n");
    for (k, t) in groups {
        let silent = t.n - t.fixed - t.rejected - t.replied;
        s.push_str(&format!(
            "| {k} | {} | {} | {} | {} | {} |\n",
            t.n,
            t.fixed,
            Tally::pct(t.fixed, t.n),
            Tally::pct(t.rejected, t.n),
            Tally::pct(silent, t.n),
        ));
    }
    s
}

/// Plain-text report. Rates are a lower bound: a fix with no reply counts as
/// no reply.
pub fn summarize(posted: &[Posted]) -> String {
    if posted.is_empty() {
        return "내가 게시한 지적(f:id 마커)을 찾지 못함.\n".to_string();
    }
    let mut out = format!("게시한 지적 {}건\n", posted.len());
    out.push_str(&table("심각도별", &group_by(posted, |p| p.severity.clone())));
    out.push_str(&table(
        "코드 블록",
        &group_by(posted, |p| if p.has_code { "있음" } else { "없음" }.to_string()),
    ));
    out.push_str(&table("확신도별", &group_by(posted, |p| band(p.confidence).to_string())));
    out.push_str("\n※ 답글 없이 고친 건 못 잡으므로 수정%는 하한이다.\n");
    out
}

/// `approve-bot feedback --repo owner/name [--limit 50] [--me login]`
pub fn run_cli(flags: &[String]) -> anyhow::Result<String> {
    let mut repo = String::new();
    let mut limit: usize = 50;
    let mut me: Option<String> = None;
    let mut i = 0;
    while i < flags.len() {
        let name = flags[i].as_str();
        let value = |i: &mut usize| -> anyhow::Result<String> {
            *i += 1;
            flags.get(*i).cloned().ok_or_else(|| anyhow::anyhow!("{name} needs a value"))
        };
        match name {
            "--repo" => repo = value(&mut i)?,
            "--limit" => limit = value(&mut i)?.parse()?,
            "--me" => me = Some(value(&mut i)?),
            other => return Err(anyhow::anyhow!("unknown flag: {other}")),
        }
        i += 1;
    }
    let (owner, name) = crate::github::split_repo(&repo)?;
    let client = crate::github::GitHubClient::new(crate::auth::fetch_gh_token()?);
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    rt.block_on(async {
        let me = match me {
            Some(m) => m,
            None => client.get_user().await?.0.login,
        };
        let pulls = client.list_recent_closed_pulls(owner, name, limit).await?;
        let mut posted = Vec::new();
        for pr in &pulls {
            match client.list_review_comments(owner, name, pr.number).await {
                Ok(comments) => posted.extend(classify(&comments, &me)),
                Err(e) => eprintln!("PR {} 건너뜀: {e}", pr.number),
            }
        }
        Ok(format!("{repo} 최근 닫힌 PR {}건, 작성자 {me}\n{}", pulls.len(), summarize(&posted)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::GhUser;

    fn c(id: u64, login: &str, body: &str, reply_to: Option<u64>) -> InlineThreadComment {
        InlineThreadComment {
            id,
            user: GhUser { login: login.into() },
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
        assert_eq!((p[0].severity.as_str(), p[0].confidence, p[0].has_code), ("major", Some(85), true));
        assert_eq!(p[1].outcome, Outcome::NoReply);
        assert_eq!((p[1].severity.as_str(), p[1].confidence, p[1].has_code), ("minor", Some(72), false));
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
    fn fixed_wins_over_rejected_and_unrelated_replies_are_replied() {
        let both = vec![c(1, "bot", BARE, None), c(2, "a", "수정: 1건. 나머지는 반영하지 않음", Some(1))];
        assert_eq!(classify(&both, "bot")[0].outcome, Outcome::Fixed);
        let other = vec![c(1, "bot", BARE, None), c(2, "a", "이건 왜 그런가요?", Some(1))];
        assert_eq!(classify(&other, "bot")[0].outcome, Outcome::Replied);
    }

    #[test]
    fn duplicate_ids_count_once_and_summary_has_rates() {
        let comments = vec![c(1, "bot", FIXED, None), c(2, "bot", FIXED, None), c(3, "a", "수정: x", Some(1))];
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
}
