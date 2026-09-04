use crate::controller::Command;
use crate::model::{
    DashboardSnapshot, RunDetailView, RunFilter, RunRow, RunSort, filter_rows, sort_rows,
};
use tokio::sync::mpsc;
use windui::prelude::*;

#[derive(Clone)]
pub struct RunsViewState {
    all_rows: Signal<Vec<RunRow>>,
    table_rows: Signal<Vec<Vec<String>>>,
    table_ids: Signal<Vec<String>>,
    filter: Signal<RunFilter>,
    sort: Signal<RunSort>,
    query: Signal<String>,
    active: Signal<String>,
    attention: Signal<String>,
    recent_terminal: Signal<String>,
    total: Signal<String>,
    daemon: Signal<String>,
    selected_id: Signal<String>,
    detail_request: Signal<u64>,
    detail_title: Signal<String>,
    overview: Signal<String>,
    logs: Signal<String>,
    artifacts: Signal<String>,
    timeline: Signal<String>,
    continuation: Signal<String>,
    log_tail: Signal<usize>,
    event_limit: Signal<usize>,
    log_wrap: Signal<bool>,
    can_cancel: Signal<bool>,
    cancel_dialog: Signal<bool>,
}

impl RunsViewState {
    pub fn new() -> Self {
        Self {
            all_rows: signal(Vec::new()),
            table_rows: signal(Vec::new()),
            table_ids: signal(Vec::new()),
            filter: signal(RunFilter::Active),
            sort: signal(RunSort::Priority),
            query: signal(String::new()),
            active: signal("0".into()),
            attention: signal("0".into()),
            recent_terminal: signal("0".into()),
            total: signal("0".into()),
            daemon: signal("Connecting to runwatchd...".into()),
            selected_id: signal(String::new()),
            detail_request: signal(0),
            detail_title: signal("Select a Run to inspect".into()),
            overview: signal("Double-click a row or use Open.".into()),
            logs: signal("No Run selected.".into()),
            artifacts: signal("No Run selected.".into()),
            timeline: signal("No Run selected.".into()),
            continuation: signal("No Run selected.".into()),
            log_tail: signal(80),
            event_limit: signal(80),
            log_wrap: signal(false),
            can_cancel: signal(false),
            cancel_dialog: signal(false),
        }
    }

    pub fn apply_snapshot(&self, snapshot: DashboardSnapshot) {
        self.active.set(snapshot.active.to_string());
        self.attention.set(snapshot.attention.to_string());
        self.recent_terminal
            .set(snapshot.recent_terminal.to_string());
        self.total.set(snapshot.total.to_string());
        self.daemon.set(format!(
            "runwatchd {} · pid {} · {}",
            snapshot.daemon_version,
            snapshot.daemon_pid,
            if snapshot.paused {
                "paused"
            } else {
                "connected"
            }
        ));
        self.all_rows.set(snapshot.rows);
        self.rebuild_visible();
    }

    pub fn daemon_unavailable(&self, error: String) {
        self.daemon.set(format!("runwatchd unavailable · {error}"));
    }

    pub fn apply_detail(&self, request_id: u64, detail: RunDetailView) {
        if request_id != self.detail_request.get() || self.selected_id.get() != detail.run_id {
            return;
        }
        self.detail_title.set(detail.title);
        self.overview.set(detail.overview);
        self.logs.set(detail.logs);
        self.artifacts.set(detail.artifacts);
        self.timeline.set(detail.timeline);
        self.continuation.set(detail.continuation);
        self.can_cancel.set(detail.can_cancel);
    }

    #[cfg(debug_assertions)]
    pub fn apply_fixture_detail(&self, detail: RunDetailView) {
        self.selected_id.set(detail.run_id.clone());
        let request_id = self.next_detail_request();
        self.apply_detail(request_id, detail);
    }

    fn rebuild_visible(&self) {
        let mut rows = filter_rows(&self.all_rows.get(), self.filter.get(), &self.query.get());
        sort_rows(&mut rows, self.sort.get());
        self.table_ids
            .set(rows.iter().map(|row| row.run_id.clone()).collect());
        self.table_rows
            .set(rows.iter().map(RunRow::table_cells).collect());
    }

