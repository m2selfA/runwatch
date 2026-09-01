use anyhow::{Context, Result, bail};
use base64::Engine;
use chrono::Utc;
use runwatch_core::{RunAttemptRecord, RunRecord, RunStatus, RunStore, RunnerKind, SubmitRunSpec};
use runwatch_ssh::HostPool;

const MAX_COMMAND_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone)]
struct SubmissionPlan {
    spec: SubmitRunSpec,
    attempt_no: u32,
    job_name: String,
    run_dir: String,
    script_path: String,
    stdout_path: String,
    stderr_path: String,
    terminal_path: String,
    receipt_path: String,
}

impl SubmissionPlan {
    fn build(spec: SubmitRunSpec) -> Result<Self> {
        validate_spec(&spec)?;
        let attempt_no = 1;
        let root = spec.workspace.cwd.trim_end_matches('/');
        let run_dir = format!("{root}/.runwatch/{}", spec.run_id);
        let job_name = scheduler_job_name(&spec.run_id, attempt_no);
        Ok(Self {
            script_path: format!("{run_dir}/attempt-{attempt_no}.sh"),
            stdout_path: format!("{run_dir}/stdout.log"),
            stderr_path: format!("{run_dir}/stderr.log"),
            terminal_path: format!("{run_dir}/terminal.json"),
            receipt_path: format!("{run_dir}/submission.receipt"),
            spec,
            attempt_no,
            job_name,
            run_dir,
        })
    }

    fn run_and_attempt(&self) -> (RunRecord, RunAttemptRecord) {
        let now = Utc::now();
        let mut run = RunRecord::new(
            self.spec.run_id.clone(),
            self.spec.workspace.host_alias.clone(),
            self.spec.runner,
        );
        run.name = self.spec.name.clone();
        run.status = RunStatus::Submitting;
        run.workspace = Some(self.spec.workspace.clone());
        run.attempt_no = Some(self.attempt_no);
        run.remote_terminal = Some(self.terminal_path.clone());
        if let Some(binding) = &self.spec.continuation {
            run.session_id = Some(binding.session_id.clone());
            run.agent = Some(binding.agent_kind.clone());
            run.project_root = Some(binding.project_root.clone());
        }
        run.updated_at = now;

        let attempt = RunAttemptRecord {
            run_id: self.spec.run_id.clone(),
            attempt_no: self.attempt_no,
            runner: self.spec.runner,
            host: self.spec.workspace.host_alias.clone(),
            workdir: self.spec.workspace.cwd.clone(),
            command: self.spec.command.clone(),
            resources: self.spec.resources.clone(),
            job_name: self.job_name.clone(),
            job_id: None,
            script_path: self.script_path.clone(),
            stdout_path: self.stdout_path.clone(),
            stderr_path: self.stderr_path.clone(),
            terminal_path: self.terminal_path.clone(),
            receipt_path: self.receipt_path.clone(),
            status: RunStatus::Submitting,
            created_at: now,
            updated_at: now,
            error: None,
        };
        (run, attempt)
    }

    fn wrapper_script(&self) -> String {
        let workdir = shell_quote(&self.spec.workspace.cwd);
        let terminal = shell_quote(&self.terminal_path);
        format!(
            "#!/usr/bin/env bash\n\
             set +e\n\
             cd -- {workdir} || exit 111\n\
             (\n{}\n)\n\
             rc=$?\n\
             if [ \"$rc\" -eq 0 ]; then status=succeeded; else status=failed; fi\n\
             finished=$(date -u +'%Y-%m-%dT%H:%M:%SZ')\n\
             terminal={terminal}\n\
             tmp=\"${{terminal}}.tmp.$$\"\n\
             printf '{{\"schema_version\":1,\"run_id\":\"{}\",\"attempt_no\":{},\"status\":\"%s\",\"exit_code\":%d,\"finished_at\":\"%s\"}}\\n' \
               \"$status\" \"$rc\" \"$finished\" > \"$tmp\"\n\
             mv -f -- \"$tmp\" \"$terminal\"\n\
             exit \"$rc\"\n",
            self.spec.command, self.spec.run_id, self.attempt_no
        )
    }

    fn deploy_command(&self) -> String {
        let encoded = base64::engine::general_purpose::STANDARD.encode(self.wrapper_script());
        format!(
            "umask 077 && mkdir -p -- {} && printf '%s' {} | base64 -d > {} && chmod 700 -- {}",
            shell_quote(&self.run_dir),
            shell_quote(&encoded),
            shell_quote(&self.script_path),
            shell_quote(&self.script_path),
        )
    }

