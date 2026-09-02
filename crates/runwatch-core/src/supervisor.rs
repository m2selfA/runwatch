use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::{data_dir, ensure_data_dir};
use crate::live;

#[cfg(windows)]
use std::os::windows::io::AsRawHandle;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
#[cfg(windows)]
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_BREAKAWAY_OK,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, SetInformationJobObject,
};
#[cfg(windows)]
use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

const SUPERVISOR_PID_NAME: &str = "supervise.pid";
const SUPERVISOR_LOCK_NAME: &str = "supervise.lock";
const STABLE_CHILD_SECS: u64 = 60;
const EXISTING_OWNER_POLL_MS: u64 = 1000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SupervisorHeartbeat {
    pub pid: u32,
    pub started_unix: u64,
}

pub fn pid_path() -> Result<PathBuf> {
    Ok(data_dir()?.join(SUPERVISOR_PID_NAME))
}

fn lock_path() -> Result<PathBuf> {
    Ok(data_dir()?.join(SUPERVISOR_LOCK_NAME))
}

pub fn read() -> Result<Option<SupervisorHeartbeat>> {
    let path = pid_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(path)?;
    Ok(serde_json::from_str(&text).ok())
}

pub fn owner_lock_held() -> Result<bool> {
    ensure_data_dir()?;
    let path = lock_path()?;
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("open supervisor lock {}", path.display()))?;
    match file.try_lock_exclusive() {
        Ok(()) => {
            file.unlock()?;
            Ok(false)
        }
        Err(_) => Ok(true),
    }
}

struct SupervisorLock {
    file: fs::File,
}

impl SupervisorLock {
    fn claim() -> Result<Self> {
        ensure_data_dir()?;
        let path = lock_path()?;
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("open supervisor lock {}", path.display()))?;
        if let Err(error) = file.try_lock_exclusive() {
            bail!(
                "runwatch supervise already owns {}: {error}",
                path.display()
            );
        }
        let heartbeat = SupervisorHeartbeat {
            pid: std::process::id(),
            started_unix: live::now_unix(),
        };
        fs::write(pid_path()?, serde_json::to_string(&heartbeat)?)?;
        Ok(Self { file })
    }
}

impl Drop for SupervisorLock {
    fn drop(&mut self) {
        if let Ok(Some(heartbeat)) = read()
            && heartbeat.pid == std::process::id()
        {
            let _ = fs::remove_file(pid_path().unwrap_or_default());
        }
        let _ = self.file.unlock();
    }
}

#[cfg(windows)]
struct ChildJob {
    handle: HANDLE,
}

#[cfg(windows)]
impl ChildJob {
    fn attach(child: &Child) -> Result<Self> {
        // SAFETY: null security/name pointers are explicitly permitted. The returned handle is owned
        // by ChildJob and closed in Drop.
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error())
                .context("create runwatch child Job Object");
        }

        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags =
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK;
        // SAFETY: limits points to a correctly sized JOBOBJECT_EXTENDED_LIMIT_INFORMATION that
        // remains live for the duration of the call.
        let set_ok = unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if set_ok == 0 {
            let error = std::io::Error::last_os_error();
            // SAFETY: handle was returned by CreateJobObjectW and has not been closed yet.
            unsafe { CloseHandle(handle) };
            return Err(error).context("configure runwatch child Job Object");
        }

        let process_handle = child.as_raw_handle() as HANDLE;
        // SAFETY: process_handle belongs to the live Child and handle belongs to this function.
        let assign_ok = unsafe { AssignProcessToJobObject(handle, process_handle) };
        if assign_ok == 0 {
            let error = std::io::Error::last_os_error();
            // SAFETY: handle was returned by CreateJobObjectW and has not been closed yet.
            unsafe { CloseHandle(handle) };
            return Err(error).context("assign runwatch serve child to Job Object");
        }
        Ok(Self { handle })
    }
}

