use anyhow::Result;
use convert_case::{Case, Casing};
use credentials_provider::CredentialsProvider;
use fs::Fs;
use futures::{FutureExt, StreamExt, future::BoxFuture};
use gpui::{App, AsyncApp, Context, ElementId, Entity, SharedString, Task, TaskExt, Window};
use http_client::{CustomHeaders, HttpClient};
use language_model::chat_completion::ChatCompletionEventMapper;
use language_model::{
    ApiKeyState, AuthenticateError, EnvVar, IconOrSvg, LanguageModel, LanguageModelClient,
    LanguageModelCompletionError, LanguageModelCompletionStream, LanguageModelEffortLevel,
    LanguageModelId, LanguageModelName, LanguageModelProvider, LanguageModelProviderId,
    LanguageModelProviderName, LanguageModelProviderState, LanguageModelRequest,
    LanguageModelToolChoiceSupport, ModelKeySlotStatus, ModelKeySlotStatusSummary,
    ModelRateLimiters, ProviderErrorCategory, ProviderSettingsView, RateLimiter,
    SubPageProviderSettings, unavailable_error,
};
use open_ai::{
    ResponseStreamEvent,
    responses::{Request as ResponseRequest, StreamEvent as ResponsesStreamEvent, stream_response},
    stream_completion,
};
use parking_lot::Mutex as ParkingMutex;
use settings::{Settings, SettingsStore};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use ui::{ElevationIndex, Tooltip, prelude::*};
use ui_input::InputField;
use util::ResultExt;

use crate::provider::open_ai::{OpenAiResponseEventMapper, into_open_ai, into_open_ai_response};
pub use settings::OpenAiCompatibleAvailableModel as AvailableModel;
pub use settings::OpenAiCompatibleModelCapabilities as ModelCapabilities;

// Re-exported so the agent_ui footer chips can format backoff countdowns the
// same way the ConfigurationView does, without duplicating the formatter.
pub use health::format_backoff_remaining;

mod health;
use health::{
    KeyHealthTracker, KeySlot, SlotHealthStatus, key_health_path_for, record_key_success,
    reload_persisted_health, retry_stream, schedule_persist_key_health_inner, snapshot_health,
};

mod fork;
use fork::*;

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
    request_limiters: ModelRateLimiters,
}

