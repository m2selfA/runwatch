use crate::ipc_client;
use crate::model::{DashboardSnapshot, RunDetailView, project_dashboard};
use crate::notifications::{Notice, NoticeKind, TransitionTracker};
use chrono::Utc;
use runwatch_core::{SubmitRunSpec, autostart};
use tokio::sync::mpsc;
use windui::prelude::Sender;

#[derive(Debug, Clone)]
pub enum Command {
    Refresh,
    SetPaused(bool),
    LoadDetail {
        request_id: u64,
        run_id: String,
        tail: usize,
        event_limit: usize,
    },
    Cancel(String),
    Probe(String),
    SubmitManual(SubmitRunSpec),
    SetDaemonAutostart(bool),
    SetGuiAutostart(bool),
}

#[derive(Debug, Clone)]
pub enum UiEvent {
    Snapshot {
        snapshot: DashboardSnapshot,
        notices: Vec<Notice>,
    },
    DaemonUnavailable(String),
    Detail {
        request_id: u64,
        detail: RunDetailView,
    },
    Hosts(Result<String, String>),
    ManualSubmitResult {
        run_id: String,
        result: Result<(), String>,
    },
    AutostartState {
        daemon_enabled: bool,
        gui_enabled: bool,
        notice: Notice,
    },
    Notice(Notice),
}

pub fn start(ui: Sender<UiEvent>) -> mpsc::UnboundedSender<Command> {
    let (command_tx, mut command_rx) = mpsc::unbounded_channel();
    std::thread::Builder::new()
        .name("runwatch-gui-controller".into())
        .spawn(move || {
            let hosts = ipc_client::host_summary().map_err(|error| format!("{error:#}"));
            if ui.send(UiEvent::Hosts(hosts)).is_err() {
                return;
            }
            let Ok(runtime) = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
            else {
                let _ = ui.send(UiEvent::Notice(Notice {
                    kind: NoticeKind::Attention,
                    title: "runwatch GUI".into(),
                    body: "Could not create the background runtime".into(),
                }));
                return;
            };
            runtime.block_on(async move {
                let mut detail_task: Option<tokio::task::JoinHandle<()>> = None;
                let mut tracker = TransitionTracker::default();
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    tokio::select! {
                        _ = interval.tick() => {
                            if !refresh(&ui, &mut tracker).await {
                                return;
                            }
                        }
                        command = command_rx.recv() => {
                            let Some(command) = command else { return; };
                            match command {
                                Command::Refresh => {
                                    if !refresh(&ui, &mut tracker).await {
                                        return;
                                    }
                                }
                                Command::SetPaused(paused) => {
                                    if let Err(error) = ipc_client::set_paused(paused).await {
                                        if ui.send(UiEvent::Notice(Notice {
                                            kind: NoticeKind::Attention,
                                            title: "Polling control failed".into(),
                                            body: format!("{error:#}"),
                                        })).is_err() {
                                            return;
                                        }
                                    }
                                    if !refresh(&ui, &mut tracker).await {
                                        return;
                                    }
                                }
                                Command::LoadDetail {
                                    request_id,
                                    run_id,
                                    tail,
                                    event_limit,
                                } => {
                                    if let Some(task) = detail_task.take() {
                                        task.abort();
                                    }
                                    let ui_detail = ui.clone();
                                    detail_task = Some(tokio::spawn(async move {
                                        match ipc_client::load_detail(&run_id, tail, event_limit).await {
                                            Ok(detail) => {
                                                let _ = ui_detail.send(UiEvent::Detail {
                                                    request_id,
                                                    detail,
                                                });
                                            }
                                            Err(error) => {
                                                let _ = ui_detail.send(UiEvent::Notice(Notice {
                                                    kind: NoticeKind::Attention,
                                                    title: format!("Could not load {run_id}"),
                                                    body: format!("{error:#}"),
                                                }));
                                            }
                                        }
                                    }));
                                }
                                Command::Cancel(run_id) => {
                                    let ui_cancel = ui.clone();
                                    tokio::spawn(async move {
                                        let notice = match ipc_client::cancel_run(&run_id).await {
                                            Ok(()) => Notice {
                                                kind: NoticeKind::Success,
                                                title: "Cancel requested".into(),
                                                body: format!("{run_id}; final state still follows durable observation"),
                                            },
                                            Err(error) => Notice {
                                                kind: NoticeKind::Attention,
                                                title: "Cancel failed".into(),
                                                body: format!("{error:#}"),
                                            },
                                        };
                                        let _ = ui_cancel.send(UiEvent::Notice(notice));
                                    });
                                }
                                Command::Probe(run_id) => {
                                    let ui_probe = ui.clone();
                                    tokio::spawn(async move {
                                        let notice = match ipc_client::probe_run(&run_id).await {
                                            Ok(errors) if errors.is_empty() => Notice {
                                                kind: NoticeKind::Success,
                                                title: "Observation refreshed".into(),
                                                body: run_id,
                                            },
                                            Ok(errors) => Notice {
                                                kind: NoticeKind::Attention,
                                                title: "Observation refreshed with issues".into(),
                                                body: errors.join("; "),
                                            },
                                            Err(error) => Notice {
                                                kind: NoticeKind::Attention,
                                                title: "Probe failed".into(),
                                                body: format!("{error:#}"),
                                            },
                                        };
                                        let _ = ui_probe.send(UiEvent::Notice(notice));
                                    });
                                }
                                Command::SubmitManual(spec) => {
                                    let ui_submit = ui.clone();
                                    tokio::spawn(async move {
                                        let requested_run_id = spec.run_id.clone();
                                        let result = ipc_client::submit_manual(spec)
                                            .await
                                            .map(|_| ())
                                            .map_err(|error| format!("{error:#}"));
                                        let _ = ui_submit.send(UiEvent::ManualSubmitResult {
                                            run_id: requested_run_id,
                                            result,
                                        });
                                    });
                                }
                                Command::SetDaemonAutostart(enabled) => {
                                    spawn_autostart_change(ui.clone(), AutostartTarget::Daemon, enabled);
                                }
                                Command::SetGuiAutostart(enabled) => {
                                    spawn_autostart_change(ui.clone(), AutostartTarget::Gui, enabled);
                                }
                            }
                        }
                    }
                }
            });
        })
        .expect("spawn runwatch GUI controller");
    command_tx
}

