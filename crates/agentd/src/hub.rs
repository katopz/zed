//! Routing state machine between detachable ACP clients and long-lived ACP
//! backends. Pure: IO lives in `daemon`, so every routing rule is unit-testable.

use crate::journal::{JournalStore, coalesce_chunks};
use crate::types::{
    BackendSpec, ClientId, DaemonStatus, DetachedPolicy, HubEvent, HubOutput, SessionRecord,
    SessionStatus,
};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::time::SystemTime;

const METHOD_NOT_FOUND: i64 = -32601;
const INTERNAL_ERROR: i64 = -32603;
const TITLE_MAX_CHARS: usize = 80;

enum InitState {
    NotStarted,
    Waiting(Vec<(ClientId, Value)>),
    Ready(Value),
}

enum RequestKind {
    NewSession {
        cwd: PathBuf,
    },
    LoadSession {
        session_id: String,
    },
    /// Re-establishes backend state for a journaled session after the backend
    /// restarted; the client was already answered from the journal.
    RestoreSession {
        session_id: String,
    },
    Prompt {
        session_id: String,
    },
    Other,
}

enum PendingRequest {
    Initialize,
    Client {
        client: ClientId,
        client_request_id: Value,
        kind: RequestKind,
    },
}

struct BackendState {
    spec: BackendSpec,
    generation: u64,
    running: bool,
    init: InitState,
    next_id: u64,
    pending: HashMap<u64, PendingRequest>,
    detached_policy: DetachedPolicy,
}

/// A backend → client request currently handed to one client.
struct ForwardedRequest {
    backend_key: String,
    session_id: Option<String>,
    message: Value,
}

struct ClientState {
    backend_key: String,
    next_id: u64,
    forwarded: HashMap<u64, ForwardedRequest>,
}

struct SessionState {
    backend_key: String,
    /// Backend generation that holds this session's in-memory state; a mismatch
    /// means the backend restarted and the session only exists in the journal.
    generation: Option<u64>,
    restoring: bool,
    attached: Vec<ClientId>,
    queued: VecDeque<(String, Value)>,
    record: SessionRecord,
}

pub struct Hub<J: JournalStore> {
    journal: J,
    backends: HashMap<String, BackendState>,
    clients: HashMap<ClientId, ClientState>,
    sessions: HashMap<String, SessionState>,
    output: Vec<HubOutput>,
}

impl<J: JournalStore> Hub<J> {
    pub fn new(journal: J) -> Self {
        let mut sessions = HashMap::new();
        match journal.load_records() {
            Ok(records) => {
                for record in records {
                    sessions.insert(
                        record.session_id.clone(),
                        SessionState {
                            backend_key: record.backend_key.clone(),
                            generation: None,
                            restoring: false,
                            attached: Vec::new(),
                            queued: VecDeque::new(),
                            record,
                        },
                    );
                }
            }
            Err(error) => eprintln!("agentd: failed to load journal: {error:#}"),
        }
        Self {
            journal,
            backends: HashMap::new(),
            clients: HashMap::new(),
            sessions,
            output: Vec::new(),
        }
    }

    pub fn handle(&mut self, event: HubEvent) -> Vec<HubOutput> {
        match event {
            HubEvent::ClientConnected {
                client,
                backend,
                detached_policy,
            } => self.client_connected(client, backend, detached_policy),
            HubEvent::ClientMessage { client, message } => self.client_message(client, message),
            HubEvent::ClientDisconnected { client } => self.client_disconnected(client),
            HubEvent::BackendMessage {
                backend_key,
                generation,
                message,
            } => {
                if self.is_current(&backend_key, generation) {
                    self.backend_message(&backend_key, message);
                }
            }
            HubEvent::BackendExited {
                backend_key,
                generation,
            } => {
                if self.is_current(&backend_key, generation) {
                    self.backend_exited(&backend_key);
                }
            }
        }
        std::mem::take(&mut self.output)
    }

    pub fn status(&self) -> DaemonStatus {
        let mut sessions: Vec<SessionStatus> = self
            .sessions
            .values()
            .map(|session| SessionStatus {
                session_id: session.record.session_id.clone(),
                title: session.record.title.clone(),
                cwd: session.record.cwd.clone(),
                updated_at: session.record.updated_at.clone(),
                live: self.is_live(session),
                attached_clients: session.attached.len(),
                queued_requests: session.queued.len(),
            })
            .collect();
        sessions.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
        DaemonStatus {
            pid: std::process::id(),
            clients: self.clients.len(),
            backends: self
                .backends
                .values()
                .filter(|backend| backend.running)
                .map(|backend| backend.spec.key().replace('\u{1f}', " "))
                .collect(),
            sessions,
        }
    }

    fn is_current(&self, backend_key: &str, generation: u64) -> bool {
        self.backends
            .get(backend_key)
            .is_some_and(|backend| backend.running && backend.generation == generation)
    }

