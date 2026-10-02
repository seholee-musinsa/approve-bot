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

#[tauri::command]
pub async fn get_sweep_status(state: State<'_, Arc<AppState>>) -> Result<crate::sweep_sched::Status, String> {
    let s = state.config.lock().await.sweep.clone();
    let running = state.sweep_running.load(std::sync::atomic::Ordering::SeqCst);
    Ok(crate::sweep_sched::status_now(&state.config_dir, &s, running))
}

#[tauri::command]
pub async fn get_sweep_log(
    state: State<'_, Arc<AppState>>,
    limit: Option<usize>,
) -> Result<Vec<crate::sweep_sched::RunEntry>, String> {
    let mut log = crate::sweep_sched::load_log(&state.config_dir);
    log.truncate(limit.unwrap_or(30));
    Ok(log)
}

/// Starts a run now (in the background). Returns false when one is already running.
#[tauri::command]
pub async fn run_sweep_now(state: State<'_, Arc<AppState>>) -> Result<bool, String> {
    let s = state.config.lock().await.sweep.clone();
    if s.repo.is_empty() {
        return Err("대상 repo 를 먼저 입력해 주세요".into());
    }
    if state.sweep_running.load(std::sync::atomic::Ordering::SeqCst) {
        return Ok(false);
    }
    let st = Arc::clone(state.inner());
    tauri::async_runtime::spawn(async move {
        crate::sweep_sched::run_once(&st, &s).await;
    });
    Ok(true)
}

/// Back to the start of a cycle: slices unread, carry-over dropped, key history kept.
#[tauri::command]
pub async fn reset_sweep_cycle(state: State<'_, Arc<AppState>>) -> Result<(), String> {
    if state.sweep_running.load(std::sync::atomic::Ordering::SeqCst) {
        return Err("실행 중에는 초기화할 수 없습니다".into());
    }
    let mut l = crate::sweep_state::load(&state.config_dir);
    l.reset_cycle();
    crate::sweep_state::save(&state.config_dir, &l).map_err(|e| e.to_string())
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Assignee {
    pub id: String,
    pub name: String,
}

/// Jira side of the sweep as the screen sees it. `allow_create` is read only here:
/// it is the second lock on writing to Jira and is changed in the file by hand.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct JiraView {
    pub cloud_id: String,
    pub project: String,
    pub parent_key: String,
    pub open_cap: usize,
    pub assignees: Vec<Assignee>,
    #[serde(default)]
    pub allow_create: bool,
}

#[tauri::command]
pub async fn get_jira_settings(state: State<'_, Arc<AppState>>) -> Result<JiraView, String> {
    let c = crate::jira::load_or_default(&state.config_dir);
    Ok(JiraView {
        cloud_id: c.cloud_id.clone(),
        project: c.project.clone(),
        parent_key: c.parent_key.clone().unwrap_or_default(),
        open_cap: c.open_cap,
        assignees: c
            .assignees
            .iter()
            .map(|id| Assignee { id: id.clone(), name: c.assignee_names.get(id).cloned().unwrap_or_else(|| id.clone()) })
            .collect(),
        allow_create: c.allow_create,
    })
}

#[tauri::command]
pub async fn update_jira_settings(state: State<'_, Arc<AppState>>, view: JiraView) -> Result<JiraView, String> {
    let mut c = crate::jira::load_or_default(&state.config_dir);
    c.cloud_id = view.cloud_id.trim().to_string();
    let project = view.project.trim();
    if !project.is_empty() {
        c.project = project.to_string();
    }
    let parent = view.parent_key.trim().to_string();
    c.parent_key = if parent.is_empty() { None } else { Some(parent) };
    c.open_cap = view.open_cap.clamp(1, 100);
    c.assignees = view.assignees.iter().map(|a| a.id.clone()).collect();
    c.assignee_names = view.assignees.into_iter().map(|a| (a.id, a.name)).collect();
    // allow_create is deliberately not taken from the screen.
    crate::jira::save_config(&state.config_dir, &c).map_err(|e| format!("{e:#}"))?;
    get_jira_settings(state).await
}

fn jira_client(dir: &std::path::Path) -> Result<(crate::jira::JiraConfig, crate::jira::Jira), String> {
    let c = crate::jira::load_config(dir).map_err(|e| format!("{e:#}"))?;
    if c.cloud_id.is_empty() {
        return Err("cloud id 를 먼저 저장해 주세요".into());
    }
    let j = crate::jira::Jira::connect(&c).map_err(|e| format!("{e:#}"))?;
    Ok((c, j))
}

#[tauri::command]
pub async fn search_jira_users(state: State<'_, Arc<AppState>>, query: String) -> Result<Vec<Assignee>, String> {
    let q = query.trim();
    if q.len() < 2 {
        return Ok(vec![]);
    }
    let (_, j) = jira_client(&state.config_dir)?;
    let users = j.search_users(q).await.map_err(|e| format!("{e:#}"))?;
    Ok(users.into_iter().map(|(id, name)| Assignee { id, name }).collect())
}

