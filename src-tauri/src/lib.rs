mod auth;
mod audit;
mod audit_import;
mod commands;
mod config;
mod confluence;
mod context;
mod diffprep;
mod eval_cli;
mod feedback;
mod gh_login;
mod github;
mod layout;
mod poller;
mod publication;
mod repocache;
mod rereview;
mod routing;
mod retry;
mod trace;
mod review;
mod state;
mod sweep;
mod jira;
mod report_job;
mod sweep_day;
mod sweep_report;
mod sweep_review;
mod sweep_sched;
mod sweep_state;

use std::sync::Arc;
use tauri::Manager;
use tracing::warn;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Headless `review-once` eval subcommand — handles argv and exits before the
    // GUI ever starts. No-op for a normal (GUI) launch.
    if eval_cli::maybe_run_headless() {
        return;
    }

    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "approve_bot_lib=info,warn".into()),
        )
        .try_init();

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_notification::init())
        .setup(|app| {
            let config_dir = app
                .path()
                .app_config_dir()
                .expect("could not resolve app config dir");
            std::fs::create_dir_all(&config_dir).ok();

            let mut cfg = match config::load(&config_dir) {
                Ok(c) => c,
                Err(e) => {
                    warn!(error = %e, "failed to load config; using defaults");
                    config::AppConfig::default()
                }
            };
            // Older builds read `review-guide.md` from the config dir ahead of the
            // bundle; the copy silently went stale. Move it aside or make it explicit.
            let migration = review::guide_migrate(&config_dir, &mut cfg);
            if matches!(migration, Some(review::GuideMigration::PromotedToExplicit(_))) {
                if let Err(e) = config::save(&config_dir, &cfg) {
                    warn!(error = %e, "could not save the promoted review guide path");
                }
            }
            let state = state::AppState::new(config_dir, cfg);
            app.manage(Arc::clone(&state));
            if let Some(m) = migration {
                let (message, detail) = match m {
                    review::GuideMigration::RenamedStale(target) => (
                        "옛 리뷰 가이드 복사본을 치우고 번들 가이드를 사용합니다".to_string(),
                        Some(format!("{}", target.display())),
                    ),
                    review::GuideMigration::PromotedToExplicit(path) => (
                        "직접 고친 리뷰 가이드를 설정의 명시 경로로 옮겼습니다. 버전 마커가 없어 stale 로 표시됩니다".to_string(),
                        Some(format!("{}", path.display())),
                    ),
                };
                let state = Arc::clone(&state);
                tauri::async_runtime::spawn(async move {
                    state.push_activity(state::ActivityEntry {
                        timestamp: chrono::Utc::now(),
                        kind: state::ActivityKind::Info,
                        repo: None, pr_number: None, pr_title: None, author: None, url: None,
                        message, detail,
                    }).await;
                });
            }

            sweep_sched::spawn(Arc::clone(&state));
            poller::spawn(app.handle().clone(), state);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_connection_status,
            commands::reconnect,
            commands::get_config,
            commands::update_config,
            commands::get_activity_log,
            commands::clear_activity_log,
            commands::force_check_now,
            commands::search_users,
            commands::start_gh_login,
            commands::get_sweep_status,
            commands::get_sweep_log,
            commands::run_sweep_now,
            commands::reset_sweep_cycle,
            commands::get_jira_settings,
            commands::update_jira_settings,
            commands::search_jira_users,
            commands::test_jira_connection,
            commands::collect_sweep_results,
            commands::assign_parent_bulk,
            commands::list_bot_tickets,
            commands::list_reports,
            commands::read_report,
            commands::generate_report,
            commands::list_waiting_candidates,
            commands::create_waiting_candidates,
            commands::discard_waiting_candidates,
            commands::publish_report,
            commands::check_report_parent,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
