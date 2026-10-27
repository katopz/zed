//! Fork-owned `Thread` policy (plan 038): stream idle timeouts, retry caps,
//! metadata-call retries and image stripping, kept out of `thread.rs` so
//! upstream syncs don't conflict on them.

use super::*;
use crate::{
    ListThreadsTool, RequestVerdictTool, SendMessageTool, ThreadListing, ThreadMessageOutcome,
    ThreadMessageRequest,
};
use futures::stream::BoxStream;
use util::path_list::PathList;

/// Fork-only [`ThreadEnvironment`] capabilities (verdict reviewers, peer-thread
/// mailbox), split into a supertrait so the upstream trait body stays
/// untouched. Every method has a rejecting default; environments opt in by
/// overriding.
pub trait ForkThreadEnvironment {
    /// The project this thread works in, if the environment can provide it.
    /// Used by the `request_verdict` tool to spawn external reviewer sessions
    /// in the worker's project. Default: `None`.
    fn project(&self, _cx: &App) -> Option<Entity<Project>> {
        None
    }

    /// This thread's work directories, if the environment can provide them.
    /// Passed to external reviewer sessions. Default: `None`.
    fn work_dirs(&self, _cx: &App) -> Option<PathList> {
        None
    }

    /// Creates a subagent for the `request_verdict` ping-pong (proposal 001),
    /// pinned to `model_override` when given, else the configured
    /// `verdict_model`. Environments that don't support verdict negotiations
    /// reject the call.
    fn create_verdict_subagent(
        &self,
        _label: String,
        _model_override: Option<LanguageModelSelection>,
        _cx: &mut App,
    ) -> Result<Rc<dyn SubagentHandle>> {
        Err(anyhow::anyhow!(
            "Verdict subagents are not supported in this environment"
        ))
    }

    /// Lists live threads in this project that `send_message_to_thread` can reach.
    fn list_threads(&self, _cx: &mut App) -> Result<Vec<ThreadListing>> {
        Err(anyhow::anyhow!(
            "Listing threads is not supported in this environment"
        ))
    }

    /// Delivers a message to another live thread: immediately when it is idle,
    /// otherwise once its current turn ends.
    fn send_message_to_thread(
        &self,
        _request: ThreadMessageRequest,
        _cx: &mut AsyncApp,
    ) -> Task<Result<ThreadMessageOutcome>> {
        Task::ready(Err(anyhow::anyhow!(
            "Messaging other threads is not supported in this environment"
        )))
    }
}

/// Delay between retries of a stream idle timeout. Longer than
/// `BASE_RETRY_DELAY` because the provider hang that caused the silent
/// stream typically outlasts a few seconds.
pub(crate) const STREAM_IDLE_RETRY_DELAY: Duration = Duration::from_secs(30);
/// Upper bound on a provider-supplied `retry_after` hint. Some providers
/// send multi-hour values on transient 429s; honored raw, they pin the turn
/// in a silent backoff with a stale retry callout for hours.
pub(crate) const MAX_RETRY_AFTER: Duration = Duration::from_secs(2 * 60);

/// A stream that goes silent past the idle timeout is a transient provider
/// hang. The manual retry button reliably recovers these once the provider
/// recovers, so keep the turn alive with a longer automatic budget (4 retries
/// × 30s, on top of each attempt's own idle-timeout wait) instead of surfacing
/// the error and waiting for the user to click retry.
pub(super) const STREAM_IDLE_RETRY_STRATEGY: RetryStrategy = RetryStrategy::FixedDelay {
    delay: STREAM_IDLE_RETRY_DELAY,
    max_attempts: MAX_RETRY_ATTEMPTS,
};

pub(crate) fn retry_after_delay(retry_after: Duration) -> Duration {
    retry_after.min(MAX_RETRY_AFTER)
}

/// Payload of the `LanguageModelCompletionError::Other` raised when a stream
/// goes silent past `agent.stream_idle_timeout_secs`. Typed so the retry
/// strategy can grant it a longer budget than other `Other(..)` errors
/// without string matching.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub(crate) struct StreamIdleTimeout {
    message: String,
}

impl StreamIdleTimeout {
    pub(crate) fn error(message: impl Into<String>) -> LanguageModelCompletionError {
        LanguageModelCompletionError::Other(anyhow::Error::new(Self {
            message: message.into(),
        }))
    }

    pub(super) fn is(error: &anyhow::Error) -> bool {
        error.downcast_ref::<Self>().is_some()
    }

