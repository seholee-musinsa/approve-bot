//! Headless `review-once` subcommand — runs the REAL review engine on a single
//! PR and prints the outcome as JSON. Exists so the eval harness can A/B two
//! review guides against the same PRs through the exact production code path
//! (no prompt re-implementation, zero drift). Never launches the Tauri GUI.
//!
//! Usage:
//!   approve-bot review-once --pr <url> [--guide <path>] [--sha <commit>]
//!                           [--diff-only] [--model <id>] [--thinking <n>]
//!
//! `--guide` overrides the team guide (empty/omitted = the config-dir guide,
//! same as the running bot). Deep mode by default, like the running bot;
//! `--diff-only` turns it off. `--sha` reviews that commit instead of the PR's
//! current head (diff = `<base>...<sha>`), so a case can be pinned to the
//! commit a human reviewed before the fixes landed.

use crate::{auth, github, review};

/// Inspect argv. If the first arg is `review-once`, handle it headlessly and
/// return `true` (caller must exit without starting Tauri). Otherwise `false`.
pub fn maybe_run_headless() -> bool {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) != Some("review-once") {
        return false;
    }
    match run(&args[1..]) {
        Ok(json) => {
            println!("{json}");
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("review-once failed: {e:#}");
            std::process::exit(1);
        }
    }
}

struct Args {
    pr: String,
    guide: String,
    sha: Option<String>,
    deep: bool,
    print_prompt: bool,
    /// Earlier `review-once` output + the commit it reviewed → follow-up round.
    prev: Option<String>,
    prev_sha: Option<String>,
    /// None = route by risk like the running bot.
    model: Option<String>,
    thinking: Option<u32>,
}

fn parse(flags: &[String]) -> anyhow::Result<Args> {
    // Mirror the production config defaults so an eval run matches the live bot.
    let mut a = Args {
        pr: String::new(),
        guide: String::new(),
        sha: None,
        deep: true,
        print_prompt: false,
        prev: None,
        prev_sha: None,
        model: None,
        thinking: None,
    };
    let mut i = 0;
    while i < flags.len() {
        match flags[i].as_str() {
            "--pr" => {
                a.pr = take(flags, &mut i, "--pr")?;
            }
            "--guide" => {
                a.guide = take(flags, &mut i, "--guide")?;
            }
            "--model" => {
                a.model = Some(take(flags, &mut i, "--model")?);
            }
            "--thinking" => {
                a.thinking = Some(take(flags, &mut i, "--thinking")?.parse()?);
            }
            "--sha" => {
                a.sha = Some(take(flags, &mut i, "--sha")?);
            }
            // Kept so older scripts still run; deep is the default now.
            "--deep" => {
                a.deep = true;
            }
            "--diff-only" => {
                a.deep = false;
            }
            // Print the assembled prompt and exit, no model call.
            "--print-prompt" => {
                a.print_prompt = true;
            }
            "--prev" => {
                a.prev = Some(take(flags, &mut i, "--prev")?);
            }
            "--prev-sha" => {
                a.prev_sha = Some(take(flags, &mut i, "--prev-sha")?);
            }
            other => return Err(anyhow::anyhow!("unknown flag: {other}")),
        }
        i += 1;
    }
    if a.pr.is_empty() {
        return Err(anyhow::anyhow!("--pr <url> is required"));
    }
    Ok(a)
}

fn take(flags: &[String], i: &mut usize, name: &str) -> anyhow::Result<String> {
    *i += 1;
    flags
        .get(*i)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("{name} needs a value"))
}

/// Parse `https://github.com/<owner>/<repo>/pull/<number>` (trailing path ok).
fn parse_pr_url(url: &str) -> anyhow::Result<(String, String, u64)> {
    let rest = url
        .split("github.com/")
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("not a github.com PR url: {url}"))?;
    let parts: Vec<&str> = rest.split('/').collect();
    // owner / repo / "pull" / number
    if parts.len() < 4 || parts[2] != "pull" {
        return Err(anyhow::anyhow!("expected .../<owner>/<repo>/pull/<n>: {url}"));
    }
    let number: u64 = parts[3]
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .unwrap_or("")
        .parse()
        .map_err(|_| anyhow::anyhow!("no PR number in url: {url}"))?;
    Ok((parts[0].to_string(), parts[1].to_string(), number))
}

