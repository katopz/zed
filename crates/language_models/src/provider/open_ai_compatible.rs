use anyhow::Result;
use convert_case::{Case, Casing};
use credentials_provider::CredentialsProvider;
use fs::Fs;
use futures::{FutureExt, StreamExt, future::BoxFuture};
use gpui::{App, AsyncApp, Context, ElementId, Entity, SharedString, Task, TaskExt, Window};
use http_client::{CustomHeaders, HttpClient};
use parking_lot::Mutex as ParkingMutex;
use language_model::{
    ApiKeyState, AuthenticateError, EnvVar, IconOrSvg, LanguageModel, LanguageModelCompletionError,
    LanguageModelCompletionEvent, LanguageModelEffortLevel, LanguageModelId, LanguageModelName,
    LanguageModelProvider, LanguageModelProviderId, LanguageModelProviderName,
    LanguageModelProviderState, LanguageModelRequest, LanguageModelToolChoice,
    LanguageModelToolSchemaFormat, ModelKeySlotStatus, ModelKeySlotStatusSummary, ProviderSettingsView,
    RateLimiter, SubPageProviderSettings,
};
use open_ai::{
    ResponseStreamEvent,
    responses::{Request as ResponseRequest, StreamEvent as ResponsesStreamEvent, stream_response},
    stream_completion,
};
use settings::{Settings, SettingsStore};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use ui::{ElevationIndex, Tooltip, prelude::*};
use ui_input::InputField;
use util::ResultExt;

use crate::provider::open_ai::{
    OpenAiEventMapper, OpenAiResponseEventMapper, into_open_ai, into_open_ai_response,
};
pub use settings::OpenAiCompatibleAvailableModel as AvailableModel;
pub use settings::OpenAiCompatibleModelCapabilities as ModelCapabilities;

// Re-exported so the agent_ui footer chips can format backoff countdowns the
// same way the ConfigurationView does, without duplicating the formatter.
pub use health::format_backoff_remaining;

mod health;
use health::{
    KeyHealthTracker, KeySlot, SlotHealthStatus,
    key_health_path_for, record_key_success, reload_persisted_health, retry_stream,
    schedule_persist_key_health_inner, snapshot_health,
};

/// Placeholder text shown in the (empty) primary/secondary/tertiary API key
/// input fields.
const API_KEY_PLACEHOLDER: &str = "000000000000000000000000000000000000000000000000000";

#[derive(Default, Clone, Debug, PartialEq)]
pub struct OpenAiCompatibleSettings {
    pub api_url: String,
    pub available_models: Vec<AvailableModel>,
    pub custom_headers: CustomHeaders,
}

pub struct OpenAiCompatibleLanguageModelProvider {
    id: LanguageModelProviderId,
    name: LanguageModelProviderName,
    http_client: Arc<dyn HttpClient>,
    state: Entity<State>,
}

pub struct State {
    id: Arc<str>,
    api_key_state: ApiKeyState,
    api_key_state_2: ApiKeyState,
    api_key_state_3: ApiKeyState,
    api_key_state_4: ApiKeyState,
    /// Shared across threads so the request closure (background executor) can
    /// record outcomes without going through GPUI's `Entity::update`.
    /// `parking_lot::Mutex` (not `std::sync::Mutex`) because the background
    /// executor's thread pool contends here on every inference request, and
    /// `parking_lot` spins before parking the OS thread — measurably better
    /// under subagent fan-out.
    key_health: Arc<ParkingMutex<KeyHealthTracker>>,
    /// Latest pending debounced save task. Replacing this cancels the prior
    /// task, coalescing bursts of failures into a single disk write.
    key_health_dirty: Arc<ParkingMutex<Option<Task<()>>>>,
    /// Path under `paths::data_dir()` where `key_health` is persisted.
    key_health_path: PathBuf,
    settings: OpenAiCompatibleSettings,
    credentials_provider: Arc<dyn CredentialsProvider>,
}

/// Derives a distinct keychain identifier for the secondary API key from the provider URL.
fn secondary_key_url(api_url: &str) -> SharedString {
    SharedString::new(format!("{api_url}#secondary"))
}

/// Derives a distinct keychain identifier for the tertiary API key from the provider URL.
fn tertiary_key_url(api_url: &str) -> SharedString {
    SharedString::new(format!("{api_url}#tertiary"))
}

/// Derives a distinct keychain identifier for the quaternary API key from the provider URL.
fn quaternary_key_url(api_url: &str) -> SharedString {
    SharedString::new(format!("{api_url}#quaternary"))
}

/// Char-safe truncated preview of an API key for display: first 3 + `...` +
/// last 3 characters. Returns the key unchanged if it's too short (<= 8 chars)
/// to be distinctive without revealing the whole value. Uses `chars()` rather
/// than byte slicing so multi-byte UTF-8 keys (rare, but possible for
/// user-supplied OpenAI-compatible secrets) never panic mid-character.
fn truncate_key_preview(key: &str) -> String {
    const HEAD: usize = 3;
    const TAIL: usize = 3;
    let char_count = key.chars().count();
    if char_count <= HEAD + TAIL {
        return key.to_string();
    }
    let head: String = key.chars().take(HEAD).collect();
    let tail: String = key.chars().rev().take(TAIL).collect::<Vec<_>>().into_iter().rev().collect();
    format!("{head}...{tail}")
}

/// Outcome of a manual "Check" probe of a single key. Stored per-slot in the
/// ConfigurationView so the button face shows the latest result for that key.
#[derive(Clone, Debug, PartialEq)]
pub enum KeyProbeResult {
    /// The probe completed without an error.
    Ok,
    /// The upstream returned a rate-limit error (429).
    RateLimit,
    /// Any other error; the message is surfaced as a tooltip.
    Err(SharedString),
}

/// Maps a `KeySlot` to its fixed index in the `[Primary, Secondary, Tertiary, Quaternary]`
/// arrays used by `ConfigurationView` (`probe_results`, `probe_tasks`). Kept as
/// a single helper so the two array sites and the `KeySlot` enum can't drift.
fn slot_index(slot: KeySlot) -> usize {
    match slot {
        KeySlot::Primary => 0,
        KeySlot::Secondary => 1,
        KeySlot::Tertiary => 2,
        KeySlot::Quaternary => 3,
    }
}

/// Inverse of [`slot_index`]. Used by `LanguageModel::set_key_slot_enabled`
/// (which takes a `usize` from the UI) to map back to a `KeySlot`. Returns
/// `None` for out-of-range indices so a stale UI can't panic the provider.
fn slot_from_index(index: usize) -> Option<KeySlot> {
    match index {
        0 => Some(KeySlot::Primary),
        1 => Some(KeySlot::Secondary),
        2 => Some(KeySlot::Tertiary),
        3 => Some(KeySlot::Quaternary),
        _ => None,
    }
}

/// Owned bundle of inputs needed to fire a single-key probe request from the
/// UI thread. Built by `State::probe_inputs` and moved into a background task
/// so the probe never holds a borrow on `State`.
struct KeyProbeInputs {
    api_key: Arc<str>,
    model: String,
    api_url: Arc<str>,
    extra_headers: CustomHeaders,
    provider_name: Arc<str>,
}

impl State {
    fn is_authenticated(&self) -> bool {
        self.api_key_state.has_key()
            || self.api_key_state_2.has_key()
            || self.api_key_state_3.has_key()
            || self.api_key_state_4.has_key()
    }

    /// Schedules a debounced write of the current `key_health` snapshot to
    /// `key_health_path`. Each call cancels any prior pending save (by
    /// replacing the stored `Task`), coalescing a burst of failures from a
    /// single retry loop into one disk write. Called after every request
    /// outcome (success or failure) from `stream_completion` / `stream_response`
    /// via the free-function form `schedule_persist_key_health_inner` (which
    /// takes `Send`-safe handles, not `AsyncApp`), and from `clear_slot_backoff`
    /// when the user clears or resets a key's backoff.
    fn schedule_persist_key_health(&self, cx: &App) {
        schedule_persist_key_health_inner(
            &self.key_health,
            &self.key_health_dirty,
            self.key_health_path.clone(),
            cx.background_executor().clone(),
            <dyn Fs>::global(cx),
        );
    }

    /// Clears a single slot's backoff (failures=0, backoff_until=None) and
    /// schedules a debounced persist. Escape hatch for the UI "Clear" button,
    /// and also called when a slot's key is reset so a freshly-added key value
    /// doesn't inherit backoff from a previously-removed key that occupied the
    /// same slot — the new key is unknown and shouldn't be penalized for the
    /// old key's failures, and the UI badge shouldn't show a stale countdown.
    /// The upstream quota may also reset before the 5h backoff window elapses
    /// (e.g. a per-minute tier), and without this the user is stuck waiting.
    /// Also overwrites the persisted state so a process restart doesn't
    /// resurrect the stale backoff either.
    fn clear_slot_backoff(&self, slot: KeySlot, cx: &App) {
        let mut tracker = self.key_health.lock();
        let health = tracker.get_mut(slot);
        health.consecutive_failures = 0;
        health.backoff_until = None;
        drop(tracker);
        self.schedule_persist_key_health(cx);
    }

    /// Toggles the user-controlled `enabled` flag on a slot. Persisted to disk
    /// so the choice survives restarts. Does not clear the failure counter or
    /// backoff window — re-enabling a previously-disabled slot preserves its
    /// prior health state. Called from the footer K1/K2/K3/K4 chips in
    /// `ThreadView` and from the ConfigurationView checkbox.
    fn set_slot_enabled(&self, slot: KeySlot, enabled: bool, cx: &App) {
        let mut tracker = self.key_health.lock();
        if tracker.get(slot).enabled == enabled {
            return;
        }
        tracker.set_enabled(slot, enabled);
        drop(tracker);
        self.schedule_persist_key_health(cx);
    }

    /// Returns a char-safe truncated preview of the configured key for the given
    /// slot (e.g. `"sk-...x9F"`), or `None` if the slot has no key. Used by the
    /// ConfigurationView so the user can tell which card is which key without
    /// exposing the full secret. The preview is computed from the raw key value,
    /// so it's only available for keys stored in the keychain (not env-var keys,
    /// whose value we deliberately don't read here).
    fn key_preview(&self, slot: KeySlot) -> Option<String> {
        let secondary_url = secondary_key_url(&self.settings.api_url);
        let tertiary_url = tertiary_key_url(&self.settings.api_url);
        let quaternary_url = quaternary_key_url(&self.settings.api_url);
        let (api_key_state, url): (&ApiKeyState, &str) = match slot {
            KeySlot::Primary => (&self.api_key_state, self.settings.api_url.as_str()),
            KeySlot::Secondary => (&self.api_key_state_2, secondary_url.as_ref()),
            KeySlot::Tertiary => (&self.api_key_state_3, tertiary_url.as_ref()),
            KeySlot::Quaternary => (&self.api_key_state_4, quaternary_url.as_ref()),
        };
        let key = api_key_state.key(url)?;
        Some(truncate_key_preview(&key))
    }

    fn set_api_key(&mut self, api_key: Option<String>, cx: &mut Context<Self>) -> Task<Result<()>> {
        let credentials_provider = self.credentials_provider.clone();
        let api_url = SharedString::new(self.settings.api_url.as_str());
        self.api_key_state.store(
            api_url,
            api_key,
            |this| &mut this.api_key_state,
            credentials_provider,
            cx,
        )
    }

