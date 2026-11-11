//! The boss (plan 056): the watchdog's hourly housekeeping lane.
//!
//! The watchdog's stalled rule deliberately skips a chain that ended with a
//! finished summary, and its idle rule compares rigs against queued jobs, not
//! repo queues — so "ordinary queue unclaimed while the rig is quiet" and
//! "a managed chain stopped over exactly such a queue" are nobody's job. The
//! boss adds those two rules plus stale-mail pickup, reusing the watchdog's
//! incident machinery (fingerprints, `min_ticks`, cooldown, owner escalation).
//! Everything is deterministic and 0-token: the action is a direct
//! `*@rig` mail with a bounded housekeeping contract, not an LLM digest.

use crate::rig::{OWNER_INBOX, RigProbe};
use crate::watch::{Incident, open_item};
use crate::{Mailroom, validate_name};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

/// Boss incidents must persist this many watchdog ticks (10 min each, ~1 h)
/// before the boss acts, so a queue being drained right now never fires.
pub const BOSS_MIN_TICKS: u32 = 6;
/// Mail rots faster than queues: fewer ticks for the unread-mail rule.
pub const BOSS_MAIL_TICKS: u32 = 3;
/// Thread-end markers older than this no longer count as false stops.
pub const THREAD_END_WINDOW: Duration = Duration::from_secs(60 * 60);
/// Mail pending past this grace is stale: delivery and the delivery-time
/// thread resume have both had their chance.
pub const MAIL_GRACE: Duration = Duration::from_secs(30 * 60);
/// The housekeeping run's budget: at most this many checklist units, then a
/// normal summary. Housekeeping is endless; runs are not.
pub const BOSS_BUDGET_UNITS: usize = 5;
/// Marker files older than this are pruned on the next scan.
const THREAD_END_RETENTION: Duration = Duration::from_secs(2 * 60 * 60);
/// Inbox names one unread-mail incident lists before "(+N more)".
const STALE_MAIL_NAMES_SHOWN: usize = 5;

/// `stop_keyword_outcome` classified the stop as a plan-031 terminal
/// (all-deferred) stop — the class that can hide an ordinary queue.
pub const END_TERMINAL: &str = "stop-keyword-terminal";
/// A plain keyword stop after its one housekeeping pass — legitimate.
pub const END_FINAL: &str = "stop-keyword";
/// The keyword stop honored after its corrective round — legitimate.
pub const END_REPEATED: &str = "stop-keyword-repeated";

/// How a managed chain stopped, recorded by auto_prompt for false-stop
/// forensics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThreadEnd {
    pub session: String,
    pub rig: String,
    #[serde(default)]
    pub cwd: String,
    pub ended_by: String,
    pub had_summary: bool,
    /// Unix millis.
    pub at: i64,
}

fn thread_ends_dir(mailroom: &Mailroom) -> PathBuf {
    mailroom.home().join("watch").join("thread_ends")
}

/// Records how a managed chain stopped so the boss can detect false stops.
/// Telemetry on the decide path: resolves the mailroom from the environment
/// and never fails the caller. Tests use [`record_stop`] with an explicit
/// mailroom instead.
pub fn note_stop(session_id: &str, cwd: Option<&str>, ended_by: &str, had_summary: bool) {
    if let Ok(mailroom) = Mailroom::from_env() {
        record_stop(&mailroom, session_id, cwd, ended_by, had_summary);
    }
}

/// [`note_stop`] with an explicit mailroom, so tests need no env mutation.
pub fn record_stop(
    mailroom: &Mailroom,
    session_id: &str,
    cwd: Option<&str>,
    ended_by: &str,
    had_summary: bool,
) {
    let Ok(session) = validate_name(session_id) else {
        return;
    };
    let end = ThreadEnd {
        session: session.to_string(),
        rig: mailroom
            .rigs()
            .map(|rigs| rigs.self_rig)
            .unwrap_or_default(),
        cwd: cwd.unwrap_or_default().to_string(),
        ended_by: ended_by.to_string(),
        had_summary,
        at: chrono::Utc::now().timestamp_millis(),
    };
    let dir = thread_ends_dir(mailroom);
    if fs::create_dir_all(&dir).is_ok()
        && let Ok(bytes) = serde_json::to_vec(&end)
    {
        fs::write(dir.join(format!("{session}.json")), bytes).ok();
    }
    prune_thread_ends(mailroom);
}

