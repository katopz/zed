//! Structured decision logging for debugging auto-prompt.
//!
//! On by default. Disable explicitly with `ZED_AUTO_PROMPT_LOG=0`; redirect the
//! destination with `ZED_AUTO_PROMPT_LOG_DIR`. Writes one JSON file per decision
//! event to `/tmp/zed_auto_prompt/` by default, named `{ms}_{seq}_{label}.json`
//! so a full trace of a single stop/resume cycle is reconstructable by file order.
//!
//! All IO is best-effort: failures are surfaced via `log::warn!` and never
//! propagated, so logging can never break the decision pipeline.

use serde_json::Value;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

/// Monotonic sequence to keep filenames unique within a millisecond.
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Whether decision logging is active. On by default; disable explicitly
/// with `ZED_AUTO_PROMPT_LOG=0` (or `false`).
fn enabled() -> bool {
    !matches!(
        std::env::var("ZED_AUTO_PROMPT_LOG").as_deref(),
        Ok("0") | Ok("false")
    )
}

/// Directory to write logs to. Defaults to `/tmp/zed_auto_prompt`.
fn log_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("ZED_AUTO_PROMPT_LOG_DIR") {
        return PathBuf::from(dir);
    }
    PathBuf::from("/tmp/zed_auto_prompt")
}

/// Truncate a string to `max` bytes on a UTF-8 char boundary, for embedding
/// potentially large fields (last_assistant_message, raw_response) in a log.
pub fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut s = text[..end].to_string();
    s.push_str("…(truncated)");
    s
}

/// Write a single decision event. `payload` is merged under a top-level object
/// alongside `timestamp` and `label`.
pub fn write_log(label: &str, payload: Value) {
    if !enabled() {
        return;
    }
    let dir = log_dir();
    if let Err(err) = std::fs::create_dir_all(&dir) {
        log::warn!("[auto_prompt::debug_log] failed to create {dir:?}: {err}");
        return;
    }
    let now = chrono::Utc::now();
    let timestamp = now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let ms = now.timestamp_millis();
    let seq = SEQUENCE.fetch_add(1, Ordering::Relaxed);

    let mut entry = serde_json::Map::new();
    entry.insert("timestamp".to_string(), Value::String(timestamp));
    entry.insert("label".to_string(), Value::String(label.to_string()));
    if let Value::Object(map) = payload {
        for (k, v) in map {
            entry.insert(k, v);
        }
    }
    let json = match serde_json::to_string_pretty(&Value::Object(entry)) {
        Ok(s) => s,
        Err(err) => {
            log::warn!("[auto_prompt::debug_log] serialize failed: {err}");
            return;
        }
    };
    let path = dir.join(format!("{ms}_{seq}_{label}.json"));
    if let Err(err) = std::fs::write(&path, json) {
        log::warn!("[auto_prompt::debug_log] failed to write {path:?}: {err}");
    }
}
