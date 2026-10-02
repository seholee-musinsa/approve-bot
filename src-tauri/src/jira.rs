//! Jira 연동(코드 정기 점검 4단계). 읽기(검색·열린 티켓 수·결과 수집)와 쓰기(생성·배정)를 나눈다.
//!
//! 자격은 `~/.config/jira.env` 를 호출 시점에 읽고 로그·오류에 남기지 않는다.
//! 사이트 고유 값(cloud id, 프로젝트, 상위 에픽 등)은 설정 폴더의 `sweep-jira.json` 에 둔다(공개 repo 에 넣지 않는다).
//! 쓰기는 설정의 `allow_create` 가 true 일 때만 호출부가 부를 수 있다.

use crate::sweep_state::{KeyOutcome, RejectReason};
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// 지적 한 건의 안정 키를 티켓에 남기는 라벨 접두(3.3 b). 라벨은 공백 없는 짧은 문자열이어야 한다.
pub const KEY_LABEL_PREFIX: &str = "sweep-";
/// 분류 라벨 접두. `sweep-` 로 시작하지 않아 키 라벨과 섞이지 않는다.
pub const CATEGORY_LABEL_PREFIX: &str = "sweepcat-";

/// 티켓의 분류(없으면 None: 분류 라벨이 생기기 전에 만든 티켓).
pub fn category_of(labels: &[String]) -> Option<&str> {
    labels.iter().find_map(|l| l.strip_prefix(CATEGORY_LABEL_PREFIX))
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JiraConfig {
    #[serde(default)]
    pub cloud_id: String,
    #[serde(default = "d_project")]
    pub project: String,
    #[serde(default = "d_issue_type")]
    pub issue_type: String,
    /// 작업 종류 라벨(기존 KTLO 티켓 관행).
    #[serde(default = "d_work_label")]
    pub work_label: String,
    /// 봇이 만든 티켓을 일괄 조회·정리하는 라벨(4.12).
    #[serde(default = "d_bot_label")]
    pub bot_label: String,
    #[serde(default = "d_estimate_field")]
    pub estimate_field: String,
    /// 상위 에픽(Jira 의 부모). 비우면 상위 에픽 없이 만든다(나중에 일괄 지정).
    #[serde(default)]
    pub parent_key: Option<String>,
    /// 담당자 accountId 목록(카나리: 본인 한 명).
    #[serde(default)]
    pub assignees: Vec<String>,
    /// accountId → 표시 이름(화면용).
    #[serde(default)]
    pub assignee_names: std::collections::BTreeMap<String, String>,
    /// Jira 사이트 주소(티켓 링크용). 비어 있으면 처음 필요할 때 API 로 알아내 저장한다.
    #[serde(default)]
    pub site_url: String,
    /// 한 사람의 열린 자동 생성 티켓 상한(5.5).
    #[serde(default = "d_open_cap")]
    pub open_cap: usize,
    /// 쓰기 허용. 기본 false.
    #[serde(default)]
    pub allow_create: bool,
}

fn d_open_cap() -> usize {
    10
}
fn d_project() -> String {
    "SID".into()
}
fn d_issue_type() -> String {
    "Dev".into()
}
fn d_work_label() -> String {
    "KTLO".into()
}
fn d_bot_label() -> String {
    "bot-created".into()
}
fn d_estimate_field() -> String {
    "customfield_12766".into()
}

pub fn config_path(dir: &Path) -> PathBuf {
    dir.join("sweep-jira.json")
}

pub fn load_config(dir: &Path) -> Result<JiraConfig> {
    let p = config_path(dir);
    let raw = std::fs::read_to_string(&p).map_err(|_| anyhow!("{} 가 없다(cloud_id 필요)", p.display()))?;
    serde_json::from_str(&raw).map_err(|e| anyhow!("{} 를 읽지 못함: {e}", p.display()))
}

/// 설정 파일이 없으면 빈 설정(화면에서 처음 채울 수 있게).
pub fn load_or_default(dir: &Path) -> JiraConfig {
    load_config(dir).unwrap_or_else(|_| serde_json::from_str("{}").expect("defaults"))
}

/// 임시 파일에 쓰고 rename 한다.
pub fn save_config(dir: &Path, c: &JiraConfig) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let p = config_path(dir);
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(c)?)?;
    std::fs::rename(tmp, p)?;
    Ok(())
}