    fn next_detail_request(&self) -> u64 {
        let next = self.detail_request.get().saturating_add(1).max(1);
        self.detail_request.set(next);
        next
    }

    fn request_detail(
        &self,
        run_id: String,
        tail: usize,
        event_limit: usize,
        commands: &mpsc::UnboundedSender<Command>,
    ) {
        let request_id = self.next_detail_request();
        let _ = commands.send(Command::LoadDetail {
            request_id,
            run_id,
            tail,
            event_limit,
        });
    }

    fn load_row(&self, row: usize, commands: &mpsc::UnboundedSender<Command>) {
        let Some(run_id) = self.table_ids.get().get(row).cloned() else {
            return;
        };
        self.selected_id.set(run_id.clone());
        self.detail_title.set(format!("Loading {run_id}..."));
        self.can_cancel.set(false);
        self.request_detail(
            run_id,
            self.log_tail.get(),
            self.event_limit.get(),
            commands,
        );
    }

    fn selected_row(&self) -> Option<RunRow> {
        let run_id = self.selected_id.get();
        self.all_rows
            .get()
            .into_iter()
            .find(|row| row.run_id == run_id)
    }

    fn reload_with_tail(&self, tail: usize, commands: &mpsc::UnboundedSender<Command>) {
        self.log_tail.set(tail);
        let run_id = self.selected_id.get();
        if !run_id.is_empty() {
            self.request_detail(run_id, tail, self.event_limit.get(), commands);
        }
    }

    fn reload_with_events(&self, event_limit: usize, commands: &mpsc::UnboundedSender<Command>) {
        self.event_limit.set(event_limit.clamp(1, 200));
        let run_id = self.selected_id.get();
        if !run_id.is_empty() {
            self.request_detail(
                run_id,
                self.log_tail.get(),
                self.event_limit.get(),
                commands,
            );
        }
    }

