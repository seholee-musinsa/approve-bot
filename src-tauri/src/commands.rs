use crate::config::{self, AppConfig};
use crate::github::{GhUserHint, GitHubClient};
use crate::poller;
use crate::state::{ActivityEntry, AppState, ConnectionStatus};
use std::sync::Arc;
use tauri::{AppHandle, State};

#[tauri::command]
pub async fn get_connection_status(state: State<'_, Arc<AppState>>) -> Result<ConnectionStatus, String> {
    Ok(state.status.lock().await.clone())
}

#[tauri::command]
pub async fn reconnect(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
) -> Result<ConnectionStatus, String> {
    poller::try_connect(&app, state.inner()).await;
    state.poll_signal.notify_one();
    Ok(state.status.lock().await.clone())
}

#[tauri::command]
pub async fn get_config(state: State<'_, Arc<AppState>>) -> Result<AppConfig, String> {
    Ok(state.config.lock().await.clone())
}

#[tauri::command]
pub async fn update_config(
    state: State<'_, Arc<AppState>>,
    config: AppConfig,
) -> Result<AppConfig, String> {
    let mut cleaned = config;
    cleaned.clamp();
    config::save(&state.config_dir, &cleaned).map_err(|e| e.to_string())?;
    *state.config.lock().await = cleaned.clone();
    state.poll_signal.notify_one();
    Ok(cleaned)
}

#[tauri::command]
pub async fn get_activity_log(
    state: State<'_, Arc<AppState>>,
    limit: Option<usize>,
) -> Result<Vec<ActivityEntry>, String> {
    Ok(state.recent_activity(limit.unwrap_or(100)).await)
}

#[tauri::command]
pub async fn clear_activity_log(state: State<'_, Arc<AppState>>) -> Result<(), String> {
    state.clear_activity().await;
    Ok(())
}

#[tauri::command]
pub async fn force_check_now(state: State<'_, Arc<AppState>>) -> Result<(), String> {
    state.poll_signal.notify_one();
    Ok(())
}

#[tauri::command]
pub async fn start_gh_login(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
) -> Result<(), String> {
    let state = Arc::clone(state.inner());
    tauri::async_runtime::spawn(async move {
        let _ = crate::gh_login::start(app, state).await;
    });
    Ok(())
}

#[tauri::command]
pub async fn search_users(
    state: State<'_, Arc<AppState>>,
    query: String,
) -> Result<Vec<GhUserHint>, String> {
    let token = state
        .token
        .lock()
        .await
        .clone()
        .ok_or_else(|| "not connected to GitHub".to_string())?;
    let client = GitHubClient::new(token);
    client.search_users(&query, 8).await.map_err(|e| e.to_string())
}
