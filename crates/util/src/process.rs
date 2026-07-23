use anyhow::{Context as _, Result};
use std::process::Stdio;

/// A wrapper around `smol::process::Child` that ensures all subprocesses
/// are killed when the process is terminated: on Unix by using process
/// groups, and on Windows by using job objects.
///
/// On Windows, dropping this struct closes the job object handle, which
/// terminates all processes in the job. This also applies when the Zed
/// process exits for any reason (including crashes), since the OS closes
/// its handles, so spawned process trees can never outlive Zed.
///
/// On Unix, dropping this struct without explicitly awaiting its exit still
/// kills the process group and reaps the zombie via a detached reaper task.
/// This prevents zombie accumulation when callers (e.g. `Drop` impls in
/// transports) signal kill without awaiting `status()`.
pub struct Child {
    process: Option<smol::process::Child>,
    #[cfg(windows)]
    job: Option<windows_job::JobObject>,
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

        // Assign the child to a job object configured to kill the entire
        // process tree when the last job handle is closed, so descendants
        // (e.g. node workers and MCP servers spawned by agent servers) are
        // reaped even if the direct child doesn't clean them up. Any process
        // the child spawns after this assignment is automatically part of the
        // job.
        //
        // There is a small race: descendants the child spawns between the
        // `spawn()` call returning and the assignment below escape the job.
        // Closing it fully would require creating the process suspended
        // (`CREATE_SUSPENDED`), assigning it, then resuming it, which the
        // std/smol process APIs don't support without reimplementing process
        // creation. The window is microseconds, and the children we care
        // about (`npx`, `node`, etc.) take far longer to load their runtime
        // and spawn anything, so in practice nothing escapes.
        let job = windows_job::JobObject::new()
            .and_then(|job| {
                job.assign_process(process.id())?;
                Ok(job)
            })
            .map_err(|error| {
                log::error!("failed to assign spawned process to a job object: {error:#}");
            })
            .ok();

        Ok(Self {
            process: Some(process),
            job,
        })
    }

    pub fn into_inner(mut self) -> smol::process::Child {
        self.process
            .take()
            .expect("Child handle used after into_inner() or drop")
    }

    /// Consumes the child, draining its stdout/stderr and waiting for it to
    /// exit, then returns the collected output.
    pub async fn output(mut self) -> Result<std::process::Output> {
        // NOTE: Keep `self` alive across this await, do not drop it (or its
        // `job` field) before the await completes. On Windows, dropping the
        // job object early triggers `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` and
        // kills the child before `output()` finishes collecting its
        // stdout/stderr.
        let process = self
            .process
            .take()
            .expect("Child handle used after into_inner() or drop");
        Ok(process.output().await?)
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
        if let Some(job) = &self.job {
            job.terminate()
        } else if let Some(process) = self.process.as_mut() {
            process.kill()?;
            Ok(())
        } else {
            Ok(())
        }
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
            // Dropping `process` here relies on smol's built-in reap-on-drop
            // behavior. `self.job` (if present) is dropped right after this
            // function returns, closing the job handle and terminating the
            // whole process tree via `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`.
            drop(process);
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

#[cfg(windows)]
mod windows_job {
    use crate::ResultExt as _;
    use anyhow::{Context as _, Result};
    use windows::Win32::{
        Foundation::{CloseHandle, HANDLE},
        System::{
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
                SetInformationJobObject, TerminateJobObject,
            },
            Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE},
        },
    };

    /// A Win32 job object configured with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`:
    /// all processes assigned to the job (and their descendants) are terminated
    /// when the last handle to the job is closed, which happens when this struct
    /// is dropped, or when the OS closes the owning process's handles after it
    /// exits for any reason.
    pub(crate) struct JobObject(HANDLE);

    // SAFETY: Job object handles can be used from any thread.
    unsafe impl Send for JobObject {}
    unsafe impl Sync for JobObject {}

    impl JobObject {
        pub(crate) fn new() -> Result<Self> {
            unsafe {
                let job =
                    Self(CreateJobObjectW(None, None).context("failed to create job object")?);
                let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                SetInformationJobObject(
                    job.0,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const _,
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
                .context("failed to set job object limits")?;
                Ok(job)
            }
        }

        pub(crate) fn assign_process(&self, pid: u32) -> Result<()> {
            unsafe {
                let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, false, pid)
                    .context("failed to open process")?;
                let result = AssignProcessToJobObject(self.0, process)
                    .context("failed to assign process to job object");
                CloseHandle(process).log_err();
                result
            }
        }

        pub(crate) fn terminate(&self) -> Result<()> {
            unsafe { TerminateJobObject(self.0, 1).context("failed to terminate job object") }
        }
    }

    impl Drop for JobObject {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0).log_err();
            }
        }
    }
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
        let child = Child::spawn(command, Stdio::null(), Stdio::null(), Stdio::null())
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

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Spawns a process tree `powershell -> ping` via `Child::spawn` and
    /// returns the `Child` along with the pid of the grandchild (`ping`).
    fn spawn_process_tree(temp_dir: &std::path::Path) -> (Child, u32) {
        let pid_file = temp_dir.join("grandchild_pid");
        let mut command = std::process::Command::new("powershell.exe");
        command.args(["-NoProfile", "-Command"]).arg(format!(
            "$p = Start-Process -FilePath ping.exe -ArgumentList @('-n','60','127.0.0.1') -PassThru -WindowStyle Hidden; \
             Set-Content -LiteralPath '{}' -Value $p.Id; \
             Wait-Process -Id $p.Id",
            pid_file.display()
        ));
        let child = Child::spawn(command, Stdio::null(), Stdio::null(), Stdio::null())
            .expect("failed to spawn powershell");

        let deadline = Instant::now() + Duration::from_secs(5);
        let grandchild_pid = loop {
            if let Ok(contents) = std::fs::read_to_string(&pid_file)
                && let Ok(pid) = contents.trim().parse::<u32>()
            {
                break pid;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for grandchild pid file"
            );
            std::thread::sleep(Duration::from_millis(50));
        };
        assert!(
            process_is_alive(grandchild_pid),
            "grandchild should be alive after spawning"
        );
        (child, grandchild_pid)
    }

    fn process_is_alive(pid: u32) -> bool {
        use windows::Win32::{
            Foundation::{CloseHandle, STILL_ACTIVE},
            System::Threading::{
                GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
            },
        };

        unsafe {
            let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
                return false;
            };
            let mut exit_code = 0u32;
            let alive = GetExitCodeProcess(handle, &mut exit_code).is_ok()
                && exit_code == STILL_ACTIVE.0 as u32;
            CloseHandle(handle).expect("failed to close process handle");
            alive
        }
    }

    fn assert_process_exits(pid: u32, message: &str) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while process_is_alive(pid) {
            assert!(Instant::now() < deadline, "{message} (pid {pid})");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    #[test]
    fn test_kill_terminates_grandchildren() {
        let temp_dir = tempfile::tempdir().unwrap();
        let (mut child, grandchild_pid) = spawn_process_tree(temp_dir.path());

        child.kill().expect("failed to kill child");

        assert_process_exits(
            grandchild_pid,
            "grandchild should be terminated after killing the child",
        );
    }

    #[test]
    fn test_drop_terminates_grandchildren() {
        let temp_dir = tempfile::tempdir().unwrap();
        let (child, grandchild_pid) = spawn_process_tree(temp_dir.path());

        drop(child);

        assert_process_exits(
            grandchild_pid,
            "grandchild should be terminated after dropping the child",
        );
    }
}