    fn scheduler_submit_command(&self) -> Result<String> {
        let receipt = shell_quote(&self.receipt_path);
        let submit = match self.spec.runner {
            RunnerKind::Slurm => self.slurm_submit_command()?,
            RunnerKind::Lsf => self.lsf_submit_command()?,
            other => bail!("Remote Execution v2 does not support runner {other:?} yet"),
        };
        let parse = match self.spec.runner {
            RunnerKind::Slurm => "job=${out%%;*}; job=${job%%$'\\n'*}",
            RunnerKind::Lsf => {
                "job=$(printf '%s\\n' \"$out\" | sed -n 's/.*Job <\\([0-9][0-9]*\\)>.*/\\1/p' | head -n 1)"
            }
            _ => unreachable!(),
        };
        Ok(format!(
            "receipt={receipt}; \
             if [ -s \"$receipt\" ]; then head -n 1 -- \"$receipt\"; exit 0; fi; \
             out=$({submit} 2>&1); rc=$?; \
             if [ \"$rc\" -ne 0 ]; then printf '%s\\n' \"$out\" >&2; exit \"$rc\"; fi; \
             {parse}; \
             case \"$job\" in ''|*[!0-9_]* ) printf 'invalid scheduler job id: %s\\n' \"$out\" >&2; exit 65;; esac; \
             tmp=\"${{receipt}}.tmp.$$\"; printf '%s\\n' \"$job\" > \"$tmp\" && mv -f -- \"$tmp\" \"$receipt\"; \
             printf '%s\\n' \"$job\""
        ))
    }

    fn slurm_submit_command(&self) -> Result<String> {
        if self.spec.resources.queue.is_some() {
            bail!("Slurm submission uses resources.partition, not resources.queue");
        }
        let mut args = vec![
            "sbatch".to_string(),
            "--parsable".into(),
            "--job-name".into(),
            shell_quote(&self.job_name),
            "--chdir".into(),
            shell_quote(&self.spec.workspace.cwd),
            "--output".into(),
            shell_quote(&self.stdout_path),
            "--error".into(),
            shell_quote(&self.stderr_path),
        ];
        push_option(&mut args, "--time", self.spec.resources.time.as_deref());
        push_option(
            &mut args,
            "--partition",
            self.spec.resources.partition.as_deref(),
        );
        push_option(
            &mut args,
            "--account",
            self.spec.resources.account.as_deref(),
        );
        if let Some(cpus) = self.spec.resources.cpus {
            args.push(format!("--cpus-per-task={cpus}"));
        }
        if let Some(mem) = self.spec.resources.mem.as_deref() {
            args.push(format!("--mem={}", shell_quote(mem)));
        }
        if let Some(gpus) = self.spec.resources.gpus {
            args.push(format!("--gpus={gpus}"));
        }
        args.push(shell_quote(&self.script_path));
        Ok(args.join(" "))
    }

    fn lsf_submit_command(&self) -> Result<String> {
        if self.spec.resources.partition.is_some() {
            bail!("LSF submission uses resources.queue, not resources.partition");
        }
        let mut args = vec![
            "bsub".to_string(),
            "-J".into(),
            shell_quote(&self.job_name),
            "-oo".into(),
            shell_quote(&self.stdout_path),
            "-eo".into(),
            shell_quote(&self.stderr_path),
        ];
        push_option(&mut args, "-W", self.spec.resources.time.as_deref());
        push_option(&mut args, "-q", self.spec.resources.queue.as_deref());
        push_option(&mut args, "-P", self.spec.resources.account.as_deref());
        if let Some(cpus) = self.spec.resources.cpus {
            args.push("-n".into());
            args.push(cpus.to_string());
        }
        push_option(&mut args, "-M", self.spec.resources.mem.as_deref());
        if let Some(gpus) = self.spec.resources.gpus {
            args.push("-gpu".into());
            args.push(shell_quote(&format!("num={gpus}")));
        }
        Ok(format!(
            "{} < {}",
            args.join(" "),
            shell_quote(&self.script_path)
        ))
    }
}

