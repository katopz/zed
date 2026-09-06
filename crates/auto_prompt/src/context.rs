use acp_thread::{AcpThread, AgentThreadEntry, AssistantMessageChunk, ToolCallStatus};
use chrono::Local;
use gpui::App;
use serde::{Deserialize, Serialize};

const MAX_ASSISTANT_CHUNK_BYTES: usize = 20000;
const MAX_TOOL_CONTENT_BYTES: usize = 5000;

/// Phase of the auto-prompt stop lifecycle.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum StopPhase {
    /// Normal working phase — LLM is actively working on tasks.
    #[default]
    Working,
    /// Pre-stop verification — LLM wants to stop but we're verifying completeness.
    PreStop,
    /// Verified stop — all checks passed, chain can terminate.
    Verified,
}

/// Serializable context payload sent to the external LLM.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AutoPromptContext {
    /// ISO 8601 datetime when the AI stopped.
    pub current_datetime: String,
    /// Current working directories associated with the thread.
    pub current_paths: Vec<String>,
    /// The thread's session ID.
    pub session_id: String,
    /// The thread's title, if any.
    pub title: Option<String>,
    /// Serialized conversation entries. Only populated when the thread has no
    /// token usage (i.e. the provider doesn't report it) — it feeds the
    /// chars/4 `approximate_token_count` fallback; runtime consumers of this
    /// context read `first_user_message`/`last_assistant_message` instead.
    pub messages: Vec<ContextMessage>,
    /// Whether tools were used since last user message.
    pub used_tools: bool,
    /// Number of total entries in the thread.
    pub entry_count: usize,
    /// Current plan from the thread (entries with status).
    pub current_plan: Vec<PlanEntryContext>,
    /// Contents of `.plan` folder files found in work directories.
    pub plan_files: Vec<PlanFileContent>,
    /// Filenames of `.docs` folder files found in work directories.
    pub doc_files: Vec<String>,
    /// Why the thread stopped (end_turn, max_tokens, cancelled, refusal).
    pub stop_reason: String,
    /// Whether the thread encountered an error (includes a merely-failed tool
    /// call — very common in normal work and not indicative of API health).
    pub had_error: bool,
    /// Narrower than `had_error`: true only when the completion request
    /// itself failed (network/stream error), never for a failed tool call.
    /// This is the signal to use when reasoning about actual API exhaustion.
    #[serde(default)]
    pub had_api_error: bool,
    /// Approximate token count of this context (chars / 4). Includes
    /// conversation messages only when `messages` is populated (no token
    /// usage reported); otherwise it covers just plan/doc sources and is
    /// superseded by `actual_input_tokens` anyway.
    pub approximate_token_count: usize,
    /// Actual input token count from the thread's API usage response.
    /// This is the real token count shown in the UI, as opposed to the
    /// rough chars/4 estimate in `approximate_token_count`.
    pub actual_input_tokens: Option<u64>,
    /// Which auto-prompt iteration this is (starts at 1).
    pub iteration_count: u32,
    /// Current phase in the stop lifecycle (Working, PreStop, Verified).
    #[serde(default)]
    pub stop_phase: StopPhase,
    /// How many pre-stop verification attempts have been made.
    #[serde(default)]
    pub verification_count: u32,
    /// Whether this context was truncated/summarized due to token limits.
    pub was_truncated: bool,
    /// Whether any plan file contains checkbox patterns (- [ ] or - [x]).
    pub plan_has_checkboxes: bool,
    /// The first plan filename that exists, or a default if none.
    pub first_plan_filename: String,
    /// The plan number (e.g., "082") from the first plan filename, or "00" if not found.
    pub plan_number: String,
    /// The first user message in the conversation, carrying the original intent.
    #[serde(default)]
    pub first_user_message: Option<String>,
    /// The last assistant message, surfaced for remaining-work detection.
    #[serde(default)]
    pub last_assistant_message: Option<String>,
    /// File paths modified by Edit/Write tool calls in this thread.
    #[serde(default)]
    pub modified_files: Vec<String>,
    /// Plans currently claimed by other agent threads. The orchestration LLM
    /// should avoid picking these plans since another agent is already working
    /// on them.
    #[serde(default)]
    pub active_plan_claims: Vec<crate::plan_registry::ActivePlanClaim>,
}