pub struct State {
    id: Arc<str>,
    pub api_key_state: ApiKeyState,
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

/// Outcome of a manual "Check" probe of a single key. Stored per-slot in the
/// ConfigurationView so the button face shows the latest result for that key.
#[derive(Clone, Debug, PartialEq)]
pub enum KeyProbeResult {
    /// The probe completed without an error.
    Ok,
    /// The upstream returned a rate-limit error (429). `retry_after` is the
    /// upstream-provided reset hint when one could be extracted (the
    /// `retry-after` header or a reset timestamp parsed from the error body),
    /// so the backoff can end exactly when the quota resets.
    RateLimit { retry_after: Option<Duration> },
    /// Any other error; the message is surfaced as a tooltip.
    Err(SharedString),
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
            request_limiters: ModelRateLimiters::default(),
        }
    }

    /// Every model this provider offers, in settings order.
    fn available_models<'a>(&self, cx: &'a App) -> &'a [AvailableModel] {
        &self.state.read(cx).settings.available_models
    }

    fn language_model(&self, model: &AvailableModel) -> LanguageModel {
        LanguageModel {
            supports_tools: model.capabilities.tools,
            supports_images: model.capabilities.images,
            tool_choice_support: LanguageModelToolChoiceSupport {
                auto: model.capabilities.tools,
                any: model.capabilities.tools,
                none: true,
            },
            supports_streaming_tools: true,
            supports_thinking: default_thinking_reasoning_effort(model).is_some(),
            supported_effort_levels: supported_thinking_effort_levels(model).into(),
            supports_split_token_display: true,
            max_output_tokens: model.max_output_tokens,
            ..LanguageModel::new(
                LanguageModelId::from(model.name.clone()),
                LanguageModelName::from(
                    model
                        .display_name
                        .clone()
                        .unwrap_or_else(|| model.name.clone()),
                ),
                self.id.clone(),
                self.name.clone(),
                format!("openai/{}", model.name),
                model.max_tokens,
            )
        }
    }

    /// The current configuration of `model`, if this provider still offers it.
    fn config(
        &self,
        model: &LanguageModel,
        cx: &App,
    ) -> Result<AvailableModel, LanguageModelCompletionError> {
        self.available_models(cx)
            .iter()
            .find(|available| available.name == model.id.0.as_ref())
            .cloned()
            .ok_or_else(|| unavailable_error(model))
    }

    fn stream_chat_completion(
        &self,
        request_limiter: &RateLimiter,
        request: open_ai::Request,
        thread_id: Option<String>,
        cx: &AsyncApp,
    ) -> BoxFuture<
        'static,
        Result<
            futures::stream::BoxStream<
                'static,
                Result<ResponseStreamEvent, LanguageModelCompletionError>,
            >,
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

        let provider = self.name.clone();
        let provider_name = provider.0.clone();
        // Capture the background executor and Fs handle up-front so the
        // rate-limited stream closure — which must be `Send` to satisfy
        // `BoxFuture<'static, ...>` — can schedule a debounced persist after
        // `retry_stream` without dragging the `!Send` `AsyncApp` along.
        let persist_executor = cx.background_executor().clone();
        let future = request_limiter.stream(async move {
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
        request_limiter: &RateLimiter,
        request: ResponseRequest,
        thread_id: Option<String>,
        cx: &AsyncApp,
    ) -> BoxFuture<
        'static,
        Result<
            futures::stream::BoxStream<
                'static,
                Result<ResponsesStreamEvent, open_ai::RequestError>,
            >,
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

        let provider = self.name.clone();
        let provider_name = provider.0.clone();
        // Capture the background executor and Fs handle up-front so the
        // rate-limited stream closure — which must be `Send` to satisfy
        // `BoxFuture<'static, ...>` — can schedule a debounced persist after
        // `retry_stream` without dragging the `!Send` `AsyncApp` along.
        let persist_executor = cx.background_executor().clone();
        let future = request_limiter.stream(async move {
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

    fn default_model(&self, cx: &App) -> Option<LanguageModel> {
        self.available_models(cx)
            .first()
            .map(|model| self.language_model(model))
    }

    fn default_fast_model(&self, _cx: &App) -> Option<LanguageModel> {
        None
    }

    fn provided_models(&self, cx: &App) -> Vec<LanguageModel> {
        self.available_models(cx)
            .iter()
            .map(|model| self.language_model(model))
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
                cx.new(|cx| ConfigurationView::new(state.clone(), http_client.clone(), window, cx))
                    .into()
            },
        )))
    }

    fn set_api_key(&self, api_key: Option<String>, cx: &mut App) -> Task<Result<()>> {
        self.state
            .update(cx, |state, cx| state.set_api_key(api_key, cx))
    }
}

impl LanguageModelClient for OpenAiCompatibleLanguageModelProvider {
    fn stream_completion(
        &self,
        model: &LanguageModel,
        mut request: LanguageModelRequest,
        cx: &AsyncApp,
    ) -> BoxFuture<'static, Result<LanguageModelCompletionStream, LanguageModelCompletionError>>
    {
        let config = match cx.update(|cx| self.config(model, cx)) {
            Ok(config) => config,
            Err(error) => return async move { Err(error) }.boxed(),
        };
        let request_limiter = self.request_limiters.for_model(&model.id);
        // `speed` can leak in from a parent thread's model; this provider never
        // supports fast mode, and arbitrary compatible endpoints reject `service_tier`.
        request.speed = None;
        // Thread identity for per-thread key stickiness (issue 029), captured
        // before the conversions below consume the request.
        let thread_id = request.thread_id.clone();

        if config.capabilities.chat_completions {
            let reasoning_effort = chat_completion_reasoning_effort(&request, &config);
            let request = match into_open_ai(
                request,
                &config.name,
                config.capabilities.parallel_tool_calls,
                config.capabilities.prompt_cache_key,
                config.max_output_tokens,
                chat_completion_max_tokens_parameter(&config),
                reasoning_effort,
                config.capabilities.interleaved_reasoning,
            ) {
                Ok(request) => request,
                Err(error) => return async move { Err(error.into()) }.boxed(),
            };
            let completions = self.stream_chat_completion(&request_limiter, request, thread_id, cx);
            let executor = cx.background_executor().clone();
            async move {
                let mapper = ChatCompletionEventMapper::new();
                Ok(language_model::stream_in_background(
                    mapper.map_stream(completions.await?).boxed(),
                    executor,
                ))
            }
            .boxed()
        } else {
            disable_response_thinking_for_none_effort(&mut request, &config);
            let request = match into_open_ai_response(
                request,
                &config.name,
                config.capabilities.parallel_tool_calls,
                config.capabilities.prompt_cache_key,
                config.max_output_tokens,
                default_thinking_reasoning_effort(&config),
                supports_none_reasoning_effort(&config),
                &self.id,
            ) {
                Ok(request) => request,
                Err(error) => return async move { Err(error.into()) }.boxed(),
            };
            let completions = self.stream_response(&request_limiter, request, thread_id, cx);
            let compaction_state_owner = self.id.clone();
            let executor = cx.background_executor().clone();
            async move {
                let mapper = OpenAiResponseEventMapper::new(compaction_state_owner);
                Ok(language_model::stream_in_background(
                    mapper.map_stream(completions.await?).boxed(),
                    executor,
                ))
            }
            .boxed()
        }
    }

