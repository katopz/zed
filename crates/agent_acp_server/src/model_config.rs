//! Model discovery/selection + project boot helpers shared by the eval loop
//! and the ACP server (moved verbatim from `eval_cli/src/main.rs`, plan 042).
use std::sync::Arc;

use anyhow::{Context as _, Result};
use gpui::{AsyncApp, Entity, UpdateGlobal};
use language_model::{
    ANTHROPIC_PROVIDER_ID, LanguageModel, LanguageModelId, LanguageModelProviderId,
    LanguageModelRegistry, SelectedModel,
};
use project::Project;
use settings::SettingsStore;
use std::time::{Duration, Instant};

use crate::NativeAgentAppState;

/// Name of the env var carrying a JSON object to merge into
/// `language_models.openai_compatible` user settings before model discovery, in
/// the same shape as Zed's `openai_compatible` settings key (provider id ->
/// `{ "api_url": ..., "available_models": [...] }`). Lets zed-eval route the
/// agent itself through an OpenAI-compatible endpoint (e.g. Baseten) that isn't
/// one of Zed's built-in providers, without hardcoding it into eval-cli.
pub const OPENAI_COMPATIBLE_PROVIDERS_ENV: &str = "ZED_OPENAI_COMPATIBLE_PROVIDERS";

pub fn openai_compatible_providers_override() -> Option<String> {
    let raw = std::env::var(OPENAI_COMPATIBLE_PROVIDERS_ENV).ok()?;
    if raw.trim().is_empty() {
        return None;
    }
    Some(raw)
}

