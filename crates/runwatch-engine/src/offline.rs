use anyhow::{Context, Result, bail};
use base64::Engine;
use runwatch_core::{AgentInvocationRecord, DeliveryPayload, RunStore};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{BufRead as StdBufRead, BufReader as StdBufReader};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};
use tokio::process::Command;

const OFFLINE_GRACE: Duration = Duration::from_secs(20);
const MAX_LAUNCHES_PER_TICK: usize = 2;
const MAX_AGENT_RUNTIME: Duration = Duration::from_secs(6 * 60 * 60);
const DELIVERY_STDIN_POLL: Duration = Duration::from_millis(500);
pub const ORPHAN_RECONNECT_GRACE: Duration = Duration::from_secs(30);
const ORPHAN_RETRY_DELAY: Duration = Duration::from_secs(30);
const MAX_CODEX_EVENT_LINE_BYTES: usize = 256 * 1024;
const MAX_CODEX_STDERR_TAIL_BYTES: usize = 64 * 1024;
const MAX_CODEX_PROMPT_BYTES: usize = 8 * 1024;
const MAX_CODEX_ROLLOUT_LINE_BYTES: usize = 1024 * 1024;
const CODEX_ROLLOUT_QUIET_PERIOD: Duration = Duration::from_secs(3);

fn delivery_keeps_rpc_stdin_open(state: Option<&str>) -> bool {
    matches!(state, Some("delivering"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AgentLauncher {
    executable: OsString,
    prefix_args: Vec<OsString>,
}

fn find_in_path(path: Option<&OsStr>, file_name: &str) -> Option<PathBuf> {
    let path = path?;
    std::env::split_paths(path)
        .map(|dir| dir.join(file_name))
        .find(|candidate| candidate.is_file())
}

#[cfg(windows)]
fn resolve_default_pi_launcher(path: Option<&OsStr>) -> Result<AgentLauncher> {
    if let Some(executable) = find_in_path(path, "pi.exe") {
        return Ok(AgentLauncher {
            executable: executable.into_os_string(),
            prefix_args: Vec::new(),
        });
    }
    if let Some(volta) = find_in_path(path, "volta.exe") {
        return Ok(AgentLauncher {
            executable: volta.into_os_string(),
            prefix_args: vec![OsString::from("run"), OsString::from("pi")],
        });
    }
    bail!(
        "no native Pi launcher found on PATH: expected pi.exe or volta.exe. Windows shell shims such as pi.cmd are not used for unattended continuation; set RUNWATCH_PI_EXECUTABLE to a native launcher"
    )
}

#[cfg(not(windows))]
fn resolve_default_pi_launcher(_path: Option<&OsStr>) -> Result<AgentLauncher> {
    Ok(AgentLauncher {
        executable: OsString::from("pi"),
        prefix_args: Vec::new(),
    })
}

fn resolve_pi_launcher() -> Result<AgentLauncher> {
    if let Some(executable) = std::env::var_os("RUNWATCH_PI_EXECUTABLE") {
        #[cfg(windows)]
        if matches!(
            Path::new(&executable)
                .extension()
                .and_then(OsStr::to_str)
                .map(|value| value.to_ascii_lowercase())
                .as_deref(),
            Some("cmd" | "bat")
        ) {
            bail!(
                "RUNWATCH_PI_EXECUTABLE must be a native executable on Windows, not a .cmd/.bat shell shim"
            );
        }
        return Ok(AgentLauncher {
            executable,
            prefix_args: Vec::new(),
        });
    }
    resolve_default_pi_launcher(std::env::var_os("PATH").as_deref())
}

#[cfg(windows)]
fn resolve_default_codex_launcher(path: Option<&OsStr>) -> Result<AgentLauncher> {
    if let Some(executable) = find_in_path(path, "codex.exe") {
        return Ok(AgentLauncher {
            executable: executable.into_os_string(),
            prefix_args: Vec::new(),
        });
    }
    bail!(
        "no native Codex launcher found on PATH: expected codex.exe. Windows shell shims such as codex.cmd are not used for unattended continuation; set RUNWATCH_CODEX_EXECUTABLE to a native launcher"
    )
}

#[cfg(not(windows))]
fn resolve_default_codex_launcher(_path: Option<&OsStr>) -> Result<AgentLauncher> {
    Ok(AgentLauncher {
        executable: OsString::from("codex"),
        prefix_args: Vec::new(),
    })
}

fn resolve_codex_launcher() -> Result<AgentLauncher> {
    if let Some(executable) = std::env::var_os("RUNWATCH_CODEX_EXECUTABLE") {
        #[cfg(windows)]
        if matches!(
            Path::new(&executable)
                .extension()
                .and_then(OsStr::to_str)
                .map(|value| value.to_ascii_lowercase())
                .as_deref(),
            Some("cmd" | "bat")
        ) {
            bail!(
                "RUNWATCH_CODEX_EXECUTABLE must be a native executable on Windows, not a .cmd/.bat shell shim"
            );
        }
        return Ok(AgentLauncher {
            executable,
            prefix_args: Vec::new(),
        });
    }
    resolve_default_codex_launcher(std::env::var_os("PATH").as_deref())
}

fn codex_completion_prompt(payload: &DeliveryPayload) -> Result<String> {
    let envelope = serde_json::json!({
        "delivery_id": payload.delivery_id,
        "run_id": payload.run_id,
        "attempt_no": payload.attempt_no,
        "status": payload.status,
        "job_id": payload.job_id,
        "workspace": payload.workspace,
    });
    let prompt = format!(
        "[runwatch continuation delivery_id={}]\nA durable scientific computation created by this exact Codex thread reached terminal state. Run metadata: {}\nContinue the existing scientific task from this thread's prior context. Do not resubmit the completed attempt. Inspect runwatch status/logs/artifacts and the recorded workspace as needed, then continue the scientific reasoning and next appropriate work.",
        payload.delivery_id, envelope
    );
    if prompt.len() > MAX_CODEX_PROMPT_BYTES {
        bail!("Codex continuation prompt exceeds {MAX_CODEX_PROMPT_BYTES} bytes");
    }
    Ok(prompt)
}

fn codex_delivery_marker(delivery_id: &str) -> String {
    format!("[runwatch continuation delivery_id={delivery_id}]")
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CodexRolloutState {
    Idle,
    ThreadBusy {
        turn_id: String,
    },
    DeliveryRunning {
        turn_id: String,
        started_at: Option<chrono::DateTime<chrono::Utc>>,
    },
    DeliveryCompleted {
        turn_id: String,
    },
    Ambiguous {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CodexRolloutInspection {
    state: CodexRolloutState,
    quiet_for: Option<Duration>,
    malformed_lines: u32,
    skipped_large_lines: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CodexRolloutLine {
    Eof,
    Line(Vec<u8>),
    SkippedTooLarge,
}

fn read_codex_rollout_line<R>(reader: &mut R) -> Result<CodexRolloutLine>
where
    R: StdBufRead,
{
    let mut line = Vec::new();
    let mut too_large = false;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return if too_large {
                Ok(CodexRolloutLine::SkippedTooLarge)
            } else if line.is_empty() {
                Ok(CodexRolloutLine::Eof)
            } else {
                Ok(CodexRolloutLine::Line(line))
            };
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let take = newline.unwrap_or(available.len());
        if !too_large {
            if line.len().saturating_add(take) > MAX_CODEX_ROLLOUT_LINE_BYTES {
                too_large = true;
                line.clear();
            } else {
                line.extend_from_slice(&available[..take]);
            }
        }
        let consumed = take + usize::from(newline.is_some());
        reader.consume(consumed);
        if newline.is_some() {
            return if too_large {
                Ok(CodexRolloutLine::SkippedTooLarge)
            } else {
                Ok(CodexRolloutLine::Line(line))
            };
        }
    }
}

fn parse_rollout_started_at(value: &serde_json::Value) -> Option<chrono::DateTime<chrono::Utc>> {
    value
        .get("payload")?
        .get("started_at")?
        .as_str()
        .and_then(|started| chrono::DateTime::parse_from_rfc3339(started).ok())
        .map(|started| started.with_timezone(&chrono::Utc))
}

fn rollout_user_message_contains_marker(value: &serde_json::Value, marker: &str) -> bool {
    let payload = match value.get("payload") {
        Some(payload) => payload,
        None => return false,
    };
    if value.get("type").and_then(serde_json::Value::as_str) != Some("response_item")
        || payload.get("type").and_then(serde_json::Value::as_str) != Some("message")
        || payload.get("role").and_then(serde_json::Value::as_str) != Some("user")
    {
        return false;
    }
    payload
        .get("content")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|items| {
            items.iter().any(|item| {
                item.get("type").and_then(serde_json::Value::as_str) == Some("input_text")
                    && item
                        .get("text")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|text| text.contains(marker))
            })
        })
}

fn inspect_codex_rollout(path: &Path, delivery_id: &str) -> Result<CodexRolloutInspection> {
    let marker = codex_delivery_marker(delivery_id);
    let file =
        File::open(path).with_context(|| format!("open Codex rollout {}", path.display()))?;
    let quiet_for = file
        .metadata()
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|modified| modified.elapsed().ok());
    let mut reader = StdBufReader::new(file);
    let mut active_turn: Option<(String, Option<chrono::DateTime<chrono::Utc>>)> = None;
    let mut marker_turn: Option<(String, Option<chrono::DateTime<chrono::Utc>>)> = None;
    let mut marker_completed = false;
    let mut marker_count = 0u32;
    let mut marker_without_turn = false;
    let mut malformed_lines = 0u32;
    let mut skipped_large_lines = 0u32;

    loop {
        let line = match read_codex_rollout_line(&mut reader)? {
            CodexRolloutLine::Eof => break,
            CodexRolloutLine::SkippedTooLarge => {
                skipped_large_lines += 1;
                continue;
            }
            CodexRolloutLine::Line(line) => line,
        };
        if line.iter().all(|byte| byte.is_ascii_whitespace()) {
            continue;
        }
        let value = match serde_json::from_slice::<serde_json::Value>(&line) {
            Ok(value) => value,
            Err(_) => {
                malformed_lines += 1;
                continue;
            }
        };
        let top = value.get("type").and_then(serde_json::Value::as_str);
        let payload = value.get("payload");
        let payload_type = payload
            .and_then(|payload| payload.get("type"))
            .and_then(serde_json::Value::as_str);

        if top == Some("event_msg") && payload_type == Some("task_started") {
            if let Some(turn_id) = payload
                .and_then(|payload| payload.get("turn_id"))
                .and_then(serde_json::Value::as_str)
            {
                active_turn = Some((turn_id.to_string(), parse_rollout_started_at(&value)));
            }
            continue;
        }
        if top == Some("event_msg") && payload_type == Some("task_complete") {
            if let Some(turn_id) = payload
                .and_then(|payload| payload.get("turn_id"))
                .and_then(serde_json::Value::as_str)
            {
                if marker_turn
                    .as_ref()
                    .is_some_and(|(marker_turn_id, _)| marker_turn_id == turn_id)
                {
                    marker_completed = true;
                }
                if active_turn
                    .as_ref()
                    .is_some_and(|(active_turn_id, _)| active_turn_id == turn_id)
                {
                    active_turn = None;
                }
            }
            continue;
        }
        if rollout_user_message_contains_marker(&value, &marker) {
            marker_count += 1;
            if marker_count == 1 {
                if let Some((turn_id, started_at)) = active_turn.clone() {
                    marker_turn = Some((turn_id, started_at));
                } else {
                    marker_without_turn = true;
                }
            }
        }
    }

    let state = if marker_count > 1 {
        CodexRolloutState::Ambiguous {
            reason: format!(
                "Codex rollout contains {marker_count} user messages for Delivery {delivery_id}"
            ),
        }
    } else if marker_without_turn {
        CodexRolloutState::Ambiguous {
            reason: format!(
                "Codex rollout contains Delivery {delivery_id} marker outside an active turn"
            ),
        }
    } else if let Some((turn_id, started_at)) = marker_turn {
        if marker_completed {
            CodexRolloutState::DeliveryCompleted { turn_id }
        } else if active_turn
            .as_ref()
            .is_some_and(|(active_turn_id, _)| active_turn_id == &turn_id)
        {
            CodexRolloutState::DeliveryRunning {
                turn_id,
                started_at,
            }
        } else {
            CodexRolloutState::Ambiguous {
                reason: format!(
                    "Codex Delivery {delivery_id} turn {turn_id} has no matching task_complete but is no longer the active turn"
                ),
            }
        }
    } else if let Some((turn_id, _)) = active_turn {
        CodexRolloutState::ThreadBusy { turn_id }
    } else {
        CodexRolloutState::Idle
    };

    Ok(CodexRolloutInspection {
        state,
        quiet_for,
        malformed_lines,
        skipped_large_lines,
    })
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct CodexExecEvidence {
    thread_id: Option<String>,
    thread_mismatch: Option<String>,
    turn_started: bool,
    turn_completed: bool,
    turn_failed: bool,
    error_event: bool,
    malformed_events: u32,
    skipped_large_events: u32,
}

impl CodexExecEvidence {
    fn observe(&mut self, value: &serde_json::Value, expected_thread_id: &str) {
        match value.get("type").and_then(serde_json::Value::as_str) {
            Some("thread.started") => {
                let actual = value
                    .get("thread_id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if actual != expected_thread_id {
                    self.thread_mismatch = Some(actual.clone());
                }
                if self.thread_id.is_none() {
                    self.thread_id = Some(actual);
                }
            }
            Some("turn.started") => self.turn_started = true,
            Some("turn.completed") => self.turn_completed = true,
            Some("turn.failed") => self.turn_failed = true,
            Some("error") => self.error_event = true,
            _ => {}
        }
    }

    fn is_success(&self, expected_thread_id: &str) -> bool {
        self.thread_mismatch.is_none()
            && self.thread_id.as_deref() == Some(expected_thread_id)
            && self.turn_started
            && self.turn_completed
            && !self.turn_failed
            && !self.error_event
            && self.malformed_events == 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CodexLine {
    Eof,
    Line(Vec<u8>),
    SkippedTooLarge,
}

async fn read_codex_event_line<R>(reader: &mut BufReader<R>) -> Result<CodexLine>
where
    R: AsyncRead + Unpin,
{
    let mut line = Vec::new();
    let mut too_large = false;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if too_large {
                Ok(CodexLine::SkippedTooLarge)
            } else if line.is_empty() {
                Ok(CodexLine::Eof)
            } else {
                Ok(CodexLine::Line(line))
            };
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let take = newline.unwrap_or(available.len());
        if !too_large {
            if line.len().saturating_add(take) > MAX_CODEX_EVENT_LINE_BYTES {
                too_large = true;
                line.clear();
            } else {
                line.extend_from_slice(&available[..take]);
            }
        }
        let consumed = take + usize::from(newline.is_some());
        reader.consume(consumed);
        if newline.is_some() {
            return if too_large {
                Ok(CodexLine::SkippedTooLarge)
            } else {
                Ok(CodexLine::Line(line))
            };
        }
    }
}

async fn parse_codex_exec_stream<R>(
    reader: R,
    expected_thread_id: &str,
) -> Result<CodexExecEvidence>
where
    R: AsyncRead + Unpin,
{
    let mut reader = BufReader::new(reader);
    let mut evidence = CodexExecEvidence::default();
    loop {
        match read_codex_event_line(&mut reader).await? {
            CodexLine::Eof => break,
            CodexLine::SkippedTooLarge => evidence.skipped_large_events += 1,
            CodexLine::Line(line) => {
                if line.iter().all(|byte| byte.is_ascii_whitespace()) {
                    continue;
                }
                match serde_json::from_slice::<serde_json::Value>(&line) {
                    Ok(value) => evidence.observe(&value, expected_thread_id),
                    Err(_) => evidence.malformed_events += 1,
                }
            }
        }
    }
    Ok(evidence)
}

async fn drain_stderr_tail<R>(mut reader: R) -> Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let mut tail = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        tail.extend_from_slice(&buffer[..read]);
        if tail.len() > MAX_CODEX_STDERR_TAIL_BYTES {
            let excess = tail.len() - MAX_CODEX_STDERR_TAIL_BYTES;
            tail.drain(..excess);
        }
    }
    Ok(tail)
}

fn bounded_stderr_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim().to_string()
}

fn codex_resume_args(thread_id: &str, prompt: &str) -> Vec<OsString> {
    vec![
        OsString::from("exec"),
        OsString::from("resume"),
        OsString::from("--json"),
        OsString::from(thread_id),
        OsString::from(prompt),
    ]
}

pub fn reconcile_orphans(store: &RunStore) -> Result<u32> {
    store.reconcile_orphaned_agent_invocations(ORPHAN_RETRY_DELAY)
}

pub fn dispatch_due(store: Arc<RunStore>) -> Result<usize> {
    let mut launched = 0;
    for _ in 0..MAX_LAUNCHES_PER_TICK {
        let Some(invocation) = store.reserve_offline_invocation(OFFLINE_GRACE)? else {
            break;
        };
        let store = store.clone();
        tokio::spawn(async move {
            if let Err(err) = run_invocation(store.clone(), invocation.clone()).await {
                let agent_kind = &invocation.payload.binding.agent_kind;
                let _ = store.finish_agent_invocation_process(
                    &invocation.invocation_id,
                    None,
                    Some(&format!(
                        "offline {agent_kind} invocation task failed: {err:#}"
                    )),
                );
            }
        });
        launched += 1;
    }
    Ok(launched)
}

async fn run_invocation(store: Arc<RunStore>, invocation: AgentInvocationRecord) -> Result<()> {
    match invocation.payload.binding.agent_kind.as_str() {
        "pi" => run_pi_invocation(store, invocation).await,
        "codex" => run_codex_invocation(store, invocation).await,
        other => bail!("unsupported offline AgentAdapter {other}"),
    }
}

async fn run_pi_invocation(store: Arc<RunStore>, invocation: AgentInvocationRecord) -> Result<()> {
    let launcher = resolve_pi_launcher()?;
    let session_file = invocation
        .session_file
        .as_deref()
        .context("offline Pi invocation has no durable session file")?;
    let adapter_path = invocation
        .adapter_path
        .as_deref()
        .context("offline Pi invocation has no pi-runs adapter path")?;
    let payload = serde_json::to_vec(&invocation.payload)?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(payload);

    let mut command = Command::new(&launcher.executable);
    command
        .args(&launcher.prefix_args)
        .args([
            "--mode",
            "rpc",
            "--session",
            session_file,
            "-e",
            adapter_path,
        ])
        .current_dir(&invocation.project_root)
        .env("RUNWATCH_OFFLINE_DELIVERY_B64", encoded)
        .env(
            "RUNWATCH_OFFLINE_OWNER_INSTANCE_ID",
            &invocation.owner_instance_id,
        )
        .env("RUNWATCH_OFFLINE_INVOCATION_ID", &invocation.invocation_id)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.as_std_mut().creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            let message = format!("spawn offline Pi RPC worker: {err}");
            store.finish_agent_invocation_process(
                &invocation.invocation_id,
                None,
                Some(&message),
            )?;
            return Err(err).context("spawn offline Pi RPC worker");
        }
    };
    let pid = child.id().context("offline Pi RPC worker has no pid")?;
    store.set_agent_invocation_pid(&invocation.invocation_id, pid)?;

    // Keep RPC stdin open while the exact Delivery is owned by this worker. Successful agent
    // turns call ctx.shutdown() after agent_settled, but trust/branch rejection can durably move
    // the Delivery to needs_rebind during session_start before any agent event exists. Pi RPC only
    // observes that early shutdown reliably once stdin reaches EOF, so the daemon closes the pipe
    // as soon as the durable Delivery leaves `delivering`.
    let stdin_store = store.clone();
    let stdin_delivery_id = invocation.delivery_id.clone();
    let stdin_guard = child.stdin.take();
    let stdin_watch = tokio::spawn(async move {
        let _stdin_guard = stdin_guard;
        loop {
            match stdin_store.get_delivery_state(&stdin_delivery_id) {
                Ok(state) if !delivery_keeps_rpc_stdin_open(state.as_deref()) => break,
                Ok(_) | Err(_) => tokio::time::sleep(DELIVERY_STDIN_POLL).await,
            }
        }
    });
    let wait = tokio::time::timeout(MAX_AGENT_RUNTIME, child.wait()).await;
    stdin_watch.abort();
    let _ = stdin_watch.await;
    match wait {
        Ok(Ok(status)) => {
            store.finish_agent_invocation_process(
                &invocation.invocation_id,
                status.code(),
                None,
            )?;
        }
        Ok(Err(err)) => {
            let message = format!("wait offline Pi RPC worker: {err}");
            store.finish_agent_invocation_process(
                &invocation.invocation_id,
                None,
                Some(&message),
            )?;
            return Err(err).context("wait offline Pi RPC worker");
        }
        Err(_) => {
            let _ = child.kill().await;
            let message = format!(
                "offline Pi RPC worker exceeded {} seconds",
                MAX_AGENT_RUNTIME.as_secs()
            );
            store.finish_agent_invocation_process(
                &invocation.invocation_id,
                None,
                Some(&message),
            )?;
        }
    }
    Ok(())
}

