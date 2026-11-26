//! Atomic rewrite of `projectRoots` in the live relay config.
//!
//! The control endpoint is the only writer while the daemon runs. The whole
//! document is parsed, one key is replaced, the result is validated through the
//! normal loader, and only then is the file swapped through a temporary
//! sibling, so a failure leaves the old config in place.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use ntfy_relay_core::config::load_config_from_text;
use ntfy_relay_core::fsutil::sync_parent;
use serde_json::Value;

pub fn write_roots(config_path: &Path, roots: &[PathBuf], home: &Path) -> Result<(), String> {
    if roots.iter().any(|root| !root.is_absolute()) {
        return Err("config: project root must be absolute".to_string());
    }
    let text = fs::read_to_string(config_path)
        .map_err(|error| format!("config: cannot read {}: {error}", config_path.display()))?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let mut document: Value =
        serde_json::from_str(text).map_err(|error| format!("config: invalid JSON: {error}"))?;
    let object = document
        .as_object_mut()
        .ok_or_else(|| "config: top level must be an object".to_string())?;
    let list = roots
        .iter()
        .map(|root| Value::String(root.to_string_lossy().into_owned()))
        .collect();
    object.insert("projectRoots".to_string(), Value::Array(list));

    let mut next = serde_json::to_string_pretty(&document)
        .map_err(|error| format!("config: cannot serialize: {error}"))?;
    next.push('\n');
    load_config_from_text(&next, Some(config_path), home).map_err(|error| error.to_string())?;

    let temp = config_path.with_extension("json.tmp");
    match replace(config_path, &temp, next.as_bytes()) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(&temp);
            Err(format!(
                "config: cannot write {}: {error}",
                config_path.display()
            ))
        }
    }
}

fn replace(path: &Path, temp: &Path, bytes: &[u8]) -> io::Result<()> {
    let permissions = fs::metadata(path).ok().map(|meta| meta.permissions());
    // Owner-only while the document is staged on Unix; the original mode is
    // applied before the swap. Windows keeps the inherited ACL.
    let mut file = ntfy_relay_core::fsutil::owner_only(temp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    if let Some(permissions) = permissions {
        let _ = fs::set_permissions(temp, permissions);
    }
    fs::rename(temp, path)?;
    sync_parent(path);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ntfy_relay_core::config::load_config;

    fn root(name: &str) -> PathBuf {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("temp")
            .join("app-config-write-tests")
            .join(name);
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn seed(dir: &Path) -> PathBuf {
        let config = dir.join("ntfy-relay.json");
        let text = serde_json::json!({
            "ntfy": { "server": "https://ntfy.test" },
            "projectRoots": [dir.join("old")],
            "logging": { "file": "relay.log" },
            "stateFile": "state.json"
        });
        fs::write(&config, serde_json::to_string_pretty(&text).unwrap()).unwrap();
        config
    }

    #[test]
    fn one_key_changes_and_the_rest_survives() {
        let dir = root("replace");
        let one = dir.join("one");
        let two = dir.join("two");
        fs::create_dir_all(&one).unwrap();
        fs::create_dir_all(&two).unwrap();
        let config = seed(&dir);

        write_roots(&config, &[one.clone(), two.clone()], &dir).expect("write");

        let loaded = load_config(&config).expect("reload");
        assert_eq!(loaded.project_roots, vec![one, two]);
        assert_eq!(loaded.ntfy.server, "https://ntfy.test");
        assert_eq!(loaded.state_file, dir.join("state.json"));
        assert!(!dir.join("ntfy-relay.json.tmp").exists());
        let text = fs::read_to_string(&config).unwrap();
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn an_empty_root_list_is_written_and_loads() {
        let dir = root("empty");
        let config = seed(&dir);
        write_roots(&config, &[], &dir).expect("write");
        let loaded = load_config(&config).expect("reload");
        assert!(loaded.project_roots.is_empty());
    }

    #[test]
    fn a_rejected_document_leaves_the_old_file() {
        let dir = root("intact");
        let config = seed(&dir);
        let before = fs::read_to_string(&config).unwrap();
        let error = write_roots(&config, &[PathBuf::from("relative")], &dir).unwrap_err();
        assert!(error.contains("absolute"), "{error}");
        assert_eq!(fs::read_to_string(&config).unwrap(), before);
        assert!(!dir.join("ntfy-relay.json.tmp").exists());
    }
}