/// A plan entry with its status.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlanEntryContext {
    pub content: String,
    pub status: String,
    pub priority: String,
}

/// Contents of a file from the `.plan` folder.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlanFileContent {
    pub path: String,
    pub content: String,
}

/// A single message in the conversation context.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ContextMessage {
    pub role: ContextMessageRole,
    pub content: String,
}

/// Role of a context message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextMessageRole {
    User,
    Assistant,
    Tool,
    Plan,
}

/// Response expected from the external LLM.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AutoPromptResponse {
    /// The next prompt to send, if any.
    pub next_prompt: Option<String>,
    /// Optional reason for the prompt (for logging/debugging).
    pub reason: Option<String>,
    /// Confidence level from 0.0 (not sure) to 1.0 (very confident).
    /// Phase-dependent thresholds decide whether to continue or stop.
    #[serde(default)]
    pub confidence: Option<f64>,
    /// Comprehensive summary of the entire thread, generated by the orchestration LLM.
    /// Replaces the raw first prompt in chained threads to prevent context drift.
    /// The active plan should be bolded (e.g. **plan 083**) in the summary text.
    #[serde(default)]
    pub thread_summary: Option<String>,
}

/// Main-thread snapshot of an [`AcpThread`] feeding [`AutoPromptContext`].
///
/// Every `cx`-dependent read is already resolved into owned plain data, so
/// this struct is `Send + 'static` and the O(thread) processing (markdown
/// stripping, per-chunk caps, JSON pretty-printing, joins, token estimation)
/// can run on a background thread via [`finish`](Self::finish).
#[derive(Debug)]
pub struct AutoPromptContextSnapshot {
    collected_at: String,
    current_paths: Vec<String>,
    session_id: String,
    title: Option<String>,
    stop_reason: String,
    had_error: bool,
    had_api_error: bool,
    actual_input_tokens: Option<u64>,
    entry_count: usize,
    collect_messages: bool,
    used_tools: bool,
    first_user_message: Option<String>,
    entries: Vec<SnapshotEntry>,
    /// Raw markdown sources of the trailing consecutive assistant run, in
    /// reverse entry order. Only gathered on the fast path (token usage
    /// reported) where `last_assistant_message` cannot be derived from
    /// `messages`.
    trailing_assistant_chunks: Vec<String>,
    current_plan: Vec<SnapshotPlanEntry>,
    plan_files: Vec<PlanFileContent>,
    doc_files: Vec<String>,
    iteration_count: u32,
}

#[derive(Debug)]
enum SnapshotEntry {
    User { source: String },
    Assistant { chunk_sources: Vec<String> },
    Plan { content_sources: Vec<String> },
    Tool(SnapshotToolCall),
}

/// Resolved `ToolCall`: label/status read out, raw input/output owned.
#[derive(Debug)]
struct SnapshotToolCall {
    label: String,
    status_label: &'static str,
    is_edit: bool,
    raw_input_markdown: Option<String>,
    raw_input: Option<serde_json::Value>,
    raw_output: Option<serde_json::Value>,
}

#[derive(Debug)]
struct SnapshotPlanEntry {
    content: String,
    status: &'static str,
    priority: &'static str,
}

