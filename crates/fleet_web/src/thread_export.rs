//! Turns a row of Zed's `threads.db` into the web view model (plan 051).
//!
//! Parsed as untyped JSON on purpose: depending on `agent` would drag gpui into
//! a headless exporter, and an unknown variant must degrade, not fail the row.

use anyhow::{Context as _, Result, bail};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::LazyLock;

/// Zed shows a long tool output collapsed; the web keeps the head of it.
pub const MAX_BLOCK_BYTES: usize = 16 * 1024;
/// A thread updated this recently is probably still generating.
pub const LIVE_SECONDS: i64 = 120;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThreadMeta {
    pub rig: String,
    pub id: String,
    pub title: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub folders: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub messages: usize,
    /// Computed at read time by the origin; stored as `false`.
    #[serde(default)]
    pub live: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tokens {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Agent,
    Resume,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockKind {
    Text,
    Thinking,
    ToolUse,
    ToolResult,
    Image,
    Mention,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Block {
    pub kind: BlockKind,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// `ok` / `error` on a tool result; `pending` on a tool use that never got one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub role: Role,
    pub blocks: Vec<Block>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThreadView {
    #[serde(flatten)]
    pub meta: ThreadMeta,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<Tokens>,
    pub entries: Vec<Entry>,
}

/// The `threads` columns the exporter reads, minus the blob.
#[derive(Debug, Clone)]
pub struct ThreadRow {
    pub id: String,
    pub summary: String,
    pub updated_at: String,
    pub created_at: Option<String>,
    pub parent_id: Option<String>,
    pub folder_paths: Option<String>,
}

pub fn decode_blob(data_type: &str, data: &[u8]) -> Result<Value> {
    let json = match data_type {
        "zstd" => zstd::decode_all(data).context("zstd")?,
        "json" => data.to_vec(),
        other => bail!("unknown thread data_type {other:?}"),
    };
    serde_json::from_slice(&json).context("thread json")
}

pub fn export(rig: &str, row: &ThreadRow, thread: &Value) -> ThreadView {
    let entries: Vec<Entry> = thread
        .get("messages")
        .and_then(Value::as_array)
        .map(|messages| messages.iter().filter_map(entry).collect())
        .unwrap_or_default();
    let title = match thread.get("title").and_then(Value::as_str) {
        Some(title) if !title.is_empty() => title.to_string(),
        _ => row.summary.clone(),
    };
    let model = thread.get("model").and_then(|model| {
        let name = model.get("model").and_then(Value::as_str)?;
        Some(match model.get("provider").and_then(Value::as_str) {
            Some(provider) => format!("{provider}/{name}"),
            None => name.to_string(),
        })
    });
    let tokens = thread.get("cumulative_token_usage").map(|usage| {
        let field = |name: &str| usage.get(name).and_then(Value::as_u64).unwrap_or(0);
        Tokens {
            input: field("input_tokens"),
            output: field("output_tokens"),
            cache_read: field("cache_read_input_tokens"),
            cache_write: field("cache_creation_input_tokens"),
        }
    });
    ThreadView {
        meta: ThreadMeta {
            rig: rig.to_string(),
            id: row.id.clone(),
            title: redact(&title),
            updated_at: row.updated_at.clone(),
            created_at: row.created_at.clone(),
            parent_id: row.parent_id.clone(),
            folders: row
                .folder_paths
                .as_deref()
                .map(|paths| {
                    paths
                        .lines()
                        .filter(|line| !line.is_empty())
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            model,
            messages: entries.len(),
            live: false,
        },
        tokens,
        entries,
    }
}

fn entry(message: &Value) -> Option<Entry> {
    if message.as_str() == Some("Resume") {
        return Some(Entry {
            role: Role::Resume,
            blocks: Vec::new(),
        });
    }
    if let Some(user) = message.get("User") {
        let blocks = content_items(user)
            .filter_map(user_block)
            .collect::<Vec<_>>();
        return Some(Entry {
            role: Role::User,
            blocks,
        });
    }
    if let Some(compaction) = message.get("Compaction") {
        let text = compaction
            .get("Summary")
            .and_then(Value::as_str)
            .unwrap_or("(context compacted)");
        return Some(Entry {
            role: Role::Resume,
            blocks: vec![block(BlockKind::Text, text, None, None)],
        });
    }
    let agent = message.get("Agent")?;
    let results = agent.get("tool_results");
    let mut blocks = Vec::new();
    for item in content_items(agent) {
        if let Some(text) = item.get("Text").and_then(Value::as_str) {
            blocks.push(block(BlockKind::Text, text, None, None));
        } else if let Some(thinking) = item.get("Thinking") {
            let text = thinking.get("text").and_then(Value::as_str).unwrap_or("");
            blocks.push(block(BlockKind::Thinking, text, None, None));
        } else if let Some(tool_use) = item.get("ToolUse") {
            let name = tool_use
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("tool");
            let input = match tool_use.get("input") {
                Some(input) if !input.is_null() => {
                    serde_json::to_string_pretty(input).unwrap_or_default()
                }
                _ => tool_use
                    .get("raw_input")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            };
            let result = tool_use
                .get("id")
                .and_then(Value::as_str)
                .and_then(|id| results?.get(id));
            let status = match result {
                None => "pending",
                Some(result) if result.get("is_error").and_then(Value::as_bool) == Some(true) => {
                    "error"
                }
                Some(_) => "ok",
            };
            blocks.push(block(BlockKind::ToolUse, &input, Some(name), Some(status)));
            if let Some(result) = result {
                let mut text = String::new();
                collect_text(result.get("content").unwrap_or(&Value::Null), &mut text);
                blocks.push(block(
                    BlockKind::ToolResult,
                    &text,
                    Some(name),
                    Some(status),
                ));
            }
        }
    }
    Some(Entry {
        role: Role::Agent,
        blocks,
    })
}

fn content_items(message: &Value) -> impl Iterator<Item = &Value> {
    message
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

fn user_block(item: &Value) -> Option<Block> {
    if let Some(text) = item.get("Text").and_then(Value::as_str) {
        return Some(block(BlockKind::Text, text, None, None));
    }
    if let Some(mention) = item.get("Mention") {
        let name = mention_name(mention.get("uri").unwrap_or(&Value::Null));
        let content = mention.get("content").and_then(Value::as_str).unwrap_or("");
        return Some(block(BlockKind::Mention, content, Some(&name), None));
    }
    item.get("Image")
        .map(|_| block(BlockKind::Image, "", None, None))
}

/// A mention URI is an enum (`File { abs_path }`, `Thread { name }`, ...);
/// any string inside it is a better label than nothing.
fn mention_name(uri: &Value) -> String {
    match uri {
        Value::String(text) => text.clone(),
        Value::Object(map) => map
            .iter()
            .map(|(kind, inner)| {
                let mut label = String::new();
                collect_text(inner, &mut label);
                let label = label.lines().next().unwrap_or("").to_string();
                match label.is_empty() {
                    true => kind.clone(),
                    false => label,
                }
            })
            .next()
            .unwrap_or_default(),
        _ => String::new(),
    }
}

fn collect_text(value: &Value, out: &mut String) {
    match value {
        Value::String(text) => {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(text);
        }
        Value::Array(items) => items.iter().for_each(|item| collect_text(item, out)),
        Value::Object(map) => {
            // Images carry base64 `source`; never ship that as text.
            if map.contains_key("source") && map.contains_key("size") {
                return;
            }
            map.values().for_each(|inner| collect_text(inner, out));
        }
        _ => {}
    }
}

fn block(kind: BlockKind, text: &str, name: Option<&str>, status: Option<&str>) -> Block {
    Block {
        kind,
        text: redact(&truncate(text, MAX_BLOCK_BYTES)),
        name: name.map(redact),
        status: status.map(str::to_string),
    }
}

pub fn truncate(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let head = &text[..text.floor_char_boundary(max_bytes)];
    format!("{head}\n… [{} bytes truncated]", text.len() - head.len())
}

static SECRET_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----",
        r"\b(?:sk|pk|rk)-[A-Za-z0-9_-]{20,}",
        r"\bgh[pousr]_[A-Za-z0-9]{30,}",
        r"\bgithub_pat_[A-Za-z0-9_]{30,}",
        r"\bxox[abprs]-[A-Za-z0-9-]{10,}",
        r"\bAKIA[0-9A-Z]{16}\b",
        r"\beyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}",
    ]
    .iter()
    .filter_map(|pattern| Regex::new(pattern).ok())
    .collect()
});

/// `NAME=value` / `"api_key": "value"` where the name says it is a secret.
static SECRET_ASSIGNMENT: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)([A-Za-z0-9_.-]*(?:secret|token|passw(?:or)?d|api[_-]?key|private[_-]?key|credential)[A-Za-z0-9_.-]*["']?\s*[:=]\s*["']?)([^\s"',;]{8,})"#,
    )
    .ok()
});

