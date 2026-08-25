use anyhow::{anyhow, Context, Result};
use std::path::Path;
use std::process::Command;

/// Locate the `gh` CLI binary as an absolute path.
///
/// GUI-launched apps (Finder/`open`) inherit a minimal PATH (`/usr/bin:/bin:…`)
/// that omits Homebrew (`/opt/homebrew/bin`) and `/usr/local/bin`, so a bare
/// `Command::new("gh")` fails to spawn. Resolve from `GH_PATH` or common install
/// locations, falling back to `"gh"` (works when PATH is already complete, e.g. dev).
pub fn gh_bin() -> String {
    if let Ok(p) = std::env::var("GH_PATH") {
        if !p.is_empty() {
            return p;
        }
    }
    for candidate in ["/opt/homebrew/bin/gh", "/usr/local/bin/gh", "/usr/bin/gh"] {
        if Path::new(candidate).exists() {
            return candidate.to_string();
        }
    }
    "gh".to_string()
}

/// Fetch the GitHub OAuth/PAT token currently in use by the local `gh` CLI.
///
/// Token is held only in memory by the caller — never written to disk.
pub fn fetch_gh_token() -> Result<String> {
    let output = Command::new(gh_bin())
        .args(["auth", "token"])
        .output()
        .map_err(|e| {
            anyhow!(
                "failed to spawn `gh`: {e}. Install GitHub CLI from https://cli.github.com/."
            )
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("not logged") || stderr.contains("authentication") {
            return Err(anyhow!(
                "gh CLI is not authenticated. Run `gh auth login` first."
            ));
        }
        return Err(anyhow!("`gh auth token` failed: {}", stderr.trim()));
    }

    let token = String::from_utf8(output.stdout)
        .context("gh auth token returned non-UTF8 output")?
        .trim()
        .to_string();

    if token.is_empty() {
        return Err(anyhow!("gh auth token returned empty string"));
    }

    Ok(token)
}
