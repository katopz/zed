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
    /// Local device name (present iff a client exists). Gates the mention
    /// scan in [`Self::on_snapshot`] — the scan needs only the name, not the
    /// client, so tests drive mention rounds without a worker via the seam.
    device_name: Option<String>,
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
    /// Unix ms of the last successful sync round (None before the first).
    last_synced_at: Option<i64>,
    /// Last sync-round failure, surfaced in the war room header so a dead
    /// worker is visible instead of silently stale.
    last_sync_error: Option<String>,
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
                device_name: None,
                resolved_room: None,
                snapshot: None,
                poll_task: None,
                mcp_server: None,
                realtime_client: None,
                poll_rounds: 0,
                last_synced_at: None,
                last_sync_error: None,
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

    /// Unix ms of the last successful sync round, if any.
    pub fn last_synced_at(&self) -> Option<i64> {
        self.last_synced_at
    }

    /// Web dashboard URL for the current room (`{worker}/?room={room}`),
    /// present only when connected — the panel links here so the operator
    /// never has to reconstruct (or hardcode) the URL by hand.
    pub fn dashboard_url(&self) -> Option<String> {
        if !self.connected() {
            return None;
        }
        let base = self.config.worker_url.trim_end_matches('/');
        let room = crate::client::urlencoding(&self.room());
        Some(format!("{base}/?room={room}"))
    }

    /// Last sync-round error (cleared on the next success).
    pub fn last_sync_error(&self) -> Option<&str> {
        self.last_sync_error.as_deref()
    }

    /// Whether the poll task exists. Local-only runtimes (empty `worker_url`)
    /// never start one — tests assert this to prove no network leaks.
    #[cfg(test)]
    pub(crate) fn has_poll_task(&self) -> bool {
        self.poll_task.is_some()
    }

    /// Test seam: drive mention rounds through [`Self::on_snapshot`] on an
    /// inert (local-only) runtime — the scan runs because a device name is
    /// set, no worker required.
    #[cfg(test)]
    pub(crate) fn set_device_name_for_tests(&mut self, device_name: &str) {
        self.device_name = Some(device_name.to_string());
    }

    // -----------------------------------------------------------------------
    // Start / lifecycle — moved verbatim from AgentBoardPanel (P0).
    // -----------------------------------------------------------------------

    /// Build the client from the config + a device identity. No-op (logs) when
    /// the worker URL or SSH key is missing — the board is strictly additive,
    /// so an unconfigured device simply falls back to local-only
    /// plan_registry.
    fn try_start(&mut self, cx: &mut Context<Self>) {
        if !self.config.enabled {
            log::info!(
                "[agent_board] board disabled (enabled=false; stack obsolete, issue 030); running local-only (no remote sync)"
            );
            return;
        }
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
        // Captured before the identity moves into the client — `on_snapshot`
        // scans mentions with just the name.
        let device_name = identity.device_name().to_string();
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
        self.device_name = Some(device_name);
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
            cx,
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
        // Replacing `poll_task` cancels the previous loop, so only one ever
        // runs — but `force_refresh` restarts it on every nudge, so the banner
        // logs once per process and restarts stay at debug.
        if self.poll_task.is_none() {
            log::info!(
                "[agent_board] runtime poll loop starting (single instance per process, interval {interval:?})"
            );
        } else {
            log::debug!("[agent_board] poll loop restarted for an immediate refresh");
        }

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
                        this.update(cx, |this, cx| {
                            this.last_sync_error = Some(format!("{error:#}"));
                            cx.notify();
                        })
                        .ok();
                    }
                }
                cx.background_executor().timer(interval).await;
            }
        });
        self.poll_task = Some(task);
    }

    /// Post-snapshot hook: scan mentions (P1), cache, notify views.
    /// Public so harnesses outside the crate (screenshot example, future
    /// embedders) can drive rounds without a worker.
    pub fn on_snapshot(&mut self, snapshot: RoomSnapshot, cx: &mut Context<Self>) {
        self.poll_rounds += 1;
        self.last_synced_at = Some(now_unix_ms());
        self.last_sync_error = None;
        log::debug!(
            "[agent_board] sync round #{} complete (room={})",
            self.poll_rounds,
            snapshot.room
        );
        if let Some(device_name) = self.device_name.as_deref() {
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

    /// SSE-event nudge: refresh immediately, but throttled to one round per
    /// ~2s so a chatty room doesn't turn the realtime stream into a GET flood.
    pub fn realtime_nudge(&mut self, cx: &mut Context<Self>) {
        let due = self
            .last_synced_at
            .map(|ts| now_unix_ms().saturating_sub(ts) >= 2_000)
            .unwrap_or(true);
        if due {
            self.force_refresh(cx);
        }
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

fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
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
    use crate::types::{BoardMessage, RoomSnapshot};
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
            // No worker → no dashboard link either (the 🌐 button hides).
            assert!(runtime.dashboard_url().is_none());
        });
    }

    /// Issue 030 kill switch: even with a worker_url fully configured, the
    /// obsolete stack must stay completely inert unless `enabled` is true.
    /// The 404 fake HTTP client fails the test run if any request escapes.
    #[gpui::test]
    async fn disabled_board_with_worker_url_stays_fully_inert(cx: &mut TestAppContext) {
        let config = AgentBoardConfig {
            worker_url: "https://agent-board.example.invalid".into(),
            realtime_enabled: true,
            ..AgentBoardConfig::default()
        };
        assert!(!config.enabled, "the obsolete stack must default to off");
        let runtime = cx.update(|cx| {
            BoardRuntime::init_global_with_config(inert_http(), config, cx)
        });
        cx.run_until_parked();
        runtime.read_with(cx, |runtime, _| {
            assert!(!runtime.connected());
            assert!(runtime.client.is_none(), "disabled board must not build a client");
            assert!(!runtime.has_poll_task(), "disabled board must not poll");
            assert!(runtime.mcp_server.is_none());
            assert!(runtime.realtime_client.is_none());
            assert_eq!(runtime.poll_rounds(), 0);
            assert!(runtime.dashboard_url().is_none());
        });
    }

    #[gpui::test]
    async fn poll_rounds_counts_each_snapshot_once_shared_by_all_views(cx: &mut TestAppContext) {
        let runtime = cx.update(|cx| {
            BoardRuntime::init_global_with_config(inert_http(), AgentBoardConfig::default(), cx)
        });
        // Sync metadata starts empty (local-only: never synced, no error).
        runtime.read_with(cx, |runtime, _| {
            assert!(runtime.last_synced_at().is_none());
            assert!(runtime.last_sync_error().is_none());
        });
        // Two rounds arrive (as the single poll loop would deliver them)…
        runtime.update(cx, |runtime, cx| runtime.on_snapshot(empty_snapshot(), cx));
        runtime.update(cx, |runtime, cx| runtime.on_snapshot(empty_snapshot(), cx));
        // …and every view of the shared runtime sees the same single counter,
        // plus the successful-sync metadata.
        cx.update(|cx| {
            let runtime = BoardRuntime::global(cx).read(cx);
            assert_eq!(runtime.poll_rounds(), 2);
            assert!(runtime.last_synced_at().is_some());
            assert!(runtime.last_sync_error().is_none());
        });
        runtime.read_with(cx, |runtime, _| assert_eq!(runtime.poll_rounds(), 2));
    }

    fn board_message(device_name: &str, sender: &str, text: &str, ts: i64) -> BoardMessage {
        BoardMessage {
            v: 1,
            device_id: String::new(),
            device_name: device_name.to_string(),
            sender: sender.to_string(),
            text: text.to_string(),
            ts,
        }
    }

    fn snapshot_with_messages(messages: Vec<BoardMessage>) -> RoomSnapshot {
        RoomSnapshot {
            v: 1,
            room: "test-room".to_string(),
            statuses: Vec::new(),
            messages,
            states: Vec::new(),
            replies: Vec::new(),
        }
    }

    /// The mention pipeline end-to-end through the runtime's post-poll hook:
    /// scan → loop guard → `inject_web_reply`. Also proves injection is
    /// panel-independent — no panel entity exists in this test at all, so
    /// mentions deliver with every panel closed.
    ///
    /// SOLE OWNER of the process-global mention state (watermark, guard,
    /// replies queue, unwatched counter): tests run in parallel in one
    /// process, so no other test may touch those statics.
    ///
    /// Covers the GOAT items: self-mention storm (dropped, no feedback),
    /// cooldown + hourly cap (log-and-suppress), agent → agent and web
    /// routing with the `📢 war-room` label, SSE/poll watermark sharing, and
    /// the local slice of the <1s 📡 injection bound.
    #[gpui::test]
    async fn mention_pipeline_storm_cooldown_cap_and_delivery(cx: &mut TestAppContext) {
        let runtime = cx.update(|cx| {
            BoardRuntime::init_global_with_config(inert_http(), AgentBoardConfig::default(), cx)
        });
        runtime.update(cx, |runtime, _| runtime.set_device_name_for_tests("m3"));

        // 60s cooldown / 3 per hour — small enough to force every branch.
        crate::mentions::configure_guard(60, 3);
        let _ = auto_prompt::peer_states::drain_web_replies();
        crate::mentions::clear_unwatched_mentions();

        // The base sits exactly on an hour boundary (4_102_444_800_000 is a
        // multiple of 3_600_000) and in 2100, above any realistic watermark;
        // every timestamp below is base + offset < 1h, so one hour bucket
        // holds for the whole test and the watermark (monotonic) is honored.
        let base = 4_102_444_800_000_i64;

        // ── Agent → agent: sibling m3:aaaa commands m3:f3a2 ──
        runtime.update(cx, |runtime, cx| {
            runtime.on_snapshot(
                snapshot_with_messages(vec![board_message(
                    "m3",
                    "m3:aaaa",
                    "@m3:f3a2 rebase please",
                    base,
                )]),
                cx,
            )
        });
        assert_eq!(
            auto_prompt::peer_states::drain_web_replies(),
            vec![
                ("f3a2".to_string(), "📢 war-room [@m3:aaaa] rebase please".to_string())
            ],
        );
        assert_eq!(crate::mentions::unwatched_mention_count(), 1);

        // ── Self-mention storm: 25 self-mentions in one round — all dropped,
        //    zero injections, no feedback loop ──
        runtime.update(cx, |runtime, cx| {
            runtime.on_snapshot(
                snapshot_with_messages(
                    (0..25)
                        .map(|i| {
                            board_message("m3", "m3:f3a2", "@m3:f3a2 keep going", base + 1 + i)
                        })
                        .collect(),
                ),
                cx,
            )
        });
        assert!(
            auto_prompt::peer_states::drain_web_replies().is_empty(),
            "self-mentions must never inject"
        );
        assert_eq!(crate::mentions::unwatched_mention_count(), 1);

        // ── Cooldown storm: 25 web mentions of the SAME target inside its
        //    cooldown window — every one suppressed, zero duplicates ──
        runtime.update(cx, |runtime, cx| {
            runtime.on_snapshot(
                snapshot_with_messages(
                    (0..25)
                        .map(|i| board_message("phone", "web", "@m3:f3a2 do it", base + 100 + i))
                        .collect(),
                ),
                cx,
            )
        });
        assert!(
            auto_prompt::peer_states::drain_web_replies().is_empty(),
            "cooldown must suppress the storm"
        );
        assert_eq!(crate::mentions::unwatched_mention_count(), 1);

        // ── Per-target isolation: the storm above locks f3a2, not bbbb —
        //    an agent's cooldown never silences its siblings ──
        runtime.update(cx, |runtime, cx| {
            runtime.on_snapshot(
                snapshot_with_messages(vec![board_message(
                    "phone",
                    "web",
                    "@m3:bbbb deploy",
                    base + 200,
                )]),
                cx,
            )
        });
        assert_eq!(
            auto_prompt::peer_states::drain_web_replies(),
            vec![("bbbb".to_string(), "📢 war-room [@web] deploy".to_string())],
        );
        assert_eq!(crate::mentions::unwatched_mention_count(), 2);

        // ── Hourly cap: `configure_guard` REPLACES the guard (fresh budget —
        //    production only calls it once at startup), so reconfigure with
        //    no cooldown to probe the cap in isolation: exactly 3 injections
        //    for f3a2 this hour, the rest log-and-suppress ──
        crate::mentions::configure_guard(0, 3);
        runtime.update(cx, |runtime, cx| {
            runtime.on_snapshot(
                snapshot_with_messages(vec![
                    board_message("phone", "web", "@m3:f3a2 two", base + 300),
                    board_message("phone", "web", "@m3:f3a2 three", base + 301),
                    board_message("phone", "web", "@m3:f3a2 four", base + 302),
                ]),
                cx,
            )
        });
        let replies = auto_prompt::peer_states::drain_web_replies();
        assert_eq!(replies.len(), 3, "a fresh budget allows exactly the cap");
        assert!(replies.iter().all(|(prefix, _)| prefix == "f3a2"));
        assert_eq!(crate::mentions::unwatched_mention_count(), 5);

        // A second storm the same hour: the cap is exhausted — zero more.
        runtime.update(cx, |runtime, cx| {
            runtime.on_snapshot(
                snapshot_with_messages(vec![
                    board_message("phone", "web", "@m3:f3a2 five", base + 310),
                    board_message("phone", "web", "@m3:f3a2 six", base + 311),
                    board_message("phone", "web", "@m3:f3a2 seven", base + 312),
                ]),
                cx,
            )
        });
        assert!(
            auto_prompt::peer_states::drain_web_replies().is_empty(),
            "the exhausted hourly cap must suppress further mentions"
        );
        assert_eq!(crate::mentions::unwatched_mention_count(), 5);

        // ── The SSE push path shares the watermark with the poll path: a
        //    pushed message delivers once; the same push again is a no-op ──
        let pushed = board_message("phone", "web", "@m3:cccc fresh", base + 400);
        assert_eq!(
            crate::mentions::handle_board_message(&pushed, "m3"),
            crate::mentions::ScanOutcome::Routed
        );
        assert_eq!(
            auto_prompt::peer_states::drain_web_replies(),
            vec![("cccc".to_string(), "📢 war-room [@web] fresh".to_string())],
        );
        assert_eq!(
            crate::mentions::handle_board_message(&pushed, "m3"),
            crate::mentions::ScanOutcome::NotRouted,
            "a re-pushed message is below the watermark — no double delivery"
        );
        assert!(auto_prompt::peer_states::drain_web_replies().is_empty());
        assert_eq!(crate::mentions::unwatched_mention_count(), 6);

        // ── Latency: one round over a 100-message snapshot (scan + guard +
        //    inject). The GOAT bound is <1s end-to-end with 📡 on; this local
        //    slice must sit far under it — network dominates the real bound. ──
        let burst: Vec<BoardMessage> = (0..100)
            .map(|i| board_message("phone", "web", &format!("@m3:dddd cmd {i}"), base + 500 + i))
            .collect();
        let snapshot = snapshot_with_messages(burst);
        let started = std::time::Instant::now();
        runtime.update(cx, |runtime, cx| runtime.on_snapshot(snapshot, cx));
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "snapshot→injection took {elapsed:?}; GOAT bound is 1s"
        );
        // dddd is a fresh target: the cap bounds the burst to three injections.
        assert_eq!(
            auto_prompt::peer_states::drain_web_replies().len(),
            3,
            "the hourly cap bounds the burst"
        );
        assert_eq!(crate::mentions::unwatched_mention_count(), 9);

        // Leave the process-global state tidy for any future test.
        let _ = auto_prompt::peer_states::drain_web_replies();
        crate::mentions::clear_unwatched_mentions();
    }
}
