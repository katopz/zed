//! Parsing for the `<agent_message …>` envelope that agent-mail wraps around
//! prompts delivered from other agent threads (see `format_mailbox_prompt` in
//! the `agent` crate).
//!
//! The user-message box renders that envelope as a GUI header (from / to /
//! session chips) instead of raw tag text, so the display and resend paths
//! need a lossless split: the editor shows only [`MailPromptEnvelope::body`],
//! and resending splices the edited body back with [`MailPromptEnvelope::rewrap`]
//! (`head` + body + `tail`), byte-identical to the original prompt while the
//! body is unchanged.
//!
//! Only the known single-envelope shapes are transformed: an optional urgent
//! delivery notice, one envelope section, then optionally the mailbox trailer.
//! Anything else — multiple envelopes, unknown trailing content — renders raw,
//! exactly as before.

use std::collections::HashMap;

use agent::{MAIL_PROMPT_PREEMPT_NOTICE, MAIL_PROMPT_TRAILER};
use agent_client_protocol::schema::v2 as acp_v2;

const OPEN_TAG: &str = "<agent_message";
const OPEN_TAG_LEGACY: &str = "<agent_mail";
const CLOSE_TAG: &str = "</agent_message>";
const CLOSE_TAG_LEGACY: &str = "</agent_mail>";

/// The marker `post_agent_mail` uses to pack the recipient into the `from`
/// attribute: `"{from} via agent-mail, to {to}"`.
const FROM_TO_SEPARATOR: &str = ", to ";
/// Suffix `post_agent_mail` appends to the recipient for fyi-mode deliveries.
const FYI_SUFFIX: &str = " (fyi — no reply needed)";

/// A parsed mail envelope: the metadata needed for the GUI header plus the
/// lossless `head`/`tail` splices around the displayed body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MailPromptEnvelope {
    /// Sender as written in the `from` attribute (before the `, to …`
    /// marker), e.g. `boss@amo via agent-mail`.
    pub from: Option<String>,
    /// Recipient, unpacked from the `from` attribute or a `to` attribute.
    pub to: Option<String>,
    /// The sender's session id (`session_id` attribute).
    pub session_id: Option<String>,
    /// The delivery was fyi-mode (no reply needed).
    pub fyi: bool,
    /// The prompt interrupted a running turn for urgent delivery.
    pub urgent: bool,
    /// Everything before the body: the optional preempt notice, the opening
    /// tag line, and its trailing newline.
    pub head: String,
    /// The newline before the closing tag plus the closing tag itself, so
    /// `head` + `body` + `tail` is this envelope's byte-exact raw form.
    pub tail: String,
    /// Whatever follows `tail` (the known mailbox trailer, trailing
    /// whitespace, or nothing), byte-exact from the original, so `rewrap`
    /// round-trips the whole prompt.
    pub trailer: String,
    /// The mail body shown in the editor.
    pub body: String,
}

