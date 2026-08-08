//! Writer bridge: implements `auto_prompt::peer_states::AgentStateBroadcaster`
//! so auto_prompt can broadcast agent states to the board without depending on
//! agent_board (breaking the circular dependency). The panel registers the
//! broadcaster during `try_start`; when the board isn't configured, no
//! broadcaster is registered and `broadcast_state` is a silent no-op.

use std::sync::{Arc, RwLock};

use auto_prompt::peer_states::AgentStateBroadcaster;
use crate::client::BoardClient;
use crate::types::{truncate_to_byte_budget, MAX_STATE_TEXT_BYTES};

/// Global: writer handle (client + room) set by the panel after `try_start`.
static WRITER: RwLock<Option<WriterHandle>> = RwLock::new(None);

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

/// Clear the writer handle. Test-only.
#[cfg(test)]
pub fn clear_for_test() {
    if let Ok(mut guard) = WRITER.write() {
        *guard = None;
    }
}
