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

/// Pure dedup decision. `reviews` = my prior reviews as (state, commit_id, is
/// engine-marker). Only marker reviews count as "reviewed" — a blind 👍 approve
/// or another bot's review must not suppress the first real review.
fn decide_review_action(
    reviews: &[(&str, Option<&str>, bool)],
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
    if reviews
        .iter()
        .any(|(s, c, marker)| *marker && !s.eq_ignore_ascii_case("DISMISSED") && *c == Some(head_sha))
    {
        return ReviewAction::Skip;
    }
    // Engine-reviewed an earlier commit → optionally approve without re-review.
    if approve_only_after_review && reviews.iter().any(|(_, _, marker)| *marker) {
        return ReviewAction::ApproveOnly;
    }
    ReviewAction::Review
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
        let tuples: Vec<(&str, Option<&str>, bool)> = my_reviews
            .iter()
            .map(|r| {
                (
                    r.state.as_str(),
                    r.commit_id.as_deref(),
                    r.body.contains(REVIEW_MARKER),
                )
            })
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
                    .find(|r| r.body.contains(REVIEW_MARKER))
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
        let meta = format!(
            "Repository: {owner}/{repo}  PR #{}\nAuthor: {}\nTitle: {}\nBody:\n(none)",
            pr.number, pr.user.login, pr.title
        );
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
        // Posted to GitHub with the marker; the in-app detail keeps the clean body.
        let posted_body = format!("{REVIEW_MARKER}\n{}", outcome.body);
        let inline_str = if outcome.inline.is_empty() {
            String::new()
        } else {
            format!(", 인라인 {}건", outcome.inline.len())
        };
        let approve = outcome.finished_cleanly
            && outcome.verdict == "approve"
            && outcome.score >= cfg.min_approve_score;

        if approve {
            match client
                .approve_pull_with_comments(
                    owner,
                    repo,
                    pr.number,
                    Some(posted_body.as_str()),
                    &outcome.inline,
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
            let why = if !outcome.finished_cleanly {
                "리뷰 미완료".to_string()
            } else if !outcome.blocking_issues.is_empty() {
                format!("blocking {}건", outcome.blocking_issues.len())
            } else {
                format!("점수 {score_str}/5 < {:.1}", cfg.min_approve_score)
            };
            match client
                .comment_pull(owner, repo, pr.number, &posted_body, &outcome.inline)
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
    let msg = if cfg.approval_message.trim().is_empty() {
        "이전 리뷰 확인됨 — 새 커밋 자동 승인".to_string()
    } else {
        cfg.approval_message.clone()
    };
    match client.approve_pull(owner, repo, pr.number, Some(msg.as_str())).await {
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
    use super::{decide_review_action, ReviewAction};

    const HEAD: &str = "aaaa1111";
    const OLD: &str = "bbbb2222";

    #[test]
    fn blind_emoji_approve_does_not_suppress_first_review() {
        // The #1876 bug: a dismissed blind 👍 (no marker) must NOT count as reviewed.
        let reviews = [("DISMISSED", Some(OLD), false)];
        assert_eq!(decide_review_action(&reviews, HEAD, true), ReviewAction::Review);
    }

    #[test]
    fn no_prior_reviews_reviews() {
        assert_eq!(decide_review_action(&[], HEAD, true), ReviewAction::Review);
    }

    #[test]
    fn engine_review_on_current_head_skips() {
        let reviews = [("COMMENTED", Some(HEAD), true)];
        assert_eq!(decide_review_action(&reviews, HEAD, true), ReviewAction::Skip);
    }

    #[test]
    fn approved_current_head_skips() {
        let reviews = [("APPROVED", Some(HEAD), false)];
        assert_eq!(decide_review_action(&reviews, HEAD, true), ReviewAction::Skip);
    }

    #[test]
    fn engine_review_on_old_commit_approve_only_when_enabled() {
        let reviews = [("DISMISSED", Some(OLD), true)];
        assert_eq!(decide_review_action(&reviews, HEAD, true), ReviewAction::ApproveOnly);
    }

    #[test]
    fn engine_review_on_old_commit_re_reviews_when_approve_only_off() {
        let reviews = [("DISMISSED", Some(OLD), true)];
        assert_eq!(decide_review_action(&reviews, HEAD, false), ReviewAction::Review);
    }

    #[test]
    fn dismissed_engine_review_on_head_re_reviews() {
        // Marker review but dismissed on the current head → stale → re-review.
        let reviews = [("DISMISSED", Some(HEAD), true)];
        assert_eq!(decide_review_action(&reviews, HEAD, false), ReviewAction::Review);
    }
}
