#![windows_subsystem = "windows"]

mod icon;

use runwatch_core::{RunRecord, RunStatus, autostart};
use runwatch_ssh::parse_ssh_config;
use std::collections::HashMap;
use std::time::Duration;
use windui::prelude::*;

struct Tick {
    header: String,
    body: String,
    toast: Option<(String, String)>,
    paused: Option<bool>,
}

fn main() {
    let gui_autostart_on = signal(autostart::gui_is_enabled());
    let daemon_autostart_on = signal(autostart::daemon_is_enabled());
    let header = signal("Connecting to runwatchd…".to_string());
    let body = signal("runwatchd owns scheduler polling and durable Run state.".to_string());
    let pause_ui = signal(false);

    let mark = icon::rgba(32);
    let window_icon = WindowIcon::from_rgba(32, 32, mark.clone());

    let tray = Tray::new()
        .tooltip("runwatch")
        .icon_rgba(32, 32, &mark)
        .on_left_click(|ctx| ctx.show_window())
        .on_double_click(|ctx| ctx.show_window())
        .menu(vec![
            TrayMenuItem::item("Open", |ctx| ctx.show_window()),
            TrayMenuItem::item("Hide", |ctx| ctx.hide_window()),
            TrayMenuItem::separator(),
            TrayMenuItem::check("Pause polling", pause_ui, move |_ctx| {
                let next = !pause_ui.get();
                pause_ui.set(next);
                std::thread::Builder::new()
                    .name("runwatch-control".into())
                    .spawn(move || {
                        let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                        else {
                            return;
                        };
                        let _ = rt.block_on(runwatch_engine::ipc::call_local(
                            "set_paused",
                            serde_json::json!({ "paused": next }),
                        ));
                    })
                    .ok();
            }),
            TrayMenuItem::check("Keep runwatchd running", daemon_autostart_on, move |ctx| {
                let next = !daemon_autostart_on.get();
                if next {
                    match sibling_cli().and_then(|exe| {
                        autostart::install_daemon(&exe, autostart::DEFAULT_DAEMON_INTERVAL_SEC)
                            .map(|_| exe)
                    }) {
                        Ok(_) => {
                            daemon_autostart_on.set(true);
                            ctx.notify("runwatchd", "Task Scheduler service enabled");
                        }
                        Err(err) => ctx.notify("runwatchd", &format!("Enable failed: {err}")),
                    }
                } else {
                    match autostart::remove_daemon() {
                        Ok(_) => {
                            daemon_autostart_on.set(false);
                            ctx.notify("runwatchd", "Task Scheduler service disabled");
                        }
                        Err(err) => ctx.notify("runwatchd", &format!("Disable failed: {err}")),
                    }
                }
            }),
            TrayMenuItem::check("Start GUI with Windows", gui_autostart_on, move |ctx| {
                let next = !gui_autostart_on.get();
                if next {
                    if let Ok(exe) = std::env::current_exe() {
                        match autostart::install_gui(&exe) {
                            Ok(_) => {
                                gui_autostart_on.set(true);
                                ctx.notify("runwatch", "GUI will start at logon");
                            }
                            Err(err) => {
                                ctx.notify("runwatch", &format!("GUI autostart failed: {err}"))
                            }
                        }
                    }
                } else {
                    match autostart::remove_gui() {
                        Ok(_) => {
                            gui_autostart_on.set(false);
                            ctx.notify("runwatch", "GUI logon autostart removed");
                        }
                        Err(err) => {
                            ctx.notify("runwatch", &format!("GUI autostart removal failed: {err}"))
                        }
                    }
                }
            }),
            TrayMenuItem::separator(),
            TrayMenuItem::item("Quit", |ctx| ctx.quit()),
        ]);

    let mut app = App::new("runwatch", 640, 480)
        .icon(window_icon.expect("icon rgba"))
        .bg(Color::hex(0xF7F4EE))
        .hide_on_close()
        .tray(tray);

    let tx = app.channel::<Tick>({
        let header = header;
        let body = body;
        move |ctx, tick| {
            header.set(tick.header);
            body.set(tick.body);
            if let Some(paused) = tick.paused {
                pause_ui.set(paused);
            }
            if let Some((title, msg)) = tick.toast {
                ctx.toast_ok(&format!("{title}: {msg}"));
            }
        }
    });

    std::thread::Builder::new()
        .name("runwatch-poll".into())
        .spawn(move || poll_loop_send(tx))
        .ok();

    let hosts_text = host_summary();
    let ui = Element::col()
        .fill()
        .bg(Color::hex(0xF7F4EE))
        .padding(20)
        .spacing(10)
        .child(
            Element::label("runwatch")
                .font_size(26.0)
                .fg(Color::hex(0x243746))
                .width_match(),
        )
        .child(
            Element::label(
                "Close hides to tray. runwatchd owns scheduler polling and SSH sessions.",
            )
            .font_size(13.0)
            .fg(Color::hex(0x5B6B75))
            .width_match(),
        )
        .child(
            Element::label_signal(header)
                .font_size(14.0)
                .fg(Color::hex(0x2AA8A0))
                .width_match(),
        )
        .child(
            Element::label_signal(body)
                .font_size(13.0)
                .fg(Color::hex(0x243746))
                .width_match()
                .weight(1.0),
        )
        .child(
            Element::label("Hosts from ~/.ssh/config")
                .font_size(12.0)
                .fg(Color::hex(0x5B6B75))
                .width_match(),
        )
        .child(
            Element::label(hosts_text)
                .font_size(12.0)
                .fg(Color::hex(0x2AA8A0))
                .width_match(),
        );

    app.content(ui).run();
}