    fn is_live(&self, session: &SessionState) -> bool {
        match (session.generation, self.backends.get(&session.backend_key)) {
            (Some(generation), Some(backend)) => {
                backend.running && backend.generation == generation
            }
            _ => false,
        }
    }

    fn client_connected(&mut self, client: ClientId, spec: BackendSpec, policy: DetachedPolicy) {
        let backend_key = spec.key();
        let backend = self
            .backends
            .entry(backend_key.clone())
            .or_insert_with(|| BackendState {
                spec: spec.clone(),
                generation: 0,
                running: false,
                init: InitState::NotStarted,
                next_id: 0,
                pending: HashMap::new(),
                detached_policy: policy,
            });
        backend.detached_policy = policy;
        backend.spec = spec;
        self.clients.insert(
            client,
            ClientState {
                backend_key,
                next_id: 0,
                forwarded: HashMap::new(),
            },
        );
    }

    fn ensure_backend_running(&mut self, backend_key: &str) {
        let Some(backend) = self.backends.get_mut(backend_key) else {
            return;
        };
        if backend.running {
            return;
        }
        backend.generation += 1;
        backend.running = true;
        backend.init = InitState::NotStarted;
        self.output.push(HubOutput::SpawnBackend {
            spec: backend.spec.clone(),
            generation: backend.generation,
        });
    }

    fn client_message(&mut self, client: ClientId, message: Value) {
        let Some(backend_key) = self
            .clients
            .get(&client)
            .map(|state| state.backend_key.clone())
        else {
            return;
        };
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let id = message.get("id").cloned();
        match (method, id) {
            (Some(method), Some(id)) => {
                self.client_request(client, &backend_key, &method, id, message)
            }
            (Some(_), None) => self.send_to_backend(&backend_key, message),
            (None, Some(id)) => self.client_response(client, id, message),
            (None, None) => {}
        }
    }

    fn client_request(
        &mut self,
        client: ClientId,
        backend_key: &str,
        method: &str,
        id: Value,
        message: Value,
    ) {
        self.ensure_backend_running(backend_key);
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let session_id = params
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        match method {
            "initialize" => self.client_initialize(client, backend_key, id, params),
            "session/list" => {
                let result = self.list_sessions(backend_key, &params);
                self.reply(client, id, Ok(result));
            }
            "session/load" => match session_id {
                Some(session_id) if self.sessions.contains_key(&session_id) => {
                    self.load_from_journal(client, backend_key, id, session_id, message)
                }
                Some(session_id) => self.forward_request(
                    client,
                    backend_key,
                    id,
                    message,
                    RequestKind::LoadSession { session_id },
                ),
                None => self.forward_request(client, backend_key, id, message, RequestKind::Other),
            },
            "session/new" => {
                let cwd = params
                    .get("cwd")
                    .and_then(Value::as_str)
                    .map(PathBuf::from)
                    .unwrap_or_default();
                self.forward_request(
                    client,
                    backend_key,
                    id,
                    message,
                    RequestKind::NewSession { cwd },
                )
            }
            "session/prompt" => match session_id {
                Some(session_id) => {
                    self.record_prompt(client, &session_id, &params);
                    self.forward_request(
                        client,
                        backend_key,
                        id,
                        message,
                        RequestKind::Prompt { session_id },
                    )
                }
                None => self.forward_request(client, backend_key, id, message, RequestKind::Other),
            },
            _ => {
                if let Some(session_id) = session_id {
                    self.attach(client, &session_id);
                }
                self.forward_request(client, backend_key, id, message, RequestKind::Other)
            }
        }
    }

    fn client_initialize(&mut self, client: ClientId, backend_key: &str, id: Value, params: Value) {
        let Some(backend) = self.backends.get_mut(backend_key) else {
            return;
        };
        match &mut backend.init {
            InitState::Ready(result) => {
                let result = advertise_daemon_capabilities(result.clone());
                self.reply(client, id, Ok(result));
            }
            InitState::Waiting(waiters) => waiters.push((client, id)),
            InitState::NotStarted => {
                backend.init = InitState::Waiting(vec![(client, id)]);
                let backend_id = backend.next_id;
                backend.next_id += 1;
                backend
                    .pending
                    .insert(backend_id, PendingRequest::Initialize);
                let message = json!({
                    "jsonrpc": "2.0",
                    "id": backend_id,
                    "method": "initialize",
                    "params": self_sufficient_client_params(params),
                });
                self.send_to_backend(backend_key, message);
            }
        }
    }

    fn forward_request(
        &mut self,
        client: ClientId,
        backend_key: &str,
        client_request_id: Value,
        mut message: Value,
        kind: RequestKind,
    ) {
        let Some(backend) = self.backends.get_mut(backend_key) else {
            return;
        };
        let backend_id = backend.next_id;
        backend.next_id += 1;
        backend.pending.insert(
            backend_id,
            PendingRequest::Client {
                client,
                client_request_id,
                kind,
            },
        );
        message["id"] = json!(backend_id);
        self.send_to_backend(backend_key, message);
    }