impl AutoPromptContextSnapshot {
    /// Pure, `cx`-free half of [`AutoPromptContext::collect`]: builds the
    /// context from the snapshot. This is the O(thread) work — safe to run on
    /// a background thread.
    pub fn finish(self) -> AutoPromptContext {
        let mut messages = Vec::with_capacity(if self.collect_messages {
            self.entries.len()
        } else {
            0
        });
        let mut modified_files = Vec::new();

        for entry in self.entries {
            match entry {
                SnapshotEntry::User { source } => {
                    if !source.is_empty() {
                        messages.push(ContextMessage {
                            role: ContextMessageRole::User,
                            content: source,
                        });
                    }
                }
                SnapshotEntry::Assistant { chunk_sources } => {
                    push_assistant_chunks(&chunk_sources, &mut messages);
                }
                SnapshotEntry::Plan { content_sources } => {
                    let content = content_sources.join("\n");
                    if !content.is_empty() {
                        messages.push(ContextMessage {
                            role: ContextMessageRole::Plan,
                            content,
                        });
                    }
                }
                SnapshotEntry::Tool(tool) => {
                    collect_modified_file(&tool, &mut modified_files);
                    if self.collect_messages {
                        let content = serialize_tool_call(&tool);
                        messages.push(ContextMessage {
                            role: ContextMessageRole::Tool,
                            content,
                        });
                    }
                }
            }
        }

        let last_assistant_message = if self.collect_messages {
            join_trailing_assistant_messages(&messages)
        } else {
            let mut trailing: Vec<ContextMessage> = Vec::new();
            push_assistant_chunks(&self.trailing_assistant_chunks, &mut trailing);
            join_trailing_assistant_messages(&trailing)
        };

        let active_plan_claims =
            crate::plan_registry::active_claims_for_others(&self.session_id);

        let mut context = AutoPromptContext {
            current_datetime: self.collected_at,
            current_paths: self.current_paths,
            session_id: self.session_id,
            title: self.title,
            messages,
            used_tools: self.used_tools,
            entry_count: self.entry_count,
            current_plan: self
                .current_plan
                .into_iter()
                .map(|entry| PlanEntryContext {
                    content: entry.content,
                    status: entry.status.to_string(),
                    priority: entry.priority.to_string(),
                })
                .collect(),
            plan_files: self.plan_files,
            doc_files: self.doc_files,
            stop_reason: self.stop_reason,
            had_error: self.had_error,
            had_api_error: self.had_api_error,
            approximate_token_count: 0,
            actual_input_tokens: self.actual_input_tokens,
            iteration_count: self.iteration_count,
            stop_phase: StopPhase::Working,
            verification_count: 0,
            was_truncated: false,
            plan_has_checkboxes: false,
            first_plan_filename: String::new(),
            plan_number: String::new(),
            first_user_message: self.first_user_message,
            last_assistant_message,
            modified_files,
            active_plan_claims,
        };

        context.approximate_token_count = context.estimate_token_count();

        log::info!(
            "[auto_prompt::context] token counts: actual_input_tokens={:?}, estimated_chars_div_4={}",
            context.actual_input_tokens,
            context.approximate_token_count
        );

        // Compute helper fields
        context.plan_has_checkboxes = context.compute_plan_has_checkboxes();
        context.first_plan_filename = context.compute_first_plan_filename();
        context.plan_number = context.compute_plan_number();

        context
    }
}

/// Take complete paragraphs until total exceeds `budget` chars.
/// The paragraph that crosses the threshold is included — this ensures we
/// always return complete paragraphs without mid-sentence cuts.
/// A single huge paragraph is always included.
pub fn truncate_to_paragraph_budget(text: &str, budget: usize) -> String {
    let paragraphs: Vec<&str> = text.split("\n\n").collect();
    let mut total = 0usize;
    let mut taken = 0;
    for paragraph in &paragraphs {
        total += paragraph.len();
        taken += 1;
        if total > budget {
            break;
        }
    }
    if taken == 0 {
        return text.to_string();
    }
    paragraphs[..taken].join("\n\n")
}

impl AutoPromptContext {
    /// Collect context from the given AcpThread.
    ///
    /// `stop_reason` comes from `AcpThreadEvent::Stopped`.
    /// `plan_files` should be pre-read from `.plan` folders on disk.
    /// `doc_files` should be pre-read filenames from `.docs` folders on disk.
    /// `iteration_count` tracks how many auto-prompt cycles have occurred.
    ///
    /// Sync form of the two-phase pipeline: [`Self::snapshot`] +
    /// [`AutoPromptContextSnapshot::finish`]. Use the split form to run the
    /// O(thread) processing off the main thread.
    pub fn collect(
        thread: &AcpThread,
        cx: &App,
        stop_reason: String,
        plan_files: Vec<PlanFileContent>,
        doc_files: Vec<String>,
        iteration_count: u32,
    ) -> Self {
        Self::snapshot(thread, cx, stop_reason, plan_files, doc_files, iteration_count).finish()
    }

