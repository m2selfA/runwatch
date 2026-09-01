use anyhow::{Context, Result, bail};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

use crate::config::ensure_data_dir;

const GUI_STARTUP_NAME: &str = "runwatch-gui.cmd";
const LEGACY_GUI_STARTUP_NAME: &str = "runwatch.cmd";
const DAEMON_TASK_NAME: &str = "runwatchd";
pub const DEFAULT_DAEMON_INTERVAL_SEC: u64 = 20;

pub fn gui_startup_dir() -> Result<PathBuf> {
    let appdata = std::env::var_os("APPDATA").context("APPDATA not set")?;
    Ok(PathBuf::from(appdata)
        .join("Microsoft")
        .join("Windows")
        .join("Start Menu")
        .join("Programs")
        .join("Startup"))
}

pub fn gui_startup_path() -> Result<PathBuf> {
    Ok(gui_startup_dir()?.join(GUI_STARTUP_NAME))
}

fn legacy_gui_startup_path() -> Result<PathBuf> {
    Ok(gui_startup_dir()?.join(LEGACY_GUI_STARTUP_NAME))
}

pub fn gui_is_enabled() -> bool {
    gui_startup_path().map(|p| p.exists()).unwrap_or(false)
        || legacy_gui_startup_path()
            .map(|p| p.exists())
            .unwrap_or(false)
}

pub fn install_gui(exe: &Path) -> Result<PathBuf> {
    let dir = gui_startup_dir()?;
    fs::create_dir_all(&dir)?;
    let path = dir.join(GUI_STARTUP_NAME);
    let body = format!("@echo off\r\nstart \"\" \"{}\"\r\n", exe.display());
    fs::write(&path, body)?;
    let legacy = dir.join(LEGACY_GUI_STARTUP_NAME);
    if legacy.exists() {
        let _ = fs::remove_file(legacy);
    }
    Ok(path)
}

pub fn remove_gui() -> Result<bool> {
    let mut removed = false;
    for path in [gui_startup_path()?, legacy_gui_startup_path()?] {
        if path.exists() {
            fs::remove_file(path)?;
            removed = true;
        }
    }
    Ok(removed)
}

pub fn daemon_task_name() -> &'static str {
    DAEMON_TASK_NAME
}

#[cfg(windows)]
pub fn daemon_is_enabled() -> bool {
    schtasks(&["/Query", "/TN", DAEMON_TASK_NAME])
        .map(|output| output.status.success())
        .unwrap_or(false)
}

#[cfg(not(windows))]
pub fn daemon_is_enabled() -> bool {
    false
}

#[cfg(windows)]
pub fn install_daemon(exe: &Path, interval_sec: u64) -> Result<()> {
    let exe = exe
        .canonicalize()
        .with_context(|| format!("resolve runwatch executable {}", exe.display()))?;
    if !exe.is_file() {
        bail!("runwatch executable is not a file: {}", exe.display());
    }
    let user = current_user_identity()?;
    let xml = daemon_task_xml(&exe, &user, interval_sec)?;
    let dir = ensure_data_dir()?;
    let xml_path = dir.join(format!(".runwatchd-task-{}.xml", std::process::id()));
    write_task_xml(&xml_path, &xml)?;
    let xml_arg = xml_path.to_string_lossy().into_owned();
    let create = schtasks(&["/Create", "/TN", DAEMON_TASK_NAME, "/XML", &xml_arg, "/F"]);
    let _ = fs::remove_file(&xml_path);
    require_schtasks(create?, "register runwatchd Task Scheduler task")?;

    require_schtasks(
        schtasks(&["/Run", "/TN", DAEMON_TASK_NAME])?,
        "start runwatchd supervisor Task Scheduler task",
    )?;
    Ok(())
}

#[cfg(not(windows))]
pub fn install_daemon(_exe: &Path, _interval_sec: u64) -> Result<()> {
    bail!("runwatchd Task Scheduler autostart is only available on Windows")
}

#[cfg(windows)]
pub fn remove_daemon() -> Result<bool> {
    if !daemon_is_enabled() {
        return Ok(false);
    }
    require_schtasks(
        schtasks(&["/Delete", "/TN", DAEMON_TASK_NAME, "/F"])?,
        "remove runwatchd Task Scheduler task",
    )?;
    Ok(true)
}