fn sibling_cli() -> anyhow::Result<std::path::PathBuf> {
    let exe = std::env::current_exe()?;
    let dir = exe
        .parent()
        .ok_or_else(|| anyhow::anyhow!("GUI executable has no parent"))?;
    let cli = dir.join("runwatch.exe");
    if cli.is_file() {
        Ok(cli)
    } else {
        anyhow::bail!("runwatch.exe not next to {}", exe.display())
    }
}

fn host_summary() -> String {
    let text = parse_ssh_config()
        .unwrap_or_default()
        .into_iter()
        .map(|h| {
            let jump = if h.proxy_jump.is_empty() {
                String::new()
            } else {
                format!(" via {}", h.proxy_jump.join(","))
            };
            format!(
                "·  {}  {}@{}{}",
                h.alias,
                h.user.as_deref().unwrap_or("?"),
                h.hostname,
                jump
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    if text.is_empty() {
        "No Host entries in ~/.ssh/config".into()
    } else {
        text
    }
}

fn summarize(runs: &[RunRecord]) -> (String, String) {
    let live = runs.iter().filter(|r| !r.status.is_terminal()).count();
    let failed = runs
        .iter()
        .filter(|r| r.status == RunStatus::Failed)
        .count();
    let header = format!("{live} live    {failed} failed    {} total", runs.len());
    let body = if runs.is_empty() {
        "No runs in runwatch.db yet.\nUse `runwatch submit` or pi-runs.".into()
    } else {
        runs.iter()
            .rev()
            .take(24)
            .map(|r| {
                format!(
                    "{}  {}  {}  job {}",
                    r.run_id,
                    status_label(r.status),
                    r.host,
                    r.job_id.as_deref().unwrap_or("-")
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    (header, body)
}

fn status_label(status: RunStatus) -> &'static str {
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

fn terminal_toast(
    previous: &HashMap<String, RunStatus>,
    runs: &[RunRecord],
) -> Option<(String, String)> {
    runs.iter().find_map(|run| {
        let from = previous.get(&run.run_id)?;
        (!from.is_terminal() && run.status.is_terminal()).then(|| {
            (
                run.run_id.clone(),
                format!("{} -> {}", status_label(*from), status_label(run.status)),
            )
        })
    })
}

fn poll_loop_send(tx: windui::prelude::Sender<Tick>) {
    poll_loop(move |tick| tx.send(tick).map_err(|_| ()));
}

fn poll_loop<S>(tx: S)
where
    S: Fn(Tick) -> Result<(), ()> + Send + 'static,
{
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(_) => return,
    };
    rt.block_on(async move {
        let mut previous = HashMap::<String, RunStatus>::new();
        loop {
            let tick = match runwatch_engine::ipc::call_local("daemon_status", serde_json::json!({})).await {
                Ok(state) => {
                    let paused = state.get("paused").and_then(serde_json::Value::as_bool).unwrap_or(false);
                    match runwatch_engine::ipc::call_local("list_runs", serde_json::json!({})).await {
                        Ok(value) => {
                            let runs: Vec<RunRecord> = value
                                .get("runs")
                                .cloned()
                                .and_then(|rows| serde_json::from_value(rows).ok())
                                .unwrap_or_default();
                            let toast = terminal_toast(&previous, &runs);
                            previous = runs
                                .iter()
                                .map(|run| (run.run_id.clone(), run.status))
                                .collect();
                            let (header, body) = summarize(&runs);
                            Tick {
                                header: format!(
                                    "{header}    daemon {}",
                                    if paused { "paused" } else { "attached" }
                                ),
                                body,
                                toast,
                                paused: Some(paused),
                            }
                        }
                        Err(err) => Tick {
                            header: "runwatchd state unavailable".into(),
                            body: format!("Could not read canonical Run state: {err:#}"),
                            toast: None,
                            paused: Some(paused),
                        },
                    }
                }
                Err(err) => Tick {
                    header: "runwatchd unavailable".into(),
                    body: format!(
                        "Start the runwatch daemon; the GUI will not take over scheduler polling.\n{err:#}"
                    ),
                    toast: None,
                    paused: None,
                },
            };
            if tx(tick).is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });
}