fn validate_spec(spec: &SubmitRunSpec) -> Result<()> {
    if spec.run_id.is_empty()
        || spec.run_id.len() > 96
        || !spec
            .run_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    {
        bail!("run_id must be 1..96 ASCII letters/digits/._-");
    }
    if spec.command.trim().is_empty() || spec.command.len() > MAX_COMMAND_BYTES {
        bail!("command must be non-empty and at most {MAX_COMMAND_BYTES} bytes");
    }
    if spec.command.contains('\0') {
        bail!("command must not contain NUL");
    }
    if spec.workspace.host_alias.trim().is_empty()
        || spec.workspace.host_alias.contains(['\r', '\n', '\0'])
    {
        bail!("workspace.host_alias is invalid");
    }
    if !spec.workspace.cwd.starts_with('/')
        || spec.workspace.cwd == "/"
        || spec.workspace.cwd.contains(['\r', '\n', '\0'])
    {
        bail!("SSH workspace.cwd must be an absolute non-root POSIX path");
    }
    match spec.runner {
        RunnerKind::Slurm | RunnerKind::Lsf => {}
        other => bail!("Remote Execution v2 does not support runner {other:?} yet"),
    }
    if let Some(binding) = &spec.continuation {
        if binding.agent_kind != "pi" {
            bail!("Remote Execution v2 currently supports continuation.agent_kind=pi only");
        }
        if binding.session_id.trim().is_empty()
            || binding.project_root.contains(['\r', '\n', '\0'])
            || binding.workspace != spec.workspace
        {
            bail!("continuation binding is invalid or does not match the submitted workspace");
        }
        if binding
            .session_file
            .as_deref()
            .is_some_and(|value| value.contains(['\r', '\n', '\0']))
            || binding
                .origin_leaf_id
                .as_deref()
                .is_some_and(|value| value.contains(['\r', '\n', '\0']))
            || binding
                .adapter_path
                .as_deref()
                .is_some_and(|value| value.is_empty() || value.contains(['\r', '\n', '\0']))
        {
            bail!("continuation binding contains invalid multiline/NUL identity data");
        }
    }

    for value in [
        spec.resources.time.as_deref(),
        spec.resources.partition.as_deref(),
        spec.resources.queue.as_deref(),
        spec.resources.account.as_deref(),
        spec.resources.mem.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        if value.is_empty() || value.contains(['\r', '\n', '\0']) {
            bail!("resource values must be non-empty single-line strings");
        }
    }
    if matches!(spec.resources.cpus, Some(0)) || matches!(spec.resources.gpus, Some(0)) {
        bail!("resource counts must be positive");
    }
    Ok(())
}

fn scheduler_job_name(run_id: &str, attempt_no: u32) -> String {
    let compact: String = run_id
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .take(48)
        .collect();
    format!("rw-{compact}-a{attempt_no}")
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn push_option(args: &mut Vec<String>, flag: &str, value: Option<&str>) {
    if let Some(value) = value {
        args.push(flag.to_string());
        args.push(shell_quote(value));
    }
}

fn submission_identity_matches(existing: &RunAttemptRecord, desired: &RunAttemptRecord) -> bool {
    existing.attempt_no == desired.attempt_no
        && existing.runner == desired.runner
        && existing.host == desired.host
        && existing.workdir == desired.workdir
        && existing.command == desired.command
        && existing.resources == desired.resources
        && existing.job_name == desired.job_name
}

fn scheduler_job_id(stdout: &str) -> Result<String> {
    let job = stdout
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim();
    if job.is_empty()
        || !job
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'_' | b'.'))
    {
        bail!("invalid scheduler job id in submission output: {stdout:?}");
    }
    Ok(job.to_string())
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
    store.persist_run_attempt_event(&run, &attempt, "submission_failed")
}

pub async fn submit_remote_run(
    store: &RunStore,
    pool: &HostPool,
    spec: SubmitRunSpec,
) -> Result<RunRecord> {
    let plan = SubmissionPlan::build(spec)?;
    // Resolve before mutating durable state. A typo/nonexistent Host alias must not create a Run
    // that can never be submitted. All later side effects happen only after the intent is durable.
    pool.resolve(&plan.spec.workspace.host_alias)?;

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
            .context("submission run exists but could not be loaded")?;
        let attempt = store
            .get_attempt(&plan.spec.run_id, plan.attempt_no)?
            .context("submission run exists without attempt 1")?;
        if !submission_identity_matches(&attempt, &desired_attempt) {
            bail!(
                "run_id {} already exists with a different submission spec",
                plan.spec.run_id
            );
        }
        if attempt.job_id.is_some() || run.status != RunStatus::Submitting {
            return Ok(run);
        }
        (run, attempt)
    };

    let deploy = plan.deploy_command();
    let deploy_out = pool.exec(&plan.spec.workspace.host_alias, &deploy).await;
    match deploy_out {
        Ok(out) if out.code.unwrap_or(1) == 0 => {}
        Ok(out) => {
            let message = format!(
                "deploy wrapper failed with exit {:?}: {}{}",
                out.code, out.stdout, out.stderr
            );
            mark_submission_failed(store, run, attempt, message.clone())?;
            bail!(message);
        }
        Err(err) => {
            let message = format!("deploy wrapper failed: {err:#}");
            mark_submission_failed(store, run, attempt, message.clone())?;
            bail!(message);
        }
    }

    let submit = plan.scheduler_submit_command()?;
    let submit_out = pool.exec(&plan.spec.workspace.host_alias, &submit).await;
    let out = match submit_out {
        Ok(out) if out.code.unwrap_or(1) == 0 => out,
        Ok(out) => {
            let message = format!(
                "scheduler submit failed with exit {:?}: {}{}",
                out.code, out.stdout, out.stderr
            );
            mark_submission_failed(store, run, attempt, message.clone())?;
            bail!(message);
        }
        Err(err) => {
            // Keep Submitting on transport ambiguity. The remote receipt makes a retry idempotent
            // when the scheduler accepted the job but the local connection died before the reply.
            attempt.updated_at = Utc::now();
            attempt.error = Some(format!(
                "ambiguous scheduler submission transport failure: {err:#}"
            ));
            run.updated_at = attempt.updated_at;
            run.note = attempt.error.clone();
            store.persist_run_attempt_event(&run, &attempt, "submission_ambiguous")?;
            return Err(err).context("scheduler submit transport failed; retry the same run_id");
        }
    };

    let job_id = scheduler_job_id(&out.stdout)?;
    let now = Utc::now();
    run.job_id = Some(job_id.clone());
    run.status = RunStatus::Queued;
    run.updated_at = now;
    run.note = None;
    attempt.job_id = Some(job_id);
    attempt.status = RunStatus::Queued;
    attempt.updated_at = now;
    attempt.error = None;
    store.persist_run_attempt_event(&run, &attempt, "scheduler_submitted")?;
    Ok(run)
}

