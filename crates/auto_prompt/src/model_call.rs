//! Model call: streams the orchestrator completion and parses its JSON decision.

use crate::AutoPromptResponse;
use anyhow::Context as _;
use futures::{StreamExt, future, pin_mut};
use language_model::{
    LanguageModel, LanguageModelCompletionEvent, LanguageModelRequest, LanguageModelRequestMessage,
    Role,
};
use std::time::Duration;

/// Streams a completion from `model` through the provider registered for it;
/// `LanguageModel` is plain data, so requests go through its provider.
pub async fn stream_model_completion(
    model: &LanguageModel,
    request: LanguageModelRequest,
    cx: &gpui::AsyncApp,
) -> Result<
    language_model::LanguageModelCompletionStream,
    language_model::LanguageModelCompletionError,
> {
    let provider = cx.update(|cx| {
        language_model::LanguageModelRegistry::read_global(cx).provider_for_model(model)
    })?;
    provider.stream_completion(model, request, cx).await
}

pub(crate) async fn call_language_model(
    model: &LanguageModel,
    system_prompt: &str,
    context_json: &str,
    cx: &gpui::AsyncApp,
) -> anyhow::Result<(String, AutoPromptResponse)> {
    let request = LanguageModelRequest {
        messages: vec![
            LanguageModelRequestMessage {
                role: Role::System,
                content: vec![system_prompt.to_owned().into()],
                cache: false,
                reasoning_details: None,
            },
            LanguageModelRequestMessage {
                role: Role::User,
                content: vec![context_json.to_owned().into()],
                cache: false,
                reasoning_details: None,
            },
        ],
        ..Default::default()
    };

    let completion_future = async {
        let mut stream = crate::stream_model_completion(model, request, cx)
            .await
            .context("auto_prompt: failed to start completion stream")?;

        let mut text_parts: Vec<String> = Vec::new();
        let mut thinking_parts: Vec<String> = Vec::new();
        let mut stream_errors: Vec<anyhow::Error> = Vec::new();
        let mut total_events: u32 = 0;
        let mut other_event_types: Vec<String> = Vec::new();
        while let Some(event) = stream.next().await {
            total_events += 1;
            match event {
                Ok(LanguageModelCompletionEvent::Text(text)) => {
                    log::debug!(
                        "auto_prompt: stream event #{}: Text ({} chars)",
                        total_events,
                        text.len()
                    );
                    text_parts.push(text);
                }
                Ok(LanguageModelCompletionEvent::Thinking { text, .. }) => {
                    log::debug!(
                        "auto_prompt: stream event #{}: Thinking ({} chars)",
                        total_events,
                        text.len()
                    );
                    thinking_parts.push(text);
                }
                Ok(ref other) => {
                    let type_name = format!("{other:?}");
                    let short = type_name.chars().take(60).collect::<String>();
                    log::debug!(
                        "auto_prompt: stream event #{}: Other: {short}",
                        total_events
                    );
                    other_event_types.push(short);
                }
                Err(err) => {
                    log::warn!(
                        "auto_prompt: stream event #{}: Error: {err:#}",
                        total_events
                    );
                    stream_errors.push(err.into());
                }
            }
        }

        log::info!(
            "auto_prompt: stream complete — {} total events: {} Text ({} chars), {} Thinking ({} chars), {} Other, {} Errors. Other types: {:?}",
            total_events,
            text_parts.len(),
            text_parts.concat().len(),
            thinking_parts.len(),
            thinking_parts.concat().len(),
            other_event_types.len(),
            stream_errors.len(),
            other_event_types,
        );

        if stream_errors
            .iter()
            .any(|e| format!("{e:#}").contains("rate_limit") || format!("{e:#}").contains("429"))
        {
            log::warn!("auto_prompt: rate limit detected in stream errors");
        }

        let text = text_parts.concat();
        if !text.trim().is_empty() {
            anyhow::Ok(text)
        } else if !thinking_parts.is_empty() {
            let thinking = thinking_parts.concat();
            if !thinking.trim().is_empty() {
                log::info!(
                    "auto_prompt: Thinking fallback: {} empty Text parts, {} Thinking events ({} chars)",
                    text_parts.len(),
                    thinking_parts.len(),
                    thinking.len()
                );
                let synthetic = serde_json::json!({
                    "next_prompt": null,
                    "reason": format!("Model returned {} Thinking events but no Text output", thinking_parts.len()),
                    "confidence": 0.3,
                    "thread_summary": null
                });
                anyhow::Ok(synthetic.to_string())
            } else {
                log::warn!(
                    "auto_prompt: model returned no usable content ({} empty Text events, {} empty Thinking events, {} stream errors), synthesizing stop",
                    text_parts.len(),
                    thinking_parts.len(),
                    stream_errors.len()
                );
                let synthetic = serde_json::json!({
                    "next_prompt": null,
                    "reason": format!("model returned no usable content ({} empty Text, {} empty Thinking, {} stream errors)", text_parts.len(), thinking_parts.len(), stream_errors.len()),
                    "confidence": 0.0,
                    "thread_summary": null
                });
                anyhow::Ok(synthetic.to_string())
            }
        } else if !stream_errors.is_empty() {
            let error_details: Vec<String> =
                stream_errors.iter().map(|e| format!("{e:#}")).collect();
            log::warn!(
                "auto_prompt: model stream produced only errors — details: {error_details:?}"
            );
            let synthetic = serde_json::json!({
                "next_prompt": null,
                "reason": format!("model stream produced only errors ({})", stream_errors.len()),
                "confidence": 0.0,
                "thread_summary": null
            });
            anyhow::Ok(synthetic.to_string())
        } else {
            log::warn!(
                "auto_prompt: model returned zero events (0 Text, 0 Thinking) out of {total_events} total events. Other types seen: {other_event_types:?}"
            );
            let synthetic = serde_json::json!({
                "next_prompt": null,
                "reason": format!("model returned zero events ({} total stream events)", total_events),
                "confidence": 0.0,
                "thread_summary": null
            });
            anyhow::Ok(synthetic.to_string())
        }
    };

    let timeout_future = cx.background_executor().timer(Duration::from_secs(60));

    pin_mut!(completion_future, timeout_future);

    match future::select(completion_future, timeout_future).await {
        future::Either::Left((Ok(response_text), _)) => {
            parse_response(&response_text).map(|parsed| (response_text, parsed))
        }
        future::Either::Left((Err(err), _)) => Err(err.context("auto_prompt: completion failed")),
        future::Either::Right(_) => {
            anyhow::bail!("auto_prompt: LLM call timed out after 60 seconds");
        }
    }
}