    fn client_response(&mut self, client: ClientId, id: Value, mut message: Value) {
        let Some(local_id) = id.as_u64() else {
            return;
        };
        let Some(forwarded) = self
            .clients
            .get_mut(&client)
            .and_then(|state| state.forwarded.remove(&local_id))
        else {
            return;
        };
        let Some(backend_id) = forwarded.message.get("id").cloned() else {
            return;
        };
        message["id"] = backend_id;
        self.send_to_backend(&forwarded.backend_key, message);
    }

    fn client_disconnected(&mut self, client: ClientId) {
        let Some(state) = self.clients.remove(&client) else {
            return;
        };
        for session in self.sessions.values_mut() {
            session.attached.retain(|attached| *attached != client);
        }
        // Prompts the departed GUI never answered go back to the session so the
        // next GUI to attach is asked instead.
        for forwarded in state.forwarded.into_values() {
            match forwarded.session_id {
                Some(session_id) => self.route_backend_request(
                    &forwarded.backend_key,
                    Some(session_id),
                    forwarded.message,
                ),
                None => self.send_error(
                    &forwarded.backend_key,
                    &forwarded.message,
                    INTERNAL_ERROR,
                    "client disconnected",
                ),
            }
        }
    }

    fn load_from_journal(
        &mut self,
        client: ClientId,
        backend_key: &str,
        id: Value,
        session_id: String,
        message: Value,
    ) {
        let updates = match self.journal.updates(&session_id) {
            Ok(updates) => coalesce_chunks(updates),
            Err(error) => {
                self.reply(client, id, Err((INTERNAL_ERROR, format!("{error:#}"))));
                return;
            }
        };
        for update in updates {
            self.output.push(HubOutput::ToClient {
                client,
                message: session_update(&session_id, update),
            });
        }
        let needs_restore = self
            .sessions
            .get(&session_id)
            .is_some_and(|session| !self.is_live(session) && !session.restoring);
        let backend_supports_load = self.backends.get(backend_key).is_some_and(|backend| {
            matches!(&backend.init, InitState::Ready(result)
                if result.pointer("/agentCapabilities/loadSession") == Some(&json!(true)))
        });
        if needs_restore && backend_supports_load {
            if let Some(session) = self.sessions.get_mut(&session_id) {
                session.restoring = true;
            }
            self.forward_request(
                client,
                backend_key,
                Value::Null,
                message,
                RequestKind::RestoreSession {
                    session_id: session_id.clone(),
                },
            );
        }
        let setup = self
            .sessions
            .get(&session_id)
            .map(|session| strip_session_id(session.record.setup.clone()))
            .unwrap_or_else(|| json!({}));
        self.reply(client, id, Ok(setup));
        self.attach(client, &session_id);
    }

    fn list_sessions(&self, backend_key: &str, params: &Value) -> Value {
        let cwd = params.get("cwd").and_then(Value::as_str).map(PathBuf::from);
        let mut records: Vec<&SessionRecord> = self
            .sessions
            .values()
            .map(|session| &session.record)
            .filter(|record| record.backend_key == backend_key)
            .filter(|record| cwd.as_ref().is_none_or(|cwd| &record.cwd == cwd))
            .collect();
        records.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
        let sessions: Vec<Value> = records
            .into_iter()
            .map(|record| {
                let mut info = json!({
                    "sessionId": record.session_id,
                    "cwd": record.cwd,
                    "updatedAt": record.updated_at,
                });
                if let Some(title) = &record.title {
                    info["title"] = json!(title);
                }
                info
            })
            .collect();
        json!({ "sessions": sessions })
    }

    fn record_prompt(&mut self, client: ClientId, session_id: &str, params: &Value) {
        self.attach(client, session_id);
        let blocks = params
            .get("prompt")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let others: Vec<ClientId> = self
            .sessions
            .get(session_id)
            .map(|session| {
                session
                    .attached
                    .iter()
                    .copied()
                    .filter(|attached| *attached != client)
                    .collect()
            })
            .unwrap_or_default();
        for block in &blocks {
            let update = json!({ "sessionUpdate": "user_message_chunk", "content": block });
            if let Err(error) = self.journal.append_update(session_id, &update) {
                eprintln!("agentd: journal append failed for {session_id}: {error:#}");
            }
            for other in &others {
                self.output.push(HubOutput::ToClient {
                    client: *other,
                    message: session_update(session_id, update.clone()),
                });
            }
        }
        let Some(session) = self.sessions.get_mut(session_id) else {
            return;
        };
        session.record.updated_at = now();
        if session.record.title.is_none() {
            session.record.title = blocks
                .iter()
                .find_map(|block| block.get("text").and_then(Value::as_str))
                .map(|text| text.chars().take(TITLE_MAX_CHARS).collect());
        }
        if let Err(error) = self.journal.write_record(&session.record) {
            eprintln!("agentd: journal record write failed for {session_id}: {error:#}");
        }
    }

