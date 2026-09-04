use crate::model::{RunDetailView, build_detail};
use anyhow::{Context, Result, anyhow};
use runwatch_core::{
    ObservationRecord, RunAttemptRecord, RunContinuationStatus, RunEventRecord, RunRecord,
    SubmitRunSpec,
};
use serde_json::{Value, json};

pub struct SnapshotPayload {
    pub version: String,
    pub protocol_version: u64,
    pub capability_count: usize,
    pub pid: u64,
    pub paused: bool,
    pub manual_submit_supported: bool,
    pub runs: Vec<RunRecord>,
    pub observations: Vec<ObservationRecord>,
    pub continuations: Vec<RunContinuationStatus>,
}

pub async fn snapshot() -> Result<SnapshotPayload> {
    let (hello, state, rows) = tokio::try_join!(
        runwatch_engine::ipc::call_local("hello", json!({})),
        runwatch_engine::ipc::call_local("daemon_status", json!({})),
        runwatch_engine::ipc::call_local("list_runs", json!({})),
    )?;
    let capabilities = hello.get("capabilities").and_then(Value::as_array);
    let capability_count = capabilities.map(Vec::len).unwrap_or_default();
    let manual_submit_supported = capabilities.is_some_and(|values| {
        values
            .iter()
            .any(|value| value.as_str() == Some("submit_run_v2"))
    });
    Ok(SnapshotPayload {
        version: hello
            .get("version")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string(),
        protocol_version: hello
            .get("protocol_version")
            .and_then(Value::as_u64)
            .unwrap_or_default(),
        capability_count,
        pid: state.get("pid").and_then(Value::as_u64).unwrap_or_default(),
        paused: state
            .get("paused")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        manual_submit_supported,
        runs: parse_field(&rows, "runs")?,
        observations: parse_field(&rows, "observations")?,
        continuations: rows
            .get("continuations")
            .cloned()
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default(),
    })
}

pub async fn set_paused(paused: bool) -> Result<()> {
    runwatch_engine::ipc::call_local("set_paused", json!({ "paused": paused })).await?;
    Ok(())
}

pub async fn submit_manual(spec: SubmitRunSpec) -> Result<RunRecord> {
    let value = runwatch_engine::ipc::call_local("submit_run_v2", json!({ "spec": spec })).await?;
    parse_field(&value, "run")
}

pub async fn cancel_run(run_id: &str) -> Result<()> {
    runwatch_engine::ipc::call_local("cancel_run", json!({ "run_id": run_id })).await?;
    Ok(())
}

pub async fn probe_run(run_id: &str) -> Result<Vec<String>> {
    let value = runwatch_engine::ipc::call_local("probe_run", json!({ "run_id": run_id })).await?;
    Ok(value
        .get("errors")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default())
}

pub async fn load_detail(run_id: &str, tail: usize, event_limit: usize) -> Result<RunDetailView> {
    let run_value =
        runwatch_engine::ipc::call_local("get_run", json!({ "run_id": run_id })).await?;
    let run: RunRecord = run_value
        .get("run")
        .cloned()
        .filter(|value| !value.is_null())
        .ok_or_else(|| anyhow!("unknown run {run_id}"))
        .and_then(|value| serde_json::from_value(value).context("parse Run detail"))?;
    let observation: Option<ObservationRecord> = run_value
        .get("observation")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .context("parse Run observation")?
        .flatten();

    let (attempt_result, continuation_result, events_result, logs_result, artifacts_result) = tokio::join!(
        runwatch_engine::ipc::call_local("get_attempt", json!({ "run_id": run_id })),
        runwatch_engine::ipc::call_local("get_continuation_status", json!({ "run_id": run_id })),
        runwatch_engine::ipc::call_local(
            "list_run_events",
            json!({ "run_id": run_id, "limit": event_limit.clamp(1, 200) })
        ),
        runwatch_engine::ipc::call_local(
            "logs",
            json!({ "run_id": run_id, "tail": tail.clamp(1, 500) })
        ),
        runwatch_engine::ipc::call_local("artifacts", json!({ "run_id": run_id })),
    );

    let (attempt, attempt_error) = match attempt_result {
        Ok(value) => match parse_optional_field::<RunAttemptRecord>(&value, "attempt") {
            Ok(attempt) => (attempt, None),
            Err(error) => (None, Some(format!("{error:#}"))),
        },
        Err(error) => (None, Some(format!("{error:#}"))),
    };
    let (continuation, continuation_error) = match continuation_result {
        Ok(value) => match parse_field::<RunContinuationStatus>(&value, "continuation") {
            Ok(status) => (status, None),
            Err(error) => (RunContinuationStatus::default(), Some(format!("{error:#}"))),
        },
        Err(error) => (RunContinuationStatus::default(), Some(format!("{error:#}"))),
    };
    let (events, events_error) = match events_result {
        Ok(value) => match parse_field::<Vec<RunEventRecord>>(&value, "events") {
            Ok(events) => (events, None),
            Err(error) => (Vec::new(), Some(format!("{error:#}"))),
        },
        Err(error) => (Vec::new(), Some(format!("{error:#}"))),
    };
    let logs = match logs_result {
        Ok(value) => format_logs(&value),
        Err(error) => format!("Logs unavailable\n{error:#}"),
    };
    let artifacts = match artifacts_result {
        Ok(value) => format_artifacts(&value),
        Err(error) => format!("Artifacts unavailable\n{error:#}"),
    };
    Ok(build_detail(
        run,
        observation,
        attempt,
        attempt_error,
        continuation,
        continuation_error,
        events,
        events_error,
        logs,
        artifacts,
    ))
}