    fn set_api_key_2(&mut self, api_key: Option<String>, cx: &mut Context<Self>) -> Task<Result<()>> {
        let credentials_provider = self.credentials_provider.clone();
        let api_url = secondary_key_url(&self.settings.api_url);
        self.api_key_state_2.store(
            api_url,
            api_key,
            |this| &mut this.api_key_state_2,
            credentials_provider,
            cx,
        )
    }

    fn set_api_key_3(&mut self, api_key: Option<String>, cx: &mut Context<Self>) -> Task<Result<()>> {
        let credentials_provider = self.credentials_provider.clone();
        let api_url = tertiary_key_url(&self.settings.api_url);
        self.api_key_state_3.store(
            api_url,
            api_key,
            |this| &mut this.api_key_state_3,
            credentials_provider,
            cx,
        )
    }

    fn set_api_key_4(&mut self, api_key: Option<String>, cx: &mut Context<Self>) -> Task<Result<()>> {
        let credentials_provider = self.credentials_provider.clone();
        let api_url = quaternary_key_url(&self.settings.api_url);
        self.api_key_state_4.store(
            api_url,
            api_key,
            |this| &mut this.api_key_state_4,
            credentials_provider,
            cx,
        )
    }

    fn authenticate(&mut self, cx: &mut Context<Self>) -> Task<Result<(), AuthenticateError>> {
        let credentials_provider = self.credentials_provider.clone();
        let api_url = SharedString::new(self.settings.api_url.clone());
        let secondary_url = secondary_key_url(&api_url);
        let tertiary_url = tertiary_key_url(&api_url);
        let quaternary_url = quaternary_key_url(&api_url);

        let task1 = self.api_key_state.load_if_needed(
            api_url,
            |this| &mut this.api_key_state,
            credentials_provider.clone(),
            cx,
        );
        let task2 = self.api_key_state_2.load_if_needed(
            secondary_url,
            |this| &mut this.api_key_state_2,
            credentials_provider.clone(),
            cx,
        );
        let task3 = self.api_key_state_3.load_if_needed(
            tertiary_url,
            |this| &mut this.api_key_state_3,
            credentials_provider.clone(),
            cx,
        );
        let task4 = self.api_key_state_4.load_if_needed(
            quaternary_url,
            |this| &mut this.api_key_state_4,
            credentials_provider,
            cx,
        );

        cx.background_spawn(async move {
            let result1 = task1.await;
            let result2 = task2.await;
            let result3 = task3.await;
            let result4 = task4.await;
            if result1.is_ok() || result2.is_ok() || result3.is_ok() || result4.is_ok() {
                Ok(())
            } else {
                result1
            }
        })
    }

    /// Collects every configured key with its slot. Used by the intra-request
    /// retry loop in `stream_completion` / `stream_response`, which needs the
    /// candidate list up-front so it can try keys one at a time without
    /// re-entering `Entity::read_with` from a `!Send` background context.
    ///
    /// Slots whose `enabled` flag is `false` (user-controlled toggle from the
    /// footer chip / settings page) are omitted entirely — `select_from_candidates`
    /// re-checks `enabled` defensively, but filtering here keeps the candidate
    /// list short and matches user intent ("don't use this key right now").
    fn gather_candidates(&self) -> Vec<(Arc<str>, KeySlot)> {
        let primary_url = self.settings.api_url.as_str();
        let secondary_url = secondary_key_url(primary_url);
        let tertiary_url = tertiary_key_url(primary_url);
        let quaternary_url = quaternary_key_url(primary_url);

        let enabled = self.key_health.lock();
        let mut out = Vec::with_capacity(4);
        if enabled.get(KeySlot::Primary).enabled {
            if let Some(key) = self.api_key_state.key(primary_url) {
                out.push((key, KeySlot::Primary));
            }
        }
        if enabled.get(KeySlot::Secondary).enabled {
            if let Some(key) = self.api_key_state_2.key(&secondary_url) {
                out.push((key, KeySlot::Secondary));
            }
        }
        if enabled.get(KeySlot::Tertiary).enabled {
            if let Some(key) = self.api_key_state_3.key(&tertiary_url) {
                out.push((key, KeySlot::Tertiary));
            }
        }
        if enabled.get(KeySlot::Quaternary).enabled {
            if let Some(key) = self.api_key_state_4.key(&quaternary_url) {
                out.push((key, KeySlot::Quaternary));
            }
        }
        out
    }

    /// Gathers the inputs needed to probe a single key's health from the UI
    /// "Check" button: the key value, the first configured model id (used for
    /// the probe request), the api_url, and the custom headers. Returns `None`
    /// if the slot has no key or no model is configured (nothing to probe).
    /// Returns owned/cloned values so the caller can move them into a
    /// background task without holding a borrow on `State`.
    fn probe_inputs(&self, slot: KeySlot) -> Option<KeyProbeInputs> {
        let secondary_url = secondary_key_url(&self.settings.api_url);
        let tertiary_url = tertiary_key_url(&self.settings.api_url);
        let quaternary_url = quaternary_key_url(&self.settings.api_url);
        let (api_key_state, url): (&ApiKeyState, &str) = match slot {
            KeySlot::Primary => (&self.api_key_state, self.settings.api_url.as_str()),
            KeySlot::Secondary => (&self.api_key_state_2, secondary_url.as_ref()),
            KeySlot::Tertiary => (&self.api_key_state_3, tertiary_url.as_ref()),
            KeySlot::Quaternary => (&self.api_key_state_4, quaternary_url.as_ref()),
        };
        let api_key = api_key_state.key(url)?;
        let model = self.settings.available_models.first()?.name.clone();
        Some(KeyProbeInputs {
            api_key,
            model,
            api_url: Arc::<str>::from(self.settings.api_url.as_str()),
            extra_headers: self.settings.custom_headers.clone(),
            provider_name: Arc::<str>::from(self.id.as_ref()),
        })
    }

    /// Probe inputs for every slot that has a key configured, in fixed
    /// `[Primary, Secondary, Tertiary, Quaternary]` order. Used by
    /// `reset_key_session` to re-verify all keys (including backed-off ones)
    /// when a new agent thread starts. Slots without a key (or with no model
    /// configured) are skipped — there is nothing to probe.
    fn all_probe_inputs(&self) -> Vec<(KeySlot, KeyProbeInputs)> {
        [
            KeySlot::Primary,
            KeySlot::Secondary,
            KeySlot::Tertiary,
            KeySlot::Quaternary,
        ]
        .into_iter()
        .filter_map(|slot| self.probe_inputs(slot).map(|inputs| (slot, inputs)))
        .collect()
    }

    /// Returns `[Primary, Secondary, Tertiary, Quaternary]` slot status for the UI. Clones
    /// the tracker under the mutex (same pattern as `snapshot_health`) so the
    /// lock is not held across the per-slot computation. Used by
    /// `ConfigurationView::render` to draw a backoff badge with a live
    /// countdown. The ConfigurationView polls this on a 1s timer while the
    /// settings page is open; see `backoff_refresh_task`.
    fn slot_health_snapshot(&self) -> [SlotHealthStatus; 4] {
        let now = Instant::now();
        let tracker = self.key_health.lock().clone();
        [
            self.slot_status(KeySlot::Primary, &tracker, now),
            self.slot_status(KeySlot::Secondary, &tracker, now),
            self.slot_status(KeySlot::Tertiary, &tracker, now),
            self.slot_status(KeySlot::Quaternary, &tracker, now),
        ]
    }

    fn slot_status(
        &self,
        slot: KeySlot,
        tracker: &KeyHealthTracker,
        now: Instant,
    ) -> SlotHealthStatus {
        let health = tracker.get(slot);
        let has_key = match slot {
            KeySlot::Primary => self.api_key_state.has_key(),
            KeySlot::Secondary => self.api_key_state_2.has_key(),
            KeySlot::Tertiary => self.api_key_state_3.has_key(),
            KeySlot::Quaternary => self.api_key_state_4.has_key(),
        };
        let backoff_remaining = match health.backoff_until {
            Some(until) => until.saturating_duration_since(now),
            None => Duration::ZERO,
        };
        let is_backed_off = health.is_backed_off(now);
        SlotHealthStatus {
            has_key,
            is_backed_off,
            backoff_remaining,
            consecutive_failures: health.consecutive_failures,
            enabled: health.enabled,
        }
    }

    /// Short label for the key most recently selected by `retry_stream`, for
    /// display on the retry button so the user can see which of the configured
    /// keys the in-flight turn picked. Returns `None` when no key has been used
    /// yet (fresh process, no OpenAI-compatible multi-key provider) or when the
    /// recorded slot has no key configured (e.g. it was removed mid-turn).
    ///
    /// Format: `K<index>` (1-indexed: K1=Primary, K2=Secondary, K3=Tertiary).
    /// Index is preferred over a key preview here because the retry button is
    /// always-visible chrome, whereas `truncate_key_preview` is only shown in
    /// the settings page behind an explicit reveal.
    fn last_used_key_label(&self) -> Option<String> {
        let slot = self.key_health.lock().last_used_slot?;
        let has_key = match slot {
            KeySlot::Primary => self.api_key_state.has_key(),
            KeySlot::Secondary => self.api_key_state_2.has_key(),
            KeySlot::Tertiary => self.api_key_state_3.has_key(),
            KeySlot::Quaternary => self.api_key_state_4.has_key(),
        };
        if !has_key {
            return None;
        }
        Some(format!("K{}", slot_index(slot) + 1))
    }
}

