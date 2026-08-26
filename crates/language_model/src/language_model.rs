mod api_key;
mod registry;
mod request;

#[cfg(any(test, feature = "test-support"))]
pub mod fake_provider;

pub use language_model_core::*;

use anyhow::Result;
use futures::FutureExt;
use futures::{StreamExt, future::BoxFuture, stream::BoxStream};
use gpui::{AnyView, App, AsyncApp, Task, Window};
use icons::IconName;
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::Duration;

pub type CreateProviderSettingsView = Arc<dyn Fn(&mut Window, &mut App) -> AnyView + 'static>;

pub use crate::api_key::{ApiKey, ApiKeyState};
pub use crate::registry::*;
pub use crate::request::{LanguageModelImageExt, gpui_size_to_image_size, image_size_to_gpui};
pub use env_var::{EnvVar, env_var};

pub fn init(cx: &mut App) {
    registry::init(cx);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisabledReason(pub SharedString);

impl DisabledReason {
    pub fn new(reason: impl Into<SharedString>) -> Self {
        Self(reason.into())
    }
}

/// The outcome of an explicit [`LanguageModel::compact`] request.
#[derive(Debug, Clone, PartialEq)]
pub struct CompactionResult {
    /// The replacement context to persist and use in subsequent requests.
    pub context: CompactedContext,
    /// Token usage of the compaction request itself, as reported by the
    /// provider.
    pub usage: TokenUsage,
}

pub struct LanguageModelTextStream {
    pub message_id: Option<String>,
    pub stream: BoxStream<'static, Result<String, LanguageModelCompletionError>>,
    // Has complete token usage after the stream has finished
    pub last_token_usage: Arc<Mutex<TokenUsage>>,
}

impl Default for LanguageModelTextStream {
    fn default() -> Self {
        Self {
            message_id: None,
            stream: Box::pin(futures::stream::empty()),
            last_token_usage: Arc::new(Mutex::new(TokenUsage::default())),
        }
    }
}

/// UI-facing snapshot of one API-key slot for providers that rotate across
/// multiple keys (OpenAI-compatible primary/secondary/tertiary/quaternary).
///
/// This is a provider-agnostic projection — it lives in the `language_model`
/// crate so any UI (e.g. the chat footer key-status chips) can consume it
/// without depending on `language_models` or knowing about the OpenAI-
/// compatible `KeyHealth` internals. `has_key` reflects whether a secret is
/// configured; `enabled` is the user-controlled toggle (a key can be present
/// but disabled, in which case it's excluded from rotation).
#[derive(Clone, Debug, PartialEq)]
pub struct ModelKeySlotStatus {
    pub has_key: bool,
    pub enabled: bool,
    pub is_backed_off: bool,
    pub backoff_remaining: Duration,
    pub consecutive_failures: u32,
}

impl Default for ModelKeySlotStatus {
    fn default() -> Self {
        Self {
            has_key: false,
            enabled: true,
            is_backed_off: false,
            backoff_remaining: Duration::ZERO,
            consecutive_failures: 0,
        }
    }
}

/// Summary of a multi-key provider's per-slot status, returned by
/// [`LanguageModel::key_slot_status`]. The four entries are always in the
/// fixed order `[Primary, Secondary, Tertiary, Quaternary]` (1-indexed as
/// K1/K2/K3/K4 in the UI). Providers that don't rotate keys return `None`
/// from `key_slot_status`, so callers render nothing.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct ModelKeySlotStatusSummary(pub [ModelKeySlotStatus; 4]);

pub trait LanguageModel: Send + Sync {
    fn id(&self) -> LanguageModelId;
    fn name(&self) -> LanguageModelName;
    fn provider_id(&self) -> LanguageModelProviderId;
    fn provider_name(&self) -> LanguageModelProviderName;
    fn upstream_provider_id(&self) -> LanguageModelProviderId {
        self.provider_id()
    }
    fn upstream_provider_name(&self) -> LanguageModelProviderName {
        self.provider_name()
    }

    /// Returns whether this model is the "latest", so we can highlight it in the UI.
    fn is_latest(&self) -> bool {
        false
    }

    /// Whether the model is currently disabled and, if so, why this is the case.
    fn is_disabled(&self) -> Option<DisabledReason> {
        None
    }

    /// Whether requests to this model require the user to consent to the
    /// upstream provider retaining inference logs (i.e. the model cannot be
    /// offered with Zero Data Retention).
    fn requires_data_retention(&self) -> bool {
        false
    }

    /// When this model refuses a request, the model ID to fall back to (same provider).
    fn refusal_fallback_model_id(&self) -> Option<&'static str> {
        None
    }

    fn telemetry_id(&self) -> String;

    fn api_key(&self, _cx: &App) -> Option<String> {
        None
    }

    /// Short label identifying which key the most recent (or in-flight) request
    /// used, for providers that rotate across multiple keys (e.g. OpenAI-
    /// compatible with primary/secondary/tertiary slots). Returns `None` for
    /// single-key providers (the default) — there's nothing to disambiguate.
    /// Surfaced on the retry button so the user can see which key the stuck
    /// turn picked (e.g. "K2" for the secondary slot).
    fn last_used_key_label(&self, _cx: &App) -> Option<String> {
        None
    }

    /// Per-slot status for multi-key providers, so the UI (e.g. the chat
    /// footer K1/K2/K3/K4 chips) can render health + enable/disable state at
    /// a glance. Returns `None` for providers that don't rotate keys (the
    /// default) — callers should then render nothing.
    ///
    /// The returned summary is a snapshot taken under the provider's health
    /// lock; callers that need live updates should poll on a timer (the
    /// ConfigurationView uses a 1s interval) since the underlying state is
    /// mutated from background request closures that bypass `cx.notify()`.
    fn key_slot_status(&self, _cx: &App) -> Option<ModelKeySlotStatusSummary> {
        None
    }

    /// Toggles the user-controlled `enabled` flag on one slot. `slot_index` is
    /// 0-based into the same fixed `[Primary, Secondary, Tertiary, Quaternary]`
    /// order used by [`Self::key_slot_status`]. Out-of-range indices are
    /// silently ignored so a stale UI can't panic the provider. Default no-op
    /// for providers that don't rotate keys.
    fn set_key_slot_enabled(&self, _slot_index: usize, _enabled: bool, _cx: &mut App) {}

    /// Starts a new key-selection session for multi-key providers: clears the
    /// session-sticky pick (so the next request re-randomizes among the healthy
    /// keys instead of inheriting the previous thread's key) and probes every
    /// configured key — including backed-off ones — in the background, clearing
    /// stale backoffs when the upstream reports the key healthy again.
    ///
    /// Called when a new agent thread starts, so prompt-cache affinity resets
    /// per thread and backoff state is re-verified against reality before the
    /// first turn picks a key. Default no-op for providers that don't rotate
    /// keys.
    fn reset_key_session(&self, _cx: &App) {}

    /// Information about the cost of using this model, if available.
    fn model_cost_info(&self) -> Option<LanguageModelCostInfo> {
        None
    }

    /// Whether this model supports thinking.
    fn supports_thinking(&self) -> bool {
        false
    }

    /// Whether thinking can be turned off entirely for this model. Some
    /// models (e.g. Claude Fable 5) always think and cannot honor an "off"
    /// request. Only meaningful when `supports_thinking` returns `true`.
    fn supports_disabling_thinking(&self) -> bool {
        true
    }

    fn supports_fast_mode(&self) -> bool {
        false
    }

    /// Returns the list of supported effort levels that can be used when thinking.
    fn supported_effort_levels(&self) -> Vec<LanguageModelEffortLevel> {
        Vec::new()
    }

    /// Returns the default effort level to use when thinking.
    fn default_effort_level(&self) -> Option<LanguageModelEffortLevel> {
        self.supported_effort_levels()
            .into_iter()
            .find(|effort_level| effort_level.is_default)
    }

    /// Whether this model supports provider-side automatic context
    /// compaction (requested via `LanguageModelRequest::compact_at_tokens`).
    fn supports_server_side_compaction(&self) -> bool {
        false
    }

    fn supports_explicit_compaction(&self) -> bool {
        false
    }

    /// The provider-enforced input size required for explicit compaction.
    fn minimum_explicit_compaction_input_tokens(&self) -> Option<u64> {
        None
    }

    fn compact(
        &self,
        _request: LanguageModelRequest,
        _cx: &AsyncApp,
    ) -> BoxFuture<'static, Result<CompactionResult, LanguageModelCompletionError>> {
        let provider = self.provider_name();
        async move {
            Err(LanguageModelCompletionError::Other(anyhow::anyhow!(
                "{provider} does not support explicit compaction"
            )))
        }
        .boxed()
    }

    /// Whether this model supports images
    fn supports_images(&self) -> bool;

    /// Whether this model supports tools.
    fn supports_tools(&self) -> bool;

    /// Whether this model supports choosing which tool to use.
    fn supports_tool_choice(&self, choice: LanguageModelToolChoice) -> bool;

    /// Returns whether this model or provider supports streaming tool calls;
    fn supports_streaming_tools(&self) -> bool {
        false
    }

    /// Returns whether this model/provider reports accurate split input/output token counts.
    /// When true, the UI may show separate input/output token indicators.
    fn supports_split_token_display(&self) -> bool {
        false
    }

    fn tool_input_format(&self) -> LanguageModelToolSchemaFormat {
        LanguageModelToolSchemaFormat::JsonSchema
    }

    fn max_token_count(&self) -> u64;
    fn max_output_tokens(&self) -> Option<u64> {
        None
    }

    fn stream_completion(
        &self,
        request: LanguageModelRequest,
        cx: &AsyncApp,
    ) -> BoxFuture<
        'static,
        Result<
            BoxStream<'static, Result<LanguageModelCompletionEvent, LanguageModelCompletionError>>,
            LanguageModelCompletionError,
        >,
    >;

    fn stream_completion_text(
        &self,
        request: LanguageModelRequest,
        cx: &AsyncApp,
    ) -> BoxFuture<'static, Result<LanguageModelTextStream, LanguageModelCompletionError>> {
        let future = self.stream_completion(request, cx);

        async move {
            let events = future.await?;
            let mut events = events.fuse();
            let mut message_id = None;
            let mut first_item_text = None;
            let last_token_usage = Arc::new(Mutex::new(TokenUsage::default()));

            if let Some(first_event) = events.next().await {
                match first_event {
                    Ok(LanguageModelCompletionEvent::StartMessage { message_id: id }) => {
                        message_id = Some(id);
                    }
                    Ok(LanguageModelCompletionEvent::Text(text)) => {
                        first_item_text = Some(text);
                    }
                    _ => (),
                }
            }

            let stream = futures::stream::iter(first_item_text.map(Ok))
                .chain(events.filter_map({
                    let last_token_usage = last_token_usage.clone();
                    move |result| {
                        let last_token_usage = last_token_usage.clone();
                        async move {
                            match result {
                                Ok(LanguageModelCompletionEvent::Queued { .. }) => None,
                                Ok(LanguageModelCompletionEvent::Started) => None,
                                Ok(LanguageModelCompletionEvent::StartMessage { .. }) => None,
                                Ok(LanguageModelCompletionEvent::Text(text)) => Some(Ok(text)),
                                Ok(LanguageModelCompletionEvent::Thinking { .. }) => None,
                                Ok(LanguageModelCompletionEvent::RedactedThinking { .. }) => None,
                                Ok(LanguageModelCompletionEvent::ReasoningDetails(_)) => None,
                                Ok(LanguageModelCompletionEvent::Stop(_)) => None,
                                Ok(LanguageModelCompletionEvent::ToolUse(_)) => None,
                                Ok(LanguageModelCompletionEvent::ToolUseJsonParseError {
                                    ..
                                }) => None,
                                Ok(LanguageModelCompletionEvent::Compaction(_)) => None,
                                Ok(LanguageModelCompletionEvent::UsageUpdate(token_usage)) => {
                                    *last_token_usage.lock() = token_usage;
                                    None
                                }
                                Err(err) => Some(Err(err)),
                            }
                        }
                    }
                }))
                .boxed();

            Ok(LanguageModelTextStream {
                message_id,
                stream,
                last_token_usage,
            })
        }
        .boxed()
    }

    fn stream_completion_tool(
        &self,
        request: LanguageModelRequest,
        cx: &AsyncApp,
    ) -> BoxFuture<'static, Result<LanguageModelToolUse, LanguageModelCompletionError>> {
        let future = self.stream_completion(request, cx);

        async move {
            let events = future.await?;
            let mut events = events.fuse();

            // Iterate through events until we find a complete ToolUse
            while let Some(event) = events.next().await {
                match event {
                    Ok(LanguageModelCompletionEvent::ToolUse(tool_use))
                        if tool_use.is_input_complete =>
                    {
                        return Ok(tool_use);
                    }
                    Err(err) => {
                        return Err(err);
                    }
                    _ => {}
                }
            }

            // Stream ended without a complete tool use
            Err(LanguageModelCompletionError::Other(anyhow::anyhow!(
                "Stream ended without receiving a complete tool use"
            )))
        }
        .boxed()
    }

    #[cfg(any(test, feature = "test-support"))]
    fn as_fake(&self) -> &fake_provider::FakeLanguageModel {
        unimplemented!()
    }
}