fn prune_thread_ends(mailroom: &Mailroom) {
    let cutoff = chrono::Utc::now().timestamp_millis()
        - chrono::Duration::from_std(THREAD_END_RETENTION)
            .unwrap_or_default()
            .num_milliseconds();
    let Ok(entries) = fs::read_dir(thread_ends_dir(mailroom)) else {
        return;
    };
    for entry in entries.flatten() {
        let fresh = fs::read(entry.path())
            .ok()
            .and_then(|bytes| serde_json::from_slice::<ThreadEnd>(&bytes).ok())
            .is_some_and(|end| end.at >= cutoff);
        if !fresh {
            fs::remove_file(entry.path()).ok();
        }
    }
}

/// Fresh thread-end markers, newest first.
pub fn recent_thread_ends(
    mailroom: &Mailroom,
    now: chrono::DateTime<chrono::Utc>,
    window: Duration,
) -> Vec<ThreadEnd> {
    let cutoff = now.timestamp_millis()
        - chrono::Duration::from_std(window)
            .unwrap_or_default()
            .num_milliseconds();
    let Ok(entries) = fs::read_dir(thread_ends_dir(mailroom)) else {
        return Vec::new();
    };
    let mut ends: Vec<ThreadEnd> = entries
        .flatten()
        .filter_map(|entry| {
            fs::read(entry.path())
                .ok()
                .and_then(|bytes| serde_json::from_slice::<ThreadEnd>(&bytes).ok())
        })
        .filter(|end| end.at >= cutoff)
        .collect();
    ends.sort_by_key(|end| std::cmp::Reverse(end.at));
    ends
}

/// Only the terminal/all-deferred stop hides work from the worker by design;
/// the final and repeated stops ran (or had) their housekeeping pass.
pub fn is_false_stop(end: &ThreadEnd) -> bool {
    end.ended_by == END_TERMINAL
}

/// The fleet's queue repos (`~/git/<repo>` everywhere, `E:\git` on Windows).
pub fn tracked_repo(name: &str) -> bool {
    name.starts_with("riir-") || name.starts_with("seal-") || name == "katgpt-rs"
}

/// Whether `cwd` points anywhere inside the repo (`/git/<repo>` marker,
/// slash-normalized so Windows paths compare too).
fn cwd_in_repo(cwd: &str, repo: &str) -> bool {
    let normalized = cwd.replace('\\', "/");
    let needle = format!("/git/{repo}");
    match normalized.find(&needle) {
        Some(index) => {
            let rest = &normalized[index + needle.len()..];
            rest.is_empty() || rest.starts_with('/')
        }
        None => false,
    }
}

/// The tracked repo a workspace root points into: its last component.
fn cwd_repo_name(cwd: &str) -> Option<String> {
    let normalized = cwd.replace('\\', "/");
    let name = normalized.trim_end_matches('/').rsplit('/').next()?;
    tracked_repo(name).then(|| name.to_string())
}

/// One open, unclaimed `- [ ]` item from a repo's `.plans`/`.issues`.
#[derive(Debug, Clone)]
pub struct OpenWork {
    pub repo: String,
    /// The `.plans`/`.issues` folder the item came from.
    pub folder: String,
    pub file: String,
    pub item: String,
    pub modified: std::time::SystemTime,
}

/// Open, unclaimed work across the fleet's repos, newest file first. No cap —
/// `backlog` and the boss rules each consume this at their own width.
pub fn scan_open_work(
    roots: &[PathBuf],
    now: chrono::DateTime<chrono::Utc>,
    exclude_repos: &BTreeSet<String>,
) -> Vec<OpenWork> {
    let mut found: Vec<OpenWork> = Vec::new();
    for root in roots {
        let Ok(repos) = fs::read_dir(root) else {
            continue;
        };
        for repo in repos.flatten() {
            let Some(repo_name) = repo.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if !tracked_repo(&repo_name) || exclude_repos.contains(&repo_name) {
                continue;
            }
            for folder in [".plans", ".issues"] {
                let Ok(files) = fs::read_dir(repo.path().join(folder)) else {
                    continue;
                };
                for file in files.flatten() {
                    let path = file.path();
                    if path.extension().is_none_or(|extension| extension != "md") {
                        continue;
                    }
                    let Ok(text) = fs::read_to_string(&path) else {
                        continue;
                    };
                    let Some(item) = open_item(&text, now) else {
                        continue;
                    };
                    let modified = file
                        .metadata()
                        .and_then(|metadata| metadata.modified())
                        .unwrap_or(std::time::UNIX_EPOCH);
                    found.push(OpenWork {
                        repo: repo_name.clone(),
                        folder: folder.to_string(),
                        file: file.file_name().to_string_lossy().into_owned(),
                        item,
                        modified,
                    });
                }
            }
        }
    }
    found.sort_by_key(|work| std::cmp::Reverse(work.modified));
    found
}