impl OpenAiCompatibleLanguageModelProvider {
    pub fn new(
        id: Arc<str>,
        http_client: Arc<dyn HttpClient>,
        credentials_provider: Arc<dyn CredentialsProvider>,
        cx: &mut App,
    ) -> Self {
        fn resolve_settings<'a>(id: &'a str, cx: &'a App) -> Option<&'a OpenAiCompatibleSettings> {
            crate::AllLanguageModelSettings::get_global(cx)
                .openai_compatible
                .get(id)
        }

        let api_key_env_var_name = format!("{}_API_KEY", id).to_case(Case::UpperSnake).into();
        let api_key_env_var_name_2 = format!("{}_API_KEY_2", id).to_case(Case::UpperSnake).into();
        let api_key_env_var_name_3 = format!("{}_API_KEY_3", id).to_case(Case::UpperSnake).into();
        let api_key_env_var_name_4 = format!("{}_API_KEY_4", id).to_case(Case::UpperSnake).into();
        let state = cx.new(|cx| {
            cx.observe_global::<SettingsStore>(|this: &mut State, cx| {
                let Some(settings) = resolve_settings(&this.id, cx).cloned() else {
                    return;
                };
                if &this.settings != &settings {
                    let credentials_provider = this.credentials_provider.clone();
                    let api_url = SharedString::new(settings.api_url.as_str());
                    let secondary_url = secondary_key_url(&api_url);
                    let tertiary_url = tertiary_key_url(&api_url);
                    let quaternary_url = quaternary_key_url(&api_url);
                    this.api_key_state.handle_url_change(
                        api_url,
                        |this| &mut this.api_key_state,
                        credentials_provider.clone(),
                        cx,
                    );
                    this.api_key_state_2.handle_url_change(
                        secondary_url,
                        |this| &mut this.api_key_state_2,
                        credentials_provider.clone(),
                        cx,
                    );
                    this.api_key_state_3.handle_url_change(
                        tertiary_url,
                        |this| &mut this.api_key_state_3,
                        credentials_provider.clone(),
                        cx,
                    );
                    this.api_key_state_4.handle_url_change(
                        quaternary_url,
                        |this| &mut this.api_key_state_4,
                        credentials_provider,
                        cx,
                    );
                    this.settings = settings;
                    cx.notify();
                }
            })
            .detach();
            let settings = resolve_settings(&id, cx).cloned().unwrap_or_default();
            let key_health = Arc::new(ParkingMutex::new(KeyHealthTracker::default()));
            let key_health_dirty: Arc<ParkingMutex<Option<Task<()>>>> =
                Arc::new(ParkingMutex::new(None));
            let key_health_path = key_health_path_for(&id);

            // Load persisted backoff state off the foreground thread. The
            // in-memory tracker starts healthy; once the load completes we
            // overwrite it under the mutex. Errors are logged inside
            // `reload_persisted_health` and never propagate — a missing or
            // corrupt file just means "start fresh", which is safe.
            //
            // Detached rather than stored: we don't need to await it, and a
            // late-arriving load just refreshes the (possibly already-mutated)
            // in-memory state. The race is benign: either the disk wins
            // (restoring backed-off state) or an in-flight request has already
            // recorded an outcome (in which case the freshest data wins).
            let fs = <dyn Fs>::global(cx);
            let load_health = key_health.clone();
            let load_path = key_health_path.clone();
            cx.background_spawn(async move {
                let loaded = reload_persisted_health(&fs, &load_path).await;
                *load_health.lock() = loaded;
            })
            .detach();

            State {
                id: id.clone(),
                api_key_state: ApiKeyState::new(
                    SharedString::new(settings.api_url.as_str()),
                    EnvVar::new(api_key_env_var_name),
                ),
                api_key_state_2: ApiKeyState::new(
                    secondary_key_url(&settings.api_url),
                    EnvVar::new(api_key_env_var_name_2),
                ),
                api_key_state_3: ApiKeyState::new(
                    tertiary_key_url(&settings.api_url),
                    EnvVar::new(api_key_env_var_name_3),
                ),
                api_key_state_4: ApiKeyState::new(
                    quaternary_key_url(&settings.api_url),
                    EnvVar::new(api_key_env_var_name_4),
                ),
                key_health,
                key_health_dirty,
                key_health_path,
                settings,
                credentials_provider,
            }
        });

        Self {
            id: id.clone().into(),
            name: id.into(),
            http_client,
            state,
        }
    }

    fn create_language_model(&self, model: AvailableModel) -> Arc<dyn LanguageModel> {
        Arc::new(OpenAiCompatibleLanguageModel {
            id: LanguageModelId::from(model.name.clone()),
            provider_id: self.id.clone(),
            provider_name: self.name.clone(),
            model,
            state: self.state.clone(),
            http_client: self.http_client.clone(),
            request_limiter: RateLimiter::new(4),
        })
    }
}

impl LanguageModelProviderState for OpenAiCompatibleLanguageModelProvider {
    type ObservableEntity = State;

    fn observable_entity(&self) -> Option<Entity<Self::ObservableEntity>> {
        Some(self.state.clone())
    }
}

impl LanguageModelProvider for OpenAiCompatibleLanguageModelProvider {
    fn id(&self) -> LanguageModelProviderId {
        self.id.clone()
    }

    fn name(&self) -> LanguageModelProviderName {
        self.name.clone()
    }

    fn icon(&self) -> IconOrSvg {
        IconOrSvg::Icon(IconName::AiOpenAiCompat)
    }

    fn default_model(&self, cx: &App) -> Option<Arc<dyn LanguageModel>> {
        self.state
            .read(cx)
            .settings
            .available_models
            .first()
            .map(|model| self.create_language_model(model.clone()))
    }

    fn default_fast_model(&self, _cx: &App) -> Option<Arc<dyn LanguageModel>> {
        None
    }

    fn provided_models(&self, cx: &App) -> Vec<Arc<dyn LanguageModel>> {
        self.state
            .read(cx)
            .settings
            .available_models
            .iter()
            .map(|model| self.create_language_model(model.clone()))
            .collect()
    }

    fn is_authenticated(&self, cx: &App) -> bool {
        self.state.read(cx).is_authenticated()
    }

    fn authenticate(&self, cx: &mut App) -> Task<Result<(), AuthenticateError>> {
        self.state.update(cx, |state, cx| state.authenticate(cx))
    }

    fn settings_view(&self, _cx: &mut App) -> Option<ProviderSettingsView> {
        let state = self.state.clone();
        let http_client = self.http_client.clone();
        Some(ProviderSettingsView::SubPage(SubPageProviderSettings::new(
            move |window, cx| {
                cx.new(|cx| {
                    ConfigurationView::new(state.clone(), http_client.clone(), window, cx)
                })
                .into()
            },
        )))
    }

    fn set_api_key(&self, api_key: Option<String>, cx: &mut App) -> Task<Result<()>> {
        self.state
            .update(cx, |state, cx| state.set_api_key(api_key, cx))
    }
}

pub struct OpenAiCompatibleLanguageModel {
    id: LanguageModelId,
    provider_id: LanguageModelProviderId,
    provider_name: LanguageModelProviderName,
    model: AvailableModel,
    state: Entity<State>,
    http_client: Arc<dyn HttpClient>,
    request_limiter: RateLimiter,
}

impl OpenAiCompatibleLanguageModel {
    fn stream_completion(
        &self,
        request: open_ai::Request,
        thread_id: Option<String>,
        cx: &AsyncApp,
    ) -> BoxFuture<
        'static,
        Result<
            futures::stream::BoxStream<'static, Result<ResponseStreamEvent>>,
            LanguageModelCompletionError,
        >,
    > {
        let http_client = self.http_client.clone();

        let (key_health, key_health_dirty, key_health_path, candidates, api_url, extra_headers, fs) =
            self.state.read_with(cx, |state, cx| {
                (
                    state.key_health.clone(),
                    state.key_health_dirty.clone(),
                    state.key_health_path.clone(),
                    state.gather_candidates(),
                    Arc::<str>::from(state.settings.api_url.as_str()),
                    state.settings.custom_headers.clone(),
                    <dyn Fs>::global(cx),
                )
            });

        let provider = self.provider_name.clone();
        let provider_name = provider.0.clone();
        // Capture the background executor and Fs handle up-front so the
        // rate-limited stream closure — which must be `Send` to satisfy
        // `BoxFuture<'static, ...>` — can schedule a debounced persist after
        // `retry_stream` without dragging the `!Send` `AsyncApp` along.
        let persist_executor = cx.background_executor().clone();
        let future = self.request_limiter.stream(async move {
            if candidates.is_empty() {
                return Err(LanguageModelCompletionError::NoApiKey { provider });
            }
            let health_before = snapshot_health(&key_health);
            let result = retry_stream(
                &candidates,
                &key_health,
                thread_id.as_deref(),
                provider,
                move |api_key| {
                    let http_client = http_client.clone();
                    let api_url = api_url.clone();
                    let extra_headers = extra_headers.clone();
                    let provider_name = provider_name.clone();
                    let attempt_request = request.clone();
                    Box::pin(async move {
                        // Return the stream directly after HTTP setup (no
                        // `probe_first_event`). Probing the first SSE event added
                        // time-to-first-token latency to the critical path of
                        // every request and held the rate-limiter permit during
                        // that wait, which serialized subagent fan-out. Setup-phase
                        // errors (auth, 429, 5xx) still surface here and drive
                        // key rotation; first-event-in-stream errors are rare and
                        // handled by the consumer like any stream error.
                        Ok(stream_completion(
                            http_client.as_ref(),
                            provider_name.as_str(),
                            api_url.as_ref(),
                            api_key.as_ref(),
                            attempt_request,
                            &extra_headers,
                        )
                        .await?)
                    })
                },
            )
            .await;
            if snapshot_health(&key_health) != health_before {
                schedule_persist_key_health_inner(
                    &key_health,
                    &key_health_dirty,
                    key_health_path.clone(),
                    persist_executor.clone(),
                    fs.clone(),
                );
            }
            result
        });

        async move { Ok(future.await?.boxed()) }.boxed()
    }

    fn stream_response(
        &self,
        request: ResponseRequest,
        thread_id: Option<String>,
        cx: &AsyncApp,
    ) -> BoxFuture<'static, Result<futures::stream::BoxStream<'static, Result<ResponsesStreamEvent>>>>
    {
        let http_client = self.http_client.clone();

        let (key_health, key_health_dirty, key_health_path, candidates, api_url, extra_headers, fs) =
            self.state.read_with(cx, |state, cx| {
                (
                    state.key_health.clone(),
                    state.key_health_dirty.clone(),
                    state.key_health_path.clone(),
                    state.gather_candidates(),
                    Arc::<str>::from(state.settings.api_url.as_str()),
                    state.settings.custom_headers.clone(),
                    <dyn Fs>::global(cx),
                )
            });

        let provider = self.provider_name.clone();
        let provider_name = provider.0.clone();
        // Capture the background executor and Fs handle up-front so the
        // rate-limited stream closure — which must be `Send` to satisfy
        // `BoxFuture<'static, ...>` — can schedule a debounced persist after
        // `retry_stream` without dragging the `!Send` `AsyncApp` along.
        let persist_executor = cx.background_executor().clone();
        let future = self.request_limiter.stream(async move {
            if candidates.is_empty() {
                return Err(LanguageModelCompletionError::NoApiKey { provider });
            }
            let health_before = snapshot_health(&key_health);
            let result = retry_stream(
                &candidates,
                &key_health,
                thread_id.as_deref(),
                provider,
                move |api_key| {
                    let http_client = http_client.clone();
                    let api_url = api_url.clone();
                    let extra_headers = extra_headers.clone();
                    let provider_name = provider_name.clone();
                    let attempt_request = request.clone();
                    Box::pin(async move {
                        // See stream_completion: no probe_first_event.
                        Ok(stream_response(
                            http_client.as_ref(),
                            provider_name.as_str(),
                            api_url.as_ref(),
                            api_key.as_ref(),
                            attempt_request,
                            &extra_headers,
                        )
                        .await?)
                    })
                },
            )
            .await;
            if snapshot_health(&key_health) != health_before {
                schedule_persist_key_health_inner(
                    &key_health,
                    &key_health_dirty,
                    key_health_path.clone(),
                    persist_executor.clone(),
                    fs.clone(),
                );
            }
            result
        });

        async move { Ok(future.await?.boxed()) }.boxed()
    }
}

fn default_thinking_reasoning_effort(model: &AvailableModel) -> Option<open_ai::ReasoningEffort> {
    model
        .reasoning_effort
        .filter(|effort| *effort != open_ai::ReasoningEffort::None)
}

fn supported_thinking_effort_levels(model: &AvailableModel) -> Vec<LanguageModelEffortLevel> {
    let Some(default_effort) = default_thinking_reasoning_effort(model) else {
        return Vec::new();
    };

    open_ai::ReasoningEffort::OPENAI_COMPATIBLE_SELECTABLE
        .into_iter()
        .map(|effort| LanguageModelEffortLevel {
            name: effort.label().into(),
            value: effort.value().into(),
            is_default: effort == default_effort,
        })
        .collect()
}

