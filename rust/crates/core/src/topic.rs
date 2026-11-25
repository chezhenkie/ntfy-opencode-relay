//! Topic derivation from a project canonical path.
//!
//! Mirrors `ntfy-topic.mjs`: the folder name is the topic, ASCII only, 1 to 64
//! characters. Windows and POSIX paths are parsed on any host because project
//! metadata can carry either form.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const TOPIC_MAX_LEN: usize = 64;

pub fn is_windows_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    (bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && is_windows_sep(bytes[2]))
        || path.contains('\\')
}

pub fn project_folder(canonical: &str) -> Option<&str> {
    let value = trim_js(canonical);
    if value.is_empty() {
        return None;
    }

    let windows = is_windows_path(value);
    let root_len = if windows {
        windows_root_len(value)
    } else {
        posix_root_len(value)
    };

    let mut end = value.len();
    while end > root_len {
        let last = value.as_bytes()[end - 1];
        let separator = if windows {
            is_windows_sep(last)
        } else {
            last == b'/'
        };
        if !separator {
            break;
        }
        end -= 1;
    }

    let trimmed = &value[..end];
    let start = if windows {
        trimmed
            .rfind(['/', '\\'])
            .map(|index| index + 1)
            .unwrap_or(0)
    } else {
        trimmed.rfind('/').map(|index| index + 1).unwrap_or(0)
    };

    let name = &trimmed[start..];
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

pub fn topic_for_project(canonical: &str) -> Option<&str> {
    let name = project_folder(canonical)?;
    if name.len() > TOPIC_MAX_LEN {
        return None;
    }
    if name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        Some(name)
    } else {
        None
    }
}

/// Comparison form for a canonical path: separators unified, Windows paths
/// lowercased, no trailing separator. Mirrors the outbound scope comparison so
/// one canonical has one key everywhere.
pub fn canonical_key(path: &Path) -> String {
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

/// Resolves a path for comparison: the filesystem form when it exists, else a
/// lexical `..`/`.` collapse. Discovery can hand over unnormalized paths.
fn resolve(path: &Path) -> PathBuf {
    if let Ok(resolved) = std::fs::canonicalize(path) {
        return resolved;
    }
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The canonical that owns each topic. Discovery fills it; inbound routing and
/// outbound both read it, so two folders with the same basename cannot both
/// route or publish to that topic.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TopicOwners {
    owners: BTreeMap<String, PathBuf>,
}

impl TopicOwners {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, topic: &str, canonical: PathBuf) {
        self.owners.insert(topic.to_string(), canonical);
    }

    pub fn owner(&self, topic: &str) -> Option<&Path> {
        self.owners.get(topic).map(PathBuf::as_path)
    }

    /// True only for the canonical that won the topic. An empty map owns
    /// nothing, so a caller with no discovery result fails closed.
    pub fn owns(&self, topic: &str, canonical: &Path) -> bool {
        match self.owner(topic) {
            Some(owner) => canonical_key(&resolve(owner)) == canonical_key(&resolve(canonical)),
            None => false,
        }
    }

    pub fn len(&self) -> usize {
        self.owners.len()
    }

    pub fn is_empty(&self) -> bool {
        self.owners.is_empty()
    }

    pub fn topics(&self) -> Vec<String> {
        self.owners.keys().cloned().collect()
    }
}

/// The live `projectRoots` list with a change counter.
///
/// Discovery replaces it after a control change, outbound reads it, and the
/// version tells a cache holder that its scope is stale. `set` bumps the
/// version only when the list really differs, so a repeated discovery pass
/// does not invalidate anything.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectRoots {
    roots: Vec<PathBuf>,
    version: u64,
}

impl ProjectRoots {
    pub fn new(roots: Vec<PathBuf>) -> Self {
        Self { roots, version: 0 }
    }

    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    /// Replaces the list and returns true when it changed.
    pub fn set(&mut self, roots: Vec<PathBuf>) -> bool {
        if self.roots == roots {
            return false;
        }
        self.roots = roots;
        self.version = self.version.wrapping_add(1);
        true
    }
}

fn is_windows_sep(byte: u8) -> bool {
    byte == b'/' || byte == b'\\'
}

fn posix_root_len(value: &str) -> usize {
    usize::from(value.as_bytes().first() == Some(&b'/'))
}

fn windows_root_len(value: &str) -> usize {
    let bytes = value.as_bytes();
    if bytes.len() >= 2 && is_windows_sep(bytes[0]) && is_windows_sep(bytes[1]) {
        let mut index = 2;
        while index < bytes.len() && !is_windows_sep(bytes[index]) {
            index += 1;
        }
        if index == 2 {
            return 2;
        }
        if index >= bytes.len() {
            return bytes.len();
        }
        index += 1;
        let share_start = index;
        while index < bytes.len() && !is_windows_sep(bytes[index]) {
            index += 1;
        }
        if index == share_start {
            return share_start;
        }
        if index < bytes.len() {
            index += 1;
        }
        return index;
    }
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && is_windows_sep(bytes[2])
    {
        return 3;
    }
    0
}

