//! Elicititation auto-answer: keeps auto-prompt chains moving when a worker
//! blocks on a decision form (`ask_user` / ACP session elicitation).
//!
//! A thread awaiting an elicitation never stops, so `on_thread_stopped` and
//! every other auto-prompt recovery path is unreachable — the job is stuck
//! until a human answers. When auto-prompt is enabled for the thread:
//!
//! 1. Primary: an LLM reasoning call picks the best option from the form and
//!    writes a one-line rationale. When it returns before the countdown, the
//!    response carries the picked option (verbatim, via the form's enum
//!    field) plus the rationale in the free-text field — the worker sees
//!    "Option — rationale" per the other>choice precedence.
//! 2. Backstop: if reasoning fails, is unconfident, or is slower than the
//!    countdown, the FIRST option is auto-selected at the deadline (the
//!    recommended default). Forms with no options are Declined so the worker
//!    gets a clear error and can recover instead of blocking forever.
//!
//! Url-mode elicitations (browser auth flows) are never auto-answered.
//! Responses are idempotent: if the user answers first, the store drops the
//! duplicate response.

use acp_thread::{AcpThread, ElicitationEntryId};
use agent_client_protocol::schema::v1 as acp;
use anyhow::Context as _;
use futures::{StreamExt, future, pin_mut};
use language_model::{
    LanguageModel, LanguageModelCompletionEvent, LanguageModelRequest,
    LanguageModelRequestMessage, Role,
};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::ConversationView;

/// Field key the `ask_user` tool uses for free-text answers. Used to prefer
/// the canonical free-text field when parsing generic form schemas.
const OTHER_FIELD: &str = "other";

/// Deadline registry for the visible countdown. Keyed by elicitation entry
/// id (string); armed in `arm_if_enabled`, cleared on
/// `AcpThreadEvent::ElicitationResponded`. The card reads it on every frame
/// (the countdown label is animation-driven) so lookups must be cheap.
static AUTO_ANSWER_DEADLINES: std::sync::RwLock<Option<std::collections::HashMap<String, Instant>>> =
    std::sync::RwLock::new(None);

pub(crate) fn set_deadline(elicitation_id: &ElicitationEntryId, deadline: Instant) {
    let mut guard = AUTO_ANSWER_DEADLINES
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    guard
        .get_or_insert_with(Default::default)
        .insert(elicitation_id.0.to_string(), deadline);
}

pub(crate) fn clear_deadline(elicitation_id: &ElicitationEntryId) {
    if let Ok(mut guard) = AUTO_ANSWER_DEADLINES.write() {
        if let Some(map) = guard.as_mut() {
            map.remove(elicitation_id.0.as_ref());
        }
    }
}

pub(crate) fn deadline_for(elicitation_id: &ElicitationEntryId) -> Option<Instant> {
    let guard = AUTO_ANSWER_DEADLINES
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    guard
        .as_ref()
        .and_then(|map| map.get(elicitation_id.0.as_ref()).copied())
}

/// The visible countdown text for an armed elicitation. Pure (takes `now`)
/// so the card can recompute it every animation frame without hidden state.
/// `None` when no auto-answer is armed for this elicitation.
pub(crate) fn countdown_text(
    elicitation_id: &ElicitationEntryId,
    now: Instant,
) -> Option<String> {
    let deadline = deadline_for(elicitation_id)?;
    let remaining = deadline.saturating_duration_since(now);
    Some(if remaining.is_zero() {
        "auto-answering…".to_string()
    } else {
        format!("auto-answer in {}s", remaining.as_secs().max(1))
    })
}

/// The parsed, answerable shape of a form elicitation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ElicitationQuestion {
    pub message: String,
    /// Verbatim option labels (enum values) of the single-select field.
    pub options: Vec<String>,
    /// The schema field carrying the options (ask_user: "choice"; other
    /// agents may name it differently — any enum-valued string field works).
    choice_field: Option<String>,
    /// A free-text field exists (ask_user: "other").
    free_text_field: Option<String>,
}

