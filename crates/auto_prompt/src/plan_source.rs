//! Git-backed plan source: reads `.plan`/`.plans` files from the repository's
//! remote-tracking refs instead of the working tree.
//!
//! Why: any checkout's working tree can be a dirty sibling-branch copy or
//! carry untracked WIP, and the auto-prompt dispatcher once ranked plans
//! against such a stale copy. Remote-tracking refs are the canonical
//! push-published state shared by every agent working the repo.
//!
//! Resolution is per file, not per branch: each plan file is read from the
//! candidate ref where it was last touched (committer date), so a hotfix that
//! only landed on `origin/main` wins over an older `origin/develop` copy of
//! the same file even when `origin/develop`'s tip is newer overall — and vice
//! versa. Hardcoding a single branch loses in this workspace: `origin/main`
//! is upstream Zed with no `.plans/` at all, while `origin/develop` is the
//! active integration line.
//!
//! All git access is async (`smol::process`): callers on the main thread must
//! use the cached accessor or prewarm via the async path.

use futures::future::{Either, select};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use smol::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use smol::process::{Child, Command};

/// Byte cap per plan file — parity with the worktree reader in
/// `auto_prompt::read_plan_files`.
const MAX_PLAN_FILE_BYTES: u64 = 100_000;

/// How long a built snapshot stays valid. Bounds git spawn rate when the
/// dispatcher re-decides on every stop; staleness of seconds is irrelevant
/// against the hours-stale working-tree trap this module fixes.
const SNAPSHOT_TTL_SECS: f64 = 30.0;

/// Minimum interval between `git fetch origin` attempts per repo. Covers
/// successes and failures alike, so an unreachable remote cannot turn every
/// stop decision into a network retry.
const FETCH_GATE_SECS: f64 = 60.0;

/// Hard wall-clock cap on `git fetch`. The fetch also runs with prompts and
/// credential helpers disabled so it fails fast instead of hanging on auth.
const FETCH_TIMEOUT_SECS: f64 = 10.0;

/// Cap on files per snapshot so a pathological `.plans/` directory cannot
/// exhaust memory or the `cat-file --batch` pipe (requests are written before
/// responses are read; the cap keeps the request block far below the 64KB
/// pipe buffer).
const MAX_SNAPSHOT_FILES: usize = 1024;

/// Candidate remote-tracking refs, most-preferred first. Ties (equal dates)
/// and identical blobs resolve to the earlier entry; presence on a single ref
/// beats preference. `develop` leads because it is this workspace's active
/// integration branch; `main`/`master` cover upstream-style repos. The
/// remote's default branch (via `origin/HEAD`) is appended when resolvable.
const CANDIDATE_REFS: [&str; 3] = ["origin/develop", "origin/main", "origin/master"];

/// A plan file read from an origin ref.
#[derive(Debug, Clone)]
pub(crate) struct OriginPlanFile {
    /// Absolute worktree-shaped path (e.g. `/repo/.plans/346_x.md`). Matches
    /// the format `plan_registry` claim keys use, so cross-agent claims keep
    /// working for origin-read plans.
    pub abs_path: String,
    /// Repo-relative path as stored in the ref's tree.
    pub rel_path: String,
    pub content: String,
    /// Ref the content was read from, e.g. `origin/develop`.
    pub ref_name: String,
    /// Blob hash of the content actually read.
    pub blob: String,
}

struct Snapshot {
    built_at_secs: f64,
    files: Arc<Vec<OriginPlanFile>>,
}

static SNAPSHOTS: Mutex<Option<HashMap<PathBuf, Snapshot>>> = Mutex::new(None);
static FETCH_ATTEMPTS: Mutex<Option<HashMap<PathBuf, f64>>> = Mutex::new(None);

