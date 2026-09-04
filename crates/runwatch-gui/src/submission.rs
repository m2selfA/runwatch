use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use runwatch_core::{RemoteWorkspaceRef, RetryRunSpec, RunResources, RunnerKind, SubmitRunSpec};
use std::sync::atomic::{AtomicU32, Ordering};

static MANUAL_NONCE: AtomicU32 = AtomicU32::new(1);
static RETRY_NONCE: AtomicU32 = AtomicU32::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManualRunner {
    Process,
    Slurm,
    Lsf,
}

impl ManualRunner {
    pub fn runner_kind(self) -> RunnerKind {
        match self {
            Self::Process => RunnerKind::Process,
            Self::Slurm => RunnerKind::Slurm,
            Self::Lsf => RunnerKind::Lsf,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Process => "Local Process",
            Self::Slurm => "Slurm",
            Self::Lsf => "LSF",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ManualRunDraft {
    pub name: String,
    pub host_alias: String,
    pub cwd: String,
    pub command: String,
    pub time: String,
    pub pool: String,
    pub account: String,
    pub cpus: String,
    pub mem: String,
    pub gpus: String,
}

impl ManualRunDraft {
    pub fn build_spec(&self, runner: ManualRunner) -> Result<SubmitRunSpec> {
        let now = Utc::now();
        let salt = MANUAL_NONCE.fetch_add(1, Ordering::Relaxed)
            ^ std::process::id()
            ^ now.timestamp_subsec_nanos();
        self.build_spec_at(runner, now, salt)
    }

    fn build_spec_at(
        &self,
        runner: ManualRunner,
        now: DateTime<Utc>,
        salt: u32,
    ) -> Result<SubmitRunSpec> {
        let command = self.command.trim();
        if command.is_empty() {
            bail!("Command is required");
        }
        let cwd = self.cwd.trim();
        if cwd.is_empty() {
            bail!("Workspace is required");
        }
        let name = manual_name(&self.name, command, runner, now)?;
        let run_id = manual_run_id(&name, now, salt);

        let (workspace, resources) = match runner {
            ManualRunner::Process => (
                RemoteWorkspaceRef {
                    host_alias: "local".into(),
                    cwd: cwd.to_string(),
                },
                RunResources::default(),
            ),
            ManualRunner::Slurm | ManualRunner::Lsf => {
                let host_alias = self.host_alias.trim();
                if host_alias.is_empty() {
                    bail!("SSH Host alias is required for remote Runs");
                }
                let pool = optional(&self.pool);
                let resources = RunResources {
                    time: optional(&self.time),
                    partition: (runner == ManualRunner::Slurm)
                        .then_some(pool.clone())
                        .flatten(),
                    queue: (runner == ManualRunner::Lsf).then_some(pool).flatten(),
                    account: optional(&self.account),
                    cpus: positive_u32("CPU count", &self.cpus)?,
                    mem: optional(&self.mem),
                    gpus: positive_u32("GPU count", &self.gpus)?,
                };
                (
                    RemoteWorkspaceRef {
                        host_alias: host_alias.to_string(),
                        cwd: cwd.to_string(),
                    },
                    resources,
                )
            }
        };

        Ok(SubmitRunSpec {
            run_id,
            name: Some(name),
            workspace,
            runner: runner.runner_kind(),
            command: command.to_string(),
            resources,
            continuation: None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryDraft {
    pub run_id: String,
    pub expected_attempt_no: u32,
    pub request_id: String,
    pub time: String,
    pub pool: String,
    pub account: String,
    pub cpus: String,
    pub mem: String,
    pub gpus: String,
}

impl RetryDraft {
    pub fn build_spec(&self, runner: RunnerKind) -> Result<RetryRunSpec> {
        if self.run_id.trim().is_empty() || self.expected_attempt_no == 0 {
            bail!("Retry requires a current durable Attempt");
        }
        if self.request_id.trim().is_empty() {
            bail!("Retry request identity is missing; reopen the Retry review");
        }
        let resources = match runner {
            RunnerKind::Process => None,
            RunnerKind::Slurm | RunnerKind::Lsf => {
                let pool = optional(&self.pool);
                Some(RunResources {
                    time: optional(&self.time),
                    partition: (runner == RunnerKind::Slurm)
                        .then_some(pool.clone())
                        .flatten(),
                    queue: (runner == RunnerKind::Lsf).then_some(pool).flatten(),
                    account: optional(&self.account),
                    cpus: positive_u32("CPU count", &self.cpus)?,
                    mem: optional(&self.mem),
                    gpus: positive_u32("GPU count", &self.gpus)?,
                })
            }
            other => bail!("Retry review does not support runner {other:?}"),
        };
        Ok(RetryRunSpec {
            run_id: self.run_id.clone(),
            expected_attempt_no: self.expected_attempt_no,
            request_id: self.request_id.clone(),
            resources,
        })
    }
}

pub fn new_retry_request_id() -> String {
    let now = Utc::now();
    let salt = RETRY_NONCE.fetch_add(1, Ordering::Relaxed)
        ^ std::process::id()
        ^ now.timestamp_subsec_nanos();
    format!(
        "gui-retry-{}-{:06x}",
        now.format("%Y%m%d-%H%M%S"),
        salt & 0x00ff_ffff
    )
}

fn optional(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn positive_u32(label: &str, value: &str) -> Result<Option<u32>> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    let parsed = value
        .parse::<u32>()
        .map_err(|_| anyhow::anyhow!("{label} must be a positive integer"))?;
    if parsed == 0 {
        bail!("{label} must be greater than zero");
    }
    Ok(Some(parsed))
}

fn manual_name(
    requested: &str,
    command: &str,
    runner: ManualRunner,
    now: DateTime<Utc>,
) -> Result<String> {
    let requested = requested.trim();
    if !requested.is_empty() {
        if requested.chars().count() > 120 {
            bail!("Name must be at most 120 characters");
        }
        if requested.contains(['\r', '\n', '\0']) {
            bail!("Name must be a single line");
        }
        return Ok(requested.to_string());
    }

    let mut words = command.split_whitespace().take(2);
    let first = words.next().unwrap_or("run");
    let second = words.next();
    let summary = match second {
        Some(second) => format!("{first} {second}"),
        None => first.to_string(),
    };
    let summary: String = summary.chars().take(42).collect();
    Ok(format!(
        "{} · {} · {}",
        summary,
        runner.label(),
        now.format("%H:%M")
    ))
}

fn manual_run_id(name: &str, now: DateTime<Utc>, salt: u32) -> String {
    let mut stem = String::new();
    let mut previous_dash = false;
    for ch in name.chars() {
        let normalized = if ch.is_ascii_alphanumeric() {
            ch.to_ascii_lowercase()
        } else {
            '-'
        };
        if normalized == '-' {
            if previous_dash || stem.is_empty() {
                continue;
            }
            previous_dash = true;
        } else {
            previous_dash = false;
        }
        stem.push(normalized);
        if stem.len() >= 24 {
            break;
        }
    }
    while stem.ends_with('-') {
        stem.pop();
    }
    if stem.is_empty() {
        stem.push_str("run");
    }
    format!(
        "manual-{stem}-{}-{:06x}",
        now.format("%Y%m%d-%H%M%S"),
        salt & 0x00ff_ffff
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 4, 16, 30, 45)
            .single()
            .unwrap()
    }

    #[test]
    fn process_submission_is_unbound_and_discards_scheduler_resources() {
        let draft = ManualRunDraft {
            cwd: r"E:\science\run".into(),
            command: "python analysis.py --fast".into(),
            time: "01:00:00".into(),
            pool: "gpu".into(),
            cpus: "8".into(),
            ..Default::default()
        };
        let spec = draft
            .build_spec_at(ManualRunner::Process, now(), 0x123456)
            .unwrap();
        assert_eq!(spec.runner, RunnerKind::Process);
        assert_eq!(spec.workspace.host_alias, "local");
        assert_eq!(spec.resources, RunResources::default());
        assert_eq!(spec.continuation, None);
        assert_eq!(
            spec.name.as_deref(),
            Some("python analysis.py · Local Process · 16:30")
        );
        assert!(spec.run_id.starts_with("manual-python-analysis-py-"));
        assert!(spec.run_id.ends_with("-20260904-163045-123456"));
    }

    #[test]
    fn slurm_and_lsf_map_the_shared_pool_field_to_the_right_resource_family() {
        let draft = ManualRunDraft {
            host_alias: "gm00".into(),
            cwd: "/share/home/shark/work".into(),
            command: "python train.py".into(),
            pool: "gpu".into(),
            cpus: "4".into(),
            gpus: "1".into(),
            ..Default::default()
        };
        let slurm = draft.build_spec_at(ManualRunner::Slurm, now(), 1).unwrap();
        assert_eq!(slurm.resources.partition.as_deref(), Some("gpu"));
        assert_eq!(slurm.resources.queue, None);
        assert_eq!(slurm.resources.cpus, Some(4));
        assert_eq!(slurm.resources.gpus, Some(1));

        let lsf = draft.build_spec_at(ManualRunner::Lsf, now(), 2).unwrap();
        assert_eq!(lsf.resources.partition, None);
        assert_eq!(lsf.resources.queue.as_deref(), Some("gpu"));
    }

    #[test]
    fn manual_preflight_rejects_missing_remote_identity_and_invalid_counts() {
        let draft = ManualRunDraft {
            cwd: "/shared/work".into(),
            command: "echo ok".into(),
            cpus: "0".into(),
            ..Default::default()
        };
        let error = draft
            .build_spec_at(ManualRunner::Slurm, now(), 1)
            .unwrap_err()
            .to_string();
        assert!(error.contains("Host alias"));

        let draft = ManualRunDraft {
            host_alias: "gm00".into(),
            cwd: "/shared/work".into(),
            command: "echo ok".into(),
            cpus: "zero".into(),
            ..Default::default()
        };
        let error = draft
            .build_spec_at(ManualRunner::Slurm, now(), 1)
            .unwrap_err()
            .to_string();
        assert!(error.contains("CPU count"));
    }

    #[test]
    fn retry_review_maps_resources_and_local_retry_has_no_scheduler_envelope() {
        let draft = RetryDraft {
            run_id: "manual-r1".into(),
            expected_attempt_no: 2,
            request_id: "gui-retry-fixed-01".into(),
            time: "04:00:00".into(),
            pool: "gpu".into(),
            account: "science".into(),
            cpus: "16".into(),
            mem: "64G".into(),
            gpus: "2".into(),
        };
        let slurm = draft.build_spec(RunnerKind::Slurm).unwrap();
        let resources = slurm.resources.unwrap();
        assert_eq!(resources.partition.as_deref(), Some("gpu"));
        assert_eq!(resources.queue, None);
        assert_eq!(resources.cpus, Some(16));
        assert_eq!(resources.gpus, Some(2));
        assert_eq!(resources.mem.as_deref(), Some("64G"));

        let lsf = draft.build_spec(RunnerKind::Lsf).unwrap();
        let resources = lsf.resources.unwrap();
        assert_eq!(resources.partition, None);
        assert_eq!(resources.queue.as_deref(), Some("gpu"));

        let local = draft.build_spec(RunnerKind::Process).unwrap();
        assert_eq!(local.resources, None);
        assert_eq!(local.request_id, "gui-retry-fixed-01");
        assert_eq!(local.expected_attempt_no, 2);
    }

    #[test]
    fn retry_review_rejects_invalid_counts_and_generates_wire_safe_request_ids() {
        let draft = RetryDraft {
            run_id: "manual-r1".into(),
            expected_attempt_no: 1,
            request_id: "gui-retry-fixed-02".into(),
            time: String::new(),
            pool: String::new(),
            account: String::new(),
            cpus: "0".into(),
            mem: String::new(),
            gpus: String::new(),
        };
        assert!(
            draft
                .build_spec(RunnerKind::Slurm)
                .unwrap_err()
                .to_string()
                .contains("greater than zero")
        );

        let id = new_retry_request_id();
        assert!(id.starts_with("gui-retry-"));
        assert!(id.len() <= 96);
        assert!(
            id.bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        );
    }

    #[test]
    fn generated_run_id_is_ascii_bounded_and_readable() {
        let id = manual_run_id("CryoEM refinement Ω / long long long name", now(), 0xabcdef);
        assert!(id.len() <= 96);
        assert!(
            id.bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        );
        assert!(id.starts_with("manual-cryoem-refinement-"));
        assert!(id.ends_with("-20260904-163045-abcdef"));
    }
}
