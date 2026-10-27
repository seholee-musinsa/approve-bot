use crate::github::{split_repo, GitHubClient, PullRequest};
use crate::state::{ActivityEntry, ActivityKind, AppState, ConnectionStatus};
use chrono::{DateTime, Utc};
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Emitter};
use tauri_plugin_notification::NotificationExt;
use tokio::time::sleep;
use tracing::{debug, info, warn};

pub const ACTIVITY_EVENT: &str = "approve-bot://activity";
pub const STATUS_EVENT: &str = "approve-bot://status-changed";
pub const PASS_EVENT: &str = "approve-bot://pass-state";

/// Published when a poll pass starts and when it ends. A pass that runs the
/// review engine takes minutes, so the UI needs the real start/end instead of
/// guessing with a timeout.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct PassState {
    pub running: bool,
    /// 1-based index of the repository being worked on, and how many there are.
    pub repo_index: usize,
    pub repo_total: usize,
    pub repo: Option<String>,
    /// 1-based index of the PR inside that repository, and how many it has.
    pub pr_index: usize,
    pub pr_total: usize,
    pub pr_number: Option<u64>,
    pub pr_title: Option<String>,
    /// What is happening right now: `PR 목록 조회`, `확인`, `리뷰 중`, `게시`.
    pub stage: Option<String>,
    /// `Some(repo)` when a "Check now" asked for one repository only.
    pub scope: Option<String>,
    /// 0..=1 share of the pass that is done, for the progress bar.
    pub fraction: f64,
}

/// Repositories a pass covers: all of them, or just the one a "Check now"
/// named (if it is configured — a stale name runs nothing rather than all).
fn pass_targets(repositories: &[String], only: Option<&str>) -> Vec<String> {
    match only {
        None => repositories.to_vec(),
        Some(one) => repositories.iter().filter(|r| r.eq_ignore_ascii_case(one)).cloned().collect(),
    }
}

/// Share of the pass done: finished repositories count whole, the current one
/// by its PRs, and the PR being reviewed or posted counts half — a review
/// runs for minutes, and a bar that sits still looks broken. A repository
/// whose PR count is not known yet counts as 0.
fn progress_fraction(repo_index: usize, repo_total: usize, pr_index: usize, pr_total: usize, in_flight: bool) -> f64 {
    if repo_total == 0 || repo_index == 0 {
        return 0.0;
    }
    let done_repos = (repo_index - 1) as f64;
    let within = if pr_total == 0 {
        0.0
    } else {
        let done_prs = pr_index.saturating_sub(1) as f64 + if in_flight && pr_index > 0 { 0.5 } else { 0.0 };
        (done_prs / pr_total as f64).min(1.0)
    };
    ((done_repos + within) / repo_total as f64).clamp(0.0, 1.0)
}

/// Flip the shared `pass_running` flag and tell the UI about it. The flag was
/// already there for the sweep scheduler; the event is what the UI needs.
fn set_pass_running(app: &AppHandle, state: &Arc<AppState>, running: bool) {
    state
        .pass_running
        .store(running, std::sync::atomic::Ordering::SeqCst);
    let snapshot = {
        let mut p = state.pass_progress.lock().unwrap_or_else(|e| e.into_inner());
        *p = PassState { running, ..PassState::default() };
        p.clone()
    };
    let _ = app.emit(PASS_EVENT, &snapshot);
}

/// Update the live pass progress and tell the UI. Called on every repository
/// and PR step so "Check now" shows where the pass is instead of a spinner.
fn progress(app: &AppHandle, state: &Arc<AppState>, update: impl FnOnce(&mut PassState)) {
    let snapshot = {
        let mut p = state.pass_progress.lock().unwrap_or_else(|e| e.into_inner());
        update(&mut p);
        p.running = true;
        let in_flight = p.stage.as_deref().is_some_and(|s| s.starts_with("리뷰 중") || s == "게시");
        p.fraction = progress_fraction(p.repo_index, p.repo_total, p.pr_index, p.pr_total, in_flight);
        p.clone()
    };
    let _ = app.emit(PASS_EVENT, &snapshot);
}

/// Hidden marker prepended to engine-posted review bodies so later polls can
/// tell our own review apart from a blind approve or another bot's review.
/// Renders invisibly on GitHub (HTML comment).
const REVIEW_MARKER: &str = "<!-- approve-bot:review -->";

/// Marker for a review that did not finish (engine error, parse failure). It
/// stops the same head from being re-posted every poll, but unlike
/// `REVIEW_MARKER` it never counts as "reviewed" — the next commit gets a real
/// review instead of a bare approve.
const FAILED_MARKER: &str = "<!-- approve-bot:review-failed -->";
/// Backoff key shared by every PR: the model's usage limit and login state
/// are account-wide, so one failure pauses the engine everywhere.
const ENGINE_KEY: &str = "engine";
/// Non-limit failures on one head are retried this many times, then left
/// until the next push — a PR that always breaks the parser must not burn
/// money every quarter hour.
const HEAD_FAILURE_LIMIT: u32 = 3;
/// Warn once per process, not once per PR, when the explicit guide is stale.
static STALE_GUIDE_WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Which engine marker (if any) a prior review of mine carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Marker {
    None,
    Reviewed,
    Failed,
}

fn marker_of(body: &str) -> Marker {
    if body.contains(FAILED_MARKER) {
        Marker::Failed
    } else if body.contains(REVIEW_MARKER) {
        Marker::Reviewed
    } else {
        Marker::None
    }
}

/// What to do with a PR on the review path, given my prior reviews.
#[derive(Debug, PartialEq, Eq)]
enum ReviewAction {
    /// Nothing to do (already approved / already reviewed this exact head).
    Skip,
    /// My earlier engine review approved an older commit. Approve again without
    /// a review only if the new commits touch docs/tests alone; else follow up.
    ApproveOnly,
    /// My earlier engine review (not an approval) is on an older commit:
    /// follow-up review of the delta + earlier findings.
    FollowUp,
    /// Run the review engine.
    Review,
}

/// What a follow-up does when the commits since my last review touch only
/// docs or tests.
#[derive(Debug, PartialEq, Eq)]
enum DocsOnlyStep {
    /// My last review approved: the approval stands, renew it without a review.
    Reapprove,
    /// My last review held (blockers, a sensitive file, a low score). A
    /// docs/tests-only push cannot change that, so nothing is posted — PR #1
    /// spent a full review and re-check on a README wording change.
    Skip,
    /// Code changed (or this is not a follow-up): review as usual.
    Review,
}

/// The commits since my last review change nothing a review would look at:
/// only docs/tests, or no file at all (core-partner-frontend#5243 round 5 ran a
/// full Opus review, $2.60, on an empty delta). A failed compare never gets
/// here — `build_round` returns no round and the PR is reviewed in full.
fn nothing_to_review(delta_diff: &str) -> bool {
    crate::review::sensitive_files(delta_diff).is_empty()
        && (crate::diffprep::changed_paths(delta_diff).is_empty() || crate::rereview::only_docs_or_tests(delta_diff))
}

/// Follow-ups can reduce the size-driven model/second-pass budget, while the
/// full diff and checkout remain available to review and the approval gate.
/// Safety paths anywhere in the PR and unsettled serious findings set a floor.
fn review_risk(full_diff: &str, round: Option<&crate::rereview::Round>) -> (crate::routing::Risk, String) {
    use crate::routing::Risk;
    let Some(round) = round else { return crate::routing::classify(full_diff) };
    if round.prev.iter().any(|p| {
        matches!(p.severity.as_str(), "major" | "blocker")
            && !matches!(p.last_reply.as_deref(), Some("resolved" | "withdrawn"))
    }) {
        return (Risk::High, "이전 major·blocker 재확인 필요".into());
    }
    if !crate::review::sensitive_files(full_diff).is_empty()
        || !crate::review::sensitive_files(&round.delta_diff).is_empty()
    {
        return (Risk::High, "전체 PR 또는 증분의 민감 파일".into());
    }
    // Remove hunks to distinguish size-driven risk from persistent path risk.
    // Both rename sides count: moving an auth file must not lower scrutiny.
    let paths = full_diff.lines().filter_map(|line| line.strip_prefix("diff --git "))
        .flat_map(|line| line.split(' '))
        .filter_map(|path| path.strip_prefix("a/").or_else(|| path.strip_prefix("b/")))
        .map(|path| format!("diff --git a/{path} b/{path}\n"))
        .collect::<String>();
    let (safety, why) = crate::routing::classify(&paths);
    if safety == Risk::High {
        return (safety, format!("전체 PR 안전 기준: {why}"));
    }
    let (risk, why) = crate::routing::classify(&round.delta_diff);
    (risk, format!("직전 리뷰 이후 증분: {why}"))
}

fn docs_only_step(action: &ReviewAction, docs_only: bool) -> DocsOnlyStep {
    match (action, docs_only) {
        (ReviewAction::ApproveOnly, true) => DocsOnlyStep::Reapprove,
        (ReviewAction::FollowUp, true) => DocsOnlyStep::Skip,
        _ => DocsOnlyStep::Review,
    }
}

/// Pure dedup decision. `reviews` = my prior reviews, oldest first, as (state,
/// commit_id, marker). Only a finished engine review counts as "reviewed" — a
/// blind 👍 approve, another bot's review or a failed run must not suppress a
/// real review.
fn decide_review_action(
    reviews: &[(&str, Option<&str>, Marker)],
    head_sha: &str,
    approve_only_after_review: bool,
) -> ReviewAction {
    // Already approved the current head → done.
    if reviews
        .iter()
        .any(|(s, c, _)| s.eq_ignore_ascii_case("APPROVED") && *c == Some(head_sha))
    {
        return ReviewAction::Skip;
    }
    // Already engine-reviewed THIS head (live) → don't re-post. A failed run
    // on this head is not a review: the engine backoff decides when to retry.
    if reviews.iter().any(|(s, c, marker)| {
        *marker == Marker::Reviewed && !s.eq_ignore_ascii_case("DISMISSED") && *c == Some(head_sha)
    }) {
        return ReviewAction::Skip;
    }
    // Earlier commit → approve without re-review only if my latest engine run
    // finished AND approved (DISMISSED = an approval a new push dismissed).
    // A COMMENT (below threshold / blocking) or a failed run must be re-reviewed,
    // otherwise its findings are waved through unchecked.
    let latest_engine = reviews.iter().rev().find(|(_, _, m)| *m != Marker::None);
    let approved = |state: &str| state.eq_ignore_ascii_case("APPROVED") || state.eq_ignore_ascii_case("DISMISSED");
    match latest_engine {
        Some((state, _, Marker::Reviewed)) if approve_only_after_review && approved(state) => {
            ReviewAction::ApproveOnly
        }
        // Any finished engine review before → follow up on it.
        // (A dismissed review on the current head has no delta: review afresh.)
        _ if reviews
            .iter()
            .any(|(_, c, m)| *m == Marker::Reviewed && *c != Some(head_sha)) =>
        {
            ReviewAction::FollowUp
        }
        _ => ReviewAction::Review,
    }
}

/// Inputs for a follow-up round, from my latest finished engine review. None
/// (→ a full first-round review) when that commit is gone (force-push) or
/// GitHub can't diff it.
async fn build_round(
    client: &GitHubClient,
    owner: &str,
    repo: &str,
    pr: &PullRequest,
    my_reviews: &[&crate::github::Review],
    me: &str,
) -> Option<crate::rereview::Round> {
    let engine: Vec<_> = my_reviews
        .iter()
        .filter(|r| marker_of(&r.body) == Marker::Reviewed)
        .collect();
    let prev_sha = engine.last()?.commit_id.clone()?;
    let delta_diff = client
        .get_compare_diff(owner, repo, &prev_sha, &pr.head.sha)
        .await
        .ok()?;
    let comments = client
        .list_review_comments(owner, repo, pr.number)
        .await
        .ok()?;
    Some(crate::rereview::Round {
        number: engine.len() + 1,
        prev_sha,
        prev: crate::rereview::prev_findings(&comments, me),
        delta_diff,
    })
}