fn time_monotonic_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// List plan files for `work_dir` from origin's remote-tracking refs, serving
/// a fresh cached snapshot when available and rebuilding it (with an optional
/// gated `git fetch origin`) otherwise.
///
/// Returns an empty list when origin carries no plan entries for this
/// directory (non-git dir, missing `git`, no remote refs, or nothing pushed
/// yet) — callers fall back to the worktree reader in that case.
///
/// `fetch_executor` supplies the timer bounding the fetch; `None` skips the
/// fetch entirely and resolves against local remote-tracking refs.
pub(crate) async fn origin_plan_files(
    work_dir: &Path,
    fetch_executor: Option<&gpui::BackgroundExecutor>,
) -> Arc<Vec<OriginPlanFile>> {
    let now = time_monotonic_secs();
    if let Some(cached) = cached_snapshot(work_dir, now) {
        return cached;
    }

    let files = Arc::new(build_origin_plan_files(work_dir, fetch_executor, now).await);
    let mut guard = SNAPSHOTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    guard
        .get_or_insert_with(HashMap::new)
        .insert(work_dir.to_path_buf(), Snapshot {
            built_at_secs: now,
            files: Arc::clone(&files),
        });
    files
}

/// TTL-fresh snapshot without any process spawning — for synchronous
/// main-thread readers that were prewarmed via [`origin_plan_files`].
pub(crate) fn cached_origin_plan_files(work_dir: &Path) -> Option<Arc<Vec<OriginPlanFile>>> {
    cached_snapshot(work_dir, time_monotonic_secs())
}

fn cached_snapshot(work_dir: &Path, now: f64) -> Option<Arc<Vec<OriginPlanFile>>> {
    let guard = SNAPSHOTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let map = guard.as_ref()?;
    let snapshot = map.get(work_dir)?;
    (now - snapshot.built_at_secs < SNAPSHOT_TTL_SECS).then(|| Arc::clone(&snapshot.files))
}

async fn build_origin_plan_files(
    work_dir: &Path,
    fetch_executor: Option<&gpui::BackgroundExecutor>,
    now: f64,
) -> Vec<OriginPlanFile> {
    let Some(repo_root) = git_toplevel(work_dir).await else {
        log::debug!(
            "[auto_prompt::plan_source] {} is not inside a git repository — worktree fallback",
            work_dir.display()
        );
        return Vec::new();
    };

    let Some(prefixes) = plan_prefixes(work_dir, &repo_root) else {
        log::debug!(
            "[auto_prompt::plan_source] cannot map {} onto repo root {} — worktree fallback",
            work_dir.display(),
            repo_root.display()
        );
        return Vec::new();
    };

    let mut candidates = candidate_refs(&repo_root).await;
    if let Some(executor) = fetch_executor {
        fetch_origin_refs(&repo_root, executor, now).await;
        // Refs may have appeared or advanced; re-list after the fetch.
        candidates = candidate_refs(&repo_root).await;
    }
    if candidates.is_empty() {
        log::info!(
            "[auto_prompt::plan_source] {} has no candidate origin refs — worktree fallback",
            repo_root.display()
        );
        return Vec::new();
    }

    // ref name -> (rel path -> blob hash), candidate order preserved.
    let mut per_ref: Vec<(String, HashMap<String, String>)> = Vec::new();
    for ref_name in &candidates {
        match ls_tree_plan_blobs(&repo_root, ref_name, &prefixes).await {
            Some(entries) => per_ref.push((ref_name.clone(), entries)),
            None => {
                log::warn!(
                    "[auto_prompt::plan_source] ls-tree failed for {ref_name} — skipping ref"
                );
            }
        }
    }

    let union_paths: Vec<String> = {
        let mut seen: HashSet<String> = HashSet::new();
        let mut paths: Vec<String> = Vec::new();
        for (_, entries) in &per_ref {
            for path in entries.keys() {
                if seen.insert(path.clone()) {
                    paths.push(path.clone());
                }
            }
        }
        paths.sort();
        paths
    };
    if union_paths.is_empty() {
        log::info!(
            "[auto_prompt::plan_source] no {} entries on any of {:?} — worktree fallback",
            prefixes.join(", "),
            candidates
        );
        return Vec::new();
    }
    if union_paths.len() > MAX_SNAPSHOT_FILES {
        log::warn!(
            "[auto_prompt::plan_source] {} plan files on origin exceeds cap {MAX_SNAPSHOT_FILES} — truncating",
            union_paths.len()
        );
    }

    // Resolve each file to (rel path, ref, blob).
    let mut resolved: Vec<(String, String, String)> = Vec::new();
    for rel_path in union_paths.iter().take(MAX_SNAPSHOT_FILES) {
        resolved
            .push(resolve_file_source(&repo_root, rel_path, &per_ref).await);
    }

    // Batch-read every chosen blob with a single `cat-file --batch`.
    let unique_blobs: Vec<String> = {
        let mut blobs: HashSet<String> = HashSet::new();
        for (_, _, blob) in &resolved {
            blobs.insert(blob.clone());
        }
        blobs.into_iter().collect()
    };
    let contents = cat_file_batch(&repo_root, &unique_blobs).await;

    let mut files = Vec::with_capacity(resolved.len());
    for (rel_path, ref_name, blob) in resolved {
        match contents.get(&blob) {
            None => {
                log::warn!(
                    "[auto_prompt::plan_source] blob {blob} for {rel_path} unreadable — skipping"
                );
                continue;
            }
            Some(None) => {
                log::debug!(
                    "[auto_prompt::plan_source] {rel_path} skipped (over {} bytes, non-UTF-8, or unreadable)",
                    MAX_PLAN_FILE_BYTES
                );
                continue;
            }
            Some(Some(content)) => {
                log::debug!(
                    "[auto_prompt::plan_source] read {rel_path} @ {ref_name} (blob {})",
                    &blob[..blob.len().min(7)]
                );
                files.push(OriginPlanFile {
                    abs_path: work_dir.join(&rel_path).to_string_lossy().to_string(),
                    rel_path,
                    content: content.clone(),
                    ref_name,
                    blob,
                });
            }
        }
    }

    let mut by_ref_counts: HashMap<&str, usize> = HashMap::new();
    for file in &files {
        *by_ref_counts.entry(file.ref_name.as_str()).or_default() += 1;
    }
    let counts = by_ref_counts
        .into_iter()
        .map(|(r, n)| format!("{r}={n}"))
        .collect::<Vec<_>>()
        .join(", ");
    log::info!(
        "[auto_prompt::plan_source] {} origin plans for {}: {counts}",
        files.len(),
        work_dir.display()
    );

    files
}