    fn attach(&mut self, client: ClientId, session_id: &str) {
        let Some(session) = self.sessions.get_mut(session_id) else {
            return;
        };
        if !session.attached.contains(&client) {
            session.attached.push(client);
        }
        let queued: Vec<(String, Value)> = session.queued.drain(..).collect();
        for (backend_key, request) in queued {
            self.route_backend_request(&backend_key, Some(session_id.to_string()), request);
        }
    }

    fn backend_message(&mut self, backend_key: &str, message: Value) {
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let id = message.get("id").cloned();
        match (method, id) {
            (Some(method), Some(_)) => {
                let session_id = message
                    .pointer("/params/sessionId")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                if method.starts_with("fs/") || method.starts_with("terminal/") {
                    // We told the backend the client lacks these capabilities.
                    self.send_error(
                        backend_key,
                        &message,
                        METHOD_NOT_FOUND,
                        "unsupported by agentd",
                    );
                } else {
                    self.route_backend_request(backend_key, session_id, message);
                }
            }
            (Some(method), None) => self.backend_notification(&method, message),
            (None, Some(id)) => self.backend_response(backend_key, id, message),
            (None, None) => {}
        }
    }

    fn backend_notification(&mut self, method: &str, message: Value) {
        let Some(session_id) = message
            .pointer("/params/sessionId")
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            return;
        };
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return;
        };
        if session.restoring {
            // The backend replays its own history while restoring; the GUI
            // already received ours.
            return;
        }
        if method == "session/update"
            && let Some(update) = message.pointer("/params/update")
        {
            if let Err(error) = self.journal.append_update(&session_id, update) {
                eprintln!("agentd: journal append failed for {session_id}: {error:#}");
            }
            if update.get("sessionUpdate").and_then(Value::as_str) == Some("session_info_update")
                && let Some(title) = update.get("title").and_then(Value::as_str)
            {
                session.record.title = Some(title.to_string());
                session.record.updated_at = now();
                if let Err(error) = self.journal.write_record(&session.record) {
                    eprintln!("agentd: journal record write failed for {session_id}: {error:#}");
                }
            }
        }
        for client in session.attached.clone() {
            self.output.push(HubOutput::ToClient {
                client,
                message: message.clone(),
            });
        }
    }

    fn backend_response(&mut self, backend_key: &str, id: Value, message: Value) {
        let Some(backend_id) = id.as_u64() else {
            return;
        };
        let Some(pending) = self
            .backends
            .get_mut(backend_key)
            .and_then(|backend| backend.pending.remove(&backend_id))
        else {
            return;
        };
        let result = message.get("result").cloned();
        match pending {
            PendingRequest::Initialize => self.backend_initialized(backend_key, message),
            PendingRequest::Client {
                client,
                client_request_id,
                kind,
            } => {
                match (kind, &result) {
                    (RequestKind::NewSession { cwd }, Some(result)) => {
                        self.register_session(client, backend_key, cwd, result)
                    }
                    (RequestKind::LoadSession { session_id }, Some(result)) => {
                        let mut result = result.clone();
                        result["sessionId"] = json!(session_id);
                        self.register_session(client, backend_key, PathBuf::new(), &result)
                    }
                    (RequestKind::RestoreSession { session_id }, _) => {
                        let generation = self
                            .backends
                            .get(backend_key)
                            .map(|backend| backend.generation);
                        if let Some(session) = self.sessions.get_mut(&session_id) {
                            session.restoring = false;
                            if result.is_some() {
                                session.generation = generation;
                            }
                        }
                        return;
                    }
                    (RequestKind::Prompt { session_id }, _) => {
                        if let Some(session) = self.sessions.get_mut(&session_id) {
                            session.record.updated_at = now();
                            if let Err(error) = self.journal.write_record(&session.record) {
                                eprintln!("agentd: journal record write failed: {error:#}");
                            }
                        }
                    }
                    _ => {}
                }
                if self.clients.contains_key(&client) {
                    let mut message = message;
                    message["id"] = client_request_id;
                    self.output.push(HubOutput::ToClient { client, message });
                }
            }
        }
    }

    fn backend_initialized(&mut self, backend_key: &str, message: Value) {
        let Some(backend) = self.backends.get_mut(backend_key) else {
            return;
        };
        let waiters = match std::mem::replace(&mut backend.init, InitState::NotStarted) {
            InitState::Waiting(waiters) => waiters,
            _ => Vec::new(),
        };
        let outcome = match message.get("result") {
            Some(result) => {
                backend.init = InitState::Ready(result.clone());
                Ok(advertise_daemon_capabilities(result.clone()))
            }
            None => Err(message.get("error").cloned().unwrap_or(Value::Null)),
        };
        for (client, id) in waiters {
            let message = match &outcome {
                Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                Err(error) => json!({ "jsonrpc": "2.0", "id": id, "error": error }),
            };
            self.output.push(HubOutput::ToClient { client, message });
        }
    }

    fn register_session(
        &mut self,
        client: ClientId,
        backend_key: &str,
        cwd: PathBuf,
        result: &Value,
    ) {
        let Some(session_id) = result.get("sessionId").and_then(Value::as_str) else {
            return;
        };
        let generation = self
            .backends
            .get(backend_key)
            .map(|backend| backend.generation);
        let timestamp = now();
        let session = self
            .sessions
            .entry(session_id.to_string())
            .or_insert_with(|| SessionState {
                backend_key: backend_key.to_string(),
                generation,
                restoring: false,
                attached: Vec::new(),
                queued: VecDeque::new(),
                record: SessionRecord {
                    session_id: session_id.to_string(),
                    backend_key: backend_key.to_string(),
                    cwd,
                    title: None,
                    created_at: timestamp.clone(),
                    updated_at: timestamp,
                    setup: strip_session_id(result.clone()),
                },
            });
        session.generation = generation;
        if let Err(error) = self.journal.write_record(&session.record) {
            eprintln!("agentd: journal record write failed for {session_id}: {error:#}");
        }
        self.attach(client, session_id);
    }

    /// Hands a backend → client request to the most recently attached GUI, or
    /// applies the detached policy when nobody is watching.
    fn route_backend_request(
        &mut self,
        backend_key: &str,
        session_id: Option<String>,
        message: Value,
    ) {
        let target = session_id.as_ref().and_then(|session_id| {
            self.sessions
                .get(session_id)
                .and_then(|session| session.attached.last().copied())
        });
        if let Some(client) = target
            && let Some(state) = self.clients.get_mut(&client)
        {
            let local_id = state.next_id;
            state.next_id += 1;
            let mut outgoing = message.clone();
            outgoing["id"] = json!(local_id);
            state.forwarded.insert(
                local_id,
                ForwardedRequest {
                    backend_key: backend_key.to_string(),
                    session_id,
                    message,
                },
            );
            self.output.push(HubOutput::ToClient {
                client,
                message: outgoing,
            });
            return;
        }
        let policy = self
            .backends
            .get(backend_key)
            .map(|backend| backend.detached_policy)
            .unwrap_or_default();
        let is_permission =
            message.get("method").and_then(Value::as_str) == Some("session/request_permission");
        match (policy, is_permission, session_id) {
            (DetachedPolicy::Allow, true, _) => {
                self.answer_permission(backend_key, &message, &["allow_once", "allow_always"])
            }
            (DetachedPolicy::Deny, true, _) => {
                self.answer_permission(backend_key, &message, &["reject_once", "reject_always"])
            }
            (_, _, Some(session_id)) => match self.sessions.get_mut(&session_id) {
                Some(session) => session.queued.push_back((backend_key.to_string(), message)),
                None => self.send_error(backend_key, &message, INTERNAL_ERROR, "unknown session"),
            },
            (_, _, None) => {
                self.send_error(backend_key, &message, INTERNAL_ERROR, "no client attached")
            }
        }
    }

    fn answer_permission(&mut self, backend_key: &str, request: &Value, preferred_kinds: &[&str]) {
        let options = request
            .pointer("/params/options")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let chosen = preferred_kinds.iter().find_map(|kind| {
            options
                .iter()
                .find(|option| option.get("kind").and_then(Value::as_str) == Some(kind))
                .and_then(|option| option.get("optionId").cloned())
        });
        let outcome = match chosen {
            Some(option_id) => json!({ "outcome": "selected", "optionId": option_id }),
            None => json!({ "outcome": "cancelled" }),
        };
        let response = json!({
            "jsonrpc": "2.0",
            "id": request.get("id").cloned().unwrap_or(Value::Null),
            "result": { "outcome": outcome },
        });
        self.send_to_backend(backend_key, response);
    }

    fn backend_exited(&mut self, backend_key: &str) {
        let Some(backend) = self.backends.get_mut(backend_key) else {
            return;
        };
        backend.running = false;
        let waiters = match std::mem::replace(&mut backend.init, InitState::NotStarted) {
            InitState::Waiting(waiters) => waiters,
            _ => Vec::new(),
        };
        let pending: Vec<PendingRequest> = backend
            .pending
            .drain()
            .map(|(_, pending)| pending)
            .collect();
        for (client, id) in waiters {
            self.reply(
                client,
                id,
                Err((INTERNAL_ERROR, "agent process exited".into())),
            );
        }
        for pending in pending {
            if let PendingRequest::Client {
                client,
                client_request_id,
                kind,
            } = pending
            {
                if let RequestKind::RestoreSession { session_id } = kind {
                    if let Some(session) = self.sessions.get_mut(&session_id) {
                        session.restoring = false;
                    }
                    continue;
                }
                self.reply(
                    client,
                    client_request_id,
                    Err((INTERNAL_ERROR, "agent process exited".into())),
                );
            }
        }
        for session in self.sessions.values_mut() {
            if session.backend_key == backend_key {
                session.queued.clear();
            }
        }
    }

    fn reply(&mut self, client: ClientId, id: Value, outcome: Result<Value, (i64, String)>) {
        let message = match outcome {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err((code, text)) => {
                json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": text } })
            }
        };
        self.output.push(HubOutput::ToClient { client, message });
    }

    fn send_error(&mut self, backend_key: &str, request: &Value, code: i64, text: &str) {
        let message = json!({
            "jsonrpc": "2.0",
            "id": request.get("id").cloned().unwrap_or(Value::Null),
            "error": { "code": code, "message": text },
        });
        self.send_to_backend(backend_key, message);
    }

    fn send_to_backend(&mut self, backend_key: &str, message: Value) {
        self.output.push(HubOutput::ToBackend {
            backend_key: backend_key.to_string(),
            message,
        });
    }
}