/// Quote-block lines for the hold reasons code enforces regardless of the
/// model's verdict (sensitive files, files the review could not see).
fn gate_notes(sensitive: &[String], omitted: &[String], continued: bool) -> String {
    let mut out = String::new();
    if !sensitive.is_empty() {
        out.push_str(&format!(
            "> ⚠️ **게이트**: 민감 파일 변경({}) — 자동 승인하지 않고 사람 승인으로 넘깁니다.\n",
            sensitive.join(", ")
        ));
    }
    if !omitted.is_empty() {
        let shown: Vec<String> = omitted.iter().take(5).cloned().collect();
        let more = if omitted.len() > 5 { format!(" 외 {}개", omitted.len() - 5) } else { String::new() };
        out.push_str(&format!(
            "> ⚠️ **게이트**: PR 이 커서 {}개 파일을 보지 못했습니다({}{more}) — 자동 승인하지 않습니다.\n",
            omitted.len(),
            shown.join(", ")
        ));
    }
    if continued {
        out.push_str(
            "> ⚠️ **게이트**: PR 이 커서 남은 파일을 따로 이어서 리뷰했습니다 — 큰 PR 은 놓치는 결함이 많아 자동 승인하지 않고 사람 승인으로 넘깁니다.\n",
        );
    }
    out
}

/// File-level threads for findings that could not be pinned to a line. Posted
/// after the review itself; a failure is logged and leaves the body's own list
/// of the finding in place. Returns how many landed.
async fn post_file_comments(
    client: &GitHubClient,
    owner: &str,
    repo: &str,
    pr: &PullRequest,
    comments: &[crate::github::FileComment],
) -> usize {
    let mut posted = 0;
    for c in comments {
        match client.post_file_comment(owner, repo, pr.number, &pr.head.sha, c).await {
            Ok(()) => posted += 1,
            Err(e) => warn!("파일 단위 코멘트 실패 {repo}#{} {}: {e}", pr.number, c.path),
        }
    }
    posted
}

/// Approve-gate verdict. Pure so every hold reason is testable.
#[derive(Debug, PartialEq, Eq)]
enum Gate {
    Approve,
    HoldUnfinished,
    HoldBlocking,
    HoldSensitive(Vec<String>),
    HoldOmitted(Vec<String>),
    HoldContinued,
    /// A confirmed major is on the page; a human decides, not the score.
    HoldMajor(usize),
    HoldVerdict,
    HoldScore,
}

/// Confidence from which a major finding holds auto-approval.
const CONFIDENT_MAJOR: f64 = 80.0;

impl Gate {
    fn reason(&self, blocking: usize, score: &str, min: f64) -> String {
        match self {
            Gate::Approve => "approve".to_string(),
            Gate::HoldUnfinished => "리뷰 미완료".to_string(),
            Gate::HoldBlocking => format!("blocking {blocking}건"),
            Gate::HoldSensitive(files) => format!("민감 파일 {}건", files.len()),
            Gate::HoldOmitted(files) => format!("미검토 파일 {}건", files.len()),
            Gate::HoldContinued => "대형 PR(이어서 리뷰)".to_string(),
            Gate::HoldMajor(n) => format!("확인한 major {n}건"),
            Gate::HoldVerdict => "판정이 approve 아님".to_string(),
            Gate::HoldScore => format!("점수 {score}/5 < {min:.1}"),
        }
    }
}

/// The one-line outcome a follow-up summary opens with. Stated from the gate,
/// not the model, so a held review never reads "Approve 합니다" — PR #1 said
/// that in every round while the .npmrc gate kept it a comment.
///
/// `counts` is the findings tally (`minor 2 · nit 1`). A held blocker gets
/// GitHub's red CAUTION box — the one case that should stop the eye.
fn outcome_line(decision: &Gate, counts: &str, blocking: usize, score: &str, min: f64) -> String {
    match decision {
        Gate::Approve => format!("✅ **승인** · {counts}"),
        Gate::HoldSensitive(files) => {
            format!("👤 **사람 승인 필요** — 민감 파일 변경({}) · {counts}", short_files(files))
        }
        Gate::HoldBlocking => format!("> [!CAUTION]\n> **머지 전에 고쳐야 할 지적이 있습니다** · {counts}"),
        Gate::HoldMajor(n) => format!("👤 **사람 확인 필요** — 확인한 major {n}건 · {counts}"),
        other => format!("⏸ **승인 보류** — {} · {counts}", other.reason(blocking, score, min)),
    }
}

/// One file named, the rest counted: `` `release.yml` 외 3개 ``. The first line
/// of ads-sdk#47 listed four full workflow paths.
fn short_files(files: &[String]) -> String {
    let name = |p: &String| p.rsplit('/').next().unwrap_or(p).to_string();
    match files {
        [] => String::new(),
        [one] => format!("`{}`", name(one)),
        [first, rest @ ..] => format!("`{}` 외 {}개", name(first), rest.len()),
    }
}

/// Whether a PR's threads need a look: yes when its `updated_at` moved since
/// the last check, or when GitHub gave none (older API shape) — then every
/// poll checks, which only costs calls on PRs that have threads.
fn needs_reply_check(last_seen: Option<&str>, updated_at: Option<&str>) -> bool {
    match updated_at {
        None => true,
        Some(now) => last_seen != Some(now),
    }
}

/// Settle my threads the author replied to without pushing. Questions close
/// on any answer, nits and minors on a fix report, a reason or a deferral; a
/// major or blocker only gets a note and waits for the push. One reply per
/// author reply (`author_reply_after_mine`), so nothing is said twice.
async fn settle_replies(
    app: &AppHandle,
    state: &Arc<AppState>,
    client: &GitHubClient,
    owner: &str,
    repo: &str,
    pr: &PullRequest,
    me: &str,
) {
    let key = format!("{owner}/{repo}#{}", pr.number);
    let stamp = pr.updated_at.clone().unwrap_or_default();
    {
        let checks = state.reply_checks.lock().await;
        if !needs_reply_check(checks.get(&key).map(String::as_str), pr.updated_at.as_deref()) {
            return;
        }
    }
    let comments = match client.list_review_comments(owner, repo, pr.number).await {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "could not read review comments; reply settlement skipped");
            return;
        }
    };
    let prev = crate::rereview::prev_findings(&comments, me);
    let author = pr.user.login.as_str();
    let candidates: Vec<(crate::rereview::PrevFinding, crate::rereview::ThreadStep)> = prev
        .into_iter()
        .filter_map(|p| {
            let reply = crate::rereview::author_reply_after_mine(&comments, p.comment_id, me, author)?;
            let step = crate::rereview::reply_step(&p.severity, crate::feedback::reply_outcome(&reply.body), p.last_reply.as_deref());
            (step != crate::rereview::ThreadStep::Leave).then_some((p, step))
        })
        .collect();
    if candidates.is_empty() {
        state.reply_checks.lock().await.insert(key, stamp);
        return;
    }
    let threads = match client.review_threads(owner, repo, pr.number).await {
        Ok(t) => t,
        Err(e) => {
            warn!(error = %e, "could not read review threads; reply settlement skipped");
            return;
        }
    };
    let head_sha = pr.head.sha.as_str();
    let mut closed: Vec<String> = Vec::new();
    let mut noted: Vec<String> = Vec::new();
    for (p, step) in candidates {
        let Some((thread_id, resolved)) = threads.get(&p.comment_id) else { continue };
        if *resolved {
            continue;
        }
        if client.ensure_publishable(owner, repo, pr.number, head_sha).await.is_err() {
            return;
        }
        match step {
            crate::rereview::ThreadStep::Close(body) => {
                if let Err(e) = client.reply_to_review_comment(owner, repo, pr.number, p.comment_id, &body).await {
                    warn!(error = %e, "settlement reply failed");
                    continue;
                }
                if let Err(e) = client.resolve_review_thread(thread_id).await {
                    warn!(error = %e, "resolving a settled thread failed");
                }
                closed.push(p.id.clone());
            }
            crate::rereview::ThreadStep::Note(body) => {
                if let Err(e) = client.reply_to_review_comment(owner, repo, pr.number, p.comment_id, &body).await {
                    warn!(error = %e, "settlement note failed");
                    continue;
                }
                noted.push(p.id.clone());
            }
            crate::rereview::ThreadStep::Leave => {}
        }
    }
    state.reply_checks.lock().await.insert(key, stamp);
    if closed.is_empty() && noted.is_empty() {
        return;
    }
    let repo_full = format!("{owner}/{repo}");
    let mut audit = crate::audit::Record::start(&repo_full, pr.number, head_sha, "none", "none", "", "none".into());
    audit.source = "settlement".into();
    audit.timing_available = true;
    audit.result = Some(serde_json::json!({"closed": closed, "noted": noted}));
    audit.update("threads_settled");
    state.record_review(&audit).await;
    let message = match (closed.len(), noted.len()) {
        (c, 0) => format!("답글을 보고 스레드 {c}건을 정리했습니다"),
        (0, n) => format!("답글에 메모 {n}건을 남겼습니다 (push 뒤 다시 확인)"),
        (c, n) => format!("답글을 보고 스레드 {c}건을 정리하고 메모 {n}건을 남겼습니다"),
    };
    push_and_emit(app, state, ActivityEntry {
        timestamp: Utc::now(), kind: ActivityKind::Info, repo: Some(repo_full),
        pr_number: Some(pr.number), pr_title: Some(pr.title.clone()), author: Some(pr.user.login.clone()),
        url: Some(pr.html_url.clone()), message, detail: None,
    }).await;
}

/// Majors that hold auto-approval: new findings at or above `CONFIDENT_MAJOR`
/// confidence, plus earlier majors the follow-up round did not settle
/// (resolved, withdrawn, or a reason not to fix). Earlier findings carry no
/// confidence, but they were posted at ≥ 70 and judged again this round.
fn count_confident_majors(
    findings: &[crate::review::Finding],
    round: Option<&crate::rereview::Round>,
    followups: &[crate::rereview::Followup],
) -> usize {
    let prev_ids: Vec<&str> = round.map(|r| r.prev.iter().map(|p| p.id.as_str()).collect()).unwrap_or_default();
    let new = findings
        .iter()
        .filter(|f| f.severity == "major" && f.confidence.unwrap_or(0.0) >= CONFIDENT_MAJOR)
        .filter(|f| !prev_ids.contains(&crate::review::finding_id(&f.path, &f.symbol, &f.claim).as_str()))
        .count();
    let open_prev = round
        .map(|r| {
            r.prev
                .iter()
                .filter(|p| p.severity == "major")
                .filter(|p| {
                    !crate::rereview::followup_for(p, followups)
                        .is_some_and(|f| matches!(f.status.as_str(), "resolved" | "withdrawn" | "wont_fix"))
                })
                .count()
        })
        .unwrap_or(0);
    new + open_prev
}

/// Hold reasons other than the one the first line already states.
fn other_notes(decision: &Gate, sensitive: &[String], omitted: &[String], continued: bool) -> String {
    let sensitive: &[String] = if matches!(decision, Gate::HoldSensitive(_)) { &[] } else { sensitive };
    let omitted: &[String] = if matches!(decision, Gate::HoldOmitted(_)) { &[] } else { omitted };
    let continued = continued && !matches!(decision, Gate::HoldContinued);
    gate_notes(sensitive, omitted, continued)
}