// ---- 순수 함수: JQL · 본문 · 해석 -------------------------------------------------

pub fn key_label(key: &str) -> String {
    format!("{KEY_LABEL_PREFIX}{key}")
}

/// 이 담당자의 열린 자동 생성 티켓(5.3).
pub fn jql_open_bot(c: &JiraConfig, assignee: &str) -> String {
    format!(
        "labels = \"{}\" AND assignee = \"{}\" AND statusCategory != Done",
        c.bot_label, assignee
    )
}

/// 같은 키가 든 티켓(열린 것과 닫힌 것 모두, 3.3 b).
pub fn jql_by_keys(keys: &[String]) -> String {
    let list: Vec<String> = keys.iter().map(|k| format!("\"{}\"", key_label(k))).collect();
    format!("labels in ({})", list.join(", "))
}

/// 일반 텍스트(제목 줄 `#`, 목록 `- `)를 Jira 문서 형식(ADF)으로 바꾼다.
pub fn to_adf(text: &str) -> Value {
    let para = |t: &str| json!({"type":"paragraph","content":[{"type":"text","text":t}]});
    let mut content: Vec<Value> = Vec::new();
    let mut list: Vec<Value> = Vec::new();
    let flush = |content: &mut Vec<Value>, list: &mut Vec<Value>| {
        if !list.is_empty() {
            content.push(json!({"type":"bulletList","content": std::mem::take(list)}));
        }
    };
    for line in text.lines() {
        let t = line.trim_end();
        if t.trim().is_empty() {
            flush(&mut content, &mut list);
        } else if let Some(item) = t.trim_start().strip_prefix("- ") {
            list.push(json!({"type":"listItem","content":[para(item)]}));
        } else if t.starts_with('#') {
            flush(&mut content, &mut list);
            let level = t.chars().take_while(|c| *c == '#').count().clamp(1, 6);
            content.push(json!({"type":"heading","attrs":{"level":level},"content":[{"type":"text","text":t.trim_start_matches('#').trim()}]}));
        } else {
            flush(&mut content, &mut list);
            content.push(para(t));
        }
    }
    flush(&mut content, &mut list);
    json!({"type":"doc","version":1,"content":content})
}

pub struct NewTicket<'a> {
    pub title: &'a str,
    pub body: &'a str,
    /// 지적 키들. 각각 `sweep-<key>` 라벨이 된다.
    pub keys: &'a [String],
    pub estimate_md: f64,
    pub assignee: Option<&'a str>,
    /// rule / split / debt. 라벨 `sweepcat-<분류>` 로 남겨 리포트가 분류별로 집계한다.
    pub category: &'a str,
}

/// 생성 요청 본문. 스프린트는 비운다(기획 시간에 넣는다).
pub fn build_create_body(c: &JiraConfig, t: &NewTicket) -> Value {
    let mut labels = vec![c.work_label.clone(), c.bot_label.clone()];
    labels.extend(t.keys.iter().map(|k| key_label(k)));
    if !t.category.is_empty() {
        labels.push(format!("{CATEGORY_LABEL_PREFIX}{}", t.category));
    }
    let mut fields = json!({
        "project": {"key": c.project},
        "issuetype": {"name": c.issue_type},
        "summary": t.title,
        "description": to_adf(t.body),
        "labels": labels,
    });
    fields[&c.estimate_field] = json!(t.estimate_md);
    if let Some(p) = &c.parent_key {
        fields["parent"] = json!({"key": p});
    }
    if let Some(a) = t.assignee {
        fields["assignee"] = json!({"accountId": a});
    }
    json!({"fields": fields})
}

#[derive(Debug, Clone, PartialEq)]
pub struct Issue {
    pub key: String,
    pub labels: Vec<String>,
    pub done: bool,
    pub resolution: Option<String>,
    /// 코멘트 본문 텍스트(첫 페이지).
    pub comments: Vec<String>,
    pub summary: String,
    /// 생성 시각, unix 초(읽지 못하면 0).
    pub created: u64,
    pub parent: Option<String>,
    pub status: String,
    pub assignee: Option<String>,
}