    /// Outcome of the turn loop's idle-timer arm. A silent stream is
    /// legitimate while tool results are still pending (the tool branch of the
    /// select fires when they complete), so this returns `None` (keep waiting)
    /// then, and only treats the stream as dead when there is nothing left to
    /// wait for.
    pub(super) fn turn_error(
        has_pending_tool_results: bool,
        stream_idle_timeout_secs: u64,
        attempt: u8,
    ) -> Option<LanguageModelCompletionError> {
        if has_pending_tool_results {
            return None;
        }
        log::warn!(
            "No completion events for {}s with no pending tool results; \
             treating the stream as dead and retrying (attempt {attempt})",
            stream_idle_timeout_secs
        );
        Some(Self::error(format!(
            "stream idle timeout: no events in {stream_idle_timeout_secs}s"
        )))
    }

    pub(super) fn compaction_no_reply(stream_idle_timeout_secs: u64) -> anyhow::Error {
        log::warn!(
            "Compaction stream request idle for {stream_idle_timeout_secs}s; \
             treating the stream as dead"
        );
        Self::error(format!(
            "stream idle timeout: no response in {stream_idle_timeout_secs}s"
        ))
        .into()
    }

    pub(super) fn compaction_stalled(stream_idle_timeout_secs: u64) -> anyhow::Error {
        log::warn!(
            "Compaction stream idle for {stream_idle_timeout_secs}s; \
             treating the stream as dead"
        );
        Self::error(format!(
            "stream idle timeout: no events in {stream_idle_timeout_secs}s"
        ))
        .into()
    }
}

impl Thread {
    /// Read-only access to the full message history.
    ///
    /// Used by thread branching to fork a conversation into a new session
    /// carrying real rendered history (separate user/assistant turns and tool
    /// cards), as opposed to flattening the thread into a transcript.
    pub fn messages(&self) -> &[Arc<Message>] {
        &self.messages
    }

    /// Replace the message history, then replay must be driven separately by
    /// the caller (see [`Thread::replay`]). Used by thread branching to seed a
    /// freshly created session with a slice of another thread's messages.
    pub fn set_messages(&mut self, messages: Vec<Arc<Message>>, cx: &mut Context<Self>) {
        self.messages = messages;
        self.clear_summary();
        cx.notify();
    }

    pub(super) fn add_verdict_tool(&mut self, environment: &Rc<dyn ThreadEnvironment>, cx: &App) {
        if AgentSettings::get_global(cx).fork.verdict_ping_pong {
            self.add_tool(RequestVerdictTool::new(environment.clone()));
        }
    }

    pub(super) fn add_peer_thread_tools(&mut self, environment: Rc<dyn ThreadEnvironment>) {
        self.add_tool(ListThreadsTool::new(environment.clone()));
        self.add_tool(SendMessageTool::new(environment));
    }

    /// A trailing Resume was never answered (the turn failed before any
    /// reply), so another one would only stack duplicate user turns.
    pub(super) fn push_resume_message(&mut self) {
        if !matches!(self.messages.last().map(Arc::as_ref), Some(Message::Resume)) {
            self.messages.push(Arc::new(Message::Resume));
        }
    }

    pub(super) fn stream_idle_timeout_secs(this: &WeakEntity<Self>, cx: &AsyncApp) -> Result<u64> {
        this.read_with(cx, |_, cx| {
            AgentSettings::get_global(cx).fork.stream_idle_timeout_secs
        })
    }

    /// Build the idle-timeout future raced against stream events. Returns a
    /// never-firing future when the timeout is disabled (`0`), so callers can
    /// always include the arm in their `select!`.
    ///
    /// The timer guards against providers that accept the HTTP connection but
    /// then never deliver SSE events: without it, `events.next()` blocks
    /// forever and the thread wedges in `Generating` until the auto-prompt
    /// watchdog intervenes. Callers create it before their `select!` because
    /// the request branch borrows `cx`, and snapshot pending tool results
    /// first because the tool branch mutably borrows `tool_results`.
    pub(crate) fn stream_idle_timeout_future(
        stream_idle_timeout_secs: u64,
        cx: &mut AsyncApp,
    ) -> futures::future::Fuse<
        std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
    > {
        if stream_idle_timeout_secs > 0 {
            cx.background_executor()
                .timer(Duration::from_secs(stream_idle_timeout_secs))
                .boxed()
                .fuse()
        } else {
            futures::future::pending::<()>().boxed().fuse()
        }
    }
}

/// Bounded retries for one-shot metadata calls (thread summary, thread
/// title) after a failure.
///
/// These calls get none of the retry benefit the main conversation turn
/// gets from user- and auto_prompt-level retry loops — each is a single
/// `stream_completion` call with no caller-side retry of its own. Provider
/// key rotation (`retry_stream`, in
/// `language_models::provider::open_ai_compatible::health`) deliberately
/// stops rotating within a single request on a rate limit — burning every
/// remaining healthy key on one request would starve the *next* request —
/// and expects that next request to land on a different, healthy key (the
/// failed one is already marked unhealthy by then). Without an outer retry
/// here, a summary/title generation that happens to land on the
/// currently-rate-limited key just fails outright instead of getting that
/// next request.
const METADATA_GENERATION_MAX_RETRIES: u32 = 2;
const METADATA_GENERATION_RETRY_DELAY: Duration = Duration::from_millis(500);

