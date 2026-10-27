//! Fork-owned gate (plan 039): offers v1 external agents the ACP subagents
//! capability without the staff-only `acp-beta` flag, which would also switch
//! whole connections to ACP v2. Mirrored from `agent.external_subagents` by
//! `agent_ui`; unset reads as off, so embedders without it keep stock behaviour.

use feature_flags::{AcpBetaFeatureFlag, FeatureFlagAppExt as _};
use gpui::{App, Global};

#[derive(Default)]
pub struct ExternalSubagents {
    pub enabled: bool,
}

impl Global for ExternalSubagents {}

impl ExternalSubagents {
    pub fn enabled(cx: &App) -> bool {
        cx.try_global::<Self>()
            .is_some_and(|external_subagents| external_subagents.enabled)
    }

    pub fn set_enabled(enabled: bool, cx: &mut App) {
        cx.set_global(Self { enabled });
    }
}

/// Whether a new v1 agent connection advertises the subagents capability.
pub(crate) fn offer_subagents(cx: &App) -> bool {
    cx.has_flag::<AcpBetaFeatureFlag>() || ExternalSubagents::enabled(cx)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::acp::selection_tests::set_beta;

    #[gpui::test]
    fn subagents_follow_the_setting_without_acp_beta(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let store = settings::SettingsStore::test(cx);
            cx.set_global(store);
            set_beta(false, cx);
            assert!(!offer_subagents(cx), "unset gate keeps stock behaviour");

            ExternalSubagents::set_enabled(true, cx);
            assert!(offer_subagents(cx));

            ExternalSubagents::set_enabled(false, cx);
            assert!(!offer_subagents(cx));

            set_beta(true, cx);
            assert!(offer_subagents(cx), "acp-beta still offers it on its own");
        });
    }
}
