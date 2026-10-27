use crate::hub::Hub;
use crate::journal::FileJournal;
use crate::types::{BackendSpec, ClientId, Handshake, HandshakeEnvelope, HubEvent, HubOutput};
use anyhow::{Context as _, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{BufRead as _, BufReader};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

/// On-disk layout. `ZED_AGENTD_HOME` overrides the default so tests and
/// multiple daemons can coexist.
pub struct Paths {
    pub root: PathBuf,
}

impl Paths {
    pub fn from_env() -> Result<Self> {
        if let Some(root) = std::env::var_os("ZED_AGENTD_HOME") {
            return Ok(Self { root: root.into() });
        }
        let home = std::env::var_os("HOME").context("HOME is not set")?;
        Ok(Self {
            root: PathBuf::from(home).join(".local/share/zed-agentd"),
        })
    }

    pub fn socket(&self) -> PathBuf {
        self.root.join("agentd.sock")
    }

    pub fn sessions(&self) -> PathBuf {
        self.root.join("sessions")
    }

    pub fn log(&self) -> PathBuf {
        self.root.join("agentd.log")
    }
}

enum DaemonEvent {
    Hub(HubEvent),
    Connected {
        client: ClientId,
        handshake: Handshake,
        writer: Sender<Value>,
    },
    Shutdown,
}

pub fn serve(paths: &Paths) -> Result<()> {
    fs::create_dir_all(&paths.root)?;
    let socket = paths.socket();
    if UnixStream::connect(&socket).is_ok() {
        anyhow::bail!("agentd already running at {}", socket.display());
    }
    if socket.exists() {
        fs::remove_file(&socket)?;
    }
    let listener =
        UnixListener::bind(&socket).with_context(|| format!("binding {}", socket.display()))?;
    eprintln!(
        "agentd: pid {} listening on {}",
        std::process::id(),
        socket.display()
    );

    let (events, inbox) = mpsc::channel();
    thread::spawn({
        let events = events.clone();
        move || accept_loop(listener, events)
    });
    let hub = Hub::new(FileJournal::new(paths.sessions())?);
    run_hub(hub, events, inbox, paths.log());
    if let Err(error) = fs::remove_file(&socket) {
        eprintln!("agentd: removing socket: {error}");
    }
    Ok(())
}

fn accept_loop(listener: UnixListener, events: Sender<DaemonEvent>) {
    let mut next_client: ClientId = 0;
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                next_client += 1;
                let client = next_client;
                let events = events.clone();
                thread::spawn(move || {
                    if let Err(error) = client_session(client, stream, &events) {
                        eprintln!("agentd: client {client}: {error:#}");
                    }
                    events
                        .send(DaemonEvent::Hub(HubEvent::ClientDisconnected { client }))
                        .ok();
                });
            }
            Err(error) => eprintln!("agentd: accept failed: {error}"),
        }
    }
}

fn client_session(
    client: ClientId,
    stream: UnixStream,
    events: &Sender<DaemonEvent>,
) -> Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut first_line = String::new();
    reader.read_line(&mut first_line)?;
    let envelope: HandshakeEnvelope =
        serde_json::from_str(first_line.trim()).context("invalid agentd handshake")?;

    let (writer, outbox) = mpsc::channel::<Value>();
    thread::spawn(move || write_lines(stream, outbox));
    events
        .send(DaemonEvent::Connected {
            client,
            handshake: envelope.agentd,
            writer,
        })
        .ok();

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(&line) {
            Ok(message) => {
                if events
                    .send(DaemonEvent::Hub(HubEvent::ClientMessage {
                        client,
                        message,
                    }))
                    .is_err()
                {
                    break;
                }
            }
            Err(error) => eprintln!("agentd: client {client} sent invalid json: {error}"),
        }
    }
    Ok(())
}

fn write_lines(mut stream: impl std::io::Write, outbox: Receiver<Value>) {
    for message in outbox {
        let mut line = match serde_json::to_vec(&message) {
            Ok(line) => line,
            Err(error) => {
                eprintln!("agentd: serialize failed: {error}");
                continue;
            }
        };
        line.push(b'\n');
        if stream
            .write_all(&line)
            .and_then(|()| stream.flush())
            .is_err()
        {
            break;
        }
    }
}

