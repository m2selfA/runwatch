use crate::types::{ObservationSource, RunRecord, RunStatus, RunnerKind};

const SENTINEL_MARKER: &str = "__RUNWATCH_SENTINEL__";
const SCHEDULER_MARKER: &str = "__RUNWATCH_SCHEDULER__";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeInterpretation {
    pub status: RunStatus,
    pub source: ObservationSource,
    pub raw_state: String,
    pub recognized: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlurmBatchProbeResult {
    pub probe: ProbeInterpretation,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LsfBatchProbeResult {
    pub probe: ProbeInterpretation,
    pub reason: Option<String>,
}

const BATCH_TERMINAL: &str = "__RUNWATCH_BATCH_TERMINAL__";
const SLURM_BATCH_SQUEUE_BEGIN: &str = "__RUNWATCH_BATCH_SQUEUE_BEGIN__";
const SLURM_BATCH_SQUEUE_END: &str = "__RUNWATCH_BATCH_SQUEUE_END__";
const SLURM_BATCH_SACCT_BEGIN: &str = "__RUNWATCH_BATCH_SACCT_BEGIN__";
const SLURM_BATCH_SACCT_END: &str = "__RUNWATCH_BATCH_SACCT_END__";

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

pub fn terminal_cmd(path: &str) -> String {
    let path = shell_quote(path);
    format!(
        "if [ -f {path} ]; then printf '{SENTINEL_MARKER}\\n'; cat -- {path}; else printf '{SCHEDULER_MARKER}\\n'; fi"
    )
}

pub fn slurm_cmd(job: &str) -> String {
    let job = shell_quote(job);
    format!(
        "state=$(squeue -h -j {job} -o '%T' 2>/dev/null | head -n 1); \
         if [ -n \"$state\" ]; then printf '%s\\n' \"$state\"; \
         else sacct -j {job} -X -n -P -o State 2>/dev/null | head -n 1 | cut -d'|' -f1; fi"
    )
}

pub fn slurm_batch_command(records: &[RunRecord]) -> Option<String> {
    if records.len() < 2
        || records.iter().any(|record| {
            record.runner != RunnerKind::Slurm
                || record.job_id.as_deref().is_none_or(str::is_empty)
                || record.remote_terminal.as_deref().is_none_or(str::is_empty)
        })
    {
        return None;
    }

    let jobs = records
        .iter()
        .filter_map(|record| record.job_id.as_deref())
        .collect::<Vec<_>>()
        .join(",");
    let jobs = shell_quote(&jobs);
    let mut command = String::from("set +e; ");
    for (index, record) in records.iter().enumerate() {
        let terminal = shell_quote(record.remote_terminal.as_deref()?);
        command.push_str(&format!(
            "if [ -f {terminal} ]; then printf '{BATCH_TERMINAL}|{index}|'; head -n 1 -- {terminal}; fi; "
        ));
    }
    command.push_str(&format!(
        "printf '{SLURM_BATCH_SQUEUE_BEGIN}\\n'; \
         squeue -h -j {jobs} -o '%i|%T' 2>/dev/null; rw_sq=$?; \
         printf '{SLURM_BATCH_SQUEUE_END}|%s\\n' \"$rw_sq\"; \
         printf '{SLURM_BATCH_SACCT_BEGIN}\\n'; \
         sacct -j {jobs} -X -n -P -o JobIDRaw,State 2>/dev/null; rw_sa=$?; \
         printf '{SLURM_BATCH_SACCT_END}|%s\\n' \"$rw_sa\"; exit 0"
    ));
    Some(command)
}

pub fn interpret_slurm_batch(records: &[RunRecord], stdout: &str) -> Vec<SlurmBatchProbeResult> {
    use std::collections::HashMap;

    let mut terminals = HashMap::<usize, String>::new();
    let mut squeue = HashMap::<String, String>::new();
    let mut sacct = HashMap::<String, String>::new();
    let mut squeue_exit = None;
    let mut sacct_exit = None;
    let mut section = 0u8;

    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix(&format!("{BATCH_TERMINAL}|")) {
            if let Some((index, body)) = rest.split_once('|')
                && let Ok(index) = index.parse::<usize>()
            {
                terminals.insert(index, body.trim().to_string());
            }
            continue;
        }
        if line == SLURM_BATCH_SQUEUE_BEGIN {
            section = 1;
            continue;
        }
        if let Some(exit) = line.strip_prefix(&format!("{SLURM_BATCH_SQUEUE_END}|")) {
            squeue_exit = exit.trim().parse::<u32>().ok();
            section = 0;
            continue;
        }
        if line == SLURM_BATCH_SACCT_BEGIN {
            section = 2;
            continue;
        }
        if let Some(exit) = line.strip_prefix(&format!("{SLURM_BATCH_SACCT_END}|")) {
            sacct_exit = exit.trim().parse::<u32>().ok();
            section = 0;
            continue;
        }
        match section {
            1 => {
                if let Some((job, state)) = line.split_once('|') {
                    let job = job.trim();
                    let state = state.trim();
                    if !job.is_empty() && !state.is_empty() {
                        squeue
                            .entry(job.to_string())
                            .or_insert_with(|| state.to_string());
                    }
                }
            }
            2 => {
                let mut fields = line.split('|');
                let job = fields.next().unwrap_or_default().trim();
                let state = fields.next().unwrap_or_default().trim();
                if !job.is_empty() && !state.is_empty() {
                    sacct
                        .entry(job.to_string())
                        .or_insert_with(|| state.to_string());
                }
            }
            _ => {}
        }
    }

    records
        .iter()
        .enumerate()
        .map(|(index, record)| {
            if let Some(body) = terminals.get(&index) {
                let mapped = parse_terminal(body);
                return SlurmBatchProbeResult {
                    probe: ProbeInterpretation {
                        status: mapped.unwrap_or(record.status),
                        source: ObservationSource::Sentinel,
                        raw_state: body.clone(),
                        recognized: mapped.is_some(),
                    },
                    reason: mapped
                        .is_none()
                        .then(|| "batch sentinel was not recognized".into()),
                };
            }

            let job_id = record.job_id.as_deref().unwrap_or_default();
            let state = squeue.get(job_id).or_else(|| sacct.get(job_id));
            let mapped = state.map(|state| map_slurm(state));
            let recognized = mapped.is_some_and(|status| status != RunStatus::Unknown);
            let reason = if recognized {
                None
            } else if squeue_exit.is_some_and(|code| code != 0)
                || sacct_exit.is_some_and(|code| code != 0)
            {
                Some(format!(
                    "batched Slurm probe failed: squeue={:?} sacct={:?}",
                    squeue_exit, sacct_exit
                ))
            } else {
                Some("batched Slurm probe returned no recognized state".into())
            };
            SlurmBatchProbeResult {
                probe: ProbeInterpretation {
                    status: mapped
                        .filter(|status| *status != RunStatus::Unknown)
                        .unwrap_or(record.status),
                    source: ObservationSource::Scheduler,
                    raw_state: state.cloned().unwrap_or_default(),
                    recognized,
                },
                reason,
            }
        })
        .collect()
}

pub fn lsf_cmd(job: &str) -> String {
    let job = shell_quote(job);
    format!(
        "state=$(bjobs -a -noheader -o stat {job} 2>/dev/null | head -n 1 | awk '{{print $1}}'); \
         if [ -n \"$state\" ]; then printf '%s\\n' \"$state\"; \
         else event=$(bhist -l {job} 2>/dev/null | grep -E 'Done successfully|Post job process done successfully|Post job process failed|Exited|Completed <exit>' | tail -n 1); \
         case \"$event\" in \
           *'Post job process failed'*|*'Exited'*|*'Completed <exit>'*) printf 'EXIT\\n';; \
           *'Post job process done successfully'*|*'Done successfully'*) printf 'DONE\\n';; \
         esac; fi"
    )
}

const LSF_BATCH_BJOBS_BEGIN: &str = "__RUNWATCH_BATCH_BJOBS_BEGIN__";
const LSF_BATCH_BJOBS_END: &str = "__RUNWATCH_BATCH_BJOBS_END__";

pub fn lsf_batch_command(records: &[RunRecord]) -> Option<String> {
    if records.len() < 2
        || records.iter().any(|record| {
            record.runner != RunnerKind::Lsf
                || record.job_id.as_deref().is_none_or(str::is_empty)
                || record.remote_terminal.as_deref().is_none_or(str::is_empty)
        })
    {
        return None;
    }

    let jobs = records
        .iter()
        .filter_map(|record| record.job_id.as_deref())
        .map(shell_quote)
        .collect::<Vec<_>>()
        .join(" ");
    let mut command = String::from("set +e; ");
    for (index, record) in records.iter().enumerate() {
        let terminal = shell_quote(record.remote_terminal.as_deref()?);
        command.push_str(&format!(
            "if [ -f {terminal} ]; then printf '{BATCH_TERMINAL}|{index}|'; head -n 1 -- {terminal}; fi; "
        ));
    }
    command.push_str(&format!(
        "printf '{LSF_BATCH_BJOBS_BEGIN}\\n'; \
         bjobs -a -noheader -o 'jobid stat' {jobs} 2>/dev/null; rw_bj=$?; \
         printf '{LSF_BATCH_BJOBS_END}|%s\\n' \"$rw_bj\"; exit 0"
    ));
    Some(command)
}

pub fn interpret_lsf_batch(records: &[RunRecord], stdout: &str) -> Vec<LsfBatchProbeResult> {
    use std::collections::HashMap;

    let mut terminals = HashMap::<usize, String>::new();
    let mut states = HashMap::<String, String>::new();
    let mut bjobs_exit = None;
    let mut in_bjobs = false;

    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix(&format!("{BATCH_TERMINAL}|")) {
            if let Some((index, body)) = rest.split_once('|')
                && let Ok(index) = index.parse::<usize>()
            {
                terminals.insert(index, body.trim().to_string());
            }
            continue;
        }
        if line == LSF_BATCH_BJOBS_BEGIN {
            in_bjobs = true;
            continue;
        }
        if let Some(exit) = line.strip_prefix(&format!("{LSF_BATCH_BJOBS_END}|")) {
            bjobs_exit = exit.trim().parse::<u32>().ok();
            in_bjobs = false;
            continue;
        }
        if in_bjobs {
            let mut fields = line.split_whitespace();
            let job = fields.next().unwrap_or_default().trim();
            let state = fields.next().unwrap_or_default().trim();
            if !job.is_empty() && !state.is_empty() {
                states
                    .entry(job.to_string())
                    .or_insert_with(|| state.to_string());
            }
        }
    }

    records
        .iter()
        .enumerate()
        .map(|(index, record)| {
            if let Some(body) = terminals.get(&index) {
                let mapped = parse_terminal(body);
                return LsfBatchProbeResult {
                    probe: ProbeInterpretation {
                        status: mapped.unwrap_or(record.status),
                        source: ObservationSource::Sentinel,
                        raw_state: body.clone(),
                        recognized: mapped.is_some(),
                    },
                    reason: mapped
                        .is_none()
                        .then(|| "batch sentinel was not recognized".into()),
                };
            }
            let job_id = record.job_id.as_deref().unwrap_or_default();
            let state = states.get(job_id);
            let mapped = state.map(|state| map_lsf(state));
            let recognized = mapped.is_some_and(|status| status != RunStatus::Unknown);
            LsfBatchProbeResult {
                probe: ProbeInterpretation {
                    status: mapped
                        .filter(|status| *status != RunStatus::Unknown)
                        .unwrap_or(record.status),
                    source: ObservationSource::Scheduler,
                    raw_state: state.cloned().unwrap_or_default(),
                    recognized,
                },
                reason: (!recognized).then(|| {
                    if bjobs_exit.is_some_and(|code| code != 0) {
                        format!(
                            "batched LSF bjobs did not return this job (exit {:?}); use per-run bhist fallback",
                            bjobs_exit
                        )
                    } else {
                        "batched LSF bjobs did not return a recognized state; use per-run bhist fallback"
                            .into()
                    }
                }),
            }
        })
        .collect()
}