fn selected_thinking_reasoning_effort(
    request: &LanguageModelRequest,
) -> Option<open_ai::ReasoningEffort> {
    request
        .thinking_effort
        .as_deref()
        .and_then(|effort| effort.parse::<open_ai::ReasoningEffort>().ok())
        .filter(|effort| *effort != open_ai::ReasoningEffort::None)
}

fn chat_completion_max_tokens_parameter(
    model: &AvailableModel,
) -> crate::provider::open_ai::ChatCompletionMaxTokensParameter {
    if model.capabilities.max_tokens_parameter {
        crate::provider::open_ai::ChatCompletionMaxTokensParameter::MaxTokens
    } else {
        crate::provider::open_ai::ChatCompletionMaxTokensParameter::MaxCompletionTokens
    }
}

fn supports_none_reasoning_effort(model: &AvailableModel) -> bool {
    model.reasoning_effort.is_some()
}

fn chat_completion_reasoning_effort(
    request: &LanguageModelRequest,
    model: &AvailableModel,
) -> Option<open_ai::ReasoningEffort> {
    if model.reasoning_effort == Some(open_ai::ReasoningEffort::None) {
        return Some(open_ai::ReasoningEffort::None);
    }

    if request.thinking_allowed {
        selected_thinking_reasoning_effort(request)
            .or_else(|| default_thinking_reasoning_effort(model))
    } else if supports_none_reasoning_effort(model) {
        Some(open_ai::ReasoningEffort::None)
    } else {
        None
    }
}

fn disable_response_thinking_for_none_effort(
    request: &mut LanguageModelRequest,
    model: &AvailableModel,
) {
    if model.reasoning_effort == Some(open_ai::ReasoningEffort::None) {
        request.thinking_allowed = false;
        request.thinking_effort = None;
    }
}

impl LanguageModel for OpenAiCompatibleLanguageModel {
    fn id(&self) -> LanguageModelId {
        self.id.clone()
    }

    fn name(&self) -> LanguageModelName {
        LanguageModelName::from(
            self.model
                .display_name
                .clone()
                .unwrap_or_else(|| self.model.name.clone()),
        )
    }

    fn provider_id(&self) -> LanguageModelProviderId {
        self.provider_id.clone()
    }

    fn provider_name(&self) -> LanguageModelProviderName {
        self.provider_name.clone()
    }

    fn supports_tools(&self) -> bool {
        self.model.capabilities.tools
    }

    fn tool_input_format(&self) -> LanguageModelToolSchemaFormat {
        LanguageModelToolSchemaFormat::JsonSchemaSubset
    }

    fn supports_images(&self) -> bool {
        self.model.capabilities.images
    }

    fn supports_tool_choice(&self, choice: LanguageModelToolChoice) -> bool {
        match choice {
            LanguageModelToolChoice::Auto => self.model.capabilities.tools,
            LanguageModelToolChoice::Any => self.model.capabilities.tools,
            LanguageModelToolChoice::None => true,
        }
    }

    fn supports_streaming_tools(&self) -> bool {
        true
    }

    fn supports_thinking(&self) -> bool {
        default_thinking_reasoning_effort(&self.model).is_some()
    }

    fn supported_effort_levels(&self) -> Vec<LanguageModelEffortLevel> {
        supported_thinking_effort_levels(&self.model)
    }

    fn supports_split_token_display(&self) -> bool {
        true
    }

    fn telemetry_id(&self) -> String {
        format!("openai/{}", self.model.name)
    }

    fn last_used_key_label(&self, cx: &App) -> Option<String> {
        self.state.read_with(cx, |state, _| state.last_used_key_label())
    }

    fn key_slot_status(&self, cx: &App) -> Option<ModelKeySlotStatusSummary> {
        // Only meaningful for providers with at least one key configured. We
        // still return the summary (with `has_key: false` slots) when zero keys
        // are configured, so the footer can render the four chips as "empty".
        // However, providers that aren't OpenAI-compatible never reach this
        // impl, so this is the only `LanguageModel` impl that returns `Some`.
        let snapshot = self.state.read_with(cx, |state, _| state.slot_health_snapshot());
        Some(ModelKeySlotStatusSummary(snapshot.map(|s| ModelKeySlotStatus {
            has_key: s.has_key,
            enabled: s.enabled,
            is_backed_off: s.is_backed_off,
            backoff_remaining: s.backoff_remaining,
            consecutive_failures: s.consecutive_failures,
        })))
    }

    fn set_key_slot_enabled(&self, slot_index: usize, enabled: bool, cx: &mut App) {
        let Some(slot) = slot_from_index(slot_index) else {
            return;
        };
        self.state.read_with(cx, |state, _| {
            state.set_slot_enabled(slot, enabled, cx);
        });
        // `set_slot_enabled` mutates `key_health` under a parking_lot mutex
        // (not via `Entity::update`), so `cx.observe(&state, ...)` wouldn't
        // fire. Notify explicitly so the footer re-renders immediately.
        // `read_with` above ran on a `&App`; we now need a `&mut App` to nudge
        // the state entity — this is a no-op if the entity is gone.
        let _ = self.state.update(cx, |_, cx| cx.notify());
    }

    fn reset_key_session(&self, cx: &App) {
        // Issue 029: per-thread sticky picks make clearing selection state
        // unnecessary — a new thread has a new id and picks fresh via the
        // rotation cursor. The probes below remain: they re-verify every
        // configured key (including backed-off ones) so stale backoffs clear
        // before the new thread's first pick.
        let (probe_inputs, key_health, key_health_dirty, key_health_path) = self
            .state
            .read_with(cx, |state, _| {
                (
                    state.all_probe_inputs(),
                    state.key_health.clone(),
                    state.key_health_dirty.clone(),
                    state.key_health_path.clone(),
                )
            });
        if probe_inputs.is_empty() {
            return;
        }
        let http_client = self.http_client.clone();
        let persist_executor = cx.background_executor().clone();
        let fs = <dyn Fs>::global(cx);
        // Detached: the new thread's first request must not wait for the
        // probes — they only *clear* stale backoffs when they arrive; the
        // in-memory health is already usable as-is.
        cx.background_spawn(async move {
            let health_before = snapshot_health(&key_health);
            for (slot, inputs) in probe_inputs {
                // Sequential on purpose: probes are 1-token pings, and a burst
                // of parallel pings right at thread start would compete with
                // the thread's own first request for the rate limiter.
                let result = run_key_probe(http_client.clone(), inputs).await;
                // Same semantics as the settings-page Check button: a healthy
                // probe clears stale backoff (the key is NOT really limited);
                // rate-limit confirms the backoff is warranted; any other
                // error is ambiguous and changes nothing.
                if result == KeyProbeResult::Ok {
                    record_key_success(&key_health, slot);
                }
                log::info!(
                    "reset_key_session probe: slot={slot:?} result={result:?}"
                );
            }
            if snapshot_health(&key_health) != health_before {
                schedule_persist_key_health_inner(
                    &key_health,
                    &key_health_dirty,
                    key_health_path,
                    persist_executor,
                    fs,
                );
            }
        })
        .detach();
    }

    fn max_token_count(&self) -> u64 {
        self.model.max_tokens
    }

    fn max_output_tokens(&self) -> Option<u64> {
        self.model.max_output_tokens
    }

    fn stream_completion(
        &self,
        mut request: LanguageModelRequest,
        cx: &AsyncApp,
    ) -> BoxFuture<
        'static,
        Result<
            futures::stream::BoxStream<
                'static,
                Result<LanguageModelCompletionEvent, LanguageModelCompletionError>,
            >,
            LanguageModelCompletionError,
        >,
    > {
        // `speed` can leak in from a parent thread's model; this provider never
        // supports fast mode, and arbitrary compatible endpoints reject `service_tier`.
        if !self.supports_fast_mode() {
            request.speed = None;
        }
        // Thread identity for per-thread key stickiness (issue 029), captured
        // before the conversions below consume the request.
        let thread_id = request.thread_id.clone();

        if self.model.capabilities.chat_completions {
            let reasoning_effort = chat_completion_reasoning_effort(&request, &self.model);
            let request = match into_open_ai(
                request,
                &self.model.name,
                self.model.capabilities.parallel_tool_calls,
                self.model.capabilities.prompt_cache_key,
                self.max_output_tokens(),
                chat_completion_max_tokens_parameter(&self.model),
                reasoning_effort,
                self.model.capabilities.interleaved_reasoning,
            ) {
                Ok(request) => request,
                Err(error) => return async move { Err(error.into()) }.boxed(),
            };
            let completions = self.stream_completion(request, thread_id, cx);
            async move {
                let mapper = OpenAiEventMapper::new();
                Ok(mapper.map_stream(completions.await?).boxed())
            }
            .boxed()
        } else {
            disable_response_thinking_for_none_effort(&mut request, &self.model);
            let request = match into_open_ai_response(
                request,
                &self.model.name,
                self.model.capabilities.parallel_tool_calls,
                self.model.capabilities.prompt_cache_key,
                self.max_output_tokens(),
                default_thinking_reasoning_effort(&self.model),
                supports_none_reasoning_effort(&self.model),
                &self.provider_id,
            ) {
                Ok(request) => request,
                Err(error) => return async move { Err(error.into()) }.boxed(),
            };
            let completions = self.stream_response(request, thread_id, cx);
            let compaction_state_owner = self.provider_id.clone();
            async move {
                let mapper = OpenAiResponseEventMapper::new(compaction_state_owner);
                Ok(mapper.map_stream(completions.await?).boxed())
            }
            .boxed()
        }
    }
}

/// Builds and fires a single non-streaming `max_completion_tokens = 1` request
/// against the chat-completions endpoint with the given key, and classifies the
/// outcome into a `KeyProbeResult` for the Check button. Uses the chat
/// completions path (not the responses path) so the probe is one concrete
/// endpoint; if the provider only speaks the responses API the probe may 404
/// and report `Err`, which is acceptable for a manual sanity check (the message
/// is surfaced as a tooltip so the user can tell).
///
/// Run on the background executor from `ConfigurationView::probe_key`; the
/// result is written back into `probe_results` on the foreground thread.
async fn run_key_probe(http_client: Arc<dyn HttpClient>, inputs: KeyProbeInputs) -> KeyProbeResult {
    let KeyProbeInputs { api_key, model, api_url, extra_headers, provider_name } = inputs;
    let request = open_ai::Request {
        model,
        messages: vec![open_ai::RequestMessage::User {
            content: open_ai::MessageContent::Plain("ping".to_string()),
        }],
        stream: false,
        stream_options: None,
        max_completion_tokens: Some(1),
        max_tokens: None,
        stop: Vec::new(),
        temperature: None,
        tool_choice: None,
        parallel_tool_calls: None,
        tools: Vec::new(),
        prompt_cache_key: None,
        reasoning_effort: None,
        service_tier: None,
    };
    match stream_completion(
        http_client.as_ref(),
        provider_name.as_ref(),
        api_url.as_ref(),
        api_key.as_ref(),
        request,
        &extra_headers,
    )
    .await
    {
        Ok(mut stream) => {
            // Drain the first event: a successful setup with an inline error
            // (common for late-detected rate limits) should classify as
            // rate-limit/error, not ok.
            match stream.next().await {
                Some(Ok(_)) => KeyProbeResult::Ok,
                Some(Err(_)) => KeyProbeResult::Ok,
                None => KeyProbeResult::Ok,
            }
        }
        Err(err) => classify_probe_error(err),
    }
}