/// Actionable item count per repo, for the fleet cache that feeds worker
/// context. The boss rules consume the full [`OpenWork`] list instead.
pub fn actionable_issue_counts(
    roots: &[PathBuf],
    now: chrono::DateTime<chrono::Utc>,
    exclude_repos: &BTreeSet<String>,
) -> BTreeMap<String, usize> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for work in scan_open_work(roots, now, exclude_repos) {
        *counts.entry(work.repo).or_default() += 1;
    }
    counts
}

/// An inbox holding pending mail whose addressee is not live anywhere.
#[derive(Debug, Clone, PartialEq)]
pub struct StaleMail {
    pub inbox: String,
    pub pending: usize,
    /// Age of the oldest pending message, in minutes.
    pub oldest_age_min: i64,
}

/// Inboxes the boss never re-routes: the wildcard/watering holes, the
/// managed-job mailbox, and the Claude-waiter `watch-*` inboxes (hundreds on
/// a long-lived rig, stale by design).
pub fn is_system_inbox(name: &str) -> bool {
    name.starts_with('_') || name == "jobs" || name.starts_with("watch-")
}

/// Pending mail past `grace` in inboxes whose addressee is not live on any
/// rig: the poller only claims live sessions and the delivery-time thread
/// resume already ran, so this mail is stuck. The owner's own inbox and the
/// local OS user's inbox are excluded. Newest-oldest first.
pub fn stale_mail(
    mailroom: &Mailroom,
    now: chrono::DateTime<chrono::Utc>,
    grace: Duration,
    live_names: &BTreeSet<String>,
) -> Vec<StaleMail> {
    let local_user = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_default();
    let mut found = Vec::new();
    for name in mailroom.pending_names() {
        if is_system_inbox(&name)
            || name == OWNER_INBOX
            || name == local_user
            || live_names.contains(&name)
        {
            continue;
        }
        let Ok(envelopes) = mailroom.peek(std::slice::from_ref(&name)) else {
            continue;
        };
        let oldest = envelopes
            .iter()
            .filter_map(|envelope| chrono::DateTime::parse_from_rfc3339(&envelope.sent_at).ok())
            .map(|sent| now.signed_duration_since(sent.with_timezone(&chrono::Utc)))
            .max();
        let Some(age) = oldest else {
            continue;
        };
        if age.num_seconds() < grace.as_secs() as i64 {
            continue;
        }
        found.push(StaleMail {
            inbox: name,
            pending: envelopes.len(),
            oldest_age_min: age.num_minutes(),
        });
    }
    found.sort_by_key(|mail| std::cmp::Reverse(mail.oldest_age_min));
    found
}

