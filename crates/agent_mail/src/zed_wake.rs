//! Per-rig Zed wake (plan 053 follow-up): when mail sits unclaimed on this
//! rig and no Zed poller is live, launch Zed so its mail bridge can spawn or
//! resume the addressed thread. One tick per invocation, driven by a
//! scheduler: `agent-mail rig zed-wake --install` writes a launchd plist
//! (macOS) that ticks every minute; on other platforms run
//! `agent-mail rig zed-wake --once` from your own scheduler (e.g. Task
//! Scheduler on Windows).

use crate::{ALL_NAME, ANY_NAME, MailMode, Mailroom};
use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

fn watch_dir(mailroom: &Mailroom) -> PathBuf {
    mailroom.home().join("watch")
}

pub const LAUNCHD_LABEL: &str = "dev.agent-mail.zed-wake";
/// Overrides the launch command (space-separated argv), e.g.
/// `AGENT_MAIL_WAKE_COMMAND="open -ga Zed"` on a rig running a different app.
pub const WAKE_COMMAND_ENV: &str = "AGENT_MAIL_WAKE_COMMAND";
/// Mail younger than this is left alone: a live Zed poller claims it within a
/// second, and a just-sent message should not yank an editor up mid-delivery.
const GRACE: Duration = Duration::from_secs(2 * 60);
/// At most one wake this often (state survives ticks via a JSON file): the
/// launch is a whole editor, not a per-thread action.
const COOLDOWN: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Default, Serialize, Deserialize)]
struct WakeState {
    last_launch: Option<String>,
}

#[derive(Debug)]
pub struct WakeReport {
    /// Inboxes that qualified for a wake, whether or not one was launched.
    pub candidates: Vec<String>,
    pub woke: bool,
}

/// One watcher pass. Wakes only when this rig has no fresh live entry: a
/// running Zed either claims the mail itself (spawn/resume on) or has the
/// setting off, and launching it again changes nothing.
pub fn tick(mailroom: &Mailroom, now: chrono::DateTime<chrono::Utc>) -> Result<WakeReport> {
    if !mailroom.live_agents().is_empty() {
        return Ok(WakeReport {
            candidates: Vec::new(),
            woke: false,
        });
    }
    let candidates: Vec<String> = mailroom
        .pending_names()
        .into_iter()
        .filter(|name| name != ANY_NAME && name != ALL_NAME)
        .filter(|name| {
            mailroom.peek(std::slice::from_ref(name)).is_ok_and(|mail| {
                mail.iter()
                    .any(|envelope| envelope.mode != MailMode::Fyi && is_past_grace(envelope, now))
            })
        })
        .collect();
    if candidates.is_empty() {
        return Ok(WakeReport {
            candidates,
            woke: false,
        });
    }
    let state_path = state_path(mailroom);
    let state: WakeState = fs::read(&state_path)
        .ok()
        .and_then(|text| serde_json::from_slice(&text).ok())
        .unwrap_or_default();
    if let Some(last) = state
        .last_launch
        .as_deref()
        .and_then(|last| chrono::DateTime::parse_from_rfc3339(last).ok())
        .map(|last| last.with_timezone(&chrono::Utc))
        && now - last < chrono::Duration::from_std(COOLDOWN).unwrap()
    {
        return Ok(WakeReport {
            candidates,
            woke: false,
        });
    }
    launch(&candidates)?;
    fs::create_dir_all(state_path.parent().context("zed-wake state dir")?)?;
    fs::write(
        &state_path,
        serde_json::to_string(&WakeState {
            last_launch: Some(now.to_rfc3339()),
        })?,
    )?;
    Ok(WakeReport {
        candidates,
        woke: true,
    })
}

fn state_path(mailroom: &Mailroom) -> PathBuf {
    watch_dir(mailroom).join("zed-wake.json")
}

fn grace() -> chrono::Duration {
    chrono::Duration::from_std(GRACE).unwrap()
}

/// Unparseable stamps count as old mail: the mailroom delivers rather than
/// drops on unparsable times too.
fn is_past_grace(envelope: &crate::Envelope, now: chrono::DateTime<chrono::Utc>) -> bool {
    chrono::DateTime::parse_from_rfc3339(&envelope.sent_at)
        .map(|sent| now - sent.with_timezone(&chrono::Utc) >= grace())
        .unwrap_or(true)
}

