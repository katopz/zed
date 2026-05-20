# Fix: Special HTML Characters Breaking LLM Thread

## Status

- [x] Task 1: Truncate assistant message chunks in `collect()`
- [x] Task 2: Sanitize/truncate tool output HTML in `serialize_tool_call()`
- [x] Task 3: Detect failed fetch patterns to set `had_error`
- [x] Task 4: Run diagnostics and fix warnings
- [x] Task 5: Commit with conventional message

## Root Cause

When a web fetch returns a large page (e.g., GitHub 404 HTML), the content gets embedded into the auto_prompt context via two paths:

1. **Assistant message chunks**: `chunk.block().to_markdown(cx)` has no length limit
2. **Tool output**: `serialize_tool_call()` has a 2000-char guard on `raw_output`, but the assistant may render the fetch result as markdown in its own message chunks (no limit)
3. **No error detection**: A 404 fetch returns `Err(...)` which sets `ToolCallStatus::Failed`, but `had_error` on the thread is not set for tool failures — only for LLM-level errors (max tokens, refusal, stream errors)

## Task 1: Truncate Assistant Message Chunks

**File**: `crates/auto_prompt/src/context.rs`
**Location**: `AutoPromptContext::collect()`, the `AgentThreadEntry::AssistantMessage` branch

Add a `MAX_CHUNK_CHARS` constant (e.g., 4000) and truncate any chunk content that exceeds it, appending a `[truncated]` marker.

```rust
const MAX_ASSISTANT_CHUNK_CHARS: usize = 4000;

// In the AssistantMessage match arm:
let content = chunk.block().to_markdown(cx).to_string();
if !content.is_empty() {
    let content = if content.len() > MAX_ASSISTANT_CHUNK_CHARS {
        format!("{}…\n[truncated: {} chars]", &content[..MAX_ASSISTANT_CHUNK_CHARS], content.len())
    } else {
        content
    };
    messages.push(ContextMessage {
        role: ContextMessageRole::Assistant,
        content,
    });
}
```

## Task 2: Sanitize/Truncate Tool Output HTML

**File**: `crates/auto_prompt/src/context.rs`
**Location**: `serialize_tool_call()`

The `raw_output` already has a 2000-char guard. Add HTML detection: if the output looks like HTML (contains `<!DOCTYPE`, `<html`, `<head`, `<body`), truncate more aggressively to 500 chars and add a `[HTML content sanitized]` marker.

```rust
fn looks_like_html(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    lower.contains("<!doctype") || lower.contains("<html") || lower.contains("<head") || lower.contains("<body")
}

// In serialize_tool_call, for raw_output:
if looks_like_html(&output_str) {
    // Aggressively truncate HTML content
    if output_str.len() > 500 {
        parts.push(format!("Output: {}…\n[HTML content sanitized, {} chars total]", &output_str[..500], output_str.len()));
    } else {
        parts.push(format!("Output: {}…\n[HTML content sanitized]", &output_str));
    }
} else if output_str.len() < 2000 {
    parts.push(format!("Output: {output_str}"));
}
```

Also apply similar truncation to `raw_input` for consistency.

## Task 3: Detect Failed Fetch Patterns

**File**: `crates/acp_thread/src/acp_thread.rs`
**Location**: `AcpThread::run_turn()`, after tool call processing

When a tool call completes with `ToolCallStatus::Failed`, set `self.had_error = true` so the auto_prompt system applies backoff instead of continuing to cycle.

Look at how tool results are processed. The `ToolCallStatus::Failed` is set in `process_tool_result` when `tool_result.is_error` is true. We need the thread to detect this and set `had_error`.

**Approach**: After tool call completion is detected during streaming, check if any tool call has `Failed` status and set `had_error` accordingly. This is already done for `StopReason::MaxTokens` and `StopReason::Refusal` — we add a check for failed tool calls.

Check in `run_turn` where tool call status updates are processed. When a tool call update with `Failed` status is received, set `this.had_error = true`.

## Task 4: Diagnostics

```bash
cargo check -p auto_prompt -p acp_thread
./script/clippy
```

Fix any errors or warnings.

## Task 5: Commit

```bash
git add -A
git commit -m "fix: sanitize HTML in auto_prompt context and detect failed tool calls"
```

## Key Files

- `crates/auto_prompt/src/context.rs` — truncation + HTML sanitization
- `crates/acp_thread/src/acp_thread.rs` — `had_error` on failed tool calls
- `crates/agent/src/tools/fetch_tool.rs` — reference only (already handles HTML→markdown, 4xx errors)