/// Say each earlier finding's follow-up verdict in its own thread, and resolve
/// the settled ones (see `rereview::thread_step`). Runs after the follow-up
/// review is posted. Best-effort per thread: a failure leaves that thread as
/// it was and is logged.
#[allow(clippy::too_many_arguments)]
async fn settle_threads(
    client: &GitHubClient,
    owner: &str,
    repo: &str,
    number: u64,
    round: &crate::rereview::Round,
    followups: &[crate::rereview::Followup],
    head_sha: &str,
) {
    let threads = match client.review_threads(owner, repo, number).await {
        Ok(t) => t,
        Err(e) => {
            warn!(error = %e, "could not read review threads; follow-up replies skipped");
            return;
        }
    };
    for p in &round.prev {
        let Some(f) = crate::rereview::followup_for(p, followups) else { continue };
        let Some((thread_id, resolved)) = threads.get(&p.comment_id) else { continue };
        // Someone already resolved it; a reply now would only reopen the noise.
        if *resolved {
            continue;
        }
        if client.ensure_publishable(owner, repo, number, head_sha).await.is_err() { return; }
        match crate::rereview::thread_step(&f.status, &f.note, head_sha, p.last_reply.as_deref()) {
            crate::rereview::ThreadStep::Close(body) => {
                if let Err(e) = client.reply_to_review_comment(owner, repo, number, p.comment_id, &body).await {
                    warn!(error = %e, "follow-up reply failed");
                    continue;
                }
                if client.ensure_publishable(owner, repo, number, head_sha).await.is_err() { return; }
                if let Err(e) = client.resolve_review_thread(thread_id).await {
                    warn!(error = %e, "resolving a settled thread failed");
                }
            }
            crate::rereview::ThreadStep::Note(body) => {
                if let Err(e) = client.reply_to_review_comment(owner, repo, number, p.comment_id, &body).await {
                    warn!(error = %e, "follow-up reply failed");
                }
            }
            crate::rereview::ThreadStep::Leave => {}
        }
    }
}

/// Hide the earlier rounds once the new one is up. Best-effort: a failure
/// leaves them visible, which is how it was before.
async fn hide_outdated(client: &GitHubClient, my_reviews: &[&crate::github::Review], head_sha: &str) {
    for id in outdated_reviews(my_reviews, head_sha) {
        if let Err(e) = client.minimize_review(id).await {
            warn!(error = %e, "could not hide an outdated review");
        }
    }
}

/// My earlier engine reviews that a new round makes outdated: every one not on
/// the current head, plus a failure notice an older build posted on this head
/// (the real review now replaces it). Hidden after the new round is posted, so
/// the timeline shows one live review instead of a stack of overviews.
fn outdated_reviews<'a>(reviews: &[&'a crate::github::Review], head_sha: &str) -> Vec<&'a str> {
    reviews
        .iter()
        .filter(|r| match marker_of(&r.body) {
            Marker::None => false,
            Marker::Failed => true,
            Marker::Reviewed => r.commit_id.as_deref() != Some(head_sha),
        })
        .filter(|r| !r.node_id.is_empty())
        .map(|r| r.node_id.as_str())
        .collect()
}

/// What to do after the model did not finish: which backoff key, how long,
/// and what to tell the user. Pure so the policy is testable.
#[derive(Debug, PartialEq)]
struct FailureStep {
    engine_wide: bool,
    kind: &'static str,
    delay_secs: u64,
    /// Always tell the user (give-up, auth); otherwise only when the kind changes.
    always_notify: bool,
    message: String,
}

fn model_failure_step(failure: Option<&crate::review::EngineFailure>, prior_head_attempts: u32, now: DateTime<Utc>) -> FailureStep {
    use crate::review::EngineFailure as F;
    match failure {
        Some(F::UsageLimit { reset_hint_secs }) => {
            let delay = reset_hint_secs.unwrap_or(3600);
            let until = (now + chrono::Duration::seconds(delay as i64)).with_timezone(&chrono::Local);
            FailureStep { engine_wide: true, kind: "usage_limit", delay_secs: delay, always_notify: false,
                message: format!("claude 사용 한도 도달 — {}까지 리뷰를 멈추고, 이후 자동으로 다시 시도합니다", until.format("%m/%d %H:%M")) }
        }
        Some(F::Auth) => FailureStep { engine_wide: true, kind: "auth", delay_secs: 900, always_notify: false,
            message: "claude 로그인이 만료됐습니다 — 터미널에서 `claude login` 뒤 15분 안에 다시 시도합니다".into() },
        other => {
            let attempts = prior_head_attempts + 1;
            if attempts >= HEAD_FAILURE_LIMIT {
                FailureStep { engine_wide: false, kind: "engine_gave_up", delay_secs: 7 * 86_400, always_notify: true,
                    message: format!("같은 커밋에서 리뷰 실행이 {attempts}회 실패해 다음 push 까지 멈춥니다") }
            } else {
                let what = match other { Some(F::Timeout) => "시간 초과", Some(F::Parse) => "출력 파싱 실패", _ => "실행 실패" };
                FailureStep { engine_wide: false, kind: "engine", delay_secs: 900 * attempts as u64, always_notify: false,
                    message: format!("리뷰 {what} ({attempts}/{HEAD_FAILURE_LIMIT}) — 15분 뒤 다시 시도합니다") }
            }
        }
    }
}

/// Approve only when every condition holds. `blocking_issues` is checked here
/// even though the prompt tells the model not to approve with blockers — a
/// self-contradicting verdict must not slip through.
fn gate(
    finished_cleanly: bool,
    verdict: &str,
    score: f64,
    blocking: usize,
    sensitive: &[String],
    omitted: &[String],
    continued: bool,
    confident_majors: usize,
    min_score: f64,
) -> Gate {
    if !finished_cleanly {
        Gate::HoldUnfinished
    } else if blocking > 0 {
        Gate::HoldBlocking
    } else if !sensitive.is_empty() {
        Gate::HoldSensitive(sensitive.to_vec())
    } else if !omitted.is_empty() {
        // Approving files nobody read would be a blind approve.
        Gate::HoldOmitted(omitted.to_vec())
    } else if continued {
        // Review quality drops on big PRs (recall ~8% above 150KB in eval), so
        // even a fully read one is left to a human.
        Gate::HoldContinued
    } else if confident_majors > 0 {
        // The score would still pass (a major costs 0.5), but a confirmed
        // defect is for a human to weigh, not to wave through at 4.5. 20
        // such approvals went out between 10/15 and 10/27.
        Gate::HoldMajor(confident_majors)
    } else if verdict != "approve" {
        Gate::HoldVerdict
    } else if score < min_score {
        Gate::HoldScore
    } else {
        Gate::Approve
    }
}

pub fn spawn(app: AppHandle, state: Arc<AppState>) {
    tauri::async_runtime::spawn(async move {
        run_loop(app, state).await;
    });
}

async fn run_loop(app: AppHandle, state: Arc<AppState>) {
    info!("poller loop started");
    loop {
        let interval_secs = {
            let cfg = state.config.lock().await;
            cfg.polling_interval_seconds
        };

        // Not connected → connect first, then fall through to the pass in the
        // SAME iteration. Connecting used to *replace* the pass, so a cold start
        // (the token is None until the first connect) or a "Check now" on a
        // disconnected app only reconnected and then slept out the whole
        // interval before looking at a single PR.
        if state.token.lock().await.is_none() {
            try_connect(&app, &state).await;
        }
        if state.token.lock().await.is_some() {
            set_pass_running(&app, &state, true);
            run_one_pass(&app, &state).await;
            set_pass_running(&app, &state, false);
        }

        // Either sleep for the configured interval or wake early on user action.
        tokio::select! {
            _ = sleep(Duration::from_secs(interval_secs)) => {}
            _ = state.poll_signal.notified() => {
                debug!("poll signal received, running immediately");
            }
        }
    }
}

pub async fn try_connect(app: &AppHandle, state: &Arc<AppState>) {
    let token_result = tokio::task::spawn_blocking(crate::auth::fetch_gh_token)
        .await
        .ok()
        .and_then(|r| r.ok());

    let Some(token) = token_result else {
        let status = ConnectionStatus::disconnected(
            "Could not fetch token from gh CLI. Run `gh auth login`.",
        );
        *state.status.lock().await = status.clone();
        let _ = app.emit(STATUS_EVENT, &status);
        return;
    };

    let client = GitHubClient::new(token.clone());
    match client.get_user().await {
        Ok((user, rate)) => {
            *state.token.lock().await = Some(token);
            *state.current_user.lock().await = Some(user.login.clone());
            let engine_paused_until = state.status.lock().await.engine_paused_until;
            let status = ConnectionStatus {
                connected: true,
                username: Some(user.login.clone()),
                rate_limit_remaining: rate.remaining,
                rate_limit_total: rate.limit,
                last_error: None,
                checked_at: Utc::now(),
                engine_paused_until,
            };
            *state.status.lock().await = status.clone();
            let _ = app.emit(STATUS_EVENT, &status);
            info!(user = %user.login, "connected to GitHub");
        }
        Err(e) => {
            let status =
                ConnectionStatus::disconnected(format!("GitHub auth check failed: {e:#}"));
            *state.status.lock().await = status.clone();
            let _ = app.emit(STATUS_EVENT, &status);
            warn!(error = %e, "connect failed");
        }
    }
}

async fn run_one_pass(app: &AppHandle, state: &Arc<AppState>) {
    let (cfg, token, current_user) = {
        let cfg = state.config.lock().await.clone();
        let token = state.token.lock().await.clone();
        let me = state.current_user.lock().await.clone();
        (cfg, token, me)
    };

    let Some(token) = token else { return };
    let Some(me) = current_user else { return };

    let client = GitHubClient::new(token);
    let allowed: std::collections::HashSet<String> =
        cfg.allowed_authors.iter().cloned().collect();

    let mut latest_rate: Option<(Option<u64>, Option<u64>)> = None;

    // A "Check now" for one repository narrows this pass; the automatic pass
    // and a plain "Check now" cover every configured repository.
    let scope = state.check_scope.lock().unwrap_or_else(|e| e.into_inner()).take();
    let targets = pass_targets(&cfg.repositories, scope.as_deref());
    let repo_total = targets.len();
    for (repo_i, repo_full) in targets.iter().enumerate() {
        progress(app, state, |p| {
            p.scope = scope.clone();
            p.repo_index = repo_i + 1;
            p.repo_total = repo_total;
            p.repo = Some(repo_full.clone());
            p.pr_index = 0;
            p.pr_total = 0;
            p.pr_number = None;
            p.pr_title = None;
            p.stage = Some("PR 목록 조회".into());
        });
        if !state.retries.lock().await.ready(repo_full, Utc::now().timestamp()) { continue; }

        let (owner, repo) = match split_repo(repo_full) {
            Ok(parts) => parts,
            Err(e) => {
                push_and_emit(
                    app,
                    state,
                    ActivityEntry {
                        timestamp: Utc::now(),
                        kind: ActivityKind::Error,
                        repo: Some(repo_full.clone()),
                        pr_number: None,
                        pr_title: None,
                        author: None,
                        url: None,
                        message: format!("invalid repo entry: {e}"),
                        detail: None,
                    },
                )
                .await;
                continue;
            }
        };

        let (pulls, rate) = match client.list_open_pulls(owner, repo).await {
            Ok(v) => v,
            Err(e) => {
                let notify = state.retries.lock().await.failure(repo_full, Utc::now().timestamp(), crate::retry::hint(&e), crate::retry::kind(&e));
                if !notify { continue; }
                push_and_emit(
                    app,
                    state,
                    ActivityEntry {
                        timestamp: Utc::now(),
                        kind: ActivityKind::Error,
                        repo: Some(repo_full.clone()),
                        pr_number: None,
                        pr_title: None,
                        author: None,
                        url: None,
                        message: format!("list pulls failed: {e:#}"),
                        detail: None,
                    },
                )
                .await;
                continue;
            }
        };
        if let Some(attempts) = state.retries.lock().await.success(repo_full) {
            push_and_emit(app, state, ActivityEntry {
                timestamp: Utc::now(), kind: ActivityKind::Info, repo: Some(repo_full.clone()),
                pr_number: None, pr_title: None, author: None, url: None,
                message: format!("GitHub 연결 복구 (실패 {attempts}회 묶음)"), detail: None,
            }).await;
        }
        latest_rate = Some((rate.remaining, rate.limit));

        let pr_total = pulls.len();
        for (pr_i, pr) in pulls.iter().enumerate() {
            progress(app, state, |p| {
                p.pr_index = pr_i + 1;
                p.pr_total = pr_total;
                p.pr_number = Some(pr.number);
                p.pr_title = Some(pr.title.clone());
                p.stage = Some("확인".into());
            });
            handle_pr(app, state, &client, &cfg, &allowed, &me, repo_full, pr).await;
        }
    }

    // Always publish a status snapshot. The UI's "Check now" spinner clears on
    // this event, so a pass with no repositories configured (or where every
    // repo call failed) still has to report back instead of hanging until the
    // frontend's timeout.
    {
        let mut s = state.status.lock().await;
        if let Some((rem, lim)) = latest_rate {
            s.rate_limit_remaining = rem;
            s.rate_limit_total = lim;
        }
        s.checked_at = Utc::now();
        let snapshot = s.clone();
        drop(s);
        let _ = app.emit(STATUS_EVENT, &snapshot);
    }
}

