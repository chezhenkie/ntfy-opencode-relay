//! Project root policy for topic edits.
//!
//! A topic is a project folder name; the served set is the folders under
//! `projectRoots`. The rules here keep that list canonical, non-overlapping,
//! and comparable across the platform's path rules.

use crate::types::ErrorCode;
use ntfy_relay_core::config::expand_home;
use std::ffi::OsStr;
use std::fs;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyError {
    Empty,
    NotAbsolute,
    ParentDir,
    Missing,
    NotDirectory,
    Covered,
    Duplicate,
    NotServed,
}

impl PolicyError {
    pub fn code(self) -> ErrorCode {
        match self {
            Self::Empty | Self::NotAbsolute | Self::ParentDir | Self::NotDirectory => {
                ErrorCode::Invalid
            }
            Self::Missing => ErrorCode::NotFound,
            Self::Covered => ErrorCode::Covered,
            Self::Duplicate | Self::NotServed => ErrorCode::Conflict,
        }
    }

    pub fn message(self) -> &'static str {
        match self {
            Self::Empty => "path is empty",
            Self::NotAbsolute => "path must be absolute",
            Self::ParentDir => "path must not contain ..",
            Self::Missing => "path does not exist",
            Self::NotDirectory => "path is not a directory",
            Self::Covered => "path is inside a served root",
            Self::Duplicate => "path is already served",
            Self::NotServed => "path is not a served root",
        }
    }
}

/// Expand `~`, require an absolute directory, and return its canonical form.
pub fn resolve(input: &str, home: &Path) -> Result<PathBuf, PolicyError> {
    if input.trim().is_empty() {
        return Err(PolicyError::Empty);
    }
    let expanded = expand_home(input, home);
    if !expanded.is_absolute() {
        return Err(PolicyError::NotAbsolute);
    }
    if expanded
        .components()
        .any(|part| matches!(part, Component::ParentDir))
    {
        return Err(PolicyError::ParentDir);
    }
    let meta = fs::metadata(&expanded).map_err(|_| PolicyError::Missing)?;
    if !meta.is_dir() {
        return Err(PolicyError::NotDirectory);
    }
    let canonical = fs::canonicalize(&expanded).map_err(|_| PolicyError::Missing)?;
    Ok(strip_verbatim(canonical))
}

/// Removal accepts a folder that no longer exists, so a stale root can be
/// dropped without hand-editing the config. An existing folder is still
/// canonicalized; a missing one keeps its expanded form.
pub fn resolve_for_removal(input: &str, home: &Path) -> Result<PathBuf, PolicyError> {
    if input.trim().is_empty() {
        return Err(PolicyError::Empty);
    }
    let expanded = expand_home(input, home);
    if !expanded.is_absolute() {
        return Err(PolicyError::NotAbsolute);
    }
    if expanded
        .components()
        .any(|part| matches!(part, Component::ParentDir))
    {
        return Err(PolicyError::ParentDir);
    }
    match fs::canonicalize(&expanded) {
        Ok(canonical) => Ok(strip_verbatim(canonical)),
        Err(_) => Ok(expanded),
    }
}

/// True when `path` is `root` or sits below it.
pub fn contains(root: &Path, path: &Path) -> bool {
    components_prefix(root, path)
}

/// Add one root, dropping entries it already covers.
pub fn add(roots: &[PathBuf], new: PathBuf) -> Result<Vec<PathBuf>, PolicyError> {
    if roots.iter().any(|root| same(root, &new)) {
        return Err(PolicyError::Duplicate);
    }
    if roots.iter().any(|root| contains(root, &new)) {
        return Err(PolicyError::Covered);
    }
    let mut next: Vec<PathBuf> = roots
        .iter()
        .filter(|root| !contains(&new, root))
        .cloned()
        .collect();
    next.push(new);
    Ok(next)
}

/// Remove one exact entry.
pub fn remove(roots: &[PathBuf], target: &Path) -> Result<Vec<PathBuf>, PolicyError> {
    let mut next = Vec::with_capacity(roots.len());
    let mut found = false;
    for root in roots {
        if same(root, target) {
            found = true;
        } else {
            next.push(root.clone());
        }
    }
    if found {
        Ok(next)
    } else {
        Err(PolicyError::NotServed)
    }
}

fn same(left: &Path, right: &Path) -> bool {
    components_prefix(left, right) && components_prefix(right, left)
}

fn components_prefix(prefix: &Path, path: &Path) -> bool {
    let mut left = prefix.components();
    let mut right = path.components();
    loop {
        match (left.next(), right.next()) {
            (None, _) => return true,
            (Some(a), Some(b)) if same_component(a.as_os_str(), b.as_os_str()) => {}
            _ => return false,
        }
    }
}

