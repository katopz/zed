//! Process-global board runtime (Plan 024 P0).
//!
//! Owns everything with a side effect: config, identity, signed client, the
//! 15s poll loop, the SSE client, and the in-process MCP server. Extracted
//! from `AgentBoardPanel` so any number of panel views (the status board, the
//! war room) can share ONE poll loop — the panel-owned version would double-
//! poll the moment a second panel appeared.
//!
//! Each sync round: GET snapshot → inject remote claims → drain replies →
//! scan mentions (P1) → post our status → cache globally → notify views.

use std::sync::Arc;
use std::time::Duration;

use gpui::{App, AppContext, AsyncApp, Context, Entity, Global, Task};
use http_client::HttpClient;

use crate::client::BoardClient;
use crate::identity::DeviceIdentity;
use crate::types::RoomSnapshot;
use crate::AgentBoardConfig;

struct GlobalBoardRuntime(Entity<BoardRuntime>);

impl Global for GlobalBoardRuntime {}

/// The single owner of the board's network + MCP surface. Views observe it;
/// nothing else talks to the worker.
pub struct BoardRuntime {
    http: Arc<dyn HttpClient>,
    config: AgentBoardConfig,
    /// Lazily-built client once an identity + worker URL are available.
    client: Option<Arc<BoardClient>>,
    /// Room name resolved from config (explicit) or identity (derived).
    resolved_room: Option<String>,
    /// Latest snapshot, mirrored for panel rendering.
    snapshot: Option<RoomSnapshot>,
    /// Background poll task; exactly one per process.
    poll_task: Option<Task<()>>,
    /// In-process MCP server (`get_agent_room` + `post_agent_board_message`),
    /// held alive to keep the Unix socket listening.
    mcp_server: Option<context_server::listener::McpServer>,
    /// Real-time SSE client (📡 toggle).
    realtime_client: Option<crate::realtime_client::RealtimeClient>,
    /// Completed poll rounds — the single-poll-loop GOAT check counts these.
    poll_rounds: u64,
}

impl BoardRuntime {
    /// Create the global runtime entity, loading the config from disk. Safe
    /// to call multiple times; the first entity wins and later calls return
    /// it (panels never spawn a second poll loop).
    pub fn init_global(http: Arc<dyn HttpClient>, cx: &mut App) -> Entity<Self> {
        let config = AgentBoardConfig::load();
        Self::init_global_with_config(http, config, cx)
    }

    /// Config-injecting variant of [`Self::init_global`] for callers that
    /// already hold a config, and for hermetic tests: a default config has an
    /// empty `worker_url`, so the runtime stays fully inert (no poll loop, no
    /// MCP socket, no SSE — no network escapes the test).
    pub fn init_global_with_config(
        http: Arc<dyn HttpClient>,
        config: AgentBoardConfig,
        cx: &mut App,
    ) -> Entity<Self> {
        if let Some(existing) = Self::try_global(cx) {
            return existing;
        }
        let runtime = cx.new(|cx| {
            let mut runtime = Self {
                http,
                config,
                client: None,
                resolved_room: None,
                snapshot: None,
                poll_task: None,
                mcp_server: None,
                realtime_client: None,
                poll_rounds: 0,
            };
            runtime.try_start(cx);
            runtime
        });
        cx.set_global(GlobalBoardRuntime(runtime.clone()));
        runtime
    }