async fn handle_pr(
    app: &AppHandle,
    state: &Arc<AppState>,
    client: &GitHubClient,
    cfg: &crate::config::AppConfig,
    allowed: &std::collections::HashSet<String>,
    me: &str,
    repo_full: &str,
    pr: &PullRequest,
) {
    let author = pr.user.login.to_lowercase();

    if cfg.skip_drafts && pr.draft {
        return;
    }
    if !allowed.contains(&author) {
        return;
    }
    if author == me.to_lowercase() {
        // GitHub disallows approving your own PRs.
        return;
    }

    let Ok((owner, repo)) = split_repo(repo_full) else {
        return;
    };

    // Coalesce pushes queued while earlier PRs were reviewed.
    if !cfg.auto_approve_enabled { return; }
    let lookup_key = format!("lookup:{repo_full}#{}", pr.number);
    if !state.retries.lock().await.ready(&lookup_key, Utc::now().timestamp()) { return; }
    let latest = match client.get_pull(owner, repo, pr.number).await {
        Ok(latest) => latest,
        Err(e) => {
            retry_error(app, state, &lookup_key, repo_full, pr, "fresh PR lookup failed", &e).await;
            return;
        }
    };
    let pr = &latest;
    let pending_key = format!("{repo_full}#{}", pr.number);
    let publication_key = format!("publication:{pending_key}@{}", pr.head.sha);
    if crate::publication::check(pr, &pr.head.sha).is_err()
        || !allowed.contains(&pr.user.login.to_lowercase())
        || pr.user.login.eq_ignore_ascii_case(me) { return; }

    // Already approved by me?
    let reviews = match client.list_reviews(owner, repo, pr.number).await {
        Ok(v) => v,
        Err(e) => {
            retry_error(app, state, &lookup_key, repo_full, pr, "list reviews failed", &e).await;
            return;
        }
    };
    state.retries.lock().await.success(&lookup_key);
    let head_sha = pr.head.sha.as_str();
    let my_reviews = reviews
        .iter()
        .filter(|r| r.user.login.eq_ignore_ascii_case(me))
        .collect::<Vec<_>>();
    // A timed-out POST may already have succeeded. Reconcile its stable run marker
    // before retrying; never rerun the model merely because its receipt was lost.
    let pending = state.pending_reviews.lock().await.get(&pending_key).cloned();
    if let Some(mut pending) = pending {
        if pending.audit.head_sha != head_sha {
            state.pending_reviews.lock().await.remove(&pending_key);
        } else if let Some(posted) = my_reviews.iter().find(|r|
            r.commit_id.as_deref() == Some(head_sha)
                && r.body.contains(&format!("<!-- approve-bot:run:{} -->", pending.audit.run_id))) {
            pending.audit.update(if posted.state.eq_ignore_ascii_case("APPROVED") {"approved"}
                else if pending.outcome.finished_cleanly {"commented"} else {"failure_notice"});
            pending.audit.review_id = (posted.id > 0).then_some(posted.id);
            pending.audit.review_url = (!posted.html_url.is_empty()).then(|| posted.html_url.clone());
            if let Some(at) = &posted.submitted_at { pending.audit.at = at.clone(); }
            pending.audit.error = None;
            state.record_review(&pending.audit).await;
            state.pending_reviews.lock().await.remove(&pending_key);
            state.retries.lock().await.success(&publication_key);
            return;
        }
    }
    if !state.retries.lock().await.ready(&publication_key, Utc::now().timestamp()) { return; }
    // Already approved the CURRENT head → nothing to do.
    let approved_head = my_reviews.iter().any(|r| {
        r.state.eq_ignore_ascii_case("APPROVED") && r.commit_id.as_deref() == Some(head_sha)
    });
    if approved_head {
        // The PR is done from the gate's side, but the author may still be
        // answering threads; settle those without a new review run.
        if cfg.auto_approve_enabled && cfg.review_enabled {
            settle_replies(app, state, client, owner, repo, pr, me).await;
        }
        return;
    }

    // Master switch: the bot only acts (review or approve) when auto-approve is
    // on. Off = idle. Skip SILENTLY — emitting a per-PR "disabled" entry every
    // poll floods the (now persisted) activity log with noise.
    if !cfg.auto_approve_enabled {
        return;
    }

    let mut round: Option<crate::rereview::Round> = None;
    if cfg.review_enabled {
        let tuples: Vec<(&str, Option<&str>, Marker)> = my_reviews
            .iter()
            .map(|r| (r.state.as_str(), r.commit_id.as_deref(), marker_of(&r.body)))
            .collect();
        match decide_review_action(&tuples, head_sha, cfg.approve_only_after_review) {
            ReviewAction::Skip => {
                settle_replies(app, state, client, owner, repo, pr, me).await;
                return;
            }
            action @ (ReviewAction::ApproveOnly | ReviewAction::FollowUp) => {
                let pr_key = format!("{repo_full}#{}", pr.number);
                if state.settled_heads.lock().await.get(&pr_key).map(String::as_str) == Some(head_sha) {
                    return;
                }
                round = build_round(client, owner, repo, pr, &my_reviews, me).await;
                // Docs/tests-only commits never get a new review. Anything else
                // is followed up, not waved through.
                let docs_only = round.as_ref().is_some_and(|r| nothing_to_review(&r.delta_diff));
                match docs_only_step(&action, docs_only) {
                    DocsOnlyStep::Reapprove => {
                        let prior = my_reviews
                            .iter()
                            .rev()
                            .find(|r| marker_of(&r.body) == Marker::Reviewed)
                            .map(|r| r.body.replace(REVIEW_MARKER, "").trim().to_string());
                        approve_only(app, state, client, cfg, repo_full, owner, repo, pr, prior).await;
                        return;
                    }
                    DocsOnlyStep::Skip => {
                        state.settled_heads.lock().await.insert(pr_key, head_sha.to_string());
                        push_and_emit(
                            app,
                            state,
                            ActivityEntry {
                                timestamp: Utc::now(),
                                kind: ActivityKind::Skipped,
                                repo: Some(repo_full.to_string()),
                                pr_number: Some(pr.number),
                                pr_title: Some(pr.title.clone()),
                                author: Some(pr.user.login.clone()),
                                url: Some(pr.html_url.clone()),
                                message: "문서·테스트만 바뀌었거나 바뀐 파일이 없어 재리뷰 생략 (직전 리뷰 결과 유지)".into(),
                                detail: None,
                            },
                        )
                        .await;
                        return;
                    }
                    DocsOnlyStep::Review => {}
                }
            }
            // Fall through to the review engine below.
            ReviewAction::Review => {}
        }
    } else {
        // Legacy blind approve: original behavior — skip while an APPROVED review
        // still stands (a new commit dismisses it, letting us re-approve).
        if my_reviews.iter().any(|r| r.state.eq_ignore_ascii_case("APPROVED")) {
            return;
        }
    }

    // ── Review engine: review the diff, gate approve on the score ──
    if cfg.review_enabled {
        let pr_key = format!("{repo_full}#{}", pr.number);
        // This head already failed before reaching the model: wait for a new
        // commit instead of retrying (and logging) every poll.
        let retry_key = format!("{pr_key}@{head_sha}");
        // Model failures live on their own key: the diff-fetch key is cleared
        // on every successful fetch, so it cannot count model attempts.
        let engine_head_key = format!("{retry_key}#engine");
        {
            let retries = state.retries.lock().await;
            let now = Utc::now().timestamp();
            if !retries.ready(&retry_key, now) || !retries.ready(&engine_head_key, now) || !retries.ready(ENGINE_KEY, now) { return; }
        }
        let diff = match client.get_pr_diff(owner, repo, pr.number).await {
            Ok(d) => d,
            Err(e) => {
                let notify = state.retries.lock().await.failure(&retry_key, Utc::now().timestamp(), crate::retry::hint(&e), crate::retry::kind(&e));
                if !notify { return; }
                push_and_emit(
                    app,
                    state,
                    err_entry(repo_full, pr, format!("get diff failed (지수 지연 후 재시도): {e:#}")),
                )
                .await;
                return;
            }
        };
        state.retries.lock().await.success(&retry_key);
        // Include the real PR description so the reviewer doesn't wrongly flag it
        // as empty. Cap length to keep the prompt bounded.
        let pr_body = pr
            .body
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| {
                if s.len() > 8000 {
                    let mut cut = 8000;
                    while !s.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    format!("{}\n[... 본문 일부 생략 ...]", &s[..cut])
                } else {
                    s.to_string()
                }
            })
            .unwrap_or_else(|| "(none)".to_string());
        let changed = crate::diffprep::changed_paths(&diff);
        let added = crate::diffprep::added_lines(&diff);
        let triggers = crate::context::load_triggers(&state.config_dir, repo_full);
        let context = crate::context::gather(
            client,
            &crate::context::Inputs {
                owner,
                repo,
                number: pr.number,
                base_ref: &pr.base.ref_name,
                changed: &changed,
                me,
                added: &added,
                triggers: &triggers,
                until: None,
                body: pr.body.as_deref().unwrap_or(""),
            },
        )
        .await;
        let round_block = round.as_ref().map(crate::rereview::render).unwrap_or_default();
        // The round block is kept apart: the deep review hands it to the main
        // pass only, never to the authorization re-check.
        let meta = format!(
            "Repository: {owner}/{repo}  PR #{}\nAuthor: {}\nTitle: {}\nBody:\n{pr_body}\n\n{context}",
            pr.number, pr.user.login, pr.title
        );
        let diff_for_gate = diff.clone();
        let resolved = crate::review::resolve_guide(&cfg.review_guide_path, me);
        if resolved.source.is_stale() && !STALE_GUIDE_WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            warn!(guide = %resolved.source.label(), "review guide lacks the current version marker; its 본문 양식 may predate the output format");
        }
        let guide_source = resolved.source.label();
        let guide = resolved.text;
        let (risk, risk_why) = review_risk(&diff, round.as_ref());
        let (model, tk) = crate::routing::pick(cfg, risk);
        let second = crate::routing::second_pass(cfg, risk);
        let trace = cfg.review_value_trace_enabled;
        // Shown in the activity log so a slow/expensive review can be traced to its route.
        let route_str = format!(", {} {} ({risk_why})", risk.label(), model.trim_start_matches("claude-"));
        let deep = cfg.review_deep;
        let owner_s = owner.to_string();
        let repo_s = repo.to_string();
        let number = pr.number;
        let token = client.token().to_string();
        let reviewed_sha = head_sha.to_string();

        // Never spend a model run on a snapshot already superseded during context collection.
        if let Err(e) = ensure_review_snapshot(client, owner, repo, pr).await {
            retry_error(app, state, &publication_key, repo_full, pr, "리뷰 시작 보류", &e).await;
            return;
        }
        let input_hash = crate::audit::fingerprint(&format!("{guide}\n{meta}\n{round_block}\n{diff}\n{}\n{}\n{model}:{tk}:{deep}:{second}:{trace}", pr.base.ref_name, pr.base.sha));
        let cached = state.pending_reviews.lock().await.get(&pending_key).cloned()
            .filter(|p| p.audit.head_sha == head_sha && p.input_hash == input_hash);
        let (outcome, mut audit) = if let Some(pending) = cached {
            (pending.outcome, pending.audit)
        } else {
        let mut audit = crate::audit::Record::start(repo_full, number, head_sha, &model, risk.label(), &guide, guide_source);
        state.record_review(&audit).await;
        progress(app, state, |p| p.stage = Some(format!("리뷰 중 · {}", model.trim_start_matches("claude-"))));
        let review_started = std::time::Instant::now();

        // `claude -p` can take minutes — keep it off the async runtime.
        let outcome = match tokio::task::spawn_blocking(move || {
            if deep {
                crate::review::review_pr_deep_at(
                    &guide, &meta, &round_block, &diff, &model, tk, &owner_s, &repo_s, number, &token, Some(&reviewed_sha), second,
                    trace,
                )
            } else {
                crate::review::review_pr(&guide, &format!("{meta}\n\n{round_block}"), &diff, &model, tk)
            }
        })
        .await
        {
            Ok(o) => o,
            Err(e) => {
                audit.update("task_failed");
                audit.elapsed_ms = review_started.elapsed().as_millis().min(u64::MAX as u128) as u64;
                audit.timing_available = true;
                audit.error = Some(e.to_string());
                state.record_review(&audit).await;
                let error = anyhow::anyhow!("review task failed: {e:#}");
                retry_error(app, state, &publication_key, repo_full, pr, "리뷰 실행 실패", &error).await;
                return;
            }
        };

        audit.elapsed_ms = review_started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        audit.timing_available = true;
        audit.cost_usd = outcome.cost_usd;
        audit.finished_cleanly = Some(outcome.finished_cleanly);
        audit.finding_ids = outcome.findings.iter().map(|f| crate::review::finding_id(&f.path, &f.symbol, &f.claim)).collect();
        audit.result = Some(serde_json::json!({"body": outcome.body, "verdict": outcome.verdict,
            "score":outcome.score, "findings":outcome.findings, "omitted_files":outcome.omitted_files,
            "blocking_issues":outcome.blocking_issues, "summary": outcome.summary,
            "followups": outcome.followups, "rechecked": outcome.rechecked,
            "continued": outcome.continued, "explored": outcome.explored}));
        if !outcome.finished_cleanly {
            audit.error = Some(outcome.body.clone());
        }
        audit.update(if outcome.finished_cleanly {"reviewed"} else {"model_failed"});
        state.record_review(&audit).await;
        (outcome, audit)
        };
        // The engine did not finish: keep the error local, back off, say it
        // once. Nothing is posted — a failure notice is not a review, and
        // Weave scored the 38 posted on 10/26 as 61–75 point reviews.
        if !outcome.finished_cleanly {
            let now = Utc::now();
            let prior = state.retries.lock().await.attempts(&engine_head_key);
            let step = model_failure_step(outcome.failure.as_ref(), prior, now);
            let key = if step.engine_wide { ENGINE_KEY.to_string() } else { engine_head_key.clone() };
            let notify = state.retries.lock().await.failure(&key, now.timestamp(), Some(step.delay_secs), step.kind);
            if step.kind == "usage_limit" {
                set_engine_pause(app, state, Some(now + chrono::Duration::seconds(step.delay_secs as i64))).await;
            }
            if notify || step.always_notify {
                push_and_emit(app, state, err_entry(repo_full, pr, format!("{} — {}", step.message, outcome.body.lines().next().unwrap_or("").chars().take(200).collect::<String>()))).await;
            }
            return;
        }
        if let Some(attempts) = state.retries.lock().await.success(ENGINE_KEY) {
            set_engine_pause(app, state, None).await;
            push_and_emit(app, state, ActivityEntry {
                timestamp: Utc::now(), kind: ActivityKind::Info, repo: Some(repo_full.to_string()),
                pr_number: None, pr_title: None, author: None, url: None,
                message: format!("리뷰 엔진 복구 (실패 {attempts}회 묶음)"), detail: None,
            }).await;
        }
        state.retries.lock().await.success(&engine_head_key);
        // A later pass hit the limit: publish the first pass, pause the rest.
        if let Some(crate::review::EngineFailure::UsageLimit { reset_hint_secs }) = &outcome.failure {
            let now = Utc::now();
            let delay = reset_hint_secs.unwrap_or(3600);
            let notify = state.retries.lock().await.failure(ENGINE_KEY, now.timestamp(), Some(delay), "usage_limit");
            set_engine_pause(app, state, Some(now + chrono::Duration::seconds(delay as i64))).await;
            if notify {
                push_and_emit(app, state, err_entry(repo_full, pr, model_failure_step(outcome.failure.as_ref(), 0, now).message)).await;
            }
        }
        {
            let mut pending = state.pending_reviews.lock().await;
            pending.insert(pending_key.clone(), crate::state::PendingReview {input_hash, outcome:outcome.clone(), audit:audit.clone()});
            if pending.len() > 128 {
                if let Some(oldest) = pending.iter().min_by_key(|(_,p)| &p.audit.started_at).map(|(k,_)|k.clone()) {
                    pending.remove(&oldest);
                }
            }
        }
        if !state.config.lock().await.auto_approve_enabled {
            audit.update("publication_disabled"); state.record_review(&audit).await;
            return;
        }
        if let Err(e) = ensure_review_snapshot(client, owner, repo, pr).await {
            audit.update("publication_withheld"); audit.error = Some(format!("{e:#}"));
            state.record_review(&audit).await;
            retry_error(app, state, &publication_key, repo_full, pr, "리뷰 게시 보류 (결과는 로컬 보관)", &e).await;
            return;
        }

        let score_str = format!("{:.1}", outcome.score);
        let cost_str = outcome
            .cost_usd
            .map(|c| format!(", ${c:.2}"))
            .unwrap_or_default();
        // Follow-up: an earlier blocker stays blocking until the model says it
        // is resolved; a finding the model re-posts under an old id is dropped.
        let mut outcome = outcome;
        if let Some(r) = &round {
            outcome
                .blocking_issues
                .extend(crate::rereview::open_blockers(&r.prev, &outcome.followups));
            outcome
                .inline
                .retain(|c| !r.prev.iter().any(|p| c.body.contains(&format!("<!-- f:{} -->", p.id))));
            outcome
                .file_comments
                .retain(|c| !r.prev.iter().any(|p| c.body.contains(&format!("<!-- f:{} -->", p.id))));
        }
        // Code-side policy the model can't talk its way past: sensitive files
        // always go to a human (the #4022-style ".env-only PR got 5/5" case).
        let sensitive = crate::review::sensitive_files(&diff_for_gate);
        let confident_majors = if cfg.hold_on_confident_major {
            count_confident_majors(&outcome.findings, round.as_ref(), &outcome.followups)
        } else {
            0
        };
        let decision = gate(
            outcome.finished_cleanly,
            &outcome.verdict,
            outcome.score,
            outcome.blocking_issues.len(),
            &sensitive,
            &outcome.omitted_files,
            outcome.continued,
            confident_majors,
            cfg.min_approve_score,
        );
        // The first line states the outcome from the gate; every other hold
        // reason goes right under it, so the body's own wording is never
        // mistaken for the outcome.
        let notes = other_notes(&decision, &sensitive, &outcome.omitted_files, outcome.continued);
        // Threads carry permalink placeholders; resolve them against this head.
        let expand = |b: &str| crate::layout::expand_permalinks(b, owner, repo, head_sha);
        let inline: Vec<crate::github::ReviewComment> = if cfg.inline_comments_enabled {
            outcome
                .inline
                .iter()
                .map(|c| crate::github::ReviewComment { body: expand(&c.body), ..c.clone() })
                .collect()
        } else {
            vec![]
        };
        let file_comments: Vec<crate::github::FileComment> = if cfg.inline_comments_enabled {
            outcome
                .file_comments
                .iter()
                .map(|c| crate::github::FileComment { body: expand(&c.body), ..c.clone() })
                .collect()
        } else {
            vec![]
        };
        // File threads are posted separately and can fail after the review succeeds;
        // retain those findings in the body until a thread actually exists.
        let posted: Vec<&str> = inline.iter().map(|c| c.body.as_str()).collect();
        let body = match &round {
            // From the second review on, post a short summary of what changed
            // and fold the model's full review under it.
            Some(r) if outcome.finished_cleanly => {
                let new_findings: Vec<&crate::review::Finding> = outcome
                    .findings
                    .iter()
                    .filter(|f| {
                        let id = crate::review::finding_id(&f.path, &f.symbol, &f.claim);
                        !r.prev.iter().any(|p| p.id == id)
                    })
                    .collect();
                let counts = match crate::layout::counts_line(&new_findings).as_str() {
                    "지적 없음" => "새 지적 없음".to_string(),
                    c => format!("새 지적 {c}"),
                };
                let unposted: Vec<&crate::review::Finding> = crate::review::unposted(&outcome.findings, &posted)
                    .into_iter()
                    .filter(|f| new_findings.iter().any(|n| std::ptr::eq(*n, *f)))
                    .collect();
                crate::rereview::followup_summary(
                    r,
                    head_sha,
                    &crate::layout::verdict_callout(
                        &outcome_line(&decision, &counts, outcome.blocking_issues.len(), &score_str, cfg.min_approve_score),
                        &outcome.summary.judgement,
                    ),
                    &notes,
                    &outcome.followups,
                    &unposted,
                    &outcome.body,
                )
            }
            _ if outcome.finished_cleanly => {
                let all: Vec<&crate::review::Finding> = outcome.findings.iter().collect();
                let counts = crate::layout::counts_line(&all);
                let (files, added, deleted) = crate::layout::diff_stats(&diff_for_gate);
                let scope = crate::layout::Scope {
                    files,
                    added,
                    deleted,
                    explored: outcome.explored,
                    rechecked: outcome.rechecked,
                    continued: outcome.continued,
                    omitted: outcome.omitted_files.len(),
                };
                let verdict_line = crate::layout::verdict_callout(
                    &outcome_line(&decision, &counts, outcome.blocking_issues.len(), &score_str, cfg.min_approve_score),
                    &outcome.summary.judgement,
                );
                let table = crate::layout::report_table(&outcome.summary, &scope);
                if decision == Gate::Approve && outcome.findings.is_empty() {
                    // Nothing for the author to act on: keep the page short.
                    crate::layout::compose_quiet_body(&outcome.body, &verdict_line, &notes, &table)
                } else {
                    crate::layout::compose_body(
                        &outcome.body,
                        &verdict_line,
                        &notes,
                        &table,
                        &crate::review::unposted(&outcome.findings, &posted),
                    )
                }
            }
            _ => outcome.body.clone(),
        };
        // Posted to GitHub with the marker; the in-app detail keeps the clean body.
        // A failed run gets its own marker so the next commit is re-reviewed.
        let marker = if outcome.finished_cleanly { REVIEW_MARKER } else { FAILED_MARKER };
        let posted_body = format!(
            "{marker}\n<!-- approve-bot:run:{} -->\n{}",
            audit.run_id,
            crate::layout::with_reply_hint(&crate::layout::fold_sections(&body), inline.len() + file_comments.len())
        );
        let inline: &[crate::github::ReviewComment] = &inline;
        let file_comments: &[crate::github::FileComment] = &file_comments;
        let mut inline_str = if inline.is_empty() {
            String::new()
        } else {
            format!(", 인라인 {}건", inline.len())
        };
        progress(app, state, |p| p.stage = Some("게시".into()));
        audit.note_posted(&posted_body);
        audit.update("publication_attempted"); audit.error = None;
        state.record_review(&audit).await;
        if decision == Gate::Approve {
            match client
                .approve_pull_with_comments(
                    owner,
                    repo,
                    pr.number,
                    head_sha,
                    Some(posted_body.as_str()),
                    inline,
                )
                .await
            {
                Ok(receipt) => {
                    state.pending_reviews.lock().await.remove(&pending_key);
                    state.retries.lock().await.success(&publication_key);
                    audit.update(if decision == Gate::Approve {"approved"} else if outcome.finished_cleanly {"commented"} else {"failure_notice"});
                    audit.review_id = Some(receipt.id); audit.review_url = Some(receipt.html_url);
                    audit.inline_omitted = receipt.inline_omitted;
                    if receipt.inline_omitted { inline_str = ", 인라인 게시 실패 — 본문에 보존".into(); }
                    if let Some(at) = receipt.submitted_at { audit.at = at; }
                    state.record_review(&audit).await;
                    observe_publication(client, owner, repo, &mut audit).await;
                    state.record_review(&audit).await;
                    let n = post_file_comments(client, owner, repo, pr, file_comments).await;
                    hide_outdated(client, &my_reviews, head_sha).await;
                    if let Some(r) = &round {
                        settle_threads(client, owner, repo, pr.number, r, &outcome.followups, head_sha).await;
                    }
                    if n > 0 {
                        inline_str.push_str(&format!(", 파일 스레드 {n}건"));
                    }
                    push_and_emit(
                        app,
                        state,
                        ActivityEntry {
                            timestamp: Utc::now(),
                            kind: ActivityKind::Approved,
                            repo: Some(repo_full.to_string()),
                            pr_number: Some(pr.number),
                            pr_title: Some(pr.title.clone()),
                            author: Some(pr.user.login.clone()),
                            url: Some(pr.html_url.clone()),
                            message: format!("approved (리뷰 {score_str}/5{cost_str}{inline_str}{route_str})"),
                            detail: Some(outcome.body.clone()),
                        },
                    )
                    .await;
                    if cfg.notifications_enabled {
                        let title = format!("Approved {repo_full}#{}", pr.number);
                        let body = format!("by @{}: {} ({score_str}/5)", pr.user.login, pr.title);
                        let _ = app.notification().builder().title(title).body(body).show();
                    }
                }
                Err(e) => {
                    audit.update("publication_unknown");audit.error=Some(format!("{e:#}"));state.record_review(&audit).await;
                    retry_error(app, state, &publication_key, repo_full, pr, "approve receipt unavailable", &e).await;
                }
            }
        } else {
            // Below threshold / blocking / unclean → post the review as a
            // COMMENT (substantive body still delivered), but do NOT approve.
            let why = decision.reason(outcome.blocking_issues.len(), &score_str, cfg.min_approve_score);
            match client
                .comment_pull(owner, repo, pr.number, head_sha, &posted_body, inline)
                .await
            {
                Ok(receipt) => {
                    state.pending_reviews.lock().await.remove(&pending_key);
                    state.retries.lock().await.success(&publication_key);
                    audit.update(if decision == Gate::Approve {"approved"} else if outcome.finished_cleanly {"commented"} else {"failure_notice"});
                    audit.review_id = Some(receipt.id); audit.review_url = Some(receipt.html_url);
                    audit.inline_omitted = receipt.inline_omitted;
                    if receipt.inline_omitted { inline_str = ", 인라인 게시 실패 — 본문에 보존".into(); }
                    if let Some(at) = receipt.submitted_at { audit.at = at; }
                    state.record_review(&audit).await;
                    observe_publication(client, owner, repo, &mut audit).await;
                    state.record_review(&audit).await;
                    let n = post_file_comments(client, owner, repo, pr, file_comments).await;
                    hide_outdated(client, &my_reviews, head_sha).await;
                    if let Some(r) = &round {
                        settle_threads(client, owner, repo, pr.number, r, &outcome.followups, head_sha).await;
                    }
                    if n > 0 {
                        inline_str.push_str(&format!(", 파일 스레드 {n}건"));
                    }
                    push_and_emit(
                        app,
                        state,
                        ActivityEntry {
                            timestamp: Utc::now(),
                            kind: ActivityKind::Info,
                            repo: Some(repo_full.to_string()),
                            pr_number: Some(pr.number),
                            pr_title: Some(pr.title.clone()),
                            author: Some(pr.user.login.clone()),
                            url: Some(pr.html_url.clone()),
                            message: format!("리뷰 코멘트 게시 (approve 보류: {why}{cost_str}{inline_str}{route_str})"),
                            detail: Some(outcome.body.clone()),
                        },
                    )
                    .await;
                }
                Err(e) => {
                    audit.update("publication_unknown");audit.error=Some(format!("{e:#}"));state.record_review(&audit).await;
                    retry_error(app, state, &publication_key, repo_full, pr, "comment receipt unavailable", &e).await;
                }
            }
        }
        return;
    }

    // ── Legacy blind approve (review engine disabled) ──
    let mut audit = approval_audit(repo_full, pr, "approval_only");
    state.record_review(&audit).await;
    let started = std::time::Instant::now();
    let result = client
        .approve_pull(owner, repo, pr.number, &pr.head.sha, Some(&cfg.approval_message))
        .await;
    finish_approval_audit(&mut audit, &result, started.elapsed().as_millis() as u64);
    state.record_review(&audit).await;
    if result.is_ok() {
        observe_publication(client, owner, repo, &mut audit).await;
        state.record_review(&audit).await;
    }
    match result {
        Ok(_receipt) => {
            state.retries.lock().await.success(&publication_key);
            push_and_emit(
                app,
                state,
                ActivityEntry {
                    timestamp: Utc::now(),
                    kind: ActivityKind::Approved,
                    repo: Some(repo_full.to_string()),
                    pr_number: Some(pr.number),
                    pr_title: Some(pr.title.clone()),
                    author: Some(pr.user.login.clone()),
                    url: Some(pr.html_url.clone()),
                    message: "approved".into(),
                    detail: None,
                },
            )
            .await;
            if cfg.notifications_enabled {
                let title = format!("Approved {repo_full}#{}", pr.number);
                let body = format!("by @{}: {}", pr.user.login, pr.title);
                let _ = app.notification().builder().title(title).body(body).show();
            }
        }
        Err(e) => {
            retry_error(app, state, &publication_key, repo_full, pr, "approve receipt unavailable", &e).await;
        }
    }
}

