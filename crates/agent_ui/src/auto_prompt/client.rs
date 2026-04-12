use std::sync::Arc;

use anyhow::{Context as _, Result};
use futures::StreamExt;
use gpui::AsyncApp;
use language_model::{
    LanguageModel, LanguageModelCompletionEvent, LanguageModelRequest, LanguageModelRequestMessage,
    Role,
};

use super::context::AutoPromptResponse;

/// Calls the configured language model with thread context as a chat completion
/// and returns the parsed response.
///
/// Uses Zed's built-in LLM infrastructure, so any configured provider
/// (OpenAI, Anthropic, Ollama, etc.) works automatically.
pub async fn call_language_model(
    model: &Arc<dyn LanguageModel>,
    system_prompt: &str,
    context_json: &str,
    cx: &AsyncApp,
) -> Result<AutoPromptResponse> {
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

    let mut stream = model
        .stream_completion(request, cx)
        .await
        .context("auto_prompt: failed to start completion stream")?;

    let mut response_text = String::new();
    while let Some(event) = stream.next().await {
        match event {
            Ok(LanguageModelCompletionEvent::Text(text)) => response_text.push_str(&text),
            Ok(_) => {}
            Err(err) => {
                log::warn!("auto_prompt: stream error: {err}");
                break;
            }
        }
    }

    parse_response(&response_text)
}

fn parse_response(text: &str) -> Result<AutoPromptResponse> {
    let json_str = extract_json(text);
    serde_json::from_str(json_str).with_context(|| {
        format!(
            "auto_prompt: failed to parse response as JSON: {}",
            text.chars().take(500).collect::<String>()
        )
    })
}

fn extract_json(text: &str) -> &str {
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
