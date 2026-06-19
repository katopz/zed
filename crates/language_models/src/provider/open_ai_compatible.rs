use anyhow::Result;
use convert_case::{Case, Casing};
use credentials_provider::CredentialsProvider;
use fs::Fs;
use futures::{FutureExt, StreamExt, future::BoxFuture};
use gpui::{AnyView, App, AsyncApp, Context, ElementId, Entity, SharedString, Task, TaskExt, Window};
use http_client::{CustomHeaders, HttpClient};
use language_model::{
    ApiKeyState, AuthenticateError, EnvVar, IconOrSvg, LanguageModel, LanguageModelCompletionError,
    LanguageModelCompletionEvent, LanguageModelId, LanguageModelName, LanguageModelProvider,
    LanguageModelProviderId, LanguageModelProviderName, LanguageModelProviderState,
    LanguageModelRequest, LanguageModelToolChoice, LanguageModelToolSchemaFormat, RateLimiter,
};
use menu;
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

mod health;
use health::{
    KeyHealthTracker, KeySlot, SlotHealthStatus, format_backoff_remaining,
    key_health_path_for, probe_first_event, reload_persisted_health, retry_stream,
    schedule_persist_key_health_inner,
};

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
    /// Shared across threads so the request closure (background executor) can
    /// record outcomes without going through GPUI's `Entity::update`.
    /// `Arc<Mutex>` because `AsyncApp` is `!Send` and can't be moved into the
    /// rate-limited stream closure.
    key_health: Arc<std::sync::Mutex<KeyHealthTracker>>,
    /// Latest pending debounced save task. Replacing this cancels the prior
    /// task, coalescing bursts of failures into a single disk write.
    key_health_dirty: Arc<std::sync::Mutex<Option<Task<()>>>>,
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

impl State {
    fn is_authenticated(&self) -> bool {
        self.api_key_state.has_key()
            || self.api_key_state_2.has_key()
            || self.api_key_state_3.has_key()
    }

    /// Schedules a debounced write of the current `key_health` snapshot to
    /// `key_health_path`. Each call cancels any prior pending save (by
    /// replacing the stored `Task`), coalescing a burst of failures from a
    /// single retry loop into one disk write. Called after every request
    /// outcome (success or failure) from `stream_completion` / `stream_response`
    /// via the free-function form `schedule_persist_key_health_inner` (which
    /// takes `Send`-safe handles, not `AsyncApp`).
    ///
    /// This method exists for symmetry + future foreground-triggered saves
    /// (e.g. on `reset_credentials`). Currently unused; the request path uses
    /// the free-function form because `AsyncApp` is `!Send` and the rate-limited
    /// stream closure must be `Send`.
    #[allow(dead_code)]
    fn schedule_persist_key_health(&self, cx: &App) {
        schedule_persist_key_health_inner(
            &self.key_health,
            &self.key_health_dirty,
            self.key_health_path.clone(),
            cx.background_executor().clone(),
            <dyn Fs>::global(cx),
        );
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

    fn authenticate(&mut self, cx: &mut Context<Self>) -> Task<Result<(), AuthenticateError>> {
        let credentials_provider = self.credentials_provider.clone();
        let api_url = SharedString::new(self.settings.api_url.clone());
        let secondary_url = secondary_key_url(&api_url);
        let tertiary_url = tertiary_key_url(&api_url);

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
            credentials_provider,
            cx,
        );

        cx.background_spawn(async move {
            let result1 = task1.await;
            let result2 = task2.await;
            let result3 = task3.await;
            if result1.is_ok() || result2.is_ok() || result3.is_ok() {
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
    fn gather_candidates(&self) -> Vec<(Arc<str>, KeySlot)> {
        let primary_url = self.settings.api_url.as_str();
        let secondary_url = secondary_key_url(primary_url);
        let tertiary_url = tertiary_key_url(primary_url);

        let mut out = Vec::with_capacity(3);
        if let Some(key) = self.api_key_state.key(primary_url) {
            out.push((key, KeySlot::Primary));
        }
        if let Some(key) = self.api_key_state_2.key(&secondary_url) {
            out.push((key, KeySlot::Secondary));
        }
        if let Some(key) = self.api_key_state_3.key(&tertiary_url) {
            out.push((key, KeySlot::Tertiary));
        }
        out
    }

    /// Returns `[Primary, Secondary, Tertiary]` slot status for the UI. Clones
    /// the tracker under the mutex (same pattern as `snapshot_health`) so the
    /// lock is not held across the per-slot computation. Used by
    /// `ConfigurationView::render` to draw a backoff badge with a live
    /// countdown. The ConfigurationView polls this on a 1s timer while the
    /// settings page is open; see `backoff_refresh_task`.
    fn slot_health_snapshot(&self) -> [SlotHealthStatus; 3] {
        let now = Instant::now();
        let tracker = match self.key_health.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        [
            self.slot_status(KeySlot::Primary, &tracker, now),
            self.slot_status(KeySlot::Secondary, &tracker, now),
            self.slot_status(KeySlot::Tertiary, &tracker, now),
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
        }
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
                        credentials_provider,
                        cx,
                    );
                    this.settings = settings;
                    cx.notify();
                }
            })
            .detach();
            let settings = resolve_settings(&id, cx).cloned().unwrap_or_default();
            let key_health = Arc::new(std::sync::Mutex::new(KeyHealthTracker::default()));
            let key_health_dirty: Arc<std::sync::Mutex<Option<Task<()>>>> =
                Arc::new(std::sync::Mutex::new(None));
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
                match load_health.lock() {
                    Ok(mut guard) => *guard = loaded,
                    Err(poisoned) => *poisoned.into_inner() = loaded,
                }
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

    fn configuration_view(
        &self,
        _target_agent: language_model::ConfigurationViewTargetAgent,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyView {
        cx.new(|cx| ConfigurationView::new(self.state.clone(), window, cx))
            .into()
    }

    fn reset_credentials(&self, cx: &mut App) -> Task<Result<()>> {
        self.state.update(cx, |state, cx| {
            let task1 = state.set_api_key(None, cx);
            let task2 = state.set_api_key_2(None, cx);
            let task3 = state.set_api_key_3(None, cx);
            cx.background_spawn(async move {
                task1.await?;
                task2.await?;
                task3.await
            })
        })
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
            let result = retry_stream(
                &candidates,
                &key_health,
                provider,
                move |api_key| {
                    let http_client = http_client.clone();
                    let api_url = api_url.clone();
                    let extra_headers = extra_headers.clone();
                    let provider_name = provider_name.clone();
                    let attempt_request = request.clone();
                    Box::pin(async move {
                        let stream = stream_completion(
                            http_client.as_ref(),
                            provider_name.as_str(),
                            api_url.as_ref(),
                            api_key.as_ref(),
                            attempt_request,
                            &extra_headers,
                        )
                        .await?;
                        probe_first_event(stream).await
                    })
                },
            )
            .await;
            // Whether the request succeeded or rotated to exhaustion, health
            // state may have changed — schedule a debounced persist so the
            // next process restart sees the up-to-date backoff state.
            schedule_persist_key_health_inner(
                &key_health,
                &key_health_dirty,
                key_health_path.clone(),
                persist_executor.clone(),
                fs.clone(),
            );
            result
        });

        async move { Ok(future.await?.boxed()) }.boxed()
    }