fn finish_codex_delivery(
    store: &RunStore,
    invocation: &AgentInvocationRecord,
    outcome: &str,
    error: Option<&str>,
) -> Result<()> {
    let updated = store.finish_delivery(
        "codex",
        &invocation.payload.binding.session_id,
        &invocation.owner_instance_id,
        &invocation.delivery_id,
        outcome,
        error,
    )?;
    if !updated {
        bail!(
            "Codex invocation no longer owns Delivery {}",
            invocation.delivery_id
        );
    }
    Ok(())
}

fn finish_codex_without_process(
    store: &RunStore,
    invocation: &AgentInvocationRecord,
    outcome: &str,
    message: Option<&str>,
) -> Result<()> {
    finish_codex_delivery(store, invocation, outcome, message)?;
    store.finish_agent_invocation_process(&invocation.invocation_id, None, message)?;
    Ok(())
}

async fn run_codex_invocation(
    store: Arc<RunStore>,
    invocation: AgentInvocationRecord,
) -> Result<()> {
    run_codex_invocation_with_resolver(store, invocation, resolve_codex_launcher).await
}

async fn run_codex_invocation_with_resolver<F>(
    store: Arc<RunStore>,
    invocation: AgentInvocationRecord,
    resolve_launcher: F,
) -> Result<()>
where
    F: FnOnce() -> Result<AgentLauncher>,
{
    let session_file = invocation
        .session_file
        .as_deref()
        .context("offline Codex invocation has no persisted rollout file")?;
    if !Path::new(session_file).is_file() {
        let message = format!("persisted Codex rollout is unavailable: {session_file}");
        finish_codex_delivery(&store, &invocation, "needs_rebind", Some(&message))?;
        store.finish_agent_invocation_process(&invocation.invocation_id, None, Some(&message))?;
        return Ok(());
    }
    if !Path::new(&invocation.project_root).is_dir() {
        let message = format!(
            "Codex project cwd is unavailable: {}",
            invocation.project_root
        );
        finish_codex_without_process(&store, &invocation, "needs_rebind", Some(&message))?;
        return Ok(());
    }

    let inspection = inspect_codex_rollout(Path::new(session_file), &invocation.delivery_id)?;
    match inspection.state {
        CodexRolloutState::DeliveryCompleted { turn_id } => {
            finish_codex_without_process(&store, &invocation, "delivered", None)?;
            let _ = turn_id;
            return Ok(());
        }
        CodexRolloutState::DeliveryRunning {
            turn_id,
            started_at,
        } => {
            let stale = started_at
                .and_then(|started| {
                    chrono::Utc::now()
                        .signed_duration_since(started)
                        .to_std()
                        .ok()
                })
                .is_some_and(|age| age >= MAX_AGENT_RUNTIME);
            let message = if stale {
                format!(
                    "Codex Delivery {} is already persisted in turn {turn_id}, but that turn has not completed within {} seconds; refusing duplicate injection",
                    invocation.delivery_id,
                    MAX_AGENT_RUNTIME.as_secs()
                )
            } else {
                format!(
                    "Codex Delivery {} is already persisted in active turn {turn_id}; waiting for task_complete instead of injecting it again",
                    invocation.delivery_id
                )
            };
            finish_codex_without_process(
                &store,
                &invocation,
                if stale { "needs_rebind" } else { "retry" },
                Some(&message),
            )?;
            return Ok(());
        }
        CodexRolloutState::ThreadBusy { turn_id } => {
            let message = format!(
                "Codex thread {} already has active turn {turn_id}; waiting for the exact thread to become idle before continuation",
                invocation.payload.binding.session_id
            );
            finish_codex_without_process(&store, &invocation, "retry", Some(&message))?;
            return Ok(());
        }
        CodexRolloutState::Ambiguous { reason } => {
            finish_codex_without_process(&store, &invocation, "needs_rebind", Some(&reason))?;
            return Ok(());
        }
        CodexRolloutState::Idle => {
            if inspection.malformed_lines > 0 {
                let stable = inspection
                    .quiet_for
                    .is_some_and(|quiet| quiet >= CODEX_ROLLOUT_QUIET_PERIOD);
                let message = format!(
                    "Codex rollout contains {} malformed/incomplete record(s); {}",
                    inspection.malformed_lines,
                    if stable {
                        "the file is already quiet, so automatic continuation is blocked"
                    } else {
                        "waiting for the active writer to finish"
                    }
                );
                finish_codex_without_process(
                    &store,
                    &invocation,
                    if stable { "needs_rebind" } else { "retry" },
                    Some(&message),
                )?;
                return Ok(());
            }
            if !inspection
                .quiet_for
                .is_some_and(|quiet| quiet >= CODEX_ROLLOUT_QUIET_PERIOD)
            {
                let message = format!(
                    "Codex rollout is not yet quiet for {} seconds; deferring continuation to avoid racing an active CLI writer",
                    CODEX_ROLLOUT_QUIET_PERIOD.as_secs()
                );
                finish_codex_without_process(&store, &invocation, "retry", Some(&message))?;
                return Ok(());
            }
        }
    }

    let launcher = resolve_launcher()?;
    run_codex_invocation_with_launcher(store, invocation, launcher).await
}