/// Pick the ref a file's content comes from:
/// 1. identical blob on every ref that has the file (the single-ref case
///    folds into this) → first candidate (preference order);
/// 2. blobs differ → the ref whose last commit touching the file has the
///    newest committer date; ties and unresolvable dates fall back to
///    preference order.
async fn resolve_file_source(
    repo_root: &Path,
    rel_path: &str,
    per_ref: &[(String, HashMap<String, String>)],
) -> (String, String, String) {
    let present: Vec<(&str, &str)> = per_ref
        .iter()
        .filter_map(|(ref_name, entries)| {
            entries.get(rel_path).map(|blob| (ref_name.as_str(), blob.as_str()))
        })
        .collect();
    let Some(&(first_ref, first_blob)) = present.first() else {
        // Union paths are built from per-ref entries, so this cannot happen;
        // an empty source is skipped by the caller's content lookup.
        return (rel_path.to_string(), String::new(), String::new());
    };
    if present.iter().all(|(_, blob)| *blob == first_blob) {
        return (
            rel_path.to_string(),
            first_ref.to_string(),
            first_blob.to_string(),
        );
    }

    let mut best: Option<(chrono::DateTime<chrono::FixedOffset>, &str, &str)> = None;
    for (ref_name, blob) in &present {
        if let Some((date, _commit)) = last_touch(repo_root, ref_name, rel_path).await {
            let replace = best
                .as_ref()
                .map_or(true, |(best_date, _, _)| date > *best_date);
            if replace {
                best = Some((date, ref_name, blob));
            }
        }
    }
    match best {
        Some((_, ref_name, blob)) => {
            (rel_path.to_string(), ref_name.to_string(), blob.to_string())
        }
        None => (
            rel_path.to_string(),
            first_ref.to_string(),
            first_blob.to_string(),
        ),
    }
}

/// Last commit (committer date + hash) that touched `rel_path` on `ref_name`.
async fn last_touch(
    repo_root: &Path,
    ref_name: &str,
    rel_path: &str,
) -> Option<(chrono::DateTime<chrono::FixedOffset>, String)> {
    let out = git_output(
        repo_root,
        &[
            "log",
            "-1",
            "--no-color",
            "--format=%cI %H",
            ref_name,
            "--",
            rel_path,
        ],
    )
    .await?;
    let line = out.lines().next()?.trim();
    let (date, commit) = line.split_once(' ')?;
    let date = chrono::DateTime::parse_from_rfc3339(date).ok()?;
    Some((date, commit.to_string()))
}