fn adf_text(v: &Value, out: &mut String) {
    if let Some(t) = v.get("text").and_then(Value::as_str) {
        out.push_str(t);
    }
    if let Some(arr) = v.get("content").and_then(Value::as_array) {
        for c in arr {
            adf_text(c, out);
        }
        if matches!(v.get("type").and_then(Value::as_str), Some("paragraph") | Some("heading")) {
            out.push('\n');
        }
    }
}

/// Jira 시각(`2026-10-02T11:13:20.123+0900`)을 unix 초로. 읽지 못하면 0.
pub fn parse_jira_time(s: &str) -> u64 {
    chrono::DateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.3f%z").map(|d| d.timestamp().max(0) as u64).unwrap_or(0)
}

pub fn parse_issues(v: &Value) -> Vec<Issue> {
    v.get("issues")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|i| {
                    let f = i.get("fields")?;
                    let comments = f
                        .pointer("/comment/comments")
                        .and_then(Value::as_array)
                        .map(|cs| {
                            cs.iter()
                                .map(|c| {
                                    let mut s = String::new();
                                    if let Some(b) = c.get("body") {
                                        adf_text(b, &mut s);
                                    }
                                    s
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    Some(Issue {
                        key: i.get("key")?.as_str()?.to_string(),
                        labels: f
                            .get("labels")
                            .and_then(Value::as_array)
                            .map(|l| l.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                            .unwrap_or_default(),
                        done: f.pointer("/status/statusCategory/key").and_then(Value::as_str) == Some("done"),
                        resolution: f.pointer("/resolution/name").and_then(Value::as_str).map(String::from),
                        comments,
                        summary: f.get("summary").and_then(Value::as_str).unwrap_or("").to_string(),
                        created: f.get("created").and_then(Value::as_str).map_or(0, parse_jira_time),
                        parent: f.pointer("/parent/key").and_then(Value::as_str).map(String::from),
                        status: f.pointer("/status/name").and_then(Value::as_str).unwrap_or("").to_string(),
                        assignee: f.pointer("/assignee/displayName").and_then(Value::as_str).map(String::from),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 코멘트 첫 줄 `사유: …` 를 거절 사유로 읽는다(4.13). 형식이 없으면 사유 없음.
pub fn reject_reason_from_comments(comments: &[String]) -> RejectReason {
    for c in comments {
        let first = c.trim().lines().next().unwrap_or("").trim();
        if let Some(rest) = first.strip_prefix("사유:").or_else(|| first.strip_prefix("사유：")) {
            let r = rest.trim();
            return if r.contains("내용이 틀림") || r.contains("사실 틀림") {
                RejectReason::Wrong
            } else if r.contains("가치 낮음") {
                RejectReason::LowValue
            } else if r.contains("지금은 어려움") || r.contains("크기") {
                RejectReason::SizeTiming
            } else if r.contains("중복") {
                RejectReason::Duplicate
            } else if r.contains("이미 해결") {
                RejectReason::AlreadyFixed
            } else {
                RejectReason::Unknown
            };
        }
    }
    RejectReason::Unknown
}

/// 티켓 하나의 처리 결과. 완료 값 해석은 요구 4.16.
/// 닫히지 않았으면 만든 것(Created), 판정에서 뺄 완료 값이면 None.
pub fn outcome_of(issue: &Issue, now: u64) -> Option<KeyOutcome> {
    if !issue.done {
        return Some(KeyOutcome::Created { ticket: issue.key.clone(), at: now });
    }
    Some(match issue.resolution.as_deref() {
        Some("Done") => KeyOutcome::Done { ticket: issue.key.clone(), at: now },
        Some("Won't Do") => KeyOutcome::Rejected { reason: reject_reason_from_comments(&issue.comments), at: now },
        Some("Duplicate") => KeyOutcome::Rejected { reason: RejectReason::Duplicate, at: now },
        Some("재현 불가") => KeyOutcome::Rejected { reason: RejectReason::Wrong, at: now },
        Some("Canceled") => KeyOutcome::Rejected { reason: RejectReason::Unknown, at: now },
        _ => return None,
    })
}

/// 티켓이 가진 키 라벨마다 처리 결과를 낸다.
pub fn outcomes(issue: &Issue, now: u64) -> Vec<(String, KeyOutcome)> {
    let Some(outcome) = outcome_of(issue, now) else { return vec![] };
    issue
        .labels
        .iter()
        .filter_map(|l| l.strip_prefix(KEY_LABEL_PREFIX))
        .map(|k| (k.to_string(), outcome.clone()))
        .collect()
}

/// 자동 생성 티켓 결과 집계(반영률). 30일 넘게 열린 티켓은 반영률의 분모에서 빼고 따로 센다.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Results {
    pub total: usize,
    pub open: usize,
    /// 열린 채 `stale_days` 넘게 처리 없는 티켓.
    pub stale: usize,
    pub done: usize,
    pub rejected: usize,
    pub wrong: usize,
    pub low_value: usize,
    pub size_timing: usize,
    pub duplicate: usize,
    pub already_fixed: usize,
    pub no_reason: usize,
    /// done / (done + rejected), 퍼센트. 결과가 없으면 None.
    pub adoption_percent: Option<f64>,
    /// 확대 기준(결과 난 건 10건 이상, 반영률 60% 이상)을 넘었는가.
    pub ready_to_expand: bool,
}

pub const EXPAND_MIN_RESULTS: usize = 10;
pub const EXPAND_MIN_PERCENT: f64 = 60.0;

pub fn results(issues: &[Issue], now: u64, stale_days: u64) -> Results {
    let mut r = Results { total: issues.len(), ..Default::default() };
    for i in issues {
        match outcome_of(i, now) {
            Some(KeyOutcome::Created { .. }) => {
                r.open += 1;
                if i.created > 0 && now.saturating_sub(i.created) >= stale_days * 86_400 {
                    r.stale += 1;
                }
            }
            Some(KeyOutcome::Done { .. }) => r.done += 1,
            Some(KeyOutcome::Rejected { reason, .. }) => {
                r.rejected += 1;
                match reason {
                    RejectReason::Wrong => r.wrong += 1,
                    RejectReason::LowValue => r.low_value += 1,
                    RejectReason::SizeTiming => r.size_timing += 1,
                    RejectReason::Duplicate => r.duplicate += 1,
                    RejectReason::AlreadyFixed => r.already_fixed += 1,
                    RejectReason::Unknown => r.no_reason += 1,
                }
            }
            None => {}
        }
    }
    let judged = r.done + r.rejected;
    if judged > 0 {
        r.adoption_percent = Some(r.done as f64 * 100.0 / judged as f64);
    }
    r.ready_to_expand = judged >= EXPAND_MIN_RESULTS && r.adoption_percent.is_some_and(|p| p >= EXPAND_MIN_PERCENT);
    r
}

/// 이 설정으로 만든 자동 생성 티켓 전부.
pub fn jql_bot_all(c: &JiraConfig) -> String {
    format!("labels = \"{}\"", c.bot_label)
}

/// 상위 에픽이 없는 자동 생성 티켓(4.14). 사람이 만든 KTLO 티켓이 섞이지 않게 봇 라벨을 함께 쓴다.
pub fn jql_bot_orphans(c: &JiraConfig) -> String {
    format!("labels = \"{}\" AND parent is EMPTY", c.bot_label)
}

/// 티켓 키로 만든 링크. 사이트 주소가 없으면 빈 문자열.
pub fn browse_url(site: &str, key: &str) -> String {
    if site.is_empty() { String::new() } else { format!("{}/browse/{key}", site.trim_end_matches('/')) }
}

pub fn parse_users(v: &Value) -> Vec<(String, String)> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter(|u| u.get("accountType").and_then(Value::as_str).map_or(true, |t| t == "atlassian"))
                .filter(|u| u.get("active").and_then(Value::as_bool).unwrap_or(true))
                .filter_map(|u| Some((u.get("accountId")?.as_str()?.to_string(), u.get("displayName")?.as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

// ---- HTTP ------------------------------------------------------------------------

pub struct Jira {
    http: reqwest::Client,
    base: String,
    email: String,
    token: String,
}

pub(crate) fn read_env_file() -> Result<(String, String)> {
    let home = std::env::var("HOME").map_err(|_| anyhow!("HOME 이 없다"))?;
    let raw = std::fs::read_to_string(Path::new(&home).join(".config/jira.env")).map_err(|_| anyhow!("~/.config/jira.env 를 읽지 못함"))?;
    let get = |k: &str| {
        raw.lines()
            .filter_map(|l| l.trim().strip_prefix("export ").or(Some(l.trim())))
            .find_map(|l| l.strip_prefix(&format!("{k}=")))
            .map(|v| v.trim().trim_matches('"').trim_matches('\'').to_string())
    };
    match (get("JIRA_EMAIL"), get("JIRA_TOKEN")) {
        (Some(e), Some(t)) if !e.is_empty() && !t.is_empty() => Ok((e, t)),
        _ => Err(anyhow!("jira.env 에 JIRA_EMAIL/JIRA_TOKEN 이 없다")),
    }
}

impl Jira {
    pub fn connect(c: &JiraConfig) -> Result<Self> {
        let (email, token) = read_env_file()?;
        Ok(Jira {
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(30))
                .build()?,
            base: format!("https://api.atlassian.com/ex/jira/{}/rest/api/3", c.cloud_id),
            email,
            token,
        })
    }

    /// 상태 코드와 본문 일부만 오류에 담는다(자격은 담지 않는다).
    async fn check(resp: reqwest::Response) -> Result<Value> {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(anyhow!("Jira {status}: {}", body.chars().take(300).collect::<String>()));
        }
        Ok(serde_json::from_str(&body).unwrap_or(Value::Null))
    }

    /// 읽기 전용 검색. 최대 `limit` 건.
    pub async fn search(&self, jql: &str, limit: usize) -> Result<Vec<Issue>> {
        let mut all = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut q = vec![
                ("jql", jql.to_string()),
                ("fields", "labels,status,resolution,comment,summary,created,parent,assignee".to_string()),
                ("maxResults", limit.min(100).to_string()),
            ];
            if let Some(t) = &token {
                q.push(("nextPageToken", t.clone()));
            }
            let resp = self.http.get(format!("{}/search/jql", self.base)).basic_auth(&self.email, Some(&self.token)).query(&q).send().await?;
            let v = Self::check(resp).await?;
            all.extend(parse_issues(&v));
            token = v.get("nextPageToken").and_then(Value::as_str).map(String::from);
            if token.is_none() || all.len() >= limit {
                break;
            }
        }
        all.truncate(limit);
        Ok(all)
    }

    pub async fn count_open_bot(&self, c: &JiraConfig, assignee: &str) -> Result<usize> {
        Ok(self.search(&jql_open_bot(c, assignee), 1000).await?.len())
    }

    /// 읽기: 연결 확인용. (표시 이름)
    pub async fn myself(&self) -> Result<String> {
        let resp = self.http.get(format!("{}/myself", self.base)).basic_auth(&self.email, Some(&self.token)).send().await?;
        let v = Self::check(resp).await?;
        Ok(v.get("displayName").and_then(Value::as_str).unwrap_or("?").to_string())
    }

    /// 읽기: 사이트 주소(`https://….atlassian.net`).
    pub async fn site_url(&self) -> Result<String> {
        let resp = self.http.get(format!("{}/serverInfo", self.base)).basic_auth(&self.email, Some(&self.token)).send().await?;
        let v = Self::check(resp).await?;
        v.get("baseUrl").and_then(Value::as_str).map(|s| s.trim_end_matches('/').to_string()).ok_or_else(|| anyhow!("응답에 baseUrl 이 없다"))
    }

    /// 읽기: 담당자 후보 검색. (accountId, 표시 이름)
    pub async fn search_users(&self, query: &str) -> Result<Vec<(String, String)>> {
        let resp = self
            .http
            .get(format!("{}/user/search", self.base))
            .basic_auth(&self.email, Some(&self.token))
            .query(&[("query", query), ("maxResults", "8")])
            .send()
            .await?;
        let v = Self::check(resp).await?;
        Ok(parse_users(&v))
    }

    /// 읽기: 상위 에픽의 (제목, 완료 여부).
    pub async fn issue_brief(&self, key: &str) -> Result<(String, bool)> {
        let resp = self
            .http
            .get(format!("{}/issue/{key}", self.base))
            .basic_auth(&self.email, Some(&self.token))
            .query(&[("fields", "summary,status")])
            .send()
            .await?;
        let v = Self::check(resp).await?;
        let title = v.pointer("/fields/summary").and_then(Value::as_str).unwrap_or("").to_string();
        let done = v.pointer("/fields/status/statusCategory/key").and_then(Value::as_str) == Some("done");
        Ok((title, done))
    }

    /// 쓰기: 티켓의 상위 에픽 지정. 호출부가 `allow_create` 를 확인한다.
    pub async fn set_parent(&self, c: &JiraConfig, key: &str, parent: &str) -> Result<()> {
        if !c.allow_create {
            return Err(anyhow!("sweep-jira.json 의 allow_create 가 false 다"));
        }
        let resp = self
            .http
            .put(format!("{}/issue/{key}", self.base))
            .basic_auth(&self.email, Some(&self.token))
            .json(&json!({"fields": {"parent": {"key": parent}}}))
            .send()
            .await?;
        Self::check(resp).await.map(|_| ())
    }

    /// 쓰기: 티켓 생성. 호출부가 `allow_create` 를 확인한다.
    pub async fn create(&self, c: &JiraConfig, t: &NewTicket<'_>) -> Result<String> {
        if !c.allow_create {
            return Err(anyhow!("sweep-jira.json 의 allow_create 가 false 다"));
        }
        let resp = self.http.post(format!("{}/issue", self.base)).basic_auth(&self.email, Some(&self.token)).json(&build_create_body(c, t)).send().await?;
        let v = Self::check(resp).await?;
        v.get("key").and_then(Value::as_str).map(String::from).ok_or_else(|| anyhow!("응답에 key 가 없다"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> JiraConfig {
        serde_json::from_str(r#"{"cloud_id":"x","parent_key":"SID-1","assignees":["acc"]}"#).unwrap()
    }

    #[test]
    fn users_skip_bots_and_inactive_accounts() {
        let v = json!([
            {"accountId":"a1","displayName":"홍길동","accountType":"atlassian","active":true},
            {"accountId":"b1","displayName":"앱","accountType":"app","active":true},
            {"accountId":"c1","displayName":"퇴사자","accountType":"atlassian","active":false}
        ]);
        assert_eq!(parse_users(&v), vec![("a1".to_string(), "홍길동".to_string())]);
    }

    #[test]
    fn config_roundtrips_and_missing_file_gives_defaults() {
        let dir = std::env::temp_dir().join(format!("jira-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let d = load_or_default(&dir);
        assert_eq!((d.cloud_id.as_str(), d.open_cap, d.allow_create), ("", 10, false));
        let mut c = cfg();
        c.open_cap = 7;
        c.assignee_names.insert("acc".into(), "나".into());
        save_config(&dir, &c).unwrap();
        let back = load_config(&dir).unwrap();
        assert_eq!((back.open_cap, back.assignee_names.get("acc").map(String::as_str)), (7, Some("나")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_defaults_and_create_is_off() {
        let c = cfg();
        assert_eq!((c.project.as_str(), c.issue_type.as_str(), c.work_label.as_str()), ("SID", "Dev", "KTLO"));
        assert!(!c.allow_create);
    }

    #[test]
    fn jql_for_open_tickets_and_keys() {
        let c = cfg();
        assert_eq!(jql_open_bot(&c, "acc"), "labels = \"bot-created\" AND assignee = \"acc\" AND statusCategory != Done");
        assert_eq!(jql_by_keys(&["a1".into(), "b2".into()]), "labels in (\"sweep-a1\", \"sweep-b2\")");
    }

    #[test]
    fn create_body_has_labels_parent_estimate_and_no_sprint() {
        let c = cfg();
        let keys = vec!["k1".to_string()];
        let b = build_create_body(&c, &NewTicket { title: "[KTLO] 제목", body: "# 배경\n설명\n- 하나\n- 둘", keys: &keys, estimate_md: 0.5, assignee: Some("acc"), category: "debt" });
        let f = &b["fields"];
        assert_eq!(f["labels"], json!(["KTLO", "bot-created", "sweep-k1", "sweepcat-debt"]));
        assert_eq!(category_of(&["x".to_string(), "sweepcat-split".to_string()]), Some("split"));
        assert_eq!(f["parent"]["key"], "SID-1");
        assert_eq!(f["customfield_12766"], 0.5);
        assert_eq!(f["assignee"]["accountId"], "acc");
        assert!(f.get("customfield_10020").is_none());
        assert_eq!(f["description"]["content"][0]["type"], "heading");
        assert_eq!(f["description"]["content"][2]["type"], "bulletList");
    }

    #[test]
    fn create_body_without_parent_or_assignee() {
        let mut c = cfg();
        c.parent_key = None;
        let b = build_create_body(&c, &NewTicket { title: "t", body: "b", keys: &[], estimate_md: 1.0, assignee: None, category: "" });
        assert!(b["fields"].get("parent").is_none() && b["fields"].get("assignee").is_none());
    }

    #[test]
    fn reject_reason_is_read_from_the_first_line_of_a_comment() {
        let r = |s: &str| reject_reason_from_comments(&[s.to_string()]);
        assert_eq!(r("사유: 사실 틀림\n이 파일은 이미 분리됨"), RejectReason::Wrong);
        assert_eq!(r("사유: 가치 낮음"), RejectReason::LowValue);
        assert_eq!(r("사유: 크기·시점"), RejectReason::SizeTiming);
        // 새 안내 문구와 예전 문구를 모두 읽는다.
        assert_eq!(r("사유: 내용이 틀림"), RejectReason::Wrong);
        assert_eq!(r("사유: 지금은 어려움(크기·시점)"), RejectReason::SizeTiming);
        assert_eq!(r("사유: 중복"), RejectReason::Duplicate);
        assert_eq!(r("사유: 이미 해결"), RejectReason::AlreadyFixed);
        assert_eq!(r("그냥 닫음"), RejectReason::Unknown);
        assert_eq!(r("메모\n사유: 중복"), RejectReason::Unknown, "첫 줄만 본다");
        assert_eq!(reject_reason_from_comments(&[]), RejectReason::Unknown);
    }

    fn issue(done: bool, res: Option<&str>, comments: &[&str]) -> Issue {
        Issue {
            key: "SID-9".into(),
            labels: vec!["KTLO".into(), "bot-created".into(), "sweep-k1".into(), "sweep-k2".into()],
            done,
            resolution: res.map(String::from),
            comments: comments.iter().map(|s| s.to_string()).collect(),
            summary: "t".into(),
            created: 0,
            parent: None,
            status: String::new(),
            assignee: None,
        }
    }

    #[test]
    fn outcomes_follow_resolution_mapping() {
        let keys = |v: Vec<(String, KeyOutcome)>| v.into_iter().map(|(k, _)| k).collect::<Vec<_>>();
        let open = outcomes(&issue(false, None, &[]), 5);
        assert_eq!(keys(open.clone()), ["k1", "k2"]);
        assert!(matches!(open[0].1, KeyOutcome::Created { .. }));
        assert!(matches!(outcomes(&issue(true, Some("Done"), &[]), 5)[0].1, KeyOutcome::Done { .. }));
        assert!(matches!(
            outcomes(&issue(true, Some("Won't Do"), &["사유: 가치 낮음"]), 5)[0].1,
            KeyOutcome::Rejected { reason: RejectReason::LowValue, .. }
        ));
        assert!(matches!(outcomes(&issue(true, Some("Duplicate"), &[]), 5)[0].1, KeyOutcome::Rejected { reason: RejectReason::Duplicate, .. }));
        assert!(matches!(outcomes(&issue(true, Some("재현 불가"), &[]), 5)[0].1, KeyOutcome::Rejected { reason: RejectReason::Wrong, .. }));
        assert!(matches!(outcomes(&issue(true, Some("Canceled"), &[]), 5)[0].1, KeyOutcome::Rejected { reason: RejectReason::Unknown, .. }));
        assert!(outcomes(&issue(true, Some("미배포완료"), &[]), 5).is_empty(), "그 밖의 값은 판정에서 뺀다");
    }

    #[test]
    fn results_count_adoption_without_open_tickets_and_flag_expansion() {
        let mk = |done: bool, res: Option<&str>, created: u64, comments: &[&str]| {
            let mut i = issue(done, res, comments);
            i.created = created;
            i
        };
        let now = 100 * 86_400;
        let mut v = vec![];
        for _ in 0..7 {
            v.push(mk(true, Some("Done"), 1, &[]));
        }
        v.push(mk(true, Some("Won't Do"), 1, &["사유: 가치 낮음"]));
        v.push(mk(true, Some("Duplicate"), 1, &[]));
        v.push(mk(true, Some("Canceled"), 1, &[]));
        v.push(mk(false, None, 99 * 86_400, &[])); // 열림, 최근
        v.push(mk(false, None, 10 * 86_400, &[])); // 열림, 90일 방치
        v.push(mk(true, Some("미배포완료"), 1, &[])); // 판정에서 뺌
        let r = results(&v, now, 30);
        assert_eq!((r.total, r.open, r.stale, r.done, r.rejected), (13, 2, 1, 7, 3));
        assert_eq!((r.low_value, r.duplicate, r.no_reason), (1, 1, 1));
        assert_eq!(r.adoption_percent, Some(70.0));
        assert!(r.ready_to_expand);
        // 결과가 10건 미만이면 판단하지 않는다.
        let few = results(&v[..5], now, 30);
        assert!(!few.ready_to_expand);
        // 반영률이 낮으면 건수가 충분해도 넓히지 않는다.
        let low: Vec<_> = (0..10).map(|n| mk(true, Some(if n < 3 { "Done" } else { "Won't Do" }), 1, &[])).collect();
        assert!(!results(&low, now, 30).ready_to_expand);
        assert_eq!(results(&[], now, 30).adoption_percent, None);
    }

    #[test]
    fn category_label_is_not_mistaken_for_a_key_label() {
        let mut i = issue(false, None, &[]);
        i.labels = vec!["sweep-abc".into(), "sweepcat-rule".into()];
        let keys: Vec<_> = outcomes(&i, 1).into_iter().map(|(k, _)| k).collect();
        assert_eq!(keys, ["abc"]);
    }

    #[test]
    fn browse_url_needs_a_site() {
        assert_eq!(browse_url("https://x.atlassian.net/", "SID-1"), "https://x.atlassian.net/browse/SID-1");
        assert_eq!(browse_url("", "SID-1"), "");
    }

    #[test]
    fn jira_time_and_orphan_query() {
        assert_eq!(parse_jira_time("2026-10-02T11:13:20.123+0900"), 1_790_907_200);
        assert_eq!(parse_jira_time("garbage"), 0);
        assert_eq!(jql_bot_orphans(&cfg()), "labels = \"bot-created\" AND parent is EMPTY");
    }

    #[test]
    fn search_response_is_parsed_including_adf_comments() {
        let v = json!({"issues":[{"key":"SID-3","fields":{
            "labels":["sweep-aa"],
            "status":{"statusCategory":{"key":"done"}},
            "resolution":{"name":"Won't Do"},
            "comment":{"comments":[{"body":{"type":"doc","content":[{"type":"paragraph","content":[{"type":"text","text":"사유: 중복"}]}]}}]}}}]});
        let is = parse_issues(&v);
        assert_eq!(is.len(), 1);
        assert!(is[0].done);
        assert_eq!(reject_reason_from_comments(&is[0].comments), RejectReason::Duplicate);
    }
}