impl std::fmt::Debug for dyn LanguageModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("<dyn LanguageModel>")
            .field("id", &self.id())
            .field("name", &self.name())
            .field("provider_id", &self.provider_id())
            .field("provider_name", &self.provider_name())
            .field("upstream_provider_name", &self.upstream_provider_name())
            .field("upstream_provider_id", &self.upstream_provider_id())
            .field("upstream_provider_id", &self.upstream_provider_id())
            .field("supports_streaming_tools", &self.supports_streaming_tools())
            .finish()
    }
}

/// Either a built-in icon name or a path to an external SVG.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IconOrSvg {
    /// A built-in icon from Zed's icon set.
    Icon(IconName),
    /// Path to a custom SVG icon file.
    Svg(SharedString),
}

impl Default for IconOrSvg {
    fn default() -> Self {
        Self::Icon(IconName::ZedAssistant)
    }
}

pub trait LanguageModelProvider: 'static {
    fn id(&self) -> LanguageModelProviderId;
    fn name(&self) -> LanguageModelProviderName;
    fn icon(&self) -> IconOrSvg {
        IconOrSvg::default()
    }
    fn default_model(&self, cx: &App) -> Option<Arc<dyn LanguageModel>>;
    fn default_fast_model(&self, cx: &App) -> Option<Arc<dyn LanguageModel>>;
    fn provided_models(&self, cx: &App) -> Vec<Arc<dyn LanguageModel>>;
    fn recommended_models(&self, _cx: &App) -> Vec<Arc<dyn LanguageModel>> {
        Vec::new()
    }
    fn is_authenticated(&self, cx: &App) -> bool;
    fn authenticate(&self, cx: &mut App) -> Task<Result<(), AuthenticateError>>;
    fn settings_view(&self, cx: &mut App) -> Option<ProviderSettingsView>;

    fn set_api_key(&self, _key: Option<String>, _cx: &mut App) -> Task<Result<()>> {
        Task::ready(Ok(()))
    }

    /// Copy shown when this provider rejects a request as unauthenticated
    /// (HTTP 401). The default assumes API-key authentication; providers using
    /// other mechanisms (account or subscription based auth) should override
    /// this so users aren't told to check an API key they don't have.
    fn authentication_error_message(&self) -> SharedString {
        format!(
            "The API key for {} is invalid or has expired. \
            Update your key in Settings > AI > LLM Providers to continue.",
            self.name().0
        )
        .into()
    }

    /// Copy shown when a request fails because no credentials are configured
    /// for this provider. The default assumes API-key authentication;
    /// providers using other mechanisms (account or subscription based auth)
    /// should override this.
    fn missing_credentials_error_message(&self) -> SharedString {
        format!(
            "No API key is configured for {}. \
            Add your key in Settings > AI > LLM Providers to continue.",
            self.name().0
        )
        .into()
    }

    /// Copy shown the first time a user enables fast mode for a model from
    /// this provider. Returning `None` skips the confirmation prompt and lets
    /// the toggle apply silently.
    fn fast_mode_confirmation(&self, _cx: &App) -> Option<FastModeConfirmation> {
        None
    }
}

