pub mod hosts;
pub mod runs;

use crate::controller::Command;
use crate::settings::GuiSettingsState;
use crate::settings::NATIVE_NOTIFICATION_AVAILABLE;
use tokio::sync::mpsc;
use windui::prelude::*;

pub fn service_page(
    service: Signal<String>,
    daemon_autostart: Signal<bool>,
    commands: mpsc::UnboundedSender<Command>,
) -> Element {
    let disable_dialog = signal(false);
    let daemon_commands = commands.clone();
    let confirm_commands = commands.clone();
    let dialog_close = disable_dialog;

    let content = Element::col()
        .fill()
        .padding(18)
        .spacing(12)
        .child(
            Element::label("Service")
                .font_size(22.0)
                .fg(Color::hex(0x243746))
                .width_match(),
        )
        .child(
            Element::label(
                "runwatchd remains the only durable Run lifecycle authority. Service controls never cancel scientific Runs implicitly.",
            )
            .font_size(13.0)
            .fg(Color::hex(0x5B6B75))
            .width_match(),
        )
        .child(
            Element::card(
                "Daemon",
                Element::label_signal(service)
                    .font_size(13.0)
                    .fg(Color::hex(0x243746))
                    .width_match(),
            )
            .width_match(),
        )
        .child(
            Element::card(
                "Resident runtime",
                Element::col()
                    .width_match()
                    .spacing(8)
                    .child(
                        Element::checkbox("Keep runwatchd running", daemon_autostart)
                            .on_click(move |_| {
                                if daemon_autostart.get() {
                                    disable_dialog.set(true);
                                } else {
                                    let _ = daemon_commands.send(Command::SetDaemonAutostart(true));
                                }
                            }),
                    )
                    .child(
                        Element::label(
                            "The current-user Task Scheduler/supervisor path keeps observation and continuation delivery alive when Pi and this GUI are closed.",
                        )
                        .font_size(12.0)
                        .fg(Color::hex(0x5B6B75))
                        .width_match(),
                    ),
            )
            .width_match(),
        );

    let dialog = Element::dialog(
        disable_dialog,
        Element::col()
            .width(500)
            .bg(Color::hex(0xFFFDF8))
            .corner(14.0)
            .padding(20)
            .spacing(12)
            .child(
                Element::label("Disable the resident runwatchd runtime?")
                    .font_size(19.0)
                    .fg(Color::hex(0x243746))
                    .width_match(),
            )
            .child(
                Element::label(
                    "Scientific scheduler jobs and breakaway Local Process Runs are not cancelled. However, this machine stops observing them and stops continuation delivery until runwatchd is running again.",
                )
                .font_size(13.0)
                .fg(Color::hex(0x5B6B75))
                .width_match(),
            )
            .child(
                Element::row()
                    .width_match()
                    .spacing(8)
                    .child(Element::label("").weight(1.0))
                    .child(
                        Element::button("Keep enabled")
                            .neutral()
                            .on_click(move |_| dialog_close.set(false)),
                    )
                    .child(
                        Element::button("Disable resident runtime")
                            .danger()
                            .on_click(move |_| {
                                let _ = confirm_commands.send(Command::SetDaemonAutostart(false));
                                disable_dialog.set(false);
                            }),
                    ),
            ),
    );

    Element::stack().fill().child(content).child(dialog)
}

