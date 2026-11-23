//! Windows Task Scheduler task definitions for agent-mail (issues 069/072).
//!
//! Windows one-shot tasks created with the old `/sc once /st 00:00`
//! incantation never self-delete: the trigger time is already past, no end
//! boundary exists so the task never "expires", and the end-of-job
//! `schtasks /delete` is admin-gated for a normal token. Every detached
//! executor/kicker thus left a permanent `agent-mail-<id>` entry behind.
//!
//! The fix shape: build the task as XML with a real trigger window
//! (`StartBoundary` in the future, `EndBoundary` past the run's worst case)
//! plus the `DeleteExpiredTaskAfter=PT0S` setting, so the scheduler removes
//! the registration itself once the window closes — no admin token needed.
//! The same builder arms the `agent-mail-zed-wake` driver, a repeating
//! one-minute tick that promotes due mail and launches Zed when no poller is
//! live (macOS gets this from launchd; Windows had nothing).

use anyhow::{Context as _, Result};
use std::fs;
use std::path::PathBuf;

/// The repeating driver that backfills the no-Zed-poller gap on Windows.
pub const ZED_WAKE_TASK: &str = "agent-mail-zed-wake";
/// One driver tick per minute; the tick itself is a cheap directory scan.
pub const ZED_WAKE_INTERVAL: &str = "PT1M";

/// A one-shot task: the action starts on demand (`schtasks /run`) or at
/// `start_boundary` as a safety net, must finish before `end_boundary`, and
/// the scheduler deletes the registration once the window closes.
pub struct OneShotTask {
    pub action: String,
    pub arguments: String,
    pub start_boundary: chrono::DateTime<chrono::Utc>,
    pub end_boundary: chrono::DateTime<chrono::Utc>,
}

/// The persistent per-minute driver: never expires, never self-deletes.
pub struct RepeatingTask {
    pub action: String,
    pub arguments: String,
    pub start_boundary: chrono::DateTime<chrono::Utc>,
    pub interval: &'static str,
}

impl OneShotTask {
    pub fn xml(&self) -> String {
        task_xml(
            &self.action,
            &self.arguments,
            ("true", "true"),
            Some("PT0S"),
            |trigger| {
                format!(
                    "{trigger}      <StartBoundary>{}</StartBoundary>\n      \
                         <EndBoundary>{}</EndBoundary>\n",
                    boundary(self.start_boundary),
                    boundary(self.end_boundary)
                )
            },
        )
    }
}

impl RepeatingTask {
    pub fn xml(&self) -> String {
        task_xml(
            &self.action,
            &self.arguments,
            ("false", "false"),
            None,
            |trigger| {
                format!(
                    "{trigger}      <StartBoundary>{}</StartBoundary>\n      \
                         <Repetition>\n        <Interval>{}</Interval>\n        \
                         <StopAtDurationEnd>false</StopAtDurationEnd>\n      \
                         </Repetition>\n",
                    boundary(self.start_boundary),
                    self.interval
                )
            },
        )
    }
}

/// Shared task skeleton: principal/settings exactly the ones the old
/// `/sc once /st 00:00` incantation produced (issue 069's authored probe),
/// `trigger_body` slotting in the trigger's boundary lines, `battery` the
/// DisallowStartIfOnBatteries/StopIfGoingOnBatteries values.
fn task_xml(
    action: &str,
    arguments: &str,
    battery: (&'static str, &'static str),
    delete_expired_after: Option<&'static str>,
    trigger_body: impl Fn(&str) -> String,
) -> String {
    let delete_expired = match delete_expired_after {
        Some(duration) => {
            format!("    <DeleteExpiredTaskAfter>{duration}</DeleteExpiredTaskAfter>\n")
        }
        None => String::new(),
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>{description}</Description>
  </RegistrationInfo>
  <Triggers>
    <TimeTrigger>
{trigger_body}      <Enabled>true</Enabled>
    </TimeTrigger>
  </Triggers>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>{battery_disallow}</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>{battery_stop}</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT72H</ExecutionTimeLimit>
    <Priority>7</Priority>
{delete_expired}  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{command}</Command>
      <Arguments>{arguments}</Arguments>
    </Exec>
  </Actions>
  <Principals>
    <Principal id="Author">
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
</Task>
"#,
        battery_disallow = battery.0,
        battery_stop = battery.1,
        description = escape("agent-mail detached helper (self-expiring; issues 069/072)"),
        trigger_body = trigger_body(""),
        command = escape(action),
        arguments = escape(arguments),
    )
}

/// UTC `Z` boundaries normalize to local on registration (validated in-shell,
/// issue 069's probe): the one form every locale accepts.
fn boundary(time: chrono::DateTime<chrono::Utc>) -> String {
    time.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Minimal XML text escaping: agent-mail argv holds user-controlled titles
/// and paths.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('\"', "&quot;")
}

/// UTF-16LE with a BOM: what `schtasks /create /xml` requires (the authored
/// probe's encoding; a UTF-8 file fails to register).
pub fn encode_utf16le_with_bom(xml: &str) -> Vec<u8> {
    let mut bytes = vec![0xFF, 0xFE];
    bytes.extend(xml.encode_utf16().flat_map(|unit| unit.to_le_bytes()));
    bytes
}