    fn stream_response(
        &self,
        request: ResponseRequest,
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
            let result = retry_stream(
                &candidates,
                &key_health,
                provider,
                move |api_key| {
                    let http_client = http_client.clone();
                    let api_url = api_url.clone();
                    let extra_headers = extra_headers.clone();
                    let provider_name = provider_name.clone();
                    let attempt_request = request.clone();
                    Box::pin(async move {
                        let stream = stream_response(
                            http_client.as_ref(),
                            provider_name.as_str(),
                            api_url.as_ref(),
                            api_key.as_ref(),
                            attempt_request,
                            &extra_headers,
                        )
                        .await?;
                        probe_first_event(stream).await
                    })
                },
            )
            .await;
            // Whether the request succeeded or rotated to exhaustion, health
            // state may have changed — schedule a debounced persist so the
            // next process restart sees the up-to-date backoff state.
            schedule_persist_key_health_inner(
                &key_health,
                &key_health_dirty,
                key_health_path.clone(),
                persist_executor.clone(),
                fs.clone(),
            );
            result
        });

        async move { Ok(future.await?.boxed()) }.boxed()
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

    fn supports_split_token_display(&self) -> bool {
        true
    }

    fn telemetry_id(&self) -> String {
        format!("openai/{}", self.model.name)
    }

    fn max_token_count(&self) -> u64 {
        self.model.max_tokens
    }

    fn max_output_tokens(&self) -> Option<u64> {
        self.model.max_output_tokens
    }

    fn stream_completion(
        &self,
        request: LanguageModelRequest,
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
        if self.model.capabilities.chat_completions {
            let request = into_open_ai(
                request,
                &self.model.name,
                self.model.capabilities.parallel_tool_calls,
                self.model.capabilities.prompt_cache_key,
                self.max_output_tokens(),
                self.model.reasoning_effort,
                self.model.capabilities.interleaved_reasoning,
            );
            let completions = self.stream_completion(request, cx);
            async move {
                let mapper = OpenAiEventMapper::new();
                Ok(mapper.map_stream(completions.await?).boxed())
            }
            .boxed()
        } else {
            let request = into_open_ai_response(
                request,
                &self.model.name,
                self.model.capabilities.parallel_tool_calls,
                self.model.capabilities.prompt_cache_key,
                self.max_output_tokens(),
                self.model
                    .reasoning_effort
                    .filter(|effort| *effort != open_ai::ReasoningEffort::None),
                self.model.reasoning_effort == Some(open_ai::ReasoningEffort::None),
            );
            let completions = self.stream_response(request, cx);
            async move {
                let mapper = OpenAiResponseEventMapper::new();
                Ok(mapper.map_stream(completions.await?).boxed())
            }
            .boxed()
        }
    }
}