/// A provider's settings UI, modeled as mutually exclusive presentation modes.
#[derive(Clone)]
pub enum ProviderSettingsView {
    ApiKey(ApiKeyConfiguration),
    Inline(InlineProviderSettings),
    SubPage(SubPageProviderSettings),
}

#[derive(Clone)]
pub struct InlineProviderSettings {
    pub title: Option<SharedString>,
    pub description: Option<InlineDescription>,
    pub create_view: CreateProviderSettingsView,
}

#[derive(Clone)]
pub struct SubPageProviderSettings {
    pub description: Option<InlineDescription>,
    pub create_view: CreateProviderSettingsView,
}

impl SubPageProviderSettings {
    pub fn new(create_view: impl Fn(&mut Window, &mut App) -> AnyView + 'static) -> Self {
        Self {
            description: None,
            create_view: Arc::new(create_view),
        }
    }

    pub fn description(mut self, description: InlineDescription) -> Self {
        self.description = Some(description);
        self
    }
}

impl ApiKeyConfiguration {
    pub fn new(
        has_key: bool,
        is_from_env_var: bool,
        env_var_name: SharedString,
        api_key_url: SharedString,
    ) -> Self {
        Self {
            has_key,
            is_from_env_var,
            env_var_name,
            api_key_url,
        }
    }
}

