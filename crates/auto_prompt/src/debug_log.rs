//! Structured decision logging for debugging auto-prompt.
//!
//! Off by default (a filesystem write per decision on the foreground thread
//! is unjustifiable as a default — see issue 006). Enable explicitly with
//! `ZED_AUTO_PROMPT_LOG=1`; redirect the destination with
//! `ZED_AUTO_PROMPT_LOG_DIR`. Writes one JSON file per decision event to
//! `/tmp/zed_auto_prompt/` by default, named `{ms}_{seq}_{label}.json` so a
//! full trace of a single stop/resume cycle is reconstructable by file order.
//!
//! The write runs on a detached background task (P2 follow-up); names still
//! carry the ordering, so out-of-order completion is harmless. Tail entries
//! may be lost on hard process exit — acceptable for a debug trace.
//!
//! All IO is best-effort: failures are surfaced via `log::warn!` and never
//! propagated, so logging can never break the decision pipeline.

use serde_json::Value;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

/// Monotonic sequence to keep filenames unique within a millisecond.
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Whether decision logging is active. Off by default to avoid a synchronous
/// filesystem write per decision on the foreground thread (issue 006); enable
/// explicitly with `ZED_AUTO_PROMPT_LOG=1` (or `true`).
fn enabled() -> bool {
    matches!(
        std::env::var("ZED_AUTO_PROMPT_LOG").as_deref(),
        Ok("1") | Ok("true")
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
    spawn_write(path, json);
}

/// Hand the actual file write to a detached background task: even as an
/// explicit opt-in, a synchronous filesystem write per decision on the
/// foreground thread was a measurable stall source (issue 006). File names
/// still carry `{ms}_{seq}` ordering, so out-of-order completion is harmless.
fn spawn_write(path: PathBuf, json: String) {
    smol::spawn(async move {
        if let Err(err) = smol::fs::write(&path, json).await {
            log::warn!("[auto_prompt::debug_log] failed to write {path:?}: {err}");
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_write_lands_file_via_background_task() {
        let dir = std::env::temp_dir().join(format!("zed_ap_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("1_0_smoke.json");
        spawn_write(path.clone(), "{\"check\": true}".to_string());
        // The write is detached on the global executor; poll briefly for it to
        // land rather than sleeping a fixed amount.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !path.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "debug_log file never landed at {path:?}"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\"check\": true}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