    /// Main-thread phase of [`Self::collect`]: resolves every `cx`-dependent
    /// read of `thread` (entity `source()` strings, tool JSON values) into an
    /// owned, `Send` snapshot. This is a memcpy-grade clone per entry — the
    /// expensive processing is deferred to
    /// [`AutoPromptContextSnapshot::finish`], which is pure and can run on a
    /// background thread.
    pub fn snapshot(
        thread: &AcpThread,
        cx: &App,
        stop_reason: String,
        plan_files: Vec<PlanFileContent>,
        doc_files: Vec<String>,
        iteration_count: u32,
    ) -> AutoPromptContextSnapshot {
        let collected_at = Local::now().to_rfc3339();

        let current_paths = thread
            .work_dirs()
            .map(|dirs| {
                dirs.paths()
                    .iter()
                    .map(|p| p.to_string_lossy().to_string())
                    .collect()
            })
            .unwrap_or_default();

        let session_id = thread.session_id().to_string();
        let title = thread.title().map(|t| t.to_string());
        let had_error = thread.had_error();
        let had_api_error = thread.had_api_error();
        let actual_input_tokens = thread.token_usage().map(|u| u.input_tokens);

        // Serializing every entry to markdown is O(thread size) and only feeds
        // the chars/4 `approximate_token_count` — no runtime consumer of this
        // context (lightweight orchestrator, plan detectors, summary flow)
        // reads `messages`. When the provider reports real token usage,
        // serialize only the first user message and the trailing assistant
        // run, which is what downstream machines read.
        let collect_messages = thread.token_usage().is_none();

        let thread_entries = thread.entries();
        let entry_count = thread_entries.len();

        let mut used_tools = false;
        let mut first_user_message: Option<String> = None;
        let mut entries = Vec::new();

        for entry in thread_entries {
            match entry {
                AgentThreadEntry::UserMessage(msg) => {
                    // On the fast path stop reading user content once the
                    // first non-empty message is captured.
                    if collect_messages || first_user_message.is_none() {
                        let source = msg.content.to_markdown(cx).to_string();
                        if !source.is_empty() {
                            if first_user_message.is_none() {
                                first_user_message = Some(source.clone());
                            }
                            if collect_messages {
                                entries.push(SnapshotEntry::User { source });
                            }
                        }
                    }
                }
                AgentThreadEntry::AssistantMessage(msg) => {
                    if collect_messages {
                        let chunk_sources = assistant_chunk_sources(&msg.chunks, cx);
                        if !chunk_sources.is_empty() {
                            entries.push(SnapshotEntry::Assistant { chunk_sources });
                        }
                    }
                }
                AgentThreadEntry::ToolCall(tool) => {
                    used_tools = true;
                    let is_edit = matches!(tool.kind, agent_client_protocol::schema::v1::ToolKind::Edit);
                    if collect_messages || is_edit {
                        entries.push(SnapshotEntry::Tool(SnapshotToolCall {
                            label: tool.label.read(cx).source().to_string(),
                            status_label: tool_status_label(&tool.status),
                            is_edit,
                            raw_input_markdown: if collect_messages {
                                tool.raw_input_markdown
                                    .as_ref()
                                    .map(|markdown| markdown.read(cx).source().to_string())
                            } else {
                                None
                            },
                            raw_input: if collect_messages {
                                tool.raw_input.clone()
                            } else {
                                None
                            },
                            raw_output: if collect_messages {
                                tool.raw_output.clone()
                            } else {
                                None
                            },
                        }));
                    }
                }
                AgentThreadEntry::CompletedPlan(plan_entries) => {
                    if collect_messages {
                        let content_sources = plan_entries
                            .iter()
                            .map(|entry| entry.content.read(cx).source().to_string())
                            .collect();
                        entries.push(SnapshotEntry::Plan { content_sources });
                    }
                }
                AgentThreadEntry::ContextCompaction(_) => {}
                AgentThreadEntry::Elicitation(_) => {}
                AgentThreadEntry::AgentBoardNotification(_) => {}
            }
        }

        // The trailing assistant run feeds `last_assistant_message` on the
        // fast path; on the full path it is derived from `messages` instead.
        // Matches the original `rev().skip_while(!assistant).take_while(
        // assistant)` walk: trailing non-assistant entries (e.g. a final tool
        // call) are skipped, then consecutive assistant entries are taken.
        let mut trailing_assistant_chunks = Vec::new();
        if !collect_messages {
            let mut in_trailing_run = false;
            for entry in thread_entries.iter().rev() {
                match entry {
                    AgentThreadEntry::AssistantMessage(msg) => {
                        in_trailing_run = true;
                        trailing_assistant_chunks
                            .extend(assistant_chunk_sources(&msg.chunks, cx));
                    }
                    _ if in_trailing_run => break,
                    _ => {}
                }
            }
        }

        let current_plan = thread
            .plan()
            .entries
            .iter()
            .map(|entry| SnapshotPlanEntry {
                content: entry.content.read(cx).source().to_string(),
                status: plan_status_label(&entry.status),
                priority: plan_priority_label(&entry.priority),
            })
            .collect();

        AutoPromptContextSnapshot {
            collected_at,
            current_paths,
            session_id,
            title,
            stop_reason,
            had_error,
            had_api_error,
            actual_input_tokens,
            entry_count,
            collect_messages,
            used_tools,
            first_user_message,
            entries,
            trailing_assistant_chunks,
            current_plan,
            plan_files,
            doc_files,
            iteration_count,
        }
    }

