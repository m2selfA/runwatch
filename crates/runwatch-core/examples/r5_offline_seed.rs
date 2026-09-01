use anyhow::{Context, Result, bail};
use chrono::Utc;
use runwatch_core::{
    ContinuationBinding, RemoteWorkspaceRef, RunAttemptRecord, RunRecord, RunResources, RunStatus,
    RunStore, RunnerKind,
};
use std::env;

fn arg(index: usize, name: &str) -> Result<String> {
    env::args()
        .nth(index)
        .filter(|value| !value.trim().is_empty())
        .with_context(|| format!("missing argument {name}"))
}

fn main() -> Result<()> {
    if env::var_os("RUNWATCH_DATA_DIR").is_none() {
        bail!("R5 acceptance seed requires RUNWATCH_DATA_DIR so it cannot touch the normal store");
    }

    let run_id = arg(1, "run_id")?;
    let session_id = arg(2, "session_id")?;
    let session_file = arg(3, "session_file")?;
    let origin_leaf_id = arg(4, "origin_leaf_id")?;
    let project_root = arg(5, "project_root")?;
    let adapter_path = arg(6, "adapter_path")?;

    let store = RunStore::open_default()?;
    if store.get(&run_id)?.is_some() {
        bail!("acceptance run_id {run_id} already exists in the isolated store");
    }

    let now = Utc::now();
    let workspace = RemoteWorkspaceRef {
        host_alias: "r5-acceptance".into(),
        cwd: "/runwatch/r5-acceptance".into(),
    };
    let binding = ContinuationBinding {
        agent_kind: "pi".into(),
        session_id: session_id.clone(),
        session_file: Some(session_file),
        origin_leaf_id: Some(origin_leaf_id),
        project_root,
        workspace: workspace.clone(),
        adapter_path: Some(adapter_path),
    };

    let mut run = RunRecord::new(
        run_id.clone(),
        workspace.host_alias.clone(),
        RunnerKind::Slurm,
    );
    run.name = Some("R5 offline continuation acceptance".into());
    run.status = RunStatus::Submitting;
    run.workspace = Some(workspace.clone());
    run.attempt_no = Some(1);
    run.session_id = Some(session_id);
    run.agent = Some("pi".into());
    run.project_root = Some(binding.project_root.clone());
    run.updated_at = now;

    let mut attempt = RunAttemptRecord {
        run_id: run_id.clone(),
        attempt_no: 1,
        runner: RunnerKind::Slurm,
        host: workspace.host_alias.clone(),
        workdir: workspace.cwd.clone(),
        command: "printf 'synthetic R5 acceptance only\\n'".into(),
        resources: RunResources::default(),
        job_name: format!("rw-{run_id}-a1"),
        job_id: None,
        script_path: "/runwatch/r5-acceptance/attempt-1.sh".into(),
        stdout_path: "/runwatch/r5-acceptance/stdout.log".into(),
        stderr_path: "/runwatch/r5-acceptance/stderr.log".into(),
        terminal_path: "/runwatch/r5-acceptance/terminal.json".into(),
        receipt_path: "/runwatch/r5-acceptance/submission.receipt".into(),
        status: RunStatus::Submitting,
        created_at: now,
        updated_at: now,
        error: None,
    };

    if !store.create_submission_intent(&run, &attempt, Some(&binding))? {
        bail!("failed to create isolated R5 submission intent");
    }

    let terminal_at = Utc::now();
    run.job_id = Some("r5-synthetic".into());
    run.status = RunStatus::Succeeded;
    run.updated_at = terminal_at;
    run.note = Some("synthetic terminal Run for R5 offline continuation acceptance".into());
    attempt.job_id = run.job_id.clone();
    attempt.status = RunStatus::Succeeded;
    attempt.updated_at = terminal_at;
    store.persist_run_attempt_event(&run, &attempt, "acceptance_terminal_seed")?;
    let delivery_id = store
        .ensure_terminal_delivery(&run)?
        .context("terminal seed did not create a Delivery")?;

    println!(
        "{}",
        serde_json::json!({
            "run_id": run_id,
            "delivery_id": delivery_id,
            "session_id": binding.session_id,
            "session_file": binding.session_file,
            "origin_leaf_id": binding.origin_leaf_id,
            "project_root": binding.project_root,
            "adapter_path": binding.adapter_path,
            "data_dir": env::var("RUNWATCH_DATA_DIR").unwrap_or_default(),
        })
    );
    Ok(())
}
