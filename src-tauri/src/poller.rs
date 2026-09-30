use crate::github::{split_repo, GitHubClient, PullRequest};
use crate::state::{ActivityEntry, ActivityKind, AppState, ConnectionStatus};
use chrono::Utc;
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Emitter};
use tauri_plugin_notification::NotificationExt;
use tokio::time::sleep;
use tracing::{debug, info, warn};

pub const ACTIVITY_EVENT: &str = "approve-bot://activity";
pub const STATUS_EVENT: &str = "approve-bot://status-changed";

/// Hidden marker prepended to engine-posted review bodies so later polls can
/// tell our own review apart from a blind approve or another bot's review.
/// Renders invisibly on GitHub (HTML comment).
const REVIEW_MARKER: &str = "<!-- approve-bot:review -->";

/// Marker for a review that did not finish (engine error, parse failure). It
/// stops the same head from being re-posted every poll, but unlike
/// `REVIEW_MARKER` it never counts as "reviewed" — the next commit gets a real
/// review instead of a bare approve.
const FAILED_MARKER: &str = "<!-- approve-bot:review-failed -->";

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
    /// Already reviewed earlier; just approve the new head (no re-review).
    ApproveOnly,
    /// Run the review engine.
    Review,
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
    // Already engine-reviewed THIS head (live) → don't re-post.
    // Engine ran on THIS head (live), finished or failed → don't re-post.
    if reviews.iter().any(|(s, c, marker)| {
        *marker != Marker::None && !s.eq_ignore_ascii_case("DISMISSED") && *c == Some(head_sha)
    }) {
        return ReviewAction::Skip;
    }
    // Earlier commit → approve without re-review only if my latest engine run
    // finished AND approved (DISMISSED = an approval a new push dismissed).
    // A COMMENT (below threshold / blocking) or a failed run must be re-reviewed,
    // otherwise its findings are waved through unchecked.
    let latest_engine = reviews.iter().rev().find(|(_, _, m)| *m != Marker::None);
    if approve_only_after_review {
        if let Some((state, _, Marker::Reviewed)) = latest_engine {
            if state.eq_ignore_ascii_case("APPROVED") || state.eq_ignore_ascii_case("DISMISSED") {
                return ReviewAction::ApproveOnly;
            }
        }
    }
    ReviewAction::Review
}

/// Approve-gate verdict. Pure so every hold reason is testable.
#[derive(Debug, PartialEq, Eq)]
enum Gate {
    Approve,
    HoldUnfinished,
    HoldBlocking,
    HoldSensitive(Vec<String>),
    HoldOmitted(Vec<String>),
    HoldVerdict,
    HoldScore,
}

impl Gate {
    fn reason(&self, blocking: usize, score: &str, min: f64) -> String {
        match self {
            Gate::Approve => "approve".to_string(),
            Gate::HoldUnfinished => "리뷰 미완료".to_string(),
            Gate::HoldBlocking => format!("blocking {blocking}건"),
            Gate::HoldSensitive(files) => format!("민감 파일 {}건", files.len()),
            Gate::HoldOmitted(files) => format!("미검토 파일 {}건", files.len()),
            Gate::HoldVerdict => "판정이 approve 아님".to_string(),
            Gate::HoldScore => format!("점수 {score}/5 < {min:.1}"),
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

        let token_opt = state.token.lock().await.clone();
        if token_opt.is_none() {
            // Not connected — try once, otherwise back off and wait.
            try_connect(&app, &state).await;
        } else {
            run_one_pass(&app, &state).await;
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
            let status = ConnectionStatus {
                connected: true,
                username: Some(user.login.clone()),
                rate_limit_remaining: rate.remaining,
                rate_limit_total: rate.limit,
                last_error: None,
                checked_at: Utc::now(),
            };
            *state.status.lock().await = status.clone();
            let _ = app.emit(STATUS_EVENT, &status);
            info!(user = %user.login, "connected to GitHub");
        }
        Err(e) => {
            let status =
                ConnectionStatus::disconnected(format!("GitHub auth check failed: {e}"));
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

    for repo_full in &cfg.repositories {
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
                        message: format!("list pulls failed: {e}"),
                        detail: None,
                    },
                )
                .await;
                continue;
            }
        };
        latest_rate = Some((rate.remaining, rate.limit));

