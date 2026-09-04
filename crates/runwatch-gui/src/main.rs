#![windows_subsystem = "windows"]

mod controller;
#[cfg(debug_assertions)]
mod fixtures;
mod icon;
mod ipc_client;
mod model;
mod notifications;
mod views;

use controller::{Command, UiEvent};
use notifications::{Notice, NoticeKind};
use runwatch_core::autostart;
use views::runs::RunsViewState;
use windui::core::EventCtx;
use windui::prelude::*;

fn main() {
    let gui_autostart_on = signal(autostart::gui_is_enabled());
    let daemon_autostart_on = signal(autostart::daemon_is_enabled());
    let pause_ui = signal(false);
    let hosts_text = signal("Reading ~/.ssh/config...".to_string());
    let hosts_usage = signal("No live Run host usage yet.".to_string());
    let service_text = signal("Connecting to runwatchd...".to_string());
    let package_text = ipc_client::package_summary();
    let runs_state = RunsViewState::new();

    let mark = icon::rgba(32);
    let window_icon = WindowIcon::from_rgba(32, 32, mark.clone());
    let mut app = App::new("runwatch", 1080, 720)
        .icon(window_icon.expect("icon rgba"))
        .bg(Color::hex(0xF7F4EE))
        .hide_on_close();

    let ui_tx = app.channel::<UiEvent>({
        let runs_state = runs_state.clone();
        move |ctx, event| match event {
            UiEvent::Snapshot { snapshot, notices } => {
                pause_ui.set(snapshot.paused);
                service_text.set(format!(
                    "runwatchd {}\nprotocol: {} · capabilities: {}\npid: {}\npolling: {}\nresident service: {}\nGUI autostart: {}\n{}",
                    snapshot.daemon_version,
                    snapshot.daemon_protocol,
                    snapshot.daemon_capabilities,
                    snapshot.daemon_pid,
                    if snapshot.paused { "paused" } else { "active" },
                    if daemon_autostart_on.get() {
                        "enabled"
                    } else {
                        "disabled"
                    },
                    if gui_autostart_on.get() {
                        "enabled"
                    } else {
                        "disabled"
                    },
                    package_text
                ));
                hosts_usage.set(model::host_usage_summary(&snapshot.rows));
                runs_state.apply_snapshot(snapshot);
                for notice in notices {
                    show_notice(ctx, notice);
                }
            }
            UiEvent::DaemonUnavailable(error) => {
                service_text.set(format!(
                    "runwatchd unavailable\n{error}\n\nThe GUI will not take over scheduler polling."
                ));
                runs_state.daemon_unavailable(error);
            }
            UiEvent::Detail { request_id, detail } => {
                runs_state.apply_detail(request_id, detail)
            }
            UiEvent::Hosts(result) => match result {
                Ok(hosts) => hosts_text.set(hosts),
                Err(error) => hosts_text.set(format!(
                    "Could not resolve OpenSSH Host aliases.\n{error}\n\nThis is a configuration error, not an empty Host list."
                )),
            },
            UiEvent::AutostartState {
                daemon_enabled,
                gui_enabled,
                notice,
            } => {
                daemon_autostart_on.set(daemon_enabled);
                gui_autostart_on.set(gui_enabled);
                show_notice(ctx, notice);
            }
            UiEvent::Notice(notice) => show_notice(ctx, notice),
        }
    });

    #[cfg(debug_assertions)]
    let commands = if let Ok(name) = std::env::var("RUNWATCH_GUI_FIXTURE") {
        let fixture = fixtures::named(&name).unwrap_or_else(|| {
            panic!("unknown RUNWATCH_GUI_FIXTURE={name}; expected dashboard, detail, or offline")
        });
        pause_ui.set(fixture.snapshot.paused);
        hosts_text.set(fixture.hosts);
        service_text.set(fixture.service);
        hosts_usage.set(model::host_usage_summary(&fixture.snapshot.rows));
        runs_state.apply_snapshot(fixture.snapshot);
        if let Some(detail) = fixture.detail {
            runs_state.apply_fixture_detail(detail);
        }
        if let Some(error) = fixture.offline_error {
            runs_state.daemon_unavailable(error);
        }
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        std::mem::forget(rx);
        tx
    } else {
        controller::start(ui_tx)
    };
    #[cfg(not(debug_assertions))]
    let commands = controller::start(ui_tx);
    let tray = build_tray(
        &mark,
        commands.clone(),
        pause_ui,
        daemon_autostart_on,
        gui_autostart_on,
    );
    app = app.tray(tray);

    let tabs = Element::tabs(
        signal(0usize),
        vec![
            ("Runs", runs_state.build(commands.clone())),
            ("Hosts", views::hosts_page(hosts_text, hosts_usage)),
            (
                "Service",
                views::service_page(service_text, daemon_autostart_on, commands.clone()),
            ),
            (
                "Settings",
                views::settings_page(gui_autostart_on, commands.clone()),
            ),
        ],
    );

    let ui = Element::col()
        .fill()
        .bg(Color::hex(0xF7F4EE))
        .padding(16)
        .spacing(10)
        .child(
            Element::row()
                .width_match()
                .cross(Align::Center)
                .child(
                    Element::label("runwatch")
                        .font_size(26.0)
                        .fg(Color::hex(0x243746))
                        .weight(1.0),
                )
                .child(
                    Element::label("Human Run Console · close hides to tray")
                        .font_size(12.5)
                        .fg(Color::hex(0x5B6B75)),
                ),
        )
        .child(tabs.weight(1.0));

    app.content(ui).screenshot_from_args().run();
}

fn build_tray(
    mark: &[u8],
    commands: tokio::sync::mpsc::UnboundedSender<Command>,
    pause_ui: Signal<bool>,
    daemon_autostart_on: Signal<bool>,
    gui_autostart_on: Signal<bool>,
) -> Tray {
    let pause_commands = commands.clone();
    let daemon_commands = commands.clone();
    let gui_commands = commands.clone();
    Tray::new()
        .tooltip("runwatch · Human Run Console")
        .icon_rgba(32, 32, mark)
        .on_left_click(|ctx| ctx.show_window())
        .on_double_click(|ctx| ctx.show_window())
        .menu(vec![
            TrayMenuItem::item("Open Run Console", |ctx| ctx.show_window()),
            TrayMenuItem::item("Hide", |ctx| ctx.hide_window()),
            TrayMenuItem::separator(),
            TrayMenuItem::check("Pause polling", pause_ui, move |_ctx| {
                let next = !pause_ui.get();
                pause_ui.set(next);
                let _ = pause_commands.send(Command::SetPaused(next));
            }),
            TrayMenuItem::check("Keep runwatchd running", daemon_autostart_on, move |ctx| {
                if daemon_autostart_on.get() {
                    ctx.notify(
                        "runwatchd",
                        "Open Run Console > Service to disable the resident runtime safely",
                    );
                    ctx.show_window();
                } else {
                    let _ = daemon_commands.send(Command::SetDaemonAutostart(true));
                }
            }),
            TrayMenuItem::check("Start GUI with Windows", gui_autostart_on, move |_ctx| {
                let _ = gui_commands.send(Command::SetGuiAutostart(!gui_autostart_on.get()));
            }),
            TrayMenuItem::separator(),
            TrayMenuItem::item("Quit", |ctx| ctx.quit()),
        ])
}

fn show_notice(ctx: &mut EventCtx, notice: Notice) {
    let message = format!("{}: {}", notice.title, notice.body);
    match notice.kind {
        NoticeKind::Success => ctx.toast_ok(&message),
        NoticeKind::Attention => ctx.toast_err(&message),
    }
}