    /// Rough token estimate: ~4 chars per token. Conversation messages count
    /// toward this only when `messages` is populated, i.e. when the thread has
    /// no token usage and this estimate is the actual fallback.
    pub fn estimate_token_count(&self) -> usize {
        let total_chars: usize = self
            .messages
            .iter()
            .map(|m| m.content.len())
            .chain(self.current_plan.iter().map(|p| p.content.len()))
            .chain(self.plan_files.iter().map(|f| f.content.len()))
            .chain(self.doc_files.iter().map(|f| f.len()))
            .sum();

        total_chars / 4
    }

    /// Returns true if this context exceeds the given token limit.
    pub fn exceeds_token_limit(&self, limit: usize) -> bool {
        self.approximate_token_count > limit
    }

    /// Returns the last assistant message content, if any.
    pub fn last_assistant_message(&self) -> Option<&str> {
        self.last_assistant_message.as_deref().or_else(|| {
            self.messages
                .iter()
                .rev()
                .find(|m| matches!(m.role, ContextMessageRole::Assistant))
                .map(|m| m.content.as_str())
        })
    }

    /// Maximum byte budget for `last_assistant_message`. Paragraphs accumulate
    /// from the start; the paragraph that crosses this threshold is included so we
    /// always return complete paragraphs (no mid-sentence cut-off).
    ///
    /// Doubled from 5_000 → 10_000 after observing real-world assistant messages
    /// (especially those including thinking blocks) getting cut off mid-way through
    /// the response, losing critical context like benchmark gaps and next-steps.
    pub const LAST_MESSAGE_PARAGRAPH_BUDGET: usize = 10_000;

    pub fn compute_last_assistant_message(&self) -> Option<String> {
        join_trailing_assistant_messages(&self.messages)
    }

    /// Returns the count of plan items by status.
    pub fn plan_stats(&self) -> (u32, u32, u32) {
        let mut pending = 0u32;
        let mut in_progress = 0u32;
        let mut completed = 0u32;
        for entry in &self.current_plan {
            match entry.status.as_str() {
                "pending" => pending += 1,
                "in_progress" => in_progress += 1,
                "completed" => completed += 1,
                _ => {}
            }
        }
        (pending, in_progress, completed)
    }

    /// Returns plan files that still have unchecked `[ ]` items,
    /// indicating incomplete plans. Useful for multi-plan transition.
    pub fn remaining_plan_files(&self) -> Vec<&PlanFileContent> {
        self.plan_files
            .iter()
            .filter(|file| {
                let mut in_code_block = false;
                for line in file.content.lines() {
                    let trimmed = line.trim_start();
                    if trimmed.starts_with("```") {
                        in_code_block = !in_code_block;
                        continue;
                    }
                    if in_code_block {
                        continue;
                    }
                    if trimmed.contains("- [ ] ") {
                        return true;
                    }
                }
                false
            })
            .collect()
    }

    pub fn compute_plan_has_checkboxes(&self) -> bool {
        self.plan_files
            .iter()
            .any(|file| self.has_task_checkboxes(&file.content))
    }

