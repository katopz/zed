//! `eval-cli --acp`: serves Zed's native agent as an ACP v1 agent over stdio,
//! so `zed-agentd attach -- eval-cli --acp` can run it detached on a remote
//! host (.issues/036, plan 037 P4).
//!
//! The SDK runs its handlers as `Send` futures off the main thread, while the
//! agent lives in gpui entities on the foreground. Handlers therefore only
//! forward [`Command`]s over a channel to one foreground loop. Outbound
//! `session/update`s come from diffing `AcpThread` entries against what was
//! already sent ([`UpdateCursor`]), so everything the GUI path does (tools,
//! terminals, retries) works unchanged.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::str::FromStr as _;
use std::sync::Arc;

use acp_thread::{
    AcpThread, AcpThreadEvent, AgentConnection as _, AgentModelList, AgentThreadEntry,
    AssistantMessageChunk, PermissionOptions, SelectedPermissionOutcome, ToolCall, ToolCallContent,
    ToolCallStatus,
};
use agent::{NativeAgent, NativeAgentConnection, Templates, ThreadStore};
use agent_client_protocol::schema::v1 as acp_v1;
use agent_client_protocol::schema::{ProtocolVersion, v2 as acp_v2};
use agent_client_protocol::{Agent, Client, ConnectionTo, Responder, Stdio};
use agent_settings::AgentSettings;
use anyhow::{Context as _, Result};
use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::{App, AppContext as _, AsyncApp, Entity, Subscription};
use language_model::{LanguageModelRegistry, SelectedModel};
use project::Project;
use serde::Serialize;
use serde::de::DeserializeOwned;
use settings::{LanguageModelSelection, Settings as _};
use util::ResultExt as _;
use util::path_list::PathList;

use crate::headless::AgentCliAppState;

pub(crate) struct ModelOptions<'a> {
    pub model_name: &'a str,
    pub thinking_override: Option<bool>,
    pub reasoning_effort: Option<&'a str>,
    pub openai_compatible_providers_json: Option<&'a str>,
    pub anthropic_available_models_json: Option<&'a str>,
}

enum Command {
    NewSession {
        request: acp_v1::NewSessionRequest,
        responder: Responder<acp_v1::NewSessionResponse>,
        client: ConnectionTo<Client>,
    },
    Prompt {
        request: acp_v1::PromptRequest,
        responder: Responder<acp_v1::PromptResponse>,
    },
    SetConfigOption {
        request: acp_v1::SetSessionConfigOptionRequest,
        responder: Responder<acp_v1::SetSessionConfigOptionResponse>,
    },
    Cancel(acp_v1::CancelNotification),
}

struct Session {
    thread: Entity<AcpThread>,
    state: Rc<RefCell<SessionState>>,
    _subscription: Subscription,
}

struct SessionState {
    session_id: acp_v1::SessionId,
    client: ConnectionTo<Client>,
    cursor: UpdateCursor,
}

