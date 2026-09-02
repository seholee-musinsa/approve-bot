mod auth;
mod commands;
mod config;
mod gh_login;
mod github;
mod poller;
mod review;
mod state;

use std::sync::Arc;
use tauri::Manager;
use tracing::warn;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
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

            let cfg = match config::load(&config_dir) {
                Ok(c) => c,
                Err(e) => {
                    warn!(error = %e, "failed to load config; using defaults");
                    config::AppConfig::default()
                }
            };
            let state = state::AppState::new(config_dir, cfg);
            app.manage(Arc::clone(&state));

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
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
