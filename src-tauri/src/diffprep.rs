//! Turns a raw unified diff into what the reviewer actually reads.
//!
//! - Every right-side line gets its new-file line number, so inline comments
//!   point at real lines instead of the model counting hunks by hand (wrong
//!   numbers were silently dropped before).
//! - Lockfiles, generated code, snapshots and binaries are listed but not sent.
//! - The budget is spent per file, source first, so a big PR loses whole
//!   low-priority files instead of being cut mid-hunk. Files left out are
//!   named, and the caller must not auto-approve when any were left out.

/// Why a file's hunks are not in the prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Skip {
    /// Not worth line review (lockfile, generated, snapshot, binary).
    Excluded(&'static str),
    /// Did not fit the budget. The review did not see this file.
    OverBudget,
}

#[derive(Debug, Clone)]
pub struct FileDiff {
    pub path: String,
    pub added: usize,
    pub deleted: usize,
    /// Raw `diff --git` block for this file.
    raw: String,
    pub skip: Option<Skip>,
}

#[derive(Debug, Clone)]
pub struct Prepared {
    pub files: Vec<FileDiff>,
}

impl Prepared {
    /// Files the review could not see because of the budget.
    pub fn omitted(&self) -> Vec<String> {
        self.files
            .iter()
            .filter(|f| f.skip == Some(Skip::OverBudget))
            .map(|f| f.path.clone())
            .collect()
    }

    /// File table + line-numbered hunks, ready for the prompt.
    pub fn render(&self) -> String {
        let mut out = String::from("[변경 파일] (+추가 -삭제, 표시 없으면 아래 diff 에 포함)\n");
        for f in &self.files {
            let note = match &f.skip {
                None => String::new(),
                Some(Skip::Excluded(why)) => format!("  — 제외: {why}"),
                Some(Skip::OverBudget) => "  — ⚠️ 생략: 예산 초과, 이 리뷰에서 보지 못함".to_string(),
            };
            out.push_str(&format!("- {} (+{} -{}){note}\n", f.path, f.added, f.deleted));
        }
        out.push_str(
            "\n각 줄 앞의 숫자는 새 파일 기준 줄번호다. 인라인 코멘트의 line 에는 이 숫자를 그대로 쓴다. \
             삭제된 줄(-)에는 번호가 없고 코멘트를 달 수 없다.\n",
        );
        for f in self.files.iter().filter(|f| f.skip.is_none()) {
            out.push('\n');
            out.push_str(&annotate(&f.raw));
        }
        out
    }
}

/// Budget for the rendered hunks, in bytes. The prompt goes over stdin, so this
/// is about keeping the model focused, not the OS argv limit.
pub const DIFF_BUDGET: usize = 300_000;

pub fn prepare(diff: &str, budget: usize) -> Prepared {
    let mut files: Vec<FileDiff> = split_files(diff);
    for f in &mut files {
        f.skip = excluded_reason(&f.path, &f.raw).map(Skip::Excluded);
    }
    // Spend the budget in priority order, but keep the original order for display.
    let mut order: Vec<usize> = (0..files.len()).filter(|&i| files[i].skip.is_none()).collect();
    order.sort_by_key(|&i| priority(&files[i].path));
    let mut used = 0usize;
    for i in order {
        let cost = files[i].raw.len() + files[i].raw.lines().count() * 7; // + line-number prefix
        if used + cost > budget {
            files[i].skip = Some(Skip::OverBudget);
        } else {
            used += cost;
        }
    }
    Prepared { files }
}

/// Paths the PR touches, in diff order.
pub fn changed_paths(diff: &str) -> Vec<String> {
    split_files(diff).into_iter().map(|f| f.path).collect()
}

fn split_files(diff: &str) -> Vec<FileDiff> {
    let mut out: Vec<FileDiff> = Vec::new();
    for line in diff.split_inclusive('\n') {
        if line.starts_with("diff --git ") {
            let path = line
                .trim_end()
                .rsplit(" b/")
                .next()
                .unwrap_or("")
                .to_string();
            out.push(FileDiff { path, added: 0, deleted: 0, raw: String::new(), skip: None });
        }
        let Some(f) = out.last_mut() else { continue };
        f.raw.push_str(line);
        if line.starts_with('+') && !line.starts_with("+++") {
            f.added += 1;
        } else if line.starts_with('-') && !line.starts_with("---") {
            f.deleted += 1;
        }
    }
    out
}

