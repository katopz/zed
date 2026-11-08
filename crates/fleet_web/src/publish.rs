//! `fleet-web publish`: mirror this rig's Zed threads into the origin rig's
//! web directory. Incremental: a thread is re-exported only when its
//! `updated_at` moved, and only changed files cross the wire.

use crate::thread_export::{self, ThreadMeta, ThreadRow};
use anyhow::{Context as _, Result, bail};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const INDEX_FILE: &str = "index.json";
/// Files exported but not yet confirmed on the origin; retried next run.
const PENDING_FILE: &str = "pending.txt";

#[derive(Debug, Default, PartialEq)]
pub struct PublishReport {
    pub total: usize,
    pub exported: usize,
    pub removed: usize,
    pub failed: Vec<String>,
    pub shipped: usize,
}

/// `~/Library/Application Support/Zed/threads/threads.db` and its siblings.
pub fn default_threads_db() -> Option<PathBuf> {
    let data_dir = if cfg!(target_os = "macos") {
        std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join("Library/Application Support/Zed"))
    } else if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(|local| PathBuf::from(local).join("Zed"))
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share"))
            })
            .map(|data| data.join("zed"))
    }?;
    Some(data_dir.join("threads/threads.db"))
}

pub fn rig_dir(web_home: &Path, rig: &str) -> PathBuf {
    web_home.join("threads").join(rig)
}