/// Extract the question + options from a form elicitation request.
///
/// Generic across schema field names: the first enum-valued string property
/// is the options field; any non-enum string property counts as free text.
/// Returns None for Url mode and forms without any string properties.
pub(crate) fn extract_question(
    request: &acp::CreateElicitationRequest,
) -> Option<ElicitationQuestion> {
    let acp::ElicitationMode::Form(form) = &request.mode else {
        return None;
    };

    let mut options: Vec<String> = Vec::new();
    let mut choice_field: Option<String> = None;
    let mut free_text_field: Option<String> = None;

    for (field, property) in &form.requested_schema.properties {
        let acp::ElicitationPropertySchema::String(string_schema) = property else {
            continue;
        };
        let enum_labels: Vec<String> = string_schema
            .one_of
            .as_ref()
            .map(|options| options.iter().map(|option| option.value.clone()).collect::<Vec<_>>())
            .unwrap_or_default()
            .into_iter()
            .chain(string_schema.enum_values.clone().unwrap_or_default())
            .collect();
        if !enum_labels.is_empty() && choice_field.is_none() {
            choice_field = Some(field.clone());
            options = enum_labels;
        } else if field == OTHER_FIELD || free_text_field.is_none() {
            free_text_field = Some(field.clone());
        }
    }

    if choice_field.is_none() && free_text_field.is_none() {
        return None;
    }

    Some(ElicitationQuestion {
        message: request.message.clone(),
        options,
        choice_field,
        free_text_field,
    })
}

/// Build the Accept response for a picked option, with the rationale riding
/// in the free-text field when the schema has one.
///
/// `choice_field` is required — auto-answering only picks options, it never
/// fabricates free text.
fn build_accept_response(
    question: &ElicitationQuestion,
    choice: &str,
    rationale: Option<&str>,
) -> acp::CreateElicitationResponse {
    let mut content: BTreeMap<String, acp::ElicitationContentValue> = BTreeMap::new();
    if let Some(field) = &question.choice_field {
        content.insert(field.clone(), acp::ElicitationContentValue::String(choice.to_string()));
    }
    if let (Some(field), Some(rationale)) = (question.free_text_field.as_deref(), rationale) {
        let combined = if rationale.contains(choice) {
            rationale.to_string()
        } else {
            format!("{choice} — {rationale}")
        };
        content.insert(field.to_string(), acp::ElicitationContentValue::String(combined));
    }
    acp::CreateElicitationResponse::new(acp::ElicitationAction::Accept(
        acp::ElicitationAcceptAction::new().content(content),
    ))
}

/// The backstop response when reasoning fails: the first option (the
/// recommended default), or Decline when the form has no options to pick.
fn build_fallback_response(question: &ElicitationQuestion) -> acp::CreateElicitationResponse {
    match question.options.first() {
        Some(first) => build_accept_response(question, first, Some("auto-selected default (countdown expired)")),
        None => acp::CreateElicitationResponse::new(acp::ElicitationAction::Decline),
    }
}

const ELICITATION_ANSWERER_SYSTEM_PROMPT: &str = r#"You answer decision questions on behalf of an autonomous coding agent's operator. The worker agent is blocked waiting for a human to pick an option; your answer keeps the chain moving.

Rules:
1. Pick the option that is most defensible for the work described in the question. Prefer the safer / more conservative option when unsure (keep existing behaviour, don't delete data, don't force-push).
2. If the question is genuinely unanswerable without the human (credentials, legal approval, destructive irreversible action), return {"choice": null}.
3. The choice MUST be one of the exact option texts, verbatim.

Respond with a SINGLE JSON object, no prose, no markdown fences:
{"choice": "<exact option text>", "reason": "<one line why>"}
or {"choice": null}"#;

#[derive(Debug, serde::Deserialize)]
struct ReasonedAnswer {
    choice: Option<String>,
    #[serde(default)]
    reason: Option<String>,
}

fn parse_reasoned_answer(text: &str) -> Option<ReasonedAnswer> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    let parsed: ReasonedAnswer = serde_json::from_str(&text[start..=end]).ok()?;
    let choice = parsed.choice?;
    if choice.trim().is_empty() {
        return None;
    }
    Some(ReasonedAnswer {
        choice: Some(choice),
        reason: parsed.reason,
    })
}

