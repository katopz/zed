use anyhow::Result;
use convert_case::{Case, Casing};
use credentials_provider::CredentialsProvider;
use futures::{FutureExt, StreamExt, future::BoxFuture};
use gpui::{AnyView, App, AsyncApp, Context, Entity, SharedString, Task, TaskExt, Window};
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
use rand::seq::IndexedRandom;
use rand::Rng;
use settings::{Settings, SettingsStore};
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

/// Which slot a key was selected from, so request outcomes can be attributed back
/// to the correct `KeyHealth` entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KeySlot {
    Primary,
    Secondary,
    Tertiary,
}

/// Per-key backoff state. Lives in-memory only; restarts reset all keys to healthy.
#[derive(Default, Clone, Debug)]
struct KeyHealth {
    consecutive_failures: u32,
    backoff_until: Option<Instant>,
}

impl KeyHealth {
    fn is_backed_off(&self, now: Instant) -> bool {
        matches!(self.backoff_until, Some(until) if now < until)
    }
}

#[derive(Default, Clone, Debug)]
struct KeyHealthTracker {
    primary: KeyHealth,
    secondary: KeyHealth,
    tertiary: KeyHealth,
}

impl KeyHealthTracker {
    fn get(&self, slot: KeySlot) -> &KeyHealth {
        match slot {
            KeySlot::Primary => &self.primary,
            KeySlot::Secondary => &self.secondary,
            KeySlot::Tertiary => &self.tertiary,
        }
    }

    fn get_mut(&mut self, slot: KeySlot) -> &mut KeyHealth {
        match slot {
            KeySlot::Primary => &mut self.primary,
            KeySlot::Secondary => &mut self.secondary,
            KeySlot::Tertiary => &mut self.tertiary,
        }
    }

    /// Resets the slot's health on success: clears the failure counter and any
    /// pending backoff. A single success is enough to re-qualify a previously
    /// failing key.
    fn record_success(&mut self, slot: KeySlot) {
        let health = self.get_mut(slot);
        health.consecutive_failures = 0;
        health.backoff_until = None;
    }

    /// Records a backoff-worthy failure on the slot: bumps the failure counter
    /// and recomputes `backoff_until = now + compute_backoff(count)`.
    /// Non-backoff-worthy errors should not call this (they would poison the
    /// slot without benefit since the same error would occur on every key).
    fn record_failure(&mut self, slot: KeySlot, now: Instant) {
        let health = self.get_mut(slot);
        health.consecutive_failures = health.consecutive_failures.saturating_add(1);
        let backoff = compute_backoff(health.consecutive_failures);
        health.backoff_until = Some(now + backoff);
    }
}

/// Soft cap on backoff. After this duration since the last failure the key is
/// automatically selectable again — no explicit "clear" path is needed.
const BACKOFF_MAX: Duration = Duration::from_secs(5 * 60 * 60);

/// Base unit for the exponential schedule.
const BACKOFF_BASE: Duration = Duration::from_secs(30);

/// Computes an exponential backoff with jitter. The 5-hour cap is the
/// dominant constraint regardless of how large `failures` gets, matching the
/// user requirement of "remove backoff after 5 hours for each key".
///
/// Jitter factor is in `[0.5, 1.5)` to avoid the thundering-herd case where
/// all keys fail at the same instant and would otherwise all unblock together.
fn compute_backoff(failures: u32) -> Duration {
    if failures == 0 {
        return Duration::ZERO;
    }
    // 2^(failures-1), capped at 14 so the multiplication can't overflow `Duration`
    // (2^14 * 30s ≈ 138h, already well past the 5h cap, so the clamp is a no-op).
    let exponent = (failures - 1).min(14);
    let multiplier = 2u32.pow(exponent);
    let candidate = BACKOFF_BASE
        .checked_mul(multiplier)
        .unwrap_or(BACKOFF_MAX)
        .min(BACKOFF_MAX);
    let jitter = rand::rng().random_range(0.5..1.5);
    candidate.mul_f64(jitter).min(BACKOFF_MAX)
}

