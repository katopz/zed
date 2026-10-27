//! Fork-owned `Thread` policy (plan 038): stream idle timeouts, retry caps,
//! metadata-call retries and image stripping, kept out of `thread.rs` so
//! upstream syncs don't conflict on them.

use super::*;

/// Delay between retries of a stream idle timeout. Longer than
/// `BASE_RETRY_DELAY` because the provider hang that caused the silent
/// stream typically outlasts a few seconds.
pub(crate) const STREAM_IDLE_RETRY_DELAY: Duration = Duration::from_secs(30);
/// Upper bound on a provider-supplied `retry_after` hint. Some providers
/// send multi-hour values on transient 429s; honored raw, they pin the turn
/// in a silent backoff with a stale retry callout for hours.
pub(crate) const MAX_RETRY_AFTER: Duration = Duration::from_secs(2 * 60);

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

    /// Build the idle-timeout future raced against stream events. Returns a
    /// never-firing future when the timeout is disabled (`0`), so callers can
    /// always include the arm in their `select!`.
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
}