fn excluded_reason(path: &str, raw: &str) -> Option<&'static str> {
    let name = path.rsplit('/').next().unwrap_or(path);
    if matches!(name, "pnpm-lock.yaml" | "package-lock.json" | "yarn.lock" | "Cargo.lock" | "bun.lockb") {
        return Some("lockfile");
    }
    if path.contains("__snapshots__/") || name.ends_with(".snap") {
        return Some("스냅샷");
    }
    if name.contains(".gen.")
        || name.ends_with(".min.js")
        || path.starts_with("dist/")
        || path.contains("/dist/")
        || path.contains("/.next/")
    {
        return Some("생성물");
    }
    if raw.contains("\nBinary files ") || raw.contains("\nGIT binary patch") {
        return Some("바이너리");
    }
    None
}

/// Lower = sent first. Code before tests before docs/config.
fn priority(path: &str) -> u8 {
    let lower = path.to_ascii_lowercase();
    let is_test = lower.contains(".test.")
        || lower.contains(".spec.")
        || lower.contains("/__tests__/")
        || lower.contains("/e2e/")
        || lower.contains(".stories.");
    let is_doc = lower.ends_with(".md") || lower.ends_with(".mdx") || lower.starts_with("docs/");
    if is_doc {
        2
    } else if is_test {
        1
    } else {
        0
    }
}

/// Prefix right-side lines with their new-file number: `+  42│code`.
fn annotate(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + raw.len() / 4);
    let mut right: u64 = 0;
    let mut in_hunk = false;
    for line in raw.split_inclusive('\n') {
        if let Some(h) = line.strip_prefix("@@ ") {
            right = h
                .split('+')
                .nth(1)
                .and_then(|s| s.split([',', ' ']).next())
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            in_hunk = true;
            out.push_str(line);
            continue;
        }
        if !in_hunk || line.starts_with("+++") || line.starts_with("---") {
            out.push_str(line);
            continue;
        }
        match line.as_bytes().first() {
            Some(b'+') => {
                out.push_str(&format!("+{right:>5}│{}", &line[1..]));
                right += 1;
            }
            Some(b' ') => {
                out.push_str(&format!(" {right:>5}│{}", &line[1..]));
                right += 1;
            }
            Some(b'-') => out.push_str(&format!("-{:>5}│{}", "", &line[1..])),
            _ => out.push_str(line),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIFF: &str = "diff --git a/src/a.ts b/src/a.ts\n\
index 1..2 100644\n\
--- a/src/a.ts\n\
+++ b/src/a.ts\n\
@@ -10,3 +10,4 @@ fn\n\
\x20keep\n\
-old\n\
+new1\n\
+new2\n\
\x20tail\n\
diff --git a/pnpm-lock.yaml b/pnpm-lock.yaml\n\
--- a/pnpm-lock.yaml\n\
+++ b/pnpm-lock.yaml\n\
@@ -1 +1 @@\n\
-a\n\
+b\n\
diff --git a/src/a.test.ts b/src/a.test.ts\n\
--- a/src/a.test.ts\n\
+++ b/src/a.test.ts\n\
@@ -1 +1,2 @@\n\
\x20x\n\
+y\n";

    #[test]
    fn numbers_right_side_lines() {
        let p = prepare(DIFF, DIFF_BUDGET);
        let r = p.render();
        assert!(r.contains(" 10│keep"), "{r}");
        assert!(r.contains("-     │old"));
        assert!(r.contains("+   11│new1"));
        assert!(r.contains("+   12│new2"));
        assert!(r.contains("    13│tail"));
    }

    #[test]
    fn excludes_lockfile_but_lists_it() {
        let p = prepare(DIFF, DIFF_BUDGET);
        let lock = p.files.iter().find(|f| f.path == "pnpm-lock.yaml").unwrap();
        assert_eq!(lock.skip, Some(Skip::Excluded("lockfile")));
        let r = p.render();
        assert!(r.contains("pnpm-lock.yaml (+1 -1)  — 제외: lockfile"));
        assert!(!r.contains("+    1│b"), "lockfile hunks must not be sent");
        assert!(p.omitted().is_empty());
    }

    #[test]
    fn budget_drops_tests_before_source_and_reports_them() {
        let src = split_files(DIFF).into_iter().find(|f| f.path == "src/a.ts").unwrap();
        let budget = src.raw.len() + src.raw.lines().count() * 7;
        let p = prepare(DIFF, budget);
        assert_eq!(p.omitted(), vec!["src/a.test.ts".to_string()]);
        assert!(p.render().contains("src/a.test.ts (+1 -0)  — ⚠️ 생략"));
    }

    #[test]
    fn counts_added_and_deleted() {
        let p = prepare(DIFF, DIFF_BUDGET);
        let a = p.files.iter().find(|f| f.path == "src/a.ts").unwrap();
        assert_eq!((a.added, a.deleted), (2, 1));
    }
}