/// A live snapshot of a single-API-key provider's credential state, used by the
/// settings UI to render the provider's "API Key" section.
#[derive(Clone)]
pub struct ApiKeyConfiguration {
    pub has_key: bool,
    pub is_from_env_var: bool,
    pub env_var_name: SharedString,
    pub api_key_url: SharedString,
}

/// The subtitle rendered beneath a provider's name when its configuration is
/// shown inline.
#[derive(Clone)]
pub enum InlineDescription {
    /// A clickable "Where to find key" link pointing at the given URL, for
    /// API-key based providers.
    ApiKeyUrl(SharedString),
    /// Plain descriptive text, e.g. explaining a sign-in based provider.
    Text(SharedString),
}

/// Provider-specific copy shown the first time a user enables fast mode.
#[derive(Debug, Clone)]
pub struct FastModeConfirmation {
    pub title: SharedString,
    pub message: SharedString,
}

pub trait LanguageModelProviderState: 'static {
    type ObservableEntity;

    fn observable_entity(&self) -> Option<gpui::Entity<Self::ObservableEntity>>;

    fn subscribe<T: 'static>(
        &self,
        cx: &mut gpui::Context<T>,
        callback: impl Fn(&mut T, &mut gpui::Context<T>) + 'static,
    ) -> Option<gpui::Subscription> {
        let entity = self.observable_entity()?;
        Some(cx.observe(&entity, move |this, _, cx| {
            callback(this, cx);
        }))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum LanguageModelCostInfo {
    /// Cost per 1,000 input and output tokens
    TokenCost {
        input_token_cost_per_1m: f64,
        output_token_cost_per_1m: f64,
    },
    /// Cost per request
    RequestCost { cost_per_request: f64 },
}

impl LanguageModelCostInfo {
    pub fn to_shared_string(&self) -> SharedString {
        match self {
            LanguageModelCostInfo::RequestCost { cost_per_request } => {
                let cost_str = format!("{}×", Self::cost_value_to_string(cost_per_request));
                SharedString::from(cost_str)
            }
            LanguageModelCostInfo::TokenCost {
                input_token_cost_per_1m,
                output_token_cost_per_1m,
            } => {
                let input_cost = Self::cost_value_to_string(input_token_cost_per_1m);
                let output_cost = Self::cost_value_to_string(output_token_cost_per_1m);
                SharedString::from(format!("{}$/{}$", input_cost, output_cost))
            }
        }
    }

    fn cost_value_to_string(cost: &f64) -> SharedString {
        if (cost.fract() - 0.0).abs() < std::f64::EPSILON {
            SharedString::from(format!("{:.0}", cost))
        } else {
            SharedString::from(format!("{:.2}", cost))
        }
    }
}