impl MailPromptEnvelope {
    /// Parses a user-message text into its envelope. Returns `None` for
    /// anything that is not exactly one known-envelope mail prompt, which the
    /// caller must then render raw. Batch prompts (several envelopes in one
    /// message) parse with [`Self::parse_sections`] instead.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let mut sections = Self::parse_sections(text)?;
        if sections.len() != 1 {
            return None;
        }
        sections.pop()
    }

    /// Parses every envelope section of a mail prompt. Batch delivery packs
    /// several messages into one user prompt (see `format_mailbox_prompt` in
    /// the `agent` crate), so a known mail shape is an optional preempt
    /// notice, N >= 1 envelope sections, then optionally the mailbox trailer.
    /// Returns `None` for anything else, which the caller must render raw.
    pub(crate) fn parse_sections(text: &str) -> Option<Vec<Self>> {
        let mut rest = text;
        let mut urgent = false;
        if let Some(after) = rest.strip_prefix(MAIL_PROMPT_PREEMPT_NOTICE) {
            urgent = true;
            rest = after.strip_prefix('\n').unwrap_or(after);
        }

        let mut sections = Vec::new();
        loop {
            let (open_tag, close_tag) = if rest.starts_with(OPEN_TAG) {
                (OPEN_TAG, CLOSE_TAG)
            } else if rest.starts_with(OPEN_TAG_LEGACY) {
                (OPEN_TAG_LEGACY, CLOSE_TAG_LEGACY)
            } else {
                return None;
            };

            // The opening tag is a single line: the attribute values are mail
            // addresses and never contain newlines.
            let tag_line_end = rest.find('\n')?;
            let tag_line = &rest[..tag_line_end];
            if !tag_line.ends_with('>') {
                return None;
            }
            let attributes = parse_tag_attributes(&tag_line[open_tag.len()..tag_line.len() - 1]);

            let after_tag = &rest[tag_line_end + 1..];
            // The writer puts each closing tag on its own line. A section
            // boundary is a closing tag followed only by another envelope, the
            // known mailbox trailer, or the end of the prompt — a closing tag
            // merely mentioned inside the body keeps the search going (the
            // same discipline the single-envelope parser applied via `rfind`).
            let close_marker = format!("\n{close_tag}");
            let mut search_from = 0;
            let (close_ix, after_close) = loop {
                let rel = after_tag[search_from..].find(&close_marker)?;
                let candidate = search_from + rel;
                let candidate_after = &after_tag[candidate + close_marker.len()..];
                let trimmed = candidate_after.trim();
                let next_opens_envelope = candidate_after.trim_start().starts_with(OPEN_TAG)
                    || candidate_after.trim_start().starts_with(OPEN_TAG_LEGACY);
                if !next_opens_envelope && !trimmed.is_empty() && trimmed != MAIL_PROMPT_TRAILER {
                    search_from = candidate + 1;
                    continue;
                }
                break (candidate, candidate_after);
            };

            // `head` is a prefix of `text`, so this span covers everything
            // before this section's body: the notice (first section only)
            // plus every earlier section, this opening tag line, and its
            // newline.
            let head = text[..text.len() - after_tag.len()].to_string();
            let body = after_tag[..close_ix].to_string();
            // A line-start envelope tag inside a body means the boundary search
            // could not attribute a quoted envelope (a foreign shape that
            // `format_mailbox_prompt` never produces) — keep the whole prompt
            // raw rather than render a mis-split section. Mid-line mentions
            // stay inside the body and render as mail.
            if [CLOSE_TAG, CLOSE_TAG_LEGACY, OPEN_TAG, OPEN_TAG_LEGACY]
                .iter()
                .any(|tag| body.contains(&format!("\n{tag}")))
            {
                return None;
            }
            let is_last = {
                let trimmed = after_close.trim();
                trimmed.is_empty() || trimmed == MAIL_PROMPT_TRAILER
            };
            let (tail, trailer, next) = if is_last {
                (close_marker, after_close.to_string(), None)
            } else {
                (
                    close_marker.clone(),
                    String::new(),
                    Some(after_close.strip_prefix('\n').unwrap_or(after_close)),
                )
            };

            let mut section = Self {
                from: attributes.get("from").cloned(),
                to: None,
                session_id: attributes.get("session_id").cloned(),
                fyi: false,
                urgent,
                head,
                tail,
                trailer,
                body,
            };
            section.unpack_from_attribute();
            sections.push(section);

            match next {
                Some(next_rest) => rest = next_rest,
                None => break,
            }
        }

        Some(sections)
    }

    /// Splits the packed `from` attribute (`"{from} via agent-mail, to {to}"`)
    /// into the `from`/`to`/`fyi` fields.
    fn unpack_from_attribute(&mut self) {
        if self.to.is_none()
            && let Some(raw) = self.from.as_deref()
            && let Some(ix) = raw.find(FROM_TO_SEPARATOR)
        {
            let mut to_value = raw[ix + FROM_TO_SEPARATOR.len()..].to_string();
            if let Some(stripped) = to_value.strip_suffix(FYI_SUFFIX) {
                self.fyi = true;
                to_value = stripped.to_string();
            }
            self.from = Some(raw[..ix].to_string());
            self.to = Some(to_value);
        }
    }

    /// Splices a (possibly edited) body back into the original prompt.
    pub(crate) fn rewrap(&self, body: &str) -> String {
        format!("{}{}{}{}", self.head, body, self.tail, self.trailer)
    }

    /// This envelope's byte-exact raw form, without the prompt-level trailer:
    /// what a `copy as raw` action hands to the clipboard.
    pub(crate) fn raw(&self) -> String {
        format!("{}{}{}", self.head, self.body, self.tail)
    }
}