fn scheduler_cmd(rec: &RunRecord) -> Option<String> {
    match (rec.runner, rec.job_id.as_deref()) {
        (RunnerKind::Slurm, Some(job)) => Some(slurm_cmd(job)),
        (RunnerKind::Lsf, Some(job)) => Some(lsf_cmd(job)),
        _ => None,
    }
}

pub fn parse_terminal(stdout: &str) -> Option<RunStatus> {
    let trimmed = stdout.trim();
    if trimmed.starts_with('{') {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
            if let Some(status) = value.get("status").and_then(serde_json::Value::as_str) {
                return parse_terminal(status);
            }
        }
    }

    match trimmed
        .lines()
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "succeeded" | "completed" => Some(RunStatus::Succeeded),
        "failed" | "timed_out" | "timeout" | "lost" => Some(RunStatus::Failed),
        "cancelled" | "canceled" => Some(RunStatus::Cancelled),
        _ => None,
    }
}

pub fn map_slurm(state: &str) -> RunStatus {
    let s = state.trim().to_ascii_uppercase();
    if s.starts_with("COMPLETED") {
        RunStatus::Succeeded
    } else if [
        "FAILED",
        "TIMEOUT",
        "NODE_FAIL",
        "OUT_OF_MEMORY",
        "PREEMPTED",
        "BOOT_FAIL",
        "DEADLINE",
        "REVOKED",
        "SPECIAL_EXIT",
    ]
    .iter()
    .any(|prefix| s.starts_with(prefix))
    {
        RunStatus::Failed
    } else if s.starts_with("CANCELLED") || s.starts_with("CANCELED") {
        RunStatus::Cancelled
    } else if ["RUNNING", "COMPLETING", "SUSPENDED", "RESIZING"]
        .iter()
        .any(|prefix| s.starts_with(prefix))
    {
        RunStatus::Running
    } else if [
        "PENDING",
        "CONFIGURING",
        "REQUEUED",
        "REQUEUE_FED",
        "REQUEUE_HOLD",
    ]
    .iter()
    .any(|prefix| s.starts_with(prefix))
    {
        RunStatus::Queued
    } else {
        RunStatus::Unknown
    }
}

