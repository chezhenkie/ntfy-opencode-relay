//! Project scope and path comparison.
//!
//! Outbound publishes only for sessions whose project canonical sits inside a
//! configured root. File paths are canonicalized before containment so `..`
//! and symlinks cannot escape. Windows comparisons fold case and separators.

use std::path::{Path, PathBuf};

use ntfy_relay_core::topic::is_windows_path;

/// Comparison form: forward slashes, no trailing slash, lowercased for
/// Windows-style paths.
pub fn normalize_for_compare(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let mut value = raw.replace('\\', "/");
    while value.len() > 1 && value.ends_with('/') {
        value.pop();
    }
    if is_windows_path(&raw) {
        value.make_ascii_lowercase();
    }
    value
}

pub fn canonical(path: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(path).ok()
}

/// True when `candidate` is the root or sits under it.
pub fn is_inside(root: &Path, candidate: &Path) -> bool {
    let root = normalize_for_compare(root);
    let candidate = normalize_for_compare(candidate);
    candidate == root || candidate.starts_with(&format!("{root}/"))
}

pub fn within_roots(path: &Path, roots: &[PathBuf]) -> bool {
    let Some(path) = canonical(path) else {
        return lexical_within_roots(path, roots);
    };
    roots.iter().any(|root| {
        canonical(root)
            .map(|root| is_inside(&root, &path))
            .unwrap_or_else(|| is_inside(root, &path))
    })
}

fn lexical_within_roots(path: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| is_inside(root, path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("temp")
            .join("outbound-scope-tests")
            .join(name);
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn containment_is_separator_and_case_insensitive_on_windows() {
        assert!(is_inside(
            Path::new("C:\\Work\\Proj"),
            Path::new("c:/work/proj/file.txt")
        ));
        assert!(is_inside(
            Path::new("/home/me/proj"),
            Path::new("/home/me/proj/sub/file")
        ));
        assert!(!is_inside(
            Path::new("/home/me/proj"),
            Path::new("/home/me/proj-other/file")
        ));
        assert!(!is_inside(
            Path::new("/home/me/proj"),
            Path::new("/home/me")
        ));
    }

    #[test]
    fn roots_accept_a_child_and_reject_a_sibling() {
        let root = temp_root("roots");
        let child = root.join("proj");
        std::fs::create_dir_all(&child).unwrap();
        let sibling = root.join("other");
        std::fs::create_dir_all(&sibling).unwrap();
        assert!(within_roots(&child, std::slice::from_ref(&root)));
        assert!(!within_roots(&sibling, &[root.join("proj")]));
    }
}
