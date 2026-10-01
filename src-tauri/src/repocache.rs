//! Cache clone of the repo the sweep reads.
//!
//! The sweep needs history (churn comes from `git log`) and file contents at
//! one fixed commit, without touching anyone's working folder. So the app keeps
//! its own clone under the config dir, refreshes it on every run and checks out
//! the newest commit of the default branch.
//!
//! A refresh that fails is not just logged: the cause is read from git's own
//! output, a matching fix is tried (re-read the token, wait out a rate limit,
//! remove a stale lock, re-clone a broken cache) and the refresh runs again.
//! Every attempt is recorded with the cause and the action taken. If nothing
//! works the caller gets a typed error and must NOT read the stale cache: the
//! sweep treats the checked-out commit as the newest one.
//!
//! git runs behind the `Git` trait so the recovery logic can be tested without
//! a network. Secrets never reach the attempt log: output is scrubbed first.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// Output of one git call.
#[derive(Debug, Clone, Default)]
pub struct GitOut {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

pub trait Git {
    fn run(&self, dir: Option<&Path>, args: &[&str]) -> GitOut;
    /// Swap the access token used by later calls (after a re-login).
    fn set_token(&self, token: Option<String>);
}

/// Real git. The token goes through the environment, never argv, so it cannot
/// show up in `ps`.
pub struct RealGit {
    token: RefCell<Option<String>>,
}

impl RealGit {
    pub fn new(token: Option<String>) -> Self {
        Self { token: RefCell::new(token) }
    }
}

impl Git for RealGit {
    fn run(&self, dir: Option<&Path>, args: &[&str]) -> GitOut {
        let mut c = Command::new("git");
        c.args(args).env("GIT_TERMINAL_PROMPT", "0");
        if let Some(t) = self.token.borrow().as_deref() {
            let basic = crate::review::base64_encode(format!("x-access-token:{t}").as_bytes());
            c.env("GIT_CONFIG_COUNT", "1")
                .env("GIT_CONFIG_KEY_0", "http.extraHeader")
                .env("GIT_CONFIG_VALUE_0", format!("Authorization: Basic {basic}"));
        }
        if let Some(d) = dir {
            c.current_dir(d);
        }
        match c.output() {
            Ok(o) => GitOut {
                ok: o.status.success(),
                stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
            },
            Err(e) => GitOut { ok: false, stdout: String::new(), stderr: format!("git spawn failed: {e}") },
        }
    }

    fn set_token(&self, token: Option<String>) {
        *self.token.borrow_mut() = token;
    }
}

/// Why a refresh step failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    Network,
    Auth,
    RateLimit,
    NotFound,
    Locked,
    Corrupt,
    DiskFull,
    Unknown,
}

impl Failure {
    pub fn label(self) -> &'static str {
        match self {
            Failure::Network => "네트워크",
            Failure::Auth => "인증",
            Failure::RateLimit => "속도 제한",
            Failure::NotFound => "repo 없음 또는 권한 없음",
            Failure::Locked => "잠금 파일",
            Failure::Corrupt => "캐시 손상",
            Failure::DiskFull => "디스크 부족",
            Failure::Unknown => "원인 미상",
        }
    }
}

/// Read the cause from git's error output. Order matters: a 403 can be either a
/// rate limit or a bad token, and "unable to access" prefixes most HTTP errors.
pub fn classify(stderr: &str) -> Failure {
    let t = stderr.to_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| t.contains(n));
    if has(&["no space left on device"]) {
        Failure::DiskFull
    } else if has(&["index.lock", "another git process", "shallow.lock", "head.lock"])
        || (t.contains("unable to create") && t.contains(".lock"))
    {
        Failure::Locked
    } else if has(&["rate limit", "too many requests", "returned error: 429"]) {
        Failure::RateLimit
    } else if has(&["repository not found", "does not appear to be a git repository", "returned error: 404"]) {
        Failure::NotFound
    } else if has(&[
        "authentication failed",
        "returned error: 401",
        "returned error: 403",
        "invalid username or password",
        "could not read username",
        "bad credentials",
    ]) {
        Failure::Auth
    } else if has(&["bad object", "corrupt", "loose object", "object file", "is empty", "packfile", "fsck", "bad tree"]) {
        Failure::Corrupt
    } else if has(&[
        "could not resolve host",
        "timed out",
        "failed to connect",
        "connection reset",
        "connection refused",
        "early eof",
        "rpc failed",
        "remote end hung up",
        "unable to access",
        "ssl",
        "tls",
        "network is unreachable",
    ]) {
        Failure::Network
    } else {
        Failure::Unknown
    }
}