pub fn host_summary() -> Result<String> {
    let hosts = runwatch_ssh::parse_ssh_config()?;
    if hosts.is_empty() {
        return Ok("No exact Host entries in ~/.ssh/config".into());
    }
    Ok(hosts
        .into_iter()
        .map(|host| {
            let jumps = if host.proxy_jump.is_empty() {
                String::new()
            } else {
                format!(" via {}", host.proxy_jump.join(", "))
            };
            format!(
                "{}   {}@{}:{}{}",
                host.alias,
                host.user.as_deref().unwrap_or("?"),
                host.hostname,
                host.port,
                jumps
            )
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

pub fn sibling_cli() -> Result<std::path::PathBuf> {
    let exe = std::env::current_exe()?;
    let dir = exe
        .parent()
        .ok_or_else(|| anyhow!("GUI executable has no parent directory"))?;
    let cli = dir.join("runwatch.exe");
    if cli.is_file() {
        Ok(cli)
    } else {
        Err(anyhow!("runwatch.exe is not next to {}", exe.display()))
    }
}

pub fn package_summary() -> String {
    let Ok(exe) = std::env::current_exe() else {
        return "package: current executable unavailable".into();
    };
    let Some(dir) = exe.parent() else {
        return "package: executable directory unavailable".into();
    };
    let expected = ["runwatch.exe", "runwatch-mcp.exe", "runwatch-gui.exe"];
    let missing = expected
        .iter()
        .filter(|name| !dir.join(name).is_file())
        .copied()
        .collect::<Vec<_>>();
    if missing.is_empty() {
        "package siblings: complete".into()
    } else {
        format!("package siblings missing: {}", missing.join(", "))
    }
}

fn parse_field<T: serde::de::DeserializeOwned>(value: &Value, field: &str) -> Result<T> {
    serde_json::from_value(
        value
            .get(field)
            .cloned()
            .ok_or_else(|| anyhow!("runwatch IPC response is missing {field}"))?,
    )
    .with_context(|| format!("parse runwatch IPC field {field}"))
}

fn parse_optional_field<T: serde::de::DeserializeOwned>(
    value: &Value,
    field: &str,
) -> Result<Option<T>> {
    let raw = value
        .get(field)
        .cloned()
        .ok_or_else(|| anyhow!("runwatch IPC response is missing {field}"))?;
    if raw.is_null() {
        Ok(None)
    } else {
        serde_json::from_value(raw)
            .map(Some)
            .with_context(|| format!("parse runwatch IPC field {field}"))
    }
}

fn format_logs(value: &Value) -> String {
    let logs = value.get("logs").unwrap_or(value);
    let stdout = logs.get("stdout").and_then(Value::as_str).unwrap_or("");
    let stderr = logs.get("stderr").and_then(Value::as_str).unwrap_or("");
    format!("STDOUT\n{stdout}\n\nSTDERR\n{stderr}")
}

fn format_artifacts(value: &Value) -> String {
    let Some(items) = value
        .get("artifacts")
        .and_then(|value| value.get("artifacts"))
        .and_then(Value::as_array)
    else {
        return "No lifecycle artifacts reported.".into();
    };
    if items.is_empty() {
        return "No lifecycle artifacts reported.".into();
    }
    items
        .iter()
        .map(|item| {
            format!(
                "{}\n{}",
                item.get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or("artifact"),
                item.get("path").and_then(Value::as_str).unwrap_or("-")
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}
