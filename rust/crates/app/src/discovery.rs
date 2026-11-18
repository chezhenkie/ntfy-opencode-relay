//! Project discovery and topic selection.
//!
//! Mirrors `buildProjectMap` and `selectSubscribedTopics` in
//! `ntfy-inbox-relay.mjs`. A canonical path inside a configured root becomes a
//! project entry; rows for the same folder merge, duplicate topics keep the
//! newer activity, and only the newest 150 topics subscribe.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ntfy_relay_core::adapter::ProjectEntry;
use ntfy_relay_core::topic::{topic_for_project, TopicOwners};
use ntfy_relay_opencode::api::ProjectInfo;

pub const MAX_TOPICS: usize = 150;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Candidate {
    canonical: PathBuf,
    project_ids: Vec<String>,
    topic: String,
    last_activity: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectMap {
    entries: Vec<ProjectEntry>,
    warnings: Vec<String>,
    owners: TopicOwners,
}

impl ProjectMap {
    /// A topic resolves only to the canonical that won it; the losing project
    /// of a basename collision gets no route.
    pub fn get(&self, topic: &str) -> Option<&ProjectEntry> {
        self.entries
            .iter()
            .find(|entry| entry.topic == topic && self.owners.owns(topic, &entry.canonical))
    }

    /// Snapshot shared with the outbound resolver and the SSE topic set.
    pub fn owners(&self) -> &TopicOwners {
        &self.owners
    }

    /// Topic order is first-seen order, as the Node `Map` preserves it.
    pub fn topics(&self) -> Vec<String> {
        self.entries
            .iter()
            .map(|entry| entry.topic.clone())
            .collect()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }
}

pub fn build_project_map(projects: &[ProjectInfo], roots: &[PathBuf]) -> ProjectMap {
    let mut kept: Vec<Candidate> = Vec::new();
    let mut by_canonical: HashMap<String, usize> = HashMap::new();
    let mut warnings = Vec::new();

    for project in projects {
        let Some(canonical) = project
            .canonical
            .as_deref()
            .filter(|value| !value.is_empty())
        else {
            continue;
        };
        if !roots.iter().any(|root| is_path_inside(canonical, root)) {
            continue;
        }
        if project.id.is_empty() {
            warnings.push(format!("skip {canonical}: project id is not a string"));
            continue;
        }
        let key = canonical_key(canonical);
        if let Some(&index) = by_canonical.get(&key) {
            let entry = &mut kept[index];
            entry.last_activity = entry.last_activity.max(project.time.active);
            if !entry.project_ids.contains(&project.id) {
                entry.project_ids.push(project.id.clone());
            }
            continue;
        }
        let Some(topic) = topic_for_project(canonical) else {
            warnings.push(format!(
                "skip {canonical}: folder name is not a valid ntfy topic"
            ));
            continue;
        };
        by_canonical.insert(key, kept.len());
        kept.push(Candidate {
            canonical: PathBuf::from(canonical),
            project_ids: vec![project.id.clone()],
            topic: topic.to_string(),
            last_activity: project.time.active,
        });
    }

    let mut topics: Vec<Candidate> = Vec::new();
    let mut by_topic: HashMap<String, usize> = HashMap::new();
    for entry in kept {
        let Some(&index) = by_topic.get(&entry.topic) else {
            by_topic.insert(entry.topic.clone(), topics.len());
            topics.push(entry);
            continue;
        };
        let existing = &topics[index];
        if entry.last_activity > existing.last_activity {
            warnings.push(format!(
                "duplicate topic {}: keeping {} (active {}), losing {}",
                entry.topic,
                entry.canonical.display(),
                entry.last_activity,
                existing.canonical.display()
            ));
            topics[index] = entry;
        } else {
            warnings.push(format!(
                "duplicate topic {}: keeping {} (active {}), losing {}",
                entry.topic,
                existing.canonical.display(),
                existing.last_activity,
                entry.canonical.display()
            ));
        }
    }

    if topics.len() > MAX_TOPICS {
        let mut ranked: Vec<Candidate> = topics;
        ranked.sort_by_key(|entry| std::cmp::Reverse(entry.last_activity));
        let mut kept_topics = Vec::with_capacity(MAX_TOPICS);
        for (index, entry) in ranked.into_iter().enumerate() {
            if index < MAX_TOPICS {
                kept_topics.push(entry);
            } else {
                warnings.push(format!(
                    "skip {}: over {MAX_TOPICS} topics, lastActivity {}",
                    entry.topic, entry.last_activity
                ));
            }
        }
        topics = kept_topics;
    }

    let mut owners = TopicOwners::new();
    for entry in &topics {
        owners.insert(&entry.topic, entry.canonical.clone());
    }

    ProjectMap {
        entries: topics
            .into_iter()
            .map(|entry| ProjectEntry {
                topic: entry.topic,
                canonical: entry.canonical,
                project_ids: entry.project_ids,
            })
            .collect(),
        warnings,
        owners,
    }
}

fn canonical_key(canonical: &str) -> String {
    if cfg!(windows) {
        canonical.replace('/', "\\").to_lowercase()
    } else {
        canonical.to_string()
    }
}

fn is_path_inside(canonical: &str, root: &Path) -> bool {
    let child = Path::new(canonical);
    if child == root {
        return true;
    }
    if cfg!(windows) {
        let child = canonical.replace('/', "\\").to_lowercase();
        let parent = root.to_string_lossy().replace('/', "\\").to_lowercase();
        let prefix = if parent.ends_with('\\') {
            parent
        } else {
            format!("{parent}\\")
        };
        child.starts_with(&prefix)
    } else {
        child.starts_with(root)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ntfy_relay_opencode::api::ProjectTime;

    fn project(id: &str, canonical: &str, active: i64) -> ProjectInfo {
        ProjectInfo {
            id: id.to_string(),
            canonical: Some(canonical.to_string()),
            time: ProjectTime { active },
        }
    }

    fn root(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("temp")
            .join("app-discovery-tests")
            .join(name)
    }

    #[test]
    fn rows_merge_by_folder_and_keep_every_project_id() {
        let root = root("merge");
        let child = root.join("proj");
        let map = build_project_map(
            &[
                project("p1", &child.to_string_lossy(), 10),
                project("p2", &child.to_string_lossy(), 30),
            ],
            std::slice::from_ref(&root),
        );
        assert_eq!(map.len(), 1);
        let entry = map.get("proj").unwrap();
        assert_eq!(entry.project_ids, vec!["p1".to_string(), "p2".to_string()]);
        assert_eq!(entry.canonical, child);
    }

    #[test]
    fn folders_outside_the_roots_and_invalid_topics_are_skipped() {
        let root = root("roots");
        let outside = root.parent().unwrap().join("outside-roots");
        let bad = root.join("bad topic");
        let map = build_project_map(
            &[
                project("p1", &outside.to_string_lossy(), 1),
                project("p2", &bad.to_string_lossy(), 1),
            ],
            std::slice::from_ref(&root),
        );
        assert_eq!(map.len(), 0);
        assert!(map
            .warnings()
            .iter()
            .any(|warning| warning.contains("not a valid ntfy topic")));
    }

    #[test]
    fn duplicate_topics_keep_the_newer_activity() {
        let root = root("duplicate");
        let older = root.join("a").join("same");
        let newer = root.join("b").join("same");
        let map = build_project_map(
            &[
                project("p1", &older.to_string_lossy(), 10),
                project("p2", &newer.to_string_lossy(), 20),
            ],
            &[root],
        );
        assert_eq!(map.len(), 1);
        assert_eq!(map.get("same").unwrap().canonical, newer);
        assert!(map.owners().owns("same", &newer));
        assert!(!map.owners().owns("same", &older));
        assert!(map
            .warnings()
            .iter()
            .any(|warning| warning.contains("duplicate topic same")));
    }

    #[test]
    fn the_newest_150_topics_survive_the_cap() {
        let root = root("cap");
        let projects: Vec<ProjectInfo> = (0..151)
            .map(|index| {
                project(
                    &format!("p{index}"),
                    &root.join(format!("proj-{index:03}")).to_string_lossy(),
                    index,
                )
            })
            .collect();
        let map = build_project_map(&projects, &[root]);
        assert_eq!(map.len(), MAX_TOPICS);
        assert!(map.get("proj-000").is_none());
        assert!(map.get("proj-150").is_some());
        assert!(map
            .warnings()
            .iter()
            .any(|warning| warning.contains("over 150 topics")));
    }

    #[test]
    fn path_prefixes_do_not_cross_a_component_boundary() {
        let root = root("prefix");
        let sibling = PathBuf::from(format!("{}-other", root.display()));
        assert!(is_path_inside(&root.join("x").to_string_lossy(), &root));
        assert!(!is_path_inside(&sibling.to_string_lossy(), &root));
    }

    #[test]
    fn a_missing_project_id_is_reported() {
        let root = root("id");
        let child = root.join("proj");
        let map = build_project_map(&[project("", &child.to_string_lossy(), 1)], &[root]);
        assert_eq!(map.len(), 0);
        assert!(map
            .warnings()
            .iter()
            .any(|warning| warning.contains("project id is not a string")));
    }
}