fn same_component(left: &OsStr, right: &OsStr) -> bool {
    #[cfg(windows)]
    {
        left.to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy())
    }
    #[cfg(not(windows))]
    {
        left == right
    }
}

/// Windows canonicalize returns a verbatim path; drop the prefix for display.
fn strip_verbatim(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    if let Some(rest) = path.to_string_lossy().strip_prefix(r"\\?\") {
        return match rest.strip_prefix(r"UNC\") {
            Some(unc) => PathBuf::from(format!(r"\\{unc}")),
            None => PathBuf::from(rest),
        };
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    fn dir(tag: &str) -> TempDir {
        TempDir::new(tag)
    }

    #[test]
    fn resolve_rejects_empty_and_relative() {
        let home = dir("home");
        assert_eq!(resolve("", home.path()), Err(PolicyError::Empty));
        assert_eq!(
            resolve("project", home.path()),
            Err(PolicyError::NotAbsolute)
        );
    }

    #[test]
    fn resolve_expands_home_and_needs_a_directory() {
        let home = dir("home");
        let work = home.path().join("work");
        fs::create_dir_all(&work).expect("work dir");
        let resolved = resolve("~/work", home.path()).expect("resolved");
        assert!(resolved.is_absolute());
        assert_eq!(resolved.file_name(), Some(OsStr::new("work")));

        let file = home.path().join("note.txt");
        fs::write(&file, b"x").expect("file");
        let absolute = file.to_string_lossy().to_string();
        assert_eq!(
            resolve(&absolute, home.path()),
            Err(PolicyError::NotDirectory)
        );
        let missing = home.path().join("missing").to_string_lossy().to_string();
        assert_eq!(resolve(&missing, home.path()), Err(PolicyError::Missing));
    }

    #[test]
    fn resolve_rejects_parent_segments() {
        let home = dir("home");
        let text = format!("{}/..", home.path().to_string_lossy());
        assert_eq!(resolve(&text, home.path()), Err(PolicyError::ParentDir));
    }

    #[test]
    fn add_rejects_duplicates_and_covered_paths() {
        let home = dir("home");
        let root = home.path().join("root");
        let inner = root.join("inner");
        fs::create_dir_all(&inner).expect("dirs");

        assert_eq!(
            add(std::slice::from_ref(&root), root.clone()),
            Err(PolicyError::Duplicate)
        );
        assert_eq!(
            add(std::slice::from_ref(&root), inner.clone()),
            Err(PolicyError::Covered)
        );
    }

    #[test]
    fn add_drops_roots_the_new_entry_covers() {
        let home = dir("home");
        let root = home.path().join("root");
        let inner = root.join("inner");
        fs::create_dir_all(&inner).expect("dirs");

        let next = add(std::slice::from_ref(&inner), root.clone()).expect("added");
        assert_eq!(next.len(), 1);
        assert!(same(&next[0], &root));
    }

    #[test]
    fn remove_needs_an_exact_entry() {
        let home = dir("home");
        let root = home.path().join("root");
        let inner = root.join("inner");
        fs::create_dir_all(&inner).expect("dirs");

        assert_eq!(
            remove(std::slice::from_ref(&root), &inner),
            Err(PolicyError::NotServed)
        );
        let next = remove(std::slice::from_ref(&root), &root).expect("removed");
        assert!(next.is_empty());
    }

    #[test]
    fn removal_accepts_a_folder_that_is_gone() {
        let home = dir("home-removal");
        let gone = home.path().join("gone");
        let text = gone.to_string_lossy().to_string();
        assert_eq!(resolve_for_removal(&text, home.path()), Ok(gone.clone()));
        assert_eq!(remove(std::slice::from_ref(&gone), &gone), Ok(Vec::new()));
        assert_eq!(
            resolve_for_removal("relative", home.path()),
            Err(PolicyError::NotAbsolute)
        );
    }

    #[test]
    fn containment_uses_component_boundaries() {
        let home = dir("home");
        let root = home.path().join("root");
        let sibling = home.path().join("rootless");
        fs::create_dir_all(&root).expect("root");
        fs::create_dir_all(&sibling).expect("sibling");
        assert!(contains(&root, &root.join("inner")));
        assert!(!contains(&root, &sibling));
    }

    #[cfg(windows)]
    #[test]
    fn windows_compare_ignores_case() {
        let home = dir("home");
        let root = home.path().join("Root");
        fs::create_dir_all(&root).expect("root");
        let upper = PathBuf::from(root.to_string_lossy().to_uppercase());
        assert!(same(&root, &upper));
    }
}