/// True when git's error text says the cache itself is damaged. Used on errors
/// from reading the history after a clean sync (e.g. `git log` for churn).
pub fn is_corruption(stderr: &str) -> bool {
    classify(stderr) == Failure::Corrupt
}

/// Replace the token (and its base64 form) and any `user:pass@` in a URL.
pub fn scrub(text: &str, token: Option<&str>) -> String {
    let mut out = text.to_string();
    if let Some(t) = token.filter(|t| !t.is_empty()) {
        out = out.replace(t, "***");
        let b64 = crate::review::base64_encode(format!("x-access-token:{t}").as_bytes());
        out = out.replace(&b64, "***");
    }
    // https://user:secret@host -> https://***@host
    let mut res = String::with_capacity(out.len());
    let mut rest = out.as_str();
    while let Some(i) = rest.find("://") {
        let (head, tail) = rest.split_at(i + 3);
        res.push_str(head);
        let end = tail.find(|c: char| c == '/' || c.is_whitespace()).unwrap_or(tail.len());
        match tail[..end].rfind('@') {
            Some(at) => {
                res.push_str("***");
                rest = &tail[at..];
            }
            None => rest = tail,
        }
    }
    res.push_str(rest);
    res
}

fn short(text: &str) -> String {
    let t = text.trim();
    let one: String = t.chars().take(300).collect();
    one.replace('\n', " | ")
}

/// One refresh pass that went wrong, and what was done about it.
#[derive(Debug, Clone)]
pub struct Attempt {
    pub n: usize,
    pub step: &'static str,
    pub failure: Failure,
    pub action: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncOk {
    /// Newest commit of the default branch, checked out.
    pub commit: String,
    pub branch: String,
    pub cloned: bool,
    /// A shallow clone has no history, so churn would read as zero everywhere.
    pub shallow: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncError {
    /// The repo name or the access is wrong. Retrying cannot help.
    NotFound,
    /// The token was refused even after it was re-read. Needs a new login.
    AuthRequired,
    DiskFull,
    /// Every automatic fix was tried.
    Failed { last: Failure, detail: String },
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyncError::NotFound => write!(f, "repo를 찾지 못했거나 권한이 없음: 설정의 repo 이름과 접근 권한을 확인해 주세요"),
            SyncError::AuthRequired => write!(f, "GitHub 인증이 거부됨: 다시 로그인이 필요합니다"),
            SyncError::DiskFull => write!(f, "디스크 공간이 부족함: 정리 후 다시 시도해 주세요"),
            SyncError::Failed { last, detail } => write!(f, "갱신하지 못함(원인: {}): {detail}", last.label()),
        }
    }
}

impl std::error::Error for SyncError {}

pub struct Policy {
    /// Waits between retries of a network failure; its length is the retry count.
    pub backoff: Vec<Duration>,
    pub rate_limit_wait: Duration,
    /// Hard cap on refresh passes, whatever the causes were.
    pub max_passes: usize,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            backoff: vec![Duration::from_secs(5), Duration::from_secs(30), Duration::from_secs(120)],
            rate_limit_wait: Duration::from_secs(60),
            max_passes: 6,
        }
    }
}

impl Policy {
    /// No waiting, for tests.
    pub fn instant() -> Self {
        Self { backoff: vec![Duration::ZERO; 3], rate_limit_wait: Duration::ZERO, max_passes: 6 }
    }
}

fn marker_path(dir: &Path) -> PathBuf {
    let mut name = dir.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".incomplete");
    dir.with_file_name(name)
}

fn is_git_repo(dir: &Path) -> bool {
    dir.join(".git").exists()
}

/// Lock files git leaves behind when it was killed.
fn find_locks(dir: &Path) -> Vec<PathBuf> {
    fn walk(p: &Path, out: &mut Vec<PathBuf>, depth: usize) {
        let Ok(rd) = std::fs::read_dir(p) else { return };
        for e in rd.flatten() {
            let path = e.path();
            if path.is_dir() {
                if depth < 6 {
                    walk(&path, out, depth + 1);
                }
            } else if path.extension().is_some_and(|x| x == "lock") {
                out.push(path);
            }
        }
    }
    let git = dir.join(".git");
    let mut out = Vec::new();
    // Top level and refs only: object packs can hold many unrelated files.
    if let Ok(rd) = std::fs::read_dir(&git) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_file() && p.extension().is_some_and(|x| x == "lock") {
                out.push(p);
            }
        }
    }
    walk(&git.join("refs"), &mut out, 0);
    out
}

