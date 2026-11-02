//! Detachable ACP agent host.
//!
//! The Zed GUI talks ACP to `zed-agentd attach -- <agent> [args]` running on
//! the remote host. The bridge forwards to a long-lived daemon that owns the
//! real agent process and journals every session, so closing the GUI never
//! interrupts a turn and reopening it replays the session via `session/load`.

#[cfg(unix)]
pub mod attach;
#[cfg(unix)]
pub mod cli;
#[cfg(unix)]
pub mod daemon;
pub mod hub;
pub mod journal;
pub mod types;

/// The remote server runs agentd as `<remote server> agentd ...`, so every
/// host Zed uploads its own server to gets a version-matched agentd for free.
pub const EMBEDDED_SUBCOMMAND: &str = "agentd";

/// Set by `remote_server exec` on the processes it starts to its own path,
/// which tells the GUI's launch shim that the embedded agentd is available.
pub const EMBEDDED_BINARY_ENV: &str = "ZED_AGENTD_REMOTE_SERVER";

/// `<remote server> agent-acp` serves Zed's native agent over ACP stdio, so the
/// Work Remote GUI can run the Zed Agent on the host under agentd.
pub const NATIVE_AGENT_SUBCOMMAND: &str = "agent-acp";

/// The native agent's model (`provider/model`). Passed by environment, not
/// argv: [`types::BackendSpec::key`] scopes sessions by command + argv, so a
/// model in argv would hide earlier sessions after the user switches models.
pub const NATIVE_AGENT_MODEL_ENV: &str = "ZED_AGENT_ACP_MODEL";