    fn last_used_key_label(&self, _model: &LanguageModel, cx: &App) -> Option<String> {
        self.state
            .read_with(cx, |state, _| state.last_used_key_label())
    }

    fn key_slot_status(
        &self,
        _model: &LanguageModel,
        cx: &App,
    ) -> Option<ModelKeySlotStatusSummary> {
        // Only meaningful for providers with at least one key configured. We
        // still return the summary (with `has_key: false` slots) when zero keys
        // are configured, so the footer can render the four chips as "empty".
        // Providers that aren't OpenAI-compatible keep the default `None`.
        let snapshot = self
            .state
            .read_with(cx, |state, _| state.slot_health_snapshot());
        Some(ModelKeySlotStatusSummary(snapshot.map(|s| {
            ModelKeySlotStatus {
                has_key: s.has_key,
                enabled: s.enabled,
                is_backed_off: s.is_backed_off,
                backoff_remaining: s.backoff_remaining,
                backoff_total: s.backoff_total,
                consecutive_failures: s.consecutive_failures,
                last_limit_hit_at: s.last_limit_hit.map(|record| record.hit_at),
                last_limit_window: s.last_limit_hit.map(|record| record.window),
            }
        })))
    }

    fn set_key_slot_enabled(
        &self,
        _model: &LanguageModel,
        slot_index: usize,
        enabled: bool,
        cx: &mut App,
    ) {
        let Some(slot) = slot_from_index(slot_index) else {
            return;
        };
        self.state.read_with(cx, |state, _| {
            state.set_slot_enabled(slot, enabled, cx);
        });
        // `set_slot_enabled` mutates `key_health` under a parking_lot mutex
        // (not via `Entity::update`), so `cx.observe(&state, ...)` wouldn't
        // fire. Notify explicitly so the footer re-renders immediately.
        self.state.update(cx, |_, cx| cx.notify());
    }

    fn reset_key_session(&self, _model: &LanguageModel, cx: &App) {
        // Issue 029: per-thread sticky picks make clearing selection state
        // unnecessary — a new thread has a new id and picks fresh via the
        // rotation cursor. The probes below remain: they re-verify every
        // configured key (including backed-off ones) so stale backoffs clear
        // before the new thread's first pick.
        let (probe_inputs, key_health, key_health_dirty, key_health_path) =
            self.state.read_with(cx, |state, _| {
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
                // Same semantics as the settings-page Check button:
                //
                // * `Ok` clears the slot outright, upstream-hinted windows
                //   included — see `KeyHealthTracker::record_success` for why a
                //   reachable endpoint outranks a reset hint parsed from a
                //   timestamp with no timezone marker.
                // * A rate limit is recorded either way. With a hint the
                //   backoff is pinned to exactly what the upstream reported;
                //   without one we still know the key is limited and only the
                //   duration is unknown, so the local schedule fills in. The
                //   old code ignored hintless 429s entirely, which left a
                //   provably limited key rendering as healthy.
                // * Any other error is genuinely ambiguous and changes nothing.
                match &result {
                    KeyProbeResult::Ok => record_key_success(&key_health, slot),
                    KeyProbeResult::RateLimit { retry_after } => {
                        let mut health = key_health.lock();
                        health.record_rate_limit(slot, Instant::now(), *retry_after);
                    }
                    KeyProbeResult::Err(_) => {}
                }
                log::info!("reset_key_session probe: slot={slot:?} result={result:?}");
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
            supports_disabling_thinking: None,
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

impl ConfigurationView {}

#[cfg(test)]
mod tests {
    mod fork_tests;
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
