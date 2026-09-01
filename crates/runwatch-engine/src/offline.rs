use anyhow::{Context, Result};
use base64::Engine;
use runwatch_core::{AgentInvocationRecord, RunStore};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;

const OFFLINE_GRACE: Duration = Duration::from_secs(20);
const MAX_LAUNCHES_PER_TICK: usize = 2;
const MAX_AGENT_RUNTIME: Duration = Duration::from_secs(6 * 60 * 60);
const DELIVERY_STDIN_POLL: Duration = Duration::from_millis(500);
pub const ORPHAN_RECONNECT_GRACE: Duration = Duration::from_secs(30);
const ORPHAN_RETRY_DELAY: Duration = Duration::from_secs(30);

fn delivery_keeps_rpc_stdin_open(state: Option<&str>) -> bool {
    matches!(state, Some("delivering"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PiLauncher {
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
fn resolve_default_pi_launcher(path: Option<&OsStr>) -> Result<PiLauncher> {
    if let Some(executable) = find_in_path(path, "pi.exe") {
        return Ok(PiLauncher {
            executable: executable.into_os_string(),
            prefix_args: Vec::new(),
        });
    }
    if let Some(volta) = find_in_path(path, "volta.exe") {
        return Ok(PiLauncher {
            executable: volta.into_os_string(),
            prefix_args: vec![OsString::from("run"), OsString::from("pi")],
        });
    }
    anyhow::bail!(
        "no native Pi launcher found on PATH: expected pi.exe or volta.exe. Windows shell shims such as pi.cmd are not used for unattended continuation; set RUNWATCH_PI_EXECUTABLE to a native launcher"
    )
}

#[cfg(not(windows))]
fn resolve_default_pi_launcher(_path: Option<&OsStr>) -> Result<PiLauncher> {
    Ok(PiLauncher {
        executable: OsString::from("pi"),
        prefix_args: Vec::new(),
    })
}

fn resolve_pi_launcher() -> Result<PiLauncher> {
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
            anyhow::bail!(
                "RUNWATCH_PI_EXECUTABLE must be a native executable on Windows, not a .cmd/.bat shell shim"
            );
        }
        return Ok(PiLauncher {
            executable,
            prefix_args: Vec::new(),
        });
    }
    resolve_default_pi_launcher(std::env::var_os("PATH").as_deref())
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
                let _ = store.finish_agent_invocation_process(
                    &invocation.invocation_id,
                    None,
                    Some(&format!("offline Pi invocation task failed: {err:#}")),
                );
            }
        });
        launched += 1;
    }
    Ok(launched)
}

async fn run_invocation(store: Arc<RunStore>, invocation: AgentInvocationRecord) -> Result<()> {
    let launcher = resolve_pi_launcher()?;
    let payload = serde_json::to_vec(&invocation.payload)?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(payload);

    let mut command = Command::new(&launcher.executable);
    command
        .args(&launcher.prefix_args)
        .args([
            "--mode",
            "rpc",
            "--session",
            &invocation.session_file,
            "-e",
            &invocation.adapter_path,
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
