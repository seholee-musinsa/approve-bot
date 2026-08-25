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
                },
            )
            .await;
            return;
        }
    };
    let already_approved = reviews
        .iter()
        .any(|r| r.user.login.eq_ignore_ascii_case(me) && r.state.eq_ignore_ascii_case("APPROVED"));
    if already_approved {
        return;
    }

    if !cfg.auto_approve_enabled {
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
                message: "auto-approve is disabled".into(),
            },
        )
        .await;
        return;
    }

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
                    message: format!("approve failed: {e}"),
                },
            )
            .await;
        }
    }
}

async fn push_and_emit(app: &AppHandle, state: &Arc<AppState>, entry: ActivityEntry) {
    state.push_activity(entry.clone()).await;
    let _ = app.emit(ACTIVITY_EVENT, &entry);
}
