//! Value tracing: many real defects sit between two files that each look fine
//! alone — a form sends `'G'` while the mapping table is keyed `'N'`, a query is
//! read under one key and invalidated under another. Before the review, trusted
//! code greps the checked-out repo for every constant and query-key root the PR
//! adds and shows where else they are used, so the model compares both sides.

use std::path::Path;
use std::process::Command;

const MAX_NAMES: usize = 16;
/// More uses than this = too generic to trace line by line (a shared flag key).
const TOO_COMMON: usize = 30;
/// Name parts that mark a mapping or code table — the usual home of value mismatches.
const MAPPING_HINTS: [&str; 9] = ["_TO_", "MAP", "_BY_", "CODE", "OPTION", "KEY", "STATUS", "TYPE", "LABEL"];
const MAX_HITS_PER_NAME: usize = 8;
const MAX_LINE: usize = 140;
const BLOCK_CAP: usize = 12_000;

/// Names worth tracing from the PR's added lines: UPPER_SNAKE constants it
/// defines or uses (mapping-like names first), and the first string of each
/// query key it reads or invalidates.
pub fn names(added: &[(String, Vec<String>)]) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let mut push = |n: String| {
        if n.len() >= 3 && !found.contains(&n) {
            found.push(n);
        }
    };
    for (path, lines) in added {
        if path.ends_with(".md") || path.contains(".test.") || path.contains(".spec.") {
            continue;
        }
        for l in lines {
            if let Some(name) = const_name(l) {
                push(name);
            }
            if let Some(key) = query_key_root(l) {
                push(key);
            }
            for ident in upper_idents(l) {
                push(ident);
            }
        }
    }
    let is_mapping = |n: &String| MAPPING_HINTS.iter().any(|h| n.contains(h)) || n.contains('-');
    let (mut first, rest): (Vec<String>, Vec<String>) = found.into_iter().partition(is_mapping);
    first.extend(rest);
    first.truncate(MAX_NAMES);
    first
}

/// UPPER_SNAKE identifiers (with at least one `_`, 6+ chars) used on a line.
fn upper_idents(line: &str) -> Vec<String> {
    line.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|w| {
            w.len() >= 6
                && w.contains('_')
                && w.chars().next().is_some_and(|c| c.is_ascii_uppercase())
                && w.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        })
        .map(str::to_string)
        .collect()
}

/// `const FOO_BAR =` / `export const FOO_BAR:` → `FOO_BAR` (UPPER_SNAKE only).
fn const_name(line: &str) -> Option<String> {
    let rest = line.trim_start().trim_start_matches("export ").trim_start();
    let rest = rest.strip_prefix("const ")?;
    let name: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
    let upper = name.chars().any(|c| c.is_ascii_uppercase())
        && name.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    (upper && name.contains('_')).then_some(name)
}

/// `queryKey: ['partner-agreement', ...]` → `partner-agreement`.
fn query_key_root(line: &str) -> Option<String> {
    let i = line.find("queryKey")?;
    let after = &line[i..];
    let open = after.find('[')?;
    let inner = after[open + 1..].trim_start();
    let quote = inner.chars().next().filter(|c| matches!(c, '\'' | '"' | '`'))?;
    let body = &inner[1..];
    let end = body.find(quote)?;
    let key = &body[..end];
    (!key.is_empty() && !key.contains('$')).then(|| key.to_string())
}

