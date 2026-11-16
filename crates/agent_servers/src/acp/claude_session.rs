//! Claude Code specific `session/new` metadata (`_meta.claudeCode.options`),
//! which `claude-agent-acp` spreads into the Claude Agent SDK options.

use std::collections::BTreeMap;

use agent_client_protocol::schema::v1::Meta;
use serde_json::{Map, Value, json};

const CLAUDE_META_KEY: &str = "claudeCode";
const OPTIONS_KEY: &str = "options";
const ENVIRONMENT_KEY: &str = "env";

/// Keeps MCP server secrets out of the Claude CLI's process arguments.
///
/// `claude-agent-acp` passes the forwarded servers to the CLI as one
/// `--mcp-config <json>` argument, so a literal header or env value is
/// readable by anything that lists processes. Claude Code expands `${VAR}`
/// inside that config, so each value is replaced by a placeholder and the
/// real value travels in the session's environment instead.
#[derive(Default)]
pub(crate) struct McpSecretEnvironment {
    variables: BTreeMap<String, String>,
}

impl McpSecretEnvironment {
    /// Returns the text to forward in place of `value`.
    pub(crate) fn placeholder(&mut self, server_name: &str, key: &str, value: &str) -> String {
        if value.is_empty() {
            return String::new();
        }
        // Names must be stable across new/load/resume: the adapter rebuilds a
        // loaded session whenever its options differ from the stored ones.
        let base_name = format!(
            "ZED_MCP_{}_{}",
            environment_name_fragment(server_name),
            environment_name_fragment(key)
        );
        let mut name = base_name.clone();
        let mut suffix = 2;
        while self
            .variables
            .get(&name)
            .is_some_and(|existing| existing != value)
        {
            name = format!("{base_name}_{suffix}");
            suffix += 1;
        }
        self.variables.insert(name.clone(), value.to_string());
        format!("${{{name}}}")
    }

    pub(crate) fn into_meta(self) -> Option<Meta> {
        if self.variables.is_empty() {
            return None;
        }
        let environment = self
            .variables
            .into_iter()
            .map(|(name, value)| (name, Value::String(value)))
            .collect::<Map<_, _>>();
        Some(Meta::from_iter([(
            CLAUDE_META_KEY.to_string(),
            json!({ OPTIONS_KEY: { ENVIRONMENT_KEY: environment } }),
        )]))
    }
}

fn environment_name_fragment(text: &str) -> String {
    text.chars()
        .map(|character| match character {
            character if character.is_ascii_alphanumeric() => character.to_ascii_uppercase(),
            _ => '_',
        })
        .collect()
}

/// Merges `overlay` into `base`, recursing into objects so that caller-pinned
/// session options and the secret environment can share `claudeCode.options`.
pub(crate) fn merge_meta(base: Option<Meta>, overlay: Option<Meta>) -> Option<Meta> {
    match (base, overlay) {
        (None, None) => None,
        (Some(meta), None) | (None, Some(meta)) => Some(meta),
        (Some(mut base), Some(overlay)) => {
            merge_objects(&mut base, overlay);
            Some(base)
        }
    }
}

fn merge_objects(base: &mut Map<String, Value>, overlay: Map<String, Value>) {
    for (key, overlay_value) in overlay {
        match (base.get_mut(&key), overlay_value) {
            (Some(Value::Object(base_object)), Value::Object(overlay_object)) => {
                merge_objects(base_object, overlay_object);
            }
            (_, overlay_value) => {
                base.insert(key, overlay_value);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_carry_no_secret_and_meta_carries_the_value() {
        let mut secrets = McpSecretEnvironment::default();
        let header = secrets.placeholder("mcp-server-web", "Authorization", "Bearer hunter2");
        assert_eq!(header, "${ZED_MCP_MCP_SERVER_WEB_AUTHORIZATION}");
        assert_eq!(secrets.placeholder("vision", "EMPTY", ""), "");

        let meta = secrets.into_meta().expect("one secret was recorded");
        assert_eq!(
            meta["claudeCode"]["options"]["env"]["ZED_MCP_MCP_SERVER_WEB_AUTHORIZATION"],
            "Bearer hunter2"
        );
        assert_eq!(
            meta["claudeCode"]["options"]["env"]
                .as_object()
                .map(|environment| environment.len()),
            Some(1)
        );
    }

    #[test]
    fn colliding_names_keep_both_values() {
        let mut secrets = McpSecretEnvironment::default();
        let first = secrets.placeholder("a-b", "KEY", "one");
        let second = secrets.placeholder("a_b", "KEY", "two");
        let repeated = secrets.placeholder("a-b", "KEY", "one");
        assert_eq!(first, "${ZED_MCP_A_B_KEY}");
        assert_eq!(second, "${ZED_MCP_A_B_KEY_2}");
        assert_eq!(repeated, first);
    }

    #[test]
    fn no_secrets_means_no_meta() {
        assert!(McpSecretEnvironment::default().into_meta().is_none());
    }

    #[test]
    fn merge_keeps_environment_beside_pinned_options() {
        let mut secrets = McpSecretEnvironment::default();
        secrets.placeholder("web", "TOKEN", "hunter2");
        let pinned = Meta::from_iter([(
            "claudeCode".to_string(),
            json!({ "options": { "model": "sonnet", "settings": { "advisorModel": "fable" } } }),
        )]);

        let merged = merge_meta(secrets.into_meta(), Some(pinned)).expect("both sides present");
        let options = &merged["claudeCode"]["options"];
        assert_eq!(options["model"], "sonnet");
        assert_eq!(options["settings"]["advisorModel"], "fable");
        assert_eq!(options["env"]["ZED_MCP_WEB_TOKEN"], "hunter2");
    }
}
