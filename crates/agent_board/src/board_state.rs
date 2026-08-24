//! Writer bridge: implements `auto_prompt::peer_states::AgentStateBroadcaster`
//! so auto_prompt can broadcast agent states to the board without depending on
//! agent_board (breaking the circular dependency). The panel registers the
//! broadcaster during `try_start`; when the board isn't configured, no
//! broadcaster is registered and `broadcast_state` is a silent no-op.

use std::sync::{Arc, RwLock};

use auto_prompt::peer_states::{AgentStateBroadcaster, ThreadEntry};
use crate::client::BoardClient;
use crate::types::{
    truncate_to_byte_budget, PostMessageBody, PostThreadBody, RoomSnapshot, ThreadEntryWire,
    MAX_STATE_TEXT_BYTES, MAX_THREAD_ENTRY_BYTES,
};

/// Global: writer handle (client + room) set by the panel after `try_start`.
static WRITER: RwLock<Option<WriterHandle>> = RwLock::new(None);

/// Global: latest room snapshot, set by the feeder after each poll round.
/// Read by the MCP tool (`GetAgentRoom`) and any other consumer that needs the
/// full room state (devices + states + messages) without holding a GPUI entity
/// handle. Stored as an `Arc` so readers never block on a clone.
static ROOM_SNAPSHOT: RwLock<Option<Arc<RoomSnapshot>>> = RwLock::new(None);

/// Serializes tests that mutate the crate-global state above. Test binaries
/// run `#[test]`s in parallel, so `clear_for_test`/`set_room_snapshot` calls
/// from different tests race without this lock.
#[cfg(test)]
use std::sync::Mutex;

#[cfg(test)]
pub(crate) static TEST_LOCK: Mutex<()> = Mutex::new(());

struct WriterHandle {
    client: Arc<BoardClient>,
    room: String,
    device_name: String,
    executor: gpui::BackgroundExecutor,
}

/// Implementation of `AgentStateBroadcaster` backed by the board client.
/// Stored globally so the trait object can be cheaply cloned (Arc) and
/// registered with auto_prompt once at panel init.
struct BoardBroadcaster;

impl AgentStateBroadcaster for BoardBroadcaster {
    fn broadcast(
        &self,
        session_id: &str,
        sub_agent_id: Option<&str>,
        state_text: &str,
        meta: &str,
    ) {
        let handle = match WRITER.read() {
            Ok(guard) => guard.as_ref().map(|h| {
                (
                    h.client.clone(),
                    h.room.clone(),
                    h.device_name.clone(),
                    h.executor.clone(),
                )
            }),
            Err(_) => None,
        };
        let Some((client, room, device_name, executor)) = handle else {
            return;
        };

        let body = crate::types::PostStateBody {
            device_name,
            session_id: session_id.to_string(),
            sub_agent_id: sub_agent_id.map(|s| s.to_string()),
            state_text: truncate_to_byte_budget(state_text, MAX_STATE_TEXT_BYTES),
            meta: truncate_to_byte_budget(meta, MAX_STATE_TEXT_BYTES),
        };

        executor
            .spawn(async move {
                if let Err(error) = client.post_state(&room, body).await {
                    log::debug!("[agent_board] post_state failed: {error:#}");
                }
            })
            .detach();
    }

    fn broadcast_thread_update(
        &self,
        session_id: &str,
        title: Option<&str>,
        entries: &[ThreadEntry],
    ) {
        if entries.is_empty() {
            return;
        }
        let handle = match WRITER.read() {
            Ok(guard) => guard.as_ref().map(|h| {
                (
                    h.client.clone(),
                    h.room.clone(),
                    h.device_name.clone(),
                    h.executor.clone(),
                )
            }),
            Err(_) => None,
        };
        let Some((client, room, device_name, executor)) = handle else {
            return;
        };

        let body = PostThreadBody {
            device_name,
            session_id: session_id.to_string(),
            title: title.map(|t| truncate_to_byte_budget(t, MAX_STATE_TEXT_BYTES)),
            entries: entries
                .iter()
                .map(|entry| ThreadEntryWire {
                    seq: entry.seq,
                    role: truncate_to_byte_budget(&entry.role, 16),
                    text: truncate_to_byte_budget(&entry.text, MAX_THREAD_ENTRY_BYTES),
                    ts: 0,
                })
                .collect(),
        };

        executor
            .spawn(async move {
                if let Err(error) = client.post_thread(&room, body).await {
                    log::debug!("[agent_board] post_thread failed: {error:#}");
                }
            })
            .detach();
    }
}