struct RunningBackend {
    _child: Child,
    writer: Sender<Value>,
}

fn run_hub(
    mut hub: Hub<FileJournal>,
    events: Sender<DaemonEvent>,
    inbox: Receiver<DaemonEvent>,
    log: PathBuf,
) {
    let mut clients: HashMap<ClientId, Sender<Value>> = HashMap::new();
    let mut backends: HashMap<String, RunningBackend> = HashMap::new();
    for event in inbox {
        let outputs = match event {
            DaemonEvent::Shutdown => break,
            DaemonEvent::Connected {
                client,
                handshake,
                writer,
            } => match handshake {
                Handshake::Attach {
                    backend,
                    detached_permission,
                } => {
                    clients.insert(client, writer);
                    hub.handle(HubEvent::ClientConnected {
                        client,
                        backend,
                        detached_policy: detached_permission,
                    })
                }
                Handshake::Status => {
                    match serde_json::to_value(hub.status()) {
                        Ok(status) => {
                            writer.send(status).ok();
                        }
                        Err(error) => eprintln!("agentd: status serialize failed: {error}"),
                    }
                    continue;
                }
                Handshake::Stop => {
                    writer.send(serde_json::json!({ "stopping": true })).ok();
                    events.send(DaemonEvent::Shutdown).ok();
                    continue;
                }
            },
            DaemonEvent::Hub(event) => {
                if let HubEvent::ClientDisconnected { client } = &event {
                    clients.remove(client);
                }
                if let HubEvent::BackendExited { backend_key, .. } = &event {
                    backends.remove(backend_key);
                }
                hub.handle(event)
            }
        };
        for output in outputs {
            match output {
                HubOutput::ToClient { client, message } => {
                    if let Some(writer) = clients.get(&client) {
                        writer.send(message).ok();
                    }
                }
                HubOutput::ToBackend {
                    backend_key,
                    message,
                } => match backends.get(&backend_key) {
                    Some(backend) => {
                        backend.writer.send(message).ok();
                    }
                    None => eprintln!("agentd: dropping message for stopped backend"),
                },
                HubOutput::SpawnBackend { spec, generation } => {
                    let backend_key = spec.key();
                    match spawn_backend(&spec, generation, &events, &log) {
                        Ok(backend) => {
                            backends.insert(backend_key, backend);
                        }
                        Err(error) => {
                            eprintln!("agentd: spawning {}: {error:#}", spec.command);
                            events
                                .send(DaemonEvent::Hub(HubEvent::BackendExited {
                                    backend_key,
                                    generation,
                                }))
                                .ok();
                        }
                    }
                }
            }
        }
    }
}

// agentd is a runtime-free std binary; the smol-preferring lint targets async gpui code.
#[allow(clippy::disallowed_methods)]
fn spawn_backend(
    spec: &BackendSpec,
    generation: u64,
    events: &Sender<DaemonEvent>,
    log: &Path,
) -> Result<RunningBackend> {
    let stderr = OpenOptions::new().create(true).append(true).open(log)?;
    let mut child = Command::new(&spec.command)
        .args(&spec.args)
        .envs(&spec.env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(stderr)
        .spawn()
        .with_context(|| format!("running {}", spec.command))?;
    let stdin: ChildStdin = child.stdin.take().context("backend stdin")?;
    let stdout = child.stdout.take().context("backend stdout")?;
    let (writer, outbox) = mpsc::channel();
    thread::spawn(move || write_lines(stdin, outbox));

    let backend_key = spec.key();
    let events = events.clone();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(&line) {
                Ok(message) => {
                    let event = HubEvent::BackendMessage {
                        backend_key: backend_key.clone(),
                        generation,
                        message,
                    };
                    if events.send(DaemonEvent::Hub(event)).is_err() {
                        return;
                    }
                }
                Err(_) => eprintln!("agentd: backend stdout (non-json): {line}"),
            }
        }
        events
            .send(DaemonEvent::Hub(HubEvent::BackendExited {
                backend_key,
                generation,
            }))
            .ok();
    });
    Ok(RunningBackend {
        _child: child,
        writer,
    })
}