async fn git_toplevel(work_dir: &Path) -> Option<PathBuf> {
    let out = git_output(work_dir, &["rev-parse", "--show-toplevel"]).await?;
    Some(PathBuf::from(out.trim_end()))
}

/// Git pathspecs (with trailing `/`) for the plan directories under
/// `work_dir`, expressed relative to the repo root so a work_dir that is a
/// subdirectory resolves correctly.
fn plan_prefixes(work_dir: &Path, repo_root: &Path) -> Option<Vec<String>> {
    let rel = work_dir.strip_prefix(repo_root).ok()?;
    let rel = rel.to_string_lossy();
    let rel = rel.trim_end_matches('/');
    Some(
        [".plan", ".plans"]
            .iter()
            .map(|dir| match rel.is_empty() {
                true => format!("{dir}/"),
                false => format!("{rel}/{dir}/"),
            })
            .collect(),
    )
}

/// Candidate refs that exist as remote-tracking refs, in preference order,
/// plus the remote default (`origin/HEAD`) when it points somewhere new.
async fn candidate_refs(repo_root: &Path) -> Vec<String> {
    let mut refs: Vec<String> = Vec::new();
    let Some(show_ref) = git_output(repo_root, &["show-ref"]).await else {
        return refs;
    };
    let existing: HashSet<&str> = show_ref
        .lines()
        .filter_map(|line| line.split_once(' ').map(|(_, r)| r.trim()))
        .collect();
    for candidate in CANDIDATE_REFS {
        if existing.contains(&*format!("refs/remotes/{candidate}")) {
            refs.push(candidate.to_string());
        }
    }
    if let Some(head) = git_output(repo_root, &["symbolic-ref", "refs/remotes/origin/HEAD"]).await
    {
        // Output looks like `refs/remotes/origin/trunk`.
        if let Some(default) = head.trim().strip_prefix("refs/remotes/") {
            if !refs.iter().any(|r| r == default) {
                refs.push(default.to_string());
            }
        }
    }
    refs
}

/// `ls-tree` blob entries directly inside the plan directories of `ref_name`.
/// Returns `None` only when git itself fails; an empty map means the ref has
/// no plan entries.
async fn ls_tree_plan_blobs(
    repo_root: &Path,
    ref_name: &str,
    prefixes: &[String],
) -> Option<HashMap<String, String>> {
    let mut args: Vec<String> = vec![
        "ls-tree".into(),
        "-z".into(),
        "--full-name".into(),
        ref_name.into(),
        "--".into(),
    ];
    args.extend(prefixes.iter().cloned());
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = git_output(repo_root, &arg_refs).await?;

    let mut entries = HashMap::new();
    for record in out.split('\0') {
        if record.is_empty() {
            continue;
        }
        // "<mode> <type> <hash>\t<path>"
        let Some((meta, path)) = record.split_once('\t') else {
            continue;
        };
        let mut parts = meta.split_whitespace();
        let object_type = parts.nth(1);
        let hash = parts.next();
        if let (Some("blob"), Some(hash)) = (object_type, hash) {
            entries.insert(path.to_string(), hash.to_string());
        }
        // Subdirectories and submodules are not plan files; the worktree
        // reader only takes flat files too.
    }
    Some(entries)
}