struct ConfigurationView {
    api_key_editor: Entity<InputField>,
    api_key_editor_2: Entity<InputField>,
    api_key_editor_3: Entity<InputField>,
    state: Entity<State>,
    load_credentials_task: Option<Task<()>>,
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
    fn new(state: Entity<State>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let api_key_editor = cx.new(|cx| {
            InputField::new(
                window,
                cx,
                "000000000000000000000000000000000000000000000000000",
            )
        });
        let api_key_editor_2 = cx.new(|cx| {
            InputField::new(
                window,
                cx,
                "000000000000000000000000000000000000000000000000000",
            )
        });
        let api_key_editor_3 = cx.new(|cx| {
            InputField::new(
                window,
                cx,
                "000000000000000000000000000000000000000000000000000",
            )
        });

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
                let mut last_snapshot: [SlotHealthStatus; 3] =
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
            state,
            load_credentials_task,
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
                .update(cx, |state, cx| state.set_api_key(None, cx))
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
                .update(cx, |state, cx| state.set_api_key_2(None, cx))
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

    fn reset_api_key_3(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.api_key_editor_3
            .update(cx, |input, cx| input.set_text("", window, cx));

        let state = self.state.clone();
        cx.spawn_in(window, async move |_, cx| {
            state
                .update(cx, |state, cx| state.set_api_key_3(None, cx))
                .await
        })
        .detach_and_log_err(cx);
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

        let api_url = state.settings.api_url.clone();

        // Per-slot health snapshot powers the backoff badge + countdown. Read
        // once per render; the `backoff_refresh_task` polls every second and
        // calls `cx.notify()` when this snapshot changes.
        let health_snapshot = state.slot_health_snapshot();
        let primary_status = &health_snapshot[0];
        let secondary_status = &health_snapshot[1];
        let tertiary_status = &health_snapshot[2];

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
                .child(
                    h_flex()
                        .flex_shrink_0()
                        .child(
                            Button::new("reset-api-key", "Reset API Key")
                                .label_size(LabelSize::Small)
                                .start_icon(Icon::new(IconName::Undo).size(IconSize::Small))
                                .layer(ElevationIndex::ModalSurface)
                                .when(primary_env_var_set, |this| {
                                    this.tooltip(Tooltip::text(format!("To reset your API key, unset the {primary_env_var_name} environment variable.")))
                                })
                                .on_click(cx.listener(|this, _, window, cx| this.reset_api_key(window, cx))),
                        ),
                )
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
                .child(
                    h_flex()
                        .flex_shrink_0()
                        .child(
                            Button::new("reset-api-key-2", "Reset")
                                .label_size(LabelSize::Small)
                                .start_icon(Icon::new(IconName::Undo).size(IconSize::Small))
                                .layer(ElevationIndex::ModalSurface)
                                .when(secondary_env_var_set, |this| {
                                    this.tooltip(Tooltip::text(format!("To reset your API key, unset the {secondary_env_var_name} environment variable.")))
                                })
                                .on_click(cx.listener(|this, _, window, cx| this.reset_api_key_2(window, cx))),
                        ),
                )
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
                .child(
                    h_flex()
                        .flex_shrink_0()
                        .child(
                            Button::new("reset-api-key-3", "Reset")
                                .label_size(LabelSize::Small)
                                .start_icon(Icon::new(IconName::Undo).size(IconSize::Small))
                                .layer(ElevationIndex::ModalSurface)
                                .when(tertiary_env_var_set, |this| {
                                    this.tooltip(Tooltip::text(format!("To reset your API key, unset the {tertiary_env_var_name} environment variable.")))
                                })
                                .on_click(cx.listener(|this, _, window, cx| this.reset_api_key_3(window, cx))),
                        ),
                )
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
                .into_any()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use health::KeyHealth;
    use std::future::Future;
    use std::pin::Pin;

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
            key_health: Arc::new(std::sync::Mutex::new(KeyHealthTracker::default())),
            key_health_dirty: Arc::new(std::sync::Mutex::new(None)),
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
        // `has_key: false`, `is_backed_off: false`, zero failures.
        let state = fake_state_with_no_keys();
        let snapshot = state.slot_health_snapshot();
        for status in &snapshot {
            assert!(!status.has_key, "no keys should be configured");
            assert!(!status.is_backed_off, "fresh slot should not be backed off");
            assert_eq!(status.consecutive_failures, 0);
            assert_eq!(status.backoff_remaining, Duration::ZERO);
        }
    }

    #[test]
    fn slot_health_snapshot_reports_backed_off_slot() {
        let state = fake_state_with_no_keys();
        // Poison Primary directly via the shared mutex, with a backoff window
        // well into the future so it can't accidentally expire mid-test.
        {
            let mut tracker = state.key_health.lock().unwrap();
            tracker.primary = KeyHealth {
                consecutive_failures: 2,
                backoff_until: Some(Instant::now() + Duration::from_secs(300)),
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
            let mut tracker = state.key_health.lock().unwrap();
            tracker.secondary = KeyHealth {
                consecutive_failures: 5,
                backoff_until: Some(Instant::now() - Duration::from_secs(60)),
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
}