/// True when some git process mentions this directory. When the process list
/// cannot be read the answer is "yes": a lock is never removed on a guess.
fn git_running_in(dir: &Path) -> bool {
    let Ok(out) = Command::new("ps").args(["-axo", "command="]).output() else { return true };
    let needle = dir.to_string_lossy().into_owned();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .any(|l| l.contains(&needle) && l.split_whitespace().next().is_some_and(|c| c.ends_with("git")))
}

/// Delete other caches next to this one (a different repo, an old name).
fn clean_siblings(dir: &Path) -> usize {
    let Some(parent) = dir.parent() else { return 0 };
    let mut n = 0;
    if let Ok(rd) = std::fs::read_dir(parent) {
        for e in rd.flatten() {
            let p = e.path();
            if p != dir && p.is_dir() && std::fs::remove_dir_all(&p).is_ok() {
                n += 1;
            }
        }
    }
    n
}

pub fn clear(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
    let _ = std::fs::remove_file(marker_path(dir));
}

/// Total size of a directory tree in bytes.
pub fn dir_size(dir: &Path) -> u64 {
    fn walk(p: &Path) -> u64 {
        let Ok(rd) = std::fs::read_dir(p) else { return 0 };
        rd.flatten()
            .map(|e| match e.file_type() {
                Ok(t) if t.is_symlink() => 0,
                Ok(t) if t.is_dir() => walk(&e.path()),
                Ok(_) => e.metadata().map(|m| m.len()).unwrap_or(0),
                Err(_) => 0,
            })
            .sum()
    }
    walk(dir)
}

struct Step {
    name: &'static str,
    out: GitOut,
}

/// One refresh pass: clone if needed, resolve the default branch, fetch it,
/// check it out. `Err` carries the step that failed.
fn pass(git: &dyn Git, dir: &Path, url: &str) -> Result<SyncOk, Step> {
    let fail = |name: &'static str, out: GitOut| Err(Step { name, out });
    let mut cloned = false;

    let marker = marker_path(dir);
    if marker.exists() || !is_git_repo(dir) {
        // A half-finished clone cannot be trusted, so start over.
        clear(dir);
        if let Some(parent) = dir.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&marker, b"clone in progress");
        let out = git.run(None, &["clone", "--quiet", "--filter=blob:none", "--no-checkout", url, &dir.to_string_lossy()]);
        if !out.ok {
            return fail("clone", out);
        }
        let _ = std::fs::remove_file(&marker);
        cloned = true;
    }

    let out = git.run(Some(dir), &["ls-remote", "--symref", url, "HEAD"]);
    if !out.ok {
        return fail("ls-remote", out);
    }
    let Some(branch) = out
        .stdout
        .lines()
        .find_map(|l| l.strip_prefix("ref: refs/heads/").and_then(|r| r.split('\t').next()))
        .map(str::to_string)
    else {
        return fail("ls-remote", GitOut { ok: false, stdout: out.stdout, stderr: "default branch not reported by the remote".into() });
    };

    // `+` forces the update, so a rewritten history is not an error.
    let spec = format!("+refs/heads/{branch}:refs/remotes/origin/{branch}");
    let out = git.run(Some(dir), &["fetch", "--quiet", "--force", "--prune", "origin", &spec]);
    if !out.ok {
        return fail("fetch", out);
    }
    let tip = format!("refs/remotes/origin/{branch}");
    let out = git.run(Some(dir), &["checkout", "--quiet", "--force", "--detach", &tip]);
    if !out.ok {
        return fail("checkout", out);
    }
    let out = git.run(Some(dir), &["clean", "--quiet", "-fdx"]);
    if !out.ok {
        return fail("clean", out);
    }
    let out = git.run(Some(dir), &["rev-parse", "HEAD"]);
    if !out.ok {
        return fail("rev-parse", out);
    }
    let commit = out.stdout.trim().to_string();
    let shallow = git
        .run(Some(dir), &["rev-parse", "--is-shallow-repository"])
        .stdout
        .trim()
        == "true";
    Ok(SyncOk { commit, branch, cloned, shallow })
}

