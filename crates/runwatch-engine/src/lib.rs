pub mod ipc;
pub mod local_process;
pub mod offline;
pub mod operations;
pub mod submission;

use anyhow::Result;
use chrono::Utc;
use runwatch_core::live::{self, ServeLock};
use runwatch_core::{
    AppConfig, ObservationHealth, ObservationRecord, ObservationSource, RunAttemptRecord,
    RunRecord, RunStatus, RunStore, RunnerKind, interpret_lsf_batch, interpret_observation,
    interpret_slurm_batch, lsf_batch_command, remote_command, slurm_batch_command,
};
use runwatch_ssh::HostPool;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const MAX_OBSERVATION_TEXT_CHARS: usize = 2048;

fn observation_text(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.chars().take(MAX_OBSERVATION_TEXT_CHARS).collect())
    }
}

fn persist_observation(
    store: &RunStore,
    rec: &RunRecord,
    source: ObservationSource,
    health: ObservationHealth,
    execution_status: RunStatus,
    raw_state: Option<String>,
    reason: Option<String>,
    command_exit_code: Option<u32>,
) -> Result<()> {
    let observation = ObservationRecord {
        run_id: rec.run_id.clone(),
        attempt_no: rec.attempt_no.unwrap_or(1),
        observed_at: Utc::now(),
        source,
        health,
        execution_status,
        raw_state,
        reason,
        command_exit_code,
    };
    store.upsert_observation(&observation)?;
    Ok(())
}

fn adaptive_probe_interval(
    rec: &RunRecord,
    observation: Option<&ObservationRecord>,
    base_interval: Duration,
) -> Duration {
    let base_secs = base_interval.as_secs().max(1);
    if rec.runner == runwatch_core::RunnerKind::Process {
        return Duration::from_secs(base_secs);
    }

    let mut factor = if rec.status == RunStatus::Running {
        2
    } else {
        1
    };
    if let Some(observation) = observation {
        factor = match observation.health {
            ObservationHealth::Fresh => factor,
            ObservationHealth::ProbeError => factor.max(2),
            ObservationHealth::Unreachable => factor.max(4),
        };
    }
    let cap_secs = 300u64.max(base_secs);
    Duration::from_secs(base_secs.saturating_mul(factor).min(cap_secs))
}

fn observation_is_due(
    rec: &RunRecord,
    observation: Option<&ObservationRecord>,
    now: chrono::DateTime<Utc>,
    base_interval: Duration,
) -> bool {
    let Some(observation) = observation else {
        return true;
    };
    let interval = adaptive_probe_interval(rec, Some(observation), base_interval);
    let elapsed = now
        .signed_duration_since(observation.observed_at)
        .to_std()
        .unwrap_or_default();
    elapsed >= interval
}

fn observation_key(rec: &RunRecord) -> (String, u32) {
    (rec.run_id.clone(), rec.attempt_no.unwrap_or(1))
}

fn batchable_v2_slurm(rec: &RunRecord, attempt: Option<&RunAttemptRecord>) -> bool {
    let Some(attempt) = attempt else {
        return false;
    };
    rec.runner == RunnerKind::Slurm
        && attempt.runner == RunnerKind::Slurm
        && rec.attempt_no == Some(attempt.attempt_no)
        && rec.job_id.as_deref().is_some()
        && rec.job_id.as_deref() == attempt.job_id.as_deref()
        && rec.remote_terminal.as_deref() == Some(attempt.terminal_path.as_str())
}

fn batchable_v2_lsf(rec: &RunRecord, attempt: Option<&RunAttemptRecord>) -> bool {
    let Some(attempt) = attempt else {
        return false;
    };
    rec.runner == RunnerKind::Lsf
        && attempt.runner == RunnerKind::Lsf
        && rec.attempt_no == Some(attempt.attempt_no)
        && rec.job_id.as_deref().is_some()
        && rec.job_id.as_deref() == attempt.job_id.as_deref()
        && rec.remote_terminal.as_deref() == Some(attempt.terminal_path.as_str())
}

