//! Fork-owned hook: when enabled, external agents in remote projects are started
//! through `zed-agentd attach` on the host, so their turns survive the GUI
//! disconnecting. Kept in its own file so the upstream transport only carries a
//! one-call diff.

use gpui::{App, Global};
use remote::command::RemoteCommand;
use util::paths::PathStyle;

#[derive(Default)]
pub struct DetachableRemoteAgents {
    pub enabled: bool,
}

impl Global for DetachableRemoteAgents {}

impl DetachableRemoteAgents {
    pub fn enabled(cx: &App) -> bool {
        cx.try_global::<Self>()
            .is_some_and(|detachable| detachable.enabled)
    }

    pub fn set_enabled(enabled: bool, cx: &mut App) {
        cx.set_global(Self { enabled });
    }
}

/// Uses `zed-agentd` when the host has it; otherwise execs the agent directly,
/// so enabling this on a host without agentd degrades to stock behaviour.
const SHIM: &str = r#"for candidate in "$(command -v zed-agentd 2>/dev/null)" "$HOME/.cargo/bin/zed-agentd" "$HOME/.local/bin/zed-agentd"; do
  if [ -n "$candidate" ] && [ -x "$candidate" ]; then exec "$candidate" attach "$@"; fi
done
while [ "$#" -gt 0 ] && [ "$1" != "--" ]; do shift; done
shift
exec "$@""#;

pub fn wrap(command: RemoteCommand, path_style: PathStyle, cx: &App) -> RemoteCommand {
    if !DetachableRemoteAgents::enabled(cx) || path_style != PathStyle::Unix {
        return command;
    }
    wrap_with_shim(command)
}

fn wrap_with_shim(command: RemoteCommand) -> RemoteCommand {
    let mut env_names: Vec<&String> = command.env.keys().collect();
    env_names.sort();
    let mut args = vec![
        "-c".to_string(),
        SHIM.to_string(),
        "zed-agentd-shim".to_string(),
    ];
    // Names only: the values reach the daemon through the bridge's own
    // environment instead of the host's process list.
    for name in env_names {
        args.push("--pass-env".to_string());
        args.push(name.clone());
    }
    args.push("--".to_string());
    args.push(command.program);
    args.extend(command.args);
    RemoteCommand {
        program: "sh".to_string(),
        args,
        env: command.env,
        working_dir: command.working_dir,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn shim_passes_env_names_and_original_command_after_separator() {
        let command = RemoteCommand {
            program: "/opt/agent".into(),
            args: vec!["--acp".into()],
            env: HashMap::from_iter([("B_KEY".to_string(), "secret".to_string()), ("A_KEY".to_string(), "x".to_string())]),
            working_dir: Some("/work".into()),
        };
        let wrapped = wrap_with_shim(command);
        assert_eq!(wrapped.program, "sh");
        assert_eq!(
            wrapped.args[3..],
            ["--pass-env", "A_KEY", "--pass-env", "B_KEY", "--", "/opt/agent", "--acp"]
        );
        assert!(!wrapped.args.iter().any(|arg| arg.contains("secret")));
        assert_eq!(wrapped.working_dir.as_deref(), Some("/work"));
    }

    #[test]
    #[allow(
        clippy::disallowed_methods,
        reason = "a synchronous test that runs a tiny shell script"
    )]
    fn shim_falls_back_to_running_the_agent_directly() -> anyhow::Result<()> {
        let output = std::process::Command::new("sh")
            .args(["-c", SHIM, "zed-agentd-shim", "--pass-env", "X", "--", "echo", "direct"])
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", "/nonexistent")
            .output()?;
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "direct");
        Ok(())
    }
}