/// Maps an `open_ai::RequestError` (the setup-phase error type returned by
/// `stream_completion`) onto the three-way `KeyProbeResult`. A 429 / rate-limit
/// becomes `RateLimit`; anything else becomes `Err` with a short message.
fn classify_probe_error(err: open_ai::RequestError) -> KeyProbeResult {
    match err {
        open_ai::RequestError::HttpResponseError { status_code, .. }
            if status_code.as_u16() == 429 =>
        {
            KeyProbeResult::RateLimit
        }
        other => KeyProbeResult::Err(format_probe_error_message(&other).into()),
    }
}

/// Trims an `open_ai::RequestError` to a one-line string short enough for a
/// tooltip. Keeps the variant name + the first line of any body/message so the
/// user can tell auth (401) from not-found (404) from a 500, without dumping
/// the full upstream JSON.
fn format_probe_error_message(err: &open_ai::RequestError) -> String {
    let raw = format!("{err:#}");
    // Take the first line and cap its length so a huge upstream body doesn't
    // blow out the tooltip. Char-safe truncation via `chars()`.
    let first_line = raw.lines().next().unwrap_or(&raw);
    let capped: String = first_line.chars().take(160).collect();
    capped
}

struct ConfigurationView {
    api_key_editor: Entity<InputField>,
    api_key_editor_2: Entity<InputField>,
    api_key_editor_3: Entity<InputField>,
    api_key_editor_4: Entity<InputField>,
    state: Entity<State>,
    http_client: Arc<dyn HttpClient>,
    load_credentials_task: Option<Task<()>>,
    /// Latest "Check" probe result per slot (Primary, Secondary, Tertiary, Quaternary), or
    /// `None` if the slot hasn't been probed (or was reset on key change).
    /// Drives the button face label (`check(ok)` / `check(hit)` / `check(err)`).
    probe_results: [Option<KeyProbeResult>; 4],
    /// In-flight probe task per slot. When `Some`, the button shows `Check…` and
    /// is disabled. Stored so dropping the view cancels pending probes.
    probe_tasks: [Option<Task<()>>; 4],
    /// Polls `slot_health_snapshot` every second while the settings page is open
    /// and calls `cx.notify()` only when the snapshot changes. Required because
    /// `key_health` is updated from background request closures through
    /// `Arc<Mutex<KeyHealthTracker>>`, bypassing `cx.notify()` on `State` —
    /// so the view wouldn't otherwise learn about backoff state transitions or
    /// the countdown ticking down. The task is dropped (cancelled) with the view.
    #[allow(dead_code)]
    backoff_refresh_task: Option<Task<()>>,
}

impl ConfigurationView {
    fn new(
        state: Entity<State>,
        http_client: Arc<dyn HttpClient>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let api_key_editor = cx.new(|cx| InputField::new(window, cx, API_KEY_PLACEHOLDER));
        let api_key_editor_2 = cx.new(|cx| InputField::new(window, cx, API_KEY_PLACEHOLDER));
        let api_key_editor_3 = cx.new(|cx| InputField::new(window, cx, API_KEY_PLACEHOLDER));
        let api_key_editor_4 = cx.new(|cx| InputField::new(window, cx, API_KEY_PLACEHOLDER));

        cx.observe(&state, |_, _, cx| {
            cx.notify();
        })
        .detach();

        let load_credentials_task = Some(cx.spawn_in(window, {
            let state = state.clone();
            async move |this, cx| {
                if let Some(task) = Some(state.update(cx, |state, cx| state.authenticate(cx))) {
                    // We don't log an error, because "not signed in" is also an error.
                    let _ = task.await;
                }
                this.update(cx, |this, cx| {
                    this.load_credentials_task = None;
                    cx.notify();
                })
                .log_err();
            }
        }));

        // Background refresh of backoff badges. `key_health` is updated by
        // request closures on the background executor via `Arc<Mutex>`, which
        // bypasses `Entity::notify`, so `cx.observe(&state, ...)` alone can't
        // catch slot backoff transitions. This task polls the snapshot every
        // second and only notifies when something changed (countdown tick,
        // slot entered/exited backoff). It self-terminates if the view is dropped
        // (the `this.update` call returns `Err`).
        let backoff_refresh_task = cx.spawn_in(window, {
            let state = state.clone();
            async move |this, cx| {
                let mut last_snapshot: [SlotHealthStatus; 4] =
                    state.read_with(cx, |state, _| state.slot_health_snapshot());
                loop {
                    cx.background_executor()
                        .timer(Duration::from_secs(1))
                        .await;
                    let update_result = this.update(cx, |_, cx| {
                        let current = state.read(cx).slot_health_snapshot();
                        let changed = current != last_snapshot;
                        last_snapshot = current;
                        changed
                    });
                    match update_result {
                        Ok(true) => {
                            let _ = this.update(cx, |_, cx| cx.notify());
                        }
                        Ok(false) => {}
                        Err(_) => break, // view dropped; exit the loop
                    }
                }
            }
        });

        Self {
            api_key_editor,
            api_key_editor_2,
            api_key_editor_3,
            api_key_editor_4,
            state,
            http_client,
            load_credentials_task,
            probe_results: Default::default(),
            probe_tasks: Default::default(),
            backoff_refresh_task: Some(backoff_refresh_task),
        }
    }

    fn save_api_key(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let api_key = self.api_key_editor.read(cx).text(cx).trim().to_string();
        if api_key.is_empty() {
            return;
        }

        // url changes can cause the editor to be displayed again
        self.api_key_editor
            .update(cx, |input, cx| input.set_text("", window, cx));

        let state = self.state.clone();
        cx.spawn_in(window, async move |_, cx| {
            state
                .update(cx, |state, cx| state.set_api_key(Some(api_key), cx))
                .await
        })
        .detach_and_log_err(cx);
    }

    fn save_api_key_2(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let api_key = self.api_key_editor_2.read(cx).text(cx).trim().to_string();
        if api_key.is_empty() {
            return;
        }

        self.api_key_editor_2
            .update(cx, |input, cx| input.set_text("", window, cx));

        let state = self.state.clone();
        cx.spawn_in(window, async move |_, cx| {
            state
                .update(cx, |state, cx| state.set_api_key_2(Some(api_key), cx))
                .await
        })
        .detach_and_log_err(cx);
    }

    fn reset_api_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.api_key_editor
            .update(cx, |input, cx| input.set_text("", window, cx));