    /// Checks if content has task-style checkboxes.
    /// Returns true only if:
    /// - Checkboxes are at start of lines (not deeply indented)
    /// - Not in code blocks (```)
    /// - Not in blockquotes (>)
    /// - Multiple checkboxes exist (3+ to avoid false positives from examples)
    fn has_task_checkboxes(&self, content: &str) -> bool {
        let mut in_code_block = false;
        let mut checkbox_count = 0;

        for line in content.lines() {
            let trimmed = line.trim_start();
            let leading_spaces = line.len() - trimmed.len();

            // Track code blocks
            if trimmed.starts_with("```") {
                in_code_block = !in_code_block;
                continue;
            }

            // Skip content in code blocks
            if in_code_block {
                continue;
            }

            // Skip blockquotes
            if trimmed.starts_with(">") {
                continue;
            }

            // Check for checkbox pattern at start of line (or with minimal indentation)
            // Allow up to 2 spaces of indentation (not code blocks or nested lists)
            let is_checkbox = leading_spaces <= 2
                && (trimmed.starts_with("- [ ") || trimmed.starts_with("- [x]"));

            if is_checkbox {
                checkbox_count += 1;
                // Require at least 3 checkboxes to consider it a task checklist
                // This avoids matching example sections with 1-2 checkboxes
                if checkbox_count >= 3 {
                    return true;
                }
            }
        }

        false
    }

    pub fn compute_first_plan_filename(&self) -> String {
        self.plan_files
            .first()
            .map(|f| f.path.rsplit('/').next().unwrap_or("plan.md").to_string())
            .unwrap_or_else(|| "plan.md".to_string())
    }

    pub fn compute_plan_number(&self) -> String {
        let filename = self.compute_first_plan_filename();
        // Extract number from patterns like "082_name.md" or "082.md"
        filename
            .split('_')
            .next()
            .map(|s| {
                let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
                if digits.is_empty() {
                    "00".to_string()
                } else {
                    digits
                }
            })
            .unwrap_or_else(|| "00".to_string())
    }
}

/// Clone the raw markdown sources of a message's non-thought chunks. Pure
/// snapshot work — processing (strip/cap) happens in `finish`.
fn assistant_chunk_sources(chunks: &[AssistantMessageChunk], cx: &App) -> Vec<String> {
    chunks
        .iter()
        .filter_map(|chunk| match chunk {
            AssistantMessageChunk::Message { block, .. } => {
                Some(block.to_markdown(cx).to_string())
            }
            AssistantMessageChunk::Thought { .. } => None,
        })
        .collect()
}

/// Serialize assistant chunk sources into `out`, skipping thoughts and empty
/// content and applying the per-chunk size cap. Shared by the full-serialization
/// path in [`AutoPromptContextSnapshot::finish`] and the trailing-run fast path
/// so both produce identical `ContextMessage`s.
fn push_assistant_chunks(chunk_sources: &[String], out: &mut Vec<ContextMessage>) {
    for source in chunk_sources {
        let content = process_assistant_chunk(source);
        if content.is_empty() {
            continue;
        }
        out.push(ContextMessage {
            role: ContextMessageRole::Assistant,
            content,
        });
    }
}

/// Process one assistant chunk source: strip code blocks, then apply the
/// per-chunk byte cap with a char-boundary-safe cut.
fn process_assistant_chunk(source: &str) -> String {
    let content = strip_code_blocks(source);
    if content.len() > MAX_ASSISTANT_CHUNK_BYTES {
        let mut end = MAX_ASSISTANT_CHUNK_BYTES;
        while !content.is_char_boundary(end) {
            end -= 1;
        }
        format!(
            "{}…\n[truncated: {} bytes total]",
            &content[..end],
            content.len()
        )
    } else {
        content
    }
}

/// Join the trailing run of assistant messages (from the end backwards, stopping
/// at the first non-assistant message) into one string under the paragraph budget.
fn join_trailing_assistant_messages(messages: &[ContextMessage]) -> Option<String> {
    let mut chunks: Vec<&str> = messages
        .iter()
        .rev()
        .skip_while(|m| !matches!(m.role, ContextMessageRole::Assistant))
        .take_while(|m| matches!(m.role, ContextMessageRole::Assistant))
        .map(|m| m.content.as_str())
        .collect();
    chunks.reverse();
    if chunks.is_empty() {
        return None;
    }
    let full = chunks.join("\n");
    Some(truncate_to_paragraph_budget(
        &full,
        AutoPromptContext::LAST_MESSAGE_PARAGRAPH_BUDGET,
    ))
}

