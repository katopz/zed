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
    /// The newline before the closing tag, the closing tag, and the known
    /// mailbox trailer (if present), byte-exact from the original.
    pub tail: String,
    /// The mail body shown in the editor.
    pub body: String,
}

impl MailPromptEnvelope {
    /// Parses a user-message text into its envelope. Returns `None` for
    /// anything that is not a known single-envelope mail prompt, which the
    /// caller must then render raw.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let mut rest = text;
        let mut urgent = false;
        if let Some(after) = rest.strip_prefix(MAIL_PROMPT_PREEMPT_NOTICE) {
            urgent = true;
            rest = after.strip_prefix('\n').unwrap_or(after);
        }

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
        let close_ix = after_tag.rfind(close_tag)?;
        // The writer puts the closing tag on its own line; requiring the
        // preceding newline keeps a body that merely mentions the tag intact.
        if !after_tag[..close_ix].ends_with('\n') {
            return None;
        }
        let body = after_tag[..close_ix - 1].to_string();
        // A batch prompt concatenates several envelope sections; a body that
        // itself contains an opening tag means the `rfind` above spanned such
        // a section, so keep the whole prompt raw instead of mis-merging the
        // envelopes into one header.
        if body.contains("<agent_message ") || body.contains("<agent_mail ") {
            return None;
        }
        let tail = after_tag[close_ix - 1..].to_string();

        // Everything after the closing tag must be exactly the known trailer
        // (plus whitespace). A second envelope section or any other trailing
        // content means this prompt was not built by `format_mailbox_prompt`
        // alone, so keep it raw rather than hide something unrecognized.
        let after_close = after_tag[close_ix + close_tag.len()..].trim();
        if !(after_close.is_empty() || after_close == MAIL_PROMPT_TRAILER) {
            return None;
        }

        let mut from = attributes.get("from").cloned();
        let mut to = attributes.get("to").cloned();
        let mut fyi = false;
        if to.is_none()
            && let Some(raw) = from.as_deref()
            && let Some(ix) = raw.find(FROM_TO_SEPARATOR)
        {
            let mut to_value = raw[ix + FROM_TO_SEPARATOR.len()..].to_string();
            if let Some(stripped) = to_value.strip_suffix(FYI_SUFFIX) {
                fyi = true;
                to_value = stripped.to_string();
            }
            from = Some(raw[..ix].to_string());
            to = Some(to_value);
        }

        Some(Self {
            from,
            to,
            session_id: attributes.get("session_id").cloned(),
            fyi,
            urgent,
            // `after_tag` is a suffix of `text`, so this span covers the
            // optional notice plus the opening tag line and its newline.
            head: text[..text.len() - after_tag.len()].to_string(),
            tail,
            body,
        })
    }

    /// Splices a (possibly edited) body back into the original prompt.
    pub(crate) fn rewrap(&self, body: &str) -> String {
        format!("{}{}{}", self.head, body, self.tail)
    }
}

/// The display form of a user message's source blocks: when the first text
/// block is a known mail envelope it is replaced by its bare body so the box
/// header can render the metadata as GUI chips instead of raw tag text.
pub(crate) fn strip_mail_envelope_blocks(
    source_blocks: &[acp_v2::ContentBlock],
) -> Vec<acp_v2::ContentBlock> {
    let Some(acp_v2::ContentBlock::Text(text)) = source_blocks.first() else {
        return source_blocks.to_vec();
    };
    let Some(envelope) = MailPromptEnvelope::parse(&text.text) else {
        return source_blocks.to_vec();
    };
    let mut display = Vec::with_capacity(source_blocks.len());
    let mut body_text = text.clone();
    body_text.text = envelope.body;
    display.push(acp_v2::ContentBlock::Text(body_text));
    display.extend(source_blocks.iter().skip(1).cloned());
    display
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
        format!("<agent_message from=\"{from}\" session_id=\"{session_id}\">\n{body}\n</agent_message>\n{MAIL_PROMPT_TRAILER}")
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
        assert!(parsed.tail.ends_with(MAIL_PROMPT_TRAILER));
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
        let text = format!(
            "{MAIL_PROMPT_PREEMPT_NOTICE}\n{envelope}",
        );
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
        let text = envelope_text(
            "a@b",
            "s",
            "the closing </agent_message> tag inside prose",
        );
        let parsed = MailPromptEnvelope::parse(&text).expect("should parse");
        assert_eq!(
            parsed.body,
            "the closing </agent_message> tag inside prose"
        );
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

        // Non-mail blocks pass through untouched.
        let plain = vec![acp_v2::ContentBlock::Text(acp_v2::TextContent::new(
            "just a prompt",
        ))];
        assert_eq!(strip_mail_envelope_blocks(&plain), plain);
    }
}