pub const REDACTED: &str = "[redacted]";

/// Threads routinely contain `.env` dumps and pasted keys; the site is behind
/// GitHub login, but a leaked cookie must not leak every credential too.
pub fn redact(text: &str) -> String {
    let mut text = std::borrow::Cow::Borrowed(text);
    for pattern in SECRET_PATTERNS.iter() {
        if pattern.is_match(&text) {
            text = std::borrow::Cow::Owned(pattern.replace_all(&text, REDACTED).into_owned());
        }
    }
    if let Some(assignment) = SECRET_ASSIGNMENT.as_ref()
        && assignment.is_match(&text)
    {
        text = std::borrow::Cow::Owned(
            assignment
                .replace_all(&text, |captures: &regex::Captures| match &captures[2] {
                    value if value == REDACTED => captures[0].to_string(),
                    _ => format!("{}{REDACTED}", &captures[1]),
                })
                .into_owned(),
        );
    }
    text.into_owned()
}

pub fn is_live(updated_at: &str, now: chrono::DateTime<chrono::Utc>) -> bool {
    chrono::DateTime::parse_from_rfc3339(updated_at)
        .map(|updated| (now - updated.with_timezone(&chrono::Utc)).num_seconds() < LIVE_SECONDS)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row() -> ThreadRow {
        ThreadRow {
            id: "t1".into(),
            summary: "Summary title".into(),
            updated_at: "2026-11-01T03:07:12.951261+00:00".into(),
            created_at: None,
            parent_id: None,
            folder_paths: Some("/Users/kat/git/zed\n/Users/kat/git/riir-ai".into()),
        }
    }

    #[test]
    fn exports_user_agent_tool_pairs_and_resume() {
        let thread = json!({
            "title": "Recent git activity",
            "model": {"provider": "GLM", "model": "glm-5.3-flash"},
            "cumulative_token_usage": {"input_tokens": 10, "output_tokens": 5, "cache_read_input_tokens": 7},
            "messages": [
                {"User": {"id": "u1", "content": [{"Text": "any news?"}, {"Mention": {"uri": {"File": {"abs_path": "/a/b.rs"}}, "content": "fn main() {}"}}]}},
                {"Agent": {"content": [
                    {"Thinking": {"text": "hmm", "signature": null}},
                    {"Text": "Checking"},
                    {"ToolUse": {"id": "c1", "name": "terminal", "raw_input": "{}", "input": {"command": "git log"}, "is_input_complete": true}},
                    {"ToolUse": {"id": "c2", "name": "grep", "raw_input": "{\"q\":1}", "input": null, "is_input_complete": true}}
                ], "tool_results": {"c1": {"tool_use_id": "c1", "tool_name": "terminal", "is_error": true, "content": [{"Text": "not in root"}], "output": null}}}},
                "Resume",
                {"Compaction": {"Summary": "earlier work"}},
                {"Unknown": {}}
            ]
        });
        let view = export("m3", &row(), &thread);
        assert_eq!(view.meta.title, "Recent git activity");
        assert_eq!(view.meta.model.as_deref(), Some("GLM/glm-5.3-flash"));
        assert_eq!(view.meta.folders.len(), 2);
        assert_eq!(view.meta.messages, 4);
        assert_eq!(
            view.tokens.as_ref().map(|tokens| tokens.cache_read),
            Some(7)
        );
        let user = &view.entries[0];
        assert_eq!(user.role, Role::User);
        assert_eq!(user.blocks[1].kind, BlockKind::Mention);
        assert_eq!(user.blocks[1].name.as_deref(), Some("/a/b.rs"));
        let kinds: Vec<_> = view.entries[1]
            .blocks
            .iter()
            .map(|block| (block.kind, block.status.clone()))
            .collect();
        assert_eq!(
            kinds,
            vec![
                (BlockKind::Thinking, None),
                (BlockKind::Text, None),
                (BlockKind::ToolUse, Some("error".into())),
                (BlockKind::ToolResult, Some("error".into())),
                (BlockKind::ToolUse, Some("pending".into())),
            ]
        );
        assert!(view.entries[1].blocks[2].text.contains("git log"));
        assert_eq!(view.entries[1].blocks[4].text, "{\"q\":1}");
        assert_eq!(view.entries[2].role, Role::Resume);
        assert_eq!(view.entries[3].blocks[0].text, "earlier work");
    }

    #[test]
    fn untitled_thread_falls_back_to_summary_column() {
        let view = export("m3", &row(), &json!({"messages": []}));
        assert_eq!(view.meta.title, "Summary title");
        assert_eq!(view.meta.messages, 0);
    }

    #[test]
    fn truncates_on_char_boundary() {
        let text = "—".repeat(10);
        let cut = truncate(&text, 4);
        assert!(cut.starts_with("—"));
        assert!(cut.contains("bytes truncated"));
        assert_eq!(truncate("short", 10), "short");
    }

    #[test]
    fn redacts_tokens_keys_and_secret_assignments() {
        let text = "FASTLY_API_SECRET=abcdEFGH12345678xyz\n\
            key sk-ant-api03-ABCDEFGHIJKLMNOPQRSTUV\n\
            gh ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789\n\
            {\"api_key\": \"hunter2hunter2\"}\n\
            password: short\n\
            -----BEGIN OPENSSH PRIVATE KEY-----\nAAAA\n-----END OPENSSH PRIVATE KEY-----\n\
            token_count = 12";
        let clean = redact(text);
        for secret in [
            "abcdEFGH12345678xyz",
            "ABCDEFGHIJKLMNOPQRSTUV",
            "ghp_ABCD",
            "hunter2hunter2",
            "AAAA",
        ] {
            assert!(!clean.contains(secret), "{secret} leaked: {clean}");
        }
        assert!(clean.contains("FASTLY_API_SECRET=[redacted]"));
        // Short values and non-secret numbers are left alone.
        assert!(clean.contains("password: short"));
        assert!(clean.contains("token_count = 12"));
    }

    #[test]
    fn live_means_updated_in_the_last_two_minutes() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-11-01T03:08:00+00:00")
            .map(|now| now.with_timezone(&chrono::Utc))
            .unwrap_or_default();
        assert!(is_live("2026-11-01T03:07:12.951261+00:00", now));
        assert!(!is_live("2026-11-01T02:07:12+00:00", now));
        assert!(!is_live("garbage", now));
    }
}
