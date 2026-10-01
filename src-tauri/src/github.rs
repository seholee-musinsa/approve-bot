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
    /// Target branch. Only the name is used (eval diffs a pinned commit against it).
    #[serde(default)]
    pub base: PrBase,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct PrBase {
    #[serde(rename = "ref", default)]
    pub ref_name: String,
    /// Base commit GitHub recorded for the PR. Unlike the branch name it does
    /// not move, so `base.sha...pinned` stays the PR's diff after it merges.
    #[serde(default)]
    pub sha: String,
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
    /// ISO-8601 UTC; only used to cut threads off at a point in time (eval).
    #[serde(default)]
    pub submitted_at: Option<String>,
}

/// One inline review comment as GitHub returns it (only the fields we feed the reviewer).
#[derive(Debug, Clone, Deserialize)]
pub struct InlineThreadComment {
    /// Replies point at their thread root through `in_reply_to_id`.
    #[serde(default)]
    pub id: u64,
    pub user: GhUser,
    pub path: String,
    #[serde(default)]
    pub line: Option<u64>,
    #[serde(default)]
    pub original_line: Option<u64>,
    pub body: String,
    #[serde(default)]
    pub in_reply_to_id: Option<u64>,
    #[serde(default)]
    pub created_at: Option<String>,
    /// `file` for a whole-file comment. GitHub reports such a comment at line 1,
    /// which is not where the problem is.
    #[serde(default)]
    pub subject_type: Option<String>,
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

/// A review comment on a whole file (no line) — for a finding that could not be
/// pinned to a diff line.
#[derive(Debug, Clone)]
pub struct FileComment {
    pub path: String,
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

    /// Post a file-level comment. The reviews API has no such comment, so it goes
    /// on its own after the review is submitted; it still opens a resolvable thread.
    pub async fn post_file_comment(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        commit_id: &str,
        c: &FileComment,
    ) -> Result<()> {
        let url = format!("{API}/repos/{owner}/{repo}/pulls/{number}/comments");
        let payload = serde_json::json!({
            "body": c.body,
            "commit_id": commit_id,
            "path": c.path,
            "subject_type": "file",
        });
        let resp = self.http.post(&url).headers(self.headers()).json(&payload).send().await?;
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        let b = resp.text().await.unwrap_or_default();
        Err(anyhow!("file comment failed: {status} {b}"))
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
        // GitHub refuses the diff media type past 300 files (406). The files
        // API still serves per-file patches (up to 3000 files), so rebuild it.
        if status == StatusCode::NOT_ACCEPTABLE {
            return self.diff_from_files(owner, repo, number).await;
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("get diff failed: {status} {body}"));
        }
        Ok(resp.text().await?)
    }