async fn run_codex_invocation_with_launcher(
    store: Arc<RunStore>,
    invocation: AgentInvocationRecord,
    launcher: AgentLauncher,
) -> Result<()> {
    let expected_thread_id = invocation.payload.binding.session_id.clone();
    let prompt = codex_completion_prompt(&invocation.payload)?;
    let mut command = Command::new(&launcher.executable);
    command
        .args(&launcher.prefix_args)
        .args(codex_resume_args(&expected_thread_id, &prompt))
        .current_dir(&invocation.project_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.as_std_mut().creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            let message = format!("spawn offline Codex exec-resume worker: {err}");
            store.finish_agent_invocation_process(
                &invocation.invocation_id,
                None,
                Some(&message),
            )?;
            return Err(err).context("spawn offline Codex exec-resume worker");
        }
    };
    let pid = child
        .id()
        .context("offline Codex exec-resume worker has no pid")?;
    store.set_agent_invocation_pid(&invocation.invocation_id, pid)?;
    let stdout = child
        .stdout
        .take()
        .context("offline Codex stdout unavailable")?;
    let stderr = child
        .stderr
        .take()
        .context("offline Codex stderr unavailable")?;
    let thread_for_parser = expected_thread_id.clone();
    let stdout_task =
        tokio::spawn(async move { parse_codex_exec_stream(stdout, &thread_for_parser).await });
    let stderr_task = tokio::spawn(async move { drain_stderr_tail(stderr).await });

    let wait = tokio::time::timeout(MAX_AGENT_RUNTIME, child.wait()).await;
    let (exit_code, timed_out, process_error) = match wait {
        Ok(Ok(status)) => (status.code(), false, None),
        Ok(Err(err)) => (
            None,
            false,
            Some(format!("wait offline Codex worker: {err}")),
        ),
        Err(_) => {
            let _ = child.kill().await;
            (
                None,
                true,
                Some(format!(
                    "offline Codex worker exceeded {} seconds",
                    MAX_AGENT_RUNTIME.as_secs()
                )),
            )
        }
    };
    let evidence = stdout_task
        .await
        .context("join offline Codex stdout parser")??;
    let stderr = stderr_task
        .await
        .context("join offline Codex stderr drain")??;
    let stderr = bounded_stderr_text(&stderr);

    let (outcome, message) = if let Some(actual) = evidence.thread_mismatch.as_deref() {
        (
            "needs_rebind",
            format!(
                "Codex resume thread mismatch: requested {expected_thread_id}, observed {actual}; refusing to continue a replacement thread"
            ),
        )
    } else if evidence.thread_id.as_deref() != Some(expected_thread_id.as_str()) {
        (
            "needs_rebind",
            "Codex exec-resume emitted no exact thread.started identity; refusing ambiguous continuation"
                .into(),
        )
    } else if !timed_out
        && process_error.is_none()
        && exit_code == Some(0)
        && evidence.is_success(&expected_thread_id)
    {
        ("delivered", String::new())
    } else {
        let mut details = process_error.unwrap_or_else(|| {
            format!(
                "Codex continuation did not complete successfully: exit={exit_code:?} turn_started={} turn_completed={} turn_failed={} error_event={} malformed_events={} skipped_large_events={}",
                evidence.turn_started,
                evidence.turn_completed,
                evidence.turn_failed,
                evidence.error_event,
                evidence.malformed_events,
                evidence.skipped_large_events,
            )
        });
        if !stderr.is_empty() {
            details.push_str("; stderr: ");
            details.push_str(&stderr);
        }
        ("retry", details)
    };

    finish_codex_delivery(
        &store,
        &invocation,
        outcome,
        (!message.is_empty()).then_some(message.as_str()),
    )?;
    store.finish_agent_invocation_process(
        &invocation.invocation_id,
        exit_code,
        (!message.is_empty()).then_some(message.as_str()),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offline_constants_are_bounded_for_daemon_operation() {
        assert!(OFFLINE_GRACE >= Duration::from_secs(10));
        assert!(MAX_LAUNCHES_PER_TICK <= 4);
        assert!(MAX_AGENT_RUNTIME <= Duration::from_secs(24 * 60 * 60));
        assert!(ORPHAN_RECONNECT_GRACE >= Duration::from_secs(20));
        assert!(ORPHAN_RECONNECT_GRACE < Duration::from_secs(120));
        assert!(ORPHAN_RETRY_DELAY >= Duration::from_secs(10));
    }

    #[test]
    fn rpc_stdin_stays_open_only_while_delivery_is_owned() {
        assert!(delivery_keeps_rpc_stdin_open(Some("delivering")));
        for state in [
            "delivered",
            "needs_rebind",
            "retrying",
            "pending",
            "missing",
        ] {
            assert!(!delivery_keeps_rpc_stdin_open(Some(state)), "state={state}");
        }
        assert!(!delivery_keeps_rpc_stdin_open(None));
    }

    #[cfg(windows)]
    #[test]
    fn windows_launcher_prefers_native_pi_then_volta_without_shell_shims() {
        let root = std::env::temp_dir().join(format!(
            "runwatch-pi-launcher-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let volta = root.join("volta.exe");
        std::fs::write(&volta, b"stub").unwrap();
        let path = std::env::join_paths([root.as_path()]).unwrap();

        let launcher = resolve_default_pi_launcher(Some(&path)).unwrap();
        assert_eq!(launcher.executable, volta.as_os_str());
        assert_eq!(launcher.prefix_args, ["run", "pi"]);

        let pi = root.join("pi.exe");
        std::fs::write(&pi, b"stub").unwrap();
        let launcher = resolve_default_pi_launcher(Some(&path)).unwrap();
        assert_eq!(launcher.executable, pi.as_os_str());
        assert!(launcher.prefix_args.is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(windows)]
    #[test]
    fn windows_launcher_does_not_treat_pi_cmd_as_native() {
        let root = std::env::temp_dir().join(format!(
            "runwatch-pi-cmd-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("pi.cmd"), b"@echo off\r\n").unwrap();
        let path = std::env::join_paths([root.as_path()]).unwrap();
        let err = resolve_default_pi_launcher(Some(&path)).unwrap_err();
        assert!(err.to_string().contains("no native Pi launcher"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(windows)]
    #[test]
    fn windows_codex_launcher_requires_native_executable() {
        let root = std::env::temp_dir().join(format!(
            "runwatch-codex-launcher-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("codex.cmd"), b"@echo off\r\n").unwrap();
        let path = std::env::join_paths([root.as_path()]).unwrap();
        let err = resolve_default_codex_launcher(Some(&path)).unwrap_err();
        assert!(err.to_string().contains("no native Codex launcher"));

        let codex = root.join("codex.exe");
        std::fs::write(&codex, b"stub").unwrap();
        let launcher = resolve_default_codex_launcher(Some(&path)).unwrap();
        assert_eq!(launcher.executable, codex.as_os_str());
        assert!(launcher.prefix_args.is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    fn codex_test_payload(thread_id: &str) -> DeliveryPayload {
        use chrono::Utc;
        use runwatch_core::{ContinuationBinding, RemoteWorkspaceRef, RunStatus};

        let workspace = RemoteWorkspaceRef {
            host_alias: "gm00".into(),
            cwd: "/shared/project".into(),
        };
        DeliveryPayload {
            delivery_id: "r9-codex:a1:terminal".into(),
            run_id: "r9-codex".into(),
            attempt_no: 1,
            status: RunStatus::Succeeded,
            job_id: Some("12345".into()),
            workspace: workspace.clone(),
            binding: ContinuationBinding {
                agent_kind: "codex".into(),
                session_id: thread_id.into(),
                session_file: Some("C:/Users/test/.codex/sessions/rollout.jsonl".into()),
                origin_leaf_id: None,
                project_root: "C:/science".into(),
                workspace,
                adapter_path: None,
            },
            created_at: Utc::now(),
        }
    }

    fn rollout_temp_file(label: &str) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "runwatch-codex-rollout-{label}-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        std::fs::create_dir_all(&root).unwrap();
        (root.clone(), root.join("rollout.jsonl"))
    }

    fn rollout_task_started(turn_id: &str) -> String {
        serde_json::json!({
            "type": "event_msg",
            "payload": {
                "type": "task_started",
                "turn_id": turn_id,
                "started_at": "2026-09-01T02:00:00Z"
            }
        })
        .to_string()
    }

    fn rollout_user_marker(delivery_id: &str) -> String {
        serde_json::json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{
                    "type": "input_text",
                    "text": format!("{} continue the scientific task", codex_delivery_marker(delivery_id))
                }]
            }
        })
        .to_string()
    }

    fn rollout_task_complete(turn_id: &str) -> String {
        serde_json::json!({
            "type": "event_msg",
            "payload": {
                "type": "task_complete",
                "turn_id": turn_id,
                "last_agent_message": "omitted from runwatch inspection"
            }
        })
        .to_string()
    }

    fn write_rollout(path: &Path, lines: &[String]) {
        let mut body = lines.join("\n");
        body.push('\n');
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn codex_rollout_state_machine_tracks_busy_running_and_completed_delivery() {
        let (root, rollout) = rollout_temp_file("state-machine");
        let delivery = "r9-rollout:a1:terminal";
        let turn = "turn-r9";

        write_rollout(&rollout, &[rollout_task_started(turn)]);
        let inspection = inspect_codex_rollout(&rollout, delivery).unwrap();
        assert_eq!(
            inspection.state,
            CodexRolloutState::ThreadBusy {
                turn_id: turn.into()
            }
        );

        write_rollout(
            &rollout,
            &[rollout_task_started(turn), rollout_user_marker(delivery)],
        );
        let inspection = inspect_codex_rollout(&rollout, delivery).unwrap();
        assert!(matches!(
            inspection.state,
            CodexRolloutState::DeliveryRunning { ref turn_id, .. } if turn_id == turn
        ));

        write_rollout(
            &rollout,
            &[
                rollout_task_started(turn),
                rollout_user_marker(delivery),
                rollout_task_complete(turn),
            ],
        );
        let inspection = inspect_codex_rollout(&rollout, delivery).unwrap();
        assert_eq!(
            inspection.state,
            CodexRolloutState::DeliveryCompleted {
                turn_id: turn.into()
            }
        );
        assert_eq!(inspection.malformed_lines, 0);
        assert_eq!(inspection.skipped_large_lines, 0);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn codex_rollout_idle_after_unrelated_completed_turn() {
        let (root, rollout) = rollout_temp_file("idle");
        let turn = "unrelated-turn";
        write_rollout(
            &rollout,
            &[rollout_task_started(turn), rollout_task_complete(turn)],
        );
        let inspection = inspect_codex_rollout(&rollout, "different-delivery").unwrap();
        assert_eq!(inspection.state, CodexRolloutState::Idle);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn codex_rollout_duplicate_or_unscoped_marker_is_ambiguous() {
        let (root, rollout) = rollout_temp_file("ambiguous");
        let delivery = "dup:a1:terminal";
        write_rollout(&rollout, &[rollout_user_marker(delivery)]);
        let inspection = inspect_codex_rollout(&rollout, delivery).unwrap();
        assert!(matches!(
            inspection.state,
            CodexRolloutState::Ambiguous { ref reason } if reason.contains("outside an active turn")
        ));

        write_rollout(
            &rollout,
            &[
                rollout_task_started("turn-1"),
                rollout_user_marker(delivery),
                rollout_task_complete("turn-1"),
                rollout_task_started("turn-2"),
                rollout_user_marker(delivery),
            ],
        );
        let inspection = inspect_codex_rollout(&rollout, delivery).unwrap();
        assert!(matches!(
            inspection.state,
            CodexRolloutState::Ambiguous { ref reason } if reason.contains("2 user messages")
        ));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn codex_rollout_skips_oversized_unrelated_record_and_keeps_delivery_evidence() {
        let (root, rollout) = rollout_temp_file("oversized");
        let delivery = "large:a1:terminal";
        let turn = "turn-large";
        let mut file = std::fs::File::create(&rollout).unwrap();
        use std::io::Write;
        writeln!(file, "{}", rollout_task_started(turn)).unwrap();
        file.write_all(&vec![b'x'; MAX_CODEX_ROLLOUT_LINE_BYTES + 1])
            .unwrap();
        file.write_all(b"\n").unwrap();
        writeln!(file, "{}", rollout_user_marker(delivery)).unwrap();
        writeln!(file, "{}", rollout_task_complete(turn)).unwrap();
        drop(file);

        let inspection = inspect_codex_rollout(&rollout, delivery).unwrap();
        assert_eq!(
            inspection.state,
            CodexRolloutState::DeliveryCompleted {
                turn_id: turn.into()
            }
        );
        assert_eq!(inspection.skipped_large_lines, 1);
        assert_eq!(inspection.malformed_lines, 0);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn codex_prompt_and_resume_argv_are_bounded_exact_and_do_not_bypass_trust() {
        let thread = "019c1234-5678-7000-8000-000000000009";
        let prompt = codex_completion_prompt(&codex_test_payload(thread)).unwrap();
        assert!(prompt.contains("delivery_id=r9-codex:a1:terminal"));
        assert!(prompt.contains("Do not resubmit the completed attempt"));
        assert!(prompt.len() <= MAX_CODEX_PROMPT_BYTES);

        let args = codex_resume_args(thread, &prompt);
        let rendered = args
            .iter()
            .map(|arg| arg.to_string_lossy())
            .collect::<Vec<_>>();
        assert_eq!(&rendered[..4], ["exec", "resume", "--json", thread]);
        assert_eq!(rendered[4], prompt);
        assert!(!rendered.iter().any(|arg| arg.contains("dangerously")));
        assert!(!rendered.iter().any(|arg| arg == "--approve"));
    }

    async fn parse_codex_bytes(bytes: Vec<u8>, expected: &str) -> CodexExecEvidence {
        let capacity = bytes.len().max(1024);
        let (mut writer, reader) = tokio::io::duplex(capacity);
        let write = tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            writer.write_all(&bytes).await.unwrap();
        });
        let evidence = parse_codex_exec_stream(reader, expected).await.unwrap();
        write.await.unwrap();
        evidence
    }

    #[tokio::test]
    async fn codex_jsonl_requires_exact_thread_and_completed_turn() {
        let thread = "019c1234-5678-7000-8000-000000000010";
        let bytes = format!(
            "{{\"type\":\"thread.started\",\"thread_id\":\"{thread}\"}}\n\
             {{\"type\":\"turn.started\"}}\n\
             {{\"type\":\"item.completed\",\"item\":{{\"type\":\"agent_message\"}}}}\n\
             {{\"type\":\"turn.completed\"}}\n"
        )
        .into_bytes();
        let evidence = parse_codex_bytes(bytes, thread).await;
        assert_eq!(evidence.thread_id.as_deref(), Some(thread));
        assert!(evidence.turn_started);
        assert!(evidence.turn_completed);
        assert!(evidence.is_success(thread));
    }

    #[tokio::test]
    async fn codex_jsonl_rejects_replacement_thread_and_failure_events() {
        let expected = "019c1234-5678-7000-8000-000000000011";
        let actual = "019c1234-5678-7000-8000-000000000012";
        let mismatch = format!(
            "{{\"type\":\"thread.started\",\"thread_id\":\"{actual}\"}}\n\
             {{\"type\":\"turn.started\"}}\n{{\"type\":\"turn.completed\"}}\n"
        )
        .into_bytes();
        let evidence = parse_codex_bytes(mismatch, expected).await;
        assert_eq!(evidence.thread_mismatch.as_deref(), Some(actual));
        assert!(!evidence.is_success(expected));

        let failed = format!(
            "{{\"type\":\"thread.started\",\"thread_id\":\"{expected}\"}}\n\
             {{\"type\":\"turn.started\"}}\n{{\"type\":\"turn.failed\"}}\n"
        )
        .into_bytes();
        let evidence = parse_codex_bytes(failed, expected).await;
        assert!(evidence.turn_failed);
        assert!(!evidence.is_success(expected));
    }

    #[tokio::test]
    async fn codex_jsonl_skips_oversized_item_without_losing_later_terminal_events() {
        let thread = "019c1234-5678-7000-8000-000000000013";
        let mut bytes = format!(
            "{{\"type\":\"thread.started\",\"thread_id\":\"{thread}\"}}\n\
             {{\"type\":\"turn.started\"}}\n"
        )
        .into_bytes();
        bytes.extend(std::iter::repeat_n(b'x', MAX_CODEX_EVENT_LINE_BYTES + 1));
        bytes.push(b'\n');
        bytes.extend_from_slice(b"{\"type\":\"turn.completed\"}\n");
        let evidence = parse_codex_bytes(bytes, thread).await;
        assert_eq!(evidence.skipped_large_events, 1);
        assert_eq!(evidence.malformed_events, 0);
        assert!(evidence.turn_completed);
        assert!(evidence.is_success(thread));
    }

    fn seed_codex_test_invocation(
        root: &Path,
        thread: &str,
        rollout: &Path,
        run_id: &str,
    ) -> (Arc<RunStore>, String, AgentInvocationRecord) {
        use chrono::Utc;
        use runwatch_core::{
            ContinuationBinding, RemoteWorkspaceRef, RunAttemptRecord, RunRecord, RunResources,
            RunStatus, RunnerKind,
        };

        let store = Arc::new(RunStore::open_default().unwrap());
        let workspace = RemoteWorkspaceRef {
            host_alias: "stub-host".into(),
            cwd: "/stub/workspace".into(),
        };
        let binding = ContinuationBinding {
            agent_kind: "codex".into(),
            session_id: thread.into(),
            session_file: Some(rollout.to_string_lossy().into_owned()),
            origin_leaf_id: None,
            project_root: root.to_string_lossy().into_owned(),
            workspace: workspace.clone(),
            adapter_path: None,
        };
        let now = Utc::now();
        let mut run = RunRecord::new(
            run_id.into(),
            workspace.host_alias.clone(),
            RunnerKind::Slurm,
        );
        run.status = RunStatus::Submitting;
        run.workspace = Some(workspace.clone());
        run.attempt_no = Some(1);
        run.session_id = Some(thread.into());
        run.agent = Some("codex".into());
        run.project_root = Some(binding.project_root.clone());
        run.updated_at = now;
        let attempt = RunAttemptRecord {
            run_id: run_id.into(),
            attempt_no: 1,
            runner: RunnerKind::Slurm,
            host: workspace.host_alias.clone(),
            workdir: workspace.cwd.clone(),
            command: "true".into(),
            resources: RunResources::default(),
            job_name: format!("rw-{run_id}-a1"),
            job_id: Some("stub-job-1".into()),
            script_path: "/stub/attempt-1.sh".into(),
            stdout_path: "/stub/stdout.log".into(),
            stderr_path: "/stub/stderr.log".into(),
            terminal_path: "/stub/terminal.json".into(),
            receipt_path: "/stub/submission.receipt".into(),
            status: RunStatus::Queued,
            created_at: now,
            updated_at: now,
            error: None,
        };
        assert!(
            store
                .create_submission_intent(&run, &attempt, Some(&binding))
                .unwrap()
        );
        run.status = RunStatus::Succeeded;
        run.job_id = Some("stub-job-1".into());
        run.updated_at = Utc::now();
        store.upsert(&run).unwrap();
        let delivery_id = store.ensure_terminal_delivery(&run).unwrap().unwrap();
        let invocation = store
            .reserve_offline_invocation(Duration::ZERO)
            .unwrap()
            .expect("Codex test invocation should reserve");
        (store, delivery_id, invocation)
    }

    #[tokio::test]
    #[ignore = "uses isolated RUNWATCH_DATA_DIR to verify rollout-completed crash recovery without launching Codex"]
    async fn codex_completed_rollout_backfills_delivery_without_relaunch() {
        let root = PathBuf::from(
            std::env::var_os("RUNWATCH_DATA_DIR")
                .expect("RUNWATCH_DATA_DIR must point at an isolated directory for this gate"),
        );
        std::fs::create_dir_all(&root).unwrap();
        let rollout = root.join("rollout-recovery.jsonl");
        std::fs::write(&rollout, b"{}\n").unwrap();
        let thread = "019c1234-5678-7000-8000-000000000015";
        let (store, delivery_id, invocation) =
            seed_codex_test_invocation(&root, thread, &rollout, "r9-codex-recovery");
        let turn = "turn-r9-recovery";
        write_rollout(
            &rollout,
            &[
                rollout_task_started(turn),
                rollout_user_marker(&delivery_id),
                rollout_task_complete(turn),
            ],
        );

        run_codex_invocation_with_resolver(store.clone(), invocation, || {
            Err(anyhow::anyhow!(
                "launcher resolution must not run after completed rollout evidence"
            ))
        })
        .await
        .expect("completed rollout should backfill Delivery without relaunch");
        assert_eq!(
            store.get_delivery_state(&delivery_id).unwrap().as_deref(),
            Some("delivered")
        );
        assert!(
            store
                .reserve_offline_invocation(Duration::ZERO)
                .unwrap()
                .is_none()
        );
        drop(store);
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(windows)]
    #[tokio::test]
    #[ignore = "spawns a native PowerShell Codex stub and exercises the durable offline Delivery path"]
    async fn real_codex_driver_native_stub_acceptance() {
        use chrono::Utc;
        use runwatch_core::{
            ContinuationBinding, RemoteWorkspaceRef, RunAttemptRecord, RunRecord, RunResources,
            RunStatus, RunnerKind,
        };

        let pwsh = find_in_path(std::env::var_os("PATH").as_deref(), "pwsh.exe")
            .expect("pwsh.exe is required for the Windows native-stub acceptance");
        let root = PathBuf::from(
            std::env::var_os("RUNWATCH_DATA_DIR")
                .expect("RUNWATCH_DATA_DIR must point at an isolated directory for this gate"),
        );
        std::fs::create_dir_all(&root).unwrap();
        let script = root.join("codex-stub.ps1");
        std::fs::write(
            &script,
            r#"param(
  [Parameter(Position=0)][string]$Verb1,
  [Parameter(Position=1)][string]$Verb2,
  [switch]$json,
  [Parameter(Position=2)][string]$Thread,
  [Parameter(Position=3)][string]$Prompt
)
# PowerShell -File treats --json as a named parameter. Model that one flag explicitly so the
# native stub receives the same remaining thread/prompt values that real codex.exe receives.
if ($Verb1 -ne 'exec' -or $Verb2 -ne 'resume' -or -not $json.IsPresent) { exit 9 }
if ($Prompt -notmatch 'runwatch continuation delivery_id=') { exit 10 }
[Console]::Out.WriteLine('{"type":"thread.started","thread_id":"' + $Thread + '"}')
[Console]::Out.WriteLine('{"type":"turn.started"}')
[Console]::Out.WriteLine('{"type":"item.completed","item":{"type":"agent_message","text":"stub"}}')
[Console]::Out.WriteLine('{"type":"turn.completed"}')
exit 0
"#,
        )
        .unwrap();

        let thread = "019c1234-5678-7000-8000-000000000014";
        let rollout = root.join("rollout.jsonl");
        std::fs::write(&rollout, b"{}\n").unwrap();
        let store = Arc::new(RunStore::open_default().unwrap());
        let workspace = RemoteWorkspaceRef {
            host_alias: "stub-host".into(),
            cwd: "/stub/workspace".into(),
        };
        let binding = ContinuationBinding {
            agent_kind: "codex".into(),
            session_id: thread.into(),
            session_file: Some(rollout.to_string_lossy().into_owned()),
            origin_leaf_id: None,
            project_root: root.to_string_lossy().into_owned(),
            workspace: workspace.clone(),
            adapter_path: None,
        };
        let now = Utc::now();
        let run_id = "r9-codex-driver-stub";
        let mut run = RunRecord::new(
            run_id.into(),
            workspace.host_alias.clone(),
            RunnerKind::Slurm,
        );
        run.status = RunStatus::Submitting;
        run.workspace = Some(workspace.clone());
        run.attempt_no = Some(1);
        run.session_id = Some(thread.into());
        run.agent = Some("codex".into());
        run.project_root = Some(binding.project_root.clone());
        run.updated_at = now;
        let attempt = RunAttemptRecord {
            run_id: run_id.into(),
            attempt_no: 1,
            runner: RunnerKind::Slurm,
            host: workspace.host_alias.clone(),
            workdir: workspace.cwd.clone(),
            command: "true".into(),
            resources: RunResources::default(),
            job_name: "rw-r9-codex-driver-stub-a1".into(),
            job_id: Some("stub-job-1".into()),
            script_path: "/stub/attempt-1.sh".into(),
            stdout_path: "/stub/stdout.log".into(),
            stderr_path: "/stub/stderr.log".into(),
            terminal_path: "/stub/terminal.json".into(),
            receipt_path: "/stub/submission.receipt".into(),
            status: RunStatus::Queued,
            created_at: now,
            updated_at: now,
            error: None,
        };
        assert!(
            store
                .create_submission_intent(&run, &attempt, Some(&binding))
                .unwrap()
        );
        run.status = RunStatus::Succeeded;
        run.job_id = Some("stub-job-1".into());
        run.updated_at = Utc::now();
        store.upsert(&run).unwrap();
        let delivery_id = store.ensure_terminal_delivery(&run).unwrap().unwrap();
        let invocation = store
            .reserve_offline_invocation(Duration::ZERO)
            .unwrap()
            .expect("Codex stub invocation should reserve");
        let launcher = AgentLauncher {
            executable: pwsh.into_os_string(),
            prefix_args: vec![
                OsString::from("-NoLogo"),
                OsString::from("-NoProfile"),
                OsString::from("-NonInteractive"),
                OsString::from("-File"),
                script.as_os_str().to_os_string(),
            ],
        };

        run_codex_invocation_with_launcher(store.clone(), invocation, launcher)
            .await
            .expect("native Codex stub invocation");
        assert_eq!(
            store.get_delivery_state(&delivery_id).unwrap().as_deref(),
            Some("delivered")
        );
        assert!(
            store
                .reserve_offline_invocation(Duration::ZERO)
                .unwrap()
                .is_none()
        );
        drop(store);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    #[ignore = "requires installed/configured Pi plus explicit RUNWATCH_R5_* acceptance environment"]
    async fn real_pi_offline_continuation_acceptance() {
        use chrono::Utc;
        use runwatch_core::{
            ContinuationBinding, RemoteWorkspaceRef, RunAttemptRecord, RunRecord, RunResources,
            RunStatus, RunnerKind,
        };
        use runwatch_ssh::HostPool;

        let session_id = std::env::var("RUNWATCH_R5_SESSION_ID")
            .expect("RUNWATCH_R5_SESSION_ID is required for ignored R5 acceptance");
        let session_file = std::env::var("RUNWATCH_R5_SESSION_FILE")
            .expect("RUNWATCH_R5_SESSION_FILE is required for ignored R5 acceptance");
        let origin_leaf_id = std::env::var("RUNWATCH_R5_ORIGIN_LEAF")
            .expect("RUNWATCH_R5_ORIGIN_LEAF is required for ignored R5 acceptance");
        let adapter_path = std::env::var("RUNWATCH_R5_ADAPTER_PATH")
            .expect("RUNWATCH_R5_ADAPTER_PATH is required for ignored R5 acceptance");
        let project_root = std::env::var("RUNWATCH_R5_PROJECT_ROOT")
            .expect("RUNWATCH_R5_PROJECT_ROOT is required for ignored R5 acceptance");
        assert!(std::env::var_os("RUNWATCH_DATA_DIR").is_some());
        assert!(std::env::var_os("RUNWATCH_ENDPOINT").is_some());

        let store = Arc::new(RunStore::open_default().expect("open isolated acceptance store"));
        let pool = Arc::new(
            HostPool::from_ssh_config_with_timeout(
                Duration::from_secs(30),
                Duration::from_secs(20),
            )
            .expect("load SSH config for isolated IPC server"),
        );
        let ipc_server = crate::ipc::spawn_local_server(
            store.clone(),
            pool,
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        );
        tokio::time::sleep(Duration::from_millis(150)).await;

        let now = Utc::now();
        let workspace = RemoteWorkspaceRef {
            host_alias: "r5-acceptance".into(),
            cwd: "/tmp/runwatch-r5-acceptance".into(),
        };
        let binding = ContinuationBinding {
            agent_kind: "pi".into(),
            session_id: session_id.clone(),
            session_file: Some(session_file.clone()),
            origin_leaf_id: Some(origin_leaf_id),
            project_root: project_root.clone(),
            workspace: workspace.clone(),
            adapter_path: Some(adapter_path),
        };
        let run_id = format!(
            "r5-acceptance-{}",
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        );
        let mut run = RunRecord::new(
            run_id.clone(),
            workspace.host_alias.clone(),
            RunnerKind::Slurm,
        );
        run.status = RunStatus::Submitting;
        run.workspace = Some(workspace.clone());
        run.attempt_no = Some(1);
        run.session_id = Some(session_id);
        run.agent = Some("pi".into());
        run.project_root = Some(project_root);
        run.updated_at = now;
        let attempt = RunAttemptRecord {
            run_id: run_id.clone(),
            attempt_no: 1,
            runner: RunnerKind::Slurm,
            host: workspace.host_alias.clone(),
            workdir: workspace.cwd.clone(),
            command: "true".into(),
            resources: RunResources::default(),
            job_name: format!("rw-{run_id}-a1"),
            job_id: Some("acceptance-1".into()),
            script_path: format!("{}/.runwatch/{run_id}/attempt-1.sh", workspace.cwd),
            stdout_path: format!("{}/.runwatch/{run_id}/stdout.log", workspace.cwd),
            stderr_path: format!("{}/.runwatch/{run_id}/stderr.log", workspace.cwd),
            terminal_path: format!("{}/.runwatch/{run_id}/terminal.json", workspace.cwd),
            receipt_path: format!("{}/.runwatch/{run_id}/submission.receipt", workspace.cwd),
            status: RunStatus::Queued,
            created_at: now,
            updated_at: now,
            error: None,
        };
        assert!(
            store
                .create_submission_intent(&run, &attempt, Some(&binding))
                .expect("create acceptance submission intent")
        );
        run.status = RunStatus::Succeeded;
        run.job_id = Some("acceptance-1".into());
        run.updated_at = Utc::now();
        store.upsert(&run).expect("persist synthetic terminal Run");
        let delivery_id = store
            .ensure_terminal_delivery(&run)
            .expect("ensure terminal Delivery")
            .expect("terminal Delivery id");
        let invocation = store
            .reserve_offline_invocation(Duration::ZERO)
            .expect("reserve offline invocation")
            .expect("offline invocation should be immediately reservable");

        run_invocation(store.clone(), invocation)
            .await
            .expect("real Pi offline invocation");
        let state = store
            .get_delivery_state(&delivery_id)
            .expect("read acceptance Delivery state");
        assert_eq!(state.as_deref(), Some("delivered"));
        ipc_server.abort();
    }
}