/// Registers (or overwrites) a task from its definition.
// agent-mail is a runtime-free std binary; the smol-preferring lint targets
// async gpui code.
#[allow(clippy::disallowed_methods)]
pub fn create_task(task_name: &str, xml: &str) -> Result<()> {
    let file: PathBuf = std::env::temp_dir().join(format!("agent-mail-{task_name}.xml"));
    fs::write(&file, encode_utf16le_with_bom(xml))
        .with_context(|| format!("writing {}", file.display()))?;
    let created = crate::command("schtasks")
        .args(["/create", "/tn", task_name, "/xml"])
        .arg(&file)
        .arg("/f")
        .output()
        .context("running schtasks /create")?;
    let _ = fs::remove_file(&file);
    anyhow::ensure!(
        created.status.success(),
        "schtasks /create {task_name} failed: {}",
        String::from_utf8_lossy(&created.stderr).trim()
    );
    Ok(())
}

/// The `agent-mail-zed-wake` driver: one `rig zed-wake --once` tick per
/// minute. Overwriting on every call is the self-heal — a spent, deleted, or
/// never-installed driver is re-armed by the next scheduled delivery.
pub fn ensure_zed_wake_driver() -> Result<()> {
    let binary = std::env::current_exe()?;
    let task = RepeatingTask {
        action: format!("\"{}\"", binary.display()),
        // The tick promotes due mail as a side effect of its inbox scan and
        // launches Zed only when no poller is live (issue 072).
        arguments: "rig zed-wake --once".to_string(),
        start_boundary: chrono::Utc::now(),
        interval: ZED_WAKE_INTERVAL,
    };
    create_task(ZED_WAKE_TASK, &task.xml())
}

/// Where a repeating driver's re-arm failure should surface: a warning, never
/// a failed delivery — the wake still works whenever a live Zed poller runs.
pub fn ensure_zed_wake_driver_warn() {
    if let Err(error) = ensure_zed_wake_driver() {
        eprintln!(
            "agent-mail: could not arm the {ZED_WAKE_TASK} driver \
             (scheduled mail still delivers when a Zed poller runs): {error:#}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_one_shot() -> OneShotTask {
        OneShotTask {
            action: "\"C:\\Users\\katop\\.cargo\\bin\\agent-mail.exe\"".to_string(),
            arguments: "rig exec 0199abcd-1234".to_string(),
            start_boundary: chrono::DateTime::parse_from_rfc3339("2026-11-22T16:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
            end_boundary: chrono::DateTime::parse_from_rfc3339("2026-11-25T17:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
        }
    }

    #[test]
    fn one_shot_xml_carries_window_and_self_delete() {
        let xml = sample_one_shot().xml();
        assert!(
            xml.contains("<StartBoundary>2026-11-22T16:00:00Z</StartBoundary>"),
            "{xml}"
        );
        assert!(
            xml.contains("<EndBoundary>2026-11-25T17:00:00Z</EndBoundary>"),
            "{xml}"
        );
        assert!(
            xml.contains("<DeleteExpiredTaskAfter>PT0S</DeleteExpiredTaskAfter>"),
            "{xml}"
        );
        assert!(xml.contains("<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>"));
        assert!(xml.contains("<StartWhenAvailable>true</StartWhenAvailable>"));
        assert!(xml.contains("<LogonType>InteractiveToken</LogonType>"));
        assert!(xml.contains("<RunLevel>LeastPrivilege</RunLevel>"));
        assert!(xml.contains("<ExecutionTimeLimit>PT72H</ExecutionTimeLimit>"));
        assert!(xml.contains("<DisallowStartIfOnBatteries>true</DisallowStartIfOnBatteries>"));
        assert!(!xml.contains("<Repetition>"), "one-shots must not repeat");
        // The executable path is XML-escaped and stays one element.
        assert!(
            xml.contains(
                "<Command>&quot;C:\\Users\\katop\\.cargo\\bin\\agent-mail.exe&quot;</Command>"
            ),
            "{xml}"
        );
    }

    #[test]
    fn repeating_driver_xml_carries_interval_and_never_expires() {
        let task = RepeatingTask {
            action: "\"C:\\agent-mail.exe\"".to_string(),
            arguments: "rig zed-wake --once".to_string(),
            start_boundary: chrono::DateTime::parse_from_rfc3339("2026-11-22T16:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
            interval: ZED_WAKE_INTERVAL,
        };
        let xml = task.xml();
        assert!(xml.contains("<Interval>PT1M</Interval>"), "{xml}");
        assert!(
            xml.contains("<StopAtDurationEnd>false</StopAtDurationEnd>"),
            "{xml}"
        );
        assert!(!xml.contains("EndBoundary"), "the driver must never expire");
        assert!(
            !xml.contains("DeleteExpiredTaskAfter"),
            "the driver must never self-delete"
        );
        assert!(xml.contains("rig zed-wake --once"), "{xml}");
        assert!(
            xml.contains("<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>"),
            "{xml}"
        );
    }

    #[test]
    fn escapes_xml_text() {
        assert_eq!(
            escape("a & b <c> \"d\""),
            "a &amp; b &lt;c&gt; &quot;d&quot;"
        );
    }

    #[test]
    fn encodes_utf16le_with_bom() {
        let bytes = encode_utf16le_with_bom("<x/>");
        assert_eq!(bytes[0..2], [0xFF, 0xFE], "BOM first");
        // '<' U+003C little-endian
        assert_eq!(bytes[2..4], [0x3C, 0x00]);
        assert_eq!(bytes.len(), 2 + "<x/>".len() * 2);
    }

    #[test]
    fn boundary_format_is_zulu_seconds() {
        let time = chrono::DateTime::parse_from_rfc3339("2026-11-22T16:00:05.123Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(boundary(time), "2026-11-22T16:00:05Z");
    }
}