/// The boss rules (plan 056). Pure over the fleet snapshot, the open-work
/// scan, the thread-end markers and the stale-mail scan, so every rule is
/// unit-testable without a fleet.
pub fn detect<'a>(
    online: &[(&'a str, &'a RigProbe)],
    rigs: &crate::Rigs,
    open_work: &[OpenWork],
    thread_ends: &[ThreadEnd],
    stale_mail: &[StaleMail],
    now: chrono::DateTime<chrono::Utc>,
    local_live_agents: bool,
) -> Vec<Incident> {
    let mut incidents = Vec::new();
    // Where the boss can act today: a QUIET rig that accepts jobs (m3 takes
    // none). Lowest name wins, so the choice is deterministic.
    let quiet_target = online
        .iter()
        .filter(|(name, probe)| {
            probe.quiet && rigs.rigs.get(*name).is_none_or(|rig| rig.accepts_jobs)
        })
        .map(|(name, _)| (*name).to_string())
        .min();

    let mut by_repo: BTreeMap<&str, Vec<&OpenWork>> = BTreeMap::new();
    for work in open_work {
        by_repo.entry(work.repo.as_str()).or_default().push(work);
    }

    // Claimed-and-worked is accounted; claimed-but-stopped is not, so a repo
    // only counts as busy while a live agent or a heavy process sits in it.
    let repo_is_worked = |repo: &str| {
        online.iter().any(|(_, probe)| {
            probe
                .agents
                .iter()
                .any(|agent| cwd_in_repo(&agent.cwd, repo))
                || (!probe.quiet
                    && probe.contenders.iter().any(|contender| {
                        contender
                            .cwd
                            .as_deref()
                            .is_some_and(|cwd| cwd_in_repo(cwd, repo))
                    }))
        })
    };

    if let Some(target) = quiet_target.as_deref() {
        for (repo, works) in &by_repo {
            if repo_is_worked(repo) {
                continue;
            }
            let examples = works
                .iter()
                .take(3)
                .map(|work| format!("{}/{}/{}", work.repo, work.folder, work.file))
                .collect::<Vec<_>>()
                .join(", ");
            incidents.push(Incident {
                fingerprint: format!("boss:queue:{repo}"),
                detail: format!(
                    "{repo} holds {} actionable item(s) with no live agent in the repo ({examples}); \
                     wake a free agent on {target} with the boss housekeeping contract",
                    works.len(),
                ),
                min_ticks: BOSS_MIN_TICKS,
            });
        }

        // A managed chain that stopped over still-open work hid the ordinary
        // queue from its worker — the exact false stop this lane exists for.
        let window_ms = chrono::Duration::from_std(THREAD_END_WINDOW)
            .unwrap_or_default()
            .num_milliseconds();
        for end in thread_ends {
            if !is_false_stop(end) || now.timestamp_millis() - end.at > window_ms {
                continue;
            }
            let Some(repo) = cwd_repo_name(&end.cwd) else {
                continue;
            };
            let Some(works) = by_repo.get(repo.as_str()) else {
                continue;
            };
            incidents.push(Incident {
                fingerprint: format!("boss:false-stop:{}", end.session),
                detail: format!(
                    "session {} on {} ended `{}` while {repo} still holds {} actionable item(s) — \
                     the terminal stop hid the ordinary queue; wake a free agent with the boss \
                     housekeeping contract",
                    end.session,
                    end.rig,
                    end.ended_by,
                    works.len(),
                ),
                min_ticks: BOSS_MIN_TICKS,
            });
        }
    }

    // Stale mail with a live Zed somewhere is the boss's to handle; with no
    // Zed at all, the rig's `zed-wake` job owns it.
    if local_live_agents && !stale_mail.is_empty() {
        let shown = stale_mail
            .iter()
            .take(STALE_MAIL_NAMES_SHOWN)
            .map(|mail| format!("{} ×{} ({} min)", mail.inbox, mail.pending, mail.oldest_age_min))
            .collect::<Vec<_>>()
            .join(", ");
        let hidden = stale_mail.len().saturating_sub(STALE_MAIL_NAMES_SHOWN);
        incidents.push(Incident {
            fingerprint: "boss:unread-mail".into(),
            detail: format!(
                "{} inbox(es) hold pending mail past the grace with no live addressee: \
                 {shown}{}; wake a free agent to read, answer or re-route it",
                stale_mail.len(),
                if hidden > 0 {
                    format!(" (+{hidden} more)")
                } else {
                    String::new()
                },
            ),
            min_ticks: BOSS_MAIL_TICKS,
        });
    }

    incidents
}