pub fn map_lsf(state: &str) -> RunStatus {
    match state.trim().to_ascii_uppercase().as_str() {
        "DONE" | "POST_DONE" => RunStatus::Succeeded,
        "EXIT" | "POST_ERR" => RunStatus::Failed,
        "PEND" | "WAIT" => RunStatus::Queued,
        "RUN" | "PSUSP" | "USUSP" | "SSUSP" => RunStatus::Running,
        _ => RunStatus::Unknown,
    }
}

fn mapped_scheduler_status(rec: &RunRecord, stdout: &str) -> Option<RunStatus> {
    let mapped = match rec.runner {
        RunnerKind::Slurm => map_slurm(stdout.trim()),
        RunnerKind::Lsf => map_lsf(stdout.trim()),
        _ => RunStatus::Unknown,
    };
    (mapped != RunStatus::Unknown).then_some(mapped)
}

pub fn interpret_observation(rec: &RunRecord, stdout: &str) -> ProbeInterpretation {
    if let Some(body) = stdout.strip_prefix(SENTINEL_MARKER) {
        let raw_state = body.trim().to_string();
        let mapped = parse_terminal(body);
        return ProbeInterpretation {
            status: mapped.unwrap_or(rec.status),
            source: ObservationSource::Sentinel,
            raw_state,
            recognized: mapped.is_some(),
        };
    }
    if let Some(body) = stdout.strip_prefix(SCHEDULER_MARKER) {
        let raw_state = body.trim().to_string();
        let mapped = mapped_scheduler_status(rec, body);
        return ProbeInterpretation {
            status: mapped.unwrap_or(rec.status),
            source: ObservationSource::Scheduler,
            raw_state,
            recognized: mapped.is_some(),
        };
    }

    // Compatibility with pre-R2 probe output while old clients/binaries may still exist.
    if rec.remote_terminal.is_some()
        && let Some(status) = parse_terminal(stdout)
    {
        return ProbeInterpretation {
            status,
            source: ObservationSource::Compatibility,
            raw_state: stdout.trim().to_string(),
            recognized: true,
        };
    }
    let mapped = mapped_scheduler_status(rec, stdout);
    ProbeInterpretation {
        status: mapped.unwrap_or(rec.status),
        source: ObservationSource::Compatibility,
        raw_state: stdout.trim().to_string(),
        recognized: mapped.is_some(),
    }
}