pub fn settings_page(
    gui_autostart: Signal<bool>,
    notification_settings: GuiSettingsState,
    commands: mpsc::UnboundedSender<Command>,
) -> Element {
    let gui_commands = commands.clone();
    let native_commands = commands.clone();
    let success_commands = commands.clone();
    let attention_commands = commands.clone();
    let privacy_commands = commands.clone();
    let native_state = notification_settings;
    let success_state = notification_settings;
    let attention_state = notification_settings;
    let privacy_state = notification_settings;
    let warning_visible = notification_settings.warning;
    Element::col()
        .fill()
        .padding(18)
        .spacing(12)
        .child(
            Element::label("Settings")
                .font_size(22.0)
                .fg(Color::hex(0x243746))
                .width_match(),
        )
        .child(
            Element::label(
                "Settings stores only desktop UX choices. Run, scheduler, SSH and continuation identity remain canonical in their owning authority plane.",
            )
            .font_size(13.0)
            .fg(Color::hex(0x5B6B75))
            .width_match(),
        )
        .child(
            Element::card(
                "Desktop",
                Element::col()
                    .width_match()
                    .spacing(8)
                    .child(
                        Element::checkbox("Start Run Console with Windows", gui_autostart)
                            .on_click(move |_| {
                                let _ = gui_commands
                                    .send(Command::SetGuiAutostart(!gui_autostart.get()));
                            }),
                    )
                    .child(
                        Element::label(
                            "This controls only runwatch-gui.exe. It does not keep scientific observation alive by itself.",
                        )
                        .font_size(12.0)
                        .fg(Color::hex(0x5B6B75))
                        .width_match(),
                    ),
            )
            .width_match(),
        )
        .child(
            Element::card(
                "Notifications & display",
                Element::col()
                    .width_match()
                    .spacing(8)
                    .child(
                        Element::checkbox(
                            "Native Windows notifications",
                            notification_settings.native_notifications,
                        )
                        .enabled(NATIVE_NOTIFICATION_AVAILABLE)
                        .on_click(move |_| {
                            native_state
                                .native_notifications
                                .set(!native_state.native_notifications.get());
                            let _ = native_commands
                                .send(Command::SaveGuiSettings(native_state.snapshot()));
                        }),
                    )
                    .child(
                        Element::checkbox(
                            "Successful Run completions",
                            notification_settings.notify_success,
                        )
                        .enabled(NATIVE_NOTIFICATION_AVAILABLE)
                        .on_click(move |_| {
                            success_state
                                .notify_success
                                .set(!success_state.notify_success.get());
                            let _ = success_commands
                                .send(Command::SaveGuiSettings(success_state.snapshot()));
                        }),
                    )
                    .child(
                        Element::checkbox(
                            "Run attention alerts",
                            notification_settings.notify_attention,
                        )
                        .enabled(NATIVE_NOTIFICATION_AVAILABLE)
                        .on_click(move |_| {
                            attention_state
                                .notify_attention
                                .set(!attention_state.notify_attention.get());
                            let _ = attention_commands
                                .send(Command::SaveGuiSettings(attention_state.snapshot()));
                        }),
                    )
                    .child(
                        Element::checkbox(
                            "Include Run name in OS notifications",
                            notification_settings.include_run_name,
                        )
                        .enabled(NATIVE_NOTIFICATION_AVAILABLE)
                        .on_click(move |_| {
                            privacy_state
                                .include_run_name
                                .set(!privacy_state.include_run_name.get());
                            let _ = privacy_commands
                                .send(Command::SaveGuiSettings(privacy_state.snapshot()));
                        }),
                    )
                    .child(
                        Element::label(
                            "Native background notifications are waiting for the public WindUI runtime-notification bridge (upstream PR #13). The controls stay disabled until that API is released; in-app transition toasts remain active.",
                        )
                        .font_size(12.0)
                        .fg(Color::hex(0xB27A1B))
                        .width_match(),
                    )
                    .child(
                        Element::label(
                            "Only terminal/attention transitions may leave the app. Submit, retry, probe and settings results remain in-app toasts. Commands, workspaces, SSH details and continuation identity are never included in OS notifications.",
                        )
                        .font_size(12.0)
                        .fg(Color::hex(0x5B6B75))
                        .width_match(),
                    )
                    .child(
                        Element::label_signal(notification_settings.warning)
                            .font_size(12.0)
                            .fg(Color::hex(0xB27A1B))
                            .width_match()
                            .visible_when(move || !warning_visible.get().is_empty()),
                    ),
            )
            .width_match(),
        )
}