/// The bounded contract a boss-woken agent runs. Bounded because housekeeping
/// is endless: after `budget` checklist units the agent ends with a normal
/// summary, and its own all-deferred stop is then the correct ending.
pub fn housekeeping_prompt(detail: &str, rig: &str, budget: usize) -> String {
    format!(
        "You are the boss-run housekeeping agent on {rig}. One bounded run: at most {budget} \
         checklist units, then end the turn with a normal ## Summary (facts only). Claim each \
         item you take with a `**Claimed:** <name>, <unix-ms>, expires <RFC3339 +2h>` line in \
         the doc you touch, so the fleet sees the lease.\n\
         \n\
         Boss incident: {detail}\n\
         \n\
         Checklist, in priority order — skip anything genuinely gated, never invent work:\n\
         1. Git hygiene: fast-forward sync the fleet repos (`~/git`; `E:/git` on Windows); prune \
         merged branches, worktrees not in use as any process cwd, and stale `/tmp/<plan>` build \
         dirs. Never drop stashes — report them. bmo has no Zed: its sweep runs from here via \
         `agent-mail rig run bmo --shared -- <command>`.\n\
         2. Boundary check-up: run the boundary-guard check for each repo you touch; fix or file \
         drift.\n\
         3. Repo queues: work `.issues`/`.plans` items — resume or resolve until done or gated; \
         update status lines as you go.\n\
         4. Mail: drain the stale inbox this incident names — read, answer or re-route; flag \
         claim collisions to the other agent.\n\
         5. Benchmarks: if a repo you touched is dirty or has a perf-relevant change, run its \
         benchmark routine and record the numbers; never defer a benchmark task.\n\
         6. Compaction: collapse long-gone issue narratives in history .md, AGENTS.md and code \
         comments — keep the fact and the critical note, drop the story (token cost).\n\
         7. `cargo refine fix` tiers on fresh clippy findings in the repos you touched.\n\
         8. Research, only within leftover budget: one perf/sec or corpus improvement, filed as \
         an issue.\n\
         \n\
         Gated decisions: ask Claude for the verdict and record it — the only owner gate is a \
         credential/secret touch to deploy. Then stop cleanly: a lone `<stop>` after the summary."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Address;
    use std::path::Path;

    fn room(dir: &Path) -> Mailroom {
        let room = Mailroom::new(dir.to_path_buf());
        room.init("amo").unwrap();
        room
    }

    fn send_mail(room: &Mailroom, name: &str, body: &str) {
        let to = Address::parse(&format!("{name}@amo")).unwrap();
        let envelope = room
            .compose(&to, "peer@amo", body.to_string(), None)
            .unwrap();
        room.send(&envelope).unwrap();
    }

    /// Rewrites every pending envelope in `name`'s inbox to look `minutes` old.
    fn age_mail(room: &Mailroom, name: &str, minutes: i64) {
        let new = room.home().join("inbox").join(name).join("new");
        let sent = (chrono::Utc::now() - chrono::Duration::minutes(minutes)).to_rfc3339();
        for file in fs::read_dir(&new).unwrap().flatten() {
            let mut envelope: serde_json::Value =
                serde_json::from_slice(&fs::read(file.path()).unwrap()).unwrap();
            envelope["sent_at"] = serde_json::Value::String(sent.clone());
            fs::write(file.path(), serde_json::to_vec(&envelope).unwrap()).unwrap();
        }
    }

    fn probe(json: &str) -> RigProbe {
        serde_json::from_str(json).unwrap()
    }

    fn open_work(repo: &str, file: &str) -> OpenWork {
        OpenWork {
            repo: repo.to_string(),
            folder: ".issues".to_string(),
            file: file.to_string(),
            item: "do the thing".to_string(),
            modified: std::time::UNIX_EPOCH,
        }
    }

    fn rigs() -> crate::Rigs {
        serde_json::from_str(
            r#"{"self":"amo","rigs":{"amo":{"ssh":[]},"bmo":{"ssh":[]},"m3":{"ssh":[],"accepts_jobs":false}}}"#,
        )
        .unwrap()
    }

    #[test]
    fn stop_markers_roundtrip_and_expire() {
        let dir = tempfile::tempdir().unwrap();
        let room = room(dir.path());
        record_stop(&room, "sess-1234", Some("/Users/k/git/katgpt-rs"), END_TERMINAL, true);
        record_stop(&room, "sess-5678", None, END_FINAL, true);

        let ends = recent_thread_ends(&room, chrono::Utc::now(), THREAD_END_WINDOW);
        assert_eq!(ends.len(), 2);
        assert_eq!(ends[0].session, "sess-5678", "newest first");
        assert!(is_false_stop(&ends[1]));
        assert!(!is_false_stop(&ends[0]));

        // A marker written outside the window no longer surfaces.
        let stale_path = thread_ends_dir(&room).join("sess-old.json");
        let mut old = ends[0].clone();
        old.session = "sess-old".into();
        old.at -= chrono::Duration::hours(2).num_milliseconds();
        fs::write(&stale_path, serde_json::to_vec(&old).unwrap()).unwrap();
        let ends = recent_thread_ends(&room, chrono::Utc::now(), THREAD_END_WINDOW);
        assert!(ends.iter().all(|end| end.session != "sess-old"));
    }

    #[test]
    fn scan_open_work_honors_status_claims_and_excludes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("git");
        for repo in ["katgpt-rs", "seal-game-editor", "zed"] {
            fs::create_dir_all(root.join(repo).join(".issues")).unwrap();
        }
        let write = |repo: &str, file: &str, text: &str| {
            fs::write(root.join(repo).join(".issues").join(file), text).unwrap();
        };
        write(
            "katgpt-rs",
            "001_done.md",
            "# one\nStatus: done — merged\n\n- [ ] leftover",
        );
        write("katgpt-rs", "002_open.md", "# two\nStatus: open\n\n- [ ] first task");
        write(
            "katgpt-rs",
            "003_live_lease.md",
            "# three\nStatus: open\n**Claimed:** zed-abc, 1, expires 2999-01-01T00:00:00+00:00\n\n- [ ] claimed now",
        );
        write(
            "katgpt-rs",
            "004_expired_lease.md",
            "# four\nStatus: open\n**Claimed:** zed-abc, 1, expires 2020-01-01T00:00:00+00:00\n\n- [ ] claimed but stopped",
        );
        write(
            "seal-game-editor",
            "005_excluded.md",
            "# five\nStatus: open\n\n- [ ] excluded repo",
        );
        write("zed", "006_not_tracked.md", "# six\nStatus: open\n\n- [ ] not tracked");

        let exclude = BTreeSet::from(["seal-game-editor".to_string()]);
        let work = scan_open_work(std::slice::from_ref(&root), chrono::Utc::now(), &exclude);
        let mut files: Vec<&str> = work.iter().map(|w| w.file.as_str()).collect();
        files.sort_unstable();
        assert_eq!(
            files,
            vec!["002_open.md", "004_expired_lease.md"],
            "done, live-lease and excluded repos are skipped; expired lease counts (claimed-but-stopped)"
        );

        let counts = actionable_issue_counts(&[root], chrono::Utc::now(), &exclude);
        assert_eq!(counts.get("katgpt-rs"), Some(&2));
    }

    #[test]
    fn queue_incident_needs_unworked_repo_and_quiet_target() {
        let rigs = rigs();
        let amo = probe(r#"{"rig":"amo","os":"macos","at":"","cores":8,"quiet":true}"#);
        let bmo = probe(r#"{"rig":"bmo","os":"macos","at":"","cores":8,"quiet":true}"#);
        let work = vec![open_work("katgpt-rs", "a.md"), open_work("katgpt-rs", "b.md")];
        let now = chrono::Utc::now();

        let online = vec![("amo", &amo), ("bmo", &bmo)];
        let incidents = detect(&online, &rigs, &work, &[], &[], now, false);
        assert_eq!(incidents.len(), 1);
        assert_eq!(incidents[0].fingerprint, "boss:queue:katgpt-rs");
        assert_eq!(incidents[0].min_ticks, BOSS_MIN_TICKS);
        assert!(incidents[0].detail.contains("2 actionable item(s)"));
        assert!(incidents[0].detail.contains("on amo"), "lowest quiet rig name wins");

        // An agent sitting in the repo means it is being worked.
        let busy_repo = probe(
            r#"{"rig":"amo","os":"macos","at":"","cores":8,"quiet":true,
                "agents":[{"name":"katgpt-rs-1","status":"idle","cwd":"/Users/k/git/katgpt-rs/crates/x"}]}"#,
        );
        let online = vec![("amo", &busy_repo), ("bmo", &bmo)];
        assert!(detect(&online, &rigs, &work, &[], &[], now, false).is_empty());

        // A busy contender in the repo counts as worked too.
        let compiling = probe(
            r#"{"rig":"amo","os":"macos","at":"","cores":8,"quiet":false,
                "contenders":[{"pid":1,"name":"cargo","cpu_pct":90.0,"rss_gb":1.0,"arm":"name","cwd":"/Users/k/git/katgpt-rs"}]}"#,
        );
        let online = vec![("amo", &compiling), ("bmo", &bmo)];
        assert!(detect(&online, &rigs, &work, &[], &[], now, false).is_empty());

        // No quiet rig that accepts jobs: nothing to fire into.
        let m3_only = probe(r#"{"rig":"m3","os":"macos","at":"","cores":8,"quiet":true}"#);
        let online = vec![("m3", &m3_only)];
        assert!(detect(&online, &rigs, &work, &[], &[], now, false).is_empty());
    }

    #[test]
    fn false_stop_incident_requires_terminal_marker_freshness_and_open_work() {
        let rigs = rigs();
        let amo = probe(r#"{"rig":"amo","os":"macos","at":"","cores":8,"quiet":true}"#);
        let online = vec![("amo", &amo)];
        let work = vec![open_work("katgpt-rs", "a.md")];
        let now = chrono::Utc::now();
        let end = |session: &str, ended_by: &str, age_min: i64| ThreadEnd {
            session: session.to_string(),
            rig: "amo".to_string(),
            cwd: "/Users/k/git/katgpt-rs".to_string(),
            ended_by: ended_by.to_string(),
            had_summary: true,
            at: now.timestamp_millis() - chrono::Duration::minutes(age_min).num_milliseconds(),
        };

        let incidents = detect(
            &online,
            &rigs,
            &work,
            &[end("s1", END_TERMINAL, 30)],
            &[],
            now,
            false,
        );
        assert_eq!(incidents.len(), 2, "queue + false-stop over the same work");
        assert!(incidents
            .iter()
            .any(|incident| incident.fingerprint == "boss:false-stop:s1"));

        assert!(detect(&online, &rigs, &work, &[end("s2", END_FINAL, 30)], &[], now, false)
            .iter()
            .all(|incident| !incident.fingerprint.starts_with("boss:false-stop")),
        "a final stop ran its housekeeping pass — not a false stop");

        assert!(detect(&online, &rigs, &work, &[end("s3", END_TERMINAL, 90)], &[], now, false)
            .iter()
            .all(|incident| !incident.fingerprint.starts_with("boss:false-stop")),
        "a terminal stop outside the window is stale news");

        let elsewhere = [ThreadEnd {
            session: "s4".into(),
            rig: "amo".into(),
            cwd: "/Users/k/git/riir-ai".into(),
            ended_by: END_TERMINAL.into(),
            had_summary: true,
            at: now.timestamp_millis(),
        }];
        assert!(detect(&online, &rigs, &work, &elsewhere, &[], now, false)
            .iter()
            .all(|incident| incident.fingerprint != "boss:false-stop:s4"),
        "terminal over a repo with no open work is a legitimate stop");
    }

    #[test]
    fn stale_mail_skips_system_owner_live_and_fresh_inboxes() {
        let dir = tempfile::tempdir().unwrap();
        let room = room(dir.path());
        send_mail(&room, "zed-dead", "stuck mail");
        age_mail(&room, "zed-dead", 60);
        send_mail(&room, "zed-fresh", "just arrived");
        send_mail(&room, "watch-aa", "waiter inbox");
        send_mail(&room, "owner", "owner mail");
        send_mail(&room, "zed-live", "being handled");
        room.write_live_entry("zed-live", "idle", "thread");

        let now = chrono::Utc::now();
        let live = room
            .live_agents()
            .into_iter()
            .map(|agent| agent.name)
            .collect::<BTreeSet<_>>();
        let mail = stale_mail(&room, now, MAIL_GRACE, &live);
        assert_eq!(
            mail,
            vec![StaleMail {
                inbox: "zed-dead".into(),
                pending: 1,
                oldest_age_min: 60,
            }],
            "system, waiter, owner, live-addressee and fresh mail are excluded"
        );

        // A live Zed on the rig makes it the boss's problem; with none, the
        // rig's zed-wake job owns the wake.
        let incident = |live: bool| {
            detect(
                &[],
                &rigs(),
                &[],
                &[],
                &mail,
                now,
                live,
            )
            .into_iter()
            .find(|incident| incident.fingerprint == "boss:unread-mail")
        };
        assert_eq!(incident(true).unwrap().min_ticks, BOSS_MAIL_TICKS);
        assert!(incident(false).is_none());
    }

    #[test]
    fn housekeeping_prompt_is_bounded_and_complete() {
        let prompt = housekeeping_prompt("incident detail", "amo", BOSS_BUDGET_UNITS);
        for expected in [
            "5 checklist units",
            "## Summary",
            "**Claimed:**",
            "boundary-guard",
            "benchmark",
            "Compaction",
            "cargo refine",
            "agent-mail rig run bmo",
            "ask Claude for the verdict",
            "<stop>",
        ] {
            assert!(prompt.contains(expected), "prompt must mention {expected}");
        }
    }
}
