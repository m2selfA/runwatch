pub mod hosts;
pub mod runs;

use crate::controller::Command;
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
    commands: mpsc::UnboundedSender<Command>,
) -> Element {
    let gui_commands = commands.clone();
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
                Element::label(
                    "Transition notification state is disposable GUI UX state; durable Run and Delivery truth never depends on it.",
                )
                .font_size(12.0)
                .fg(Color::hex(0x5B6B75))
                .width_match(),
            )
            .width_match(),
        )
}