/// Whether `response` is a stop this module synthesized because the model
/// produced nothing usable (empty or errored stream, unparseable JSON) rather
/// than a real verdict. Matches the reasons written by `call_language_model`
/// and `parse_response`.
pub(crate) fn is_synthetic_failure(response: &AutoPromptResponse) -> bool {
    response.confidence.unwrap_or(1.0) <= 0.3
        && response.reason.as_deref().is_some_and(|reason| {
            let lower = reason.to_ascii_lowercase();
            lower.starts_with("model returned")
                || lower.starts_with("model stream")
                || lower.starts_with("unparseable response")
        })
}

pub(crate) fn parse_response(text: &str) -> anyhow::Result<AutoPromptResponse> {
    let json_str = extract_json(text);
    match serde_json::from_str(json_str) {
        Ok(response) => Ok(response),
        Err(parse_err) => {
            let err_string = format!("{parse_err}");
            if err_string.contains("duplicate field") {
                if let Ok(response) = deduplicate_and_parse(json_str) {
                    log::info!("auto_prompt: recovered from duplicate key error: {parse_err}");
                    return Ok(response);
                }
            }
            let preview = text.chars().take(200).collect::<String>();
            log::warn!("auto_prompt: failed to parse response as JSON ({parse_err}): {preview:?}");
            log::warn!("auto_prompt: synthesizing stop response to avoid retry loop");
            Ok(AutoPromptResponse {
                next_prompt: None,
                reason: Some(format!(
                    "unparseable response ({} bytes, {} extracted): {parse_err}",
                    text.len(),
                    json_str.len()
                )),
                confidence: Some(0.0),
                thread_summary: None,
            })
        }
    }
}

