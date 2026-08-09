//! Symlinks Claude Code's `CLAUDE.md` onto Zed's `AGENTS.md`.
//!
//! Zed reads `AGENTS.md` — user-global at [`paths::agents_file`], per-project at
//! the worktree root. Claude Code reads `CLAUDE.md` from `~/.claude/` and from
//! the project root, and never looks at `AGENTS.md`, so anyone running both has
//! to maintain two copies of the same instructions. A symlink keeps one source
//! of truth, which is why this links rather than copies.
//!
//! Nothing here ever replaces something the user put there: an existing regular
//! file, or a symlink aimed somewhere else, is left alone and reported back as a
//! [`SymlinkOutcome`] the caller can log.

use std::path::{Path, PathBuf};

use anyhow::Result;
use fs::Fs;

/// The filename Claude Code reads instructions from.
pub const CLAUDE_MD: &str = "CLAUDE.md";
/// The filename Zed reads instructions from.
pub const AGENTS_MD: &str = "AGENTS.md";

/// What [`link_global_claude_md`] / [`link_project_claude_md`] did, or why they
/// declined to do anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SymlinkOutcome {
    Created,
    /// The link already exists and already points where we want it.
    AlreadyLinked,
    /// A regular file or directory occupies the link path.
    Occupied,
    /// A symlink is already there, aimed somewhere else.
    PointsElsewhere(PathBuf),
    /// There is no `AGENTS.md` to link to.
    NoSource,
    /// `~/.claude` is missing, i.e. Claude Code isn't installed for this user.
    NoClaudeConfigDir,
}

impl SymlinkOutcome {
    fn describe(&self, link: &Path) -> String {
        match self {
            Self::Created => format!("linked {} to AGENTS.md", link.display()),
            Self::AlreadyLinked => format!("{} already links to AGENTS.md", link.display()),
            Self::Occupied => format!("{} exists and is not a symlink; left alone", link.display()),
            Self::PointsElsewhere(target) => format!(
                "{} already links to {}; left alone",
                link.display(),
                target.display()
            ),
            Self::NoSource => format!("no AGENTS.md to link {} to", link.display()),
            Self::NoClaudeConfigDir => "Claude Code config directory not found".to_string(),
        }
    }
}

/// The directory Claude Code keeps user-global configuration in.
pub fn claude_config_dir() -> PathBuf {
    paths::home_dir().join(".claude")
}

/// Links `~/.claude/CLAUDE.md` to the user-global `AGENTS.md`.
pub async fn link_global_claude_md(fs: &dyn Fs) -> Result<SymlinkOutcome> {
    let claude_dir = claude_config_dir();
    if !fs.is_dir(&claude_dir).await {
        return Ok(SymlinkOutcome::NoClaudeConfigDir);
    }

    let source = paths::agents_file();
    let link = claude_dir.join(CLAUDE_MD);
    let outcome = ensure_symlink(fs, &link, source.clone(), source).await?;
    log::info!("CLAUDE.md symlink: {}", outcome.describe(&link));
    Ok(outcome)
}

/// Links `<worktree_root>/CLAUDE.md` to the project's own `AGENTS.md`.
///
/// The link target is relative so that it survives the repository being copied,
/// cloned, or checked out as an additional `git worktree`.
pub async fn link_project_claude_md(fs: &dyn Fs, worktree_root: &Path) -> Result<SymlinkOutcome> {
    let link = worktree_root.join(CLAUDE_MD);
    let outcome = ensure_symlink(
        fs,
        &link,
        PathBuf::from(AGENTS_MD),
        &worktree_root.join(AGENTS_MD),
    )
    .await?;
    log::info!("CLAUDE.md symlink: {}", outcome.describe(&link));
    Ok(outcome)
}