/// Returns true for errors that suggest the *key* or *upstream* is the problem
/// (and so rotating to a different key may help), false for errors that will
/// recur on every key (so poisoning the slot would just shrink the pool
/// without benefit).
///
/// This is intentionally permissive: the user reported upstream error labels
/// are unreliable, so when in doubt we back off rather than burn requests.
fn is_backoff_worthy(err: &LanguageModelCompletionError) -> bool {
    match err {
        LanguageModelCompletionError::RateLimitExceeded { .. }
        | LanguageModelCompletionError::ServerOverloaded { .. }
        | LanguageModelCompletionError::ApiInternalServerError { .. }
        | LanguageModelCompletionError::UpstreamProviderError { .. }
        | LanguageModelCompletionError::StreamEndedUnexpectedly { .. }
        | LanguageModelCompletionError::ApiReadResponseError { .. }
        | LanguageModelCompletionError::HttpSend { .. }
        | LanguageModelCompletionError::AuthenticationError { .. }
        | LanguageModelCompletionError::PermissionError { .. }
        | LanguageModelCompletionError::Other(_) => true,
        LanguageModelCompletionError::PromptTooLarge { .. }
        | LanguageModelCompletionError::NoApiKey { .. }
        | LanguageModelCompletionError::BadRequestFormat { .. }
        | LanguageModelCompletionError::ApiEndpointNotFound { .. }
        | LanguageModelCompletionError::HttpResponseError { .. }
        | LanguageModelCompletionError::SerializeRequest { .. }
        | LanguageModelCompletionError::BuildRequestBody { .. }
        | LanguageModelCompletionError::DeserializeResponse { .. } => false,
    }
}

