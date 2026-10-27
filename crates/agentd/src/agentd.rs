//! Detachable ACP agent host.
//!
//! The Zed GUI talks ACP to `zed-agentd attach -- <agent> [args]` running on
//! the remote host. The bridge forwards to a long-lived daemon that owns the
//! real agent process and journals every session, so closing the GUI never
//! interrupts a turn and reopening it replays the session via `session/load`.

pub mod attach;
pub mod daemon;
pub mod hub;
pub mod journal;
pub mod types;
