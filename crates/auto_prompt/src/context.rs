use acp_thread::{AcpThread, AgentThreadEntry, ContentBlock, ToolCall, ToolCallStatus};
use chrono::Local;
use gpui::App;
use serde::{Deserialize, Serialize};

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
    /// Serialized conversation entries.
    pub messages: Vec<ContextMessage>,
    /// Whether tools were used since last user message.
    pub used_tools: bool,
    /// Number of total entries in the thread.
    pub entry_count: usize,
    /// Current plan from the thread (entries with status).
    pub current_plan: Vec<PlanEntryContext>,
    /// Contents of `.plan` folder files found in work directories.
    pub plan_files: Vec<PlanFileContent>,
    /// Contents of `.doc` folder files found in work directories.
    pub doc_files: Vec<PlanFileContent>,
    /// Why the thread stopped (end_turn, max_tokens, cancelled, refusal).
    pub stop_reason: String,
    /// Whether the thread encountered an error.
    pub had_error: bool,
    /// Approximate token count of this context (chars / 4).
    pub approximate_token_count: usize,
    /// Which auto-prompt iteration this is (starts at 1).
    pub iteration_count: u32,
    /// Whether this context was truncated/summarized due to token limits.
    pub was_truncated: bool,
    /// Whether any plan file contains checkbox patterns (- [ ] or - [x]).
    pub plan_has_checkboxes: bool,
    /// The first plan filename that exists, or a default if none.
    pub first_plan_filename: String,
    /// The plan number (e.g., "082") from the first plan filename, or "00" if not found.
    pub plan_number: String,
    /// Whether we're starting a new plan implementation.
    pub is_starting_new_plan: bool,
}

/// A plan entry with its status.
#[derive(Clone, Debug, Serialize, Deserialize)]
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
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContextMessage {
    pub role: ContextMessageRole,
    pub content: String,
}

/// Role of a context message.
#[derive(Clone, Debug, Serialize, Deserialize)]
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
    /// Whether the external LLM wants to continue the conversation.
    #[serde(default)]
    pub should_continue: bool,
    /// The next prompt to send, if any.
    pub next_prompt: Option<String>,
    /// Optional reason for the prompt (for logging/debugging).
    pub reason: Option<String>,
    /// Set to true when all plan items are verified complete against the code.
    #[serde(default)]
    pub all_plan_done: bool,
    /// Confidence level from 0.0 (not sure) to 1.0 (very confident).
    /// Below 0.5 means the LLM is too uncertain and the chain should stop.
    #[serde(default)]
    pub confidence: Option<f64>,
}

impl AutoPromptContext {
    /// Collect context from the given AcpThread.
    ///
    /// `stop_reason` comes from `AcpThreadEvent::Stopped`.
    /// `plan_files` should be pre-read from `.plan` folders on disk.
    /// `doc_files` should be pre-read from `.doc` folders on disk.
    /// `iteration_count` tracks how many auto-prompt cycles have occurred.
    pub fn collect(
        thread: &AcpThread,
        cx: &App,
        stop_reason: String,
        plan_files: Vec<PlanFileContent>,
        doc_files: Vec<PlanFileContent>,
        iteration_count: u32,
    ) -> Self {
        let current_datetime = Local::now().to_rfc3339();

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

        let entries = thread.entries();
        let entry_count = entries.len();

        let mut used_tools = false;
        let mut messages = Vec::with_capacity(entry_count);

        for entry in entries {
            match entry {
                AgentThreadEntry::UserMessage(msg) => {
                    let content = msg.content.to_markdown(cx).to_string();
                    if !content.is_empty() {
                        messages.push(ContextMessage {
                            role: ContextMessageRole::User,
                            content,
                        });
                    }
                }
                AgentThreadEntry::AssistantMessage(msg) => {
                    for chunk in &msg.chunks {
                        let content = chunk.block().to_markdown(cx).to_string();
                        if !content.is_empty() {
                            messages.push(ContextMessage {
                                role: ContextMessageRole::Assistant,
                                content,
                            });
                        }
                    }
                }
                AgentThreadEntry::ToolCall(tool) => {
                    used_tools = true;
                    let content = serialize_tool_call(tool, cx);
                    messages.push(ContextMessage {
                        role: ContextMessageRole::Tool,
                        content,
                    });
                }
                AgentThreadEntry::CompletedPlan(plan_entries) => {
                    let content = plan_entries
                        .iter()
                        .map(|entry| entry.content.read(cx).source().to_string())
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !content.is_empty() {
                        messages.push(ContextMessage {
                            role: ContextMessageRole::Plan,
                            content,
                        });
                    }
                }
            }
        }

        let current_plan = collect_plan_entries(thread, cx);

        let mut context = Self {
            current_datetime,
            current_paths,
            session_id,
            title,
            messages,
            used_tools,
            entry_count,
            current_plan,
            plan_files,
            doc_files,
            stop_reason,
            had_error,
            approximate_token_count: 0,
            iteration_count,
            was_truncated: false,
            plan_has_checkboxes: false,
            first_plan_filename: String::new(),
            plan_number: String::new(),
            is_starting_new_plan: false,
        };

        context.approximate_token_count = context.estimate_token_count();

        // Compute helper fields
        context.plan_has_checkboxes = context.compute_plan_has_checkboxes();
        context.first_plan_filename = context.compute_first_plan_filename();
        context.plan_number = context.compute_plan_number();
        context.is_starting_new_plan = context.compute_is_starting_new_plan();

        context
    }