    /// Fetch a single PR's metadata (title/body/author). Used by the headless
    /// `review-once` eval subcommand, which reviews closed/merged PRs that the
    /// open-PR poller never lists.
    /// Diff of a branch point `base...head` (three dots = since the merge base),
    /// the same shape GitHub shows for a PR. Eval uses it to rebuild the diff a
    /// reviewer saw at an earlier commit.
    pub async fn get_compare_diff(&self, owner: &str, repo: &str, base: &str, head: &str) -> Result<String> {
        let url = format!("{API}/repos/{owner}/{repo}/compare/{base}...{head}");
        let mut headers = self.headers();
        headers.insert(ACCEPT, HeaderValue::from_static("application/vnd.github.v3.diff"));
        let resp = self.http.get(&url).headers(headers).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("compare diff failed: {status} {body}"));
        }
        Ok(resp.text().await?)
    }

    /// Unified diff rebuilt from `GET /pulls/{n}/files` (paginated). Files whose
    /// patch GitHub leaves out (too large, binary) carry `PATCH_OMITTED_MARK`,
    /// which `diffprep` treats as not reviewed, so the gate will not auto-approve.
    pub async fn diff_from_files(&self, owner: &str, repo: &str, number: u64) -> Result<String> {
        #[derive(Deserialize)]
        struct F {
            filename: String,
            #[serde(default)]
            previous_filename: Option<String>,
            #[serde(default)]
            status: String,
            #[serde(default)]
            patch: Option<String>,
        }
        let mut out = String::new();
        for page in 1..=30 {
            let url = format!("{API}/repos/{owner}/{repo}/pulls/{number}/files?per_page=100&page={page}");
            let files: Vec<F> = self.get_json(&url).await?;
            let n = files.len();
            for f in files {
                let old = f.previous_filename.as_deref().unwrap_or(&f.filename);
                let (a, b) = match f.status.as_str() {
                    "added" => ("/dev/null".to_string(), format!("b/{}", f.filename)),
                    "removed" => (format!("a/{old}"), "/dev/null".to_string()),
                    _ => (format!("a/{old}"), format!("b/{}", f.filename)),
                };
                out.push_str(&format!("diff --git a/{old} b/{}\n--- {a}\n+++ {b}\n", f.filename));
                match f.patch {
                    Some(p) => {
                        out.push_str(&p);
                        out.push('\n');
                    }
                    None => {
                        out.push_str(crate::diffprep::PATCH_OMITTED_MARK);
                        out.push('\n');
                    }
                }
            }
            if n < 100 {
                break;
            }
        }
        Ok(out)
    }

    /// GET a JSON list, following nothing past the first page (`per_page=100`).
    async fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T> {
        let resp = self.http.get(url).headers(self.headers()).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("GET {url} failed: {status} {body}"));
        }
        Ok(resp.json().await?)
    }

    /// Committer date of a commit (ISO-8601 UTC).
    pub async fn get_commit_date(&self, owner: &str, repo: &str, sha: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct C {
            commit: Inner,
        }
        #[derive(Deserialize)]
        struct Inner {
            committer: Who,
        }
        #[derive(Deserialize)]
        struct Who {
            date: String,
        }
        let url = format!("{API}/repos/{owner}/{repo}/commits/{sha}");
        let c: C = self.get_json(&url).await?;
        Ok(c.commit.committer.date)
    }

    /// Commit headlines of the PR, oldest first.
    pub async fn list_commit_headlines(&self, owner: &str, repo: &str, number: u64) -> Result<Vec<String>> {
        #[derive(Deserialize)]
        struct C {
            commit: Inner,
        }
        #[derive(Deserialize)]
        struct Inner {
            message: String,
        }
        let url = format!("{API}/repos/{owner}/{repo}/pulls/{number}/commits?per_page=100");
        let cs: Vec<C> = self.get_json(&url).await?;
        Ok(cs
            .into_iter()
            .map(|c| c.commit.message.lines().next().unwrap_or("").to_string())
            .collect())
    }

    /// Inline review comments (people and bots), oldest first.
    pub async fn list_review_comments(&self, owner: &str, repo: &str, number: u64) -> Result<Vec<InlineThreadComment>> {
        let url = format!("{API}/repos/{owner}/{repo}/pulls/{number}/comments?per_page=100");
        self.get_json(&url).await
    }

    /// Most recently updated closed PRs (merged or not), newest first.
    pub async fn list_recent_closed_pulls(&self, owner: &str, repo: &str, limit: usize) -> Result<Vec<PullRequest>> {
        let per_page = limit.clamp(1, 100);
        let url = format!(
            "{API}/repos/{owner}/{repo}/pulls?state=closed&per_page={per_page}&sort=updated&direction=desc"
        );
        self.get_json(&url).await
    }

    /// File content at a git ref, or None when the file does not exist there.
    pub async fn get_file_at(&self, owner: &str, repo: &str, path: &str, git_ref: &str) -> Result<Option<String>> {
        let url = format!("{API}/repos/{owner}/{repo}/contents/{path}?ref={git_ref}");
        let mut headers = self.headers();
        headers.insert(ACCEPT, HeaderValue::from_static("application/vnd.github.raw"));
        let resp = self.http.get(&url).headers(headers).send().await?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("get file {path} failed: {status} {body}"));
        }
        Ok(Some(resp.text().await?))
    }

    /// File names in a directory at a git ref (empty when the dir is absent).
    pub async fn list_dir_at(&self, owner: &str, repo: &str, path: &str, git_ref: &str) -> Result<Vec<String>> {
        #[derive(Deserialize)]
        struct E {
            name: String,
            #[serde(rename = "type")]
            kind: String,
        }
        let url = format!("{API}/repos/{owner}/{repo}/contents/{path}?ref={git_ref}");
        let resp = self.http.get(&url).headers(self.headers()).send().await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(vec![]);
        }
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("list dir {path} failed: {status} {body}"));
        }
        let es: Vec<E> = resp.json().await?;
        Ok(es.into_iter().filter(|e| e.kind == "file").map(|e| e.name).collect())
    }

    pub async fn get_pull(&self, owner: &str, repo: &str, number: u64) -> Result<PullRequest> {
        let url = format!("{API}/repos/{owner}/{repo}/pulls/{number}");
        let resp = self.http.get(&url).headers(self.headers()).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("get pull failed: {status} {body}"));
        }
        Ok(resp.json().await?)
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