fn run(flags: &[String]) -> anyhow::Result<String> {
    let args = parse(flags)?;
    let (owner, repo, number) = parse_pr_url(&args.pr)?;

    let token = auth::fetch_gh_token()?;
    let client = github::GitHubClient::new(token);

    // Single blocking Tokio runtime for the network calls.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let (login, pr, diff, context, round) = rt.block_on(async {
        let (me, _) = client.get_user().await?;
        let pr = client.get_pull(&owner, &repo, number).await?;
        let diff = match &args.sha {
            Some(sha) => {
                // The branch name moves: once the PR merges, main...sha is empty.
                let base = if pr.base.sha.is_empty() { &pr.base.ref_name } else { &pr.base.sha };
                client.get_compare_diff(&owner, &repo, base, sha).await?
            },
            None => client.get_pr_diff(&owner, &repo, number).await?,
        };
        // Same context the poller gathers.
        let changed = crate::diffprep::changed_paths(&diff);
        let added = crate::diffprep::added_lines(&diff);
        let triggers = crate::context::load_triggers(&config_dir()?, &format!("{owner}/{repo}"));
        // A pinned commit sees only the threads written before it; otherwise
        // later reviews that name the scored defects would leak the answers.
        let until = match &args.sha {
            Some(sha) => Some(client.get_commit_date(&owner, &repo, sha).await?),
            None => None,
        };
        let context = crate::context::gather(
            &client,
            &crate::context::Inputs {
                owner: &owner,
                repo: &repo,
                number,
                base_ref: &pr.base.ref_name,
                changed: &changed,
                me: &me.login,
                added: &added,
                triggers: &triggers,
                until: until.as_deref(),
            },
        )
        .await;
        // Follow-up round: earlier findings come from the earlier run's JSON
        // (the bot never posted them), the delta from the two commits.
        let round = match (&args.prev, &args.prev_sha, &args.sha) {
            (Some(prev), Some(prev_sha), Some(sha)) => {
                let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(prev)?)?;
                let findings: Vec<review::Finding> =
                    serde_json::from_value(v["findings"].clone()).unwrap_or_default();
                let prev_findings = findings
                    .iter()
                    .filter(|f| matches!(f.severity.as_str(), "blocker" | "major" | "minor"))
                    .map(|f| crate::rereview::PrevFinding {
                        id: review::finding_id(&f.path, &f.symbol, &f.claim),
                        severity: f.severity.clone(),
                        path: f.path.clone(),
                        line: f.line,
                        claim: f.claim.clone(),
                    })
                    .collect();
                let delta_diff = client.get_compare_diff(&owner, &repo, prev_sha, sha).await?;
                Some(crate::rereview::Round { number: 2, prev_sha: prev_sha.clone(), prev: prev_findings, delta_diff })
            }
            (None, None, _) => None,
            _ => return Err(anyhow::anyhow!("--prev and --prev-sha go together, with --sha")),
        };
        anyhow::Ok((me.login, pr, diff, format!("{context}\n\n{}", round.as_ref().map(crate::rereview::render).unwrap_or_default()), round))
    })?;

    // Resolve the guide exactly like the poller: `--guide` path wins, else the
    // config-dir copy, else the bundled default. `{{GITHUB_LOGIN}}` substituted.
    let config_dir = config_dir()?;
    let guide = review::load_guide(&config_dir, &args.guide, &login);

    // Build the PR meta block identically to poller.rs (capped body).
    let meta = format!("{}\n\n{context}", build_meta(&owner, &repo, &pr));

    // Route like the running bot unless --model/--thinking pin it.
    let (risk, risk_why) = crate::routing::classify(&diff);
    let (routed_model, routed_thinking) = crate::routing::pick(&crate::config::AppConfig::default(), risk);
    let model = args.model.clone().unwrap_or(routed_model);
    let thinking = args.thinking.unwrap_or(routed_thinking);

    if args.print_prompt {
        return Ok(review::preview_prompt(&guide, &meta, &diff, args.deep));
    }

    let token2 = client.token().to_string();
    let outcome = if args.deep {
        review::review_pr_deep_at(
            &guide,
            &meta,
            &diff,
            &model,
            thinking,
            &owner,
            &repo,
            number,
            &token2,
            args.sha.as_deref(),
        )
    } else {
        review::review_pr(&guide, &meta, &diff, &model, thinking)
    };

    // Emit a stable JSON shape the runner can parse.
    let inline: Vec<serde_json::Value> = outcome
        .inline
        .iter()
        .map(|c| {
            serde_json::json!({ "path": c.path, "line": c.line, "comment": c.body })
        })
        .collect();
    let out = serde_json::json!({
        "pr": args.pr,
        "owner_repo": format!("{owner}/{repo}"),
        "number": number,
        "guide": if args.guide.is_empty() { "config-dir/default".into() } else { args.guide.clone() },
        "deep": args.deep,
        "explored": outcome.explored,
        "omitted_files": outcome.omitted_files,
        "findings": outcome.findings,
        "followups": outcome.followups,
        "open_prev_blockers": round
            .as_ref()
            .map(|r| crate::rereview::open_blockers(&r.prev, &outcome.followups))
            .unwrap_or_default(),
        "sha": args.sha,
        "model": model,
        "thinking": thinking,
        "risk": risk.label(),
        "risk_why": risk_why,
        "verdict": outcome.verdict,
        "score": outcome.score,
        "finished_cleanly": outcome.finished_cleanly,
        "blocking_issues": outcome.blocking_issues,
        "inline": inline,
        "cost_usd": outcome.cost_usd,
        "body": outcome.body,
    });
    Ok(serde_json::to_string(&out)?)
}