/// Serves until the client closes stdin.
pub(crate) async fn serve(
    app_state: &Arc<AgentCliAppState>,
    model: ModelOptions<'_>,
    cx: &mut AsyncApp,
) -> Result<()> {
    let selected = SelectedModel::from_str(model.model_name).map_err(|e| anyhow::anyhow!("{e}"))?;
    crate::wait_for_model(&selected, cx).await?;
    cx.update(|cx| {
        crate::configure_agent_settings(
            &selected,
            model.model_name,
            model.thinking_override,
            model.reasoning_effort,
            model.openai_compatible_providers_json,
            model.anthropic_available_models_json,
            false,
            cx,
        )
    })?;

    let agent = cx.update(|cx| {
        let thread_store = cx.new(|cx| ThreadStore::new(cx));
        NativeAgent::new(thread_store, Templates::new(), app_state.fs.clone(), cx)
    });
    let connection = Rc::new(NativeAgentConnection(agent));

    let (commands, mut inbox) = mpsc::unbounded();
    let transport = cx.background_spawn(serve_stdio(commands));

    let sessions: Rc<RefCell<HashMap<acp_v1::SessionId, Session>>> = Rc::default();
    while let Some(command) = inbox.next().await {
        match command {
            Command::NewSession {
                request,
                responder,
                client,
            } => {
                let result =
                    new_session(app_state, &connection, &sessions, request, client, cx).await;
                match result {
                    Ok(response) => responder.respond(response),
                    Err(error) => responder.respond_with_error(internal_error(&error)),
                }
                .log_err();
            }
            Command::Prompt { request, responder } => {
                let Some((thread, state)) = sessions
                    .borrow()
                    .get(&request.session_id)
                    .map(|session| (session.thread.clone(), session.state.clone()))
                else {
                    responder
                        .respond_with_error(acp_v1::ErrorCode::InvalidParams.into())
                        .log_err();
                    continue;
                };
                let connection = connection.clone();
                // Not awaited: a cancel for this turn must reach the loop meanwhile.
                cx.spawn(async move |cx| {
                    let result = prompt(&connection, &thread, request, cx).await;
                    // Flush before answering so every update precedes the response.
                    cx.update(|cx| flush(&thread, &state, cx));
                    match result {
                        Ok(stop_reason) => {
                            responder.respond(acp_v1::PromptResponse::new(stop_reason))
                        }
                        Err(error) => responder.respond_with_error(internal_error(&error)),
                    }
                    .log_err();
                })
                .detach();
            }
            Command::SetConfigOption { request, responder } => {
                let state = sessions
                    .borrow()
                    .get(&request.session_id)
                    .map(|session| session.state.clone());
                let result = match state {
                    Some(state) => set_config_option(&connection, &state, request, cx).await,
                    None => Err(anyhow::anyhow!("unknown session {}", request.session_id)),
                };
                match result {
                    Ok(options) => {
                        responder.respond(acp_v1::SetSessionConfigOptionResponse::new(options))
                    }
                    Err(error) => responder.respond_with_error(internal_error(&error)),
                }
                .log_err();
            }
            Command::Cancel(notification) => {
                let thread = sessions
                    .borrow()
                    .get(&notification.session_id)
                    .map(|session| session.thread.clone());
                if let Some(thread) = thread {
                    thread.update(cx, |thread, cx| thread.cancel(cx)).detach();
                }
            }
        }
    }
    transport.await
}