    pub fn global(cx: &App) -> Entity<Self> {
        cx.global::<GlobalBoardRuntime>().0.clone()
    }

    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalBoardRuntime>()
            .map(|global| global.0.clone())
    }

    // -----------------------------------------------------------------------
    // Read accessors for views.
    // -----------------------------------------------------------------------

    pub fn snapshot(&self) -> Option<&RoomSnapshot> {
        self.snapshot.as_ref()
    }

    pub fn config(&self) -> &AgentBoardConfig {
        &self.config
    }

    pub fn connected(&self) -> bool {
        self.client.is_some()
    }

    pub fn room(&self) -> String {
        self.resolved_room
            .clone()
            .unwrap_or_else(|| self.config.room.clone())
    }

    pub fn realtime_enabled(&self) -> bool {
        self.config.realtime_enabled
    }

    pub fn poll_rounds(&self) -> u64 {
        self.poll_rounds
    }

    /// Whether the poll task exists. Local-only runtimes (empty `worker_url`)
    /// never start one — tests assert this to prove no network leaks.
    #[cfg(test)]
    pub(crate) fn has_poll_task(&self) -> bool {
        self.poll_task.is_some()
    }

    // -----------------------------------------------------------------------
    // Start / lifecycle — moved verbatim from AgentBoardPanel (P0).
    // -----------------------------------------------------------------------

    /// Build the client from the config + a device identity. No-op (logs) when
    /// the worker URL or SSH key is missing — the board is strictly additive,
    /// so an unconfigured device simply falls back to local-only
    /// plan_registry.
    fn try_start(&mut self, cx: &mut Context<Self>) {
        if self.config.worker_url.trim().is_empty() {
            log::info!(
                "[agent_board] worker_url not set in config; running local-only (no remote sync)"
            );
            return;
        }
        let key_path = match crate::identity::expand_ssh_path(&self.config.ssh_key_path) {
            Ok(path) => path,
            Err(error) => {
                log::warn!("[agent_board] ssh key not found: {error:#}; not starting sync");
                return;
            }
        };
        let device_name = hostname();
        let location_hash = crate::identity::location_hash();
        let identity = match DeviceIdentity::load(&key_path, device_name, location_hash) {
            Ok(identity) => identity,
            Err(error) => {
                log::warn!("[agent_board] failed to load device identity: {error:#}");
                return;
            }
        };
        let identity = Arc::new(identity);
        // Room = hash(ssh-key) by default so two devices sharing a key auto-join
        // the same room; an explicit config.room overrides.
        let room = if self.config.room.trim().is_empty() {
            identity.room_id().to_string()
        } else {
            self.config.room.clone()
        };
        let client = Arc::new(BoardClient::new(
            self.http.clone(),
            self.config.worker_url.clone(),
            identity,
        ));
        let muted: Vec<auto_prompt::peer_states::PeerStateMute> = self
            .config
            .muted
            .iter()
            .map(|m| auto_prompt::peer_states::PeerStateMute {
                device_id: m.device_id.clone(),
                session_id: m.session_id.clone(),
                sub_agent_id: m.sub_agent_id.clone(),
            })
            .collect();
        auto_prompt::peer_states::set_muted(muted);
        crate::board_state::register_writer(
            Some(client.clone()),
            Some(room.clone()),
            cx.background_executor().clone(),
        );
        crate::mentions::configure_guard(
            self.config.mention_cooldown_secs,
            self.config.mention_max_per_hour,
        );
        self.resolved_room = Some(room);
        self.client = Some(client);
        self.start_poll(cx);
        self.start_mcp_server(cx);
        if self.config.realtime_enabled {
            self.start_realtime(cx);
        }
    }

    fn start_mcp_server(&mut self, cx: &mut Context<Self>) {
        if self.mcp_server.is_some() {
            return;
        }
        cx.spawn(async move |this, cx: &mut AsyncApp| {
            let server_task = context_server::listener::McpServer::new(cx);
            match server_task.await {
                Ok(mut server) => {
                    server.add_tool(crate::mcp_tools::GetAgentRoom);
                    server.add_tool(crate::mcp_tools::PostAgentBoardMessage);
                    log::info!(
                        "[agent_board] MCP server listening at {} (tools: get_agent_room, post_agent_board_message)",
                        server.socket_path().display()
                    );
                    let _ = this.update(cx, |this, cx| {
                        this.mcp_server = Some(server);
                        cx.notify();
                    });
                }
                Err(error) => {
                    log::warn!("[agent_board] failed to start MCP server: {error:#}");
                }
            }
        })
        .detach();
    }

    /// Start or restart the real-time SSE client (📡 toggle / room change).
    fn start_realtime(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let Some(room) = self.resolved_room.clone() else {
            return;
        };
        let identity = client.identity().clone();
        let base_url = self.config.worker_url.trim_end_matches('/').to_string();
        self.realtime_client = Some(crate::realtime_client::RealtimeClient::start(
            self.http.clone(),
            base_url,
            room,
            identity,
            cx.background_executor().clone(),
        ));
        log::info!("[agent_board] real-time SSE client started (📡 toggle ON)");
    }

    /// Toggle the real-time push on/off. Called by the 📡 button on either panel.
    pub fn toggle_realtime(&mut self, cx: &mut Context<Self>) {
        self.config.realtime_enabled = !self.config.realtime_enabled;
        if let Err(error) = self.config.save() {
            log::warn!("[agent_board] failed to save config: {error:#}");
        }
        if self.config.realtime_enabled {
            self.start_realtime(cx);
        } else {
            self.realtime_client = None;
            log::info!("[agent_board] real-time SSE client stopped (📡 toggle OFF)");
        }
        cx.notify();
    }

    /// Start (or restart) the single poll loop. Restarting is also the cheap
    /// "force refresh" path: it fires an immediate round without a second
    /// concurrent fetch.
    fn start_poll(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let identity = client.identity().clone();
        let Some(room) = self.resolved_room.clone() else {
            return;
        };
        let interval = Duration::from_secs(self.config.poll_interval_secs.max(5));
        log::info!(
            "[agent_board] runtime poll loop starting (single instance per process, interval {interval:?})"
        );

        let task = cx.spawn(async move |this, cx: &mut AsyncApp| {
            loop {
                let client = client.clone();
                let identity = identity.clone();
                let room = room.clone();
                let result = cx
                    .background_spawn(async move {
                        let project_path = String::new();
                        let local_session_id = String::new();
                        crate::feeder::sync_round(
                            &client,
                            &identity,
                            &room,
                            &project_path,
                            &local_session_id,
                        )
                        .await
                    })
                    .await;
                match result {
                    Ok(snapshot) => {
                        this.update(cx, |this, cx| this.on_snapshot(snapshot, cx))
                            .ok();
                    }
                    Err(error) => {
                        log::debug!("[agent_board] sync round failed: {error:#}");
                    }
                }
                cx.background_executor().timer(interval).await;
            }
        });
        self.poll_task = Some(task);
    }

    /// Post-snapshot hook: scan mentions (P1), cache, notify views.
    /// `pub(crate)` so panel tests can drive rounds without a worker.
    pub(crate) fn on_snapshot(&mut self, snapshot: RoomSnapshot, cx: &mut Context<Self>) {
        self.poll_rounds += 1;
        log::debug!(
            "[agent_board] sync round #{} complete (room={})",
            self.poll_rounds,
            snapshot.room
        );
        if let Some(client) = &self.client {
            let device_name = client.identity().device_name();
            let watermark = crate::mentions::watermark_ms();
            let scan =
                crate::feeder::extract_mentions_for_device(&snapshot, device_name, watermark);
            for route in &scan.routes {
                crate::mentions::inject_route(route);
            }
            crate::mentions::advance_watermark(scan.new_watermark_ts);
        }
        self.snapshot = Some(snapshot);
        cx.notify();
    }

    /// Nudge the poll loop for an immediate round (used after local writes so
    /// the panel reflects them without waiting out the interval).
    pub fn force_refresh(&mut self, cx: &mut Context<Self>) {
        self.start_poll(cx);
    }

    /// Set the room name and restart everything bound to it.
    pub fn set_room(&mut self, room: String, cx: &mut Context<Self>) {
        self.config.room = room.trim().to_string();
        if let Err(error) = self.config.save() {
            log::warn!("[agent_board] could not persist config: {error:#}");
        }
        self.client = None;
        self.resolved_room = None;
        self.realtime_client = None;
        self.try_start(cx);
        cx.notify();
    }

    /// Post a message to the room feed as the local operator.
    pub fn post_message(&mut self, text: String, cx: &mut Context<Self>) {
        self.post_message_as(text, "operator", cx);
    }

    /// Post a message with an explicit sender label (`"operator"`, `"web"`, or
    /// a posting agent's `device:sess4`).
    pub fn post_message_as(&mut self, text: String, sender: &str, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            log::warn!("[agent_board] not connected to a worker; cannot post message");
            return;
        };
        let room = self.room();
        let device_name = client.identity().device_name().to_string();
        let body = crate::types::PostMessageBody {
            device_name,
            sender: sender.to_string(),
            text,
        };
        cx.spawn(async move |this, cx: &mut AsyncApp| {
            let result = cx
                .background_spawn(async move { client.post_message(&room, body).await })
                .await;
            match result {
                Ok(_) => {
                    let _ = this.update(cx, |this, cx| this.force_refresh(cx));
                }
                Err(error) => {
                    log::warn!("[agent_board] post_message failed: {error:#}");
                }
            }
        })
        .detach();
    }

    /// Toggle mute on a specific agent state; persists config and updates the
    /// auto_prompt runtime filter immediately.
    pub fn toggle_mute(&mut self, key: crate::types::MuteKey, cx: &mut Context<Self>) {
        if let Some(pos) = self.config.muted.iter().position(|m| *m == key) {
            self.config.muted.remove(pos);
        } else {
            self.config.muted.push(key);
        }
        if let Err(error) = self.config.save() {
            log::warn!("[agent_board] could not persist muted set: {error:#}");
        }
        let muted: Vec<auto_prompt::peer_states::PeerStateMute> = self
            .config
            .muted
            .iter()
            .map(|m| auto_prompt::peer_states::PeerStateMute {
                device_id: m.device_id.clone(),
                session_id: m.session_id.clone(),
                sub_agent_id: m.sub_agent_id.clone(),
            })
            .collect();
        auto_prompt::peer_states::set_muted(muted);
        cx.notify();
    }
}

