use super::*;
use acp_thread::ClientUserMessageId;
use agent::{
    AgentMessage, Message as AgentMessageKind, UserMessage as NativeUserMessage, UserMessageContent,
};

// --- slice_messages_for_branch (native message slice) ---

fn native_user(id: &ClientUserMessageId, text: &str) -> Arc<AgentMessageKind> {
    Arc::new(AgentMessageKind::User(NativeUserMessage {
        id: id.clone(),
        content: vec![UserMessageContent::Text(text.to_string())].into(),
    }))
}

fn native_agent(text: &str) -> Arc<AgentMessageKind> {
    // `AgentMessage`'s content fields are `pub(crate)`, so we can't build a
    // populated one from outside the `agent` crate. That's fine: the slice
    // logic only matches on user-message ids, so an empty default agent
    // message suffices to prove inter-user messages are carried through.
    let _ = text;
    Arc::new(AgentMessageKind::Agent(AgentMessage::default()))
}

#[test]
fn slice_carries_first_turn_exclusively_of_the_next() {
    let id_a = ClientUserMessageId::new();
    let id_b = ClientUserMessageId::new();
    let messages = vec![
        native_user(&id_a, "u1"),
        native_agent("a1"),
        native_user(&id_b, "u2"),
        native_agent("a2"),
    ];
    // Carrying 1 turn keeps u1 + its reply, drops u2 and everything after.
    let sliced = slice_messages_for_branch(&messages, 1);
    assert_eq!(sliced.len(), 2);
    assert_eq!(sliced[0], messages[0]);
    assert_eq!(sliced[1], messages[1]);
}

#[test]
fn slice_carry_at_turn_count_forks_whole_history() {
    // The separator at the bottom of a thread carries every turn, which
    // must still fork the full history (previously the `None` case).
    let id_a = ClientUserMessageId::new();
    let id_b = ClientUserMessageId::new();
    let messages = vec![
        native_user(&id_a, "u1"),
        native_agent("a1"),
        native_user(&id_b, "u2"),
        native_agent("a2"),
    ];
    let sliced = slice_messages_for_branch(&messages, 2);
    assert_eq!(sliced.len(), 4);
}

#[test]
fn slice_carry_past_turn_count_still_forks_whole_history() {
    // A stale separator can carry more turns than remain after edits;
    // degrading to the whole history is the safe floor, never an empty fork.
    let id_a = ClientUserMessageId::new();
    let messages = vec![native_user(&id_a, "u1"), native_agent("a1")];
    let sliced = slice_messages_for_branch(&messages, 5);
    assert_eq!(sliced.len(), 2);
}

#[test]
fn slice_keeps_non_user_messages_above_the_boundary() {
    // Resume/Compaction markers interleaved with the carried turns come
    // along; the ones after the boundary are dropped with their turn.
    let id_a = ClientUserMessageId::new();
    let id_b = ClientUserMessageId::new();
    let messages = vec![
        Arc::new(AgentMessageKind::Resume),
        native_user(&id_a, "u1"),
        native_agent("a1"),
        native_user(&id_b, "u2"),
        Arc::new(AgentMessageKind::Resume),
        native_agent("a2"),
    ];
    let sliced = slice_messages_for_branch(&messages, 1);
    assert_eq!(sliced.len(), 3);
    assert!(matches!(&*sliced[0], AgentMessageKind::Resume));
    assert_eq!(sliced[1], messages[1]);
    assert_eq!(sliced[2], messages[2]);
}

#[test]
fn slice_zero_carry_forks_nothing() {
    // API floor: carry 0 = branch before the first turn. Unreachable from
    // the separator (it always renders after at least one user turn) but
    // pinned so the boundary arithmetic stays honest.
    let id_a = ClientUserMessageId::new();
    let messages = vec![native_user(&id_a, "u1"), native_agent("a1")];
    assert!(slice_messages_for_branch(&messages, 0).is_empty());
}

// --- transcript_entry_count (external ACP transcript fallback) ---

fn entry_user(id: Option<ClientUserMessageId>, _text: &str) -> AgentThreadEntry {
    AgentThreadEntry::UserMessage(acp_thread::UserMessage {
        identity: acp_thread::MessageIdentity::Legacy(None),
        meta: None,
        client_id: id,
        is_optimistic: false,
        // `content` is irrelevant to the count logic under test; an empty
        // `MessageContent` avoids needing a Markdown entity + cx.
        content: acp_thread::MessageContent::default(),
        indented: false,
    })
}

fn entry_assistant() -> AgentThreadEntry {
    AgentThreadEntry::AssistantMessage(acp_thread::AssistantMessage {
        chunks: vec![],
        indented: false,
        is_subagent_output: false,
    })
}

