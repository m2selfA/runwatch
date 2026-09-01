use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use runwatch_core::autostart;
use runwatch_core::{AppConfig, RunRecord, RunnerKind};
use runwatch_engine::serve_loop;
use runwatch_ssh::{HostPool, parse_ssh_config};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "runwatch",
    about = "Watch durable SSH/HPC runs and wake agents"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List ~/.ssh/config host aliases
    Hosts,
    /// Show configured watch hosts and ledger path
    Status,
    /// List runs in the local ledger
    List,
    /// Adopt/register an existing scheduler Run (durable new submissions use submit_run_v2 via agent integrations)
    Submit {
        run_id: String,
        host: String,
        #[arg(long)]
        job_id: Option<String>,
        #[arg(long, default_value = "slurm")]
        runner: String,
        #[arg(long)]
        terminal: Option<String>,
        #[arg(long, hide = true)]
        on_success: Option<String>,
        #[arg(long, hide = true)]
        on_failure: Option<String>,
        #[arg(long)]
        session_id: Option<String>,
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        agent: Option<String>,
    },
    /// Probe one host (russh exec true)
    Ping { host: String },
    /// Poll slurm/file status for one run over SSH
    Refresh { run_id: String },
    /// Poll every non-terminal run once
    Tick,
    /// Probe the local runwatch daemon IPC endpoint
    DaemonStatus,
    /// Headless poll loop (keeps russh sessions warm)
    Serve {
        #[arg(long, default_value_t = 20)]
        interval: u64,
    },
    /// Resident process supervisor for runwatch serve
    Supervise {
        #[arg(long, default_value_t = 20)]
        interval: u64,
    },
    /// Block until a run is terminal
    Wait {
        run_id: String,
        #[arg(long, default_value_t = 3600)]
        timeout: u64,
    },
    /// Manage the resident runwatchd Task Scheduler task
    Autostart {
        #[arg(long)]
        install: bool,
        #[arg(long)]
        remove: bool,
        #[arg(long, default_value_t = autostart::DEFAULT_DAEMON_INTERVAL_SEC)]
        interval: u64,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Hosts => {
            for host in parse_ssh_config()? {
                let jump = if host.proxy_jump.is_empty() {
                    String::new()
                } else {
                    format!(" via {}", host.proxy_jump.join(","))
                };
                println!(
                    "{:<16} {}@{}:{}{}",
                    host.alias,
                    host.user.as_deref().unwrap_or("?"),
                    host.hostname,
                    host.port,
                    jump
                );
            }
        }
        Cmd::Status => {
            let cfg = AppConfig::load_or_default()?;
            println!(
                "ledger {}",
                runwatch_core::data_dir()?.join("runwatch.db").display()
            );
            match runwatch_core::live::read()? {
                Some(hb) if runwatch_core::live::is_alive() => {
                    println!(
                        "serve pid={} beat={}s ago",
                        hb.pid,
                        runwatch_core::live::now_unix().saturating_sub(hb.beat_unix)
                    );
                }
                _ => println!("serve not running"),
            }
            println!("watched hosts: {}", cfg.hosts.len());
            for h in cfg.hosts {
                println!("  {} enabled={} poll={}s", h.alias, h.enabled, h.poll_sec);
            }
        }
        Cmd::List => {
            let value =
                runwatch_engine::ipc::call_local("list_runs", serde_json::json!({})).await?;
            let rows: Vec<RunRecord> = serde_json::from_value(
                value
                    .get("runs")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!([])),
            )?;
            for row in rows {
                println!(
                    "{}  {:<10}  host={} job={}",
                    row.run_id,
                    format!("{:?}", row.status).to_lowercase(),
                    row.host,
                    row.job_id.as_deref().unwrap_or("-")
                );
            }
        }
        Cmd::Submit {
            run_id,
            host,
            job_id,
            runner,
            terminal,
            on_success,
            on_failure,
            session_id,
            project,
            agent,
        } => {
            let runner = parse_runner(&runner)?;
            if on_success.is_some() || on_failure.is_some() {
                bail!(
                    "--on-success/--on-failure shell callbacks were retired; use durable continuation Delivery instead"
                );
            }
            let mut rec = RunRecord::new(run_id, host, runner);
            rec.job_id = job_id;
            rec.remote_terminal = terminal;
            rec.session_id = session_id;
            rec.project_root = project;
            rec.agent = agent;
            let value =
                runwatch_engine::ipc::call_local("adopt_run_v1", serde_json::json!({ "run": rec }))
                    .await?;
            let adopted: RunRecord = serde_json::from_value(
                value
                    .get("run")
                    .cloned()
                    .context("daemon adopt_run_v1 returned no Run")?,
            )?;
            println!("{}", adopted.run_id);
        }
        Cmd::Ping { host } => {
            let pool = configured_pool()?;
            let out = pool.exec(&host, "echo runwatch-ok && hostname").await?;
            print!("{}", out.stdout);
            if !out.stderr.is_empty() {
                eprint!("{}", out.stderr);
            }
            if out.code.unwrap_or(1) != 0 {
                bail!("remote exit {:?}", out.code);
            }
        }
        Cmd::Refresh { run_id } => {
            let _ = runwatch_engine::ipc::call_local("tick", serde_json::json!({})).await?;
            let value = runwatch_engine::ipc::call_local(
                "get_run",
                serde_json::json!({ "run_id": run_id }),
            )
            .await?;
            let Some(run) = value.get("run").cloned() else {
                bail!("unknown run {run_id}");
            };
            let rec: RunRecord = serde_json::from_value(run)?;
            println!("{:?}", rec.status);
        }
        Cmd::Tick => {
            let value = runwatch_engine::ipc::call_local("tick", serde_json::json!({})).await?;
            let transitions = value
                .get("transitions")
                .and_then(serde_json::Value::as_array)
                .cloned()
                .unwrap_or_default();
            let errors = value
                .get("errors")
                .and_then(serde_json::Value::as_array)
                .cloned()
                .unwrap_or_default();
            for transition in &transitions {
                println!(
                    "{} {} -> {}",
                    transition
                        .get("run_id")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("?"),
                    transition
                        .get("from")
                        .map(ToString::to_string)
                        .unwrap_or_else(|| "?".into()),
                    transition
                        .get("to")
                        .map(ToString::to_string)
                        .unwrap_or_else(|| "?".into()),
                );
            }
            for error in &errors {
                eprintln!("{}", error.as_str().unwrap_or("unknown daemon tick error"));
            }
            if transitions.is_empty() && errors.is_empty() {
                let count = value
                    .get("runs")
                    .and_then(serde_json::Value::as_array)
                    .map_or(0, Vec::len);
                println!("{count} runs, no changes");
            }
        }
        Cmd::DaemonStatus => {
            let value = runwatch_engine::ipc::probe_local_server().await?;
            println!("{}", serde_json::to_string_pretty(&value)?);
        }
        Cmd::Serve { interval } => {
            eprintln!("runwatch serve interval={interval}s");
            serve_loop(
                Arc::new(AtomicBool::new(false)),
                Duration::from_secs(interval),
                |report| {
                    let (header, _) = report.summarize();
                    eprintln!("{header}");
                    for t in report.transitions {
                        eprintln!("  {} {:?} -> {:?}", t.run_id, t.from, t.to);
                    }
                    true
                },
            )
            .await?;
        }
        Cmd::Supervise { interval } => {
            eprintln!("runwatch supervise interval={interval}s");
            let exe = std::env::current_exe()?;
            runwatch_core::supervisor::supervise(&exe, interval)?;
        }
        Cmd::Wait { run_id, timeout } => {
            let value = runwatch_engine::ipc::call_local(
                "wait_run",
                serde_json::json!({ "run_id": run_id, "timeout_sec": timeout }),
            )
            .await?;
            match value.get("run").cloned() {
                Some(run) if !run.is_null() => {
                    let rec: RunRecord = serde_json::from_value(run)?;
                    println!("{} {:?}", rec.run_id, rec.status);
                }
                _ => bail!("unknown run {run_id}"),
            }
        }
        Cmd::Autostart {
            install,
            remove,
            interval,
        } => {
            if remove {
                let gone = autostart::remove_daemon()?;
                println!("{}", if gone { "removed" } else { "not installed" });
            } else if install {
                let exe = std::env::current_exe()?;
                autostart::install_daemon(&exe, interval)?;
                println!(
                    "installed Task Scheduler task {}",
                    autostart::daemon_task_name()
                );
            } else {
                println!(
                    "daemon={} task={}  gui_autostart={}",
                    if autostart::daemon_is_enabled() {
                        "enabled"
                    } else {
                        "disabled"
                    },
                    autostart::daemon_task_name(),
                    if autostart::gui_is_enabled() {
                        "enabled"
                    } else {
                        "disabled"
                    }
                );
            }
        }
    }
    Ok(())
}

fn configured_pool() -> Result<HostPool> {
    let config = AppConfig::load_or_default()?;
    HostPool::from_ssh_config_with_timeout(
        Duration::from_secs(config.ssh.alive_interval.max(1)),
        Duration::from_secs(config.ssh.cmd_timeout_sec.max(1)),
    )
}

fn parse_runner(s: &str) -> Result<RunnerKind> {
    Ok(match s {
        "slurm" => RunnerKind::Slurm,
        "lsf" => RunnerKind::Lsf,
        "process" => RunnerKind::Process,
        "file" => RunnerKind::File,
        "powershell" => RunnerKind::Powershell,
        other => bail!("unknown runner {other}"),
    })
}