/// Read many blobs with one `git cat-file --batch`. Returns blob hash →
/// content; oversized or unreadable blobs map to `None`.
async fn cat_file_batch(
    repo_root: &Path,
    blobs: &[String],
) -> HashMap<String, Option<String>> {
    let mut result = HashMap::with_capacity(blobs.len());
    if blobs.is_empty() {
        return result;
    }

    let mut child = {
        let mut command = git_command(repo_root, &["cat-file", "--batch"]);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        match command.spawn() {
            Ok(child) => child,
            Err(err) => {
                log::warn!("[auto_prompt::plan_source] cannot spawn git cat-file: {err}");
                return result;
            }
        }
    };

    // Requests stay far below the pipe buffer (see MAX_SNAPSHOT_FILES), so
    // writing them all before reading any response cannot deadlock.
    if let Some(mut stdin) = child.stdin.take() {
        let mut requests = String::new();
        for blob in blobs {
            requests.push_str(blob);
            requests.push('\n');
        }
        if let Err(err) = stdin.write_all(requests.as_bytes()).await {
            log::warn!("[auto_prompt::plan_source] cat-file request write failed: {err}");
        }
        if let Err(err) = stdin.flush().await {
            log::warn!("[auto_prompt::plan_source] cat-file flush failed: {err}");
        }
    }
    // stdin is dropped here — EOF tells git to exit after the last response.

    let stdout = child.stdout.take();
    let mut reader = stdout.map(|out| BufReader::with_capacity(256 * 1024, out));
    for blob in blobs {
        let Some(reader) = reader.as_mut() else {
            break;
        };
        let mut header = String::new();
        if reader.read_line(&mut header).await.unwrap_or(0) == 0 {
            break;
        }
        let header = header.trim_end();
        let mut parts = header.split_whitespace();
        let (returned_hash, object_type, size_str) =
            (parts.next(), parts.next(), parts.next());
        let (Some(returned_hash), Some(object_type), Some(size_str)) =
            (returned_hash, object_type, size_str)
        else {
            // "<hash> missing" or garbage: no payload follows.
            continue;
        };
        if returned_hash != blob.as_str() || object_type != "blob" {
            continue;
        }
        let Ok(size) = size_str.parse::<u64>() else {
            continue;
        };
        let content = if size > MAX_PLAN_FILE_BYTES {
            // Must still consume the payload to keep the stream aligned.
            read_and_discard(reader, size).await;
            None
        } else {
            let mut bytes = vec![0u8; size as usize];
            match reader.read_exact(&mut bytes).await {
                // Invalid UTF-8 is skipped, matching the worktree reader's
                // read_to_string behavior (binary strays in .plans/ such as
                // screenshots must not pollute the dispatcher context).
                Ok(()) => String::from_utf8(bytes).ok(),
                Err(_) => None,
            }
        };
        // Trailing newline after each payload.
        let mut newline = [0u8; 1];
        let _ = reader.read_exact(&mut newline).await;
        result.insert(blob.clone(), content);
    }

    if let Err(err) = child.status().await {
        log::warn!("[auto_prompt::plan_source] git cat-file status failed: {err}");
    }
    result
}

/// Consume `size` bytes from `reader`. Oversized plan blobs are skipped this
/// way without buffering them; stream errors abort the read.
async fn read_and_discard<R: AsyncRead + Unpin + ?Sized>(
    reader: &mut R,
    size: u64,
) -> Option<()> {
    let mut remaining = size;
    let mut sink = [0u8; 8192];
    while remaining > 0 {
        let want = remaining.min(sink.len() as u64) as usize;
        match reader.read(&mut sink[..want]).await {
            Ok(0) | Err(_) => return None,
            Ok(n) => remaining -= n as u64,
        }
    }
    Some(())
}

fn should_fetch(repo_root: &Path, now: f64) -> bool {
    let mut guard = FETCH_ATTEMPTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    let expired = match map.get(repo_root) {
        Some(last) => now - *last >= FETCH_GATE_SECS,
        None => true,
    };
    if expired {
        map.insert(repo_root.to_path_buf(), now);
    }
    expired
}