    pub fn build(&self, commands: mpsc::UnboundedSender<Command>) -> Element {
        let cards = Element::row()
            .width_match()
            .spacing(10)
            .child(summary_card("Active", self.active).weight(1.0))
            .child(summary_card("Attention", self.attention).weight(1.0))
            .child(summary_card("Recent 24h", self.recent_terminal).weight(1.0))
            .child(summary_card("Total", self.total).weight(1.0))
            .child(
                Element::card(
                    "Daemon",
                    Element::label_signal(self.daemon)
                        .font_size(12.0)
                        .fg(Color::hex(0x2AA8A0))
                        .width_match(),
                )
                .weight(2.0),
            );

        let apply_state = self.clone();
        let clear_state = self.clone();
        let active_state = self.clone();
        let attention_state = self.clone();
        let all_state = self.clone();
        let priority_state = self.clone();
        let newest_state = self.clone();
        let name_state = self.clone();
        let host_state = self.clone();
        let refresh_commands = commands.clone();
        let filters = Element::row()
            .width_match()
            .cross(Align::Center)
            .spacing(8)
            .child(Element::button("Active").small().on_click(move |_| {
                active_state.filter.set(RunFilter::Active);
                active_state.rebuild_visible();
            }))
            .child(
                Element::button("Attention")
                    .neutral()
                    .small()
                    .on_click(move |_| {
                        attention_state.filter.set(RunFilter::Attention);
                        attention_state.rebuild_visible();
                    }),
            )
            .child(Element::button("All").neutral().small().on_click(move |_| {
                all_state.filter.set(RunFilter::All);
                all_state.rebuild_visible();
            }))
            .child(
                Element::label("Sort")
                    .font_size(12.0)
                    .fg(Color::hex(0x5B6B75)),
            )
            .child(
                Element::button("Priority")
                    .outline()
                    .small()
                    .on_click(move |_| {
                        priority_state.sort.set(RunSort::Priority);
                        priority_state.rebuild_visible();
                    }),
            )
            .child(
                Element::button("Newest")
                    .neutral()
                    .outline()
                    .small()
                    .on_click(move |_| {
                        newest_state.sort.set(RunSort::Newest);
                        newest_state.rebuild_visible();
                    }),
            )
            .child(
                Element::button("Name")
                    .neutral()
                    .outline()
                    .small()
                    .on_click(move |_| {
                        name_state.sort.set(RunSort::Name);
                        name_state.rebuild_visible();
                    }),
            )
            .child(
                Element::button("Host")
                    .neutral()
                    .outline()
                    .small()
                    .on_click(move |_| {
                        host_state.sort.set(RunSort::Host);
                        host_state.rebuild_visible();
                    }),
            )
            .child(
                Element::text_input(self.query, "Search name, Run ID, host, handle, workspace")
                    .leading_icon('\u{1F50D}')
                    .weight(1.0),
            )
            .child(
                Element::button("Apply")
                    .outline()
                    .small()
                    .on_click(move |_| apply_state.rebuild_visible()),
            )
            .child(
                Element::button("Clear")
                    .neutral()
                    .outline()
                    .small()
                    .on_click(move |_| {
                        clear_state.query.set(String::new());
                        clear_state.rebuild_visible();
                    }),
            )
            .child(
                Element::button("Refresh")
                    .outline()
                    .small()
                    .on_click(move |_| {
                        let _ = refresh_commands.send(Command::Refresh);
                    }),
            );

        let action_state = self.clone();
        let action_commands = commands.clone();
        let activate_state = self.clone();
        let activate_commands = commands.clone();
        let table = Element::table_virtual(
            vec![
                ("State", 1.1),
                ("Name", 2.0),
                ("Runner", 0.9),
                ("Host", 1.0),
                ("Handle", 1.0),
                ("Observation", 1.7),
                ("Continuation", 1.2),
                ("Updated", 0.8),
            ],
            self.table_rows,
            42,
        )
        .actions("", 0.7, move |row| {
            let state = action_state.clone();
            let commands = action_commands.clone();
            Element::button("Open")
                .neutral()
                .outline()
                .small()
                .on_click(move |_| state.load_row(row, &commands))
        })
        .on_row_activate(move |_ctx, row| activate_state.load_row(row, &activate_commands))
        .width_match()
        .height(280);

        let detail_state = self.clone();
        let detail_commands = commands.clone();
        let probe_state = self.clone();
        let probe_commands = commands.clone();
        let copy_id_state = self.clone();
        let copy_handle_state = self.clone();
        let copy_workspace_state = self.clone();
        let cancel_state = self.clone();
        let detail_header = Element::row()
            .width_match()
            .cross(Align::Center)
            .spacing(8)
            .child(
                Element::label_signal(self.detail_title)
                    .font_size(18.0)
                    .fg(Color::hex(0x243746))
                    .weight(1.0),
            )
            .child(
                Element::button("Reload")
                    .neutral()
                    .outline()
                    .small()
                    .on_click(move |_| {
                        let run_id = detail_state.selected_id.get();
                        if !run_id.is_empty() {
                            detail_state.request_detail(
                                run_id,
                                detail_state.log_tail.get(),
                                detail_state.event_limit.get(),
                                &detail_commands,
                            );
                        }
                    }),
            )
            .child(
                Element::button("Probe now")
                    .neutral()
                    .outline()
                    .small()
                    .on_click(move |ctx| {
                        if let Some(row) = probe_state.selected_row() {
                            if row.active {
                                let _ = probe_commands.send(Command::Probe(row.run_id));
                            } else {
                                ctx.toast("Terminal Runs do not need an execution probe");
                            }
                        } else {
                            ctx.toast("Select a Run first");
                        }
                    }),
            )
            .child(
                Element::button("Copy ID")
                    .neutral()
                    .outline()
                    .small()
                    .on_click(move |ctx| {
                        let value = copy_id_state.selected_id.get();
                        if !value.is_empty() {
                            ctx.clipboard_set(&value);
                            ctx.toast("Run ID copied");
                        }
                    }),
            )
            .child(
                Element::button("Copy Handle")
                    .neutral()
                    .outline()
                    .small()
                    .on_click(move |ctx| {
                        if let Some(row) = copy_handle_state.selected_row() {
                            ctx.clipboard_set(&row.handle);
                            ctx.toast("Run handle copied");
                        }
                    }),
            )
            .child(
                Element::button("Copy Workspace")
                    .neutral()
                    .outline()
                    .small()
                    .on_click(move |ctx| {
                        if let Some(row) = copy_workspace_state.selected_row() {
                            ctx.clipboard_set(&row.workspace);
                            ctx.toast("Workspace copied");
                        }
                    }),
            )
            .child(
                Element::button("Cancel Run")
                    .danger()
                    .outline()
                    .small()
                    .on_click(move |ctx| {
                        if cancel_state.can_cancel.get() {
                            cancel_state.cancel_dialog.set(true);
                        } else {
                            ctx.toast("Select a non-terminal Run first");
                        }
                    }),
            );

        let tail80_state = self.clone();
        let tail80_commands = commands.clone();
        let tail200_state = self.clone();
        let tail200_commands = commands.clone();
        let tail500_state = self.clone();
        let tail500_commands = commands.clone();
        let copy_logs = self.logs;
        let wrap_state = self.clone();
        let wrapped_visible = self.log_wrap;
        let unwrapped_visible = self.log_wrap;
        let logs_panel = Element::col()
            .fill()
            .spacing(6)
            .child(
                Element::row()
                    .width_match()
                    .cross(Align::Center)
                    .spacing(6)
                    .child(Element::label("Tail lines").fg(Color::hex(0x5B6B75)))
                    .child(Element::button("80").small().on_click(move |_| {
                        tail80_state.reload_with_tail(80, &tail80_commands);
                    }))
                    .child(Element::button("200").neutral().small().on_click(move |_| {
                        tail200_state.reload_with_tail(200, &tail200_commands);
                    }))
                    .child(Element::button("500").neutral().small().on_click(move |_| {
                        tail500_state.reload_with_tail(500, &tail500_commands);
                    }))
                    .child(
                        Element::button("Copy")
                            .neutral()
                            .outline()
                            .small()
                            .on_click(move |ctx| {
                                ctx.clipboard_set(&copy_logs.get());
                                ctx.toast("Logs copied");
                            }),
                    )
                    .child(
                        Element::button("Toggle wrap")
                            .neutral()
                            .outline()
                            .small()
                            .on_click(move |_| wrap_state.log_wrap.set(!wrap_state.log_wrap.get())),
                    ),
            )
            .child(
                readonly_text(self.logs, true)
                    .visible_when(move || wrapped_visible.get())
                    .weight(1.0),
            )
            .child(
                readonly_text(self.logs, false)
                    .visible_when(move || !unwrapped_visible.get())
                    .weight(1.0),
            );
        let copy_artifacts = self.artifacts;
        let artifacts_panel = Element::col()
            .fill()
            .spacing(6)
            .child(
                Element::row()
                    .width_match()
                    .child(Element::label("").weight(1.0))
                    .child(
                        Element::button("Copy paths")
                            .neutral()
                            .outline()
                            .small()
                            .on_click(move |ctx| {
                                ctx.clipboard_set(&copy_artifacts.get());
                                ctx.toast("Artifact inventory copied");
                            }),
                    ),
            )
            .child(text_panel(self.artifacts, true).weight(1.0));

        let events80_state = self.clone();
        let events80_commands = commands.clone();
        let events200_state = self.clone();
        let events200_commands = commands.clone();
        let timeline_panel = Element::col()
            .fill()
            .spacing(6)
            .child(
                Element::row()
                    .width_match()
                    .cross(Align::Center)
                    .spacing(6)
                    .child(Element::label("Recent events").fg(Color::hex(0x5B6B75)))
                    .child(Element::button("80").small().on_click(move |_| {
                        events80_state.reload_with_events(80, &events80_commands);
                    }))
                    .child(Element::button("200").neutral().small().on_click(move |_| {
                        events200_state.reload_with_events(200, &events200_commands);
                    })),
            )
            .child(text_panel(self.timeline, true).weight(1.0));

        let detail_tabs = Element::tabs(
            signal(0usize),
            vec![
                ("Overview", text_panel(self.overview, true)),
                ("Logs", logs_panel),
                ("Artifacts", artifacts_panel),
                ("Timeline", timeline_panel),
                ("Continuation", text_panel(self.continuation, true)),
            ],
        )
        .weight(1.0);

        let cancel_dialog_state = self.clone();
        let cancel_commands = commands.clone();
        let cancel_close = self.cancel_dialog;
        let cancel_dialog = Element::dialog(
            self.cancel_dialog,
            Element::col()
                .width(460)
                .bg(Color::hex(0xFFFDF8))
                .corner(14.0)
                .padding(20)
                .spacing(12)
                .child(
                    Element::label("Request Run cancellation?")
                        .font_size(19.0)
                        .fg(Color::hex(0x243746))
                        .width_match(),
                )
                .child(
                    Element::label(
                        "runwatch will send a cancel request. The durable Run is not considered Cancelled until later observation confirms the terminal state.",
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
                            Element::button("Back")
                                .neutral()
                                .on_click(move |_| cancel_close.set(false)),
                        )
                        .child(
                            Element::button("Request cancel")
                                .danger()
                                .on_click(move |_| {
                                    let run_id = cancel_dialog_state.selected_id.get();
                                    if !run_id.is_empty() && cancel_dialog_state.can_cancel.get() {
                                        let _ = cancel_commands.send(Command::Cancel(run_id));
                                    }
                                    cancel_dialog_state.cancel_dialog.set(false);
                                }),
                        ),
                ),
        );

        Element::stack()
            .fill()
            .child(
                Element::col()
                    .fill()
                    .padding(16)
                    .spacing(10)
                    .child(cards)
                    .child(filters)
                    .child(table)
                    .child(Element::divider())
                    .child(detail_header)
                    .child(detail_tabs),
            )
            .child(cancel_dialog)
    }
}

