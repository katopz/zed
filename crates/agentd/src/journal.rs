use crate::types::SessionRecord;
use anyhow::{Context as _, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{BufRead as _, BufReader, Write as _};
use std::path::PathBuf;

/// Durable session history, so a session outlives both the GUI and the daemon.
pub trait JournalStore {
    fn load_records(&self) -> Result<Vec<SessionRecord>>;
    fn write_record(&mut self, record: &SessionRecord) -> Result<()>;
    fn append_update(&mut self, session_id: &str, update: &Value) -> Result<()>;
    fn updates(&self, session_id: &str) -> Result<Vec<Value>>;
    /// Size of the session's update log, the part that grows per token.
    fn log_bytes(&self, session_id: &str) -> Result<u64>;
    fn remove_session(&mut self, session_id: &str) -> Result<()>;
}

/// Bounds the descriptors a long-lived daemon holds for append handles.
const MAX_OPEN_LOGS: usize = 64;

/// `<root>/<hash>.json` holds the record, `<root>/<hash>.jsonl` the update log.
/// Session ids are agent-chosen strings, hence hashed for filenames.
pub struct FileJournal {
    root: PathBuf,
    /// Agents journal one update per streamed token; reopening the log for
    /// each one costs ~25µs on the hub thread (benches/journal_replay.rs).
    open_logs: HashMap<String, fs::File>,
}

impl FileJournal {
    pub fn new(root: PathBuf) -> Result<Self> {
        fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;
        Ok(Self {
            root,
            open_logs: HashMap::new(),
        })
    }

    fn stem(session_id: &str) -> String {
        blake3::hash(session_id.as_bytes()).to_hex()[..32].to_string()
    }

    fn record_path(&self, session_id: &str) -> PathBuf {
        self.root.join(format!("{}.json", Self::stem(session_id)))
    }

    fn log_path(&self, session_id: &str) -> PathBuf {
        self.root.join(format!("{}.jsonl", Self::stem(session_id)))
    }
}

impl JournalStore for FileJournal {
    fn load_records(&self) -> Result<Vec<SessionRecord>> {
        let mut records = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let path = entry?.path();
            if path.extension().is_none_or(|extension| extension != "json") {
                continue;
            }
            let parsed = fs::read(&path)
                .map_err(anyhow::Error::from)
                .and_then(|bytes| Ok(serde_json::from_slice::<SessionRecord>(&bytes)?));
            match parsed {
                Ok(record) => records.push(record),
                Err(error) => {
                    eprintln!("agentd: skipping unreadable {}: {error:#}", path.display())
                }
            }
        }
        Ok(records)
    }

    fn write_record(&mut self, record: &SessionRecord) -> Result<()> {
        let path = self.record_path(&record.session_id);
        let temp = path.with_extension("json.tmp");
        fs::write(&temp, serde_json::to_vec_pretty(record)?)?;
        fs::rename(&temp, &path)?;
        Ok(())
    }

    fn append_update(&mut self, session_id: &str, update: &Value) -> Result<()> {
        let mut line = serde_json::to_vec(update)?;
        line.push(b'\n');
        if !self.open_logs.contains_key(session_id) {
            if self.open_logs.len() >= MAX_OPEN_LOGS {
                self.open_logs.clear();
            }
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.log_path(session_id))?;
            self.open_logs.insert(session_id.to_string(), file);
        }
        let file = self
            .open_logs
            .get_mut(session_id)
            .context("journal log handle missing after insert")?;
        if let Err(error) = file.write_all(&line) {
            // Drop the handle so the next append reopens instead of reusing a
            // descriptor that may be broken (e.g. the file was unlinked).
            self.open_logs.remove(session_id);
            return Err(error.into());
        }
        Ok(())
    }

    fn updates(&self, session_id: &str) -> Result<Vec<Value>> {
        let path = self.log_path(session_id);
        let file = match fs::File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut updates = Vec::new();
        for line in BufReader::new(file).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            // A crash mid-append leaves a torn last line; keep everything before it.
            match serde_json::from_str(&line) {
                Ok(update) => updates.push(update),
                Err(_) => break,
            }
        }
        Ok(updates)
    }

    fn log_bytes(&self, session_id: &str) -> Result<u64> {
        match fs::metadata(self.log_path(session_id)) {
            Ok(metadata) => Ok(metadata.len()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(error) => Err(error.into()),
        }
    }

    fn remove_session(&mut self, session_id: &str) -> Result<()> {
        // A cached append handle would keep writing into the unlinked inode.
        self.open_logs.remove(session_id);
        // Log before record: a crash in between leaves a record with an empty
        // history, which the next gc still sees and removes, never an orphan log.
        for path in [self.log_path(session_id), self.record_path(session_id)] {
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).with_context(|| format!("removing {}", path.display()));
                }
            }
        }
        Ok(())
    }
}

#[derive(Default)]
pub struct MemoryJournal {
    pub records: HashMap<String, SessionRecord>,
    pub logs: HashMap<String, Vec<Value>>,
}