        let state = self.state.clone();
        cx.spawn_in(window, async move |_, cx| {
            state
                .update(cx, |state, cx| {
                    // A freshly-cleared slot is unknown and shouldn't inherit
                    // backoff recorded against whatever key previously occupied
                    // it; also overwrites the persisted state so a restart
                    // doesn't resurrect the stale backoff.
                    state.clear_slot_backoff(KeySlot::Primary, cx);
                    state.set_api_key(None, cx)
                })
                .await
        })
        .detach_and_log_err(cx);
    }

    fn reset_api_key_2(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.api_key_editor_2
            .update(cx, |input, cx| input.set_text("", window, cx));

        let state = self.state.clone();
        cx.spawn_in(window, async move |_, cx| {
            state
                .update(cx, |state, cx| {
                    state.clear_slot_backoff(KeySlot::Secondary, cx);
                    state.set_api_key_2(None, cx)
                })
                .await
        })
        .detach_and_log_err(cx);
    }

    fn save_api_key_3(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let api_key = self.api_key_editor_3.read(cx).text(cx).trim().to_string();
        if api_key.is_empty() {
            return;
        }

        self.api_key_editor_3
            .update(cx, |input, cx| input.set_text("", window, cx));

        let state = self.state.clone();
        cx.spawn_in(window, async move |_, cx| {
            state
                .update(cx, |state, cx| state.set_api_key_3(Some(api_key), cx))
                .await
        })
        .detach_and_log_err(cx);
    }

    fn save_api_key_4(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let api_key = self.api_key_editor_4.read(cx).text(cx).trim().to_string();
        if api_key.is_empty() {
            return;
        }

        self.api_key_editor_4
            .update(cx, |input, cx| input.set_text("", window, cx));

        let state = self.state.clone();
        cx.spawn_in(window, async move |_, cx| {
            state
                .update(cx, |state, cx| state.set_api_key_4(Some(api_key), cx))
                .await
        })
        .detach_and_log_err(cx);
    }

    fn reset_api_key_3(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.api_key_editor_3
            .update(cx, |input, cx| input.set_text("", window, cx));

        let state = self.state.clone();
        cx.spawn_in(window, async move |_, cx| {
            state
                .update(cx, |state, cx| {
                    state.clear_slot_backoff(KeySlot::Tertiary, cx);
                    state.set_api_key_3(None, cx)
                })
                .await
        })
        .detach_and_log_err(cx);
    }

    fn reset_api_key_4(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.api_key_editor_4
            .update(cx, |input, cx| input.set_text("", window, cx));

        let state = self.state.clone();
        cx.spawn_in(window, async move |_, cx| {
            state
                .update(cx, |state, cx| {
                    state.clear_slot_backoff(KeySlot::Quaternary, cx);
                    state.set_api_key_4(None, cx)
                })
                .await
        })
        .detach_and_log_err(cx);
    }

    /// Clears one slot's backoff immediately (escape hatch for when the upstream
    /// quota has already reset before the 5h window elapsed). No-op if the slot
    /// isn't currently backed off — the button is only rendered when it is, but
    /// this stays defensive against a stale snapshot between render and click.
    fn clear_backoff(&mut self, slot: KeySlot, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| state.clear_slot_backoff(slot, cx));
        // A manual clear is a strong signal the user wants this slot forgotten;
        // also discard any stale probe result so the button resets to idle.
        let idx = slot_index(slot);
        self.probe_results[idx] = None;
        cx.notify();
    }

    /// Fires a minimal completion probe against the given slot's key and records
    /// the outcome in `probe_results` so the Check button's face label updates.
    /// The probe is a single non-streaming request with `max_completion_tokens
    /// = 1` against the provider's first configured model — cheap, and enough
    /// to distinguish ok / rate-limit / other-error. Concurrent probes on the
    /// same slot are coalesced (a new probe cancels the prior task by replacing
    /// it in `probe_tasks`).
    ///
    /// On `Ok` the slot's backoff is also cleared: a successful probe is direct
    /// evidence the key works right now, so any stale backoff (e.g. the upstream
    /// quota reset) shouldn't keep the key rotated out until the 1h window
    /// elapses. This mirrors the per-key success path in `retry_stream`, which
    /// clears health on the first successful request.
    fn probe_key(&mut self, slot: KeySlot, window: &mut Window, cx: &mut Context<Self>) {
        let idx = slot_index(slot);
        // Coalesce: if a probe is already in flight for this slot, ignore.
        if self.probe_tasks[idx].is_some() {
            return;
        }
        let Some(inputs) = self.state.read(cx).probe_inputs(slot) else {
            return;
        };

        let http_client = self.http_client.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = run_key_probe(http_client, inputs).await;
            let _ = this.update(cx, |this, cx| {
                let idx = slot_index(slot);
                // A successful probe means the key is healthy right now — clear
                // any backoff so the slot re-enters rotation immediately instead
                // of waiting out the 1h window. Non-ok results leave backoff
                // untouched (a rate-limit probe confirms the backoff is still
                // warranted; an error probe is ambiguous and shouldn't quietly
                // clear a backoff earned by real request failures).
                if result == KeyProbeResult::Ok {
                    this.state.update(cx, |state, cx| state.clear_slot_backoff(slot, cx));
                }
                this.probe_results[idx] = Some(result);
                this.probe_tasks[idx] = None;
                cx.notify();
            });
        });
        self.probe_tasks[idx] = Some(task);
        cx.notify();
    }

    /// Builds the right-hand action cluster for a configured-key card:
    /// [Clear backoff?] [Check] [Reset].
    ///
    /// - `Clear backoff` only appears when `status.is_backed_off` (no point
    ///   clearing a healthy slot). Escape hatch for when the upstream quota
    ///   resets before the 1h window elapses.
    /// - `Check` probes the key and reflects the latest result on its face:
    ///   `Check…` (in flight, disabled), `check(ok)` (green), `check(hit)`
    ///   (warning, rate-limited), `check(err)` (error, other) with the message
    ///   as a tooltip.
    /// - `Reset` clears the key (unchanged from before).
    ///
    /// `id_prefix` must be unique per slot so the `Button::new` ids don't
    /// collide across the three cards.
    fn render_key_actions(
        &self,
        slot: KeySlot,
        status: &SlotHealthStatus,
        env_var_set: bool,
        env_var_name: &SharedString,
        id_prefix: &'static str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let idx = slot_index(slot);
        let probe_in_flight = self.probe_tasks[idx].is_some();
        let probe_result = self.probe_results[idx].clone();

        let clear_button = status.is_backed_off.then(|| {
            Button::new(format!("{id_prefix}-clear"), "Clear")
                .label_size(LabelSize::Small)
                .start_icon(Icon::new(IconName::XCircle).size(IconSize::Small))
                .layer(ElevationIndex::ModalSurface)
                .tooltip(Tooltip::text(
                    "Clear this key's backoff and re-qualify it immediately. \
                     Use when the upstream quota has already reset.",
                ))
                .on_click(cx.listener(move |this, _, _window, cx| this.clear_backoff(slot, cx)))
        });

        // Check button: label + color reflect the latest probe result.
        // The tooltip message is built as a plain `Option<SharedString>` first,
        // then wrapped in `Tooltip::text` once after the match — otherwise each
        // match arm would produce a distinct opaque `impl Fn` type and the arms
        // wouldn't unify.
        let (check_label, check_color, check_tooltip_msg): (String, Color, Option<SharedString>) =
            match (&probe_result, probe_in_flight) {
                (_, true) => ("Check…".to_string(), Color::Default, None),
                (Some(KeyProbeResult::Ok), false) => {
                    ("check(ok)".to_string(), Color::Success, None)
                }
                (Some(KeyProbeResult::RateLimit), false) => (
                    "check(hit)".to_string(),
                    Color::Warning,
                    Some("Upstream returned a rate-limit (429) for this key.".into()),
                ),
                (Some(KeyProbeResult::Err(msg)), false) => (
                    "check(err)".to_string(),
                    Color::Error,
                    Some(format!("Probe failed: {msg}").into()),
                ),
                (None, false) => ("Check".to_string(), Color::Default, None),
            };
        let check_button = Button::new(format!("{id_prefix}-check"), check_label)
            .label_size(LabelSize::Small)
            .start_icon(Icon::new(IconName::MagnifyingGlass).size(IconSize::Small))
            .layer(ElevationIndex::ModalSurface)
            .disabled(probe_in_flight)
            .color(Some(check_color))
            .when_some(check_tooltip_msg, |this, msg| {
                this.tooltip(Tooltip::text(msg))
            })
            .on_click(cx.listener(move |this, _, window, cx| this.probe_key(slot, window, cx)));

        let reset_tooltip = env_var_set.then(|| {
            Tooltip::text(format!(
                "To reset your API key, unset the {env_var_name} environment variable."
            ))
        });
        let reset_button = Button::new(format!("{id_prefix}-reset"), "Reset")
            .label_size(LabelSize::Small)
            .start_icon(Icon::new(IconName::Undo).size(IconSize::Small))
            .layer(ElevationIndex::ModalSurface)
            .when_some(reset_tooltip, |this, tooltip| this.tooltip(tooltip))
            .on_click(cx.listener(move |this, _, window, cx| match slot {
                KeySlot::Primary => this.reset_api_key(window, cx),
                KeySlot::Secondary => this.reset_api_key_2(window, cx),
                KeySlot::Tertiary => this.reset_api_key_3(window, cx),
                KeySlot::Quaternary => this.reset_api_key_4(window, cx),
            }));

        h_flex()
            .flex_shrink_0()
            .gap_1()
            .when_some(clear_button, |this, btn| this.child(btn))
            .child(check_button)
            .child(reset_button)
    }

    /// Builds the left-hand status row of a configured-key card. Shows a green
    /// check by default; replaces it with a warning icon + live backoff
    /// countdown when the slot is currently backed off. The countdown string
    /// stays in sync with `backoff_refresh_task`, which polls every second and
    /// re-renders when the snapshot changes.
    ///
    /// `badge_id` must be unique per slot so the stateful tooltip div doesn't
    /// collide across the three rendered cards.
    fn render_key_status_row(
        status: &SlotHealthStatus,
        label_text: SharedString,
        badge_id: impl Into<ElementId>,
    ) -> impl IntoElement {
        let label_node = div()
            .w_full()
            .overflow_x_hidden()
            .text_ellipsis()
            .child(Label::new(label_text));

        if status.is_backed_off {
            let countdown = format_backoff_remaining(status.backoff_remaining);
            let failures = status.consecutive_failures;
            let tooltip_msg = format!(
                "Key temporarily rotated out after {failures} consecutive \
                 failure(s). Auto-recovers when backoff expires (max 5h)."
            );
            h_flex()
                .flex_1()
                .min_w_0()
                .gap_1()
                .child(Icon::new(IconName::Warning).color(Color::Warning))
                .child(label_node)
                .child(
                    // `tooltip` is on `StatefulInteractiveElement`, so the div
                    // needs an id to become stateful.
                    div()
                        .id(badge_id)
                        .tooltip(Tooltip::text(tooltip_msg))
                        .child(
                            Label::new(format!("In backoff: {countdown}"))
                                .size(LabelSize::Small)
                                .color(Color::Warning),
                        ),
                )
        } else {
            h_flex()
                .flex_1()
                .min_w_0()
                .gap_1()
                .child(Icon::new(IconName::Check).color(Color::Success))
                .child(label_node)
        }
    }
}