pub fn apply_openai_compatible_providers(providers_json: &str, cx: &mut gpui::App) -> Result<()> {
    let settings = format!(r#"{{"language_models": {{"openai_compatible": {providers_json}}}}}"#);
    SettingsStore::update_global(cx, |store, cx| {
        store.set_user_settings(&settings, cx).result()
    })
    .context("applying openai_compatible provider settings")?;
    Ok(())
}

/// Name of the env var carrying a JSON array to merge into
/// `language_models.anthropic.available_models` user settings before model
/// discovery, in the same shape as Zed's `anthropic.available_models` settings
/// key (a list of `{ "name": ..., "max_tokens": ..., ... }` entries). Lets
/// zed-eval run models that exist on the Anthropic API for the configured
/// key (e.g. early-access-program models) but aren't returned by the live
/// `/v1/models` listing, without hardcoding them into eval-cli.
pub const ANTHROPIC_AVAILABLE_MODELS_ENV: &str = "ZED_ANTHROPIC_AVAILABLE_MODELS";

pub fn anthropic_available_models_override() -> Option<String> {
    let raw = std::env::var(ANTHROPIC_AVAILABLE_MODELS_ENV).ok()?;
    if raw.trim().is_empty() {
        return None;
    }
    Some(raw)
}

pub fn apply_anthropic_available_models(models_json: &str, cx: &mut gpui::App) -> Result<()> {
    let settings =
        format!(r#"{{"language_models": {{"anthropic": {{"available_models": {models_json}}}}}}}"#);
    SettingsStore::update_global(cx, |store, cx| {
        store.set_user_settings(&settings, cx).result()
    })
    .context("applying anthropic available_models settings")?;
    Ok(())
}

const MODEL_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(30);

const MODEL_DISCOVERY_POLL_INTERVAL: Duration = Duration::from_millis(100);

pub async fn wait_for_model(selected: &SelectedModel, cx: &mut AsyncApp) -> Result<()> {
    let started_at = Instant::now();

    loop {
        let found = cx.update(|cx| find_available_model(selected, cx).is_some());
        if found {
            return Ok(());
        }

        cx.update(|cx| ensure_provider_authenticated(selected, cx))?;

        let selected_provider_has_models = cx.update(|cx| {
            LanguageModelRegistry::global(cx)
                .read(cx)
                .available_models(cx)
                .any(|model| model.provider_id() == selected.provider)
        });
        let should_wait_for_discovery =
            selected.provider == ANTHROPIC_PROVIDER_ID || !selected_provider_has_models;

        if !should_wait_for_discovery || started_at.elapsed() >= MODEL_DISCOVERY_TIMEOUT {
            return Err(cx.update(|cx| model_not_found_error(&selected_model_name(selected), cx)));
        }

        cx.background_executor()
            .timer(MODEL_DISCOVERY_POLL_INTERVAL)
            .await;
    }
}

fn ensure_provider_authenticated(selected: &SelectedModel, cx: &gpui::App) -> Result<()> {
    let registry = LanguageModelRegistry::global(cx);
    let provider = registry
        .read(cx)
        .provider(&selected.provider)
        .ok_or_else(|| anyhow::anyhow!("Provider {} not found", selected.provider.0))?;

    anyhow::ensure!(
        provider.is_authenticated(cx),
        "Provider {} is not authenticated",
        selected.provider.0
    );

    Ok(())
}

pub fn find_available_model(selected: &SelectedModel, cx: &gpui::App) -> Option<LanguageModel> {
    let registry = LanguageModelRegistry::global(cx);
    let models = registry.read(cx).available_models(cx).collect::<Vec<_>>();

    if let Some(model) = models
        .iter()
        .find(|model| model.provider_id() == selected.provider && model.id() == selected.model)
    {
        return Some(model.clone());
    }

    models
        .into_iter()
        .filter(|model| {
            model.provider_id() == selected.provider
                && model_id_matches_selected(&model.provider_id(), &model.id(), &selected.model)
        })
        .max_by(|left, right| left.id().0.to_string().cmp(&right.id().0.to_string()))
}

pub fn model_id_matches_selected(
    provider_id: &LanguageModelProviderId,
    available: &LanguageModelId,
    selected: &LanguageModelId,
) -> bool {
    if available == selected {
        return true;
    }

    if provider_id != &ANTHROPIC_PROVIDER_ID {
        return false;
    }

    anthropic_model_ids_match(available.0.as_ref(), selected.0.as_ref())
}

fn anthropic_model_ids_match(available: &str, selected: &str) -> bool {
    let available = anthropic_model_alias_base(available);
    let selected = anthropic_model_alias_base(selected);

    available == selected || anthropic_dated_model_id_matches_base(available, selected)
}

fn anthropic_model_alias_base(mut model_id: &str) -> &str {
    if let Some(stripped) = model_id.strip_suffix("-latest") {
        model_id = stripped;
    }
    if let Some(stripped) = model_id.strip_suffix("-thinking") {
        model_id = stripped;
    }
    if let Some(stripped) = model_id.strip_suffix("-1m-context") {
        model_id = stripped;
    }
    model_id
}

fn anthropic_dated_model_id_matches_base(available: &str, selected: &str) -> bool {
    let Some(suffix) = available.strip_prefix(selected) else {
        return false;
    };
    let Some(date) = suffix.strip_prefix('-') else {
        return false;
    };

    date.len() == 8 && date.chars().all(|character| character.is_ascii_digit())
}

pub fn selected_model_name(selected: &SelectedModel) -> String {
    format!("{}/{}", selected.provider.0, selected.model.0)
}

pub fn model_not_found_error(model_name: &str, cx: &gpui::App) -> anyhow::Error {
    let available = LanguageModelRegistry::global(cx)
        .read(cx)
        .available_models(cx)
        .map(|model| format!("{}/{}", model.provider_id().0, model.id().0))
        .collect::<Vec<_>>();
    let available = if available.is_empty() {
        "(none)".to_string()
    } else {
        available.join(", ")
    };

    anyhow::anyhow!("Model {model_name} not found. Available: {available}")
}

/// Selects the model and writes the agent settings both run modes share.
pub fn configure_agent_settings(
    selected: &SelectedModel,
    model_name: &str,
    thinking_override: Option<bool>,
    reasoning_effort: Option<&str>,
    openai_compatible_providers_json: Option<&str>,
    anthropic_available_models_json: Option<&str>,
    auto_allow_tools: bool,
    cx: &mut gpui::App,
) -> Result<()> {
    let registry = LanguageModelRegistry::global(cx);
    // Eval runs are unattended; ACP serving asks the client instead.
    let tool_permissions = if auto_allow_tools {
        r#""tool_permissions": {"default": "allow"},"#
    } else {
        ""
    };
    let model =
        find_available_model(&selected, cx).ok_or_else(|| model_not_found_error(model_name, cx))?;
    let supports_thinking = model.supports_thinking();
    let model_id = model.id().0.to_string();

    registry.update(cx, |registry, cx| {
        registry.set_default_model(Some(model), cx);
    });

    let enable_thinking = thinking_override.unwrap_or(supports_thinking);
    let effort = if enable_thinking {
        match reasoning_effort {
            Some(level) => format!("\"{level}\""),
            None => "\"high\"".to_string(),
        }
    } else {
        "null".to_string()
    };
    let provider_id = selected.provider.0.to_string();
    // set_user_settings replaces the whole user settings buffer, so the
    // openai_compatible/anthropic blocks applied earlier (before model
    // discovery) have to be folded back in here, or they would be dropped
    // by this call.
    let mut language_models_fields = Vec::new();
    if let Some(providers_json) = openai_compatible_providers_json {
        language_models_fields.push(format!(r#""openai_compatible": {providers_json}"#));
    }
    if let Some(models_json) = anthropic_available_models_json {
        language_models_fields.push(format!(
            r#""anthropic": {{"available_models": {models_json}}}"#
        ));
    }
    let language_models_settings = format!("{{{}}}", language_models_fields.join(","));
    // Disable specific tools (e.g. `fetch`/`search_web` on air-gapped
    // benchmarks) the canonical way: via the agent profile. The model only
    // sees a built-in tool when the active profile enables it (see
    // Thread::enabled_tools), so we define a dedicated "eval" profile that
    // mirrors the built-in "write" profile minus the disabled tools, and make
    // it the default profile. A fresh profile key is NOT deep-merged against
    // the defaults, so it can't inherit "write"'s tools — the full set is
    // listed explicitly here. Keep WRITE_TOOLS in sync with the "write"
    // profile in assets/settings/default.json.
    let profile_field = {
        let raw = std::env::var("ZED_EVAL_DISABLE_TOOLS").unwrap_or_default();
        let disabled = raw
            .split(',')
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty())
            .collect::<std::collections::HashSet<String>>();
        if disabled.is_empty() {
            String::new()
        } else {
            const WRITE_TOOLS: &[&str] = &[
                "copy_path",
                "create_directory",
                "create_thread",
                "delete_path",
                "diagnostics",
                "apply_code_action",
                "edit_file",
                "write_file",
                "fetch",
                "find_path",
                "find_references",
                "get_code_actions",
                "go_to_definition",
                "list_agents_and_models",
                "list_directory",
                "move_path",
                "rename_symbol",
                "read_file",
                "grep",
                "skill",
                "spawn_agent",
                "request_verdict",
                "terminal",
                "search_web",
            ];
            let tools = WRITE_TOOLS
                .iter()
                .filter(|tool| !disabled.contains(**tool))
                .map(|tool| format!(r#""{tool}": true"#))
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                r#","default_profile": "eval", "profiles": {{"eval": {{"name": "Eval", "enable_all_context_servers": true, "tools": {{{tools}}}}}}}"#
            )
        }
    };
    SettingsStore::update_global(cx, |store, cx| {
        let settings = format!(
            r#"{{
                "language_models": {language_models_settings},
                "agent": {{
                    {tool_permissions}
                    "default_model": {{
                        "provider": "{provider_id}",
                        "model": "{model_id}",
                        "enable_thinking": {enable_thinking},
                        "effort": {effort}
                    }}{profile_field}
                }},
                "autosave": "off",
                "format_on_save": "off"
            }}
            "#
        );
        store.set_user_settings(&settings, cx).result()
    })
    .context("updating agent settings")?;

    anyhow::Ok(())
}

/// A local project with one fully scanned worktree at `workdir`.
pub async fn open_local_project(
    app_state: &Arc<NativeAgentAppState>,
    workdir: &std::path::Path,
    cx: &mut AsyncApp,
) -> Result<Entity<Project>> {
    let project = cx.update(|cx| {
        Project::local(
            app_state.client.clone(),
            app_state.node_runtime.clone(),
            app_state.user_store.clone(),
            app_state.languages.clone(),
            app_state.fs.clone(),
            None,
            project::LocalProjectFlags {
                init_worktree_trust: false,
                ..Default::default()
            },
            cx,
        )
    });
    let worktree = project
        .update(cx, |project, cx| project.create_worktree(workdir, true, cx))
        .await
        .context("creating worktree")?;
    let scan = worktree.update(cx, |tree, _cx| {
        tree.as_local()
            .context("expected local worktree")
            .map(|local| local.scan_complete())
    })?;
    scan.await;
    Ok(project)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anthropic_latest_alias_matches_listed_base_model() {
        assert!(model_id_matches_selected(
            &ANTHROPIC_PROVIDER_ID,
            &LanguageModelId("claude-sonnet-4-6".into()),
            &LanguageModelId("claude-sonnet-4-6-latest".into()),
        ));
    }

    #[test]
    fn anthropic_thinking_alias_matches_listed_base_model() {
        assert!(model_id_matches_selected(
            &ANTHROPIC_PROVIDER_ID,
            &LanguageModelId("claude-sonnet-4-6".into()),
            &LanguageModelId("claude-sonnet-4-6-1m-context-thinking-latest".into()),
        ));
    }

    #[test]
    fn anthropic_latest_alias_matches_listed_dated_model() {
        assert!(model_id_matches_selected(
            &ANTHROPIC_PROVIDER_ID,
            &LanguageModelId("claude-sonnet-4-6-20260518".into()),
            &LanguageModelId("claude-sonnet-4-6-latest".into()),
        ));
    }

    #[test]
    fn non_anthropic_models_require_exact_ids() {
        assert!(!model_id_matches_selected(
            &LanguageModelProviderId("other".into()),
            &LanguageModelId("claude-sonnet-4-6".into()),
            &LanguageModelId("claude-sonnet-4-6-latest".into()),
        ));
    }
}