fn persist_remote_transition(
    store: &RunStore,
    rec: &mut RunRecord,
    next: RunStatus,
    transitions: &mut Vec<Transition>,
) -> Result<()> {
    if next == rec.status {
        return Ok(());
    }
    let from = rec.status;
    rec.status = next;
    rec.updated_at = Utc::now();
    store.upsert(rec)?;
    if next.is_terminal() {
        let _ = store.ensure_terminal_delivery(rec)?;
    }
    transitions.push(Transition {
        run_id: rec.run_id.clone(),
        from,
        to: next,
    });
    Ok(())
}

async fn probe_slurm_batch(
    store: &RunStore,
    pool: &HostPool,
    records: Vec<RunRecord>,
    transitions: &mut Vec<Transition>,
    errors: &mut Vec<String>,
) -> Result<HashSet<String>> {
    let handled = records
        .iter()
        .map(|record| record.run_id.clone())
        .collect::<HashSet<_>>();
    let Some(command) = slurm_batch_command(&records) else {
        return Ok(HashSet::new());
    };
    let host = records
        .first()
        .map(|record| record.host.clone())
        .unwrap_or_default();
    match pool.exec(&host, &command).await {
        Ok(out) if out.code == Some(0) => {
            let batch_results = interpret_slurm_batch(&records, &out.stdout);
            for (mut rec, result) in records.into_iter().zip(batch_results) {
                if result.probe.recognized {
                    persist_observation(
                        store,
                        &rec,
                        result.probe.source,
                        ObservationHealth::Fresh,
                        result.probe.status,
                        observation_text(&result.probe.raw_state),
                        None,
                        out.code,
                    )?;
                    persist_remote_transition(store, &mut rec, result.probe.status, transitions)?;
                } else {
                    let reason = result.reason.or_else(|| {
                        Some("batched Slurm probe returned no recognized state".into())
                    });
                    persist_observation(
                        store,
                        &rec,
                        result.probe.source,
                        ObservationHealth::ProbeError,
                        rec.status,
                        observation_text(&result.probe.raw_state),
                        reason.clone(),
                        out.code,
                    )?;
                    errors.push(format!(
                        "{}: {}",
                        rec.run_id,
                        reason.as_deref().unwrap_or("batched Slurm probe failed")
                    ));
                }
            }
        }
        Ok(out) => {
            let stderr = observation_text(&out.stderr).unwrap_or_default();
            let detail = if stderr.is_empty() {
                format!("batched Slurm remote probe exit status {:?}", out.code)
            } else {
                format!(
                    "batched Slurm remote probe exit status {:?}: {stderr}",
                    out.code
                )
            };
            let reason = observation_text(&detail);
            for rec in &records {
                persist_observation(
                    store,
                    rec,
                    ObservationSource::Scheduler,
                    ObservationHealth::ProbeError,
                    rec.status,
                    None,
                    reason.clone(),
                    out.code,
                )?;
                errors.push(format!("{}: {detail}", rec.run_id));
            }
        }
        Err(err) => {
            let reason = observation_text(&format!("{err:#}"));
            for rec in &records {
                persist_observation(
                    store,
                    rec,
                    ObservationSource::Transport,
                    ObservationHealth::Unreachable,
                    rec.status,
                    None,
                    reason.clone(),
                    None,
                )?;
                errors.push(format!("{}: {err}", rec.run_id));
            }
            pool.drop_host(&host).await;
        }
    }
    Ok(handled)
}