/// `git grep` each name in the checked-out tree and render the block.
pub fn build(repo_dir: &Path, added: &[(String, Vec<String>)]) -> String {
    let names = names(added);
    if names.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "=== 값 추적 (신뢰 코드가 PR head 에서 grep 한 결과 — 출처와 사용처를 대조할 것) ===\n\
         PR 이 새로 정의하거나 쓰는 상수·쿼리 키가 레포 어디서 쓰이는지다. 값을 만드는 쪽(폼 옵션, 응답, 조회 키)과 \
         받는 쪽(변환표, 분기, 무효화 키)의 값·키가 실제로 일치하는지 양쪽을 열어 확인한다.\n",
    );
    for n in &names {
        let hits = grep(repo_dir, n);
        if hits.len() > TOO_COMMON {
            out.push_str(&format!("\n[{n}] {}곳 — 너무 흔해 목록 생략\n", hits.len()));
            continue;
        }
        out.push_str(&format!("\n[{n}] {}곳\n", hits.len()));
        for h in hits.iter().take(MAX_HITS_PER_NAME) {
            out.push_str(&format!("  {h}\n"));
        }
        if hits.len() > MAX_HITS_PER_NAME {
            out.push_str(&format!("  …(외 {}곳)\n", hits.len() - MAX_HITS_PER_NAME));
        }
        if out.len() > BLOCK_CAP {
            out.push_str("\n(이후 생략)\n");
            break;
        }
    }
    out
}

fn grep(repo_dir: &Path, needle: &str) -> Vec<String> {
    let out = Command::new("git")
        .args(["grep", "-n", "-I", "-F", "-e", needle, "--", ".", ":(exclude)*.lock", ":(exclude)pnpm-lock.yaml"])
        .current_dir(repo_dir)
        .output();
    let Ok(out) = out else { return vec![] };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| {
            let l = l.trim();
            if l.len() > MAX_LINE {
                let mut cut = MAX_LINE;
                while !l.is_char_boundary(cut) {
                    cut -= 1;
                }
                format!("{}…", &l[..cut])
            } else {
                l.to_string()
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_upper_snake_constants() {
        assert_eq!(const_name("export const GENDER_CODE_TO_UDH: Record<string, string> = {"), Some("GENDER_CODE_TO_UDH".into()));
        assert_eq!(const_name("  const MAX_RETRY = 3;"), Some("MAX_RETRY".into()));
        assert_eq!(const_name("const camelCase = 1"), None);
        assert_eq!(const_name("const FOO = 1"), None, "single word is too generic");
    }

    #[test]
    fn picks_query_key_roots() {
        assert_eq!(query_key_root("  queryKey: ['partner-agreement', 'list', id],"), Some("partner-agreement".into()));
        assert_eq!(
            query_key_root("qc.invalidateQueries({ queryKey: [\"coupon-detail\"] })"),
            Some("coupon-detail".into())
        );
        assert_eq!(query_key_root("queryKey: [`x-${id}`]"), None);
        assert_eq!(query_key_root("queryKey: keys.all"), None);
    }

    #[test]
    fn used_mapping_tables_come_first() {
        let added = vec![(
            "src/a.ts".to_string(),
            vec![
                "const x = MAX_RETRY_COUNT + 1;".to_string(),
                "['gender', Object.values(GENDER_CODE_TO_UDH)],".to_string(),
            ],
        )];
        assert_eq!(names(&added), vec!["GENDER_CODE_TO_UDH".to_string(), "MAX_RETRY_COUNT".to_string()]);
    }

    #[test]
    fn names_skip_tests_and_dedupe() {
        let added = vec![
            ("src/a.ts".to_string(), vec!["export const DELIVERY_TO_UDH = {".to_string(), "queryKey: ['p-list']".to_string()]),
            ("src/b.ts".to_string(), vec!["queryKey: ['p-list', 2]".to_string()]),
            ("src/a.test.ts".to_string(), vec!["const TEST_ONLY_X = 1".to_string()]),
        ];
        assert_eq!(names(&added), vec!["DELIVERY_TO_UDH".to_string(), "p-list".to_string()]);
    }

    #[test]
    fn build_greps_a_real_checkout() {
        // This repo itself is a git checkout: trace a constant defined in review.rs.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let added = vec![("x.rs".to_string(), vec!["const INLINE_MIN_CONFIDENCE: f64 = 70.0;".to_string()])];
        let block = build(root, &added);
        assert!(block.contains("[INLINE_MIN_CONFIDENCE]"), "{block}");
        assert!(block.contains("src-tauri/src/review.rs:"), "{block}");
    }
}