/// Read only: who the credentials belong to, and whether the parent Epic is usable.
#[tauri::command]
pub async fn test_jira_connection(state: State<'_, Arc<AppState>>) -> Result<String, String> {
    let (c, j) = jira_client(&state.config_dir)?;
    let me = j.myself().await.map_err(|e| format!("{e:#}"))?;
    let epic = match &c.parent_key {
        None => "부모 Epic 미설정(부모 없이 만들고 나중에 지정)".to_string(),
        Some(k) => match j.issue_brief(k).await {
            Ok((t, true)) => format!("⚠️ 부모 Epic {k} 가 완료 상태입니다: {t}"),
            Ok((t, false)) => format!("부모 Epic {k}: {t}"),
            Err(e) => format!("⚠️ 부모 Epic {k} 를 읽지 못함: {e:#}"),
        },
    };
    Ok(format!("연결됨: {me} · {epic}"))
}

#[derive(serde::Serialize)]
pub struct OrphanTicket {
    pub key: String,
    pub summary: String,
}

#[derive(serde::Serialize)]
pub struct ResultsView {
    pub results: crate::jira::Results,
    /// 부모 없는 봇 티켓(4.14).
    pub orphans: Vec<OrphanTicket>,
    pub parent_key: String,
    pub allow_create: bool,
}

/// Read Jira for the bot's tickets, fold what happened to them into the ledger
/// (so rejections and completions steer later proposals), and total the results.
#[tauri::command]
pub async fn collect_sweep_results(state: State<'_, Arc<AppState>>) -> Result<ResultsView, String> {
    let (c, j) = jira_client(&state.config_dir)?;
    let issues = j.search(&crate::jira::jql_bot_all(&c), 500).await.map_err(|e| format!("{e:#}"))?;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let mut ledger = crate::sweep_state::load(&state.config_dir);
    for i in &issues {
        for (k, o) in crate::jira::outcomes(i, now) {
            ledger.merge_outcome(&k, o);
        }
    }
    crate::sweep_state::save(&state.config_dir, &ledger).map_err(|e| e.to_string())?;
    let orphans = issues
        .iter()
        .filter(|i| i.parent.is_none() && !i.done)
        .map(|i| OrphanTicket { key: i.key.clone(), summary: i.summary.clone() })
        .collect();
    Ok(ResultsView {
        results: crate::jira::results(&issues, now, 30),
        orphans,
        parent_key: c.parent_key.clone().unwrap_or_default(),
        allow_create: c.allow_create,
    })
}

/// Write: put the listed bot tickets under the configured parent Epic. Needs
/// `allow_create`, and refuses a parent that is already done. Only tickets that
/// carry the bot label and have no parent are touched.
#[tauri::command]
pub async fn assign_parent_bulk(state: State<'_, Arc<AppState>>, keys: Vec<String>) -> Result<String, String> {
    let (c, j) = jira_client(&state.config_dir)?;
    if !c.allow_create {
        return Err("Jira 쓰기가 꺼져 있습니다(sweep-jira.json 의 allow_create)".into());
    }
    let parent = c.parent_key.clone().ok_or_else(|| "부모 Epic 이 설정되어 있지 않습니다".to_string())?;
    let (title, done) = j.issue_brief(&parent).await.map_err(|e| format!("{e:#}"))?;
    if done {
        return Err(format!("부모 Epic {parent} 가 완료 상태라 지정하지 않았습니다: {title}"));
    }
    // 화면에서 넘어온 키를 믿지 않고 지금 부모 없는 봇 티켓인지 다시 확인한다.
    let orphans = j.search(&crate::jira::jql_bot_orphans(&c), 500).await.map_err(|e| format!("{e:#}"))?;
    let allowed: std::collections::HashSet<&str> = orphans.iter().map(|i| i.key.as_str()).collect();
    let (mut ok, mut skipped, mut failed) = (0, 0, Vec::new());
    for k in &keys {
        if !allowed.contains(k.as_str()) {
            skipped += 1;
            continue;
        }
        match j.set_parent(&c, k, &parent).await {
            Ok(()) => ok += 1,
            Err(e) => failed.push(format!("{k}: {e:#}")),
        }
    }
    let mut msg = format!("{parent} 아래로 {ok}건 지정");
    if skipped > 0 {
        msg.push_str(&format!(" · 건너뜀 {skipped}건(이미 부모가 있거나 봇 티켓이 아님)"));
    }
    if !failed.is_empty() {
        msg.push_str(&format!(" · 실패 {}건: {}", failed.len(), failed.join(" / ")));
    }
    Ok(msg)
}