async fn probe_lsf_batch(
    store: &RunStore,
    pool: &HostPool,
    records: Vec<RunRecord>,
    transitions: &mut Vec<Transition>,
    errors: &mut Vec<String>,
) -> Result<HashSet<String>> {
    let Some(command) = lsf_batch_command(&records) else {
        return Ok(HashSet::new());
    };
    let host = records
        .first()
        .map(|record| record.host.clone())
        .unwrap_or_default();
    let mut handled = HashSet::new();
    match pool.exec(&host, &command).await {
        Ok(out) if out.code == Some(0) => {
            let batch_results = interpret_lsf_batch(&records, &out.stdout);
            for (mut rec, result) in records.into_iter().zip(batch_results) {
                if result.probe.recognized {
                    persist_observation(
                        store,
                        &rec,
                        result.probe.source,
                        ObservationHealth::Fresh,
                        result.probe.status,
                        observation_text(&result.probe.raw_state),
                        None,
                        out.code,
                    )?;
                    persist_remote_transition(store, &mut rec, result.probe.status, transitions)?;
                    handled.insert(rec.run_id);
                } else if result.probe.source == ObservationSource::Sentinel {
                    let reason = result
                        .reason
                        .or_else(|| Some("batched LSF sentinel was not recognized".into()));
                    persist_observation(
                        store,
                        &rec,
                        ObservationSource::Sentinel,
                        ObservationHealth::ProbeError,
                        rec.status,
                        observation_text(&result.probe.raw_state),
                        reason.clone(),
                        out.code,
                    )?;
                    errors.push(format!(
                        "{}: {}",
                        rec.run_id,
                        reason.as_deref().unwrap_or("batched LSF sentinel failed")
                    ));
                    handled.insert(rec.run_id);
                }
                // Missing/unknown bjobs rows intentionally remain unhandled so tick_selected
                // runs the existing per-Run bjobs -> bhist history fallback in this same tick.
            }
        }
        Ok(out) => {
            let stderr = observation_text(&out.stderr).unwrap_or_default();
            let detail = if stderr.is_empty() {
                format!("batched LSF remote probe exit status {:?}", out.code)
            } else {
                format!(
                    "batched LSF remote probe exit status {:?}: {stderr}",
                    out.code
                )
            };
            let reason = observation_text(&detail);
            for rec in &records {
                persist_observation(
                    store,
                    rec,
                    ObservationSource::Scheduler,
                    ObservationHealth::ProbeError,
                    rec.status,
                    None,
                    reason.clone(),
                    out.code,
                )?;
                errors.push(format!("{}: {detail}", rec.run_id));
                handled.insert(rec.run_id.clone());
            }
        }
        Err(err) => {
            let reason = observation_text(&format!("{err:#}"));
            for rec in &records {
                persist_observation(
                    store,
                    rec,
                    ObservationSource::Transport,
                    ObservationHealth::Unreachable,
                    rec.status,
                    None,
                    reason.clone(),
                    None,
                )?;
                errors.push(format!("{}: {err}", rec.run_id));
                handled.insert(rec.run_id.clone());
            }
            pool.drop_host(&host).await;
        }
    }
    Ok(handled)
}

#[derive(Debug, Clone)]
pub struct Transition {
    pub run_id: String,
    pub from: RunStatus,
    pub to: RunStatus,
}

#[derive(Debug, Clone)]
pub struct TickReport {
    pub runs: Vec<RunRecord>,
    pub transitions: Vec<Transition>,
    pub errors: Vec<String>,
}