        for pr in pulls {
            handle_pr(app, state, &client, &cfg, &allowed, &me, repo_full, &pr).await;
        }
    }

    if let Some((rem, lim)) = latest_rate {
        let mut s = state.status.lock().await;
        s.rate_limit_remaining = rem;
        s.rate_limit_total = lim;
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

    // Already approved by me?
    let reviews = match client.list_reviews(owner, repo, pr.number).await {
        Ok(v) => v,
        Err(e) => {
            push_and_emit(
                app,
                state,
                ActivityEntry {
                    timestamp: Utc::now(),
                    kind: ActivityKind::Error,
                    repo: Some(repo_full.to_string()),
                    pr_number: Some(pr.number),
                    pr_title: Some(pr.title.clone()),
                    author: Some(pr.user.login.clone()),
                    url: Some(pr.html_url.clone()),
                    message: format!("list reviews failed: {e}"),
                    detail: None,
                },
            )
            .await;
            return;
        }
    };
    let head_sha = pr.head.sha.as_str();
    let my_reviews = reviews
        .iter()
        .filter(|r| r.user.login.eq_ignore_ascii_case(me))
        .collect::<Vec<_>>();
    // Already approved the CURRENT head → nothing to do.
    let approved_head = my_reviews.iter().any(|r| {
        r.state.eq_ignore_ascii_case("APPROVED") && r.commit_id.as_deref() == Some(head_sha)
    });
    if approved_head {
        return;
    }

    // Master switch: the bot only acts (review or approve) when auto-approve is
    // on. Off = idle. Skip SILENTLY — emitting a per-PR "disabled" entry every
    // poll floods the (now persisted) activity log with noise.
    if !cfg.auto_approve_enabled {
        return;
    }

    if cfg.review_enabled {
        let tuples: Vec<(&str, Option<&str>, Marker)> = my_reviews
            .iter()
            .map(|r| (r.state.as_str(), r.commit_id.as_deref(), marker_of(&r.body)))
            .collect();
        match decide_review_action(&tuples, head_sha, cfg.approve_only_after_review) {
            ReviewAction::Skip => return,
            ReviewAction::ApproveOnly => {
                // Already reviewed earlier — just approve the new head (auto-approve
                // already confirmed above). Carry the prior review body so the UI
                // still shows it.
                let prior = my_reviews
                    .iter()
                    .rev()
                    .find(|r| marker_of(&r.body) == Marker::Reviewed)
                    .map(|r| r.body.replace(REVIEW_MARKER, "").trim().to_string());
                approve_only(app, state, client, cfg, repo_full, owner, repo, pr, prior).await;
                return;
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
        let diff = match client.get_pr_diff(owner, repo, pr.number).await {
            Ok(d) => d,
            Err(e) => {
                push_and_emit(app, state, err_entry(repo_full, pr, format!("get diff failed: {e}")))
                    .await;
                return;
            }
        };
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
            },
        )
        .await;
        let meta = format!(
            "Repository: {owner}/{repo}  PR #{}\nAuthor: {}\nTitle: {}\nBody:\n{pr_body}\n\n{context}",
            pr.number, pr.user.login, pr.title
        );
        let diff_for_gate = diff.clone();
        let guide = crate::review::load_guide(&state.config_dir, &cfg.review_guide_path, me);
        let model = cfg.review_model.clone();
        let tk = cfg.review_thinking_tokens;
        let deep = cfg.review_deep;
        let owner_s = owner.to_string();
        let repo_s = repo.to_string();
        let number = pr.number;
        let token = client.token().to_string();

        // `claude -p` can take minutes — keep it off the async runtime.
        let outcome = match tokio::task::spawn_blocking(move || {
            if deep {
                crate::review::review_pr_deep(
                    &guide, &meta, &diff, &model, tk, &owner_s, &repo_s, number, &token,
                )
            } else {
                crate::review::review_pr(&guide, &meta, &diff, &model, tk)
            }
        })
        .await
        {
            Ok(o) => o,
            Err(e) => {
                push_and_emit(app, state, err_entry(repo_full, pr, format!("review task failed: {e}")))
                    .await;
                return;
            }
        };

        let score_str = format!("{:.1}", outcome.score);
        let cost_str = outcome
            .cost_usd
            .map(|c| format!(", ${c:.2}"))
            .unwrap_or_default();
        // Code-side policy the model can't talk its way past: sensitive files
        // always go to a human (the #4022-style ".env-only PR got 5/5" case).
        let sensitive = crate::review::sensitive_files(&diff_for_gate);
        let decision = gate(
            outcome.finished_cleanly,
            &outcome.verdict,
            outcome.score,
            outcome.blocking_issues.len(),
            &sensitive,
            &outcome.omitted_files,
            cfg.min_approve_score,
        );
        let body = match &decision {
            Gate::HoldSensitive(files) => format!(
                "> ⚠️ **게이트**: 민감 파일 변경({}) — 자동 승인하지 않고 사람 승인으로 넘깁니다.\n\n{}",
                files.join(", "),
                outcome.body
            ),
            Gate::HoldOmitted(files) => format!(
                "> ⚠️ **게이트**: PR 이 커서 {}개 파일을 보지 못했습니다({}) — 자동 승인하지 않습니다.\n\n{}",
                files.len(),
                files.iter().take(5).cloned().collect::<Vec<_>>().join(", "),
                outcome.body
            ),
            _ => outcome.body.clone(),
        };
        // Posted to GitHub with the marker; the in-app detail keeps the clean body.
        // A failed run gets its own marker so the next commit is re-reviewed.
        let marker = if outcome.finished_cleanly { REVIEW_MARKER } else { FAILED_MARKER };
        let posted_body = format!("{marker}\n{body}");
        let inline: &[crate::github::ReviewComment] = if cfg.inline_comments_enabled {
            &outcome.inline
        } else {
            &[]
        };
        let inline_str = if inline.is_empty() {
            String::new()
        } else {
            format!(", 인라인 {}건", inline.len())
        };
        if decision == Gate::Approve {
            match client
                .approve_pull_with_comments(
                    owner,
                    repo,
                    pr.number,
                    Some(posted_body.as_str()),
                    inline,
                )
                .await
            {
                Ok(()) => {
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
                            message: format!("approved (리뷰 {score_str}/5{cost_str}{inline_str})"),
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
                    push_and_emit(app, state, err_entry(repo_full, pr, format!("approve failed: {e}")))
                        .await;
                }
            }
        } else {
            // Below threshold / blocking / unclean → post the review as a
            // COMMENT (substantive body still delivered), but do NOT approve.
            let why = decision.reason(outcome.blocking_issues.len(), &score_str, cfg.min_approve_score);
            match client
                .comment_pull(owner, repo, pr.number, &posted_body, inline)
                .await
            {
                Ok(()) => {
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
                            message: format!("리뷰 코멘트 게시 (approve 보류: {why}{inline_str})"),
                            detail: Some(outcome.body.clone()),
                        },
                    )
                    .await;
                }
                Err(e) => {
                    push_and_emit(app, state, err_entry(repo_full, pr, format!("comment failed: {e}")))
                        .await;
                }
            }
        }
        return;
    }

    // ── Legacy blind approve (review engine disabled) ──
    match client
        .approve_pull(owner, repo, pr.number, Some(&cfg.approval_message))
        .await
    {
        Ok(()) => {
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
            push_and_emit(app, state, err_entry(repo_full, pr, format!("approve failed: {e}"))).await;
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
    match client.approve_pull(owner, repo, pr.number, None).await {
        Ok(()) => {
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
            push_and_emit(app, state, err_entry(repo_full, pr, format!("approve failed: {e}")))
                .await;
        }
    }
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
    use super::{decide_review_action, gate, Gate, Marker, ReviewAction};

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
        assert_eq!(decide_review_action(&reviews, HEAD, false), ReviewAction::Review);
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
        assert_eq!(decide_review_action(&reviews, HEAD, true), ReviewAction::Review);
    }

    #[test]
    fn failed_run_on_old_commit_is_re_reviewed() {
        let reviews = [("DISMISSED", Some(OLD), R), ("COMMENTED", Some(OLD), F)];
        assert_eq!(decide_review_action(&reviews, HEAD, true), ReviewAction::Review);
    }

    #[test]
    fn failed_run_on_head_is_not_reposted() {
        let reviews = [("COMMENTED", Some(HEAD), F)];
        assert_eq!(decide_review_action(&reviews, HEAD, true), ReviewAction::Skip);
    }

    #[test]
    fn latest_engine_review_decides_approve_only() {
        let reviews = [("COMMENTED", Some("c1"), R), ("APPROVED", Some(OLD), R)];
        assert_eq!(decide_review_action(&reviews, HEAD, true), ReviewAction::ApproveOnly);
    }

    #[test]
    fn gate_rejects_approve_with_blocking_issues() {
        assert_eq!(gate(true, "approve", 5.0, 1, &[], &[], 4.0), Gate::HoldBlocking);
    }

    #[test]
    fn gate_holds_sensitive_files() {
        let files = vec![".env.production".to_string()];
        assert_eq!(gate(true, "approve", 5.0, 0, &files, &[], 4.0), Gate::HoldSensitive(files));
    }

    #[test]
    fn gate_holds_when_files_were_not_reviewed() {
        let omitted = vec!["src/big.ts".to_string()];
        assert_eq!(gate(true, "approve", 5.0, 0, &[], &omitted, 4.0), Gate::HoldOmitted(omitted));
    }

    #[test]
    fn gate_approves_only_when_all_hold() {
        assert_eq!(gate(true, "approve", 4.0, 0, &[], &[], 4.0), Gate::Approve);
        assert_eq!(gate(false, "approve", 5.0, 0, &[], &[], 4.0), Gate::HoldUnfinished);
        assert_eq!(gate(true, "comment", 5.0, 0, &[], &[], 4.0), Gate::HoldVerdict);
        assert_eq!(gate(true, "approve", 3.5, 0, &[], &[], 4.0), Gate::HoldScore);
    }
}