#[cfg(windows)]
impl Drop for ChildJob {
    fn drop(&mut self) {
        // SAFETY: handle is uniquely owned by ChildJob. KILL_ON_JOB_CLOSE makes this the final
        // containment boundary if the supervisor itself is terminated.
        unsafe { CloseHandle(self.handle) };
    }
}

#[cfg(not(windows))]
struct ChildJob;

#[cfg(not(windows))]
impl ChildJob {
    fn attach(_child: &Child) -> Result<Self> {
        Ok(Self)
    }
}

fn restart_delay(failures: u32) -> Duration {
    Duration::from_secs(match failures {
        0 | 1 => 1,
        2 => 2,
        3 => 5,
        4 => 10,
        _ => 30,
    })
}

fn should_wait_for_existing_daemon() -> Result<bool> {
    // The ServeLock is the ownership authority. A fresh heartbeat can remain after a hard kill,
    // while the OS releases the file lock immediately when the daemon process exits.
    live::owner_lock_held()
}

fn spawn_serve(exe: &Path, interval_sec: u64) -> Result<(Child, ChildJob)> {
    let executable = if cfg!(windows) {
        PathBuf::from(crate::autostart::task_scheduler_path(exe))
    } else {
        exe.to_path_buf()
    };
    let mut command = Command::new(&executable);
    command
        .arg("serve")
        .arg("--interval")
        .arg(interval_sec.to_string());
    if let Some(parent) = exe.parent() {
        command.current_dir(parent);
    }
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);

    let mut child = command
        .spawn()
        .with_context(|| format!("spawn {} serve", executable.display()))?;
    let job = match ChildJob::attach(&child) {
        Ok(job) => job,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
    };
    Ok((child, job))
}

pub fn supervise(exe: &Path, interval_sec: u64) -> Result<()> {
    if !(1..=3600).contains(&interval_sec) {
        bail!("supervisor interval must be between 1 and 3600 seconds");
    }
    let exe = exe
        .canonicalize()
        .with_context(|| format!("resolve runwatch executable {}", exe.display()))?;
    if !exe.is_file() {
        bail!("runwatch executable is not a file: {}", exe.display());
    }

    let _lock = SupervisorLock::claim()?;
    let mut consecutive_failures = 0u32;

    loop {
        if should_wait_for_existing_daemon()? {
            thread::sleep(Duration::from_millis(EXISTING_OWNER_POLL_MS));
            continue;
        }

        let started = Instant::now();
        match spawn_serve(&exe, interval_sec) {
            Ok((mut child, _job)) => {
                let pid = child.id();
                eprintln!("runwatch supervise child pid={pid} interval={interval_sec}s");
                match child.wait() {
                    Ok(status) => eprintln!("runwatch serve child pid={pid} exited {status}"),
                    Err(error) => eprintln!("runwatch serve child pid={pid} wait failed: {error}"),
                }
                if started.elapsed() >= Duration::from_secs(STABLE_CHILD_SECS) {
                    consecutive_failures = 0;
                } else {
                    consecutive_failures = consecutive_failures.saturating_add(1);
                }
            }
            Err(error) => {
                consecutive_failures = consecutive_failures.saturating_add(1);
                eprintln!("runwatch supervise spawn failed: {error:#}");
            }
        }

        thread::sleep(restart_delay(consecutive_failures));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supervisor_restart_backoff_is_bounded() {
        assert_eq!(restart_delay(0), Duration::from_secs(1));
        assert_eq!(restart_delay(1), Duration::from_secs(1));
        assert_eq!(restart_delay(2), Duration::from_secs(2));
        assert_eq!(restart_delay(3), Duration::from_secs(5));
        assert_eq!(restart_delay(4), Duration::from_secs(10));
        assert_eq!(restart_delay(5), Duration::from_secs(30));
        assert_eq!(restart_delay(100), Duration::from_secs(30));
    }

    #[cfg(windows)]
    #[test]
    fn supervisor_job_allows_scientific_process_breakaway() {
        assert_ne!(JOB_OBJECT_LIMIT_BREAKAWAY_OK, 0);
    }
}
