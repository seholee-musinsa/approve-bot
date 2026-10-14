#[cfg(not(test))]
fn main() {
    println!("cargo:rerun-if-changed=src");
    for path in ["review-guide.md", "Cargo.toml", "Cargo.lock", "build.rs", "tauri.conf.json", "capabilities"] {
        println!("cargo:rerun-if-changed={path}");
    }
    for name in ["HEAD", "refs/heads", "packed-refs"] {
        if let Ok(out) = std::process::Command::new("git").args(["rev-parse", "--git-path", name]).output() {
            if out.status.success() { println!("cargo:rerun-if-changed={}", String::from_utf8_lossy(&out.stdout).trim()); }
        }
    }
    println!("cargo:rustc-env=APPROVE_BOT_BUILD_SHA={}", build_identity(std::path::Path::new(".")));
    tauri_build::build()
}

fn build_identity(root: &std::path::Path) -> String {
    let sha = std::process::Command::new("git").args(["rev-parse", "HEAD"]).current_dir(root).output()
        .ok().filter(|o|o.status.success()).map(|o|String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_else(||"unknown".into());
    format!("{sha}-src-{}", source_fingerprint(root))
}

/// Stable content identity, independent of mtimes and directory enumeration.
/// FNV-1a is an audit fingerprint, not a cryptographic integrity assertion.
fn source_fingerprint(root: &std::path::Path) -> String {
    fn collect(root: &std::path::Path, path: &std::path::Path, files: &mut Vec<std::path::PathBuf>) {
        if path.is_dir() {
            for entry in std::fs::read_dir(path).expect("read build inputs") {
                collect(root, &entry.expect("read build input entry").path(), files);
            }
        } else {
            files.push(path.strip_prefix(root).expect("relative build input").to_path_buf());
        }
    }
    let mut files = vec![];
    for path in ["src", "review-guide.md", "Cargo.toml", "Cargo.lock", "build.rs"] {
        collect(root, &root.join(path), &mut files);
    }
    for path in ["tauri.conf.json", "capabilities"] {
        if root.join(path).exists() { collect(root, &root.join(path), &mut files); }
    }
    files.sort();
    let mut hash = 0xcbf29ce484222325u64;
    for relative in files {
        let name = relative.to_string_lossy();
        let content = std::fs::read(root.join(&relative)).expect("read build input contents");
        for bytes in [name.as_bytes(), content.as_slice()] {
            for byte in (bytes.len() as u64).to_le_bytes().iter().chain(bytes) {
                hash ^= u64::from(*byte);
                hash = hash.wrapping_mul(0x100000001b3);
            }
        }
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn build_identity_tracks_content_and_new_modules_not_only_git_dirty_state() {
        let root = std::env::temp_dir().join(format!("approve-build-test-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(root.join("src")).unwrap();
        for name in ["Cargo.toml", "Cargo.lock", "build.rs", "review-guide.md", "src/main.rs"] {
            std::fs::write(root.join(name), "initial").unwrap();
        }
        let first = build_identity(&root);
        assert_eq!(first, build_identity(&root), "same inputs must be reproducible");
        for name in ["src/main.rs", "Cargo.toml", "Cargo.lock", "build.rs", "review-guide.md"] {
            std::fs::write(root.join(name), "changed").unwrap();
            assert_ne!(first, build_identity(&root), "changed {name} must identify a different build");
            std::fs::write(root.join(name), "initial").unwrap();
            assert_eq!(first, build_identity(&root));
        }
        std::fs::write(root.join("src/new_module.rs"), "new module").unwrap();
        assert_ne!(first, build_identity(&root), "new source files change the fingerprint");
        std::fs::remove_file(root.join("src/new_module.rs")).unwrap();
        assert_eq!(first, build_identity(&root));
        std::fs::remove_dir_all(root).unwrap();
    }
}