#[derive(Clone, Copy)]
enum AutostartTarget {
    Daemon,
    Gui,
}

fn spawn_autostart_change(ui: Sender<UiEvent>, target: AutostartTarget, enabled: bool) {
    tokio::task::spawn_blocking(move || {
        let result = match (target, enabled) {
            (AutostartTarget::Daemon, true) => ipc_client::sibling_cli().and_then(|exe| {
                autostart::install_daemon(&exe, autostart::DEFAULT_DAEMON_INTERVAL_SEC)
                    .map_err(Into::into)
            }),
            (AutostartTarget::Daemon, false) => {
                autostart::remove_daemon().map(|_| ()).map_err(Into::into)
            }
            (AutostartTarget::Gui, true) => std::env::current_exe()
                .map_err(Into::into)
                .and_then(|exe| autostart::install_gui(&exe).map(|_| ()).map_err(Into::into)),
            (AutostartTarget::Gui, false) => {
                autostart::remove_gui().map(|_| ()).map_err(Into::into)
            }
        };
        let daemon_enabled = autostart::daemon_is_enabled();
        let gui_enabled = autostart::gui_is_enabled();
        let noun = match target {
            AutostartTarget::Daemon => "Resident runwatchd",
            AutostartTarget::Gui => "GUI autostart",
        };
        let notice = match result {
            Ok(()) => Notice {
                kind: NoticeKind::Success,
                title: noun.into(),
                body: if enabled {
                    "enabled".into()
                } else {
                    "disabled".into()
                },
            },
            Err(error) => Notice {
                kind: NoticeKind::Attention,
                title: format!("{noun} change failed"),
                body: format!("{error:#}"),
            },
        };
        let _ = ui.send(UiEvent::AutostartState {
            daemon_enabled,
            gui_enabled,
            notice,
        });
    });
}

async fn refresh(ui: &Sender<UiEvent>, tracker: &mut TransitionTracker) -> bool {
    match ipc_client::snapshot().await {
        Ok(payload) => {
            let snapshot = project_dashboard(
                payload.version,
                payload.protocol_version,
                payload.capability_count,
                payload.pid,
                payload.paused,
                payload.manual_submit_supported,
                payload.runs,
                payload.observations,
                payload.continuations,
                Utc::now(),
            );
            let notices = tracker.observe(&snapshot.rows);
            ui.send(UiEvent::Snapshot { snapshot, notices }).is_ok()
        }
        Err(error) => ui
            .send(UiEvent::DaemonUnavailable(format!("{error:#}")))
            .is_ok(),
    }
}