/// Attempt to parse JSON with duplicate object keys by keeping the first occurrence
/// of each key. This handles LLMs that emit e.g.
/// `"thread_summary": "...", "thread_summary": null`.
pub(crate) fn deduplicate_and_parse(json_str: &str) -> anyhow::Result<AutoPromptResponse> {
    let deduped = rebuild_deduplicated_json(json_str)?;
    log::debug!("auto_prompt: deduplicated JSON: {deduped}");
    serde_json::from_str(&deduped).context("re-parsed deduplicated JSON")
}

/// Rebuild a JSON object string, keeping only the first occurrence of each key.
pub(crate) fn rebuild_deduplicated_json(json_str: &str) -> anyhow::Result<String> {
    let trimmed = json_str.trim();
    anyhow::ensure!(
        trimmed.starts_with('{') && trimmed.ends_with('}'),
        "expected JSON object"
    );

    // Walk the string character by character to split top-level key-value pairs,
    // tracking brace/bracket depth to avoid splitting inside nested structures.
    let inner = &trimmed[1..trimmed.len() - 1];
    let mut seen_keys: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut result_entries: Vec<String> = Vec::new();

    let bytes = inner.as_bytes();
    let mut pos = 0;

    while pos < bytes.len() {
        // Skip whitespace
        while pos < bytes.len() && bytes[pos] == b' '
            || bytes[pos] == b'\n'
            || bytes[pos] == b'\r'
            || bytes[pos] == b'\t'
        {
            pos += 1;
        }
        if pos >= bytes.len() {
            break;
        }

        // Expect a key (quoted string)
        if bytes[pos] != b'"' {
            break;
        }
        let key_start = pos;
        pos += 1;
        let mut escaped = false;
        while pos < bytes.len() {
            if escaped {
                escaped = false;
            } else if bytes[pos] == b'\\' {
                escaped = true;
            } else if bytes[pos] == b'"' {
                break;
            }
            pos += 1;
        }
        if pos >= bytes.len() {
            break;
        }
        pos += 1; // skip closing quote
        let key_end = pos;

        // Parse the actual key value
        let key_str = &trimmed[1 + key_start..1 + key_end]; // +1 for the opening brace
        let parsed_key: String = serde_json::from_str(key_str).unwrap_or_default();

        // Skip whitespace and colon
        while pos < bytes.len()
            && (bytes[pos] == b' '
                || bytes[pos] == b'\n'
                || bytes[pos] == b'\r'
                || bytes[pos] == b'\t'
                || bytes[pos] == b':')
        {
            pos += 1;
        }

        // Now parse the value — could be string, number, bool, null, object, or array
        let value_start = pos;
        // We need to find where the value ends (at top level, before a comma)
        let mut depth = 0i32;
        let mut in_string = false;
        let mut was_escaped = false;
        while pos < bytes.len() {
            let ch = bytes[pos];
            if was_escaped {
                was_escaped = false;
            } else if in_string {
                if ch == b'\\' {
                    was_escaped = true;
                } else if ch == b'"' {
                    in_string = false;
                }
            } else {
                match ch {
                    b'"' => in_string = true,
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => depth -= 1,
                    b',' if depth == 0 => break,
                    _ => {}
                }
            }
            pos += 1;
        }
        let value_end = pos;

        // Skip optional comma
        if pos < bytes.len() && bytes[pos] == b',' {
            pos += 1;
        }

        // The raw entry from key_start to value_end (relative to `inner`)
        let full_entry = format!("{key_str}: {}", &inner[value_start..value_end]);

        if seen_keys.insert(parsed_key.clone()) {
            result_entries.push(full_entry);
        } else {
            log::debug!("auto_prompt: deduplicating key '{parsed_key}' in LLM response");
        }
    }

    Ok(format!("{{{}}}", result_entries.join(",")))
}

