//! Reads the plan and doc files that feed the auto-prompt context.

use crate::lightweight_context::has_unchecked_items;
use crate::{PlanFileContent, extract_active_project, plan_registry, plan_source};
use agent_client_protocol::schema::v2 as acp;
use std::sync::Arc;

/// Cheap inputs needed to read the `.plans/`/`.docs/` context files off the
/// main thread. Gathered on the main thread (needs `&App`); consumed by the
/// pure-fs readers below, which are safe to run on a background executor.
#[derive(Clone)]
pub(crate) struct ContextFileInputs {
    pub(crate) session_id: acp::SessionId,
    pub(crate) work_dirs: Option<Vec<std::path::PathBuf>>,
    first_user_message: Option<String>,
}

pub(crate) fn context_file_inputs(
    thread: &acp_thread::AcpThread,
    cx: &gpui::App,
) -> ContextFileInputs {
    let work_dirs = thread.work_dirs().map(|pl| pl.paths().to_vec());
    let first_user_message = thread.entries().iter().find_map(|entry| {
        if let acp_thread::AgentThreadEntry::UserMessage(msg) = entry {
            let content = msg.content.to_markdown(cx);
            if !content.is_empty() {
                return Some(content);
            }
        }
        None
    });
    ContextFileInputs {
        session_id: thread.session_id().clone(),
        work_dirs,
        first_user_message,
    }
}

/// Plan-file inputs without the first-user-message project hint — the shape
/// the Claude parity paths historically used.
pub(crate) fn plan_inputs_without_message(thread: &acp_thread::AcpThread) -> ContextFileInputs {
    ContextFileInputs {
        session_id: thread.session_id().clone(),
        work_dirs: thread.work_dirs().map(|pl| pl.paths().to_vec()),
        first_user_message: None,
    }
}

/// Read plan files for the dispatcher.
///
/// Canonical source is origin's remote-tracking refs (`plan_source`): a
/// dirty sibling-branch checkout or untracked WIP must never feed the
/// dispatcher stale plan contents. Per file, the ref where the file was last
/// touched wins (`origin/develop` vs `origin/main` vs …), so a hotfix that
/// only landed on one branch is seen even when another branch's tip is
/// newer. The worktree is only a fallback for repos without origin-published
/// plans (non-git dirs, no remote, nothing pushed yet).
///
/// `fetch_executor` supplies the timer bounding the gated `git fetch
/// origin` — pass `Some` only from background-thread callers; `None` resolves
/// against local remote-tracking refs without fetching.
pub(crate) async fn read_plan_files(
    inputs: &ContextFileInputs,
    fetch_executor: Option<&gpui::BackgroundExecutor>,
) -> Vec<PlanFileContent> {
    log::info!(
        "[auto_prompt::read_plan_files] Reading plan files (origin refs, fetch={})",
        fetch_executor.is_some()
    );

    let Some(work_dirs) = inputs.work_dirs.as_deref() else {
        log::info!("[auto_prompt::read_plan_files] No work directories configured");
        return Vec::new();
    };
    let session_id = inputs.session_id.to_string();

    let mut plan_files = Vec::new();

    for work_dir in work_dirs {
        let origin_files = plan_source::origin_plan_files(work_dir, fetch_executor).await;
        let candidates = origin_candidates(&origin_files, work_dir);
        push_unclaimed(candidates, &session_id, &mut plan_files);
    }

    finalize_plan_files(&mut plan_files, inputs.first_user_message.as_deref())
}

/// Synchronous variant for main-thread callers (the Claude parity paths): a
/// TTL-fresh origin snapshot when [`warm_plan_snapshots`] ran, otherwise the
/// worktree fallback. Never spawns git.
pub(crate) fn read_plan_files_cached(inputs: &ContextFileInputs) -> Vec<PlanFileContent> {
    log::info!("[auto_prompt::read_plan_files] Reading plan files (cached origin refs)");

    let Some(work_dirs) = inputs.work_dirs.as_deref() else {
        log::info!("[auto_prompt::read_plan_files] No work directories configured");
        return Vec::new();
    };
    let session_id = inputs.session_id.to_string();

    let mut plan_files = Vec::new();

    for work_dir in work_dirs {
        let cached = plan_source::cached_origin_plan_files(work_dir);
        let candidates = match cached {
            Some(origin_files) if !origin_files.is_empty() => {
                origin_candidates(&origin_files, work_dir)
            }
            _ => read_plan_dir_from_worktree(work_dir),
        };
        push_unclaimed(candidates, &session_id, &mut plan_files);
    }

    finalize_plan_files(&mut plan_files, inputs.first_user_message.as_deref())
}

/// Background-refresh the origin plan snapshots for these work dirs so the
/// synchronous Claude parity reads see canonical origin contents. Bounded by
/// the per-repo fetch gate (60s) and the 10s fetch timeout.
pub fn warm_plan_snapshots(
    work_dirs: Vec<std::path::PathBuf>,
    executor: gpui::BackgroundExecutor,
) -> gpui::Task<()> {
    let inner = executor.clone();
    executor.spawn(async move {
        for work_dir in work_dirs {
            plan_source::origin_plan_files(&work_dir, Some(&inner)).await;
        }
    })
}