/// Strip fenced code blocks (```...```) from markdown content.
fn strip_code_blocks(content: &str) -> String {
    let mut result = String::with_capacity(content.len());
    let mut in_code_block = false;
    for line in content.lines() {
        if line.trim_start().starts_with("```") {
            in_code_block = !in_code_block;
            continue;
        }
        if !in_code_block {
            if !result.is_empty() {
                result.push('\n');
            }
            result.push_str(line);
        }
    }
    result
}

/// Extract file path from an Edit tool call snapshot label and add to the list.
fn collect_modified_file(tool: &SnapshotToolCall, modified_files: &mut Vec<String>) {
    if !tool.is_edit {
        return;
    }
    for path in extract_backtick_paths(&tool.label) {
        if !modified_files.contains(&path) {
            modified_files.push(path);
        }
    }
}

/// Extract backtick-enclosed paths from a string.
fn extract_backtick_paths(text: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut in_backtick = false;
    let mut current = String::new();
    for ch in text.chars() {
        if ch == '`' {
            if in_backtick {
                let trimmed = current.trim();
                if !trimmed.is_empty() && !paths.iter().any(|p| p == trimmed) {
                    paths.push(trimmed.to_string());
                }
                current.clear();
            }
            in_backtick = !in_backtick;
        } else if in_backtick {
            current.push(ch);
        }
    }
    paths
}

/// Serialize a tool call snapshot into a readable string for context.
fn serialize_tool_call(tool: &SnapshotToolCall) -> String {
    let mut parts = vec![format!("[Tool: {} ({})]", tool.label, tool.status_label)];

    if let Some(raw_input) = &tool.raw_input_markdown {
        if !raw_input.is_empty() {
            parts.push(format!("Input: {raw_input}"));
        }
    } else if let Some(raw_input) = &tool.raw_input {
        let input_str =
            serde_json::to_string_pretty(raw_input).unwrap_or_else(|_| raw_input.to_string());
        if input_str.len() < MAX_TOOL_CONTENT_BYTES {
            parts.push(format!("Input: {input_str}"));
        }
    }

    if let Some(raw_output) = &tool.raw_output {
        let output_str =
            serde_json::to_string_pretty(raw_output).unwrap_or_else(|_| raw_output.to_string());
        if output_str.len() < MAX_TOOL_CONTENT_BYTES {
            parts.push(format!("Output: {output_str}"));
        } else {
            let mut end = MAX_TOOL_CONTENT_BYTES;
            while !output_str.is_char_boundary(end) {
                end -= 1;
            }
            parts.push(format!(
                "Output: {}…\n[truncated: {} bytes total]",
                &output_str[..end],
                output_str.len()
            ));
        }
    }

    parts.join("\n")
}

fn tool_status_label(status: &ToolCallStatus) -> &'static str {
    match status {
        ToolCallStatus::Pending => "pending",
        ToolCallStatus::WaitingForConfirmation { .. } => "waiting_confirmation",
        ToolCallStatus::InProgress => "in_progress",
        ToolCallStatus::Completed => "completed",
        ToolCallStatus::Failed => "failed",
        _ => "unknown",
    }
}

fn plan_status_label(
    status: &agent_client_protocol::schema::v1::PlanEntryStatus,
) -> &'static str {
    match status {
        agent_client_protocol::schema::v1::PlanEntryStatus::Pending => "pending",
        agent_client_protocol::schema::v1::PlanEntryStatus::InProgress => "in_progress",
        agent_client_protocol::schema::v1::PlanEntryStatus::Completed => "completed",
        _ => "unknown",
    }
}

fn plan_priority_label(
    priority: &agent_client_protocol::schema::v1::PlanEntryPriority,
) -> &'static str {
    match priority {
        agent_client_protocol::schema::v1::PlanEntryPriority::High => "high",
        agent_client_protocol::schema::v1::PlanEntryPriority::Medium => "medium",
        agent_client_protocol::schema::v1::PlanEntryPriority::Low => "low",
        _ => "unknown",
    }
}