/// Best-effort `git fetch origin`: prompts and credential helpers disabled,
/// hard timeout (via the gpui timer, since `smol::Timer` is disallowed in
/// this workspace), failures logged and swallowed — the caller proceeds with
/// local remote-tracking refs either way.
async fn fetch_origin_refs(
    repo_root: &Path,
    executor: &gpui::BackgroundExecutor,
    now: f64,
) {
    if !should_fetch(repo_root, now) {
        return;
    }

    let mut child: Child = {
        let mut command = git_command(repo_root, &["-c", "credential.helper="]);
        command
            .args(["fetch", "--quiet", "--no-tags", "origin"])
            .env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes -o ConnectTimeout=5")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        match command.spawn() {
            Ok(child) => child,
            Err(err) => {
                log::warn!("[auto_prompt::plan_source] cannot spawn git fetch: {err}");
                return;
            }
        }
    };

    let timer = executor.timer(Duration::from_secs_f64(FETCH_TIMEOUT_SECS));
    let wait = Box::pin(child.status());
    match select(wait, Box::pin(timer)).await {
        Either::Left((status, _timer)) => {
            match status {
                Ok(status) if status.success() => log::info!(
                    "[auto_prompt::plan_source] fetched origin for {}",
                    repo_root.display()
                ),
                Ok(status) => log::warn!(
                    "[auto_prompt::plan_source] git fetch failed for {} ({status})",
                    repo_root.display()
                ),
                Err(err) => {
                    log::warn!("[auto_prompt::plan_source] git fetch status failed: {err}")
                }
            }
        }
        Either::Right(((), pending_status)) => {
            // Drop the status borrow before killing; then reap the child.
            drop(pending_status);
            if let Err(err) = child.kill() {
                log::warn!("[auto_prompt::plan_source] git fetch kill failed: {err}");
            }
            let _ = child.status().await; // Reaping; status is irrelevant after a kill.
            log::warn!(
                "[auto_prompt::plan_source] git fetch timed out after {FETCH_TIMEOUT_SECS}s for {} — using local refs",
                repo_root.display()
            );
        }
    }
}

fn git_command(repo: &Path, args: &[&str]) -> Command {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0");
    command
}