/// Same meta format the poller feeds the reviewer (title + capped body).
fn build_meta(owner: &str, repo: &str, pr: &github::PullRequest) -> String {
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
    format!(
        "Repository: {owner}/{repo}  PR #{}\nAuthor: {}\nTitle: {}\nBody:\n{pr_body}",
        pr.number, pr.user.login, pr.title
    )
}

/// The app config dir (same one the GUI uses), so `--guide ""` resolves the
/// live bot's guide. Falls back to the OS config dir convention.
fn config_dir() -> anyhow::Result<std::path::PathBuf> {
    let base = dirs_config_dir().ok_or_else(|| anyhow::anyhow!("no config dir"))?;
    Ok(base.join("com.approvebot.app"))
}

#[cfg(target_os = "macos")]
fn dirs_config_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME")
        .map(|h| std::path::PathBuf::from(h).join("Library/Application Support"))
}

#[cfg(not(target_os = "macos"))]
fn dirs_config_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config"))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pr_url() {
        let (o, r, n) = parse_pr_url("https://github.com/musinsa/core-partner-frontend/pull/5637").unwrap();
        assert_eq!((o.as_str(), r.as_str(), n), ("musinsa", "core-partner-frontend", 5637));
    }

    #[test]
    fn parses_pr_url_trailing_path() {
        let (_, _, n) = parse_pr_url("https://github.com/a/b/pull/12/files").unwrap();
        assert_eq!(n, 12);
    }

    #[test]
    fn rejects_non_pr_url() {
        assert!(parse_pr_url("https://github.com/a/b/issues/3").is_err());
    }

    #[test]
    fn defaults_match_production() {
        let a = parse(&["--pr".into(), "x".into()]).unwrap();
        assert!(a.model.is_none() && a.thinking.is_none(), "routes by risk unless pinned");
        assert!(a.deep, "production runs deep by default");
        assert!(a.sha.is_none());
    }

    #[test]
    fn diff_only_and_sha_flags() {
        let a = parse(&["--pr".into(), "x".into(), "--diff-only".into(), "--sha".into(), "abc".into()])
            .unwrap();
        assert!(!a.deep);
        assert_eq!(a.sha.as_deref(), Some("abc"));
    }
}
