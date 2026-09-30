//! Review depth by risk. Most PRs get the normal model; the ones where a missed
//! defect costs most (server/BFF calls, auth and access control, routes,
//! settlement/payment, very large diffs) get the stronger model. Docs/tests-only
//! PRs get less thinking.
//!
//! Eval basis (23 truth cases, 2026-09-30): opus found +11pp of confirmed
//! defects over sonnet at ~2.5x cost, and the extra finds were in exactly these
//! areas. thinking 12000 vs 4000 cost the same and found a little more.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Risk {
    Low,
    Normal,
    High,
}

impl Risk {
    pub fn label(self) -> &'static str {
        match self {
            Risk::Low => "low",
            Risk::Normal => "normal",
            Risk::High => "high",
        }
    }
}

/// Added lines above which a PR is high risk regardless of paths.
const LARGE_ADDED: usize = 600;

/// Path fragments that mark server-side, auth or money code.
const HIGH_FRAGMENTS: [&str; 14] = [
    "bff", "middleware", "auth", "permission", "guard", "polic", "/api/", "route.ts", "router",
    "settlement", "payment", "security", "access-control", "exclusive",
];

fn is_doc_or_test(path: &str) -> bool {
    let l = path.to_ascii_lowercase();
    l.ends_with(".md")
        || l.ends_with(".mdx")
        || l.starts_with("docs/")
        || l.contains(".test.")
        || l.contains(".spec.")
        || l.contains("/__tests__/")
        || l.contains(".stories.")
        || l.contains("/e2e/")
        || l.ends_with(".snap")
        || l.ends_with("lock.yaml")
        || l.ends_with("package-lock.json")
        || l.ends_with("yarn.lock")
}

/// Risk class and a short reason (shown in the activity log).
pub fn classify(diff: &str) -> (Risk, String) {
    let files = crate::diffprep::added_lines(diff);
    let code: Vec<&(String, Vec<String>)> = files.iter().filter(|(p, _)| !is_doc_or_test(p)).collect();
    if code.is_empty() {
        return (Risk::Low, "문서·테스트만 변경".into());
    }
    let added: usize = files.iter().map(|(_, l)| l.len()).sum();
    if let Some((p, _)) = code
        .iter()
        .find(|(p, _)| HIGH_FRAGMENTS.iter().any(|f| p.to_ascii_lowercase().contains(f)))
    {
        return (Risk::High, format!("서버·권한 경로 {p}"));
    }
    if added > LARGE_ADDED {
        return (Risk::High, format!("추가 {added}줄"));
    }
    (Risk::Normal, format!("추가 {added}줄"))
}

/// Whether this risk class gets the authorization/contract second pass.
pub fn second_pass(cfg: &crate::config::AppConfig, risk: Risk) -> bool {
    cfg.review_second_pass_enabled && risk == Risk::High
}

/// Model and thinking budget for a risk class under the current config.
pub fn pick(cfg: &crate::config::AppConfig, risk: Risk) -> (String, u32) {
    if !cfg.review_routing_enabled {
        return (cfg.review_model.clone(), cfg.review_thinking_tokens);
    }
    match risk {
        Risk::High => (cfg.review_model_high.clone(), cfg.review_thinking_high),
        Risk::Normal => (cfg.review_model.clone(), cfg.review_thinking_tokens),
        Risk::Low => (cfg.review_model.clone(), cfg.review_thinking_low),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diff(files: &[(&str, usize)]) -> String {
        let mut d = String::new();
        for (p, n) in files {
            d.push_str(&format!("diff --git a/{p} b/{p}\n--- a/{p}\n+++ b/{p}\n@@ -1 +1,{n} @@\n"));
            for i in 0..*n {
                d.push_str(&format!("+line {i}\n"));
            }
        }
        d
    }

    #[test]
    fn docs_and_tests_only_is_low() {
        assert_eq!(classify(&diff(&[("README.md", 5), ("src/a.test.ts", 50)])).0, Risk::Low);
    }

    #[test]
    fn server_and_auth_paths_are_high() {
        assert_eq!(classify(&diff(&[("layers/services/products-bff/src/x.ts", 3)])).0, Risk::High);
        assert_eq!(classify(&diff(&[("apps/pis/src/auth/useExclusive.ts", 3)])).0, Risk::High);
        assert_eq!(classify(&diff(&[("app/api/download/route.ts", 3)])).0, Risk::High);
    }

    #[test]
    fn large_diff_is_high_small_ui_is_normal() {
        assert_eq!(classify(&diff(&[("src/ui/Big.tsx", 601)])).0, Risk::High);
        assert_eq!(classify(&diff(&[("src/ui/Small.tsx", 40)])).0, Risk::Normal);
    }

    #[test]
    fn pick_follows_config_and_can_be_turned_off() {
        let mut cfg = crate::config::AppConfig::default();
        assert_eq!(pick(&cfg, Risk::High), ("claude-opus-5-5".to_string(), 12000));
        assert_eq!(pick(&cfg, Risk::Normal), ("claude-sonnet-5-5".to_string(), 12000));
        assert_eq!(pick(&cfg, Risk::Low), ("claude-sonnet-5-5".to_string(), 4000));
        cfg.review_routing_enabled = false;
        assert_eq!(pick(&cfg, Risk::High), ("claude-sonnet-5-5".to_string(), 12000));
    }
}