const IMAGE_PLACEHOLDER: &str = "[image omitted: this model doesn't support images]";

/// Metadata calls (thread summary/title), compaction, and main turns run
/// against models that can differ from the one that accepted an image when it
/// was added to the thread (summarization/compaction model selection, model
/// switch mid-thread, refusal fallback). Replaying history that contains
/// image parts to a model without image support gets the whole request
/// rejected by the endpoint (e.g. GLM error 1210 `messages.content.type is
/// invalid, allowed values: ['text']`), so image parts are replaced with text
/// placeholders. Takes the request by value and only rewrites content when
/// image parts are actually present, so the hot path pays a scan, not a copy.
pub(crate) fn strip_unsupported_images(
    model: &LanguageModel,
    mut request: LanguageModelRequest,
) -> LanguageModelRequest {
    fn contains_image(message: &LanguageModelRequestMessage) -> bool {
        message.content.iter().any(|content| match content {
            MessageContent::Image(_) => true,
            MessageContent::ToolResult(tool_result) => tool_result
                .content
                .iter()
                .any(|part| matches!(part, LanguageModelToolResultContent::Image(_))),
            _ => false,
        })
    }

    if model.supports_images() || !request.messages.iter().any(contains_image) {
        return request;
    }

    for message in &mut request.messages {
        for content in &mut message.content {
            match content {
                MessageContent::Image(_) => {
                    *content = MessageContent::Text(IMAGE_PLACEHOLDER.into())
                }
                MessageContent::ToolResult(tool_result) => {
                    for part in &mut tool_result.content {
                        if matches!(part, LanguageModelToolResultContent::Image(_)) {
                            *part = LanguageModelToolResultContent::Text(IMAGE_PLACEHOLDER.into());
                        }
                    }
                }
                _ => {}
            }
        }
    }
    request
}

pub(crate) async fn stream_completion_with_retry(
    model: &LanguageModel,
    request: &LanguageModelRequest,
    cx: &AsyncApp,
) -> std::result::Result<
    BoxStream<
        'static,
        std::result::Result<LanguageModelCompletionEvent, LanguageModelCompletionError>,
    >,
    LanguageModelCompletionError,
> {
    let provider =
        cx.update(|cx| LanguageModelRegistry::read_global(cx).provider_for_model(model))?;
    let request = &strip_unsupported_images(model, request.clone());
    let mut attempt = 0;
    loop {
        match provider.stream_completion(model, request.clone(), cx).await {
            Ok(stream) => return Ok(stream),
            Err(err) if attempt < METADATA_GENERATION_MAX_RETRIES => {
                attempt += 1;
                log::warn!(
                    "metadata generation call failed (attempt {attempt}/{METADATA_GENERATION_MAX_RETRIES}): {err:#}, retrying"
                );
                cx.background_executor()
                    .timer(METADATA_GENERATION_RETRY_DELAY)
                    .await;
            }
            Err(err) => return Err(err),
        }
    }
}

pub(super) fn push_image_markdown(markdown: &mut String, image: &LanguageModelImage) {
    match &image.uri {
        Some(uri) => markdown.push_str(&format!("<image src=\"{uri}\" />\n")),
        None => markdown.push_str("<image />\n"),
    }
}

pub(super) fn image_content_block(image: LanguageModelImage) -> acp_v2::ContentBlock {
    let mut content = acp_v2::ImageContent::new(image.source, "image/png");
    if let Some(uri) = image.uri {
        content = content.uri(uri);
    }
    acp_v2::ContentBlock::Image(content)
}

pub(super) fn convert_image(image_content: acp_v2::ImageContent) -> LanguageModelImage {
    LanguageModelImage {
        source: image_content.data.into(),
        uri: image_content.uri,
    }
}

impl ToolCallEventStream {
    /// Whether the user already acknowledged the Windows-drive (DrvFs)
    /// sandbox-integrity warning for this thread; when true the terminal tool
    /// skips the re-prompt for later commands in the thread.
    pub(crate) fn windows_fs_warning_acknowledged(&self) -> bool {
        self.sandbox_grants
            .borrow()
            .windows_fs_warning_acknowledged()
    }

    /// Acknowledging the warning records a thread-scoped flag so later
    /// commands in this thread skip the re-prompt, and persists it so it
    /// survives a reopen (same pattern as the other "for this thread" grants).
    pub(super) fn windows_fs_warning_acknowledger(
        &self,
    ) -> impl FnOnce(&AsyncApp) -> Result<()> + 'static {
        let sandbox_grants = self.sandbox_grants.clone();
        let thread = self.thread.clone();
        move |cx| {
            sandbox_grants.borrow_mut().record_windows_fs_warning_ack();
            Self::persist_thread_grants(&thread, cx);
            Ok(())
        }
    }
}
