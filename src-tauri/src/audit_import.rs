//! Historical GitHub evidence import. Missing execution metadata stays unknown.
use anyhow::Result;
use serde_json::Value;

pub fn github_records(input: &Value, me: &str) -> Result<Vec<crate::audit::Record>> {
    use crate::audit::Record;
    use anyhow::{anyhow, ensure, Context};
    use std::collections::BTreeMap;

    ensure!(!me.trim().is_empty(), "reviewer login is required");
    let pulls: Vec<&Value> = match input {
        Value::Array(pulls) => pulls.iter().collect(),
        Value::Object(_) => vec![input],
        _ => anyhow::bail!("GitHub import must be a pull object or an array of pull objects"),
    };
    let mut records: BTreeMap<u64, Record> = BTreeMap::new();
    for pull in pulls {
        let repo = pull["repo"]
            .as_str()
            .filter(|s| {
                let parts: Vec<_> = s.split('/').collect();
                parts.len() == 2 && parts.iter().all(|part| !part.trim().is_empty())
            })
            .ok_or_else(|| anyhow!("pull repo must be owner/name"))?;
        let pr = pull["number"]
            .as_u64()
            .filter(|n| *n > 0)
            .ok_or_else(|| anyhow!("{repo}: positive pull number required"))?;
        let reviews = pull["reviews"]
            .as_array()
            .ok_or_else(|| anyhow!("{repo}#{pr}: reviews array required"))?;
        let comments = pull["comments"]
            .as_array()
            .ok_or_else(|| anyhow!("{repo}#{pr}: comments array required"))?;
        for review in reviews {
            if !review["user"]["login"]
                .as_str()
                .is_some_and(|login| login.eq_ignore_ascii_case(me))
            {
                continue;
            }
            let body = review["body"].as_str().unwrap_or("");
            let state = review["state"].as_str().unwrap_or("unknown");
            let (status, finished_cleanly) = if body.contains("<!-- approve-bot:review-failed -->")
            {
                ("failure_notice", Some(false))
            } else if body.contains("<!-- approve-bot:review -->") {
                // DISMISSED is a current state, not evidence of its original
                // approval event. Preserve it below without inferring approval.
                (
                    if state.eq_ignore_ascii_case("APPROVED") {
                        "approved"
                    } else {
                        "commented"
                    },
                    Some(true),
                )
            } else if body.trim().is_empty() && state.eq_ignore_ascii_case("APPROVED") {
                ("reapproval", None)
            } else {
                continue;
            };
            let id = review["id"].as_u64().filter(|id| *id > 0).ok_or_else(|| {
                anyhow!("{repo}#{pr}: selected review requires a positive numeric id")
            })?;
            let at = review["submitted_at"]
                .as_str()
                .ok_or_else(|| anyhow!("review {id}: submitted_at required"))?;
            chrono::DateTime::parse_from_rfc3339(at)
                .with_context(|| format!("review {id}: invalid submitted_at"))?;
            let mut finding_ids = Vec::new();
            for comment in comments {
                if comment["pull_request_review_id"].as_u64() != Some(id)
                    || !comment["in_reply_to_id"].is_null()
                {
                    continue;
                }
                if let Some(fid) = comment["body"]
                    .as_str()
                    .and_then(crate::rereview::marker_id)
                {
                    if !finding_ids.contains(&fid) {
                        finding_ids.push(fid);
                    }
                }
            }
            if let Some(existing) = records.get_mut(&id) {
                ensure!(
                    existing.repo == repo && existing.pr == pr,
                    "review {id} occurs under conflicting pull contexts"
                );
                for fid in finding_ids {
                    if !existing.finding_ids.contains(&fid) {
                        existing.finding_ids.push(fid);
                    }
                }
                continue;
            }
            records.insert(id, Record {
                run_id: format!("github:{id}"),
                at: at.into(),
                repo: repo.into(),
                pr,
                head_sha: review["commit_id"].as_str().filter(|s| !s.is_empty()).unwrap_or("unknown").into(),
                source: "github_import".into(),
                status: status.into(),
                model: "unknown".into(),
                risk: "unknown".into(),
                build_sha: "unknown".into(),
                guide_hash: "unknown".into(),
                guide_source: "unknown".into(),
                finished_cleanly,
                review_id: Some(id),
                review_url: review["html_url"].as_str().filter(|s| !s.is_empty()).map(str::to_owned),
                finding_ids,
                result: Some(serde_json::json!({"body":body,"github_state":state,"timing_available":false,"execution_metadata_available":false})),
                ..Record::default()
            });
        }
    }
    Ok(records.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn review(id: u64, body: &str, state: &str) -> Value {
        json!({"id":id,"user":{"login":"reviewer"},"body":body,"state":state,
            "submitted_at":"2026-10-14T01:02:03Z","commit_id":"abc1234",
            "html_url":format!("https://github.com/example/repo/pull/12#pullrequestreview-{id}")})
    }
    fn pull(reviews: Vec<Value>) -> Value {
        json!({"repo":"example/repo","number":12,"reviews":reviews,"comments":[]})
    }

    #[test]
    fn selects_engine_failures_and_bare_approvals_without_counting_reply_reviews() {
        let mut other = review(6, "<!-- approve-bot:review -->", "COMMENTED");
        other["user"]["login"] = json!("other");
        let input = pull(vec![
            review(1, "<!-- approve-bot:review -->\nresult", "APPROVED"),
            review(2, "<!-- approve-bot:review -->\nresult", "COMMENTED"),
            review(
                3,
                "<!-- approve-bot:review-failed -->\ntimeout",
                "COMMENTED",
            ),
            review(4, "", "APPROVED"),
            review(5, "", "COMMENTED"),
            other,
            review(7, "human note", "COMMENTED"),
            review(8, "<!-- approve-bot:review -->\nold result", "DISMISSED"),
        ]);
        let records = github_records(&input, "REVIEWER").unwrap();
        assert_eq!(records.len(), 5);
        assert_eq!(
            records
                .iter()
                .map(|r| r.status.as_str())
                .collect::<Vec<_>>(),
            vec![
                "approved",
                "commented",
                "failure_notice",
                "reapproval",
                "commented"
            ]
        );
        assert_eq!(
            records
                .iter()
                .map(|r| r.finished_cleanly)
                .collect::<Vec<_>>(),
            vec![Some(true), Some(true), Some(false), None, Some(true)]
        );
        assert_eq!(records[0].run_id, "github:1");
        assert_eq!(records[0].at, "2026-10-14T01:02:03Z");
        assert_eq!(records[0].source, "github_import");
        assert_eq!(records[0].head_sha, "abc1234");
        assert_eq!(records[0].review_id, Some(1));
        assert!(records.iter().all(|r| r.cost_usd.is_none()
            && r.build_sha == "unknown"
            && r.guide_hash == "unknown"));
        assert_eq!(
            records[4].result.as_ref().unwrap()["github_state"],
            "DISMISSED"
        );
    }

    #[test]
    fn associates_root_findings_by_review_id_and_deduplicates_imports() {
        let mut p = pull(vec![review(11, "<!-- approve-bot:review -->", "COMMENTED")]);
        p["comments"] = json!([
            {"id":1,"pull_request_review_id":11,"body":"<!-- f:aaaa1111 -->"},
            {"id":2,"pull_request_review_id":11,"body":"<!-- f:aaaa1111 -->"},
            {"id":3,"pull_request_review_id":11,"body":"<!-- f:bbbb2222 -->","in_reply_to_id":1},
            {"id":4,"pull_request_review_id":12,"body":"<!-- f:cccc3333 -->"}
        ]);
        let rows = github_records(&json!([p.clone(), p]), "reviewer").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].finding_ids, vec!["aaaa1111"]);
    }

    #[test]
    fn rejects_invalid_selected_review_identity_dates_and_pull_context() {
        for (field, value) in [
            ("id", json!(0)),
            ("id", json!("")),
            ("submitted_at", json!("yesterday")),
            ("submitted_at", Value::Null),
        ] {
            let mut r = review(1, "<!-- approve-bot:review -->", "COMMENTED");
            r[field] = value;
            assert!(
                github_records(&pull(vec![r]), "reviewer").is_err(),
                "{field}"
            );
        }
        assert!(github_records(
            &json!({"repo":"", "number":0,"reviews":[],"comments":[]}),
            "reviewer"
        )
        .is_err());
        assert!(github_records(&pull(vec![]), "").is_err());
        assert!(github_records(&json!(null), "reviewer").is_err());
    }
}