pub fn interpret(rec: &RunRecord, stdout: &str) -> RunStatus {
    interpret_observation(rec, stdout).status
}

pub fn remote_command(rec: &RunRecord) -> Option<String> {
    let scheduler = scheduler_cmd(rec);
    match (&rec.remote_terminal, scheduler) {
        (Some(path), Some(scheduler)) => {
            let path = shell_quote(path);
            Some(format!(
                "if [ -f {path} ]; then printf '{SENTINEL_MARKER}\\n'; cat -- {path}; \
                 else printf '{SCHEDULER_MARKER}\\n'; {scheduler}; fi"
            ))
        }
        (Some(path), None) => Some(terminal_cmd(path)),
        (None, Some(scheduler)) => Some(format!("printf '{SCHEDULER_MARKER}\\n'; {scheduler}")),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(runner: RunnerKind) -> RunRecord {
        let mut rec = RunRecord::new("r1".into(), "host".into(), runner);
        rec.job_id = Some("123".into());
        rec
    }

    #[test]
    fn slurm_states_cover_hpc_failures() {
        assert_eq!(map_slurm("COMPLETED"), RunStatus::Succeeded);
        assert_eq!(map_slurm("OUT_OF_MEMORY"), RunStatus::Failed);
        assert_eq!(map_slurm("PREEMPTED"), RunStatus::Failed);
        assert_eq!(map_slurm("NODE_FAIL"), RunStatus::Failed);
        assert_eq!(map_slurm("PENDING"), RunStatus::Queued);
        assert_eq!(map_slurm("SUSPENDED"), RunStatus::Running);
    }

    #[test]
    fn lsf_states_include_post_processing() {
        assert_eq!(map_lsf("DONE"), RunStatus::Succeeded);
        assert_eq!(map_lsf("POST_DONE"), RunStatus::Succeeded);
        assert_eq!(map_lsf("POST_ERR"), RunStatus::Failed);
        assert_eq!(map_lsf("WAIT"), RunStatus::Queued);
    }

    #[test]
    fn lsf_probe_falls_back_to_bhist_for_aged_out_finished_jobs() {
        let command = lsf_cmd("123");
        assert!(command.contains("bjobs -a"));
        assert!(command.contains("bhist -l '123'"));
        assert!(command.contains("Done successfully"));
        assert!(command.contains("Completed <exit>"));
        assert!(command.contains("printf 'DONE\\n'"));
        assert!(command.contains("printf 'EXIT\\n'"));
    }

    #[test]
    fn terminal_parser_is_exact_and_supports_json() {
        assert_eq!(parse_terminal("succeeded 0\n"), Some(RunStatus::Succeeded));
        assert_eq!(
            parse_terminal(r#"{"status":"failed","exit_code":2}"#),
            Some(RunStatus::Failed)
        );
        assert_eq!(parse_terminal("log says not-failed-yet"), None);
    }

    #[test]
    fn sentinel_missing_falls_back_to_scheduler_in_same_probe() {
        let mut rec = record(RunnerKind::Slurm);
        rec.remote_terminal = Some("/shared/a b/terminal.json".into());
        let command = remote_command(&rec).expect("probe command");
        assert!(command.contains(SENTINEL_MARKER));
        assert!(command.contains(SCHEDULER_MARKER));
        assert!(command.contains("squeue"));
        assert!(command.contains("sacct"));
        assert!(command.contains("'/shared/a b/terminal.json'"));
    }

    #[test]
    fn unrecognized_scheduler_output_preserves_last_trusted_state() {
        let mut rec = record(RunnerKind::Slurm);
        rec.status = RunStatus::Running;
        assert_eq!(
            interpret(&rec, "__RUNWATCH_SCHEDULER__\n"),
            RunStatus::Running
        );
        assert_eq!(
            interpret(&rec, "__RUNWATCH_SCHEDULER__\nFUTURE_STATE"),
            RunStatus::Running
        );
    }

    #[test]
    fn lsf_batch_command_uses_one_bjobs_query_and_terminal_fast_paths() {
        let mut a = record(RunnerKind::Lsf);
        a.run_id = "a".into();
        a.job_id = Some("201".into());
        a.remote_terminal = Some("/shared/a/terminal.json".into());
        let mut b = record(RunnerKind::Lsf);
        b.run_id = "b".into();
        b.job_id = Some("202".into());
        b.remote_terminal = Some("/shared/b/terminal.json".into());
        let command = lsf_batch_command(&[a, b]).expect("batch command");
        assert_eq!(command.matches("bjobs -a").count(), 1);
        assert_eq!(command.matches(BATCH_TERMINAL).count(), 2);
        assert!(command.contains("'201' '202'"));
    }

    #[test]
    fn lsf_batch_interpretation_handles_active_states_and_leaves_missing_for_history_fallback() {
        let mut a = record(RunnerKind::Lsf);
        a.status = RunStatus::Queued;
        a.job_id = Some("201".into());
        a.remote_terminal = Some("/a".into());
        let mut b = record(RunnerKind::Lsf);
        b.status = RunStatus::Running;
        b.job_id = Some("202".into());
        b.remote_terminal = Some("/b".into());
        let mut c = record(RunnerKind::Lsf);
        c.status = RunStatus::Running;
        c.job_id = Some("203".into());
        c.remote_terminal = Some("/c".into());
        let stdout = format!(
            "{BATCH_TERMINAL}|0|{{\"status\":\"succeeded\"}}\n\
             {LSF_BATCH_BJOBS_BEGIN}\n202 RUN\n{LSF_BATCH_BJOBS_END}|255\n"
        );
        let results = interpret_lsf_batch(&[a, b, c], &stdout);
        assert_eq!(results[0].probe.source, ObservationSource::Sentinel);
        assert_eq!(results[0].probe.status, RunStatus::Succeeded);
        assert!(results[0].probe.recognized);
        assert_eq!(results[1].probe.status, RunStatus::Running);
        assert!(results[1].probe.recognized);
        assert!(!results[2].probe.recognized);
        assert!(
            results[2]
                .reason
                .as_deref()
                .unwrap()
                .contains("bhist fallback")
        );
    }

    #[test]
    fn slurm_batch_command_uses_one_scheduler_query_pair_and_terminal_fast_paths() {
        let mut a = record(RunnerKind::Slurm);
        a.run_id = "a".into();
        a.job_id = Some("101".into());
        a.remote_terminal = Some("/shared/a/terminal.json".into());
        let mut b = record(RunnerKind::Slurm);
        b.run_id = "b".into();
        b.job_id = Some("102_7".into());
        b.remote_terminal = Some("/shared/b/terminal.json".into());
        let command = slurm_batch_command(&[a, b]).expect("batch command");
        assert_eq!(command.matches("squeue -h -j").count(), 1);
        assert_eq!(command.matches("sacct -j").count(), 1);
        assert_eq!(command.matches(BATCH_TERMINAL).count(), 2);
        assert!(command.contains("'101,102_7'"));
    }

    #[test]
    fn slurm_batch_interpretation_prefers_terminal_then_squeue_then_sacct() {
        let mut a = record(RunnerKind::Slurm);
        a.status = RunStatus::Running;
        a.job_id = Some("101".into());
        a.remote_terminal = Some("/a".into());
        let mut b = record(RunnerKind::Slurm);
        b.status = RunStatus::Queued;
        b.job_id = Some("102".into());
        b.remote_terminal = Some("/b".into());
        let mut c = record(RunnerKind::Slurm);
        c.status = RunStatus::Running;
        c.job_id = Some("103".into());
        c.remote_terminal = Some("/c".into());
        let stdout = format!(
            "{BATCH_TERMINAL}|0|{{\"status\":\"succeeded\"}}\n\
             {SLURM_BATCH_SQUEUE_BEGIN}\n102|RUNNING\n{SLURM_BATCH_SQUEUE_END}|0\n\
             {SLURM_BATCH_SACCT_BEGIN}\n101|FAILED|\n102|PENDING|\n103|TIMEOUT|\n{SLURM_BATCH_SACCT_END}|0\n"
        );
        let results = interpret_slurm_batch(&[a, b, c], &stdout);
        assert_eq!(results[0].probe.source, ObservationSource::Sentinel);
        assert_eq!(results[0].probe.status, RunStatus::Succeeded);
        assert_eq!(results[1].probe.status, RunStatus::Running);
        assert_eq!(results[1].probe.raw_state, "RUNNING");
        assert_eq!(results[2].probe.status, RunStatus::Failed);
        assert_eq!(results[2].probe.raw_state, "TIMEOUT");
        assert!(results.iter().all(|result| result.reason.is_none()));
    }

    #[test]
    fn slurm_batch_unknown_state_preserves_execution_and_reports_probe_error_reason() {
        let mut a = record(RunnerKind::Slurm);
        a.status = RunStatus::Running;
        a.job_id = Some("101".into());
        a.remote_terminal = Some("/a".into());
        let stdout = format!(
            "{SLURM_BATCH_SQUEUE_BEGIN}\n101|FUTURE_STATE\n{SLURM_BATCH_SQUEUE_END}|0\n\
             {SLURM_BATCH_SACCT_BEGIN}\n{SLURM_BATCH_SACCT_END}|0\n"
        );
        let result = interpret_slurm_batch(&[a], &stdout).remove(0);
        assert_eq!(result.probe.status, RunStatus::Running);
        assert!(!result.probe.recognized);
        assert_eq!(result.probe.raw_state, "FUTURE_STATE");
        assert!(result.reason.unwrap().contains("no recognized state"));
    }

    #[test]
    fn probe_interpretation_exposes_source_and_recognition_without_losing_state() {
        let mut rec = record(RunnerKind::Slurm);
        rec.status = RunStatus::Running;
        let scheduler = interpret_observation(&rec, "__RUNWATCH_SCHEDULER__\nCOMPLETED\n");
        assert_eq!(scheduler.source, ObservationSource::Scheduler);
        assert_eq!(scheduler.status, RunStatus::Succeeded);
        assert!(scheduler.recognized);
        assert_eq!(scheduler.raw_state, "COMPLETED");

        let unknown = interpret_observation(&rec, "__RUNWATCH_SCHEDULER__\nFUTURE_STATE\n");
        assert_eq!(unknown.source, ObservationSource::Scheduler);
        assert_eq!(unknown.status, RunStatus::Running);
        assert!(!unknown.recognized);
        assert_eq!(unknown.raw_state, "FUTURE_STATE");
    }

    #[test]
    fn shell_values_are_quoted_in_probe_commands() {
        let mut rec = record(RunnerKind::Slurm);
        rec.job_id = Some("123'; echo injected; '".into());
        rec.remote_terminal = Some("/tmp/x'; touch nope; '".into());
        let command = remote_command(&rec).expect("probe command");
        assert!(command.contains("'\"'\"'"));
        assert!(!command.contains("-j 123';"));
        assert!(!command.contains("-f /tmp/x';"));
    }
}