/// Approve the current head without re-reviewing — used when this PR already has
/// an engine review on an earlier commit and `approve_only_after_review` is on.
#[allow(clippy::too_many_arguments)]
async fn approve_only(
    app: &AppHandle,
    state: &Arc<AppState>,
    client: &GitHubClient,
    cfg: &crate::config::AppConfig,
    repo_full: &str,
    owner: &str,
    repo: &str,
    pr: &PullRequest,
    prior_review: Option<String>,
) {
    // Already reviewed earlier — approve with NO body (a bare approval, no
    // comment on the PR). The prior review already carries the substance.
    let mut audit = approval_audit(repo_full, pr, "reapproval");
    state.record_review(&audit).await;
    let started = std::time::Instant::now();
    let result = client.approve_pull(owner, repo, pr.number, &pr.head.sha, None).await;
    finish_approval_audit(&mut audit, &result, started.elapsed().as_millis() as u64);
    state.record_review(&audit).await;
    if result.is_ok() {
        observe_publication(client, owner, repo, &mut audit).await;
        state.record_review(&audit).await;
    }
    let retry_key = format!("publication:{repo_full}#{}@{}", pr.number, pr.head.sha);
    match result {
        Ok(_receipt) => {
            state.retries.lock().await.success(&retry_key);
            push_and_emit(
                app,
                state,
                ActivityEntry {
                    timestamp: Utc::now(),
                    kind: ActivityKind::Approved,
                    repo: Some(repo_full.to_string()),
                    pr_number: Some(pr.number),
                    pr_title: Some(pr.title.clone()),
                    author: Some(pr.user.login.clone()),
                    url: Some(pr.html_url.clone()),
                    message: "approved (이전 리뷰 있음 — 재리뷰 생략)".into(),
                    detail: prior_review,
                },
            )
            .await;
            if cfg.notifications_enabled {
                let title = format!("Approved {repo_full}#{}", pr.number);
                let body = format!("by @{}: {}", pr.user.login, pr.title);
                let _ = app.notification().builder().title(title).body(body).show();
            }
        }
        Err(e) => {
            retry_error(app, state, &retry_key, repo_full, pr, "approve receipt unavailable", &e).await;
        }
    }
}