impl JournalStore for MemoryJournal {
    fn load_records(&self) -> Result<Vec<SessionRecord>> {
        Ok(self.records.values().cloned().collect())
    }

    fn write_record(&mut self, record: &SessionRecord) -> Result<()> {
        self.records
            .insert(record.session_id.clone(), record.clone());
        Ok(())
    }

    fn append_update(&mut self, session_id: &str, update: &Value) -> Result<()> {
        self.logs
            .entry(session_id.to_string())
            .or_default()
            .push(update.clone());
        Ok(())
    }

    fn updates(&self, session_id: &str) -> Result<Vec<Value>> {
        Ok(self.logs.get(session_id).cloned().unwrap_or_default())
    }

    fn log_bytes(&self, session_id: &str) -> Result<u64> {
        let Some(updates) = self.logs.get(session_id) else {
            return Ok(0);
        };
        let mut bytes = 0;
        for update in updates {
            bytes += serde_json::to_vec(update)?.len() as u64 + 1;
        }
        Ok(bytes)
    }

    fn remove_session(&mut self, session_id: &str) -> Result<()> {
        self.records.remove(session_id);
        self.logs.remove(session_id);
        Ok(())
    }
}

/// Streaming agents emit one notification per token; merging adjacent text
/// chunks keeps a reattach replay from flooding the GUI with thousands of events.
pub fn coalesce_chunks(updates: Vec<Value>) -> Vec<Value> {
    let mut merged: Vec<Value> = Vec::with_capacity(updates.len());
    for update in updates {
        if let Some(previous) = merged.last_mut()
            && let Some(text) = mergeable_text(&update)
            && mergeable_text(previous).is_some()
            && previous.get("sessionUpdate") == update.get("sessionUpdate")
            && let Some(Value::String(previous_text)) = previous.pointer_mut("/content/text")
        {
            previous_text.push_str(text);
            continue;
        }
        merged.push(update);
    }
    merged
}

fn mergeable_text(update: &Value) -> Option<&str> {
    match update.get("sessionUpdate")?.as_str()? {
        "agent_message_chunk" | "agent_thought_chunk" | "user_message_chunk" => {}
        _ => return None,
    }
    let content = update.get("content")?;
    match content.get("type")?.as_str()? {
        "text" if content.get("annotations").is_none() => content.get("text")?.as_str(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn coalesces_adjacent_text_chunks_of_the_same_kind() {
        let updates = vec![
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Hel"}}),
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "lo"}}),
            json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "hm"}}),
            json!({"sessionUpdate": "tool_call", "toolCallId": "1"}),
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "!"}}),
        ];
        let merged = coalesce_chunks(updates);
        assert_eq!(merged.len(), 4);
        assert_eq!(merged[0]["content"]["text"], "Hello");
        assert_eq!(merged[3]["content"]["text"], "!");
    }

    #[test]
    fn file_journal_round_trips_and_survives_torn_tail() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut journal = FileJournal::new(dir.path().to_path_buf())?;
        let record = SessionRecord {
            session_id: "sess/1".into(),
            backend_key: "agent".into(),
            cwd: "/tmp".into(),
            title: Some("hi".into()),
            created_at: "t0".into(),
            updated_at: "t1".into(),
            setup: json!({"modes": null}),
        };
        journal.write_record(&record)?;
        journal.append_update("sess/1", &json!({"sessionUpdate": "plan"}))?;
        let log = dir
            .path()
            .join(format!("{}.jsonl", FileJournal::stem("sess/1")));
        let mut file = OpenOptions::new().append(true).open(log)?;
        file.write_all(b"{\"torn\":")?;

        assert_eq!(journal.load_records()?, vec![record]);
        assert_eq!(journal.updates("sess/1")?.len(), 1);
        Ok(())
    }

    #[test]
    fn file_journal_remove_session_drops_cached_handle() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut journal = FileJournal::new(dir.path().to_path_buf())?;
        journal.append_update("s", &json!({"n": 0}))?;
        assert!(journal.log_bytes("s")? > 0);
        assert!(journal.open_logs.contains_key("s"));

        journal.remove_session("s")?;
        assert!(!journal.open_logs.contains_key("s"));
        assert_eq!(journal.log_bytes("s")?, 0);

        // A later append must land in a fresh, visible file.
        journal.append_update("s", &json!({"n": 1}))?;
        assert_eq!(journal.updates("s")?, vec![json!({"n": 1})]);
        journal.remove_session("missing")?;
        Ok(())
    }

    #[test]
    fn file_journal_appends_survive_handle_eviction() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut journal = FileJournal::new(dir.path().to_path_buf())?;
        let sessions = MAX_OPEN_LOGS * 2 + 1;
        for round in 0..2 {
            for session in 0..sessions {
                journal.append_update(&format!("s{session}"), &json!({"round": round}))?;
            }
        }
        assert!(journal.open_logs.len() <= MAX_OPEN_LOGS);
        for session in 0..sessions {
            let updates = journal.updates(&format!("s{session}"))?;
            assert_eq!(updates, vec![json!({"round": 0}), json!({"round": 1})]);
        }
        Ok(())
    }
}
