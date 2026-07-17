use anyhow::{Context as _, Result};
use std::process::Stdio;

/// A wrapper around `smol::process::Child` that ensures all subprocesses
/// are killed when the process is terminated by using process groups.
///
/// Dropping a `Child` without explicitly awaiting its exit will still kill
/// the process group and reap the zombie via a detached reaper task. This
/// prevents zombie accumulation when callers (e.g. `Drop` impls in transports)
/// signal kill without awaiting `status()`.
pub struct Child {
    process: Option<smol::process::Child>,
}

impl std::ops::Deref for Child {
    type Target = smol::process::Child;

    fn deref(&self) -> &Self::Target {
        self.process
            .as_ref()
            .expect("Child handle used after into_inner() or drop")
    }
}

impl std::ops::DerefMut for Child {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.process
            .as_mut()
            .expect("Child handle used after into_inner() or drop")
    }
}

impl Child {
    #[cfg(not(windows))]
    pub fn spawn(
        mut command: std::process::Command,
        stdin: Stdio,
        stdout: Stdio,
        stderr: Stdio,
    ) -> Result<Self> {
        crate::set_pre_exec_to_start_new_session(&mut command);
        let mut command = smol::process::Command::from(command);
        let process = command
            .stdin(stdin)
            .stdout(stdout)
            .stderr(stderr)
            .spawn()
            .with_context(|| {
                format!(
                    "failed to spawn command {}",
                    crate::redact::redact_command(&format!("{command:?}"))
                )
            })?;
        Ok(Self {
            process: Some(process),
        })
    }

    #[cfg(windows)]
    pub fn spawn(
        command: std::process::Command,
        stdin: Stdio,
        stdout: Stdio,
        stderr: Stdio,
    ) -> Result<Self> {
        // TODO(windows): create a job object and add the child process handle to it,
        // see https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects
        let mut command = smol::process::Command::from(command);
        let process = command
            .stdin(stdin)
            .stdout(stdout)
            .stderr(stderr)
            .spawn()
            .with_context(|| {
                format!(
                    "failed to spawn command {}",
                    crate::redact::redact_command(&format!("{command:?}"))
                )
            })?;

        Ok(Self {
            process: Some(process),
        })
    }

    pub fn into_inner(mut self) -> smol::process::Child {
        self.process
            .take()
            .expect("Child handle used after into_inner() or drop")
    }

    #[cfg(not(windows))]
    pub fn kill(&mut self) -> Result<()> {
        let Some(process) = self.process.as_ref() else {
            return Ok(());
        };
        let pid = process.id();
        unsafe {
            libc::killpg(pid as i32, libc::SIGKILL);
        }
        Ok(())
    }

    #[cfg(windows)]
    pub fn kill(&mut self) -> Result<()> {
        // TODO(windows): terminate the job object in kill
        if let Some(process) = self.process.as_mut() {
            process.kill()?;
        }
        Ok(())
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        let Some(process) = self.process.take() else {
            return;
        };

        #[cfg(not(windows))]
        {
            // Best-effort SIGKILL to the whole process group so the child (and
            // any descendants it spawned via setsid) exits promptly. Already-dead
            // children return ESRCH which we ignore.
            let pid = process.id() as i32;
            unsafe {
                libc::killpg(pid, libc::SIGKILL);
            }
            reap_detached(process);
        }

        #[cfg(windows)]
        {
            let mut process = process;
            let _ = process.kill();
            // smol::process::Child on Windows reaps on drop when kill succeeds,
            // so nothing further is needed here.
        }
    }
}

/// Reaps a smol child off-thread so it cannot become a zombie.
///
/// Used by `Drop for Child` to guarantee that even callers which kill a
/// process without awaiting `status()` (e.g. transport Drop impls) do not
/// leak kernel process-table slots.
///
/// We do not call raw `libc::waitpid` here because smol tracks each spawned
/// pid internally via its SIGCHLD driver; an external reap would race with
/// smol's internal bookkeeping and break the eventual `status()` call.
/// `smol::spawn` lazily brings up a single global executor thread if none is
/// running yet, so this never fails to schedule the reaper.
#[cfg(not(windows))]
fn reap_detached(mut child: smol::process::Child) {
    smol::spawn(async move {
        let _ = child.status().await;
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Regression test for the zombie leak that occurs when callers drop a
    /// `Child` without awaiting its exit. Before the `Drop` impl was added,
    /// smol's `Child` was released without reaping, leaving a kernel zombie
    /// parented by the test runner (and, in production, by the Zed editor).
    #[cfg(not(windows))]
    #[test]
    fn dropping_child_kills_and_reaps_subprocess() {
        // Spawn a child that would run forever if not killed.
        let mut command = std::process::Command::new("sleep");
        command.arg("300");
        let child = Child::spawn(
            command,
            Stdio::null(),
            Stdio::null(),
            Stdio::null(),
        )
        .expect("failed to spawn sleep");
        let pid = child.id() as i32;
        assert!(pid > 0, "child has a valid pid");

        // Drop without awaiting. This must trigger SIGKILL to the process group
        // and a detached reap, otherwise a zombie would persist.
        drop(child);

        // Wait up to 5s for the process to be reaped (no longer present in the
        // kernel process table at all). If `Drop` failed to reap, `kill -0`
        // would keep succeeding for the zombie entry.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            // `kill -0` returns Ok for both live and zombie processes; once the
            // process has been reaped it returns ESRCH.
            let alive = unsafe { libc::kill(pid, 0) };
            if alive != 0 {
                break;
            }
            if Instant::now() >= deadline {
                panic!(
                    "pid {pid} still present after 5s; Drop did not reap the child (zombie leak)"
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}