/// The display form of a user message's source blocks: when the first text
/// block is a known mail prompt it is replaced by the bare bodies of its
/// envelope sections (batch prompts join them with a blank line), so the box
/// header can render each section's metadata as GUI chips instead of raw tag
/// text.
pub(crate) fn strip_mail_envelope_blocks(
    source_blocks: &[acp_v2::ContentBlock],
) -> Vec<acp_v2::ContentBlock> {
    let Some(acp_v2::ContentBlock::Text(text)) = source_blocks.first() else {
        return source_blocks.to_vec();
    };
    let Some(sections) = MailPromptEnvelope::parse_sections(&text.text) else {
        return source_blocks.to_vec();
    };
    let mut display = Vec::with_capacity(source_blocks.len());
    let mut body_text = text.clone();
    body_text.text = sections
        .iter()
        .map(|section| section.body.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    display.push(acp_v2::ContentBlock::Text(body_text));
    display.extend(source_blocks.iter().skip(1).cloned());
    display
}

/// Whether a user message packs several mail envelopes into one prompt (batch
/// delivery). Such prompts render their sections read-only: the editor shows
/// the joined bodies, which no longer splice back into per-envelope shapes.
pub(crate) fn is_batch_mail_blocks(source_blocks: &[acp_v2::ContentBlock]) -> bool {
    match source_blocks.first() {
        Some(acp_v2::ContentBlock::Text(text)) => matches!(
            MailPromptEnvelope::parse_sections(&text.text),
            Some(sections) if sections.len() > 1
        ),
        _ => false,
    }
}

/// Parses `key="value"` pairs out of a tag's attribute text. Values must be
/// double-quoted and must not contain embedded quotes (mail addresses never
/// do); malformed pieces are skipped rather than failing the parse.
fn parse_tag_attributes(input: &str) -> HashMap<String, String> {
    let mut attributes = HashMap::default();
    let mut rest = input;
    while let Some(eq_ix) = rest.find('=') {
        let key = rest[..eq_ix].trim();
        let Some(value) = rest[eq_ix + 1..].strip_prefix('"') else {
            break;
        };
        let Some(value_end) = value.find('"') else {
            break;
        };
        if !key.is_empty() {
            attributes.insert(key.to_string(), value[..value_end].to_string());
        }
        rest = &value[value_end + 1..];
    }
    attributes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope_text(from: &str, session_id: &str, body: &str) -> String {
        format!(
            "<agent_message from=\"{from}\" session_id=\"{session_id}\">\n{body}\n</agent_message>\n{MAIL_PROMPT_TRAILER}"
        )
    }

    #[test]
    fn parses_single_envelope_with_trailer() {
        let text = envelope_text(
            "boss@amo via agent-mail, to *@amo",
            "boss@amo",
            "One bounded run: at most 5 checklist units.",
        );
        let parsed = MailPromptEnvelope::parse(&text).expect("should parse");

        assert_eq!(parsed.from.as_deref(), Some("boss@amo via agent-mail"));
        assert_eq!(parsed.to.as_deref(), Some("*@amo"));
        assert_eq!(parsed.session_id.as_deref(), Some("boss@amo"));
        assert!(!parsed.fyi);
        assert!(!parsed.urgent);
        assert_eq!(parsed.body, "One bounded run: at most 5 checklist units.");
        // Lossless splice: unchanged body round-trips byte-identically.
        assert_eq!(parsed.rewrap(&parsed.body), text);
        assert!(parsed.head.starts_with("<agent_message from="));
        assert!(parsed.tail.starts_with("\n</agent_message>"));
        assert_eq!(parsed.tail, "\n</agent_message>");
        assert!(parsed.trailer.ends_with(MAIL_PROMPT_TRAILER));
        // The raw form carries the envelope without the prompt-level trailer.
        assert_eq!(
            parsed.raw(),
            text.replace(&MAIL_PROMPT_TRAILER, "").trim_end()
        );
    }

    #[test]
    fn parses_fyi_and_urgent_variants() {
        // The envelope helper already appends the trailer, so the notice-prefixed
        // prompt must reuse that ending rather than appending a second one.
        let envelope = envelope_text(
            "boss@amo via agent-mail, to *@amo (fyi — no reply needed)",
            "boss@amo",
            "status report",
        );
        let text = format!("{MAIL_PROMPT_PREEMPT_NOTICE}\n{envelope}",);
        let parsed = MailPromptEnvelope::parse(&text).expect("should parse");
        assert!(parsed.urgent);
        assert!(parsed.fyi);
        assert_eq!(parsed.to.as_deref(), Some("*@amo"));
        assert_eq!(parsed.body, "status report");
        assert_eq!(parsed.rewrap(&parsed.body), text);
    }

    #[test]
    fn parses_without_trailer() {
        let text = "<agent_message from=\"a@b\" session_id=\"s\">\nhello\n</agent_message>\n";
        let parsed = MailPromptEnvelope::parse(text).expect("should parse");
        assert_eq!(parsed.body, "hello");
        assert_eq!(parsed.from.as_deref(), Some("a@b"));
        assert_eq!(parsed.to, None);
        assert_eq!(parsed.rewrap("hi"), text.replace("hello", "hi"));
    }

    #[test]
    fn rejects_multiple_envelopes_and_foreign_suffixes() {
        // A second envelope section must keep the prompt raw.
        let batch = format!(
            "{}{}",
            envelope_text("a@b", "s1", "first"),
            envelope_text("c@d", "s2", "second")
        );
        assert_eq!(MailPromptEnvelope::parse(&batch), None);

        // Unknown trailing content must keep the prompt raw.
        let text = format!(
            "{}\nSome other text the sender appended.",
            "<agent_message from=\"a@b\" session_id=\"s\">\nhello\n</agent_message>\n"
        );
        assert_eq!(MailPromptEnvelope::parse(&text), None);

        // Plain user prompts are not envelopes.
        assert_eq!(MailPromptEnvelope::parse("just a normal prompt"), None);
    }

    #[test]
    fn keeps_body_that_mentions_the_tag() {
        let text = envelope_text("a@b", "s", "the closing </agent_message> tag inside prose");
        let parsed = MailPromptEnvelope::parse(&text).expect("should parse");
        assert_eq!(parsed.body, "the closing </agent_message> tag inside prose");
        assert_eq!(parsed.rewrap(&parsed.body), text);
    }

    #[test]
    fn strips_envelope_from_source_blocks() {
        let blocks = vec![acp_v2::ContentBlock::Text(acp_v2::TextContent::new(
            envelope_text("a@b", "s", "hello"),
        ))];
        let stripped = strip_mail_envelope_blocks(&blocks);
        assert_eq!(stripped.len(), 1);
        let acp_v2::ContentBlock::Text(text) = &stripped[0] else {
            panic!("expected text block");
        };
        assert_eq!(text.text, "hello");

        let plain = vec![acp_v2::ContentBlock::Text(acp_v2::TextContent::new(
            "just a prompt",
        ))];
        assert_eq!(strip_mail_envelope_blocks(&plain), plain);
    }

    #[test]
    fn parses_batch_of_envelopes() {
        let text = format!(
            "<agent_message from=\"jobs@bmo via agent-mail, to zed-x@amo\" session_id=\"jobs@bmo\">\njob STARTED\n</agent_message>\n<agent_message from=\"zed-1@amo via agent-mail, to zed-x@amo\" session_id=\"zed-1@amo\">\njob DONE\nprobe-done\n\n</agent_message>\n{MAIL_PROMPT_TRAILER}"
        );
        let sections = MailPromptEnvelope::parse_sections(&text).expect("batch parses");
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].body, "job STARTED");
        assert_eq!(sections[1].body, "job DONE\nprobe-done\n");
        assert_eq!(sections[0].from.as_deref(), Some("jobs@bmo via agent-mail"));
        assert_eq!(sections[1].to.as_deref(), Some("zed-x@amo"));
        // Section raw forms are byte-exact slices of the prompt.
        assert!(text.starts_with(sections[0].raw().as_str()));
        assert!(text.contains(sections[1].raw().as_str()));
        // The whole prompt round-trips through the last section.
        assert_eq!(sections[1].rewrap(&sections[1].body), text);
        // `parse` remains single-envelope-only: batches render via sections.
        assert!(MailPromptEnvelope::parse(&text).is_none());

        // Display joins the bare bodies; the prompt counts as a batch.
        let blocks = vec![acp_v2::ContentBlock::Text(acp_v2::TextContent::new(text))];
        let stripped = strip_mail_envelope_blocks(&blocks);
        let acp_v2::ContentBlock::Text(display) = &stripped[0] else {
            panic!("expected text block");
        };
        assert_eq!(display.text, "job STARTED\n\njob DONE\nprobe-done\n");
        assert!(is_batch_mail_blocks(&blocks));
        assert!(!is_batch_mail_blocks(&[acp_v2::ContentBlock::Text(
            acp_v2::TextContent::new(envelope_text("a@b", "s", "hello"))
        )]));
    }

    #[test]
    fn batch_with_preempt_notice_marks_sections_urgent() {
        let text = format!(
            "{MAIL_PROMPT_PREEMPT_NOTICE}\n<agent_message from=\"a@b\" session_id=\"s1\">\nfirst\n</agent_message>\n<agent_message from=\"c@d\" session_id=\"s2\">\nsecond\n</agent_message>"
        );
        let sections = MailPromptEnvelope::parse_sections(&text).expect("batch parses");
        assert_eq!(sections.len(), 2);
        assert!(sections.iter().all(|section| section.urgent));
        assert_eq!(sections[0].body, "first");
        assert_eq!(sections[1].body, "second");
    }

    #[test]
    fn batch_keeps_body_that_mentions_the_tags() {
        let text = concat!(
            "<agent_message from=\"a@b\" session_id=\"s1\">\n",
            "the closing </agent_message> tag inside prose; an opening <agent_message from=\"x\" session_id=\"y\"> mention too\n",
            "</agent_message>\n",
            "<agent_message from=\"c@d\" session_id=\"s2\">\nsecond\n</agent_message>",
        );
        let sections = MailPromptEnvelope::parse_sections(text).expect("batch parses");
        assert_eq!(sections.len(), 2);
        assert_eq!(
            sections[0].body,
            "the closing </agent_message> tag inside prose; an opening <agent_message from=\"x\" session_id=\"y\"> mention too"
        );
        assert_eq!(sections[1].body, "second");
    }
}
