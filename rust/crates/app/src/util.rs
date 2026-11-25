//! Small shared helpers with no state.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Wall-clock milliseconds. Used for queue age, replay arithmetic, and logs;
/// control holds use `core::clock` instead.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// `clip` in the Node relay: cap a value before it reaches a log line or push.
pub fn clip(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        value.to_string()
    } else {
        let mut out: String = value.chars().take(limit).collect();
        out.push_str("...");
        out
    }
}

/// `Math.round(ms * (0.8 + Math.random() * 0.4))` without a PRNG dependency.
pub fn jitter(ms: i64) -> i64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.subsec_nanos())
        .unwrap_or(0);
    let factor = 0.8 + (nanos as f64 / 1_000_000_000.0) * 0.4;
    (ms as f64 * factor).round() as i64
}

/// Folder name used in push titles. Falls back to `project`.
pub fn project_name(canonical: &Path) -> String {
    canonical
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "project".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_keeps_short_values_and_marks_long_ones() {
        assert_eq!(clip("short", 10), "short");
        assert_eq!(clip("0123456789abc", 10), "0123456789...");
    }

    #[test]
    fn jitter_stays_inside_the_node_range() {
        for _ in 0..20 {
            let value = jitter(1000);
            assert!((800..=1200).contains(&value), "{value}");
        }
    }
}