#[test]
fn transcript_carries_turn_tail_before_next_user_entry() {
    let entries = vec![
        entry_user(None, "u1"),
        entry_assistant(),
        entry_other(),
        entry_user(None, "u2"),
        entry_assistant(),
    ];
    // Carrying 1 turn keeps u1, its reply, and the turn's trailing marker;
    // only u2 and after are dropped.
    assert_eq!(transcript_entry_count(&entries, 1), 3);
}

#[test]
fn transcript_carry_at_turn_count_takes_whole_thread() {
    let entries = vec![
        entry_user(None, "u1"),
        entry_assistant(),
        entry_user(None, "u2"),
        entry_assistant(),
    ];
    assert_eq!(transcript_entry_count(&entries, 2), 4);
}

#[test]
fn transcript_carry_past_turn_count_takes_whole_thread() {
    let entries = vec![entry_user(None, "u1"), entry_assistant()];
    assert_eq!(transcript_entry_count(&entries, 9), 2);
}

#[test]
fn transcript_zero_carry_takes_nothing() {
    let entries = vec![entry_user(None, "u1"), entry_assistant()];
    assert_eq!(transcript_entry_count(&entries, 0), 0);
}

// --- turns_above_separator (turn-end separator branch point) ---

#[test]
fn turns_above_separator_counts_user_entries_through_the_separator() {
    let entries = vec![
        entry_user(None, "u1"),
        entry_assistant(),
        entry_user(None, "u2"),
        entry_assistant(),
    ];
    // Separator after a1 (turn 1): carry 1 turn.
    assert_eq!(turns_above_separator(&entries, 1), 1);
    // Separator after a2 (turn 2): carry both turns.
    assert_eq!(turns_above_separator(&entries, 3), 2);
}

#[test]
fn turns_above_separator_includes_trailing_turn_tail() {
    // The turn's trailing marker sits after the closing assistant message;
    // counting only up to the assistant (not past it) must still count the
    // turn's own user message.
    let entries = vec![entry_user(None, "u1"), entry_assistant(), entry_other()];
    assert_eq!(turns_above_separator(&entries, 1), 1);
}

// --- assistant_message_closes_turn (render_turn_end_separator guard) ---

// Stand-in for any non-user, non-assistant entry that can trail an
// assistant message at the end of a turn (canceled ToolCall, CompletedPlan
// snapshot, ContextCompaction marker). The closes-turn logic only
// discriminates on variant, not fields, so a ContextCompaction with no
// summary is the cheapest to construct without a cx.
use acp_thread::ContextCompactionId;
fn entry_other() -> AgentThreadEntry {
    AgentThreadEntry::ContextCompaction(acp_thread::ContextCompaction {
        id: ContextCompactionId(std::sync::Arc::from("")),
        status: acp_thread::ContextCompactionStatus::Completed,
        error: None,
        summary: acp_thread::MessageContent::default(),
        meta: None,
    })
}

#[test]
fn closes_turn_last_entry_is_vacuously_true() {
    // Assistant is the last entry — always closes the turn.
    let entries = vec![entry_user(None, "u1"), entry_assistant()];
    assert!(assistant_message_closes_turn(&entries, 1));
}

#[test]
fn closes_turn_next_is_user_message() {
    // Classic turn boundary: assistant immediately followed by next user.
    let entries = vec![
        entry_user(None, "u1"),
        entry_assistant(),
        entry_user(None, "u2"),
    ];
    assert!(assistant_message_closes_turn(&entries, 1));
}

#[test]
fn closes_turn_next_is_another_assistant_in_same_turn() {
    // Multi-step turn: assistant → tool → assistant. The first assistant
    // does NOT close the turn (there's a later assistant before the next
    // user message).
    let entries = vec![
        entry_user(None, "u1"),
        entry_assistant(),
        entry_other(),
        entry_assistant(),
    ];
    assert!(!assistant_message_closes_turn(&entries, 1));
    assert!(assistant_message_closes_turn(&entries, 3));
}

#[test]
fn closes_turn_trailing_other_entries_do_not_block() {
    // Regression for the either/or bug: turn ends on a trailing non-user,
    // non-assistant entry (e.g. a canceled ToolCall after Stop). The
    // preceding assistant message MUST still close the turn so "Branch
    // New Thread" shows alongside "Restore Checkpoint".
    let entries = vec![entry_user(None, "u1"), entry_assistant(), entry_other()];
    assert!(
        assistant_message_closes_turn(&entries, 1),
        "trailing non-assistant entry must not suppress the turn separator"
    );
}

#[test]
fn closes_turn_other_then_user_message() {
    // Compaction marker logged right before the next user message — the
    // assistant before it still closes the turn.
    let entries = vec![
        entry_user(None, "u1"),
        entry_assistant(),
        entry_other(),
        entry_user(None, "u2"),
    ];
    assert!(assistant_message_closes_turn(&entries, 1));
}
