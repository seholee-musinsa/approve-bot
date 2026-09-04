use anyhow::{anyhow, Result};
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, USER_AGENT};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};

const API: &str = "https://api.github.com";
const UA: &str = "approve-bot/0.1.0";

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GhUser {
    pub login: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GhUserHint {
    pub login: String,
    pub avatar_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UserSearchResp {
    items: Vec<GhUserHint>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct PrHead {
    #[serde(default)]
    pub sha: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub draft: bool,
    pub html_url: String,
    pub user: GhUser,
    /// Current head commit — reviews are deduped against this so a new commit
    /// (or a dismissed prior review) is re-reviewed.
    #[serde(default)]
    pub head: PrHead,
    /// PR description body. Fed to the reviewer so it doesn't wrongly flag the PR
    /// as having no description. Null when the author left it empty.
    #[serde(default)]
    pub body: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Review {
    pub user: GhUser,
    pub state: String,
    /// Commit the review was left on (null for pending).
    #[serde(default)]
    pub commit_id: Option<String>,
    /// Review body — used to detect our own engine reviews (they carry a marker)
    /// vs a blind approve or another bot's review.
    #[serde(default)]
    pub body: String,
}

#[derive(Debug, Clone)]
pub struct RateLimit {
    pub remaining: Option<u64>,
    pub limit: Option<u64>,
}

/// An inline PR review comment on a specific line (RIGHT side of the diff).
#[derive(Debug, Clone)]
pub struct ReviewComment {
    pub path: String,
    pub line: u64,
    pub body: String,
}

pub struct GitHubClient {
    http: Client,
    token: String,
}

impl GitHubClient {
    pub fn new(token: String) -> Self {
        Self {
            http: Client::builder()
                .build()
                .expect("failed to build reqwest client"),
            token,
        }
    }

    /// The in-memory token (used by the deep-review clone to authenticate).
    pub fn token(&self) -> &str {
        &self.token
    }

    fn headers(&self) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(USER_AGENT, HeaderValue::from_static(UA));
        h.insert(
            ACCEPT,
            HeaderValue::from_static("application/vnd.github+json"),
        );
        h.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", self.token))
                .expect("token contains invalid header chars"),
        );
        h.insert(
            "X-GitHub-Api-Version",
            HeaderValue::from_static("2022-11-28"),
        );
        h
    }

    fn extract_rate(headers: &reqwest::header::HeaderMap) -> RateLimit {
        let parse =
            |name: &str| -> Option<u64> { headers.get(name)?.to_str().ok()?.parse().ok() };
        RateLimit {
            remaining: parse("x-ratelimit-remaining"),
            limit: parse("x-ratelimit-limit"),
        }
    }

    pub async fn get_user(&self) -> Result<(GhUser, RateLimit)> {
        let url = format!("{API}/user");
        let resp = self
            .http
            .get(&url)
            .headers(self.headers())
            .send()
            .await?;
        let rate = Self::extract_rate(resp.headers());
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("GET /user failed: {status} {body}"));
        }
        let user: GhUser = resp.json().await?;
        Ok((user, rate))
    }

    pub async fn list_open_pulls(
        &self,
        owner: &str,
        repo: &str,
    ) -> Result<(Vec<PullRequest>, RateLimit)> {
        let url = format!(
            "{API}/repos/{owner}/{repo}/pulls?state=open&per_page=100&sort=created&direction=desc"
        );
        let resp = self
            .http
            .get(&url)
            .headers(self.headers())
            .send()
            .await?;
        let rate = Self::extract_rate(resp.headers());
        let status = resp.status();
        if status == StatusCode::NOT_FOUND {
            return Err(anyhow!("repository {owner}/{repo} not found (404)"));
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("list pulls failed: {status} {body}"));
        }
        let pulls: Vec<PullRequest> = resp.json().await?;
        Ok((pulls, rate))
    }

    pub async fn list_reviews(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<Vec<Review>> {
        let url = format!("{API}/repos/{owner}/{repo}/pulls/{number}/reviews?per_page=100");
        let resp = self
            .http
            .get(&url)
            .headers(self.headers())
            .send()
            .await?;
        if !resp.status().is_success() {
            let s = resp.status();
            let b = resp.text().await.unwrap_or_default();
            return Err(anyhow!("list reviews failed: {s} {b}"));
        }
        Ok(resp.json().await?)
    }

    pub async fn search_users(&self, query: &str, limit: u8) -> Result<Vec<GhUserHint>> {
        let q = query.trim();
        if q.is_empty() {
            return Ok(vec![]);
        }
        // Match user logins that start with the prefix; falls back to fuzzy if no prefix hits.
        let q_param = format!("{q} in:login type:user");
        let url = format!("{API}/search/users");
        let resp = self
            .http
            .get(&url)
            .headers(self.headers())
            .query(&[
                ("q", q_param.as_str()),
                ("per_page", &limit.to_string()),
            ])
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("user search failed: {status} {body}"));
        }
        let parsed: UserSearchResp = resp.json().await?;
        Ok(parsed.items)
    }

    pub async fn approve_pull(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        body: Option<&str>,
    ) -> Result<()> {
        self.submit_review(owner, repo, number, "APPROVE", body, &[]).await
    }

    /// Approve with inline line comments attached (creates resolvable threads).
    pub async fn approve_pull_with_comments(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        body: Option<&str>,
        comments: &[ReviewComment],
    ) -> Result<()> {
        self.submit_review(owner, repo, number, "APPROVE", body, comments).await
    }

    /// Post a non-approving COMMENT review carrying the review body (+ optional
    /// inline line comments). Used when the score is below the approve threshold.
    pub async fn comment_pull(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        body: &str,
        comments: &[ReviewComment],
    ) -> Result<()> {
        self.submit_review(owner, repo, number, "COMMENT", Some(body), comments).await
    }

    /// Submit a review. Inline `comments` reference diff lines; if GitHub rejects
    /// them (422 — a line not in the diff), retry once WITHOUT comments so the
    /// review body still lands.
    async fn submit_review(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        event: &str,
        body: Option<&str>,
        comments: &[ReviewComment],
    ) -> Result<()> {
        let url = format!("{API}/repos/{owner}/{repo}/pulls/{number}/reviews");
        let build = |with_comments: bool| -> serde_json::Value {
            let mut payload = serde_json::Map::new();
            payload.insert("event".into(), serde_json::Value::String(event.into()));
            if let Some(b) = body.filter(|s| !s.trim().is_empty()) {
                payload.insert("body".into(), serde_json::Value::String(b.to_string()));
            }
            if with_comments && !comments.is_empty() {
                let arr = comments
                    .iter()
                    .map(|c| {
                        serde_json::json!({
                            "path": c.path,
                            "line": c.line,
                            "side": "RIGHT",
                            "body": c.body,
                        })
                    })
                    .collect::<Vec<_>>();
                payload.insert("comments".into(), serde_json::Value::Array(arr));
            }
            serde_json::Value::Object(payload)
        };

        let resp = self
            .http
            .post(&url)
            .headers(self.headers())
            .json(&build(true))
            .send()
            .await?;
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        // 422 usually means an inline comment pointed at a line not in the diff.
        // Retry without the inline comments so the review body still posts.
        if status == StatusCode::UNPROCESSABLE_ENTITY && !comments.is_empty() {
            let resp2 = self
                .http
                .post(&url)
                .headers(self.headers())
                .json(&build(false))
                .send()
                .await?;
            let s2 = resp2.status();
            if s2.is_success() {
                return Ok(());
            }
            let b = resp2.text().await.unwrap_or_default();
            return Err(anyhow!("{event} failed (no-comments retry): {s2} {b}"));
        }
        let b = resp.text().await.unwrap_or_default();
        Err(anyhow!("{event} failed: {status} {b}"))
    }

    /// Fetch the unified diff for a PR (Accept: `...v3.diff`). Bounded by the
    /// server; the caller truncates before handing it to the review engine.
    pub async fn get_pr_diff(&self, owner: &str, repo: &str, number: u64) -> Result<String> {
        let url = format!("{API}/repos/{owner}/{repo}/pulls/{number}");
        let mut headers = self.headers();
        headers.insert(ACCEPT, HeaderValue::from_static("application/vnd.github.v3.diff"));
        let resp = self.http.get(&url).headers(headers).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("get diff failed: {status} {body}"));
        }
        Ok(resp.text().await?)
    }
}

/// Parse "owner/repo" into (owner, repo). Trims whitespace and rejects malformed entries.
pub fn split_repo(full: &str) -> Result<(&str, &str)> {
    let trimmed = full.trim();
    let mut parts = trimmed.splitn(2, '/');
    let owner = parts
        .next()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("invalid repo `{full}`: expected owner/repo"))?;
    let repo = parts
        .next()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("invalid repo `{full}`: expected owner/repo"))?;
    if repo.contains('/') {
        return Err(anyhow!("invalid repo `{full}`: too many slashes"));
    }
    Ok((owner, repo))
}