pub async fn submit_run(
    store: &RunStore,
    pool: &HostPool,
    spec: SubmitRunSpec,
) -> Result<RunRecord> {
    match spec.runner {
        RunnerKind::Process => crate::local_process::submit_local_run(store, spec),
        RunnerKind::Slurm | RunnerKind::Lsf => submit_remote_run(store, pool, spec).await,
        other => bail!("submit_run_v2 does not support runner {other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use runwatch_core::{RemoteWorkspaceRef, RunResources};

    fn spec(runner: RunnerKind) -> SubmitRunSpec {
        SubmitRunSpec {
            run_id: "run-test-01".into(),
            name: Some("test".into()),
            workspace: RemoteWorkspaceRef {
                host_alias: "cluster".into(),
                cwd: "/shared/project with spaces".into(),
            },
            runner,
            command: "python run.py --x 'hello'".into(),
            resources: RunResources {
                time: Some("04:00:00".into()),
                partition: if runner == RunnerKind::Slurm {
                    Some("gpu".into())
                } else {
                    None
                },
                queue: if runner == RunnerKind::Lsf {
                    Some("gpuq".into())
                } else {
                    None
                },
                account: Some("science".into()),
                cpus: Some(8),
                mem: Some("32G".into()),
                gpus: Some(2),
            },
            continuation: None,
        }
    }

    #[test]
    fn slurm_plan_has_receipt_guard_and_scheduler_outputs() {
        let plan = SubmissionPlan::build(spec(RunnerKind::Slurm)).unwrap();
        let cmd = plan.scheduler_submit_command().unwrap();
        assert!(cmd.contains("[ -s \"$receipt\" ]"));
        assert!(cmd.contains("sbatch --parsable"));
        assert!(cmd.contains("--partition 'gpu'"));
        assert!(cmd.contains("--gpus=2"));
        assert!(cmd.contains("'/shared/project with spaces/.runwatch/run-test-01/stdout.log'"));
    }

    #[test]
    fn lsf_plan_has_receipt_guard_and_resource_mapping() {
        let plan = SubmissionPlan::build(spec(RunnerKind::Lsf)).unwrap();
        let cmd = plan.scheduler_submit_command().unwrap();
        assert!(cmd.contains("bsub -J"));
        assert!(cmd.contains("-q 'gpuq'"));
        assert!(cmd.contains("-gpu 'num=2'"));
        assert!(cmd.contains("sed -n"));
    }

    #[test]
    fn wrapper_writes_atomic_structured_terminal() {
        let plan = SubmissionPlan::build(spec(RunnerKind::Slurm)).unwrap();
        let script = plan.wrapper_script();
        assert!(script.contains("schema_version"));
        assert!(script.contains("attempt_no"));
        assert!(script.contains("mv -f -- \"$tmp\" \"$terminal\""));
        assert!(script.contains("(\npython run.py"));
    }

    #[test]
    fn submission_spec_rejects_unsafe_identity_and_wrong_resource_family() {
        let mut bad = spec(RunnerKind::Slurm);
        bad.run_id = "bad;id".into();
        assert!(SubmissionPlan::build(bad).is_err());

        let mut wrong = spec(RunnerKind::Slurm);
        wrong.resources.queue = Some("lsf-only".into());
        assert!(
            SubmissionPlan::build(wrong)
                .unwrap()
                .scheduler_submit_command()
                .is_err()
        );
    }

    #[test]
    fn scheduler_job_id_is_strict() {
        assert_eq!(scheduler_job_id("12345\n").unwrap(), "12345");
        assert_eq!(scheduler_job_id("12345_7\n").unwrap(), "12345_7");
        assert!(scheduler_job_id("Submitted batch job 12345\n").is_err());
    }
}
