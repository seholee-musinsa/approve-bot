//! Pure publication policy. Unknown state fails closed.
use crate::github::PullRequest;

pub fn check(pr: &PullRequest, expected_sha: &str) -> Result<(), &'static str> {
    if pr.state != "open" || pr.merged || pr.merged_at.is_some() {
        Err("PR closed, merged, or state unknown")
    } else if pr.draft {
        Err("PR is a draft")
    } else if expected_sha.is_empty() || pr.head.sha != expected_sha {
        Err("PR head changed or is unknown")
    } else {
        Ok(())
    }
}

pub fn check_snapshot(pr: &PullRequest, expected: &PullRequest) -> Result<(), &'static str> {
    check(pr, &expected.head.sha)?;
    if pr.base.sha != expected.base.sha
        || pr.base.ref_name != expected.base.ref_name
        || pr.body != expected.body
        || pr.title != expected.title
    {
        Err("PR base or review context changed")
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr() -> PullRequest {
        serde_json::from_value(serde_json::json!({
            "number": 1, "title": "change", "draft": false,
            "html_url": "https://github.com/o/r/pull/1", "user": {"login": "author"},
            "head": {"sha": "abc"}, "state": "open"
        }))
        .unwrap()
    }

    #[test]
    fn only_the_open_reviewed_head_can_be_published() {
        let mut p = pr();
        assert!(check(&p, "abc").is_ok());
        assert!(check(&p, "old").is_err());
        p.draft = true;
        assert!(check(&p, "abc").is_err());
        p.draft = false;
        for state in ["closed", "", "unknown"] {
            p.state = state.into();
            assert!(check(&p, "abc").is_err(), "{state}");
        }
        p.state = "open".into();
        p.merged = true;
        assert!(check(&p, "abc").is_err());
        p.merged = false;
        p.merged_at = Some("2026-01-01T00:00:00Z".into());
        assert!(check(&p, "abc").is_err());
    }

    #[test]
    fn missing_sha_is_never_approvable() {
        let mut p = pr();
        p.head.sha.clear();
        assert!(check(&p, "").is_err());
    }

    #[test]
    fn retargeting_or_rewriting_context_invalidates_a_completed_review() {
        let original = pr();
        let mut changed = original.clone();
        assert!(check_snapshot(&changed, &original).is_ok());
        changed.base.ref_name = "release".into();
        assert!(check_snapshot(&changed, &original).is_err());
        changed = original.clone();
        changed.base.sha = "new-base".into();
        assert!(check_snapshot(&changed, &original).is_err());
        changed = original.clone();
        changed.body = Some("Depends on #2".into());
        assert!(check_snapshot(&changed, &original).is_err());
    }
}