/// Bring the cache up to date. Failures are diagnosed and fixed as far as they
/// can be; `log` gets one entry per failed pass.
pub fn sync(
    git: &dyn Git,
    dir: &Path,
    url: &str,
    policy: &Policy,
    refresh_token: &dyn Fn() -> Option<String>,
    token_for_scrub: Option<&str>,
    log: &mut Vec<Attempt>,
) -> Result<SyncOk, SyncError> {
    let mut network_retries = 0;
    let mut token_reloaded = false;
    let mut rate_waited = false;
    let mut recloned = false;
    let mut disk_cleaned = false;
    let mut unknown_retried = false;

    for n in 1..=policy.max_passes {
        let step = match pass(git, dir, url) {
            Ok(ok) => return Ok(ok),
            Err(s) => s,
        };
        let mut failure = classify(&step.out.stderr);
        // An error git does not explain: check whether the cache itself is broken.
        if failure == Failure::Unknown && is_git_repo(dir) {
            let fsck = git.run(Some(dir), &["fsck", "--connectivity-only", "--no-dangling"]);
            if !fsck.ok {
                failure = Failure::Corrupt;
            }
        }
        let detail = scrub(&short(&step.out.stderr), token_for_scrub);
        let mut record = |action: String| {
            log.push(Attempt { n, step: step.name, failure, action, detail: detail.clone() });
        };

        match failure {
            Failure::NotFound => {
                record("재시도하지 않음".into());
                return Err(SyncError::NotFound);
            }
            Failure::Network => {
                let Some(wait) = policy.backoff.get(network_retries).copied() else {
                    record("재시도 횟수를 모두 씀".into());
                    return Err(SyncError::Failed { last: failure, detail });
                };
                network_retries += 1;
                record(format!("{}초 기다린 뒤 재시도({network_retries}/{})", wait.as_secs(), policy.backoff.len()));
                std::thread::sleep(wait);
            }
            Failure::Auth => {
                if token_reloaded {
                    record("토큰을 다시 읽었으나 거부됨".into());
                    return Err(SyncError::AuthRequired);
                }
                token_reloaded = true;
                git.set_token(refresh_token());
                record("토큰을 다시 읽고 재시도".into());
            }
            Failure::RateLimit => {
                if rate_waited {
                    record("기다렸으나 계속 제한됨".into());
                    return Err(SyncError::Failed { last: failure, detail });
                }
                rate_waited = true;
                record(format!("{}초 기다린 뒤 재시도", policy.rate_limit_wait.as_secs()));
                std::thread::sleep(policy.rate_limit_wait);
            }
            Failure::Locked => {
                if git_running_in(dir) {
                    // Someone is still using the cache; give it time, within the retry budget.
                    let Some(wait) = policy.backoff.get(network_retries).copied() else {
                        record("다른 git 작업이 끝나지 않음".into());
                        return Err(SyncError::Failed { last: failure, detail });
                    };
                    network_retries += 1;
                    record(format!("다른 git 작업이 실행 중, {}초 기다림", wait.as_secs()));
                    std::thread::sleep(wait);
                } else {
                    let locks = find_locks(dir);
                    let removed = locks.iter().filter(|p| std::fs::remove_file(p).is_ok()).count();
                    record(format!("실행 중인 git 이 없어 잠금 파일 {removed}개를 지우고 재시도"));
                }
            }
            Failure::Corrupt => {
                if recloned {
                    record("다시 클론했으나 같은 오류".into());
                    return Err(SyncError::Failed { last: failure, detail });
                }
                recloned = true;
                clear(dir);
                record("캐시를 지우고 처음부터 다시 클론".into());
            }
            Failure::DiskFull => {
                if disk_cleaned {
                    record("정리 후에도 공간이 부족함".into());
                    return Err(SyncError::DiskFull);
                }
                disk_cleaned = true;
                let n = clean_siblings(dir);
                record(format!("다른 캐시 {n}개를 정리하고 재시도"));
            }
            Failure::Unknown => {
                if unknown_retried {
                    record("한 번 더 재시도했으나 같은 오류".into());
                    return Err(SyncError::Failed { last: failure, detail });
                }
                unknown_retried = true;
                record("원인을 분류하지 못해 1회 재시도".into());
            }
        }
    }
    Err(SyncError::Failed {
        last: log.last().map(|a| a.failure).unwrap_or(Failure::Unknown),
        detail: "재시도 횟수 상한에 도달".into(),
    })
}