fn now() -> String {
    chrono::DateTime::<chrono::Utc>::from(SystemTime::now()).to_rfc3339()
}

fn session_update(session_id: &str, update: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": { "sessionId": session_id, "update": update },
    })
}

fn strip_session_id(mut value: Value) -> Value {
    if let Some(object) = value.as_object_mut() {
        object.remove("sessionId");
    }
    value
}

/// The backend runs on the same host as the files, so it must use its own
/// fs/terminal tools: a detached GUI cannot service those requests.
fn self_sufficient_client_params(mut params: Value) -> Value {
    if !params.is_object() {
        params = json!({});
    }
    let capabilities = params.as_object_mut().map(|object| {
        object
            .entry("clientCapabilities")
            .or_insert_with(|| Value::Object(Map::new()))
    });
    if let Some(Value::Object(capabilities)) = capabilities {
        capabilities.insert(
            "fs".into(),
            json!({ "readTextFile": false, "writeTextFile": false }),
        );
        capabilities.insert("terminal".into(), json!(false));
    }
    params
}

/// The daemon owns persistence, so it answers load/list itself and hides
/// backend features that would bypass its journal (resume, fork, close).
fn advertise_daemon_capabilities(mut result: Value) -> Value {
    let Some(object) = result.as_object_mut() else {
        return result;
    };
    let capabilities = object
        .entry("agentCapabilities")
        .or_insert_with(|| Value::Object(Map::new()));
    if let Some(capabilities) = capabilities.as_object_mut() {
        capabilities.insert("loadSession".into(), json!(true));
        let session_capabilities = capabilities
            .entry("sessionCapabilities")
            .or_insert_with(|| Value::Object(Map::new()));
        if let Some(session_capabilities) = session_capabilities.as_object_mut() {
            session_capabilities.insert("list".into(), json!({}));
            for hidden in ["resume", "fork", "close"] {
                session_capabilities.remove(hidden);
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::MemoryJournal;

    const GUI: ClientId = 1;
    const SECOND_GUI: ClientId = 2;

    fn spec() -> BackendSpec {
        BackendSpec {
            command: "fake-agent".into(),
            args: vec!["--acp".into()],
            env: Default::default(),
        }
    }

    fn connect(hub: &mut Hub<MemoryJournal>, client: ClientId, policy: DetachedPolicy) {
        hub.handle(HubEvent::ClientConnected {
            client,
            backend: spec(),
            detached_policy: policy,
        });
    }

    fn from_client(
        hub: &mut Hub<MemoryJournal>,
        client: ClientId,
        message: Value,
    ) -> Vec<HubOutput> {
        hub.handle(HubEvent::ClientMessage { client, message })
    }

    fn from_backend(hub: &mut Hub<MemoryJournal>, message: Value) -> Vec<HubOutput> {
        hub.handle(HubEvent::BackendMessage {
            backend_key: spec().key(),
            generation: 1,
            message,
        })
    }

    fn to_backend(outputs: &[HubOutput]) -> Vec<&Value> {
        outputs
            .iter()
            .filter_map(|output| match output {
                HubOutput::ToBackend { message, .. } => Some(message),
                _ => None,
            })
            .collect()
    }

    fn to_client(outputs: &[HubOutput], target: ClientId) -> Vec<&Value> {
        outputs
            .iter()
            .filter_map(|output| match output {
                HubOutput::ToClient { client, message } if *client == target => Some(message),
                _ => None,
            })
            .collect()
    }

    /// Drives initialize + session/new; returns the session id.
    fn start_session(hub: &mut Hub<MemoryJournal>, client: ClientId) -> String {
        let outputs = from_client(
            hub,
            client,
            json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"protocolVersion": 1, "clientCapabilities": {"terminal": true}}}),
        );
        assert!(matches!(
            outputs[0],
            HubOutput::SpawnBackend { generation: 1, .. }
        ));
        let initialize = to_backend(&outputs)[0].clone();
        assert_eq!(
            initialize["params"]["clientCapabilities"]["terminal"],
            json!(false)
        );
        let outputs = from_backend(
            hub,
            json!({"jsonrpc": "2.0", "id": initialize["id"], "result": {"protocolVersion": 1, "agentCapabilities": {"loadSession": false, "sessionCapabilities": {"resume": {}}}}}),
        );
        let reply = to_client(&outputs, client)[0];
        assert_eq!(
            reply["result"]["agentCapabilities"]["loadSession"],
            json!(true)
        );
        assert_eq!(
            reply["result"]["agentCapabilities"]["sessionCapabilities"],
            json!({"list": {}})
        );

        let outputs = from_client(
            hub,
            client,
            json!({"jsonrpc": "2.0", "id": 1, "method": "session/new", "params": {"cwd": "/work", "mcpServers": []}}),
        );
        let new_session = to_backend(&outputs)[0].clone();
        let outputs = from_backend(
            hub,
            json!({"jsonrpc": "2.0", "id": new_session["id"], "result": {"sessionId": "s1", "modes": {"currentModeId": "code"}}}),
        );
        assert_eq!(to_client(&outputs, client)[0]["id"], json!(1));
        "s1".into()
    }

    #[test]
    fn turn_keeps_streaming_into_journal_after_gui_detaches_and_replays_on_reattach() {
        let mut hub = Hub::new(MemoryJournal::default());
        connect(&mut hub, GUI, DetachedPolicy::Wait);
        let session_id = start_session(&mut hub, GUI);

        let outputs = from_client(
            &mut hub,
            GUI,
            json!({"jsonrpc": "2.0", "id": 2, "method": "session/prompt", "params": {"sessionId": session_id, "prompt": [{"type": "text", "text": "refactor the parser"}]}}),
        );
        let prompt_id = to_backend(&outputs)[0]["id"].clone();

        hub.handle(HubEvent::ClientDisconnected { client: GUI });
        for text in ["Work", "ing"] {
            let outputs = from_backend(
                &mut hub,
                json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s1", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}}}}),
            );
            assert!(outputs.is_empty(), "nobody attached, nothing sent");
        }
        let outputs = from_backend(
            &mut hub,
            json!({"jsonrpc": "2.0", "id": prompt_id, "result": {"stopReason": "end_turn"}}),
        );
        assert!(outputs.is_empty());

        connect(&mut hub, SECOND_GUI, DetachedPolicy::Wait);
        let outputs = from_client(
            &mut hub,
            SECOND_GUI,
            json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {}}),
        );
        assert!(
            to_backend(&outputs).is_empty(),
            "initialize answered from cache"
        );

        let outputs = from_client(
            &mut hub,
            SECOND_GUI,
            json!({"jsonrpc": "2.0", "id": 1, "method": "session/list", "params": {}}),
        );
        let sessions = &to_client(&outputs, SECOND_GUI)[0]["result"]["sessions"];
        assert_eq!(sessions[0]["sessionId"], "s1");
        assert_eq!(sessions[0]["title"], "refactor the parser");

        let outputs = from_client(
            &mut hub,
            SECOND_GUI,
            json!({"jsonrpc": "2.0", "id": 2, "method": "session/load", "params": {"sessionId": "s1", "cwd": "/work", "mcpServers": []}}),
        );
        let replay = to_client(&outputs, SECOND_GUI);
        assert_eq!(replay.len(), 3);
        assert_eq!(
            replay[0]["params"]["update"]["sessionUpdate"],
            "user_message_chunk"
        );
        assert_eq!(replay[1]["params"]["update"]["content"]["text"], "Working");
        assert_eq!(replay[2]["id"], json!(2));
        assert_eq!(replay[2]["result"]["modes"]["currentModeId"], "code");
        assert!(
            to_backend(&outputs).is_empty(),
            "live session needs no backend restore"
        );
    }

    #[test]
    fn permission_request_waits_for_reattach_then_routes_answer_back() {
        let mut hub = Hub::new(MemoryJournal::default());
        connect(&mut hub, GUI, DetachedPolicy::Wait);
        start_session(&mut hub, GUI);
        hub.handle(HubEvent::ClientDisconnected { client: GUI });

        let permission = json!({"jsonrpc": "2.0", "id": 77, "method": "session/request_permission", "params": {"sessionId": "s1", "options": [{"optionId": "yes", "kind": "allow_once"}]}});
        assert!(from_backend(&mut hub, permission).is_empty());

        connect(&mut hub, SECOND_GUI, DetachedPolicy::Wait);
        let outputs = from_client(
            &mut hub,
            SECOND_GUI,
            json!({"jsonrpc": "2.0", "id": 5, "method": "session/load", "params": {"sessionId": "s1"}}),
        );
        let forwarded = to_client(&outputs, SECOND_GUI)
            .into_iter()
            .find(|message| message["method"] == "session/request_permission")
            .cloned()
            .expect("queued permission delivered on attach");

        let outputs = from_client(
            &mut hub,
            SECOND_GUI,
            json!({"jsonrpc": "2.0", "id": forwarded["id"], "result": {"outcome": {"outcome": "selected", "optionId": "yes"}}}),
        );
        assert_eq!(to_backend(&outputs)[0]["id"], json!(77));
    }

    #[test]
    fn detached_allow_policy_answers_permission_immediately() {
        let mut hub = Hub::new(MemoryJournal::default());
        connect(&mut hub, GUI, DetachedPolicy::Allow);
        start_session(&mut hub, GUI);
        hub.handle(HubEvent::ClientDisconnected { client: GUI });

        let outputs = from_backend(
            &mut hub,
            json!({"jsonrpc": "2.0", "id": 9, "method": "session/request_permission", "params": {"sessionId": "s1", "options": [{"optionId": "no", "kind": "reject_once"}, {"optionId": "ok", "kind": "allow_once"}]}}),
        );
        let answer = to_backend(&outputs)[0];
        assert_eq!(answer["id"], json!(9));
        assert_eq!(answer["result"]["outcome"]["optionId"], "ok");
    }

    #[test]
    fn unanswered_permission_is_requeued_when_gui_disconnects() {
        let mut hub = Hub::new(MemoryJournal::default());
        connect(&mut hub, GUI, DetachedPolicy::Wait);
        start_session(&mut hub, GUI);
        let outputs = from_backend(
            &mut hub,
            json!({"jsonrpc": "2.0", "id": 3, "method": "session/request_permission", "params": {"sessionId": "s1", "options": []}}),
        );
        assert_eq!(to_client(&outputs, GUI).len(), 1);
        hub.handle(HubEvent::ClientDisconnected { client: GUI });
        assert_eq!(hub.status().sessions[0].queued_requests, 1);
    }

    #[test]
    fn backend_fs_and_terminal_requests_are_rejected() {
        let mut hub = Hub::new(MemoryJournal::default());
        connect(&mut hub, GUI, DetachedPolicy::Wait);
        start_session(&mut hub, GUI);
        let outputs = from_backend(
            &mut hub,
            json!({"jsonrpc": "2.0", "id": 4, "method": "terminal/create", "params": {"sessionId": "s1"}}),
        );
        assert_eq!(
            to_backend(&outputs)[0]["error"]["code"],
            json!(METHOD_NOT_FOUND)
        );
    }

    #[test]
    fn journaled_session_is_restored_silently_after_backend_restart() {
        let mut hub = Hub::new(MemoryJournal::default());
        connect(&mut hub, GUI, DetachedPolicy::Wait);
        start_session(&mut hub, GUI);
        hub.handle(HubEvent::BackendExited {
            backend_key: spec().key(),
            generation: 1,
        });
        assert!(!hub.status().sessions[0].live);

        let outputs = from_client(
            &mut hub,
            GUI,
            json!({"jsonrpc": "2.0", "id": 10, "method": "initialize", "params": {}}),
        );
        assert!(matches!(
            outputs[0],
            HubOutput::SpawnBackend { generation: 2, .. }
        ));
        let initialize_id = to_backend(&outputs)[0]["id"].clone();
        hub.handle(HubEvent::BackendMessage {
            backend_key: spec().key(),
            generation: 2,
            message: json!({"jsonrpc": "2.0", "id": initialize_id, "result": {"agentCapabilities": {"loadSession": true}}}),
        });

        let outputs = from_client(
            &mut hub,
            GUI,
            json!({"jsonrpc": "2.0", "id": 11, "method": "session/load", "params": {"sessionId": "s1"}}),
        );
        let restore = to_backend(&outputs)[0].clone();
        assert_eq!(restore["method"], "session/load");
        assert_eq!(
            to_client(&outputs, GUI)
                .last()
                .map(|reply| reply["id"].clone()),
            Some(json!(11))
        );

        let replayed = hub.handle(HubEvent::BackendMessage {
            backend_key: spec().key(),
            generation: 2,
            message: json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s1", "update": {"sessionUpdate": "plan"}}}),
        });
        assert!(replayed.is_empty(), "backend's own replay is suppressed");
        let done = hub.handle(HubEvent::BackendMessage {
            backend_key: spec().key(),
            generation: 2,
            message: json!({"jsonrpc": "2.0", "id": restore["id"], "result": {}}),
        });
        assert!(done.is_empty(), "restore result is internal");
        assert!(hub.status().sessions[0].live);
    }

    #[test]
    fn stale_generation_events_are_ignored() {
        let mut hub = Hub::new(MemoryJournal::default());
        connect(&mut hub, GUI, DetachedPolicy::Wait);
        start_session(&mut hub, GUI);
        let outputs = hub.handle(HubEvent::BackendExited {
            backend_key: spec().key(),
            generation: 99,
        });
        assert!(outputs.is_empty());
        assert!(hub.status().sessions[0].live);
    }
}