fn approval_audit(repo: &str, pr: &PullRequest, source: &str) -> crate::audit::Record {
    let mut audit = crate::audit::Record::start(repo, pr.number, &pr.head.sha, "none", "not_run", "", "not_used".into());
    audit.source = source.into();
    audit.guide_hash.clear();
    audit.cost_usd = Some(0.0); // No model was invoked.
    audit.update("publication_attempted");
    audit
}

/// GitHub offers no atomic open/head precondition on review creation. Record a
/// post-write observation so a remaining race is visible, without claiming when
/// a close/push occurred relative to the server's write.
async fn observe_publication(client: &GitHubClient, owner: &str, repo: &str, audit: &mut crate::audit::Record) {
    let observation = match client.get_pull(owner, repo, audit.pr).await {
        Ok(pr) => serde_json::json!({"checked_at":Utc::now().to_rfc3339(), "state":pr.state,
            "merged":pr.merged, "merged_at":pr.merged_at, "draft":pr.draft, "head_sha":pr.head.sha,
            "still_eligible":crate::publication::check(&pr, &audit.head_sha).is_ok()}),
        Err(e) => serde_json::json!({"checked_at":Utc::now().to_rfc3339(), "lookup_error":format!("{e:#}")}),
    };
    audit.post_publication = Some(observation);
}

async fn ensure_review_snapshot(client: &GitHubClient, owner: &str, repo: &str, expected: &PullRequest) -> anyhow::Result<()> {
    let latest = client.get_pull(owner, repo, expected.number).await?;
    crate::publication::check_snapshot(&latest, expected).map_err(|reason| anyhow::anyhow!("publication withheld: {reason}"))
}

fn finish_approval_audit(audit: &mut crate::audit::Record, result: &anyhow::Result<crate::github::ReviewReceipt>, elapsed_ms: u64) {
    audit.elapsed_ms = elapsed_ms;
    audit.timing_available = true;
    match result {
        Ok(receipt) => {
            audit.update(if audit.source == "reapproval" {"reapproval"} else {"approved"});
            audit.review_id = Some(receipt.id);
            audit.review_url = Some(receipt.html_url.clone());
            if let Some(at) = &receipt.submitted_at {audit.at = at.clone();}
        }
        Err(e) => {audit.update("publication_unknown"); audit.error = Some(format!("{e:#}"));}
    }
}