impl Render for ConfigurationView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state.read(cx);

        let primary_env_var_set = state.api_key_state.is_from_env_var();
        let primary_env_var_name = state.api_key_state.env_var_name().clone();
        let primary_has_key = state.api_key_state.has_key();

        let secondary_env_var_set = state.api_key_state_2.is_from_env_var();
        let secondary_env_var_name = state.api_key_state_2.env_var_name().clone();
        let secondary_has_key = state.api_key_state_2.has_key();

        let tertiary_env_var_set = state.api_key_state_3.is_from_env_var();
        let tertiary_env_var_name = state.api_key_state_3.env_var_name().clone();
        let tertiary_has_key = state.api_key_state_3.has_key();

        let quaternary_env_var_set = state.api_key_state_4.is_from_env_var();
        let quaternary_env_var_name = state.api_key_state_4.env_var_name().clone();
        let quaternary_has_key = state.api_key_state_4.has_key();

        let api_url = state.settings.api_url.clone();

        // Per-slot health snapshot powers the backoff badge + countdown. Read
        // once per render; the `backoff_refresh_task` polls every second and
        // calls `cx.notify()` when this snapshot changes.
        let health_snapshot = state.slot_health_snapshot();
        let primary_status = &health_snapshot[0];
        let secondary_status = &health_snapshot[1];
        let tertiary_status = &health_snapshot[2];
        let quaternary_status = &health_snapshot[3];

        // Truncated key previews (e.g. `sk-...x9F`) so the user can tell cards
        // apart. Only available for keychain-stored keys; env-var keys keep the
        // env-var label since we deliberately don't read the env value here.
        let primary_preview = if primary_env_var_set { None } else { state.key_preview(KeySlot::Primary) };
        let secondary_preview = if secondary_env_var_set { None } else { state.key_preview(KeySlot::Secondary) };
        let tertiary_preview = if tertiary_env_var_set { None } else { state.key_preview(KeySlot::Tertiary) };
        let quaternary_preview = if quaternary_env_var_set { None } else { state.key_preview(KeySlot::Quaternary) };

        // Primary API key section
        let primary_section = if !primary_has_key {
            v_flex()
                .on_action(cx.listener(Self::save_api_key))
                .child(Label::new("To use Zed's agent with an OpenAI-compatible provider, you need to add an API key."))
                .child(
                    div()
                        .pt(DynamicSpacing::Base04.rems(cx))
                        .child(self.api_key_editor.clone())
                )
                .child(
                    Label::new(
                        format!("You can also set the {primary_env_var_name} environment variable and restart Zed."),
                    )
                    .size(LabelSize::Small).color(Color::Muted),
                )
                .into_any()
        } else {
            let label_text = if primary_env_var_set {
                format!("Primary API key set in {primary_env_var_name} environment variable")
            } else if let Some(preview) = primary_preview.as_deref() {
                format!("Primary: {preview}")
            } else {
                format!("Primary API key configured for {api_url}")
            };
            h_flex()
                .mt_1()
                .p_1()
                .justify_between()
                .rounded_md()
                .border_1()
                .border_color(cx.theme().colors().border)
                .bg(cx.theme().colors().background)
                .child(Self::render_key_status_row(primary_status, label_text.into(), "primary-backoff-badge"))
                .child(self.render_key_actions(
                    KeySlot::Primary,
                    primary_status,
                    primary_env_var_set,
                    &primary_env_var_name,
                    "primary",
                    cx,
                ))
                .into_any()
        };

        // Secondary API key section (optional, for load balancing)
        let secondary_section = if !secondary_has_key {
            v_flex()
                .on_action(cx.listener(Self::save_api_key_2))
                .mt_2()
                .child(
                    Label::new("Additional API Key (optional)")
                        .size(LabelSize::Small)
                )
                .child(
                    Label::new(
                        "Add a second key to enable random load balancing across both keys.",
                    )
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
                .child(
                    div()
                        .pt(DynamicSpacing::Base04.rems(cx))
                        .child(self.api_key_editor_2.clone())
                )
                .child(
                    Label::new(
                        format!("You can also set the {secondary_env_var_name} environment variable and restart Zed."),
                    )
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
                .into_any()
        } else {
            let label_text: SharedString = if secondary_env_var_set {
                format!("Secondary API key set in {secondary_env_var_name} environment variable").into()
            } else if let Some(preview) = secondary_preview.as_deref() {
                format!("Secondary: {preview}").into()
            } else {
                "Secondary API key configured for load balancing".into()
            };
            h_flex()
                .mt_1()
                .p_1()
                .justify_between()
                .rounded_md()
                .border_1()
                .border_color(cx.theme().colors().border)
                .bg(cx.theme().colors().background)
                .child(Self::render_key_status_row(secondary_status, label_text, "secondary-backoff-badge"))
                .child(self.render_key_actions(
                    KeySlot::Secondary,
                    secondary_status,
                    secondary_env_var_set,
                    &secondary_env_var_name,
                    "secondary",
                    cx,
                ))
                .into_any()
        };

        // Tertiary API key section (optional, for load balancing + backoff rotation)
        let tertiary_section = if !tertiary_has_key {
            v_flex()
                .on_action(cx.listener(Self::save_api_key_3))
                .mt_2()
                .child(
                    Label::new("Additional API Key (optional)")
                        .size(LabelSize::Small)
                )
                .child(
                    Label::new(
                        "Add a third key for broader load balancing. Failing keys are temporarily rotated out (up to 5h).",
                    )
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
                .child(
                    div()
                        .pt(DynamicSpacing::Base04.rems(cx))
                        .child(self.api_key_editor_3.clone())
                )
                .child(
                    Label::new(
                        format!("You can also set the {tertiary_env_var_name} environment variable and restart Zed."),
                    )
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
                .into_any()
        } else {
            let label_text: SharedString = if tertiary_env_var_set {
                format!("Tertiary API key set in {tertiary_env_var_name} environment variable").into()
            } else if let Some(preview) = tertiary_preview.as_deref() {
                format!("Tertiary: {preview}").into()
            } else {
                "Tertiary API key configured for load balancing".into()
            };
            h_flex()
                .mt_1()
                .p_1()
                .justify_between()
                .rounded_md()
                .border_1()
                .border_color(cx.theme().colors().border)
                .bg(cx.theme().colors().background)
                .child(Self::render_key_status_row(tertiary_status, label_text, "tertiary-backoff-badge"))
                .child(self.render_key_actions(
                    KeySlot::Tertiary,
                    tertiary_status,
                    tertiary_env_var_set,
                    &tertiary_env_var_name,
                    "tertiary",
                    cx,
                ))
                .into_any()
        };

        // Quaternary API key section (optional, for load balancing + backoff rotation)
        let quaternary_section = if !quaternary_has_key {
            v_flex()
                .on_action(cx.listener(Self::save_api_key_4))
                .mt_2()
                .child(
                    Label::new("Additional API Key (optional)")
                        .size(LabelSize::Small)
                )
                .child(
                    Label::new(
                        "Add a fourth key for broader load balancing. Failing keys are temporarily rotated out (up to 5h).",
                    )
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
                .child(
                    div()
                        .pt(DynamicSpacing::Base04.rems(cx))
                        .child(self.api_key_editor_4.clone())
                )
                .child(
                    Label::new(
                        format!("You can also set the {quaternary_env_var_name} environment variable and restart Zed."),
                    )
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
                .into_any()
        } else {
            let label_text: SharedString = if quaternary_env_var_set {
                format!("Quaternary API key set in {quaternary_env_var_name} environment variable").into()
            } else if let Some(preview) = quaternary_preview.as_deref() {
                format!("Quaternary: {preview}").into()
            } else {
                "Quaternary API key configured for load balancing".into()
            };
            h_flex()
                .mt_1()
                .p_1()
                .justify_between()
                .rounded_md()
                .border_1()
                .border_color(cx.theme().colors().border)
                .bg(cx.theme().colors().background)
                .child(Self::render_key_status_row(quaternary_status, label_text, "quaternary-backoff-badge"))
                .child(self.render_key_actions(
                    KeySlot::Quaternary,
                    quaternary_status,
                    quaternary_env_var_set,
                    &quaternary_env_var_name,
                    "quaternary",
                    cx,
                ))
                .into_any()
        };

        if self.load_credentials_task.is_some() {
            div().child(Label::new("Loading credentials…")).into_any()
        } else {
            v_flex()
                .size_full()
                .child(primary_section)
                .child(secondary_section)
                .child(tertiary_section)
                .child(quaternary_section)
                .into_any()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use health::KeyHealth;
    use serde_json::json;
    use std::future::Future;
    use std::pin::Pin;

    fn available_model(reasoning_effort: Option<open_ai::ReasoningEffort>) -> AvailableModel {
        AvailableModel {
            name: "custom-model".to_string(),
            display_name: None,
            max_tokens: 128_000,
            max_output_tokens: None,
            max_completion_tokens: None,
            reasoning_effort,
            capabilities: ModelCapabilities {
                chat_completions: false,
                ..Default::default()
            },
        }
    }

    #[test]
    fn configured_reasoning_effort_supports_thinking() {
        assert_eq!(
            default_thinking_reasoning_effort(&available_model(Some(
                open_ai::ReasoningEffort::High
            ))),
            Some(open_ai::ReasoningEffort::High)
        );
    }

    #[test]
    fn missing_or_none_reasoning_effort_does_not_support_thinking() {
        assert_eq!(
            default_thinking_reasoning_effort(&available_model(None)),
            None
        );
        assert_eq!(
            default_thinking_reasoning_effort(&available_model(Some(
                open_ai::ReasoningEffort::None
            ))),
            None
        );
    }

    #[test]
    fn supported_thinking_effort_levels_use_configured_effort_as_default() {
        let effort_levels = supported_thinking_effort_levels(&available_model(Some(
            open_ai::ReasoningEffort::High,
        )));
        let values = effort_levels
            .iter()
            .map(|level| level.value.as_ref())
            .collect::<Vec<_>>();

        assert_eq!(values, ["minimal", "low", "medium", "high", "xhigh", "max"]);
        assert_eq!(
            effort_levels
                .iter()
                .find(|level| level.is_default)
                .map(|level| level.value.as_ref()),
            Some("high")
        );
    }

    /// Minimal `CredentialsProvider` impl for unit tests; not actually read
    /// since these tests construct `State` directly without going through
    /// `authenticate`.
    struct FakeCredentialsProvider;
    impl CredentialsProvider for FakeCredentialsProvider {
        fn read_credentials<'a>(
            &'a self,
            _url: &'a str,
            _cx: &'a AsyncApp,
        ) -> Pin<Box<dyn Future<Output = Result<Option<(String, Vec<u8>)>>> + 'a>> {
            Box::pin(async { Ok(None) })
        }
        fn write_credentials<'a>(
            &'a self,
            _url: &'a str,
            _username: &'a str,
            _password: &'a [u8],
            _cx: &'a AsyncApp,
        ) -> Pin<Box<dyn Future<Output = Result<()>> + 'a>> {
            Box::pin(async { Ok(()) })
        }
        fn delete_credentials<'a>(
            &'a self,
            _url: &'a str,
            _cx: &'a AsyncApp,
        ) -> Pin<Box<dyn Future<Output = Result<()>> + 'a>> {
            Box::pin(async { Ok(()) })
        }
    }

    fn fake_state_with_no_keys() -> State {
        State {
            id: "test".into(),
            api_key_state: ApiKeyState::new(
                "https://example.test".into(),
                EnvVar::new("TEST_API_KEY".into()),
            ),
            api_key_state_2: ApiKeyState::new(
                secondary_key_url("https://example.test"),
                EnvVar::new("TEST_API_KEY_2".into()),
            ),
            api_key_state_3: ApiKeyState::new(
                tertiary_key_url("https://example.test"),
                EnvVar::new("TEST_API_KEY_3".into()),
            ),
            api_key_state_4: ApiKeyState::new(
                quaternary_key_url("https://example.test"),
                EnvVar::new("TEST_API_KEY_4".into()),
            ),
            key_health: Arc::new(ParkingMutex::new(KeyHealthTracker::default())),
            key_health_dirty: Arc::new(ParkingMutex::new(None)),
            key_health_path: key_health_path_for("test"),
            settings: OpenAiCompatibleSettings {
                api_url: "https://example.test".to_string(),
                ..Default::default()
            },
            credentials_provider: Arc::new(FakeCredentialsProvider),
        }
    }

    #[test]
    fn gather_candidates_returns_nothing_for_fake_state_with_no_keys() {
        // Sanity: the test fixture really has no keys configured, otherwise
        // the slot_health_snapshot tests below would be misleading.
        let state = fake_state_with_no_keys();
        assert!(state.gather_candidates().is_empty());
    }

    // ------------------------------------------------------------------
    // slot_health_snapshot tests
    //
    // These exercise the UI-facing projection of per-key health exposed by
    // `State`. They build a fresh `State`, push failures directly into
    // `key_health` (bypassing the request closure), and verify the snapshot
    // returns the expected mix of healthy / backed-off / unconfigured slots.
    // The underlying `KeyHealth` / `KeyHealthTracker` mechanics (backoff
    // computation, expiry, persistence) are tested in `health.rs`.
    // ------------------------------------------------------------------

    #[test]
    fn slot_health_snapshot_fresh_state_is_all_clear() {
        // No keys configured, no failures — every slot should report
        // `has_key: false`, `is_backed_off: false`, zero failures, `enabled: true`.
        let state = fake_state_with_no_keys();
        let snapshot = state.slot_health_snapshot();
        for status in &snapshot {
            assert!(!status.has_key, "no keys should be configured");
            assert!(!status.is_backed_off, "fresh slot should not be backed off");
            assert_eq!(status.consecutive_failures, 0);
            assert_eq!(status.backoff_remaining, Duration::ZERO);
            assert!(status.enabled, "fresh slot should default to enabled");
        }
    }

    #[test]
    fn slot_health_snapshot_reports_backed_off_slot() {
        let state = fake_state_with_no_keys();
        // Poison Primary directly via the shared mutex, with a backoff window
        // well into the future so it can't accidentally expire mid-test.
        {
            let mut tracker = state.key_health.lock();
            tracker.primary = KeyHealth {
                consecutive_failures: 2,
                backoff_until: Some(Instant::now() + Duration::from_secs(300)),
                ..Default::default()
            };
        }
        let snapshot = state.slot_health_snapshot();
        // [Primary, Secondary, Tertiary]
        assert!(snapshot[0].is_backed_off, "Primary should be backed off");
        assert_eq!(snapshot[0].consecutive_failures, 2);
        // 300s backoff, so remaining should be just under 300s. We don't assert
        // the exact value (depends on how long the lock + read took), only that
        // it's in the expected band.
        assert!(
            snapshot[0].backoff_remaining > Duration::from_secs(290)
                && snapshot[0].backoff_remaining <= Duration::from_secs(300),
            "Primary backoff_remaining should be ~300s, got {:?}",
            snapshot[0].backoff_remaining
        );
        // Other slots untouched.
        assert!(!snapshot[1].is_backed_off);
        assert!(!snapshot[2].is_backed_off);
    }

    #[test]
    fn slot_health_snapshot_handles_expired_backoff_gracefully() {
        // If `backoff_until` is in the past, the slot should read as NOT backed
        // off (auto-recovered) and `backoff_remaining` should be ZERO (not
        // negative — `saturating_duration_since` clamps).
        let state = fake_state_with_no_keys();
        {
            let mut tracker = state.key_health.lock();
            tracker.secondary = KeyHealth {
                consecutive_failures: 5,
                backoff_until: Some(Instant::now() - Duration::from_secs(60)),
                ..Default::default()
            };
        }
        let snapshot = state.slot_health_snapshot();
        assert!(!snapshot[1].is_backed_off, "expired backoff should auto-clear");
        assert_eq!(
            snapshot[1].backoff_remaining, Duration::ZERO,
            "expired backoff remaining should clamp to zero"
        );
        // consecutive_failures is still recorded (historical), even though the
        // slot is no longer backed off. The UI badge only shows when
        // is_backed_off is true, so this doesn't affect rendering.
        assert_eq!(snapshot[1].consecutive_failures, 5);
    }

    // ------------------------------------------------------------------
    // clear_slot_backoff contract
    //
    // Verifies the in-memory mutation `clear_slot_backoff` performs: only the
    // targeted slot's health is reset, siblings are untouched. This is also
    // the method called from `ConfigurationView::reset_api_key*` when the
    // user clears/resets a slot's key, and from the "Clear" button. The
    // persist scheduling is exercised by the persistence tests in `health.rs`.
    // ------------------------------------------------------------------

    #[test]
    fn clear_slot_backoff_clears_only_targeted_slot() {
        let state = fake_state_with_no_keys();
        // Poison every slot distinctly.
        {
            let mut tracker = state.key_health.lock();
            tracker.primary = KeyHealth {
                consecutive_failures: 3,
                backoff_until: Some(Instant::now() + Duration::from_secs(300)),
                ..Default::default()
            };
            tracker.secondary = KeyHealth {
                consecutive_failures: 2,
                backoff_until: Some(Instant::now() + Duration::from_secs(60)),
                ..Default::default()
            };
            tracker.tertiary = KeyHealth {
                consecutive_failures: 5,
                backoff_until: Some(Instant::now() + Duration::from_secs(3600)),
                ..Default::default()
            };
        }

        // The exact in-memory operation `clear_slot_backoff(Secondary)` performs.
        {
            let mut tracker = state.key_health.lock();
            let health = tracker.get_mut(KeySlot::Secondary);
            health.consecutive_failures = 0;
            health.backoff_until = None;
        }

        let after = state.slot_health_snapshot();
        // Secondary cleared.
        assert!(!after[1].is_backed_off, "Secondary should be cleared");
        assert_eq!(after[1].consecutive_failures, 0);
        // Primary + Tertiary untouched (still poisoned).
        assert!(after[0].is_backed_off, "Primary should still be backed off");
        assert_eq!(after[0].consecutive_failures, 3);
        assert!(after[2].is_backed_off, "Tertiary should still be backed off");
        assert_eq!(after[2].consecutive_failures, 5);
    }

    // ------------------------------------------------------------------
    // truncate_key_preview + slot_index + classify_probe_error helpers
    // ------------------------------------------------------------------

    #[test]
    fn truncate_key_preview_short_key_returned_as_is() {
        // At or below the HEAD+TAIL threshold (6 chars) the key is too short to
        // truncate meaningfully, so it's returned verbatim.
        assert_eq!(truncate_key_preview("abc"), "abc");
        assert_eq!(truncate_key_preview("abcdef"), "abcdef");
    }

    #[test]
    fn truncate_key_preview_long_key_shows_head_and_tail() {
        assert_eq!(truncate_key_preview("sk-abcdef1234567xyz"), "sk-...xyz");
        // 7 chars: just over the threshold → head3 + tail3 (one char dropped).
        assert_eq!(truncate_key_preview("abcdefg"), "abc...efg");
    }

    #[test]
    fn truncate_key_preview_is_char_safe_for_multibyte() {
        // A key whose 4th byte lands inside a multi-byte char (é = 2 bytes)
        // must not panic on slicing. `truncate_key_preview` uses `chars()`, so
        // the split happens on character boundaries.
        let key = "abcédéfg"; // chars: a b c é d é f g (8 chars)
        let preview = truncate_key_preview(key);
        // head = "abc", tail = "éfg"
        assert_eq!(preview, "abc...éfg");
        // Emoji (4-byte) at the boundary likewise must not panic.
        let key = "abc😀defg"; // chars: a b c 😀 d e f g
        let preview = truncate_key_preview(key);
        assert_eq!(preview, "abc...efg");
    }

    #[test]
    fn slot_index_maps_slots_in_order() {
        assert_eq!(slot_index(KeySlot::Primary), 0);
        assert_eq!(slot_index(KeySlot::Secondary), 1);
        assert_eq!(slot_index(KeySlot::Tertiary), 2);
        assert_eq!(slot_index(KeySlot::Quaternary), 3);
    }

    #[test]
    fn classify_probe_error_recognizes_rate_limit_status() {
        let err = open_ai::RequestError::HttpResponseError {
            provider: "test".to_string(),
            status_code: http_client::http::StatusCode::TOO_MANY_REQUESTS,
            body: String::new(),
            headers: Box::new(http_client::http::HeaderMap::new()),
        };
        assert_eq!(classify_probe_error(err), KeyProbeResult::RateLimit);
    }

    #[test]
    fn classify_probe_error_other_status_becomes_err_with_message() {
        let err = open_ai::RequestError::HttpResponseError {
            provider: "test".to_string(),
            status_code: http_client::http::StatusCode::UNAUTHORIZED,
            body: "unauthorized".to_string(),
            headers: Box::new(http_client::http::HeaderMap::new()),
        };
        match classify_probe_error(err) {
            KeyProbeResult::Err(msg) => {
                assert!(msg.as_ref().contains("401"), "message should mention status: {msg}");
            }
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[test]
    fn supported_thinking_effort_levels_hide_missing_or_none_effort() {
        assert!(supported_thinking_effort_levels(&available_model(None)).is_empty());
        assert!(
            supported_thinking_effort_levels(&available_model(Some(
                open_ai::ReasoningEffort::None
            )))
            .is_empty()
        );
    }

    #[test]
    fn chat_completion_reasoning_effort_honors_request_and_configured_effort() {
        let model = available_model(Some(open_ai::ReasoningEffort::Medium));
        let mut request = LanguageModelRequest {
            thinking_allowed: true,
            ..Default::default()
        };

        assert_eq!(
            chat_completion_reasoning_effort(&request, &model),
            Some(open_ai::ReasoningEffort::Medium)
        );

        request.thinking_effort = Some("high".to_string());
        assert_eq!(
            chat_completion_reasoning_effort(&request, &model),
            Some(open_ai::ReasoningEffort::High)
        );

        request.thinking_effort = Some("not-supported".to_string());
        assert_eq!(
            chat_completion_reasoning_effort(&request, &model),
            Some(open_ai::ReasoningEffort::Medium)
        );

        request.thinking_allowed = false;
        assert_eq!(
            chat_completion_reasoning_effort(&request, &model),
            Some(open_ai::ReasoningEffort::None)
        );
    }

    #[test]
    fn chat_completion_reasoning_effort_omits_missing_effort() {
        let model = available_model(None);
        let request = LanguageModelRequest {
            thinking_allowed: false,
            ..Default::default()
        };

        assert_eq!(chat_completion_reasoning_effort(&request, &model), None);
    }

    #[test]
    fn chat_completion_reasoning_effort_preserves_explicit_none() {
        let model = available_model(Some(open_ai::ReasoningEffort::None));
        let request = LanguageModelRequest {
            thinking_allowed: true,
            thinking_effort: Some("high".to_string()),
            ..Default::default()
        };

        assert_eq!(
            chat_completion_reasoning_effort(&request, &model),
            Some(open_ai::ReasoningEffort::None)
        );
    }

    #[test]
    fn chat_completion_max_tokens_parameter_defaults_to_max_completion_tokens() {
        let model = available_model(Some(open_ai::ReasoningEffort::Medium));

        assert_eq!(
            chat_completion_max_tokens_parameter(&model),
            crate::provider::open_ai::ChatCompletionMaxTokensParameter::MaxCompletionTokens
        );
    }

    #[test]
    fn chat_completion_max_tokens_parameter_uses_max_tokens_when_configured() {
        let mut model = available_model(Some(open_ai::ReasoningEffort::Medium));
        model.capabilities.max_tokens_parameter = true;

        assert_eq!(
            chat_completion_max_tokens_parameter(&model),
            crate::provider::open_ai::ChatCompletionMaxTokensParameter::MaxTokens
        );
    }

    #[test]
    fn response_request_includes_reasoning_when_effort_is_configured() {
        let model = available_model(Some(open_ai::ReasoningEffort::High));
        let request = LanguageModelRequest {
            thinking_allowed: true,
            ..Default::default()
        };

        let request = into_open_ai_response(
            request,
            &model.name,
            model.capabilities.parallel_tool_calls,
            model.capabilities.prompt_cache_key,
            model.max_output_tokens,
            default_thinking_reasoning_effort(&model),
            supports_none_reasoning_effort(&model),
            &LanguageModelProviderId::new("test-compatible-provider"),
        )
        .unwrap();
        let serialized = serde_json::to_value(request).unwrap();

        assert_eq!(
            serialized["reasoning"],
            json!({ "effort": "high", "summary": "auto" })
        );
        assert_eq!(
            serialized["include"],
            json!(["reasoning.encrypted_content"])
        );
    }

    #[test]
    fn response_request_omits_reasoning_when_effort_is_missing() {
        let model = available_model(None);
        let request = LanguageModelRequest {
            thinking_allowed: true,
            ..Default::default()
        };

        let request = into_open_ai_response(
            request,
            &model.name,
            model.capabilities.parallel_tool_calls,
            model.capabilities.prompt_cache_key,
            model.max_output_tokens,
            default_thinking_reasoning_effort(&model),
            supports_none_reasoning_effort(&model),
            &LanguageModelProviderId::new("test-compatible-provider"),
        )
        .unwrap();
        let serialized = serde_json::to_value(request).unwrap();

        assert_eq!(serialized.get("reasoning"), None);
        assert_eq!(serialized.get("include"), None);
    }

    #[test]
    fn chat_completion_request_includes_selected_reasoning_effort() {
        let mut model = available_model(Some(open_ai::ReasoningEffort::Medium));
        model.capabilities.chat_completions = true;
        let request = LanguageModelRequest {
            thinking_allowed: true,
            thinking_effort: Some("high".to_string()),
            ..Default::default()
        };
        let reasoning_effort = chat_completion_reasoning_effort(&request, &model);

        let request = into_open_ai(
            request,
            &model.name,
            model.capabilities.parallel_tool_calls,
            model.capabilities.prompt_cache_key,
            model.max_output_tokens,
            chat_completion_max_tokens_parameter(&model),
            reasoning_effort,
            model.capabilities.interleaved_reasoning,
        )
        .unwrap();
        let serialized = serde_json::to_value(request).unwrap();

        assert_eq!(serialized["reasoning_effort"], json!("high"));
    }

    #[test]
    fn configured_reasoning_effort_supports_none_reasoning_effort() {
        assert!(supports_none_reasoning_effort(&available_model(Some(
            open_ai::ReasoningEffort::Medium
        ))));
        assert!(supports_none_reasoning_effort(&available_model(Some(
            open_ai::ReasoningEffort::None
        ))));
        assert!(!supports_none_reasoning_effort(&available_model(None)));
    }

    #[test]
    fn response_thinking_effort_preserves_explicit_none() {
        let model = available_model(Some(open_ai::ReasoningEffort::None));
        let mut request = LanguageModelRequest {
            thinking_allowed: true,
            thinking_effort: Some("high".to_string()),
            ..Default::default()
        };

        disable_response_thinking_for_none_effort(&mut request, &model);
        assert!(!request.thinking_allowed);
        assert_eq!(request.thinking_effort, None);
    }
}