impl TickReport {
    pub fn summarize(&self) -> (String, String) {
        let live = self.runs.iter().filter(|r| !r.status.is_terminal()).count();
        let failed = self
            .runs
            .iter()
            .filter(|r| r.status == RunStatus::Failed)
            .count();
        let header = format!(
            "{live} live    {failed} failed    {} total",
            self.runs.len()
        );
        let body = if self.runs.is_empty() {
            "No runs in runwatch.db yet.".into()
        } else {
            self.runs
                .iter()
                .rev()
                .take(24)
                .map(|r| {
                    format!(
                        "{}  {:?}  {}  job {}",
                        r.run_id,
                        r.status,
                        r.host,
                        r.job_id.as_deref().unwrap_or("-")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        (header, body)
    }
}

async fn tick_selected<F>(
    store: &RunStore,
    pool: &HostPool,
    mut should_probe: F,
) -> Result<TickReport>
where
    F: FnMut(&RunRecord) -> bool,
{
    let mut transitions = Vec::new();
    let mut errors = Vec::new();
    for mut rec in store.list()? {
        if rec.status.is_terminal() {
            let _ = store.ensure_terminal_delivery(&rec)?;
            continue;
        }
        if !should_probe(&rec) {
            continue;
        }
        if rec.runner == runwatch_core::RunnerKind::Process {
            match local_process::observe_local_run(store, &mut rec) {
                Ok(next) => {
                    persist_observation(
                        store,
                        &rec,
                        ObservationSource::LocalProcess,
                        ObservationHealth::Fresh,
                        next,
                        rec.job_id.clone(),
                        None,
                        None,
                    )?;
                    if next != rec.status {
                        let from = rec.status;
                        rec.status = next;
                        rec.updated_at = Utc::now();
                        if let Some(attempt_no) = rec.attempt_no
                            && let Some(mut attempt) = store.get_attempt(&rec.run_id, attempt_no)?
                        {
                            attempt.status = next;
                            attempt.updated_at = rec.updated_at;
                            store.persist_run_attempt_event(
                                &rec,
                                &attempt,
                                "local_process_observed",
                            )?;
                        } else {
                            store.upsert(&rec)?;
                        }
                        if next.is_terminal() {
                            let _ = store.ensure_terminal_delivery(&rec)?;
                        }
                        transitions.push(Transition {
                            run_id: rec.run_id.clone(),
                            from,
                            to: next,
                        });
                    }
                }
                Err(err) => {
                    let reason = observation_text(&format!("{err:#}"));
                    persist_observation(
                        store,
                        &rec,
                        ObservationSource::LocalProcess,
                        ObservationHealth::ProbeError,
                        rec.status,
                        rec.job_id.clone(),
                        reason.clone(),
                        None,
                    )?;
                    errors.push(format!("{}: {err:#}", rec.run_id));
                }
            }
            continue;
        }
        let Some(cmd) = remote_command(&rec) else {
            continue;
        };
        match pool.exec(&rec.host, &cmd).await {
            Ok(out) => {
                let probe = interpret_observation(&rec, &out.stdout);
                let successful_exit = out.code == Some(0);
                let (health, next, reason) = if successful_exit && probe.recognized {
                    (ObservationHealth::Fresh, probe.status, None)
                } else if !successful_exit {
                    let stderr = observation_text(&out.stderr).unwrap_or_default();
                    let detail = if stderr.is_empty() {
                        format!("remote probe exit status {:?}", out.code)
                    } else {
                        format!("remote probe exit status {:?}: {stderr}", out.code)
                    };
                    (
                        ObservationHealth::ProbeError,
                        rec.status,
                        observation_text(&detail),
                    )
                } else {
                    (
                        ObservationHealth::ProbeError,
                        rec.status,
                        Some("remote probe returned an unrecognized execution state".into()),
                    )
                };
                persist_observation(
                    store,
                    &rec,
                    probe.source,
                    health,
                    next,
                    observation_text(&probe.raw_state),
                    reason.clone(),
                    out.code,
                )?;
                if health != ObservationHealth::Fresh {
                    errors.push(format!(
                        "{}: {}",
                        rec.run_id,
                        reason.as_deref().unwrap_or("remote probe failed")
                    ));
                    continue;
                }
                persist_remote_transition(store, &mut rec, next, &mut transitions)?;
            }
            Err(err) => {
                let reason = observation_text(&format!("{err:#}"));
                persist_observation(
                    store,
                    &rec,
                    ObservationSource::Transport,
                    ObservationHealth::Unreachable,
                    rec.status,
                    None,
                    reason,
                    None,
                )?;
                errors.push(format!("{}: {err}", rec.run_id));
                pool.drop_host(&rec.host).await;
            }
        }
    }
    Ok(TickReport {
        runs: store.list()?,
        transitions,
        errors,
    })
}

pub async fn tick(store: &RunStore, pool: &HostPool) -> Result<TickReport> {
    tick_selected(store, pool, |_| true).await
}

pub async fn probe_run(store: &RunStore, pool: &HostPool, run_id: &str) -> Result<TickReport> {
    if store.get(run_id)?.is_none() {
        anyhow::bail!("unknown run {run_id}");
    }
    tick_selected(store, pool, |record| record.run_id == run_id).await
}

pub async fn tick_due(
    store: &RunStore,
    pool: &HostPool,
    base_interval: Duration,
) -> Result<TickReport> {
    let now = Utc::now();
    let observations = store
        .list_observations()?
        .into_iter()
        .map(|observation| {
            (
                (observation.run_id.clone(), observation.attempt_no),
                observation,
            )
        })
        .collect::<HashMap<_, _>>();
    let runs = store.list()?;
    let due = runs
        .iter()
        .filter(|rec| {
            !rec.status.is_terminal()
                && observation_is_due(
                    rec,
                    observations.get(&observation_key(rec)),
                    now,
                    base_interval,
                )
        })
        .map(|rec| rec.run_id.clone())
        .collect::<HashSet<_>>();

    let mut slurm_groups = HashMap::<String, Vec<RunRecord>>::new();
    let mut lsf_groups = HashMap::<String, Vec<RunRecord>>::new();
    for rec in runs.iter().filter(|rec| due.contains(&rec.run_id)) {
        let attempt = match rec.attempt_no {
            Some(attempt_no) => store.get_attempt(&rec.run_id, attempt_no)?,
            None => None,
        };
        if batchable_v2_slurm(rec, attempt.as_ref()) {
            slurm_groups
                .entry(rec.host.clone())
                .or_default()
                .push(rec.clone());
        } else if batchable_v2_lsf(rec, attempt.as_ref()) {
            lsf_groups
                .entry(rec.host.clone())
                .or_default()
                .push(rec.clone());
        }
    }

    let mut batch_transitions = Vec::new();
    let mut batch_errors = Vec::new();
    let mut handled = HashSet::new();
    for records in slurm_groups
        .into_values()
        .filter(|records| records.len() >= 2)
    {
        handled.extend(
            probe_slurm_batch(
                store,
                pool,
                records,
                &mut batch_transitions,
                &mut batch_errors,
            )
            .await?,
        );
    }
    for records in lsf_groups
        .into_values()
        .filter(|records| records.len() >= 2)
    {
        handled.extend(
            probe_lsf_batch(
                store,
                pool,
                records,
                &mut batch_transitions,
                &mut batch_errors,
            )
            .await?,
        );
    }

    let mut report = tick_selected(store, pool, |rec| {
        due.contains(&rec.run_id) && !handled.contains(&rec.run_id)
    })
    .await?;
    batch_transitions.append(&mut report.transitions);
    batch_errors.append(&mut report.errors);
    report.transitions = batch_transitions;
    report.errors = batch_errors;
    report.runs = store.list()?;
    Ok(report)
}

#[cfg(test)]
mod polling_tests {
    use super::*;
    use chrono::Duration as ChronoDuration;
    use runwatch_core::RunnerKind;

    fn run(status: RunStatus, runner: RunnerKind) -> RunRecord {
        let mut run = RunRecord::new("r-poll".into(), "cluster".into(), runner);
        run.status = status;
        run.attempt_no = Some(1);
        run
    }

    fn observation(
        now: chrono::DateTime<Utc>,
        health: ObservationHealth,
        status: RunStatus,
    ) -> ObservationRecord {
        ObservationRecord {
            run_id: "r-poll".into(),
            attempt_no: 1,
            observed_at: now,
            source: ObservationSource::Scheduler,
            health,
            execution_status: status,
            raw_state: None,
            reason: None,
            command_exit_code: Some(0),
        }
    }

    #[test]
    fn slurm_batch_eligibility_requires_matching_durable_v2_attempt() {
        let mut rec = run(RunStatus::Running, RunnerKind::Slurm);
        rec.job_id = Some("123".into());
        rec.remote_terminal = Some("/shared/r/terminal.json".into());
        let now = Utc::now();
        let attempt = RunAttemptRecord {
            run_id: rec.run_id.clone(),
            attempt_no: 1,
            runner: RunnerKind::Slurm,
            host: rec.host.clone(),
            workdir: "/shared/r".into(),
            command: "true".into(),
            resources: Default::default(),
            job_name: "rw".into(),
            job_id: Some("123".into()),
            script_path: "/shared/r/a.sh".into(),
            stdout_path: "/shared/r/stdout".into(),
            stderr_path: "/shared/r/stderr".into(),
            terminal_path: "/shared/r/terminal.json".into(),
            receipt_path: "/shared/r/receipt".into(),
            status: RunStatus::Running,
            created_at: now,
            updated_at: now,
            error: None,
        };
        assert!(batchable_v2_slurm(&rec, Some(&attempt)));
        let mut mismatch = attempt.clone();
        mismatch.job_id = Some("999".into());
        assert!(!batchable_v2_slurm(&rec, Some(&mismatch)));
        assert!(!batchable_v2_slurm(&rec, None));
    }

    #[test]
    fn lsf_batch_eligibility_requires_matching_durable_v2_attempt() {
        let mut rec = run(RunStatus::Running, RunnerKind::Lsf);
        rec.job_id = Some("456".into());
        rec.remote_terminal = Some("/shared/r/terminal.json".into());
        let now = Utc::now();
        let attempt = RunAttemptRecord {
            run_id: rec.run_id.clone(),
            attempt_no: 1,
            runner: RunnerKind::Lsf,
            host: rec.host.clone(),
            workdir: "/shared/r".into(),
            command: "true".into(),
            resources: Default::default(),
            job_name: "rw".into(),
            job_id: Some("456".into()),
            script_path: "/shared/r/a.sh".into(),
            stdout_path: "/shared/r/stdout".into(),
            stderr_path: "/shared/r/stderr".into(),
            terminal_path: "/shared/r/terminal.json".into(),
            receipt_path: "/shared/r/receipt".into(),
            status: RunStatus::Running,
            created_at: now,
            updated_at: now,
            error: None,
        };
        assert!(batchable_v2_lsf(&rec, Some(&attempt)));
        let mut mismatch = attempt.clone();
        mismatch.terminal_path = "/other".into();
        assert!(!batchable_v2_lsf(&rec, Some(&mismatch)));
        assert!(!batchable_v2_lsf(&rec, None));
    }

    #[tokio::test]
    #[ignore = "submits and cancels two short real Slurm jobs on RUNWATCH_R2C_BATCH_HOST"]
    async fn real_slurm_batch_probe_acceptance() {
        let host = std::env::var("RUNWATCH_R2C_BATCH_HOST")
            .expect("set RUNWATCH_R2C_BATCH_HOST to an SSH alias with Slurm");
        let config = AppConfig::load_or_default().expect("load config");
        let pool = HostPool::from_ssh_config_with_timeout(
            Duration::from_secs(config.ssh.alive_interval.max(1)),
            Duration::from_secs(config.ssh.cmd_timeout_sec.max(1)),
        )
        .expect("host pool");
        let submit = pool
            .exec(
                &host,
                "sbatch --parsable --job-name=runwatch-batch-smoke --wrap='sleep 30'; sbatch --parsable --job-name=runwatch-batch-smoke --wrap='sleep 30'",
            )
            .await
            .expect("submit smoke jobs");
        assert_eq!(submit.code, Some(0), "submit stderr={}", submit.stderr);
        let jobs = submit
            .stdout
            .lines()
            .filter_map(|line| line.trim().split(';').next())
            .filter(|job| {
                !job.is_empty()
                    && job
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || matches!(byte, b'_' | b'.'))
            })
            .take(2)
            .map(str::to_string)
            .collect::<Vec<_>>();
        assert_eq!(
            jobs.len(),
            2,
            "unexpected sbatch output: {:?}",
            submit.stdout
        );

        let records = jobs
            .iter()
            .enumerate()
            .map(|(index, job)| {
                let mut rec = run(RunStatus::Queued, RunnerKind::Slurm);
                rec.run_id = format!("batch-real-{index}");
                rec.host = host.clone();
                rec.job_id = Some(job.clone());
                rec.remote_terminal = Some(format!(
                    "/tmp/runwatch-batch-smoke-{}-{index}.terminal.json",
                    std::process::id()
                ));
                rec
            })
            .collect::<Vec<_>>();
        let command = slurm_batch_command(&records).expect("batch command");
        let batch = pool.exec(&host, &command).await;
        let cleanup_command = format!("scancel {} 2>/dev/null || true", jobs.join(" "));
        let _ = pool.exec(&host, &cleanup_command).await;

        let batch = batch.expect("batch probe transport");
        assert_eq!(batch.code, Some(0), "batch stderr={}", batch.stderr);
        let results = interpret_slurm_batch(&records, &batch.stdout);
        assert_eq!(results.len(), 2);
        for result in results {
            assert!(
                result.probe.recognized,
                "unrecognized batch output: stdout={:?}, reason={:?}",
                batch.stdout, result.reason
            );
            assert!(matches!(
                result.probe.status,
                RunStatus::Queued | RunStatus::Running
            ));
        }
    }

    #[tokio::test]
    #[ignore = "submits and cancels two short real LSF jobs on RUNWATCH_R2C_LSF_BATCH_HOST"]
    async fn real_lsf_batch_probe_acceptance() {
        let host = std::env::var("RUNWATCH_R2C_LSF_BATCH_HOST")
            .expect("set RUNWATCH_R2C_LSF_BATCH_HOST to an SSH alias with IBM LSF");
        let config = AppConfig::load_or_default().expect("load config");
        let pool = HostPool::from_ssh_config_with_timeout(
            Duration::from_secs(config.ssh.alive_interval.max(1)),
            Duration::from_secs(config.ssh.cmd_timeout_sec.max(1)),
        )
        .expect("host pool");
        let submit = pool
            .exec(
                &host,
                "bsub -J runwatch-lsf-batch-smoke -o /dev/null -e /dev/null 'sleep 30'; bsub -J runwatch-lsf-batch-smoke -o /dev/null -e /dev/null 'sleep 30'",
            )
            .await
            .expect("submit LSF smoke jobs");
        assert_eq!(submit.code, Some(0), "submit stderr={}", submit.stderr);
        let jobs = submit
            .stdout
            .lines()
            .filter_map(|line| {
                let start = line.find("Job <")? + "Job <".len();
                let rest = &line[start..];
                let end = rest.find('>')?;
                let job = &rest[..end];
                (!job.is_empty() && job.bytes().all(|byte| byte.is_ascii_digit()))
                    .then(|| job.to_string())
            })
            .take(2)
            .collect::<Vec<_>>();
        assert_eq!(jobs.len(), 2, "unexpected bsub output: {:?}", submit.stdout);

        let records = jobs
            .iter()
            .enumerate()
            .map(|(index, job)| {
                let mut rec = run(RunStatus::Queued, RunnerKind::Lsf);
                rec.run_id = format!("lsf-batch-real-{index}");
                rec.host = host.clone();
                rec.job_id = Some(job.clone());
                rec.remote_terminal = Some(format!(
                    "/tmp/runwatch-lsf-batch-smoke-{}-{index}.terminal.json",
                    std::process::id()
                ));
                rec
            })
            .collect::<Vec<_>>();
        let command = lsf_batch_command(&records).expect("LSF batch command");
        let mut last_stdout = String::new();
        let mut last_stderr = String::new();
        let mut accepted = None;
        for _ in 0..10 {
            let batch = pool
                .exec(&host, &command)
                .await
                .expect("LSF batch transport");
            last_stdout = batch.stdout.clone();
            last_stderr = batch.stderr.clone();
            if batch.code == Some(0) {
                let results = interpret_lsf_batch(&records, &batch.stdout);
                if results.len() == 2 && results.iter().all(|result| result.probe.recognized) {
                    accepted = Some(results);
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let cleanup_command = format!("bkill {} 2>/dev/null || true", jobs.join(" "));
        let _ = pool.exec(&host, &cleanup_command).await;

        let results = accepted.unwrap_or_else(|| {
            panic!(
                "LSF batch never recognized both jobs: stdout={last_stdout:?} stderr={last_stderr:?}"
            )
        });
        for result in results {
            assert!(matches!(
                result.probe.status,
                RunStatus::Queued | RunStatus::Running
            ));
        }
    }

    #[test]
    fn adaptive_polling_is_conservative_for_queued_and_local_process_runs() {
        let base = Duration::from_secs(20);
        assert_eq!(
            adaptive_probe_interval(&run(RunStatus::Queued, RunnerKind::Slurm), None, base),
            base
        );
        assert_eq!(
            adaptive_probe_interval(&run(RunStatus::Running, RunnerKind::Process), None, base),
            base
        );
    }

    #[test]
    fn adaptive_polling_slows_remote_running_and_unreachable_runs_with_cap() {
        let base = Duration::from_secs(20);
        let running = run(RunStatus::Running, RunnerKind::Slurm);
        assert_eq!(
            adaptive_probe_interval(&running, None, base),
            Duration::from_secs(40)
        );
        let now = Utc::now();
        let unreachable = observation(now, ObservationHealth::Unreachable, RunStatus::Running);
        assert_eq!(
            adaptive_probe_interval(&running, Some(&unreachable), base),
            Duration::from_secs(80)
        );
        let large_base = Duration::from_secs(120);
        assert_eq!(
            adaptive_probe_interval(&running, Some(&unreachable), large_base),
            Duration::from_secs(300)
        );
    }

    #[test]
    fn due_decision_uses_observation_age_without_changing_explicit_tick() {
        let now = Utc::now();
        let run = run(RunStatus::Running, RunnerKind::Slurm);
        let recent = observation(
            now - ChronoDuration::seconds(39),
            ObservationHealth::Fresh,
            RunStatus::Running,
        );
        assert!(!observation_is_due(
            &run,
            Some(&recent),
            now,
            Duration::from_secs(20)
        ));
        let due = observation(
            now - ChronoDuration::seconds(40),
            ObservationHealth::Fresh,
            RunStatus::Running,
        );
        assert!(observation_is_due(
            &run,
            Some(&due),
            now,
            Duration::from_secs(20)
        ));
        assert!(observation_is_due(&run, None, now, Duration::from_secs(20)));
    }
}

pub async fn serve_loop<F>(
    paused: Arc<AtomicBool>,
    interval: Duration,
    mut on_report: F,
) -> Result<()>
where
    F: FnMut(TickReport) -> bool,
{
    let _lock = ServeLock::claim()?;
    let store = Arc::new(RunStore::open_default()?);
    let config = AppConfig::load_or_default()?;
    let pool = Arc::new(HostPool::from_ssh_config_with_timeout(
        Duration::from_secs(config.ssh.alive_interval.max(1)),
        Duration::from_secs(config.ssh.cmd_timeout_sec.max(1)),
    )?);
    let _ipc_server = ipc::spawn_local_server(store.clone(), pool.clone(), paused.clone());
    let orphan_recovery_after = tokio::time::Instant::now() + offline::ORPHAN_RECONNECT_GRACE;
    loop {
        let _ = live::write_beat();
        if !paused.load(Ordering::SeqCst) {
            let mut report = match tick_due(&store, &pool, interval).await {
                Ok(report) => report,
                Err(err) => TickReport {
                    runs: store.list().unwrap_or_default(),
                    transitions: Vec::new(),
                    errors: vec![err.to_string()],
                },
            };
            if tokio::time::Instant::now() >= orphan_recovery_after {
                if let Err(err) = offline::reconcile_orphans(&store) {
                    report
                        .errors
                        .push(format!("offline orphan reconciliation: {err:#}"));
                }
            }
            if let Err(err) = offline::dispatch_due(store.clone()) {
                report
                    .errors
                    .push(format!("offline continuation dispatch: {err:#}"));
            }
            if !on_report(report) {
                return Ok(());
            }
        }
        tokio::time::sleep(interval).await;
    }
}
