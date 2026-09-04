use chrono::{DateTime, Utc};
use runwatch_core::{
    ObservationHealth, ObservationRecord, RunAttemptRecord, RunContinuationStatus, RunEventRecord,
    RunRecord, RunResources, RunStatus, RunnerKind,
};
use std::collections::HashMap;

pub const STALE_AFTER_SECS: i64 = 120;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunFilter {
    Active,
    Attention,
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunSort {
    Priority,
    Newest,
    Name,
    Host,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostCardView {
    pub alias: String,
    pub endpoint: String,
    pub route: String,
    pub live_runs: usize,
}

impl HostCardView {
    pub fn usage_label(&self) -> String {
        match self.live_runs {
            0 => "Idle · no live Runs".into(),
            1 => "1 live Run".into(),
            count => format!("{count} live Runs"),
        }
    }
}

pub fn apply_host_run_usage(hosts: &mut [HostCardView], rows: &[RunRow]) {
    let counts = active_host_counts(rows);
    for host in hosts {
        host.live_runs = counts.get(&host.alias).copied().unwrap_or_default();
    }
}

fn active_host_counts(rows: &[RunRow]) -> std::collections::BTreeMap<String, usize> {
    let mut counts = std::collections::BTreeMap::<String, usize>::new();
    for row in rows.iter().filter(|row| row.active) {
        *counts.entry(row.host.clone()).or_default() += 1;
    }
    counts
}

#[derive(Debug, Clone)]
pub struct RunRow {
    pub run_id: String,
    pub name: String,
    pub status: RunStatus,
    pub runner: RunnerKind,
    pub host: String,
    pub handle: String,
    pub observation: String,
    pub continuation: String,
    pub updated: String,
    pub workspace: String,
    pub active: bool,
    pub attention: bool,
    pub attention_reason: Option<String>,
    pub updated_at: DateTime<Utc>,
}

impl RunRow {
    pub fn table_cells(&self) -> Vec<String> {
        vec![
            if self.attention {
                format!("! {}", status_label(self.status))
            } else {
                status_label(self.status).to_string()
            },
            self.name.clone(),
            runner_label(self.runner).to_string(),
            self.host.clone(),
            self.handle.clone(),
            self.observation.clone(),
            self.continuation.clone(),
            self.updated.clone(),
        ]
    }

    fn matches_query(&self, query: &str) -> bool {
        if query.is_empty() {
            return true;
        }
        let haystack = format!(
            "{}\n{}\n{}\n{}\n{}",
            self.name, self.run_id, self.host, self.handle, self.workspace
        )
        .to_lowercase();
        haystack.contains(query)
    }
}

#[derive(Debug, Clone)]
pub struct DashboardSnapshot {
    pub daemon_version: String,
    pub daemon_protocol: u64,
    pub daemon_capabilities: usize,
    pub daemon_pid: u64,
    pub paused: bool,
    pub manual_submit_supported: bool,
    pub retry_supported: bool,
    pub rows: Vec<RunRow>,
    pub active: usize,
    pub attention: usize,
    pub recent_terminal: usize,
    pub total: usize,
}

#[derive(Debug, Clone)]
pub struct RunDetailView {
    pub run_id: String,
    pub title: String,
    pub selected_attempt_no: Option<u32>,
    pub attempt_numbers: Vec<u32>,
    pub attempt_label: String,
    pub overview: String,
    pub logs: String,
    pub artifacts: String,
    pub timeline: String,
    pub continuation: String,
    pub retry_context: Option<RetryContextView>,
    pub can_cancel: bool,
}

#[derive(Debug, Clone)]
pub struct RetryContextView {
    pub run_id: String,
    pub expected_attempt_no: u32,
    pub runner: RunnerKind,
    pub host: String,
    pub workdir: String,
    pub command: String,
    pub resources: RunResources,
}

pub fn project_dashboard(
    daemon_version: String,
    daemon_protocol: u64,
    daemon_capabilities: usize,
    daemon_pid: u64,
    paused: bool,
    manual_submit_supported: bool,
    retry_supported: bool,
    runs: Vec<RunRecord>,
    observations: Vec<ObservationRecord>,
    continuations: Vec<RunContinuationStatus>,
    now: DateTime<Utc>,
) -> DashboardSnapshot {
    let observations: HashMap<(String, u32), ObservationRecord> = observations
        .into_iter()
        .map(|observation| {
            (
                (observation.run_id.clone(), observation.attempt_no),
                observation,
            )
        })
        .collect();
    let continuations: HashMap<String, RunContinuationStatus> = continuations
        .into_iter()
        .map(|status| (status.run_id.clone(), status))
        .collect();
    let mut rows: Vec<RunRow> = runs
        .into_iter()
        .map(|run| {
            let observation = run
                .attempt_no
                .and_then(|attempt_no| observations.get(&(run.run_id.clone(), attempt_no)));
            project_run(&run, observation, continuations.get(&run.run_id), now)
        })
        .collect();
    rows.sort_by(|a, b| {
        b.attention
            .cmp(&a.attention)
            .then_with(|| b.active.cmp(&a.active))
            .then_with(|| b.updated_at.cmp(&a.updated_at))
            .then_with(|| a.name.cmp(&b.name))
    });
    let active = rows.iter().filter(|row| row.active).count();
    let attention = rows.iter().filter(|row| row.attention).count();
    let recent_terminal = rows
        .iter()
        .filter(|row| {
            !row.active
                && now
                    .signed_duration_since(row.updated_at)
                    .num_seconds()
                    .clamp(0, i64::MAX)
                    <= 86_400
        })
        .count();
    let total = rows.len();
    DashboardSnapshot {
        daemon_version,
        daemon_protocol,
        daemon_capabilities,
        daemon_pid,
        paused,
        manual_submit_supported,
        retry_supported,
        rows,
        active,
        attention,
        recent_terminal,
        total,
    }
}

fn project_run(
    run: &RunRecord,
    observation: Option<&ObservationRecord>,
    continuation: Option<&RunContinuationStatus>,
    now: DateTime<Utc>,
) -> RunRow {
    let active = !run.status.is_terminal();
    let observation_unhealthy = observation
        .map(|value| value.health != ObservationHealth::Fresh)
        .unwrap_or(false);
    let observation_stale = active
        && observation
            .map(|value| {
                now.signed_duration_since(value.observed_at)
                    .num_seconds()
                    .max(0)
                    > STALE_AFTER_SECS
            })
            .unwrap_or(false);
    let continuation_attention = continuation
        .map(|value| value.retrying > 0 || value.needs_rebind > 0)
        .unwrap_or(false);
    let cancel_pending = active
        && run
            .note
            .as_deref()
            .map(|note| note.to_ascii_lowercase().contains("cancel requested"))
            .unwrap_or(false);
    let attention_reason = if run.status == RunStatus::Failed {
        Some("Run failed".to_string())
    } else if continuation
        .map(|value| value.needs_rebind > 0)
        .unwrap_or(false)
    {
        Some("Continuation needs explicit rebind".to_string())
    } else if continuation
        .map(|value| value.retrying > 0)
        .unwrap_or(false)
    {
        Some("Continuation delivery is retrying".to_string())
    } else if observation_unhealthy {
        observation.map(|value| format!("Observation is {}", health_label(value.health)))
    } else if observation_stale {
        Some("Observation is stale".to_string())
    } else if cancel_pending {
        Some("Cancel requested; awaiting terminal observation".to_string())
    } else {
        None
    };
    let attention = attention_reason.is_some() || continuation_attention;
    let workspace = run
        .workspace
        .as_ref()
        .map(|workspace| workspace.cwd.clone())
        .unwrap_or_default();
    RunRow {
        run_id: run.run_id.clone(),
        name: run
            .name
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| short_id(&run.run_id)),
        status: run.status,
        runner: run.runner,
        host: run.host.clone(),
        handle: run.job_id.clone().unwrap_or_else(|| "-".into()),
        observation: observation_label(observation, now, observation_stale),
        continuation: continuation_label(continuation),
        updated: format_age(
            now.signed_duration_since(run.updated_at)
                .num_seconds()
                .max(0),
        ),
        workspace,
        active,
        attention,
        attention_reason,
        updated_at: run.updated_at,
    }
}

pub fn filter_rows(rows: &[RunRow], filter: RunFilter, query: &str) -> Vec<RunRow> {
    let query = query.trim().to_lowercase();
    rows.iter()
        .filter(|row| match filter {
            RunFilter::Active => row.active,
            RunFilter::Attention => row.attention,
            RunFilter::All => true,
        })
        .filter(|row| row.matches_query(&query))
        .cloned()
        .collect()
}

pub fn sort_rows(rows: &mut [RunRow], sort: RunSort) {
    match sort {
        RunSort::Priority => {}
        RunSort::Newest => rows.sort_by(|left, right| {
            right
                .updated_at
                .cmp(&left.updated_at)
                .then_with(|| left.name.cmp(&right.name))
        }),
        RunSort::Name => rows.sort_by(|left, right| {
            left.name
                .to_ascii_lowercase()
                .cmp(&right.name.to_ascii_lowercase())
                .then_with(|| left.run_id.cmp(&right.run_id))
        }),
        RunSort::Host => rows.sort_by(|left, right| {
            left.host
                .to_ascii_lowercase()
                .cmp(&right.host.to_ascii_lowercase())
                .then_with(|| left.name.cmp(&right.name))
        }),
    }
}

pub fn host_usage_summary(rows: &[RunRow]) -> String {
    let counts = active_host_counts(rows);
    if counts.is_empty() {
        "No live Runs are currently using an SSH/local Host.".into()
    } else {
        counts
            .into_iter()
            .map(|(host, count)| {
                format!(
                    "{host}   {count} live Run{}",
                    if count == 1 { "" } else { "s" }
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

pub fn build_detail(
    run: RunRecord,
    observation: Option<ObservationRecord>,
    attempt: Option<RunAttemptRecord>,
    attempts: Vec<RunAttemptRecord>,
    selected_attempt_no: Option<u32>,
    attempt_error: Option<String>,
    continuation: RunContinuationStatus,
    continuation_error: Option<String>,
    events: Vec<RunEventRecord>,
    events_error: Option<String>,
    logs: String,
    artifacts: String,
) -> RunDetailView {
    let title = run
        .name
        .clone()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| short_id(&run.run_id));
    let workspace = run
        .workspace
        .as_ref()
        .map(|value| format!("{}:{}", value.host_alias, value.cwd))
        .unwrap_or_else(|| "-".into());
    let observation_text = observation
        .as_ref()
        .map(|value| {
            format!(
                "{} / {}\nobserved: {}\nraw: {}\nreason: {}\nexit: {}",
                health_label(value.health),
                source_label(value),
                value.observed_at.to_rfc3339(),
                value.raw_state.as_deref().unwrap_or("-"),
                value.reason.as_deref().unwrap_or("-"),
                value
                    .command_exit_code
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "-".into())
            )
        })
        .unwrap_or_else(|| "No durable observation yet".into());
    let attempt_text = if let Some(error) = attempt_error {
        format!("Attempt metadata unavailable\n{error}")
    } else {
        attempt
            .as_ref()
            .map(|value| {
                format!(
                    "attempt {}\nstatus: {}\nhandle: {}\ncreated: {}\nworkdir: {}\ncommand: {}\nresources: {}",
                    value.attempt_no,
                    status_label(value.status),
                    value.job_id.as_deref().unwrap_or("-"),
                    value.created_at.to_rfc3339(),
                    value.workdir,
                    bounded_text(&value.command, 800),
                    serde_json::to_string(&value.resources).unwrap_or_else(|_| "{}".into())
                )
            })
            .unwrap_or_else(|| "No durable Attempt metadata yet".into())
    };
    let attempt_numbers = attempts
        .iter()
        .map(|value| value.attempt_no)
        .collect::<Vec<_>>();
    let attempt_history = if attempts.is_empty() {
        "No durable Attempt history yet".into()
    } else {
        attempts
            .iter()
            .map(|value| {
                format!(
                    "#{} {} {}",
                    value.attempt_no,
                    status_label(value.status),
                    value.job_id.as_deref().unwrap_or("-")
                )
            })
            .collect::<Vec<_>>()
            .join("  ·  ")
    };
    let attempt_label = selected_attempt_no
        .map(|selected| {
            let position = attempt_numbers
                .iter()
                .position(|value| *value == selected)
                .map(|index| index + 1)
                .unwrap_or(0);
            let marker = if run.attempt_no == Some(selected) {
                "current"
            } else {
                "historical"
            };
            format!(
                "Attempt {selected} · {position}/{} · {marker}",
                attempt_numbers.len()
            )
        })
        .unwrap_or_else(|| "No Attempt".into());
    let timeline = if let Some(error) = events_error {
        format!("Timeline unavailable\n{error}")
    } else if events.is_empty() {
        "No lifecycle events recorded.".into()
    } else {
        events
            .iter()
            .map(|event| {
                format!(
                    "{}  {}\n{}",
                    event.at.to_rfc3339(),
                    event.kind,
                    compact_json(&event.payload)
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    let continuation_known_unbound = continuation_error.is_none() && !continuation.configured;
    let current_attempt_no = run.attempt_no;
    let selected_is_current =
        selected_attempt_no.is_some() && selected_attempt_no == current_attempt_no;
    let retry_context = if selected_is_current
        && continuation_known_unbound
        && matches!(run.status, RunStatus::Failed | RunStatus::Cancelled)
    {
        attempt.as_ref().and_then(|value| {
            matches!(value.status, RunStatus::Failed | RunStatus::Cancelled).then(|| {
                RetryContextView {
                    run_id: run.run_id.clone(),
                    expected_attempt_no: value.attempt_no,
                    runner: value.runner,
                    host: value.host.clone(),
                    workdir: value.workdir.clone(),
                    command: value.command.clone(),
                    resources: value.resources.clone(),
                }
            })
        })
    } else {
        None
    };
    let retry_policy = if !selected_is_current {
        "Historical Attempt selected; return to the current Attempt before retrying."
    } else if continuation_error.is_some() {
        "Continuation status is unavailable; human Retry fails closed."
    } else if continuation.configured {
        "This Run is agent-bound; Retry is managed by the owning Agent Integration."
    } else if !matches!(run.status, RunStatus::Failed | RunStatus::Cancelled) {
        "Human Retry is available only after the current Attempt is failed or cancelled."
    } else if retry_context.is_some() {
        "Eligible for human Retry. Command/workspace/runner/host stay fixed; only scheduler resources may be reviewed."
    } else {
        "Retry is unavailable because durable current-Attempt metadata is incomplete."
    };
    let overview = format!(
        "Run ID: {}\nStatus: {}\nRunner: {}\nHost: {}\nHandle: {}\nWorkspace: {}\nUpdated: {}\n\nAttempt history\n{}\n\nSelected observation\n{}\n\nSelected Attempt\n{}\n\nRetry policy\n{}",
        run.run_id,
        status_label(run.status),
        runner_label(run.runner),
        run.host,
        run.job_id.as_deref().unwrap_or("-"),
        workspace,
        run.updated_at.to_rfc3339(),
        attempt_history,
        observation_text,
        attempt_text,
        retry_policy
    );
    let continuation_text = if let Some(error) = continuation_error {
        format!("Continuation status unavailable\n{error}")
    } else if continuation.configured {
        format!(
            "Agent: {}\nSession: {}\nProject: {}\nPending: {}  Delivering: {}  Retrying: {}  Needs rebind: {}  Delivered: {}\nLast state: {}\nLast error: {}{}",
            continuation.agent_kind.as_deref().unwrap_or("-"),
            short_id(continuation.session_id.as_deref().unwrap_or("-")),
            continuation.project_root.as_deref().unwrap_or("-"),
            continuation.pending,
            continuation.delivering,
            continuation.retrying,
            continuation.needs_rebind,
            continuation.delivered,
            continuation.last_state.as_deref().unwrap_or("-"),
            continuation.last_error.as_deref().unwrap_or("-"),
            if continuation.needs_rebind > 0 {
                "\n\nAction required: return to the bound agent/Pi session and perform an explicit rebind there."
            } else {
                ""
            }
        )
    } else {
        "No agent continuation is bound to this Run.".into()
    };
    RunDetailView {
        run_id: run.run_id.clone(),
        title,
        selected_attempt_no,
        attempt_numbers,
        attempt_label,
        overview,
        logs,
        artifacts,
        timeline,
        continuation: continuation_text,
        retry_context,
        can_cancel: selected_attempt_no == current_attempt_no && !run.status.is_terminal(),
    }
}

fn bounded_text(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let bounded: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{bounded}... [truncated]")
    } else {
        bounded
    }
}

fn compact_json(value: &serde_json::Value) -> String {
    let text = serde_json::to_string(value).unwrap_or_else(|_| "{}".into());
    let mut chars = text.chars();
    let compact: String = chars.by_ref().take(600).collect();
    if chars.next().is_some() {
        format!("{compact}...")
    } else {
        compact
    }
}

pub fn status_label(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Submitting => "submitting",
        RunStatus::Queued => "queued",
        RunStatus::Running => "running",
        RunStatus::Succeeded => "succeeded",
        RunStatus::Failed => "failed",
        RunStatus::Cancelled => "cancelled",
        RunStatus::Unknown => "unknown",
    }
}

fn runner_label(runner: RunnerKind) -> &'static str {
    match runner {
        RunnerKind::Slurm => "Slurm",
        RunnerKind::Lsf => "LSF",
        RunnerKind::Process => "Process",
        RunnerKind::File => "File",
        RunnerKind::Powershell => "PowerShell",
    }
}

fn health_label(health: ObservationHealth) -> &'static str {
    match health {
        ObservationHealth::Fresh => "fresh",
        ObservationHealth::ProbeError => "probe error",
        ObservationHealth::Unreachable => "unreachable",
    }
}

fn source_label(observation: &ObservationRecord) -> &'static str {
    use runwatch_core::ObservationSource;
    match observation.source {
        ObservationSource::LocalProcess => "local process",
        ObservationSource::Sentinel => "sentinel",
        ObservationSource::Scheduler => "scheduler",
        ObservationSource::Compatibility => "compatibility",
        ObservationSource::Transport => "transport",
    }
}

fn continuation_label(status: Option<&RunContinuationStatus>) -> String {
    let Some(status) = status else {
        return "-".into();
    };
    if status.needs_rebind > 0 {
        format!("needs rebind ({})", status.needs_rebind)
    } else if status.retrying > 0 {
        format!("retrying ({})", status.retrying)
    } else if status.delivering > 0 {
        format!("delivering ({})", status.delivering)
    } else if status.pending > 0 {
        format!("pending ({})", status.pending)
    } else if status.configured {
        "bound".into()
    } else {
        "-".into()
    }
}

fn observation_label(
    observation: Option<&ObservationRecord>,
    now: DateTime<Utc>,
    stale: bool,
) -> String {
    let Some(observation) = observation else {
        return "no observation".into();
    };
    let age = format_age(
        now.signed_duration_since(observation.observed_at)
            .num_seconds()
            .max(0),
    );
    if stale {
        format!("stale {age} / {}", health_label(observation.health))
    } else {
        format!(
            "{} / {} / {age}",
            health_label(observation.health),
            source_label(observation)
        )
    }
}

fn format_age(seconds: i64) -> String {
    match seconds {
        0..=59 => format!("{seconds}s"),
        60..=3599 => format!("{}m", seconds / 60),
        3600..=86_399 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86_400),
    }
}

fn short_id(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    if chars.len() <= 18 {
        return value.to_string();
    }
    format!(
        "{}...{}",
        chars.iter().take(8).collect::<String>(),
        chars.iter().skip(chars.len() - 6).collect::<String>()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use runwatch_core::{ObservationSource, RemoteWorkspaceRef};

    fn run(id: &str, status: RunStatus, updated_at: DateTime<Utc>) -> RunRecord {
        RunRecord {
            run_id: id.into(),
            name: Some(format!("name-{id}")),
            host: "gm00".into(),
            job_id: Some("31842".into()),
            runner: RunnerKind::Slurm,
            remote_terminal: None,
            status,
            workspace: Some(RemoteWorkspaceRef {
                host_alias: "gm00".into(),
                cwd: "/share/work".into(),
            }),
            attempt_no: Some(1),
            session_id: None,
            project_root: None,
            agent: None,
            updated_at,
            note: None,
        }
    }

    fn attempt(
        run_id: &str,
        attempt_no: u32,
        status: RunStatus,
        now: DateTime<Utc>,
    ) -> RunAttemptRecord {
        RunAttemptRecord {
            run_id: run_id.into(),
            attempt_no,
            runner: RunnerKind::Slurm,
            host: "gm00".into(),
            workdir: "/share/work".into(),
            command: "python refine.py --input map.mrc".into(),
            resources: RunResources {
                time: Some("02:00:00".into()),
                partition: Some("gpu".into()),
                cpus: Some(8),
                mem: Some("32G".into()),
                ..RunResources::default()
            },
            job_name: format!("rw-{run_id}-a{attempt_no}"),
            job_id: Some(format!("3184{attempt_no}")),
            script_path: format!("/share/work/.runwatch/{run_id}/attempt-{attempt_no}/run.sh"),
            stdout_path: format!("/share/work/.runwatch/{run_id}/attempt-{attempt_no}/stdout.log"),
            stderr_path: format!("/share/work/.runwatch/{run_id}/attempt-{attempt_no}/stderr.log"),
            terminal_path: format!(
                "/share/work/.runwatch/{run_id}/attempt-{attempt_no}/terminal.json"
            ),
            receipt_path: format!(
                "/share/work/.runwatch/{run_id}/attempt-{attempt_no}/submission.receipt"
            ),
            status,
            created_at: now,
            updated_at: now,
            error: None,
        }
    }

    fn dashboard(
        runs: Vec<RunRecord>,
        observations: Vec<ObservationRecord>,
        continuations: Vec<RunContinuationStatus>,
        now: DateTime<Utc>,
    ) -> DashboardSnapshot {
        project_dashboard(
            "0.1.0".into(),
            1,
            24,
            4242,
            false,
            true,
            true,
            runs,
            observations,
            continuations,
            now,
        )
    }

    #[test]
    fn running_unreachable_keeps_execution_state_and_gets_attention() {
        let now = Utc::now();
        let run = run("r1", RunStatus::Running, now);
        let observation = ObservationRecord {
            run_id: "r1".into(),
            attempt_no: 1,
            observed_at: now,
            source: ObservationSource::Transport,
            health: ObservationHealth::Unreachable,
            execution_status: RunStatus::Running,
            raw_state: None,
            reason: Some("ssh down".into()),
            command_exit_code: None,
        };
        let snapshot = dashboard(vec![run], vec![observation], vec![], now);
        assert_eq!(snapshot.rows[0].status, RunStatus::Running);
        assert!(snapshot.rows[0].attention);
        assert!(snapshot.rows[0].observation.contains("unreachable"));
    }

    #[test]
    fn stale_active_observation_is_attention_but_terminal_age_is_not() {
        let now = Utc::now();
        let old = now - Duration::seconds(STALE_AFTER_SECS + 1);
        let observation = ObservationRecord {
            run_id: "r1".into(),
            attempt_no: 1,
            observed_at: old,
            source: ObservationSource::Scheduler,
            health: ObservationHealth::Fresh,
            execution_status: RunStatus::Running,
            raw_state: None,
            reason: None,
            command_exit_code: None,
        };
        let live = dashboard(
            vec![run("r1", RunStatus::Running, now)],
            vec![observation.clone()],
            vec![],
            now,
        );
        assert!(live.rows[0].attention);
        let terminal = dashboard(
            vec![run("r1", RunStatus::Succeeded, now)],
            vec![observation],
            vec![],
            now,
        );
        assert!(!terminal.rows[0].attention);
    }

    #[test]
    fn continuation_rebind_is_promoted_to_dashboard_attention() {
        let now = Utc::now();
        let snapshot = dashboard(
            vec![run("r1", RunStatus::Succeeded, now)],
            vec![],
            vec![RunContinuationStatus {
                run_id: "r1".into(),
                configured: true,
                needs_rebind: 1,
                ..RunContinuationStatus::default()
            }],
            now,
        );
        assert!(snapshot.rows[0].attention);
        assert_eq!(snapshot.rows[0].continuation, "needs rebind (1)");
    }

    #[test]
    fn empty_single_and_fifty_run_snapshots_preserve_counts() {
        let now = Utc::now();
        let empty = dashboard(vec![], vec![], vec![], now);
        assert_eq!(empty.total, 0);
        assert_eq!(empty.active, 0);
        assert_eq!(empty.attention, 0);

        let single = dashboard(
            vec![run("one", RunStatus::Running, now)],
            vec![],
            vec![],
            now,
        );
        assert_eq!(single.total, 1);
        assert_eq!(single.active, 1);

        let fifty = dashboard(
            (0..50)
                .map(|index| {
                    run(
                        &format!("r-{index:02}"),
                        if index % 2 == 0 {
                            RunStatus::Running
                        } else {
                            RunStatus::Succeeded
                        },
                        now - Duration::seconds(index),
                    )
                })
                .collect(),
            vec![],
            vec![],
            now,
        );
        assert_eq!(fifty.total, 50);
        assert_eq!(fifty.active, 25);
    }

    #[test]
    fn host_usage_counts_only_live_runs() {
        let now = Utc::now();
        let mut rows = dashboard(
            vec![
                run("a", RunStatus::Running, now),
                run("b", RunStatus::Queued, now),
                run("c", RunStatus::Succeeded, now),
            ],
            vec![],
            vec![],
            now,
        )
        .rows;
        rows[0].host = "gm00".into();
        rows[1].host = "gm00".into();
        rows[2].host = "other".into();
        let text = host_usage_summary(&rows);
        assert!(text.contains("gm00   2 live Runs"));
        assert!(!text.contains("other"));
    }

    #[test]
    fn host_cards_receive_live_run_counts_by_exact_alias() {
        let now = Utc::now();
        let mut rows = dashboard(
            vec![
                run("a", RunStatus::Running, now),
                run("b", RunStatus::Queued, now),
                run("c", RunStatus::Succeeded, now),
            ],
            vec![],
            vec![],
            now,
        )
        .rows;
        rows[0].host = "gm00".into();
        rows[1].host = "gm00".into();
        rows[2].host = "compute-gw".into();
        let mut hosts = vec![
            HostCardView {
                alias: "gm00".into(),
                endpoint: "u@gm00:22".into(),
                route: "Direct".into(),
                live_runs: 0,
            },
            HostCardView {
                alias: "compute-gw".into(),
                endpoint: "u@compute:22".into(),
                route: "Direct".into(),
                live_runs: 99,
            },
        ];
        apply_host_run_usage(&mut hosts, &rows);
        assert_eq!(hosts[0].live_runs, 2);
        assert_eq!(hosts[1].live_runs, 0);
    }

    #[test]
    fn unresolved_cancel_is_attention_without_forcing_cancelled_state() {
        let now = Utc::now();
        let mut value = run("r-cancel", RunStatus::Running, now);
        value.note = Some("cancel requested for scheduler job 31842".into());
        let snapshot = dashboard(vec![value], vec![], vec![], now);
        assert_eq!(snapshot.rows[0].status, RunStatus::Running);
        assert!(snapshot.rows[0].attention);
        assert_eq!(
            snapshot.rows[0].attention_reason.as_deref(),
            Some("Cancel requested; awaiting terminal observation")
        );
    }

    #[test]
    fn explicit_sort_modes_preserve_stable_human_ordering() {
        let now = Utc::now();
        let mut rows = dashboard(
            vec![
                run("z", RunStatus::Running, now - Duration::seconds(30)),
                run("a", RunStatus::Failed, now - Duration::seconds(10)),
                run("m", RunStatus::Succeeded, now),
            ],
            vec![],
            vec![],
            now,
        )
        .rows;
        sort_rows(&mut rows, RunSort::Newest);
        assert_eq!(rows[0].run_id, "m");
        sort_rows(&mut rows, RunSort::Name);
        assert_eq!(rows[0].run_id, "a");
        rows[0].host = "zz-host".into();
        rows[1].host = "aa-host".into();
        sort_rows(&mut rows, RunSort::Host);
        assert_eq!(rows[0].host, "aa-host");
    }

    #[test]
    fn detail_partial_failures_are_explicit_not_missing_data() {
        let now = Utc::now();
        let detail = build_detail(
            run("r-detail", RunStatus::Running, now),
            None,
            None,
            vec![],
            Some(1),
            Some("attempt IPC failed".into()),
            RunContinuationStatus::default(),
            Some("continuation IPC failed".into()),
            vec![],
            Some("events IPC failed".into()),
            "Logs unavailable\nlog IPC failed".into(),
            "Artifacts unavailable\nartifact IPC failed".into(),
        );
        assert!(detail.overview.contains("Attempt metadata unavailable"));
        assert!(detail.overview.contains("attempt IPC failed"));
        assert!(detail.timeline.contains("Timeline unavailable"));
        assert!(detail.timeline.contains("events IPC failed"));
        assert!(
            detail
                .continuation
                .contains("Continuation status unavailable")
        );
        assert!(detail.continuation.contains("continuation IPC failed"));
        assert!(detail.logs.contains("Logs unavailable"));
        assert!(detail.artifacts.contains("Artifacts unavailable"));
    }

    #[test]
    fn human_retry_context_is_fail_closed_around_binding_and_attempt_selection() {
        let now = Utc::now();
        let failed = attempt("r-retry-ui", 1, RunStatus::Failed, now);
        let eligible = build_detail(
            run("r-retry-ui", RunStatus::Failed, now),
            None,
            Some(failed.clone()),
            vec![failed.clone()],
            Some(1),
            None,
            RunContinuationStatus::default(),
            None,
            vec![],
            None,
            String::new(),
            String::new(),
        );
        let retry = eligible
            .retry_context
            .expect("unbound failed current Attempt should retry");
        assert_eq!(retry.expected_attempt_no, 1);
        assert_eq!(retry.command, failed.command);
        assert_eq!(retry.resources.cpus, Some(8));
        assert!(eligible.overview.contains("Eligible for human Retry"));

        let bound = build_detail(
            run("r-retry-ui", RunStatus::Failed, now),
            None,
            Some(failed.clone()),
            vec![failed.clone()],
            Some(1),
            None,
            RunContinuationStatus {
                run_id: "r-retry-ui".into(),
                configured: true,
                agent_kind: Some("pi".into()),
                session_id: Some("session-bound".into()),
                ..RunContinuationStatus::default()
            },
            None,
            vec![],
            None,
            String::new(),
            String::new(),
        );
        assert!(bound.retry_context.is_none());
        assert!(
            bound
                .overview
                .contains("managed by the owning Agent Integration")
        );

        let unknown_binding = build_detail(
            run("r-retry-ui", RunStatus::Failed, now),
            None,
            Some(failed.clone()),
            vec![failed.clone()],
            Some(1),
            None,
            RunContinuationStatus::default(),
            Some("continuation IPC unavailable".into()),
            vec![],
            None,
            String::new(),
            String::new(),
        );
        assert!(unknown_binding.retry_context.is_none());
        assert!(unknown_binding.overview.contains("fails closed"));

        let mut current = run("r-retry-ui", RunStatus::Failed, now);
        current.attempt_no = Some(2);
        current.job_id = Some("31842".into());
        let current_attempt = attempt("r-retry-ui", 2, RunStatus::Failed, now);
        let historical = build_detail(
            current,
            None,
            Some(failed.clone()),
            vec![failed, current_attempt],
            Some(1),
            None,
            RunContinuationStatus::default(),
            None,
            vec![],
            None,
            String::new(),
            String::new(),
        );
        assert!(historical.retry_context.is_none());
        assert!(historical.overview.contains("Historical Attempt selected"));
    }

    #[test]
    fn filters_and_search_scale_to_large_local_snapshots() {
        let now = Utc::now();
        let runs = (0..500)
            .map(|index| {
                run(
                    &format!("run-{index:03}"),
                    if index % 5 == 0 {
                        RunStatus::Failed
                    } else if index % 2 == 0 {
                        RunStatus::Running
                    } else {
                        RunStatus::Succeeded
                    },
                    now - Duration::seconds(index),
                )
            })
            .collect();
        let snapshot = dashboard(runs, vec![], vec![], now);
        assert_eq!(snapshot.total, 500);
        assert_eq!(
            filter_rows(&snapshot.rows, RunFilter::Attention, "").len(),
            100
        );
        assert_eq!(
            filter_rows(&snapshot.rows, RunFilter::All, "run-042").len(),
            1
        );
    }
}
