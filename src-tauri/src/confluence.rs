//! Confluence 연동(리포트 게시, R.1·R.6·R.8). 같은 Atlassian 자격을 호출 때마다 읽고 로그·오류에 남기지 않는다.
//! 쓰기(생성·갱신)는 호출부가 게시 설정을 확인한 뒤에만 부른다. 같은 제목의 페이지가 있으면 새로 만들지 않고 갱신한다.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::time::Duration;

/// 마크다운(리포트에서 쓰는 부분집합)을 Confluence 저장 형식(XHTML)으로 바꾼다.
/// 제목(#, ##), 목록(- ), 표(| … |), 문단, 굵게(**), 링크([글](주소))를 다룬다. 나머지는 문단으로 둔다.
pub fn md_to_storage(md: &str) -> String {
    let mut out = String::new();
    let mut list_open = false;
    let lines: Vec<&str> = md.lines().collect();
    let mut i = 0;
    let close_list = |out: &mut String, open: &mut bool| {
        if *open {
            out.push_str("</ul>");
            *open = false;
        }
    };
    while i < lines.len() {
        let line = lines[i].trim_end();
        let t = line.trim_start();
        if t.is_empty() {
            close_list(&mut out, &mut list_open);
        } else if t.starts_with('|') {
            close_list(&mut out, &mut list_open);
            let mut rows: Vec<&str> = vec![];
            while i < lines.len() && lines[i].trim_start().starts_with('|') {
                rows.push(lines[i].trim());
                i += 1;
            }
            out.push_str(&table(&rows));
            continue;
        } else if let Some(rest) = t.strip_prefix("- ") {
            if !list_open {
                out.push_str("<ul>");
                list_open = true;
            }
            out.push_str(&format!("<li>{}</li>", inline(rest)));
        } else if t.starts_with('#') {
            close_list(&mut out, &mut list_open);
            let level = t.chars().take_while(|c| *c == '#').count().clamp(1, 6);
            out.push_str(&format!("<h{level}>{}</h{level}>", inline(t.trim_start_matches('#').trim())));
        } else {
            close_list(&mut out, &mut list_open);
            out.push_str(&format!("<p>{}</p>", inline(t)));
        }
        i += 1;
    }
    close_list(&mut out, &mut list_open);
    out
}

fn split_row(row: &str) -> Vec<String> {
    // `\|` 는 칸 안의 | 로 둔다.
    let inner = row.trim().trim_start_matches('|').trim_end_matches('|');
    let mut cells = vec![];
    let mut cur = String::new();
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' && chars.peek() == Some(&'|') {
            cur.push('|');
            chars.next();
        } else if c == '|' {
            cells.push(cur.trim().to_string());
            cur.clear();
        } else {
            cur.push(c);
        }
    }
    cells.push(cur.trim().to_string());
    cells
}