/// Map origin files to (abs path, content) pairs, or fall back to the
/// worktree when origin carries nothing for this dir.
pub(crate) fn origin_candidates(
    origin_files: &Arc<Vec<plan_source::OriginPlanFile>>,
    work_dir: &std::path::Path,
) -> Vec<(String, String)> {
    if origin_files.is_empty() {
        return read_plan_dir_from_worktree(work_dir);
    }
    for file in origin_files.iter() {
        log::info!(
            "[auto_prompt::read_plan_files] origin: {} @ {} (blob {})",
            file.rel_path,
            file.ref_name,
            &file.blob[..file.blob.len().min(7)]
        );
    }
    origin_files
        .iter()
        .map(|file| (file.abs_path.clone(), file.content.clone()))
        .collect()
}

/// Claim-filtered merge into `plan_files`.
pub(crate) fn push_unclaimed(
    candidates: Vec<(String, String)>,
    session_id: &str,
    plan_files: &mut Vec<PlanFileContent>,
) {
    for (path_str, content) in candidates {
        if plan_registry::is_claimed_by_other(&path_str, session_id) {
            log::info!(
                "[auto_prompt::read_plan_files] Skipping plan claimed by another agent: {path_str}"
            );
            continue;
        }
        plan_files.push(PlanFileContent {
            path: path_str,
            content,
        });
    }
}

/// Sort/filter/truncate rules shared by both readers: active-project first,
/// incomplete plans from other projects, newest-first within groups, cap 10.
pub(crate) fn finalize_plan_files(
    plan_files: &mut Vec<PlanFileContent>,
    first_user_message: Option<&str>,
) -> Vec<PlanFileContent> {
    // Identify active project from first user message.
    // This prioritizes the session's target project over other workspace projects.
    // Supports: file:///... URLs, zed:///... URLs, and plain absolute paths containing /.plans/ or /.plan/
    let active_project = first_user_message.and_then(|msg| extract_active_project(msg));

    if let Some(ref active) = active_project {
        log::info!("[auto_prompt::read_plan_files] Active project: {active}");

        // Sort: active project's plans first, then newest (highest index) first within each group
        plan_files.sort_by(|a, b| {
            let a_active = a.path.starts_with(active.as_str());
            let b_active = b.path.starts_with(active.as_str());
            match (a_active, b_active) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => b.path.cmp(&a.path),
            }
        });

        // Filter: keep all active project plans, only incomplete plans from other projects
        let before = plan_files.len();
        plan_files.retain(|f| {
            if f.path.starts_with(active.as_str()) {
                true
            } else {
                has_unchecked_items(&f.content)
            }
        });
        log::info!(
            "[auto_prompt::read_plan_files] Cross-project filter: {before} → {} plan files",
            plan_files.len()
        );
    } else {
        plan_files.sort_by(|a, b| b.path.cmp(&a.path));
    }

    let max_plan_files = 10;
    if plan_files.len() > max_plan_files {
        let before = plan_files.len();
        plan_files.truncate(max_plan_files);
        log::info!(
            "[auto_prompt::read_plan_files] Truncated to {max_plan_files} most recent plan files: {before} → {}",
            plan_files.len()
        );
    }

    if !plan_files.is_empty() {
        log::info!(
            "[auto_prompt::read_plan_files] Loaded {} plan file(s): {:?}",
            plan_files.len(),
            plan_files.iter().map(|p| &p.path).collect::<Vec<_>>()
        );
    } else {
        log::info!("[auto_prompt::read_plan_files] No plan files found in any .plan directory");
    }

    std::mem::take(plan_files)
}

/// Worktree fallback for one work dir — the historical read behavior, kept
/// for repos whose origin carries no plan entries. Claim filtering happens
/// in the caller so both sources share one code path.
pub(crate) fn read_plan_dir_from_worktree(work_dir: &std::path::Path) -> Vec<(String, String)> {
    let plan_dir_candidates: Vec<std::path::PathBuf> = crate::plan_source::PLAN_DIR_NAMES
        .iter()
        .map(|dir| work_dir.join(dir))
        .collect();
    let Some(plan_dir) = plan_dir_candidates.iter().find(|d| d.is_dir()) else {
        log::info!(
            "[auto_prompt::read_plan_files] None of {:?} directories exist in {}",
            crate::plan_source::PLAN_DIR_NAMES,
            work_dir.display()
        );
        return Vec::new();
    };
    log::info!(
        "[auto_prompt::read_plan_files] Found plan directory (worktree fallback): {}",
        plan_dir.display()
    );

    let entries = match std::fs::read_dir(plan_dir) {
        Ok(entries) => entries,
        Err(err) => {
            log::warn!(
                "[auto_prompt::read_plan_files] Cannot read directory {}: {err}",
                plan_dir.display()
            );
            return Vec::new();
        }
    };

    let mut files = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        let metadata = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if metadata.len() > 100_000 {
            log::debug!(
                "auto_prompt: skipping large plan file ({} bytes): {}",
                metadata.len(),
                path.display()
            );
            continue;
        }

        if let Ok(content) = std::fs::read_to_string(&path) {
            files.push((path.to_string_lossy().to_string(), content));
        }
    }
    files
}

pub(crate) fn read_doc_files(work_dirs: &[std::path::PathBuf]) -> Vec<String> {
    let mut doc_files = Vec::new();

    for work_dir in work_dirs {
        let doc_dir_candidates = [work_dir.join(".docs")];
        let Some(doc_dir) = doc_dir_candidates.iter().find(|d| d.is_dir()) else {
            continue;
        };

        let entries = match std::fs::read_dir(doc_dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }

            let filename = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            if filename.is_empty() {
                continue;
            }

            doc_files.push(filename);
        }
    }

    if !doc_files.is_empty() {
        log::info!(
            "[auto_prompt::read_doc_files] Found {} doc file(s): {:?}",
            doc_files.len(),
            doc_files
        );
    }

    doc_files
}