async fn git_output(repo: &Path, args: &[&str]) -> Option<String> {
    let output: Output = git_command(repo, args).output().await.ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::future::BoxFuture;
    use smol::process::Command as SmolCommand;
    use tempfile::TempDir;

    struct Fixture {
        _tmp: TempDir,
        work: PathBuf,
    }

    fn have_git() -> bool {
        smol::block_on(async {
            SmolCommand::new("git")
                .arg("--version")
                .output()
                .await
                .map(|out| out.status.success())
                .unwrap_or(false)
        })
    }

    fn git(dir: &Path, args: &[&str], date: Option<&str>) -> BoxFuture<'static, Output> {
        let mut command = SmolCommand::new("git");
        command.arg("-C").arg(dir).args(args);
        command
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t");
        if let Some(date) = date {
            command
                .env("GIT_AUTHOR_DATE", date)
                .env("GIT_COMMITTER_DATE", date);
        }
        Box::pin(async move { command.output().await.expect("git spawn") })
    }

    fn git_ok(dir: &Path, args: &[&str], date: Option<&str>) -> BoxFuture<'static, ()> {
        let dir = dir.to_path_buf();
        let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
        let date = date.map(str::to_string);
        Box::pin(async move {
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            let out = git(&dir, &refs, date.as_deref()).await;
            assert!(
                out.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        })
    }

    /// Origin with `main` and `develop` carrying deliberately divergent plan
    /// contents and dates, plus a checked-out `feature` branch.
    ///
    /// main:    001@"main v1"@08-20, 007 shared, 004@08-22, 005 hotfix@08-26
    /// develop: 001@"develop v2"@08-25 (newer), 002, 005@"dev five"@08-25
    fn pushed_fixture() -> Fixture {
        smol::block_on(async {
            let tmp = TempDir::new().expect("tempdir");
            let origin = tmp.path().join("origin.git");
            let work = tmp.path().join("work");
            std::fs::create_dir_all(&work).expect("mkdir work");
            git_ok(
                tmp.path(),
                &["init", "-q", "--bare", "-b", "main", "origin.git"],
                None,
            )
            .await;
            git_ok(&work, &["init", "-q", "-b", "main"], None).await;
            git_ok(
                &work,
                &[
                    "remote",
                    "add",
                    "origin",
                    origin.to_str().expect("utf8 path"),
                ],
                None,
            )
            .await;

            let plans = work.join(".plans");
            std::fs::create_dir(&plans).expect("mkdir plans");

            // main base @ 2026-08-20.
            std::fs::write(plans.join("001_a.md"), "main v1").expect("write");
            std::fs::write(plans.join("007_e.md"), "shared seven").expect("write");
            commit_all(&work, "main base", "2026-08-20T10:00:00+00:00").await;
            git_ok(&work, &["push", "-q", "origin", "main"], None).await;

            // develop @ 2026-08-25: newer 001, develop-only 002 and 005.
            git_ok(&work, &["checkout", "-q", "-b", "develop"], None).await;
            std::fs::write(plans.join("001_a.md"), "develop v2").expect("write");
            std::fs::write(plans.join("002_b.md"), "dev two").expect("write");
            std::fs::write(plans.join("005_c.md"), "dev five").expect("write");
            commit_all(&work, "develop updates", "2026-08-25T10:00:00+00:00").await;
            git_ok(&work, &["push", "-q", "origin", "develop"], None).await;

            // main again: 004 @ 08-22, then a 005 hotfix @ 08-26 — newer than
            // develop's copy and than develop's tip, proving per-file
            // resolution rather than branch-tip selection.
            git_ok(&work, &["checkout", "-q", "main"], None).await;
            std::fs::write(plans.join("004_m.md"), "main four").expect("write");
            commit_all(&work, "main four", "2026-08-22T10:00:00+00:00").await;
            std::fs::write(plans.join("005_c.md"), "main five hotfix").expect("write");
            commit_all(&work, "main five hotfix", "2026-08-26T10:00:00+00:00").await;
            git_ok(&work, &["push", "-q", "origin", "main"], None).await;

            git_ok(&work, &["checkout", "-q", "-b", "feature"], None).await;
            Fixture { _tmp: tmp, work }
        })
    }

    /// Dirty the `feature` checkout: stale edit over 001 plus an untracked
    /// WIP file.
    fn dirty_checkout(fixture: &Fixture) {
        std::fs::write(
            fixture.work.join(".plans").join("001_a.md"),
            "STALE LOCAL",
        )
        .expect("write stale");
        std::fs::write(
            fixture.work.join(".plans").join("003_local.md"),
            "untracked wip",
        )
        .expect("write untracked");
    }

    fn commit_all(work: &Path, message: &str, date: &str) -> BoxFuture<'static, ()> {
        let work = work.to_path_buf();
        let message = message.to_string();
        let date = date.to_string();
        Box::pin(async move {
            git_ok(&work, &["add", "-A"], None).await;
            git_ok(
                &work,
                &["commit", "-q", "-m", &message],
                Some(&date),
            )
            .await;
        })
    }

    fn by_rel(files: &[OriginPlanFile]) -> HashMap<String, OriginPlanFile> {
        files
            .iter()
            .cloned()
            .map(|file| (file.rel_path.clone(), file))
            .collect()
    }

    /// THE guard test: a dirty non-main checkout must yield per-file newest
    /// origin contents, ignoring the working tree entirely.
    #[test]
    fn guard_dirty_checkout_yields_per_file_newest_origin_contents() {
        if !have_git() {
            return;
        }
        let fixture = pushed_fixture();
        dirty_checkout(&fixture);
        let files = smol::block_on(origin_plan_files(&fixture.work, None));
        let by_rel = by_rel(&files);

        // Stale working-tree copy loses to origin/develop's newer commit.
        let file_001 = by_rel.get(".plans/001_a.md").expect("001 present");
        assert_eq!(file_001.content, "develop v2");
        assert_eq!(file_001.ref_name, "origin/develop");

        // Develop-only file is visible even though absent from the checked
        // out branch.
        let file_002 = by_rel.get(".plans/002_b.md").expect("002 present");
        assert_eq!(file_002.content, "dev two");

        // Untracked local file must not be visible.
        assert!(!by_rel.contains_key(".plans/003_local.md"));

        // Main-only file is included even though develop exists.
        let file_004 = by_rel.get(".plans/004_m.md").expect("004 present");
        assert_eq!(file_004.content, "main four");
        assert_eq!(file_004.ref_name, "origin/main");

        // Per-file newest: main's 08-26 hotfix beats develop's 08-25 copy
        // even though develop is the preferred ref.
        let file_005 = by_rel.get(".plans/005_c.md").expect("005 present");
        assert_eq!(file_005.content, "main five hotfix");
        assert_eq!(file_005.ref_name, "origin/main");

        // Identical blob on both refs resolves via preference order.
        let file_007 = by_rel.get(".plans/007_e.md").expect("007 present");
        assert_eq!(file_007.content, "shared seven");
        assert_eq!(file_007.ref_name, "origin/develop");

        // Claim-key shape: absolute worktree paths under the work dir.
        for file in files.iter() {
            assert!(file
                .abs_path
                .starts_with(fixture.work.to_str().expect("utf8 path")));
        }
    }

    #[test]
    fn oversized_blob_is_skipped() {
        if !have_git() {
            return;
        }
        let fixture = pushed_fixture();
        smol::block_on(async {
            git_ok(&fixture.work, &["checkout", "-q", "develop"], None).await;
            std::fs::write(
                fixture.work.join(".plans").join("008_big.md"),
                "x".repeat(150_000),
            )
            .expect("write big");
            commit_all(&fixture.work, "big", "2026-08-27T10:00:00+00:00").await;
            git_ok(&fixture.work, &["push", "-q", "origin", "develop"], None).await;
        });

        let files = smol::block_on(origin_plan_files(&fixture.work, None));
        let by_rel = by_rel(&files);
        assert!(
            !by_rel.contains_key(".plans/008_big.md"),
            "oversized blob must be skipped"
        );
        // Ordinary files still resolve (001 keeps develop's newer copy).
        assert_eq!(
            by_rel.get(".plans/001_a.md").expect("001").content,
            "develop v2"
        );
    }

    #[test]
    fn subdir_work_dir_resolves_plans_under_its_prefix() {
        if !have_git() {
            return;
        }
        let fixture = pushed_fixture();
        smol::block_on(async {
            git_ok(&fixture.work, &["checkout", "-q", "develop"], None).await;
            let sub_plans = fixture.work.join("sub").join(".plans");
            std::fs::create_dir_all(&sub_plans).expect("mkdir sub plans");
            std::fs::write(sub_plans.join("s1.md"), "sub one").expect("write");
            commit_all(&fixture.work, "sub plans", "2026-08-27T10:00:00+00:00").await;
            git_ok(&fixture.work, &["push", "-q", "origin", "develop"], None).await;
        });

        let files = smol::block_on(origin_plan_files(&fixture.work.join("sub"), None));
        let by_rel = by_rel(&files);
        let s1 = by_rel.get("sub/.plans/s1.md").expect("sub plan visible");
        assert_eq!(s1.content, "sub one");
        // Top-level plans must not leak into a subdir work dir.
        assert!(!by_rel.contains_key(".plans/001_a.md"));
    }

    #[test]
    fn binary_blob_is_skipped() {
        if !have_git() {
            return;
        }
        let fixture = pushed_fixture();
        smol::block_on(async {
            git_ok(&fixture.work, &["checkout", "-q", "develop"], None).await;
            std::fs::write(
                fixture.work.join(".plans").join("009_shot.png"),
                vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0xFF, 0xFE],
            )
            .expect("write png");
            commit_all(&fixture.work, "binary", "2026-08-27T11:00:00+00:00").await;
            git_ok(&fixture.work, &["push", "-q", "origin", "develop"], None).await;
        });

        let files = smol::block_on(origin_plan_files(&fixture.work, None));
        let by_rel = by_rel(&files);
        assert!(
            !by_rel.contains_key(".plans/009_shot.png"),
            "non-UTF-8 blob must be skipped (worktree read_to_string parity)"
        );
    }

    #[test]
    fn non_git_dir_yields_nothing() {
        if !have_git() {
            return;
        }
        let tmp = TempDir::new().expect("tempdir");
        let plans = tmp.path().join(".plans");
        std::fs::create_dir(&plans).expect("mkdir");
        std::fs::write(plans.join("x.md"), "local only").expect("write");
        assert!(smol::block_on(origin_plan_files(tmp.path(), None)).is_empty());
    }

    #[test]
    fn repo_without_remote_refs_yields_nothing() {
        if !have_git() {
            return;
        }
        let tmp = TempDir::new().expect("tempdir");
        let work = tmp.path().join("work");
        std::fs::create_dir_all(work.join(".plans")).expect("mkdir");
        smol::block_on(async {
            git_ok(&work, &["init", "-q", "-b", "main"], None).await;
            std::fs::write(work.join(".plans").join("x.md"), "never pushed").expect("write");
            git_ok(&work, &["add", "-A"], None).await;
            git_ok(
                &work,
                &["commit", "-q", "-m", "x"],
                Some("2026-08-27T10:00:00+00:00"),
            )
            .await;
        });
        assert!(smol::block_on(origin_plan_files(&work, None)).is_empty());
    }
}
