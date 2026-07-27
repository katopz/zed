//! Drives the Gemini web app in an attached Chrome tab.
//!
//! Gemini's markup is not a public API and its class names change, so every
//! element is located through a list of candidate selectors rather than one
//! hardcoded string, and [`GeminiPage::diagnose`] reports which candidates
//! currently match so a break can be identified and fixed quickly.

use anyhow::{Result, bail};
use serde_json::Value;
use std::time::Duration;

use crate::cdp::Cdp;

pub const GEMINI_ORIGIN: &str = "https://gemini.google.com";
pub const GEMINI_APP_URL: &str = "https://gemini.google.com/app";

/// The prompt composer. Gemini renders a Quill rich-text editor, so the
/// `contenteditable` variants are the ones that normally hit.
const COMPOSER_SELECTORS: &[&str] = &[
    "div.ql-editor[contenteditable=\"true\"]",
    "rich-textarea div[contenteditable=\"true\"]",
    "div[contenteditable=\"true\"][role=\"textbox\"]",
    "div[contenteditable=\"true\"]",
    "textarea[aria-label]",
];

/// Individual model response blocks, newest last.
const RESPONSE_SELECTORS: &[&str] = &[
    "model-response message-content",
    "message-content.model-response-text",
    "message-content",
    ".model-response-text",
    "div.markdown",
];

/// Poll cadence while a response streams in.
const POLL_INTERVAL: Duration = Duration::from_millis(400);

/// Consecutive unchanged polls before a response is considered complete. Text
/// stability is used rather than a "stop generating" button selector because it
/// does not depend on knowing yet another unstable class name.
const STABLE_POLLS: usize = 3;

pub struct GeminiPage {
    cdp: Cdp,
}

impl GeminiPage {
    /// Attaches to an existing Gemini tab, or opens one.
    pub async fn open(mut cdp: Cdp) -> Result<Self> {
        cdp.attach_to_page(GEMINI_ORIGIN, GEMINI_APP_URL).await?;
        Ok(Self { cdp })
    }

    /// Evaluates arbitrary JavaScript in the Gemini tab.
    ///
    /// Exposed so the page can be inspected and driven directly when the
    /// selectors below need updating.
    pub async fn evaluate(&mut self, expression: &str) -> Result<Value> {
        self.cdp.evaluate(expression).await
    }