/// Group repeated errors while preserving server-provided retry delays.
async fn retry_error(app: &AppHandle, state: &Arc<AppState>, key: &str, repo: &str,
    pr: &PullRequest, message: &str, error: &anyhow::Error) {
    let notify = state.retries.lock().await.failure(key, Utc::now().timestamp(), crate::retry::hint(error), crate::retry::kind(error));
    if notify {
        push_and_emit(app, state, err_entry(repo, pr, format!("{message} (지수 지연 후 재시도): {error:#}"))).await;
    }
}

/// Show (or clear) the engine pause in the connection status the UI renders.
async fn set_engine_pause(app: &AppHandle, state: &Arc<AppState>, until: Option<DateTime<Utc>>) {
    let status = {
        let mut s = state.status.lock().await;
        if s.engine_paused_until == until { return; }
        s.engine_paused_until = until;
        s.clone()
    };
    let _ = app.emit(STATUS_EVENT, &status);
}

/// Build an `Error` activity entry for a PR (shared by the review/approve paths).
fn err_entry(repo_full: &str, pr: &PullRequest, message: String) -> ActivityEntry {
    ActivityEntry {
        timestamp: Utc::now(),
        kind: ActivityKind::Error,
        repo: Some(repo_full.to_string()),
        pr_number: Some(pr.number),
        pr_title: Some(pr.title.clone()),
        author: Some(pr.user.login.clone()),
        url: Some(pr.html_url.clone()),
        message,
        detail: None,
    }
}

async fn push_and_emit(app: &AppHandle, state: &Arc<AppState>, entry: ActivityEntry) {
    state.push_activity(entry.clone()).await;
    let _ = app.emit(ACTIVITY_EVENT, &entry);
}

#[cfg(test)]
mod tests {
    use super::{docs_only_step, nothing_to_review, review_risk, DocsOnlyStep};
    use super::{decide_review_action, gate, outcome_line, Gate, Marker, ReviewAction};

    const R: Marker = Marker::Reviewed;
    const N: Marker = Marker::None;
    const F: Marker = Marker::Failed;

    const HEAD: &str = "aaaa1111";
    const OLD: &str = "bbbb2222";

    #[test]
    fn blind_emoji_approve_does_not_suppress_first_review() {
        // The #1876 bug: a dismissed blind 👍 (no marker) must NOT count as reviewed.
        let reviews = [("DISMISSED", Some(OLD), N)];
        assert_eq!(decide_review_action(&reviews, HEAD, true), ReviewAction::Review);
    }

    #[test]
    fn no_prior_reviews_reviews() {
        assert_eq!(decide_review_action(&[], HEAD, true), ReviewAction::Review);
    }

    #[test]
    fn engine_review_on_current_head_skips() {
        let reviews = [("COMMENTED", Some(HEAD), R)];
        assert_eq!(decide_review_action(&reviews, HEAD, true), ReviewAction::Skip);
    }

    #[test]
    fn approved_current_head_skips() {
        let reviews = [("APPROVED", Some(HEAD), N)];
        assert_eq!(decide_review_action(&reviews, HEAD, true), ReviewAction::Skip);
    }

    #[test]
    fn engine_review_on_old_commit_approve_only_when_enabled() {
        let reviews = [("DISMISSED", Some(OLD), R)];
        assert_eq!(decide_review_action(&reviews, HEAD, true), ReviewAction::ApproveOnly);
    }

    #[test]
    fn engine_review_on_old_commit_re_reviews_when_approve_only_off() {
        let reviews = [("DISMISSED", Some(OLD), R)];
        assert_eq!(decide_review_action(&reviews, HEAD, false), ReviewAction::FollowUp);
    }

    #[test]
    fn dismissed_engine_review_on_head_re_reviews() {
        // Marker review but dismissed on the current head → stale → re-review.
        let reviews = [("DISMISSED", Some(HEAD), R)];
        assert_eq!(decide_review_action(&reviews, HEAD, false), ReviewAction::Review);
    }

    #[test]
    fn commented_engine_review_on_old_commit_is_re_reviewed() {
        // Below-threshold / blocking review → new push must be checked, not waved through.
        let reviews = [("COMMENTED", Some(OLD), R)];
        assert_eq!(decide_review_action(&reviews, HEAD, true), ReviewAction::FollowUp);
    }

    #[test]
    fn failed_run_on_old_commit_is_re_reviewed() {
        let reviews = [("DISMISSED", Some(OLD), R), ("COMMENTED", Some(OLD), F)];
        assert_eq!(decide_review_action(&reviews, HEAD, true), ReviewAction::FollowUp);
        // A failed run alone is not a review to follow up on.
        let only_failed = [("COMMENTED", Some(OLD), F)];
        assert_eq!(decide_review_action(&only_failed, HEAD, true), ReviewAction::Review);
    }

    #[test]
    fn failed_notice_on_head_is_reviewed_again() {
        // The backoff state owns retry frequency now; a notice is not a review.
        let reviews = [("COMMENTED", Some(HEAD), F)];
        assert_eq!(decide_review_action(&reviews, HEAD, true), ReviewAction::Review);
        // …and with an earlier finished review it becomes a follow-up round.
        let reviews = [("COMMENTED", Some(OLD), R), ("COMMENTED", Some(HEAD), F)];
        assert_eq!(decide_review_action(&reviews, HEAD, true), ReviewAction::FollowUp);
    }

    #[test]
    fn check_now_scope_narrows_the_pass_to_one_configured_repository() {
        let repos: Vec<String> = ["musinsa/a", "musinsa/b"].iter().map(|s| s.to_string()).collect();
        assert_eq!(super::pass_targets(&repos, None), repos);
        assert_eq!(super::pass_targets(&repos, Some("musinsa/B")), vec!["musinsa/b".to_string()]);
        assert!(super::pass_targets(&repos, Some("musinsa/gone")).is_empty(), "a repository no longer configured is not reviewed");
    }

    #[test]
    fn progress_fraction_counts_finished_repos_whole_and_the_current_one_by_prs() {
        use super::progress_fraction as f;
        assert_eq!(f(0, 0, 0, 0, false), 0.0);
        assert_eq!(f(1, 4, 0, 0, false), 0.0, "first repo, PR count unknown");
        assert_eq!(f(2, 4, 0, 0, false), 0.25, "one repo done");
        assert_eq!(f(2, 4, 3, 4, false), 0.25 + 0.5 / 4.0, "third of four PRs in the second repo: two done");
        assert_eq!(f(2, 4, 3, 4, true), 0.25 + 0.625 / 4.0, "the PR under review counts half");
        assert_eq!(f(4, 4, 4, 4, false), 0.75 + 0.75 / 4.0);
        assert!(f(9, 4, 9, 1, true) <= 1.0, "never past the end");
        assert_eq!(f(1, 1, 1, 1, true), 0.5, "one repo, one PR, mid-review");
        let json = serde_json::to_value(super::PassState { running: true, repo_index: 1, repo_total: 2, ..Default::default() }).unwrap();
        assert_eq!(json["repo_total"], 2);
        assert_eq!(json["fraction"], 0.0);
    }

    #[test]
    fn reply_check_runs_only_when_pr_updated_at_changes() {
        assert!(super::needs_reply_check(None, Some("2026-10-27T01:00:00Z")), "never checked");
        assert!(!super::needs_reply_check(Some("2026-10-27T01:00:00Z"), Some("2026-10-27T01:00:00Z")), "nothing happened");
        assert!(super::needs_reply_check(Some("2026-10-27T01:00:00Z"), Some("2026-10-27T02:00:00Z")), "activity");
        assert!(super::needs_reply_check(Some("x"), None), "no timestamp from GitHub → check");
    }

    #[test]
    fn gate_holds_a_confident_major_when_the_policy_is_on_and_approves_when_off() {
        assert_eq!(gate(true, "approve", 4.5, 0, &[], &[], false, 1, 4.0), Gate::HoldMajor(1));
        assert_eq!(gate(true, "approve", 4.5, 0, &[], &[], false, 0, 4.0), Gate::Approve, "policy off passes a count of 0");
        // Earlier holds still come first; a blocker is not demoted to a major hold.
        assert_eq!(gate(true, "approve", 4.5, 1, &[], &[], false, 2, 4.0), Gate::HoldBlocking);
        assert_eq!(gate(true, "approve", 5.0, 0, &[], &[], true, 2, 4.0), Gate::HoldContinued);
        assert_eq!(Gate::HoldMajor(2).reason(0, "4.0", 4.0), "확인한 major 2건");
        assert_eq!(outcome_line(&Gate::HoldMajor(1), "major 1 · minor 2", 0, "4.5", 4.0), "👤 **사람 확인 필요** — 확인한 major 1건 · major 1 · minor 2");
    }

    #[test]
    fn confident_majors_count_new_findings_and_unsettled_earlier_ones() {
        let mk = |sev: &str, conf: f64, claim: &str| crate::review::Finding {
            severity: sev.into(), confidence: Some(conf), path: "a.ts".into(), symbol: "f".into(), claim: claim.into(), ..Default::default()
        };
        let findings = vec![mk("major", 85.0, "a"), mk("major", 70.0, "b"), mk("minor", 95.0, "c"), mk("blocker", 90.0, "d")];
        assert_eq!(super::count_confident_majors(&findings, None, &[]), 1, "only the confident major");
        // On a follow-up, a re-posted earlier finding is not new; an open earlier major counts.
        let prev = |id: &str, sev: &str| crate::rereview::PrevFinding {
            id: id.into(), severity: sev.into(), path: "a.ts".into(), line: Some(1), claim: "x".into(), comment_id: 1, last_reply: None,
        };
        let reposted = crate::review::finding_id("a.ts", "f", "a");
        let round = crate::rereview::Round {
            number: 2, prev_sha: "p".into(), delta_diff: String::new(),
            prev: vec![prev(&reposted, "major"), prev("open1", "major"), prev("fixed1", "major"), prev("minor1", "minor")],
        };
        let fu = |id: &str, status: &str| crate::rereview::Followup { id: id.into(), status: status.into(), note: String::new() };
        let followups = vec![fu(&reposted, "resolved"), fu("fixed1", "wont_fix")];
        // reposted: resolved → 0; open1: no followup → 1; fixed1: wont_fix → 0; minor1 ignored; new confident major "a" is reposted → not new.
        assert_eq!(super::count_confident_majors(&findings, Some(&round), &followups), 1);
        let followups = vec![fu("open1", "partial")];
        assert_eq!(super::count_confident_majors(&findings, Some(&round), &followups), 3, "partial and silent earlier majors stay open; 'a' is new again");
    }

    #[test]
    fn outdated_reviews_include_a_failed_notice_on_head() {
        let mk = |body: &str, sha: &str, node: &str| crate::github::Review {
            id: 0, html_url: String::new(), user: crate::github::GhUser { login: "me".into() }, state: "COMMENTED".into(),
            body: body.into(), commit_id: Some(sha.into()), submitted_at: None, node_id: node.into(),
        };
        let a = mk("<!-- approve-bot:review -->", OLD, "n-old");
        let b = mk("<!-- approve-bot:review-failed -->", HEAD, "n-fail");
        let c = mk("<!-- approve-bot:review -->", HEAD, "n-live");
        let d = mk("👍", OLD, "n-human");
        let ids = super::outdated_reviews(&[&a, &b, &c, &d], HEAD);
        assert_eq!(ids, vec!["n-old", "n-fail"]);
    }

