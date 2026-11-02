//! Fork-owned (plan 042): under Work Remote, a "Zed Agent (host)" entry that
//! runs Zed's native agent on the remote host as `<remote server> agent-acp`.
//! It is an ordinary external agent to the rest of Zed, so it goes through the
//! same zed-agentd shim (turns survive this window closing) and gets the same
//! session list / reattach handling as Claude Code or Codex.

use std::any::Any;
use collections::HashMap;

use anyhow::{Context as _, Result};
use gpui::{App, AsyncApp, Task};
use language_model::LanguageModelRegistry;
use project::agent_server_store::{AgentServerCommand, ExternalAgentEntry, ExternalAgentServer};
use project::{AgentId, AgentServersUpdated, ExternalAgentSource, Project};

pub const HOST_AGENT_ID: &str = "zed-agent-host";
const DISPLAY_NAME: &str = "Zed Agent (host)";

/// Adds the host agent to a remote project's agent list. No-op for local
/// projects, and idempotent.
pub fn register(project: &Project, cx: &mut App) {
    if !project.is_via_remote_server() {
        return;
    }
    project.agent_server_store().clone().update(cx, |store, cx| {
        let agent_id = AgentId::new(HOST_AGENT_ID);
        if store.external_agents.contains_key(&agent_id) {
            return;
        }
        store.external_agents.insert(
            agent_id,
            ExternalAgentEntry::new(
                Box::new(HostNativeAgent),
                ExternalAgentSource::Custom,
                None,
                Some(DISPLAY_NAME.into()),
            ),
        );
        cx.emit(AgentServersUpdated);
    });
}

struct HostNativeAgent;

impl ExternalAgentServer for HostNativeAgent {
    fn get_command(
        &mut self,
        extra_args: Vec<String>,
        extra_env: HashMap<String, String>,
        cx: &mut AsyncApp,
    ) -> Task<Result<AgentServerCommand>> {
        // Callers hold the store's update borrow while calling this, so the
        // registry can only be read once that borrow is released.
        cx.spawn(async move |cx| cx.update(|cx| host_command(extra_args, extra_env, cx)))
    }

    fn survives_remote_refresh(&self) -> bool {
        true
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// Runs with the model selected in this window, and with that model's
/// credentials in the environment (never argv). The remote server's own path
/// arrives in [`agentd::EMBEDDED_BINARY_ENV`], set by `remote_server exec`.
fn host_command(
    extra_args: Vec<String>,
    extra_env: HashMap<String, String>,
    cx: &App,
) -> Result<AgentServerCommand> {
    let registry = LanguageModelRegistry::read_global(cx);
    let model = registry
        .default_model()
        .context("select a model for the Zed Agent first: the host agent uses the same one")?;
    let provider = registry
        .provider(&model.provider_id)
        .with_context(|| format!("language model provider {} is not available", model.provider_id.0))?;

    let mut env: HashMap<String, String> =
        provider.host_environment(&model, cx).into_iter().collect();
    env.insert(
        agentd::NATIVE_AGENT_MODEL_ENV.to_string(),
        format!("{}/{}", model.provider_id.0, model.id.0),
    );
    env.extend(extra_env);

    let mut args = vec!["-c".to_string(), launch_script(), HOST_AGENT_ID.to_string()];
    args.extend(extra_args);
    Ok(AgentServerCommand {
        path: "sh".into(),
        args,
        env: Some(env),
    })
}

/// A server zed.dev downloaded has no `agent-acp`; say so instead of failing
/// with a bare `exec` error.
fn launch_script() -> String {
    let binary_env = agentd::EMBEDDED_BINARY_ENV;
    let subcommand = agentd::NATIVE_AGENT_SUBCOMMAND;
    format!(
        r#"server="${{{binary_env}:-}}"
if [ -z "$server" ] || [ ! -x "$server" ]; then
  echo "{DISPLAY_NAME} needs the remote server built by this Zed; this host runs one without it" >&2
  exit 1
fi
exec "$server" {subcommand} "$@""#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(
        clippy::disallowed_methods,
        reason = "a synchronous test that runs a tiny shell script"
    )]
    fn run_script(server: Option<&std::path::Path>) -> anyhow::Result<std::process::Output> {
        let mut command = std::process::Command::new("sh");
        command
            .args(["-c", &launch_script(), HOST_AGENT_ID, "--flag"])
            .env_remove(agentd::EMBEDDED_BINARY_ENV);
        if let Some(server) = server {
            command.env(agentd::EMBEDDED_BINARY_ENV, server);
        }
        Ok(command.output()?)
    }

    #[cfg(unix)]
    #[test]
    fn launch_script_execs_the_servers_agent_subcommand() -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;
        let directory = tempfile::tempdir()?;
        let server = directory.path().join("remote_server");
        std::fs::write(&server, "#!/bin/sh\necho \"$@\"\n")?;
        std::fs::set_permissions(&server, std::fs::Permissions::from_mode(0o755))?;

        let output = run_script(Some(&server))?;
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout)?.trim(),
            format!("{} --flag", agentd::NATIVE_AGENT_SUBCOMMAND)
        );
        Ok(())
    }

    #[test]
    fn launch_script_explains_a_server_without_the_agent() -> anyhow::Result<()> {
        let output = run_script(None)?;
        assert!(!output.status.success());
        assert!(String::from_utf8(output.stderr)?.contains(DISPLAY_NAME));
        Ok(())
    }
}