    /// Reports which candidate selectors match right now, and how many nodes
    /// each finds. The first non-zero entry in each list is what `ask` will use.
    pub async fn diagnose(&mut self) -> Result<String> {
        let expression = format!(
            r#"(function() {{
                const groups = {{ composer: {composer}, response: {response} }};
                const lines = [];
                lines.push('url: ' + location.href);
                lines.push('title: ' + document.title);
                for (const [group, selectors] of Object.entries(groups)) {{
                    for (const selector of selectors) {{
                        let count;
                        try {{ count = document.querySelectorAll(selector).length; }}
                        catch (error) {{ count = 'invalid selector'; }}
                        lines.push(group + '  ' + count + '  ' + selector);
                    }}
                }}
                return lines.join('\n');
            }})()"#,
            composer = serde_json::to_string(COMPOSER_SELECTORS)?,
            response = serde_json::to_string(RESPONSE_SELECTORS)?,
        );
        let report = self.cdp.evaluate(&expression).await?;
        Ok(report.as_str().unwrap_or_default().to_string())
    }

    /// Sends `prompt` to Gemini and returns the response text.
    ///
    /// Focuses the composer, inserts the prompt through Chrome's input pipeline,
    /// presses Enter, then waits for a new response block to appear and stop
    /// changing.
    pub async fn ask(&mut self, prompt: &str, timeout: Duration) -> Result<String> {
        // Pin whichever selector already matches so the before/after counts are
        // comparable. Re-resolving mid-wait could switch to a different selector
        // whose unrelated count exceeds the baseline, which would read an older
        // message as if it were the new reply.
        let pinned_selector = self.first_matching_selector(RESPONSE_SELECTORS).await?;
        let baseline_count = match pinned_selector.as_deref() {
            Some(selector) => self.count_matching(selector).await?,
            // A brand-new conversation has no response blocks at all, so no
            // selector matches yet and every block that appears later is new.
            None => 0,
        };

        self.focus_and_select_composer().await?;
        self.cdp.insert_text(prompt).await?;
        self.cdp.press_enter().await?;

        self.wait_for_response(pinned_selector, baseline_count, timeout)
            .await
    }

    /// Focuses the composer and selects any existing draft text so the inserted
    /// prompt replaces it instead of being appended to it.
    async fn focus_and_select_composer(&mut self) -> Result<()> {
        let expression = format!(
            r#"(function() {{
                const selectors = {selectors};
                for (const selector of selectors) {{
                    const element = document.querySelector(selector);
                    if (!element) continue;
                    element.focus();
                    if (element.scrollIntoView) element.scrollIntoView({{ block: 'center' }});
                    const selection = window.getSelection();
                    if (selection) {{
                        selection.removeAllRanges();
                        const range = document.createRange();
                        range.selectNodeContents(element);
                        selection.addRange(range);
                    }}
                    return selector;
                }}
                return null;
            }})()"#,
            selectors = serde_json::to_string(COMPOSER_SELECTORS)?,
        );

        let matched = self.cdp.evaluate(&expression).await?;
        if matched.is_null() {
            bail!(
                "Could not find Gemini's prompt box. If the Chrome window shows a sign-in page, \
                 sign in to Gemini there and try again. If you are already signed in, Gemini's \
                 markup likely changed — run the Gemini diagnose action to see which selectors \
                 still match."
            );
        }
        Ok(())
    }

    async fn first_matching_selector(&mut self, selectors: &[&str]) -> Result<Option<String>> {
        let expression = format!(
            r#"(function() {{
                const selectors = {selectors};
                for (const selector of selectors) {{
                    try {{
                        if (document.querySelectorAll(selector).length > 0) return selector;
                    }} catch (error) {{ /* skip selectors this Chrome rejects */ }}
                }}
                return null;
            }})()"#,
            selectors = serde_json::to_string(selectors)?,
        );
        let matched = self.cdp.evaluate(&expression).await?;
        Ok(matched.as_str().map(str::to_string))
    }

    async fn count_matching(&mut self, selector: &str) -> Result<usize> {
        let expression = format!(
            "document.querySelectorAll({}).length",
            serde_json::to_string(selector)?
        );
        let count = self.cdp.evaluate(&expression).await?;
        Ok(count.as_u64().unwrap_or(0) as usize)
    }

    /// Waits for a response block beyond `baseline_count` to appear, then for
    /// its text to stop growing.
    async fn wait_for_response(
        &mut self,
        pinned_selector: Option<String>,
        baseline_count: usize,
        timeout: Duration,
    ) -> Result<String> {
        let deadline_polls = (timeout.as_millis() / POLL_INTERVAL.as_millis()).max(1);
        let mut last_text = String::new();
        let mut stable_polls = 0;
        let mut saw_new_response = false;

        for _ in 0..deadline_polls {
            self.cdp.executor().timer(POLL_INTERVAL).await;

            // With no pinned selector the conversation started empty, so keep
            // probing until the first response block renders and becomes
            // matchable.
            let selector = match &pinned_selector {
                Some(selector) => selector.clone(),
                None => match self.first_matching_selector(RESPONSE_SELECTORS).await? {
                    Some(selector) => selector,
                    None => continue,
                },
            };
            let count = self.count_matching(&selector).await?;
            if count <= baseline_count {
                continue;
            }
            saw_new_response = true;

            let expression = format!(
                r#"(function() {{
                    const elements = document.querySelectorAll({selector});
                    if (!elements.length) return '';
                    return elements[elements.length - 1].innerText || '';
                }})()"#,
                selector = serde_json::to_string(&selector)?,
            );
            let text = self
                .cdp
                .evaluate(&expression)
                .await?
                .as_str()
                .unwrap_or_default()
                .to_string();

            if text.trim().is_empty() {
                continue;
            }
            if text == last_text {
                stable_polls += 1;
                if stable_polls >= STABLE_POLLS {
                    return Ok(text);
                }
            } else {
                stable_polls = 0;
                last_text = text;
            }
        }

        if saw_new_response && !last_text.trim().is_empty() {
            // Timed out mid-stream. A partial answer is more useful than an
            // error, so return what rendered.
            log::warn!("Gemini response did not settle within {timeout:?}; returning partial text");
            return Ok(last_text);
        }
        if saw_new_response {
            bail!("Gemini started a response but it stayed empty for {timeout:?}");
        }
        bail!(
            "Gemini did not produce a response within {timeout:?}. Check the Chrome window — it \
             may be showing a sign-in prompt, a rate limit, or a consent dialog."
        )
    }
}

/// Truncates `text` to at most `max_bytes`, never splitting a UTF-8 character.
///
/// Used for log/preview strings; Gemini output routinely contains em-dashes and
/// other multi-byte characters, so byte slicing without a boundary check would
/// panic.
pub fn truncate_at_char_boundary(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.get(..end).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_at_char_boundary_never_splits_multibyte_characters() {
        // An em-dash is three bytes, so a naive `&text[..4]` would panic here.
        let text = "ab—cd";
        assert_eq!(truncate_at_char_boundary(text, 4), "ab");
        assert_eq!(truncate_at_char_boundary(text, 5), "ab—");
        assert_eq!(truncate_at_char_boundary(text, 100), text);
        assert_eq!(truncate_at_char_boundary("", 4), "");
    }
}