#[cfg(not(windows))]
pub fn remove_daemon() -> Result<bool> {
    Ok(false)
}

#[cfg(windows)]
fn schtasks(args: &[&str]) -> Result<Output> {
    let mut cmd = Command::new("schtasks.exe");
    cmd.args(args).creation_flags(CREATE_NO_WINDOW);
    cmd.output().context("run schtasks.exe")
}

#[cfg(windows)]
fn require_schtasks(output: Output, action: &str) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    bail!(
        "{action} failed: {}{}{}",
        stdout,
        if !stdout.is_empty() && !stderr.is_empty() {
            " | "
        } else {
            ""
        },
        stderr
    )
}

#[cfg(windows)]
fn current_user_identity() -> Result<String> {
    let user = std::env::var("USERNAME").context("USERNAME not set")?;
    let domain = std::env::var("USERDOMAIN").unwrap_or_default();
    Ok(if domain.trim().is_empty() {
        user
    } else {
        format!("{domain}\\{user}")
    })
}

#[cfg(test)]
fn decode_task_output(bytes: &[u8]) -> String {
    let looks_utf16le =
        bytes.starts_with(&[0xFF, 0xFE]) || (bytes.len() >= 4 && bytes[1] == 0 && bytes[3] == 0);
    if looks_utf16le {
        let start = usize::from(bytes.starts_with(&[0xFF, 0xFE])) * 2;
        let units = bytes[start..]
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect::<Vec<_>>();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

pub(crate) fn task_scheduler_path(path: &Path) -> String {
    let value = path.to_string_lossy();
    if let Some(rest) = value.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}");
    }
    if let Some(rest) = value.strip_prefix(r"\\?\") {
        return rest.to_string();
    }
    value.into_owned()
}

fn daemon_task_xml(exe: &Path, user: &str, interval_sec: u64) -> Result<String> {
    if !(1..=3600).contains(&interval_sec) {
        bail!("daemon interval must be between 1 and 3600 seconds")
    }
    let working_dir = exe
        .parent()
        .context("runwatch executable has no parent directory")?;
    let exe_text = task_scheduler_path(exe);
    let working_dir_text = task_scheduler_path(working_dir);
    let start_boundary = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.4" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Durable runwatch daemon for long-running scientific computations.</Description>
  </RegistrationInfo>
  <Triggers>
    <TimeTrigger>
      <Repetition>
        <Interval>PT1M</Interval>
        <StopAtDurationEnd>false</StopAtDurationEnd>
      </Repetition>
      <StartBoundary>{start_boundary}</StartBoundary>
      <Enabled>true</Enabled>
    </TimeTrigger>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{user}</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>true</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{exe}</Command>
      <Arguments>supervise --interval {interval_sec}</Arguments>
      <WorkingDirectory>{working_dir}</WorkingDirectory>
    </Exec>
  </Actions>
</Task>
"#,
        user = xml_escape(user),
        exe = xml_escape(&exe_text),
        working_dir = xml_escape(&working_dir_text),
        start_boundary = xml_escape(&start_boundary),
    ))
}

