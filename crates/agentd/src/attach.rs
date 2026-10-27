use crate::daemon::Paths;
use crate::types::{Handshake, HandshakeEnvelope};
use anyhow::{Context as _, Result};
use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(10);

/// How the running binary invokes agentd again, so `attach` can start the
/// daemon with `<program> <prefix>... serve`.
pub struct SelfLauncher {
    pub program: PathBuf,
    pub prefix: Vec<OsString>,
}

impl SelfLauncher {
    pub fn standalone() -> Result<Self> {
        Ok(Self {
            program: std::env::current_exe()?,
            prefix: Vec::new(),
        })
    }

    /// agentd embedded as the `subcommand` of another binary (the remote server).
    pub fn embedded(subcommand: &str) -> Result<Self> {
        Ok(Self {
            program: std::env::current_exe()?,
            prefix: vec![subcommand.into()],
        })
    }

    fn command(&self, subcommand: &str) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.prefix).arg(subcommand);
        command
    }
}

/// Bridges this process's stdio (owned by the GUI's SSH session) to the daemon.
/// When the GUI goes away only this bridge dies; the daemon and agent keep going.
pub fn attach(paths: &Paths, launcher: &SelfLauncher, handshake: Handshake) -> Result<()> {
    let stream = connect_or_spawn(paths, launcher)?;
    let mut writer = stream.try_clone()?;
    let mut line = serde_json::to_vec(&HandshakeEnvelope { agentd: handshake })?;
    line.push(b'\n');
    writer.write_all(&line)?;

    let mut reader = stream.try_clone()?;
    let downstream = thread::spawn(move || {
        let mut stdout = std::io::stdout().lock();
        let mut buffer = [0u8; 64 * 1024];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    if stdout
                        .write_all(&buffer[..read])
                        .and_then(|()| stdout.flush())
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    });

    let copied = std::io::copy(&mut std::io::stdin().lock(), &mut writer);
    // Half-close so the daemon sees EOF for this client but can still flush.
    stream.shutdown(std::net::Shutdown::Write).ok();
    if let Err(error) = copied {
        eprintln!("zed-agentd: stdin closed: {error}");
    }
    downstream
        .join()
        .map_err(|_| anyhow::anyhow!("output thread panicked"))?;
    Ok(())
}

/// One-shot request (status/stop): prints the single JSON reply.
pub fn query(paths: &Paths, handshake: Handshake) -> Result<String> {
    let mut stream = UnixStream::connect(paths.socket())
        .with_context(|| format!("agentd is not running ({})", paths.socket().display()))?;
    let mut line = serde_json::to_vec(&HandshakeEnvelope { agentd: handshake })?;
    line.push(b'\n');
    stream.write_all(&line)?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply)?;
    Ok(reply)
}

// agentd is a runtime-free std binary; the smol-preferring lint targets async gpui code.
#[allow(clippy::disallowed_methods)]
fn connect_or_spawn(paths: &Paths, launcher: &SelfLauncher) -> Result<UnixStream> {
    if let Ok(stream) = UnixStream::connect(paths.socket()) {
        return Ok(stream);
    }
    std::fs::create_dir_all(&paths.root)?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.log())?;
    let mut command = launcher.command("serve");
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log);
    // SAFETY: setsid is async-signal-safe and touches no Rust state; detaching
    // into a new session is what lets the daemon outlive the SSH session that
    // spawned this bridge.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn().context("spawning agentd daemon")?;

    let started = Instant::now();
    loop {
        match UnixStream::connect(paths.socket()) {
            Ok(stream) => return Ok(stream),
            Err(error) if started.elapsed() > DAEMON_START_TIMEOUT => {
                return Err(error).with_context(|| {
                    format!("agentd did not start; see {}", paths.log().display())
                });
            }
            Err(_) => thread::sleep(Duration::from_millis(50)),
        }
    }
}