/// Creates `link` pointing at `link_target`, unless something is already there.
///
/// `link_target` is written into the symlink verbatim (so it can be relative),
/// while `source` is the resolved path used to check that the target exists —
/// we refuse to create a dangling link, because Claude Code would then report a
/// read error instead of silently ignoring the file.
async fn ensure_symlink(
    fs: &dyn Fs,
    link: &Path,
    link_target: PathBuf,
    source: &Path,
) -> Result<SymlinkOutcome> {
    if !fs.is_file(source).await {
        return Ok(SymlinkOutcome::NoSource);
    }

    // `read_link` is the probe rather than `metadata` because a *broken*
    // symlink has to be recognized as a symlink: `metadata` follows the link,
    // so a `CLAUDE.md` aimed at a since-deleted file reads as "nothing here"
    // and we'd replace something the user put there. `read_link` errors on
    // both a missing path and a non-symlink, which the `metadata` call below
    // then tells apart.
    if let Ok(existing) = fs.read_link(link).await {
        return match existing == link_target {
            true => Ok(SymlinkOutcome::AlreadyLinked),
            false => Ok(SymlinkOutcome::PointsElsewhere(existing)),
        };
    }

    if fs.metadata(link).await?.is_some() {
        return Ok(SymlinkOutcome::Occupied);
    }

    fs::Fs::create_symlink(fs, link, link_target).await?;
    Ok(SymlinkOutcome::Created)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use gpui::TestAppContext;
    use std::sync::Arc;
    use util::path;

    async fn setup(cx: &mut TestAppContext) -> Arc<FakeFs> {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), serde_json::json!({}))
            .await;
        fs
    }

    #[gpui::test]
    async fn test_links_project_claude_md(cx: &mut TestAppContext) {
        let fs = setup(cx).await;
        fs.insert_file(path!("/project/AGENTS.md"), b"rules".to_vec())
            .await;

        let outcome = link_project_claude_md(fs.as_ref(), Path::new(path!("/project")))
            .await
            .unwrap();

        assert_eq!(outcome, SymlinkOutcome::Created);
        assert_eq!(
            fs.read_link(Path::new(path!("/project/CLAUDE.md")))
                .await
                .unwrap(),
            PathBuf::from(AGENTS_MD)
        );
    }

    #[gpui::test]
    async fn test_skips_when_no_agents_md(cx: &mut TestAppContext) {
        let fs = setup(cx).await;

        let outcome = link_project_claude_md(fs.as_ref(), Path::new(path!("/project")))
            .await
            .unwrap();

        assert_eq!(outcome, SymlinkOutcome::NoSource);
        assert!(
            fs.metadata(Path::new(path!("/project/CLAUDE.md")))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[gpui::test]
    async fn test_preserves_existing_claude_md(cx: &mut TestAppContext) {
        let fs = setup(cx).await;
        fs.insert_file(path!("/project/AGENTS.md"), b"zed rules".to_vec())
            .await;
        fs.insert_file(path!("/project/CLAUDE.md"), b"claude rules".to_vec())
            .await;

        let outcome = link_project_claude_md(fs.as_ref(), Path::new(path!("/project")))
            .await
            .unwrap();

        assert_eq!(outcome, SymlinkOutcome::Occupied);
        assert_eq!(
            fs.load(Path::new(path!("/project/CLAUDE.md")))
                .await
                .unwrap(),
            "claude rules"
        );
    }

    /// `OTHER.md` deliberately doesn't exist: a *broken* symlink is the case
    /// where following the link (rather than reading it) would report "nothing
    /// here" and clobber a file the user put there on purpose.
    #[gpui::test]
    async fn test_preserves_symlink_pointing_elsewhere(cx: &mut TestAppContext) {
        let fs = setup(cx).await;
        fs.insert_file(path!("/project/AGENTS.md"), b"zed rules".to_vec())
            .await;
        fs.insert_symlink(path!("/project/CLAUDE.md"), PathBuf::from("OTHER.md"))
            .await;

        let outcome = link_project_claude_md(fs.as_ref(), Path::new(path!("/project")))
            .await
            .unwrap();

        assert_eq!(
            outcome,
            SymlinkOutcome::PointsElsewhere(PathBuf::from("OTHER.md"))
        );
    }

    #[gpui::test]
    async fn test_is_idempotent(cx: &mut TestAppContext) {
        let fs = setup(cx).await;
        fs.insert_file(path!("/project/AGENTS.md"), b"rules".to_vec())
            .await;

        link_project_claude_md(fs.as_ref(), Path::new(path!("/project")))
            .await
            .unwrap();
        let outcome = link_project_claude_md(fs.as_ref(), Path::new(path!("/project")))
            .await
            .unwrap();

        assert_eq!(outcome, SymlinkOutcome::AlreadyLinked);
    }
}