pub(crate) fn extract_json(text: &str) -> &str {
    if let Some(start) = text.find("```json") {
        let content_start = start + 7;
        if let Some(end) = text[content_start..].find("```") {
            return text[content_start..content_start + end].trim();
        }
    }
    if let Some(start) = text.find('{') {
        if let Some(end) = text.rfind('}') {
            if end > start {
                return &text[start..=end];
            }
        }
    }
    text.trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_response_recovers_from_duplicate_key() {
        let raw = r#"```json
{"confidence": 0.75, "next_prompt": "Continue with plan 260", "reason": "Plan 257 done", "thread_summary": "Plan 257 fully implemented.", "thread_summary": null}
```"#;
        let result = parse_response(raw).expect("should recover from duplicate key");
        assert_eq!(
            result.next_prompt.as_deref(),
            Some("Continue with plan 260")
        );
        assert_eq!(result.confidence, Some(0.75));
        assert_eq!(
            result.thread_summary.as_deref(),
            Some("Plan 257 fully implemented.")
        );
    }

    #[test]
    fn test_parse_response_no_duplicate_still_works() {
        let raw = r#"```json
{"confidence": 0.9, "next_prompt": "Do the thing", "reason": "work remains", "thread_summary": "Summary here."}
```"#;
        let result = parse_response(raw).expect("normal JSON should parse fine");
        assert_eq!(result.next_prompt.as_deref(), Some("Do the thing"));
        assert_eq!(result.confidence, Some(0.9));
    }

    #[test]
    fn test_rebuild_deduplicated_json_keeps_first_occurrence() {
        let json = r#"{"a": 1, "b": "two", "a": 3}"#;
        let result = rebuild_deduplicated_json(json).expect("should succeed");
        let parsed: serde_json::Value =
            serde_json::from_str(&result).expect("should be valid JSON");
        assert_eq!(parsed["a"], 1);
        assert_eq!(parsed["b"], "two");
    }

    #[test]
    fn test_rebuild_deduplicated_json_duplicate_null_after_value() {
        let json = r#"{"thread_summary": "long summary here", "confidence": 0.75, "thread_summary": null}"#;
        let result = rebuild_deduplicated_json(json).expect("should succeed");
        let parsed: serde_json::Value =
            serde_json::from_str(&result).expect("should be valid JSON");
        assert_eq!(parsed["thread_summary"], "long summary here");
        assert_eq!(parsed["confidence"], 0.75);
    }

    fn response(confidence: f64, reason: &str) -> AutoPromptResponse {
        AutoPromptResponse {
            next_prompt: None,
            reason: Some(reason.to_string()),
            confidence: Some(confidence),
            thread_summary: None,
        }
    }

    #[test]
    fn synthetic_failure_covers_every_synthesized_reason() {
        let unparseable = parse_response("not json").expect("synthesized");
        assert!(is_synthetic_failure(&unparseable));
        for reason in [
            "model returned zero events (0 total stream events)",
            "Model returned 2 Thinking events but no Text output",
            "model stream produced only errors (1)",
        ] {
            assert!(is_synthetic_failure(&response(0.0, reason)), "{reason}");
        }
    }

    #[test]
    fn synthetic_failure_ignores_real_verdicts() {
        assert!(!is_synthetic_failure(&response(
            0.1,
            "Model is done, nothing left"
        )));
        assert!(!is_synthetic_failure(&response(
            0.9,
            "model returned zero events"
        )));
        assert!(!is_synthetic_failure(&response(0.1, "all tasks complete")));
    }
}