fn table(rows: &[&str]) -> String {
    let is_sep = |r: &str| split_row(r).iter().all(|c| !c.is_empty() && c.chars().all(|ch| matches!(ch, '-' | ':' | ' ')));
    let mut s = String::from("<table><tbody>");
    for (n, r) in rows.iter().enumerate() {
        if n == 1 && is_sep(r) {
            continue;
        }
        let tag = if n == 0 { "th" } else { "td" };
        s.push_str("<tr>");
        for c in split_row(r) {
            s.push_str(&format!("<{tag}>{}</{tag}>", inline(&c)));
        }
        s.push_str("</tr>");
    }
    s.push_str("</tbody></table>");
    s
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// 한 줄 안의 `**굵게**` 와 `[글](주소)`. 나머지는 이스케이프한 글자.
fn inline(s: &str) -> String {
    let mut out = String::new();
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        // 링크
        if chars[i] == '[' {
            if let Some(close) = chars[i..].iter().position(|c| *c == ']') {
                let close = i + close;
                if chars.get(close + 1) == Some(&'(') {
                    if let Some(end) = chars[close + 2..].iter().position(|c| *c == ')') {
                        let end = close + 2 + end;
                        let text: String = chars[i + 1..close].iter().collect();
                        let url: String = chars[close + 2..end].iter().collect();
                        if url.starts_with("http://") || url.starts_with("https://") {
                            out.push_str(&format!("<a href=\"{}\">{}</a>", escape(&url).replace('"', "&quot;"), escape(&text)));
                            i = end + 1;
                            continue;
                        }
                    }
                }
            }
        }
        // 굵게
        if chars[i] == '*' && chars.get(i + 1) == Some(&'*') {
            if let Some(rel) = (i + 2..chars.len().saturating_sub(1)).find(|&k| chars[k] == '*' && chars[k + 1] == '*') {
                let text: String = chars[i + 2..rel].iter().collect();
                out.push_str(&format!("<strong>{}</strong>", escape(&text)));
                i = rel + 2;
                continue;
            }
        }
        out.push_str(&escape(&chars[i].to_string()));
        i += 1;
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageRef {
    pub id: String,
    pub version: u64,
}

pub fn build_create_body(space: &str, parent_id: &str, title: &str, storage: &str) -> Value {
    json!({
        "type": "page",
        "title": title,
        "space": {"key": space},
        "ancestors": [{"id": parent_id}],
        "body": {"storage": {"value": storage, "representation": "storage"}},
    })
}

pub fn build_update_body(page: &PageRef, title: &str, storage: &str) -> Value {
    json!({
        "type": "page",
        "title": title,
        "version": {"number": page.version + 1},
        "body": {"storage": {"value": storage, "representation": "storage"}},
    })
}

/// 제목 검색 결과에서 첫 페이지.
pub fn parse_search(v: &Value) -> Option<PageRef> {
    let p = v.get("results")?.as_array()?.first()?;
    Some(PageRef { id: p.get("id")?.as_str()?.to_string(), version: p.pointer("/version/number")?.as_u64()? })
}

/// 응답의 `_links` 로 페이지 주소를 만든다.
pub fn page_url(v: &Value) -> String {
    let base = v.pointer("/_links/base").and_then(Value::as_str).unwrap_or("");
    let ui = v.pointer("/_links/webui").and_then(Value::as_str).unwrap_or("");
    if base.is_empty() || ui.is_empty() { String::new() } else { format!("{base}{ui}") }
}

pub struct Confluence {
    http: reqwest::Client,
    base: String,
    email: String,
    token: String,
}

impl Confluence {
    pub fn connect(cloud_id: &str) -> Result<Self> {
        let (email, token) = crate::jira::read_env_file()?;
        Ok(Confluence {
            http: reqwest::Client::builder().connect_timeout(Duration::from_secs(10)).timeout(Duration::from_secs(30)).build()?,
            base: format!("https://api.atlassian.com/ex/confluence/{cloud_id}/wiki/rest/api"),
            email,
            token,
        })
    }

    async fn check(resp: reqwest::Response) -> Result<Value> {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(anyhow!("Confluence {status}: {}", body.chars().take(300).collect::<String>()));
        }
        Ok(serde_json::from_str(&body).unwrap_or(Value::Null))
    }

    /// 읽기: 같은 space 에서 제목이 같은 페이지.
    pub async fn find_page(&self, space: &str, title: &str) -> Result<Option<PageRef>> {
        let resp = self
            .http
            .get(format!("{}/content", self.base))
            .basic_auth(&self.email, Some(&self.token))
            .query(&[("spaceKey", space), ("title", title), ("type", "page"), ("expand", "version"), ("limit", "1")])
            .send()
            .await?;
        Ok(parse_search(&Self::check(resp).await?))
    }

    /// 읽기: 페이지 제목과 space(연결·부모 확인용).
    pub async fn page_brief(&self, id: &str) -> Result<(String, String)> {
        let resp = self
            .http
            .get(format!("{}/content/{id}", self.base))
            .basic_auth(&self.email, Some(&self.token))
            .query(&[("expand", "space")])
            .send()
            .await?;
        let v = Self::check(resp).await?;
        Ok((
            v.get("title").and_then(Value::as_str).unwrap_or("").to_string(),
            v.pointer("/space/key").and_then(Value::as_str).unwrap_or("").to_string(),
        ))
    }

    /// 쓰기: 제목이 같은 페이지가 있으면 갱신, 없으면 부모 아래 생성(R.6). 페이지 주소를 돌려준다.
    pub async fn publish(&self, space: &str, parent_id: &str, title: &str, storage: &str) -> Result<String> {
        match self.find_page(space, title).await? {
            Some(page) => {
                let resp = self
                    .http
                    .put(format!("{}/content/{}", self.base, page.id))
                    .basic_auth(&self.email, Some(&self.token))
                    .json(&build_update_body(&page, title, storage))
                    .send()
                    .await?;
                Ok(page_url(&Self::check(resp).await?))
            }
            None => {
                let resp = self
                    .http
                    .post(format!("{}/content", self.base))
                    .basic_auth(&self.email, Some(&self.token))
                    .json(&build_create_body(space, parent_id, title, storage))
                    .send()
                    .await?;
                Ok(page_url(&Self::check(resp).await?))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headings_lists_paragraphs() {
        let h = md_to_storage("# 제목\n\n본문 한 줄\n\n## 1. 요약\n\n- 하나\n- 둘 **굵게**\n\n끝");
        assert_eq!(h, "<h1>제목</h1><p>본문 한 줄</p><h2>1. 요약</h2><ul><li>하나</li><li>둘 <strong>굵게</strong></li></ul><p>끝</p>");
    }

    #[test]
    fn tables_skip_the_separator_row_and_keep_escaped_pipes() {
        let md = "| 구분 | 값 |\n|---|--:|\n| a \\| b | 1 |\n";
        assert_eq!(md_to_storage(md), "<table><tbody><tr><th>구분</th><th>값</th></tr><tr><td>a | b</td><td>1</td></tr></tbody></table>");
    }

    #[test]
    fn links_and_html_are_escaped() {
        let h = md_to_storage("| [SID-1](https://x.atlassian.net/browse/SID-1) | <b>&</b> |");
        assert!(h.contains("<a href=\"https://x.atlassian.net/browse/SID-1\">SID-1</a>"));
        assert!(h.contains("&lt;b&gt;&amp;&lt;/b&gt;"));
        // 주소가 http(s) 가 아니면 링크로 만들지 않는다.
        assert!(!md_to_storage("[x](javascript:alert(1))").contains("<a "));
    }

    #[test]
    fn real_report_shape_converts_without_leaking_markdown_syntax() {
        let md = "# 정기 스윕 주간 리포트 2026-W40\n\n기간: 2026-09-28 ~ 2026-10-04\n\n## 3. 결과\n\n| 구분 | 완료 |\n|---|--:|\n| 누적 | 4 |\n\n방치율: 0건\n";
        let h = md_to_storage(md);
        assert!(h.starts_with("<h1>") && h.contains("<table>") && !h.contains("|---") && !h.contains("## "));
    }

    #[test]
    fn bodies_and_search_parsing() {
        let c = build_create_body("SP", "42", "제목", "<p>x</p>");
        assert_eq!((c["space"]["key"].as_str(), c["ancestors"][0]["id"].as_str(), c["body"]["storage"]["representation"].as_str()), (Some("SP"), Some("42"), Some("storage")));
        let p = parse_search(&json!({"results":[{"id":"7","version":{"number":3}}]})).unwrap();
        assert_eq!(p, PageRef { id: "7".into(), version: 3 });
        assert_eq!(build_update_body(&p, "제목", "<p/>")["version"]["number"], 4);
        assert!(parse_search(&json!({"results":[]})).is_none());
        assert_eq!(page_url(&json!({"_links":{"base":"https://w/wiki","webui":"/spaces/S/pages/7"}})), "https://w/wiki/spaces/S/pages/7");
        assert_eq!(page_url(&json!({})), "");
    }
}