    #[test]
    fn model_failure_step_pauses_engine_on_limit_and_gives_up_per_head() {
        use crate::review::EngineFailure as F;
        use chrono::TimeZone;
        let now = chrono::Utc.with_ymd_and_hms(2026, 10, 26, 5, 0, 0).unwrap();
        let limit = super::model_failure_step(Some(&F::UsageLimit { reset_hint_secs: Some(36_000) }), 0, now);
        assert!(limit.engine_wide && limit.kind == "usage_limit" && limit.delay_secs == 36_000, "{limit:?}");
        assert!(limit.message.contains("사용 한도"));
        let unknown = super::model_failure_step(Some(&F::UsageLimit { reset_hint_secs: None }), 0, now);
        assert_eq!(unknown.delay_secs, 3600, "no reset text → one hour");
        let auth = super::model_failure_step(Some(&F::Auth), 0, now);
        assert!(auth.engine_wide && auth.kind == "auth" && auth.message.contains("claude login"));
        let first = super::model_failure_step(Some(&F::Timeout), 0, now);
        assert!(!first.engine_wide && first.kind == "engine" && first.delay_secs == 900 && !first.always_notify);
        assert!(first.message.contains("1/3"));
        let last = super::model_failure_step(Some(&F::Parse), 2, now);
        assert!(last.kind == "engine_gave_up" && last.always_notify && last.delay_secs >= 86_400, "{last:?}");
        let none = super::model_failure_step(None, 0, now);
        assert_eq!(none.kind, "engine");
    }

    #[test]
    fn latest_engine_review_decides_approve_only() {
        let reviews = [("COMMENTED", Some("c1"), R), ("APPROVED", Some(OLD), R)];
        assert_eq!(decide_review_action(&reviews, HEAD, true), ReviewAction::ApproveOnly);
    }

    #[test]
    fn gate_rejects_approve_with_blocking_issues() {
        assert_eq!(gate(true, "approve", 5.0, 1, &[], &[], false, 0, 4.0), Gate::HoldBlocking);
    }

    #[test]
    fn gate_holds_sensitive_files() {
        let files = vec![".env.production".to_string()];
        assert_eq!(gate(true, "approve", 5.0, 0, &files, &[], false, 0, 4.0), Gate::HoldSensitive(files));
    }

    #[test]
    fn gate_holds_when_files_were_not_reviewed() {
        let omitted = vec!["src/big.ts".to_string()];
        assert_eq!(gate(true, "approve", 5.0, 0, &[], &omitted, false, 0, 4.0), Gate::HoldOmitted(omitted));
    }

    #[test]
    fn an_empty_delta_has_nothing_to_review() {
        use super::nothing_to_review;
        assert!(nothing_to_review(""), "no file changed");
        assert!(nothing_to_review("diff --git a/README.md b/README.md\n--- a/README.md\n+++ b/README.md\n@@ -1 +1 @@\n-a\n+b\n"));
        assert!(!nothing_to_review("diff --git a/src/a.ts b/src/a.ts\n--- a/src/a.ts\n+++ b/src/a.ts\n@@ -1 +1 @@\n-a\n+b\n"));
    }

    #[test]
    fn sensitive_docs_never_take_the_reapproval_shortcut() {
        for path in ["docs/deploy.sh", "docs/.env.production", ".github/README.md"] {
            let delta = format!("diff --git a/{path} b/{path}\n-old\n+new\n");
            assert_eq!(docs_only_step(&ReviewAction::ApproveOnly, nothing_to_review(&delta)), DocsOnlyStep::Review, "{path}");
        }
        let rename = "diff --git a/scripts/deploy.sh b/docs/deploy.md\nrename from scripts/deploy.sh\nrename to docs/deploy.md\n";
        assert!(!nothing_to_review(rename), "a sensitive source path still requires review after renaming");
    }

    fn routing_diff(path: &str, additions: usize) -> String {
        format!("diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -1 +1 @@\n{}", "+new\n".repeat(additions))
    }

    fn routing_round(delta_diff: String) -> crate::rereview::Round {
        crate::rereview::Round { number: 2, prev_sha: "previous".into(), prev: vec![], delta_diff }
    }

    #[test]
    fn small_followup_to_large_ui_change_uses_delta_model_and_second_pass_budget() {
        use crate::routing::{pick, second_pass, Risk};
        let full = routing_diff("src/ui/Panel.tsx", 650);
        let round = routing_round(routing_diff("src/ui/Panel.tsx", 2));
        let cfg = crate::config::AppConfig::default();
        assert_eq!(review_risk(&full, None).0, Risk::High);
        let (risk, _) = review_risk(&full, Some(&round));
        assert_eq!(risk, Risk::Normal);
        assert_eq!(pick(&cfg, risk).0, cfg.review_model);
        assert!(!second_pass(&cfg, risk));
    }

    #[test]
    fn followup_keeps_full_pr_safety_risk_even_when_delta_is_small() {
        use crate::routing::Risk;
        let round = routing_round(routing_diff("src/ui/Panel.tsx", 2));
        for path in ["src/auth/permission.ts", "src/api/service.ts", "docs/deploy.sh"] {
            assert_eq!(review_risk(&routing_diff(path, 1), Some(&round)).0, Risk::High, "{path}");
        }
        let rename = "diff --git a/src/auth/access.ts b/src/ui/access.ts\nrename from src/auth/access.ts\nrename to src/ui/access.ts\n";
        assert_eq!(review_risk(rename, Some(&round)).0, Risk::High);
    }

    #[test]
    fn followup_keeps_unresolved_serious_findings_at_high_risk() {
        use crate::routing::Risk;
        let full = routing_diff("src/ui/Panel.tsx", 1);
        let mut round = routing_round(full.clone());
        for severity in ["major", "blocker"] {
            for status in [None, Some("unresolved"), Some("partial"), Some("wont_fix")] {
                round.prev = vec![crate::rereview::PrevFinding { severity: severity.into(), last_reply: status.map(str::to_string), ..Default::default() }];
                assert_eq!(review_risk(&full, Some(&round)).0, Risk::High, "{severity} {status:?}");
            }
        }
        round.prev[0].last_reply = Some("resolved".into());
        assert_eq!(review_risk(&full, Some(&round)).0, Risk::Normal);
        round.prev[0].last_reply = Some("withdrawn".into());
        assert_eq!(review_risk(&full, Some(&round)).0, Risk::Normal);
    }

    #[test]
    fn followup_new_high_risk_delta_is_not_hidden_by_normal_full_diff() {
        let full = routing_diff("src/ui/Panel.tsx", 2);
        let round = routing_round(routing_diff("src/auth/guard.ts", 1));
        assert_eq!(review_risk(&full, Some(&round)).0, crate::routing::Risk::High);
    }

    #[test]
    fn docs_only_push_after_a_held_review_is_skipped() {
        use super::{docs_only_step, DocsOnlyStep, ReviewAction};
        // PR #1 round 8: README-only push on a PR held by the .npmrc gate.
        assert_eq!(docs_only_step(&ReviewAction::FollowUp, true), DocsOnlyStep::Skip);
        assert_eq!(docs_only_step(&ReviewAction::ApproveOnly, true), DocsOnlyStep::Reapprove);
        // Code changed → review as before, whatever the last outcome was.
        assert_eq!(docs_only_step(&ReviewAction::FollowUp, false), DocsOnlyStep::Review);
        assert_eq!(docs_only_step(&ReviewAction::ApproveOnly, false), DocsOnlyStep::Review);
        assert_eq!(docs_only_step(&ReviewAction::Review, true), DocsOnlyStep::Review);
    }

    #[test]
    fn outcome_line_follows_the_gate_not_the_model() {
        use super::{outcome_line, Gate};
        assert_eq!(outcome_line(&Gate::Approve, "minor 2", 0, "4.5", 4.0), "✅ **승인** · minor 2");
        let held = outcome_line(&Gate::HoldSensitive(vec![".npmrc".into()]), "minor 4 · 질문 1", 0, "5.0", 4.0);
        assert_eq!(held, "👤 **사람 승인 필요** — 민감 파일 변경(`.npmrc`) · minor 4 · 질문 1");
        let many: Vec<String> = [".github/workflows/release.yml", ".github/workflows/version-sdk.yml", "scripts/setup.sh"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            outcome_line(&Gate::HoldSensitive(many), "새 지적 없음", 0, "5.0", 4.0),
            "👤 **사람 승인 필요** — 민감 파일 변경(`release.yml` 외 2개) · 새 지적 없음"
        );
        assert!(!held.contains("Approve") && !held.contains("/5"), "no score on the first line");
        assert_eq!(
            outcome_line(&Gate::HoldBlocking, "blocker 1", 1, "3.0", 4.0),
            "> [!CAUTION]\n> **머지 전에 고쳐야 할 지적이 있습니다** · blocker 1"
        );
        assert_eq!(outcome_line(&Gate::HoldScore, "minor 1", 0, "3.5", 4.0), "⏸ **승인 보류** — 점수 3.5/5 < 4.0 · minor 1");
    }

    #[test]
    fn notes_skip_the_reason_the_first_line_states() {
        use super::{other_notes, Gate};
        let s = vec![".npmrc".to_string()];
        let o = vec!["big.ts".to_string()];
        let n = other_notes(&Gate::HoldSensitive(s.clone()), &s, &o, false);
        assert!(!n.contains("민감 파일") && n.contains("보지 못했습니다"), "{n}");
        assert!(other_notes(&Gate::HoldSensitive(s.clone()), &s, &[], false).is_empty());
    }

    #[test]
    fn a_new_round_hides_every_earlier_engine_review() {
        use super::outdated_reviews;
        use crate::github::{GhUser, Review};
        let r = |body: &str, sha: &str, id: &str| Review {
            id: 0, html_url: String::new(),
            user: GhUser { login: "me".into() },
            state: "COMMENTED".into(),
            commit_id: Some(sha.into()),
            body: body.into(),
            submitted_at: None,
            node_id: id.into(),
        };
        let old = r("<!-- approve-bot:review -->\n본문", "aaa", "R1");
        let failed = r("<!-- approve-bot:review-failed -->\n실패", "bbb", "R2");
        let human = r("사람이 쓴 코멘트", "aaa", "R3");
        let current = r("<!-- approve-bot:review -->\n방금 것", "head", "R4");
        let all = vec![&old, &failed, &human, &current];
        assert_eq!(outdated_reviews(&all, "head"), vec!["R1", "R2"]);
    }

    #[test]
    fn gate_notes_list_every_hold_reason() {
        use super::gate_notes;
        let omitted: Vec<String> = (0..7).map(|i| format!("f{i}.ts")).collect();
        let n = gate_notes(&[".env".to_string()], &omitted, false);
        assert!(n.contains("민감 파일 변경(.env)"));
        assert!(n.contains("7개 파일을 보지 못했습니다(f0.ts, f1.ts, f2.ts, f3.ts, f4.ts 외 2개)"));
        assert!(gate_notes(&[], &[], false).is_empty());
        assert!(gate_notes(&[], &[], true).contains("이어서 리뷰했습니다"));
    }

    #[test]
    fn continued_review_is_held_even_when_everything_else_passes() {
        assert_eq!(gate(true, "approve", 5.0, 0, &[], &[], true, 0, 4.0), Gate::HoldContinued);
        // Higher-priority holds still win, so the reason shown is the first one hit.
        let omitted = vec!["src/big.ts".to_string()];
        assert_eq!(gate(true, "approve", 5.0, 0, &[], &omitted, true, 0, 4.0), Gate::HoldOmitted(omitted));
        assert_eq!(gate(true, "approve", 5.0, 1, &[], &[], true, 0, 4.0), Gate::HoldBlocking);
    }

    #[test]
    fn gate_approves_only_when_all_hold() {
        assert_eq!(gate(true, "approve", 4.0, 0, &[], &[], false, 0, 4.0), Gate::Approve);
        assert_eq!(gate(false, "approve", 5.0, 0, &[], &[], false, 0, 4.0), Gate::HoldUnfinished);
        assert_eq!(gate(true, "comment", 5.0, 0, &[], &[], false, 0, 4.0), Gate::HoldVerdict);
        assert_eq!(gate(true, "approve", 3.5, 0, &[], &[], false, 0, 4.0), Gate::HoldScore);
    }
}