async fn serve_stdio(commands: mpsc::UnboundedSender<Command>) -> Result<()> {
    let new_session = {
        let commands = commands.clone();
        async move |request: acp_v1::NewSessionRequest,
                    responder: Responder<acp_v1::NewSessionResponse>,
                    client: ConnectionTo<Client>| {
            forward(
                &commands,
                Command::NewSession {
                    request,
                    responder,
                    client,
                },
            );
            Ok(())
        }
    };
    let prompt = {
        let commands = commands.clone();
        async move |request: acp_v1::PromptRequest,
                    responder: Responder<acp_v1::PromptResponse>,
                    _client: ConnectionTo<Client>| {
            forward(&commands, Command::Prompt { request, responder });
            Ok(())
        }
    };
    let set_config_option = {
        let commands = commands.clone();
        async move |request: acp_v1::SetSessionConfigOptionRequest,
                    responder: Responder<acp_v1::SetSessionConfigOptionResponse>,
                    _client: ConnectionTo<Client>| {
            forward(&commands, Command::SetConfigOption { request, responder });
            Ok(())
        }
    };
    let cancel = {
        let commands = commands.clone();
        async move |notification: acp_v1::CancelNotification, _client: ConnectionTo<Client>| {
            forward(&commands, Command::Cancel(notification));
            Ok(())
        }
    };
    Agent
        .builder()
        .name("zed-native")
        .on_receive_request(
            async move |request: acp_v1::InitializeRequest,
                        responder: Responder<acp_v1::InitializeResponse>,
                        _client: ConnectionTo<Client>| {
                let version = request.protocol_version.min(ProtocolVersion::V1);
                responder.respond(
                    acp_v1::InitializeResponse::new(version)
                        .agent_capabilities(acp_v1::AgentCapabilities::new()),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(new_session, agent_client_protocol::on_receive_request!())
        .on_receive_request(prompt, agent_client_protocol::on_receive_request!())
        .on_receive_request(
            set_config_option,
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(cancel, agent_client_protocol::on_receive_notification!())
        .connect_to(Stdio::new())
        .await
        .map_err(|error| anyhow::anyhow!("ACP stdio transport: {error}"))
}

fn forward(commands: &mpsc::UnboundedSender<Command>, command: Command) {
    if let Err(error) = commands.unbounded_send(command) {
        // The foreground loop is gone; refuse instead of leaving the client hanging.
        match error.into_inner() {
            Command::NewSession { responder, .. } => {
                responder.respond_with_error(acp_v1::ErrorCode::InternalError.into())
            }
            Command::Prompt { responder, .. } => {
                responder.respond_with_error(acp_v1::ErrorCode::InternalError.into())
            }
            Command::SetConfigOption { responder, .. } => {
                responder.respond_with_error(acp_v1::ErrorCode::InternalError.into())
            }
            Command::Cancel(_) => Ok(()),
        }
        .log_err();
    }
}

fn internal_error(error: &anyhow::Error) -> agent_client_protocol::Error {
    agent_client_protocol::Error::internal_error().data(format!("{error:#}"))
}

async fn new_session(
    app_state: &Arc<AgentCliAppState>,
    connection: &Rc<NativeAgentConnection>,
    sessions: &Rc<RefCell<HashMap<acp_v1::SessionId, Session>>>,
    request: acp_v1::NewSessionRequest,
    client: ConnectionTo<Client>,
    cx: &mut AsyncApp,
) -> Result<acp_v1::NewSessionResponse> {
    let project = crate::open_local_project(app_state, &request.cwd, cx).await?;
    let thread = cx
        .update(|cx| {
            connection
                .clone()
                .new_session(project, PathList::new(&[request.cwd.as_path()]), cx)
        })
        .await
        .context("creating native agent session")?;
    let native_session_id = thread.read_with(cx, |thread, _| thread.session_id().clone());
    let session_id = acp_v1::SessionId::new(native_session_id.0.to_string());
    let config_options = model_config_options(connection, &native_session_id, cx).await;
    let state = Rc::new(RefCell::new(SessionState {
        session_id: session_id.clone(),
        client,
        cursor: UpdateCursor::default(),
    }));
    let subscription = cx.subscribe(&thread, {
        let state = state.clone();
        move |thread, event, cx| {
            if matches!(
                event,
                AcpThreadEvent::NewEntry | AcpThreadEvent::EntryUpdated(_)
            ) {
                flush(&thread, &state, cx);
            }
        }
    });
    sessions.borrow_mut().insert(
        session_id.clone(),
        Session {
            thread,
            state,
            _subscription: subscription,
        },
    );
    Ok(acp_v1::NewSessionResponse::new(session_id).config_options(config_options))
}

const MODEL_CONFIG_ID: &str = "model";

/// The native agent's model picker as an ACP select option; empty when the
/// agent lists no models, so clients simply show no picker.
async fn model_config_options(
    connection: &Rc<NativeAgentConnection>,
    session_id: &acp_v2::SessionId,
    cx: &mut AsyncApp,
) -> Vec<acp_v1::SessionConfigOption> {
    let Some(selector) = connection.model_selector(session_id) else {
        return Vec::new();
    };
    let (models, selected) =
        cx.update(|cx| (selector.list_models(cx), selector.selected_model(cx)));
    let (Some(models), Some(selected)) = (models.await.log_err(), selected.await.log_err()) else {
        return Vec::new();
    };
    let models = match models {
        AgentModelList::Flat(models) => models,
        AgentModelList::Grouped(groups) => groups.into_values().flatten().collect(),
    };
    let options = models
        .into_iter()
        .map(|model| {
            acp_v1::SessionConfigSelectOption::new(model.id.to_string(), model.name.to_string())
                .description(model.description.map(|description| description.to_string()))
        })
        .collect::<Vec<_>>();
    vec![
        acp_v1::SessionConfigOption::select(
            MODEL_CONFIG_ID,
            "Model",
            selected.id.to_string(),
            options,
        )
        .category(acp_v1::SessionConfigOptionCategory::Model),
    ]
}

/// Switches the session's model and announces the new options as a session
/// update too, so zed-agentd journals it and a reattaching client sees the
/// current model rather than the one from `session/new`.
async fn set_config_option(
    connection: &Rc<NativeAgentConnection>,
    state: &Rc<RefCell<SessionState>>,
    request: acp_v1::SetSessionConfigOptionRequest,
    cx: &mut AsyncApp,
) -> Result<Vec<acp_v1::SessionConfigOption>> {
    let acp_v1::SessionConfigOptionValue::ValueId { value } = &request.value else {
        anyhow::bail!("unsupported value for config option {}", request.config_id);
    };
    anyhow::ensure!(
        request.config_id.0.as_ref() == MODEL_CONFIG_ID,
        "unknown config option {}",
        request.config_id
    );
    let session_id = acp_v2::SessionId::new(request.session_id.0.to_string());
    cx.update(|cx| set_model(connection, &session_id, value.0.as_ref(), cx))?;
    let options = model_config_options(connection, &session_id, cx).await;
    let state = state.borrow();
    state
        .client
        .send_notification(acp_v1::SessionNotification::new(
            state.session_id.clone(),
            acp_v1::SessionUpdate::ConfigOptionUpdate(acp_v1::ConfigOptionUpdate::new(
                options.clone(),
            )),
        ))
        .log_err();
    Ok(options)
}

/// `NativeAgentModelSelector::select_model` minus its settings-file write: on
/// this host that write would persist into the real settings file, and the
/// reload replaces the in-memory provider settings `--acp` was started with.
fn set_model(
    connection: &NativeAgentConnection,
    session_id: &acp_v2::SessionId,
    model_id: &str,
    cx: &mut App,
) -> Result<()> {
    let thread = connection
        .thread(session_id, cx)
        .with_context(|| format!("unknown session {session_id}"))?;
    let model = LanguageModelRegistry::read_global(cx)
        .available_models(cx)
        .find(|model| format!("{}/{}", model.provider_id().0, model.id().0) == model_id)
        .with_context(|| format!("unknown model {model_id}"))?;
    let favorite = AgentSettings::get_global(cx)
        .favorite_models
        .iter()
        .find(|favorite| {
            favorite.provider.0 == model.provider_id().0.as_ref()
                && favorite.model == model.id().0.as_ref()
        })
        .cloned();
    let LanguageModelSelection {
        enable_thinking,
        effort,
        speed,
        ..
    } = agent_settings::language_model_to_selection(&model, favorite.as_ref());
    thread.update(cx, |thread, cx| {
        thread.set_model(model, cx);
        thread.set_thinking_effort(effort, cx);
        thread.set_thinking_enabled(enable_thinking, cx);
        if let Some(speed) = speed {
            thread.set_speed(speed, cx);
        }
    });
    Ok(())
}

async fn prompt(
    connection: &Rc<NativeAgentConnection>,
    thread: &Entity<AcpThread>,
    request: acp_v1::PromptRequest,
    cx: &mut AsyncApp,
) -> Result<acp_v1::StopReason> {
    let content = request
        .prompt
        .into_iter()
        .map(acp_thread::content::from_v1)
        .collect::<Result<Vec<_>>>()?;
    let stop_reason = cx
        .update(|cx| connection.send_and_wait_for_completion(thread.clone(), content, cx))
        .await?;
    Ok(match stop_reason {
        acp_v2::StopReason::Error(details) => {
            return Err(details
                .error
                .map(|error| anyhow::Error::new(*error))
                .unwrap_or_else(|| anyhow::anyhow!("agent stopped because of an error")));
        }
        acp_v2::StopReason::Cancelled => acp_v1::StopReason::Cancelled,
        acp_v2::StopReason::MaxTokens => acp_v1::StopReason::MaxTokens,
        acp_v2::StopReason::Refusal => acp_v1::StopReason::Refusal,
        _ => acp_v1::StopReason::EndTurn,
    })
}

/// Sends every not-yet-sent change, then asks the client about tool calls
/// that newly wait for permission.
fn flush(thread: &Entity<AcpThread>, state: &Rc<RefCell<SessionState>>, cx: &mut App) {
    let (updates, permissions) = {
        let mut state = state.borrow_mut();
        let updates = state.cursor.collect(thread.read(cx), cx);
        let permissions = state.cursor.take_permission_requests(thread.read(cx));
        (updates, permissions)
    };
    let state = state.borrow();
    for update in updates {
        state
            .client
            .send_notification(acp_v1::SessionNotification::new(
                state.session_id.clone(),
                update,
            ))
            .log_err();
    }
    for (tool_call_id, options) in permissions {
        request_permission(thread, &state, tool_call_id, options, cx);
    }
}

fn request_permission(
    thread: &Entity<AcpThread>,
    state: &SessionState,
    tool_call_id: acp_v2::ToolCallId,
    options: PermissionOptions,
    cx: &mut App,
) {
    let request = acp_v1::RequestPermissionRequest::new(
        state.session_id.clone(),
        acp_v1::ToolCallUpdate::new(
            acp_v1::ToolCallId::new(tool_call_id.0.to_string()),
            acp_v1::ToolCallUpdateFields::new(),
        ),
        flat_options(&options)
            .into_iter()
            .filter_map(|option| via_json(&option))
            .collect(),
    );
    let client = state.client.clone();
    let thread = thread.downgrade();
    cx.spawn(async move |cx| {
        let response = client.send_request(request).block_task().await;
        let selected = match response {
            Ok(response) => match response.outcome {
                acp_v1::RequestPermissionOutcome::Selected(selected) => options
                    .option_for_id(&acp_v2::PermissionOptionId::new(
                        selected.option_id.0.to_string(),
                    ))
                    .cloned(),
                _ => None,
            },
            Err(error) => {
                log::warn!("[eval-cli] permission request failed: {error}");
                None
            }
        };
        // A cancelled or failed prompt rejects, matching a dismissed GUI prompt.
        let Some(option) = selected.or_else(|| {
            options
                .first_option_of_kind(acp_v2::PermissionOptionKind::RejectOnce)
                .cloned()
        }) else {
            return;
        };
        thread
            .update(cx, |thread, cx| {
                thread.authorize_tool_call(
                    tool_call_id,
                    SelectedPermissionOutcome {
                        option_id: option.option_id,
                        option_kind: option.kind,
                        params: None,
                    },
                    cx,
                )
            })
            .log_err();
    })
    .detach();
}

/// Only the once-options: an "always" answer persists a rule to the host's
/// settings file, and the reload that follows drops the model and provider
/// settings this server injected in memory, failing the rest of the turn.
fn flat_options(options: &PermissionOptions) -> Vec<acp_v2::PermissionOption> {
    let all: Vec<acp_v2::PermissionOption> = match options {
        PermissionOptions::Flat(options) => options.clone(),
        PermissionOptions::Dropdown(choices)
        | PermissionOptions::DropdownWithPatterns { choices, .. } => choices
            .iter()
            .flat_map(|choice| [choice.allow.clone(), choice.deny.clone()])
            .collect(),
    };
    let mut once: Vec<acp_v2::PermissionOption> = Vec::new();
    for option in all {
        let is_once = matches!(
            option.kind,
            acp_v2::PermissionOptionKind::AllowOnce | acp_v2::PermissionOptionKind::RejectOnce
        );
        if is_once && !once.iter().any(|kept| kept.option_id == option.option_id) {
            once.push(option);
        }
    }
    once
}

/// v1 and v2 share wire names for the enums converted here; a variant v1
/// lacks fails to parse and the caller falls back.
fn via_json<T: Serialize, U: DeserializeOwned>(value: &T) -> Option<U> {
    serde_json::to_value(value)
        .and_then(serde_json::from_value)
        .ok()
}

/// What has already been sent for each thread entry, by entry index.
#[derive(Default)]
pub(crate) struct UpdateCursor {
    entries: Vec<SentEntry>,
}

#[derive(Default)]
enum SentEntry {
    #[default]
    Nothing,
    /// Source blocks sent, per assistant chunk.
    Assistant(Vec<usize>),
    ToolCall(SentToolCall),
}

#[derive(PartialEq)]
struct SentToolCall {
    status: ToolCallStatus,
    content_len: usize,
    asked_permission: bool,
}

impl UpdateCursor {
    pub(crate) fn collect(&mut self, thread: &AcpThread, cx: &App) -> Vec<acp_v1::SessionUpdate> {
        let entries = thread.entries();
        if self.entries.len() < entries.len() {
            self.entries.resize_with(entries.len(), SentEntry::default);
        }
        let mut updates = Vec::new();
        for (entry, sent) in entries.iter().zip(&mut self.entries) {
            match entry {
                AgentThreadEntry::AssistantMessage(message) => {
                    if !matches!(sent, SentEntry::Assistant(_)) {
                        *sent = SentEntry::Assistant(Vec::new());
                    }
                    let SentEntry::Assistant(sent_blocks) = sent else {
                        continue;
                    };
                    if sent_blocks.len() < message.chunks.len() {
                        sent_blocks.resize(message.chunks.len(), 0);
                    }
                    for (chunk, sent_count) in message.chunks.iter().zip(sent_blocks.iter_mut()) {
                        let (block, thought) = match chunk {
                            AssistantMessageChunk::Message { block, .. } => (block, false),
                            AssistantMessageChunk::Thought { block, .. } => (block, true),
                        };
                        let source = block.source_blocks();
                        for content in source.iter().skip(*sent_count) {
                            let Some(content) =
                                acp_thread::content::to_v1(content.clone()).log_err()
                            else {
                                continue;
                            };
                            let chunk = acp_v1::ContentChunk::new(content);
                            updates.push(if thought {
                                acp_v1::SessionUpdate::AgentThoughtChunk(chunk)
                            } else {
                                acp_v1::SessionUpdate::AgentMessageChunk(chunk)
                            });
                        }
                        *sent_count = (*sent_count).max(source.len());
                    }
                }
                AgentThreadEntry::ToolCall(call) => {
                    let current = SentToolCall {
                        status: call.status(),
                        content_len: call.content().len(),
                        asked_permission: false,
                    };
                    match sent {
                        SentEntry::ToolCall(previous) => {
                            if previous.status != current.status
                                || previous.content_len != current.content_len
                            {
                                updates.push(acp_v1::SessionUpdate::ToolCallUpdate(
                                    tool_call_update(call, thread.project(), cx),
                                ));
                                previous.status = current.status;
                                previous.content_len = current.content_len;
                            }
                        }
                        _ => {
                            updates.push(acp_v1::SessionUpdate::ToolCall(tool_call(
                                call,
                                thread.project(),
                                cx,
                            )));
                            *sent = SentEntry::ToolCall(current);
                        }
                    }
                }
                // The client sent the user messages itself; the rest are
                // GUI-only annotations with no ACP counterpart.
                _ => {}
            }
        }
        updates
    }

    /// Tool calls that wait for permission and have not been asked about yet.
    fn take_permission_requests(
        &mut self,
        thread: &AcpThread,
    ) -> Vec<(acp_v2::ToolCallId, PermissionOptions)> {
        let mut requests = Vec::new();
        for (entry, sent) in thread.entries().iter().zip(&mut self.entries) {
            let (AgentThreadEntry::ToolCall(call), SentEntry::ToolCall(sent)) = (entry, sent)
            else {
                continue;
            };
            if sent.asked_permission || call.status() != ToolCallStatus::WaitingForConfirmation {
                continue;
            }
            let Some(options) = thread
                .permission_request_for_tool(&call.id)
                .and_then(|request| request.legacy_options())
            else {
                continue;
            };
            sent.asked_permission = true;
            requests.push((call.id.clone(), options.clone()));
        }
        requests
    }
}

fn tool_call(call: &ToolCall, project: &Entity<Project>, cx: &App) -> acp_v1::ToolCall {
    let mut tool_call = acp_v1::ToolCall::new(
        acp_v1::ToolCallId::new(call.id.0.to_string()),
        call.label.read(cx).source().to_string(),
    )
    .status(tool_status(call.status()))
    .content(tool_content(call, project, cx));
    if let Some(kind) = via_json(call.kind()) {
        tool_call = tool_call.kind(kind);
    }
    if let Some(raw_input) = &call.raw_input {
        tool_call = tool_call.raw_input(raw_input.clone());
    }
    if let Some(raw_output) = &call.raw_output {
        tool_call = tool_call.raw_output(raw_output.clone());
    }
    tool_call
}

fn tool_call_update(
    call: &ToolCall,
    project: &Entity<Project>,
    cx: &App,
) -> acp_v1::ToolCallUpdate {
    let mut fields = acp_v1::ToolCallUpdateFields::new()
        .status(tool_status(call.status()))
        .content(tool_content(call, project, cx));
    if let Some(raw_output) = &call.raw_output {
        fields = fields.raw_output(raw_output.clone());
    }
    acp_v1::ToolCallUpdate::new(acp_v1::ToolCallId::new(call.id.0.to_string()), fields)
}

/// Edits go out as structured diffs; terminals and other content as markdown
/// text, since the terminals live on this host, not the client's.
fn tool_content(
    call: &ToolCall,
    project: &Entity<Project>,
    cx: &App,
) -> Vec<acp_v1::ToolCallContent> {
    call.content()
        .iter()
        .map(|content| match content {
            ToolCallContent::LegacyDiff { source, .. } => {
                acp_v1::ToolCallContent::Diff(source.clone())
            }
            ToolCallContent::Diff(diff) => {
                let diff = diff.read(cx);
                let path = diff.file_path(cx).unwrap_or_default();
                let path = absolute_path(project, &path, cx).unwrap_or_else(|| path.into());
                acp_v1::ToolCallContent::Diff(
                    acp_v1::Diff::new(path, diff.new_text(cx))
                        .old_text(diff.old_text().map(|text| text.to_string())),
                )
            }
            _ => acp_v1::ToolCallContent::Content(acp_v1::Content::new(
                acp_v1::ContentBlock::Text(acp_v1::TextContent::new(content.to_markdown(cx))),
            )),
        })
        .collect()
}

/// `acp_thread::Diff` paths are worktree-relative (or prefixed with the
/// worktree name); ACP wants absolute ones.
fn absolute_path(project: &Entity<Project>, path: &str, cx: &App) -> Option<PathBuf> {
    let project = project.read(cx);
    let project_path = project.find_project_path(path, cx)?;
    project.absolute_path(&project_path, cx)
}

fn tool_status(status: ToolCallStatus) -> acp_v1::ToolCallStatus {
    match status {
        ToolCallStatus::Pending | ToolCallStatus::WaitingForConfirmation => {
            acp_v1::ToolCallStatus::Pending
        }
        ToolCallStatus::InProgress => acp_v1::ToolCallStatus::InProgress,
        ToolCallStatus::Completed => acp_v1::ToolCallStatus::Completed,
        ToolCallStatus::Failed | ToolCallStatus::Rejected | ToolCallStatus::Canceled => {
            acp_v1::ToolCallStatus::Failed
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use acp_thread::StubAgentConnection;
    use fs::FakeFs;
    use gpui::TestAppContext;
    use project::Project;
    use settings::SettingsStore;
    use std::path::Path;

    fn text(update: &acp_v1::SessionUpdate) -> Option<(&'static str, String)> {
        let (kind, chunk) = match update {
            acp_v1::SessionUpdate::AgentMessageChunk(chunk) => ("message", chunk),
            acp_v1::SessionUpdate::AgentThoughtChunk(chunk) => ("thought", chunk),
            _ => return None,
        };
        match &chunk.content {
            acp_v1::ContentBlock::Text(content) => Some((kind, content.text.clone())),
            _ => None,
        }
    }

    #[gpui::test]
    async fn cursor_sends_each_thread_change_exactly_once(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let mut settings_store = SettingsStore::test(cx);
            settings_store.register_setting::<feature_flags::FeatureFlagsSettings>();
            cx.set_global(settings_store);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(util::path!("/project"), serde_json::json!({}))
            .await;
        let project = Project::test(fs, [Path::new(util::path!("/project"))], cx).await;
        let connection = Rc::new(StubAgentConnection::new());
        let thread = cx
            .update(|cx| {
                connection.clone().new_session(
                    project,
                    PathList::new(&[Path::new(util::path!("/project"))]),
                    cx,
                )
            })
            .await
            .expect("stub session");
        let session_id = thread.read_with(cx, |thread, _| thread.session_id().clone());
        let send = |update: acp_v1::SessionUpdate, cx: &mut TestAppContext| {
            cx.update(|cx| connection.send_update(session_id.clone(), update, cx));
        };
        let mut cursor = UpdateCursor::default();
        let mut collect =
            |cx: &mut TestAppContext| thread.read_with(cx, |thread, cx| cursor.collect(thread, cx));

        send(
            acp_v1::SessionUpdate::AgentMessageChunk(acp_v1::ContentChunk::new("Hel".into())),
            cx,
        );
        let updates = collect(cx);
        assert_eq!(
            updates.iter().filter_map(text).collect::<Vec<_>>(),
            vec![("message", "Hel".to_string())]
        );

        send(
            acp_v1::SessionUpdate::AgentMessageChunk(acp_v1::ContentChunk::new("lo".into())),
            cx,
        );
        send(
            acp_v1::SessionUpdate::AgentThoughtChunk(acp_v1::ContentChunk::new("hm".into())),
            cx,
        );
        let updates = collect(cx);
        assert_eq!(
            updates.iter().filter_map(text).collect::<Vec<_>>(),
            vec![("message", "lo".to_string()), ("thought", "hm".to_string())]
        );
        assert!(collect(cx).is_empty(), "nothing new, nothing sent");

        send(
            acp_v1::SessionUpdate::ToolCall(
                acp_v1::ToolCall::new("t1", "Read file").status(acp_v1::ToolCallStatus::Pending),
            ),
            cx,
        );
        let updates = collect(cx);
        let [acp_v1::SessionUpdate::ToolCall(call)] = updates.as_slice() else {
            panic!("expected one tool_call, got {updates:?}");
        };
        assert_eq!(call.tool_call_id.0.as_ref(), "t1");
        assert_eq!(call.title, "Read file");

        send(
            acp_v1::SessionUpdate::ToolCallUpdate(acp_v1::ToolCallUpdate::new(
                "t1",
                acp_v1::ToolCallUpdateFields::new()
                    .status(acp_v1::ToolCallStatus::Completed)
                    .content(vec!["file body".into()]),
            )),
            cx,
        );
        let updates = collect(cx);
        let [acp_v1::SessionUpdate::ToolCallUpdate(update)] = updates.as_slice() else {
            panic!("expected one tool_call_update, got {updates:?}");
        };
        assert_eq!(
            update.fields.status,
            Some(acp_v1::ToolCallStatus::Completed)
        );
        assert_eq!(update.fields.content.as_ref().map(Vec::len), Some(1));
        assert!(collect(cx).is_empty(), "tool call already up to date");

        let diff =
            acp_v1::Diff::new(util::path!("/project/a.rs"), "new").old_text("old".to_string());
        send(
            acp_v1::SessionUpdate::ToolCall(
                acp_v1::ToolCall::new("t2", "Edit a.rs")
                    .status(acp_v1::ToolCallStatus::Completed)
                    .content(vec![acp_v1::ToolCallContent::Diff(diff.clone())]),
            ),
            cx,
        );
        let updates = collect(cx);
        let [acp_v1::SessionUpdate::ToolCall(call)] = updates.as_slice() else {
            panic!("expected one tool_call, got {updates:?}");
        };
        assert_eq!(
            call.content,
            vec![acp_v1::ToolCallContent::Diff(diff)],
            "edits go out as structured diffs, not markdown"
        );
    }
}
