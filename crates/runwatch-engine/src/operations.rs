use anyhow::{Context, Result, bail};
use chrono::Utc;
use runwatch_core::{RunRecord, RunStatus, RunStore, RunnerKind};
use runwatch_ssh::HostPool;
use serde::Serialize;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

const DEFAULT_LOG_TAIL_LINES: usize = 80;
const MAX_LOG_TAIL_LINES: usize = 500;
const MAX_LOG_BYTES_PER_STREAM: usize = 64 * 1024;

#[derive(Debug, Clone, Serialize)]
pub struct RunLogs {
    pub run_id: String,
    pub status: RunStatus,
    pub attempt_no: u32,
    pub stdout: String,
    pub stderr: String,
    pub tail_lines: usize,
    pub byte_limit_per_stream: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunArtifact {
    pub kind: &'static str,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunArtifacts {
    pub run_id: String,
    pub status: RunStatus,
    pub attempt_no: u32,
    pub artifacts: Vec<RunArtifact>,
}

fn attempt_artifacts(attempt: &runwatch_core::RunAttemptRecord) -> Vec<RunArtifact> {
    [
        ("script", &attempt.script_path),
        ("stdout", &attempt.stdout_path),
        ("stderr", &attempt.stderr_path),
        ("terminal", &attempt.terminal_path),
        ("receipt", &attempt.receipt_path),
    ]
    .into_iter()
    .map(|(kind, path)| RunArtifact {
        kind,
        path: path.clone(),
    })
    .collect()
}

pub fn artifacts_run(
    store: &RunStore,
    run_id: &str,
    requested_attempt_no: Option<u32>,
) -> Result<RunArtifacts> {
    let run = store
        .get(run_id)?
        .with_context(|| format!("unknown run {run_id}"))?;
    let attempt_no = requested_attempt_no
        .or(run.attempt_no)
        .context("Run has no durable attempt metadata yet")?;
    let attempt = store
        .get_attempt(run_id, attempt_no)?
        .with_context(|| format!("Run attempt {attempt_no} metadata is missing"))?;
    Ok(RunArtifacts {
        run_id: run.run_id,
        status: attempt.status,
        attempt_no,
        artifacts: attempt_artifacts(&attempt),
    })
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn bounded_tail_lines(tail: Option<usize>) -> usize {
    tail.unwrap_or(DEFAULT_LOG_TAIL_LINES)
        .clamp(1, MAX_LOG_TAIL_LINES)
}

fn tail_command(path: &str, tail_lines: usize) -> String {
    let path = shell_quote(path);
    format!(
        "if [ -f {path} ]; then tail -n {tail_lines} -- {path} 2>/dev/null | tail -c {MAX_LOG_BYTES_PER_STREAM}; fi"
    )
}

async fn read_remote_tail(
    pool: &HostPool,
    host: &str,
    path: &str,
    tail_lines: usize,
) -> Result<String> {
    let out = pool.exec(host, &tail_command(path, tail_lines)).await?;
    if out.code.unwrap_or(1) != 0 {
        bail!(
            "remote log tail failed with exit {:?}: {}{}",
            out.code,
            out.stdout,
            out.stderr
        );
    }
    Ok(out.stdout)
}

fn read_local_tail(path: &str, tail_lines: usize) -> Result<String> {
    let path = Path::new(path);
    if !path.exists() {
        return Ok(String::new());
    }
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(MAX_LOG_BYTES_PER_STREAM as u64);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    if start > 0
        && let Some(newline) = bytes.iter().position(|byte| *byte == b'\n')
    {
        bytes.drain(..=newline);
    }
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<_> = text.lines().collect();
    let from = lines.len().saturating_sub(tail_lines);
    let mut result = lines[from..].join("\n");
    if !result.is_empty() {
        result.push('\n');
    }
    Ok(result)
}

pub async fn logs_run(
    store: &RunStore,
    pool: &HostPool,
    run_id: &str,
    requested_attempt_no: Option<u32>,
    tail: Option<usize>,
) -> Result<RunLogs> {
    let run = store
        .get(run_id)?
        .with_context(|| format!("unknown run {run_id}"))?;
    let attempt_no = requested_attempt_no
        .or(run.attempt_no)
        .context("Run has no durable attempt metadata yet")?;
    let attempt = store
        .get_attempt(run_id, attempt_no)?
        .with_context(|| format!("Run attempt {attempt_no} metadata is missing"))?;
    let tail_lines = bounded_tail_lines(tail);
    let (stdout, stderr) = if attempt.runner == RunnerKind::Process {
        (
            read_local_tail(&attempt.stdout_path, tail_lines)?,
            read_local_tail(&attempt.stderr_path, tail_lines)?,
        )
    } else {
        (
            read_remote_tail(pool, &attempt.host, &attempt.stdout_path, tail_lines).await?,
            read_remote_tail(pool, &attempt.host, &attempt.stderr_path, tail_lines).await?,
        )
    };
    Ok(RunLogs {
        run_id: run.run_id,
        status: attempt.status,
        attempt_no,
        stdout,
        stderr,
        tail_lines,
        byte_limit_per_stream: MAX_LOG_BYTES_PER_STREAM,
    })
}

fn cancel_command(runner: RunnerKind, job_id: &str) -> Result<String> {
    let job = shell_quote(job_id);
    match runner {
        RunnerKind::Slurm => Ok(format!("scancel {job}")),
        RunnerKind::Lsf => Ok(format!("bkill {job}")),
        other => bail!("cancel is not implemented for runner {other:?}"),
    }
}

pub async fn cancel_run(store: &RunStore, pool: &HostPool, run_id: &str) -> Result<RunRecord> {
    let mut run = store
        .get(run_id)?
        .with_context(|| format!("unknown run {run_id}"))?;
    if run.status.is_terminal() {
        return Ok(run);
    }
    if run.runner == RunnerKind::Process {
        crate::local_process::request_cancel(store, &mut run)?;
        return Ok(run);
    }
    let job_id = run
        .job_id
        .as_deref()
        .context("Run has no scheduler job id to cancel")?;
    let command = cancel_command(run.runner, job_id)?;
    let out = pool.exec(&run.host, &command).await?;
    if out.code.unwrap_or(1) != 0 {
        bail!(
            "scheduler cancel failed with exit {:?}: {}{}",
            out.code,
            out.stdout,
            out.stderr
        );
    }

    run.updated_at = Utc::now();
    run.note = Some(format!("cancel requested for scheduler job {job_id}"));
    if let Some(attempt_no) = run.attempt_no {
        if let Some(attempt) = store.get_attempt(run_id, attempt_no)? {
            store.persist_run_attempt_event(&run, &attempt, "cancel_requested")?;
            return Ok(run);
        }
    }
    store.upsert(&run)?;
    Ok(run)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn log_tail_is_bounded_and_shell_quoted() {
        let command = tail_command("/shared/a b/x'; touch nope; '.log", 9999);
        assert!(command.contains("tail -c 65536"));
        assert!(command.contains("'\"'\"'"));
        assert_eq!(bounded_tail_lines(Some(9999)), MAX_LOG_TAIL_LINES);
        assert_eq!(bounded_tail_lines(Some(0)), 1);
    }

    #[test]
    fn local_log_tail_is_line_and_byte_bounded() {
        let path =
            std::env::temp_dir().join(format!("runwatch-local-tail-{}.log", std::process::id()));
        fs::write(&path, "a\nb\nc\nd\n").unwrap();
        assert_eq!(
            read_local_tail(&path.to_string_lossy(), 2).unwrap(),
            "c\nd\n"
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn lifecycle_artifact_inventory_is_attempt_scoped() {
        let now = Utc::now();
        let attempt = runwatch_core::RunAttemptRecord {
            run_id: "r1".into(),
            attempt_no: 2,
            runner: RunnerKind::Slurm,
            host: "gm00".into(),
            workdir: "/work".into(),
            command: "true".into(),
            resources: Default::default(),
            job_name: "rw-r1-a2".into(),
            job_id: Some("123".into()),
            script_path: "/work/run.sh".into(),
            stdout_path: "/work/stdout.log".into(),
            stderr_path: "/work/stderr.log".into(),
            terminal_path: "/work/terminal.json".into(),
            receipt_path: "/work/job.id".into(),
            status: RunStatus::Succeeded,
            created_at: now,
            updated_at: now,
            error: None,
        };
        let items = attempt_artifacts(&attempt);
        assert_eq!(items.len(), 5);
        assert_eq!(items[0].kind, "script");
        assert_eq!(items[3].path, "/work/terminal.json");
        assert_eq!(items[4].kind, "receipt");
    }

    #[test]
    fn scheduler_cancel_commands_quote_handles() {
        let slurm = cancel_command(RunnerKind::Slurm, "123'; echo nope; '").unwrap();
        let lsf = cancel_command(RunnerKind::Lsf, "456").unwrap();
        assert!(slurm.starts_with("scancel '"));
        assert!(slurm.contains("'\"'\"'"));
        assert_eq!(lsf, "bkill '456'");
        assert!(cancel_command(RunnerKind::Powershell, "1").is_err());
        assert!(cancel_command(RunnerKind::Process, "local:1:1").is_err());
    }
}
