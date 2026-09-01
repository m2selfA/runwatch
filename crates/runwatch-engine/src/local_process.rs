use anyhow::{Context, Result, bail};
use base64::Engine;
use chrono::Utc;
use runwatch_core::{
    ContinuationBinding, RemoteWorkspaceRef, RunAttemptRecord, RunRecord, RunResources, RunStatus,
    RunStore, RunnerKind, SubmitRunSpec, parse_terminal,
};
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
#[cfg(windows)]
use windows_sys::Win32::System::Threading::{
    CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, GetExitCodeProcess,
    GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};

const MAX_COMMAND_BYTES: usize = 64 * 1024;
const LOCAL_HOST_ALIAS: &str = "local";
const STILL_ACTIVE_EXIT_CODE: u32 = 259;
const START_GATE_TIMEOUT_HOURS: u32 = 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalProcessHandle {
    pub pid: u32,
    pub creation_time: u64,
}

impl LocalProcessHandle {
    pub fn encode(self) -> String {
        format!("local:{}:{:016x}", self.pid, self.creation_time)
    }

    pub fn parse(value: &str) -> Result<Self> {
        let mut parts = value.split(':');
        if parts.next() != Some("local") {
            bail!("invalid local process handle {value:?}");
        }
        let pid = parts
            .next()
            .context("local process handle is missing pid")?
            .parse::<u32>()
            .context("parse local process pid")?;
        let creation_time = u64::from_str_radix(
            parts
                .next()
                .context("local process handle is missing creation time")?,
            16,
        )
        .context("parse local process creation time")?;
        if parts.next().is_some() || pid == 0 {
            bail!("invalid local process handle {value:?}");
        }
        Ok(Self { pid, creation_time })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalProcessState {
    Running,
    Exited(u32),
    Missing,
    Reused,
}

#[derive(Debug, Deserialize)]
struct StartedReceipt {
    pid: u32,
}

#[derive(Debug, Clone)]
struct LocalPlan {
    spec: SubmitRunSpec,
    attempt_no: u32,
    run_dir: PathBuf,
    wrapper_path: PathBuf,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    terminal_path: PathBuf,
    receipt_path: PathBuf,
    started_path: PathBuf,
    armed_path: PathBuf,
}

impl LocalPlan {
    fn build(spec: SubmitRunSpec) -> Result<Self> {
        validate_local_spec(&spec)?;
        let attempt_no = 1;
        let run_dir = PathBuf::from(&spec.workspace.cwd)
            .join(".runwatch")
            .join(&spec.run_id);
        Ok(Self {
            wrapper_path: run_dir.join(format!("attempt-{attempt_no}.ps1")),
            stdout_path: run_dir.join("stdout.log"),
            stderr_path: run_dir.join("stderr.log"),
            terminal_path: run_dir.join("terminal.json"),
            receipt_path: run_dir.join("submission.receipt"),
            started_path: run_dir.join("started.json"),
            armed_path: run_dir.join("armed"),
            spec,
            attempt_no,
            run_dir,
        })
    }

    fn run_and_attempt(&self) -> (RunRecord, RunAttemptRecord) {
        let now = Utc::now();
        let mut run = RunRecord::new(
            self.spec.run_id.clone(),
            LOCAL_HOST_ALIAS.into(),
            RunnerKind::Process,
        );
        run.name = self.spec.name.clone();
        run.status = RunStatus::Submitting;
        run.workspace = Some(self.spec.workspace.clone());
        run.attempt_no = Some(self.attempt_no);
        run.remote_terminal = Some(path_text(&self.terminal_path));
        if let Some(binding) = &self.spec.continuation {
            run.session_id = Some(binding.session_id.clone());
            run.agent = Some(binding.agent_kind.clone());
            run.project_root = Some(binding.project_root.clone());
        }
        run.updated_at = now;

        let attempt = RunAttemptRecord {
            run_id: self.spec.run_id.clone(),
            attempt_no: self.attempt_no,
            runner: RunnerKind::Process,
            host: LOCAL_HOST_ALIAS.into(),
            workdir: self.spec.workspace.cwd.clone(),
            command: self.spec.command.clone(),
            resources: self.spec.resources.clone(),
            job_name: format!("rw-local-{}-a{}", self.spec.run_id, self.attempt_no),
            job_id: None,
            script_path: path_text(&self.wrapper_path),
            stdout_path: path_text(&self.stdout_path),
            stderr_path: path_text(&self.stderr_path),
            terminal_path: path_text(&self.terminal_path),
            receipt_path: path_text(&self.receipt_path),
            status: RunStatus::Submitting,
            created_at: now,
            updated_at: now,
            error: None,
        };
        (run, attempt)
    }

    fn wrapper_script(&self) -> String {
        let encoded = encode_powershell(&self.spec.command);
        let q = ps_quote;
        format!(
            "$ErrorActionPreference='Stop'\n\
             $started={}\n\
             $armed={}\n\
             $terminal={}\n\
             $stdout={}\n\
             $stderr={}\n\
             function Write-AtomicJson([string]$path,[hashtable]$value) {{\n\
               $tmp=\"$path.tmp.$PID\"\n\
               $json=$value | ConvertTo-Json -Compress\n\
               [IO.File]::WriteAllText($tmp,$json,[Text.UTF8Encoding]::new($false))\n\
               Move-Item -LiteralPath $tmp -Destination $path -Force\n\
             }}\n\
             Write-AtomicJson $started @{{schema_version=1;run_id={};attempt_no={};pid=$PID}}\n\
             $deadline=[DateTime]::UtcNow.AddHours({})\n\
             while(-not (Test-Path -LiteralPath $armed)) {{\n\
               if([DateTime]::UtcNow -ge $deadline) {{\n\
                 Write-AtomicJson $terminal @{{schema_version=1;run_id={};attempt_no={};status='failed';exit_code=124;finished_at=[DateTime]::UtcNow.ToString('o')}}\n\
                 exit 124\n\
               }}\n\
               Start-Sleep -Milliseconds 200\n\
             }}\n\
             $ErrorActionPreference='Continue'\n\
             $pwsh=Join-Path $PSHOME 'pwsh.exe'\n\
             & $pwsh -NoLogo -NoProfile -NonInteractive -EncodedCommand {} 1> $stdout 2> $stderr\n\
             $rc=$LASTEXITCODE\n\
             if($rc -eq 0) {{$status='succeeded'}} else {{$status='failed'}}\n\
             Write-AtomicJson $terminal @{{schema_version=1;run_id={};attempt_no={};status=$status;exit_code=$rc;finished_at=[DateTime]::UtcNow.ToString('o')}}\n\
             exit $rc\n",
            q(&path_text(&self.started_path)),
            q(&path_text(&self.armed_path)),
            q(&path_text(&self.terminal_path)),
            q(&path_text(&self.stdout_path)),
            q(&path_text(&self.stderr_path)),
            q(&self.spec.run_id),
            self.attempt_no,
            START_GATE_TIMEOUT_HOURS,
            q(&self.spec.run_id),
            self.attempt_no,
            q(&encoded),
            q(&self.spec.run_id),
            self.attempt_no,
        )
    }
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn ps_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn encode_powershell(command: &str) -> String {
    let bytes: Vec<u8> = command.encode_utf16().flat_map(u16::to_le_bytes).collect();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn validate_run_id(run_id: &str) -> Result<()> {
    if run_id.is_empty()
        || run_id.len() > 96
        || !run_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    {
        bail!("run_id must be 1..96 ASCII letters/digits/._-");
    }
    Ok(())
}

fn validate_binding(binding: &ContinuationBinding, workspace: &RemoteWorkspaceRef) -> Result<()> {
    if binding.agent_kind != "pi"
        || binding.session_id.trim().is_empty()
        || binding.project_root.contains(['\r', '\n', '\0'])
        || &binding.workspace != workspace
    {
        bail!("continuation binding is invalid or does not match the submitted workspace");
    }
    for value in [
        binding.session_file.as_deref(),
        binding.origin_leaf_id.as_deref(),
        binding.adapter_path.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        if value.is_empty() || value.contains(['\r', '\n', '\0']) {
            bail!("continuation binding contains invalid multiline/NUL identity data");
        }
    }
    Ok(())
}

fn validate_local_spec(spec: &SubmitRunSpec) -> Result<()> {
    if !cfg!(windows) {
        bail!("Local Process v1 is currently supported on Windows only");
    }
    validate_run_id(&spec.run_id)?;
    if spec.runner != RunnerKind::Process {
        bail!("local submission requires runner=process");
    }
    if spec.command.trim().is_empty() || spec.command.len() > MAX_COMMAND_BYTES {
        bail!("command must be non-empty and at most {MAX_COMMAND_BYTES} bytes");
    }
    if spec.command.contains('\0') {
        bail!("command must not contain NUL");
    }
    if spec.workspace.host_alias != LOCAL_HOST_ALIAS {
        bail!("local Process workspace.host_alias must be 'local'");
    }
    if spec.workspace.cwd.contains(['\r', '\n', '\0']) {
        bail!("local Process workspace.cwd must be a single-line absolute path");
    }
    let cwd = Path::new(&spec.workspace.cwd);
    if !cwd.is_absolute() || !cwd.is_dir() {
        bail!("local Process workspace.cwd must be an existing absolute directory");
    }
    if spec.resources != RunResources::default() {
        bail!("local Process v1 does not accept scheduler resource requests");
    }
    if let Some(binding) = &spec.continuation {
        validate_binding(binding, &spec.workspace)?;
    }
    Ok(())
}

fn submission_identity_matches(existing: &RunAttemptRecord, desired: &RunAttemptRecord) -> bool {
    existing.attempt_no == desired.attempt_no
        && existing.runner == desired.runner
        && existing.host == desired.host
        && existing.workdir == desired.workdir
        && existing.command == desired.command
        && existing.resources == desired.resources
}

fn started_path(attempt: &RunAttemptRecord) -> Result<PathBuf> {
    Ok(Path::new(&attempt.terminal_path)
        .parent()
        .context("local terminal path has no parent")?
        .join("started.json"))
}

fn armed_path(attempt: &RunAttemptRecord) -> Result<PathBuf> {
    Ok(Path::new(&attempt.terminal_path)
        .parent()
        .context("local terminal path has no parent")?
        .join("armed"))
}

fn read_started_pid(path: &Path) -> Result<Option<u32>> {
    if !path.exists() {
        return Ok(None);
    }
    let value: StartedReceipt =
        serde_json::from_str(&fs::read_to_string(path)?).context("parse local started.json")?;
    Ok(Some(value.pid))
}

fn read_receipt(path: &Path) -> Result<Option<LocalProcessHandle>> {
    if !path.exists() {
        return Ok(None);
    }
    let value = fs::read_to_string(path)?;
    Ok(Some(LocalProcessHandle::parse(value.trim())?))
}

fn write_receipt(path: &Path, handle: LocalProcessHandle) -> Result<()> {
    if path.exists() {
        let existing = read_receipt(path)?.context("local receipt disappeared")?;
        if existing != handle {
            bail!("local submission receipt conflicts with live process identity");
        }
        return Ok(());
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    fs::write(&tmp, format!("{}\n", handle.encode()))?;
    fs::rename(&tmp, path)?;
    Ok(())
}

fn arm(attempt: &RunAttemptRecord) -> Result<()> {
    let path = armed_path(attempt)?;
    if !path.exists() {
        fs::write(path, b"armed\n")?;
    }
    Ok(())
}

#[cfg(windows)]
fn filetime_value(value: FILETIME) -> u64 {
    (u64::from(value.dwHighDateTime) << 32) | u64::from(value.dwLowDateTime)
}

#[cfg(windows)]
fn query_pid(pid: u32) -> Result<Option<(LocalProcessHandle, u32)>> {
    // SAFETY: OpenProcess is called with a numeric PID and a read-only query access mask.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(87) {
            return Ok(None);
        }
        return Err(error).with_context(|| format!("open local process pid {pid}"));
    }
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    let mut exit_code = 0u32;
    // SAFETY: all output pointers refer to live stack variables and handle is valid until CloseHandle.
    let times_ok =
        unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) };
    // SAFETY: exit_code is a live output pointer and handle is still valid.
    let code_ok = unsafe { GetExitCodeProcess(handle, &mut exit_code) };
    // SAFETY: handle was opened above and is closed exactly once here.
    unsafe { CloseHandle(handle) };
    if times_ok == 0 || code_ok == 0 {
        return Err(std::io::Error::last_os_error()).context("query local process identity");
    }
    Ok(Some((
        LocalProcessHandle {
            pid,
            creation_time: filetime_value(creation),
        },
        exit_code,
    )))
}

#[cfg(not(windows))]
fn query_pid(_pid: u32) -> Result<Option<(LocalProcessHandle, u32)>> {
    bail!("Local Process v1 is currently supported on Windows only")
}

pub fn query_handle(handle: LocalProcessHandle) -> Result<LocalProcessState> {
    let Some((actual, exit_code)) = query_pid(handle.pid)? else {
        return Ok(LocalProcessState::Missing);
    };
    if actual.creation_time != handle.creation_time {
        return Ok(LocalProcessState::Reused);
    }
    if exit_code == STILL_ACTIVE_EXIT_CODE {
        Ok(LocalProcessState::Running)
    } else {
        Ok(LocalProcessState::Exited(exit_code))
    }
}

fn recover_handle(attempt: &RunAttemptRecord) -> Result<Option<LocalProcessHandle>> {
    if let Some(handle) = attempt
        .job_id
        .as_deref()
        .map(LocalProcessHandle::parse)
        .transpose()?
    {
        return Ok(Some(handle));
    }
    if let Some(handle) = read_receipt(Path::new(&attempt.receipt_path))? {
        return Ok(Some(handle));
    }
    let Some(pid) = read_started_pid(&started_path(attempt)?)? else {
        return Ok(None);
    };
    Ok(query_pid(pid)?.map(|(handle, _)| handle))
}

#[cfg(windows)]
fn local_process_spawn_error(error: std::io::Error) -> anyhow::Error {
    if error.raw_os_error() == Some(5) {
        anyhow::anyhow!(
            "spawn local Process wrapper denied Windows job breakaway (ERROR_ACCESS_DENIED); refusing a non-durable launch. Start runwatch through `runwatch supervise` / Task Scheduler autostart, or another host process whose Job Object permits breakaway: {error}"
        )
    } else {
        anyhow::anyhow!("spawn local Process wrapper: {error}")
    }
}

#[cfg(windows)]
fn spawn_wrapper(plan: &LocalPlan) -> Result<LocalProcessHandle> {
    let mut command = Command::new("pwsh.exe");
    command
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(&plan.wrapper_path)
        .current_dir(&plan.spec.workspace.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_BREAKAWAY_FROM_JOB | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    let child = command.spawn().map_err(local_process_spawn_error)?;
    let pid = child.id();
    drop(child);
    let (handle, _) = query_pid(pid)?.context("local Process wrapper disappeared during launch")?;
    Ok(handle)
}

#[cfg(not(windows))]
fn spawn_wrapper(_plan: &LocalPlan) -> Result<LocalProcessHandle> {
    bail!("Local Process v1 is currently supported on Windows only")
}

fn mark_submission_failed(
    store: &RunStore,
    mut run: RunRecord,
    mut attempt: RunAttemptRecord,
    error: String,
) -> Result<()> {
    let now = Utc::now();
    run.status = RunStatus::Failed;
    run.updated_at = now;
    run.note = Some(error.clone());
    attempt.status = RunStatus::Failed;
    attempt.updated_at = now;
    attempt.error = Some(error);
    store.persist_run_attempt_event(&run, &attempt, "local_submission_failed")
}

pub fn submit_local_run(store: &RunStore, spec: SubmitRunSpec) -> Result<RunRecord> {
    let plan = LocalPlan::build(spec)?;
    let (desired_run, desired_attempt) = plan.run_and_attempt();
    let created = store.create_submission_intent(
        &desired_run,
        &desired_attempt,
        plan.spec.continuation.as_ref(),
    )?;
    let (mut run, mut attempt) = if created {
        (desired_run, desired_attempt)
    } else {
        let run = store
            .get(&plan.spec.run_id)?
            .context("local submission Run exists but could not be loaded")?;
        let attempt = store
            .get_attempt(&plan.spec.run_id, plan.attempt_no)?
            .context("local submission Run exists without attempt 1")?;
        if !submission_identity_matches(&attempt, &desired_attempt) {
            bail!(
                "run_id {} already exists with a different local submission spec",
                plan.spec.run_id
            );
        }
        if attempt.job_id.is_some() && run.status != RunStatus::Submitting {
            return Ok(run);
        }
        (run, attempt)
    };

    fs::create_dir_all(&plan.run_dir)?;
    fs::write(&plan.wrapper_path, plan.wrapper_script())?;

    let mut handle = recover_handle(&attempt)?;
    if let Some(candidate) = handle
        && matches!(
            query_handle(candidate)?,
            LocalProcessState::Missing | LocalProcessState::Reused
        )
    {
        handle = None;
    }
    let handle = match handle {
        Some(handle) => handle,
        None => {
            for path in [&plan.receipt_path, &plan.started_path, &plan.armed_path] {
                let _ = fs::remove_file(path);
            }
            match spawn_wrapper(&plan) {
                Ok(handle) => handle,
                Err(error) => {
                    let message = format!("local Process launch failed: {error:#}");
                    mark_submission_failed(store, run, attempt, message.clone())?;
                    bail!(message);
                }
            }
        }
    };

    write_receipt(&plan.receipt_path, handle)?;
    let now = Utc::now();
    let job_id = handle.encode();
    run.job_id = Some(job_id.clone());
    run.status = RunStatus::Running;
    run.updated_at = now;
    run.note = None;
    attempt.job_id = Some(job_id);
    attempt.status = RunStatus::Running;
    attempt.updated_at = now;
    attempt.error = None;
    store.persist_run_attempt_event(&run, &attempt, "local_process_started")?;
    arm(&attempt)?;
    Ok(run)
}

fn cancel_requested(run: &RunRecord) -> bool {
    run.note
        .as_deref()
        .is_some_and(|value| value.starts_with("local process cancel requested"))
}

pub fn observe_local_run(store: &RunStore, run: &mut RunRecord) -> Result<RunStatus> {
    let attempt_no = run
        .attempt_no
        .context("local Process Run has no attempt metadata")?;
    let mut attempt = store
        .get_attempt(&run.run_id, attempt_no)?
        .context("local Process attempt metadata is missing")?;

    if Path::new(&attempt.terminal_path).exists() {
        let text = fs::read_to_string(&attempt.terminal_path)?;
        return Ok(parse_terminal(&text).unwrap_or(RunStatus::Failed));
    }

    let Some(handle) = recover_handle(&attempt)? else {
        return Ok(run.status);
    };
    let encoded = handle.encode();
    if run.job_id.as_deref() != Some(&encoded) || attempt.job_id.as_deref() != Some(&encoded) {
        let now = Utc::now();
        run.job_id = Some(encoded.clone());
        run.updated_at = now;
        attempt.job_id = Some(encoded);
        attempt.updated_at = now;
        store.persist_run_attempt_event(run, &attempt, "local_process_adopted")?;
    }

    match query_handle(handle)? {
        LocalProcessState::Running => {
            arm(&attempt)?;
            Ok(RunStatus::Running)
        }
        LocalProcessState::Exited(_) | LocalProcessState::Missing | LocalProcessState::Reused => {
            if Path::new(&attempt.terminal_path).exists() {
                let text = fs::read_to_string(&attempt.terminal_path)?;
                Ok(parse_terminal(&text).unwrap_or(RunStatus::Failed))
            } else if cancel_requested(run) {
                Ok(RunStatus::Cancelled)
            } else {
                Ok(RunStatus::Failed)
            }
        }
    }
}

pub fn request_cancel(store: &RunStore, run: &mut RunRecord) -> Result<()> {
    if !cfg!(windows) {
        bail!("Local Process v1 is currently supported on Windows only");
    }
    let handle = LocalProcessHandle::parse(
        run.job_id
            .as_deref()
            .context("local Process Run has no process handle")?,
    )?;
    if query_handle(handle)? != LocalProcessState::Running {
        bail!("local Process is no longer running; wait for observation before cancelling");
    }
    let now = Utc::now();
    run.updated_at = now;
    run.note = Some(format!(
        "local process cancel requested for {}",
        handle.encode()
    ));
    let attempt_no = run
        .attempt_no
        .context("local Process has no attempt metadata")?;
    let attempt = store
        .get_attempt(&run.run_id, attempt_no)?
        .context("local Process attempt metadata is missing")?;
    store.persist_run_attempt_event(run, &attempt, "cancel_requested")?;

    let output = Command::new("taskkill.exe")
        .args(["/PID", &handle.pid.to_string(), "/T", "/F"])
        .stdin(Stdio::null())
        .output()
        .context("taskkill local Process tree")?;
    if !output.status.success() {
        bail!(
            "local Process cancellation failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_handle_roundtrips_and_rejects_other_handles() {
        let handle = LocalProcessHandle {
            pid: 42,
            creation_time: 0x1234_5678_9abc_def0,
        };
        assert_eq!(LocalProcessHandle::parse(&handle.encode()).unwrap(), handle);
        assert!(LocalProcessHandle::parse("123").is_err());
        assert!(LocalProcessHandle::parse("local:0:0001").is_err());
    }

    #[test]
    fn powershell_payload_is_utf16le_base64() {
        let encoded = encode_powershell("Write-Output 'ok'");
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap();
        let words: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        assert_eq!(String::from_utf16(&words).unwrap(), "Write-Output 'ok'");
    }

    #[test]
    fn wrapper_protocol_has_started_arm_and_atomic_terminal_boundaries() {
        let spec = SubmitRunSpec {
            run_id: "local-wrapper-test".into(),
            name: None,
            workspace: RemoteWorkspaceRef {
                host_alias: LOCAL_HOST_ALIAS.into(),
                cwd: std::env::temp_dir().to_string_lossy().into_owned(),
            },
            runner: RunnerKind::Process,
            command: "Write-Output 'science'".into(),
            resources: RunResources::default(),
            continuation: None,
        };
        let script = LocalPlan::build(spec).unwrap().wrapper_script();
        assert!(script.contains("started.json"));
        assert!(script.contains("while(-not (Test-Path -LiteralPath $armed))"));
        assert!(script.contains("Move-Item -LiteralPath $tmp -Destination $path -Force"));
        assert!(script.contains("terminal.json"));
        assert!(script.contains("-EncodedCommand"));
    }

    #[cfg(windows)]
    #[test]
    fn access_denied_explains_durable_breakaway_requirement() {
        let message = local_process_spawn_error(std::io::Error::from_raw_os_error(5)).to_string();
        assert!(message.contains("breakaway"));
        assert!(message.contains("non-durable"));
        assert!(message.contains("runwatch supervise"));
    }

    #[cfg(windows)]
    #[test]
    fn local_process_identity_observes_running_process_and_pid_token() {
        let mut child = Command::new("pwsh.exe")
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Start-Sleep -Seconds 2",
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .expect("spawn identity smoke process");
        let pid = child.id();
        let (handle, _) = query_pid(pid)
            .expect("query pid")
            .expect("process should exist");
        assert_eq!(handle.pid, pid);
        assert_ne!(handle.creation_time, 0);
        assert_eq!(query_handle(handle).unwrap(), LocalProcessState::Running);
        child.wait().expect("wait identity smoke process");
        assert!(matches!(
            query_handle(handle).unwrap(),
            LocalProcessState::Exited(_) | LocalProcessState::Missing
        ));
    }
}