fn write_task_xml(path: &Path, xml: &str) -> Result<()> {
    let mut bytes = Vec::with_capacity(2 + xml.len() * 2);
    bytes.extend_from_slice(&[0xFF, 0xFE]);
    for unit in xml.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    fs::write(path, bytes).with_context(|| format!("write {}", path.display()))
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_task_xml_has_resident_user_semantics() {
        let xml = daemon_task_xml(
            Path::new("C:/Tools/A&B/runwatch.exe"),
            "LAB\\user&name",
            DEFAULT_DAEMON_INTERVAL_SEC,
        )
        .unwrap();
        assert!(xml.contains("<LogonType>InteractiveToken</LogonType>"));
        assert!(xml.contains("<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>"));
        assert!(xml.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
        assert!(xml.contains("<TimeTrigger>"));
        assert!(xml.contains("<Repetition>"));
        assert!(xml.contains("<Interval>PT1M</Interval>"));
        assert!(xml.contains("<StopAtDurationEnd>false</StopAtDurationEnd>"));
        assert!(!xml.contains("<Duration>"));
        assert!(!xml.contains("<RestartOnFailure>"));
        assert!(xml.contains("supervise --interval 20"));
        assert!(xml.contains("LAB\\user&amp;name"));
        assert!(xml.contains("A&amp;B/runwatch.exe"));
    }

    #[test]
    fn daemon_task_xml_rejects_unbounded_poll_interval() {
        assert!(daemon_task_xml(Path::new("C:/runwatch.exe"), "user", 0).is_err());
        assert!(daemon_task_xml(Path::new("C:/runwatch.exe"), "user", 3601).is_err());
    }

    #[test]
    fn task_scheduler_paths_drop_windows_verbatim_prefixes() {
        assert_eq!(
            task_scheduler_path(Path::new(r"\\?\C:\Tools\runwatch.exe")),
            r"C:\Tools\runwatch.exe"
        );
        assert_eq!(
            task_scheduler_path(Path::new(r"\\?\UNC\server\share\runwatch.exe")),
            r"\\server\share\runwatch.exe"
        );
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "registers and deletes a unique real Windows Task Scheduler task"]
    fn real_task_scheduler_xml_registration_smoke() {
        use std::time::{SystemTime, UNIX_EPOCH};

        let exe = std::env::current_exe().expect("current test executable");
        let user = current_user_identity().expect("current user identity");
        let xml = daemon_task_xml(&exe, &user, DEFAULT_DAEMON_INTERVAL_SEC).expect("task xml");
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let task_name = format!("runwatch-task-smoke-{}-{nonce}", std::process::id());
        let xml_path = std::env::temp_dir().join(format!("{task_name}.xml"));
        write_task_xml(&xml_path, &xml).expect("write task xml");
        let xml_arg = xml_path.to_string_lossy().into_owned();

        let create = schtasks(&["/Create", "/TN", &task_name, "/XML", &xml_arg, "/F"])
            .expect("create task command");
        if !create.status.success() {
            let _ = fs::remove_file(&xml_path);
            panic!(
                "task registration failed: stdout={} stderr={}",
                String::from_utf8_lossy(&create.stdout),
                String::from_utf8_lossy(&create.stderr)
            );
        }

        let query = schtasks(&["/Query", "/TN", &task_name, "/XML"]).expect("query task command");
        let delete = schtasks(&["/Delete", "/TN", &task_name, "/F"]).expect("delete task command");
        let _ = fs::remove_file(&xml_path);
        assert!(
            query.status.success(),
            "registered task should be queryable: {} {}",
            String::from_utf8_lossy(&query.stdout),
            String::from_utf8_lossy(&query.stderr)
        );
        let registered_xml = decode_task_output(&query.stdout);
        assert!(registered_xml.contains("<TimeTrigger>"));
        assert!(registered_xml.contains("<Repetition>"));
        assert!(registered_xml.contains("<Interval>PT1M</Interval>"));
        assert!(!registered_xml.contains("<Duration>"));
        assert!(!registered_xml.contains("<RestartOnFailure>"));
        assert!(registered_xml.contains("supervise --interval 20"));
        assert!(
            delete.status.success(),
            "temporary task should be deleted: {} {}",
            String::from_utf8_lossy(&delete.stdout),
            String::from_utf8_lossy(&delete.stderr)
        );
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "starts a disposable supervisor, recovers a killed serve child, then recovers a killed supervisor"]
    fn real_task_scheduler_daemon_restart_smoke() {
        use std::thread;
        use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

        fn read_pid(path: &Path) -> Option<u32> {
            let text = fs::read_to_string(path).ok()?;
            serde_json::from_str::<serde_json::Value>(&text)
                .ok()?
                .get("pid")?
                .as_u64()
                .and_then(|pid| u32::try_from(pid).ok())
        }

        fn wait_for_pid(path: &Path, previous_pid: Option<u32>, timeout: Duration) -> u32 {
            let deadline = Instant::now() + timeout;
            while Instant::now() < deadline {
                if let Some(pid) = read_pid(path)
                    && previous_pid != Some(pid)
                {
                    return pid;
                }
                thread::sleep(Duration::from_millis(250));
            }
            panic!(
                "timed out waiting for pid change; previous_pid={previous_pid:?} path={}",
                path.display()
            );
        }

        fn daemon_status_ok(exe: &Path, data_dir: &Path, endpoint: &str) -> bool {
            let mut command = Command::new(exe);
            command
                .arg("daemon-status")
                .env("RUNWATCH_DATA_DIR", data_dir)
                .env("RUNWATCH_ENDPOINT", endpoint)
                .creation_flags(CREATE_NO_WINDOW);
            command
                .output()
                .map(|output| output.status.success())
                .unwrap_or(false)
        }

        fn wait_for_owner(
            exe: &Path,
            data_dir: &Path,
            endpoint: &str,
            task_name: &str,
            root: &Path,
            previous_pid: Option<u32>,
            timeout: Duration,
        ) -> u32 {
            let heartbeat = data_dir.join("serve.pid");
            let deadline = Instant::now() + timeout;
            while Instant::now() < deadline {
                if let Some(pid) = read_pid(&heartbeat)
                    && previous_pid != Some(pid)
                    && daemon_status_ok(exe, data_dir, endpoint)
                {
                    return pid;
                }
                thread::sleep(Duration::from_millis(500));
            }
            let task = schtasks(&["/Query", "/TN", task_name, "/V", "/FO", "LIST"])
                .map(|output| {
                    format!(
                        "status={} stdout={} stderr={}",
                        output.status,
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    )
                })
                .unwrap_or_else(|error| format!("query failed: {error:#}"));
            let read_diag = |name: &str| {
                fs::read_to_string(root.join(name)).unwrap_or_else(|_| "<missing>".into())
            };
            panic!(
                "timed out waiting for runwatchd owner; previous_pid={previous_pid:?} heartbeat={} wrapper_started={} wrapper_exit={} runwatch_stderr={} task={task}",
                heartbeat.display(),
                read_diag("wrapper.started"),
                read_diag("wrapper.exit"),
                read_diag("runwatch.stderr.log")
            );
        }

        fn taskkill(pid: u32) -> Output {
            let mut command = Command::new("taskkill.exe");
            command
                .args(["/PID", &pid.to_string(), "/F"])
                .creation_flags(CREATE_NO_WINDOW);
            command.output().expect("taskkill runwatchd")
        }

        let exe = PathBuf::from(
            std::env::var_os("RUNWATCH_R8C_EXE")
                .expect("set RUNWATCH_R8C_EXE to the real runwatch.exe for this acceptance"),
        )
        .canonicalize()
        .expect("resolve RUNWATCH_R8C_EXE");
        assert!(exe.is_file(), "RUNWATCH_R8C_EXE must be a file");
        let exe_for_cmd = task_scheduler_path(&exe);

        let user = current_user_identity().expect("current user identity");
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let task_name = format!("runwatch-service-smoke-{}-{nonce}", std::process::id());
        let root =
            std::env::temp_dir().join(format!("runwatch-r8c-{}-{nonce}", std::process::id()));
        let data_dir = root.join("data");
        fs::create_dir_all(&data_dir).expect("create R8c data dir");
        let endpoint = format!(r"\\.\pipe\runwatch-r8c-{}-{nonce}", std::process::id());
        let wrapper = root.join("runwatch-r8c.cmd");
        fs::write(
            &wrapper,
            format!(
                "@echo off\r\n>\"{}\" echo started\r\nset \"RUNWATCH_DATA_DIR={}\"\r\nset \"RUNWATCH_ENDPOINT={}\"\r\n\"{}\" supervise --interval 300 1>>\"{}\" 2>>\"{}\"\r\nset \"runwatch_exit=%ERRORLEVEL%\"\r\n>\"{}\" echo %runwatch_exit%\r\nexit /b %runwatch_exit%\r\n",
                root.join("wrapper.started").display(),
                data_dir.display(),
                endpoint,
                exe_for_cmd,
                root.join("runwatch.stdout.log").display(),
                root.join("runwatch.stderr.log").display(),
                root.join("wrapper.exit").display()
            ),
        )
        .expect("write R8c wrapper");

        let comspec = PathBuf::from(
            std::env::var_os("COMSPEC").unwrap_or_else(|| "C:\\Windows\\System32\\cmd.exe".into()),
        )
        .canonicalize()
        .expect("resolve COMSPEC");
        let mut xml = daemon_task_xml(&comspec, &user, 300).expect("R8c task xml");
        let old_args = "<Arguments>supervise --interval 300</Arguments>";
        let wrapper_args = format!(r#"/d /c "{}""#, wrapper.display());
        let new_args = format!("<Arguments>{}</Arguments>", xml_escape(&wrapper_args));
        assert!(xml.contains(old_args), "daemon action anchor missing");
        xml = xml.replacen(old_args, &new_args, 1);

        let xml_path = root.join("task.xml");
        write_task_xml(&xml_path, &xml).expect("write R8c task xml");
        let xml_arg = xml_path.to_string_lossy().into_owned();

        let create = schtasks(&["/Create", "/TN", &task_name, "/XML", &xml_arg, "/F"])
            .expect("create R8c task");
        if !create.status.success() {
            let _ = fs::remove_dir_all(&root);
            panic!(
                "R8c task registration failed: stdout={} stderr={}",
                String::from_utf8_lossy(&create.stdout),
                String::from_utf8_lossy(&create.stderr)
            );
        }

        let result = std::panic::catch_unwind(|| {
            let run = schtasks(&["/Run", "/TN", &task_name]).expect("start R8c task");
            assert!(
                run.status.success(),
                "R8c task should start: {} {}",
                String::from_utf8_lossy(&run.stdout),
                String::from_utf8_lossy(&run.stderr)
            );

            let first_pid = wait_for_owner(
                &exe,
                &data_dir,
                &endpoint,
                &task_name,
                &root,
                None,
                Duration::from_secs(20),
            );
            let killed = taskkill(first_pid);
            assert!(
                killed.status.success(),
                "first runwatchd should be killed: {} {}",
                String::from_utf8_lossy(&killed.stdout),
                String::from_utf8_lossy(&killed.stderr)
            );

            let second_pid = wait_for_owner(
                &exe,
                &data_dir,
                &endpoint,
                &task_name,
                &root,
                Some(first_pid),
                Duration::from_secs(20),
            );
            assert_ne!(
                first_pid, second_pid,
                "runwatch supervisor must create a new daemon owner"
            );
            eprintln!(
                "R8C_SUPERVISOR_RESTART first_pid={first_pid} second_pid={second_pid} endpoint={endpoint}"
            );

            let supervisor_path = data_dir.join("supervise.pid");
            let first_supervisor_pid =
                read_pid(&supervisor_path).expect("supervisor pid should exist");
            let killed_supervisor = taskkill(first_supervisor_pid);
            assert!(
                killed_supervisor.status.success(),
                "supervisor should be killed: {} {}",
                String::from_utf8_lossy(&killed_supervisor.stdout),
                String::from_utf8_lossy(&killed_supervisor.stderr)
            );

            let second_supervisor_pid = wait_for_pid(
                &supervisor_path,
                Some(first_supervisor_pid),
                Duration::from_secs(80),
            );
            let third_pid = wait_for_owner(
                &exe,
                &data_dir,
                &endpoint,
                &task_name,
                &root,
                Some(second_pid),
                Duration::from_secs(20),
            );
            assert_ne!(
                first_supervisor_pid, second_supervisor_pid,
                "Task Scheduler reconcile must create a new supervisor owner"
            );
            assert_ne!(
                second_pid, third_pid,
                "Job Object must remove the old serve child before the new supervisor starts one"
            );
            eprintln!(
                "R8C_TASK_RECONCILE first_supervisor_pid={first_supervisor_pid} second_supervisor_pid={second_supervisor_pid} old_serve_pid={second_pid} new_serve_pid={third_pid}"
            );
        });

        let _ = schtasks(&["/End", "/TN", &task_name]);
        if let Some(pid) = read_pid(&data_dir.join("supervise.pid")) {
            let _ = taskkill(pid);
        }
        if let Some(pid) = read_pid(&data_dir.join("serve.pid")) {
            let _ = taskkill(pid);
        }
        let delete = schtasks(&["/Delete", "/TN", &task_name, "/F"]).expect("delete R8c task");
        let _ = fs::remove_dir_all(&root);
        assert!(
            delete.status.success(),
            "R8c temporary task should be deleted: {} {}",
            String::from_utf8_lossy(&delete.stdout),
            String::from_utf8_lossy(&delete.stderr)
        );

        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }
}
