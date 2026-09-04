use crate::controller::Command;
use crate::model::{
    DashboardSnapshot, RunDetailView, RunFilter, RunRow, RunSort, filter_rows, sort_rows,
};
use crate::submission::{ManualRunDraft, ManualRunner};
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
    manual_submit_supported: Signal<bool>,
    create_dialog: Signal<bool>,
    create_runner: Signal<ManualRunner>,
    create_pending: Signal<bool>,
    create_message: Signal<String>,
    create_name: Signal<String>,
    create_host: Signal<String>,
    create_cwd: Signal<String>,
    create_command: Signal<String>,
    create_time: Signal<String>,
    create_pool: Signal<String>,
    create_account: Signal<String>,
    create_cpus: Signal<String>,
    create_mem: Signal<String>,
    create_gpus: Signal<String>,
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
            manual_submit_supported: signal(false),
            create_dialog: signal(false),
            create_runner: signal(ManualRunner::Process),
            create_pending: signal(false),
            create_message: signal(String::new()),
            create_name: signal(String::new()),
            create_host: signal(String::new()),
            create_cwd: signal(String::new()),
            create_command: signal(String::new()),
            create_time: signal(String::new()),
            create_pool: signal(String::new()),
            create_account: signal(String::new()),
            create_cpus: signal(String::new()),
            create_mem: signal(String::new()),
            create_gpus: signal(String::new()),
        }
    }

    pub fn apply_snapshot(&self, snapshot: DashboardSnapshot) {
        self.manual_submit_supported
            .set(snapshot.manual_submit_supported);
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
        self.manual_submit_supported.set(false);
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

    pub fn manual_submit_succeeded(&self) {
        self.create_pending.set(false);
        self.create_dialog.set(false);
        self.create_message.set(String::new());
        self.create_name.set(String::new());
        self.create_host.set(String::new());
        self.create_cwd.set(String::new());
        self.create_command.set(String::new());
        self.create_time.set(String::new());
        self.create_pool.set(String::new());
        self.create_account.set(String::new());
        self.create_cpus.set(String::new());
        self.create_mem.set(String::new());
        self.create_gpus.set(String::new());
    }

    pub fn manual_submit_failed(&self, error: String) {
        self.create_pending.set(false);
        self.create_message
            .set(format!("Submission failed: {error}"));
    }

    fn manual_draft(&self) -> ManualRunDraft {
        ManualRunDraft {
            name: self.create_name.get(),
            host_alias: self.create_host.get(),
            cwd: self.create_cwd.get(),
            command: self.create_command.get(),
            time: self.create_time.get(),
            pool: self.create_pool.get(),
            account: self.create_account.get(),
            cpus: self.create_cpus.get(),
            mem: self.create_mem.get(),
            gpus: self.create_gpus.get(),
        }
    }

    #[cfg(debug_assertions)]
    pub fn apply_fixture_detail(&self, detail: RunDetailView) {
        self.selected_id.set(detail.run_id.clone());
        let request_id = self.next_detail_request();
        self.apply_detail(request_id, detail);
    }

    #[cfg(debug_assertions)]
    pub fn apply_fixture_create_dialog(&self) {
        self.create_runner.set(ManualRunner::Slurm);
        self.create_name.set("refine-map-manual".into());
        self.create_host.set("gm00".into());
        self.create_cwd.set("/share/project/refine".into());
        self.create_command
            .set("python refine.py --input map.mrc --iterations 12".into());
        self.create_time.set("02:00:00".into());
        self.create_pool.set("gpu".into());
        self.create_account.set("science".into());
        self.create_cpus.set("8".into());
        self.create_mem.set("32G".into());
        self.create_gpus.set("1".into());
        self.create_message.set(
            "Fixture preview: this remains an unbound manual Run and will be validated by runwatchd."
                .into(),
        );
        self.create_dialog.set(true);
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
        let new_run_state = self.clone();
        let manual_submit_visible = self.manual_submit_supported;
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
                Element::button("New Run")
                    .outline()
                    .small()
                    .on_click(move |ctx| {
                        if !new_run_state.manual_submit_supported.get() {
                            ctx.toast("This runwatchd does not advertise submit_run_v2");
                            return;
                        }
                        new_run_state.create_message.set(
                            "Manual Runs are not bound to Pi/Codex continuation. runwatchd remains the sole submission authority."
                                .into(),
                        );
                        new_run_state.create_dialog.set(true);
                    })
                    .visible_when(move || manual_submit_visible.get()),
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
                ("State", 0.9),
                ("Name", 1.75),
                ("Runner", 0.9),
                ("Host", 0.85),
                ("Handle", 1.0),
                ("Observation", 1.55),
                ("Continuation", 1.5),
                ("Updated", 1.15),
            ],
            self.table_rows,
            42,
        )
        .cell_lines(1)
        .actions("", 0.65, move |row| {
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
        .weight(1.0);

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

        let runner_process_state = self.clone();
        let runner_slurm_state = self.clone();
        let runner_lsf_state = self.clone();
        let remote_host_visible = self.create_runner;
        let remote_resources_visible = self.create_runner;
        let remote_hint_visible = self.create_runner;
        let create_back_state = self.clone();
        let create_submit_state = self.clone();
        let create_commands = commands.clone();
        let create_dialog = Element::dialog(
            self.create_dialog,
            Element::col()
                .width(720)
                .bg(Color::hex(0xFFFDF8))
                .corner(14.0)
                .padding(20)
                .spacing(9)
                .child(
                    Element::label("Start a manual Run")
                        .font_size(20.0)
                        .fg(Color::hex(0x243746))
                        .width_match(),
                )
                .child(
                    Element::label(
                        "The GUI sends one unbound submit_run_v2 request to runwatchd. No Pi/Codex session is attached, so terminal completion stays in the Human Run Console rather than resuming an agent automatically.",
                    )
                    .font_size(12.5)
                    .fg(Color::hex(0x5B6B75))
                    .width_match(),
                )
                .child(
                    Element::row()
                        .width_match()
                        .cross(Align::Center)
                        .spacing(7)
                        .child(Element::label("Runner").width(120))
                        .child(Element::button("Local Process").small().on_click(move |_| {
                            runner_process_state
                                .create_runner
                                .set(ManualRunner::Process);
                        }))
                        .child(
                            Element::button("Slurm")
                                .neutral()
                                .small()
                                .on_click(move |_| {
                                    runner_slurm_state.create_runner.set(ManualRunner::Slurm);
                                }),
                        )
                        .child(
                            Element::button("LSF")
                                .neutral()
                                .small()
                                .on_click(move |_| {
                                    runner_lsf_state.create_runner.set(ManualRunner::Lsf);
                                }),
                        ),
                )
                .child(form_field(
                    "Name",
                    self.create_name,
                    "Optional; a readable name is generated when empty",
                ))
                .child(
                    form_field(
                        "SSH Host alias",
                        self.create_host,
                        "Exact Host from ~/.ssh/config, e.g. gm00",
                    )
                    .visible_when(move || remote_host_visible.get() != ManualRunner::Process),
                )
                .child(form_field(
                    "Workspace",
                    self.create_cwd,
                    "Existing absolute local directory, or remote POSIX path",
                ))
                .child(
                    Element::label(
                        "Remote scheduler workspaces must be persistent and shared between the login host and compute nodes; node-local /tmp is not a safe default.",
                    )
                    .font_size(11.5)
                    .fg(Color::hex(0xB27A1B))
                    .width_match()
                    .visible_when(move || remote_hint_visible.get() != ManualRunner::Process),
                )
                .child(
                    Element::col()
                        .width_match()
                        .spacing(4)
                        .child(Element::label("Command").fg(Color::hex(0x5B6B75)))
                        .child(
                            Element::text_input(
                                self.create_command,
                                "PowerShell command locally; shell command under Slurm/LSF",
                            )
                            .multiline()
                            .wrap(true)
                            .width_match(),
                        ),
                )
                .child(
                    Element::col()
                        .width_match()
                        .spacing(6)
                        .child(form_field("Time", self.create_time, "e.g. 02:00:00"))
                        .child(form_field(
                            "Partition / queue",
                            self.create_pool,
                            "Slurm partition or LSF queue",
                        ))
                        .child(form_field("Account", self.create_account, "Optional"))
                        .child(form_field("CPUs", self.create_cpus, "Optional positive integer"))
                        .child(form_field("Memory", self.create_mem, "e.g. 32G"))
                        .child(form_field("GPUs", self.create_gpus, "Optional positive integer"))
                        .visible_when(move || {
                            remote_resources_visible.get() != ManualRunner::Process
                        }),
                )
                .child(
                    Element::label_signal(self.create_message)
                        .font_size(12.0)
                        .fg(Color::hex(0xB27A1B))
                        .width_match(),
                )
                .child(
                    Element::row()
                        .width_match()
                        .spacing(8)
                        .child(Element::label("").weight(1.0))
                        .child(Element::button("Back").neutral().on_click(move |ctx| {
                            if create_back_state.create_pending.get() {
                                ctx.toast("Submission is in progress");
                            } else {
                                create_back_state.create_dialog.set(false);
                            }
                        }))
                        .child(Element::button("Start Run").on_click(move |ctx| {
                            if create_submit_state.create_pending.get() {
                                ctx.toast("Submission is already in progress");
                                return;
                            }
                            match create_submit_state
                                .manual_draft()
                                .build_spec(create_submit_state.create_runner.get())
                            {
                                Ok(spec) => {
                                    let run_id = spec.run_id.clone();
                                    create_submit_state.create_pending.set(true);
                                    create_submit_state
                                        .create_message
                                        .set(format!("Submitting {run_id}..."));
                                    if create_commands.send(Command::SubmitManual(spec)).is_err() {
                                        create_submit_state.create_pending.set(false);
                                        create_submit_state.create_message.set(
                                            "Submission failed: GUI controller is unavailable".into(),
                                        );
                                    }
                                }
                                Err(error) => create_submit_state
                                    .create_message
                                    .set(format!("Check the form: {error:#}")),
                            }
                        })),
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
            .child(create_dialog)
    }
}

fn form_field(label: &str, value: Signal<String>, placeholder: &str) -> Element {
    Element::row()
        .width_match()
        .cross(Align::Center)
        .spacing(8)
        .child(Element::label(label).fg(Color::hex(0x5B6B75)).width(120))
        .child(Element::text_input(value, placeholder).weight(1.0))
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