pub fn read_index(dir: &Path) -> Vec<ThreadMeta> {
    fs::read(dir.join(INDEX_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Exports changed threads from `db` into `web_home/threads/<rig>/`.
/// Returns the report and the file names to ship.
pub fn export_changed(
    db: &Path,
    rig: &str,
    web_home: &Path,
) -> Result<(PublishReport, Vec<String>)> {
    let dir = rig_dir(web_home, rig);
    fs::create_dir_all(&dir)?;
    let uri = db.to_str().context("threads db path is not UTF-8")?;
    let connection = sqlez::connection::Connection::open_read_only(uri)
        .with_context(|| format!("open {uri}"))?;
    let rows = connection.select::<(
        String,
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
    )>(
        "SELECT id, summary, updated_at, created_at, parent_id, folder_paths FROM threads",
    )?()?;
    let mut blob = connection.select_row_bound::<&str, (String, Vec<u8>)>(
        "SELECT data_type, data FROM threads WHERE id = ? LIMIT 1",
    )?;

    let previous: BTreeMap<String, ThreadMeta> = read_index(&dir)
        .into_iter()
        .map(|meta| (meta.id.clone(), meta))
        .collect();
    let mut report = PublishReport {
        total: rows.len(),
        ..Default::default()
    };
    let mut changed = Vec::new();
    let mut index = Vec::with_capacity(rows.len());
    for (id, summary, updated_at, created_at, parent_id, folder_paths) in rows {
        if !is_safe_id(&id) {
            continue;
        }
        if let Some(meta) = previous.get(&id)
            && meta.updated_at == updated_at
            && dir.join(format!("{id}.json")).exists()
        {
            index.push(meta.clone());
            continue;
        }
        let row = ThreadRow {
            id: id.clone(),
            summary,
            updated_at,
            created_at,
            parent_id,
            folder_paths,
        };
        let view = blob(&id)
            .and_then(|found| found.context("row vanished"))
            .and_then(|(data_type, data)| thread_export::decode_blob(&data_type, &data))
            .map(|thread| thread_export::export(rig, &row, &thread));
        match view {
            Ok(view) => {
                let file = format!("{id}.json");
                write_atomic(&dir.join(&file), &serde_json::to_vec(&view)?)?;
                index.push(view.meta);
                changed.push(file);
                report.exported += 1;
            }
            Err(error) => report.failed.push(format!("{id}: {error:#}")),
        }
    }
    let live: BTreeSet<&str> = index.iter().map(|meta| meta.id.as_str()).collect();
    for id in previous.keys().filter(|id| !live.contains(id.as_str())) {
        fs::remove_file(dir.join(format!("{id}.json"))).ok();
        report.removed += 1;
    }
    if !changed.is_empty() || report.removed > 0 || previous.len() != index.len() {
        index.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        write_atomic(&dir.join(INDEX_FILE), &serde_json::to_vec(&index)?)?;
        changed.push(INDEX_FILE.to_string());
    }
    Ok((report, changed))
}

/// Ships `files` (plus anything a failed run left pending) to the origin rig
/// as one tar stream over SSH. Records what failed so the next run retries.
pub fn ship(
    mailroom: &agent_mail::Mailroom,
    origin: &str,
    rig: &str,
    web_home: &Path,
    files: Vec<String>,
) -> Result<usize> {
    let dir = rig_dir(web_home, rig);
    let pending_path = dir.join(PENDING_FILE);
    let mut all: BTreeSet<String> = fs::read_to_string(&pending_path)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect();
    all.extend(files);
    all.retain(|file| dir.join(file).exists());
    if all.is_empty() {
        return Ok(0);
    }
    let rigs = mailroom.rigs()?;
    let routes = &rigs
        .rigs
        .get(origin)
        .with_context(|| format!("origin rig {origin:?} is not in rigs.json"))?
        .ssh;
    let remote_dir = format!(".agent-mail/web/threads/{rig}");
    let mut last_error = None;
    for route in routes {
        match send_tar(route, &dir, &remote_dir, &all) {
            Ok(()) => {
                fs::remove_file(&pending_path).ok();
                return Ok(all.len());
            }
            Err(error) => last_error = Some(error),
        }
    }
    let list: Vec<&str> = all.iter().map(String::as_str).collect();
    fs::write(&pending_path, list.join("\n"))?;
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("origin rig {origin:?} has no ssh route")))
}

// A headless std binary: the smol-preferring lint targets async gpui code.
#[allow(clippy::disallowed_methods)]
fn send_tar(
    route: &agent_mail::SshRoute,
    dir: &Path,
    remote_dir: &str,
    files: &BTreeSet<String>,
) -> Result<()> {
    let mut tar = Command::new("tar")
        .arg("-cf")
        .arg("-")
        .arg("-C")
        .arg(dir)
        .args(files)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn tar")?;
    let tar_output = tar.stdout.take().context("tar stdout")?;
    let mut ssh = Command::new("ssh");
    ssh.args([
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=10",
        "-o",
        "StrictHostKeyChecking=accept-new",
    ]);
    if let Some(jump) = route.jump() {
        ssh.args(["-o", &format!("ProxyJump={jump}")]);
    }
    let ssh = ssh
        .arg(route.target())
        .arg(format!(
            "mkdir -p {remote_dir} && tar -xf - -C {remote_dir}"
        ))
        .stdin(Stdio::from(tar_output))
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .context("spawn ssh")?;
    let tar_status = tar.wait()?;
    if !tar_status.success() {
        bail!("tar exit {:?}", tar_status.code());
    }
    if !ssh.status.success() {
        bail!(
            "ssh {route} exit {:?}: {}",
            ssh.status.code(),
            String::from_utf8_lossy(&ssh.stderr).trim()
        );
    }
    Ok(())
}

/// Thread ids are UUIDs; anything else must not become a path component.
pub fn is_safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let temp = path.with_extension("tmp");
    fs::write(&temp, bytes)?;
    fs::rename(&temp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed_db(path: &Path, rows: &[(&str, &str, &str)]) -> Result<()> {
        let connection = sqlez::connection::Connection::open_file(path.to_str().context("utf8")?);
        connection.exec(
            "CREATE TABLE IF NOT EXISTS threads (id TEXT PRIMARY KEY, summary TEXT NOT NULL, updated_at TEXT NOT NULL, data_type TEXT NOT NULL, data BLOB NOT NULL, parent_id TEXT, folder_paths TEXT, folder_paths_order TEXT, created_at TEXT)",
        )?()?;
        connection.exec("DELETE FROM threads")?()?;
        for (id, updated_at, text) in rows {
            let json = serde_json::json!({
                "title": format!("title {id}"),
                "messages": [{"User": {"id": "u", "content": [{"Text": text}]}}]
            });
            let data = zstd::encode_all(serde_json::to_vec(&json)?.as_slice(), 3)?;
            connection.exec_bound::<(&str, &str, Vec<u8>)>(
                "INSERT INTO threads (id, summary, updated_at, data_type, data) VALUES (?, 's', ?, 'zstd', ?)",
            )?((id, updated_at, data))?;
        }
        Ok(())
    }

    #[test]
    fn exports_only_changed_threads_and_drops_deleted_ones() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let db = dir.path().join("threads.db");
        let web = dir.path().join("web");
        seed_db(
            &db,
            &[
                ("a-1", "2026-11-01T01:00:00+00:00", "hi"),
                ("b-2", "2026-11-01T02:00:00+00:00", "yo"),
            ],
        )?;

        let (report, files) = export_changed(&db, "m3", &web)?;
        assert_eq!((report.total, report.exported, report.removed), (2, 2, 0));
        assert_eq!(files, vec!["a-1.json", "b-2.json", INDEX_FILE]);
        let index = read_index(&rig_dir(&web, "m3"));
        assert_eq!(index[0].id, "b-2", "newest first");

        let (report, files) = export_changed(&db, "m3", &web)?;
        assert_eq!(report.exported, 0);
        assert!(files.is_empty(), "nothing changed, nothing to ship");

        seed_db(&db, &[("a-1", "2026-11-01T03:00:00+00:00", "again")])?;
        let (report, files) = export_changed(&db, "m3", &web)?;
        assert_eq!((report.exported, report.removed), (1, 1));
        assert_eq!(files, vec!["a-1.json", INDEX_FILE]);
        assert!(!rig_dir(&web, "m3").join("b-2.json").exists());
        Ok(())
    }

    #[test]
    fn rejects_path_like_ids() {
        assert!(is_safe_id("5abb983b-7b43-4705-9b63-ab43da8ac7c6"));
        assert!(!is_safe_id("../etc"));
        assert!(!is_safe_id("a/b"));
        assert!(!is_safe_id(""));
    }
}