/// Register the writer handle + broadcaster. Called by the panel after
/// `try_start` succeeds. Stores the client/room/executor so `BoardBroadcaster`
/// can use them, and registers the trait object with auto_prompt. Pass `None`
/// to clear (board disabled).
pub fn register_writer(
    client: Option<Arc<BoardClient>>,
    room: Option<String>,
    executor: gpui::BackgroundExecutor,
) {
    let handle = match (client, room) {
        (Some(client), Some(room)) => {
            let device_name = client.identity().device_name().to_string();
            Some(WriterHandle {
                client,
                room,
                device_name,
                executor,
            })
        }
        _ => None,
    };
    if let Ok(mut guard) = WRITER.write() {
        *guard = handle;
    }
    // Register the broadcaster trait object with auto_prompt. When handle is
    // None (board disabled), we still register the broadcaster — it will just
    // find no writer and silently skip. This avoids re-registration churn.
    auto_prompt::peer_states::register_broadcaster(Some(Arc::new(BoardBroadcaster)));
}

/// Store the latest room snapshot. Called by the feeder after each successful
/// poll round. Wrapped in `Arc` for cheap concurrent reads.
pub fn set_room_snapshot(snapshot: RoomSnapshot) {
    if let Ok(mut guard) = ROOM_SNAPSHOT.write() {
        *guard = Some(Arc::new(snapshot));
    }
}

/// Read the latest room snapshot (clone of the `Arc`). Returns `None` when no
/// poll has succeeded yet.
pub fn current_room_snapshot() -> Option<Arc<RoomSnapshot>> {
    ROOM_SNAPSHOT.read().ok()?.as_ref().cloned()
}

/// This device's name, when the board is configured. Used to label local
/// claims in the work-board projection and to accent own messages in feeds.
pub fn device_name() -> Option<String> {
    WRITER
        .read()
        .ok()
        .and_then(|guard| guard.as_ref().map(|handle| handle.device_name.clone()))
}

/// Post a message to the room feed without holding a GPUI entity handle
/// (Plan 024 P4). Fire-and-forget: clones the writer, spawns the POST on the
/// background executor, logs failures at debug. `sender` labels the composer
/// (`"operator"`, `"web"`, or a posting agent's `device:sess4`).
pub fn post_message(text: &str, sender: &str) {
    let handle = match WRITER.read() {
        Ok(guard) => guard.as_ref().map(|handle| {
            (
                handle.client.clone(),
                handle.room.clone(),
                handle.device_name.clone(),
                handle.executor.clone(),
            )
        }),
        Err(_) => None,
    };
    let Some((client, room, device_name, executor)) = handle else {
        log::debug!("[agent_board] board not configured; dropping post_message");
        return;
    };
    let body = PostMessageBody {
        device_name,
        sender: truncate_to_byte_budget(sender, 64),
        text: truncate_to_byte_budget(text, 1024),
    };
    executor
        .spawn(async move {
            if let Err(error) = client.post_message(&room, body).await {
                log::debug!("[agent_board] post_message failed: {error:#}");
            }
        })
        .detach();
}

/// Clear the writer handle. Test-only.
#[cfg(test)]
pub fn clear_for_test() {
    if let Ok(mut guard) = WRITER.write() {
        *guard = None;
    }
    if let Ok(mut guard) = ROOM_SNAPSHOT.write() {
        *guard = None;
    }
}