fn trim_js(value: &str) -> &str {
    value.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_path_detection() {
        assert!(is_windows_path(r"C:\Users\me"));
        assert!(is_windows_path("C:/Users/me"));
        assert!(is_windows_path(r"\\server\share"));
        assert!(!is_windows_path("/home/me"));
    }

    #[test]
    fn windows_and_posix() {
        assert_eq!(
            topic_for_project(r"C:\Users\me\ocdev\ntfy-proj"),
            Some("ntfy-proj")
        );
        assert_eq!(
            topic_for_project("/home/me/ocdev/ntfy-proj"),
            Some("ntfy-proj")
        );
    }

    #[test]
    fn trailing_separators() {
        assert_eq!(
            topic_for_project(r"C:\Users\me\ocdev\ntfy-proj\\"),
            Some("ntfy-proj")
        );
        assert_eq!(
            topic_for_project("/home/me/ocdev/ntfy-proj/"),
            Some("ntfy-proj")
        );
        assert_eq!(
            topic_for_project("/home/me/ocdev/ntfy-proj///"),
            Some("ntfy-proj")
        );
    }

    #[test]
    fn invalid_folder_names_are_rejected() {
        assert_eq!(topic_for_project(r"C:\Users\me\ocdev\My App"), None);
        assert_eq!(topic_for_project("/home/me/Caf\u{e9}"), None);
        assert_eq!(topic_for_project("/home/me/Projekt \u{dc}ber"), None);
    }

    #[test]
    fn folder_name_length_limit_is_64() {
        let name = "x".repeat(64);
        let path = format!("/home/me/{name}");
        assert_eq!(topic_for_project(&path), Some(name.as_str()));

        let too_long = "x".repeat(65);
        let path = format!("/home/me/{too_long}");
        assert_eq!(topic_for_project(&path), None);
    }

    #[test]
    fn empty_and_root_paths() {
        assert_eq!(topic_for_project(""), None);
        assert_eq!(topic_for_project("/"), None);
        assert_eq!(topic_for_project("C:\\"), None);
    }

    #[test]
    fn duplicate_basenames_collide_by_design() {
        assert_eq!(
            topic_for_project(r"C:\one\proj"),
            topic_for_project(r"D:\two\proj")
        );
    }

    #[test]
    fn a_topic_has_exactly_one_owner() {
        let mut owners = TopicOwners::new();
        owners.insert("proj", PathBuf::from(r"C:\one\proj"));
        assert!(owners.owns("proj", Path::new("c:/one/proj")));
        assert!(!owners.owns("proj", Path::new(r"D:\two\proj")));
        assert!(!owners.owns("other", Path::new(r"C:\one\proj")));
        assert_eq!(owners.owner("proj"), Some(Path::new(r"C:\one\proj")));
    }

    #[test]
    fn an_empty_owner_map_owns_nothing() {
        let owners = TopicOwners::new();
        assert!(!owners.owns("proj", Path::new("/home/me/proj")));
        assert!(owners.is_empty());
    }

    #[test]
    fn project_roots_bump_the_version_only_on_a_real_change() {
        let mut live = ProjectRoots::new(vec![PathBuf::from("/srv/one")]);
        assert_eq!(live.version(), 0);
        assert!(!live.set(vec![PathBuf::from("/srv/one")]));
        assert_eq!(live.version(), 0);

        assert!(live.set(vec![PathBuf::from("/srv/one"), PathBuf::from("/srv/two")]));
        assert_eq!(live.version(), 1);
        assert_eq!(live.roots().len(), 2);

        assert!(live.set(Vec::new()));
        assert_eq!(live.version(), 2);
        assert!(live.roots().is_empty());
    }

    #[test]
    fn unc_paths_use_the_share_basename() {
        assert_eq!(topic_for_project(r"\\server\share\proj"), Some("proj"));
        assert_eq!(topic_for_project(r"\\server\share\proj\\"), Some("proj"));
        assert_eq!(topic_for_project("\\\\server\\share\\"), None);
    }

    #[test]
    fn mixed_separators_on_windows() {
        assert_eq!(
            topic_for_project("C:/Users/me/ocdev/ntfy-proj"),
            Some("ntfy-proj")
        );
        assert_eq!(
            topic_for_project("C:/Users/me/ocdev/ntfy-proj/"),
            Some("ntfy-proj")
        );
    }

    #[test]
    fn leading_bom_is_trimmed_like_javascript() {
        assert_eq!(
            topic_for_project("\u{feff}/home/me/ntfy-proj"),
            Some("ntfy-proj")
        );
    }
}