    /// Rough token estimate: ~4 chars per token.
    pub fn estimate_token_count(&self) -> usize {
        let total_chars: usize = self
            .messages
            .iter()
            .map(|m| m.content.len())
            .chain(self.current_plan.iter().map(|p| p.content.len()))
            .chain(self.plan_files.iter().map(|f| f.content.len()))
            .chain(self.doc_files.iter().map(|f| f.content.len()))
            .sum();

        total_chars / 4
    }

    /// Returns true if this context exceeds the given token limit.
    pub fn exceeds_token_limit(&self, limit: usize) -> bool {
        self.approximate_token_count > limit
    }

    /// Returns the last assistant message content, if any.
    pub fn last_assistant_message(&self) -> Option<&str> {
        self.messages
            .iter()
            .rev()
            .find(|m| matches!(m.role, ContextMessageRole::Assistant))
            .map(|m| m.content.as_str())
    }

    /// Returns true if the last assistant message looks like a question.
    pub fn last_message_is_question(&self) -> bool {
        if let Some(last) = self.last_assistant_message() {
            let trimmed = last.trim();
            let ends_with_question = trimmed.ends_with('?');
            let has_question_words = trimmed
                .split('.')
                .next_back()
                .map(|s| {
                    let s = s.to_lowercase();
                    s.contains("should i")
                        || s.contains("do you")
                        || s.contains("what would")
                        || s.contains("how should")
                        || s.contains("which ")
                        || s.contains("would you like")
                })
                .unwrap_or(false);
            ends_with_question || has_question_words
        } else {
            false
        }
    }

    /// Returns true if the last assistant message indicates task completion.
    pub fn last_message_indicates_completion(&self) -> bool {
        if let Some(last) = self.last_assistant_message() {
            let lower = last.to_lowercase();
            let completion_markers = [
                "all done",
                "task complete",
                "everything is done",
                "finished all",
                "nothing more to do",
                "all tasks completed",
                "all plan items are done",
                "all_plan_done",
                "implementation is complete",
                "all changes have been made",
                "no further action",
                "nothing left to do",
                "mission accomplished",
            ];
            completion_markers
                .iter()
                .any(|marker| lower.contains(marker))
        } else {
            false
        }
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

    pub fn compute_is_starting_new_plan(&self) -> bool {
        if self.plan_files.is_empty() {
            return false;
        }

        // Check if entry count is low (user message + maybe one assistant response)
        if self.entry_count > 4 {
            return false;
        }

        // Check if user message contains plan implementation keywords
        let user_messages: Vec<_> = self
            .messages
            .iter()
            .filter(|m| matches!(m.role, ContextMessageRole::User))
            .collect();

        if user_messages.is_empty() {
            return false;
        }

        let last_user_msg = user_messages.last().unwrap();
        let lower_msg = last_user_msg.content.to_lowercase();

        let plan_keywords = [
            "impl as a plan",
            "implement the plan",
            "execute the plan",
            "start the plan",
            "work on plan",
            "follow the plan",
        ];

        plan_keywords
            .iter()
            .any(|keyword| lower_msg.contains(keyword))
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

/// Collect plan entries from the thread.
fn collect_plan_entries(thread: &AcpThread, cx: &App) -> Vec<PlanEntryContext> {
    thread
        .plan()
        .entries
        .iter()
        .map(|entry| {
            let content = entry.content.read(cx).source().to_string();
            let status = match entry.status {
                agent_client_protocol::PlanEntryStatus::Pending => "pending",
                agent_client_protocol::PlanEntryStatus::InProgress => "in_progress",
                agent_client_protocol::PlanEntryStatus::Completed => "completed",
                _ => "unknown",
            };
            let priority = match entry.priority {
                agent_client_protocol::PlanEntryPriority::High => "high",
                agent_client_protocol::PlanEntryPriority::Medium => "medium",
                agent_client_protocol::PlanEntryPriority::Low => "low",
                _ => "unknown",
            };
            PlanEntryContext {
                content,
                status: status.to_string(),
                priority: priority.to_string(),
            }
        })
        .collect()
}

/// Serialize a tool call into a readable string for context.
fn serialize_tool_call(tool: &ToolCall, cx: &App) -> String {
    let status_label = match &tool.status {
        ToolCallStatus::Pending => "pending",
        ToolCallStatus::WaitingForConfirmation { .. } => "waiting_confirmation",
        ToolCallStatus::InProgress => "in_progress",
        ToolCallStatus::Completed => "completed",
        ToolCallStatus::Failed => "failed",
        _ => "unknown",
    };

    let title = tool.label.read(cx).source().to_string();

    let mut parts = vec![format!("[Tool: {title} ({status_label})]")];

    if let Some(raw_input) = &tool.raw_input_markdown {
        let input_text = raw_input.read(cx).source().to_string();
        if !input_text.is_empty() {
            parts.push(format!("Input: {input_text}"));
        }
    } else if let Some(raw_input) = &tool.raw_input {
        let input_str =
            serde_json::to_string_pretty(raw_input).unwrap_or_else(|_| raw_input.to_string());
        if input_str.len() < 2000 {
            parts.push(format!("Input: {input_str}"));
        }
    }

    if let Some(raw_output) = &tool.raw_output {
        let output_str =
            serde_json::to_string_pretty(raw_output).unwrap_or_else(|_| raw_output.to_string());
        if output_str.len() < 2000 {
            parts.push(format!("Output: {output_str}"));
        }
    }

    parts.join("\n")
}

/// Helper trait to get the content block from an AssistantMessageChunk.
trait AssistantMessageChunkExt {
    fn block(&self) -> &ContentBlock;
}

impl AssistantMessageChunkExt for acp_thread::AssistantMessageChunk {
    fn block(&self) -> &ContentBlock {
        match self {
            Self::Message { block } => block,
            Self::Thought { block } => block,
        }
    }
}