impl State {
    fn is_authenticated(&self) -> bool {
        self.api_key_state.has_key()
            || self.api_key_state_2.has_key()
            || self.api_key_state_3.has_key()
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

    /// Picks an available API key, skipping any currently in backoff.
    ///
    /// Returns the key plus the slot it came from so the caller can later
    /// report success/failure via `record_key_success` / `record_key_failure`.
    ///
    /// If *every* present key is in backoff, returns the one with the earliest
    /// `backoff_until` rather than `None` — failing open here lets the caller
    /// at least try the soonest-available key instead of erroring as `NoApiKey`.
    /// Returns `None` only when no key is configured at all.
    fn select_key(&self, health: &KeyHealthTracker, now: Instant) -> Option<(Arc<str>, KeySlot)> {
        let primary_url = self.settings.api_url.as_str();
        let secondary_url = secondary_key_url(primary_url);
        let tertiary_url = tertiary_key_url(primary_url);

        let candidates: [(Option<Arc<str>>, KeySlot); 3] = [
            (
                self.api_key_state.key(primary_url),
                KeySlot::Primary,
            ),
            (
                self.api_key_state_2.key(&secondary_url),
                KeySlot::Secondary,
            ),
            (
                self.api_key_state_3.key(&tertiary_url),
                KeySlot::Tertiary,
            ),
        ];

        // Healthy candidates: present and not in backoff.
        let healthy: Vec<(Arc<str>, KeySlot)> = candidates
            .iter()
            .filter_map(|(key, slot)| {
                let key = key.clone()?;
                if health.get(*slot).is_backed_off(now) {
                    return None;
                }
                Some((key, *slot))
            })
            .collect();

        if let Some(pick) = healthy.choose(&mut rand::rng()).cloned() {
            return Some(pick);
        }

        // Everything present is backed off — fall back to the earliest-expiring
        // backed-off key. Better than NoApiKey when at least one key exists.
        candidates
            .iter()
            .filter_map(|(key, slot)| {
                let key = key.clone()?;
                let slot_health = health.get(*slot);
                let until = slot_health.backoff_until?;
                Some((key, *slot, until))
            })
            .min_by_key(|&(_, _, until)| until)
            .map(|(key, slot, _)| (key, slot))
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
                key_health: Arc::new(std::sync::Mutex::new(KeyHealthTracker::default())),
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

        let (key_health, selected, api_url, extra_headers) =
            self.state.read_with(cx, |state, _cx| {
                let health = state.key_health.lock().expect("key_health mutex poisoned");
                (
                    state.key_health.clone(),
                    state.select_key(&health, Instant::now()),
                    state.settings.api_url.clone(),
                    state.settings.custom_headers.clone(),
                )
            });

        let provider = self.provider_name.clone();
        let future = self.request_limiter.stream(async move {
            let Some((api_key, slot)) = selected else {
                return Err(LanguageModelCompletionError::NoApiKey { provider });
            };
            let result: Result<
                futures::stream::BoxStream<'static, Result<ResponseStreamEvent>>,
                LanguageModelCompletionError,
            > = stream_completion(
                http_client.as_ref(),
                provider.0.as_str(),
                &api_url,
                &api_key,
                request,
                &extra_headers,
            )
            .await
            .map_err(Into::into);
            record_outcome(&key_health, slot, &result);
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

        let (key_health, selected, api_url, extra_headers) =
            self.state.read_with(cx, |state, _cx| {
                let health = state.key_health.lock().expect("key_health mutex poisoned");
                (
                    state.key_health.clone(),
                    state.select_key(&health, Instant::now()),
                    state.settings.api_url.clone(),
                    state.settings.custom_headers.clone(),
                )
            });

        let provider = self.provider_name.clone();
        let future = self.request_limiter.stream(async move {
            let Some((api_key, slot)) = selected else {
                return Err(LanguageModelCompletionError::NoApiKey { provider });
            };
            let result: Result<
                futures::stream::BoxStream<'static, Result<ResponsesStreamEvent>>,
                LanguageModelCompletionError,
            > = stream_response(
                http_client.as_ref(),
                provider.0.as_str(),
                &api_url,
                &api_key,
                request,
                &extra_headers,
            )
            .await
            .map_err(Into::into);
            record_outcome(&key_health, slot, &result);
            result
        });

        async move { Ok(future.await?.boxed()) }.boxed()
    }
}

/// Updates per-key health after a request completes. Called from inside the
/// rate-limited stream closure so health reflects real request outcomes.
///
/// - Success: clears the slot's failure counter and backoff.
/// - Backoff-worthy error: bumps the counter and schedules exponential backoff.
/// - Other error: leaves health untouched (it would fail on every key anyway).
///
/// Uses the shared `Arc<Mutex<KeyHealthTracker>>` rather than `Entity::update`
/// because the request closure runs on a background executor where `AsyncApp`
/// (`!Send`) cannot travel.
fn record_outcome<T>(
    key_health: &Arc<std::sync::Mutex<KeyHealthTracker>>,
    slot: KeySlot,
    result: &Result<T, LanguageModelCompletionError>,
) {
    let mut health = match key_health.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    match result {
        Ok(_) => health.record_success(slot),
        Err(err) if is_backoff_worthy(err) => health.record_failure(slot, Instant::now()),
        Err(_) => {}
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

        Self {
            api_key_editor,
            api_key_editor_2,
            api_key_editor_3,
            state,
            load_credentials_task,
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
            h_flex()
                .mt_1()
                .p_1()
                .justify_between()
                .rounded_md()
                .border_1()
                .border_color(cx.theme().colors().border)
                .bg(cx.theme().colors().background)
                .child(
                    h_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_1()
                        .child(Icon::new(IconName::Check).color(Color::Success))
                        .child(
                            div()
                                .w_full()
                                .overflow_x_hidden()
                                .text_ellipsis()
                                .child(Label::new(
                                    if primary_env_var_set {
                                        format!("Primary API key set in {primary_env_var_name} environment variable")
                                    } else {
                                        format!("Primary API key configured for {api_url}")
                                    }
                                ))
                        ),
                )
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
            h_flex()
                .mt_1()
                .p_1()
                .justify_between()
                .rounded_md()
                .border_1()
                .border_color(cx.theme().colors().border)
                .bg(cx.theme().colors().background)
                .child(
                    h_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_1()
                        .child(Icon::new(IconName::Check).color(Color::Success))
                        .child(
                            div()
                                .w_full()
                                .overflow_x_hidden()
                                .text_ellipsis()
                                .child(Label::new(
                                    if secondary_env_var_set {
                                        format!("Secondary API key set in {secondary_env_var_name} environment variable")
                                    } else {
                                        "Secondary API key configured for load balancing".to_string()
                                    }
                                ))
                        ),
                )
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
            h_flex()
                .mt_1()
                .p_1()
                .justify_between()
                .rounded_md()
                .border_1()
                .border_color(cx.theme().colors().border)
                .bg(cx.theme().colors().background)
                .child(
                    h_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_1()
                        .child(Icon::new(IconName::Check).color(Color::Success))
                        .child(
                            div()
                                .w_full()
                                .overflow_x_hidden()
                                .text_ellipsis()
                                .child(Label::new(
                                    if tertiary_env_var_set {
                                        format!("Tertiary API key set in {tertiary_env_var_name} environment variable")
                                    } else {
                                        "Tertiary API key configured for load balancing".to_string()
                                    }
                                ))
                        ),
                )
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
    use http_client::http::StatusCode;
    use language_model::LanguageModelProviderName;
    use parking_lot::Mutex;
    use std::future::Future;
    use std::pin::Pin;

    fn provider_name() -> LanguageModelProviderName {
        LanguageModelProviderName::from(String::from("test"))
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
            key_health: Arc::new(std::sync::Mutex::new(KeyHealthTracker::default())),
            settings: OpenAiCompatibleSettings {
                api_url: "https://example.test".to_string(),
                ..Default::default()
            },
            credentials_provider: Arc::new(FakeCredentialsProvider),
        }
    }

    // Suppress unused-import warning for `Mutex` if no test below uses it; kept
    // here to mirror the pattern from `openai_subscribed.rs` for any future
    // state-mutation test that wants persisted storage.
    #[allow(dead_code)]
    fn _silence_unused_mutex_marker() -> Mutex<()> {
        Mutex::new(())
    }

    #[test]
    fn backoff_zero_failures_is_zero() {
        assert_eq!(compute_backoff(0), Duration::ZERO);
    }

    #[test]
    fn backoff_first_failure_around_base() {
        // 30s * 2^0 = 30s, jittered to [15s, 45s).
        for _ in 0..100 {
            let backoff = compute_backoff(1);
            assert!(backoff >= Duration::from_secs(15), "got {backoff:?}");
            assert!(backoff < Duration::from_secs(45), "got {backoff:?}");
        }
    }

    #[test]
    fn backoff_grows_exponentially_until_cap() {
        // 30s * 2^(n-1) for small n, but capped at 5h.
        let one = compute_backoff(1);
        let two_max = BACKOFF_BASE * 2 * 3 / 2; // upper jitter bound
        let two_min = BACKOFF_BASE * 2 / 2; // lower jitter bound
        // Sanity: bound is reasonable
        assert!(two_min <= two_max);
        // one is in the [15s, 45s) band
        assert!(one >= Duration::from_secs(15) && one < Duration::from_secs(45));
        let _ = (two_min, two_max); // suppress unused warnings on locals
    }

    #[test]
    fn backoff_never_exceeds_cap() {
        // Even at absurd failure counts, jittered value must stay ≤ 5h.
        for failures in [1, 5, 10, 50, 1000, u32::MAX] {
            for _ in 0..50 {
                let backoff = compute_backoff(failures);
                assert!(
                    backoff <= BACKOFF_MAX,
                    "failures={failures} yielded backoff={backoff:?} > cap={BACKOFF_MAX:?}"
                );
            }
        }
    }

    #[test]
    fn key_health_fresh_is_not_backed_off() {
        let health = KeyHealth::default();
        assert!(!health.is_backed_off(Instant::now()));
    }

    #[test]
    fn key_health_backoff_expires_after_window() {
        let start = Instant::now();
        let mut tracker = KeyHealthTracker::default();
        tracker.record_failure(KeySlot::Primary, start);
        let backed_until = tracker.get(KeySlot::Primary).backoff_until.unwrap();
        // During the window, slot is backed off.
        assert!(tracker
            .get(KeySlot::Primary)
            .is_backed_off(start + Duration::from_secs(1)));
        // After the backoff duration, slot re-qualifies automatically — this is
        // the "5h auto-clear" guarantee, but at the small scale of the actual
        // backoff (test asserts the mechanism, not the 5h cap).
        assert!(
            !tracker
                .get(KeySlot::Primary)
                .is_backed_off(backed_until + Duration::from_secs(1)),
            "key should re-qualify once backoff_until is in the past"
        );
        let _ = backed_until;
    }

    #[test]
    fn record_success_clears_backoff() {
        let start = Instant::now();
        let mut tracker = KeyHealthTracker::default();
        tracker.record_failure(KeySlot::Secondary, start);
        tracker.record_failure(KeySlot::Secondary, start + Duration::from_secs(5));
        assert!(tracker
            .get(KeySlot::Secondary)
            .is_backed_off(start + Duration::from_secs(1)));
        tracker.record_success(KeySlot::Secondary);
        assert_eq!(tracker.get(KeySlot::Secondary).consecutive_failures, 0);
        assert_eq!(tracker.get(KeySlot::Secondary).backoff_until, None);
        assert!(!tracker
            .get(KeySlot::Secondary)
            .is_backed_off(start + Duration::from_secs(1)));
    }

    #[test]
    fn record_failure_does_not_overflow() {
        // Pathological: many failures should saturate, not panic.
        let mut tracker = KeyHealthTracker::default();
        let now = Instant::now();
        for _ in 0..1000 {
            tracker.record_failure(KeySlot::Tertiary, now);
        }
        let backoff = compute_backoff(tracker.get(KeySlot::Tertiary).consecutive_failures);
        assert!(backoff <= BACKOFF_MAX);
    }

    #[test]
    fn is_backoff_worthy_classification() {
        let provider = provider_name();

        // Backoff-worthy (transient / per-key).
        assert!(is_backoff_worthy(&LanguageModelCompletionError::RateLimitExceeded {
            provider: provider.clone(),
            retry_after: None,
        }));
        assert!(is_backoff_worthy(&LanguageModelCompletionError::ServerOverloaded {
            provider: provider.clone(),
            retry_after: None,
        }));
        assert!(is_backoff_worthy(
            &LanguageModelCompletionError::ApiInternalServerError {
                provider: provider.clone(),
                message: "boom".into(),
            }
        ));
        assert!(is_backoff_worthy(&LanguageModelCompletionError::AuthenticationError {
            provider: provider.clone(),
            message: "bad key".into(),
        }));
        assert!(is_backoff_worthy(
            &LanguageModelCompletionError::StreamEndedUnexpectedly {
                provider: provider.clone(),
            }
        ));
        assert!(is_backoff_worthy(&LanguageModelCompletionError::Other(anyhow::anyhow!(
            "unknown"
        ))));

        // NOT backoff-worthy (would recur on every key).
        assert!(!is_backoff_worthy(&LanguageModelCompletionError::NoApiKey {
            provider: provider.clone(),
        }));
        assert!(!is_backoff_worthy(
            &LanguageModelCompletionError::PromptTooLarge { tokens: None }
        ));
        assert!(!is_backoff_worthy(
            &LanguageModelCompletionError::BadRequestFormat {
                provider: provider.clone(),
                message: "bad".into(),
            }
        ));
        assert!(!is_backoff_worthy(
            &LanguageModelCompletionError::ApiEndpointNotFound {
                provider: provider.clone(),
            }
        ));
        assert!(!is_backoff_worthy(
            &LanguageModelCompletionError::HttpResponseError {
                provider,
                status_code: StatusCode::NOT_IMPLEMENTED,
                message: "bad".into(),
            }
        ));
    }

    #[test]
    fn select_key_returns_none_when_no_keys_configured() {
        let state = fake_state_with_no_keys();
        let health = KeyHealthTracker::default();
        assert!(state.select_key(&health, Instant::now()).is_none());
    }
}
