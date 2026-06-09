# Plan 05: Agent Edit Conflict Detection

## Problem

When multiple auto_prompt agent threads run in parallel (e.g., Plan A and Plan B), they can
simultaneously edit the same file. The second agent's LLM generates edits based on stale file
content (read before Agent A's edits), leading to:

1. Wasted tokens when `old_text` matching fails
2. Lost edits if non-overlapping changes get clobbered
3. Edit wars where agents keep overwriting each other

## Solution: Global Edit Registry + Tool Response Warning

A lightweight global registry tracks which files are being actively edited by which agent session.
Tools check this registry and include warnings in their responses.

### Layer 1: Read-time Warning (cheapest — saves tokens)
When `read_file` reads a file that another agent is actively editing, include a warning in the
tool response. The LLM sees this and can choose to avoid the file or proceed cautiously.

### Layer 2: Edit-time Wait (prevents most conflicts)
When `EditSession::new()` opens a buffer for editing, check if another agent holds an active
lock. If yes, wait up to 60s (backoff 5s intervals) for the lock to release before proceeding.

### Layer 3: Existing Safety Net (already works)
`old_text` matching in `extract_match()` catches remaining conflicts — returns error, LLM
re-reads and retries. This is the fallback; Layers 1+2 prevent most cases.

## Architecture

```
┌─────────────────────────────────────────────────────────┐
│  EditConflictRegistry (global, process-local)            │
│                                                          │
│  papaya::HashMap<Arc<Path>, EditLock>                    │
│  - session_id: which agent holds the lock                │
│  - locked_at: when the lock was acquired                 │
│  - last_heartbeat: most recent edit activity timestamp   │
│                                                          │
│  register(path, session_id)  → lock file                 │
│  heartbeat(path)             → update last_heartbeat     │
│  release(path)               → unlock file               │
│  check_conflict(path, self_session_id) → Option<EditLock>│
│  cleanup_stale(max_age)      → remove locks > max_age    │
└─────────────────────────────────────────────────────────┘
```

## Files to Modify

| File | Change |
|---|---|
| `crates/agent/src/tools/edit_conflict.rs` | NEW: `EditConflictRegistry`, `EditLock` structs |
| `crates/agent/src/tools/mod.rs` | Add `mod edit_conflict` |
| `crates/agent/src/tools/edit_session.rs` | Register/release lock in `EditSession` new/drop |
| `crates/agent/src/tools/read_file_tool.rs` | Check registry, append warning to response |
| `crates/agent/src/tools/edit_file_tool.rs` | Check registry before creating edit session |

## Tasks

- [x] Task 1: Create `EditConflictRegistry` in new file `edit_conflict.rs`
- [x] Task 2: Register/release edit locks in `EditSession`
- [x] Task 3: Add read-time warning in `ReadFileTool`
- [x] Task 4: Add edit-time conflict check in `EditFileTool` / `EditSession::new()`
- [x] Task 5: Add stale lock cleanup (heartbeat timeout)
- [x] Task 6: Build + test + fix diagnostics
- [x] Task 7: Commit

## Design Decisions

- **papaya HashMap** over `Arc<Mutex<HashMap>>` — lock-free, per user's AGENTS.md preference
- **Process-local** — only coordinates agents within same Zed process. Cross-process would need
  lock files in `.agents/locks/`, deferred to future work
- **No LLM prompt injection** — warnings are part of tool response strings, not separate messages
- **Heartbeat via `buffer_edited` hook** — every time the edit pipeline applies a chunk, update
  `last_heartbeat`. This is the natural "I'm still working" signal
- **60s timeout** — if another agent hasn't touched a file in 60s, the lock is stale
- **`file_read_times` mtime cross-check** — `ActionLog` already tracks per-file mtime, use as
  secondary signal for stale lock detection
