//! Loading the control document from disk.
//!
//! A missing file means the built-in table. Anything else is read whole,
//! bounded, and validated; a bad document stops the relay at startup instead
//! of being merged.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::table::{
    builtin_control_table, validate_control_table, ControlConfigError, ControlTable,
    MAX_CONTROL_FILE_BYTES,
};

#[derive(Debug)]
pub struct ControlLoadError {
    message: String,
}

impl ControlLoadError {
    fn new(message: String) -> Self {
        Self { message }
    }

    /// The path and reason, for the startup log line.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ControlLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ControlLoadError {}

impl From<ControlConfigError> for ControlLoadError {
    fn from(error: ControlConfigError) -> Self {
        Self::new(error.to_string())
    }
}

#[derive(Debug, Clone)]
pub struct LoadedControl {
    pub table: ControlTable,
    /// True when the file did not exist and the built-in table was selected.
    pub missing: bool,
    pub path: PathBuf,
}

pub fn default_control_path(root: &Path) -> PathBuf {
    root.join("ntfy-relay-control.json")
}

pub fn load_control(path: &Path) -> Result<LoadedControl, ControlLoadError> {
    let display = path.display();
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(LoadedControl {
                table: builtin_control_table(),
                missing: true,
                path: path.to_path_buf(),
            });
        }
        Err(error) => {
            return Err(ControlLoadError::new(format!(
                "control: cannot read {display}: {error}"
            )));
        }
    };
    if !metadata.is_file() {
        return Err(ControlLoadError::new(format!(
            "control: {display} is not a regular file"
        )));
    }
    if metadata.len() > MAX_CONTROL_FILE_BYTES {
        return Err(ControlLoadError::new(format!(
            "control: {display} exceeds {MAX_CONTROL_FILE_BYTES} bytes"
        )));
    }

    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(LoadedControl {
                table: builtin_control_table(),
                missing: true,
                path: path.to_path_buf(),
            });
        }
        Err(error) => {
            return Err(ControlLoadError::new(format!(
                "control: cannot read {display}: {error}"
            )));
        }
    };
    let text = String::from_utf8_lossy(&bytes);
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);

    let raw: Value = serde_json::from_str(text).map_err(|error| {
        ControlLoadError::new(format!("control: invalid JSON in {display}: {error}"))
    })?;
    let table = validate_control_table(&raw)
        .map_err(|error| ControlLoadError::new(format!("control: {display}: {error}")))?;

    Ok(LoadedControl {
        table,
        missing: false,
        path: path.to_path_buf(),
    })
}