/// Cache folder for one repo under `root`.
pub fn cache_dir(root: &Path, owner: &str, repo: &str) -> PathBuf {
    root.join(format!("{owner}__{repo}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    // ---- pure pieces -----------------------------------------------------

    #[test]
    fn classifies_each_cause_from_gits_own_words() {
        let cases = [
            ("fatal: unable to access 'https://x/': Could not resolve host: github.com", Failure::Network),
            ("error: RPC failed; curl 56 OpenSSL SSL_read: connection was reset", Failure::Network),
            ("fatal: the remote end hung up unexpectedly", Failure::Network),
            ("remote: Invalid username or password.\nfatal: Authentication failed for 'https://x/'", Failure::Auth),
            ("fatal: unable to access 'https://x/': The requested URL returned error: 403", Failure::Auth),
            ("fatal: unable to access 'https://x/': The requested URL returned error: 401", Failure::Auth),
            ("remote: API rate limit exceeded\nfatal: unable to access: The requested URL returned error: 403", Failure::RateLimit),
            ("fatal: unable to access: The requested URL returned error: 429", Failure::RateLimit),
            ("remote: Repository not found.\nfatal: repository 'https://x/' not found", Failure::NotFound),
            ("fatal: '/nope' does not appear to be a git repository", Failure::NotFound),
            ("fatal: Unable to create '/c/.git/index.lock': File exists.", Failure::Locked),
            ("fatal: Another git process seems to be running in this repository", Failure::Locked),
            ("error: object file .git/objects/ab/cd is empty\nfatal: loose object is corrupt", Failure::Corrupt),
            ("fatal: bad object HEAD", Failure::Corrupt),
            ("error: unable to write file: No space left on device", Failure::DiskFull),
            ("something nobody has seen before", Failure::Unknown),
            ("", Failure::Unknown),
        ];
        for (text, want) in cases {
            assert_eq!(classify(text), want, "{text}");
        }
    }

    #[test]
    fn scrub_removes_the_token_its_header_form_and_url_credentials() {
        let token = "ghp_secretTOKEN123";
        let b64 = crate::review::base64_encode(format!("x-access-token:{token}").as_bytes());
        let text = format!("fatal: https://user:{token}@github.com/o/r failed; header {b64}; raw {token}");
        let out = scrub(&text, Some(token));
        assert!(!out.contains(token) && !out.contains(&b64), "{out}");
        assert!(out.contains("https://***@github.com/o/r"), "{out}");
        assert_eq!(scrub("plain text", Some("")), "plain text");
        assert_eq!(scrub("see https://github.com/o/r now", None), "see https://github.com/o/r now");
    }

    #[test]
    fn marker_sits_next_to_the_cache_not_inside_it() {
        assert_eq!(marker_path(Path::new("/c/sweep/o__r")), PathBuf::from("/c/sweep/o__r.incomplete"));
        assert_eq!(cache_dir(Path::new("/c/sweep"), "o", "r"), PathBuf::from("/c/sweep/o__r"));
    }

    // ---- recovery logic against a scripted git ---------------------------

    struct Scripted {
        /// (substring of the command, queued outputs). A command with an empty
        /// queue succeeds with a sensible default.
        rules: RefCell<Vec<(String, VecDeque<GitOut>)>>,
        calls: RefCell<Vec<String>>,
        tokens: RefCell<Vec<Option<String>>>,
    }

    fn bad(stderr: &str) -> GitOut {
        GitOut { ok: false, stdout: String::new(), stderr: stderr.into() }
    }

    impl Scripted {
        fn new(rules: Vec<(&str, Vec<GitOut>)>) -> Self {
            Self {
                rules: RefCell::new(rules.into_iter().map(|(k, v)| (k.to_string(), v.into())).collect()),
                calls: RefCell::new(vec![]),
                tokens: RefCell::new(vec![]),
            }
        }
        fn count(&self, needle: &str) -> usize {
            self.calls.borrow().iter().filter(|c| c.contains(needle)).count()
        }
    }

    impl Git for Scripted {
        fn run(&self, _dir: Option<&Path>, args: &[&str]) -> GitOut {
            let cmd = args.join(" ");
            self.calls.borrow_mut().push(cmd.clone());
            for (needle, q) in self.rules.borrow_mut().iter_mut() {
                if cmd.contains(needle.as_str()) {
                    if let Some(o) = q.pop_front() {
                        return o;
                    }
                }
            }
            let stdout = if cmd.starts_with("ls-remote") {
                "ref: refs/heads/main\tHEAD\nabc123\tHEAD\n"
            } else if cmd == "rev-parse HEAD" {
                "abc123\n"
            } else if cmd.contains("--is-shallow-repository") {
                "false\n"
            } else {
                ""
            };
            GitOut { ok: true, stdout: stdout.into(), stderr: String::new() }
        }
        fn set_token(&self, token: Option<String>) {
            self.tokens.borrow_mut().push(token);
        }
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("repocache-{name}-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A directory that looks like an existing clone, so the pass skips cloning.
    fn existing_clone(root: &Path) -> PathBuf {
        let dir = root.join("o__r");
        std::fs::create_dir_all(dir.join(".git/refs/remotes")).unwrap();
        dir
    }

    fn run_sync(git: &Scripted, dir: &Path, refresh: &dyn Fn() -> Option<String>, log: &mut Vec<Attempt>) -> Result<SyncOk, SyncError> {
        sync(git, dir, "https://example/o/r.git", &Policy::instant(), refresh, None, log)
    }

    #[test]
    fn a_network_blip_is_retried_and_logged_with_its_cause() {
        let root = tmp("net");
        let dir = existing_clone(&root);
        let git = Scripted::new(vec![("fetch", vec![bad("fatal: unable to access: Could not resolve host: github.com"), bad("fatal: the remote end hung up unexpectedly")])]);
        let mut log = vec![];
        let ok = run_sync(&git, &dir, &|| None, &mut log).expect("third try works");
        assert_eq!(ok.commit, "abc123");
        assert_eq!(ok.branch, "main");
        assert_eq!(git.count("fetch"), 3);
        assert_eq!(log.len(), 2);
        assert!(log.iter().all(|a| a.failure == Failure::Network && a.step == "fetch"), "{log:?}");
        assert!(log[0].action.contains("재시도(1/3)"), "{:?}", log[0]);
    }

    #[test]
    fn a_network_that_never_recovers_stops_after_the_retry_budget() {
        let root = tmp("netdown");
        let dir = existing_clone(&root);
        let down = bad("fatal: unable to access: Failed to connect to github.com port 443");
        let git = Scripted::new(vec![("fetch", vec![down.clone(), down.clone(), down.clone(), down.clone(), down])]);
        let mut log = vec![];
        let err = run_sync(&git, &dir, &|| None, &mut log).unwrap_err();
        assert!(matches!(err, SyncError::Failed { last: Failure::Network, .. }), "{err:?}");
        assert_eq!(git.count("fetch"), 4, "one try plus three retries");
    }

    #[test]
    fn a_refused_token_is_re_read_once_then_the_user_is_asked_to_log_in() {
        let root = tmp("auth");
        let dir = existing_clone(&root);
        let git = Scripted::new(vec![("fetch", vec![bad("remote: Invalid username or password.\nfatal: Authentication failed"), GitOut { ok: true, ..Default::default() }])]);
        let mut log = vec![];
        run_sync(&git, &dir, &|| Some("fresh".into()), &mut log).expect("works with the fresh token");
        assert_eq!(git.tokens.borrow().as_slice(), &[Some("fresh".to_string())]);
        assert_eq!(log[0].failure, Failure::Auth);

        let still_bad = bad("fatal: Authentication failed");
        let git = Scripted::new(vec![("fetch", vec![still_bad.clone(), still_bad.clone(), still_bad])]);
        let mut log = vec![];
        let err = run_sync(&git, &dir, &|| Some("fresh".into()), &mut log).unwrap_err();
        assert_eq!(err, SyncError::AuthRequired);
        assert_eq!(git.tokens.borrow().len(), 1, "the token is re-read once, not in a loop");
    }

    #[test]
    fn a_missing_repo_is_not_retried() {
        let root = tmp("nf");
        let dir = existing_clone(&root);
        let git = Scripted::new(vec![("ls-remote", vec![bad("remote: Repository not found.\nfatal: repository 'https://x/' not found")])]);
        let mut log = vec![];
        assert_eq!(run_sync(&git, &dir, &|| None, &mut log).unwrap_err(), SyncError::NotFound);
        assert_eq!(git.count("ls-remote"), 1);
        assert_eq!(log[0].action, "재시도하지 않음");
    }

    #[test]
    fn a_rate_limit_is_waited_out_once() {
        let root = tmp("rl");
        let dir = existing_clone(&root);
        let git = Scripted::new(vec![("fetch", vec![bad("remote: API rate limit exceeded\nreturned error: 403")])]);
        let mut log = vec![];
        run_sync(&git, &dir, &|| None, &mut log).expect("works after the wait");
        assert_eq!(log[0].failure, Failure::RateLimit);
        assert_eq!(git.count("fetch"), 2);
    }

    #[test]
    fn a_stale_lock_is_removed_when_no_git_is_running_there() {
        let root = tmp("lock");
        let dir = existing_clone(&root);
        let lock = dir.join(".git/index.lock");
        std::fs::write(&lock, b"").unwrap();
        let git = Scripted::new(vec![("checkout", vec![bad(&format!("fatal: Unable to create '{}': File exists.", lock.display()))])]);
        let mut log = vec![];
        run_sync(&git, &dir, &|| None, &mut log).expect("works once the lock is gone");
        assert!(!lock.exists(), "the stale lock is deleted");
        assert_eq!(log[0].failure, Failure::Locked);
        assert!(log[0].action.contains("잠금 파일 1개"), "{:?}", log[0]);
    }

    #[test]
    fn an_error_git_does_not_explain_triggers_an_integrity_check_and_a_fresh_clone() {
        let root = tmp("unk");
        let dir = existing_clone(&root);
        let git = Scripted::new(vec![("fetch", vec![bad("error: something odd happened")]), ("fsck", vec![bad("error: bad tree")])]);
        let mut log = vec![];
        run_sync(&git, &dir, &|| None, &mut log).expect("works after a re-clone");
        assert_eq!(log[0].failure, Failure::Corrupt, "fsck failed, so the cache itself is broken");
        assert_eq!(git.count("clone "), 1);

        // The check passes: the oddity is retried once as is.
        let root = tmp("unk2");
        let dir = existing_clone(&root);
        let git = Scripted::new(vec![("fetch", vec![bad("error: something odd happened"), bad("error: something odd happened")])]);
        let mut log = vec![];
        let err = run_sync(&git, &dir, &|| None, &mut log).unwrap_err();
        assert!(matches!(err, SyncError::Failed { last: Failure::Unknown, .. }), "{err:?}");
        assert_eq!(git.count("clone "), 0, "a healthy cache is not thrown away");
    }

    #[test]
    fn a_full_disk_frees_other_caches_once() {
        let root = tmp("disk");
        let dir = existing_clone(&root);
        let other = root.join("someone__else");
        std::fs::create_dir_all(&other).unwrap();
        let git = Scripted::new(vec![("fetch", vec![bad("error: unable to write file: No space left on device")])]);
        let mut log = vec![];
        run_sync(&git, &dir, &|| None, &mut log).expect("works after freeing space");
        assert!(!other.exists() && dir.exists());
        assert!(log[0].action.contains("1개를 정리"), "{:?}", log[0]);
    }

    #[test]
    fn the_attempt_log_never_holds_the_token() {
        let root = tmp("scrub");
        let dir = existing_clone(&root);
        let token = "ghp_VerySecret999";
        let git = Scripted::new(vec![("fetch", vec![bad(&format!("fatal: unable to access 'https://x:{token}@github.com/o/r': Could not resolve host"))])]);
        let mut log = vec![];
        sync(&git, &dir, "https://example/o/r.git", &Policy::instant(), &|| None, Some(token), &mut log).unwrap();
        let text = format!("{log:?}");
        assert!(!text.contains(token), "{text}");
    }

    // ---- real git against a local remote ---------------------------------

    fn git_ok(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A bare remote with one commit on `main`, plus the work tree that pushes to it.
    fn remote(root: &Path) -> (String, PathBuf) {
        let bare = root.join("remote.git");
        let work = root.join("work");
        std::fs::create_dir_all(&bare).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        git_ok(&bare, &["init", "--bare", "--quiet", "-b", "main"]);
        git_ok(&bare, &["config", "uploadpack.allowFilter", "true"]);
        git_ok(&work, &["init", "--quiet", "-b", "main"]);
        git_ok(&work, &["remote", "add", "origin", &bare.to_string_lossy()]);
        std::fs::write(work.join("a.ts"), "export const a = 1;\n").unwrap();
        git_ok(&work, &["add", "."]);
        git_ok(&work, &["commit", "--quiet", "-m", "first"]);
        git_ok(&work, &["push", "--quiet", "origin", "main"]);
        (format!("file://{}", bare.display()), work)
    }

    fn real_sync(dir: &Path, url: &str) -> (Result<SyncOk, SyncError>, Vec<Attempt>) {
        let git = RealGit::new(None);
        let mut log = vec![];
        let r = sync(&git, dir, url, &Policy::instant(), &|| None, None, &mut log);
        (r, log)
    }

    #[test]
    fn clones_first_then_fetches_and_follows_a_force_push() {
        let root = tmp("real");
        let (url, work) = remote(&root);
        let dir = root.join("cache/o__r");

        let (r, log) = real_sync(&dir, &url);
        let first = r.expect("first sync clones");
        assert!(first.cloned && !first.shallow && first.branch == "main", "{first:?}");
        assert!(log.is_empty());
        assert_eq!(first.commit, git_ok(&work, &["rev-parse", "HEAD"]));
        assert!(dir.join("a.ts").exists(), "the working tree is checked out at that commit");

        std::fs::write(work.join("b.ts"), "export const b = 2;\n").unwrap();
        git_ok(&work, &["add", "."]);
        git_ok(&work, &["commit", "--quiet", "-m", "second"]);
        git_ok(&work, &["push", "--quiet", "origin", "main"]);
        let (r, _) = real_sync(&dir, &url);
        let second = r.expect("second sync fetches");
        assert!(!second.cloned);
        assert_eq!(second.commit, git_ok(&work, &["rev-parse", "HEAD"]));
        assert!(dir.join("b.ts").exists());

        // Rewrite history: drop the second commit and add a different one.
        git_ok(&work, &["reset", "--quiet", "--hard", "HEAD~1"]);
        std::fs::write(work.join("c.ts"), "export const c = 3;\n").unwrap();
        git_ok(&work, &["add", "."]);
        git_ok(&work, &["commit", "--quiet", "-m", "rewritten"]);
        git_ok(&work, &["push", "--quiet", "--force", "origin", "main"]);
        let (r, log) = real_sync(&dir, &url);
        let third = r.expect("a forced update is followed, not an error");
        assert_eq!(third.commit, git_ok(&work, &["rev-parse", "HEAD"]));
        assert!(dir.join("c.ts").exists() && !dir.join("b.ts").exists());
        assert!(log.is_empty(), "{log:?}");
    }

    #[test]
    fn a_half_finished_clone_is_discarded_and_redone() {
        let root = tmp("half");
        let (url, _work) = remote(&root);
        let dir = root.join("cache/o__r");
        std::fs::create_dir_all(dir.join("junk")).unwrap();
        std::fs::write(marker_path(&dir), b"clone in progress").unwrap();
        let (r, _) = real_sync(&dir, &url);
        let ok = r.expect("re-cloned");
        assert!(ok.cloned);
        assert!(!dir.join("junk").exists() && !marker_path(&dir).exists());
    }

    #[test]
    fn a_missing_remote_is_reported_without_retries() {
        let root = tmp("gone");
        let dir = root.join("cache/o__r");
        let (r, log) = real_sync(&dir, &format!("file://{}/does-not-exist.git", root.display()));
        assert_eq!(r.unwrap_err(), SyncError::NotFound);
        assert_eq!(log.len(), 1, "{log:?}");
    }

    #[test]
    fn a_stale_lock_left_in_a_real_cache_is_cleared() {
        let root = tmp("reallock");
        let (url, _work) = remote(&root);
        let dir = root.join("cache/o__r");
        real_sync(&dir, &url).0.expect("first sync");
        std::fs::write(dir.join(".git/index.lock"), b"").unwrap();
        let (r, log) = real_sync(&dir, &url);
        r.expect("recovers");
        assert!(!dir.join(".git/index.lock").exists());
        assert!(log.iter().any(|a| a.failure == Failure::Locked), "{log:?}");
    }

    #[test]
    fn a_damaged_pack_is_caught_when_git_cannot_read_it_and_fixed_by_a_fresh_clone() {
        let root = tmp("damaged");
        let (url, work) = remote(&root);
        let dir = root.join("cache/o__r");
        real_sync(&dir, &url).0.expect("first sync");
        // Truncate every pack in the cache. With nothing new on the remote the
        // fetch has to read the cache's own objects, so git notices.
        let packs = dir.join(".git/objects/pack");
        for e in std::fs::read_dir(&packs).unwrap().flatten() {
            if e.path().extension().is_some_and(|x| x == "pack") {
                // Git writes packs read-only, so replace the file instead of editing it.
                std::fs::remove_file(e.path()).unwrap();
                std::fs::write(e.path(), b"PACK").unwrap();
            }
        }
        let (r, log) = real_sync(&dir, &url);
        let ok = r.expect("recovered by re-cloning");
        assert_eq!(ok.commit, git_ok(&work, &["rev-parse", "HEAD"]));
        assert!(log.iter().any(|a| a.failure == Failure::Corrupt && a.action.contains("클론")), "{log:?}");
    }

    #[test]
    fn corruption_is_recognised_from_the_error_text_of_a_later_read() {
        // `git log` over a damaged history fails after a clean sync; the caller
        // uses this to decide that the cache must be rebuilt.
        assert!(is_corruption("error: file .git/objects/pack/p.pack is far too short to be a packfile\nfatal: bad object abc"));
        assert!(is_corruption("fatal: bad tree object 1234"));
        assert!(!is_corruption("fatal: not a git repository"));
        assert!(!is_corruption(""));
    }

    #[test]
    fn dir_size_counts_files_and_clear_removes_the_marker_too() {
        let root = tmp("size");
        let dir = root.join("o__r");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a"), vec![0u8; 100]).unwrap();
        std::fs::write(dir.join("sub/b"), vec![0u8; 50]).unwrap();
        assert_eq!(dir_size(&dir), 150);
        std::fs::write(marker_path(&dir), b"x").unwrap();
        clear(&dir);
        assert!(!dir.exists() && !marker_path(&dir).exists());
    }
}