fn summary_card(title: &str, value: Signal<String>) -> Element {
    Element::card(
        title,
        Element::label_signal(value)
            .font_size(22.0)
            .fg(Color::hex(0x243746))
            .width_match(),
    )
}

fn text_panel(text: Signal<String>, wrap: bool) -> Element {
    readonly_text(text, wrap)
}

fn readonly_text(text: Signal<String>, wrap: bool) -> Element {
    Element::text_input(text, "")
        .multiline()
        .wrap(wrap)
        .disabled(true)
        .fill()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detail(run_id: &str, title: &str) -> RunDetailView {
        RunDetailView {
            run_id: run_id.into(),
            title: title.into(),
            overview: format!("overview-{title}"),
            logs: format!("logs-{title}"),
            artifacts: format!("artifacts-{title}"),
            timeline: format!("timeline-{title}"),
            continuation: format!("continuation-{title}"),
            can_cancel: true,
        }
    }

    #[test]
    fn stale_detail_response_cannot_replace_newer_selection_or_reload() {
        let state = RunsViewState::new();
        state.selected_id.set("run-b".into());
        state.detail_request.set(2);

        state.apply_detail(1, detail("run-a", "old-a"));
        assert_eq!(state.detail_title.get(), "Select a Run to inspect");

        state.apply_detail(2, detail("run-b", "new-b"));
        assert_eq!(state.detail_title.get(), "new-b");
        assert_eq!(state.logs.get(), "logs-new-b");

        state.detail_request.set(4);
        state.apply_detail(3, detail("run-b", "old-tail"));
        assert_eq!(state.detail_title.get(), "new-b");
        state.apply_detail(4, detail("run-b", "new-tail"));
        assert_eq!(state.detail_title.get(), "new-tail");
    }
}