fn hostname() -> String {
    sysinfo::System::host_name().unwrap_or_else(|| "unknown".to_string())
}

// ---------------------------------------------------------------------------
// Tests — the single-poll-loop invariant (Plan 024 P0 / GOAT gate), verified
// hermetically: a default config keeps the runtime inert, `init_global` is
// idempotent, and `poll_rounds` counts snapshots once per round regardless of
// how many panel views exist.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::RoomSnapshot;
    use gpui::TestAppContext;

    fn inert_http() -> Arc<dyn HttpClient> {
        // Local-only runtimes never issue requests; if one ever does, the 404
        // fails the round instead of hitting the network.
        http_client::FakeHttpClient::with_404_response()
    }

    fn empty_snapshot() -> RoomSnapshot {
        RoomSnapshot {
            v: 1,
            room: "test-room".to_string(),
            statuses: Vec::new(),
            messages: Vec::new(),
            states: Vec::new(),
            replies: Vec::new(),
        }
    }

    #[gpui::test]
    async fn init_is_idempotent_one_runtime_per_process(cx: &mut TestAppContext) {
        let first = cx.update(|cx| {
            BoardRuntime::init_global_with_config(inert_http(), AgentBoardConfig::default(), cx)
        });
        let second = cx.update(|cx| {
            BoardRuntime::init_global_with_config(inert_http(), AgentBoardConfig::default(), cx)
        });
        let global = cx.update(|cx| BoardRuntime::global(cx));
        assert_eq!(
            first.entity_id(),
            second.entity_id(),
            "re-init must return the existing entity, not spawn a second poll loop"
        );
        assert_eq!(first.entity_id(), global.entity_id());
    }

    #[gpui::test]
    async fn local_only_config_starts_no_poll_task_or_socket(cx: &mut TestAppContext) {
        let runtime = cx.update(|cx| {
            BoardRuntime::init_global_with_config(inert_http(), AgentBoardConfig::default(), cx)
        });
        cx.run_until_parked();
        runtime.read_with(cx, |runtime, _| {
            assert!(!runtime.connected());
            assert!(!runtime.has_poll_task(), "local-only runtime must not poll");
            assert!(runtime.mcp_server.is_none());
            assert!(runtime.realtime_client.is_none());
            assert_eq!(runtime.poll_rounds(), 0);
        });
    }

    #[gpui::test]
    async fn poll_rounds_counts_each_snapshot_once_shared_by_all_views(cx: &mut TestAppContext) {
        let runtime = cx.update(|cx| {
            BoardRuntime::init_global_with_config(inert_http(), AgentBoardConfig::default(), cx)
        });
        // Two rounds arrive (as the single poll loop would deliver them)…
        runtime.update(cx, |runtime, cx| runtime.on_snapshot(empty_snapshot(), cx));
        runtime.update(cx, |runtime, cx| runtime.on_snapshot(empty_snapshot(), cx));
        // …and every view of the shared runtime sees the same single counter.
        cx.update(|cx| {
            assert_eq!(BoardRuntime::global(cx).read(cx).poll_rounds(), 2);
        });
        runtime.read_with(cx, |runtime, _| assert_eq!(runtime.poll_rounds(), 2));
    }
}
