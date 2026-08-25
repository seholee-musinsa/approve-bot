use crate::poller;
use crate::state::AppState;
use anyhow::{anyhow, Result};
use serde::Serialize;
use std::process::Stdio;
use std::sync::Arc;
use tauri::{AppHandle, Emitter};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{ChildStdin, Command};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

pub const PROGRESS_EVENT: &str = "approve-bot://gh-login";

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GhLoginProgress {
    Started,
    Code { code: String },
    Done,
    Failed { message: String },
}

pub async fn start(app: AppHandle, state: Arc<AppState>) -> Result<()> {
    {
        let mut guard = state.gh_login_busy.lock().await;
        if *guard {
            return Err(anyhow!("login already in progress"));
        }
        *guard = true;
    }

    let result = run(&app).await;
    *state.gh_login_busy.lock().await = false;

    match &result {
        Ok(()) => {
            let _ = app.emit(PROGRESS_EVENT, &GhLoginProgress::Done);
            poller::try_connect(&app, &state).await;
            state.poll_signal.notify_one();
        }
        Err(e) => {
            let _ = app.emit(
                PROGRESS_EVENT,
                &GhLoginProgress::Failed {
                    message: e.to_string(),
                },
            );
        }
    }
    result
}

async fn run(app: &AppHandle) -> Result<()> {
    let _ = app.emit(PROGRESS_EVENT, &GhLoginProgress::Started);

    let mut child = Command::new(crate::auth::gh_bin())
        .args([
            "auth",
            "login",
            "--hostname",
            "github.com",
            "--git-protocol",
            "https",
            "--web",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| {
            anyhow!("failed to spawn `gh`: {e}. Install GitHub CLI from https://cli.github.com/.")
        })?;

    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");
    let stdin = Arc::new(Mutex::new(child.stdin.take().expect("stdin piped")));

    let stdin_out = Arc::clone(&stdin);
    let stdin_err = Arc::clone(&stdin);
    let app_out = app.clone();
    let app_err = app.clone();

    let out_task = tokio::spawn(async move {
        pump_stream(stdout, stdin_out, app_out, "stdout").await;
    });
    let err_task = tokio::spawn(async move {
        pump_stream(stderr, stdin_err, app_err, "stderr").await;
    });

    let status = child.wait().await?;
    let _ = out_task.await;
    let _ = err_task.await;

    if !status.success() {
        return Err(anyhow!("`gh auth login` exited with {status}"));
    }
    Ok(())
}

async fn pump_stream<R: AsyncReadExt + Unpin>(
    mut reader: R,
    stdin: Arc<Mutex<ChildStdin>>,
    app: AppHandle,
    label: &'static str,
) {
    let mut acc = String::new();
    let mut buf = [0u8; 512];
    let mut detector = PromptDetector::default();
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                let chunk = String::from_utf8_lossy(&buf[..n]);
                debug!(target: "gh_login", stream = label, "{}", chunk);
                acc.push_str(&chunk);
                if acc.len() > 16 * 1024 {
                    let drop_to = acc.len() - 8 * 1024;
                    acc.drain(..drop_to);
                }
                detector.process(&acc, &app, &stdin).await;
            }
            Err(e) => {
                warn!(error = %e, stream = label, "read failed");
                break;
            }
        }
    }
}

#[derive(Default)]
struct PromptDetector {
    emitted_code: bool,
    responded_reauth: bool,
    responded_git_creds: bool,
    responded_press_enter: bool,
    responded_protocol: bool,
    responded_ssh_key: bool,
}

impl PromptDetector {
    async fn process(&mut self, buf: &str, app: &AppHandle, stdin: &Arc<Mutex<ChildStdin>>) {
        if !self.emitted_code {
            if let Some(code) = extract_one_time_code(buf) {
                info!(code = %code, "gh device code received");
                let _ = app.emit(PROGRESS_EVENT, &GhLoginProgress::Code { code });
                self.emitted_code = true;
            }
        }
        let lower = buf.to_lowercase();
        if !self.responded_reauth
            && (lower.contains("re-authenticate") || lower.contains("already logged"))
        {
            write_response(stdin, "Y").await;
            self.responded_reauth = true;
        }
        if !self.responded_git_creds
            && lower.contains("authenticate git with your github credentials")
        {
            write_response(stdin, "Y").await;
            self.responded_git_creds = true;
        }
        if !self.responded_protocol && lower.contains("preferred protocol for git operations") {
            write_response(stdin, "HTTPS").await;
            self.responded_protocol = true;
        }
        if !self.responded_ssh_key && lower.contains("upload your ssh public key") {
            write_response(stdin, "Skip").await;
            self.responded_ssh_key = true;
        }
        if !self.responded_press_enter && lower.contains("press enter") {
            write_response(stdin, "").await;
            self.responded_press_enter = true;
        }
    }
}

async fn write_response(stdin: &Arc<Mutex<ChildStdin>>, text: &str) {
    let mut g = stdin.lock().await;
    let payload = format!("{text}\n");
    if let Err(e) = g.write_all(payload.as_bytes()).await {
        warn!(error = %e, "failed to write to gh stdin");
        return;
    }
    let _ = g.flush().await;
}

fn extract_one_time_code(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let n = bytes.len();
    if n < 9 {
        return None;
    }
    for i in 0..=n - 9 {
        if bytes[i + 4] != b'-' {
            continue;
        }
        let left_ok = (0..4).all(|j| is_code_char(bytes[i + j]));
        let right_ok = (5..9).all(|j| is_code_char(bytes[i + j]));
        if !left_ok || !right_ok {
            continue;
        }
        let before_ok = i == 0 || !is_code_char(bytes[i - 1]);
        let after_ok = i + 9 == n || !is_code_char(bytes[i + 9]);
        if before_ok && after_ok {
            let slice = &bytes[i..i + 9];
            return std::str::from_utf8(slice).ok().map(|s| s.to_uppercase());
        }
    }
    None
}

fn is_code_char(b: u8) -> bool {
    b.is_ascii_alphanumeric()
}