/// agent-mail is a runtime-free std binary; the smol-preferring lint targets
/// async gpui code.
#[allow(clippy::disallowed_methods)]
fn launch(candidates: &[String]) -> Result<()> {
    let argv = launch_argv()?;
    println!(
        "zed-wake: launching {:?} for {}",
        argv,
        candidates.join(", ")
    );
    crate::command(&argv[0])
        .args(&argv[1..])
        .spawn()
        .context("launching Zed")?;
    Ok(())
}

fn launch_argv() -> Result<Vec<String>> {
    if let Ok(command) = std::env::var(WAKE_COMMAND_ENV) {
        let argv: Vec<String> = command.split_whitespace().map(str::to_string).collect();
        if argv.is_empty() {
            bail!("{WAKE_COMMAND_ENV} is set but empty");
        }
        return Ok(argv);
    }
    #[cfg(target_os = "macos")]
    return Ok(vec!["open".into(), "-ga".into(), "Zed Dev".into()]);
    #[cfg(not(target_os = "macos"))]
    bail!(
        "no default Zed wake command on this platform; set {WAKE_COMMAND_ENV} to a space-separated argv"
    )
}

/// Writes a launchd plist that ticks [`tick`] every minute (macOS).
pub fn install_launchd(mailroom: &Mailroom) -> Result<PathBuf> {
    if let Err(error) = crate::jobs::require_standalone() {
        anyhow::bail!("{error}; the launchd plist must run the standalone agent-mail, not the embedding host");
    }
    let binary = std::env::current_exe()?;
    let home = std::env::var("HOME").context("HOME unset")?;
    fs::create_dir_all(watch_dir(mailroom))?;
    let log = watch_dir(mailroom).join("zed-wake.log");
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LAUNCHD_LABEL}</string>
  <key>ProgramArguments</key>
  <array><string>{}</string><string>rig</string><string>zed-wake</string><string>--once</string></array>
  <key>StartInterval</key><integer>60</integer>
  <key>ThrottleInterval</key><integer>10</integer>
  <key>AbandonProcessGroup</key><true/>
  <key>EnvironmentVariables</key>
  <dict>
    <key>HOME</key><string>{home}</string>
    <key>PATH</key><string>{home}/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
  </dict>
  <key>StandardOutPath</key><string>{}</string>
  <key>StandardErrorPath</key><string>{}</string>
</dict>
</plist>
"#,
        binary.display(),
        log.display(),
        log.display()
    );
    let path = PathBuf::from(&home)
        .join("Library/LaunchAgents")
        .join(format!("{LAUNCHD_LABEL}.plist"));
    fs::create_dir_all(path.parent().context("plist dir")?)?;
    fs::write(&path, plist)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Address;
    use std::path::Path;

    fn room(dir: &Path) -> Mailroom {
        let room = Mailroom::new(dir.to_path_buf());
        room.init("m3").unwrap();
        room
    }

    fn send_mail(room: &Mailroom, name: &str, body: &str) {
        let to = Address::parse(&format!("{name}@m3")).unwrap();
        let envelope = room
            .compose(&to, "peer@m3", body.to_string(), None)
            .unwrap();
        room.send(&envelope).unwrap();
    }

    #[test]
    fn wakes_for_orphan_mail_past_grace() {
        let dir = tempfile::tempdir().unwrap();
        let room = room(dir.path());
        send_mail(&room, "task-foo", "run me on a rig with Zed closed");
        let now = chrono::Utc::now();

        // Fresh mail: a running Zed might still claim it, so hold off.
        assert!(!tick(&room, now).unwrap().woke);
        // Past grace with no live Zed: wake.
        assert!(tick(&room, now + grace()).unwrap().woke);
        // Cooldown suppresses the next tick even though mail still waits.
        assert!(!tick(&room, now + grace() + grace()).unwrap().woke);
    }

    #[test]
    fn never_wakes_when_a_live_entry_exists() {
        let dir = tempfile::tempdir().unwrap();
        let room = room(dir.path());
        send_mail(&room, "task-foo", "a Zed is running here");
        room.write_live_entry("zed-abcd1234", "idle", "thread");
        assert!(!tick(&room, chrono::Utc::now() + grace()).unwrap().woke);
    }

    #[test]
    fn fyi_only_mail_never_wakes() {
        let dir = tempfile::tempdir().unwrap();
        let room = room(dir.path());
        let to = Address::parse("task-quiet@m3").unwrap();
        let mut envelope = room
            .compose(&to, "peer@m3", "heads up".to_string(), None)
            .unwrap();
        envelope.mode = MailMode::Fyi;
        room.send(&envelope).unwrap();
        assert!(!tick(&room, chrono::Utc::now() + grace()).unwrap().woke);
    }
}