/// Answerer context: the worker's last assistant message, when cheaply
/// available. Assistant blocks render from `Entity<Markdown>` with no cheap
/// text accessor, so this is currently empty — the answerer's safety rules
/// (prefer the conservative option) cover the gap. Kept as a seam so a future
/// change can thread real context in without touching the orchestration.
fn last_assistant_text(_thread: &AcpThread) -> String {
    String::new()
}

async fn call_answerer(
    model: &Arc<dyn LanguageModel>,
    question: &ElicitationQuestion,
    context: &str,
    cx: &gpui::AsyncApp,
) -> anyhow::Result<ReasonedAnswer> {
    let options_list = if question.options.is_empty() {
        "(no fixed options — but this path only runs when options exist)".to_string()
    } else {
        question
            .options
            .iter()
            .map(|option| format!("- {option}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let user_turn = format!(
        "Question from the worker:\n{}\n\nOptions:\n{}\n\n--- worker's last message ---\n{}\n--- end ---",
        question.message, options_list, context
    );

    let request = LanguageModelRequest {
        messages: vec![
            LanguageModelRequestMessage {
                role: Role::System,
                content: vec![ELICITATION_ANSWERER_SYSTEM_PROMPT.into()],
                cache: false,
                reasoning_details: None,
            },
            LanguageModelRequestMessage {
                role: Role::User,
                content: vec![user_turn.into()],
                cache: false,
                reasoning_details: None,
            },
        ],
        ..Default::default()
    };

    let completion_future = async {
        let mut stream = model
            .stream_completion(request, cx)
            .await
            .context("elicitation answerer: failed to start completion stream")?;
        let mut text_parts: Vec<String> = Vec::new();
        while let Some(event) = stream.next().await {
            match event {
                Ok(LanguageModelCompletionEvent::Text(text)) => text_parts.push(text),
                Ok(_) => {}
                Err(err) => anyhow::bail!("elicitation answerer stream error: {err:#}"),
            }
        }
        let text = text_parts.concat();
        anyhow::Ok(text)
    };

    let timeout_future = cx.background_executor().timer(Duration::from_secs(45));
    pin_mut!(completion_future, timeout_future);
    match future::select(completion_future, timeout_future).await {
        future::Either::Left((Ok(text), _)) => parse_reasoned_answer(&text)
            .context("elicitation answerer: response was not a valid choice JSON"),
        future::Either::Left((Err(err), _)) => Err(err),
        future::Either::Right(_) => {
            anyhow::bail!("elicitation answerer: timed out after 45 seconds")
        }
    }
}

/// Arm the auto-answer for a pending elicitation on a thread whose
/// auto-prompt is enabled. No-op when the feature is disabled in config, the
/// elicitation is not an answerable form, or there is nothing to answer.
pub fn arm_if_enabled(
    conversation_view: &ConversationView,
    thread: &gpui::Entity<AcpThread>,
    elicitation_id: &ElicitationEntryId,
    cx: &mut gpui::Context<ConversationView>,
) {
    let config = auto_prompt::load_config_cached().unwrap_or_default();
    if !config.elicitation_auto_answer_enabled {
        return;
    }

    let auto_prompt_enabled = conversation_view
        .active_thread()
        .is_some_and(|thread_view| thread_view.read(cx).auto_prompt_enabled);
    if !auto_prompt_enabled {
        return;
    }

    let question = thread
        .read(cx)
        .elicitation(elicitation_id)
        .map(|(_, elicitation)| elicitation)
        .and_then(|elicitation| extract_question(&elicitation.request));
    let Some(question) = question else {
        return;
    };

    let model = language_model::LanguageModelRegistry::read_global(cx)
        .default_model()
        .map(|configured| configured.model);
    let context = last_assistant_text(thread.read(cx));
    let countdown = Duration::from_secs(config.elicitation_countdown_secs.max(1));
    // Register the deadline BEFORE spawning so the countdown label renders
    // on the card's very next frame, not one countdown late.
    set_deadline(elicitation_id, Instant::now() + countdown);
    let elicitation_id = elicitation_id.clone();
    let thread = thread.clone();

    cx.spawn(async move |_view, cx| {
        let background = cx.background_executor().clone();
        // The coroutine captures a shared `cx` reborrow plus cloned data; it is
        // dropped before the `thread.update` below so the mutable borrow is
        // free again (pinned futures otherwise live to scope end).
        let answerer = {
            let cx: &gpui::AsyncApp = cx;
            let question = question.clone();
            let context = context.clone();
            let model = model.clone();
            async move {
                let model = model?;
                call_answerer(&model, &question, &context, cx).await.ok()
            }
        };
        let countdown_future = background.timer(countdown);

        let response = {
            pin_mut!(answerer, countdown_future);
            match future::select(answerer, countdown_future).await {
                // Reasoning won within the countdown: respond with the LLM's pick
                // (validated against the exact option texts).
                future::Either::Left((Some(answer), _)) => {
                    let choice = answer.choice.unwrap_or_default();
                    if question.options.contains(&choice) {
                        log::warn!(
                            "[auto_prompt::elicitation_auto_answer] Auto-answered '{}': {}",
                            question.message,
                            choice
                        );
                        build_accept_response(&question, &choice, answer.reason.as_deref())
                    } else {
                        log::warn!(
                            "[auto_prompt::elicitation_auto_answer] Reasoner picked unknown option '{choice}' — falling back to first option"
                        );
                        build_fallback_response(&question)
                    }
                }
                // Reasoning failed/absent: hold the line until the countdown,
                // then auto-select the first option.
                future::Either::Left((None, countdown_future)) => {
                    countdown_future.await;
                    log::warn!(
                        "[auto_prompt::elicitation_auto_answer] Countdown expired without a reasoned answer — auto-selecting first option"
                    );
                    build_fallback_response(&question)
                }
                // Countdown expired while reasoning was still running: don't
                // wait longer; the job is stuck meanwhile.
                future::Either::Right(_) => {
                    log::warn!(
                        "[auto_prompt::elicitation_auto_answer] Countdown expired before reasoning finished — auto-selecting first option"
                    );
                    build_fallback_response(&question)
                }
            }
        };

        thread.update(cx, |thread, cx| {
            thread.respond_to_elicitation(&elicitation_id, response, cx);
        });
        log::info!(
            "[auto_prompt::elicitation_auto_answer] Auto-answer dispatched for elicitation {elicitation_id:?}"
        );
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Field key the `ask_user` tool uses for its single-select options.
    const CHOICE_FIELD: &str = "choice";

    fn ask_user_request(options: &[&str], allow_free_text: bool) -> acp::CreateElicitationRequest {
        let mut schema = acp::ElicitationSchema::new();
        if !options.is_empty() {
            let enum_options: Vec<acp::EnumOption> = options
                .iter()
                .map(|label| acp::EnumOption::new(label.to_string(), label.to_string()))
                .collect();
            schema = schema.property(
                CHOICE_FIELD,
                acp::StringPropertySchema::new()
                    .title("Choose an option")
                    .one_of(enum_options),
                !allow_free_text,
            );
        }
        if allow_free_text {
            schema = schema.property(
                OTHER_FIELD,
                acp::StringPropertySchema::new().title("Or type your own answer"),
                options.is_empty(),
            );
        }
        acp::CreateElicitationRequest::new(
            acp::ElicitationFormMode::new(
                acp::ElicitationSessionScope::new(acp::SessionId::new("test-session")),
                schema,
            ),
            "Which approach?",
        )
    }

    #[test]
    fn test_extract_question_ask_user_shape() {
        let question = extract_question(&ask_user_request(&["Approach A", "Approach B"], true))
            .expect("ask_user form should parse");
        assert_eq!(question.message, "Which approach?");
        assert_eq!(question.options, vec!["Approach A", "Approach B"]);
        assert_eq!(question.choice_field.as_deref(), Some("choice"));
        assert_eq!(question.free_text_field.as_deref(), Some("other"));
        assert!(question.free_text_field.is_some());
    }

    #[test]
    fn test_extract_question_no_free_text() {
        let question = extract_question(&ask_user_request(&["Yes", "No"], false))
            .expect("options-only form should parse");
        assert!(question.free_text_field.is_none());
        assert_eq!(question.options.len(), 2);
    }

    #[test]
    fn test_extract_question_free_text_only() {
        let question =
            extract_question(&ask_user_request(&[], true)).expect("free-text form should parse");
        assert!(question.options.is_empty());
        assert!(question.free_text_field.is_some());
    }

    #[test]
    fn test_extract_question_generic_enum_field_name() {
        // Non-ask_user agents may name the enum field differently.
        let mut schema = acp::ElicitationSchema::new();
        schema = schema.property(
            "strategy",
            acp::StringPropertySchema::new().one_of(vec![
                acp::EnumOption::new("S1", "First"),
                acp::EnumOption::new("S2", "Second"),
            ]),
            true,
        );
        let request = acp::CreateElicitationRequest::new(
            acp::ElicitationFormMode::new(
                acp::ElicitationSessionScope::new(acp::SessionId::new("s")),
                schema,
            ),
            "Pick",
        );
        let question = extract_question(&request).expect("generic form should parse");
        assert_eq!(question.options, vec!["S1", "S2"]);
        assert_eq!(question.choice_field.as_deref(), Some("strategy"));
    }

    #[test]
    fn test_build_accept_response_fields() {
        let question = extract_question(&ask_user_request(&["Approach A", "Approach B"], true))
            .unwrap();
        let response = build_accept_response(&question, "Approach A", Some("safer, keeps behaviour"));
        let acp::ElicitationAction::Accept(accept) = &response.action else {
            panic!("expected Accept");
        };
        let content = accept.content.as_ref().unwrap();
        assert_eq!(
            content.get("choice"),
            Some(&acp::ElicitationContentValue::String("Approach A".into()))
        );
        assert_eq!(
            content.get("other"),
            Some(&acp::ElicitationContentValue::String(
                "Approach A — safer, keeps behaviour".into()
            ))
        );
    }

    #[test]
    fn test_build_fallback_response_first_option() {
        let question = extract_question(&ask_user_request(&["Approach A", "Approach B"], false))
            .unwrap();
        let response = build_fallback_response(&question);
        let acp::ElicitationAction::Accept(accept) = &response.action else {
            panic!("expected Accept");
        };
        let content = accept.content.as_ref().unwrap();
        assert_eq!(
            content.get("choice"),
            Some(&acp::ElicitationContentValue::String("Approach A".into()))
        );
    }

    #[test]
    fn test_build_fallback_response_no_options_declines() {
        let question = extract_question(&ask_user_request(&[], true)).unwrap();
        let response = build_fallback_response(&question);
        assert!(matches!(
            response.action,
            acp::ElicitationAction::Decline
        ));
    }

    #[test]
    fn test_parse_reasoned_answer_valid() {
        let answer =
            parse_reasoned_answer(r#"{"choice": "Approach A", "reason": "safer"}"#).unwrap();
        assert_eq!(answer.choice.as_deref(), Some("Approach A"));
        assert_eq!(answer.reason.as_deref(), Some("safer"));
    }

    #[test]
    fn test_parse_reasoned_answer_null_choice_is_none() {
        assert!(parse_reasoned_answer(r#"{"choice": null}"#).is_none());
    }

    #[test]
    fn test_parse_reasoned_answer_fenced() {
        let answer =
            parse_reasoned_answer("```json\n{\"choice\": \"No\", \"reason\": \"r\"}\n```").unwrap();
        assert_eq!(answer.choice.as_deref(), Some("No"));
    }

    #[test]
    fn test_parse_reasoned_answer_garbage_is_none() {
        assert!(parse_reasoned_answer("I think Approach A").is_none());
    }

    #[test]
    fn test_countdown_text_formats_remaining_seconds() {
        let id = ElicitationEntryId("countdown-test".into());
        let now = Instant::now();
        // Not armed → no label.
        assert_eq!(countdown_text(&id, now), None);

        set_deadline(&id, now + Duration::from_secs(42));
        assert_eq!(
            countdown_text(&id, now),
            Some("auto-answer in 42s".to_string())
        );
        // Sub-second remaining clamps up to 1s (never shows 0s).
        assert_eq!(
            countdown_text(&id, now + Duration::from_millis(41_500)),
            Some("auto-answer in 1s".to_string())
        );
        // Past the deadline → answering.
        assert_eq!(
            countdown_text(&id, now + Duration::from_secs(43)),
            Some("auto-answering…".to_string())
        );

        clear_deadline(&id);
        assert_eq!(countdown_text(&id, now), None);
    }
}
